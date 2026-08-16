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
/// `format` is a parameter and never assumed: [`crate::cli::Agent::log_format`] is the ONE
/// table, and hardcoding [`LogFormat::StreamJson`] here read every prose or docker
/// transcript as a stream-json log missing its terminal record — `Infra { "truncated" }` for
/// a run that was perfectly healthy, which is the misclassification [`LogFormat`] exists to
/// prevent.
///
/// `exit` for the mirror-image reason: nothing in an opaque log tells a finished run from a
/// killed one, so [`classify`] gives the exit the whole burden of proof, and a hardcoded
/// [`Exit::Unobserved`] left such a backend only `Unknown` — a gate that cannot fire.
pub fn classify_log(log: &Path, format: LogFormat, exit: Exit) -> Health {
    match read_tail(log) {
        Ok(tail) => classify(&tail, format, exit),
        Err(e) => Health::Unknown {
            why: format!("cannot read {}: {e}", log.display()),
        },
    }
}

fn read_metrics(metrics_json: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(metrics_json).ok()?).ok()
}

/// Report-only detail. Deliberately not a classifier: see the docs on
/// [`crate::domain::health`] and `SIGXFSZ`.
pub fn exit_code(metrics_json: &Path) -> Option<i64> {
    read_metrics(metrics_json)?.get("exit_code")?.as_i64()
}

/// How the agent process ended, as the run itself recorded it. The audit watched nothing, but
/// the harness did: `agents::exit::merge_agent_exit` wrote what it saw beside the transcript,
/// and reading it back is the only sight the gate has for a [`LogFormat::Opaque`] backend — a
/// wall-clock-killed run is `Infra { "timeout" }` and not a result. No record stays
/// [`Exit::Unobserved`], which is not a failure: the backends that never call
/// `record_agent_exit` record nothing, and a verdict invented for them is the gate reporting
/// what nobody observed.
pub fn recorded_exit(metrics_json: &Path) -> Exit {
    let Some(metrics) = read_metrics(metrics_json) else {
        return Exit::Unobserved;
    };
    // `merge_agent_exit` writes both keys or neither, and a null `exit_code` is a real
    // observation of a signal kill — so presence, not parseability, says it was watched.
    let Some(code) = metrics.get("exit_code") else {
        return Exit::Unobserved;
    };
    let code = code.as_i64();
    let timed_out = metrics.get("timed_out").and_then(|t| t.as_bool());
    // `timeout` exits 124; records written before `timed_out` existed carry only the code.
    if timed_out == Some(true) || code == Some(124) {
        return Exit::Timeout;
    }
    match code {
        Some(0) => Exit::Success,
        Some(c) => Exit::Failure {
            code: i32::try_from(c).ok(),
        },
        None => Exit::Failure { code: None },
    }
}

#[derive(Debug, Clone)]
pub struct CaseHealth {
    pub name: String,
    pub health: Health,
    pub exit_code: Option<i64>,
    pub log: PathBuf,
}

/// Classify every case under `results_dir` that has a verify or translate log,
/// preferring the verify log when both exist, since verify is the later phase.
///
/// One `format` for the whole tree because `results_dir` is agent-scoped —
/// `results/<Dataset>/<agent_key>`, see [`crate::battery::Paths::new`] — so every transcript
/// beneath it came from the one backend the caller named.
pub fn audit(results_dir: &Path, format: LogFormat) -> Result<Vec<CaseHealth>> {
    let mut out = Vec::new();
    collect(results_dir, results_dir, format, &mut out)
        .with_context(|| format!("auditing agent health under {}", results_dir.display()))?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn collect(root: &Path, dir: &Path, format: LogFormat, out: &mut Vec<CaseHealth>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let verify = p.join("verified/logs/verify.log");
        let translate = p.join("translated/logs/translation.log");
        let (log, metrics) = if verify.is_file() {
            (verify, p.join("verified/verification.json"))
        } else if translate.is_file() {
            (translate, p.join("translated/translation.json"))
        } else {
            collect(root, &p, format, out)?;
            continue;
        };
        let name = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .into_owned();
        out.push(CaseHealth {
            name,
            health: classify_log(&log, format, recorded_exit(&metrics)),
            exit_code: exit_code(&metrics),
            log,
        });
    }
    Ok(())
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
            let ec = c
                .exit_code
                .map(|e| format!(" exit={e}"))
                .unwrap_or_default();
            s.push_str(&format!("  {}{ec}\n     {detail}\n", c.name));
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
                "exit_code": c.exit_code,
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

    #[test]
    fn audit_prefers_the_verify_log_over_the_translate_log() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("B01/case_x");
        write(&case, "translated/logs/translation.log", CLEAN);
        write(&case, "verified/logs/verify.log", DEAD);
        let a = audit(tmp.path(), LogFormat::StreamJson).unwrap();
        assert_eq!(a.len(), 1, "one case, not one per phase");
        assert_eq!(a[0].name, "B01/case_x");
        assert!(
            a[0].health.is_infra(),
            "verify is the later phase and must win"
        );
    }

    #[test]
    fn audit_reads_exit_code_as_corroboration_only() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("hb/jansson");
        write(&case, "verified/logs/verify.log", DEAD);
        std::fs::write(
            case.join("verified/verification.json"),
            r#"{"exit_code":1,"success":true,"duration_secs":4569}"#,
        )
        .unwrap();
        let a = audit(tmp.path(), LogFormat::StreamJson).unwrap();
        assert_eq!(a[0].exit_code, Some(1));
        // `success:true` here is the cargo-check gate, NOT agent health.
        assert!(a[0].health.is_infra());
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

        let a = audit(tmp.path(), LogFormat::Opaque).unwrap();
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

        let as_stream_json = audit(tmp.path(), LogFormat::StreamJson).unwrap();
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
    #[test]
    fn a_wall_clock_killed_opaque_run_is_an_infra_failure() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        // One transcript shape for all three cases, so only the recorded exit can tell them
        // apart — which is the whole claim under test.
        for case in ["HB/jansson", "HB/mujs", "HB/zlib"] {
            write(
                &tmp.path().join(case),
                "translated/logs/translation.log",
                KIRO_KILLED,
            );
        }
        for (case, record) in [
            // Killed at the wall clock: `timeout` exits 124.
            (
                "HB/jansson",
                r#"{"exit_code":124,"timed_out":true,"success":false,"duration_secs":10800}"#,
            ),
            (
                "HB/mujs",
                r#"{"exit_code":0,"timed_out":false,"success":true,"duration_secs":912}"#,
            ),
            // A backend that never calls `record_agent_exit` records no exit to read.
            ("HB/zlib", r#"{"success":false,"duration_secs":10800}"#),
        ] {
            std::fs::write(
                tmp.path().join(case).join("translated/translation.json"),
                record,
            )
            .unwrap();
        }

        let a = audit(tmp.path(), LogFormat::Opaque).unwrap();
        let health = |name: &str| {
            a.iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("{name} not audited"))
                .health
                .clone()
        };
        match health("HB/jansson") {
            Health::Infra { reason, detail } => {
                assert_eq!(reason, "timeout");
                assert!(detail.contains("wall clock"), "{detail}");
            }
            other => panic!("a run killed at the wall clock has no measurement: {other:?}"),
        }
        assert!(
            !health("HB/mujs").is_infra(),
            "a run watched to a clean exit has a measurement: {:?}",
            health("HB/mujs")
        );
        assert!(
            !health("HB/zlib").is_infra(),
            "an unrecorded exit is no evidence of failure: {:?}",
            health("HB/zlib")
        );

        let msg = describe_infra_failures(&a).expect("the gate must fire for an opaque backend");
        assert!(msg.contains("1 of 3"), "counts: {msg}");
        assert!(msg.contains("HB/jansson"), "names the case: {msg}");
        record_infra_failures(tmp.path(), &a).unwrap();
        let doc: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("INFRA_FAILURES.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(doc["infra_failures"], serde_json::json!(1));
        assert_eq!(doc["cases"][0]["case"], serde_json::json!("HB/jansson"));
        assert_eq!(doc["cases"][0]["exit_code"], serde_json::json!(124));

        // The fixture is the transcript the hardcoded stream-json reading called
        // `Infra { "truncated" }`: the killed run is still blocked — now on the evidence rather
        // than on a misread — and its two healthy neighbours no longer with it.
        let as_stream_json = audit(tmp.path(), LogFormat::StreamJson).unwrap();
        assert!(
            as_stream_json.iter().all(|c| c.health.is_infra()),
            "fixture must trip the hardcoded classifier, or this proves nothing: {as_stream_json:?}"
        );
    }

    #[test]
    fn describe_is_none_when_everything_completed() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        write(&tmp.path().join("b/c1"), "verified/logs/verify.log", CLEAN);
        let a = audit(tmp.path(), LogFormat::StreamJson).unwrap();
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
        let a = audit(tmp.path(), LogFormat::StreamJson).unwrap();
        let msg = describe_infra_failures(&a).expect("one failure");
        assert!(msg.contains("1 of 2"), "counts: {msg}");
        assert!(msg.contains("b/bad"), "names the case: {msg}");
        assert!(
            !msg.contains("b/good"),
            "does not list healthy cases: {msg}"
        );
    }
}
