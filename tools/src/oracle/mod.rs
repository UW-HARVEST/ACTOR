//! Running a reference implementation and grading a translation against it.

pub mod gtest;
pub mod runtests;
pub mod score;

pub use gtest::run_harvest_bench_test;
pub use runtests::run_test_corpus;
pub use score::{Covers, Scoring, Summary};

use anyhow::Result;
use std::path::Path;

/// Translated crates that pull in `openssl-sys` otherwise fail to build for
/// environmental reasons unrelated to the translation.
fn openssl_dir() -> String {
    std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into())
}

// ── Enrichment: the ONE definition of result.json metadata ─────────────
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

/// Backfill `result.json` (unsafe/loc/credits) for the cases NAMED, and no others.
///
/// This was `enrich_test_corpus(paths, battery)`, which harvest-bench called with an EMPTY battery so
/// its `read_dir` would land a level higher. Taking case dirs removes the coincidence and the walk.
pub fn enrich_cases(cases: &[&Path]) -> Result<usize> {
    let mut enriched = 0usize;
    for case_dir in cases {
        // Each phase's result.json is enriched against ITS OWN crate. enrich_file
        // no-ops on absent files, so single-phase cases just skip verified/.
        for phase in [crate::battery::TRANSLATED, crate::battery::VERIFIED] {
            let pdir = crate::battery::phase_dir(case_dir, phase);
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
    Ok(enriched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// What `merge_into` actually records into a scored `result.json`. Its inverse,
    /// `check_enrichment`, went with the `--check` mode that was its only caller.
    #[test]
    fn merge_into_records_the_unsafe_blocks_and_loc_it_measured() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
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

        let stored: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&rj).unwrap()).unwrap();
        assert_eq!(
            stored["unsafe"]["blocks"], 1,
            "the one unsafe block is counted"
        );
        assert!(
            stored["loc"]["code"].as_u64().unwrap() >= 2,
            "and the code it measured is not zero"
        );
    }

    /// `result.json` is still written back INTO the artifact directory, so it must stay outside the
    /// digest and the one traversal policy; the rest lands in [`crate::eval`]'s tree.
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
}
