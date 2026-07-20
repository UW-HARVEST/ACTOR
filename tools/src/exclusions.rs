//! Canonical CRUST-Bench project exclusion list — the SINGLE source of truth.
//!
//! CRUST-Bench ships 100 C projects. The paper reports results over 87 of them,
//! excluding 13 with defective test suites / interfaces that no correct
//! translation can satisfy (verified case-by-case: the C reference itself fails,
//! or the ground-truth Rust test is broken/asserts non-C behavior).
//!
//! The 13 split into two operational categories:
//!   * `NOT_RUN` (5): excluded at *discovery* time — never translated or scored.
//!     These predate the current analysis (hangs, hardcoded absolute paths, etc).
//!   * `EXCLUDED_FROM_COUNT` (8): translated and present in results/, but removed
//!     from the reported denominator because their ground-truth tests are broken.
//!
//! 100 − 5 (NOT_RUN, absent from results/) − 8 (EXCLUDED_FROM_COUNT, present in
//! results/ but not counted) = 87 reported projects.
//!
//! Both `battery.rs` (discovery/validation) and `report.rs` (scoring denominator)
//! consume this module, so the 87 denominator is reproducible from one place.

/// Total CRUST-Bench projects shipped by the benchmark.
pub const CRUST_TOTAL: usize = 100;

/// Reported denominator after removing the 13 excluded projects.
pub const CRUST_REPORTED: usize = 87;

/// Projects skipped at discovery time — never translated/scored (absent from
/// results/). Reasons are infrastructure/benchmark defects unrelated to ACTOR.
pub const NOT_RUN: &[&str] = &[
    "Genetic_neural_network_for_simple_control", // C test >120s with -O2; CRUST-bench issue 40
    "Holdem_Odds",                               // contradictory tests; CRUST-bench issue 37
    "VaultSync",                                 // test hardcodes /home/elhalili/... absolute path
    "bitset",                                    // test uses bs.test() but C checks raw bits; issue 41
    "clog",                                      // THIS_FILE hardcodes C filename; issue 39
];

/// Projects present in results/ but removed from the reported denominator because
/// their ground-truth tests are broken (verified: C reference also fails, or the
/// test asserts non-C / undefined behavior, or the harness/fixture is defective).
pub const EXCLUDED_FROM_COUNT: &[&str] = &[
    "cissy",           // benchmark test defect
    "libpgn",          // benchmark test defect
    "libwecan",        // benchmark test defect
    "razz_simulation", // benchmark test defect
    "fs_c",            // benchmark test defect (flaky filesystem tests)
    "utf8",            // test feeds illegal UTF-8; C relies on arbitrary bytes
    "inversion_list",  // test asserts C's accidental NULL-as-terminator behavior
    "tisp",            // test harness never prints the failing sub-test
];

/// Normalize a project name for comparison: hyphens→underscores, lowercased.
/// The corpus dir uses hyphens (e.g. `inversion-list`, `Holdem-Odds`) while
/// results dirs use underscores; this makes matching robust across both.
fn norm(name: &str) -> String {
    name.replace('-', "_").to_lowercase()
}

/// True if a project is skipped at discovery time (never scored).
pub fn is_not_run(name: &str) -> bool {
    let n = norm(name);
    NOT_RUN.iter().any(|p| norm(p) == n)
}

/// True if a project must be removed from the reported denominator.
/// This covers BOTH categories, so a project scored in results/ but in
/// `EXCLUDED_FROM_COUNT` is not counted.
pub fn is_excluded(name: &str) -> bool {
    let n = norm(name);
    NOT_RUN.iter().chain(EXCLUDED_FROM_COUNT).any(|p| norm(p) == n)
}

/// Number of distinct excluded projects (must be 13).
pub fn excluded_count() -> usize {
    NOT_RUN.len() + EXCLUDED_FROM_COUNT.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thirteen_exclusions_yield_eighty_seven() {
        assert_eq!(excluded_count(), 13, "expected 13 excluded projects");
        assert_eq!(
            CRUST_TOTAL - excluded_count(),
            CRUST_REPORTED,
            "100 - 13 must equal the reported 87"
        );
    }

    #[test]
    fn no_overlap_between_categories() {
        for p in NOT_RUN {
            assert!(!EXCLUDED_FROM_COUNT.contains(p), "{p} in both lists");
        }
    }
}
