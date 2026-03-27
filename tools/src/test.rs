use crate::battery::Paths;
use crate::translate::copy_dir_all;
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Summary {
    pub cases_tested: usize,
    pub cases_passed: usize,
    pub vectors_passed: usize,
    pub vectors_failed: usize,
    pub vectors_skipped: usize,
    pub failed_cases: Vec<String>,
}

pub fn run(repo_root: &Path, battery_name: &str, update: bool) -> Result<()> {
    let paths = Paths::new(repo_root);
    let output_dir = paths.output_dir(battery_name);

    println!();
    println!("========================================");
    println!("  Testing translations");
    println!("========================================");

    // Copy test_vectors and runner from corpus
    copy_test_artifacts(&paths, battery_name)?;

    // Generate workspace Cargo.toml for lib runners
    generate_workspace(&output_dir)?;

    // Run MIT runtests
    let (summary, per_case) = run_runtests(&paths, battery_name)?;

    if update {
        // Write per-case result.json
        for (case_name, data) in &per_case {
            let case_dir = output_dir.join(case_name);
            if case_dir.is_dir() {
                let result_path = case_dir.join("result.json");
                let json = serde_json::to_string_pretty(data)?;
                std::fs::write(&result_path, format!("{json}\n"))?;
            }
        }

        // Write battery summary.json
        let summary_path = output_dir.join("summary.json");
        let json = serde_json::to_string_pretty(&summary)?;
        std::fs::write(&summary_path, format!("{json}\n"))?;

        let vt = summary.vectors_passed + summary.vectors_failed;
        println!("   📝 Updated: {}/{} cases, {}/{vt} vectors",
            summary.cases_passed, summary.cases_tested, summary.vectors_passed);
    } else {
        // Compare against stored
        let expected = load_summary(&output_dir);
        let evt = expected.vectors_passed + expected.vectors_failed;
        let avt = summary.vectors_passed + summary.vectors_failed;
        println!("   stored:  {}/{} cases, {}/{evt} vectors",
            expected.cases_passed, expected.cases_tested, expected.vectors_passed);
        println!("   actual:  {}/{} cases, {}/{avt} vectors",
            summary.cases_passed, summary.cases_tested, summary.vectors_passed);

        let errors = check_battery(&expected, &summary);
        if errors.is_empty() {
            println!("   ✅ OK");
        } else {
            println!("   ❌ MISMATCH: {}", errors.join("; "));
        }
    }

    // Print overall summary
    let vt = summary.vectors_passed + summary.vectors_failed;
    let pct = if vt > 0 {
        format!("{:.1}%", 100.0 * summary.vectors_passed as f64 / vt as f64)
    } else {
        "N/A".to_string()
    };
    println!();
    println!("========================================");
    println!("  {battery_name}: {}/{} cases, {}/{vt} vectors ({pct})",
        summary.cases_passed, summary.cases_tested, summary.vectors_passed);
    println!("========================================");

    Ok(())
}

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

fn run_runtests(paths: &Paths, battery: &str) -> Result<(Summary, HashMap<String, serde_json::Value>)> {
    let output_dir = paths.output_dir(battery);
    let scripts_dir = paths.corpus_dir.join("deployment/scripts/github-actions");

    let mut pythonpath = scripts_dir.to_string_lossy().to_string();
    if let Ok(existing) = std::env::var("PYTHONPATH") {
        pythonpath = format!("{pythonpath}:{existing}");
    }

    let output = Command::new("python3")
        .args(["-m", "runtests.rust", "--root", &output_dir.to_string_lossy(), "--subset", &output_dir.to_string_lossy(), "--keep-going"])
        .env("PYTHONPATH", &pythonpath)
        .current_dir(&paths.corpus_dir)
        .output()
        .context("running MIT runtests")?;

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{text}");

    // Parse summary
    let extract = |pattern: &str| -> usize {
        Regex::new(pattern)
            .ok()
            .and_then(|re| re.captures(&text))
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0)
    };

    let cases_discovered = extract(r"Test Cases Discovered:\s+(\d+)");
    let cases_tested = extract(r"Test Cases Tested:\s+(\d+)");
    let cases_failed = extract(r"Test Cases Failed:\s+(\d+)");
    let cases_skipped = extract(r"Test Cases Skipped:\s+(\d+)");
    let vectors_passed = extract(r"Test Vectors Passed:\s+(\d+)");
    let vectors_failed = extract(r"Test Vectors Failed:\s+(\d+)");
    let vectors_skipped = extract(r"Test Vectors Skipped:\s+(\d+)");

    // cases_failed includes build failures (not in cases_tested), so compute from discovered
    let cases_passed = cases_discovered.saturating_sub(cases_failed + cases_skipped);

    // Parse per-case failures
    let fail_re = Regex::new(r"^- (\S+): Test failed")?;
    let mut failed_cases: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(caps) = fail_re.captures(line) {
            let name = caps[1].to_string();
            if !failed_cases.contains(&name) {
                failed_cases.push(name);
            }
        }
    }
    failed_cases.sort();

    // Parse per-case results
    let exec_re = Regex::new(r"Executing (\S+)")?;
    let mut per_case = HashMap::new();
    for caps in exec_re.captures_iter(&text) {
        let case = caps[1].to_string();
        let vf = if failed_cases.contains(&case) { 1 } else { 0 };
        per_case.insert(
            case.clone(),
            serde_json::json!({
                "case": case,
                "battery": battery,
                "vectors_failed": vf,
                "passed": vf == 0,
            }),
        );
    }

    let summary = Summary {
        cases_tested,
        cases_passed,
        vectors_passed,
        vectors_failed,
        vectors_skipped,
        failed_cases,
    };

    Ok((summary, per_case))
}

fn load_summary(output_dir: &Path) -> Summary {
    let path = output_dir.join("summary.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn check_battery(expected: &Summary, actual: &Summary) -> Vec<String> {
    let mut errors = Vec::new();
    if actual.vectors_passed != expected.vectors_passed {
        errors.push(format!("vectors_passed: {} -> {}", expected.vectors_passed, actual.vectors_passed));
    }
    if actual.vectors_failed != expected.vectors_failed {
        errors.push(format!("vectors_failed: {} -> {}", expected.vectors_failed, actual.vectors_failed));
    }
    if actual.cases_passed != expected.cases_passed {
        errors.push(format!("cases_passed: {} -> {}", expected.cases_passed, actual.cases_passed));
    }
    if actual.cases_tested != expected.cases_tested {
        errors.push(format!("cases_tested: {} -> {}", expected.cases_tested, actual.cases_tested));
    }
    errors
}
