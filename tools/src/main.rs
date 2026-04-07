mod battery;
mod cargo_toml;
mod cli;
mod test;
mod translate;
mod verify;

use anyhow::Result;
use cli::{Cli, Command, Dataset, TranslatePlan, VerifyPlan, TestPlan};

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
            limit,
            blind,
        } => {
            let dataset = Dataset::detect(target, blind);
            let paths = battery::Paths::new(&repo_root, agent, dataset);
            let inner = Dataset::strip_prefix(target);

            let tp = make_translate_plan(&paths, inner, include_regex.as_deref(), parallel, limit)?;
            let vp = if no_verify || dataset == Dataset::Crust {
                VerifyPlan::Skip
            } else {
                make_verify_plan(&paths, inner, include_regex.as_deref(), parallel, false)?
            };
            let test_p = make_test_plan(&paths, inner, test::TestMode::Update)?;

            execute_translate(&paths, &tp)?;
            execute_verify(&repo_root, &paths, &vp)?;
            execute_test(&paths, &test_p)?;
        }
        Command::Translate {
            ref target,
            include_regex,
            parallel,
            limit,
            blind,
        } => {
            let dataset = Dataset::detect(target, blind);
            let paths = battery::Paths::new(&repo_root, agent, dataset);
            let inner = Dataset::strip_prefix(target);

            let plan = make_translate_plan(&paths, inner, include_regex.as_deref(), parallel, limit)?;
            execute_translate(&paths, &plan)?;
        }
        Command::Verify {
            ref target,
            include_regex,
            force,
            parallel,
            blind,
        } => {
            let dataset = Dataset::detect(target, blind);
            let paths = battery::Paths::new(&repo_root, agent, dataset);
            let inner = Dataset::strip_prefix(target);

            let plan = make_verify_plan(&paths, inner, include_regex.as_deref(), parallel, force)?;
            execute_verify(&repo_root, &paths, &plan)?;
        }
        Command::Test {
            ref target,
            update,
            check,
            blind,
        } => {
            let dataset = Dataset::detect(target, blind);
            let paths = battery::Paths::new(&repo_root, agent, dataset);
            let inner = Dataset::strip_prefix(target);
            let mode = if update {
                test::TestMode::Update
            } else if check {
                test::TestMode::Check
            } else {
                test::TestMode::Run
            };

            let plan = make_test_plan(&paths, inner, mode)?;
            let outcome = execute_test(&paths, &plan)?;
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

// ── Plan constructors ──────────────────────────────────────────────────

fn make_translate_plan(
    paths: &battery::Paths, target: &str, _include_regex: Option<&str>,
    parallel: usize, limit: Option<usize>,
) -> Result<TranslatePlan> {
    match paths.dataset {
        Dataset::TestCorpus => {
            let batteries = resolve_batteries(&paths.corpus_dir, target)?;
            Ok(TranslatePlan::TestCorpus { batteries, parallel })
        }
        Dataset::Crust => {
            let projects = resolve_crust_projects(&paths.corpus_dir, target, limit)?;
            Ok(TranslatePlan::Crust { projects, parallel })
        }
        Dataset::BlindCrust => {
            let projects = resolve_crust_projects(&paths.corpus_dir, target, limit)?;
            Ok(TranslatePlan::BlindCrust { projects, parallel })
        }
    }
}

fn make_verify_plan(
    paths: &battery::Paths, target: &str, _include_regex: Option<&str>,
    parallel: usize, force: bool,
) -> Result<VerifyPlan> {
    match paths.dataset {
        Dataset::TestCorpus => {
            let batteries = resolve_batteries(&paths.corpus_dir, target)?;
            Ok(VerifyPlan::TestCorpus { batteries, parallel, force })
        }
        Dataset::Crust => Ok(VerifyPlan::Skip),
        Dataset::BlindCrust => {
            let projects = resolve_crust_projects(&paths.corpus_dir, target, None)?;
            Ok(VerifyPlan::BlindCrust { projects, parallel, force })
        }
    }
}

fn make_test_plan(
    paths: &battery::Paths, target: &str, mode: test::TestMode,
) -> Result<TestPlan> {
    match paths.dataset {
        Dataset::TestCorpus => {
            let batteries = resolve_batteries(&paths.corpus_dir, target)?;
            Ok(TestPlan::TestCorpus { batteries, mode })
        }
        Dataset::Crust => {
            let projects = resolve_crust_projects(&paths.corpus_dir, target, None)?;
            Ok(TestPlan::Crust { projects, mode })
        }
        Dataset::BlindCrust => {
            let projects = resolve_crust_projects(&paths.corpus_dir, target, None)?;
            Ok(TestPlan::BlindCrust { projects, mode })
        }
    }
}

fn resolve_batteries(corpus_dir: &std::path::Path, target: &str) -> Result<Vec<String>> {
    if target == "all" {
        battery::all_batteries(corpus_dir)
    } else {
        Ok(vec![target.to_string()])
    }
}

fn resolve_crust_projects(
    corpus_dir: &std::path::Path, target: &str, limit: Option<usize>,
) -> Result<Vec<battery::CrustProject>> {
    if target.eq_ignore_ascii_case("crust") || target == "all" {
        battery::CrustProject::discover(corpus_dir, limit)
    } else {
        Ok(vec![battery::CrustProject::validated(corpus_dir, target)?])
    }
}

// ── Plan executors ─────────────────────────────────────────────────────

fn execute_translate(paths: &battery::Paths, plan: &TranslatePlan) -> Result<()> {
    match plan {
        TranslatePlan::TestCorpus { batteries, parallel } => {
            if batteries.len() > 1 && *parallel > 1 {
                let (shared_bats, indie_bats): (Vec<&str>, Vec<&str>) = batteries.iter()
                    .map(String::as_str)
                    .partition(|b| battery::has_shared_source_groups(&paths.corpus_dir, b));

                let indie_parallel = parallel.saturating_sub(shared_bats.len()).max(1);

                let errors: Vec<anyhow::Error> = std::thread::scope(|s| {
                    let mut handles = Vec::new();

                    for bat in &shared_bats {
                        handles.push(s.spawn(move || -> Result<()> {
                            let (name, filter) = parse_target(bat, None);
                            translate::run_test_corpus(paths, &name, filter.as_deref(), 1)
                        }));
                    }

                    if !indie_bats.is_empty() {
                        handles.push(s.spawn(|| -> Result<()> {
                            for bat in &indie_bats {
                                let (name, filter) = parse_target(bat, None);
                                translate::run_test_corpus(paths, &name, filter.as_deref(), indie_parallel)?;
                            }
                            Ok(())
                        }));
                    }

                    handles.into_iter().filter_map(|h| match h.join() {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(e),
                        Err(_) => Some(anyhow::anyhow!("translate thread panicked")),
                    }).collect()
                });

                if let Some(first) = errors.into_iter().next() {
                    return Err(first);
                }
            } else {
                for bat in batteries {
                    let (name, filter) = parse_target(bat, None);
                    translate::run_test_corpus(paths, &name, filter.as_deref(), *parallel)?;
                }
            }
        }
        TranslatePlan::Crust { projects, parallel } => {
            translate::run_crust(paths, projects, *parallel)?;
        }
        TranslatePlan::BlindCrust { projects, parallel } => {
            translate::run_crust_blind(paths, projects, *parallel)?;
        }
    }
    Ok(())
}

fn execute_verify(repo_root: &std::path::Path, paths: &battery::Paths, plan: &VerifyPlan) -> Result<()> {
    match plan {
        VerifyPlan::TestCorpus { batteries, parallel, force } => {
            if batteries.len() > 1 {
                verify::run_all(repo_root, paths, batteries, *force, *parallel)?;
            } else {
                for bat in batteries {
                    let (name, filter) = parse_target(bat, None);
                    verify::run(repo_root, paths, &name, filter.as_deref(), *force, *parallel)?;
                }
            }
        }
        VerifyPlan::BlindCrust { projects, parallel, force } => {
            translate::verify_crust_blind(paths, projects, *parallel, *force)?;
        }
        VerifyPlan::Skip => {}
    }
    Ok(())
}

fn execute_test(paths: &battery::Paths, plan: &TestPlan) -> Result<test::TestOutcome> {
    match plan {
        TestPlan::TestCorpus { batteries, mode } => {
            let mut all_mismatches = Vec::new();
            for bat in batteries {
                if let test::TestOutcome::Failed(m) = test::run_test_corpus(paths, bat, *mode)? {
                    all_mismatches.extend(m);
                }
            }
            if all_mismatches.is_empty() {
                Ok(test::TestOutcome::Passed)
            } else {
                Ok(test::TestOutcome::Failed(all_mismatches))
            }
        }
        TestPlan::Crust { projects, mode } => {
            test::run_crust_test(paths, projects, *mode)
        }
        TestPlan::BlindCrust { projects, mode } => {
            test::run_blind_crust_test(paths, projects, *mode)
        }
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

fn parse_target(target: &str, include_regex: Option<&str>) -> (String, Option<String>) {
    if let Some((battery, case)) = target.split_once('/') {
        (battery.to_string(), Some(format!("{}$", case)))
    } else {
        (target.to_string(), include_regex.map(String::from))
    }
}
