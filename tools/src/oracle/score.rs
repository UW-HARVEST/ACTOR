use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub enum TestMode {
    Run,
    /// Run, then write summary.json / result.json.
    Update,
    /// Run, then compare against stored summary.json. Returns failure on mismatch.
    Check,
}

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

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Summary {
    pub cases_tested: usize,
    pub cases_passed: usize,
    pub vectors_passed: usize,
    pub vectors_failed: usize,
    pub vectors_skipped: usize,
    pub failed_cases: Vec<String>,
}
