//! Canonical project scoring — the SINGLE source of the pass/build rule, applied by
//! every dataset's scorer. Inlined per-call-site verdicts once diverged: one counted a
//! crate that compiled but ran ZERO ground-truth tests as a pass, scoring our own system
//! more leniently than the systems it was compared against.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectOutcome {
    pub built: bool,
    pub tests_ok: u32,
    pub tests_failed: u32,
}

impl ProjectOutcome {
    /// A crate that compiles but runs zero tests (empty or mis-structured translation,
    /// no runnable test target) is deliberately NOT a pass.
    pub fn passed(&self) -> bool {
        self.tests_ok > 0 && self.tests_failed == 0
    }

    // `allow`, not `expect`: `dead_code` fires in the BIN target (where `scoring` is a
    // private module) but not in the LIB target, so an `expect` would be unfulfilled in
    // one of the two builds — itself a warning.
    #[allow(
        dead_code,
        reason = "the Builds column is reported per-dataset today, but the rule that \
                  compiling and passing are separate measurements belongs with the type \
                  owning both fields, not at each reporting site"
    )]
    pub fn built(&self) -> bool {
        self.built
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(built: bool, tests_ok: u32, tests_failed: u32) -> ProjectOutcome {
        ProjectOutcome { built, tests_ok, tests_failed }
    }

    #[test]
    fn build_but_zero_tests_is_not_a_pass() {
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
        let compiled_no_tests = outcome(true, 0, 0);
        assert!(compiled_no_tests.built());
        assert!(!compiled_no_tests.passed());
    }
}
