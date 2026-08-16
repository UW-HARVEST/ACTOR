use anyhow::Result;
// Never re-declare these with `mod` here; see the note in lib.rs.
use harvest_tools::agents::opencode;
use harvest_tools::analyse::report;
use harvest_tools::cli::{Cli, Command, Dataset};
use harvest_tools::{agent_health, battery, benchmark, cache, cli, oracle, provenance};

fn main() -> Result<()> {
    let cli = Cli::parse_args();
    let repo_root = find_repo_root()?;
    let agent = cli.agent;
    let model = cli.model.as_deref();
    let cache = cli.cache;

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
                cache.into(),
                harvest_tools::io::sandbox::Enforcement::from_allow_unsandboxed_flag(
                    cli.allow_unsandboxed,
                ),
            )?;
            let inner = Dataset::strip_prefix(target);
            let bench = benchmark::for_dataset(dataset);

            bench.translate(&paths, inner, include_regex.as_deref(), parallel)?;
            if !no_verify && bench.verifies(agent) {
                bench.verify(
                    &repo_root,
                    &paths,
                    inner,
                    include_regex.as_deref(),
                    false,
                    parallel,
                )?;
            }
            // `Update` covers enrichment and table regeneration; no separate steps.
            run_test(
                &repo_root,
                bench.as_ref(),
                &paths,
                inner,
                oracle::TestMode::Update,
                false,
            )?;
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
                cache.into(),
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
                cache.into(),
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
            bench.verify(
                &repo_root,
                &paths,
                inner,
                include_regex.as_deref(),
                force,
                parallel,
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
                cache.into(),
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

            run_test(
                &repo_root,
                benchmark::for_dataset(dataset).as_ref(),
                &paths,
                inner,
                mode,
                allow_infra_failures,
            )?;
        }
        Command::Enrich { ref target } => {
            let dataset = Dataset::detect(target);
            let paths = battery::Paths::new(
                &repo_root,
                agent,
                dataset,
                model,
                cache.into(),
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

/// The only path into scoring and table regeneration, so the health gate below can
/// live here rather than in each dataset's scorer.
fn run_test(
    repo_root: &std::path::Path,
    bench: &dyn benchmark::Benchmark,
    paths: &battery::Paths,
    target: &str,
    mode: oracle::TestMode,
    allow_infra_failures: bool,
) -> Result<()> {
    // Must precede scoring: `bench.test` writes result.json and `report::generate`
    // rewrites all of tables/, so a warning afterwards comes too late to help.
    let audit = agent_health::audit(&paths.results_dir, paths.agent.log_format())?;
    if let Some(report) = agent_health::describe_infra_failures(&audit) {
        agent_health::record_infra_failures(&paths.results_dir, &audit)?;
        if !allow_infra_failures {
            anyhow::bail!(
                "{report}\n\
                 Refusing to score. An infrastructure failure is not a result.\n\
                 Re-run those cases (`verify <target> --force` after fixing the cause), \
                 or pass --allow-infra-failures to score anyway.\n\
                 Details written to {}/INFRA_FAILURES.json",
                paths.results_dir.display()
            );
        }
        eprintln!(
            "⚠️  --allow-infra-failures: scoring despite dead agent runs.\n{report}\
             These cases have no measurement; treat any number derived from them as unsupported."
        );
    }

    let outcome = bench.test(paths, target, mode)?;
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
    report_test_outcome(outcome);
    Ok(())
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
