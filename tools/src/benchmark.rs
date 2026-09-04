//! What differs BETWEEN datasets, and nothing else.
//!
//! Running the chain is `crate::chain`'s job and identical for both; a dataset differs only in what
//! it must preflight, how it enumerates its units, when a unit may be published, and how it is
//! scored. `translate` and `verify` are gone from this trait: they were the same function at two
//! prompts, so a dataset had no business having one method per step.

use crate::battery::{self, Paths};
use crate::chain::{self, Follower, Job};
use crate::cli::Dataset;
use crate::eval::Resolved;
use crate::oracle::{self, Scoring};
use crate::prompt::Shape;
use crate::transform::Artifact;
use anyhow::{Context, Result};
use std::path::Path;

/// Every unit a run may publish, derived ONCE from what the store served. No `Clone`, private field, and
/// [`Self::derive`] the only constructor -- so a caller cannot hand a phase one unit at a time, which is
/// how `run` starved the pool (`spec-27.md`).
pub struct InScope(Vec<String>);

impl InScope {
    /// The derivation itself: a unit is publishable only if the store served every case of it
    /// (`attests`). Out-of-scope units are announced by name rather than dropped quietly.
    ///
    /// Takes the jobs the CHAIN was handed, not a target to re-derive them from, so "every case"
    /// means every case the run covers.
    pub fn derive(
        paths: &Paths,
        units: &[(String, Vec<Job>)],
        resolved: &Resolved,
    ) -> Result<(Self, crate::analyse::report::Attested)> {
        let mut attested = crate::analyse::report::Attested::default();
        let mut publishable = Vec::new();
        for (unit, jobs) in units {
            match attests(jobs, resolved) {
                Ok(()) => {
                    attested.insert(paths, unit);
                    publishable.push(unit.clone());
                }
                Err(why) => println!("\u{23ed}\u{fe0f}  {unit}: out of scope — {why}"),
            }
        }
        anyhow::ensure!(
            !publishable.is_empty(),
            "the store serves no unit of {} in full, so this run can publish nothing",
            units
                .iter()
                .map(|(u, _)| u.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok((Self(publishable), attested))
    }

    pub fn units(&self) -> &[String] {
        &self.0
    }
}

/// May this unit's number be PUBLISHED? Only if EVERY case came from a keyed replay: one case the
/// store cannot name leaves the number resting on an artifact nothing attests, and the attested
/// subset would be a smaller denominator nobody asked for. ONE definition for both datasets, whose
/// two impls stated this over differently derived lists and could not see an EMPTY list pass.
fn attests(jobs: &[Job], resolved: &Resolved) -> Result<()> {
    let dirs = chain::case_dirs(jobs);
    anyhow::ensure!(
        !dirs.is_empty(),
        "it has no case at all, so there is nothing for the store to serve"
    );
    let missing = dirs
        .iter()
        .filter(|d| !resolved.keys().any(|k| k.starts_with(d)))
        .count();
    anyhow::ensure!(
        missing == 0,
        "the store serves {} of its {} case(s), {missing} unresolved",
        dirs.len() - missing,
        dirs.len()
    );
    Ok(())
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

/// What CMake version the harvest-bench gtest suites need.
///
/// A suite that fails to configure reports an empty verdict list, which printed `Builds: no` against
/// seven crates that had compiled. It lived in a shell script's PATH line -- nowhere `run HB` saw.
const CMAKE_MIN: (u32, u32) = (3, 24);

/// The edge: run the program. [`accept_cmake`] is the judgement, and pure.
fn require_cmake(min: (u32, u32)) -> Result<()> {
    let out = std::process::Command::new("cmake")
        .arg("--version")
        .output()
        .map_err(|e| {
            anyhow::anyhow!(
                "cmake is not runnable ({e}), and the gtest suites are configured with it"
            )
        })?;
    accept_cmake(&String::from_utf8_lossy(&out.stdout), min)
}

fn accept_cmake(version_text: &str, min: (u32, u32)) -> Result<()> {
    let found = cmake_version(version_text).with_context(|| {
        format!("`cmake --version` printed no version this can read: {version_text:?}")
    })?;
    anyhow::ensure!(
        found >= min,
        "cmake {}.{} is too old to configure the harvest-bench gtest suites, which need {}.{}. \
         Every project would report a failed build for a crate that compiled. Put a newer cmake \
         first on PATH.",
        found.0,
        found.1,
        min.0,
        min.1
    );
    Ok(())
}

fn cmake_version(text: &str) -> Option<(u32, u32)> {
    let digits = text
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = digits.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next().unwrap_or("0").parse().unwrap_or(0),
    ))
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
    fn batteries(&self, paths: &Paths, target: &str) -> Result<Vec<String>>;

    /// Every case of one unit, carrying the layout facts the chain needs.
    ///
    /// THE dataset difference, and the one the rewrite left out: a Test-Corpus case lives at
    /// `Public-Tests/<battery>/<case>` with its vectors beside the C, while a harvest-bench project
    /// lives at `tests/<name>` and IS its own unit. The chain called `battery::discover` directly, so
    /// it asked for `tests/Public-Tests/<project>` and every HB run died before its first invocation.
    fn jobs(&self, paths: &Paths, unit: &str, filter: Option<&str>) -> Result<Vec<Job>>;

    fn test(&self, paths: &Preflighted, target: &str, scoring: &Scoring<'_>) -> Result<()>;
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
pub(crate) fn one_filter(
    from_target: Option<String>,
    from_flag: Option<&str>,
) -> Result<Option<String>> {
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
pub(crate) fn no_filter_here(from_flag: Option<&str>) -> Result<()> {
    if let Some(f) = from_flag {
        anyhow::bail!(
            "--include-regex ({f}) does not apply to harvest-bench: its unit is a project, and \
             nothing under it filters by case. Name the project in the target instead"
        );
    }
    Ok(())
}

/// Split a `battery` or `battery/case` target into (battery, optional case regex).
pub(crate) fn parse_target(target: &str) -> (String, Option<String>) {
    if let Some((battery, case)) = target.split_once('/') {
        (battery.to_string(), Some(format!("{}$", case)))
    } else {
        (target.to_string(), None)
    }
}

/// What one run covers: which units to run, and which of each unit's cases. Resolved ONCE and handed to
/// the chain, the publishability check and the scorer alike. Derived twice, a single-case run translated
/// and verified correctly and then scored a battery literally named `B01_organic/bin2hex_lib`, found
/// nothing, and refused -- after paying for both agents. One regex, because `battery::discover` applies
/// one, so a case in the target AND the flag is refused.
pub struct Scope {
    units: Vec<String>,
    filter: Option<String>,
}

impl Scope {
    pub fn resolve(
        bench: &dyn Benchmark,
        paths: &Paths,
        target: &str,
        include_regex: Option<&str>,
    ) -> Result<Self> {
        let (units, filter) = match paths.dataset {
            Dataset::TestCorpus => {
                let (unit, from_target) = parse_target(target);
                let filter = one_filter(from_target, include_regex)?;
                (bench.batteries(paths, &unit)?, filter)
            }
            // Harvest-bench's unit is a project, and nothing below it filters by case.
            Dataset::HarvestBench => {
                no_filter_here(include_regex)?;
                (bench.batteries(paths, target)?, None)
            }
        };
        Ok(Self { units, filter })
    }

    pub fn units(&self) -> &[String] {
        &self.units
    }

    pub fn filter(&self) -> Option<&str> {
        self.filter.as_deref()
    }

    /// Which of a unit's cases the score may claim. From this one value rather than the flag, so a case
    /// named in the TARGET narrows the roster exactly as `--include-regex` does.
    pub fn covers(&self) -> oracle::Covers<'_> {
        oracle::Covers::of(self.filter())
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

    fn jobs(&self, paths: &Paths, battery: &str, filter: Option<&str>) -> Result<Vec<Job>> {
        let input_dir = paths.input_dir(battery);
        battery::discover(&paths.corpus_dir, battery, filter)
            .with_context(|| format!("discovering the cases of {battery}"))?
            .cases
            .iter()
            .map(|case| test_corpus_job(paths, battery, &input_dir, case))
            .collect()
    }

    fn name(&self) -> &'static str {
        "test-corpus"
    }
    fn test(&self, paths: &Preflighted, target: &str, scoring: &Scoring<'_>) -> Result<()> {
        oracle::run_test_corpus(paths, target, scoring)
    }
}

/// One Test-Corpus case: name, prompt shape, ARTIFACT shape, followers. Was `chain::describe`, where
/// it made the shared driver Test-Corpus-only. The two shapes stay separate: dropping the followers
/// is why such a battery published nothing, and collapsing the shapes is why it lost its `driver`.
fn test_corpus_job(
    paths: &Paths,
    battery: &str,
    input_dir: &Path,
    case: &battery::Case,
) -> Result<Job> {
    Ok(match case {
        battery::Case::Independent(c) => Job {
            name: c.name.clone(),
            corpus: crate::tree::Corpus::at(input_dir.join(&c.name).join(CORPUS_C))?,
            case_inputs: input_dir.join(&c.name),
            case_dir: paths.case_dir(battery, &c.name),
            shape: Shape::of(c.is_lib, false),
            artifact: if c.is_lib {
                // The case-dir name IS the right lib name where the corpus runner names no other:
                // `cando2`'s short-form `harness!` resolves `lib<case>.so`.
                Artifact::Cdylib {
                    lib_name: battery::extract_lib_name(input_dir, &c.name)
                        .unwrap_or_else(|| c.name.clone()),
                }
            } else {
                Artifact::Driver
            },
            followers: Vec::new(),
        },
        // One invocation for the real case; its followers are derived by a transform, not re-run --
        // and the real case is the TEMPLATE they are derived from, so it keeps both targets.
        battery::Case::SharedSource(g) => Job {
            name: g.real_case.clone(),
            corpus: crate::tree::Corpus::at(input_dir.join(&g.real_case).join(CORPUS_C))?,
            case_inputs: input_dir.join(&g.real_case),
            case_dir: paths.case_dir(battery, &g.real_case),
            shape: Shape::Shared,
            artifact: Artifact::Template {
                default_features: battery::extract_features_from_path(
                    &input_dir.join(&g.real_case).join("CMakePresets.json"),
                )?,
                needs_driver: !g.real_case.ends_with("_lib"),
            },
            followers: g
                .configs
                .iter()
                .map(|cfg| {
                    Ok(Follower {
                        corpus: crate::tree::Corpus::at(input_dir.join(&cfg.name).join(CORPUS_C))?,
                        case_inputs: input_dir.join(&cfg.name),
                        case_dir: paths.case_dir(battery, &cfg.name),
                        cfg: cfg.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        },
    })
}

/// Both datasets spell the C the same way, and this is the only place either says so.
const CORPUS_C: &str = "test_case";

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
        require_cmake(CMAKE_MIN)?;
        Ok(Preflighted(paths))
    }

    fn batteries(&self, paths: &Paths, target: &str) -> Result<Vec<String>> {
        Ok(resolve_harvest_bench_projects(&paths.corpus_dir, target)?
            .iter()
            .map(|p| p.name().to_string())
            .collect())
    }

    /// A project is ONE case and its own unit: no battery level above, no case level below. The
    /// library shape and the cdylib name are not choices -- the suite links `lib<project>.so` by ABI,
    /// which is what the pre-rewrite `translate_one_harvest_bench` did by hand.
    fn jobs(&self, paths: &Paths, project: &str, filter: Option<&str>) -> Result<Vec<Job>> {
        no_filter_here(filter)?;
        let p = battery::HarvestBenchProject::resolve(&paths.corpus_dir, project)?;
        Ok(vec![Job {
            name: p.name().to_string(),
            corpus: crate::tree::Corpus::at(p.test_case())?,
            case_inputs: paths.input_dir(project),
            case_dir: paths.output_dir(project),
            shape: Shape::Library,
            artifact: Artifact::Cdylib {
                lib_name: p.name().to_string(),
            },
            followers: Vec::new(),
        }])
    }

    fn name(&self) -> &'static str {
        "harvest-bench"
    }
    fn test(&self, paths: &Preflighted, target: &str, scoring: &Scoring<'_>) -> Result<()> {
        let projects = resolve_harvest_bench_projects(&paths.corpus_dir, target)?;
        oracle::run_harvest_bench_test(paths, &projects, scoring)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hb_corpus(root: &Path, project: &str) {
        let p = root.join("harvest-bench/tests").join(project);
        std::fs::create_dir_all(p.join("test_case/src")).unwrap();
        std::fs::write(p.join("test_case/src/lib.c"), "int f(void){return 1;}\n").unwrap();
        std::fs::create_dir_all(p.join("gtest_suite")).unwrap();
        std::fs::write(p.join("gtest_suite/CMakeLists.txt"), "# suite\n").unwrap();
    }

    fn paths_for(root: &Path, dataset: Dataset) -> Paths {
        Paths::new(
            root,
            crate::cli::Tool::Claude,
            crate::cli::Variant::Default,
            dataset,
            None,
            crate::store::Mode::ReadWrite,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap()
    }

    /// What made every harvest-bench run die at second zero for all three tools.
    #[test]
    fn a_harvest_bench_project_is_one_case_at_its_own_layout() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        hb_corpus(tmp.path(), "libsodium");
        let paths = paths_for(tmp.path(), Dataset::HarvestBench);

        let jobs = HarvestBench
            .jobs(&paths, "libsodium", None)
            .expect("a project in harvest-bench's own layout must be discoverable");

        assert_eq!(jobs.len(), 1, "a project IS one case");
        let job = &jobs[0];
        assert_eq!(job.name, "libsodium");
        assert!(
            job.case_inputs.join("test_case").is_dir(),
            "the C the chain assembles must be inside case_inputs: {}",
            job.case_inputs.display()
        );
        assert_eq!(
            job.case_dir,
            paths.output_dir("libsodium"),
            "and it must publish exactly where the harvest-bench scorer reads"
        );
        assert_eq!(
            job.case_dir.parent(),
            Some(paths.results_dir.as_path()),
            "no battery level above it and no case level below it"
        );
        assert!(matches!(job.shape, Shape::Library));
        assert!(
            matches!(&job.artifact, Artifact::Cdylib { lib_name } if lib_name == "libsodium"),
            "the suite links lib<project>.so by ABI, so the name comes from the project"
        );
        assert!(job.followers.is_empty());

        // Non-vacuous: Test-Corpus discovery really cannot see this corpus.
        let err = match TestCorpus.jobs(
            &paths_for(tmp.path(), Dataset::TestCorpus),
            "libsodium",
            None,
        ) {
            Ok(_) => panic!("the Test-Corpus layout holds no such battery"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("Public-Tests"),
            "and it fails for exactly the reason HB used to: {err:#}"
        );
    }

    /// A number may be published only if the store served every case the run covers -- and a run
    /// whose artifacts land where the scorer does not look has served none of them.
    #[test]
    fn a_unit_attests_only_when_every_case_dir_it_publishes_into_was_served() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        hb_corpus(tmp.path(), "lz4");
        let paths = paths_for(tmp.path(), Dataset::HarvestBench);
        let jobs = HarvestBench.jobs(&paths, "lz4", None).unwrap();
        let tree = || crate::tree::Tree::for_test(tmp.path()).unwrap();

        let empty = Resolved::new();
        assert!(
            attests(&jobs, &empty).is_err(),
            "a store that served nothing must not publish a number"
        );

        let mut served = Resolved::new();
        served.insert(
            jobs[0].case_dir.join(crate::prompt::Role::Translate.dir()),
            tree(),
        );
        attests(&jobs, &served).expect("the dir the chain publishes into must attest it");

        // The scorer's lookup is EXACT, not a prefix: `run_harvest_bench_test` asks
        // `resolved.get(output_dir(project)/<role>)` and counts a build failure when it misses.
        let asked_for = paths
            .output_dir("lz4")
            .join(crate::prompt::Role::Translate.dir());
        assert!(
            served.contains_key(&asked_for),
            "the scorer must find the very tree the chain published"
        );
        let mut nested = Resolved::new();
        nested.insert(paths.output_dir("lz4").join("lz4/translated"), tree());
        assert!(
            !nested.contains_key(&asked_for),
            "non-vacuous: an extra case level really is invisible to that lookup, even though \
             `attests`'s prefix test cannot tell"
        );

        let err = attests(&[], &served).expect_err("an empty unit attests nothing");
        assert!(format!("{err:#}").contains("no case at all"), "{err:#}");
    }

    /// See [`CMAKE_MIN`]: on a 3.22 box no suite configures, and `Builds: no` was published against
    /// seven crates that had compiled.
    #[test]
    fn a_cmake_too_old_for_the_gtest_suites_is_refused_before_the_money() {
        for text in [
            "cmake version 3.24.0",
            "cmake version 3.31.1",
            "cmake 4.0.1",
        ] {
            accept_cmake(text, CMAKE_MIN).unwrap_or_else(|e| panic!("{text} must pass: {e:#}"));
        }
        let err = accept_cmake("cmake version 3.22.2", CMAKE_MIN)
            .expect_err("3.22 is the version this box ships and it cannot build a suite");
        let text = format!("{err:#}");
        assert!(
            text.contains("3.22") && text.contains("3.24"),
            "the refusal must name what was found and what is needed: {text}"
        );
        // A probe that says nothing is not a pass -- that is the gate going quiet.
        assert!(accept_cmake("", CMAKE_MIN).is_err());
        assert!(accept_cmake("cmake version banana", CMAKE_MIN).is_err());
    }

    fn tc_corpus(root: &Path, battery: &str) {
        let b = root.join("test-corpus/Public-Tests").join(battery);
        let case = |name: &str| {
            let d = b.join(name);
            std::fs::create_dir_all(d.join("test_vectors")).unwrap();
            d
        };
        for independent in ["alpha_lib", "beta"] {
            let d = case(independent);
            std::fs::create_dir_all(d.join("test_case")).unwrap();
            std::fs::write(d.join("test_case/lib.c"), "int f(void){return 1;}\n").unwrap();
        }
        let real = case("macrodepth_add_5");
        std::fs::create_dir_all(real.join("test_case")).unwrap();
        std::fs::write(real.join("test_case/lib.c"), "int g(void){return 2;}\n").unwrap();
        for follower in ["macrodepth_add_10", "macrodepth_add_20"] {
            std::os::unix::fs::symlink(real.join("test_case"), case(follower).join("test_case"))
                .unwrap();
        }
    }

    /// The Test-Corpus half of the same move: layout and followers came out of the shared driver. On a
    /// FIXTURE, because the real submodule is absent from the fork-safe checkout CI runs this in.
    #[test]
    fn a_test_corpus_battery_keeps_its_battery_level_and_its_shared_source_followers() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        tc_corpus(tmp.path(), "B02_synthetic");
        let paths = paths_for(tmp.path(), Dataset::TestCorpus);
        assert!(
            paths
                .input_dir("B02_synthetic")
                .join("macrodepth_add_10/test_case")
                .is_symlink(),
            "without the symlink there is no group to find and this test proves nothing"
        );
        let jobs = TestCorpus.jobs(&paths, "B02_synthetic", None).unwrap();
        assert_eq!(jobs.len(), 3, "two independent cases and one shared group");

        let shared: Vec<&Job> = jobs.iter().filter(|j| !j.followers.is_empty()).collect();
        assert_eq!(
            shared.len(),
            1,
            "the macrodepth cases are one shared-source group"
        );
        assert_eq!(shared[0].followers.len(), 2);
        let g = shared[0];
        assert!(matches!(g.shape, Shape::Shared));
        assert!(
            matches!(g.artifact, Artifact::Template { .. }),
            "the real case is the template its followers are derived from"
        );
        for f in &g.followers {
            assert_eq!(
                f.case_dir.parent().and_then(|p| p.file_name()),
                Some(std::ffi::OsStr::new("B02_synthetic")),
                "a Test-Corpus case keeps its battery level: {}",
                f.case_dir.display()
            );
            assert!(f.case_inputs.join("test_case").exists());
        }
        // Every case covered once, followers included: the scorer's denominator.
        assert_eq!(
            crate::chain::case_dirs(&jobs).len(),
            battery::all_case_names(
                &battery::discover(&paths.corpus_dir, "B02_synthetic", None).unwrap()
            )
            .len()
        );
    }

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

    /// The chain and the scorer must read ONE scope: both agents ran and were cached, then the oracle
    /// looked for a battery named `B01_organic/bin2hex_lib` and refused, after the money was spent.
    #[test]
    fn a_case_named_in_the_target_narrows_the_unit_and_the_score_alike() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let paths = Paths::new(
            tmp.path(),
            crate::cli::Tool::Claude,
            crate::cli::Variant::Default,
            Dataset::TestCorpus,
            None,
            crate::store::Mode::ReadWrite,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap();

        let one = Scope::resolve(&TestCorpus, &paths, "B01_organic/bin2hex_lib", None).unwrap();
        assert_eq!(
            one.units(),
            ["B01_organic".to_string()],
            "the unit is the BATTERY; the corpus holds no `B01_organic/bin2hex_lib` battery"
        );
        assert_eq!(one.filter(), Some("bin2hex_lib$"));
        assert_eq!(
            one.covers(),
            oracle::Covers::Subset("bin2hex_lib$"),
            "else the infra gate grades every case the run never asked for"
        );

        // Non-vacuous: a whole-battery target really does differ on both counts.
        let whole = Scope::resolve(&TestCorpus, &paths, "B01_organic", None).unwrap();
        assert_eq!(whole.units(), ["B01_organic".to_string()]);
        assert_eq!(whole.filter(), None);
        assert_eq!(whole.covers(), oracle::Covers::WholeBattery);
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
