//! Canonical CRUST-Bench evaluation — the SINGLE source of the pass/build rule.
//!
//! CRUST results live in two on-disk formats:
//!   * ACTOR runs write a per-project `result.json` (`build_ok`, `tests_ok`/
//!     `tests_failed` for test-repair, `real_tests_ok`/`real_tests_failed` for
//!     the blind / self-generated setting).
//!   * CRUST-Bench baselines write a `test_report_<N>.json` array of
//!     `{project, ok, fail}` (no build flag).
//!
//! Historically each call site inlined its own verdict, and they diverged: ACTOR
//! was scored `build_ok && tests_failed == 0` (a crate that compiles but runs
//! ZERO ground-truth tests counted as a pass), while baselines were scored
//! `ok > 0 && fail == 0`. That scored ACTOR more leniently than the systems it is
//! compared against. This module removes the possibility: BOTH formats parse into
//! one `CrustOutcome`, and ONE `passed()` predicate is applied to every system.

/// A per-project CRUST outcome, normalized across the two on-disk formats so a
/// single pass/build rule can be applied uniformly to ACTOR and the baselines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrustOutcome {
    /// The translated crate compiled. For ACTOR this is the `build_ok` flag; for
    /// baselines (which carry no build flag) it is inferred as "at least one test
    /// ran", i.e. `tests_ok + tests_failed > 0`.
    pub built: bool,
    /// Ground-truth tests that passed.
    pub tests_ok: u32,
    /// Ground-truth tests that failed.
    pub tests_failed: u32,
}

impl CrustOutcome {
    /// Parse an ACTOR `result.json` value. Accepts BOTH schemas: test-repair
    /// (`tests_ok`/`tests_failed`) and blind/self-generated
    /// (`real_tests_ok`/`real_tests_failed`), preferring the plain keys.
    pub fn from_actor(v: &serde_json::Value) -> Self {
        let tests_ok = v
            .get("tests_ok")
            .or_else(|| v.get("real_tests_ok"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        let tests_failed = v
            .get("tests_failed")
            .or_else(|| v.get("real_tests_failed"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        let built = v.get("build_ok").and_then(|x| x.as_bool()).unwrap_or(false);
        Self { built, tests_ok, tests_failed }
    }

    /// Parse a CRUST-Bench baseline `{project, ok, fail}` item. When we re-score a
    /// baseline ourselves we also record a real `built` flag (`cargo` compiled the
    /// crate); honor it if present. The CRUST-Bench authors' original reports carry
    /// no build flag, so there `built` falls back to "at least one test ran" — a
    /// lower bound on compilation.
    pub fn from_baseline(v: &serde_json::Value) -> Self {
        let tests_ok = v.get("ok").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        let tests_failed = v.get("fail").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        let built = v
            .get("built")
            .and_then(|x| x.as_bool())
            .unwrap_or(tests_ok + tests_failed > 0);
        Self { built, tests_ok, tests_failed }
    }

    /// THE canonical CRUST pass rule, applied identically to every system: at
    /// least one ground-truth test passed and none failed. A crate that compiles
    /// but runs zero tests (an empty or mis-structured translation with no runnable
    /// test target) is NOT a pass — this is what keeps ACTOR and the baselines on
    /// exactly equal footing.
    pub fn passed(&self) -> bool {
        self.tests_ok > 0 && self.tests_failed == 0
    }

    /// Whether the crate compiled (the "Builds" column).
    pub fn built(&self) -> bool {
        self.built
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_but_zero_tests_is_not_a_pass() {
        // The exact degenerate case (e.g. libfor, hamta): compiles, runs 0 tests.
        let o = CrustOutcome::from_actor(&json!({"build_ok": true, "tests_ok": 0, "tests_failed": 0}));
        assert!(o.built());
        assert!(!o.passed(), "0 passing / 0 failing must NOT count as a pass");
    }

    #[test]
    fn empty_translation_is_not_a_pass() {
        // kiro impcheck: loc=0, build_ok=true (nothing to compile), no tests.
        let o = CrustOutcome::from_actor(&json!({"build_ok": true, "loc": {"code": 0}, "tests_ok": 0, "tests_failed": 0}));
        assert!(!o.passed());
    }

    #[test]
    fn genuine_pass_test_repair_schema() {
        let o = CrustOutcome::from_actor(&json!({"build_ok": true, "tests_ok": 6, "tests_failed": 0}));
        assert!(o.built() && o.passed());
    }

    #[test]
    fn blind_schema_uses_real_tests() {
        let pass = CrustOutcome::from_actor(&json!({"build_ok": true, "real_tests_ok": 4, "real_tests_failed": 0}));
        assert!(pass.passed());
        let fail = CrustOutcome::from_actor(&json!({"build_ok": true, "real_tests_ok": 3, "real_tests_failed": 1}));
        assert!(!fail.passed());
    }

    #[test]
    fn any_failing_test_is_not_a_pass() {
        let o = CrustOutcome::from_actor(&json!({"build_ok": true, "tests_ok": 5, "tests_failed": 1}));
        assert!(!o.passed());
    }

    #[test]
    fn build_failure_is_not_a_pass() {
        let o = CrustOutcome::from_actor(&json!({"build_ok": false, "tests_ok": 0, "tests_failed": 0}));
        assert!(!o.built() && !o.passed());
    }

    #[test]
    fn baseline_parse_and_pass_rule_matches_actor() {
        // Same numbers -> same verdict, regardless of source format.
        let base = CrustOutcome::from_baseline(&json!({"project": "x", "ok": 3, "fail": 0}));
        let actor = CrustOutcome::from_actor(&json!({"build_ok": true, "tests_ok": 3, "tests_failed": 0}));
        assert_eq!(base.passed(), actor.passed());
        assert!(base.passed());

        // Baseline 0/0 -> not built, not passed (mirrors ACTOR degenerate case).
        let degen = CrustOutcome::from_baseline(&json!({"project": "libfor", "ok": 0, "fail": 0}));
        assert!(!degen.built() && !degen.passed());
    }

    #[test]
    fn any_recorded_failure_is_not_a_pass() {
        // The pass rule (tests_ok>0 && tests_failed==0) is unchanged and matches
        // CRUST-Bench's downstream `ok>0 && fail==0`. NOTE: counting is now done in
        // test.rs by CRUST-Bench's protocol (count `... ok`/`... FAILED`, ignore
        // exit code), so a binary that aborts after printing `... ok` lines counts
        // those as passes — matching upstream. This test only checks that a recorded
        // failure blocks a pass.
        let with_fail = CrustOutcome::from_actor(
            &json!({"build_ok": true, "tests_ok": 4, "tests_failed": 1}),
        );
        assert!(with_fail.built());
        assert!(!with_fail.passed(), "any recorded ... FAILED blocks a pass");
    }

    #[test]
    fn explicit_built_flag_beats_ran_a_test_inference() {
        // A crate that compiled but ran zero tests: with a real build flag we know
        // it built (Builds should count it), even though no test ran (not a pass).
        let compiled_no_tests =
            CrustOutcome::from_baseline(&json!({"project": "libfor", "ok": 0, "fail": 0, "built": true}));
        assert!(compiled_no_tests.built(), "explicit built:true must be honored");
        assert!(!compiled_no_tests.passed(), "0 tests is still not a pass");

        // Explicit built:false overrides the (ran-a-test) inference too.
        let not_built =
            CrustOutcome::from_baseline(&json!({"project": "x", "ok": 0, "fail": 0, "built": false}));
        assert!(!not_built.built());
    }
}
