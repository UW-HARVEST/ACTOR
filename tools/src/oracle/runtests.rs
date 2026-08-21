use super::{
    check_enrichment, openssl_dir, BatteryMismatch, Covers, Enrichment, Scoring, Summary, TestMode,
    TestOutcome,
};
use crate::agent_health::Run;
use crate::artifact::{Phase, Published, Translate, Verify};
use crate::battery::Paths;
use crate::eval::{Materialised, Source};
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;

pub fn run_test_corpus(paths: &Paths, target: &str, scoring: &Scoring<'_>) -> Result<TestOutcome> {
    // The denominator comes from the CORPUS: `results/` is the output and may not define the input.
    let named = (target != "all").then_some(target);
    let batteries = match named {
        Some(battery) => vec![battery.to_string()],
        None => crate::battery::all_batteries(&paths.corpus_dir)?,
    };

    // Every run the score COVERS, not only the ones that published: a run that produced nothing
    // publishes no artifact, and that is the `api_error` class the refusal exists for.
    scoring
        .gate
        .grade(&covered_runs(paths, &batteries, scoring.covers)?)?;

    let mut passes: Vec<Pass> = Vec::new();
    let mut unresolved = false;
    for battery in &batteries {
        let resolved = materialise_battery(paths, battery, scoring)?;
        unresolved |= resolved.is_empty();
        passes.extend(resolved);
    }

    if let Some(battery) = named.filter(|_| unresolved) {
        return nothing_to_score(paths, battery, scoring.mode);
    }
    // The fan-out above may legitimately skip a battery: one this agent never ran has no record to
    // disagree with. Skipping EVERY battery is the same hole one level up, so it is not skipped.
    anyhow::ensure!(
        !passes.is_empty(),
        "no battery under {target} holds an artifact this score may cover, so nothing was scored."
    );

    let mut all_mismatches = Vec::new();
    let mut check_rows: Vec<CheckRow> = Vec::new();

    for pass in &passes {
        println!();
        println!("========================================");
        println!("  Testing: {} [{}]", pass.battery, pass.phase);
        println!("========================================");
        if let TestOutcome::Failed(mm) = score_pass(paths, pass, scoring, &mut check_rows)? {
            all_mismatches.extend(mm);
        }
    }

    if matches!(scoring.mode, TestMode::Check) && !check_rows.is_empty() {
        print_check_summary(&check_rows);
    }

    match scoring.mode {
        TestMode::Check if !all_mismatches.is_empty() => Ok(TestOutcome::Failed(all_mismatches)),
        TestMode::Check => Ok(TestOutcome::Passed),
        _ => Ok(TestOutcome::Ok),
    }
}

struct CheckRow {
    battery: String,
    phase: &'static str,
    expected: Summary,
    actual: Summary,
    ok: bool,
}

struct Pass {
    battery: String,
    phase: &'static str,
    record: &'static str,
    tree: Materialised,
}

/// The convention [`crate::analyse::report`] reads: `summary.json` is a battery's HEADLINE record — where a verify-less agent
/// files its translate score — and `summary_translated.json` the translate record beside a verified headline. Pick the wrong
/// one and the comparison is against another phase's number.
const HEADLINE_SUMMARY: &str = "summary.json";
const TRANSLATE_BESIDE_HEADLINE: &str = "summary_translated.json";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Headline {
    Verified,
    Translated,
}

impl Headline {
    fn for_agent(agent: crate::cli::Agent) -> Self {
        if crate::agents::invocation::has_verify_phase(agent) {
            Self::Verified
        } else {
            Self::Translated
        }
    }

    fn translate_summary(self, verified_pass: bool) -> &'static str {
        if self == Self::Translated && !verified_pass {
            HEADLINE_SUMMARY
        } else {
            TRANSLATE_BESIDE_HEADLINE
        }
    }
}

fn covered_cases(paths: &Paths, battery: &str, covers: Covers<'_>) -> Result<Vec<String>> {
    Ok(crate::battery::all_case_names(&crate::battery::discover(
        &paths.corpus_dir,
        battery,
        covers.case_filter(),
    )?))
}

fn covered_runs(paths: &Paths, batteries: &[String], covers: Covers<'_>) -> Result<Vec<Run>> {
    let mut runs = Vec::new();
    for battery in batteries {
        if !paths.input_dir(battery).is_dir() {
            continue;
        }
        let output_dir = paths.output_dir(battery);
        for name in covered_cases(paths, battery, covers)? {
            runs.push(Run {
                name: format!("{battery}/{name}"),
                case_dir: output_dir.join(&name),
            });
        }
    }
    Ok(runs)
}

fn materialise_battery(paths: &Paths, battery: &str, scoring: &Scoring<'_>) -> Result<Vec<Pass>> {
    let Source { translate, verify } = scoring.source;
    let translated = materialise_phase::<Translate>(paths, battery, scoring, translate)?;
    let verified = materialise_phase::<Verify>(paths, battery, scoring, verify)?;

    let mut wanted = Vec::new();
    if let Some(tree) = translated {
        let record = Headline::for_agent(paths.agent).translate_summary(verified.is_some());
        wanted.push((Translate::DIR, record, tree));
    }
    if let Some(tree) = verified {
        wanted.push((Verify::DIR, HEADLINE_SUMMARY, tree));
    }

    Ok(wanted
        .into_iter()
        .map(|(phase, record, tree)| Pass {
            battery: battery.to_string(),
            phase,
            record,
            tree,
        })
        .collect())
}

fn materialise_phase<P: Phase>(
    paths: &Paths,
    battery: &str,
    scoring: &Scoring<'_>,
    resolved: &HashMap<std::path::PathBuf, Published<P>>,
) -> Result<Option<Materialised>> {
    let input_dir = paths.input_dir(battery);
    if !input_dir.is_dir() {
        return Ok(None);
    }
    let output_dir = paths.output_dir(battery);

    let mut scope = scoring.tree.scope(&format!("{battery}/{}", P::DIR))?;
    let mut any = false;
    for name in covered_cases(paths, battery, scoring.covers)? {
        let Some(artifact) = resolved.get(&output_dir.join(&name)) else {
            continue;
        };
        scope.materialise(&name, artifact, &input_dir.join(&name))?;
        any = true;
    }
    if !any {
        return Ok(None);
    }
    Ok(Some(scope.finish()?))
}

fn score_pass(
    paths: &Paths,
    pass: &Pass,
    scoring: &Scoring<'_>,
    check_rows: &mut Vec<CheckRow>,
) -> Result<TestOutcome> {
    let mode = scoring.mode;
    let (summary, per_case) = run_runtests(paths, pass, mode)?;
    let scored: BTreeSet<String> = per_case.keys().cloned().collect();
    pass.tree.reconcile(summary.cases_tested, &scored)?;

    let vt = summary.vectors_passed + summary.vectors_failed;
    let pct = if vt > 0 {
        format!("{:.1}%", 100.0 * summary.vectors_passed as f64 / vt as f64)
    } else {
        "N/A".to_string()
    };
    println!(
        "  {} [{}]: {}/{} cases, {}/{vt} vectors ({pct})",
        pass.battery,
        pass.phase,
        summary.cases_passed,
        summary.cases_tested,
        summary.vectors_passed
    );

    match mode {
        TestMode::Update => {
            write_results(paths, pass, &summary, &per_case, scoring.covers)?;
            Ok(TestOutcome::Ok)
        }
        TestMode::Check => Ok(check(paths, pass, &summary, &per_case, check_rows)),
        TestMode::Run => Ok(TestOutcome::Ok),
    }
}

/// A record that cannot be read is a MISMATCH, never a pass: a `--check` printing OK having compared
/// nothing is worse than no check. Unparseable is named apart from absent, or corrupt reads as absent.
fn stored_summary(path: &Path) -> Result<Summary, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("no stored record at {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "the stored record at {} cannot be read: {e}",
            path.display()
        )
    })
}

/// A battery NAMED on the command line that materialised nothing is refused in every mode, as
/// [`crate::oracle::gtest`] refuses a project it resolved no crate for: `Passed` over nothing scored is
/// a check reporting OK having seen nothing.
fn nothing_to_score(paths: &Paths, battery: &str, mode: TestMode) -> Result<TestOutcome> {
    let input_dir = paths.input_dir(battery);
    let why = if input_dir.is_dir() {
        "no case of it resolved a crate for either phase".to_string()
    } else {
        format!(
            "the corpus holds no {battery}: {} is absent",
            input_dir.display()
        )
    };
    let stored = paths.output_dir(battery).join(HEADLINE_SUMMARY);
    let wanted = stored_summary(&stored).map_or_else(
        |why| why,
        |s| {
            format!(
                "{} records {}/{} cases and {} vectors",
                stored.display(),
                s.cases_passed,
                s.cases_tested,
                s.vectors_passed
            )
        },
    );
    let diff = format!(
        "nothing was materialised for {battery} ({why}), so nothing was compared: {wanted}"
    );
    if !matches!(mode, TestMode::Check) {
        anyhow::bail!(
            "{diff}\nRefusing: a battery that was asked for and produced nothing is not a score."
        );
    }
    println!("   ❌ {battery}: {diff}");
    Ok(TestOutcome::Failed(vec![BatteryMismatch {
        battery: battery.to_string(),
        diffs: vec![diff],
    }]))
}

fn check(
    paths: &Paths,
    pass: &Pass,
    summary: &Summary,
    per_case: &HashMap<String, serde_json::Value>,
    check_rows: &mut Vec<CheckRow>,
) -> TestOutcome {
    let mut diffs = Vec::new();
    // Always compared, never skipped: a run PRODUCED this number, so a record it cannot find is
    // missing, not merely unfiled.
    let stored = paths.output_dir(&pass.battery).join(pass.record);
    let expected = Some(match stored_summary(&stored) {
        Ok(expected) => {
            diffs.extend(diff_summaries(&expected, summary));
            expected
        }
        Err(why) => {
            diffs.push(why);
            Summary::default()
        }
    });
    for case in pass.tree.cases() {
        if !per_case.contains_key(&case.name) {
            continue;
        }
        let crate_root = pass.tree.crate_root(&case.name);
        let (tlog, vlog) = transcripts(&crate_root);
        for d in check_enrichment(
            &case.record_into.join("result.json"),
            &crate_root.join("src"),
            &[("translate", &tlog), ("verify", &vlog)],
            paths.agent,
        ) {
            diffs.push(format!("{}: {d}", case.name));
        }
    }
    let ok = diffs.is_empty();
    if let Some(expected) = expected {
        check_rows.push(CheckRow {
            battery: pass.battery.clone(),
            phase: pass.phase,
            expected,
            actual: summary.clone(),
            ok,
        });
        if ok {
            println!("   ✅ {} [{}]: OK", pass.battery, pass.phase);
        }
    }
    if ok {
        TestOutcome::Passed
    } else {
        println!(
            "   ❌ {} [{}]: MISMATCH: {}",
            pass.battery,
            pass.phase,
            diffs.join("; ")
        );
        TestOutcome::Failed(vec![BatteryMismatch {
            battery: format!("{} [{}]", pass.battery, pass.phase),
            diffs,
        }])
    }
}

/// Out of the materialised crate, so a phase that wrote no transcript cannot borrow another's.
fn transcripts(crate_root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let logs = crate_root.join("logs");
    (logs.join(Translate::LOG), logs.join(Verify::LOG))
}

fn print_check_summary(rows: &[CheckRow]) {
    println!();
    println!("========================================");
    println!("  Check Summary");
    println!("========================================");
    println!(
        "  {:<25} {:>10} {:>15} {:>15}  Status",
        "Battery", "Phase", "Stored", "Actual"
    );
    println!("  {}", "─".repeat(85));
    for row in rows {
        let fmt = |s: &Summary| {
            format!(
                "{}/{} ({}v)",
                s.cases_passed, s.cases_tested, s.vectors_passed
            )
        };
        println!(
            "  {:<25} {:>10} {:>15} {:>15}  {}",
            row.battery,
            row.phase,
            fmt(&row.expected),
            fmt(&row.actual),
            if row.ok { "✅" } else { "❌" }
        );
    }
    println!("========================================");
}

/// Cases discovered but not one vector judged: the oracle measured nothing, whatever it printed. All
/// 128 of P01's crates once failed to build on a runner whose registry lacked their dependency, and the
/// pass reported `0/128` and regenerated `tables/` from it -- caught only by `git diff`.
fn measured_nothing(cases_discovered: usize, vectors_passed: usize, vectors_failed: usize) -> bool {
    cases_discovered > 0 && vectors_passed + vectors_failed == 0
}

fn run_runtests(
    paths: &Paths,
    pass: &Pass,
    mode: TestMode,
) -> Result<(Summary, HashMap<String, serde_json::Value>)> {
    let battery = &pass.battery;
    let root = pass.tree.root().to_string_lossy().to_string();
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
            &root,
            "--subset",
            &root,
            "--keep-going",
            "--verbose",
        ])
        .env("PYTHONPATH", &pythonpath)
        .env("OPENSSL_DIR", openssl_dir())
        // The agent translates in a sandbox with no network and leaves `[net] offline = true` in the
        // crate's `.cargo/config.toml`, which the artifact carries because `.cargo/` is a real build
        // input in 16 corpus cases. That is the AGENT's sandbox policy, not the scorer's: P01's crate
        // declares `aes = "=0.8.4"`, so on any machine without it already in the registry all 128
        // builds failed at once -- and the version is pinned exactly and checksum-verified, so
        // resolving it changes nothing about what is measured.
        .env("CARGO_NET_OFFLINE", "false")
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
    let _ = std::fs::write(paths.output_dir(battery).join("test.log"), &text);

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

    anyhow::ensure!(
        !measured_nothing(cases_discovered, vectors_passed, vectors_failed),
        "{battery} [{}] discovered {cases_discovered} case(s) and ran NO test vector, so this is not a \
         score of zero -- nothing was measured. Every crate failing to build looks exactly like this. \
         The build output is in {}.",
        pass.phase,
        paths.output_dir(battery).join("test.log").display(),
    );

    // runtests' grammar: "- NAME: Build failed …", or "- NAME: Test failed (testN: REASON" — ONE
    // vector, opening a block of diff lines, then "expected rc=A, actual rc=B", then ")" — and
    // "Executing NAME" for a case that ran. Accumulated per vector so vectors_failed is exact and the
    // diff snippets reach result.json rather than only the battery test.log.
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
    for name in executed_cases(&text)? {
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

/// ANCHORED: `runtests` prints a bare "Executing test cases..." header too, which files a case `test`.
fn executed_cases(text: &str) -> Result<Vec<String>> {
    let re = Regex::new(r"^\s+Executing (\S+)$")?;
    Ok(text
        .lines()
        .filter_map(|line| re.captures(line).map(|c| c[1].to_string()))
        .collect())
}

fn write_results(
    paths: &Paths,
    pass: &Pass,
    summary: &Summary,
    per_case: &HashMap<String, serde_json::Value>,
    covers: Covers<'_>,
) -> Result<()> {
    for case in pass.tree.cases() {
        let Some(data) = per_case.get(&case.name) else {
            continue;
        };
        let mut val = data.clone();
        let crate_root = pass.tree.crate_root(&case.name);
        let (tlog, vlog) = transcripts(&crate_root);
        Enrichment::compute(
            &crate_root.join("src"),
            &[("translate", &tlog), ("verify", &vlog)],
        )
        .merge_into(&mut val);
        let json = serde_json::to_string_pretty(&val)?;
        std::fs::write(case.record_into.join("result.json"), format!("{json}\n"))?;
    }
    match covers {
        Covers::WholeBattery => {
            let json = serde_json::to_string_pretty(summary)?;
            std::fs::write(
                paths.output_dir(&pass.battery).join(pass.record),
                format!("{json}\n"),
            )?;
        }
        // `analyse::report` rebuilds `tables/` from this file, and a subset's count is not the battery's.
        Covers::Subset(regex) => println!(
            "   ⏭️  {} [{}]: --include-regex {regex} covered part of the battery, so {} keeps the \
             whole battery's number and is not rewritten",
            pass.battery, pass.phase, pass.record
        ),
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{Keep, Tree};
    use std::fs;

    fn crate_at(dir: &std::path::Path) {
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]\n[workspace]\n").unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    }

    fn corpus_battery(root: &Path, battery: &str, cases: &[&str]) {
        for case in cases {
            let corpus = root
                .join("test-corpus/Public-Tests")
                .join(battery)
                .join(case);
            fs::create_dir_all(corpus.join("test_case")).unwrap();
            fs::create_dir_all(corpus.join("test_vectors")).unwrap();
            fs::write(corpus.join("test_vectors/t1.txt"), "vector").unwrap();
        }
    }

    fn paths_at(root: &Path, agent: crate::cli::Agent) -> Paths {
        fs::create_dir_all(root.join("results")).unwrap();
        Paths::new(
            root,
            agent,
            crate::cli::Dataset::TestCorpus,
            None,
            crate::cache::Mode::Bypass,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap()
    }

    fn gate_at(paths: &Paths) -> crate::agent_health::Gate<'_> {
        crate::agent_health::Gate {
            format: paths.agent.log_format(),
            on_failure: crate::agent_health::OnInfraFailure::Refuse,
            results_dir: &paths.results_dir,
        }
    }

    /// Sites 1, 2 and 4, the operator's complaint: ONE stale `verified/Cargo.toml` promoted a whole
    /// battery's headline, and `crate_dir` then scored `014_dead_code_lib` off a five-day-old crate.
    #[test]
    fn a_stale_verified_dir_neither_promotes_the_battery_nor_scores_its_case() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        corpus_battery(tmp.path(), "B01", &["fresh", "stale"]);
        let paths = paths_at(tmp.path(), crate::cli::Agent::Claude);

        // `stale` is the trap: no translation this run, a COMPLETE verified/ crate from an earlier.
        let fresh = paths.case_dir("B01", "fresh");
        crate_at(&fresh.join(Translate::DIR));
        let stale = paths.case_dir("B01", "stale");
        crate_at(&stale.join(Verify::DIR));
        assert!(
            crate::battery::has_crate(&stale.join(Verify::DIR)),
            "the fixture must hold the stale verified crate the old chooser preferred"
        );
        assert_eq!(
            crate::battery::all_case_names(
                &crate::battery::discover(&paths.corpus_dir, "B01", None).unwrap()
            )
            .len(),
            2,
            "and both cases must be candidates, or nothing was declined"
        );

        let mut translations = crate::translate::Translations::new();
        translations.insert(
            fresh.clone(),
            Published::<Translate>::unkeyed_from_phase_dir(&fresh).unwrap(),
        );
        let verifications = crate::verify::Verifications::new();
        let tree = Tree::create_empty(&paths, Keep::ForPostMortem).unwrap();
        let gate = gate_at(&paths);
        let scoring = Scoring {
            mode: TestMode::Run,
            source: Source {
                translate: &translations,
                verify: &verifications,
            },
            tree: &tree,
            gate: &gate,
            covers: Covers::WholeBattery,
        };

        let passes = materialise_battery(&paths, "B01", &scoring).unwrap();
        let phases: Vec<&str> = passes.iter().map(|p| p.phase).collect();
        assert_eq!(
            phases,
            vec![Translate::DIR],
            "a battery that verified nothing must not get a verified pass from one leftover dir"
        );
        assert_eq!(
            passes[0].record, TRANSLATE_BESIDE_HEADLINE,
            "and claude's headline stays the verified record it declares a phase for, so a \
             --no-verify sweep cannot file a translate number over it"
        );
        let scored: Vec<&str> = passes[0]
            .tree
            .cases()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            scored,
            vec!["fresh"],
            "and the case with only a stale verified/ is ABSENT from the score, not passing"
        );
        assert!(
            !passes[0].tree.root().join("stale").exists(),
            "nothing of it reaches the tree the oracle discovers cases from"
        );
    }

    fn one_case_pass(
        paths: &Paths,
        tree: &Tree,
        translations: &crate::translate::Translations,
        covers: Covers<'_>,
    ) -> Vec<Pass> {
        let verifications = crate::verify::Verifications::new();
        let gate = gate_at(paths);
        materialise_battery(
            paths,
            "B01",
            &Scoring {
                mode: TestMode::Check,
                source: Source {
                    translate: translations,
                    verify: &verifications,
                },
                tree,
                gate: &gate,
                covers,
            },
        )
        .unwrap()
    }

    /// `--check` compared nothing and printed a pass for 88 of the 101 shipped batteries: only
    /// `claude/*` and `kiro/*` hold `summary_translated.json`, and measured, `c2rust/B01_synthetic` is
    /// 85 `translated/` crates, no `verified/` crate and `summary.json` at 85/85 (393v).
    #[test]
    fn a_verify_less_agents_translate_score_is_the_headline_and_a_missing_record_is_a_mismatch() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        corpus_battery(tmp.path(), "B01", &["only"]);
        let paths = paths_at(tmp.path(), crate::cli::Agent::C2rust);
        let case = paths.case_dir("B01", "only");
        crate_at(&case.join(Translate::DIR));
        let mut translations = crate::translate::Translations::new();
        translations.insert(
            case.clone(),
            Published::<Translate>::unkeyed_from_phase_dir(&case).unwrap(),
        );

        let tree = Tree::create_empty(&paths, Keep::ForPostMortem).unwrap();
        let passes = one_case_pass(&paths, &tree, &translations, Covers::WholeBattery);
        assert_eq!(
            passes.len(),
            1,
            "an agent with no verify phase gets exactly the translate pass"
        );
        assert_eq!(
            passes[0].record, HEADLINE_SUMMARY,
            "and its translate score IS the battery headline, which is where the archive files it \
             and where analyse::report reads it"
        );

        let scored = Summary {
            cases_tested: 1,
            cases_passed: 1,
            vectors_passed: 7,
            ..Default::default()
        };
        let stored = paths.output_dir("B01").join(HEADLINE_SUMMARY);
        let mut rows = Vec::new();
        let outcome = check(&paths, &passes[0], &scored, &HashMap::new(), &mut rows);
        let TestOutcome::Failed(mismatches) = outcome else {
            panic!(
                "a --check with no record to compare against must not report a pass: {outcome:?}"
            );
        };
        assert!(
            mismatches[0]
                .diffs
                .iter()
                .any(|d| d.contains(HEADLINE_SUMMARY)),
            "and it must name the file it wanted: {:?}",
            mismatches[0].diffs
        );
        assert_eq!(
            rows.len(),
            1,
            "with the battery still in the Check Summary table rather than absent from it"
        );

        fs::write(&stored, serde_json::to_string(&scored).unwrap()).unwrap();
        rows.clear();
        assert!(
            matches!(
                check(&paths, &passes[0], &scored, &HashMap::new(), &mut rows),
                TestOutcome::Passed
            ),
            "an agreeing record still passes, or every check is red and none of them means anything"
        );

        let drifted = Summary {
            vectors_passed: 6,
            ..scored.clone()
        };
        fs::write(&stored, serde_json::to_string(&drifted).unwrap()).unwrap();
        rows.clear();
        assert!(
            matches!(
                check(&paths, &passes[0], &scored, &HashMap::new(), &mut rows),
                TestOutcome::Failed(_)
            ),
            "and one vector of drift against that record is still caught"
        );
    }

    fn stored_1_of_1(paths: &Paths, battery: &str) {
        let stored = paths.output_dir(battery).join(HEADLINE_SUMMARY);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, r#"{"cases_tested":1,"cases_passed":1,"vectors_passed":7,"vectors_failed":0,"vectors_skipped":0,"failed_cases":[]}"#).unwrap();
        assert_eq!(
            stored_summary(&stored).unwrap().vectors_passed,
            7,
            "the fixture must hold the record whose never being read is the defect"
        );
    }

    /// Exhaustive over the shapes the parsed counts can take, because the one that matters is
    /// indistinguishable from a real zero in the printed output: `0/128 cases, 0/0 vectors`.
    #[test]
    fn a_battery_that_judged_no_vector_measured_nothing_however_many_cases_it_found() {
        for (found, passed, failed, nothing) in [
            // Every crate failed to build: the CI shape this exists for.
            (128, 0, 0, true),
            (1, 0, 0, true),
            // A genuine zero SCORE still judged vectors, so it is a measurement.
            (128, 0, 128, false),
            (42, 1001, 24, false),
            (1, 30, 0, false),
            // Nothing discovered is `nothing_to_score`'s business, not this guard's.
            (0, 0, 0, false),
        ] {
            assert_eq!(
                measured_nothing(found, passed, failed),
                nothing,
                "{found} case(s), {passed} passed, {failed} failed"
            );
        }
    }

    /// A run that RESOLVED NOTHING — the shape a battery takes when every case missed.
    fn resolved_nothing(
        paths: &Paths,
        target: &str,
        tree: &Tree,
        mode: TestMode,
    ) -> Result<TestOutcome> {
        let gate = gate_at(paths);
        let (t, v) = (
            crate::translate::Translations::new(),
            crate::verify::Verifications::new(),
        );
        run_test_corpus(
            paths,
            target,
            &Scoring {
                mode,
                source: Source {
                    translate: &t,
                    verify: &v,
                },
                tree,
                gate: &gate,
                covers: Covers::WholeBattery,
            },
        )
    }

    /// Refuting `spec-20.md`: both shapes below once exited 0 having compared nothing — no crate
    /// resolved for the battery, and no corpus battery dir at all (a fresh worktree).
    #[test]
    fn a_named_battery_that_materialised_nothing_is_never_a_pass() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        corpus_battery(tmp.path(), "B01", &["case1"]);
        let paths = paths_at(tmp.path(), crate::cli::Agent::C2rust);
        let case = paths.case_dir("B01", "case1");
        fs::create_dir_all(case.join(Translate::DIR).join("logs")).unwrap();
        stored_1_of_1(&paths, "B01");
        assert!(
            !crate::battery::has_crate(&case.join(Translate::DIR)),
            "and no crate for it, or something was scored after all"
        );

        let tree = Tree::create_empty(&paths, Keep::ForPostMortem).unwrap();
        let outcome = resolved_nothing(&paths, "B01", &tree, TestMode::Check).unwrap();
        let TestOutcome::Failed(mismatches) = outcome else {
            panic!("a --check that materialised nothing must not report a pass: {outcome:?}")
        };
        assert!(
            mismatches[0]
                .diffs
                .iter()
                .any(|d| d.contains(HEADLINE_SUMMARY) && d.contains('7')),
            "naming the record it wanted to compare against: {:?}",
            mismatches[0].diffs
        );
        resolved_nothing(&paths, "B01", &tree, TestMode::Update)
            .expect_err("and --update must refuse rather than silently write nothing");
        resolved_nothing(&paths, "all", &tree, TestMode::Check)
            .expect_err("nor may the fan-out report a pass when it skipped EVERY battery");

        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let paths = paths_at(tmp.path(), crate::cli::Agent::C2rust);
        crate_at(&paths.case_dir("B01", "case1").join(Translate::DIR));
        stored_1_of_1(&paths, "B01");
        assert!(
            !paths.input_dir("B01").is_dir(),
            "the second fixture holds a complete crate and NO corpus battery"
        );
        let tree = Tree::create_empty(&paths, Keep::ForPostMortem).unwrap();
        let outcome = resolved_nothing(&paths, "B01", &tree, TestMode::Check).unwrap();
        let TestOutcome::Failed(mismatches) = outcome else {
            panic!("an absent corpus is not an agreeing score either: {outcome:?}")
        };
        assert!(
            mismatches[0]
                .diffs
                .iter()
                .any(|d| d.contains("Public-Tests")),
            "naming what is absent, since the archive itself was complete: {:?}",
            mismatches[0].diffs
        );
    }

    /// That header was scored as a case named `test`, so `reconcile` refused every battery before comparing.
    #[test]
    fn the_oracles_own_header_is_not_scored_as_a_case_no_tree_materialised() {
        let log = "Executing test cases...\n   Executing 001_helloworld\n   Executing 014_dead_code_lib\n";
        assert_eq!(
            executed_cases(log).unwrap(),
            vec!["001_helloworld", "014_dead_code_lib"],
            "the two indented lines are the cases; the header names none"
        );
    }

    /// `run B01 --include-regex one` wrote `cases_tested: 1` over the stored 85 and regenerated
    /// `tables/` from it; `reconcile` compares the subset against itself and agrees.
    #[test]
    fn a_subset_sweep_does_not_file_its_own_count_under_the_batterys_name() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        corpus_battery(tmp.path(), "B01", &["one", "two"]);
        let paths = paths_at(tmp.path(), crate::cli::Agent::C2rust);
        let case = paths.case_dir("B01", "one");
        crate_at(&case.join(Translate::DIR));
        let mut translations = crate::translate::Translations::new();
        translations.insert(
            case.clone(),
            Published::<Translate>::unkeyed_from_phase_dir(&case).unwrap(),
        );

        let stored = paths.output_dir("B01").join(HEADLINE_SUMMARY);
        let whole_battery = r#"{"cases_tested":2,"cases_passed":2,"vectors_passed":14,
             "vectors_failed":0,"vectors_skipped":0,"failed_cases":[]}"#;
        fs::write(&stored, whole_battery).unwrap();
        let subset = Summary {
            cases_tested: 1,
            cases_passed: 1,
            vectors_passed: 7,
            ..Default::default()
        };

        let tree = Tree::create_empty(&paths, Keep::ForPostMortem).unwrap();
        let passes = one_case_pass(&paths, &tree, &translations, Covers::Subset("one"));
        write_results(
            &paths,
            &passes[0],
            &subset,
            &HashMap::new(),
            Covers::Subset("one"),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&stored).unwrap(),
            whole_battery,
            "a subset score is not the battery's score and must not replace it"
        );

        write_results(
            &paths,
            &passes[0],
            &subset,
            &HashMap::new(),
            Covers::WholeBattery,
        )
        .unwrap();
        assert_eq!(
            stored_summary(&stored).unwrap().cases_tested,
            1,
            "while a sweep that covered the whole battery still writes it, or nothing is recorded"
        );
    }

    /// The gate was handed the cases that PUBLISHED — the one set with no infra failure in it, since a
    /// run that died on `api_error` publishes nothing. Base audited the whole agent tree instead.
    #[test]
    fn a_case_that_published_nothing_is_still_graded_and_a_subset_grades_only_what_it_ran() {
        const DEAD: &str = r#"{"type":"result","is_error":true,"terminal_reason":"api_error","api_error_status":403,"result":"expired token"}"#;
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        corpus_battery(tmp.path(), "B01", &["fresh", "dead"]);
        let paths = paths_at(tmp.path(), crate::cli::Agent::ClaudeCombined);

        let fresh = paths.case_dir("B01", "fresh");
        crate_at(&fresh.join(Translate::DIR));
        let dead = paths.case_dir("B01", "dead");
        let log = crate::artifact::phase_log::<Translate>(&dead);
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, DEAD).unwrap();
        assert!(
            !crate::battery::has_crate(&dead.join(Translate::DIR)),
            "the fixture must publish nothing for the dead case, or there is nothing invisible"
        );

        let batteries = vec!["B01".to_string()];
        let runs = covered_runs(&paths, &batteries, Covers::WholeBattery).unwrap();
        assert_eq!(
            runs.len(),
            2,
            "the roster is what the score COVERS, published or not: {:?}",
            runs.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        let gate = gate_at(&paths);
        let err = gate
            .grade(&runs)
            .expect_err("a run that produced nothing is an infrastructure failure, not a result");
        let text = format!("{err:#}");
        assert!(text.contains("B01/dead"), "and it is named: {text}");

        let touched = covered_runs(&paths, &batteries, Covers::Subset("fresh")).unwrap();
        assert_eq!(touched.len(), 1, "a subset sweep ran one case");
        gate.grade(&touched)
            .expect("so a stale dead transcript in a case it never touched must not block it");
    }
}
