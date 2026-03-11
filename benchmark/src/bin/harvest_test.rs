use clap::Parser;
use harvest_benchmark::harness::{discover_binary, library, parse_test_vectors, run_exec_tests};
use harvest_benchmark::io::{collect_program_dirs, write_csv_results};
use harvest_benchmark::stats::{ProgramEvalStats, SummaryStats};
use harvest_benchmark::HarvestResult;
use rayon::prelude::*;
use regex::Regex;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "harvest-test", about = "Test pre-translated Rust projects against test vectors")]
struct Args {
    /// Test corpus directory (test_case/, test_vectors/, runner/ per case)
    corpus_dir: PathBuf,
    /// Directory containing translated Rust projects (Cargo.toml + src/ per case)
    results_dir: PathBuf,
    #[arg(long, default_value = "10")]
    timeout: u64,
    #[arg(long)]
    filter: Option<String>,
    #[arg(long, conflicts_with = "filter")]
    exclude: Option<String>,
}

fn main() -> HarvestResult<()> {
    let args = Args::parse();
    let results_dir = args.results_dir.canonicalize().map_err(|e| format!("{}: {}", args.results_dir.display(), e))?;
    let corpus_dir = args.corpus_dir.canonicalize().map_err(|e| format!("{}: {}", args.corpus_dir.display(), e))?;

    // Phase 1: Discover
    eprintln!("Phase 1: Discovering cases...");
    let mut case_dirs = collect_program_dirs(&results_dir)?;
    case_dirs.retain(|d| {
        let name = d.file_name().unwrap_or_default();
        d.join("Cargo.toml").exists() && corpus_dir.join(name).join("test_vectors").exists()
    });

    if let Some(ref pat) = args.filter {
        let rx = Regex::new(pat).map_err(|e| format!("bad regex: {}", e))?;
        case_dirs.retain(|d| rx.is_match(&d.file_name().unwrap_or_default().to_string_lossy()));
    }
    if let Some(ref pat) = args.exclude {
        let rx = Regex::new(pat).map_err(|e| format!("bad regex: {}", e))?;
        case_dirs.retain(|d| !rx.is_match(&d.file_name().unwrap_or_default().to_string_lossy()));
    }
    case_dirs.sort();

    let lib_case_names: Vec<String> = case_dirs.iter()
        .filter(|d| d.file_name().unwrap_or_default().to_string_lossy().ends_with("_lib"))
        .map(|d| d.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    let n_exec = case_dirs.len() - lib_case_names.len();
    eprintln!("  Found {} cases ({} exec, {} lib)", case_dirs.len(), n_exec, lib_case_names.len());

    // Phase 2: Build all translations in parallel
    eprintln!("Phase 2: Building translations...");
    let build_results: Vec<(String, Result<PathBuf, String>)> = case_dirs
        .par_iter()
        .map(|dir| {
            let name = dir.file_name().unwrap_or_default().to_string_lossy().to_string();
            let result = discover_binary(dir).map_err(|e| e.to_string());
            (name, result)
        })
        .collect();

    // Phase 3: Prepare lib workspace + batch build runners
    if !lib_case_names.is_empty() {
        eprintln!("Phase 3: Preparing {} lib runners...", lib_case_names.len());
        match library::prepare_lib_workspace(&corpus_dir, &results_dir, &lib_case_names) {
            Ok(runner_names) => {
                eprintln!("  Building runners...");
                if let Err(e) = library::build_runners_batch(&results_dir, &runner_names) {
                    eprintln!("  Warning: runner build failed: {}", e);
                }
            }
            Err(e) => eprintln!("  Warning: lib prep failed: {}", e),
        }
    }

    // Phase 4: Run tests in parallel
    eprintln!("Phase 4: Running tests...");
    let timeout = args.timeout;
    let results: Vec<ProgramEvalStats> = build_results
        .into_par_iter()
        .map(|(name, build_result)| {
            let is_lib = name.ends_with("_lib");
            let mut stats = ProgramEvalStats::new(&name);

            let tv_dir = corpus_dir.join(&name).join("test_vectors");
            let test_cases = match parse_test_vectors(&tv_dir) {
                Ok(tc) => tc,
                Err(e) => { stats.error_message = Some(e.to_string()); return stats; }
            };
            stats.total_tests = test_cases.len();

            if is_lib {
                stats.translation_success = true;
                stats.rust_build_success = build_result.is_ok();
                let runner_name = format!("_{}_runner", name);
                match library::run_lib_tests(&results_dir, &runner_name, &test_cases, timeout) {
                    Ok((tr, errs, passed)) => {
                        stats.test_results = tr;
                        stats.passed_tests = passed;
                        if !errs.is_empty() { stats.error_message = Some(errs.join("\n")); }
                    }
                    Err(e) => { stats.error_message = Some(e.to_string()); }
                }
            } else {
                match build_result {
                    Ok(bin) => {
                        stats.translation_success = true;
                        stats.rust_build_success = true;
                        let (tr, errs, passed) = run_exec_tests(&bin, &test_cases, timeout);
                        stats.test_results = tr;
                        stats.passed_tests = passed;
                        if !errs.is_empty() { stats.error_message = Some(errs.join("\n")); }
                    }
                    Err(e) => { stats.error_message = Some(e); }
                }
            }
            stats
        })
        .collect();

    // Output
    let csv_path = results_dir.join("results.csv");
    write_csv_results(&csv_path, &results)?;

    let summary = SummaryStats::from_results(&results);
    let total: usize = results.iter().map(|r| r.total_tests).sum();
    let passed: usize = results.iter().map(|r| r.passed_tests).sum();
    let n_passing = results.iter().filter(|r| r.passed_tests == r.total_tests && r.total_tests > 0).count();

    eprintln!("\n{}", "=".repeat(60));
    eprintln!("  Test vectors: {}/{} passed ({:.1}%)", passed, total, summary.overall_success_rate());
    eprintln!("  Cases: {}/{} fully passing", n_passing, results.len());
    eprintln!("{}", "=".repeat(60));

    let failed: Vec<_> = results.iter().filter(|r| r.passed_tests < r.total_tests || r.total_tests == 0).collect();
    if !failed.is_empty() {
        eprintln!("\nFailing cases:");
        for r in &failed {
            let reason = if r.total_tests == 0 { "no vectors".to_string() }
                else { format!("{}/{}", r.passed_tests, r.total_tests) };
            eprintln!("  {} ({})", r.program_name, reason);
        }
    }

    eprintln!("\nCSV: {}", csv_path.display());
    Ok(())
}
