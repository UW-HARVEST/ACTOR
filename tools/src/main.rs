mod battery;
mod benchmark;
mod cargo_toml;
mod cli;
mod exclusions;
mod report;
mod scoring;
mod test;
mod translate;
mod verify;

use anyhow::Result;
use cli::{Cli, Command, Dataset};

fn main() -> Result<()> {
    let cli = Cli::parse_args();
    let repo_root = find_repo_root()?;
    let agent = cli.agent;
    let model = cli.model.as_deref();

    if agent == cli::Agent::Oneshot && model.is_none() {
        anyhow::bail!("--model is required with --agent oneshot (e.g. --model openai/gpt-5.4)");
    }
    if agent != cli::Agent::Oneshot && model.is_some() {
        anyhow::bail!("--model is only valid with --agent oneshot");
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
            // `test --update` folds enrichment in; it is never a separate step.
            let outcome = bench.test(&paths, inner, test::TestMode::Update)?;
            report_test_outcome(outcome);
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

            let outcome = benchmark::for_dataset(dataset).test(&paths, inner, mode)?;
            report_test_outcome(outcome);
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
