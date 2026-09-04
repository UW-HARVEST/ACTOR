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

/// What the transcript proved about the model pin.
///
/// This exists so the CHECK cannot go dead. `assert_pins_honoured` had zero callers while
/// `runners/mod.rs` and `reproduce.sh` both asserted in prose that it runs -- and it is the check that
/// catches a CLI silently substituting a model, the failure that made 338 kiro rows unattributable.
/// [`crate::invocation::Ran`] now requires one of these, so the only way to build a `Ran` in
/// `Cli::execute` is to have called the check, and deleting the call stops compiling.
///
/// `NotReported` is not a pass. It is recorded in `agent.json`, so an artifact says whether its pin was
/// confirmed instead of leaving the reader to assume it.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PinReport {
    /// The session reported the model, and it is the one asked for.
    Confirmed,
    /// The transcript carries no model record: kiro writes prose, codex its own JSON.
    #[default]
    NotReported,
}

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
    /// codex `exec --json`: a DIFFERENT dialect -- `turn.completed`/`turn.failed`, not claude's
    /// `result`/`terminal_reason`. Marked `StreamJson` it was the bug above, so every codex run
    /// classified `Infra { truncated }` and could never seal. That is the mechanism of `Codex 0/338`.
    CodexJson,
    /// Prose (kiro, kimi, oneshot) or tool output (c2rust, laertes, c2saferrust).
    /// Proves nothing about completion, so the exit status is the only evidence.
    Opaque,
}

/// The harness's LIVE observation of how the agent process ended.
///
/// [`Exit::Unobserved`] is deliberately distinct from [`Exit::Failure`]: nobody watched, not something
/// went wrong. An audit recovers what the run recorded and lands here only when there is no record --
/// it must never manufacture an observation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    Success,
    Failure {
        code: Option<i32>,
    },
    /// `timeout` killed the child (it reports 124) and the transcript had gone SILENT well before the
    /// kill: the agent was hung, so there is no measurement.
    Timeout,
    /// Killed at the wall clock while still writing. The agent was working and did not converge, which
    /// is the tool's answer rather than the harness's fault. Only the edge can tell these apart -- it
    /// takes the transcript's last-write time, which the pure layer never sees.
    Exhausted,
    Unobserved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Ran to completion. Says nothing about whether the translation or
    /// verification *succeeded* — that is a result, the scorer's business.
    Completed,
    /// Did not finish for a reason outside the thing being measured: auth, rate
    /// limiting, transport, or a truncated log.
    Infra { reason: String, detail: String },
    /// The agent used its ENTIRE wall-clock ceiling and was killed: the harness gave it the budget and
    /// it did not finish, so it belongs with `Refused`, not `Infra`. Measured -- kiro spent all 43_200s
    /// on `001_perlin_noise` still reporting "1500 cases, 7 real mismatches". Only a meaningful signal
    /// once the ceiling stopped varying by tool: a kill at kiro's old 2_700s said nothing.
    Exhausted { secs: u64 },
    /// The PROVIDER declined on content grounds. Terminal and reproducible, unlike `Infra`, so a retry
    /// cannot help. Separated because one codex refusal voided all 85 of its B01_synthetic cases as
    /// though a blip had lost an entry -- and that corpus holds 12 cases named for buffer overflows.
    Refused { kind: RefusalKind, detail: String },
    /// No evidence either way (kiro logs are not stream-json; results predating
    /// this module have no terminal record). Not a failure.
    Unknown { why: String },
}

/// Why a provider declined. A named enum, not a string, so a second spelling of the same refusal
/// cannot appear in the counter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalKind {
    /// The provider's own safety classifier flagged the request. codex names it in its `message`.
    HighRiskCyberActivity,
}

impl Health {
    /// Whether the run finished. Says nothing about whether the translation SUCCEEDED -- that is a
    /// result, and the scorer's business.
    pub fn is_completed(&self) -> bool {
        matches!(self, Health::Completed)
    }

    pub fn is_infra(&self) -> bool {
        matches!(self, Health::Infra { .. })
    }

    /// A refusal the provider will repeat. NOT `is_infra`: the infra gate exists to stop a transport
    /// failure being scored as a measurement, and this IS a measurement.
    pub fn refusal(&self) -> Option<&RefusalKind> {
        match self {
            Health::Refused { kind, .. } => Some(kind),
            _ => None,
        }
    }

    /// The tool answered, and the answer is "no": it declined, or it spent the whole budget without
    /// finishing. Either way the case is scored as a failure and the battery still publishes -- unlike
    /// `Infra`, which means the harness cannot say what the tool would have done.
    pub fn is_tool_answer_failure(&self) -> bool {
        matches!(self, Health::Refused { .. } | Health::Exhausted { .. })
    }
}

/// Classify a run from its transcript and how the process ended.
///
/// Takes the text, never the path, so the decision needs no fixture on disk;
/// [`crate::agent_health::classify_log`] is the after-the-fact counterpart for auditing a tree.
/// For [`LogFormat::StreamJson`] the terminal record is authoritative and `exit` is deliberately
/// ignored -- see the module docs on `SIGXFSZ`: a test binary killed by a signal fails commands inside
/// a session that is itself fine, and that is a *result*.
pub fn classify(text: &str, format: LogFormat, exit: Exit) -> Health {
    // `Exhausted` BEFORE the per-format arms, for every backend. It used to be reachable only from the
    // `Opaque` arm, because the other two never received `exit` -- so one wall-clock kill of a session
    // that was still writing was `Exhausted { secs }` for kiro and `Infra { "truncated" }` for claude
    // and codex. `Infra` is transient, so the identical event was RETRIED three times for two backends
    // and RECORDED as a terminal answer for the third. Which backend it was cannot be what decides.
    if exit == Exit::Exhausted {
        return Health::Exhausted { secs: 0 };
    }
    match format {
        LogFormat::StreamJson => classify_stream_json(text),
        LogFormat::CodexJson => classify_codex_json(text),
        // An opaque log cannot distinguish "finished" from "killed", so the exit
        // status carries the whole burden of proof.
        LogFormat::Opaque => match exit {
            // `Exhausted` is handled above, for every backend.
            Exit::Exhausted => unreachable!("returned before the format is consulted"),
            Exit::Success => Health::Completed,
            // The run was cut off: there is no measurement, exactly as a truncated
            // stream-json log has none.
            Exit::Timeout => Health::Infra {
                reason: "timeout".into(),
                detail: "the agent was killed at the wall clock".into(),
            },
            // A tool that ran and failed is a RESULT and stays in the denominator; treating that as
            // infra inflates the score. The PROVIDER saying "temporarily unavailable" is NOT that, and
            // `Unknown` made it neither retryable nor visible to the gate.
            Exit::Failure { code } => match provider_transient(text) {
                Some(reason) => Health::Infra {
                    reason: reason.into(),
                    detail: "the provider reported a transient failure".into(),
                },
                None => Health::Unknown {
                    why: format!(
                        "opaque log, agent exited {}",
                        code.map(|c| c.to_string())
                            .unwrap_or_else(|| "by signal".into())
                    ),
                },
            },
            Exit::Unobserved => Health::Unknown {
                why: "opaque log and no observed exit status".into(),
            },
        },
    }
}

/// The provider's own "come back later", in ONE place like `provider_refusal`. Narrow: a genuine tool
/// failure must keep classifying `Unknown` rather than being retried.
pub fn provider_transient(message: &str) -> Option<&'static str> {
    let m = message.to_ascii_lowercase();
    if m.contains("temporarily unavailable") {
        return Some("model_unavailable");
    }
    if m.contains("throttl") || m.contains("rate limit") || m.contains("too many requests") {
        return Some("throttled");
    }
    if m.contains("service unavailable") || m.contains("bad gateway") {
        return Some("service_unavailable");
    }
    None
}

/// The provider's own marker for a content REFUSAL, matched in ONE place. Keyed on the stable
/// documentation URL rather than the prose, which is a marketing string: a refusal is a fact about the
/// provider, not about the log format, so every dialect shares this.
fn provider_refusal(message: &str) -> Option<RefusalKind> {
    let m = message.to_ascii_lowercase();
    (m.contains("/guides/safety-checks")
        || m.contains("flagged for potentially high-risk")
        || m.contains("high-risk cyber activity"))
    .then_some(RefusalKind::HighRiskCyberActivity)
}

fn classify_codex_json(tail: &str) -> Health {
    let completed = tail.contains(r#""type":"turn.completed""#);
    let failed = tail.contains(r#""type":"turn.failed""#);
    if completed {
        return Health::Completed;
    }
    if failed {
        let detail =
            last_str(tail, "message").unwrap_or_else(|| "codex reported turn.failed".into());
        if let Some(kind) = provider_refusal(&detail) {
            return Health::Refused { kind, detail };
        }
        return Health::Infra {
            reason: "turn.failed".into(),
            detail,
        };
    }
    // Neither: the process died mid-turn, exactly as a truncated stream-json log did.
    Health::Infra {
        reason: "truncated".into(),
        detail: "no codex terminal record: the agent was killed before finishing".into(),
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
mod transient_tests {
    use super::*;

    /// kiro's pcre2 ended with `temporarily unavailable` and exit 1, classified `Unknown`, and left
    /// the table with nothing refusing. A tool that merely failed must still classify `Unknown`.
    #[test]
    fn a_provider_transient_is_infra_and_a_tool_failure_is_still_unknown() {
        let kiro =
            "Kiro is having trouble responding right now:\n    The model you've selected is \
                    temporarily unavailable. Please relaunch with '--model <model_id>'.";
        let h = classify(kiro, LogFormat::Opaque, Exit::Failure { code: Some(1) });
        assert!(
            matches!(&h, Health::Infra { reason, .. } if reason == "model_unavailable"),
            "the provider's own words must be read as infra: {h:?}"
        );
        assert!(h.is_infra(), "so the gate can refuse rather than shrug");

        // Non-vacuous: a real failure at the same exit code must NOT become retryable.
        let real = "error: could not compile `translation` due to 12 previous errors";
        let h = classify(real, LogFormat::Opaque, Exit::Failure { code: Some(1) });
        assert!(matches!(h, Health::Unknown { .. }), "{h:?}");

        for (text, want) in [
            (
                "Model is temporarily unavailable",
                Some("model_unavailable"),
            ),
            ("ThrottlingException: Too many requests", Some("throttled")),
            ("503 Service Unavailable", Some("service_unavailable")),
            ("assertion failed: left == right", None),
        ] {
            assert_eq!(provider_transient(text), want, "{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wall-clock kill means two different things and the harness must not conflate them. kiro spent
    /// all 43_200s on `001_perlin_noise`, writing to its log until the minute it was killed, still
    /// reporting "1500 cases, 7 real mismatches": it was WORKING and did not converge, which is the
    /// tool's answer. Recorded as `Infra` it voided the battery instead, exactly as a hung agent would.
    #[test]
    fn a_kill_while_still_working_is_the_tools_answer_not_an_infra_failure() {
        let log = "MISMATCH ...\n1500 cases, 7 real mismatches\n";
        let worked = classify(log, LogFormat::Opaque, Exit::Exhausted);
        assert!(worked.is_tool_answer_failure(), "got {worked:?}");
        assert!(
            !worked.is_infra(),
            "the infra gate must not fire: the tool was given the budget and used it"
        );
        assert!(!worked.is_completed(), "and it did not finish");

        // Non-vacuous: a kill after the transcript went SILENT is still infra, because a hung agent
        // produced no measurement. Only the edge can tell the two apart, from the log's last write.
        let hung = classify(log, LogFormat::Opaque, Exit::Timeout);
        assert!(
            hung.is_infra() && !hung.is_tool_answer_failure(),
            "got {hung:?}"
        );
    }

    /// The provider declining on content grounds is a MEASUREMENT, not an infrastructure failure.
    /// Classified as `Infra`, codex's one refusal on `030_mutable_buffer_overlap_extrahard_lib`
    /// discarded all 85 of its B01_synthetic cases through `attests`, exactly as a lost entry would.
    /// The message below is verbatim from that transcript.
    #[test]
    fn a_provider_content_refusal_is_not_an_infrastructure_failure() {
        let tail = concat!(
            r#"{"type":"turn.failed","error":{"message":"This request has been flagged for "#,
            r#"potentially high-risk cyber activity. Learn more here: "#,
            r#"https://platform.openai.com/docs/guides/safety-checks/cybersecurity"}}"#,
        );
        let h = classify(tail, LogFormat::CodexJson, Exit::Failure { code: Some(1) });
        assert_eq!(
            h.refusal(),
            Some(&RefusalKind::HighRiskCyberActivity),
            "got {h:?}"
        );
        assert!(
            !h.is_infra(),
            "the infra gate stops a transport failure being scored; this must not trip it"
        );
        assert!(!h.is_completed(), "and it certainly did not complete");

        // Non-vacuous: an ordinary turn.failed with no refusal marker is still Infra.
        let plain = classify(
            r#"{"type":"turn.failed","error":{"message":"upstream connect error"}}"#,
            LogFormat::CodexJson,
            Exit::Failure { code: Some(1) },
        );
        assert!(
            plain.is_infra() && plain.refusal().is_none(),
            "got {plain:?}"
        );
    }

    /// Verbatim shape of a real credential-expiry terminal record.
    const DEAD: &str = r#"{"type":"system","subtype":"api_retry","attempt":4,"error_status":403}
{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error","api_error_status":403,"duration_ms":4569000,"num_turns":12,"result":"Failed to authenticate. API Error: 403 The security token included in the request is expired","session_id":"abc"}"#;

    const CLEAN: &str = r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","duration_ms":573254,"num_turns":88,"total_cost_usd":4.2,"result":"Verified. c_src/ was not modified.","session_id":"def"}"#;

    /// What the audit sees: a transcript and nothing else.
    fn from_log(text: &str) -> Health {
        classify(text, LogFormat::StreamJson, Exit::Unobserved)
    }

    /// A codex run that WROTE A WORKING TRANSLATION must not read as a killed one. This fixture is
    /// trimmed from one that built and produced byte-identical output.
    #[test]
    fn a_finished_codex_run_is_completed_and_a_dead_one_is_infra() {
        let done = r#"{"type":"item.completed","item":{"id":"item_34","type":"command_execution","exit_code":0}}
{"type":"turn.completed","usage":{"input_tokens":230888,"output_tokens":4573}}"#;
        assert_eq!(
            classify(done, LogFormat::CodexJson, Exit::Success),
            Health::Completed,
            "a completed codex turn is a result"
        );
        // Non-vacuity: the old classifier really did reject this exact text, so the fixture
        // contains the trap rather than merely passing under a rule that accepts anything.
        assert!(
            matches!(
                classify(done, LogFormat::StreamJson, Exit::Success),
                Health::Infra { .. }
            ),
            "fixture check: claude's classifier must be the thing that mis-reads it"
        );

        let dead = r#"{"type":"error","message":"unexpected status 404 Not Found: The model does not exist"}
{"type":"turn.failed","error":{"message":"unexpected status 404 Not Found"}}"#;
        let Health::Infra { reason, .. } = classify(dead, LogFormat::CodexJson, Exit::Success)
        else {
            panic!("turn.failed with no turn.completed is a dead run, whatever the exit status")
        };
        assert_eq!(reason, "turn.failed");

        // And a log cut off mid-turn is still truncated, not a pass.
        let cut = r#"{"type":"turn.started"}"#;
        assert!(matches!(
            classify(cut, LogFormat::CodexJson, Exit::Success),
            Health::Infra { .. }
        ));
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
            classify(log, LogFormat::Opaque, Exit::Success).is_completed(),
            "an opaque log with a clean exit is the one case that classifies completed"
        );
        // ...and the old path really did refuse it, so this test is not vacuous.
        assert!(!from_log(log).is_completed());
    }

    #[test]
    fn an_opaque_log_never_completes_on_a_failure_or_without_an_observation() {
        for exit in [
            Exit::Failure { code: Some(1) },
            Exit::Failure { code: None },
            Exit::Timeout,
            Exit::Unobserved,
        ] {
            assert!(
                !classify("error: could not compile\n", LogFormat::Opaque, exit).is_completed(),
                "must not classify {exit:?} as completed"
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
