//! One lifecycle, parameterized by dataset.
//!
//! Every benchmark moves through the same logical lifecycle:
//!
//! ```text
//!   translate → [verify?] → enrich → score(test)
//! ```
//!
//! The lifecycle is written ONCE (in `main.rs`, against this trait). Datasets
//! differ only in HOW each phase is carried out — discovery, the translate
//! invocation, whether a separate C-as-oracle verify phase runs, and how the
//! result is scored. This trait replaces what used to be four parallel
//! per-dataset match ladders (`make_translate_plan` / `make_verify_plan` /
//! `make_test_plan` / the `execute_*` arms), three plan enums, and the
//! nine-clause verify-skip `if` that lived inline in `Run`.
//!
//! Each `impl` here is intentionally thin: it DELEGATES to the existing
//! dataset functions in `translate` / `verify` / `test` rather than
//! reimplementing them, so this refactor changes structure, not behavior.

use crate::battery::{self, Paths};
use crate::cli::{Agent, Dataset};
use crate::test::{self, TestMode, TestOutcome};
use crate::{translate, verify};
use anyhow::Result;
use std::path::Path;

/// A benchmark dataset's participation in the shared lifecycle.
pub trait Benchmark {
    /// Human-readable label, for diagnostics. Part of the trait's public
    /// surface so callers can identify a `Box<dyn Benchmark>`; not all call
    /// sites use it today.
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    /// Does a separate C-as-oracle verify phase run for this agent?
    ///
    /// Replaces the nine-clause skip `if`. Datasets whose verification is
    /// folded into the agentic translate prompt (CRUST, harvest-bench) return
    /// `false`; ablation/combined agents that self-verify or skip verify by
    /// design return `false` too (see [`agent_runs_separate_verify`]).
    fn verifies(&self, agent: Agent) -> bool;

    /// Translate the target. All discovery + parallelism is internal.
    fn translate(&self, paths: &Paths, target: &str, filter: Option<&str>,
                 parallel: usize, limit: Option<usize>) -> Result<()>;

    /// Run the verify phase. Only reached from `Run` when [`verifies`] is true;
    /// also invoked directly by the `verify` subcommand. Datasets with no
    /// separate verify phase inherit the no-op default.
    fn verify(&self, _repo_root: &Path, _paths: &Paths, _target: &str,
              _filter: Option<&str>, _force: bool, _parallel: usize) -> Result<()> {
        Ok(())
    }

    /// Score the translated crate(s) against ground-truth tests.
    fn test(&self, paths: &Paths, target: &str, mode: TestMode) -> Result<TestOutcome>;

    /// Backfill result.json enrichment (unsafe/loc/credits). This is folded
    /// into `test --update`; the `enrich` subcommand re-runs just this step.
    fn enrich(&self, paths: &Paths, target: &str) -> Result<()>;
}

/// Construct the benchmark for a dataset. The single dispatch point.
pub fn for_dataset(d: Dataset) -> Box<dyn Benchmark> {
    match d {
        Dataset::TestCorpus => Box::new(TestCorpus),
        Dataset::Crust => Box::new(Crust),
        Dataset::BlindCrust => Box::new(BlindCrust),
        Dataset::HarvestBench => Box::new(HarvestBench),
    }
}

/// The verify-skip predicate shared by TestCorpus and BlindCrust. These agents
/// either merge translate+verify into one session (ClaudeCombined), skip verify
/// by prompt-ablation design (the other Claude* variants), or run their own
/// translate-then-verify pipeline in-harness (Codex). For all of them the
/// separate ACTOR verify phase does not run.
fn agent_runs_separate_verify(agent: Agent) -> bool {
    !matches!(agent,
        Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter
        | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt
        | Agent::CodexGpt55 | Agent::CodexGpt54)
}

// ── Shared discovery helpers (moved from main.rs) ──────────────────────

fn resolve_batteries(corpus_dir: &Path, target: &str) -> Result<Vec<String>> {
    if target == "all" {
        battery::all_batteries(corpus_dir)
    } else {
        Ok(vec![target.to_string()])
    }
}

fn resolve_crust_projects(corpus_dir: &Path, target: &str, limit: Option<usize>)
    -> Result<Vec<battery::CrustProject>> {
    if target.eq_ignore_ascii_case("crust") || target == "all" {
        battery::CrustProject::discover(corpus_dir, limit)
    } else {
        Ok(vec![battery::CrustProject::validated(corpus_dir, target)?])
    }
}

fn resolve_harvest_bench_projects(corpus_dir: &Path, target: &str)
    -> Result<Vec<battery::HarvestBenchProject>> {
    if target.eq_ignore_ascii_case("hb") || target == "all" {
        battery::HarvestBenchProject::discover(corpus_dir)
    } else {
        Ok(vec![battery::HarvestBenchProject::resolve(corpus_dir, target)?])
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

// ── TestCorpus ─────────────────────────────────────────────────────────

struct TestCorpus;

impl Benchmark for TestCorpus {
    fn name(&self) -> &'static str { "test-corpus" }

    fn verifies(&self, agent: Agent) -> bool { agent_runs_separate_verify(agent) }

    fn translate(&self, paths: &Paths, target: &str, _filter: Option<&str>,
                 parallel: usize, _limit: Option<usize>) -> Result<()> {
        let batteries = resolve_batteries(&paths.corpus_dir, target)?;

        // Shared-source batteries must run single-threaded (the follower configs
        // are propagated from one real translation); independent batteries can
        // share the remaining parallel budget. This partitioning is unchanged
        // from the previous `execute_translate` TestCorpus arm.
        if batteries.len() > 1 && parallel > 1 {
            let (shared_bats, indie_bats): (Vec<&str>, Vec<&str>) = batteries.iter()
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
                            translate::run_test_corpus(paths, &name, filter.as_deref(), indie_parallel)?;
                        }
                        Ok(())
                    }));
                }
                handles.into_iter().filter_map(|h| match h.join() {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e),
                    Err(_) => Some(anyhow::anyhow!("translate thread panicked")),
                }).collect()
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

    fn verify(&self, repo_root: &Path, paths: &Paths, target: &str,
              _filter: Option<&str>, force: bool, parallel: usize) -> Result<()> {
        let batteries = resolve_batteries(&paths.corpus_dir, target)?;
        if batteries.len() > 1 {
            verify::run_all(repo_root, paths, &batteries, force, parallel)
        } else {
            for bat in &batteries {
                let (name, filter) = parse_target(bat);
                verify::run(repo_root, paths, &name, filter.as_deref(), force, parallel)?;
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

// ── CRUST ──────────────────────────────────────────────────────────────

struct Crust;

impl Benchmark for Crust {
    fn name(&self) -> &'static str { "CRUST" }

    // Verification is folded into the CRUST translate prompt (it iterates
    // `cargo test` to green in one session), so no separate verify phase runs.
    fn verifies(&self, _agent: Agent) -> bool { false }

    fn translate(&self, paths: &Paths, target: &str, _filter: Option<&str>,
                 parallel: usize, limit: Option<usize>) -> Result<()> {
        let projects = resolve_crust_projects(&paths.corpus_dir, target, limit)?;
        translate::run_crust(paths, &projects, parallel)
    }

    fn test(&self, paths: &Paths, target: &str, mode: TestMode) -> Result<TestOutcome> {
        let projects = resolve_crust_projects(&paths.corpus_dir, target, None)?;
        test::run_crust_test(paths, &projects, mode)
    }

    fn enrich(&self, paths: &Paths, target: &str) -> Result<()> {
        let projects = resolve_crust_projects(&paths.corpus_dir, target, None)?;
        test::enrich_crust(paths, &projects)
    }
}

// ── CRUST blind ────────────────────────────────────────────────────────

struct BlindCrust;

impl Benchmark for BlindCrust {
    fn name(&self) -> &'static str { "CRUST-blind" }

    fn verifies(&self, agent: Agent) -> bool { agent_runs_separate_verify(agent) }

    fn translate(&self, paths: &Paths, target: &str, _filter: Option<&str>,
                 parallel: usize, limit: Option<usize>) -> Result<()> {
        let projects = resolve_crust_projects(&paths.corpus_dir, target, limit)?;
        translate::run_crust_blind(paths, &projects, parallel)
    }

    fn verify(&self, _repo_root: &Path, paths: &Paths, target: &str,
              _filter: Option<&str>, force: bool, parallel: usize) -> Result<()> {
        let projects = resolve_crust_projects(&paths.corpus_dir, target, None)?;
        translate::verify_crust_blind(paths, &projects, parallel, force)
    }

    fn test(&self, paths: &Paths, target: &str, mode: TestMode) -> Result<TestOutcome> {
        let projects = resolve_crust_projects(&paths.corpus_dir, target, None)?;
        test::run_blind_crust_test(paths, &projects, mode)
    }

    fn enrich(&self, paths: &Paths, target: &str) -> Result<()> {
        let projects = resolve_crust_projects(&paths.corpus_dir, target, None)?;
        test::enrich_blind_crust(paths, &projects)
    }
}

// ── harvest-bench ──────────────────────────────────────────────────────

struct HarvestBench;

impl Benchmark for HarvestBench {
    fn name(&self) -> &'static str { "harvest-bench" }

    // Full parity with Test-Corpus: HB gets a real C-as-oracle verify phase
    // for verifying agents (kiro/claude/codex-gpt5*), using the SAME shared
    // prompts/claude/verify.md — subagent protocol + Phase A/B/C/D differential
    // testing (SYMBOLS.md + ERRORS.md gated). Ablation agents (Combined/
    // Minimal/NoIter/…) skip verify by design, same as elsewhere.
    fn verifies(&self, agent: Agent) -> bool { agent_runs_separate_verify(agent) }

    fn translate(&self, paths: &Paths, target: &str, _filter: Option<&str>,
                 parallel: usize, _limit: Option<usize>) -> Result<()> {
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        translate::run_harvest_bench(paths, &projects, parallel)
    }

    fn verify(&self, _repo_root: &Path, paths: &Paths, target: &str,
              _filter: Option<&str>, force: bool, parallel: usize) -> Result<()> {
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        verify::run_harvest_bench(paths, &projects, parallel, force)
    }

    fn test(&self, paths: &Paths, target: &str, mode: TestMode) -> Result<TestOutcome> {
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        test::run_harvest_bench_test(paths, &projects, mode)
    }

    fn enrich(&self, paths: &Paths, _target: &str) -> Result<()> {
        // harvest-bench results are per-project directly under
        // results/HarvestBench/<agent>/ (no battery grouping) — the same
        // per-case shape enrich_test_corpus expects with an empty battery.
        test::enrich_test_corpus(paths, "")
    }
}
