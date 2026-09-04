use super::{openssl_dir, Enrichment, Scoring};
use crate::battery::Paths;
use crate::prompt::Role;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

// ── harvest-bench testing ──────────────────────────────────────────────

/// What scoring one project concluded. Every variant is a statement about the TRANSLATION, so a broken
/// harness is none of them and returns `Err`. `build_ok: bool` was ONE flag for two unrelated facts --
/// "produced a cdylib" and "the harness came back" -- so a 3.24 keyword on a 3.22 box printed
/// `Builds: \textbf{no}` against seven crates that compiled.
#[derive(Debug)]
enum ProjectScore {
    /// No cdylib. This is what the published `Builds` column means, and all it means.
    CrateDidNotBuild,
    /// A cdylib the suite could not LINK against: the translation does not export the ABI. Neither of
    /// the others -- it DID produce a cdylib, yet nothing was measured.
    AbiIncomplete { missing: Vec<String> },
    Measured {
        tests_ok: usize,
        tests_failed: usize,
        tests_skipped: usize,
        /// PRINT-ONLY: inside `record()` this would rewrite every published result.json.
        failing: Vec<String>,
    },
}

impl ProjectScore {
    /// Infallible BECAUSE a harness failure never gets this far.
    fn record(&self) -> HarvestBenchResult {
        match self {
            Self::CrateDidNotBuild => HarvestBenchResult {
                tests_ok: 0,
                tests_failed: 0,
                tests_skipped: 0,
                build_ok: false,
                missing_symbols: None,
            },
            Self::AbiIncomplete { missing } => HarvestBenchResult {
                tests_ok: 0,
                tests_failed: 0,
                tests_skipped: 0,
                build_ok: true,
                missing_symbols: Some(missing.clone()),
            },
            Self::Measured {
                tests_ok,
                tests_failed,
                tests_skipped,
                ..
            } => HarvestBenchResult {
                tests_ok: *tests_ok,
                tests_failed: *tests_failed,
                tests_skipped: *tests_skipped,
                build_ok: true,
                missing_symbols: None,
            },
        }
    }
}

/// A struct, not `get("passed").unwrap_or(false)`: that counted a `passed`-less verdict as a FAILURE.
#[derive(Deserialize)]
struct Verdict {
    passed: bool,
    #[serde(default)]
    skipped: bool,
    #[serde(default)]
    name: String,
    #[serde(default)]
    failure: String,
}

/// The published record; `passed` defers to `crate::domain::outcome` so the column means one thing.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct HarvestBenchResult {
    tests_ok: usize,
    tests_failed: usize,
    tests_skipped: usize,
    build_ok: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    missing_symbols: Option<Vec<String>>,
}

impl HarvestBenchResult {
    fn passed(&self) -> bool {
        crate::domain::outcome::ProjectOutcome {
            built: self.build_ok,
            tests_ok: self.tests_ok as u32,
            tests_failed: self.tests_failed as u32,
        }
        .passed()
    }
}

/// `corpus_dir` is `harvest-bench/tests`, hence the `.parent()`.
pub(crate) fn harvest_bench_runner(corpus_dir: &Path) -> Result<PathBuf> {
    let bin = corpus_dir
        .parent()
        .context("harvest-bench/tests has no parent")?
        .join("runner/target/release/harvest-bench");
    anyhow::ensure!(bin.is_file(),
        "harvest-bench runner not built: {} (run `cargo build --release --manifest-path harvest-bench/runner/Cargo.toml`)",
        bin.display());
    Ok(bin)
}

/// The gtest suite links `lib<name>.so` by ABI, so the name must match exactly.
/// Build the cdylib the suite links against. `Err` is INFRA, not a failed translation.
///
/// It used to return `(Option<PathBuf>, String)` and never inspect `out.status`: the verdict was
/// `so.is_file()` alone, and a spawn failure returned `(None, "failed to spawn cargo build")`. So a
/// missing `timeout` or `cargo` on the scoring process's PATH, `timeout`'s own 124 or 127, or a fork
/// returning EAGAIN under `--parallel` printed "build failed (no cdylib)" and recorded
/// `CrateDidNotBuild` -> `Builds: no` in `harvest-bench.tex` for all seven projects at once. That is
/// exactly what a cmake needing 3.24 on a 3.22 box already did once, against seven crates that
/// compiled perfectly.
fn build_harvest_bench_lib(crate_dir: &Path, name: &str) -> Result<(Option<PathBuf>, String)> {
    let out = Command::new("timeout")
        .args(["600", "cargo", "build", "--release"])
        .env("OPENSSL_DIR", openssl_dir())
        .env("OPENSSL_NO_VENDOR", "1")
        .current_dir(crate_dir)
        .output()
        .context("spawning `cargo build --release` under `timeout`")?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // cargo normalizes `-`→`_` in the cdylib output name.
    let lib_stem = name.replace('-', "_");
    let so = crate_dir.join(format!("target/release/lib{lib_stem}.so"));
    if so.is_file() {
        return Ok((Some(so), stderr));
    }
    // 124 is `timeout`'s kill and 127 is "command not found": neither says anything about the
    // translation, and calling them a build failure attributes an infra fault to the agent.
    match out.status.code() {
        Some(124) => anyhow::bail!(
            "`cargo build` for {name} was killed at the 600s ceiling, so whether the crate builds is \
             unmeasured -- not `Builds: no`"
        ),
        Some(127) => anyhow::bail!(
            "`timeout` or `cargo` is not on the scoring process's PATH, so no project can be built \
             here. That is an infrastructure fault, not seven failed translations."
        ),
        _ => Ok((None, stderr)),
    }
}

/// The symbols the suite needed and the cdylib did not export; `None` if this is not a link failure.
/// Keyed on the RUNNER'S OWN step label, so IT says which exit-2 meaning applies -- plus `undefined
/// reference`, since that step also fails for compiler reasons.
fn unlinkable_abi(stderr: &str) -> Option<Vec<String>> {
    if !stderr.contains("cmake build (suite) failed") || !stderr.contains("undefined reference to")
    {
        return None;
    }
    let mut missing: Vec<String> = stderr
        .split("undefined reference to `")
        .skip(1)
        .filter_map(|rest| rest.split('\'').next())
        .map(str::to_owned)
        .collect();
    missing.sort_unstable();
    missing.dedup();
    Some(missing)
}

/// `Err` means the HARNESS did not work, never that the translation scored zero: the caller refuses.
///
/// A named variant rather than a `(usize, usize, usize)` the caller re-labels: transposing the failed
/// and skipped counts turns a project with failures and no skips into a PASS.
fn score_harvest_bench_suite(
    runner: &Path,
    suite_dir: &Path,
    lib: &Path,
    report_json: &Path,
) -> Result<ProjectScore> {
    // Suite build dir is per-result so parallel/rerun don't collide.
    let build_dir = report_json
        .parent()
        .unwrap_or(Path::new("."))
        .join("gtest_build");

    // The runner writes the report only once the suite ran, so a rerun that dies earlier would leave
    // the PREVIOUS run's file to be scored as this one's.
    if report_json.exists() {
        std::fs::remove_file(report_json)
            .with_context(|| format!("removing the stale report {}", report_json.display()))?;
    }

    let out = Command::new(runner)
        .arg("run")
        .args(["--suite".as_ref(), suite_dir.as_os_str()])
        .args(["--lib".as_ref(), lib.as_os_str()])
        .args(["--build-dir".as_ref(), build_dir.as_os_str()])
        .args(["--json".as_ref(), report_json.as_os_str()])
        .output()
        .context("invoking harvest-bench runner")?;

    // The runner exits 0 when every test passed and 1 when some failed; both are results and both
    // write the report. Exit 2 is the runner returning `Err`: its own failure, OR `cmake build (suite)`
    // failing to LINK against the translated cdylib -- which is the TRANSLATION's, so it is scored.
    if !matches!(out.status.code(), Some(0 | 1)) {
        let err = String::from_utf8_lossy(&out.stderr);
        if let Some(missing) = unlinkable_abi(&err) {
            return Ok(ProjectScore::AbiIncomplete { missing });
        }
        let tail: Vec<&str> = err.lines().rev().take(20).collect();
        anyhow::bail!(
            "the harvest-bench runner exited {} on suite {}, so the HARNESS failed and this project \
             has no score. Its stderr:\n{}",
            out.status,
            suite_dir.display(),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }

    // Exiting 0 or 1 PROMISES a report; each broken promise below names itself, none being the
    // translation's fault.
    let data = std::fs::read_to_string(report_json).with_context(|| {
        format!(
            "the runner exited {:?} promising a report at {}, and wrote none",
            out.status.code(),
            report_json.display()
        )
    })?;
    let json: serde_json::Value = serde_json::from_str(&data).with_context(|| {
        format!(
            "the runner's report at {} is not JSON",
            report_json.display()
        )
    })?;
    let verdicts = json
        .pointer("/run/verdicts")
        .cloned()
        .with_context(|| format!("{} has no /run/verdicts", report_json.display()))?;
    let verdicts: Vec<Verdict> = serde_json::from_value(verdicts)
        .with_context(|| format!("{} holds a verdict this cannot read", report_json.display()))?;

    let (mut tests_ok, mut tests_failed, mut tests_skipped) = (0usize, 0usize, 0usize);
    let mut failing = Vec::new();
    for v in &verdicts {
        if v.skipped {
            tests_skipped += 1;
        } else if v.passed {
            tests_ok += 1;
        } else {
            tests_failed += 1;
            let why = v.failure.lines().next().unwrap_or_default();
            failing.push(if why.is_empty() {
                v.name.clone()
            } else {
                format!("{} ({why})", v.name)
            });
        }
    }

    // `runtests::measured_nothing`'s rule: a skip is not a judgement, so `passed + failed == 0`
    // measured NOTHING however many verdicts came back.
    anyhow::ensure!(
        tests_ok + tests_failed > 0,
        "the suite for {} returned {} verdict(s) and judged NONE of them ({tests_skipped} skipped), \
         so this is not a score of zero -- nothing was measured",
        suite_dir.display(),
        verdicts.len(),
    );
    Ok(ProjectScore::Measured {
        tests_ok,
        tests_failed,
        tests_skipped,
        failing,
    })
}

pub fn run_harvest_bench_test(
    paths: &Paths,
    projects: &[crate::battery::HarvestBenchProject],
    scoring: &Scoring<'_>,
) -> Result<()> {
    let runner = harvest_bench_runner(&paths.corpus_dir)?;

    // Every project REQUESTED, not only those that resolved a crate: grading the resolved set grades
    // the one set with no infra failure in it.
    scoring.gate.grade(
        &projects
            .iter()
            .map(|p| crate::agent_health::Run {
                name: p.name().to_string(),
                case_dir: paths.output_dir(p.name()),
            })
            .collect::<Vec<_>>(),
    )?;

    let mut scope = scoring.tree.scope("")?;
    let mut absent: Vec<&str> = Vec::new();
    for project in projects {
        let case_dir = paths.output_dir(project.name());
        // The LAST role the chain RESOLVED, not whichever phase dir a stat happens to find: that is
        // what let a five-day-old `verified/` be scored as this run's.
        let last = scoring.roles.iter().rev().find_map(|r| {
            scoring
                .resolved
                .get(&case_dir.join(r.dir()))
                .map(|t| (*r, t))
        });
        match last {
            Some((role, tree)) => {
                // The suite is built from `project.gtest_suite()` in the corpus, so the tree needs
                // the crate and nothing else. This passed `case_dir` -- a RESULTS dir -- as a corpus one.
                scope.materialise(
                    project.name(),
                    tree,
                    &crate::transform::Graded::AbiSuite,
                    &case_dir.join(role.dir()),
                )?;
            }
            None => absent.push(project.name()),
        }
    }
    let materialised = scope.finish()?;

    let mut results: std::collections::BTreeMap<String, HarvestBenchResult> = Default::default();
    let mut passed = 0usize;
    let mut build_failed = 0usize;
    let mut unlinkable = 0usize;
    let mut recorded = 0usize;

    // Refused together below: several failing for the SAME reason is the signal that says "environment",
    // and aborting on the first hides it. Scoring costs no agent, so finishing is free.
    let mut unmeasured: Vec<String> = Vec::new();

    // A project the harness got no crate out of is a FAILED project, not an absent one:
    // `continue`ing shrank the denominator, publishing `N/6` for 7 projects.
    for name in &absent {
        build_failed += 1;
        println!("  ❌ {name}: no crate this run resolved — counted as a build failure");
        results.insert((*name).to_string(), ProjectScore::CrateDidNotBuild.record());
    }

    for project in projects {
        let name = project.name();
        let Some(case) = materialised.cases().iter().find(|c| c.name == name) else {
            continue;
        };
        let crate_dir = materialised.crate_root(name);
        let logs_dir = case.record_into.join("logs");
        std::fs::create_dir_all(&logs_dir)?;

        let (so, build_log) = build_harvest_bench_lib(&crate_dir, name)?;
        std::fs::write(logs_dir.join("test.log"), &build_log)?;

        let score = match so {
            None => {
                build_failed += 1;
                println!("  ❌ {name}: build failed (no cdylib)");
                ProjectScore::CrateDidNotBuild
            }
            Some(so) => {
                let report = crate_dir.join("harvest_bench_report.json");
                match score_harvest_bench_suite(&runner, project.gtest_suite(), &so, &report) {
                    Ok(score) => score,
                    Err(why) => {
                        println!("  ‼️  {name}: NOT MEASURED — {why:#}");
                        unmeasured.push(name.to_string());
                        // No record: there is no honest result.json for a measurement that never happened.
                        continue;
                    }
                }
            }
        };

        let r = score.record();
        if r.passed() {
            passed += 1;
            println!(
                "  ✅ {name}: {} ok, {} skipped",
                r.tests_ok, r.tests_skipped
            );
        } else if r.tests_failed > 0 {
            let failing = match &score {
                ProjectScore::Measured { failing, .. } => failing.clone(),
                _ => Vec::new(),
            };
            println!(
                "  ⚠️  {name}: {} ok, {} FAILED, {} skipped (e.g. {})",
                r.tests_ok,
                r.tests_failed,
                r.tests_skipped,
                failing
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        } else if let Some(missing) = &r.missing_symbols {
            unlinkable += 1;
            println!(
                "  ⚠️  {name}: cdylib built, but the suite cannot link it — {} symbol(s) not exported \
                 (e.g. {})",
                missing.len(),
                missing.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            );
        }

        {
            let mut json = serde_json::to_value(&r)?;
            // From the CASE dir, and BOTH roles. This read `crate_dir/logs/translation.log`: the wrong
            // tree (an eval-tree crate can hold no `logs/` -- see `Role::transcript_in`), and the
            // `translate` key hardcoded onto whichever step ran, so a verify session's credits were
            // filed under a key `analyse::report` sums separately. kiro's real harvest-bench spend was
            // dropped outright.
            let tlog = Role::Translate.transcript_in(&case.case_dir);
            let vlog = Role::Verify.transcript_in(&case.case_dir);
            Enrichment::compute(
                &crate_dir.join("src"),
                &[("translate", &tlog), ("verify", &vlog)],
            )
            .merge_into(&mut json);
            let at = case.record_into.join("result.json");
            crate::oracle::runtests::stamp(&mut json, &at, scoring.provenance);
            std::fs::write(&at, serde_json::to_string_pretty(&json)? + "\n")?;
            recorded += 1;
        }

        results.insert(name.to_string(), r);
    }

    // BEFORE the denominator check, which these also trip: "a project was dropped" says nothing of why.
    anyhow::ensure!(
        unmeasured.is_empty(),
        "{} of {} harvest-bench project(s) judged no test vector: {}. These are HARNESS failures, not \
         scores of zero -- publishing them prints `Builds: no` against crates that may compile \
         perfectly, which is what a CMake keyword needing 3.24 on a 3.22 box did to all seven at \
         once. Each project's reason is above.",
        unmeasured.len(),
        projects.len(),
        unmeasured.join(", "),
    );

    let total = results.len();
    anyhow::ensure!(
        total == projects.len(),
        "harvest-bench denominator is {total} but {} projects were requested; a project \
         was dropped rather than scored, which is how `N/6 projects pass` was once \
         published for a 7-project dataset",
        projects.len()
    );
    print!("\nharvest-bench: {passed}/{total} projects pass ({build_failed} build failure(s)");
    if unlinkable > 0 {
        print!(", {unlinkable} compiled but exported an incomplete ABI");
    }
    println!(")");

    println!("📝 result.json written for {recorded} of {total} projects");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const STALE: &str = r#"{"run":{"verdicts":[{"passed":true},{"passed":true},{"passed":true}]}}"#;

    /// Three passes reported for a rerun that produced none.
    #[test]
    fn a_failed_rerun_never_scores_the_previous_runs_report() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let report = tmp.path().join("harvest_bench_report.json");
        fs::write(&report, STALE).unwrap();

        let refused = score_harvest_bench_suite(
            Path::new("/bin/false"),
            tmp.path(),
            Path::new("libx.so"),
            &report,
        )
        .expect_err("the stale report must not become this run's score");

        assert!(
            !report.exists(),
            "and must not be left to mislead the next run either"
        );
        assert!(
            !format!("{refused:#}").contains("score of zero"),
            "a runner that never started is a harness failure, not the no-vector case: {refused:#}"
        );
    }

    /// Exit 2 is the runner failing, not a test failing: whatever report is on disk
    /// afterwards did not come from a completed scoring run.
    #[test]
    fn a_runner_that_errors_is_not_scored_from_the_file_it_left() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let report = tmp.path().join("harvest_bench_report.json");
        let fake = crate::io::workdir::fake_program(
            tmp.path(),
            "fake-runner",
            &format!(
                "while [ $# -gt 0 ]; do\n\
             \x20 case \"$1\" in --json) shift; printf '%s' '{STALE}' > \"$1\";; esac\n\
             \x20 shift\n\
             done\n\
             exit 2"
            ),
        )
        .unwrap();

        let refused =
            score_harvest_bench_suite(Path::new(&fake), tmp.path(), Path::new("libx.so"), &report)
                .expect_err("a runner error must not be scored as 3 passes");

        assert!(
            report.is_file(),
            "fixture assumption: the fake runner did write a report"
        );
        assert!(
            format!("{refused:#}").contains("exited"),
            "and the refusal must say the RUNNER failed, not blame the translation: {refused:#}"
        );
    }

    /// Exits 0 and writes `body`, so what follows is decided by the report alone.
    fn runner_writing(dir: &Path, name: &str, body: &str) -> PathBuf {
        PathBuf::from(
            crate::io::workdir::fake_program(
                dir,
                name,
                &format!(
                    "while [ $# -gt 0 ]; do\n\
             \x20 case \"$1\" in --json) shift; printf '%s' '{body}' > \"$1\";; esac\n\
             \x20 shift\n\
             done\n\
             exit 0"
                ),
            )
            .unwrap(),
        )
    }

    /// Every way a report comes back with no judgement, and the ONE way it carries some. A suite whose
    /// gtest build failed reports an empty verdict list while the runner exits 0 -- see [`ProjectScore`].
    #[test]
    fn a_report_that_judges_nothing_is_refused_and_a_real_one_is_scored() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let refused = [
            ("empty", r#"{"run":{"verdicts":[]}}"#),
            (
                "all_skipped",
                r#"{"run":{"verdicts":[{"passed":false,"skipped":true},{"passed":false,"skipped":true}]}}"#,
            ),
            ("no_verdicts_key", r#"{"run":{}}"#),
            ("not_json", r#"this is not json"#),
            // A verdict with no `passed` field used to be counted as a FAILED test by
            // `unwrap_or(false)`, inventing a result out of a malformed report.
            (
                "verdict_without_passed",
                r#"{"run":{"verdicts":[{"name":"x"}]}}"#,
            ),
        ];
        for (case, body) in refused {
            let report = tmp.path().join(format!("{case}.json"));
            let runner = runner_writing(tmp.path(), &format!("runner-{case}"), body);
            let err = score_harvest_bench_suite(&runner, tmp.path(), Path::new("libx.so"), &report)
                .err()
                .unwrap_or_else(|| panic!("{case}: judged no vector, so it is not a score"));
            assert!(
                report.is_file(),
                "{case} fixture: the runner must really have written its report, or this refuses \
                 for the wrong reason"
            );
            assert!(!format!("{err:#}").is_empty(), "{case}: {err:#}");
        }

        // Non-vacuity: the refusals above must not have swallowed the measurable case too.
        let report = tmp.path().join("real.json");
        let runner = runner_writing(
            tmp.path(),
            "runner-real",
            r#"{"run":{"verdicts":[{"passed":true,"skipped":false},{"passed":false,"skipped":false,"name":"Sodium.pad","failure":"expected 3 got 4\nand a second line"},{"passed":false,"skipped":true}]}}"#,
        );
        let scored = score_harvest_bench_suite(&runner, tmp.path(), Path::new("libx.so"), &report)
            .expect("one pass and one failure IS a measurement");
        let r = scored.record();
        assert_eq!(
            (r.tests_ok, r.tests_failed, r.tests_skipped, r.build_ok),
            (1, 1, 1, true),
            "a skip is counted apart from a judgement, and a measured suite records build_ok"
        );
        let ProjectScore::Measured { failing, .. } = &scored else {
            panic!("a measured suite is Measured")
        };
        assert_eq!(failing, &["Sodium.pad (expected 3 got 4)".to_string()]);
    }

    /// A suite that will not LINK is the translation's failure, not the harness's. kiro's zstd compiled
    /// to a cdylib missing `ZSTD_flushStream` and four more; as a harness failure that ONE project
    /// refused its whole tool's run, five passing projects and every tool's merged tables with it.
    #[test]
    fn a_suite_that_cannot_link_the_cdylib_is_the_translations_failure_not_the_harness() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let ld = "error: cmake build (suite) failed:\n\
             /usr/bin/ld: glue_small.c:(.text+0x3fc): undefined reference to `ZSTD_flushStream'\n\
             /usr/bin/ld: glue_dicts.c:(.text+0x14): undefined reference to `ZSTD_createCDict'\n\
             /usr/bin/ld: glue_dicts.c:(.text+0x40): undefined reference to `ZSTD_createCDict'\n\
             collect2: error: ld returned 1 exit status";
        let runner = crate::io::workdir::fake_program(
            tmp.path(),
            "runner-unlinkable",
            &format!("cat >&2 <<'STDERR'\n{ld}\nSTDERR\nexit 2"),
        )
        .unwrap();
        let report = tmp.path().join("unlinkable.json");
        let score = score_harvest_bench_suite(
            Path::new(&runner),
            tmp.path(),
            Path::new("libzstd.so"),
            &report,
        )
        .expect("an unlinkable ABI is a score, not a refusal");
        let ProjectScore::AbiIncomplete { missing } = &score else {
            panic!("expected AbiIncomplete, got {score:?}")
        };
        assert_eq!(
            missing,
            &[
                "ZSTD_createCDict".to_string(),
                "ZSTD_flushStream".to_string()
            ],
            "every missing symbol once, so the record names what to translate next"
        );
        let r = score.record();
        assert!(
            r.build_ok && r.tests_ok == 0 && r.tests_failed == 0 && !r.passed(),
            "the crate DID compile, and nothing was measured: {r:?}"
        );

        // Non-vacuous: exit 2 for the runner's OWN failure still refuses.
        let broken = crate::io::workdir::fake_program(
            tmp.path(),
            "runner-broken",
            "echo 'error: cmake configure (suite) failed:\nNo CMAKE_CXX_COMPILER could be found.' >&2\nexit 2",
        )
        .unwrap();
        let err = score_harvest_bench_suite(
            Path::new(&broken),
            tmp.path(),
            Path::new("libzstd.so"),
            &tmp.path().join("broken.json"),
        )
        .expect_err("a runner that could not configure has measured nothing");
        assert!(format!("{err:#}").contains("HARNESS failed"));
    }

    /// The published `Builds` column reads this field, so it must mean the crate compiled and nothing else.
    #[test]
    fn only_a_crate_that_produced_no_cdylib_records_a_failed_build() {
        assert!(!ProjectScore::CrateDidNotBuild.record().build_ok);
        assert!(
            ProjectScore::AbiIncomplete {
                missing: vec!["ZSTD_flushStream".into()]
            }
            .record()
            .build_ok,
            "a linker complaint is not the compiler's: this crate produced a cdylib"
        );
        assert!(
            ProjectScore::Measured {
                tests_ok: 0,
                tests_failed: 3,
                tests_skipped: 0,
                failing: vec!["Sodium.pad".into()],
            }
            .record()
            .build_ok,
            "a crate whose tests all FAIL still compiled, and must not be reported as not building"
        );
    }
}
