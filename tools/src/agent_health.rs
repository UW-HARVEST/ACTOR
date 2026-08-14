//! Did the agent actually run, or did the infrastructure fail?
//!
//! Nothing in the harness asked this before, and it cost us two runs.
//!
//! On 2026-08-14 all seven HarvestBench verify agents died on expired
//! credentials. Scoring ran anyway: projects whose crate would not build were
//! dropped by a bare `continue`, the denominator silently moved from 7 to 5,
//! `harvest-bench: 3/5 projects pass` was printed, and every file in `tables/`
//! was regenerated from it. The same outage hit 43 of 209 Test-Corpus cases
//! earlier the same day, and those were scored too.
//!
//! An infrastructure failure is not a result. A case whose agent never got to
//! run has *no* measurement, and must not be reported as one.
//!
//! # Do not branch on `subtype`
//!
//! The terminal record of a dead run looks like this — note the two fields side
//! by side:
//!
//! ```text
//! "subtype":"success", "is_error":true, "terminal_reason":"api_error",
//! "api_error_status":403,
//! "result":"Failed to authenticate. API Error: 403 The security token ... is expired"
//! ```
//!
//! `subtype` is `"success"` in 214 of 214 real logs, **including every 403**.
//! The discriminator is `terminal_reason`, which was `completed` or `api_error`
//! across every log examined and is 1:1 with `is_error`.
//!
//! # Why the process exit code is not the discriminator
//!
//! `verification.json` already persists `exit_code`, and it happens to be 1:1
//! with `terminal_reason` today — but it cannot stay that way. Since the agent
//! runs under `ulimit -f`/`-d` (see [`crate::workdir`]), a test binary killed by
//! `SIGXFSZ` makes commands inside the session fail without the session itself
//! being an infrastructure failure. That is a *result*. So the exit code is
//! reported as corroborating detail and never classified on.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// How much of the log tail to read. The terminal record is the last line, and
/// its `result` field can carry several KB of final prose.
const TAIL_BYTES: u64 = 16 * 1024;

/// PROOF that an agent invocation ran to completion.
///
/// The unit field is private, so a `Completed` cannot be constructed outside this
/// module: the only way to obtain one is [`Health::completed`], i.e. by passing a
/// real log through [`classify_log`]. Anything that requires `&Completed` therefore
/// cannot be reached for an infra-failed run — that is a compile error rather than
/// a runtime check someone can forget. See `crate::artifact::Scrubbed::seal`.
pub struct Completed(());

impl Completed {
    /// Test-only constructor. `#[cfg(test)]` so production code still cannot forge
    /// a proof, while the crate's own tests can exercise the seal path.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self(())
    }
}

/// Verdict for one agent invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// The agent ran to completion. Says nothing about whether the translation
    /// or verification *succeeded* — that is a result, and results are the
    /// scorer's business, not ours.
    Completed,
    /// The agent did not get to finish for a reason outside the thing being
    /// measured: auth, rate limiting, transport, or a truncated log.
    Infra { reason: String, detail: String },
    /// No evidence either way. Kiro logs are not stream-json, and results
    /// produced before this module existed have no terminal record. Callers
    /// must not treat this as a failure.
    Unknown { why: String },
}

impl Health {
    /// Proof of completion, if this run completed. The ONLY constructor of
    /// [`Completed`].
    pub fn completed(&self) -> Option<Completed> {
        matches!(self, Health::Completed).then_some(Completed(()))
    }

    pub fn is_infra(&self) -> bool {
        matches!(self, Health::Infra { .. })
    }
}

/// Classify one agent log by its terminal record.
pub fn classify_log(log: &Path) -> Health {
    let tail = match read_tail(log) {
        Ok(t) => t,
        Err(e) => {
            return Health::Unknown { why: format!("cannot read {}: {e}", log.display()) };
        }
    };

    // Not stream-json at all (kiro-cli writes prose): no opinion.
    if !tail.contains("\"type\":\"result\"") && !tail.contains("\"terminal_reason\"") {
        if tail.contains("Credits:") {
            return Health::Unknown { why: "kiro-cli log, no terminal record".into() };
        }
        // Stream-json that stops mid-flight. This is the case the operator's
        // resume script had to special-case by hand: a run killed partway leaves
        // a log with a fresh mtime and no terminal record, and skipping on
        // existence alone would score it as if it had finished.
        return Health::Infra {
            reason: "truncated".into(),
            detail: "no terminal record: the agent was killed before finishing".into(),
        };
    }

    if let Some(reason) = last_str(&tail, "terminal_reason") {
        if reason != "completed" {
            let status = last_num(&tail, "api_error_status")
                .map(|s| format!(" (HTTP {s})"))
                .unwrap_or_default();
            return Health::Infra {
                reason: reason.clone(),
                detail: format!("terminal_reason={reason}{status}{}", first_line_of_result(&tail)),
            };
        }
        return Health::Completed;
    }

    // A terminal record without `terminal_reason`: fall back to `is_error`,
    // which was 1:1 with it. Never `subtype`.
    match last_bool(&tail, "is_error") {
        Some(true) => Health::Infra {
            reason: "is_error".into(),
            detail: format!("is_error=true{}", first_line_of_result(&tail)),
        },
        Some(false) => Health::Completed,
        None => Health::Unknown { why: "terminal record has neither terminal_reason nor is_error".into() },
    }
}

/// Corroborating process-level detail from `verification.json` /
/// `translation.json`, for the report only. Deliberately not a classifier: see
/// the module docs on `SIGXFSZ`.
pub fn exit_code(metrics_json: &Path) -> Option<i64> {
    let s = std::fs::read_to_string(metrics_json).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("exit_code")?.as_i64()
}

/// One case's health, for reporting.
#[derive(Debug, Clone)]
pub struct CaseHealth {
    pub name: String,
    pub health: Health,
    pub exit_code: Option<i64>,
    pub log: PathBuf,
}

/// Classify every case under `results_dir` that has a verify or translate log.
///
/// Walks `<results_dir>/**/{verified,translated}/logs/{verify,translation}.log`,
/// preferring the verify log when both exist, since verify is the later phase.
pub fn audit(results_dir: &Path) -> Result<Vec<CaseHealth>> {
    let mut out = Vec::new();
    collect(results_dir, results_dir, &mut out)
        .with_context(|| format!("auditing agent health under {}", results_dir.display()))?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<CaseHealth>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(()) };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        // A case dir is one holding a phase dir with a log we recognise.
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
        let name = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned();
        out.push(CaseHealth {
            name,
            health: classify_log(&log),
            exit_code: exit_code(&metrics),
            log,
        });
    }
    Ok(())
}

/// Human-readable report of the infra failures in an audit, or `None` if clean.
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
            let ec = c.exit_code.map(|e| format!(" exit={e}")).unwrap_or_default();
            s.push_str(&format!("  {}{ec}\n     {detail}\n", c.name));
        }
    }
    if bad.len() > 30 {
        s.push_str(&format!("  ... and {} more\n", bad.len() - 30));
    }
    Some(s)
}

/// Persist the audit's infra failures next to the results, so downstream
/// consumers (and a human reading the tree later) can see which cases have no
/// measurement. Written even when `--allow-infra-failures` lets scoring proceed:
/// the point is that the exclusion is never invisible.
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
    .with_context(|| format!("writing INFRA_FAILURES.json under {}", results_dir.display()))?;
    Ok(())
}

// ── parsing helpers ────────────────────────────────────────────────────────
//
// Deliberately string-scanning rather than serde: the logs are NOT pure JSONL
// (the harness merges the agent's stderr into the same stream via `2>&1 | tee`),
// so a line-by-line `from_str` would fail on the first non-JSON line.

pub(crate) fn read_tail(path: &Path) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn last_str(hay: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = hay.rfind(&pat)? + pat.len();
    let rest = &hay[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn last_bool(hay: &str, key: &str) -> Option<bool> {
    let pat = format!("\"{key}\":");
    let start = hay.rfind(&pat)? + pat.len();
    let rest = hay[start..].trim_start();
    if rest.starts_with("true") { Some(true) }
    else if rest.starts_with("false") { Some(false) }
    else { None }
}

fn last_num(hay: &str, key: &str) -> Option<i64> {
    let pat = format!("\"{key}\":");
    let start = hay.rfind(&pat)? + pat.len();
    let rest = hay[start..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// First ~160 chars of the terminal `result` field, for the operator's benefit.
fn first_line_of_result(tail: &str) -> String {
    match last_str(tail, "result") {
        Some(r) if !r.is_empty() => {
            let one: String = r.chars().take(160).collect();
            format!(": {one}")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim shape of a real 2026-08-14 credential-expiry terminal record.
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
    fn a_credential_expiry_is_infra_despite_subtype_success() {
        // THE bug this module exists for. subtype says "success"; the run is dead.
        let tmp = tempfile::tempdir().unwrap();
        let log = write(tmp.path(), "verify.log", DEAD);
        match classify_log(&log) {
            Health::Infra { reason, detail } => {
                assert_eq!(reason, "api_error");
                assert!(detail.contains("HTTP 403"), "detail should name the status: {detail}");
                assert!(detail.contains("expired"), "detail should carry the message: {detail}");
            }
            other => panic!("expected Infra, got {other:?}"),
        }
    }

    #[test]
    fn subtype_success_alone_never_makes_a_run_healthy() {
        // Guard against a future refactor keying on `subtype`.
        let tmp = tempfile::tempdir().unwrap();
        let log = write(tmp.path(), "v.log", DEAD);
        assert!(classify_log(&log).is_infra());
        assert!(DEAD.contains("\"subtype\":\"success\""), "fixture must retain the trap");
    }

    #[test]
    fn a_completed_run_is_completed() {
        let tmp = tempfile::tempdir().unwrap();
        let log = write(tmp.path(), "v.log", CLEAN);
        assert_eq!(classify_log(&log), Health::Completed);
    }

    #[test]
    fn a_completed_run_that_failed_verification_is_still_completed() {
        // Health is not the verdict. A run that completed and concluded "the
        // translation is wrong" is a RESULT and must reach the scorer.
        let body = CLEAN.replace(
            "Verified. c_src/ was not modified.",
            "Phase B found a divergence; the Rust port returns 0 where C returns -1.",
        );
        let tmp = tempfile::tempdir().unwrap();
        let log = write(tmp.path(), "v.log", &body);
        assert_eq!(classify_log(&log), Health::Completed);
    }

    #[test]
    fn a_truncated_log_is_infra_not_silently_ok() {
        // A killed run leaves a fresh mtime and no terminal record. Skipping on
        // existence alone is what corrupted 4 cases in the 2026-08-13 sweep.
        let tmp = tempfile::tempdir().unwrap();
        let log = write(tmp.path(), "v.log",
            "{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"assistant\"}\n");
        match classify_log(&log) {
            Health::Infra { reason, .. } => assert_eq!(reason, "truncated"),
            other => panic!("expected Infra/truncated, got {other:?}"),
        }
    }

    #[test]
    fn a_kiro_log_is_unknown_not_a_failure() {
        // kiro-cli is not stream-json. Absence of evidence must not block scoring.
        let tmp = tempfile::tempdir().unwrap();
        let log = write(tmp.path(), "v.log", "Credits: 1.25\nTime: 3m 4s\n");
        assert!(matches!(classify_log(&log), Health::Unknown { .. }));
    }

    #[test]
    fn stderr_interleaved_into_the_stream_does_not_break_parsing() {
        // The harness pipes the agent through `2>&1 | tee`, so the log is not
        // pure JSONL. A serde-per-line parser would die on the first such line.
        let body = format!("warning: something on stderr\n{CLEAN}\n");
        let tmp = tempfile::tempdir().unwrap();
        let log = write(tmp.path(), "v.log", &body);
        assert_eq!(classify_log(&log), Health::Completed);
    }

    #[test]
    fn the_last_terminal_record_wins() {
        // Retries can leave earlier records in the stream.
        let body = format!("{DEAD}\n{CLEAN}\n");
        let tmp = tempfile::tempdir().unwrap();
        let log = write(tmp.path(), "v.log", &body);
        assert_eq!(classify_log(&log), Health::Completed, "later record must win");
    }

    #[test]
    fn audit_prefers_the_verify_log_over_the_translate_log() {
        let tmp = tempfile::tempdir().unwrap();
        let case = tmp.path().join("B01/case_x");
        write(&case, "translated/logs/translation.log", CLEAN);
        write(&case, "verified/logs/verify.log", DEAD);
        let a = audit(tmp.path()).unwrap();
        assert_eq!(a.len(), 1, "one case, not one per phase");
        assert_eq!(a[0].name, "B01/case_x");
        assert!(a[0].health.is_infra(), "verify is the later phase and must win");
    }

    #[test]
    fn audit_reads_exit_code_as_corroboration_only() {
        let tmp = tempfile::tempdir().unwrap();
        let case = tmp.path().join("hb/jansson");
        write(&case, "verified/logs/verify.log", DEAD);
        std::fs::write(
            case.join("verified/verification.json"),
            r#"{"exit_code":1,"success":true,"duration_secs":4569}"#,
        ).unwrap();
        let a = audit(tmp.path()).unwrap();
        assert_eq!(a[0].exit_code, Some(1));
        // `success:true` is the cargo-check gate, NOT agent health -- the whole
        // reason exit_code/success could not answer this question on their own.
        assert!(a[0].health.is_infra());
    }

    #[test]
    fn describe_is_none_when_everything_completed() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("b/c1"), "verified/logs/verify.log", CLEAN);
        let a = audit(tmp.path()).unwrap();
        assert!(describe_infra_failures(&a).is_none());
    }

    #[test]
    fn describe_names_the_cases_and_the_counts() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("b/good"), "verified/logs/verify.log", CLEAN);
        write(&tmp.path().join("b/bad"), "verified/logs/verify.log", DEAD);
        let a = audit(tmp.path()).unwrap();
        let msg = describe_infra_failures(&a).expect("one failure");
        assert!(msg.contains("1 of 2"), "counts: {msg}");
        assert!(msg.contains("b/bad"), "names the case: {msg}");
        assert!(!msg.contains("b/good"), "does not list healthy cases: {msg}");
    }
}
