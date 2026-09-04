//! The agentic CLI backends: claude, codex and kiro.
//!
//! The only KEYED runners. An iterating agent session is expensive and nondeterministic, which is
//! what makes an entry worth having; a transpiler is neither.

use crate::agents::session::Session;
use crate::domain::health::{Exit, LogFormat, PinReport};
use crate::invocation::{Execute, Ran};
use crate::runners::CliVersion;
use crate::store::ModelId;
use crate::tree::WorkDir;
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;

/// What the harness observed of the child.
///
/// Lives here, not on `Exit` in `domain::health`: the pure layer may not name a process, and the
/// architecture gate said so the moment I put it there. `timeout` reports 124, which is a KILL rather
/// than a failure of the thing being measured -- and because every session pipes through `tee`, a
/// status of 0 proves nothing on its own; the transcript is the discriminator.
fn observed(status: std::process::ExitStatus, log: &Path) -> Exit {
    match status.code() {
        Some(0) => Exit::Success,
        // A kill at the wall clock means two different things, and only here can they be told apart.
        // If the transcript was still growing when the axe fell, the agent was WORKING and did not
        // converge -- that is the tool's answer and the case is scored as a failure. If it had gone
        // silent, the agent was hung and there is no measurement. Measured: kiro spent all 43_200s on
        // `001_perlin_noise` and wrote to its log until the minute it was killed, still reporting
        // "1500 cases, 7 real mismatches"; classifying that as infrastructure voided the battery.
        Some(124) => {
            if wrote_recently(log, SILENT_AFTER) {
                Exit::Exhausted
            } else {
                Exit::Timeout
            }
        }
        code => Exit::Failure { code },
    }
}

/// How long a transcript may be silent before a wall-clock kill reads as a hang rather than as work.
/// Generous: an agent can legitimately sit in one long build, and calling that a hang would blame the
/// harness for the tool's failure to finish.
const SILENT_AFTER: std::time::Duration = std::time::Duration::from_secs(900);

fn wrote_recently(log: &Path, within: std::time::Duration) -> bool {
    std::fs::metadata(log)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|since| since <= within)
}

/// Which CLI, and what it needs beyond a working dir and a prompt.
pub enum Backend {
    Claude,
    Codex { region: &'static str },
    Kiro,
}

/// Whether this backend can be given the read/write policy at all.
///
/// A named enum rather than an `Option<PathBuf>`, because the absent case is not "no path yet" but
/// "this CLI has no mechanism to accept one", and the two read identically at a call site. The policy
/// was written for EVERY backend and passed only to claude: codex ran
/// `--dangerously-bypass-approvals-and-sandbox` and kiro `--trust-all-tools` with
/// `<work>/.claude/settings.json` sitting unread beside them, so the repo -- the graded oracle's
/// `test_vectors/` and every sibling agent's translation -- was readable, while `Enforcement::Required`
/// refused to launch on the grounds that it was not. `require_enforceable` only probes PATH, so it
/// passed. Now the backend STATES it, `execute` writes the policy only where it can be applied, and
/// [`crate::io::sandbox::Sandboxed`] carries the answer into the entry -- so an artifact says which way
/// it was instead of leaving a reader to assume.
pub enum Sandboxing {
    /// Reads `--settings <file>`: the policy can be enforced.
    Settings,
    /// No mechanism to accept a filesystem policy.
    Unavailable,
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::Claude => "claude",
            Backend::Codex { .. } => "codex",
            Backend::Kiro => "kiro",
        }
    }

    /// How this backend's transcript is written. Exhaustive, so a new backend has to state its
    /// format rather than inherit one: defaulting codex into claude's is what made 7 of 17 agents'
    /// logs classify as `Unknown`, silencing the infra gate two files away.
    fn log_format(&self) -> LogFormat {
        match self {
            Backend::Claude => LogFormat::StreamJson,
            Backend::Codex { .. } => LogFormat::CodexJson,
            // Prose. Nothing machine-readable, so no cost and no model confirmation.
            Backend::Kiro => LogFormat::Opaque,
        }
    }

    /// Exhaustive for the same reason `log_format` is: a new backend must SAY whether its policy can
    /// be applied, rather than inherit an answer that happens to be claude's.
    pub fn sandboxing(&self) -> Sandboxing {
        match self {
            Backend::Claude => Sandboxing::Settings,
            Backend::Codex { .. } | Backend::Kiro => Sandboxing::Unavailable,
        }
    }
}

/// Fail loudly if the CLI did not honour the model pin, or is not the build the key
/// was computed for.
///
/// `--model` is a request, not a guarantee: an unrecognised id can be silently
/// substituted, and the sweep would then be cached under a key naming a model that
/// never ran. The CLI build has the same problem from the other end — it auto-updates
/// through a shim, so the binary can change between the probe and the run. The
/// transcript's `init` record is the CLI's own report of both.
pub fn assert_pins_honoured(
    log_path: &Path,
    want: &ModelId,
    cli: &CliVersion,
) -> Result<PinReport> {
    // The HEAD, not the whole file: the `init` record is the first line, and `read_to_string` here
    // allocated 672 MB for a runaway transcript on a check that reads one line.
    let text = match crate::agent_health::read_head(log_path) {
        Ok(t) => t,
        // The health classifier already treats a missing log as a non-completion.
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(PinReport::NotReported)
        }
        Err(e) => return Err(e).context("reading the transcript for its model pin"),
    };
    let Some(init) = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|r| r["type"] == "system" && r["subtype"] == "init")
    else {
        // kiro writes prose and codex its own JSON, neither of which reports the model, so there is
        // nothing here to compare against. RECORDED as such rather than reported as confirmed --
        // "classify it as no-evidence" is how a gate goes quiet without anyone disabling it.
        return Ok(PinReport::NotReported);
    };
    // A typed refusal, not an ensure!: the sweep collects these and fails the command,
    // where an anyhow error would have been folded into an ordinary red X.
    if let Some(got) = init["model"].as_str() {
        if got != want.as_str() {
            return Err(crate::refusal::Refusal::ModelSubstituted {
                asked: want.as_str().to_string(),
                got: got.to_string(),
            }
            .into());
        }
    }
    if let Some(got) = init["claude_code_version"].as_str() {
        anyhow::ensure!(
            cli.covers(got),
            "the CLI upgraded under the run: the key names {}, the session reports {got}. \
             Refusing to store this artifact under a key naming another build.",
            cli.as_str()
        );
    }
    Ok(PinReport::Confirmed)
}

pub struct Cli {
    pub session: Session,
    pub tool: crate::cli::Tool,
    pub backend: Backend,
    pub model: ModelId,
    pub cli_version: CliVersion,
    pub repo_root: std::path::PathBuf,
    pub enforcement: crate::io::sandbox::Enforcement,
}

impl Execute for Cli {
    fn execute(&self, work: &WorkDir, prompt: &str, log: &Path) -> Result<Ran> {
        let started = Instant::now();
        // The WORK ROOT, not the crate: the prompts describe `c_src/` and `translation/` as siblings
        // and only the root resolves both. Standing in the crate made every `c_src/` reference a path
        // that does not exist.
        let cwd = work.root().to_path_buf();
        // Per INVOCATION, both of them, so three tools running at once cannot share an agent TMPDIR,
        // a settings file, or -- for opencode, whose `XDG_CONFIG_HOME` this is -- a credential store.
        // They were resolved from the machine-wide work base, which every tool and every case shared.
        //
        // TMPDIR lives in its own scratch rather than inside the working dir: `tmp/` is not on the
        // digest's ignore list, so agent scratch there would be hashed into the tree.
        let scratch = crate::io::workdir::tempdir("harvest-agent-")?;
        let agent_tmp = scratch.path().join("tmp");
        std::fs::create_dir_all(&agent_tmp)?;
        // `<work_root>/.claude/settings.json`, and the policy's allow-list is that same work dir --
        // one field, so the agent is never launched somewhere its own policy denies. `.claude` is
        // root-anchored ignored, so the file does not reach the digest.
        // Written only for a backend that can be GIVEN it, and the answer is RECORDED. It used to be
        // written unconditionally and passed to claude alone, so two of three tools ran with the policy
        // sitting unread beside them and nothing said so.
        let sandboxed = match self.backend.sandboxing() {
            Sandboxing::Settings => crate::io::sandbox::Sandboxed::Enforced,
            Sandboxing::Unavailable => crate::io::sandbox::Sandboxed::NotSupportedByBackend,
        };
        let mut command = match &self.backend {
            Backend::Claude => {
                let settings = crate::io::sandbox::write_settings(crate::io::sandbox::Policy {
                    repo_root: &self.repo_root,
                    work_root: work.root(),
                    enforcement: self.enforcement,
                })?;
                self.session
                    .claude_command(&crate::agents::session::ClaudeRun {
                        cwd: &cwd,
                        prompt,
                        log,
                        settings: &settings,
                        model: &self.model,
                        agent_tmp: &agent_tmp,
                    })
            }
            Backend::Codex { region } => {
                self.session
                    .codex_command(&cwd, prompt, log, self.model.as_str(), region)
            }
            Backend::Kiro => self.session.kiro_command(&cwd, prompt, log, &self.model),
        };
        let status = command
            .status()
            .with_context(|| format!("invoking the {} CLI", self.backend.name()))?;
        // BEFORE anything reads it, so the published copy and the entry's `run.log` are the same bytes
        // and neither can exceed what the store is able to carry. See `KEEP_WHOLE_BYTES`.
        if crate::agent_health::bound_transcript(log)? {
            println!(
                "  \u{2702}\u{fe0f}  {}: the transcript was elided to fit the store; see the marker in {}",
                self.backend.name(),
                log.display()
            );
        }
        // Classified from the transcript, with the exit only as corroboration: every session pipes
        // through `tee`, so a killed agent reports a clean 0.
        let health = crate::agent_health::classify_log(
            log,
            self.backend.log_format(),
            observed(status, log),
        );
        // Before the `Ran` is built, because `Ran` requires the report: the check cannot be skipped
        // without the compiler noticing.
        let pin = assert_pins_honoured(log, &self.model, &self.cli_version)?;
        Ok(Ran {
            health,
            wall_secs: started.elapsed().as_secs(),
            cost_usd: crate::agent_health::cost_usd(log, self.backend.log_format()),
            cli: self.cli_version.as_str().to_string(),
            pin,
            sandboxed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every backend must STATE whether its policy can be applied, and the two that cannot must not be
    /// recorded as if they could.
    ///
    /// Exhaustive over `Backend`, and asserting the MAPPING rather than one hand-built value -- the
    /// defect this replaces was that `write_settings` ran for all three while only claude was passed the
    /// path, so codex and kiro ran unconfined with nothing saying so. A new backend has to choose here;
    /// it cannot inherit claude's answer.
    #[test]
    fn every_backend_states_whether_its_sandbox_policy_can_be_applied() {
        use crate::io::sandbox::Sandboxed;
        for (backend, want) in [
            (Backend::Claude, Sandboxed::Enforced),
            (
                Backend::Codex {
                    region: "us-east-2",
                },
                Sandboxed::NotSupportedByBackend,
            ),
            (Backend::Kiro, Sandboxed::NotSupportedByBackend),
        ] {
            // The same derivation `execute` uses, so the test cannot agree with a mapping the run does
            // not apply.
            let recorded = match backend.sandboxing() {
                Sandboxing::Settings => Sandboxed::Enforced,
                Sandboxing::Unavailable => Sandboxed::NotSupportedByBackend,
            };
            assert_eq!(
                recorded,
                want,
                "{} records the wrong answer about its own confinement",
                backend.name()
            );
        }
        // Non-vacuity: the two answers must be DIFFERENT values, or every assertion above holds for a
        // mapping that says the same thing about every backend -- which is the bug.
        assert_ne!(Sandboxed::Enforced, Sandboxed::NotSupportedByBackend);
        // The default is the cautious one: an entry written before this was recorded has no answer, and
        // `Enforced` would be a claim nobody checked.
        assert_eq!(Sandboxed::default(), Sandboxed::NotSupportedByBackend);
    }
}
