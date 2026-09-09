//! Linting the translation. A MEASUREMENT of the artifact, never a gate on it: a lint-heavy
//! translation still scores exactly what its vectors earned.
//!
//! Split the way the rest of [`crate::oracle`] is: [`lint_crate`] spawns and hands off, and every
//! decision about what a run MEANT is taken by [`verdict`] over `(exit, stdout)` alone, so it is
//! table-testable from literals with no cargo on the box.

use super::openssl_dir;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

/// The ceiling `build_harvest_bench_lib` already uses for a cargo invocation on one crate.
const TIMEOUT_SECS: &str = "600";

/// Lint counts for ONE crate, keyed by the lint's full name as cargo spells it
/// (`clippy::needless_range_loop`, `unused_variables`). The `clippy::` prefix is what separates a
/// clippy lint from a plain rustc warning, so stripping it would make the two indistinguishable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClippyCounts {
    pub warnings: usize,
    /// Clippy's `correctness` group is deny-by-default, so a crate that COMPILES can still emit
    /// `level: "error"` diagnostics. Those are lints the crate earned; a rustc error is not.
    pub errors: usize,
    pub by_lint: BTreeMap<String, usize>,
}

/// Externally tagged so `{"unmeasured": {…}}` and `{"measured": {"warnings": 0, …}}` cannot be
/// confused by any reader. A crate the linter could not judge is not a clean crate, and a flag
/// beside the counts is exactly the shape that lets a reader average one into the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clippy {
    Measured(ClippyCounts),
    Unmeasured { why: String },
}

/// A battery's lint tally. `cases_measured + cases_unmeasured` is the roster, so a reader can see
/// the denominator the warning count is over.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClippyTotals {
    pub warnings: usize,
    pub errors: usize,
    pub cases_measured: usize,
    pub cases_unmeasured: usize,
    pub by_lint: BTreeMap<String, usize>,
}

impl ClippyTotals {
    pub fn of<'a>(verdicts: impl Iterator<Item = &'a Clippy>) -> Self {
        let mut totals = Self::default();
        for verdict in verdicts {
            match verdict {
                Clippy::Unmeasured { .. } => totals.cases_unmeasured += 1,
                Clippy::Measured(counts) => {
                    totals.cases_measured += 1;
                    totals.warnings += counts.warnings;
                    totals.errors += counts.errors;
                    for (lint, n) in &counts.by_lint {
                        *totals.by_lint.entry(lint.clone()).or_default() += n;
                    }
                }
            }
        }
        totals
    }
}

/// `--no-deps` is the ONLY thing keeping a dependency's lints out of the translation's count, and
/// `--message-format=json` the only thing making the lints readable at all. Built here rather than
/// inline so both stay assertable.
///
/// Deliberately NOT `--all-targets` (stock clippy checks lib and bins, and widening the target set
/// is an ACTOR lint policy) and not `--locked` (an eval-tree crate may carry no lockfile, as
/// `runtests`' own build assumes).
fn clippy_argv(manifest: &Path, target_dir: &Path) -> Vec<OsString> {
    vec![
        TIMEOUT_SECS.into(),
        "cargo".into(),
        "clippy".into(),
        "--no-deps".into(),
        "--message-format=json".into(),
        "--manifest-path".into(),
        manifest.into(),
        "--target-dir".into(),
        target_dir.into(),
    ]
}

/// What a clippy run meant. `exit: None` is a spawn failure.
///
/// `Err` is INFRA and refuses the whole score, as [`super::gtest`] refuses a missing `cargo`: one
/// absent binary would otherwise blank every case in the battery at once and publish the blanks.
fn verdict(exit: Option<i32>, stdout: &str) -> Result<Clippy> {
    match exit {
        Some(127) => anyhow::bail!(
            "`timeout` or `cargo` is not on the scoring process's PATH, so no crate can be linted \
             here. That is an infrastructure fault, not a battery of unlintable translations."
        ),
        Some(124) => {
            return Ok(Clippy::Unmeasured {
                why: format!("clippy was killed at the {TIMEOUT_SECS}s ceiling"),
            })
        }
        None => {
            return Ok(Clippy::Unmeasured {
                why: "clippy could not be spawned".to_string(),
            })
        }
        _ => {}
    }

    let mut counts = ClippyCounts::default();
    let mut saw_a_message = false;
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(reason) = msg.get("reason").and_then(serde_json::Value::as_str) else {
            continue;
        };
        saw_a_message = true;
        if reason != "compiler-message" {
            continue;
        }
        let Some(diagnostic) = msg.get("message") else {
            continue;
        };
        let level = diagnostic
            .get("level")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let code = diagnostic
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(serde_json::Value::as_str);
        match (level, code) {
            ("error", Some(lint)) if lint.starts_with("clippy::") => {
                counts.errors += 1;
                *counts.by_lint.entry(lint.to_string()).or_default() += 1;
            }
            // Any other error is rustc's, so there is no lint verdict to give: the crate did not
            // compile. Judged on the CODE and not the exit status, because the deny-by-default
            // `correctness` group makes clippy exit non-zero over a crate that compiled perfectly.
            ("error", _) => {
                let why = diagnostic
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("the crate did not compile");
                return Ok(Clippy::Unmeasured {
                    why: why.to_string(),
                });
            }
            ("warning", Some(lint)) => {
                counts.warnings += 1;
                *counts.by_lint.entry(lint.to_string()).or_default() += 1;
            }
            // A codeless warning is the `N warnings emitted` trailer, which is a restatement of the
            // diagnostics above it. Counting it adds one phantom lint to every crate.
            _ => {}
        }
    }

    if !saw_a_message {
        return Ok(Clippy::Unmeasured {
            why: "cargo produced no JSON output to read a lint verdict from".to_string(),
        });
    }
    Ok(Clippy::Measured(counts))
}

/// Lint one crate of the EVAL tree. Sets no `current_dir`: the manifest is named instead, so nothing
/// here can run inside a published artifact.
///
/// `clippy-target/` is its own directory because `rust.py` pins the scoring build to the crate's
/// `target/`, and clippy-driver's fingerprints would thrash against it. Both die with the eval tree.
pub fn lint_crate(crate_root: &Path) -> Result<Clippy> {
    let out = Command::new("timeout")
        .args(clippy_argv(
            &crate_root.join("Cargo.toml"),
            &crate_root.join("clippy-target"),
        ))
        .env("OPENSSL_DIR", openssl_dir())
        .env("OPENSSL_NO_VENDOR", "1")
        // Same reason as the scoring build's: the agent's `[net] offline = true` is its sandbox
        // policy, not the scorer's, and an unresolvable pinned dependency would blank every case.
        .env("CARGO_NET_OFFLINE", "false")
        .output();
    match out {
        Ok(out) => verdict(out.status.code(), &String::from_utf8_lossy(&out.stdout))
            .with_context(|| format!("linting {}", crate_root.display())),
        Err(_) => verdict(None, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(level: &str, code: Option<&str>, text: &str) -> String {
        let code = match code {
            Some(c) => format!("{{\"code\":\"{c}\"}}"),
            None => "null".to_string(),
        };
        format!(
            "{{\"reason\":\"compiler-message\",\"message\":{{\"level\":\"{level}\",\
             \"code\":{code},\"message\":\"{text}\"}}}}"
        )
    }

    fn measured(v: &Clippy) -> &ClippyCounts {
        match v {
            Clippy::Measured(c) => c,
            Clippy::Unmeasured { why } => {
                panic!("expected a measured crate, got unmeasured: {why}")
            }
        }
    }

    /// A crate that does not compile has no lint verdict. Recording it as zero warnings puts a
    /// perfect score in the column for the crates that failed hardest.
    #[test]
    fn a_crate_the_linter_could_not_compile_is_unmeasured_rather_than_clean() {
        let lint = message("warning", Some("clippy::needless_range_loop"), "the loop");
        let broken = message("error", Some("E0425"), "cannot find value `x`");

        let with_error = format!("{lint}\n{broken}\n");
        assert!(
            matches!(
                verdict(Some(101), &with_error).unwrap(),
                Clippy::Unmeasured { .. }
            ),
            "a rustc error means the crate was never linted"
        );

        // Non-vacuity: the SAME fixture without the error line parses and counts, so the verdict
        // above came from the error and not from an unreadable fixture.
        let counts = verdict(Some(0), &format!("{lint}\n")).unwrap();
        assert_eq!(measured(&counts).warnings, 1);
    }

    /// Clippy's `correctness` group is deny-by-default. Reading its `level: "error"` as a failure to
    /// compile would blank the record of every crate that trips one.
    #[test]
    fn a_deny_by_default_lint_is_a_lint_the_crate_earned_not_a_failure_to_compile() {
        let text = format!(
            "{}\n{}\n",
            message("error", Some("clippy::approx_constant"), "3.14 is PI"),
            message("warning", Some("clippy::needless_range_loop"), "the loop"),
        );
        let v = verdict(Some(101), &text).unwrap();
        let counts = measured(&v);
        assert_eq!((counts.errors, counts.warnings), (1, 1));
        assert_eq!(counts.by_lint["clippy::approx_constant"], 1);
    }

    /// cargo closes a run with a codeless `N warnings emitted`, which restates the diagnostics above
    /// it. Counting it adds one phantom lint to every crate in every battery.
    #[test]
    fn the_warnings_emitted_trailer_is_not_counted_as_a_lint() {
        let trailer = message("warning", None, "2 warnings emitted");
        let text = format!(
            "{}\n{}\n{trailer}\n",
            message("warning", Some("clippy::needless_range_loop"), "a"),
            message("warning", Some("clippy::redundant_clone"), "b"),
        );
        assert!(
            text.contains("\"code\":null"),
            "the fixture must really contain the codeless trailer, or this asserts nothing"
        );
        assert_eq!(measured(&verdict(Some(0), &text).unwrap()).warnings, 2);
    }

    /// A parser that returns a clean crate from nothing is a check that passes while seeing nothing.
    #[test]
    fn cargo_that_printed_nothing_is_unmeasured_rather_than_a_clean_crate() {
        for empty in ["", "   \n\n", "not json at all\n"] {
            assert!(
                matches!(verdict(Some(0), empty).unwrap(), Clippy::Unmeasured { .. }),
                "{empty:?} carries no verdict, so it is not a verdict of zero"
            );
        }
        // Non-vacuity: a run that DID emit compiler messages is measured, so the three above were
        // refused for their emptiness and not because nothing is ever measured.
        let artifact = "{\"reason\":\"compiler-artifact\",\"target\":{}}\n";
        assert_eq!(measured(&verdict(Some(0), artifact).unwrap()).warnings, 0);
    }

    /// A missing `cargo` is one fault affecting every case at once. Recorded per case it publishes a
    /// battery of blanks that reads as a measurement nobody made.
    #[test]
    fn an_absent_cargo_refuses_the_score_instead_of_blanking_every_case() {
        assert!(
            verdict(Some(127), "").is_err(),
            "127 is `command not found`, which says nothing about any translation"
        );
        for exit in [Some(124), None] {
            assert!(
                matches!(verdict(exit, "").unwrap(), Clippy::Unmeasured { .. }),
                "{exit:?} leaves this ONE crate unmeasured, and does not refuse the battery"
            );
        }
    }

    /// Without `--no-deps` a crate's count carries every lint its dependencies trip, so a
    /// translation is judged on code no agent wrote. The command line is the only thing excluding
    /// them -- there is no filter downstream.
    #[test]
    fn dependency_lints_are_never_counted_against_the_translation() {
        let argv = clippy_argv(Path::new("/c/Cargo.toml"), Path::new("/c/clippy-target"));
        assert!(argv.iter().any(|a| a == "--no-deps"));
        assert!(argv.iter().any(|a| a == "--message-format=json"));
        assert!(
            !argv.iter().any(|a| a == "--all-targets"),
            "stock clippy checks lib and bins; widening the target set is a lint policy"
        );
    }

    /// The warning count is over the crates that were LINTED. Folding an unmeasured crate in as a
    /// silent zero shrinks nothing visible and makes a battery look cleaner the more it failed.
    #[test]
    fn a_battery_totals_only_the_cases_the_linter_actually_judged() {
        let counts = |warnings, lint: &str| {
            Clippy::Measured(ClippyCounts {
                warnings,
                errors: 0,
                by_lint: BTreeMap::from([(lint.to_string(), warnings)]),
            })
        };
        let verdicts = [
            counts(3, "clippy::needless_range_loop"),
            counts(2, "clippy::needless_range_loop"),
            Clippy::Unmeasured {
                why: "did not compile".into(),
            },
        ];
        let totals = ClippyTotals::of(verdicts.iter());
        assert_eq!(totals.warnings, 5);
        assert_eq!((totals.cases_measured, totals.cases_unmeasured), (2, 1));
        assert_eq!(totals.by_lint["clippy::needless_range_loop"], 5);
    }
}
