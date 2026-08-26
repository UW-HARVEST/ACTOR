use super::{openssl_dir, Enrichment, Scoring};
use crate::battery::Paths;
use crate::prompt::Role;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

// ── harvest-bench testing ──────────────────────────────────────────────

/// What scoring one project concluded. Both variants are statements about the TRANSLATION, the only
/// thing this dataset publishes, so a broken harness is neither and returns `Err` -- not a third
/// variant every match site would carry forever. `build_ok: bool` was ONE flag for two unrelated
/// facts, "the crate produced a cdylib" and "the harness came back", so when a CMake keyword needing
/// 3.24 met a 3.22 box the table printed `Builds: \textbf{no}` against seven crates that compiled.
#[derive(Debug)]
enum ProjectScore {
    /// No cdylib. This is what the published `Builds` column means, and all it means.
    CrateDidNotBuild,
    Measured {
        tests_ok: usize,
        tests_failed: usize,
        tests_skipped: usize,
    },
}

impl ProjectScore {
    /// Infallible BECAUSE a harness failure never gets this far: there is no score to project.
    fn record(&self) -> HarvestBenchResult {
        match *self {
            Self::CrateDidNotBuild => HarvestBenchResult {
                tests_ok: 0,
                tests_failed: 0,
                tests_skipped: 0,
                build_ok: false,
            },
            Self::Measured {
                tests_ok,
                tests_failed,
                tests_skipped,
            } => HarvestBenchResult {
                tests_ok,
                tests_failed,
                tests_skipped,
                build_ok: true,
            },
        }
    }
}

/// A struct, not `get("passed").unwrap_or(false)`: that counted a verdict with no `passed` field as a
/// FAILED test, inventing a result out of a malformed report.
#[derive(Deserialize)]
struct Verdict {
    passed: bool,
    #[serde(default)]
    skipped: bool,
}

/// The published record. `passed` defers to the canonical project pass rule in
/// `crate::domain::outcome`, so the pass column means the same thing across datasets.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct HarvestBenchResult {
    tests_ok: usize,
    tests_failed: usize,
    tests_skipped: usize,
    build_ok: bool,
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
fn build_harvest_bench_lib(crate_dir: &Path, name: &str) -> (Option<PathBuf>, String) {
    let out = Command::new("timeout")
        .args(["600", "cargo", "build", "--release"])
        .env("OPENSSL_DIR", openssl_dir())
        .env("OPENSSL_NO_VENDOR", "1")
        .current_dir(crate_dir)
        .output();
    let Ok(out) = out else {
        return (None, "failed to spawn cargo build".into());
    };
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // cargo normalizes `-`→`_` in the cdylib output name.
    let lib_stem = name.replace('-', "_");
    let so = crate_dir.join(format!("target/release/lib{lib_stem}.so"));
    if so.is_file() {
        (Some(so), stderr)
    } else {
        (None, stderr)
    }
}

/// `Err` means the HARNESS did not work, never that the translation scored zero -- the caller batches
/// these and refuses, exactly as `runtests::measured_nothing` does for Test-Corpus.
///
/// Returns a named variant rather than a `(usize, usize, usize)` the caller re-labels: `passed()` is
/// `tests_ok > 0 && tests_failed == 0`, so transposing the failed and skipped counts turns a project
/// with failures and no skips into a PASS.
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

    // The runner only writes the report once the suite ran, so a rerun that dies
    // earlier leaves the PREVIOUS run's file in place and the code below would score
    // it as this run's result.
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
    // write the report. Any other status (2 = its own error, or a signal) means it failed before
    // scoring, so there is no measurement to report either way.
    if !matches!(out.status.code(), Some(0 | 1)) {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(20).collect();
        anyhow::bail!(
            "the harvest-bench runner exited {} on suite {}, so the HARNESS failed and this project \
             has no score. Its stderr:\n{}",
            out.status,
            suite_dir.display(),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }

    // Exiting 0 or 1 is the runner PROMISING it wrote a report; each broken promise below says which
    // one broke, because none of them is the translation's fault.
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
    for v in &verdicts {
        if v.skipped {
            tests_skipped += 1;
        } else if v.passed {
            tests_ok += 1;
        } else {
            tests_failed += 1;
        }
    }

    // `runtests::measured_nothing`'s rule, for the same reason: a skip is not a judgement, so
    // `passed + failed == 0` measured NOTHING however many verdicts came back.
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
    })
}

pub fn run_harvest_bench_test(
    paths: &Paths,
    projects: &[crate::battery::HarvestBenchProject],
    scoring: &Scoring<'_>,
) -> Result<()> {
    let runner = harvest_bench_runner(&paths.corpus_dir)?;

    // Every project REQUESTED, not only those that resolved a crate: a run that died on `api_error`
    // publishes nothing, so grading the resolved set grades the one set with no infra failure in it.
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
        // The LAST role the chain resolved, from the values rather than a stat: a chain's final tree
        // is what its numbers describe, and asking the filesystem which phase dir exists is what let
        // a five-day-old `verified/` be scored as this run's.
        let last = scoring.roles.iter().rev().find_map(|r| {
            scoring
                .resolved
                .get(&case_dir.join(r.dir()))
                .map(|t| (*r, t))
        });
        match last {
            Some((role, tree)) => {
                scope.materialise(project.name(), tree, &case_dir, &case_dir.join(role.dir()))?;
            }
            None => absent.push(project.name()),
        }
    }
    let materialised = scope.finish()?;

    let mut results: std::collections::BTreeMap<String, HarvestBenchResult> = Default::default();
    let mut passed = 0usize;
    let mut build_failed = 0usize;
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

        let (so, build_log) = build_harvest_bench_lib(&crate_dir, name);
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
            println!(
                "  ⚠️  {name}: {} ok, {} FAILED, {} skipped",
                r.tests_ok, r.tests_failed, r.tests_skipped
            );
        }

        {
            let mut json = serde_json::to_value(&r)?;
            let tlog = crate_dir.join("logs").join(Role::Translate.log());
            Enrichment::compute(&crate_dir.join("src"), &[("translate", &tlog)])
                .merge_into(&mut json);
            std::fs::write(
                case.record_into.join("result.json"),
                serde_json::to_string_pretty(&json)? + "\n",
            )?;
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
    println!("\nharvest-bench: {passed}/{total} projects pass ({build_failed} build failures)");

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
        );

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
        PathBuf::from(crate::io::workdir::fake_program(
            dir,
            name,
            &format!(
                "while [ $# -gt 0 ]; do\n\
             \x20 case \"$1\" in --json) shift; printf '%s' '{body}' > \"$1\";; esac\n\
             \x20 shift\n\
             done\n\
             exit 0"
            ),
        ))
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
            r#"{"run":{"verdicts":[{"passed":true,"skipped":false},{"passed":false,"skipped":false},{"passed":false,"skipped":true}]}}"#,
        );
        let scored = score_harvest_bench_suite(&runner, tmp.path(), Path::new("libx.so"), &report)
            .expect("one pass and one failure IS a measurement");
        let r = scored.record();
        assert_eq!(
            (r.tests_ok, r.tests_failed, r.tests_skipped, r.build_ok),
            (1, 1, 1, true),
            "a skip is counted apart from a judgement, and a measured suite records build_ok"
        );
    }

    /// The published `Builds` column reads this field, so it must mean the crate compiled and nothing else.
    #[test]
    fn only_a_crate_that_produced_no_cdylib_records_a_failed_build() {
        assert!(!ProjectScore::CrateDidNotBuild.record().build_ok);
        assert!(
            ProjectScore::Measured {
                tests_ok: 0,
                tests_failed: 3,
                tests_skipped: 0,
            }
            .record()
            .build_ok,
            "a crate whose tests all FAIL still compiled, and must not be reported as not building"
        );
    }
}
