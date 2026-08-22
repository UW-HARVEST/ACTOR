use serde::{Deserialize, Serialize};

pub struct Scoring<'a> {
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

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Summary {
    pub cases_tested: usize,
    pub cases_passed: usize,
    pub vectors_passed: usize,
    pub vectors_failed: usize,
    pub vectors_skipped: usize,
    pub failed_cases: Vec<String>,
}

impl Summary {
    /// A battery's record is the SUM of its cases': `B02_organic` 44/261/2/11 either way (`spec-27.md`).
    pub fn absorb(&mut self, case: Summary) {
        self.cases_tested += case.cases_tested;
        self.cases_passed += case.cases_passed;
        self.vectors_passed += case.vectors_passed;
        self.vectors_failed += case.vectors_failed;
        self.vectors_skipped += case.vectors_skipped;
        self.failed_cases.extend(case.failed_cases);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field: one left out of the sum reads low for the whole battery.
    #[test]
    fn absorbing_a_case_adds_every_field_of_it() {
        let case = |t, p, vp, vf, vs, failed: &[&str]| Summary {
            cases_tested: t,
            cases_passed: p,
            vectors_passed: vp,
            vectors_failed: vf,
            vectors_skipped: vs,
            failed_cases: failed.iter().map(|s| s.to_string()).collect(),
        };

        let mut battery = Summary::default();
        battery.absorb(case(1, 1, 7, 0, 2, &[]));
        battery.absorb(case(1, 0, 3, 4, 1, &["broken"]));
        battery.absorb(case(1, 1, 5, 0, 0, &[]));

        assert_eq!(
            (
                battery.cases_tested,
                battery.cases_passed,
                battery.vectors_passed,
                battery.vectors_failed,
                battery.vectors_skipped,
                battery.failed_cases.as_slice(),
            ),
            (3, 2, 15, 4, 3, ["broken".to_string()].as_slice()),
            "three cases summed, and the one that failed named"
        );
    }
}
