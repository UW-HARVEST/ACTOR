//! One lifecycle — translate → [verify?] → enrich → score(test) — written once in
//! `main.rs` against this trait; datasets differ only in how each phase is carried out.
//! Each `impl` must stay thin, delegating to `translate` / `verify` / `test` rather than
//! reimplementing them.

use crate::battery::{self, Paths};
use crate::cli::{Agent, Dataset};
use crate::test::{self, TestMode, TestOutcome};
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

    fn translate(
        &self,
        paths: &Paths,
        target: &str,
        filter: Option<&str>,
        parallel: usize,
    ) -> Result<()>;

    /// Reached from `Run` only when [`verifies`] is true, but also invoked directly by
    /// the `verify` subcommand, so an impl cannot assume that gate ran.
    fn verify(
        &self,
        _repo_root: &Path,
        _paths: &Paths,
        _target: &str,
        _filter: Option<&str>,
        _force: bool,
        _parallel: usize,
    ) -> Result<()> {
        Ok(())
    }

    fn test(&self, paths: &Paths, target: &str, mode: TestMode) -> Result<TestOutcome>;

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

/// ClaudeCombined merges translate+verify into one session, the other Claude* variants
/// are prompt ablations that skip verify by design, and Codex runs its own
/// translate-then-verify pipeline in-harness — none get the separate ACTOR verify phase.
fn agent_runs_separate_verify(agent: Agent) -> bool {
    !matches!(
        agent,
        Agent::ClaudeCombined
            | Agent::ClaudeMinimal
            | Agent::ClaudeNoIter
            | Agent::ClaudeNoFeatures
            | Agent::ClaudeNoSubtask
            | Agent::ClaudeCrossPrompt
            | Agent::CodexGpt55
            | Agent::CodexGpt54
    )
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
        agent_runs_separate_verify(agent)
    }

    fn translate(
        &self,
        paths: &Paths,
        target: &str,
        _filter: Option<&str>,
        parallel: usize,
    ) -> Result<()> {
        let batteries = resolve_batteries(&paths.corpus_dir, target)?;

        // Shared-source batteries must run single-threaded: their follower configs are
        // propagated from one real translation. Independent batteries split the rest of
        // the parallel budget.
        if batteries.len() > 1 && parallel > 1 {
            let (shared_bats, indie_bats): (Vec<&str>, Vec<&str>) = batteries
                .iter()
                .map(String::as_str)
                .partition(|b| battery::has_shared_source_groups(&paths.corpus_dir, b));

            let indie_parallel = parallel.saturating_sub(shared_bats.len()).max(1);

            let errors: Vec<anyhow::Error> = std::thread::scope(|s| {
                let mut handles = Vec::new();
                for bat in &shared_bats {
                    handles.push(s.spawn(move || -> Result<()> {
                        let (name, filter) = parse_target(bat);
                        translate::run_test_corpus(paths, &name, filter.as_deref(), 1)
                    }));
                }
                if !indie_bats.is_empty() {
                    handles.push(s.spawn(|| -> Result<()> {
                        for bat in &indie_bats {
                            let (name, filter) = parse_target(bat);
                            translate::run_test_corpus(
                                paths,
                                &name,
                                filter.as_deref(),
                                indie_parallel,
                            )?;
                        }
                        Ok(())
                    }));
                }
                handles
                    .into_iter()
                    .filter_map(|h| match h.join() {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(e),
                        Err(_) => Some(anyhow::anyhow!("translate thread panicked")),
                    })
                    .collect()
            });
            if let Some(first) = errors.into_iter().next() {
                return Err(first);
            }
        } else {
            for bat in &batteries {
                let (name, filter) = parse_target(bat);
                translate::run_test_corpus(paths, &name, filter.as_deref(), parallel)?;
            }
        }
        Ok(())
    }

    // `_repo_root` is unused in every impl: the real repo root travels on `Paths` (see
    // crate::sandbox), so the trait parameter could be dropped as a follow-up.
    fn verify(
        &self,
        _repo_root: &Path,
        paths: &Paths,
        target: &str,
        _filter: Option<&str>,
        force: bool,
        parallel: usize,
    ) -> Result<()> {
        let batteries = resolve_batteries(&paths.corpus_dir, target)?;
        if batteries.len() > 1 {
            verify::run_all(paths, &batteries, force, parallel)
        } else {
            for bat in &batteries {
                let (name, filter) = parse_target(bat);
                verify::run(paths, &name, filter.as_deref(), force, parallel)?;
            }
            Ok(())
        }
    }

    fn test(&self, paths: &Paths, target: &str, mode: TestMode) -> Result<TestOutcome> {
        let batteries = resolve_batteries(&paths.corpus_dir, target)?;
        let mut all_mismatches = Vec::new();
        for bat in &batteries {
            if let TestOutcome::Failed(m) = test::run_test_corpus(paths, bat, mode)? {
                all_mismatches.extend(m);
            }
        }
        Ok(if all_mismatches.is_empty() {
            TestOutcome::Passed
        } else {
            TestOutcome::Failed(all_mismatches)
        })
    }

    fn enrich(&self, paths: &Paths, target: &str) -> Result<()> {
        for bat in resolve_batteries(&paths.corpus_dir, target)? {
            test::enrich_test_corpus(paths, &bat)?;
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
        agent_runs_separate_verify(agent)
    }

    fn translate(
        &self,
        paths: &Paths,
        target: &str,
        _filter: Option<&str>,
        parallel: usize,
    ) -> Result<()> {
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        translate::run_harvest_bench(paths, &projects, parallel)
    }

    fn verify(
        &self,
        _repo_root: &Path,
        paths: &Paths,
        target: &str,
        _filter: Option<&str>,
        force: bool,
        parallel: usize,
    ) -> Result<()> {
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        verify::run_harvest_bench(paths, &projects, parallel, force)
    }

    fn test(&self, paths: &Paths, target: &str, mode: TestMode) -> Result<TestOutcome> {
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        test::run_harvest_bench_test(paths, &projects, mode)
    }

    fn enrich(&self, paths: &Paths, _target: &str) -> Result<()> {
        // HB results sit per-project directly under results/HarvestBench/<agent>/ with no
        // battery grouping, which is the shape enrich_test_corpus sees for an empty battery.
        test::enrich_test_corpus(paths, "")
    }
}
