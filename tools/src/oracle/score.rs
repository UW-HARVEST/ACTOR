use serde::{Deserialize, Serialize};

pub struct Scoring<'a> {
    /// Which steps this run actually resolved, in order. Data rather than two typed fields, so a
    /// chain of three needs no third field and no third branch downstream.
    pub roles: &'a [crate::prompt::Role],
    /// Every tree this run produced, by the phase dir it was published into.
    pub resolved: &'a crate::eval::Resolved,
    pub tree: &'a crate::eval::EvalTree,
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
    /// From the run's resolved case filter -- which may have come from `--include-regex` OR from a case
    /// named in the target. Reading only the flag is what let a `battery/case` run claim a whole-battery
    /// roster.
    pub fn of(filter: Option<&'a str>) -> Self {
        match filter {
            Some(filter) => Self::Subset(filter),
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

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Summary {
    pub cases_tested: usize,
    pub cases_passed: usize,
    pub vectors_passed: usize,
    pub vectors_failed: usize,
    pub vectors_skipped: usize,
    pub failed_cases: Vec<String>,
}
