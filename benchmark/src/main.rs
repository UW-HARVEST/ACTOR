mod cli;
mod ir_utils;
mod logger;

use clap::Parser;
use cli::Args;
use harvest_benchmark::error::HarvestResult;
use harvest_benchmark::harness::{
    cleanup_benchmarks, parse_benchmark_dir, parse_test_vectors, validate_binary_output,
};
use harvest_benchmark::io::{
    collect_program_dirs, ensure_output_directory, log_failing_programs, log_found_programs,
    log_summary_stats, validate_input_directory, write_csv_results, write_error_file,
};
use harvest_benchmark::stats::{ProgramEvalStats, SummaryStats, TestResult};
use harvest_core::utils::get_version;
use harvest_core::HarvestIR;
use harvest_translate::{transpile, util::set_user_only_umask};
use ir_utils::{cargo_build_result, raw_cargo_package, raw_source};
use logger::TeeLogger;
use regex::Regex;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Encapsulate important results from transpilation
pub struct TranspilationResult {
    translation_success: bool,
    build_success: bool,
    rust_binary_path: PathBuf,
    build_error: Option<String>,
}

impl TranspilationResult {
    /// Extract relevant info from HarvestIR
    pub fn from_ir(ir: &HarvestIR) -> Self {
        let translation_success = raw_cargo_package(ir).is_ok();
        let (build_success, rust_binary_path, build_error) = match cargo_build_result(ir) {
            Ok(artifacts) => {
                if artifacts.is_empty() {
                    // Empty artifacts list indicates build succeeded but produced no output
                    (
                        false,
                        PathBuf::new(),
                        Some("Build succeeded but produced no artifacts".to_string()),
                    )
                } else {
                    // Prefer the first artifact as the "binary" path for executable cases.
                    let first = artifacts.first().cloned().unwrap();
                    (true, first, None)
                }
            }
            Err(err) => (false, PathBuf::new(), Some(err.clone())),
        };

        Self {
            translation_success,
            build_success,
            rust_binary_path,
            build_error,
        }
    }
}

/// Translates a C source directory to a Rust Cargo project using harvest_translate
pub fn translate_c_directory_to_rust_project(
    input_dir: &Path,
    output_dir: &Path,
    config_overrides: &[String],
    modular: bool,
) -> TranspilationResult {
    let args: Arc<harvest_translate::cli::Args> = harvest_translate::cli::Args {
        input: Some(input_dir.to_path_buf()),
        output: Some(output_dir.to_path_buf()),
        print_config_path: false,
        config: config_overrides.to_vec(),
        force: false,
        modular,
    }
    .into();
    let mut config = harvest_translate::cli::initialize(args).expect("Failed to generate config");
    if config.log_filter.is_empty() {
        config.log_filter = "off".to_owned(); // Disable console logging in harvest_translate
    }
    /*
    TODO: This isn't general anyway, only logs a single tool's parameters

    let tool_config = &config.tools.raw_source_to_cargo_llm;
    log::info!(
        "Translating code using {}:{} with max tokens: {}",
        tool_config.backend,
        tool_config.model,
        tool_config.max_tokens
    );*/
    match transpile(config.into()) {
        Ok(ir) => {
            match raw_source(&ir) {
                Ok(raw_c_source) => {
                    if let Err(e) = raw_c_source.materialize(output_dir.join("c_src")) {
                        log::warn!("Failed to materialize C source: {}", e);
                    }
                }
                Err(e) => log::warn!("Failed to retrieve raw C source from IR: {}", e),
            }
            TranspilationResult::from_ir(&ir)
        }
        Err(e) => TranspilationResult {
            translation_success: false,
            build_success: false,
            rust_binary_path: PathBuf::new(),
            build_error: Some(format!("Failed to transpile: {}", e)),
        },
    }
}

/// Run all benchmarks for a list of programs
pub fn run_all_benchmarks(
    program_dirs: &[PathBuf],
    output_dir: &Path,
    config_overrides: &[String],
    timeout: u64,
    modular: bool,
) -> HarvestResult<Vec<ProgramEvalStats>> {
    // Process all examples
    let mut results = Vec::new();
    let total_examples = program_dirs.len();

    for (i, program_dir) in program_dirs.iter().enumerate() {
        log::error!("\n{}", "=".repeat(80));
        log::info!("Processing example {} of {}", i + 1, total_examples);
        log::info!("{}", "=".repeat(80));

        let result =
            benchmark_single_program(program_dir, output_dir, config_overrides, timeout, modular);

        results.push(result);
    }

    Ok(results)
}

/// Run list of tests and output result/errors
fn run_test_validation(
    binary_path: &Path,
    test_cases: &[harvest_benchmark::harness::TestCase],
    timeout: u64,
    output_dir: &Path,
) -> (Vec<TestResult>, Vec<String>, usize) {
    let mut test_results = Vec::new();
    let mut error_messages = Vec::new();
    let mut passed_tests = 0;

    log::info!("Validating Rust binary outputs against test cases...");

    for (i, test_case) in test_cases.iter().enumerate() {
        log::info!(
            "Running test case {} ({} of {})...",
            test_case.filename,
            i + 1,
            test_cases.len()
        );

        log::info!(
            "Validating output for test case with args: {:?} stdin: {:?}",
            test_case.argv,
            test_case.stdin,
        );

        let timeout_opt = Some(timeout);
        match validate_binary_output(binary_path, test_case, timeout_opt) {
            Ok(()) => {
                passed_tests += 1;
                test_results.push(TestResult {
                    filename: test_case.filename.clone(),
                    passed: true,
                });
                log::info!("✅ Test case {} passed", test_case.filename);
            }
            Err(e) => {
                test_results.push(TestResult {
                    filename: test_case.filename.clone(),
                    passed: false,
                });
                let error = format!("Test case {} failed: {}", test_case.filename, e);
                error_messages.push(error);
                log::info!("❌ Test case {} failed: {}", test_case.filename, e);
                test_case
                    .write_to_disk(output_dir)
                    .expect("failed to write test case to disk");
            }
        }
    }

    (test_results, error_messages, passed_tests)
}

/// Run all benchmarks for a single program
fn benchmark_single_program(
    program_dir: &Path,
    output_root_dir: &Path,
    config_overrides: &[String],
    timeout: u64,
    modular: bool,
) -> ProgramEvalStats {
    let program_name = program_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let mut result = ProgramEvalStats::new(&program_name);

    log::info!("Translating program: {}", program_name);
    log::info!("Input directory: {}", program_dir.display());
    let is_lib = program_name.ends_with("_lib");
    log::info!(
        "Detected project type: {}",
        if is_lib { "library" } else { "executable" }
    );

    // Get program output directory
    let output_dir = output_root_dir.join(&program_name);
    log::info!("Output directory: {}", output_dir.display());

    // Check for required subdirectories & log error if we don't find them
    // We use the test_case root (not src/) so translate can see CMakeLists.txt.
    let (test_case_dir, test_vectors_dir) = match parse_benchmark_dir(program_dir) {
        Ok(dirs) => dirs,
        Err(e) => {
            result.error_message = Some(e.to_string());
            return result;
        }
    };

    // Parse test vectors
    let test_cases = match parse_test_vectors(test_vectors_dir) {
        Ok(vectors) => vectors,
        Err(e) => {
            result.error_message = Some(e.to_string());
            return result;
        }
    };

    result.total_tests = test_cases.len();

    // Log test case parsing success
    if !test_cases.is_empty() {
        log::info!("✅ Successfully parsed {} test case(s)", test_cases.len());
    }

    // Do the actual translation
    let translation_result = translate_c_directory_to_rust_project(
        &test_case_dir,
        &output_dir,
        config_overrides,
        modular,
    );

    result.translation_success = translation_result.translation_success;
    result.rust_build_success = translation_result.build_success;

    if translation_result.translation_success {
        log::info!("✅ Translation completed successfully!");
    } else {
        let error = format!(
            "Failed to translate C project: {:?}",
            translation_result.build_error
        );
        result.error_message = Some(error.clone());
        log::info!("❌ Translation failed");
        return result;
    }

    if translation_result.build_success {
        log::info!("✅ Rust build completed successfully!");
    } else {
        let error = format!(
            "Failed to build Rust project: {:?}",
            translation_result.build_error
        );
        result.error_message = Some(error.clone());
        log::info!("❌ Rust build failed");
        return result;
    }

    if !is_lib && !translation_result.rust_binary_path.exists() {
        let error = format!(
            "Rust build reported success, but expected output artifact was not found at {:?}",
            translation_result.rust_binary_path
        );
        log::error!("{}", error);
        result.error_message = Some(error);
        return result;
    }

    // Library and executable validation differ.
    let (test_results, error_messages, passed_tests) = if is_lib {
        match harvest_benchmark::harness::library::run_library_validation(
            &program_name,
            program_dir,
            &output_dir,
            &test_cases,
            timeout,
        ) {
            Ok(r) => r,
            Err(e) => {
                let error_msg = format!("Library validation failed: {}", e);
                log::error!("{}", error_msg);
                result.error_message = Some(error_msg);
                return result;
            }
        }
    } else {
        run_test_validation(
            &translation_result.rust_binary_path,
            &test_cases,
            timeout,
            &output_dir,
        )
    };

    result.test_results = test_results;
    result.passed_tests = passed_tests;

    // Print summary for this example
    log::info!("\nResults for {}:", program_name);
    log::info!(
        "  Translation: {}",
        status_emoji(result.translation_success)
    );
    log::info!("  Rust Build: {}", status_emoji(result.rust_build_success));
    log::info!(
        "  Tests: {}/{} passed ({:.1}%)",
        result.passed_tests,
        result.total_tests,
        result.success_rate()
    );

    // Write error messages to results.err file in the output directory if it was created
    if !error_messages.is_empty() {
        let error_file_path = output_dir.join("results.err");
        if let Err(e) = write_error_file(&error_file_path, &error_messages) {
            log::info!("Warning: Failed to write error file: {}", e);
        }
    }

    result
}

fn main() -> HarvestResult<()> {
    set_user_only_umask();
    let args = Args::parse();

    // Validate input directory exists
    validate_input_directory(&args.input_dir)?;

    // Create output directory if it doesn't exist
    ensure_output_directory(&args.output_dir)?;

    let log_file = File::create(args.output_dir.join("output.log"))?;
    TeeLogger::init(log::LevelFilter::Info, log_file)?;
    log::info!("Harvest version: {}", get_version());
    run(args)
}

fn apply_regex_filter(
    program_dirs: &mut Vec<PathBuf>,
    pattern: &str,
    keep_matches: bool,
    label: &str,
) -> HarvestResult<()> {
    let regex =
        Regex::new(pattern).map_err(|e| format!("Invalid regex pattern '{}': {}", pattern, e))?;
    let mut removed_names = Vec::new();
    program_dirs.retain(|path| {
        let matches = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| regex.is_match(name))
            .unwrap_or(false);
        let keep = if keep_matches { matches } else { !matches };
        if !keep {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                removed_names.push(name.to_string());
            }
        }
        keep
    });
    log::info!(
        "{} '{}' applied: {} programs remaining, {} removed",
        label,
        pattern,
        program_dirs.len(),
        removed_names.len(),
    );
    if !removed_names.is_empty() {
        let past_tense = match label {
            "Filter" => "Filtered",
            "Exclude" => "Excluded",
            _ => label,
        };
        log::info!("{}: {}", past_tense, removed_names.join(", "));
    }
    Ok(())
}

fn run(args: Args) -> HarvestResult<()> {
    log::info!("Running Benchmarks");
    log::info!("Input directory: {}", args.input_dir.display());
    log::info!("Output directory: {}", args.output_dir.display());
    log::info!(
        "Using {} Translation",
        if args.modular {
            "Modular"
        } else {
            "All-at-once"
        }
    );

    // Get the programs to evaluate.
    // If the input itself is a single test case root, run just that; otherwise, run children.
    let mut program_dirs = if parse_benchmark_dir(&args.input_dir).is_ok() {
        vec![args.input_dir.clone()]
    } else {
        collect_program_dirs(&args.input_dir)?
    };

    if let Some(filter_pattern) = &args.filter {
        apply_regex_filter(&mut program_dirs, filter_pattern, true, "Filter")?;
    }

    if let Some(exclude_pattern) = &args.exclude {
        apply_regex_filter(&mut program_dirs, exclude_pattern, false, "Exclude")?;
    }

    log_found_programs(&program_dirs, &args.input_dir)?;

    // Process all programs
    let results = run_all_benchmarks(
        &program_dirs,
        &args.output_dir,
        &args.config,
        args.timeout,
        args.modular,
    )?;
    let csv_output_path = args.output_dir.join("results.csv");
    write_csv_results(&csv_output_path, &results)?;

    let summary_stats = SummaryStats::from_results(&results);
    log_summary_stats(&summary_stats);

    log::info!("\nOutput Files:");
    log::info!("  Translated projects: {}", args.output_dir.display());
    log::info!("  CSV results: {}", csv_output_path.display());
    log::info!("  Error logs: results.err files in each translated project directory");

    // Print examples with issues
    log_failing_programs(&results);

    log::info!("\nProcessing complete! Check the CSV file and individual project directories for detailed results.");

    cleanup_benchmarks(&results, &args.output_dir);

    Ok(())
}

fn status_emoji(success: bool) -> &'static str {
    match success {
        true => "✅",
        false => "❌",
    }
}
