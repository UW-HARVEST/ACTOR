//! Running a reference implementation and grading a translation against it.

pub mod gtest;
pub mod runtests;
pub mod score;

pub use gtest::run_harvest_bench_test;
pub use runtests::run_test_corpus;
pub use score::{BatteryMismatch, Summary, TestMode, TestOutcome};

use crate::battery::Paths;
use anyhow::Result;
use std::path::Path;

/// Translated crates that pull in `openssl-sys` otherwise fail to build for
/// environmental reasons unrelated to the translation.
fn openssl_dir() -> String {
    std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into())
}

// ── Enrichment: the ONE definition of result.json metadata ─────────────
//
// INVARIANT: every writer of result.json metadata goes through `merge_into`,
// and [`check_enrichment`] stays a pure inverse of it. Duplicating either side
// is how a stored result.json drifts from what `test --check` recomputes.
pub struct Enrichment {
    unsafe_: crate::analyse::metrics::UnsafeCounts,
    loc: crate::analyse::metrics::LocCounts,
    /// Only phases whose log existed, in the given key order.
    meta: Vec<(String, crate::battery::AgentRunMeta)>,
}

impl Enrichment {
    /// `logs` maps each result.json phase key to that phase's agent log.
    pub fn compute(src_dir: &Path, logs: &[(&str, &Path)]) -> Self {
        let meta = logs
            .iter()
            .filter_map(|(key, log)| {
                crate::battery::extract_agent_meta(log).map(|m| (key.to_string(), m))
            })
            .collect();
        Self {
            unsafe_: crate::analyse::metrics::count_unsafe(src_dir),
            loc: crate::analyse::metrics::count_loc(src_dir),
            meta,
        }
    }

    /// `json` must be an object, or a null (which serde_json promotes to an empty
    /// object): assigning through `json[key]` panics on any other value. Both
    /// in-process callers build their object here from a struct or a `json!`
    /// literal; the one caller that reads a `Value` off disk checks first, in
    /// `enrich_file`.
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
        if !rj.exists() {
            return Ok(false);
        }
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
    let Ok(data) = std::fs::read_to_string(result_json) else {
        return diffs;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else {
        return diffs;
    };

    let live = Enrichment::compute(src_dir, log_paths);

    match json.get("unsafe") {
        Some(stored) => {
            let sb = stored.get("blocks").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let sf = stored.get("fns").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let si = stored.get("impls").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if sb != live.unsafe_.blocks {
                diffs.push(format!(
                    "unsafe.blocks expected={sb} actual={}",
                    live.unsafe_.blocks
                ));
            }
            if sf != live.unsafe_.fns {
                diffs.push(format!(
                    "unsafe.fns expected={sf} actual={}",
                    live.unsafe_.fns
                ));
            }
            if si != live.unsafe_.impls {
                diffs.push(format!(
                    "unsafe.impls expected={si} actual={}",
                    live.unsafe_.impls
                ));
            }
            let sl = stored.get("lines").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if sl != live.unsafe_.lines {
                diffs.push(format!(
                    "unsafe.lines expected={sl} actual={}",
                    live.unsafe_.lines
                ));
            }
        }
        None => diffs.push("missing unsafe field".into()),
    }

    match json.get("loc") {
        Some(stored) => {
            let sc = stored.get("code").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if sc != live.loc.code {
                diffs.push(format!("loc.code expected={sc} actual={}", live.loc.code));
            }
        }
        None => diffs.push("missing loc field".into()),
    }

    // `live.meta` is filtered and keyed exactly as merge_into's, so a phase whose
    // log is absent is simply not compared.
    let require_credits = matches!(agent, crate::cli::Agent::Kiro);
    for (key, live) in &live.meta {
        match json.get(key) {
            Some(stored) => {
                let sc = stored
                    .get("credits")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let sw = stored
                    .get("wall_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if (sc - live.credits.as_f64()).abs() > 0.001 {
                    diffs.push(format!(
                        "{key}.credits expected={sc} actual={}",
                        live.credits.as_f64()
                    ));
                }
                if sw != live.wall_secs {
                    diffs.push(format!(
                        "{key}.wall_secs expected={sw} actual={}",
                        live.wall_secs
                    ));
                }
            }
            None if require_credits => diffs.push(format!("missing {key} field")),
            None => {}
        }
    }
    diffs
}

pub fn enrich_test_corpus(paths: &Paths, battery: &str) -> Result<()> {
    let output_dir = paths.results_dir.join(battery);
    if !output_dir.is_dir() {
        return Ok(());
    }
    let mut enriched = 0usize;
    for entry in std::fs::read_dir(&output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
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
            )? {
                enriched += 1;
            }
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
        let tmp = crate::workdir::test_tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            "pub fn f() { unsafe { let _p = 1u8 as *const u8; } }\npub fn g() {}\n",
        )
        .unwrap();

        // No logs on disk → no meta phases; a claude-family agent (no credits).
        let rj = tmp.path().join("result.json");
        let mut json = serde_json::json!({"passed": true});
        let missing = tmp.path().join("nope.log");
        Enrichment::compute(&src, &[("translate", &missing)]).merge_into(&mut json);
        fs::write(&rj, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let diffs = check_enrichment(
            &rj,
            &src,
            &[("translate", &missing)],
            crate::cli::Agent::Claude,
        );
        assert!(
            diffs.is_empty(),
            "merge_into output should pass its own check: {diffs:?}"
        );

        // And it actually recorded the unsafe block + loc (not a vacuous pass).
        let stored: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&rj).unwrap()).unwrap();
        assert_eq!(stored["unsafe"]["blocks"], 1);
        assert!(stored["loc"]["code"].as_u64().unwrap() >= 2);
    }

    /// Moving the scorer's build out of `results/` is measurement-neutral only if every
    /// file the scorer writes back into a scored crate is outside the digest and outside
    /// every [`crate::domain::contents::Carry`]. Enumerated beside the writers: `write_results`
    /// and `run_harvest_bench_test` (result.json, logs/test.log),
    /// `score_harvest_bench_suite` (harvest_bench_report.json, gtest_build/) and
    /// `build_harvest_bench_lib` (target/).
    #[test]
    fn every_file_the_scorer_writes_back_is_excluded_from_the_artifact() {
        use crate::domain::contents::{classify, Disposition};
        use crate::domain::relpath::RelPath;
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
        let tmp = crate::workdir::test_tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            "pub fn f() { unsafe { let _x = 0; } }\n",
        )
        .unwrap();

        let rj = tmp.path().join("result.json");
        let mut json = serde_json::json!({});
        let _missing = tmp.path().join("nope.log");
        Enrichment::compute(&src, &[]).merge_into(&mut json);
        json["unsafe"]["blocks"] = serde_json::json!(99); // tamper
        fs::write(&rj, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let diffs = check_enrichment(&rj, &src, &[], crate::cli::Agent::Claude);
        assert!(
            diffs.iter().any(|d| d.contains("unsafe.blocks")),
            "tamper should be caught: {diffs:?}"
        );
    }
}
