//! One lifecycle — translate → [verify?] → enrich → score(test) — written once in
//! `main.rs` against this trait; datasets differ only in how each phase is carried out.
//! Each `impl` must stay thin, delegating to `translate` / `verify` / `oracle` rather than
//! reimplementing them.

use crate::agents::invocation::has_verify_phase;
use crate::battery::{self, Paths};
use crate::cli::{Agent, Dataset};
use crate::oracle::{self, Scoring};
use crate::translate::Translations;
use crate::verify::Verifications;
use crate::{translate, verify};
use anyhow::Result;
use std::path::Path;

/// Every unit a run may publish, derived ONCE from what the store served. No `Clone`, private field, and
/// [`Self::derive`] the only constructor -- so a caller cannot hand a phase one unit at a time, which is
/// how `run` starved the pool (`spec-27.md`).
pub struct InScope(Vec<String>);

impl InScope {
    /// The derivation itself: a unit is publishable only if the store served every case of it
    /// ([`Benchmark::attests`]). Out-of-scope units are announced by name rather than dropped quietly.
    pub fn derive(
        bench: &dyn Benchmark,
        paths: &Paths,
        target: &str,
        resolved: &Translations,
    ) -> Result<(Self, crate::analyse::report::Attested)> {
        let mut attested = crate::analyse::report::Attested::default();
        let mut units = Vec::new();
        for unit in bench.batteries(paths, target)? {
            match bench.attests(paths, &unit, resolved) {
                Ok(()) => {
                    attested.insert(paths.agent_key.as_str(), &unit);
                    units.push(unit);
                }
                Err(why) => println!("\u{23ed}\u{fe0f}  {unit}: out of scope — {why}"),
            }
        }
        anyhow::ensure!(
            !units.is_empty(),
            "the store serves no unit of {target} in full, so this run can publish nothing"
        );
        Ok((Self(units), attested))
    }

    pub fn units(&self) -> &[String] {
        &self.0
    }
}

/// [`Paths`] a dataset's preflight has vouched for. It OWNS them rather than being a free-standing
/// token, so nothing is an unused parameter and a proof minted from Test-Corpus cannot be handed to a
/// harvest-bench phase. [`Benchmark::preflight`] is the only constructor, so no phase is reachable
/// without one -- CI found two missing inputs the other way, 19 minutes into a leg each time.
pub struct Preflighted(Paths);

impl std::ops::Deref for Preflighted {
    type Target = Paths;
    fn deref(&self) -> &Paths {
        &self.0
    }
}

/// MIT `runtests` needs >= 3.10 and the default `python3` here is 3.9. Lived in `reproduce.sh`, so
/// `translate` and `test` invoked directly never had it.
fn require_python_310() -> Result<()> {
    let ok = std::process::Command::new("python3")
        .args([
            "-c",
            "import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)",
        ])
        .status()
        .map_err(|e| {
            anyhow::anyhow!("python3 is not runnable ({e}), and scoring goes through it")
        })?;
    anyhow::ensure!(
        ok.success(),
        "python3 is older than 3.10, which MIT runtests needs"
    );
    Ok(())
}

pub trait Benchmark {
    /// Everything a phase needs HOURS from now: binaries, interpreters, corpus dirs. Required, not
    /// defaulted, so a dataset added later cannot quietly have none.
    fn preflight(&self, paths: Paths) -> Result<Preflighted>;

    #[allow(
        dead_code,
        reason = "part of the trait's public surface so a caller holding a \
                  Box<dyn Benchmark> can identify it; not every call site needs it yet"
    )]
    fn name(&self) -> &'static str;

    fn verifies(&self, agent: Agent) -> bool;

    fn batteries(&self, paths: &Paths, target: &str) -> Result<Vec<String>>;

    /// May this unit's number be PUBLISHED? Only if EVERY case came from a keyed replay: one case the
    /// store cannot name leaves the number resting on an artifact nothing attests, and counting the
    /// attested subset would be a smaller denominator nobody asked for.
    fn attests(&self, paths: &Paths, battery: &str, resolved: &Translations) -> Result<()>;

    /// Returns what it RESOLVED, per case: a value this run produced, not a phase dir to be asked.
    fn translate(
        &self,
        paths: &Preflighted,
        target: &str,
        filter: Option<&str>,
        pool: &crate::agents::Pool,
    ) -> Result<Translations>;

    /// Reached from `Run` only when [`Self::verifies`] is true, but also invoked directly by
    /// the `verify` subcommand, so an impl cannot assume that gate ran.
    ///
    /// `translations` is the hand-off: a case absent from it is refused, not seeded from disk.
    /// EVERY unit, not one at a time: both impls schedule under one pool, so a caller that loops leaves
    /// it nothing to overlap.
    fn verify(
        &self,
        _paths: &Preflighted,
        _scope: &InScope,
        _filter: Option<&str>,
        _force: bool,
        _pool: &crate::agents::Pool,
        _translations: &Translations,
    ) -> Result<Verifications> {
        Ok(Verifications::new())
    }

    fn test(&self, paths: &Preflighted, target: &str, scoring: &Scoring<'_>) -> Result<()>;

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
    fn preflight(&self, paths: Paths) -> Result<Preflighted> {
        anyhow::ensure!(
            paths.corpus_dir.is_dir(),
            "the test-corpus submodule is absent at {} -- run: git submodule update --init test-corpus",
            paths.corpus_dir.display()
        );
        require_python_310()?;
        Ok(Preflighted(paths))
    }

    fn batteries(&self, paths: &Paths, target: &str) -> Result<Vec<String>> {
        resolve_batteries(&paths.corpus_dir, target)
    }

    fn attests(&self, paths: &Paths, battery: &str, resolved: &Translations) -> Result<()> {
        let output_dir = paths.output_dir(battery);
        let cases = battery::all_case_names(&battery::discover(&paths.corpus_dir, battery, None)?);
        let missing = cases
            .iter()
            .filter(|n| !resolved.contains_key(&output_dir.join(n)))
            .count();
        let unkeyed = crate::translate::unkeyed_seeds(resolved, &output_dir);
        anyhow::ensure!(
            missing == 0 && unkeyed == 0,
            "the store serves {} of its {} case(s) ({missing} unresolved, {unkeyed} with no key)",
            cases.len() - missing - unkeyed,
            cases.len()
        );
        Ok(())
    }
    fn name(&self) -> &'static str {
        "test-corpus"
    }

    fn verifies(&self, agent: Agent) -> bool {
        has_verify_phase(agent)
    }

    fn translate(
        &self,
        paths: &Preflighted,
        target: &str,
        filter_flag: Option<&str>,
        pool: &crate::agents::Pool,
    ) -> Result<Translations> {
        let mut resolved = Translations::new();
        // One pool for the whole sweep, so there is no budget left to hand-split. The arithmetic here
        // used to partition `parallel` between shared-source batteries (pinned to 1) and the rest --
        // necessary only because each call minted its own pool. A group's followers are still derived
        // sequentially, inside `translate_one_shared`, which is where that constraint actually lives.
        let mut errors: Vec<anyhow::Error> = Vec::new();
        std::thread::scope(|s| {
            let handles: Vec<_> = resolve_batteries(&paths.corpus_dir, target)
                .into_iter()
                .flatten()
                .map(|bat| {
                    s.spawn(move || -> Result<Translations> {
                        let (name, filter) = parse_target(&bat);
                        let filter = one_filter(filter, filter_flag)?;
                        translate::run_test_corpus(paths, &name, filter.as_deref(), pool)
                    })
                })
                .collect();
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
        Ok(resolved)
    }

    fn verify(
        &self,
        paths: &Preflighted,
        scope: &InScope,
        filter_flag: Option<&str>,
        force: bool,
        pool: &crate::agents::Pool,
        translations: &Translations,
    ) -> Result<Verifications> {
        // One battery goes through `run_all` too: a second spelling of "verify these" is what drifted.
        let units = scope.units();
        let filter = one_filter(units.iter().find_map(|u| parse_target(u).1), filter_flag)?;
        let names: Vec<String> = units.iter().map(|u| parse_target(u).0).collect();
        verify::run_all(paths, &names, filter.as_deref(), force, pool, translations)
    }

    fn test(&self, paths: &Preflighted, target: &str, scoring: &Scoring<'_>) -> Result<()> {
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
    fn preflight(&self, paths: Paths) -> Result<Preflighted> {
        anyhow::ensure!(
            paths.corpus_dir.is_dir(),
            "harvest-bench/tests is absent at {} -- run: git submodule update --init harvest-bench",
            paths.corpus_dir.display()
        );
        // The same function scoring calls, not a second copy of the path.
        crate::oracle::gtest::harvest_bench_runner(&paths.corpus_dir)?;
        Ok(Preflighted(paths))
    }

    fn batteries(&self, paths: &Paths, target: &str) -> Result<Vec<String>> {
        Ok(resolve_harvest_bench_projects(&paths.corpus_dir, target)?
            .iter()
            .map(|p| p.name().to_string())
            .collect())
    }

    fn attests(&self, paths: &Paths, project: &str, resolved: &Translations) -> Result<()> {
        let dir = paths.results_dir.join(project);
        anyhow::ensure!(
            resolved.contains_key(&dir) && crate::translate::unkeyed_seeds(resolved, &dir) == 0,
            "the store does not serve {project} with a key"
        );
        Ok(())
    }
    fn name(&self) -> &'static str {
        "harvest-bench"
    }

    fn verifies(&self, agent: Agent) -> bool {
        has_verify_phase(agent)
    }

    fn translate(
        &self,
        paths: &Preflighted,
        target: &str,
        filter_flag: Option<&str>,
        pool: &crate::agents::Pool,
    ) -> Result<Translations> {
        no_filter_here(filter_flag)?;
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        translate::run_harvest_bench(paths, &projects, pool)
    }

    fn verify(
        &self,
        paths: &Preflighted,
        scope: &InScope,
        filter_flag: Option<&str>,
        force: bool,
        pool: &crate::agents::Pool,
        translations: &Translations,
    ) -> Result<Verifications> {
        no_filter_here(filter_flag)?;
        let projects: Vec<battery::HarvestBenchProject> =
            resolve_harvest_bench_projects(&paths.corpus_dir, "HB")?
                .into_iter()
                .filter(|p| scope.units().iter().any(|u| u == p.name()))
                .collect();
        verify::run_harvest_bench(paths, &projects, pool, force, translations)
    }

    fn test(&self, paths: &Preflighted, target: &str, scoring: &Scoring<'_>) -> Result<()> {
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

    /// Pinning the mapping itself, not any one caller: nothing tested `--include-regex`, which is how
    /// every impl came to discard it as `_filter`.
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
