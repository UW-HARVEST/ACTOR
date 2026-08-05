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

impl CrustTestResult {
    /// THE canonical CRUST pass rule (see `crate::scoring::CrustOutcome::passed`):
    /// at least one ground-truth test passed and none failed. A crate that builds
    /// but runs zero tests is NOT a pass. Kept identical to the report generator
    /// and the baselines so ACTOR is never scored more leniently than its rivals.
    fn passed(&self) -> bool {
        crate::scoring::CrustOutcome {
            built: self.build_ok,
            tests_ok: self.tests_ok as u32,
            tests_failed: self.tests_failed as u32,
        }
        .passed()
    }
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

/// Default OpenSSL location for `openssl-sys` builds. Projects like
/// `c_blind_rsa_signatures` depend on it; without this set the build
/// fails for environmental reasons unrelated to the translation.
fn openssl_dir() -> String {
    std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into())
}

/// Extract the body of a Cargo.toml `[dependencies]` table (the lines after the
/// header up to the next `[section]` or EOF), trimmed. Empty if absent.
fn extract_dependencies(toml: &str) -> String {
    let mut out = Vec::new();
    let mut in_deps = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            continue;
        }
        if in_deps && !t.is_empty() {
            out.push(line);
        }
    }
    out.join("\n")
}

/// Merge a scaffold Cargo.toml (authoritative for package metadata and
/// `[[test]]`/`[[bin]]` targets) with the agent's `[dependencies]`. When the
/// scaffold's dependency table is empty but the agent declared crates, keep the
/// agent's — otherwise the real-test build fails to resolve those crates even
/// though the translation is correct. If the scaffold already lists
/// dependencies, it wins (it is the ground truth for that project).
fn merge_cargo_deps(scaffold: &str, agent: &str) -> String {
    let scaffold_deps = extract_dependencies(scaffold);
    if !scaffold_deps.is_empty() {
        return scaffold.to_string();
    }
    let agent_deps = extract_dependencies(agent);
    if agent_deps.is_empty() {
        return scaffold.to_string();
    }
    // Replace the scaffold's (empty) [dependencies] block with the agent's,
    // or append one if the scaffold has no [dependencies] section at all.
    if scaffold.contains("[dependencies]") {
        let mut result = Vec::new();
        let mut in_deps = false;
        for line in scaffold.lines() {
            let t = line.trim();
            if t == "[dependencies]" {
                result.push(line.to_string());
                result.push(agent_deps.clone());
                in_deps = true;
                continue;
            }
            if in_deps {
                // skip the scaffold's (empty) dep lines until the next section
                if t.starts_with('[') { in_deps = false; } else { continue; }
            }
            result.push(line.to_string());
        }
        result.join("\n") + "\n"
    } else {
        format!("{}\n\n[dependencies]\n{}\n", scaffold.trim_end(), agent_deps)
    }
}

/// Run cargo test on a single CRUST project, return typed result.
fn test_one_crust(proj_dir: &Path) -> Result<CrustTestResult> {
    // Clean up test artifacts and shared temp dirs (some CRUST tests use ./tmp)
    for artifact in [".vsync", "tmp"] {
        let p = proj_dir.join(artifact);
        if p.exists() { let _ = std::fs::remove_dir_all(&p); }
    }

    let openssl = openssl_dir();

    // Pre-fetch dependencies before the timed build. Otherwise a first-run
    // crates.io index population can race the `cargo test` step and surface as
    // a spurious `E0432: unresolved import` build failure (the crate is fetched
    // moments later). Fetching first makes the build deterministic and offline.
    let _ = Command::new("cargo")
        .arg("fetch")
        .env("OPENSSL_DIR", &openssl)
        .env("OPENSSL_NO_VENDOR", "1")
        .current_dir(proj_dir)
        .output();

    let run_cargo_test = || -> Result<std::process::Output> {
        Command::new("timeout")
            .args(["60", "cargo", "test", "--", "--test-threads=1"])
            .env("OPENSSL_DIR", &openssl)
        .env("OPENSSL_NO_VENDOR", "1")
            .current_dir(proj_dir)
            .output()
            .with_context(|| format!("running cargo test in {}", proj_dir.display()))
    };

    let mut output = run_cargo_test()?;
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // `OPENSSL_NO_VENDOR=1` (set above) forces the system OpenSSL and is the primary
    // fix for the flake below: a few crates enable openssl's `vendored` feature,
    // which builds OpenSSL from source via `openssl-src` and non-deterministically
    // fails ("'perl' reported failure"). Forcing system OpenSSL makes it stable.
    // This retry stays as a belt-and-suspenders: retry ONCE on the openssl-sys
    // build-script signature (never on genuine `error[...]`/`could not compile`, so
    // real compile bugs are still caught first-run).
    let openssl_flake = stderr.contains("failed to run custom build command for `openssl-sys`")
        && !stderr.contains("error[")
        && !stderr.contains("could not compile");
    if openssl_flake {
        output = run_cargo_test()?;
        stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    }

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    let (tests_ok, tests_failed) = parse_cargo_test_results(&stdout);
    let build_ok = !stderr.contains("error[") && !stderr.contains("could not compile")
        && !stderr.contains("failed to run custom build command");

    // NOTE: we deliberately do NOT add an exit-code guard here. CRUST-Bench's
    // protocol (compile_projects.py) scores purely by counting `... ok`/`... FAILED`
    // in stdout and ignores the process exit code — so a test binary that aborts
    // AFTER printing some `... ok` lines still counts those as passes. To keep our
    // numbers directly comparable to the CRUST-Bench leaderboard we match that
    // behavior exactly (a stricter exit-code rule would diverge from upstream).

    // If build failed or no tests ran, re-run with --verbose for full diagnostics
    let (final_stdout, final_stderr) = if !build_ok || (tests_ok == 0 && tests_failed == 0) {
        let verbose = Command::new("timeout")
            .args(["60", "cargo", "test", "--verbose"])
            .env("OPENSSL_DIR", &openssl)
        .env("OPENSSL_NO_VENDOR", "1")
            .current_dir(proj_dir)
            .output()
            .ok();
        if let Some(v) = verbose {
            (String::from_utf8_lossy(&v.stdout).into_owned(),
             String::from_utf8_lossy(&v.stderr).into_owned())
        } else {
            (stdout.clone(), stderr.clone())
        }
    } else {
        (stdout.clone(), stderr.clone())
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

/// Read the Cargo package name from a crate's `Cargo.toml` `[package] name`. The
/// workspace directory name and the package name often DIFFER (e.g. dir
/// `lambda_calculus_eval` -> pkg `lambda-calculus-eval`, dir `proj_2DPartInt` ->
/// pkg `twoDPartInt`), and `cargo -p` matches the PACKAGE name; using the dir name
/// silently matches nothing and runs zero tests. Falls back to the dir name.
fn workspace_member_package_name(crate_dir: &Path, dir_name: &str) -> String {
    let toml_path = crate_dir.join("Cargo.toml");
    if let Ok(text) = std::fs::read_to_string(&toml_path) {
        if let Ok(doc) = text.parse::<toml_edit::DocumentMut>() {
            if let Some(name) = doc
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
            {
                return name.to_string();
            }
        }
    }
    dir_name.to_string()
}

/// Score one workspace member via `cargo test -p <pkg>` from the workspace root.
/// The baseline crates use `{ workspace = true }` dependencies, so each must be
/// built inside its workspace rather than copied out standalone. `pkg` MUST be the
/// Cargo package name (see `workspace_member_package_name`), not the directory
/// name. Returns `(tests_ok, tests_failed, built)`, where `built` is a real
/// compile check (same build-failure detection as `test_one_crust`), so the
/// "Builds" column means exactly the same thing for baselines as ACTOR's
/// `build_ok` flag.
fn score_workspace_member(workspace_root: &Path, pkg: &str) -> (usize, usize, bool) {
    let openssl = openssl_dir();
    // Pre-fetch to keep the timed build deterministic/offline (mirrors test_one_crust).
    let _ = Command::new("cargo")
        .args(["fetch", "-p", pkg])
        .env("OPENSSL_DIR", &openssl)
        .env("OPENSSL_NO_VENDOR", "1")
        .current_dir(workspace_root)
        .output();
    let run_cargo_test = || {
        Command::new("timeout")
            .args(["120", "cargo", "test", "-p", pkg, "--", "--test-threads=1"])
            .env("OPENSSL_DIR", &openssl)
        .env("OPENSSL_NO_VENDOR", "1")
            .current_dir(workspace_root)
            .output()
    };
    let Ok(mut output) = run_cargo_test() else { return (0, 0, false) };
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Retry once on the openssl-sys cold-build flake (see test_one_crust); do not
    // retry genuine compile errors, so real bugs are still caught first-run.
    if stderr.contains("failed to run custom build command for `openssl-sys`")
        && !stderr.contains("error[") && !stderr.contains("could not compile")
    {
        if let Ok(retry) = run_cargo_test() {
            output = retry;
            stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        }
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    // Same compile-failure signature as test_one_crust, so "Builds" is consistent.
    // `cargo -p` matching no package is a hard usage error, not a build failure —
    // guard against it so a name-resolution slip can't masquerade as "built".
    let no_match = stderr.contains("did not match any packages");
    let built = !no_match
        && !stderr.contains("error[")
        && !stderr.contains("could not compile")
        && !stderr.contains("failed to run custom build command");
    let (ok, fail) = parse_cargo_test_results(&stdout);
    // No exit-code guard: match CRUST-Bench's protocol exactly (count `... ok`/
    // `... FAILED` in stdout, ignore exit code), so baselines and ACTOR are scored
    // identically to the upstream leaderboard.
    (ok, fail, built)
}

/// Score one CRUST-Bench baseline workspace against ground-truth tests, writing a
/// `{project, ok, fail, built}` array to `<workspace>/<out_file>`. `built` is a
/// real compile check, so the report generator's "Builds"/"Tests" columns mean
/// exactly the same thing for baselines as for ACTOR (one `CrustOutcome` rule).
fn score_baseline_workspace(ws: &Path, out_file: &str) -> Result<Option<(u32, u32, u32)>> {
    if !ws.join("Cargo.toml").is_file() {
        println!("⏭️  {}: no workspace Cargo.toml, skipping", ws.display());
        return Ok(None);
    }
    // Workspace members = immediate subdirectories containing a Cargo.toml.
    let mut projects: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(ws)? {
        let entry = entry?;
        if entry.path().is_dir() && entry.path().join("Cargo.toml").is_file() {
            projects.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    projects.sort();
    println!("▶ scoring {}: {} member crates (ground-truth tests)", ws.display(), projects.len());

    let mut report: Vec<serde_json::Value> = Vec::new();
    let (mut pass, mut builds, mut counted) = (0u32, 0u32, 0u32);
    for proj in &projects {
        // `proj` is the directory name (the key used by exclusions + the authors'
        // reports); `cargo -p` needs the actual package name, which often differs.
        let pkg = workspace_member_package_name(&ws.join(proj), proj);
        let (ok, fail, built) = score_workspace_member(ws, &pkg);
        report.push(serde_json::json!({ "project": proj, "ok": ok, "fail": fail, "built": built }));
        if !crate::exclusions::is_excluded(proj) {
            counted += 1;
            let outcome = crate::scoring::CrustOutcome::from_baseline(
                &serde_json::json!({ "ok": ok, "fail": fail, "built": built }),
            );
            if outcome.passed() { pass += 1; }
            if outcome.built() { builds += 1; }
            let mark = if outcome.passed() { "✅" } else { "❌" };
            let b = if built { "" } else { " (build FAILED)" };
            println!("  {mark} {proj}: {ok} ok, {fail} fail{b}");
        }
    }
    let out_path = ws.join(out_file);
    std::fs::write(&out_path, serde_json::to_string_pretty(&report)? + "\n")?;
    println!(
        "📝 {}: builds {builds}/{counted}, tests {pass}/{counted} over the 87-subset",
        out_path.display()
    );
    Ok(Some((builds, pass, counted)))
}

/// Re-score ALL CRUST-Bench baselines (self-generated single-shot AND test-repair)
/// against ground-truth tests through the one shared pipeline, so every baseline
/// cell — Builds and Tests — is data-derived by the identical rule as ACTOR.
///
/// Self-generated workspaces (`gpt54`, `kimi_k25`, `gemini31pro`) are single-shot
/// transpilations (verified: transpilation-only prompt, no repair metadata, no
/// test access). Test-repair workspaces (`*_test_repair`) additionally iterate
/// with test feedback. Both ship identical ground-truth tests. Output goes to
/// `test_report_selfgen.json` (self-gen) / `test_report_scored.json` (test-repair)
/// as `{project, ok, fail, built}`.
pub fn score_selfgen_baselines(repo_root: &Path) -> Result<()> {
    let outputs = repo_root.join("crust-bench/src/outputs");
    let workspaces = [
        ("gpt54", "test_report_selfgen.json"),
        ("kimi_k25", "test_report_selfgen.json"),
        ("gemini31pro", "test_report_selfgen.json"),
        ("gpt54_test_repair", "test_report_scored.json"),
        ("kimi_k25_test_repair", "test_report_scored.json"),
        ("gemini31pro_test_repair", "test_report_scored.json"),
    ];
    for (name, out_file) in workspaces {
        score_baseline_workspace(&outputs.join(name), out_file)?;
    }
    Ok(())
}

/// Parse `test result: ok. N passed; M failed; ...` lines from cargo test stdout.
/// Deterministic regardless of output interleaving.
fn parse_cargo_test_results(stdout: &str) -> (usize, usize) {
    // Match CRUST-Bench's protocol EXACTLY (compile_projects.py::test):
    //   oks   = stdout.count('... ok')
    //   fails = stdout.count('... FAILED')
    // i.e. count per-test result lines, NOT the `test result:` summary line. This is
    // deliberately identical to upstream so our numbers are directly comparable to the
    // CRUST-Bench leaderboard (a crate is scored pass downstream iff ok>0 && fail==0).
    let ok = stdout.matches("... ok").count();
    let failed = stdout.matches("... FAILED").count();
    (ok, failed)
}

/// Load per-project result.json files into a baseline (for CI --check without re-running tests).
fn load_stored_results(paths: &Paths) -> Result<CrustBaseline> {
    let mut results = std::collections::BTreeMap::new();
    if !paths.results_dir.is_dir() { return Ok(CrustBaseline(results)); }
    for entry in std::fs::read_dir(&paths.results_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        // Reader rule: score lives in the case's current phase dir (verified/
        // else translated/). CRUST is single-phase → translated/result.json.
        let result_path = crate::battery::crate_dir(&entry.path()).join("result.json");
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
        // Blind-CRUST's verified/ phase dir holds the scored result.json.
        let rj = crate::battery::phase_dir(&entry.path(), crate::battery::VERIFIED).join("result.json");
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
        // CRUST is single-phase: the crate is the case's `translated/` dir,
        // carrying its own result.json + logs.
        let proj_dir = crate::battery::phase_dir(&paths.output_dir(name), crate::battery::TRANSLATED);
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

        // --update: write result.json immediately (enrichment is part of the
        // write, never a separate step — see `Enrichment`).
        if matches!(mode, TestMode::Update) {
            let mut json = serde_json::to_value(&r)?;
            let tlog = proj_dir.join("logs/translation.log");
            Enrichment::compute(&proj_dir.join("src"), &[("agent", &tlog)]).merge_into(&mut json);
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
                let proj_dir = crate::battery::phase_dir(&paths.output_dir(name), crate::battery::TRANSLATED);
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
                    let log_path = crate::battery::phase_dir(&paths.output_dir(&r.project), crate::battery::TRANSLATED).join("logs/test.log");
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
            let ok = r.passed();
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
            // Swap in the scaffold's Cargo.toml so [[test]]/[[bin]] entries match
            // the real test files, BUT preserve the agent's [dependencies]. The
            // scaffold ships an empty [dependencies]; blindly overwriting drops
            // crates the translation legitimately needs (e.g. termion for a C
            // program that uses termios/ioctl), spuriously failing the build.
            std::fs::rename(&cargo_toml, &cargo_backup)?;
            let scaffold_toml = std::fs::read_to_string(project.scaffold().join("Cargo.toml"))
                .unwrap_or_default();
            let agent_toml = std::fs::read_to_string(&cargo_backup).unwrap_or_default();
            let merged = merge_cargo_deps(&scaffold_toml, &agent_toml);
            std::fs::write(&cargo_toml, merged)?;
        }

        let real_result = test_one_crust(proj_dir.as_ref())?;
        let real_ok = real_result.passed();
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
            let mut json = serde_json::json!({
                "llm_tests_ok": llm_result.tests_ok,
                "llm_tests_failed": llm_result.tests_failed,
                "real_tests_ok": real_result.tests_ok,
                "real_tests_failed": real_result.tests_failed,
                "build_ok": real_result.build_ok,
            });
            let src = paths.translate_dir(name).join("src");
            let tlog = paths.translate_dir(name).join("logs/translation.log");
            let vlog = proj_dir.join("logs/verify.log");
            Enrichment::compute(&src, &[("translate", &tlog), ("verify", &vlog)]).merge_into(&mut json);
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
// one agent-run-meta object per phase (`agent` for single-phase CRUST,
// `translate`+`verify` for the two-phase pipelines). This used to be
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

pub fn enrich_blind_crust(paths: &Paths, projects: &[crate::battery::CrustProject]) -> Result<()> {
    let mut enriched = 0usize;
    for project in projects {
        let name = project.name();
        let src = paths.translate_dir(name).join("src");
        let tlog = paths.translate_dir(name).join("logs/translation.log");
        let vlog = paths.verify_dir(name).join("logs/verify.log");
        if Enrichment::enrich_file(
            &paths.verify_dir(name).join("result.json"),
            &src,
            &[("translate", &tlog), ("verify", &vlog)],
        )? { enriched += 1; }
    }
    println!("✅ Enriched {enriched} CRUST-blind result.json files");
    Ok(())
}

pub fn enrich_crust(paths: &Paths, projects: &[crate::battery::CrustProject]) -> Result<()> {
    let mut enriched = 0usize;
    for project in projects {
        let proj_dir = paths.output_dir(project.name());
        let tlog = proj_dir.join("logs/translation.log");
        if Enrichment::enrich_file(
            &proj_dir.join("result.json"),
            &proj_dir.join("src"),
            &[("agent", &tlog)],
        )? { enriched += 1; }
    }
    println!("✅ Enriched {enriched} CRUST result.json files");
    Ok(())
}

// ── harvest-bench testing ──────────────────────────────────────────────

/// Per-project harvest-bench result: build the translated crate into a cdylib,
/// then run the upstream GoogleTest suite against it via the harvest-bench
/// runner. `passed` uses the same rule as CRUST (built, ≥1 test ok, 0 failed)
/// so the pass column means the same thing across datasets.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct HarvestBenchResult {
    tests_ok: usize,
    tests_failed: usize,
    tests_skipped: usize,
    build_ok: bool,
}

impl HarvestBenchResult {
    fn passed(&self) -> bool {
        crate::scoring::CrustOutcome {
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
        let missing = tmp.path().join("nope.log");
        Enrichment::compute(&src, &[]).merge_into(&mut json);
        json["unsafe"]["blocks"] = serde_json::json!(99); // tamper
        fs::write(&rj, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let diffs = check_enrichment(&rj, &src, &[], crate::cli::Agent::Claude);
        assert!(diffs.iter().any(|d| d.contains("unsafe.blocks")), "tamper should be caught: {diffs:?}");
    }

    #[test]
    fn load_blind_stored_results_reads_from_verify_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let results_dir = tmp.path().join("results/CRUST-blind/kiro");

        // Create project with verified/result.json (post-verify phase dir)
        let proj = results_dir.join("vec/verified");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("result.json"), r#"{"real_tests_ok": 22, "real_tests_failed": 0}"#).unwrap();

        // Create another project — result.json at case root (no phase dir)
        // should be ignored: blind results live under verified/.
        let proj2 = results_dir.join("hamta");
        fs::create_dir_all(&proj2).unwrap();
        fs::write(proj2.join("result.json"), r#"{"real_tests_ok": 99, "real_tests_failed": 1}"#).unwrap();

        let paths = crate::battery::Paths::new(
            tmp.path(), crate::cli::Agent::Kiro, crate::cli::Dataset::BlindCrust, None,
        );

        let stored = load_blind_stored_results(&paths).unwrap();
        assert_eq!(stored.len(), 1, "only verified/ layout should be found");
        assert_eq!(stored["vec"].real_tests_ok, 22);
        assert_eq!(stored["vec"].real_tests_failed, 0);
    }
}
