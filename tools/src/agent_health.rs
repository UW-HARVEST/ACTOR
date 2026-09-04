//! Reading an agent transcript, and auditing a finished results tree with nothing but what
//! the run left behind: the transcript, and the exit it recorded beside it.
//!
//! The verdict itself belongs to [`crate::domain::health`], which is handed the text: a
//! classifier that opened the file could not be tested without one on disk.

use crate::domain::health::{classify, Exit, Health, LogFormat};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The terminal record is the last line, and its `result` field can carry several
/// KB of final prose.
const TAIL_BYTES: u64 = 16 * 1024;

/// Classify a run after the fact, from its transcript and the exit it recorded.
///
/// `format` is a parameter and never assumed: [`crate::runners::log_format`] is the ONE
/// table, and hardcoding [`LogFormat::StreamJson`] here read every prose or docker
/// transcript as a stream-json log missing its terminal record — `Infra { "truncated" }` for
/// a run that was perfectly healthy, which is the misclassification [`LogFormat`] exists to
/// prevent.
///
/// `exit` for the mirror-image reason: nothing in an opaque log tells a finished run from a
/// killed one, so [`classify`] gives the exit the whole burden of proof, and a hardcoded
/// [`Exit::Unobserved`] left such a backend only `Unknown` — a gate that cannot fire.
/// What the run cost, read from the transcript.
///
/// `None` where the format carries no spend: kiro writes prose, so there is nothing to read, and a
/// `0.0` there would be a measurement nobody made -- the shape that put `\ActorKiroTests{0/338}` in
/// print against records saying 325/338.
pub fn cost_usd(log: &Path, format: LogFormat) -> Option<f64> {
    if format == LogFormat::Opaque {
        return None;
    }
    let text = read_tail(log).ok()?;
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|r| r["total_cost_usd"].as_f64())
}

pub fn classify_log(log: &Path, format: LogFormat, exit: Exit) -> Health {
    match read_tail(log) {
        Ok(tail) => classify(&tail, format, exit),
        Err(e) => Health::Unknown {
            why: format!("cannot read {}: {e}", log.display()),
        },
    }
}

#[derive(Debug, Clone)]
pub struct CaseHealth {
    pub name: String,
    pub health: Health,
    pub log: PathBuf,
}

pub struct Run {
    pub name: String,
    pub case_dir: PathBuf,
}

/// Classify each run in the ROSTER from its TRANSCRIPT, preferring the verify log where both exist. A
/// roster and not a root to recurse from: `audit(paths.results_dir, ..)` refused B01_synthetic with all
/// 85 cases fresh, for 27 dead runs in other batteries. One `format`, because a roster is agent-scoped.
///
/// [`Exit::Unobserved`], stated once and here: this runs AFTER the process is gone, so there is no exit
/// to observe. It used to read `<phase>/{verification,translation}.json` for one -- files nothing in
/// this crate has ever written (`find results -name verification.json` returns 0), so every call
/// already passed `Unobserved` while two functions and a `CaseHealth.exit_code` field maintained the
/// appearance of a second source of evidence. The authoritative record is the store's
/// `AgentRecord.outcome`, classified at run time WITH the observed exit; this gate is a backstop over
/// published transcripts and says so.
pub fn audit(runs: &[Run], format: LogFormat) -> Vec<CaseHealth> {
    let mut out = Vec::new();
    for run in runs {
        let verify = run.case_dir.join("verified/logs/verify.log");
        let translate = run.case_dir.join("translated/logs/translation.log");
        let log = if verify.is_file() {
            verify
        } else if translate.is_file() {
            translate
        } else {
            continue;
        };
        out.push(CaseHealth {
            name: run.name.clone(),
            health: classify_log(&log, format, Exit::Unobserved),
            log,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OnInfraFailure {
    Refuse,
    ScoreAnyway,
}

impl OnInfraFailure {
    pub fn from_allow_infra_failures_flag(flag: bool) -> Self {
        if flag {
            Self::ScoreAnyway
        } else {
            Self::Refuse
        }
    }
}

/// The refusal itself is kept: an infrastructure failure is not a result. Only its scope changed.
pub struct Gate<'a> {
    pub format: LogFormat,
    pub on_failure: OnInfraFailure,
    pub results_dir: &'a Path,
}

impl Gate<'_> {
    pub fn grade(&self, runs: &[Run]) -> Result<()> {
        let audit = audit(runs, self.format);
        let Some(report) = describe_infra_failures(&audit) else {
            return Ok(());
        };
        record_infra_failures(self.results_dir, &audit)?;
        anyhow::ensure!(
            self.on_failure == OnInfraFailure::ScoreAnyway,
            "{report}\n\
             Refusing to score. An infrastructure failure is not a result.\n\
             Re-run those cases after fixing the cause: a dead run stores no cache entry, \
             so `verify <target>` re-runs it, and `--force` is needed only under \
             `--cache off`. Or pass --allow-infra-failures to score anyway.\n\
             Details written to {}/INFRA_FAILURES.json",
            self.results_dir.display()
        );
        eprintln!(
            "⚠️  --allow-infra-failures: scoring despite dead agent runs.\n{report}\
             These cases have no measurement; treat any number derived from them as unsupported."
        );
        Ok(())
    }
}

pub fn describe_infra_failures(audit: &[CaseHealth]) -> Option<String> {
    let bad: Vec<&CaseHealth> = audit.iter().filter(|c| c.health.is_infra()).collect();
    if bad.is_empty() {
        return None;
    }
    let mut s = format!(
        "{} of {} agent runs did not complete for infrastructure reasons.\n\
         These have NO measurement and must not be scored as results:\n",
        bad.len(),
        audit.len()
    );
    for c in bad.iter().take(30) {
        if let Health::Infra { detail, .. } = &c.health {
            s.push_str(&format!("  {}\n     {detail}\n", c.name));
        }
    }
    if bad.len() > 30 {
        s.push_str(&format!("  ... and {} more\n", bad.len() - 30));
    }
    Some(s)
}

/// Written even when `--allow-infra-failures` lets scoring proceed, so that the
/// cases with no measurement are never invisible to a later reader.
pub fn record_infra_failures(results_dir: &Path, audit: &[CaseHealth]) -> Result<()> {
    let bad: Vec<serde_json::Value> = audit
        .iter()
        .filter(|c| c.health.is_infra())
        .map(|c| {
            let (reason, detail) = match &c.health {
                Health::Infra { reason, detail } => (reason.clone(), detail.clone()),
                _ => unreachable!("filtered to Infra"),
            };
            serde_json::json!({
                "case": c.name,
                "reason": reason,
                "detail": detail,
                "log": c.log.to_string_lossy(),
            })
        })
        .collect();
    let doc = serde_json::json!({
        "generated": chrono::Utc::now().to_rfc3339(),
        "runs_audited": audit.len(),
        "infra_failures": bad.len(),
        "cases": bad,
    });
    std::fs::create_dir_all(results_dir)?;
    std::fs::write(
        results_dir.join("INFRA_FAILURES.json"),
        serde_json::to_string_pretty(&doc)? + "\n",
    )
    .with_context(|| {
        format!(
            "writing INFRA_FAILURES.json under {}",
            results_dir.display()
        )
    })?;
    Ok(())
}

/// The one read of an agent transcript. Tail-only: these reach 10+ MB, and both the
/// terminal record and kiro's `Credits:` line sit at the end.
pub(crate) fn read_tail(path: &Path) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim shape of a real credential-expiry terminal record.
    const DEAD: &str = r#"{"type":"system","subtype":"api_retry","attempt":4,"error_status":403}
{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error","api_error_status":403,"duration_ms":4569000,"num_turns":12,"result":"Failed to authenticate. API Error: 403 The security token included in the request is expired","session_id":"abc"}"#;

    const CLEAN: &str = r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","duration_ms":573254,"num_turns":88,"total_cost_usd":4.2,"result":"Verified. c_src/ was not modified.","session_id":"def"}"#;

    /// A healthy laertes/c2saferrust transcript: docker output, no terminal record anywhere.
    const OPAQUE: &str = "Unable to find image locally\n=== OPENROUTER REQUEST ===\n\
                          Finished `release` profile [optimized] target(s) in 41.02s\n";

    /// A kiro transcript cut off mid-flight: prose, and no `Credits:` line because the run
    /// never reached the one it prints on the way out.
    const KIRO_KILLED: &str = "> I'll start by reading c_src/parson.c\n\
                               > Writing src/lib.rs\n";

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    }

    fn roster(root: &Path, names: &[&str]) -> Vec<Run> {
        names
            .iter()
            .map(|n| Run {
                name: (*n).to_string(),
                case_dir: root.join(n),
            })
            .collect()
    }

    #[test]
    fn audit_prefers_the_verify_log_over_the_translate_log() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("B01/case_x");
        write(&case, "translated/logs/translation.log", CLEAN);
        write(&case, "verified/logs/verify.log", DEAD);
        let a = audit(&roster(tmp.path(), &["B01/case_x"]), LogFormat::StreamJson);
        assert_eq!(a.len(), 1, "one case, not one per phase");
        assert_eq!(a[0].name, "B01/case_x");
        assert!(
            a[0].health.is_infra(),
            "verify is the later phase and must win"
        );
    }

    /// Making translate's four log paths one function of the phase put the prose and docker
    /// transcripts under `translated/logs/` where the audit finds them. Reading them as
    /// stream-json calls every healthy laertes/c2saferrust/kimi/oneshot run
    /// `Infra { reason: "truncated" }`, and `run_test` then refuses to score the backend.
    #[test]
    fn an_opaque_backend_is_not_audited_as_a_truncated_stream_json_log() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("B01/case_x");
        write(&case, "translated/logs/translation.log", OPAQUE);
        std::fs::write(
            case.join("translated/translation.json"),
            r#"{"exit_code":0,"success":true,"timed_out":false}"#,
        )
        .unwrap();

        let a = audit(&roster(tmp.path(), &["B01/case_x"]), LogFormat::Opaque);
        assert_eq!(
            a.len(),
            1,
            "the transcript has to be found to be classified"
        );
        assert!(
            !a[0].health.is_infra(),
            "a completed docker run has a measurement: {:?}",
            a[0].health
        );
        assert!(describe_infra_failures(&a).is_none());

        let as_stream_json = audit(&roster(tmp.path(), &["B01/case_x"]), LogFormat::StreamJson);
        assert!(
            as_stream_json[0].health.is_infra(),
            "fixture must trip the hardcoded classifier, or this proves nothing"
        );
    }

    /// A wall-clock kill is the infra failure an opaque transcript cannot show, so the exit the
    /// run recorded is the only evidence there is. Auditing the seven opaque backends with
    /// `Exit::Unobserved` made `Unknown` their only possible verdict, and both consumers of the
    /// gate filter on `is_infra` — so it could not fire for them at all, no INFRA_FAILURES.json
    /// was written, and a run killed at three hours was scored as a result.
    /// A wall-clock kill is the SAME outcome whichever backend it happened to.
    ///
    /// `Exit::Exhausted` used to be consumed only by the `Opaque` arm, because `classify_stream_json`
    /// and `classify_codex_json` never received `exit` -- so one `timeout` kill of a session that was
    /// still writing was `Exhausted` for kiro and `Infra { "truncated" }` for claude and codex.
    /// `Infra` is transient, so the identical event was retried three times for two backends and
    /// recorded as a terminal answer for the third. Which backend it was cannot be what decides.
    ///
    /// This replaces a test that hand-wrote `<phase>/translation.json` fixtures. Nothing in this crate
    /// has ever written that file -- `find results -name translation.json` returns 0 -- so it proved
    /// only that the classifier reads a shape no run produces. `audit` now says in one place that it
    /// has no exit to observe, and the authoritative record is the store's `AgentRecord.outcome`,
    /// classified here at run time WITH the exit.
    #[test]
    fn a_wall_clock_kill_is_exhausted_for_every_backend_not_just_the_opaque_one() {
        for format in [
            LogFormat::StreamJson,
            LogFormat::CodexJson,
            LogFormat::Opaque,
        ] {
            assert!(
                matches!(
                    crate::domain::health::classify(KIRO_KILLED, format, Exit::Exhausted),
                    Health::Exhausted { .. }
                ),
                "{format:?} calls an exhausted session something else"
            );
        }
        // Non-vacuity: without the observed exit, a truncated transcript is still infra for the two
        // JSON formats, so the assertion above is about the EXIT and not about the text.
        assert!(
            crate::domain::health::classify(KIRO_KILLED, LogFormat::StreamJson, Exit::Unobserved)
                .is_infra(),
            "an unobserved truncated stream-json log must still be infra"
        );
    }

    #[test]
    fn a_dead_run_in_a_battery_nobody_is_scoring_does_not_block_the_one_being_scored() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        write(
            &tmp.path().join("B01/fresh"),
            "verified/logs/verify.log",
            CLEAN,
        );
        write(
            &tmp.path().join("B02/dead"),
            "verified/logs/verify.log",
            DEAD,
        );
        let gate = Gate {
            format: LogFormat::StreamJson,
            on_failure: OnInfraFailure::Refuse,
            results_dir: tmp.path(),
        };

        gate.grade(&roster(tmp.path(), &["B01/fresh"]))
            .expect("a clean battery must score even with a dead run elsewhere under the root");

        let err = gate
            .grade(&roster(tmp.path(), &["B01/fresh", "B02/dead"]))
            .expect_err("the refusal must still fire when the dead run IS in the roster");
        let text = format!("{err:#}");
        assert!(text.contains("B02/dead"), "and must name it: {text}");
        assert!(text.contains("1 of 2"), "counting only the roster: {text}");
    }

    #[test]
    fn describe_is_none_when_everything_completed() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        write(&tmp.path().join("b/c1"), "verified/logs/verify.log", CLEAN);
        let a = audit(&roster(tmp.path(), &["b/c1"]), LogFormat::StreamJson);
        assert!(describe_infra_failures(&a).is_none());
    }

    #[test]
    fn describe_names_the_cases_and_the_counts() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        write(
            &tmp.path().join("b/good"),
            "verified/logs/verify.log",
            CLEAN,
        );
        write(&tmp.path().join("b/bad"), "verified/logs/verify.log", DEAD);
        let a = audit(
            &roster(tmp.path(), &["b/good", "b/bad"]),
            LogFormat::StreamJson,
        );
        let msg = describe_infra_failures(&a).expect("one failure");
        assert!(msg.contains("1 of 2"), "counts: {msg}");
        assert!(msg.contains("b/bad"), "names the case: {msg}");
        assert!(
            !msg.contains("b/good"),
            "does not list healthy cases: {msg}"
        );
    }
}
