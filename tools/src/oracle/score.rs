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

pub struct Scoring<'a> {
    pub mode: TestMode,
    pub source: crate::eval::Source<'a>,
    pub tree: &'a crate::eval::Tree,
    pub gate: &'a crate::agent_health::Gate<'a>,
    pub covers: Covers<'a>,
}

/// Which of a battery's cases a score covers: all the corpus holds, or the subset a `--include-regex`
/// sweep touched. It decides the roster the infra gate grades and the right to rewrite the battery
/// summary, which a subset's count may not claim.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Covers<'a> {
    WholeBattery,
    Subset(&'a str),
}

impl<'a> Covers<'a> {
    pub fn from_include_regex(regex: Option<&'a str>) -> Self {
        match regex {
            Some(regex) => Self::Subset(regex),
            None => Self::WholeBattery,
        }
    }

    pub(crate) fn case_filter(self) -> Option<&'a str> {
        match self {
            Self::WholeBattery => None,
            Self::Subset(regex) => Some(regex),
        }
    }
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
