//! How the agent process ended, and what that invocation cost.
//!
//! The exit status is captured deep inside the per-agent dispatch but written out by the
//! caller that records the case, so it is stashed in a thread-local instead of being
//! threaded back through ~12 match arms. Sound only because each case runs on its own
//! thread: "last agent exit on this thread" is unambiguously this case's.
//!
//! `exit_code` is the shell pipeline's status (`set -o pipefail` makes it the agent's
//! own code). Absent for agents with no single agent process (kimi/oneshot, c2rust).

#[derive(Clone, Copy, Default)]
pub struct AgentExit {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub recorded: bool,
}

thread_local! {
    static LAST_AGENT_EXIT: std::cell::Cell<AgentExit> =
        const { std::cell::Cell::new(AgentExit { exit_code: None, timed_out: false, recorded: false }) };
}

pub fn record_agent_exit(status: std::process::ExitStatus) {
    let code = status.code();
    LAST_AGENT_EXIT.with(|c| {
        c.set(AgentExit {
            exit_code: code,
            timed_out: code == Some(124), // `timeout` exits 124 when it kills the child
            recorded: true,
        })
    });
}

/// Call at the start of a case: a non-CLI agent on a re-used thread must not inherit
/// the previous case's exit code.
pub fn clear_agent_exit() {
    LAST_AGENT_EXIT.with(|c| c.set(AgentExit::default()));
}

fn take_agent_exit() -> AgentExit {
    LAST_AGENT_EXIT.with(|c| c.replace(AgentExit::default()))
}

/// The observed exit, WITHOUT consuming it.
///
/// `merge_agent_exit` must be called exactly once per invocation because it takes the
/// thread-local, and it runs later, when the metrics are written. The classifier needs
/// the same observation first, so this peeks. `Copy` on [`AgentExit`] is what makes that
/// a read rather than a second claim on it.
pub fn observed_exit() -> crate::domain::health::Exit {
    use crate::domain::health::Exit;
    let e = LAST_AGENT_EXIT.with(|c| c.get());
    if !e.recorded {
        return Exit::Unobserved;
    }
    if e.timed_out {
        return Exit::Timeout;
    }
    match e.exit_code {
        Some(0) => Exit::Success,
        code => Exit::Failure { code },
    }
}

/// Shared by translate and verify so both report agent process health identically.
pub(crate) fn merge_agent_exit(metrics: &mut serde_json::Value) {
    let e = take_agent_exit();
    if e.recorded {
        metrics["exit_code"] = serde_json::json!(e.exit_code);
        metrics["timed_out"] = serde_json::json!(e.timed_out);
    }
}

/// What one agent invocation cost and how it exited.
///
/// Must be built exactly once per invocation: `merge_agent_exit` consumes the
/// recorded exit. The one value then feeds both `verification.json` and the cache
/// entry, so the two cannot disagree about the same run.
pub fn agent_provenance(tool: &str, duration_secs: u64) -> serde_json::Value {
    let mut p = serde_json::json!({
        "agent": tool,
        "duration_secs": duration_secs,
        "success": true,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        // Which harness code produced this; results are otherwise unattributable
        // after the fact.
        "harness": crate::provenance::harness_id(),
        "sandboxed": crate::io::sandbox::is_enforceable(),
    });
    merge_agent_exit(&mut p);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// ExitStatus cannot be constructed directly, so shell out for a real one.
    fn exit_status(code: i32) -> std::process::ExitStatus {
        Command::new("sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .status()
            .unwrap()
    }

    #[test]
    fn merge_agent_exit_records_code_when_captured() {
        clear_agent_exit();
        record_agent_exit(exit_status(0));
        let mut m = serde_json::json!({"success": true});
        merge_agent_exit(&mut m);
        assert_eq!(m["exit_code"], serde_json::json!(0));
        assert_eq!(m["timed_out"], serde_json::json!(false));
    }

    #[test]
    fn merge_agent_exit_flags_timeout_124() {
        clear_agent_exit();
        record_agent_exit(exit_status(124)); // `timeout` uses 124
        let mut m = serde_json::json!({});
        merge_agent_exit(&mut m);
        assert_eq!(m["exit_code"], serde_json::json!(124));
        assert_eq!(m["timed_out"], serde_json::json!(true));
    }

    #[test]
    fn merge_agent_exit_absent_for_non_cli_agent() {
        // No record_agent_exit call, as on the kimi/oneshot API path.
        clear_agent_exit();
        let mut m = serde_json::json!({"success": true});
        merge_agent_exit(&mut m);
        assert!(
            m.get("exit_code").is_none(),
            "exit_code must be absent when no CLI agent ran"
        );
        assert!(m.get("timed_out").is_none());
    }

    #[test]
    fn take_agent_exit_clears_so_next_case_starts_fresh() {
        record_agent_exit(exit_status(1));
        let _ = take_agent_exit(); // consume
        let second = take_agent_exit(); // must be empty now
        assert!(
            !second.recorded,
            "exit must not leak into the next case on a reused thread"
        );
    }
}
