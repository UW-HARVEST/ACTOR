use crate::battery::Paths;
use crate::translate::copy_dir_all;
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── Types ──────────────────────────────────────────────────────────────

/// How the test subcommand should behave after running tests.
#[derive(Debug, Clone, Copy)]
pub enum TestMode {
    /// Just run and print results.
    Run,
    /// Run, then write summary.json / result.json.
    Update,
    /// Run, then compare against stored summary.json. Returns failure on mismatch.
    Check,
}

/// Outcome of running tests for one or more batteries.
#[derive(Debug)]
pub enum TestOutcome {
    /// All batteries matched their stored summaries (--check).
    Passed,
    /// At least one battery mismatched (--check).
    Failed(Vec<BatteryMismatch>),
    /// Summaries were written (--update) or just printed (run).
    Ok,
}

#[derive(Debug)]
pub struct BatteryMismatch {
    pub battery: String,
    pub diffs: Vec<String>,
}

/// Parsed runtests output for a single battery.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Summary {
    pub cases_tested: usize,
    pub cases_passed: usize,
    pub vectors_passed: usize,
    pub vectors_failed: usize,
    pub vectors_skipped: usize,
    pub failed_cases: Vec<String>,
}

/// RAII guard that removes test_vectors/ and runner/ from result dirs on drop.
struct TestArtifactGuard {
    output_dir: PathBuf,
}

impl Drop for TestArtifactGuard {
    fn drop(&mut self) {
        let _ = cleanup_test_artifacts(&self.output_dir);
    }
}

// ── Public API ─────────────────────────────────────────────────────────

/// Entry point: run tests for one battery or all batteries.
pub fn run_test_corpus(paths: &Paths, target: &str, mode: TestMode) -> Result<TestOutcome> {
    let batteries = if target == "all" {
        discover_batteries(&paths.results_dir)?
    } else {
        vec![target.to_string()]
    };

    let mut all_mismatches = Vec::new();
    let mut check_rows: Vec<CheckRow> = Vec::new();

    for battery in &batteries {
        let result = run_battery(&paths, battery, mode, &mut check_rows)?;
        if let TestOutcome::Failed(ref mm) = result {
            all_mismatches.extend(mm.iter().map(|m| BatteryMismatch {
                battery: m.battery.clone(),
                diffs: m.diffs.clone(),
            }));
        }
    }

    // Print recap table for --check mode
    if matches!(mode, TestMode::Check) && !check_rows.is_empty() {
        println!();
        println!("========================================");
        println!("  Check Summary");
        println!("========================================");
        println!("  {:<25} {:>15} {:>15}  {}", "Battery", "Stored", "Actual", "Status");
        println!("  {}", "─".repeat(75));
        for row in &check_rows {
            let stored = format!("{}/{} ({}v)", row.expected.cases_passed, row.expected.cases_tested,
                row.expected.vectors_passed);
            let actual = format!("{}/{} ({}v)", row.actual.cases_passed, row.actual.cases_tested,
                row.actual.vectors_passed);
            let status = if row.ok { "✅" } else { "❌" };
            println!("  {:<25} {:>15} {:>15}  {}", row.battery, stored, actual, status);
        }
        println!("========================================");
    }

    match mode {
        TestMode::Check if !all_mismatches.is_empty() => Ok(TestOutcome::Failed(all_mismatches)),
        TestMode::Check => Ok(TestOutcome::Passed),
        _ => Ok(TestOutcome::Ok),
    }
}

struct CheckRow {
    battery: String,
    expected: Summary,
    actual: Summary,
    ok: bool,
}

// ── CRUST-bench testing ────────────────────────────────────────────────

/// Per-project test result — strongly typed, not loose JSON.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct CrustTestResult {
    tests_ok: usize,
    tests_failed: usize,
    build_ok: bool,
}

/// Aggregated CRUST results keyed by project name.
#[derive(Debug, Serialize, Deserialize)]
struct CrustBaseline(std::collections::BTreeMap<String, CrustTestResult>);

/// A single regression found during --check.
#[derive(Debug)]
struct Regression {
    project: String,
    field: &'static str,
    expected: String,
    actual: String,
}

/// Pure function: compare baseline vs actual, return regressions.
fn find_regressions(expected: &CrustBaseline, actual: &CrustBaseline) -> Vec<Regression> {
    let mut regressions = Vec::new();
    for (name, exp) in &expected.0 {
        match actual.0.get(name) {
            None => regressions.push(Regression {
                project: name.clone(), field: "missing",
                expected: "present".into(), actual: "not found".into(),
            }),
            Some(act) => {
                if act.tests_ok < exp.tests_ok {
                    regressions.push(Regression {
                        project: name.clone(), field: "tests_ok",
                        expected: exp.tests_ok.to_string(), actual: act.tests_ok.to_string(),
                    });
                }
                if act.tests_failed > exp.tests_failed {
                    regressions.push(Regression {
                        project: name.clone(), field: "tests_failed",
                        expected: exp.tests_failed.to_string(), actual: act.tests_failed.to_string(),
                    });
                }
                if exp.build_ok && !act.build_ok {
                    regressions.push(Regression {
                        project: name.clone(), field: "build_ok",
                        expected: "true".into(), actual: "false".into(),
                    });
                }
            }
        }
    }
    regressions
}

/// Run cargo test on a single CRUST project, return typed result.
fn test_one_crust(proj_dir: &Path) -> Result<CrustTestResult> {
    // Clean up test artifacts and shared temp dirs (some CRUST tests use ./tmp)
    for artifact in [".vsync", "tmp"] {
        let p = proj_dir.join(artifact);
        if p.exists() { let _ = std::fs::remove_dir_all(&p); }
    }

    let output = Command::new("timeout")
        .args(["60", "cargo", "test", "--", "--test-threads=1"])
        .current_dir(proj_dir)
        .output()
        .with_context(|| format!("running cargo test in {}", proj_dir.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let (tests_ok, tests_failed) = parse_cargo_test_results(&stdout);
    let build_ok = !stderr.contains("error[") && !stderr.contains("could not compile")
        && !stderr.contains("failed to run custom build command");

    // If build failed or no tests ran, re-run with --verbose for full diagnostics
    let (final_stdout, final_stderr) = if !build_ok || (tests_ok == 0 && tests_failed == 0) {
        let verbose = Command::new("timeout")
            .args(["60", "cargo", "test", "--verbose"])
            .current_dir(proj_dir)
            .output()
            .ok();
        if let Some(v) = verbose {
            (String::from_utf8_lossy(&v.stdout).into_owned(),
             String::from_utf8_lossy(&v.stderr).into_owned())
        } else {
            (stdout.into_owned(), stderr.into_owned())
        }
    } else {
        (stdout.into_owned(), stderr.into_owned())
    };

    let logs_dir = proj_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    std::fs::write(logs_dir.join("test.log"), format!("{final_stdout}\n{final_stderr}"))?;

    // Print diagnostic snippet when something went wrong
    if !build_ok || tests_failed > 0 || (tests_ok == 0 && tests_failed == 0) {
        let err_lines: Vec<&str> = final_stderr.lines()
            .filter(|l| l.contains("error") || l.contains("FAILED") || l.contains("cannot find")
                || l.contains("linking") || l.contains("Could not find") || l.contains("run custom build"))
            .take(10)
            .collect();
        if !err_lines.is_empty() {
            for line in &err_lines {
                eprintln!("    │ {line}");
            }
        }
    }

    Ok(CrustTestResult { tests_ok, tests_failed, build_ok })
}

/// Parse `test result: ok. N passed; M failed; ...` lines from cargo test stdout.
/// Deterministic regardless of output interleaving.
fn parse_cargo_test_results(stdout: &str) -> (usize, usize) {
    let re = Regex::new(r"test result: \S+\. (\d+) passed; (\d+) failed;").unwrap();
    let (mut ok, mut failed) = (0usize, 0usize);
    for caps in re.captures_iter(stdout) {
        ok += caps[1].parse::<usize>().unwrap_or(0);
        failed += caps[2].parse::<usize>().unwrap_or(0);
    }
    (ok, failed)
}

/// Load per-project result.json files into a baseline (for CI --check without re-running tests).
fn load_stored_results(paths: &Paths) -> Result<CrustBaseline> {
    let mut results = std::collections::BTreeMap::new();
    if !paths.results_dir.is_dir() { return Ok(CrustBaseline(results)); }
    for entry in std::fs::read_dir(&paths.results_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let result_path = entry.path().join("result.json");
        if result_path.exists() {
            let data = std::fs::read_to_string(&result_path)?;
            if let Ok(r) = serde_json::from_str::<CrustTestResult>(&data) {
                results.insert(entry.file_name().to_string_lossy().into_owned(), r);
            }
        }
    }
    Ok(CrustBaseline(results))
}

/// Stored blind CRUST result with both LLM and real test fields.
#[derive(Debug, Deserialize)]
struct BlindCrustStored {
    #[serde(default)]
    real_tests_ok: usize,
    #[serde(default)]
    real_tests_failed: usize,
    #[serde(default)]
    flaky: bool,
}

fn load_blind_stored_results(paths: &Paths) -> Result<std::collections::BTreeMap<String, BlindCrustStored>> {
    let mut map = std::collections::BTreeMap::new();
    if !paths.results_dir.is_dir() { return Ok(map); }
    for entry in std::fs::read_dir(&paths.results_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let rj = entry.path().join("verify/result.json");
        if rj.exists() {
            let data = std::fs::read_to_string(&rj)?;
            if let Ok(r) = serde_json::from_str::<BlindCrustStored>(&data) {
                map.insert(entry.file_name().to_string_lossy().into_owned(), r);
            }
        }
    }
    Ok(map)
}

pub fn run_crust_test(paths: &Paths, projects: &[crate::battery::CrustProject], mode: TestMode) -> Result<TestOutcome> {
    // Load stored result.json files as the baseline (single source of truth).
    let stored = load_stored_results(paths)?;

    let mut results = CrustBaseline(std::collections::BTreeMap::new());
    let mut passed = 0usize;
    let mut build_failed = 0usize;

    for project in projects {
        let name = project.name();
        let proj_dir = paths.output_dir(name);
        if !proj_dir.join("Cargo.toml").exists() { continue; }

        let r = test_one_crust(&proj_dir)?;

        if !r.build_ok {
            build_failed += 1;
            println!("  ❌ {name}: build failed");
        } else if r.tests_failed > 0 {
            println!("  ⚠️  {name}: {} ok, {} FAILED", r.tests_ok, r.tests_failed);
        } else if r.tests_ok > 0 {
            passed += 1;
            println!("  ✅ {name}: {} ok", r.tests_ok);
        } else {
            println!("  ⚠️  {name}: no tests ran");
        }

        // --update: write result.json immediately
        if matches!(mode, TestMode::Update) {
            let mut json = serde_json::to_value(&r)?;
            if let Some(m) = crate::battery::extract_agent_meta(&proj_dir.join("logs/translation.log")) {
                json["agent"] = serde_json::to_value(&m).unwrap();
            }
            json["unsafe"] = serde_json::to_value(&crate::battery::count_unsafe(&proj_dir.join("src"))).unwrap();
            std::fs::write(proj_dir.join("result.json"), serde_json::to_string_pretty(&json)? + "\n")?;
        }

        results.0.insert(name.to_string(), r);
    }

    let total = results.0.len();
    println!("\nCRUST: {passed}/{total} projects pass ({build_failed} build failures)");

    match mode {
        TestMode::Update => {
            println!("📝 result.json written for {total} projects");
            Ok(TestOutcome::Ok)
        }
        TestMode::Check => {
            // If no tests ran (CI without translated code), nothing to regress against.
            if results.0.is_empty() {
                println!("✅ No translated projects found — nothing to check");
                return Ok(TestOutcome::Passed);
            }
            let regressions = find_regressions(&stored, &results);
            // Check credits + unsafe
            let mut enrich_diffs = Vec::new();
            for name in results.0.keys() {
                let proj_dir = paths.output_dir(name);
                let tlog = proj_dir.join("logs/translation.log");
                for d in check_enrichment(&proj_dir.join("result.json"), &proj_dir.join("src"), &[("agent", &tlog)], paths.agent) {
                    enrich_diffs.push(format!("{name}: {d}"));
                }
            }
            if regressions.is_empty() && enrich_diffs.is_empty() {
                println!("✅ No regressions");
                Ok(TestOutcome::Passed)
            } else {
                let total = regressions.len() + enrich_diffs.len();
                println!("\n❌ {} regression(s):", total);
                for r in &regressions {
                    println!("  {}: {} expected={} actual={}", r.project, r.field, r.expected, r.actual);
                    // Dump test log for regression diagnosis
                    let log_path = paths.output_dir(&r.project).join("logs/test.log");
                    if let Ok(log) = std::fs::read_to_string(&log_path) {
                        println!("  ┌── test.log for {} ──", r.project);
                        for line in log.lines().take(200) {
                            println!("  │ {line}");
                        }
                        let total_lines = log.lines().count();
                        if total_lines > 200 {
                            println!("  │ ... ({} more lines)", total_lines - 200);
                        }
                        println!("  └──");
                    }
                }
                for d in &enrich_diffs { println!("  {d}"); }
                let mut all_diffs: Vec<String> = regressions.iter().map(|r| format!("{}: {}", r.project, r.field)).collect();
                all_diffs.extend(enrich_diffs);
                Ok(TestOutcome::Failed(vec![BatteryMismatch {
                    battery: "CRUST".into(),
                    diffs: all_diffs,
                }]))
            }
        }
        TestMode::Run => Ok(TestOutcome::Ok),
    }
}

/// Blind CRUST test: run LLM-generated tests, then swap in real tests and run again.
pub fn run_blind_crust_test(
    paths: &Paths,
    projects: &[crate::battery::CrustProject],
    mode: TestMode,
) -> Result<TestOutcome> {
    let mut llm_passed = 0usize;
    let mut real_passed = 0usize;
    let mut total = 0usize;
    let mut results: Vec<(String, CrustTestResult, CrustTestResult)> = Vec::new();

    let check_only = matches!(mode, TestMode::Check);

    for project in projects {
        let name = project.name();
        let proj_dir = paths.verify_dir(name);
        if !proj_dir.join("Cargo.toml").exists() { continue; }
        total += 1;

        let bin_dir = proj_dir.join("src/bin");

        // Phase 1: run with LLM-generated tests (skip in --check for speed)
        let (llm_result, llm_ok) = if check_only {
            (CrustTestResult { tests_ok: 0, tests_failed: 0, build_ok: true }, false)
        } else {
            let r = test_one_crust(proj_dir.as_ref())?;
            let ok = r.build_ok && r.tests_ok > 0 && r.tests_failed == 0;
            if ok { llm_passed += 1; }
            // Preserve LLM test log
            let logs_dir = proj_dir.join("logs");
            let _ = std::fs::rename(logs_dir.join("test.log"), logs_dir.join("test_llm.log"));
            (r, ok)
        };

        // Save LLM tests aside
        let llm_backup = proj_dir.join("src/bin_llm");
        if !check_only && bin_dir.is_dir() {
            if llm_backup.exists() { std::fs::remove_dir_all(&llm_backup)?; }
            crate::translate::copy_dir_all(&bin_dir, &llm_backup)?;
        }

        // Phase 2: swap in real tests from scaffold (src/bin + Cargo.toml)
        let cargo_toml = proj_dir.join("Cargo.toml");
        let cargo_backup = proj_dir.join("Cargo.toml.llm");
        let real_bin = project.scaffold().join("src/bin");
        if real_bin.is_dir() {
            if bin_dir.is_dir() { std::fs::remove_dir_all(&bin_dir)?; }
            let _ = std::fs::remove_dir_all(proj_dir.join("target"));
            crate::translate::copy_dir_all(&real_bin, &bin_dir)?;
            // Swap Cargo.toml so [[test]] entries match the real test files
            std::fs::rename(&cargo_toml, &cargo_backup)?;
            std::fs::copy(project.scaffold().join("Cargo.toml"), &cargo_toml)?;
        }

        let real_result = test_one_crust(proj_dir.as_ref())?;
        let real_ok = real_result.build_ok && real_result.tests_ok > 0 && real_result.tests_failed == 0;
        if real_ok { real_passed += 1; }

        // Preserve real test log
        let logs_dir = proj_dir.join("logs");
        let _ = std::fs::rename(logs_dir.join("test.log"), logs_dir.join("test_real.log"));

        // Restore verify's Cargo.toml and LLM tests
        if cargo_backup.exists() {
            let _ = std::fs::remove_file(&cargo_toml);
            std::fs::rename(&cargo_backup, &cargo_toml)?;
        }
        if !check_only && llm_backup.is_dir() {
            if bin_dir.is_dir() { std::fs::remove_dir_all(&bin_dir)?; }
            let _ = std::fs::remove_dir_all(proj_dir.join("target"));
            std::fs::rename(&llm_backup, &bin_dir)?;
        }

        // Report
        let llm_icon = if llm_ok { "✅" } else { "❌" };
        let real_icon = if real_ok { "✅" } else { "❌" };
        println!("  {name}: LLM {llm_icon} ({}/{})  Real {real_icon} ({}/{})",
            llm_result.tests_ok, llm_result.tests_ok + llm_result.tests_failed,
            real_result.tests_ok, real_result.tests_ok + real_result.tests_failed);

        results.push((name.to_string(), real_result.clone(), llm_result.clone()));

        if matches!(mode, TestMode::Update) {
            let translate_meta = crate::battery::extract_agent_meta(
                &paths.translate_dir(name).join("logs/translation.log"),
            );
            let verify_meta = crate::battery::extract_agent_meta(
                &proj_dir.join("logs/verify.log"),
            );
            let mut json = serde_json::json!({
                "llm_tests_ok": llm_result.tests_ok,
                "llm_tests_failed": llm_result.tests_failed,
                "real_tests_ok": real_result.tests_ok,
                "real_tests_failed": real_result.tests_failed,
                "build_ok": real_result.build_ok,
                "unsafe": crate::battery::count_unsafe(&paths.translate_dir(name).join("src")),
            });
            if let Some(m) = translate_meta {
                json["translate"] = serde_json::to_value(&m).unwrap();
            }
            if let Some(m) = verify_meta {
                json["verify"] = serde_json::to_value(&m).unwrap();
            }
            std::fs::write(proj_dir.join("result.json"), serde_json::to_string_pretty(&json)? + "\n")?;
        }
    }

    println!("\nCRUST-blind: {llm_passed}/{total} pass (LLM tests)");
    println!("CRUST-blind: {real_passed}/{total} pass (real tests)");

    match mode {
        TestMode::Check => {
            // Load stored result.json and compare real_tests fields
            let stored = load_blind_stored_results(paths)?;
            let mut regressions = Vec::new();
            for (name, actual_real, _actual_llm) in results.iter() {
                if let Some(stored_r) = stored.get(name.as_str()) {
                    if actual_real.tests_ok != stored_r.real_tests_ok {
                        regressions.push(format!("{name}: real_tests_ok expected={} actual={}", stored_r.real_tests_ok, actual_real.tests_ok));
                    }
                    if actual_real.tests_failed != stored_r.real_tests_failed {
                        regressions.push(format!("{name}: real_tests_failed expected={} actual={}", stored_r.real_tests_failed, actual_real.tests_failed));
                    }
                }
                // Check credits + unsafe
                let rj = paths.verify_dir(name).join("result.json");
                let src = paths.translate_dir(name).join("src");
                let tlog = paths.translate_dir(name).join("logs/translation.log");
                let vlog = paths.verify_dir(name).join("logs/verify.log");
                for d in check_enrichment(&rj, &src, &[("translate", &tlog), ("verify", &vlog)], paths.agent) {
                    regressions.push(format!("{name}: {d}"));
                }
            }
            if regressions.is_empty() {
                println!("✅ No regressions");
                Ok(TestOutcome::Passed)
            } else {
                println!("\n❌ {} regression(s):", regressions.len());
                for r in &regressions {
                    println!("  {r}");
                    // Extract project name and dump test_real.log
                    let proj = r.split(':').next().unwrap_or("");
                    let log_path = paths.verify_dir(proj).join("logs/test_real.log");
                    if let Ok(log) = std::fs::read_to_string(&log_path) {
                        println!("  ┌── test_real.log for {proj} ──");
                        for line in log.lines().rev().take(50).collect::<Vec<_>>().into_iter().rev() {
                            println!("  │ {line}");
                        }
                        println!("  └──");
                    }
                }
                Ok(TestOutcome::Failed(vec![BatteryMismatch {
                    battery: "CRUST-blind".into(),
                    diffs: regressions,
                }]))
            }
        }
        _ => Ok(TestOutcome::Ok),
    }
}

// ── Battery discovery ──────────────────────────────────────────────────

fn discover_batteries(results_dir: &Path) -> Result<Vec<String>> {
    let mut batteries = Vec::new();
    if !results_dir.is_dir() {
        return Ok(batteries);
    }
    for entry in std::fs::read_dir(results_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // Must contain at least one case with translated_rust/
        let has_cases = std::fs::read_dir(entry.path())?
            .filter_map(|e| e.ok())
            .any(|e| e.path().join("translated_rust").is_dir());
        if has_cases {
            batteries.push(name);
        }
    }
    batteries.sort();
    Ok(batteries)
}

// ── Single battery ─────────────────────────────────────────────────────

fn run_battery(paths: &Paths, battery: &str, mode: TestMode, check_rows: &mut Vec<CheckRow>) -> Result<TestOutcome> {
    let output_dir = paths.output_dir(battery);

    if !output_dir.is_dir() {
        println!("⚠️  {battery}: no results directory, skipping");
        return Ok(TestOutcome::Ok);
    }

    println!();
    println!("========================================");
    println!("  Testing: {battery}");
    println!("========================================");

    // Copy test infra from corpus (cleaned up by guard on drop)
    copy_test_artifacts(paths, battery)?;
    let _guard = TestArtifactGuard { output_dir: output_dir.clone() };

    // Clean stale build artifacts
    clean_targets(&output_dir)?;

    // Generate workspace Cargo.toml for lib runners
    generate_workspace(&output_dir)?;

    // Run MIT runtests — the source of truth for all test outcomes.
    let (summary, per_case) = run_runtests(paths, battery, mode)?;

    // Print summary line
    let vt = summary.vectors_passed + summary.vectors_failed;
    let pct = if vt > 0 {
        format!("{:.1}%", 100.0 * summary.vectors_passed as f64 / vt as f64)
    } else {
        "N/A".to_string()
    };
    println!("  {battery}: {}/{} cases, {}/{vt} vectors ({pct})",
        summary.cases_passed, summary.cases_tested, summary.vectors_passed);
    println!("========================================");

    match mode {
        TestMode::Update => {
            write_results(&output_dir, battery, &summary, &per_case)?;
            let vt = summary.vectors_passed + summary.vectors_failed;
            println!("   📝 Updated: {}/{} cases, {}/{vt} vectors",
                summary.cases_passed, summary.cases_tested, summary.vectors_passed);
            Ok(TestOutcome::Ok)
        }
        TestMode::Check => {
            let expected = load_summary(&output_dir);
            let mut diffs = diff_summaries(&expected, &summary);
            // Check credits + unsafe per case
            for case_name in per_case.keys() {
                let case_dir = output_dir.join(case_name);
                let tlog = case_dir.join("logs/translation.log");
                let vlog = case_dir.join("logs/verify.log");
                for d in check_enrichment(
                    &case_dir.join("result.json"),
                    &case_dir.join("translated_rust/src"),
                    &[("translate", &tlog), ("verify", &vlog)],
                    paths.agent,
                ) {
                    diffs.push(format!("{case_name}: {d}"));
                }
            }
            let ok = diffs.is_empty();
            check_rows.push(CheckRow {
                battery: battery.to_string(),
                expected: expected.clone(),
                actual: summary.clone(),
                ok,
            });
            if ok {
                println!("   ✅ {battery}: OK");
                Ok(TestOutcome::Passed)
            } else {
                println!("   ❌ {battery}: MISMATCH: {}", diffs.join("; "));
                Ok(TestOutcome::Failed(vec![BatteryMismatch {
                    battery: battery.to_string(),
                    diffs,
                }]))
            }
        }
        TestMode::Run => {
            Ok(TestOutcome::Ok)
        }
    }
}

// ── Test artifact management ───────────────────────────────────────────

fn copy_test_artifacts(paths: &Paths, battery: &str) -> Result<()> {
    let input_dir = paths.input_dir(battery);
    let output_dir = paths.output_dir(battery);

    for entry in std::fs::read_dir(&output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let corpus_case = input_dir.join(&name);
        let case_dir = entry.path();

        if !case_dir.join("translated_rust").is_dir() {
            continue;
        }

        // Copy test_vectors
        let tv_src = corpus_case.join("test_vectors");
        let tv_dst = case_dir.join("test_vectors");
        if tv_src.is_dir() && !tv_dst.exists() {
            copy_dir_all(&tv_src, &tv_dst)?;
        }

        // Copy runner
        let runner_src = corpus_case.join("runner");
        let runner_dst = case_dir.join("runner");
        if runner_src.is_dir() && !runner_dst.exists() {
            copy_dir_all(&runner_src, &runner_dst)?;

            // Fix cando2 path in runner Cargo.toml
            let runner_cargo = runner_dst.join("Cargo.toml");
            if runner_cargo.exists() {
                let cando2_abs = paths.corpus_dir.join("tools/cando2");
                if cando2_abs.is_dir() {
                    let content = std::fs::read_to_string(&runner_cargo)?;
                    let fixed = content.replace(
                        "path = \"../../../../tools/cando2\"",
                        &format!("path = \"{}\"", cando2_abs.display()),
                    );
                    std::fs::write(&runner_cargo, fixed)?;
                }
            }
        }
    }
    Ok(())
}

fn cleanup_test_artifacts(output_dir: &Path) -> Result<()> {
    if !output_dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        for subdir in ["test_vectors", "runner"] {
            let path = entry.path().join(subdir);
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            }
        }
    }
    // Remove workspace Cargo.toml generated for lib runners
    let ws_toml = output_dir.join("Cargo.toml");
    if ws_toml.exists() {
        let _ = std::fs::remove_file(&ws_toml);
    }
    Ok(())
}

fn clean_targets(output_dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let target = entry.path().join("translated_rust/target");
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
    }
    Ok(())
}

// ── Workspace generation ───────────────────────────────────────────────

fn generate_workspace(output_dir: &Path) -> Result<()> {
    let mut members = Vec::new();
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        let runner_toml = entry.path().join("runner/Cargo.toml");
        if runner_toml.exists() {
            let name = entry.file_name().to_string_lossy().to_string();
            members.push(format!("    \"{name}/runner\""));
        }
    }
    if !members.is_empty() {
        members.sort();
        let content = format!(
            "[workspace]\nmembers = [\n{},\n]\nresolver = \"2\"\n",
            members.join(",\n")
        );
        std::fs::write(output_dir.join("Cargo.toml"), content)?;
    }
    Ok(())
}

// ── Run runtests ───────────────────────────────────────────────────────

fn run_runtests(paths: &Paths, battery: &str, mode: TestMode) -> Result<(Summary, HashMap<String, serde_json::Value>)> {
    let output_dir = paths.output_dir(battery);
    let scripts_dir = paths.corpus_dir.join("deployment/scripts/github-actions");

    let mut pythonpath = scripts_dir.to_string_lossy().to_string();
    if let Ok(existing) = std::env::var("PYTHONPATH") {
        pythonpath = format!("{pythonpath}:{existing}");
    }

    let output = Command::new("python3")
        .args(["-m", "runtests.rust", "--root", &output_dir.to_string_lossy(),
               "--subset", &output_dir.to_string_lossy(), "--keep-going", "--verbose"])
        .env("PYTHONPATH", &pythonpath)
        .current_dir(&paths.corpus_dir)
        .output()
        .context("running MIT runtests")?;

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !matches!(mode, TestMode::Check) {
        print!("{text}");
    }
    let _ = std::fs::write(output_dir.join("test.log"), &text);

    let extract = |pattern: &str| -> usize {
        Regex::new(pattern)
            .ok()
            .and_then(|re| re.captures(&text))
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0)
    };

    let cases_discovered = extract(r"Test Cases Discovered:\s+(\d+)");
    let vectors_passed = extract(r"Test Vectors Passed:\s+(\d+)");
    let vectors_failed = extract(r"Test Vectors Failed:\s+(\d+)");
    let vectors_skipped = extract(r"Test Vectors Skipped:\s+(\d+)");

    // Parse ALL per-case outcomes from runtests output.
    // Runtests reports every failure as: "- CASE_NAME: Build failed ..." or "- CASE_NAME: Test failed ..."
    // and every executed case as: "Executing CASE_NAME"
    let mut per_case: HashMap<String, serde_json::Value> = HashMap::new();
    let mut failed_cases: Vec<String> = Vec::new();

    // 1. Parse "- NAME: Build failed ..." lines
    let build_fail_re = Regex::new(r"^- (\S+): Build failed")?;
    for line in text.lines() {
        if let Some(caps) = build_fail_re.captures(line) {
            let name = caps[1].to_string();
            if !failed_cases.contains(&name) { failed_cases.push(name.clone()); }
            per_case.insert(name.clone(), serde_json::json!({
                "case": name, "battery": battery,
                "vectors_failed": 1, "passed": false,
                "error": "build failed",
            }));
        }
    }

    // 2. Parse "- NAME: Test failed ..." lines
    let test_fail_re = Regex::new(r"^- (\S+): Test failed")?;
    for line in text.lines() {
        if let Some(caps) = test_fail_re.captures(line) {
            let name = caps[1].to_string();
            if !failed_cases.contains(&name) { failed_cases.push(name.clone()); }
            per_case.insert(name.clone(), serde_json::json!({
                "case": name, "battery": battery,
                "vectors_failed": 1, "passed": false,
                "error": "test failed",
            }));
        }
    }

    // 3. Parse "Executing NAME" lines — these passed (unless already marked failed)
    let exec_re = Regex::new(r"Executing (\S+)")?;
    for caps in exec_re.captures_iter(&text) {
        let name = caps[1].to_string();
        per_case.entry(name.clone()).or_insert_with(|| serde_json::json!({
            "case": name, "battery": battery,
            "vectors_failed": 0, "passed": true,
        }));
    }

    failed_cases.sort();
    let cases_passed = cases_discovered.saturating_sub(failed_cases.len());

    Ok((Summary {
        cases_tested: cases_discovered,
        cases_passed,
        vectors_passed,
        vectors_failed,
        vectors_skipped,
        failed_cases,
    }, per_case))
}

// ── Summary I/O ────────────────────────────────────────────────────────

fn write_results(
    output_dir: &Path,
    _battery: &str,
    summary: &Summary,
    per_case: &HashMap<String, serde_json::Value>,
) -> Result<()> {
    for (case_name, data) in per_case {
        let case_dir = output_dir.join(case_name);
        if case_dir.is_dir() {
            let mut val = data.clone();
            for log in ["logs/translation.log", "logs/verify.log"] {
                let log_path = case_dir.join(log);
                if let Some(m) = crate::battery::extract_agent_meta(&log_path) {
                    let key = if log.contains("translation") { "translate" } else { "verify" };
                    val[key] = serde_json::to_value(&m).unwrap();
                }
            }
            val["unsafe"] = serde_json::to_value(
                &crate::battery::count_unsafe(&case_dir.join("translated_rust/src")),
            ).unwrap();
            let json = serde_json::to_string_pretty(&val)?;
            std::fs::write(case_dir.join("result.json"), format!("{json}\n"))?;
        }
    }
    let json = serde_json::to_string_pretty(summary)?;
    std::fs::write(output_dir.join("summary.json"), format!("{json}\n"))?;
    Ok(())
}

fn load_summary(output_dir: &Path) -> Summary {
    let path = output_dir.join("summary.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn diff_summaries(expected: &Summary, actual: &Summary) -> Vec<String> {
    let mut diffs = Vec::new();
    macro_rules! cmp {
        ($field:ident) => {
            if actual.$field != expected.$field {
                diffs.push(format!("{}: {} → {}", stringify!($field), expected.$field, actual.$field));
            }
        };
    }
    cmp!(vectors_passed);
    cmp!(vectors_failed);
    cmp!(cases_passed);
    cmp!(cases_tested);
    let added: Vec<_> = actual.failed_cases.iter().filter(|c| !expected.failed_cases.contains(c)).collect();
    let removed: Vec<_> = expected.failed_cases.iter().filter(|c| !actual.failed_cases.contains(c)).collect();
    if !added.is_empty() {
        diffs.push(format!("new failures: {}", added.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if !removed.is_empty() {
        diffs.push(format!("no longer failing: {}", removed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    diffs
}


// ── Enrich: backfill result.json with credits + unsafe (no test runs) ──

/// Compare stored credits + unsafe in result.json against live extraction.
/// Returns a list of mismatch descriptions (empty = all good).
fn check_enrichment(
    result_json: &Path,
    src_dir: &Path,
    log_paths: &[(&str, &Path)],
    agent: crate::cli::Agent,
) -> Vec<String> {
    let mut diffs = Vec::new();
    let Ok(data) = std::fs::read_to_string(result_json) else { return diffs };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else { return diffs };

    // All agents require unsafe counts
    let live = crate::battery::count_unsafe(src_dir);
    match json.get("unsafe") {
        Some(stored) => {
            let sb = stored.get("blocks").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let sf = stored.get("fns").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let si = stored.get("impls").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if sb != live.blocks { diffs.push(format!("unsafe.blocks expected={sb} actual={}", live.blocks)); }
            if sf != live.fns { diffs.push(format!("unsafe.fns expected={sf} actual={}", live.fns)); }
            if si != live.impls { diffs.push(format!("unsafe.impls expected={si} actual={}", live.impls)); }
            let sl = stored.get("lines").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if sl != live.lines { diffs.push(format!("unsafe.lines expected={sl} actual={}", live.lines)); }
        }
        None => diffs.push("missing unsafe field".into()),
    }

    // Only kiro has credits
    let require_credits = matches!(agent, crate::cli::Agent::Kiro);
    for &(key, log) in log_paths {
        let live_meta = crate::battery::extract_agent_meta(log);
        match (json.get(key), live_meta) {
            (Some(stored), Some(live)) => {
                let sc = stored.get("credits").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let sw = stored.get("wall_secs").and_then(|v| v.as_u64()).unwrap_or(0);
                if (sc - live.credits.0).abs() > 0.001 {
                    diffs.push(format!("{key}.credits expected={sc} actual={}", live.credits.0));
                }
                if sw != live.wall_secs {
                    diffs.push(format!("{key}.wall_secs expected={sw} actual={}", live.wall_secs));
                }
            }
            (None, Some(_)) if require_credits => diffs.push(format!("missing {key} field")),
            _ => {}
        }
    }
    diffs
}

pub fn enrich_blind_crust(paths: &Paths, projects: &[crate::battery::CrustProject]) -> Result<()> {
    let mut enriched = 0usize;
    for project in projects {
        let name = project.name();
        let rj = paths.verify_dir(name).join("result.json");
        if !rj.exists() { continue; }
        let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&rj)?)?;

        if let Some(m) = crate::battery::extract_agent_meta(&paths.translate_dir(name).join("logs/translation.log")) {
            json["translate"] = serde_json::to_value(&m)?;
        }
        if let Some(m) = crate::battery::extract_agent_meta(&paths.verify_dir(name).join("logs/verify.log")) {
            json["verify"] = serde_json::to_value(&m)?;
        }
        json["unsafe"] = serde_json::to_value(&crate::battery::count_unsafe(&paths.translate_dir(name).join("src")))?;

        std::fs::write(&rj, serde_json::to_string_pretty(&json)? + "\n")?;
        enriched += 1;
    }
    println!("✅ Enriched {enriched} CRUST-blind result.json files");
    Ok(())
}

pub fn enrich_crust(paths: &Paths, projects: &[crate::battery::CrustProject]) -> Result<()> {
    let mut enriched = 0usize;
    for project in projects {
        let name = project.name();
        let proj_dir = paths.output_dir(name);
        let rj = proj_dir.join("result.json");
        if !rj.exists() { continue; }
        let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&rj)?)?;

        if let Some(m) = crate::battery::extract_agent_meta(&proj_dir.join("logs/translation.log")) {
            json["agent"] = serde_json::to_value(&m)?;
        }
        json["unsafe"] = serde_json::to_value(&crate::battery::count_unsafe(&proj_dir.join("src")))?;

        std::fs::write(&rj, serde_json::to_string_pretty(&json)? + "\n")?;
        enriched += 1;
    }
    println!("✅ Enriched {enriched} CRUST result.json files");
    Ok(())
}

pub fn enrich_test_corpus(paths: &Paths, battery: &str) -> Result<()> {
    let output_dir = paths.results_dir.join(battery);
    if !output_dir.is_dir() { return Ok(()); }
    let mut enriched = 0usize;
    for entry in std::fs::read_dir(&output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let case_dir = entry.path();
        let rj = case_dir.join("result.json");
        if !rj.exists() { continue; }
        let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&rj)?)?;

        for log in ["logs/translation.log", "logs/verify.log"] {
            if let Some(m) = crate::battery::extract_agent_meta(&case_dir.join(log)) {
                let key = if log.contains("translation") { "translate" } else { "verify" };
                json[key] = serde_json::to_value(&m)?;
            }
        }
        json["unsafe"] = serde_json::to_value(&crate::battery::count_unsafe(&case_dir.join("translated_rust/src")))?;

        std::fs::write(&rj, serde_json::to_string_pretty(&json)? + "\n")?;
        enriched += 1;
    }
    println!("✅ Enriched {enriched} {battery} result.json files");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn load_blind_stored_results_reads_from_verify_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let results_dir = tmp.path().join("results/CRUST-blind/kiro");

        // Create project with verify/result.json
        let proj = results_dir.join("vec/verify");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("result.json"), r#"{"real_tests_ok": 22, "real_tests_failed": 0}"#).unwrap();

        // Create another project — result.json at root (old layout) should be ignored
        let proj2 = results_dir.join("hamta");
        fs::create_dir_all(&proj2).unwrap();
        fs::write(proj2.join("result.json"), r#"{"real_tests_ok": 99, "real_tests_failed": 1}"#).unwrap();

        let paths = crate::battery::Paths::new(
            tmp.path(), crate::cli::Agent::Kiro, crate::cli::Dataset::BlindCrust,
        );

        let stored = load_blind_stored_results(&paths).unwrap();
        assert_eq!(stored.len(), 1, "only verify/ layout should be found");
        assert_eq!(stored["vec"].real_tests_ok, 22);
        assert_eq!(stored["vec"].real_tests_failed, 0);
    }
}
