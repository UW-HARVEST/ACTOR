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
        Command::Populate { ref target, blind } => {
            let dataset = Dataset::detect(target, blind);
            let dst_paths = battery::Paths::new(&repo_root, agent, dataset, model);
            let src_paths = battery::Paths::new(&repo_root, cli::Agent::Kiro, dataset, None);
            let inner = Dataset::strip_prefix(target);
            benchmark::for_dataset(dataset).populate(&src_paths, &dst_paths, inner)?;
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

// ── Populate: copy pre-verify artifacts into kiro-translate tree ────────
// Called from the Benchmark::populate impls in benchmark.rs.

pub(crate) fn populate_test_corpus(src: &battery::Paths, dst: &battery::Paths, battery: &str) -> Result<()> {
    let src_dir = src.results_dir.join(battery);
    let dst_dir = dst.results_dir.join(battery);
    if !src_dir.is_dir() { anyhow::bail!("{} not found", src_dir.display()); }
    let mut count = 0usize;
    for entry in std::fs::read_dir(&src_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let name = entry.file_name();
        let orig = entry.path().join(crate::battery::TRANSLATED_RUST_ORIGINAL);
        if !orig.is_dir() { continue; }
        let case_dst = dst_dir.join(&name);
        let tr_dst = case_dst.join(crate::battery::TRANSLATED_RUST);
        if tr_dst.is_dir() { count += 1; continue; } // already populated
        // Copy translated_rust_original → translated_rust (skip target/)
        copy_tree_no_target(&orig, &tr_dst)?;
        // Copy translation log for credits
        let log_src = entry.path().join("logs/translation.log");
        if log_src.exists() {
            let log_dst = case_dst.join("logs");
            std::fs::create_dir_all(&log_dst)?;
            std::fs::copy(&log_src, log_dst.join("translation.log"))?;
        }
        count += 1;
    }
    println!("✅ Populated {count} cases → {}", dst_dir.display());
    Ok(())
}

pub(crate) fn populate_blind_crust(
    src: &battery::Paths, dst: &battery::Paths,
    projects: &[battery::CrustProject],
) -> Result<()> {
    let mut count = 0usize;
    for project in projects {
        let name = project.name();
        let translate_src = src.results_dir.join(name).join(battery::TRANSLATE_DIR);
        if !translate_src.is_dir() { continue; }
        let proj_dst = dst.results_dir.join(name);
        if proj_dst.join(battery::VERIFY_DIR).join("Cargo.toml").exists() { count += 1; continue; }
        // translate/ → translate/ (unchanged)
        copy_tree_no_target(&translate_src, &proj_dst.join(battery::TRANSLATE_DIR))?;
        // translate/ → verify/ (test harness runs from verify_dir)
        copy_tree_no_target(&translate_src, &proj_dst.join(battery::VERIFY_DIR))?;
        count += 1;
    }
    println!("✅ Populated {count} CRUST-blind projects → {}", dst.results_dir.display());
    Ok(())
}

/// Copy a directory tree, skipping `target/` build artifacts.
fn copy_tree_no_target(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" { continue; }
        let dst_path = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree_no_target(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
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
