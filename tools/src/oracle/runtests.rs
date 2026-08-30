use super::{openssl_dir, Covers, Enrichment, Scoring, Summary};
use crate::agent_health::Run;
use crate::battery::Paths;
use crate::eval::Materialised;
use crate::prompt::Role;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;

pub fn run_test_corpus(paths: &Paths, target: &str, scoring: &Scoring<'_>) -> Result<()> {
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
        return nothing_to_score(paths, battery);
    }
    // The fan-out above may legitimately skip a battery: one this agent never ran has no record to
    // disagree with. Skipping EVERY battery is the same hole one level up, so it is not skipped.
    anyhow::ensure!(
        !passes.is_empty(),
        "no battery under {target} holds an artifact this score may cover, so nothing was scored."
    );

    for pass in &passes {
        println!();
        println!("========================================");
        println!("  Testing: {} [{}]", pass.battery, pass.phase);
        println!("========================================");
        score_pass(paths, pass, scoring)?;
    }

    Ok(())
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
    /// Whether a translate pass owns the headline summary depends on whether the chain DECLARES a
    /// verify step -- not on whether one resolved, and not on the tool. A one-step chain's translate
    /// numbers ARE the result; a two-step chain's are not, even on a sweep that ran only the first.
    fn for_chain(declares_verify: bool) -> Self {
        if declares_verify {
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
    // One loop over the roles the chain ran, rather than one call per phase type. A third step needs
    // no third branch here.
    let mut wanted = Vec::new();
    let mut resolved_roles = Vec::new();
    for role in scoring.roles {
        if let Some(tree) = materialise_role(paths, battery, scoring, *role)? {
            resolved_roles.push(*role);
            wanted.push((role.dir(), tree));
        }
    }
    // Two different questions, and conflating them lets a `--steps 1` sweep file a translate number
    // over the headline of a chain that declares a verify step.
    let declares_verify = scoring.roles.contains(&Role::Verify);
    let verify_resolved = resolved_roles.contains(&Role::Verify);
    Ok(wanted
        .into_iter()
        .map(|(phase, tree)| Pass {
            battery: battery.to_string(),
            phase,
            record: if phase == Role::Verify.dir() {
                HEADLINE_SUMMARY
            } else {
                Headline::for_chain(declares_verify).translate_summary(verify_resolved)
            },
            tree,
        })
        .collect())
}

fn materialise_role(
    paths: &Paths,
    battery: &str,
    scoring: &Scoring<'_>,
    role: Role,
) -> Result<Option<Materialised>> {
    let input_dir = paths.input_dir(battery);
    if !input_dir.is_dir() {
        return Ok(None);
    }
    let output_dir = paths.output_dir(battery);

    let mut scope = scoring.tree.scope(&format!("{battery}/{}", role.dir()))?;
    let mut any = false;
    for name in covered_cases(paths, battery, scoring.covers)? {
        let published = output_dir.join(&name).join(role.dir());
        let Some(tree) = scoring.resolved.get(&published) else {
            continue;
        };
        scope.materialise(&name, tree, &input_dir.join(&name), &published)?;
        any = true;
    }
    if !any {
        return Ok(None);
    }
    Ok(Some(scope.finish()?))
}

fn score_pass(paths: &Paths, pass: &Pass, scoring: &Scoring<'_>) -> Result<()> {
    let (summary, per_case) = run_runtests(paths, pass)?;
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

    write_results(paths, pass, &summary, &per_case, scoring.covers)
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

/// A battery NAMED on the command line that materialised nothing is refused, as
/// [`crate::oracle::gtest`] refuses a project it resolved no crate for: reporting a score over nothing
/// scored is reporting OK having seen nothing.
fn nothing_to_score(paths: &Paths, battery: &str) -> Result<()> {
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
    anyhow::bail!(
        "{diff}\nRefusing: a battery that was asked for and produced nothing is not a score."
    )
}

/// Out of the materialised crate, so a phase that wrote no transcript cannot borrow another's.
fn transcripts(crate_root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let logs = crate_root.join("logs");
    (
        logs.join(Role::Translate.log()),
        logs.join(Role::Verify.log()),
    )
}

/// Cases discovered but NOT ONE of them reaching any verdict: the oracle never ran, whatever it
/// printed. All 128 of P01's crates once failed to build on a runner whose registry lacked their
/// dependency, and the pass reported `0/128` and regenerated `tables/` from it -- caught only by
/// `git diff`.
///
/// Keyed on cases, not vectors, and that correction matters: "every crate failed to build" IS a
/// result, and it is one four agents already publish. `smartc2rust`, `c2rust`, `c2saferrust` and
/// `gpt-5.4` each record `0/128` for P01 with `Tested: 0, Failed: 128, Vectors: 0/0` -- the exact
/// shape the vector form refused, so the code could not re-derive four rows of its own tables.
/// Nobody noticed because `reproduce.sh` replays claude only.
///
/// The registry class this was written for is now stopped at source by `CARGO_NET_OFFLINE=false`
/// below, and any table that moves for any reason is caught by `reproduce.sh`'s byte-identical
/// diff. What remains here is the one signal counts can honestly carry: a battery where the oracle
/// judged nothing at all, not one where it judged everything a failure.
fn measured_nothing(cases_discovered: usize, cases_tested: usize, cases_failed: usize) -> bool {
    cases_discovered > 0 && cases_tested + cases_failed == 0
}

/// The cases runtests failed OUTRIGHT, with the reason, from its text alone.
///
/// Pure and separately testable because the shape LIST is where the bug was: only `Build failed` and
/// `Test failed (` were matched, so `Execution failed` -- a case whose binary never ran -- stayed out
/// of `failed_cases`, and `cases_passed = discovered - failed.len()` counted it as a PASS. 76 such
/// cases sit in the committed logs across 10 agents, every one scored as passing;
/// `claude-cross-prompt` alone holds 56 of them against a published 63/210. It stayed hidden because
/// the one battery where EVERY case took this shape was refused by `measured_nothing` instead, so the
/// parser gap surfaced as a different error.
fn hard_failures(text: &str) -> Result<Vec<(String, &'static str)>> {
    let shapes = [
        (Regex::new(r"^- (\S+): Build failed")?, "build failed"),
        (
            Regex::new(r"^- (\S+): Execution failed")?,
            "execution failed",
        ),
    ];
    let mut out = Vec::new();
    for line in text.lines() {
        for (re, label) in &shapes {
            if let Some(caps) = re.captures(line) {
                out.push((caps[1].to_string(), *label));
            }
        }
    }
    Ok(out)
}

fn run_runtests(
    paths: &Paths,
    pass: &Pass,
) -> Result<(Summary, HashMap<String, serde_json::Value>)> {
    let battery = &pass.battery;
    let root = pass.tree.root().to_string_lossy().to_string();
    // Inside the evaluation tree, so it is removed with it and no run can read another's.
    let report = pass.tree.root().join("junit.xml");
    let report_arg = report.to_string_lossy().to_string();
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
            "--junit-xml",
            &report_arg,
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
    print!("{text}");
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
    let cases_tested = extract(r"Test Cases Tested:\s+(\d+)");
    let cases_failed = extract(r"Test Cases Failed:\s+(\d+)");
    let vectors_passed = extract(r"Test Vectors Passed:\s+(\d+)");
    let vectors_failed = extract(r"Test Vectors Failed:\s+(\d+)");
    let vectors_skipped = extract(r"Test Vectors Skipped:\s+(\d+)");

    anyhow::ensure!(
        !measured_nothing(cases_discovered, cases_tested, cases_failed),
        "{battery} [{}] discovered {cases_discovered} case(s) and reached a verdict on NONE of them, \
         so this is not a score of zero -- the oracle never ran. The build output is in {}.",
        pass.phase,
        paths.output_dir(battery).join("test.log").display(),
    );

    // runtests' grammar: "- NAME: Build failed …", "- NAME: Execution failed: …", or "- NAME: Test
    // failed (testN: REASON" — ONE vector, opening a block of diff lines, then "expected rc=A, actual
    // rc=B", then ")" — and "Executing NAME" for a case that ran. Accumulated per vector so
    // vectors_failed is exact and the diff snippets reach result.json rather than only the test.log.
    let mut per_case: HashMap<String, serde_json::Value> = HashMap::new();
    let mut failed_cases: Vec<String> = Vec::new();

    for (name, label) in hard_failures(&text)? {
        if !failed_cases.contains(&name) {
            failed_cases.push(name.clone());
        }
        per_case.insert(
            name.clone(),
            serde_json::json!({
                "case": name, "battery": battery,
                "vectors_failed": 1, "passed": false,
                "error": label,
            }),
        );
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
    let ran = cases_in_report(&std::fs::read_to_string(&report).with_context(|| {
        format!(
            "reading the runtests report at {} -- without it there is no authoritative record of \
             which cases ran",
            report.display()
        )
    })?)?;
    for name in ran {
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

/// The cases `runtests` RAN, read from the report it writes rather than from its console output. That
/// output is the interleaved stdout of parallel jobs, and a single mangled `Executing NAME` line drops
/// the case from the set [`crate::eval::Materialised::reconcile`] compares -- which refuses the whole
/// battery. Observed on `tfm_lib` and, two runs later, on `confusion_lib`. A report written once, at
/// the end, cannot interleave. `<testsuite\s` also excludes the `<testsuites>` root element.
fn cases_in_report(xml: &str) -> Result<Vec<String>> {
    let re = Regex::new(r#"<testsuite\s[^>]*name="([^"]+)""#)?;
    Ok(re.captures_iter(xml).map(|c| c[1].to_string()).collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{EvalTree, Keep};
    use std::fs;

    /// A PUBLISHED phase dir: the crate directly, which is what `has_crate` asks about and what the
    /// scorer's `translated_rust/` is assembled from.
    fn crate_at(dir: &std::path::Path) {
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]\n[workspace]\n").unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    }

    /// A TREE: the working-dir shape, `c_src/` beside `translation/`. Distinct from a published phase
    /// dir on purpose -- conflating the two is what this split exists to prevent.
    fn tree_of(at: &std::path::Path) -> crate::tree::Tree {
        let crate_dir = at.join(crate::tree::TRANSLATION);
        fs::create_dir_all(crate_dir.join("src")).unwrap();
        fs::write(crate_dir.join("Cargo.toml"), "[package]\n[workspace]\n").unwrap();
        fs::write(crate_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::create_dir_all(at.join(crate::tree::C_SRC)).unwrap();
        fs::write(at.join(crate::tree::C_SRC).join("lib.c"), "int f(void);\n").unwrap();
        crate::tree::Tree::for_test(at).unwrap()
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

    fn paths_at(root: &Path, tool: crate::cli::Tool, variant: crate::cli::Variant) -> Paths {
        Paths::new(
            root,
            tool,
            variant,
            crate::cli::Dataset::TestCorpus,
            None,
            crate::store::Mode::ReadWrite,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap()
    }

    fn gate_at(paths: &Paths) -> crate::agent_health::Gate<'_> {
        crate::agent_health::Gate {
            format: crate::runners::log_format(paths.tool),
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
        let paths = paths_at(
            tmp.path(),
            crate::cli::Tool::Claude,
            crate::cli::Variant::Default,
        );

        // `stale` is the trap: no translation this run, a COMPLETE verified/ crate from an earlier.
        let fresh = paths.case_dir("B01", "fresh");
        crate_at(&fresh.join(Role::Translate.dir()));
        let stale = paths.case_dir("B01", "stale");
        crate_at(&stale.join(Role::Verify.dir()));
        assert!(
            crate::battery::has_crate(&stale.join(Role::Verify.dir())),
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

        let mut resolved = crate::eval::Resolved::new();
        resolved.insert(
            fresh.join(Role::Translate.dir()),
            tree_of(&fresh.join("tree")),
        );
        let tree = EvalTree::create_empty(&paths, "T", Keep::ForPostMortem).unwrap();
        let gate = gate_at(&paths);
        let scoring = Scoring {
            roles: crate::prompt::chain(paths.tool, paths.variant),
            resolved: &resolved,
            tree: &tree,
            gate: &gate,
            covers: Covers::WholeBattery,
        };

        let passes = materialise_battery(&paths, "B01", &scoring).unwrap();
        let phases: Vec<&str> = passes.iter().map(|p| p.phase).collect();
        assert_eq!(
            phases,
            vec![Role::Translate.dir()],
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
        tree: &EvalTree,
        resolved: &crate::eval::Resolved,
        covers: Covers<'_>,
    ) -> Vec<Pass> {
        let gate = gate_at(paths);
        materialise_battery(
            paths,
            "B01",
            &Scoring {
                roles: crate::prompt::chain(paths.tool, paths.variant),
                resolved,
                tree,
                gate: &gate,
                covers,
            },
        )
        .unwrap()
    }

    /// Which summary a verify-less agent's translate score is filed as. Only `claude/*` and `kiro/*`
    /// hold `summary_translated.json`; measured, `c2rust/B01_synthetic` is 85 `translated/` crates, no
    /// `verified/` crate, and `summary.json` at 85/85 (393v) -- so the translate score IS the headline.
    #[test]
    fn a_verify_less_agents_translate_score_is_the_battery_headline() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        corpus_battery(tmp.path(), "B01", &["only"]);
        let paths = paths_at(
            tmp.path(),
            crate::cli::Tool::C2rust,
            crate::cli::Variant::Default,
        );
        let case = paths.case_dir("B01", "only");
        crate_at(&case.join(Role::Translate.dir()));
        let mut resolved = crate::eval::Resolved::new();
        resolved.insert(
            case.join(Role::Translate.dir()),
            tree_of(&case.join("tree")),
        );

        let tree = EvalTree::create_empty(&paths, "T", Keep::ForPostMortem).unwrap();
        let passes = one_case_pass(&paths, &tree, &resolved, Covers::WholeBattery);
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

    /// A case whose binary never ran is a FAILURE, and the scanner must see it.
    ///
    /// The real line from `codex-gpt56-sol/P00_perlin_noise`, whose crate built but whose `[[bin]]`
    /// the verify phase had removed, so the executable did not exist. runtests printed `[FAIL]` and
    /// counted it in `Test Cases Failed`, but no pattern matched, so it scored as a pass.
    #[test]
    fn a_case_whose_binary_never_ran_is_recorded_as_failed() {
        let real = "- 001_perlin_noise: Execution failed: CommandError(\"Command Failed \
                    target/release/driver FileNotFoundError(2, 'No such file or directory')\")";
        assert_eq!(
            hard_failures(real).unwrap(),
            vec![("001_perlin_noise".to_string(), "execution failed")],
            "runtests counted this in Test Cases Failed, so it must reach failed_cases"
        );
        // The shape that always worked, still working.
        assert_eq!(
            hard_failures("- 014_dead_code: Build failed: error[E0433]").unwrap(),
            vec![("014_dead_code".to_string(), "build failed")]
        );
        // Non-vacuity: a healthy log yields nothing, so this is not matching every line.
        assert!(
            hard_failures("   Executing 001_helloworld\n- Test Vectors Passed: 7")
                .unwrap()
                .is_empty()
        );
    }

    /// Exhaustive over the count shapes, with the real ones named: the whole point is that "every
    /// crate failed to build" and "the oracle never ran" print almost identically, and only one of
    /// them is a result.
    ///
    /// The vector-keyed form of this guard called BOTH of them unmeasured, which made four published
    /// table rows unreproducible by the code that wrote them.
    #[test]
    fn a_battery_where_no_case_reached_a_verdict_measured_nothing() {
        for (discovered, tested, failed, nothing, why) in [
            // No case reached ANY verdict: the oracle did not run. Refuse.
            (128, 0, 0, true, "128 discovered, nothing attempted"),
            (1, 0, 0, true, "one case, nothing attempted"),
            // Every crate failed to build IS a result -- measured on smartc2rust/P01_sphincs_plus,
            // which publishes 0/128 with exactly these counts, as do c2rust, c2saferrust and gpt-5.4.
            (
                128,
                0,
                128,
                false,
                "smartc2rust P01: all 128 failed to build",
            ),
            // codex-gpt56-sol's P00_perlin_noise: one case, binary missing, verdict reached.
            (1, 0, 1, false, "P00: the one case failed to run"),
            // smartc2rust/B01_synthetic: a partial battery.
            (85, 41, 47, false, "partial: 41 tested, 47 failed"),
            (42, 18, 0, false, "everything passed"),
            // Nothing discovered is `nothing_to_score`'s business, not this guard's.
            (0, 0, 0, false, "nothing discovered"),
        ] {
            assert_eq!(
                measured_nothing(discovered, tested, failed),
                nothing,
                "{why}: {discovered} discovered, {tested} tested, {failed} failed"
            );
        }
    }

    /// A run that RESOLVED NOTHING — the shape a battery takes when every case missed.
    fn resolved_nothing(paths: &Paths, target: &str, tree: &EvalTree) -> Result<()> {
        let gate = gate_at(paths);
        let resolved = crate::eval::Resolved::new();
        run_test_corpus(
            paths,
            target,
            &Scoring {
                roles: crate::prompt::chain(paths.tool, paths.variant),
                resolved: &resolved,
                tree,
                gate: &gate,
                covers: Covers::WholeBattery,
            },
        )
    }

    /// Refuting `spec-20.md`: both shapes below once exited 0 having compared nothing — no crate
    /// resolved for the battery, and no corpus battery dir at all (a fresh worktree). Every mode used
    /// to have to be checked separately; there is one now, and it refuses.
    #[test]
    fn a_named_battery_that_materialised_nothing_is_never_a_pass() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        corpus_battery(tmp.path(), "B01", &["case1"]);
        let paths = paths_at(
            tmp.path(),
            crate::cli::Tool::C2rust,
            crate::cli::Variant::Default,
        );
        let case = paths.case_dir("B01", "case1");
        fs::create_dir_all(case.join(Role::Translate.dir()).join("logs")).unwrap();
        stored_1_of_1(&paths, "B01");
        assert!(
            !crate::battery::has_crate(&case.join(Role::Translate.dir())),
            "and no crate for it, or something was scored after all"
        );

        let tree = EvalTree::create_empty(&paths, "T", Keep::ForPostMortem).unwrap();
        let err = resolved_nothing(&paths, "B01", &tree)
            .expect_err("a battery that materialised nothing must refuse, not report a score");
        let text = format!("{err:#}");
        assert!(
            text.contains(HEADLINE_SUMMARY) && text.contains('7'),
            "naming the record it would have been compared against: {text}"
        );
        resolved_nothing(&paths, "all", &tree)
            .expect_err("nor may the fan-out pass when it skipped EVERY battery");

        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let paths = paths_at(
            tmp.path(),
            crate::cli::Tool::C2rust,
            crate::cli::Variant::Default,
        );
        crate_at(&paths.case_dir("B01", "case1").join(Role::Translate.dir()));
        stored_1_of_1(&paths, "B01");
        assert!(
            !paths.input_dir("B01").is_dir(),
            "the second fixture holds a complete crate and NO corpus battery"
        );
        let tree = EvalTree::create_empty(&paths, "T", Keep::ForPostMortem).unwrap();
        let err = resolved_nothing(&paths, "B01", &tree)
            .expect_err("an absent corpus is not an agreeing score either");
        assert!(
            format!("{err:#}").contains("Public-Tests"),
            "naming what is absent, since the record itself was complete: {err:#}"
        );
    }

    /// The `<testsuites>` root carries a `name` too, and counting it files a case called `Tests` — the
    /// same shape as the console header once scored as a case named `test`, which made `reconcile` refuse
    /// every battery. A case that FAILED TO BUILD still gets a suite, with no vectors in it.
    #[test]
    fn the_reports_own_root_element_is_not_scored_as_a_case() {
        let xml = concat!(
            r#"<?xml version="1.0"?><testsuites name="Tests" tests="3" failures="1">"#,
            r#"<testsuite name="001_helloworld" tests="2" failures="0">"#,
            r#"<testcase name="test0" classname="001_helloworld" />"#,
            r#"<testcase name="test1" classname="001_helloworld" /></testsuite>"#,
            r#"<testsuite name="014_dead_code_lib" tests="1" failures="1">"#,
            r#"<testcase name="test0" classname="014_dead_code_lib">"#,
            r#"<failure message="stdout mismatch" /></testcase></testsuite>"#,
            r#"<testsuite name="confusion_lib" tests="0" failures="0" /></testsuites>"#,
        );
        assert_eq!(
            cases_in_report(xml).unwrap(),
            vec!["001_helloworld", "014_dead_code_lib", "confusion_lib"],
            "every suite is a case and the root element is not one"
        );
    }

    /// `run B01 --include-regex one` wrote `cases_tested: 1` over the stored 85 and regenerated
    /// `tables/` from it; `reconcile` compares the subset against itself and agrees.
    #[test]
    fn a_subset_sweep_does_not_file_its_own_count_under_the_batterys_name() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        corpus_battery(tmp.path(), "B01", &["one", "two"]);
        let paths = paths_at(
            tmp.path(),
            crate::cli::Tool::C2rust,
            crate::cli::Variant::Default,
        );
        let case = paths.case_dir("B01", "one");
        crate_at(&case.join(Role::Translate.dir()));
        let mut resolved = crate::eval::Resolved::new();
        resolved.insert(
            case.join(Role::Translate.dir()),
            tree_of(&case.join("tree")),
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

        let tree = EvalTree::create_empty(&paths, "T", Keep::ForPostMortem).unwrap();
        let passes = one_case_pass(&paths, &tree, &resolved, Covers::Subset("one"));
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
        let paths = paths_at(
            tmp.path(),
            crate::cli::Tool::Claude,
            crate::cli::Variant::Combined,
        );

        let fresh = paths.case_dir("B01", "fresh");
        crate_at(&fresh.join(Role::Translate.dir()));
        let dead = paths.case_dir("B01", "dead");
        let log = dead
            .join(Role::Translate.dir())
            .join("logs")
            .join(Role::Translate.log());
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, DEAD).unwrap();
        assert!(
            !crate::battery::has_crate(&dead.join(Role::Translate.dir())),
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
