//! Canonical project scoring — the SINGLE source of the pass/build rule.
//!
//! A scored project reduces to three facts: did the translated crate compile,
//! how many ground-truth tests passed, and how many failed. Every dataset's
//! scorer normalizes its own on-disk `result.json` shape into this
//! [`ProjectOutcome`] and then asks ONE predicate, [`ProjectOutcome::passed`],
//! for the verdict.
//!
//! That indirection exists because it was once absent. Each call site inlined
//! its own verdict and they diverged: one path scored `build_ok &&
//! tests_failed == 0` — so a crate that compiled but ran ZERO ground-truth
//! tests counted as a pass — while another scored `ok > 0 && fail == 0`. The
//! first rule is strictly more generous, which scored our own system more
//! leniently than the systems it was compared against. Keeping the rule in one
//! place, applied by every scorer, is what removes that possibility.

/// A per-project outcome, normalized so a single pass/build rule can be applied
/// uniformly to every system and dataset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectOutcome {
    /// The translated crate compiled.
    pub built: bool,
    /// Ground-truth tests that passed.
    pub tests_ok: u32,
    /// Ground-truth tests that failed.
    pub tests_failed: u32,
}

impl ProjectOutcome {
    /// THE canonical pass rule, applied identically to every system: at least
    /// one ground-truth test passed and none failed. A crate that compiles but
    /// runs zero tests (an empty or mis-structured translation with no runnable
    /// test target) is NOT a pass — this is what keeps every system on exactly
    /// equal footing.
    pub fn passed(&self) -> bool {
        self.tests_ok > 0 && self.tests_failed == 0
    }

    /// Whether the crate compiled (the "Builds" column). Distinct from
    /// [`passed`](Self::passed) on purpose: "compiles" and "is correct" are
    /// separate measurements and a report must never conflate them.
    #[allow(dead_code)] // reported per-dataset today; the rule lives here.
    pub fn built(&self) -> bool {
        self.built
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a candidate outcome the way a scorer does.
    fn outcome(built: bool, tests_ok: u32, tests_failed: u32) -> ProjectOutcome {
        ProjectOutcome { built, tests_ok, tests_failed }
    }

    #[test]
    fn build_but_zero_tests_is_not_a_pass() {
        // The exact degenerate case: compiles, runs 0 tests.
        let o = outcome(true, 0, 0);
        assert!(o.built());
        assert!(!o.passed(), "0 passing / 0 failing must NOT count as a pass");
    }

    #[test]
    fn genuine_pass() {
        let o = outcome(true, 6, 0);
        assert!(o.built() && o.passed());
    }

    #[test]
    fn any_failing_test_is_not_a_pass() {
        let o = outcome(true, 5, 1);
        assert!(o.built(), "a crate can compile and still fail its tests");
        assert!(!o.passed(), "any recorded failure blocks a pass");
    }

    #[test]
    fn build_failure_is_not_a_pass() {
        let o = outcome(false, 0, 0);
        assert!(!o.built() && !o.passed());
    }

    #[test]
    fn built_and_passed_are_independent() {
        // A crate that compiled but ran zero tests counts for Builds, never for
        // Tests. Conflating the two is the drift this module exists to prevent.
        let compiled_no_tests = outcome(true, 0, 0);
        assert!(compiled_no_tests.built());
        assert!(!compiled_no_tests.passed());
    }
}
