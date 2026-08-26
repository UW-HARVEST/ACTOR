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
    }

    harvest_tools::prompt::supports(cli.tool, cli.variant)?;

    let enforcement =
        harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(cli.allow_unsandboxed);

    match cli.command {
        Command::Run {
            ref target,
            steps,
            ref include_regex,
            parallel,
        } => {
            // The run's ONE budget; a step cannot mint a second (see `agents::Pool`).
            let pool = harvest_tools::agents::Pool::for_run(parallel);
            let dataset = Dataset::detect(target);
            let bench = benchmark::for_dataset(dataset);
            let paths = bench.preflight(battery::Paths::new(
                &repo_root,
                cli.tool,
                cli.variant,
                dataset,
                cli.model.as_deref(),
                mode,
                enforcement,
            )?)?;
            let inner = Dataset::strip_prefix(target);
            let store = store::Store::open(&repo_root, mode)?;

            // ONE loop over the units, each running the whole chain. There is no translate pass and
            // no verify pass: a unit's cases go end to end, which is what `run_or_replay` being one
            // function buys.
            let (units, filter) =
                benchmark::scope(bench.as_ref(), &paths, inner, include_regex.as_deref())?;
            let mut resolved = eval::Resolved::new();
            for unit in &units {
                let ran = chain::run_unit(&paths, &store, unit, filter.as_deref(), steps, &pool)?;
                resolved.extend(ran.resolved);
            }
            println!("{}", store.tally_line());

            let (scope, attested) =
                benchmark::InScope::derive(bench.as_ref(), &paths, inner, &resolved)?;
            let roles = {
                let all = harvest_tools::prompt::chain(cli.tool, cli.variant);
                &all[..steps.map_or(all.len(), |n| n.min(all.len()))]
            };
            for unit in scope.units() {
                run_test(
                    bench.as_ref(),
                    &paths,
                    unit,
                    Score {
                        on_failure: agent_health::OnInfraFailure::Refuse,
                        keep: eval::Keep::from_keep_eval_tree_flag(cli.keep_eval_tree),
                        roles,
                        resolved: &resolved,
                        covers: oracle::Covers::from_include_regex(include_regex.as_deref()),
                    },
                )?;
            }

            // Written ONCE from the whole scope: per unit it would erase every other unit's rows.
            // Each dataset writes only ITS tables -- the two are earned by separate runs, so folding
            // them together would have a Test-Corpus run blank every harvest-bench row it never saw.
            let whole_scope = include_regex.is_none()
                && inner
                    == match dataset {
                        Dataset::TestCorpus => "all",
                        Dataset::HarvestBench => "HB",
                    };
            if whole_scope {
                match dataset {
                    Dataset::TestCorpus => report::generate(&repo_root, &attested)?,
                    Dataset::HarvestBench => report::generate_harvest_bench(&repo_root, &attested)?,
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
                cli.tool,
                cli.variant,
                dataset,
                cli.model.as_deref(),
                mode,
                enforcement,
            )?;
            benchmark::for_dataset(dataset).enrich(&paths, Dataset::strip_prefix(target))?;
        }
    }
    Ok(())
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
    let tree = eval::EvalTree::create_empty(paths, keep)?;
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
        fn enrich(&self, _paths: &battery::Paths, _target: &str) -> Result<()> {
            unreachable!("scoring only")
        }
        fn batteries(&self, _paths: &battery::Paths, target: &str) -> Result<Vec<String>> {
            Ok(vec![target.to_string()])
        }
        fn attests(
            &self,
            _paths: &battery::Paths,
            _battery: &str,
            _resolved: &harvest_tools::eval::Resolved,
        ) -> Result<()> {
            Ok(())
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
