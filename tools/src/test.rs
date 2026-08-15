use crate::battery::Paths;
use crate::translate::copy_dir_all;
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum TestMode {
    Run,
    /// Run, then write summary.json / result.json.
    Update,
    /// Run, then compare against stored summary.json. Returns failure on mismatch.
    Check,
}

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

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Summary {
    pub cases_tested: usize,
    pub cases_passed: usize,
    pub vectors_passed: usize,
    pub vectors_failed: usize,
    pub vectors_skipped: usize,
    pub failed_cases: Vec<String>,
}

struct TestArtifactGuard {
    output_dir: PathBuf,
}

impl Drop for TestArtifactGuard {
    fn drop(&mut self) {
        let _ = cleanup_test_artifacts(&self.output_dir);
    }
}

// ── Public API ─────────────────────────────────────────────────────────

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

/// Translated crates that pull in `openssl-sys` otherwise fail to build for
/// environmental reasons unrelated to the translation.
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

    copy_test_artifacts(paths, battery)?;
    let _guard = TestArtifactGuard { output_dir: output_dir.clone() };

    generate_workspace(&output_dir)?;

    let has_verified = std::fs::read_dir(&output_dir)?.filter_map(|e| e.ok())
        .any(|e| crate::battery::phase_dir(&e.path(), crate::battery::VERIFIED).join("Cargo.toml").exists());

    // Order matters: the LAST phase scored becomes the headline summary, so
    // verified/ must follow translated/. Each pass stages `translated_rust` at
    // its phase dir so unmodified runtests scores that crate. translated/ is
    // scored unconditionally, which is what makes the headline unconditional.
    let mut headline = score_phase(paths, battery, &output_dir, crate::battery::TRANSLATED, mode)?;
    if has_verified {
        headline = score_phase(paths, battery, &output_dir, crate::battery::VERIFIED, mode)?;
    }
    println!("========================================");

    let (summary, per_case) = headline;

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

/// Scores ONE phase's crate and, under `--update`, writes that phase's results.
/// Returning the summary by value (rather than accumulating an `Option` across a
/// phase loop) is what lets `run_battery` name its headline without unwrapping.
fn score_phase(
    paths: &Paths,
    battery: &str,
    output_dir: &Path,
    phase: &str,
    mode: TestMode,
) -> Result<(Summary, HashMap<String, serde_json::Value>)> {
    stage_phase_for_runtests(output_dir, phase)?;
    clean_targets(output_dir)?;
    let (summary, per_case) = run_runtests(paths, battery, mode)?;
    unstage_phase(output_dir)?;

    let vt = summary.vectors_passed + summary.vectors_failed;
    let pct = if vt > 0 {
        format!("{:.1}%", 100.0 * summary.vectors_passed as f64 / vt as f64)
    } else { "N/A".to_string() };
    println!("  {battery} [{phase}]: {}/{} cases, {}/{vt} vectors ({pct})",
        summary.cases_passed, summary.cases_tested, summary.vectors_passed);

    if matches!(mode, TestMode::Update) {
        write_results(output_dir, phase, &summary, &per_case)?;
    }
    Ok((summary, per_case))
}

// ── runtests phase staging ─────────────────────────────────────────────
//
// MIT's `runtests` hardcodes each case's crate at `<case>/translated_rust/`
// (test-corpus/.../discovery/rust.py), while canonical storage uses
// `translated/`/`verified/`. To score a phase WITHOUT modifying runtests,
// `<case>/translated_rust` is symlinked to `<case>/<phase>`; runtests calls
// `.resolve()`, so it builds that phase's crate. TestArtifactGuard removes it.

fn stage_phase_for_runtests(output_dir: &Path, phase: &str) -> Result<usize> {
    let mut staged = 0usize;
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let case_dir = entry.path();
        let phase_path = crate::battery::phase_dir(&case_dir, phase);
        if !phase_path.join("Cargo.toml").exists() { continue; }
        let link = case_dir.join(crate::battery::TRANSLATED_RUST);
        if link.is_symlink() || link.exists() {
            let _ = std::fs::remove_file(&link);
            if link.is_dir() { let _ = std::fs::remove_dir_all(&link); }
        }
        std::os::unix::fs::symlink(phase, &link)?;
        staged += 1;
    }
    Ok(staged)
}

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

        let tv_src = corpus_case.join("test_vectors");
        let tv_dst = case_dir.join("test_vectors");
        if tv_src.is_dir() && !tv_dst.exists() {
            copy_dir_all(&tv_src, &tv_dst)?;
        }

        let runner_src = corpus_case.join("runner");
        let runner_dst = case_dir.join("runner");
        if runner_src.is_dir() && !runner_dst.exists() {
            copy_dir_all(&runner_src, &runner_dst)?;

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
        let link = entry.path().join(crate::battery::TRANSLATED_RUST);
        if link.is_symlink() {
            let _ = std::fs::remove_file(&link);
        }
    }
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

    // runtests' output grammar: failures are "- NAME: Build failed ..." or
    // "- NAME: Test failed ...", executed cases are "Executing NAME". One "Test
    // failed" is ONE vector, and opens a multi-line block:
    //   - NAME: Test failed (testN: REASON
    //   <diff lines>
    //   expected rc=A, actual rc=B
    //   )
    // Failures are accumulated per vector so vectors_failed is exact and the diff
    // snippets land in result.json rather than only in the battery test.log.
    let mut per_case: HashMap<String, serde_json::Value> = HashMap::new();
    let mut failed_cases: Vec<String> = Vec::new();

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

    // Consecutive blocks can belong to the same case, one per failed vector.
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

            // Unbounded scan is fine: blocks run ~10 lines.
            let start = i + 1;
            let mut end = start;
            while end < lines.len() && lines[end].trim() != ")" {
                end += 1;
            }
            let body = lines[start..end].join("\n");
            let (expected_rc, actual_rc) = rc_re.captures(&body)
                .map(|c| (c[1].parse::<i64>().unwrap_or(-1), c[2].parse::<i64>().unwrap_or(-1)))
                .unwrap_or((-1, -1));

            let diff = body.lines()
                .filter(|l| !rc_re.is_match(l))
                .collect::<Vec<_>>()
                .join("\n");
            let diff = diff.trim().to_string();

            // e.g. "stdout mismatch", "stderr mismatch, return code mismatch".
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

    // A case can fail with no vector-level "(testN:" block at all (timeout, build
    // mid-run); record a generic 1-vector failure so it is not counted as passing.
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

    // Executed and not already recorded as failed ⇒ passed.
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

/// Each case's result.json lands INSIDE `<case>/<phase>/`, co-located with the
/// crate it scores. The battery summary is split by phase filename so report.rs
/// can read the headline and no-validate numbers independently.
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

fn summary_file(phase: &str) -> &'static str {
    if phase == crate::battery::VERIFIED { "summary.json" } else { "summary_translated.json" }
}

fn load_summary(output_dir: &Path) -> Summary {
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
// INVARIANT: every writer of result.json metadata goes through `merge_into`,
// and [`check_enrichment`] stays a pure inverse of it. Duplicating either side
// is how a stored result.json drifts from what `test --check` recomputes.
pub struct Enrichment {
    unsafe_: crate::battery::UnsafeCounts,
    loc: crate::battery::LocCounts,
    /// Only phases whose log existed, in the given key order.
    meta: Vec<(String, crate::battery::AgentRunMeta)>,
}

impl Enrichment {
    /// `logs` maps each result.json phase key to that phase's agent log.
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

    /// `json` must be an object, or a null (which serde_json promotes to an empty
    /// object): assigning through `json[key]` panics on any other value. Both
    /// in-process callers build their object here from a struct or a `json!`
    /// literal; the one caller that reads a `Value` off disk checks first, in
    /// [`Self::enrich_file`].
    ///
    /// The `to_value` calls cannot fail: every field of `UnsafeCounts`,
    /// `LocCounts` and `AgentRunMeta` is a derived-`Serialize` struct of integers,
    /// floats, strings and `Option`/`Vec` thereof, so there is no map with
    /// non-string keys and no hand-written `Serialize` to return an error.
    pub fn merge_into(&self, json: &mut serde_json::Value) {
        json["unsafe"] = serde_json::to_value(&self.unsafe_).unwrap();
        json["loc"] = serde_json::to_value(&self.loc).unwrap();
        for (key, m) in &self.meta {
            json[key] = serde_json::to_value(m).unwrap();
        }
    }

    fn enrich_file(rj: &Path, src_dir: &Path, logs: &[(&str, &Path)]) -> Result<bool> {
        if !rj.exists() { return Ok(false); }
        let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(rj)?)?;
        // The only [`Self::merge_into`] caller whose value comes off disk, so the
        // only one that can hand it a scalar or an array — which `json[key] = ..`
        // panics on. A result.json that parses but is not an object is corrupt
        // input for one case; report it with the path instead of panicking.
        anyhow::ensure!(
            matches!(json, serde_json::Value::Object(_) | serde_json::Value::Null),
            "{}: result.json must hold a JSON object",
            rj.display(),
        );
        Self::compute(src_dir, logs).merge_into(&mut json);
        std::fs::write(rj, serde_json::to_string_pretty(&json)? + "\n")?;
        Ok(true)
    }
}

/// Pure inverse of [`Enrichment::merge_into`]; returns mismatch descriptions.
/// `agent` gates the "missing meta" check to kiro, the only agent that records
/// credits.
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

    match json.get("loc") {
        Some(stored) => {
            let sc = stored.get("code").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if sc != live.loc.code { diffs.push(format!("loc.code expected={sc} actual={}", live.loc.code)); }
        }
        None => diffs.push("missing loc field".into()),
    }

    // `live.meta` is filtered and keyed exactly as merge_into's, so a phase whose
    // log is absent is simply not compared.
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

/// `passed` defers to the canonical project pass rule in `crate::scoring`, so
/// the pass column means the same thing across datasets.
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

/// `corpus_dir` is `harvest-bench/tests`, hence the `.parent()`.
fn harvest_bench_runner(corpus_dir: &Path) -> Result<PathBuf> {
    let bin = corpus_dir
        .parent().context("harvest-bench/tests has no parent")?
        .join("runner/target/release/harvest-bench");
    anyhow::ensure!(bin.is_file(),
        "harvest-bench runner not built: {} (run `cargo build --release --manifest-path harvest-bench/runner/Cargo.toml`)",
        bin.display());
    Ok(bin)
}

/// The gtest suite links `lib<name>.so` by ABI, so the name must match exactly.
fn build_harvest_bench_lib(crate_dir: &Path, name: &str) -> (Option<PathBuf>, String) {
    let out = Command::new("timeout")
        .args(["600", "cargo", "build", "--release"])
        .env("OPENSSL_DIR", openssl_dir())
        .env("OPENSSL_NO_VENDOR", "1")
        .current_dir(crate_dir)
        .output();
    let Ok(out) = out else { return (None, "failed to spawn cargo build".into()) };
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // cargo normalizes `-`→`_` in the cdylib output name.
    let lib_stem = name.replace('-', "_");
    let so = crate_dir.join(format!("target/release/lib{lib_stem}.so"));
    if so.is_file() { (Some(so), stderr) } else { (None, stderr) }
}

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

    // A missing or malformed report (gtest suite failed to build, cdylib
    // incompatible, cmake choked) must record a zero-score case, not abort the
    // whole sweep with an error.
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
        // Reader rule: verified/ if verify produced a valid crate, else
        // translated/ — which also covers verify breaking the crate, since the
        // compile gate then discards verified/ entirely.
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
        // Each phase's result.json is enriched against ITS OWN crate. enrich_file
        // no-ops on absent files, so single-phase cases just skip verified/.
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

    /// Guards the `merge_into` / `check_enrichment` inverse invariant.
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

    /// Moving the scorer's build out of `results/` is measurement-neutral only if every
    /// file the scorer writes back into a scored crate is outside the digest and outside
    /// every [`crate::artifact::Carry`]. Enumerated beside the writers: `write_results`
    /// and `run_harvest_bench_test` (result.json, logs/test.log),
    /// `score_harvest_bench_suite` (harvest_bench_report.json, gtest_build/) and
    /// `build_harvest_bench_lib` (target/).
    #[test]
    fn every_file_the_scorer_writes_back_is_excluded_from_the_artifact() {
        use crate::artifact::{classify, Disposition, RelPath};
        let of = |p: &str| classify(&RelPath::new(p).unwrap(), false);

        for written in ["result.json", "harvest_bench_report.json", "logs/test.log"] {
            assert_eq!(
                of(written),
                Disposition::Ignore,
                "{written} is written into the crate being scored, so scoring would \
                 change that crate's identity"
            );
        }
        assert_eq!(of("gtest_build/CMakeCache.txt"), Disposition::BuildOutput);
        assert_eq!(of("target/release/libfoo.so"), Disposition::BuildOutput);

        // The one write a scored build makes that IS hashed — hence why the build has to
        // move out of the tree, and not merely its reports.
        assert_eq!(of("Cargo.lock"), Disposition::StoreAndHash);
    }

    /// Proves the check above is not vacuously empty.
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
