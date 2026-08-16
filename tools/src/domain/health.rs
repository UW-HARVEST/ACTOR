//! Did the agent actually run, or did the infrastructure fail?
//!
//! A case whose agent never got to run has *no* measurement and must not be
//! scored as one: a credential outage once killed all seven HarvestBench verify
//! agents, and unbuildable crates were dropped by a bare `continue` so the
//! denominator silently went 7 → 5 and `3/5 projects pass` reached `tables/`.
//!
//! Do **not** branch on `subtype`: it reads `"success"` in 214 of 214 real logs,
//! including every 403. The discriminator is `terminal_reason` (`completed` or
//! `api_error` in every log examined, 1:1 with `is_error`), which sits in the same
//! record as `"subtype":"success","is_error":true`.
//!
//! The process exit code is not a discriminator either, though it is 1:1 with
//! `terminal_reason` today: the agent runs under `ulimit -f`/`-d` (see
//! [`crate::io::workdir`]), so a test binary killed by `SIGXFSZ` fails commands inside
//! a session that is itself fine — that is a *result*. Exit codes are therefore
//! reported as corroborating detail only.

/// What a backend's log can prove about completion ON ITS OWN.
///
/// Named, because the two kinds make the *same* observation mean opposite things: a
/// missing terminal record is a truncated run for a stream-json backend and no
/// evidence at all for a prose one. Conflating them is why 9 of 16 translate
/// backends could not be sealed — [`crate::agent_health::classify_log`] called a
/// perfectly healthy c2rust or docker log `Infra { reason: "truncated" }`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// stream-json carrying a terminal record. Its ABSENCE means truncation.
    StreamJson,
    /// Prose (kiro, kimi, oneshot) or tool output (c2rust, laertes, c2saferrust).
    /// Proves nothing about completion, so the exit status is the only evidence.
    Opaque,
}

/// The harness's LIVE observation of how the agent process ended.
///
/// [`Exit::Unobserved`] is deliberately distinct from [`Exit::Failure`]: it means
/// nobody watched, not that anything went wrong. An after-the-fact audit of a
/// results tree has no observation, and must not manufacture one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    Success,
    Failure {
        code: Option<i32>,
    },
    /// `timeout` killed the child — it reports 124.
    Timeout,
    Unobserved,
}

/// PROOF that an agent invocation ran to completion.
///
/// The private unit field makes [`Health::completed`] the only way to obtain one,
/// so code requiring `&Completed` is unreachable for an infra-failed run as a
/// compile error, not a forgettable runtime check. See
/// `crate::artifact::Scrubbed::seal`.
pub struct Completed(());

impl Completed {
    /// `#[cfg(test)]` so production code still cannot forge a proof.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Ran to completion. Says nothing about whether the translation or
    /// verification *succeeded* — that is a result, the scorer's business.
    Completed,
    /// Did not finish for a reason outside the thing being measured: auth, rate
    /// limiting, transport, or a truncated log.
    Infra { reason: String, detail: String },
    /// No evidence either way (kiro logs are not stream-json; results predating
    /// this module have no terminal record). Not a failure.
    Unknown { why: String },
}

impl Health {
    /// The ONLY constructor of [`Completed`].
    pub fn completed(&self) -> Option<Completed> {
        matches!(self, Health::Completed).then_some(Completed(()))
    }

    pub fn is_infra(&self) -> bool {
        matches!(self, Health::Infra { .. })
    }
}

/// Classify a run from its transcript and how the process ended.
///
/// Takes the text, never the path: the caller knows which backend it launched, how the
/// process ended, and where the transcript is, so all three are its business and the
/// decision here needs no fixture on disk. [`crate::agent_health::classify_log`] is the
/// after-the-fact counterpart for auditing a results tree, where no observation is
/// available.
///
/// For [`LogFormat::StreamJson`] the terminal record is authoritative and `exit` is
/// deliberately ignored — see the module docs on `SIGXFSZ`: the agent runs under
/// `ulimit -f`/`-d`, so a test binary killed by a signal fails commands inside a
/// session that is itself fine, and that is a *result*.
pub fn classify(text: &str, format: LogFormat, exit: Exit) -> Health {
    match format {
        LogFormat::StreamJson => classify_stream_json(text),
        // An opaque log cannot distinguish "finished" from "killed", so the exit
        // status carries the whole burden of proof.
        LogFormat::Opaque => match exit {
            Exit::Success => Health::Completed,
            // The run was cut off: there is no measurement, exactly as a truncated
            // stream-json log has none.
            Exit::Timeout => Health::Infra {
                reason: "timeout".into(),
                detail: "the agent was killed at the wall clock".into(),
            },
            // The tool ran and failed. That is a RESULT, not an infra failure, and
            // must stay in the denominator — treating it as infra is how a project
            // silently leaves the denominator and inflates the score. There is also
            // nothing to seal, so no proof is needed.
            Exit::Failure { code } => Health::Unknown {
                why: format!(
                    "opaque log, agent exited {}",
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "by signal".into())
                ),
            },
            Exit::Unobserved => Health::Unknown {
                why: "opaque log and no observed exit status".into(),
            },
        },
    }
}

fn classify_stream_json(tail: &str) -> Health {
    // kiro-cli writes prose, not stream-json: no opinion.
    if !tail.contains("\"type\":\"result\"") && !tail.contains("\"terminal_reason\"") {
        if tail.contains("Credits:") {
            return Health::Unknown {
                why: "kiro-cli log, no terminal record".into(),
            };
        }
        // Stream-json that stops mid-flight: a run killed partway leaves a log
        // with a fresh mtime and no terminal record, so skipping on existence
        // alone would score it as if it had finished.
        return Health::Infra {
            reason: "truncated".into(),
            detail: "no terminal record: the agent was killed before finishing".into(),
        };
    }

    if let Some(reason) = last_str(tail, "terminal_reason") {
        if reason != "completed" {
            let status = last_num(tail, "api_error_status")
                .map(|s| format!(" (HTTP {s})"))
                .unwrap_or_default();
            return Health::Infra {
                reason: reason.clone(),
                detail: format!(
                    "terminal_reason={reason}{status}{}",
                    first_line_of_result(tail)
                ),
            };
        }
        return Health::Completed;
    }

    // Terminal record without `terminal_reason`: fall back to `is_error`, which
    // was 1:1 with it. Never `subtype`.
    match last_bool(tail, "is_error") {
        Some(true) => Health::Infra {
            reason: "is_error".into(),
            detail: format!("is_error=true{}", first_line_of_result(tail)),
        },
        Some(false) => Health::Completed,
        None => Health::Unknown {
            why: "terminal record has neither terminal_reason nor is_error".into(),
        },
    }
}

// String-scanning rather than serde: the harness merges the agent's stderr into
// the stream via `2>&1 | tee`, so the logs are not pure JSONL and a line-by-line
// `from_str` would fail on the first non-JSON line.

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
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn last_num(hay: &str, key: &str) -> Option<i64> {
    let pat = format!("\"{key}\":");
    let start = hay.rfind(&pat)? + pat.len();
    let rest = hay[start..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

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

    /// Verbatim shape of a real credential-expiry terminal record.
    const DEAD: &str = r#"{"type":"system","subtype":"api_retry","attempt":4,"error_status":403}
{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error","api_error_status":403,"duration_ms":4569000,"num_turns":12,"result":"Failed to authenticate. API Error: 403 The security token included in the request is expired","session_id":"abc"}"#;

    const CLEAN: &str = r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","duration_ms":573254,"num_turns":88,"total_cost_usd":4.2,"result":"Verified. c_src/ was not modified.","session_id":"def"}"#;

    /// What the audit sees: a transcript and nothing else.
    fn from_log(text: &str) -> Health {
        classify(text, LogFormat::StreamJson, Exit::Unobserved)
    }

    #[test]
    fn a_credential_expiry_is_infra_despite_subtype_success() {
        // subtype says "success"; the run is dead.
        match from_log(DEAD) {
            Health::Infra { reason, detail } => {
                assert_eq!(reason, "api_error");
                assert!(
                    detail.contains("HTTP 403"),
                    "detail should name the status: {detail}"
                );
                assert!(
                    detail.contains("expired"),
                    "detail should carry the message: {detail}"
                );
            }
            other => panic!("expected Infra, got {other:?}"),
        }
    }

    #[test]
    fn subtype_success_alone_never_makes_a_run_healthy() {
        // Guard against a future refactor keying on `subtype`.
        assert!(from_log(DEAD).is_infra());
        assert!(
            DEAD.contains("\"subtype\":\"success\""),
            "fixture must retain the trap"
        );
    }

    #[test]
    fn a_completed_run_is_completed() {
        assert_eq!(from_log(CLEAN), Health::Completed);
    }

    #[test]
    fn a_completed_run_that_failed_verification_is_still_completed() {
        // Concluding "the translation is wrong" is a RESULT: it must be scored.
        let body = CLEAN.replace(
            "Verified. c_src/ was not modified.",
            "Phase B found a divergence; the Rust port returns 0 where C returns -1.",
        );
        assert_eq!(from_log(&body), Health::Completed);
    }

    #[test]
    fn a_truncated_log_is_infra_not_silently_ok() {
        // A killed run leaves a fresh mtime and no terminal record; skipping on
        // existence alone corrupted 4 cases of a real sweep.
        match from_log("{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"assistant\"}\n") {
            Health::Infra { reason, .. } => assert_eq!(reason, "truncated"),
            other => panic!("expected Infra/truncated, got {other:?}"),
        }
    }

    #[test]
    fn a_kiro_log_is_unknown_not_a_failure() {
        // kiro-cli is not stream-json. Absence of evidence must not block scoring.
        assert!(matches!(
            from_log("Credits: 1.25\nTime: 3m 4s\n"),
            Health::Unknown { .. }
        ));
    }

    #[test]
    fn stderr_interleaved_into_the_stream_does_not_break_parsing() {
        assert_eq!(
            from_log(&format!("warning: something on stderr\n{CLEAN}\n")),
            Health::Completed
        );
    }

    #[test]
    fn the_last_terminal_record_wins() {
        // Retries can leave earlier records in the stream.
        assert_eq!(
            from_log(&format!("{DEAD}\n{CLEAN}\n")),
            Health::Completed,
            "later record must win"
        );
    }

    #[test]
    fn an_opaque_log_that_exited_cleanly_can_be_sealed() {
        // THE DEFECT: a c2rust cmake/cargo log or a kiro prose log carries no terminal
        // record, so classifying it as stream-json calls it Infra/truncated or Unknown and
        // `completed()` returns None. `Scrubbed::seal` demands a Completed, so 9 of 16
        // translate backends could never publish, and kiro's verify phase never could
        // either.
        let log = "Finished release [optimized] target(s)\n";
        assert_eq!(
            classify(log, LogFormat::Opaque, Exit::Success),
            Health::Completed
        );
        assert!(
            classify(log, LogFormat::Opaque, Exit::Success)
                .completed()
                .is_some(),
            "the proof seal() needs must be obtainable"
        );
        // ...and the old path really did refuse it, so this test is not vacuous.
        assert!(from_log(log).completed().is_none());
    }

    #[test]
    fn an_opaque_log_is_never_sealed_on_a_failure_or_without_an_observation() {
        for exit in [
            Exit::Failure { code: Some(1) },
            Exit::Failure { code: None },
            Exit::Timeout,
            Exit::Unobserved,
        ] {
            assert!(
                classify("error: could not compile\n", LogFormat::Opaque, exit)
                    .completed()
                    .is_none(),
                "must not mint a proof for {exit:?}"
            );
        }
    }

    #[test]
    fn a_failing_tool_stays_in_the_denominator_but_a_timeout_does_not() {
        // A tool that ran and could not translate is a RESULT and must be scored;
        // calling it infra is how a project silently leaves the denominator and
        // inflates the published rate. A timeout genuinely has no measurement.
        let log = "c2rust: unsupported construct\n";
        assert!(
            !classify(log, LogFormat::Opaque, Exit::Failure { code: Some(1) }).is_infra(),
            "a failed translation is a result, not an infra failure"
        );
        assert!(classify(log, LogFormat::Opaque, Exit::Timeout).is_infra());
    }

    #[test]
    fn a_stream_json_verdict_is_never_overridden_by_the_exit_code() {
        // The agent runs under `ulimit -f`/`-d`, so a test binary killed by SIGXFSZ
        // fails commands inside a session that is itself fine — a result, not an infra
        // failure. And an api_error run must stay infra however cleanly the shell exited.
        for exit in [
            Exit::Success,
            Exit::Failure { code: Some(137) },
            Exit::Timeout,
            Exit::Unobserved,
        ] {
            assert_eq!(
                classify(CLEAN, LogFormat::StreamJson, exit),
                Health::Completed,
                "terminal record is authoritative for {exit:?}"
            );
            assert!(classify(DEAD, LogFormat::StreamJson, exit).is_infra());
        }
    }
}
