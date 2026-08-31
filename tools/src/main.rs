use anyhow::Result;
// Never re-declare these with `mod` here; see the note in lib.rs.
use harvest_tools::analyse::report;
use harvest_tools::cli::{Cli, Command, Dataset};
use harvest_tools::eval;
use harvest_tools::{agent_health, battery, benchmark, chain, cli, oracle, provenance, store};

fn main() -> Result<()> {
    let cli = Cli::parse_args();
    let repo_root = find_repo_root()?;
    let mode = cli.cache_mode();

    // Before, not after, a run that can take hours: a binary that does not match the checkout cannot
    // produce an attributable measurement.
    if cli.command.produces_artifacts() {
        provenance::require_reproducible(if cli.allow_dirty {
            provenance::OnUnreproducible::WarnAndStamp
        } else {
            provenance::OnUnreproducible::Refuse
        })?;
        harvest_tools::refusal::require_pinned_toolchain()?;
    }

    anyhow::ensure!(!cli.tool.is_empty(), "--tool names no tool");
    // Refused before any work: an ablation on a tool with no ablation prompts would otherwise read
    // the base prompt and file the result as an experiment that never ran.
    for &tool in &cli.tool {
        harvest_tools::prompt::supports(tool, cli.variant)?;
    }

    let enforcement =
        harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(cli.allow_unsandboxed);

    match cli.command {
        Command::Run {
            ref target,
            steps,
            ref include_regex,
        } => {
            let dataset = Dataset::detect(target);
            let inner = Dataset::strip_prefix(target);

            // One thread per tool, each with its own budget: `--parallel 3` over three tools is three
            // in flight PER TOOL. Nothing is shared that a parallel run could corrupt -- each tool has
            // its own results tree, evaluation tree and store prefix
            // (`battery::tests::no_two_tools_share_an_output_or_evaluation_path`).
            // Borrowed once outside the closures: `move` on a `PathBuf` or an `Option<String>` would
            // take it from the enclosing scope, which still needs both after the join.
            let root = repo_root.as_path();
            let model = cli.model.as_deref();
            let variant = cli.variant;
            let keep = eval::Keep::from_keep_eval_tree_flag(cli.keep_eval_tree);
            let attested: Vec<report::Attested> = std::thread::scope(|scope| {
                let handles: Vec<_> = cli
                    .tool
                    .iter()
                    .map(|&tool| {
                        scope.spawn(move || {
                            run_tool(RunTool {
                                repo_root: root,
                                tool,
                                variant,
                                model,
                                dataset,
                                inner,
                                steps,
                                include_regex: include_regex.as_deref(),
                                parallel: cli.parallel,
                                mode,
                                enforcement,
                                keep,
                            })
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| panic!("a tool's thread panicked"))
                    })
                    .collect::<Result<Vec<_>>>()
            })?;

            // Written ONCE, from every tool's attestation merged. Per tool it would rewrite `tables/`
            // from that tool's rows alone and blank the others' -- which is what one run per tool did.
            // Each dataset still writes only ITS tables: the two are earned by separate runs, so
            // folding them together would have a Test-Corpus run blank every harvest-bench row.
            let whole_scope = include_regex.is_none()
                && inner
                    == match dataset {
                        Dataset::TestCorpus => "all",
                        Dataset::HarvestBench => "HB",
                    };
            if whole_scope {
                let merged =
                    attested
                        .into_iter()
                        .fold(report::Attested::default(), |mut acc, a| {
                            acc.absorb(a);
                            acc
                        });
                match dataset {
                    Dataset::TestCorpus => report::generate(&repo_root, &merged)?,
                    Dataset::HarvestBench => report::generate_harvest_bench(&repo_root, &merged)?,
                }
                println!("\u{1f4ca} Tables regenerated (tables/)");
            }
        }
        Command::Cache { action } => match action {
            cli::CacheAction::Failures => {
                let failures = store::Store::open(&repo_root, mode)?.failures()?;
                println!("{} recorded failure(s):", failures.len());
                for (at, outcome) in &failures {
                    println!("  {outcome:?}  {at}");
                }
            }
            cli::CacheAction::Stats => {
                let (entries, bytes) = store::Store::open(&repo_root, mode)?.stats()?;
                println!(
                    "{entries} entries, {:.1} MB at {}/results/.cache",
                    bytes as f64 / 1_048_576.0,
                    repo_root.display()
                );
            }
        },
        Command::Enrich { ref target } => {
            let dataset = Dataset::detect(target);
            let paths = battery::Paths::new(
                &repo_root,
                cli.tool[0],
                cli.variant,
                dataset,
                cli.model.as_deref(),
                mode,
                enforcement,
            )?;
            // The same door the chain uses, so this backfills what a run publishes -- followers
            // included, and nothing a `read_dir` happened to find.
            let bench = benchmark::for_dataset(dataset);
            let inner = Dataset::strip_prefix(target);
            let scope = benchmark::Scope::resolve(bench.as_ref(), &paths, inner, None)?;
            let mut enriched = 0usize;
            for unit in scope.units() {
                let jobs = bench.jobs(&paths, unit, scope.filter())?;
                enriched += oracle::enrich_cases(&chain::case_dirs(&jobs))?;
            }
            println!("\u{2705} Enriched {enriched} result.json file(s) under {inner}");
        }
    }
    Ok(())
}

/// Everything one tool's run needs. A struct because half of these are `&str`/`Option<&str>` and
/// positional arguments of the same type transpose silently.
struct RunTool<'a> {
    repo_root: &'a std::path::Path,
    tool: cli::Tool,
    variant: cli::Variant,
    model: Option<&'a str>,
    dataset: Dataset,
    inner: &'a str,
    steps: Option<usize>,
    include_regex: Option<&'a str>,
    parallel: usize,
    mode: store::Mode,
    enforcement: harvest_tools::io::sandbox::Enforcement,
    keep: eval::Keep,
}

/// One tool, end to end: preflight, run every unit's chain, score, and report what it attested.
///
/// Returns the attestation rather than writing tables, so the caller can merge every tool's rows into
/// one `tables/` write instead of having each tool clobber the last.
fn run_tool(r: RunTool<'_>) -> Result<report::Attested> {
    // This tool's ONE budget; a step cannot mint a second (see `agents::Pool`).
    let pool = harvest_tools::agents::Pool::for_run(r.parallel);
    let bench = benchmark::for_dataset(r.dataset);
    let paths = bench.preflight(battery::Paths::new(
        r.repo_root,
        r.tool,
        r.variant,
        r.dataset,
        r.model,
        r.mode,
        r.enforcement,
    )?)?;
    let store = store::Store::open(r.repo_root, r.mode)?;

    // ONE loop over the units, each running the whole chain. There is no translate pass and no verify
    // pass: a unit's cases go end to end, which is what `run_or_replay` being one function buys.
    let scope = benchmark::Scope::resolve(bench.as_ref(), &paths, r.inner, r.include_regex)?;
    // Resolved BEFORE the first invocation and reused by the chain, the publishability check and
    // enrich: an unreadable corpus must refuse before the money, and a case list derived twice is how
    // a one-case run came to score a battery named `B01_organic/bin2hex_lib` after paying for it.
    let units: Vec<(String, Vec<chain::Job>)> = scope
        .units()
        .iter()
        .map(|unit| {
            bench
                .jobs(&paths, unit, scope.filter())
                .map(|jobs| (unit.clone(), jobs))
        })
        .collect::<Result<_>>()?;
    let mut resolved = eval::Resolved::new();
    let mut refused: Vec<String> = Vec::new();
    for (unit, jobs) in &units {
        let ran = chain::run_unit(&paths, &store, unit, jobs, r.steps, &pool)?;
        resolved.extend(ran.resolved);
        refused.extend(ran.refused);
    }
    println!("{} {}", cli::tool_dir(r.tool), store.tally_line());
    // Counted where the tally is, so a refusal cannot be read as a translation failure. A
    // C-to-Rust memory-safety corpus earns these: B01_synthetic alone holds 12 cases named for
    // buffer overflows, and codex's classifier declined one of them.
    if !refused.is_empty() {
        println!(
            "{} \u{1f6ab} provider refusals: {} ({})",
            cli::tool_dir(r.tool),
            refused.len(),
            refused.join(", ")
        );
    }

    let (publishable, attested) = benchmark::InScope::derive(&paths, &units, &resolved)?;
    let all_roles = harvest_tools::prompt::chain(r.tool, r.variant);
    let roles = &all_roles[..r.steps.map_or(all_roles.len(), |n| n.min(all_roles.len()))];
    for unit in publishable.units() {
        run_test(
            bench.as_ref(),
            &paths,
            unit,
            Score {
                on_failure: agent_health::OnInfraFailure::Refuse,
                keep: r.keep,
                roles,
                resolved: &resolved,
                covers: scope.covers(),
            },
        )?;
    }
    Ok(attested)
}

struct Score<'a> {
    on_failure: agent_health::OnInfraFailure,
    keep: eval::Keep,
    roles: &'a [harvest_tools::prompt::Role],
    resolved: &'a eval::Resolved,
    covers: oracle::Covers<'a>,
}

/// A score REFUSES rather than reporting a verdict, so the only way out is `?` -- which unwinds,
/// running the [`eval::EvalTree`] `Drop` that removes the tree. The `--check` mode this replaced ended
/// in `process::exit`, which runs no destructor and once left the whole tree standing.
fn run_test(
    bench: &dyn benchmark::Benchmark,
    paths: &benchmark::Preflighted,
    target: &str,
    score: Score<'_>,
) -> Result<()> {
    let Score {
        on_failure,
        keep,
        roles,
        resolved,
        covers,
    } = score;
    let gate = agent_health::Gate {
        format: harvest_tools::runners::log_format(paths.tool),
        on_failure,
        results_dir: &paths.results_dir,
    };
    let tree = eval::EvalTree::create_empty(paths, target, keep)?;
    bench.test(
        paths,
        target,
        &oracle::Scoring {
            roles,
            resolved,
            tree: &tree,
            gate: &gate,
            covers,
        },
    )
}

fn find_repo_root() -> Result<std::path::PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("test-corpus").is_dir() && dir.join("results").is_dir() {
            return Ok(dir);
        }
        if !dir.pop() {
            anyhow::bail!("Could not find repo root (looking for test-corpus/ and results/)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_root(paths: &battery::Paths) -> std::path::PathBuf {
        paths.repo_root.join(eval::EVAL_DIR).join(
            paths
                .results_dir
                .strip_prefix(
                    paths
                        .results_dir
                        .ancestors()
                        .nth(3)
                        .unwrap_or(&paths.results_dir),
                )
                .unwrap_or(&paths.results_dir),
        )
    }

    struct Refusing;

    impl benchmark::Benchmark for Refusing {
        fn preflight(&self, _paths: battery::Paths) -> Result<benchmark::Preflighted> {
            unreachable!("the test mints its proof through the real HarvestBench preflight")
        }
        fn name(&self) -> &'static str {
            "refusing"
        }
        fn test(
            &self,
            paths: &benchmark::Preflighted,
            target: &str,
            scoring: &oracle::Scoring<'_>,
        ) -> Result<()> {
            scoring.tree.scope(target)?;
            assert!(
                eval_root(paths).join(target).is_dir(),
                "the tree must be standing while the score runs, or its removal proves nothing"
            );
            anyhow::bail!("{target}: vectors_passed 393 → 0")
        }
        fn batteries(&self, _paths: &battery::Paths, target: &str) -> Result<Vec<String>> {
            Ok(vec![target.to_string()])
        }
        fn jobs(
            &self,
            _paths: &battery::Paths,
            _unit: &str,
            _filter: Option<&str>,
        ) -> Result<Vec<chain::Job>> {
            unreachable!("scoring only")
        }
    }

    /// A score that REFUSES leaves by `?`, which unwinds and runs the tree's `Drop`. The `--check`
    /// mode this replaced left by `process::exit`, which runs no destructor and once left the whole tree
    /// standing on the one path a failure takes, unasked for `--keep-eval-tree`.
    #[test]
    fn a_score_that_refuses_still_removes_the_evaluation_tree() {
        let tmp = harvest_tools::io::workdir::test_tempdir().unwrap();
        let unchecked = || {
            battery::Paths::new(
                tmp.path(),
                cli::Tool::C2rust,
                cli::Variant::Default,
                Dataset::HarvestBench,
                None,
                store::Mode::ReadWrite,
                harvest_tools::io::sandbox::Enforcement::AllowUnsandboxed,
            )
            .unwrap()
        };
        let bench = benchmark::for_dataset(Dataset::HarvestBench);

        // The only way to obtain what `run_test` takes, which IS the guarantee. Absent inputs first, so
        // the pass below is not vacuous.
        let missing = bench
            .preflight(unchecked())
            .err()
            .expect("no harvest-bench/tests, so no phase may start");
        assert!(
            format!("{missing:#}").contains("harvest-bench"),
            "and it must name what is missing: {missing:#}"
        );
        std::fs::create_dir_all(tmp.path().join("harvest-bench/tests")).unwrap();
        let scorer = tmp.path().join("harvest-bench/runner/target/release");
        std::fs::create_dir_all(&scorer).unwrap();
        std::fs::write(scorer.join("harvest-bench"), "").unwrap();
        // The HB preflight also demands a cmake new enough to configure a gtest suite. Prepending a
        // fake is race-free HERE and nowhere else: this test target holds exactly one test. Prepended,
        // not replaced, so every other program a test spawns still resolves.
        let bin = tmp.path().join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        // The old cmake FIRST, so the check has a named failing input: without it, deleting the
        // check from preflight would turn nothing red. 3.22 is what this class of box ships.
        harvest_tools::io::workdir::fake_program(&bin, "cmake", "echo 'cmake version 3.22.2'");
        let old_cmake = bench
            .preflight(unchecked())
            .err()
            .expect("a cmake that cannot configure a gtest suite must refuse before the money");
        assert!(
            format!("{old_cmake:#}").contains("cmake 3.22"),
            "and must name it: {old_cmake:#}"
        );
        harvest_tools::io::workdir::fake_program(&bin, "cmake", "echo 'cmake version 3.28.6'");

        let paths = bench
            .preflight(unchecked())
            .expect("every input is present now");
        let resolved = eval::Resolved::new();

        let refused = run_test(
            &Refusing,
            &paths,
            "B01",
            Score {
                on_failure: agent_health::OnInfraFailure::Refuse,
                keep: eval::Keep::Discard,
                roles: &[harvest_tools::prompt::Role::Translate],
                resolved: &resolved,
                covers: oracle::Covers::WholeBattery,
            },
        );

        let err = refused.expect_err("the fixture refuses, so this must carry the refusal out");
        assert!(
            format!("{err:#}").contains("393"),
            "carrying the reason to `main` rather than exiting in here: {err:#}"
        );
        assert!(
            !eval_root(&paths).exists(),
            "and the tree must be gone by then: {}",
            eval_root(&paths).display()
        );
    }
}
