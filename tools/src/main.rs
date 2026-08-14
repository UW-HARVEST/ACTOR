mod agent_health;
mod battery;
mod benchmark;
mod cargo_toml;
mod cli;
mod exclusions;
mod opencode;
mod sandbox;
mod report;
mod scoring;
mod test;
mod translate;
mod verify;
mod workdir;

use anyhow::Result;
use cli::{Cli, Command, Dataset};

fn main() -> Result<()> {
    let cli = Cli::parse_args();
    let repo_root = find_repo_root()?;
    let agent = cli.agent;
    let model = cli.model.as_deref();

    // Two agents pick their model at runtime: `oneshot` (an OpenRouter model id)
    // and `opencode` (an OpenCode `provider/model` id). Every other agent has its
    // model fixed by the variant, so a `--model` there would be silently ignored.
    let model_driven = matches!(agent, cli::Agent::Oneshot | cli::Agent::OpenCode);
    if model_driven && model.is_none() {
        anyhow::bail!(
            "--model is required with --agent {}\n  \
             oneshot:  --model openai/gpt-5.4\n  \
             opencode: --model amazon-bedrock/us.anthropic.claude-sonnet-5",
            if agent == cli::Agent::Oneshot { "oneshot" } else { "opencode" },
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
            limit,
            blind,
        } => {
            let dataset = Dataset::detect(target, blind);
            let paths = battery::Paths::new(&repo_root, agent, dataset, model);
            let inner = Dataset::strip_prefix(target);
            let bench = benchmark::for_dataset(dataset);

            // The ONE lifecycle: translate → [verify?] → enrich → score.
            // `verifies` folds the old nine-clause skip `if` into a property.
            bench.translate(&paths, inner, include_regex.as_deref(), parallel, limit)?;
            if !no_verify && bench.verifies(agent) {
                bench.verify(&repo_root, &paths, inner, include_regex.as_deref(), false, parallel)?;
            }
            // `test --update` folds enrichment in; it is never a separate step,
            // and run_test regenerates the tables so they never drift.
            run_test(&repo_root, bench.as_ref(), &paths, inner, test::TestMode::Update, false)?;
        }
        Command::Translate {
            ref target,
            include_regex,
            parallel,
            limit,
            blind,
        } => {
            let dataset = Dataset::detect(target, blind);
            let paths = battery::Paths::new(&repo_root, agent, dataset, model);
            let inner = Dataset::strip_prefix(target);
            benchmark::for_dataset(dataset)
                .translate(&paths, inner, include_regex.as_deref(), parallel, limit)?;
        }
        Command::Verify {
            ref target,
            include_regex,
            force,
            parallel,
            blind,
        } => {
            let dataset = Dataset::detect(target, blind);
            let paths = battery::Paths::new(&repo_root, agent, dataset, model);
            let inner = Dataset::strip_prefix(target);
            benchmark::for_dataset(dataset)
                .verify(&repo_root, &paths, inner, include_regex.as_deref(), force, parallel)?;
        }
        Command::Test {
            ref target,
            update,
            check,
            blind,
            allow_infra_failures,
        } => {
            let dataset = Dataset::detect(target, blind);
            let paths = battery::Paths::new(&repo_root, agent, dataset, model);
            let inner = Dataset::strip_prefix(target);
            let mode = if update {
                test::TestMode::Update
            } else if check {
                test::TestMode::Check
            } else {
                test::TestMode::Run
            };

            run_test(&repo_root, benchmark::for_dataset(dataset).as_ref(), &paths, inner, mode, allow_infra_failures)?;
        }
        Command::Enrich { ref target, blind } => {
            let dataset = Dataset::detect(target, blind);
            let paths = battery::Paths::new(&repo_root, agent, dataset, model);
            let inner = Dataset::strip_prefix(target);
            benchmark::for_dataset(dataset).enrich(&paths, inner)?;
        }
        Command::Report => {
            report::generate(&repo_root)?;
        }
        Command::ScoreSelfgenBaselines => {
            test::score_selfgen_baselines(&repo_root)?;
        }
    }
    Ok(())
}

/// Score a target, then — in `--update` mode — regenerate the report tables so
/// `tables/` always reflect what has just been run. This is the single seam
/// that keeps scored data and the generated tables from drifting: there is no
/// separate `report` step to remember. `report::generate` reads every agent's
/// data from disk, so the tables reflect the whole current state (not just the
/// slice re-run), and it self-guards (skips results.md if the corpus submodule
/// is absent, bails on inconsistency), so a partial run can't clobber tables.
fn run_test(
    repo_root: &std::path::Path,
    bench: &dyn benchmark::Benchmark,
    paths: &battery::Paths,
    target: &str,
    mode: test::TestMode,
    allow_infra_failures: bool,
) -> Result<()> {
    // Gate BEFORE scoring, not after: `bench.test` writes result.json and
    // `report::generate` rewrites every file in tables/, so by the time a
    // warning printed the damage would already be on disk. This is the only
    // path into either, which is why the check lives here rather than in each
    // dataset's scorer.
    let audit = agent_health::audit(&paths.results_dir)?;
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
    if matches!(mode, test::TestMode::Update) {
        // Regenerate tables from the current on-disk state. The tables are a
        // whole-corpus roll-up with cross-agent invariants (report::generate
        // bails if, say, an agent dir is missing), so on a partial/in-progress
        // tree it can legitimately fail — that must NOT fail the score run the
        // user asked for. Treat regeneration as best-effort: succeed silently
        // when the tree is complete, warn (don't error) when it isn't, so a
        // single-tool rerun always works and never silently leaves stale tables.
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

/// Print a test outcome and exit non-zero on `--check` mismatch.
fn report_test_outcome(outcome: test::TestOutcome) {
    if let test::TestOutcome::Failed(ref mismatches) = outcome {
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
