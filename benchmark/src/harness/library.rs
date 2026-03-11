//! Library test validation for MITLL TRACTOR benchmarks.
//!
//! This module handles the testing of translated Rust libraries using the cando2
//! framework. It manages:
//! - Locating compiled shared libraries (.so/.dylib/.dll)
//! - Setting up the cando2 test framework in the output directory
//! - Preparing and building test runners
//! - Executing library tests via FFI
//!
//! # Architecture
//!
//! The library testing process uses the cando2 framework, which provides a harness
//! for dynamically loading shared libraries and testing them against JSON test vectors.
//! The runner is a Rust binary that uses the `harness!` macro from cando2 to:
//! 1. Load the compiled library (.so file)
//! 2. Call exported C-ABI functions
//! 3. Compare results against expected values from test vectors

use crate::error::HarvestResult;
use crate::harness::TestCase;
use crate::stats::TestResult;
use harvest_core::cargo_utils;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

// === Constants ===

/// Standard location where cando2 expects to find Rust-compiled libraries
const RUST_ARTIFACTS_SUBDIR: &str = "translated_rust/target/release";

/// Location where cando2 framework is copied
const CANDO2_TOOLS_PATH: &str = "translated_tools/cando2";

/// Relative path from runner directory to cando2 tools
const CANDO2_RELATIVE_PATH: &str = "../translated_tools/cando2";

/// Environment variable that tells cando2 to load Rust-compiled libraries
const RUST_ARTIFACTS_ENV: &str = "RUST_ARTIFACTS";

/// Environment variable for shared library search paths
#[cfg(target_os = "macos")]
const LD_LIBRARY_PATH_ENV: &str = "DYLD_LIBRARY_PATH";
#[cfg(target_os = "windows")]
const LD_LIBRARY_PATH_ENV: &str = "PATH";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const LD_LIBRARY_PATH_ENV: &str = "LD_LIBRARY_PATH";

/// Supported shared library extensions
const LIBRARY_EXTENSIONS: &[&str] = &["so", "dylib", "dll"];

/// Subdirectory for runner build artifacts
const RUNNER_BUILD_SUBDIR: &str = "target/runner_build";

/// Validates a translated Rust library against test vectors using the cando2 runner.
///
/// This is the main entry point for library validation. It orchestrates the complete
/// testing process by:
/// 1. Preparing library-specific configuration (workspace guard, cdylib)
/// 2. Rebuilding the project as a cdylib
/// 3. Locating the compiled shared library
/// 4. Setting up the test environment (cando2, runner)
/// 5. Building and running the test runner
/// 6. Collecting and returning test results
///
/// # Arguments
/// * `program_name` - Name of the program being tested
/// * `input_dir` - Directory containing the original test case (used to locate cando2)
/// * `output_dir` - Directory containing the translated Rust project
/// * `test_cases` - Test vectors to validate against
/// * `timeout` - Timeout in seconds for each test case
///
/// # Returns
/// Tuple of (test_results, error_messages, passed_count)
pub fn run_library_validation(
    program_name: &str,
    input_dir: &Path,
    output_dir: &Path,
    test_cases: &[TestCase],
    timeout: u64,
) -> HarvestResult<(Vec<TestResult>, Vec<String>, usize)> {
    // === Library-specific preparation ===

    // Prevent cargo from attaching to the parent workspace
    cargo_utils::add_workspace_guard(&output_dir.join("Cargo.toml"))
        .map_err(|e| format!("Failed to add workspace guard: {}", e))?;

    // Copy runner directory for cando2 testing
    cargo_utils::copy_directory_recursive(&input_dir.join("runner"), &output_dir.join("runner"))
        .map_err(|e| format!("Failed to copy runner directory: {}", e))?;

    // Copy test_vectors directory
    cargo_utils::copy_directory_recursive(
        &input_dir.join("test_vectors"),
        &output_dir.join("test_vectors"),
    )
    .map_err(|e| format!("Failed to copy test_vectors directory: {}", e))?;

    // Ensure Cargo.toml has cdylib crate-type
    cargo_utils::ensure_cdylib(&output_dir.join("Cargo.toml"))
        .map_err(|e| format!("Failed to ensure cdylib: {}", e))?;

    // Rebuild to generate cdylib
    log::info!("Rebuilding project as cdylib...");
    let output = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .current_dir(output_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run cargo build: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("cargo build --release failed: {}", stderr).into());
    }
    log::info!("✅ Cdylib build completed successfully");

    // === Validation logic ===

    // Locate the compiled library
    let lib_path = locate_compiled_library(output_dir, program_name)?;
    log::info!("Located library at: {}", lib_path.display());

    // Prepare the test environment
    let ld_library_path = prepare_test_environment(input_dir, output_dir, &lib_path, program_name)?;
    log::info!("Test environment prepared");

    // Build the runner
    let runner_bin = build_runner(output_dir)?;
    log::info!("Runner binary located at: {}", runner_bin.display());

    // Run tests
    run_test_suite(&runner_bin, &ld_library_path, test_cases, timeout)
}

/// Locates the compiled shared library artifact in the target directory.
///
/// # Search Strategy
/// Simply uses the package name (with - replaced by _) to construct the library name.
/// Example: package "_001_helloworld_lib" -> "lib_001_helloworld_lib.so"
///
/// # Arguments
/// * `output_dir` - The output directory containing the translated project
/// * `program_name` - Name of the program (used as fallback)
///
/// # Returns
/// Path to the shared library file (.so, .dylib, or .dll)
pub fn locate_compiled_library(output_dir: &Path, program_name: &str) -> HarvestResult<PathBuf> {
    let pkg_name = cargo_utils::read_package_name(&output_dir.join("Cargo.toml"))
        .unwrap_or_else(|| program_name.to_string());
    let target_release = output_dir.join("target").join("release");

    // Construct expected library name from package name
    let lib_stem = format!("lib{}", pkg_name.replace('-', "_"));

    // Try common extensions
    for ext in LIBRARY_EXTENSIONS {
        let lib_path = target_release.join(format!("{}.{}", lib_stem, ext));
        if lib_path.exists() {
            return Ok(lib_path);
        }
    }

    // If not found, return error with helpful message
    Err(format!(
        "Library not found: expected {} in {}",
        lib_stem,
        target_release.display()
    )
    .into())
}

/// Prepares the test environment for library validation.
///
/// This includes:
/// - Copying the library to the standard location (translated_rust/target/release/)
/// - Setting up cando2 framework
/// - Configuring library search paths
///
/// # Why copy the library?
/// The cando2 framework expects libraries in a specific directory structure
/// when `RUST_ARTIFACTS=1` is set. This maintains compatibility with the
/// Test-Corpus conventions without modifying cando2 or all existing runners.
///
/// # Arguments
/// * `input_dir` - Directory containing the original test case (used to locate cando2)
/// * `output_dir` - Directory containing the translated Rust project
/// * `lib_path` - Path to the compiled shared library
/// * `program_name` - Name of the program being tested
///
/// # Returns
/// The LD_LIBRARY_PATH value (colon-separated directory list)
pub fn prepare_test_environment(
    input_dir: &Path,
    output_dir: &Path,
    lib_path: &Path,
    program_name: &str,
) -> HarvestResult<String> {
    // Copy library to standard location
    let rust_artifacts_dir = copy_library_to_standard_location(output_dir, lib_path, program_name)?;

    // Setup cando2 framework
    setup_cando2_framework(input_dir, output_dir)?;

    // Configure library search paths
    let ld_library_path = configure_library_paths(output_dir, &rust_artifacts_dir);

    Ok(ld_library_path)
}

/// Copies the compiled library to the standard location expected by cando2.
///
/// Target location: `output_dir/translated_rust/target/release/lib<name>.<ext>`
///
/// # Arguments
/// * `output_dir` - The output directory
/// * `lib_path` - Path to the compiled shared library
/// * `program_name` - Program name for determining the target filename
fn copy_library_to_standard_location(
    output_dir: &Path,
    lib_path: &Path,
    program_name: &str,
) -> HarvestResult<PathBuf> {
    let rust_artifacts_dir = output_dir.join(RUST_ARTIFACTS_SUBDIR);
    fs::create_dir_all(&rust_artifacts_dir)?;

    let pkg_name = cargo_utils::read_package_name(&output_dir.join("Cargo.toml"))
        .unwrap_or_else(|| program_name.to_string());
    let desired_stem = read_library_stem_hint(output_dir)
        .unwrap_or_else(|| format!("lib{}", pkg_name.replace('-', "_")));

    let lib_extension = lib_path
        .extension()
        .and_then(|ext| ext.to_str())
        .ok_or_else(|| {
            format!(
                "Selected library '{}' has no file extension; expected .so, .dylib, or .dll",
                lib_path.display()
            )
        })?;

    let dest_name = format!("{}.{}", desired_stem, lib_extension);
    let dest_path = rust_artifacts_dir.join(dest_name);
    fs::copy(lib_path, &dest_path).map_err(|e| {
        format!(
            "Failed to copy library artifact to {}: {}",
            dest_path.display(),
            e
        )
    })?;

    Ok(rust_artifacts_dir)
}

/// Sets up the cando2 test framework in the output directory.
///
/// This includes:
/// - Copying cando2 source to translated_tools/cando2/
/// - Updating runner manifests to use the local copy
/// - Adding workspace guards to prevent parent workspace interference
///
/// # Arguments
/// * `input_dir` - Directory containing the original test case (used to locate cando2)
/// * `output_dir` - Directory containing the translated Rust project
fn setup_cando2_framework(input_dir: &Path, output_dir: &Path) -> HarvestResult<()> {
    // Copy cando2 source
    copy_cando2_source(input_dir, output_dir)?;

    // Configure runner manifests
    configure_runner_manifests(output_dir)?;

    Ok(())
}

/// Copies the cando2 source code into the output directory.
///
/// # Why copy?
/// - Self-containment: Output can be moved/shared independently of Test-Corpus
/// - Version isolation: Different outputs can use different cando2 versions
/// - Portability: No dependency on Test-Corpus location
///
/// The cando2 source is searched for by walking up the directory tree from the
/// input directory (test case location) looking for `tools/cando2/` or
/// `Test-Corpus/tools/cando2/`.
///
/// # Arguments
/// * `input_dir` - Directory containing the original test case (used to locate cando2)
/// * `output_dir` - Directory containing the translated Rust project (destination)
fn copy_cando2_source(input_dir: &Path, output_dir: &Path) -> HarvestResult<()> {
    let cando_dst = output_dir.join(CANDO2_TOOLS_PATH);
    if !cando_dst.exists() {
        let cando_src = find_cando2_source(input_dir).ok_or_else(|| {
            "Unable to locate cando2 source (searched ancestors for tools/cando2)".to_string()
        })?;
        cargo_utils::copy_directory_recursive(&cando_src, &cando_dst)?;
    }
    Ok(())
}

/// Configures runner Cargo manifests for the test environment.
///
/// # Modifications
/// 1. Add workspace guard to prevent parent workspace interference
/// 2. Rewrite cando2 dependency path to use local copy
///
/// Both the main runner and fuzz runner (if present) are configured.
fn configure_runner_manifests(output_dir: &Path) -> HarvestResult<()> {
    let runner_dir = output_dir.join("runner");

    // Main runner manifest
    cargo_utils::add_workspace_guard(&runner_dir.join("Cargo.toml"))?;
    cargo_utils::update_dependency_path(
        &runner_dir.join("Cargo.toml"),
        "cando2",
        CANDO2_RELATIVE_PATH,
    )?;

    // Fuzz manifest (if exists)
    let fuzz_manifest = runner_dir.join("fuzz").join("Cargo.toml");
    if fuzz_manifest.exists() {
        cargo_utils::add_workspace_guard(&fuzz_manifest)?;
        cargo_utils::update_dependency_path(&fuzz_manifest, "cando2", CANDO2_RELATIVE_PATH)?;
    }

    Ok(())
}

/// Configures the library search path for dynamic library loading.
///
/// Returns a platform-specific string of directories to search for shared libraries.
/// Includes both the copied artifact location and the original build location as a fallback.
fn configure_library_paths(output_dir: &Path, rust_artifacts_dir: &Path) -> String {
    let runner_release_dir = output_dir.join("target").join("release");

    // Use std::env::join_paths to construct a platform-appropriate search path,
    // falling back to the previous behavior if joining fails for any reason.
    let joined = std::env::join_paths([
        rust_artifacts_dir.as_os_str(),
        runner_release_dir.as_os_str(),
    ])
    .unwrap_or_else(|_| {
        std::ffi::OsString::from(format!(
            "{}:{}",
            rust_artifacts_dir.display(),
            runner_release_dir.display()
        ))
    });

    joined.to_string_lossy().into_owned()
}

/// Builds the test runner binary.
///
/// The runner is compiled to a separate target directory to avoid conflicts with
/// the main project build.
///
/// # Returns
/// Path to the compiled runner executable
pub fn build_runner(output_dir: &Path) -> HarvestResult<PathBuf> {
    let runner_dir = output_dir.join("runner");
    let runner_target_dir = output_dir.join(RUNNER_BUILD_SUBDIR);

    fs::create_dir_all(&runner_target_dir)?;

    // Use absolute path for target-dir to avoid path confusion
    let runner_target_dir_abs = runner_target_dir
        .canonicalize()
        .unwrap_or(runner_target_dir.clone());

    // Build command
    log::info!("Building test runner...");
    let output = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--target-dir")
        .arg(&runner_target_dir_abs)
        .current_dir(&runner_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to build runner in {}: {}", runner_dir.display(), e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Runner build failed: {}", stderr).into());
    }

    // Locate compiled binary
    locate_runner_binary(&runner_dir, &runner_target_dir)
}

/// Locates the compiled runner binary by name.
///
/// Tries to find the binary using the package name from Cargo.toml, falling back to
/// the generic name "runner" if the package name is not found.
fn locate_runner_binary(runner_dir: &Path, runner_target_dir: &Path) -> HarvestResult<PathBuf> {
    let pkg_name = cargo_utils::read_package_name(&runner_dir.join("Cargo.toml"));
    let runner_release = runner_target_dir.join("release");

    let runner_bin = if let Some(name) = pkg_name {
        let named = runner_release.join(&name);
        if named.exists() {
            named
        } else {
            runner_release.join("runner")
        }
    } else {
        runner_release.join("runner")
    };

    if !runner_bin.exists() {
        // List what files are actually in the directory for debugging
        if let Ok(entries) = std::fs::read_dir(&runner_release) {
            let files: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            return Err(format!(
                "Runner binary not found at {}\nFiles in directory: {:?}",
                runner_bin.display(),
                files
            )
            .into());
        }
        return Err(format!("Runner binary not found at {}", runner_bin.display()).into());
    }

    // Convert to absolute path to avoid issues with current_dir changes
    let runner_bin_abs = runner_bin
        .canonicalize()
        .unwrap_or(runner_bin.to_path_buf());

    Ok(runner_bin_abs)
}

/// Runs the complete test suite for a library.
///
/// # Process
/// For each test case:
/// 1. Spawn runner with: `runner lib -c <test_case.json>`
/// 2. Set environment: RUST_ARTIFACTS=1, LD_LIBRARY_PATH
/// 3. Apply timeout
/// 4. Validate exit code (success = test passed)
///
/// # Arguments
/// * `runner_bin` - Path to the compiled runner executable
/// * `ld_library_path` - Colon-separated list of directories for LD_LIBRARY_PATH
/// * `test_cases` - Test vectors to run
/// * `timeout` - Timeout in seconds for each test
///
/// # Returns
/// Tuple of (test_results, error_messages, passed_count)
pub fn run_test_suite(
    runner_bin: &Path,
    ld_library_path: &str,
    test_cases: &[TestCase],
    timeout: u64,
) -> HarvestResult<(Vec<TestResult>, Vec<String>, usize)> {
    let mut test_results = Vec::new();
    let mut error_messages = Vec::new();
    let mut passed_tests = 0;
    let timeout_duration = Duration::from_secs(timeout);

    log::info!("Validating library outputs against test cases...");

    for (i, test_case) in test_cases.iter().enumerate() {
        log::info!(
            "Running library test case {} ({} of {})...",
            test_case.filename,
            i + 1,
            test_cases.len()
        );

        let result =
            run_single_library_test(runner_bin, test_case, ld_library_path, timeout_duration);

        match result {
            Ok(output) if output.status.success() => {
                passed_tests += 1;
                test_results.push(TestResult {
                    filename: test_case.filename.clone(),
                    passed: true,
                });
                log::info!("✅ Test case {} passed", test_case.filename);
            }
            Ok(output) => {
                test_results.push(TestResult {
                    filename: test_case.filename.clone(),
                    passed: false,
                });
                let error = format_test_failure(&test_case.filename, &output);
                error_messages.push(error.clone());
                log::info!("❌ Test case {} failed: {}", test_case.filename, error);
            }
            Err(e) => {
                // Treat runner errors as failed tests rather than aborting the entire suite
                test_results.push(TestResult {
                    filename: test_case.filename.clone(),
                    passed: false,
                });
                let error = format!("Test case {} failed: {}", test_case.filename, e);
                error_messages.push(error.clone());
                log::info!("❌ {}", error);
            }
        }
    }

    Ok((test_results, error_messages, passed_tests))
}

/// Runs a single library test case.
///
/// Spawns the runner process with appropriate environment variables and applies a timeout.
/// The runner is invoked with: `runner lib -c <test_case.json>`
///
/// # Environment Variables
/// - `RUST_ARTIFACTS=1`: Tells cando2 to load Rust-compiled libraries
/// - Library search path variable (`LD_LIBRARY_PATH`/`DYLD_LIBRARY_PATH`/`PATH` depending on platform)
fn run_single_library_test(
    runner_bin: &Path,
    test_case: &TestCase,
    ld_library_path: &str,
    timeout: Duration,
) -> HarvestResult<Output> {
    let runner_dir = runner_bin.parent().ok_or_else(|| {
        format!(
            "Runner binary path has no parent directory: {}",
            runner_bin.display()
        )
    })?;

    let mut cmd = Command::new(runner_bin);
    cmd.arg("lib")
        .arg("-c")
        .arg(&test_case.filename)
        .current_dir(runner_dir)
        .env(RUST_ARTIFACTS_ENV, "1")
        .env(LD_LIBRARY_PATH_ENV, ld_library_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn runner for {}: {}", test_case.filename, e))?;

    // Apply timeout
    match child.wait_timeout(timeout) {
        Ok(Some(_)) => child
            .wait_with_output()
            .map_err(|e| format!("Failed to read runner output: {}", e).into()),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("Runner timed out after {} seconds", timeout.as_secs()).into())
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("Error waiting for runner: {}", e).into())
        }
    }
}

/// Formats a test failure message with stdout/stderr from the runner.
fn format_test_failure(filename: &str, output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!(
        "Runner failed for {}: status {:?}\nstdout:\n{}\nstderr:\n{}",
        filename,
        output.status.code(),
        stdout,
        stderr
    )
}

/// Finds the cando2 source directory by walking up ancestors from `start` and looking for
/// `tools/cando2/Cargo.toml` or `Test-Corpus/tools/cando2/Cargo.toml`.
///
/// # Search Strategy
/// Searches two possible locations at each ancestor level:
/// - `tools/cando2/` (harvest repo structure)
/// - `Test-Corpus/tools/cando2/` (Test-Corpus as submodule)
///
/// # Arguments
/// * `start` - Starting directory for the search (typically the input test case directory)
///
/// # Returns
/// Path to the cando2 directory if found, `None` otherwise
fn find_cando2_source(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let search_paths = [
            ancestor.join("tools").join("cando2"),
            ancestor.join("Test-Corpus").join("tools").join("cando2"),
        ];
        for path in search_paths {
            if path.join("Cargo.toml").exists() {
                return Some(path);
            }
        }
    }
    None
}

/// Parses runner/src/main.rs to extract the library name hint.
///
/// Looks for: `library: "hello",` in the harness! macro invocation.
/// Returns: `Some("libhello")` or `None`
///
/// # Note
/// This is a simple text-based parse, not a full AST parse. It assumes the runner
/// follows standard cando2 conventions. Using a full parser (like syn) would add
/// significant compile-time overhead for a rarely-needed feature.
///
/// # Returns
/// Library stem with "lib" prefix if found (e.g., "libhello"), or `None`
fn read_library_stem_hint(output_dir: &Path) -> Option<String> {
    let runner_main = output_dir.join("runner").join("src").join("main.rs");
    let contents = fs::read_to_string(&runner_main).ok()?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("library:") {
            if let Some((_, val)) = rest.split_once('"') {
                let name = val.split('"').next().unwrap_or("").trim();
                if !name.is_empty() {
                    return Some(format!("lib{}", name));
                }
            }
        }
    }
    None
}

/// Build all lib runners in one cargo command from the workspace root.
pub fn build_runners_batch(workspace_root: &Path, runner_names: &[String]) -> HarvestResult<()> {
    if runner_names.is_empty() {
        return Ok(());
    }
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build").arg("--release");
    for name in runner_names {
        cmd.arg("-p").arg(name);
    }
    let output = cmd
        .current_dir(workspace_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("cargo build runners failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Runner batch build failed:\n{}", stderr).into());
    }
    Ok(())
}

/// Run lib tests using a pre-built runner from the workspace target dir.
/// The runner binary is at `workspace_root/target/release/<runner_name>`.
/// Sets RUST_ARTIFACTS="" so cando2 loads from translated_rust/target/release/.
pub fn run_lib_tests(
    workspace_root: &Path,
    runner_name: &str,
    test_cases: &[TestCase],
    timeout: u64,
) -> HarvestResult<(Vec<crate::stats::TestResult>, Vec<String>, usize)> {
    let runner_bin = workspace_root.join("target").join("release").join(runner_name);
    if !runner_bin.exists() {
        return Err(format!("Runner binary not found: {}", runner_bin.display()).into());
    }

    let mut test_results = Vec::new();
    let mut error_messages = Vec::new();
    let mut passed = 0;
    let timeout_duration = Duration::from_secs(timeout);

    for test_case in test_cases {
        let mut cmd = Command::new(&runner_bin);
        cmd.arg("lib")
            .arg("-c")
            .arg(&test_case.filename)
            .current_dir(workspace_root)
            .env(RUST_ARTIFACTS_ENV, "")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let result = match cmd.spawn() {
            Ok(mut child) => {
                use wait_timeout::ChildExt;
                match child.wait_timeout(timeout_duration) {
                    Ok(Some(_)) => child.wait_with_output().ok(),
                    _ => { let _ = child.kill(); let _ = child.wait(); None }
                }
            }
            Err(_) => None,
        };

        match result {
            Some(output) if output.status.success() => {
                passed += 1;
                test_results.push(crate::stats::TestResult { filename: test_case.filename.clone(), passed: true });
            }
            Some(output) => {
                let error = format_test_failure(&test_case.filename, &output);
                error_messages.push(error);
                test_results.push(crate::stats::TestResult { filename: test_case.filename.clone(), passed: false });
            }
            None => {
                let error = format!("{}: runner failed or timed out", test_case.filename);
                error_messages.push(error);
                test_results.push(crate::stats::TestResult { filename: test_case.filename.clone(), passed: false });
            }
        }
    }
    Ok((test_results, error_messages, passed))
}

/// Prepare lib cases for batch testing: copy cando2 once, copy runners, generate workspace.
/// Returns the list of runner package names for batch building.
pub fn prepare_lib_workspace(
    corpus_dir: &Path,
    results_dir: &Path,
    lib_case_names: &[String],
) -> HarvestResult<Vec<String>> {
    // 1. Find and copy cando2 once to results_dir/tools/cando2/
    let cando2_dst = results_dir.join("tools").join("cando2");
    if !cando2_dst.exists() {
        let cando2_src = find_cando2_source(corpus_dir)
            .ok_or("Cannot find tools/cando2 in corpus ancestors")?;
        cargo_utils::copy_directory_recursive(&cando2_src, &cando2_dst)?;
    }

    let mut runner_names = Vec::new();

    for name in lib_case_names {
        let corpus_case = corpus_dir.join(name);
        let results_case = results_dir.join(name);
        let runner_dst = results_case.join("runner");

        // 2. Copy fresh runner from corpus (overwrites any HARVEST-modified version)
        if corpus_case.join("runner").exists() {
            if runner_dst.exists() { let _ = fs::remove_dir_all(&runner_dst); }
            cargo_utils::copy_directory_recursive(&corpus_case.join("runner"), &runner_dst)?;
        }

        // 3. Rewrite cando2 path to shared copy
        let runner_toml = runner_dst.join("Cargo.toml");
        if runner_toml.exists() {
            cargo_utils::update_dependency_path(&runner_toml, "cando2", "../../tools/cando2")?;
        }

        // 4. Copy test_vectors from corpus if not present
        let tv_dst = results_case.join("test_vectors");
        if !tv_dst.exists() {
            let tv_src = corpus_case.join("test_vectors");
            if tv_src.exists() {
                cargo_utils::copy_directory_recursive(&tv_src, &tv_dst)?;
            }
        }

        // 5. Copy .so to where cando2 expects it
        // cando2 looks at: <case>/translated_rust/target/release/lib<name>.so
        // where <name> = CANDIDATE_NAME = case directory name (e.g. bin2hex_lib)
        let so_name = format!("lib{}.so", name);
        let expected_dir = results_case.join("translated_rust").join("target").join("release");
        let expected_so = expected_dir.join(&so_name);

        if !expected_so.exists() {
            // Find the actual .so in target/release/
            let build_release = results_case.join("target").join("release");
            if let Ok(entries) = fs::read_dir(&build_release) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().map_or(false, |e| e == "so" || e == "dylib") {
                        fs::create_dir_all(&expected_dir)?;
                        fs::copy(&p, &expected_so)?;
                        break;
                    }
                }
            }
        }

        let runner_name = format!("_{}_runner", name);
        runner_names.push(runner_name);
    }

    // 6. Generate workspace Cargo.toml
    let ws_toml = results_dir.join("Cargo.toml");
    let mut members = String::from("[workspace]\nresolver = \"2\"\nmembers = [\n");
    for name in lib_case_names {
        members.push_str(&format!("    \"{}/runner\",\n", name));
    }
    members.push_str("]\n");
    fs::write(&ws_toml, members)?;

    Ok(runner_names)
}
