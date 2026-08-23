use anyhow::Result;
// Never re-declare these with `mod` here; see the note in lib.rs.
use harvest_tools::agents::opencode;
use harvest_tools::analyse::report;
use harvest_tools::cli::{Cli, Command, Dataset};
use harvest_tools::eval;
use harvest_tools::{agent_health, battery, benchmark, cache, cli, oracle, provenance};

fn main() -> Result<()> {
    let cli = Cli::parse_args();
    let repo_root = find_repo_root()?;
    let agent = cli.agent;
    let model = cli.model.as_deref();
    let cache = cli.cache_mode();

    // Before, not after, a run that can take hours: a binary that does not match the
    // checkout cannot produce an attributable measurement.
    if cli.command.produces_artifacts() {
        provenance::require_reproducible(if cli.allow_dirty {
            provenance::OnUnreproducible::WarnAndStamp
        } else {
            provenance::OnUnreproducible::Refuse
        })?;
    }

    // Only these two take a model id at runtime; every other agent has its model fixed
    // by the variant, so a `--model` there would be silently ignored.
    let model_driven = matches!(agent, cli::Agent::Oneshot | cli::Agent::OpenCode);
    if model_driven && model.is_none() {
        anyhow::bail!(
            "--model is required with --agent {}\n  \
             oneshot:  --model openai/gpt-5.4\n  \
             opencode: --model amazon-bedrock/us.anthropic.claude-sonnet-5",
            if agent == cli::Agent::Oneshot {
                "oneshot"
            } else {
                "opencode"
            },
        );
    }
    if !model_driven && model.is_some() {
        anyhow::bail!("--model is only valid with --agent oneshot or --agent opencode");
    }
    // Fail at startup rather than deep inside a multi-hour agent run.
    if agent == cli::Agent::OpenCode {
        opencode::parse_model(model.unwrap_or_default())?;
    }

    match cli.command {
        Command::Run {
            ref target,
            no_verify,
            include_regex,
            parallel,
        } => {
            // The run's ONE budget; a phase cannot mint a second (see `agents::Pool`).
            let pool = harvest_tools::agents::Pool::for_run(parallel);
            let dataset = Dataset::detect(target);
            let bench = benchmark::for_dataset(dataset);
            // Before the first agent: every binary, interpreter and corpus dir the phases below reach for.
            let paths = bench.preflight(battery::Paths::new(
                &repo_root,
                agent,
                dataset,
                model,
                cache,
                harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(
                    cli.allow_unsandboxed,
                ),
            )?)?;
            let inner = Dataset::strip_prefix(target);

            let translations = bench.translate(&paths, inner, include_regex.as_deref(), &pool)?;

            // `InScope` cannot be built a unit at a time, which is what starved `verify` below.
            let (scope, attested) =
                benchmark::InScope::derive(bench.as_ref(), &paths, inner, &translations)?;

            // Verify gets the WHOLE scope in ONE call; scoring stays per unit, each with its own scope.
            let verifications = if !no_verify && bench.verifies(agent) {
                bench.verify(
                    &paths,
                    &scope,
                    include_regex.as_deref(),
                    false,
                    &pool,
                    &translations,
                )?
            } else {
                harvest_tools::verify::Verifications::new()
            };
            for battery in scope.units() {
                run_test(
                    bench.as_ref(),
                    &paths,
                    battery,
                    Score {
                        on_failure: agent_health::OnInfraFailure::Refuse,
                        keep: eval::Keep::from_keep_eval_tree_flag(cli.keep_eval_tree),
                        source: eval::Source {
                            translate: &translations,
                            verify: &verifications,
                        },
                        covers: oracle::Covers::from_include_regex(include_regex.as_deref()),
                    },
                )?;
            }

            // Written ONCE from the whole scope: per battery it would erase every other battery's
            // rows, which is `Covers::Subset`'s rule one level up. A partial target writes none, and
            // each dataset writes only ITS tables -- the two are earned by separate runs, so folding
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
                println!("📊 Tables regenerated (tables/)");
            }
        }
        Command::Translate {
            ref target,
            include_regex,
            parallel,
        } => {
            let pool = harvest_tools::agents::Pool::for_run(parallel);
            let dataset = Dataset::detect(target);
            let bench = benchmark::for_dataset(dataset);
            let paths = bench.preflight(battery::Paths::new(
                &repo_root,
                agent,
                dataset,
                model,
                cache,
                harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(
                    cli.allow_unsandboxed,
                ),
            )?)?;
            let inner = Dataset::strip_prefix(target);
            bench.translate(&paths, inner, include_regex.as_deref(), &pool)?;
        }
        Command::Verify {
            ref target,
            include_regex,
            force,
            parallel,
        } => {
            let pool = harvest_tools::agents::Pool::for_run(parallel);
            let dataset = Dataset::detect(target);
            let bench = benchmark::for_dataset(dataset);
            let paths = bench.preflight(battery::Paths::new(
                &repo_root,
                agent,
                dataset,
                model,
                cli::honouring(cache, cli::Reuse::from_force_flag(force)),
                harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(
                    cli.allow_unsandboxed,
                ),
            )?)?;
            let inner = Dataset::strip_prefix(target);
            // At startup, not per case: this agent has no C-as-oracle verify phase, and
            // the sweep would otherwise print a ✅ per case for a phase that never ran.
            anyhow::ensure!(
                bench.verifies(agent),
                "--agent {} has no separate C-as-oracle verify phase, so there is nothing \
                 to verify. Its `translated/` result is what gets scored (`test`).",
                format!("{agent:?}").to_lowercase(),
            );
            // Verify is seeded from a translation THIS RUN resolved, so translations are resolved first —
            // through `cli::seeding`, a store that may only REPLAY. A command named `verify` must not pay
            // the translate agent or replace what it checks, and `--force` reaches verify and nothing else.
            let seeds = bench.preflight(battery::Paths::new(
                &repo_root,
                agent,
                dataset,
                model,
                cli::seeding(cache)?,
                harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(
                    cli.allow_unsandboxed,
                ),
            )?)?;
            let translations = bench.translate(&seeds, inner, include_regex.as_deref(), &pool)?;
            let (scope, _) =
                benchmark::InScope::derive(bench.as_ref(), &paths, inner, &translations)?;
            bench.verify(
                &paths,
                &scope,
                include_regex.as_deref(),
                force,
                &pool,
                &translations,
            )?;
        }
        Command::Cache { action } => match action {
            cli::CacheAction::Stats => {
                // Bypass, not the operator's --cache: `stats` must never create an entry.
                let store = cache::Store::open(&repo_root, cache::Mode::Bypass)?;
                let (entries, bytes) = store.stats()?;
                println!(
                    "{entries} entries, {:.1} MB at {}/results/.cache",
                    bytes as f64 / 1_048_576.0,
                    repo_root.display()
                );
            }
            cli::CacheAction::Failures => {
                let store = cache::Store::open(&repo_root, cache::Mode::Bypass)?;
                let failures = store.failures()?;
                println!(
                    "{} recorded failure(s) under {}/results/.cache/{}/{}",
                    failures.len(),
                    repo_root.display(),
                    cache::SCHEMA,
                    cache::FAILED
                );
                for (phase, agent, key, attempt) in &failures {
                    println!("  {phase:<10} {agent:<12} {key}  attempt {attempt}");
                }
            }
        },
        Command::Enrich { ref target } => {
            let dataset = Dataset::detect(target);
            let paths = battery::Paths::new(
                &repo_root,
                agent,
                dataset,
                model,
                cache,
                harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(
                    cli.allow_unsandboxed,
                ),
            )?;
            let inner = Dataset::strip_prefix(target);
            benchmark::for_dataset(dataset).enrich(&paths, inner)?;
        }
    }
    Ok(())
}

struct Score<'a> {
    on_failure: agent_health::OnInfraFailure,
    keep: eval::Keep,
    source: eval::Source<'a>,
    covers: oracle::Covers<'a>,
}

/// A score REFUSES rather than reporting a verdict, so the only way out is `?` — which unwinds, running
/// the [`eval::Tree`] `Drop` that removes the tree. The `--check` mode this replaced ended in
/// `process::exit`, which runs no destructor and once left the whole tree standing.
fn run_test(
    bench: &dyn benchmark::Benchmark,
    paths: &benchmark::Preflighted,
    target: &str,
    score: Score<'_>,
) -> Result<()> {
    let Score {
        on_failure,
        keep,
        source,
        covers,
    } = score;
    let gate = agent_health::Gate {
        format: paths.agent.log_format(),
        on_failure,
        results_dir: &paths.results_dir,
    };
    let tree = eval::Tree::create_empty(paths, keep)?;
    bench.test(
        paths,
        target,
        &oracle::Scoring {
            source,
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
        paths
            .repo_root
            .join(eval::EVAL_DIR)
            .join(paths.agent_key.as_str())
    }

    struct Refusing;

    impl benchmark::Benchmark for Refusing {
        fn preflight(&self, _paths: battery::Paths) -> Result<benchmark::Preflighted> {
            unreachable!("the test mints its proof through the real HarvestBench preflight")
        }
        fn name(&self) -> &'static str {
            "refusing"
        }
        fn verifies(&self, _agent: cli::Agent) -> bool {
            false
        }
        fn translate(
            &self,
            _paths: &benchmark::Preflighted,
            _target: &str,
            _filter: Option<&str>,
            _pool: &harvest_tools::agents::Pool,
        ) -> Result<harvest_tools::translate::Translations> {
            unreachable!("scoring only")
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
            _resolved: &harvest_tools::translate::Translations,
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
                cli::Agent::C2rust,
                Dataset::HarvestBench,
                None,
                cache::Mode::Bypass,
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
        let translations = harvest_tools::translate::Translations::new();
        let verifications = harvest_tools::verify::Verifications::new();

        let refused = run_test(
            &Refusing,
            &paths,
            "B01",
            Score {
                on_failure: agent_health::OnInfraFailure::Refuse,
                keep: eval::Keep::Discard,
                source: eval::Source {
                    translate: &translations,
                    verify: &verifications,
                },
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
