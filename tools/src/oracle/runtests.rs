use super::{
    check_enrichment, openssl_dir, BatteryMismatch, Enrichment, Summary, TestMode, TestOutcome,
};
use crate::battery::Paths;
use crate::translate::copy_dir_all;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        println!(
            "  {:<25} {:>15} {:>15}  Status",
            "Battery", "Stored", "Actual"
        );
        println!("  {}", "─".repeat(75));
        for row in &check_rows {
            let stored = format!(
                "{}/{} ({}v)",
                row.expected.cases_passed, row.expected.cases_tested, row.expected.vectors_passed
            );
            let actual = format!(
                "{}/{} ({}v)",
                row.actual.cases_passed, row.actual.cases_tested, row.actual.vectors_passed
            );
            let status = if row.ok { "✅" } else { "❌" };
            println!(
                "  {:<25} {:>15} {:>15}  {}",
                row.battery, stored, actual, status
            );
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

fn run_battery(
    paths: &Paths,
    battery: &str,
    mode: TestMode,
    check_rows: &mut Vec<CheckRow>,
) -> Result<TestOutcome> {
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
    let _guard = TestArtifactGuard {
        output_dir: output_dir.clone(),
    };

    generate_workspace(&output_dir)?;

    let has_verified = std::fs::read_dir(&output_dir)?
        .filter_map(|e| e.ok())
        .any(|e| {
            crate::battery::has_crate(&crate::battery::phase_dir(
                &e.path(),
                crate::battery::VERIFIED,
            ))
        });

    // Order matters: the LAST phase scored becomes the headline summary, so
    // verified/ must follow translated/. Each pass stages `translated_rust` at
    // its phase dir so unmodified runtests scores that crate. translated/ is
    // scored unconditionally, which is what makes the headline unconditional.
    let mut headline = score_phase(
        paths,
        battery,
        &output_dir,
        crate::battery::TRANSLATED,
        mode,
    )?;
    if has_verified {
        headline = score_phase(paths, battery, &output_dir, crate::battery::VERIFIED, mode)?;
    }
    println!("========================================");

    let (summary, per_case) = headline;

    match mode {
        TestMode::Update => {
            let vt = summary.vectors_passed + summary.vectors_failed;
            println!(
                "   📝 Updated: {}/{} cases, {}/{vt} vectors",
                summary.cases_passed, summary.cases_tested, summary.vectors_passed
            );
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
        TestMode::Run => Ok(TestOutcome::Ok),
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
    } else {
        "N/A".to_string()
    };
    println!(
        "  {battery} [{phase}]: {}/{} cases, {}/{vt} vectors ({pct})",
        summary.cases_passed, summary.cases_tested, summary.vectors_passed
    );

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
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let case_dir = entry.path();
        let phase_path = crate::battery::phase_dir(&case_dir, phase);
        if !crate::battery::has_crate(&phase_path) {
            continue;
        }
        let link = case_dir.join(crate::battery::TRANSLATED_RUST);
        if link.is_symlink() || link.exists() {
            let _ = std::fs::remove_file(&link);
            if link.is_dir() {
                let _ = std::fs::remove_dir_all(&link);
            }
        }
        std::os::unix::fs::symlink(phase, &link)?;
        staged += 1;
    }
    Ok(staged)
}

fn unstage_phase(output_dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
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
        if !entry.file_type()?.is_dir() {
            continue;
        }
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

fn run_runtests(
    paths: &Paths,
    battery: &str,
    mode: TestMode,
) -> Result<(Summary, HashMap<String, serde_json::Value>)> {
    let output_dir = paths.output_dir(battery);
    let scripts_dir = paths.corpus_dir.join("deployment/scripts/github-actions");

    let mut pythonpath = scripts_dir.to_string_lossy().to_string();
    if let Ok(existing) = std::env::var("PYTHONPATH") {
        pythonpath = format!("{pythonpath}:{existing}");
    }

    let output = Command::new("python3")
        .args([
            "-m",
            "runtests.rust",
            "--root",
            &output_dir.to_string_lossy(),
            "--subset",
            &output_dir.to_string_lossy(),
            "--keep-going",
            "--verbose",
        ])
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
            if !failed_cases.contains(&name) {
                failed_cases.push(name.clone());
            }
            per_case.insert(
                name.clone(),
                serde_json::json!({
                    "case": name, "battery": battery,
                    "vectors_failed": 1, "passed": false,
                    "error": "build failed",
                }),
            );
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
            let (expected_rc, actual_rc) = rc_re
                .captures(&body)
                .map(|c| {
                    (
                        c[1].parse::<i64>().unwrap_or(-1),
                        c[2].parse::<i64>().unwrap_or(-1),
                    )
                })
                .unwrap_or((-1, -1));

            let diff = body
                .lines()
                .filter(|l| !rc_re.is_match(l))
                .collect::<Vec<_>>()
                .join("\n");
            let diff = diff.trim().to_string();

            // e.g. "stdout mismatch", "stderr mismatch, return code mismatch".
            let reason = reason_first_line.trim_end_matches(',').trim().to_string();

            case_vector_fails
                .entry(name.clone())
                .or_default()
                .push(serde_json::json!({
                    "vector": vector,
                    "reason": reason,
                    "expected_rc": expected_rc,
                    "actual_rc": actual_rc,
                    "diff": diff,
                }));
            if !failed_cases.contains(&name) {
                failed_cases.push(name.clone());
            }
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
            if !failed_cases.contains(&name) {
                failed_cases.push(name.clone());
            }
            case_vector_fails.entry(name).or_insert_with(|| {
                vec![serde_json::json!({
                    "vector": "unknown",
                    "reason": "test failed (no vector-level detail)",
                    "expected_rc": -1,
                    "actual_rc": -1,
                    "diff": "",
                })]
            });
        }
    }

    for (name, failures) in case_vector_fails {
        per_case.insert(
            name.clone(),
            serde_json::json!({
                "case": name,
                "battery": battery,
                "vectors_failed": failures.len(),
                "passed": false,
                "error": "test failed",
                "failures": failures,
            }),
        );
    }

    // Executed and not already recorded as failed ⇒ passed.
    let exec_re = Regex::new(r"Executing (\S+)")?;
    for caps in exec_re.captures_iter(&text) {
        let name = caps[1].to_string();
        per_case.entry(name.clone()).or_insert_with(|| {
            serde_json::json!({
                "case": name, "battery": battery,
                "vectors_failed": 0, "passed": true,
            })
        });
    }

    failed_cases.sort();
    let cases_passed = cases_discovered.saturating_sub(failed_cases.len());

    Ok((
        Summary {
            cases_tested: cases_discovered,
            cases_passed,
            vectors_passed,
            vectors_failed,
            vectors_skipped,
            failed_cases,
        },
        per_case,
    ))
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
            )
            .merge_into(&mut val);
            let json = serde_json::to_string_pretty(&val)?;
            std::fs::write(phase_dir.join("result.json"), format!("{json}\n"))?;
        }
    }
    let json = serde_json::to_string_pretty(summary)?;
    std::fs::write(output_dir.join(summary_file(phase)), format!("{json}\n"))?;
    Ok(())
}

fn summary_file(phase: &str) -> &'static str {
    if phase == crate::battery::VERIFIED {
        "summary.json"
    } else {
        "summary_translated.json"
    }
}

fn load_summary(output_dir: &Path) -> Summary {
    let verified = output_dir.join("summary.json");
    let path = if verified.exists() {
        verified
    } else {
        output_dir.join("summary_translated.json")
    };
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
                diffs.push(format!(
                    "{}: {} → {}",
                    stringify!($field),
                    expected.$field,
                    actual.$field
                ));
            }
        };
    }
    cmp!(vectors_passed);
    cmp!(vectors_failed);
    cmp!(cases_passed);
    cmp!(cases_tested);
    let added: Vec<_> = actual
        .failed_cases
        .iter()
        .filter(|c| !expected.failed_cases.contains(c))
        .collect();
    let removed: Vec<_> = expected
        .failed_cases
        .iter()
        .filter(|c| !actual.failed_cases.contains(c))
        .collect();
    if !added.is_empty() {
        diffs.push(format!(
            "new failures: {}",
            added
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !removed.is_empty() {
        diffs.push(format!(
            "no longer failing: {}",
            removed
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    diffs
}
