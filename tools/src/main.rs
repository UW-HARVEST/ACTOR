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
            let bench = benchmark::for_dataset(dataset);

            let translations =
                bench.translate(&paths, inner, include_regex.as_deref(), parallel)?;
            let verifications = if !no_verify && bench.verifies(agent) {
                bench.verify(
                    &paths,
                    inner,
                    include_regex.as_deref(),
                    false,
                    parallel,
                    &translations,
                )?
            } else {
                harvest_tools::verify::Verifications::new()
            };
            // `Update` covers enrichment and table regeneration; no separate steps.
            report_test_outcome(run_test(
                &repo_root,
                bench.as_ref(),
                &paths,
                inner,
                Score {
                    mode: oracle::TestMode::Update,
                    on_failure: agent_health::OnInfraFailure::Refuse,
                    keep: eval::Keep::from_keep_eval_tree_flag(cli.keep_eval_tree),
                    source: eval::Source::Run {
                        translate: &translations,
                        verify: &verifications,
                    },
                    covers: oracle::Covers::from_include_regex(include_regex.as_deref()),
                },
            )?);
        }
        Command::Translate {
            ref target,
            include_regex,
            parallel,
        } => {
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
            benchmark::for_dataset(dataset).translate(
                &paths,
                inner,
                include_regex.as_deref(),
                parallel,
            )?;
        }
        Command::Verify {
            ref target,
            include_regex,
            force,
            parallel,
        } => {
            let dataset = Dataset::detect(target);
            let paths = battery::Paths::new(
                &repo_root,
                agent,
                dataset,
                model,
                cli::honouring(cache, cli::Reuse::from_force_flag(force)),
                harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(
                    cli.allow_unsandboxed,
                ),
            )?;
            let inner = Dataset::strip_prefix(target);
            let bench = benchmark::for_dataset(dataset);
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
            let seeds = battery::Paths::new(
                &repo_root,
                agent,
                dataset,
                model,
                cli::seeding(cache)?,
                harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(
                    cli.allow_unsandboxed,
                ),
            )?;
            let translations =
                bench.translate(&seeds, inner, include_regex.as_deref(), parallel)?;
            bench.verify(
                &paths,
                inner,
                include_regex.as_deref(),
                force,
                parallel,
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
        Command::Test {
            ref target,
            update,
            check,
            allow_infra_failures,
        } => {
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
            let mode = if update {
                oracle::TestMode::Update
            } else if check {
                oracle::TestMode::Check
            } else {
                oracle::TestMode::Run
            };

            report_test_outcome(run_test(
                &repo_root,
                benchmark::for_dataset(dataset).as_ref(),
                &paths,
                inner,
                Score {
                    mode,
                    on_failure: agent_health::OnInfraFailure::from_allow_infra_failures_flag(
                        allow_infra_failures,
                    ),
                    keep: eval::Keep::from_keep_eval_tree_flag(cli.keep_eval_tree),
                    source: eval::Source::Archive,
                    covers: oracle::Covers::WholeBattery,
                },
            )?);
        }
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
        Command::Report => {
            report::generate(&repo_root)?;
        }
    }
    Ok(())
}

struct Score<'a> {
    mode: oracle::TestMode,
    on_failure: agent_health::OnInfraFailure,
    keep: eval::Keep,
    source: eval::Source<'a>,
    covers: oracle::Covers<'a>,
}

/// The only path into scoring and table regeneration, so the tree and the gate are built once here. It
/// RETURNS the outcome rather than reporting it: [`report_test_outcome`] ends in `process::exit`, which
/// runs no destructor, and the [`eval::Tree`] whose `Drop` removes the tree is live in this frame.
fn run_test(
    repo_root: &std::path::Path,
    bench: &dyn benchmark::Benchmark,
    paths: &battery::Paths,
    target: &str,
    score: Score<'_>,
) -> Result<oracle::TestOutcome> {
    let Score {
        mode,
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
    let outcome = bench.test(
        paths,
        target,
        &oracle::Scoring {
            mode,
            source,
            tree: &tree,
            gate: &gate,
            covers,
        },
    )?;
    if matches!(mode, oracle::TestMode::Update) {
        // Best-effort: the tables are a whole-corpus roll-up, so `report::generate`
        // legitimately fails on a partial tree and must not fail the score run.
        match report::generate(repo_root) {
            Ok(()) => println!("📊 Tables regenerated (tables/)"),
            Err(e) => eprintln!(
                "⚠️  Skipped table regeneration (results tree not complete enough): {e}\n   \
                 Run `harvest-tools report` once all agents/datasets are populated."
            ),
        }
    }
    Ok(outcome)
}

fn report_test_outcome(outcome: oracle::TestOutcome) {
    if let oracle::TestOutcome::Failed(ref mismatches) = outcome {
        eprintln!("\n❌ {} battery(ies) mismatched:", mismatches.len());
        for m in mismatches {
            eprintln!("  {}: {}", m.battery, m.diffs.join("; "));
        }
        std::process::exit(1);
    }
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

    struct Mismatching;

    impl benchmark::Benchmark for Mismatching {
        fn name(&self) -> &'static str {
            "mismatching"
        }
        fn verifies(&self, _agent: cli::Agent) -> bool {
            false
        }
        fn translate(
            &self,
            _paths: &battery::Paths,
            _target: &str,
            _filter: Option<&str>,
            _parallel: usize,
        ) -> Result<harvest_tools::translate::Translations> {
            unreachable!("scoring only")
        }
        fn test(
            &self,
            paths: &battery::Paths,
            target: &str,
            scoring: &oracle::Scoring<'_>,
        ) -> Result<oracle::TestOutcome> {
            scoring.tree.scope(target)?;
            assert!(
                eval_root(paths).join(target).is_dir(),
                "the tree must be standing while the score runs, or its removal proves nothing"
            );
            Ok(oracle::TestOutcome::Failed(vec![oracle::BatteryMismatch {
                battery: target.to_string(),
                diffs: vec!["vectors_passed: 393 → 0".to_string()],
            }]))
        }
        fn enrich(&self, _paths: &battery::Paths, _target: &str) -> Result<()> {
            unreachable!("scoring only")
        }
    }

    /// `process::exit` runs no destructor, so reporting the mismatch from inside `run_test` left the
    /// whole tree standing on the one path a `--check` failure takes, unasked for `--keep-eval-tree`.
    #[test]
    fn a_mismatch_reported_to_the_operator_still_removes_the_evaluation_tree() {
        let tmp = harvest_tools::io::workdir::test_tempdir().unwrap();
        let paths = battery::Paths::new(
            tmp.path(),
            cli::Agent::C2rust,
            Dataset::TestCorpus,
            None,
            cache::Mode::Bypass,
            harvest_tools::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap();
        let translations = harvest_tools::translate::Translations::new();
        let verifications = harvest_tools::verify::Verifications::new();

        let outcome = run_test(
            tmp.path(),
            &Mismatching,
            &paths,
            "B01",
            Score {
                mode: oracle::TestMode::Check,
                on_failure: agent_health::OnInfraFailure::Refuse,
                keep: eval::Keep::Discard,
                source: eval::Source::Run {
                    translate: &translations,
                    verify: &verifications,
                },
                covers: oracle::Covers::WholeBattery,
            },
        )
        .unwrap();

        assert!(
            matches!(outcome, oracle::TestOutcome::Failed(_)),
            "the mismatch must come back to `main` to be exited on, not be exited on in here"
        );
        assert!(
            !eval_root(&paths).exists(),
            "and the tree must be gone by then: {}",
            eval_root(&paths).display()
        );
    }
}
