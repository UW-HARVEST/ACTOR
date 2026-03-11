/// This module `harness` is intended to contain code that is specific to a particular set of benchmarks,
/// for example, parsing code for benchmark-specific configs.
/// Currently, that is just the MITLL tractor benchmarks.
pub mod library;

use crate::runner;
use crate::stats::ProgramEvalStats;
use crate::HarvestResult;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Represents the expected stdout pattern in a test case
/// Used for tractor test cases
#[derive(Default, Debug, Serialize, Deserialize, Clone)]
pub struct StdoutPattern {
    pub pattern: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[serde(default)]
    pub is_regex: bool,
}

/// Represents a test case with command arguments, input, and expected output
/// Used for tractor test cases
#[derive(Default, Debug, Serialize, Deserialize, Clone)]
pub struct TestCase {
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub stdout: StdoutPattern,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub rc: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub has_ub: Option<String>,
    #[serde(skip)] // Don't serialize/deserialize this field as it's not part of the JSON
    pub filename: String,
}

impl TestCase {
    /// Writes the TestCase to a JSON file on disk
    /// Creates the directory structure {output_dir}/failed_tests/ and
    /// writes to {output_dir}/failed_tests/{filename}
    pub fn write_to_disk(&self, output_dir: &Path) -> HarvestResult<()> {
        let failed_tests_dir = output_dir.join("failed_tests");

        // Create the failed_tests directory if it doesn't exist
        fs::create_dir_all(&failed_tests_dir).map_err(|e| {
            format!(
                "Failed to create directory {}: {}",
                failed_tests_dir.display(),
                e
            )
        })?;

        let file_path = failed_tests_dir.join(&self.filename);

        log::info!("Saving failed test case to {}", file_path.display());

        let mut json_str = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize TestCase to JSON: {}", e))?;
        json_str.push('\n'); // To be consistent with original test vector styling

        fs::write(&file_path, json_str).map_err(|e| {
            format!(
                "Failed to write TestCase to file {}: {}",
                file_path.display(),
                e
            )
        })?;

        Ok(())
    }
}

/// Parses a JSON file into a TestCase struct
pub fn parse_test_case_json<P: AsRef<Path>>(file_path: P) -> HarvestResult<TestCase> {
    let file_path = file_path.as_ref();

    // Read the JSON content from the file
    let json_str = fs::read_to_string(file_path).map_err(|e| {
        format!(
            "Failed to read test case file {}: {}",
            file_path.display(),
            e
        )
    })?;

    let mut test_case: TestCase = serde_json::from_str(&json_str).map_err(|e| {
        format!(
            "Failed to parse test case JSON from {}: {}",
            file_path.display(),
            e
        )
    })?;

    // Set the filename field to the file name
    test_case.filename = file_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string();

    Ok(test_case)
}

/// Validate that required benchmark subdirectories exist.
/// Returns (test_case_dir, test_vectors_dir). Note: we return the `test_case`
/// root (not `test_case/src`) so downstream tools can see CMakeLists.txt if
/// present.
pub fn parse_benchmark_dir(input_dir: &Path) -> HarvestResult<(PathBuf, PathBuf)> {
    if !input_dir.exists() {
        return Err(format!("Input directory does not exist: {}", input_dir.display()).into());
    }
    if !input_dir.is_dir() {
        return Err(format!("Input path is not a directory: {}", input_dir.display()).into());
    }

    // Check for required subdirectories
    let test_case_dir = input_dir.join("test_case");
    let test_case_src_dir = test_case_dir.join("src");
    let test_vectors_dir = input_dir.join("test_vectors");

    if !test_case_dir.exists() || !test_case_dir.is_dir() {
        return Err(format!(
            "Required test_case directory not found: {}",
            test_case_dir.display()
        )
        .into());
    }

    if !test_case_src_dir.exists() || !test_case_src_dir.is_dir() {
        return Err(format!(
            "Required test_case/src directory not found: {}",
            test_case_src_dir.display()
        )
        .into());
    }

    if !test_vectors_dir.exists() || !test_vectors_dir.is_dir() {
        return Err(format!(
            "Required test_vectors directory not found: {}",
            test_vectors_dir.display()
        )
        .into());
    }

    // Return the test_case root so downstream tools can find metadata files
    // (e.g., CMakeLists.txt) while still containing the src/ subtree.
    Ok((test_case_dir, test_vectors_dir))
}

/// Reads all files in a directory and parses them as TestCase JSON files
/// These sorts of files can be found in the test_vectors/ directory of the benchmark
pub fn parse_test_vectors<P: AsRef<Path>>(directory_path: P) -> HarvestResult<Vec<TestCase>> {
    let dir_path = directory_path.as_ref();

    // Read directory entries
    let entries = fs::read_dir(dir_path)
        .map_err(|e| format!("Failed to read directory {}: {}", dir_path.display(), e))?;

    // Process each file and collect successful test cases
    let mut test_cases = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!(
                "Failed to read directory entry in {}: {}",
                dir_path.display(),
                e
            )
        })?;
        let file_path = entry.path();

        // Only accept plain .json files (exclude backups like .json.bak, directories, etc.)
        if file_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext != "json")
            .unwrap_or(true)
        {
            continue;
        }

        // Try to parse the file as a test case JSON
        if let Ok(test_case) = parse_test_case_json(&file_path) {
            test_cases.push(test_case);
        }
    }
    Ok(test_cases)
}

/// Clean up build artifacts from successfully translated Rust projects
pub fn cleanup_benchmarks(results: &[ProgramEvalStats], output_dir: &Path) {
    let mut cleaned_count = 0;
    let mut cleanup_errors = Vec::new();

    for result in results {
        if result.translation_success {
            let project_dir = output_dir.join(&result.program_name);

            // Check if Cargo.toml exists to confirm it's a Rust project
            if project_dir.join("Cargo.toml").exists() {
                match Command::new("cargo")
                    .arg("clean")
                    .current_dir(&project_dir)
                    .output()
                {
                    Ok(output) if output.status.success() => {
                        cleaned_count += 1;
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let error_msg =
                            format!("Failed to clean {}: {}", result.program_name, stderr);
                        cleanup_errors.push(error_msg.clone());
                        log::info!("  ❌ {}", error_msg);
                    }
                    Err(e) => {
                        let error_msg = format!(
                            "Failed to execute cargo clean for {}: {}",
                            result.program_name, e
                        );
                        cleanup_errors.push(error_msg.clone());
                        log::info!("  ❌ {}", error_msg);
                    }
                }
            } else {
                log::info!("Skipping {}: No Cargo.toml found", result.program_name);
            }
        }
    }

    log::info!("\nCleanup Summary:");
    log::info!("  Successfully cleaned: {} projects", cleaned_count);
    if !cleanup_errors.is_empty() {
        log::info!("  Cleanup errors: {} projects", cleanup_errors.len());
        for error in &cleanup_errors {
            log::info!("    - {}", error);
        }
    }

    log::info!("\nDone!");
}

// TODO: need to skip test cases that have UB flag set.

/// Runs a binary with test case inputs and compares its output against expected values.
///
/// This function executes the binary with the command line arguments and stdin
/// from the provided test case, then compares the actual stdout and stderr
/// against the expected values in the test case.
pub fn validate_binary_output(
    binary_path: &Path,
    test_case: &TestCase,
    timeout_seconds: Option<u64>,
) -> HarvestResult<()> {
    let timeout = Duration::from_secs(timeout_seconds.unwrap_or(10));

    // Run the binary
    let output = runner::run_binary_with_timeout(binary_path, test_case, timeout)
        .map_err(|e| format!("Failed to run binary {}: {}", binary_path.display(), e))?;

    let actual_stdout = String::from_utf8_lossy(&output.stdout);
    let actual_stderr = String::from_utf8_lossy(&output.stderr);

    // Compare stdout against expected pattern
    let matches = if test_case.stdout.is_regex {
        // Use regex matching
        let regex = Regex::new(&test_case.stdout.pattern).map_err(|e| {
            format!(
                "Invalid regex pattern '{}': {}",
                test_case.stdout.pattern, e
            )
        })?;
        regex.is_match(actual_stdout.trim())
    } else {
        // Use simple equality matching
        actual_stdout.trim() == test_case.stdout.pattern.trim()
    };

    if matches {
        Ok(())
    } else {
        let pattern_type = if test_case.stdout.is_regex {
            "regex pattern"
        } else {
            "expected stdout"
        };
        Err(format!(
            "❌ Binary produced unexpected output:\n\n{}: {}\n\nActual stdout:\n{}\n\nActual stderr:\n{}",
            pattern_type.chars().next().unwrap().to_uppercase().collect::<String>() + &pattern_type[1..],
            test_case.stdout.pattern,
            actual_stdout,
            actual_stderr
        ).into())
    }
}

/// Run all exec test vectors against a built binary.
/// Returns (test_results, error_messages, passed_count).
pub fn run_exec_tests(
    binary_path: &Path,
    test_cases: &[TestCase],
    timeout: u64,
) -> (Vec<crate::stats::TestResult>, Vec<String>, usize) {
    let mut test_results = Vec::new();
    let mut error_messages = Vec::new();
    let mut passed = 0;

    for test_case in test_cases {
        match validate_binary_output(binary_path, test_case, Some(timeout)) {
            Ok(()) => {
                passed += 1;
                test_results.push(crate::stats::TestResult { filename: test_case.filename.clone(), passed: true });
            }
            Err(e) => {
                let error = format!("{}: {}", test_case.filename, e);
                error_messages.push(error);
                test_results.push(crate::stats::TestResult { filename: test_case.filename.clone(), passed: false });
            }
        }
    }
    (test_results, error_messages, passed)
}

/// Build a case and discover the binary path from cargo JSON output.
pub fn discover_binary(project_dir: &Path) -> HarvestResult<PathBuf> {
    let output = std::process::Command::new("cargo")
        .args(["build", "--release", "--message-format=json"])
        .current_dir(project_dir)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("cargo build failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("cargo build failed:\n{}", stderr).into());
    }

    // Parse the last compiler-artifact message with an executable
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().rev() {
        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) {
            if msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-artifact") {
                if let Some(exe) = msg.get("executable").and_then(|e| e.as_str()) {
                    return Ok(PathBuf::from(exe));
                }
            }
        }
    }
    Err("No executable found in cargo build output".into())
}
