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

    match cli.command {
        Command::Run {
            ref target,
            no_verify,
            include_regex,
        } => {
            let (battery_name, case_filter) = parse_target(target, include_regex.as_deref());
            translate::run(&repo_root, &battery_name, case_filter.as_deref())?;
            if !no_verify {
                verify::run(&repo_root, &battery_name, case_filter.as_deref(), false)?;
            }
            test::run(&repo_root, &battery_name, true)?;
        }
        Command::Translate {
            ref target,
            include_regex,
        } => {
            let (battery_name, case_filter) = parse_target(target, include_regex.as_deref());
            translate::run(&repo_root, &battery_name, case_filter.as_deref())?;
        }
        Command::Verify {
            ref target,
            include_regex,
            force,
        } => {
            let (battery_name, case_filter) = parse_target(target, include_regex.as_deref());
            verify::run(&repo_root, &battery_name, case_filter.as_deref(), force)?;
        }
        Command::Test {
            ref target,
            update,
        } => {
            let (battery_name, _) = parse_target(target, None);
            test::run(&repo_root, &battery_name, update)?;
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
