//! One lifecycle — translate → [verify?] → enrich → score(test) — written once in
//! `main.rs` against this trait; datasets differ only in how each phase is carried out.
//! Each `impl` must stay thin, delegating to `translate` / `verify` / `oracle` rather than
//! reimplementing them.

use crate::agents::invocation::has_verify_phase;
use crate::battery::{self, Paths};
use crate::cli::{Agent, Dataset};
use crate::oracle::{self, Scoring, TestOutcome};
use crate::translate::Translations;
use crate::verify::Verifications;
use crate::{translate, verify};
use anyhow::Result;
use std::path::Path;

pub trait Benchmark {
    #[allow(
        dead_code,
        reason = "part of the trait's public surface so a caller holding a \
                  Box<dyn Benchmark> can identify it; not every call site needs it yet"
    )]
    fn name(&self) -> &'static str;

    /// Does a separate C-as-oracle verify phase run for this agent?
    fn verifies(&self, agent: Agent) -> bool;

    /// Returns what it RESOLVED, per case: a value this run produced, not a phase dir to be asked.
    fn translate(
        &self,
        paths: &Paths,
        target: &str,
        filter: Option<&str>,
        parallel: usize,
    ) -> Result<Translations>;

    /// Reached from `Run` only when [`Self::verifies`] is true, but also invoked directly by
    /// the `verify` subcommand, so an impl cannot assume that gate ran.
    ///
    /// `translations` is the hand-off: a case absent from it is refused, not seeded from disk.
    fn verify(
        &self,
        _paths: &Paths,
        _target: &str,
        _filter: Option<&str>,
        _force: bool,
        _parallel: usize,
        _translations: &Translations,
    ) -> Result<Verifications> {
        Ok(Verifications::new())
    }

    fn test(&self, paths: &Paths, target: &str, scoring: &Scoring<'_>) -> Result<TestOutcome>;

    /// Backfills result.json (unsafe/loc/credits); already folded into `test --update`,
    /// so the `enrich` subcommand only re-runs this step.
    fn enrich(&self, paths: &Paths, target: &str) -> Result<()>;
}

pub fn for_dataset(d: Dataset) -> Box<dyn Benchmark> {
    match d {
        Dataset::TestCorpus => Box::new(TestCorpus),
        Dataset::HarvestBench => Box::new(HarvestBench),
    }
}

fn resolve_batteries(corpus_dir: &Path, target: &str) -> Result<Vec<String>> {
    if target == "all" {
        battery::all_batteries(corpus_dir)
    } else {
        Ok(vec![target.to_string()])
    }
}

fn resolve_harvest_bench_projects(
    corpus_dir: &Path,
    target: &str,
) -> Result<Vec<battery::HarvestBenchProject>> {
    if target.eq_ignore_ascii_case("hb") || target == "all" {
        battery::HarvestBenchProject::discover(corpus_dir)
    } else {
        Ok(vec![battery::HarvestBenchProject::resolve(
            corpus_dir, target,
        )?])
    }
}

/// The one filter for a case list, from the two places a caller can name one.
///
/// `--include-regex` was accepted, documented in `--help`, threaded through `main.rs` into this
/// trait -- and then discarded by every impl, which took it as `_filter`. So asking for one case
/// silently ran the whole battery, and at harvest-bench prices that is a four-figure mistake
/// rather than a small one. `battery::discover` applies ONE regex, and this crate's `regex` has no
/// lookahead to and them together, so naming a case in the target AND passing the flag is refused
/// rather than resolved by preferring one -- silently dropping either is the bug being fixed.
fn one_filter(from_target: Option<String>, from_flag: Option<&str>) -> Result<Option<String>> {
    match (from_target, from_flag) {
        (Some(t), Some(f)) => anyhow::bail!(
            "the target names a case ({t}) and --include-regex was also given ({f}); both filter \
             the same list and only one can apply, so pass one or the other"
        ),
        (Some(t), None) => Ok(Some(t)),
        (None, Some(f)) => Ok(Some(f.to_string())),
        (None, None) => Ok(None),
    }
}

/// Harvest-bench's unit is a project, not a case, and nothing below it filters -- so a filter
/// here cannot be honoured. Refuse instead of accepting it and doing nothing.
fn no_filter_here(from_flag: Option<&str>) -> Result<()> {
    if let Some(f) = from_flag {
        anyhow::bail!(
            "--include-regex ({f}) does not apply to harvest-bench: its unit is a project, and \
             nothing under it filters by case. Name the project in the target instead"
        );
    }
    Ok(())
}

/// Split a `battery` or `battery/case` target into (battery, optional case regex).
fn parse_target(target: &str) -> (String, Option<String>) {
    if let Some((battery, case)) = target.split_once('/') {
        (battery.to_string(), Some(format!("{}$", case)))
    } else {
        (target.to_string(), None)
    }
}

struct TestCorpus;

impl Benchmark for TestCorpus {
    fn name(&self) -> &'static str {
        "test-corpus"
    }

    fn verifies(&self, agent: Agent) -> bool {
        has_verify_phase(agent)
    }

    fn translate(
        &self,
        paths: &Paths,
        target: &str,
        filter_flag: Option<&str>,
        parallel: usize,
    ) -> Result<Translations> {
        let batteries = resolve_batteries(&paths.corpus_dir, target)?;
        let mut resolved = Translations::new();

        // Shared-source batteries must run single-threaded: their follower configs are
        // propagated from one real translation. Independent batteries split the rest of
        // the parallel budget.
        if batteries.len() > 1 && parallel > 1 {
            let (shared_bats, indie_bats): (Vec<&str>, Vec<&str>) = batteries
                .iter()
                .map(String::as_str)
                .partition(|b| battery::has_shared_source_groups(&paths.corpus_dir, b));

            let indie_parallel = parallel.saturating_sub(shared_bats.len()).max(1);

            let mut errors: Vec<anyhow::Error> = Vec::new();
            // Merged as each thread joins; case dirs are disjoint, so no merge drops one.
            std::thread::scope(|s| {
                let mut handles = Vec::new();
                for bat in &shared_bats {
                    handles.push(s.spawn(move || -> Result<Translations> {
                        let (name, filter) = parse_target(bat);
                        let filter = one_filter(filter, filter_flag)?;
                        translate::run_test_corpus(paths, &name, filter.as_deref(), 1)
                    }));
                }
                if !indie_bats.is_empty() {
                    handles.push(s.spawn(|| -> Result<Translations> {
                        let mut mine = Translations::new();
                        for bat in &indie_bats {
                            let (name, filter) = parse_target(bat);
                            let filter = one_filter(filter, filter_flag)?;
                            mine.extend(translate::run_test_corpus(
                                paths,
                                &name,
                                filter.as_deref(),
                                indie_parallel,
                            )?);
                        }
                        Ok(mine)
                    }));
                }
                for h in handles {
                    match h.join() {
                        Ok(Ok(t)) => resolved.extend(t),
                        Ok(Err(e)) => errors.push(e),
                        Err(_) => errors.push(anyhow::anyhow!("translate thread panicked")),
                    }
                }
            });
            if let Some(first) = errors.into_iter().next() {
                return Err(first);
            }
        } else {
            for bat in &batteries {
                let (name, filter) = parse_target(bat);
                let filter = one_filter(filter, filter_flag)?;
                resolved.extend(translate::run_test_corpus(
                    paths,
                    &name,
                    filter.as_deref(),
                    parallel,
                )?);
            }
        }
        Ok(resolved)
    }

    fn verify(
        &self,
        paths: &Paths,
        target: &str,
        filter_flag: Option<&str>,
        force: bool,
        parallel: usize,
        translations: &Translations,
    ) -> Result<Verifications> {
        let batteries = resolve_batteries(&paths.corpus_dir, target)?;
        if batteries.len() > 1 {
            verify::run_all(paths, &batteries, force, parallel, translations)
        } else {
            let mut resolved = Verifications::new();
            for bat in &batteries {
                let (name, filter) = parse_target(bat);
                let filter = one_filter(filter, filter_flag)?;
                resolved.extend(verify::run(
                    paths,
                    &name,
                    filter.as_deref(),
                    force,
                    parallel,
                    translations,
                )?);
            }
            Ok(resolved)
        }
    }

    fn test(&self, paths: &Paths, target: &str, scoring: &Scoring<'_>) -> Result<TestOutcome> {
        oracle::run_test_corpus(paths, target, scoring)
    }

    fn enrich(&self, paths: &Paths, target: &str) -> Result<()> {
        for bat in resolve_batteries(&paths.corpus_dir, target)? {
            oracle::enrich_test_corpus(paths, &bat)?;
        }
        Ok(())
    }
}

struct HarvestBench;

impl Benchmark for HarvestBench {
    fn name(&self) -> &'static str {
        "harvest-bench"
    }

    fn verifies(&self, agent: Agent) -> bool {
        has_verify_phase(agent)
    }

    fn translate(
        &self,
        paths: &Paths,
        target: &str,
        filter_flag: Option<&str>,
        parallel: usize,
    ) -> Result<Translations> {
        no_filter_here(filter_flag)?;
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        translate::run_harvest_bench(paths, &projects, parallel)
    }

    fn verify(
        &self,
        paths: &Paths,
        target: &str,
        filter_flag: Option<&str>,
        force: bool,
        parallel: usize,
        translations: &Translations,
    ) -> Result<Verifications> {
        no_filter_here(filter_flag)?;
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        verify::run_harvest_bench(paths, &projects, parallel, force, translations)
    }

    fn test(&self, paths: &Paths, target: &str, scoring: &Scoring<'_>) -> Result<TestOutcome> {
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        oracle::run_harvest_bench_test(paths, &projects, scoring)
    }

    fn enrich(&self, paths: &Paths, _target: &str) -> Result<()> {
        // HB results sit per-project directly under results/HarvestBench/<agent>/ with no
        // battery grouping, which is the shape enrich_test_corpus sees for an empty battery.
        oracle::enrich_test_corpus(paths, "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--include-regex` was accepted, documented, threaded into this trait and then discarded by
    /// every impl as `_filter`, so asking for one case ran the whole battery. Nothing tested the
    /// flag, which is why it survived — so these pin the mapping itself rather than any one caller.
    #[test]
    fn a_case_named_in_the_target_becomes_the_filter() {
        assert_eq!(
            parse_target("B01_synthetic/001_helloworld"),
            ("B01_synthetic".into(), Some("001_helloworld$".into())),
            "anchored at the end, so 001_helloworld does not also select 001_helloworld_lib"
        );
        assert_eq!(
            parse_target("B01_synthetic"),
            ("B01_synthetic".into(), None)
        );
    }

    #[test]
    fn the_include_regex_flag_reaches_the_case_list() {
        assert_eq!(
            one_filter(None, Some("01[0-9]_")).unwrap(),
            Some("01[0-9]_".to_string()),
            "a flag with no case in the target must be the filter, not be dropped"
        );
    }

    #[test]
    fn a_filter_named_twice_is_refused_rather_than_half_applied() {
        let err = one_filter(Some("001_helloworld$".into()), Some("01[0-9]_"))
            .expect_err("two filters for one list cannot both apply");
        let text = format!("{err:#}");
        assert!(
            text.contains("001_helloworld$") && text.contains("01[0-9]_"),
            "and the refusal must name both, or the operator cannot tell which to drop: {text}"
        );
    }

    #[test]
    fn a_filter_harvest_bench_cannot_honour_is_refused_rather_than_ignored() {
        assert!(no_filter_here(None).is_ok(), "no flag is not an error");
        let err = no_filter_here(Some("libsodium")).expect_err(
            "harvest-bench filters nothing below a project, so accepting this would be the \
             silent-ignore bug again",
        );
        assert!(format!("{err:#}").contains("libsodium"));
    }
}
