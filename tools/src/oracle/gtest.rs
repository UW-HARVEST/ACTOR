use super::{openssl_dir, BatteryMismatch, Enrichment, Scoring, TestMode, TestOutcome};
use crate::artifact::{Phase, Translate, Verify};
use crate::battery::Paths;
use crate::eval::Source;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

// ── harvest-bench testing ──────────────────────────────────────────────

/// `passed` defers to the canonical project pass rule in `crate::domain::outcome`, so
/// the pass column means the same thing across datasets.
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
fn harvest_bench_runner(corpus_dir: &Path) -> Result<PathBuf> {
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

/// Returns the whole [`HarvestBenchResult`] rather than a `(usize, usize, usize)` the
/// caller re-labels: `passed()` is `tests_ok > 0 && tests_failed == 0`, so transposing the
/// failed and skipped counts turns a project with failures and no skips into a PASS.
/// The cdylib has already linked by the time this runs, so `build_ok` here reports
/// whether the suite produced a readable report — false meaning nothing was measured.
fn score_harvest_bench_suite(
    runner: &Path,
    suite_dir: &Path,
    lib: &Path,
    report_json: &Path,
) -> Result<HarvestBenchResult> {
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

    // The runner exits 0 when every test passed and 1 when some failed; both are
    // results and both write the report. Any other status (2 = its own error, or a
    // signal) means it failed before scoring, and its stderr was previously discarded.
    if !matches!(out.status.code(), Some(0 | 1)) {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(20).collect();
        eprintln!(
            "⚠️  harvest-bench runner {} on suite {} — recording 0 tests\n{}",
            out.status,
            suite_dir.display(),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
        // The runner failed or its report is unusable: nothing measured, and build_ok stays
        // false so this is a failure rather than a silent zero.
        return Ok(HarvestBenchResult {
            tests_ok: 0,
            tests_failed: 0,
            tests_skipped: 0,
            build_ok: false,
        });
    }

    // A missing or malformed report (gtest suite failed to build, cdylib
    // incompatible, cmake choked) must record a zero-score case, not abort the
    // whole sweep with an error.
    //
    // `build_ok: false`, for the same reason the runner-error branch above uses it:
    // in this function it means "nothing was measured", not "the cdylib failed to
    // link". Exiting 0 or 1 is the runner promising it wrote a report; if the report
    // is absent or unreadable that promise is broken, and recording `build_ok: true`
    // with zero tests would present an infra failure as a legitimate zero — the
    // silent-zero the stale-report deletion above exists to prevent.
    let unmeasured = || HarvestBenchResult {
        tests_ok: 0,
        tests_failed: 0,
        tests_skipped: 0,
        build_ok: false,
    };
    let Ok(data) = std::fs::read_to_string(report_json) else {
        eprintln!(
            "⚠️  harvest-bench runner produced no report {} — recording 0 tests",
            report_json.display()
        );
        return Ok(unmeasured());
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else {
        eprintln!(
            "⚠️  harvest-bench runner report at {} is not valid JSON — recording 0 tests",
            report_json.display()
        );
        return Ok(unmeasured());
    };
    let verdicts = json.pointer("/run/verdicts").and_then(|v| v.as_array());
    let Some(verdicts) = verdicts else {
        return Ok(unmeasured());
    };

    // Past here a report was read and parsed, so the suite did run: `build_ok` is
    // true even if the verdict list turns out to be empty.
    let mut res = HarvestBenchResult {
        tests_ok: 0,
        tests_failed: 0,
        tests_skipped: 0,
        build_ok: true,
    };
    for v in verdicts {
        let passed = v.get("passed").and_then(|b| b.as_bool()).unwrap_or(false);
        let skip = v.get("skipped").and_then(|b| b.as_bool()).unwrap_or(false);
        if skip {
            res.tests_skipped += 1;
        } else if passed {
            res.tests_ok += 1;
        } else {
            res.tests_failed += 1;
        }
    }
    Ok(res)
}

fn load_harvest_bench_stored(
    covered: &[crate::eval::Case],
) -> std::collections::BTreeMap<String, HarvestBenchResult> {
    let mut map = std::collections::BTreeMap::new();
    for crate::eval::Case {
        name, record_into, ..
    } in covered
    {
        if let Ok(data) = std::fs::read_to_string(record_into.join("result.json")) {
            if let Ok(r) = serde_json::from_str::<HarvestBenchResult>(&data) {
                map.insert(name.clone(), r);
            }
        }
    }
    map
}

pub fn run_harvest_bench_test(
    paths: &Paths,
    projects: &[crate::battery::HarvestBenchProject],
    scoring: &Scoring<'_>,
) -> Result<TestOutcome> {
    let runner = harvest_bench_runner(&paths.corpus_dir)?;

    // Verify's artifact where the run resolved one, else translate's — from the values, not a stat.
    let (archive_t, archive_v);
    let (translate, verify) = match &scoring.source {
        Source::Run { translate, verify } => (*translate, *verify),
        Source::Archive => {
            archive_t = crate::artifact::archived_artifacts::<Translate>(&paths.results_dir)?;
            archive_v = crate::artifact::archived_artifacts::<Verify>(&paths.results_dir)?;
            (&archive_t, &archive_v)
        }
    };

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
        if let Some(v) = verify.get(&case_dir) {
            scope.materialise(project.name(), v, &case_dir)?;
        } else if let Some(t) = translate.get(&case_dir) {
            scope.materialise(project.name(), t, &case_dir)?;
        } else {
            absent.push(project.name());
        }
    }
    let materialised = scope.finish()?;
    scoring.source.provenance().announce();

    let stored = load_harvest_bench_stored(materialised.cases());
    let mode = scoring.mode;

    let mut results: std::collections::BTreeMap<String, HarvestBenchResult> = Default::default();
    let mut passed = 0usize;
    let mut build_failed = 0usize;
    let mut recorded = 0usize;

    // A project the harness got no crate out of is a FAILED project, not an absent one:
    // `continue`ing shrank the denominator, publishing `N/6` for 7 projects.
    for name in &absent {
        build_failed += 1;
        println!("  ❌ {name}: no crate this run resolved — counted as a build failure");
        results.insert(
            (*name).to_string(),
            HarvestBenchResult {
                tests_ok: 0,
                tests_failed: 0,
                tests_skipped: 0,
                build_ok: false,
            },
        );
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

        let r = match so {
            None => {
                build_failed += 1;
                println!("  ❌ {name}: build failed (no cdylib)");
                HarvestBenchResult {
                    tests_ok: 0,
                    tests_failed: 0,
                    tests_skipped: 0,
                    build_ok: false,
                }
            }
            Some(so) => {
                let report = crate_dir.join("harvest_bench_report.json");
                let res = score_harvest_bench_suite(&runner, project.gtest_suite(), &so, &report)?;
                if res.passed() {
                    passed += 1;
                    println!(
                        "  ✅ {name}: {} ok, {} skipped",
                        res.tests_ok, res.tests_skipped
                    );
                } else if res.tests_failed > 0 {
                    println!(
                        "  ⚠️  {name}: {} ok, {} FAILED, {} skipped",
                        res.tests_ok, res.tests_failed, res.tests_skipped
                    );
                } else {
                    println!("  ⚠️  {name}: no tests passed");
                }
                res
            }
        };

        if matches!(mode, TestMode::Update) {
            let mut json = serde_json::to_value(&r)?;
            let tlog = crate_dir.join("logs").join(Translate::LOG);
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

    let total = results.len();
    anyhow::ensure!(
        total == projects.len(),
        "harvest-bench denominator is {total} but {} projects were requested; a project \
         was dropped rather than scored, which is how `N/6 projects pass` was once \
         published for a 7-project dataset",
        projects.len()
    );
    println!("\nharvest-bench: {passed}/{total} projects pass ({build_failed} build failures)");

    match mode {
        TestMode::Update => {
            println!("📝 result.json written for {recorded} of {total} projects");
            Ok(TestOutcome::Ok)
        }
        TestMode::Check => {
            let mut diffs = Vec::new();
            for (name, actual) in &results {
                match stored.get(name) {
                    None => diffs.push(format!("{name}: missing stored result")),
                    Some(exp) => {
                        if actual.tests_ok < exp.tests_ok {
                            diffs.push(format!(
                                "{name}: tests_ok expected={} actual={}",
                                exp.tests_ok, actual.tests_ok
                            ));
                        }
                        if actual.tests_failed > exp.tests_failed {
                            diffs.push(format!(
                                "{name}: tests_failed expected={} actual={}",
                                exp.tests_failed, actual.tests_failed
                            ));
                        }
                        if exp.build_ok && !actual.build_ok {
                            diffs.push(format!("{name}: build_ok expected=true actual=false"));
                        }
                    }
                }
            }
            if diffs.is_empty() {
                println!("✅ No regressions");
                Ok(TestOutcome::Passed)
            } else {
                println!("\n❌ {} regression(s):", diffs.len());
                for d in &diffs {
                    println!("  {d}");
                }
                Ok(TestOutcome::Failed(vec![BatteryMismatch {
                    battery: "harvest-bench".into(),
                    diffs,
                }]))
            }
        }
        TestMode::Run => Ok(TestOutcome::Ok),
    }
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

        let scored = score_harvest_bench_suite(
            Path::new("/bin/false"),
            tmp.path(),
            Path::new("libx.so"),
            &report,
        )
        .unwrap();

        assert_eq!(
            (
                scored.tests_ok,
                scored.tests_failed,
                scored.tests_skipped,
                scored.build_ok
            ),
            (0, 0, 0, false),
            "the stale report must not be scored"
        );
        assert!(
            !report.exists(),
            "and must not be left to mislead the next run either"
        );
    }

    /// Exit 2 is the runner failing, not a test failing: whatever report is on disk
    /// afterwards did not come from a completed scoring run.
    #[test]
    fn a_runner_that_errors_is_not_scored_from_the_file_it_left() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let report = tmp.path().join("harvest_bench_report.json");
        let fake = crate::cache::fake_program(
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

        let scored =
            score_harvest_bench_suite(Path::new(&fake), tmp.path(), Path::new("libx.so"), &report)
                .unwrap();

        assert!(
            report.is_file(),
            "fixture assumption: the fake runner did write a report"
        );
        assert_eq!(
            (
                scored.tests_ok,
                scored.tests_failed,
                scored.tests_skipped,
                scored.build_ok
            ),
            (0, 0, 0, false),
            "a runner error must not be scored as 3 passes"
        );
    }
}
