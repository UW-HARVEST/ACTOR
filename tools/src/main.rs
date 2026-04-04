mod battery;
mod cargo_toml;
mod cli;
mod test;
mod translate;
mod verify;

use anyhow::Result;
use cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse_args();
    let repo_root = find_repo_root()?;
    let agent = cli.agent;

    match cli.command {
        Command::Run {
            ref target,
            no_verify,
            include_regex,
            parallel,
        } => {
            let (battery_name, case_filter) = parse_target(target, include_regex.as_deref());
            translate::run(&repo_root, &battery_name, case_filter.as_deref(), agent, parallel)?;
            if !no_verify {
                verify::run(&repo_root, &battery_name, case_filter.as_deref(), false, agent, parallel)?;
            }
            test::run(&repo_root, &battery_name, test::TestMode::Update, agent)?;
        }
        Command::Translate {
            ref target,
            include_regex,
            parallel,
        } => {
            let (battery_name, case_filter) = parse_target(target, include_regex.as_deref());
            translate::run(&repo_root, &battery_name, case_filter.as_deref(), agent, parallel)?;
        }
        Command::Verify {
            ref target,
            include_regex,
            force,
            parallel,
        } => {
            let (battery_name, case_filter) = parse_target(target, include_regex.as_deref());
            verify::run(&repo_root, &battery_name, case_filter.as_deref(), force, agent, parallel)?;
        }
        Command::Test {
            ref target,
            update,
            check,
        } => {
            let mode = if update {
                test::TestMode::Update
            } else if check {
                test::TestMode::Check
            } else {
                test::TestMode::Run
            };
            let (battery_name, _) = parse_target(target, None);
            let outcome = test::run(&repo_root, &battery_name, mode, agent)?;
            if let test::TestOutcome::Failed(ref mismatches) = outcome {
                eprintln!("\n❌ {} battery(ies) mismatched:", mismatches.len());
                for m in mismatches {
                    eprintln!("  {}: {}", m.battery, m.diffs.join("; "));
                }
                std::process::exit(1);
            }
        }
    }
    Ok(())
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

fn parse_target(target: &str, include_regex: Option<&str>) -> (String, Option<String>) {
    if let Some((battery, case)) = target.split_once('/') {
        (battery.to_string(), Some(format!("{}$", case)))
    } else {
        (target.to_string(), include_regex.map(String::from))
    }
}
