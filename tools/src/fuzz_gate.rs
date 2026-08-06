//! Coverage-based fuzz-completeness gate — the mechanical check ON the verify
//! agent, never the agent's job.
//!
//! The claim we enforce: EVERY public function of the C library was actually
//! executed under a coverage-guided fuzzing campaign, so it was differentially
//! compared against the Rust translation on adversarial inputs (not just the few
//! fixed values a hand-written test happens to pick). A function never executed
//! during fuzzing cannot have been fuzz-verified, no matter how many properties
//! the agent claims to have written.
//!
//! This is the fuzz-completeness twin of the translate symbol-parity gate:
//!   S           = public FUNC symbols of the C reference `.so` (`nm -D`)
//!   covered     = C functions with execution count > 0 in the coverage the
//!                 AGENT's own campaigns produced (`llvm-cov export` against the
//!                 coverage-instrumented C `.so`, using the pooled profiles)
//!   left_behind = S − covered      # must be empty
//!
//! We do NOT run campaigns here — the agent decides how long/hard to fuzz (the
//! C reference is built with a pooled `-fprofile-instr-generate` path so its
//! campaigns accumulate coverage), and this module only MEASURES what they left
//! behind. Everything is derived from the C source + the agent's own coverage;
//! we supply the METHOD and the GATE, never inputs, domains, or which functions
//! matter. Zero benchmark-specific content.
//!
//! Validated end-to-end (libpng): FuzzTest's sancov guidance and llvm-cov's
//! `__llvm_covmap` coexist on the C `.so`; a one-property probe campaign yielded
//! exactly `{png_access_version_number}` covered of 528 mapped functions — the
//! precise signal this gate consumes.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Outcome of the completeness gate for one library case.
#[derive(Debug, Clone)]
pub struct GateReport {
    /// Public FUNC symbols of the C reference `.so` (the definition of "done").
    pub symbols: BTreeSet<String>,
    /// C functions actually executed during the fuzzing campaigns.
    pub covered: BTreeSet<String>,
    /// `symbols − covered`: public functions never fuzzed. Empty ⇒ gate passes.
    pub left_behind: BTreeSet<String>,
    /// Whether coverage could be measured at all (false ⇒ gate is inconclusive
    /// and the caller must fall back to the manifest check, logging the downgrade).
    pub measured: bool,
}

impl GateReport {
    pub fn passed(&self) -> bool {
        self.measured && self.left_behind.is_empty()
    }
}

/// Public FUNC symbols exported by a `.so` via `nm -D` (the `S` set). Only
/// defined text symbols (`T`) are counted — undefined (`U`) imports and data
/// objects are not functions to fuzz.
pub fn public_function_symbols(so: &Path) -> Result<BTreeSet<String>> {
    let out = Command::new("nm")
        .args(["-D", "--defined-only"])
        .arg(so)
        .output()
        .context("running nm -D on the C reference .so")?;
    if !out.status.success() {
        anyhow::bail!("nm -D failed on {}", so.display());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter_map(|l| {
            // Format: "<addr> <type> <name>"; type T/t == text (function).
            let mut it = l.split_whitespace();
            let _addr = it.next()?;
            let ty = it.next()?;
            let name = it.next()?;
            if ty.eq_ignore_ascii_case("t") {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect())
}

/// C functions with execution count > 0, read from `llvm-cov export` against the
/// coverage-instrumented C reference `.so` (which carries `__llvm_covmap`) using
/// the merged campaign profile. Returns an empty set (not an error) when the
/// object has no coverage mapping, so the caller can distinguish "measured, none
/// covered" from "could not measure".
pub fn covered_functions(c_so: &Path, profdata: &Path) -> Result<BTreeSet<String>> {
    let out = Command::new("llvm-cov")
        .arg("export")
        .arg("-object")
        .arg(c_so)
        .arg(format!("-instr-profile={}", profdata.display()))
        .output()
        .context("running llvm-cov export")?;
    if !out.status.success() {
        // No coverage mapping / bad profile — surface as an error so the caller
        // treats the gate as inconclusive rather than "0 covered".
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("llvm-cov export failed: {}", err.trim());
    }
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing llvm-cov export JSON")?;
    let mut covered = BTreeSet::new();
    if let Some(funcs) = json
        .pointer("/data/0/functions")
        .and_then(|v| v.as_array())
    {
        for f in funcs {
            let count = f.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
            if count > 0 {
                if let Some(name) = f.get("name").and_then(|n| n.as_str()) {
                    covered.insert(name.to_string());
                }
            }
        }
    }
    Ok(covered)
}

/// Compute the gate report from the C symbol set and the covered set.
pub fn evaluate(symbols: BTreeSet<String>, covered: BTreeSet<String>, measured: bool) -> GateReport {
    // A function counts as fuzzed only if it is BOTH a public symbol and was
    // executed. Restrict `covered` to `symbols` so private/helper functions the
    // campaign happened to run don't paper over an un-fuzzed public entry point.
    let covered_public: BTreeSet<String> = covered.intersection(&symbols).cloned().collect();
    let left_behind: BTreeSet<String> = symbols.difference(&covered_public).cloned().collect();
    GateReport {
        symbols,
        covered: covered_public,
        left_behind,
        measured,
    }
}

/// All candidate coverage-instrumented C reference `.so`s anywhere under
/// `verify_env/`. We do NOT assume a build-dir name: the agent may build in
/// `build-fuzz/`, `build-test/`, `build/`, or elsewhere, and may rebuild — so we
/// gather every `lib*.so` that actually carries an `__llvm_covmap` section (the
/// mark of source-based coverage instrumentation) and let the caller pick the
/// one whose instrumentation matches the pooled profile. Framework `.so`s
/// (gtest/fuzztest/abseil/antlr) are uninstrumented, so they carry no covmap and
/// are naturally excluded.
fn find_covmapped_sos(verify_env: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().map_or(false, |x| x == "so")
                && p.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.starts_with("lib"))
                && so_has_covmap(&p)
            {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(verify_env, &mut out);
    out
}

/// Whether an ELF `.so` carries an `__llvm_covmap` section (source-based
/// coverage instrumentation). Uses `readelf -S`; falls back to false if the tool
/// is unavailable so a missing readelf never masquerades as "instrumented".
fn so_has_covmap(so: &Path) -> bool {
    Command::new("readelf")
        .arg("-S")
        .arg(so)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("__llvm_covmap"))
        .unwrap_or(false)
}

/// Every non-empty `*.profraw` under `root` (recursively). The C reference is
/// built with `-fprofile-instr-generate=<verify_env>/cov/cov-%m.profraw`, so the
/// difftest run pools its coverage there (regardless of CWD, accumulating across
/// runs). We sweep the whole crate tree in case a profile leaked elsewhere.
fn find_profraws(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().map_or(false, |x| x == "profraw")
                // Skip EMPTY pool slots. The `%m` merge pool pre-creates a file
                // per binary hash; slots for binaries that never ran stay 0-byte,
                // and `llvm-profdata merge` folds a 0-byte profile into the result
                // as a DEGENERATE 0-function profile — silently zeroing real
                // coverage. Only merge profraws that actually hold counters.
                && std::fs::metadata(&p).map_or(false, |m| m.len() > 0)
            {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// Measure the coverage the AGENT's own fuzzing campaigns produced, and evaluate
/// the completeness gate against it. Does NOT run any campaign — the agent
/// decides how long/hard to fuzz (guided by the prompt); this only reads what its
/// campaigns left behind. `verify_env` is the materialized env in the agent's
/// workspace (holds a coverage-instrumented C `lib<target>.so` in some build dir
/// + `cov/*.profraw` from the campaigns).
///
/// Two robustness points, learned from real runs:
///   * The agent may build in any dir (`build-fuzz/`, `build-test/`, …) and may
///     rebuild — so we search ALL build dirs for covmap'd `.so`s, not one path.
///   * An `%m`-named profile is keyed to the EXACT instrumented binary that
///     produced it; a rebuilt `.so` won't match and llvm-cov reports 0. So we
///     try every candidate `.so` against the merged profile and keep the pairing
///     that actually resolves coverage (the one the profile belongs to).
///
/// Returns an inconclusive report (`measured=false`) rather than erroring when
/// the pieces are missing (no covmap'd C `.so`, no profiles, or no candidate
/// matches the profile), so a case where the agent never really fuzzed is
/// recorded as "not measured" instead of aborting verify or lying "0 covered".
///
/// `crate_root` is the agent's workspace (`translated_rust/`): it contains both
/// `c_src/build/lib<target>.so` (the coverage-instrumented C reference) and
/// `verify_env/cov/*.profraw` (the profiles the difftest run pooled), so we
/// search the whole tree for each.
pub fn measure_existing(crate_root: &Path) -> Result<GateReport> {
    let candidates = find_covmapped_sos(crate_root);
    if candidates.is_empty() {
        anyhow::bail!(
            "no coverage-instrumented C .so under {} — did the agent build the difftest env (build.sh)?",
            crate_root.display()
        );
    }

    // The public-symbol set S is a property of the library, identical across
    // builds of the same C source — take it from the first candidate.
    let symbols = public_function_symbols(&candidates[0])?;

    // Merge whatever profiles the difftest run pooled (verify_env/cov/*.profraw).
    let raws = find_profraws(crate_root);
    if raws.is_empty() {
        // No campaigns were actually run — coverage is unmeasurable, so nothing
        // can be certified as fuzzed. Report inconclusive (not "0 covered").
        return Ok(GateReport { symbols, covered: BTreeSet::new(), left_behind: BTreeSet::new(), measured: false });
    }
    let profdata = crate_root.join(crate::verify_env::VERIFY_ENV_DIR).join("cov").join("merged.profdata");
    std::fs::create_dir_all(profdata.parent().unwrap())?;
    let mut merge = Command::new("llvm-profdata");
    merge.arg("merge").arg("-sparse");
    for r in &raws {
        merge.arg(r);
    }
    merge.arg("-o").arg(&profdata);
    let out = merge.output().context("merging agent campaign profiles")?;
    if !out.status.success() {
        anyhow::bail!(
            "llvm-profdata merge failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    // Try each candidate .so against the merged profile; keep the one that
    // resolves the most covered PUBLIC functions — that is the binary the
    // profile actually belongs to. A mismatched .so yields 0 (or errors), so the
    // max is the correct pairing.
    let mut best: Option<BTreeSet<String>> = None;
    for so in &candidates {
        if let Ok(cov) = covered_functions(so, &profdata) {
            let public_hits = cov.intersection(&symbols).count();
            let better = best.as_ref().map_or(true, |b| {
                public_hits > b.intersection(&symbols).count()
            });
            if better {
                best = Some(cov);
            }
        }
    }

    match best {
        // A profile that matches no candidate .so (all rebuilt away) leaves
        // coverage unmeasurable — inconclusive, not a false "0 covered".
        None => Ok(GateReport { symbols, covered: BTreeSet::new(), left_behind: BTreeSet::new(), measured: false }),
        Some(cov) if cov.intersection(&symbols).next().is_none() => {
            Ok(GateReport { symbols, covered: BTreeSet::new(), left_behind: BTreeSet::new(), measured: false })
        }
        Some(cov) => Ok(evaluate(symbols, cov, true)),
    }
}

/// Render the gate report as a `FUZZ_GATE.md` artifact (audit trail written
/// alongside verify.log): headline pass/fail, counts, and the left-behind list.
pub fn render_report(report: &GateReport, lib_name: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Fuzz-completeness gate — {lib_name}\n\n"));
    if !report.measured {
        s.push_str("**INCONCLUSIVE** — coverage could not be measured (see log). \
                    Falling back to the FUZZ.md manifest check.\n");
        return s;
    }
    let (n_s, n_cov, n_left) = (
        report.symbols.len(),
        report.covered.len(),
        report.left_behind.len(),
    );
    let verdict = if report.passed() { "PASS ✅" } else { "FAIL ❌" };
    s.push_str(&format!(
        "**{verdict}** — {n_cov}/{n_s} public functions fuzzed; {n_left} left behind.\n\n"
    ));
    if !report.left_behind.is_empty() {
        s.push_str("## Left behind (never executed under fuzzing)\n\n");
        for f in &report.left_behind {
            s.push_str(&format!("- {f}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_flags_left_behind() {
        let s: BTreeSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let cov: BTreeSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let r = evaluate(s, cov, true);
        assert!(!r.passed());
        assert_eq!(r.left_behind.iter().cloned().collect::<Vec<_>>(), vec!["c"]);
    }

    #[test]
    fn evaluate_passes_when_all_covered() {
        let s: BTreeSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        // A private helper the campaign also ran is ignored (not a public symbol).
        let cov: BTreeSet<String> = ["a", "b", "helper_priv"].iter().map(|s| s.to_string()).collect();
        let r = evaluate(s, cov, true);
        assert!(r.passed());
        assert!(r.left_behind.is_empty());
        assert!(!r.covered.contains("helper_priv"), "private funcs excluded from covered-public");
    }

    #[test]
    fn evaluate_inconclusive_when_unmeasured() {
        let s: BTreeSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        let r = evaluate(s, BTreeSet::new(), false);
        assert!(!r.passed(), "unmeasured coverage never passes the gate");
    }

    #[test]
    fn find_profraws_skips_empty_pool_slots() {
        // The %m merge pool pre-creates a 0-byte file per binary hash; merging one
        // in silently zeroes real coverage. find_profraws must drop empties.
        let tmp = tempfile::tempdir().unwrap();
        let ve = tmp.path();
        std::fs::create_dir_all(ve.join("cov")).unwrap();
        std::fs::write(ve.join("cov/empty.profraw"), b"").unwrap();
        std::fs::write(ve.join("cov/real.profraw"), b"not empty").unwrap();
        std::fs::write(ve.join("cov/notprof.txt"), b"ignore me").unwrap();
        let found = find_profraws(ve);
        assert_eq!(found.len(), 1, "only the non-empty .profraw is kept");
        assert!(found[0].ends_with("real.profraw"));
    }
}

