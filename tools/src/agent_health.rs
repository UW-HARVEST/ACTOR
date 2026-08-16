//! Reading an agent transcript, and auditing a finished results tree with nothing but
//! the transcripts it left behind.
//!
//! The verdict itself belongs to [`crate::domain::health`], which is handed the text: a
//! classifier that opened the file could not be tested without one on disk.

use crate::domain::health::{classify, Exit, Health, LogFormat};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The terminal record is the last line, and its `result` field can carry several
/// KB of final prose.
const TAIL_BYTES: u64 = 16 * 1024;

/// Classify from the log alone, with no live observation.
///
/// Used by [`audit`] over a finished results tree. It cannot mint a
/// [`crate::domain::health::Completed`] for an opaque log, because nothing in such a log
/// distinguishes a finished run from a killed one — and it has no observed exit status to
/// go on, which is exactly what [`Exit::Unobserved`] states.
pub fn classify_log(log: &Path) -> Health {
    match read_tail(log) {
        Ok(tail) => classify(&tail, LogFormat::StreamJson, Exit::Unobserved),
        Err(e) => Health::Unknown {
            why: format!("cannot read {}: {e}", log.display()),
        },
    }
}

/// Report-only detail. Deliberately not a classifier: see the docs on
/// [`crate::domain::health`] and `SIGXFSZ`.
pub fn exit_code(metrics_json: &Path) -> Option<i64> {
    let s = std::fs::read_to_string(metrics_json).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("exit_code")?.as_i64()
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
pub fn audit(results_dir: &Path) -> Result<Vec<CaseHealth>> {
    let mut out = Vec::new();
    collect(results_dir, results_dir, &mut out)
        .with_context(|| format!("auditing agent health under {}", results_dir.display()))?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<CaseHealth>) -> Result<()> {
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
            collect(root, &p, out)?;
            continue;
        };
        let name = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .into_owned();
        out.push(CaseHealth {
            name,
            health: classify_log(&log),
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
        let a = audit(tmp.path()).unwrap();
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
        let a = audit(tmp.path()).unwrap();
        assert_eq!(a[0].exit_code, Some(1));
        // `success:true` here is the cargo-check gate, NOT agent health.
        assert!(a[0].health.is_infra());
    }

    #[test]
    fn describe_is_none_when_everything_completed() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        write(&tmp.path().join("b/c1"), "verified/logs/verify.log", CLEAN);
        let a = audit(tmp.path()).unwrap();
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
        let a = audit(tmp.path()).unwrap();
        let msg = describe_infra_failures(&a).expect("one failure");
        assert!(msg.contains("1 of 2"), "counts: {msg}");
        assert!(msg.contains("b/bad"), "names the case: {msg}");
        assert!(
            !msg.contains("b/good"),
            "does not list healthy cases: {msg}"
        );
    }
}
