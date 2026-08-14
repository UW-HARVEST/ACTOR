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
        let result = run_battery(paths, battery, mode, &mut check_rows)?;
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
        println!("  {:<25} {:>15} {:>15}  Status", "Battery", "Stored", "Actual");
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

/// Default OpenSSL location for `openssl-sys` builds. Some translated crates
/// depend on it; without this set the build fails for environmental reasons
/// unrelated to the translation.
fn openssl_dir() -> String {
    std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into())
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
        // Must contain at least one case with a translated/ phase dir
        let has_cases = std::fs::read_dir(entry.path())?
            .filter_map(|e| e.ok())
            .any(|e| crate::battery::phase_dir(&e.path(), crate::battery::TRANSLATED).is_dir());
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

    // Generate workspace Cargo.toml for lib runners
    generate_workspace(&output_dir)?;

    // Does any case have a verified/ phase (i.e. did a verify phase run)? If so
    // we score TWO phases: the validated result (verified/) and the no-validate
    // result (translated/). Otherwise a single translated/ pass suffices.
    let has_verified = std::fs::read_dir(&output_dir)?.filter_map(|e| e.ok())
        .any(|e| crate::battery::phase_dir(&e.path(), crate::battery::VERIFIED).join("Cargo.toml").exists());

    // Score the pre-verify (translated/) phase first; if a verify phase ran,
    // score the post-verify (verified/) phase second and treat IT as the
    // battery's headline summary. Each pass stages `translated_rust` → its
    // phase dir so unmodified runtests scores that crate, writes result.json
    // into that phase dir, and writes a per-phase battery summary.
    let mut phases: Vec<&str> = vec![crate::battery::TRANSLATED];
    if has_verified { phases.push(crate::battery::VERIFIED); }

    let mut headline: Option<(Summary, HashMap<String, serde_json::Value>)> = None;
    for phase in &phases {
        stage_phase_for_runtests(&output_dir, phase)?;
        clean_targets(&output_dir)?;
        let (summary, per_case) = run_runtests(paths, battery, mode)?;
        unstage_phase(&output_dir)?;

        let vt = summary.vectors_passed + summary.vectors_failed;
        let pct = if vt > 0 {
            format!("{:.1}%", 100.0 * summary.vectors_passed as f64 / vt as f64)
        } else { "N/A".to_string() };
        println!("  {battery} [{phase}]: {}/{} cases, {}/{vt} vectors ({pct})",
            summary.cases_passed, summary.cases_tested, summary.vectors_passed);

        if matches!(mode, TestMode::Update) {
            write_results(&output_dir, phase, &summary, &per_case)?;
        }
        headline = Some((summary, per_case)); // last phase (verified if present) is headline
    }
    println!("========================================");

    let (summary, per_case) = headline.expect("at least the translated phase is scored");

    match mode {
        TestMode::Update => {
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
                let phase = crate::battery::crate_dir(&case_dir);
                let tlog = phase.join("logs/translation.log");
                let vlog = phase.join("logs/verify.log");
                for d in check_enrichment(
                    &phase.join("result.json"),
                    &phase.join("src"),
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

// ── runtests phase staging ─────────────────────────────────────────────
//
// MIT's `runtests` (unmodified) discovers each case's crate at the hardcoded
// path `<case>/translated_rust/` (test-corpus/.../discovery/rust.py). Our
// canonical storage uses `translated/` and `verified/` instead. To score a
// given phase with runtests WITHOUT touching runtests, we stage the phase dir
// under the name runtests expects: `<case>/translated_rust` becomes a symlink
// to `<case>/<phase>`. runtests resolves the symlink (`.resolve()`), so it
// transparently builds and scores that phase's crate. The symlink is a
// transient scoring artifact, removed by the TestArtifactGuard.

/// Point every case's `translated_rust` symlink at the given phase dir, for the
/// cases that have that phase. Returns the number of cases staged.
fn stage_phase_for_runtests(output_dir: &Path, phase: &str) -> Result<usize> {
    let mut staged = 0usize;
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let case_dir = entry.path();
        let phase_path = crate::battery::phase_dir(&case_dir, phase);
        if !phase_path.join("Cargo.toml").exists() { continue; }
        let link = case_dir.join(crate::battery::TRANSLATED_RUST);
        // Replace any prior symlink/dir at translated_rust.
        if link.is_symlink() || link.exists() {
            let _ = std::fs::remove_file(&link);
            if link.is_dir() { let _ = std::fs::remove_dir_all(&link); }
        }
        std::os::unix::fs::symlink(phase, &link)?;
        staged += 1;
    }
    Ok(staged)
}

/// Remove the transient `translated_rust` staging symlinks.
fn unstage_phase(output_dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let link = entry.path().join(crate::battery::TRANSLATED_RUST);
        if link.is_symlink() {
            let _ = std::fs::remove_file(&link);
        }
    }
    Ok(())
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

        if !crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED).is_dir() {
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
        // Remove the transient runtests staging symlink (translated_rust → phase).
        let link = entry.path().join(crate::battery::TRANSLATED_RUST);
        if link.is_symlink() {
            let _ = std::fs::remove_file(&link);
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
        // Clean the build target of whichever phase dir is current (verified/
        // else translated/) — the crate runtests will build.
        let target = crate::battery::crate_dir(&entry.path()).join("target");
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
        .env("OPENSSL_DIR", openssl_dir())
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
    // and every executed case as: "Executing CASE_NAME". Each "Test failed" line
    // belongs to ONE failed test vector and is followed by a multi-line block:
    //   - NAME: Test failed (testN: REASON
    //   <diff lines>
    //   expected rc=A, actual rc=B
    //   )
    // We accumulate per-vector failures so result.json reflects the true
    // vectors_failed count and includes per-vector diff snippets — without this,
    // analyzing failures requires hand-grepping the battery-level test.log.
    let mut per_case: HashMap<String, serde_json::Value> = HashMap::new();
    let mut failed_cases: Vec<String> = Vec::new();

    // 1. Parse "- NAME: Build failed ..." lines (single-line, one per case)
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

    // 2. Parse "- NAME: Test failed (testN: REASON\n...diff...\n)" blocks.
    //    Multiple consecutive blocks belong to the same case (one per vector).
    let test_fail_open_re = Regex::new(r"^- (\S+): Test failed \((test\w+): ([^\n]*)$")?;
    let rc_re = Regex::new(r"expected rc=(\d+), actual rc=(\d+)")?;
    let mut case_vector_fails: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if let Some(caps) = test_fail_open_re.captures(lines[i]) {
            let name = caps[1].to_string();
            let vector = caps[2].to_string();
            let reason_first_line = caps[3].to_string();

            // Walk forward to the closing `)` line (blocks are short, ~10 lines).
            let start = i + 1;
            let mut end = start;
            while end < lines.len() && lines[end].trim() != ")" {
                end += 1;
            }
            let body = lines[start..end].join("\n");
            let (expected_rc, actual_rc) = rc_re.captures(&body)
                .map(|c| (c[1].parse::<i64>().unwrap_or(-1), c[2].parse::<i64>().unwrap_or(-1)))
                .unwrap_or((-1, -1));

            // Strip the rc line + trailing blank lines from the diff snippet.
            let diff = body.lines()
                .filter(|l| !rc_re.is_match(l))
                .collect::<Vec<_>>()
                .join("\n");
            let diff = diff.trim().to_string();

            // Reason like "stdout mismatch", "stderr mismatch, return code mismatch", etc.
            let reason = reason_first_line.trim_end_matches(',').trim().to_string();

            case_vector_fails.entry(name.clone()).or_default().push(serde_json::json!({
                "vector": vector,
                "reason": reason,
                "expected_rc": expected_rc,
                "actual_rc": actual_rc,
                "diff": diff,
            }));
            if !failed_cases.contains(&name) { failed_cases.push(name.clone()); }
            i = end + 1;
        } else {
            i += 1;
        }
    }

    // Some cases fail without any vector-level "(testN:" block (e.g. timeout,
    // build mid-run). Detect them by a fallback regex and surface a 1-vector
    // generic failure record.
    let test_fail_simple_re = Regex::new(r"^- (\S+): Test failed")?;
    for line in text.lines() {
        if let Some(caps) = test_fail_simple_re.captures(line) {
            let name = caps[1].to_string();
            if !failed_cases.contains(&name) { failed_cases.push(name.clone()); }
            case_vector_fails.entry(name).or_insert_with(|| vec![serde_json::json!({
                "vector": "unknown",
                "reason": "test failed (no vector-level detail)",
                "expected_rc": -1,
                "actual_rc": -1,
                "diff": "",
            })]);
        }
    }

    for (name, failures) in case_vector_fails {
        per_case.insert(name.clone(), serde_json::json!({
            "case": name,
            "battery": battery,
            "vectors_failed": failures.len(),
            "passed": false,
            "error": "test failed",
            "failures": failures,
        }));
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

/// Write per-case result.json + battery summary for one scored `phase`.
/// Each case's result.json + enrichment goes INTO its `<case>/<phase>/` dir,
/// co-located with the crate it scores (logs live there too). The battery
/// summary goes to `<battery>/summary.json` for the verified phase (the
/// headline) and `<battery>/summary_translated.json` for the pre-verify
/// (no-validate) phase, so report.rs can read each independently.
fn write_results(
    output_dir: &Path,
    phase: &str,
    summary: &Summary,
    per_case: &HashMap<String, serde_json::Value>,
) -> Result<()> {
    for (case_name, data) in per_case {
        let phase_dir = crate::battery::phase_dir(&output_dir.join(case_name), phase);
        if phase_dir.is_dir() {
            let mut val = data.clone();
            let tlog = phase_dir.join("logs/translation.log");
            let vlog = phase_dir.join("logs/verify.log");
            Enrichment::compute(
                &phase_dir.join("src"),
                &[("translate", &tlog), ("verify", &vlog)],
            ).merge_into(&mut val);
            let json = serde_json::to_string_pretty(&val)?;
            std::fs::write(phase_dir.join("result.json"), format!("{json}\n"))?;
        }
    }
    let json = serde_json::to_string_pretty(summary)?;
    std::fs::write(output_dir.join(summary_file(phase)), format!("{json}\n"))?;
    Ok(())
}

/// The battery-level summary filename for a phase: the verified phase is the
/// headline `summary.json`; the pre-verify phase is `summary_translated.json`.
fn summary_file(phase: &str) -> &'static str {
    if phase == crate::battery::VERIFIED { "summary.json" } else { "summary_translated.json" }
}

fn load_summary(output_dir: &Path) -> Summary {
    // Headline summary: verified phase if it was scored, else the translated one.
    let verified = output_dir.join("summary.json");
    let path = if verified.exists() { verified } else { output_dir.join("summary_translated.json") };
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


// ── Enrichment: the ONE definition of result.json metadata ─────────────
//
// Every result.json carries the same derived metadata alongside its test
// outcome: `unsafe` (AST-counted unsafe usage), `loc` (translated LOC), and
// one agent-run-meta object per phase (`translate` alone for a single-phase
// dataset, `translate`+`verify` for the two-phase pipelines). This used to be
// hand-written in six places (three `--update` blocks + three `enrich_*`
// fns) and hand-checked in a seventh (`check_enrichment`) — which is exactly
// how a result.json could drift from what `test --check` expected.
//
// `Enrichment` is now the single source of truth. `compute` gathers the live
// values from a translated `src/` dir plus a set of `(json_key, log)` phase
// logs; `merge_into` writes them onto a result.json value; `check` diffs
// stored-vs-live and is a pure inverse of `merge_into`. All writers call
// `merge_into` (via `enrich_file` or inline); `test --check` calls `check`.
// They can no longer drift.
pub struct Enrichment {
    unsafe_: crate::battery::UnsafeCounts,
    loc: crate::battery::LocCounts,
    /// Per-phase run metadata, in the given key order, for logs that existed.
    meta: Vec<(String, crate::battery::AgentRunMeta)>,
}

impl Enrichment {
    /// Gather live enrichment values. `src_dir` is the translated crate's
    /// `src/`; `logs` maps each result.json phase key to its agent log.
    pub fn compute(src_dir: &Path, logs: &[(&str, &Path)]) -> Self {
        let meta = logs.iter()
            .filter_map(|(key, log)| {
                crate::battery::extract_agent_meta(log).map(|m| (key.to_string(), m))
            })
            .collect();
        Self {
            unsafe_: crate::battery::count_unsafe(src_dir),
            loc: crate::battery::count_loc(src_dir),
            meta,
        }
    }

    /// Write the computed values onto a result.json value.
    pub fn merge_into(&self, json: &mut serde_json::Value) {
        json["unsafe"] = serde_json::to_value(&self.unsafe_).unwrap();
        json["loc"] = serde_json::to_value(&self.loc).unwrap();
        for (key, m) in &self.meta {
            json[key] = serde_json::to_value(m).unwrap();
        }
    }

    /// Enrich one result.json file in place (read → merge → write). No-op if
    /// the file is missing. Returns whether it was written.
    fn enrich_file(rj: &Path, src_dir: &Path, logs: &[(&str, &Path)]) -> Result<bool> {
        if !rj.exists() { return Ok(false); }
        let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(rj)?)?;
        Self::compute(src_dir, logs).merge_into(&mut json);
        std::fs::write(rj, serde_json::to_string_pretty(&json)? + "\n")?;
        Ok(true)
    }
}

/// Compare stored credits + unsafe + loc in result.json against live values.
/// Pure inverse of [`Enrichment::merge_into`]. Returns mismatch descriptions
/// (empty = all good). `agent` gates the "missing meta" check to kiro, the
/// only agent that records credits.
fn check_enrichment(
    result_json: &Path,
    src_dir: &Path,
    log_paths: &[(&str, &Path)],
    agent: crate::cli::Agent,
) -> Vec<String> {
    let mut diffs = Vec::new();
    let Ok(data) = std::fs::read_to_string(result_json) else { return diffs };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else { return diffs };

    let live = Enrichment::compute(src_dir, log_paths);

    // All agents require unsafe counts
    match json.get("unsafe") {
        Some(stored) => {
            let sb = stored.get("blocks").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let sf = stored.get("fns").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let si = stored.get("impls").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if sb != live.unsafe_.blocks { diffs.push(format!("unsafe.blocks expected={sb} actual={}", live.unsafe_.blocks)); }
            if sf != live.unsafe_.fns { diffs.push(format!("unsafe.fns expected={sf} actual={}", live.unsafe_.fns)); }
            if si != live.unsafe_.impls { diffs.push(format!("unsafe.impls expected={si} actual={}", live.unsafe_.impls)); }
            let sl = stored.get("lines").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if sl != live.unsafe_.lines { diffs.push(format!("unsafe.lines expected={sl} actual={}", live.unsafe_.lines)); }
        }
        None => diffs.push("missing unsafe field".into()),
    }

    // LOC counts
    match json.get("loc") {
        Some(stored) => {
            let sc = stored.get("code").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if sc != live.loc.code { diffs.push(format!("loc.code expected={sc} actual={}", live.loc.code)); }
        }
        None => diffs.push("missing loc field".into()),
    }

    // Only kiro has credits. `live.meta` holds exactly the phases whose logs
    // existed (same filter as merge_into), keyed identically. A phase whose log
    // is absent is simply not compared — matching the original behavior, which
    // only checked keys with a live log.
    let require_credits = matches!(agent, crate::cli::Agent::Kiro);
    for (key, live) in &live.meta {
        match json.get(key) {
            Some(stored) => {
                let sc = stored.get("credits").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let sw = stored.get("wall_secs").and_then(|v| v.as_u64()).unwrap_or(0);
                if (sc - live.credits.0).abs() > 0.001 {
                    diffs.push(format!("{key}.credits expected={sc} actual={}", live.credits.0));
                }
                if sw != live.wall_secs {
                    diffs.push(format!("{key}.wall_secs expected={sw} actual={}", live.wall_secs));
                }
            }
            None if require_credits => diffs.push(format!("missing {key} field")),
            None => {}
        }
    }
    diffs
}

// ── harvest-bench testing ──────────────────────────────────────────────

/// Per-project harvest-bench result: build the translated crate into a cdylib,
/// then run the upstream GoogleTest suite against it via the harvest-bench
/// runner. `passed` uses the canonical project pass rule (see
/// `crate::scoring`), so the pass column means the same thing across datasets.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct HarvestBenchResult {
    tests_ok: usize,
    tests_failed: usize,
    tests_skipped: usize,
    build_ok: bool,
}

impl HarvestBenchResult {
    fn passed(&self) -> bool {
        crate::scoring::ProjectOutcome {
            built: self.build_ok,
            tests_ok: self.tests_ok as u32,
            tests_failed: self.tests_failed as u32,
        }.passed()
    }
}

/// Locate the prebuilt harvest-bench runner (`harvest-bench/runner/target/
/// release/harvest-bench`). `corpus_dir` is `harvest-bench/tests`.
fn harvest_bench_runner(corpus_dir: &Path) -> Result<PathBuf> {
    let bin = corpus_dir
        .parent().context("harvest-bench/tests has no parent")?
        .join("runner/target/release/harvest-bench");
    anyhow::ensure!(bin.is_file(),
        "harvest-bench runner not built: {} (run `cargo build --release --manifest-path harvest-bench/runner/Cargo.toml`)",
        bin.display());
    Ok(bin)
}

/// Build the translated crate into a cdylib and return the `.so` path (or a
/// build-failure). The suite links `lib<name>.so` by ABI.
fn build_harvest_bench_lib(crate_dir: &Path, name: &str) -> (Option<PathBuf>, String) {
    let out = Command::new("timeout")
        .args(["600", "cargo", "build", "--release"])
        .env("OPENSSL_DIR", openssl_dir())
        .env("OPENSSL_NO_VENDOR", "1")
        .current_dir(crate_dir)
        .output();
    let Ok(out) = out else { return (None, "failed to spawn cargo build".into()) };
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // cdylib output name derives from the [lib] name (set to the project name),
    // with `-`→`_` normalization cargo applies.
    let lib_stem = name.replace('-', "_");
    let so = crate_dir.join(format!("target/release/lib{lib_stem}.so"));
    if so.is_file() { (Some(so), stderr) } else { (None, stderr) }
}

/// Run the upstream suite against a built `.so` and parse the JSON report.
fn score_harvest_bench_suite(
    runner: &Path, suite_dir: &Path, lib: &Path, report_json: &Path,
) -> Result<(usize, usize, usize)> {
    // Suite build dir is per-result so parallel/rerun don't collide.
    let build_dir = report_json.parent().unwrap_or(Path::new(".")).join("gtest_build");
    let _ = Command::new(runner)
        .arg("run")
        .args(["--suite".as_ref(), suite_dir.as_os_str()])
        .args(["--lib".as_ref(), lib.as_os_str()])
        .args(["--build-dir".as_ref(), build_dir.as_os_str()])
        .args(["--json".as_ref(), report_json.as_os_str()])
        .output()
        .context("invoking harvest-bench runner")?;

    // Parse `{"run": {"verdicts": [{"passed": bool, "skipped": bool}, ...]}}`.
    //
    // If the runner produced no report at all (e.g. the gtest suite failed to
    // build, the cdylib is missing/incompatible, cmake choked, etc.), return a
    // clean zero-score result instead of erroring out the whole `run` command
    // — a scoring failure should record a failed case (build_ok already False
    // from build_harvest_bench_lib caller), not abort the sweep. Same for a
    // truncated/malformed report.
    let Ok(data) = std::fs::read_to_string(report_json) else {
        eprintln!("⚠️  harvest-bench runner produced no report {} — recording 0 tests", report_json.display());
        return Ok((0, 0, 0));
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else {
        eprintln!("⚠️  harvest-bench runner report at {} is not valid JSON — recording 0 tests", report_json.display());
        return Ok((0, 0, 0));
    };
    let verdicts = json.pointer("/run/verdicts").and_then(|v| v.as_array());
    let Some(verdicts) = verdicts else { return Ok((0, 0, 0)) };

    let mut ok = 0usize; let mut failed = 0usize; let mut skipped = 0usize;
    for v in verdicts {
        let passed = v.get("passed").and_then(|b| b.as_bool()).unwrap_or(false);
        let skip = v.get("skipped").and_then(|b| b.as_bool()).unwrap_or(false);
        if skip { skipped += 1; }
        else if passed { ok += 1; }
        else { failed += 1; }
    }
    Ok((ok, failed, skipped))
}

/// Load stored harvest-bench result.json files as a baseline for --check.
fn load_harvest_bench_stored(paths: &Paths) -> std::collections::BTreeMap<String, HarvestBenchResult> {
    let mut map = std::collections::BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(&paths.results_dir) else { return map };
    for entry in entries.filter_map(|e| e.ok()) {
        if !entry.path().is_dir() { continue; }
        // Single-phase: score lives in translated/result.json (reader rule).
        let rj = crate::battery::crate_dir(&entry.path()).join("result.json");
        if let Ok(data) = std::fs::read_to_string(&rj) {
            if let Ok(r) = serde_json::from_str::<HarvestBenchResult>(&data) {
                map.insert(entry.file_name().to_string_lossy().into_owned(), r);
            }
        }
    }
    map
}

pub fn run_harvest_bench_test(
    paths: &Paths,
    projects: &[crate::battery::HarvestBenchProject],
    mode: TestMode,
) -> Result<TestOutcome> {
    let runner = harvest_bench_runner(&paths.corpus_dir)?;
    let stored = load_harvest_bench_stored(paths);

    let mut results: std::collections::BTreeMap<String, HarvestBenchResult> = Default::default();
    let mut passed = 0usize;
    let mut build_failed = 0usize;

    for project in projects {
        let name = project.name();
        let case_dir = paths.output_dir(name);
        // Score the canonical crate: verified/ if verify produced a valid one,
        // else translated/ (the reader rule). This handles both single-phase
        // (no verify → only translated/) and two-phase (verify ran → verified/,
        // or verify broke the crate → compile-gate discarded verified/, fallback).
        let crate_dir = crate::battery::crate_dir(&case_dir);
        if !crate_dir.join("Cargo.toml").exists() { continue; }

        let logs_dir = crate_dir.join("logs");
        std::fs::create_dir_all(&logs_dir)?;

        let (so, build_log) = build_harvest_bench_lib(&crate_dir, name);
        std::fs::write(logs_dir.join("test.log"), &build_log)?;

        let r = match so {
            None => {
                build_failed += 1;
                println!("  ❌ {name}: build failed (no cdylib)");
                HarvestBenchResult { tests_ok: 0, tests_failed: 0, tests_skipped: 0, build_ok: false }
            }
            Some(so) => {
                let report = crate_dir.join("harvest_bench_report.json");
                let (ok, fail, skip) = score_harvest_bench_suite(&runner, project.gtest_suite(), &so, &report)?;
                let res = HarvestBenchResult { tests_ok: ok, tests_failed: fail, tests_skipped: skip, build_ok: true };
                if res.passed() {
                    passed += 1;
                    println!("  ✅ {name}: {ok} ok, {skip} skipped");
                } else if fail > 0 {
                    println!("  ⚠️  {name}: {ok} ok, {fail} FAILED, {skip} skipped");
                } else {
                    println!("  ⚠️  {name}: no tests passed");
                }
                res
            }
        };

        if matches!(mode, TestMode::Update) {
            let mut json = serde_json::to_value(&r)?;
            let tlog = logs_dir.join("translation.log");
            Enrichment::compute(&crate_dir.join("src"), &[("translate", &tlog)]).merge_into(&mut json);
            std::fs::write(crate_dir.join("result.json"), serde_json::to_string_pretty(&json)? + "\n")?;
        }

        results.insert(name.to_string(), r);
    }

    let total = results.len();
    println!("\nharvest-bench: {passed}/{total} projects pass ({build_failed} build failures)");

    match mode {
        TestMode::Update => {
            println!("📝 result.json written for {total} projects");
            Ok(TestOutcome::Ok)
        }
        TestMode::Check => {
            let mut diffs = Vec::new();
            for (name, actual) in &results {
                match stored.get(name) {
                    None => diffs.push(format!("{name}: missing stored result")),
                    Some(exp) => {
                        if actual.tests_ok < exp.tests_ok {
                            diffs.push(format!("{name}: tests_ok expected={} actual={}", exp.tests_ok, actual.tests_ok));
                        }
                        if actual.tests_failed > exp.tests_failed {
                            diffs.push(format!("{name}: tests_failed expected={} actual={}", exp.tests_failed, actual.tests_failed));
                        }
                        if exp.build_ok && !actual.build_ok {
                            diffs.push(format!("{name}: build_ok expected=true actual=false"));
                        }
                    }
                }
            }
            if diffs.is_empty() {
                println!("✅ No regressions");
                Ok(TestOutcome::Passed)
            } else {
                println!("\n❌ {} regression(s):", diffs.len());
                for d in &diffs { println!("  {d}"); }
                Ok(TestOutcome::Failed(vec![BatteryMismatch { battery: "harvest-bench".into(), diffs }]))
            }
        }
        TestMode::Run => Ok(TestOutcome::Ok),
    }
}

pub fn enrich_test_corpus(paths: &Paths, battery: &str) -> Result<()> {
    let output_dir = paths.results_dir.join(battery);
    if !output_dir.is_dir() { return Ok(()); }
    let mut enriched = 0usize;
    for entry in std::fs::read_dir(&output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let case_dir = entry.path();
        // Enrich each phase dir's own result.json in place; enrich_file no-ops
        // on absent files, so single-phase cases (only translated/) just skip
        // verified/. Each phase's result.json is enriched against its own crate.
        for phase in [crate::battery::TRANSLATED, crate::battery::VERIFIED] {
            let pdir = crate::battery::phase_dir(&case_dir, phase);
            let tlog = pdir.join("logs/translation.log");
            let vlog = pdir.join("logs/verify.log");
            if Enrichment::enrich_file(
                &pdir.join("result.json"),
                &pdir.join("src"),
                &[("translate", &tlog), ("verify", &vlog)],
            )? { enriched += 1; }
        }
    }
    println!("✅ Enriched {enriched} {battery} result.json files");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The whole point of Tier 1: `merge_into` and `check_enrichment` are
    /// inverses. Enrich a fresh result.json, then check it — zero diffs. This
    /// is the invariant that used to be maintained by hand across 7 sites.
    #[test]
    fn merge_into_then_check_has_no_diffs() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("lib.rs"),
            "pub fn f() { unsafe { let _p = 1u8 as *const u8; } }\npub fn g() {}\n").unwrap();

        // No logs on disk → no meta phases; a claude-family agent (no credits).
        let rj = tmp.path().join("result.json");
        let mut json = serde_json::json!({"passed": true});
        let missing = tmp.path().join("nope.log");
        Enrichment::compute(&src, &[("translate", &missing)]).merge_into(&mut json);
        fs::write(&rj, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let diffs = check_enrichment(&rj, &src, &[("translate", &missing)], crate::cli::Agent::Claude);
        assert!(diffs.is_empty(), "merge_into output should pass its own check: {diffs:?}");

        // And it actually recorded the unsafe block + loc (not a vacuous pass).
        let stored: serde_json::Value = serde_json::from_str(&fs::read_to_string(&rj).unwrap()).unwrap();
        assert_eq!(stored["unsafe"]["blocks"], 1);
        assert!(stored["loc"]["code"].as_u64().unwrap() >= 2);
    }

    /// Tampering with a stored field is caught by check — proving check isn't
    /// vacuously empty.
    #[test]
    fn check_detects_tampered_unsafe_count() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("lib.rs"), "pub fn f() { unsafe { let _x = 0; } }\n").unwrap();

        let rj = tmp.path().join("result.json");
        let mut json = serde_json::json!({});
        let _missing = tmp.path().join("nope.log");
        Enrichment::compute(&src, &[]).merge_into(&mut json);
        json["unsafe"]["blocks"] = serde_json::json!(99); // tamper
        fs::write(&rj, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let diffs = check_enrichment(&rj, &src, &[], crate::cli::Agent::Claude);
        assert!(diffs.iter().any(|d| d.contains("unsafe.blocks")), "tamper should be caught: {diffs:?}");
    }
}
