//! The agentic CLI backends: claude, codex, kiro, opencode.
//!
//! The only KEYED runners. An iterating agent session is expensive and nondeterministic, which is
//! what makes an entry worth having; a transpiler is neither.

use crate::agents::session::Session;
use crate::domain::health::{Exit, LogFormat};
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
fn observed(status: std::process::ExitStatus) -> Exit {
    match status.code() {
        Some(0) => Exit::Success,
        Some(124) => Exit::Timeout,
        code => Exit::Failure { code },
    }
}

/// Which CLI, and what it needs beyond a working dir and a prompt.
pub enum Backend {
    Claude,
    Codex { region: &'static str },
    Kiro,
    OpenCode { model_arg: String },
}

impl Backend {
    /// How this backend's transcript is written. Exhaustive, so a new backend has to state its
    /// format rather than inherit one: defaulting codex into claude's is what made 7 of 17 agents'
    /// logs classify as `Unknown`, silencing the infra gate two files away.
    fn name(&self) -> &'static str {
        match self {
            Backend::Claude => "claude",
            Backend::Codex { .. } => "codex",
            Backend::Kiro => "kiro",
            Backend::OpenCode { .. } => "opencode",
        }
    }

    fn log_format(&self) -> LogFormat {
        match self {
            Backend::Claude | Backend::OpenCode { .. } => LogFormat::StreamJson,
            Backend::Codex { .. } => LogFormat::CodexJson,
            // Prose. Nothing machine-readable, so no cost and no model confirmation.
            Backend::Kiro => LogFormat::Opaque,
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
pub fn assert_pins_honoured(log_path: &Path, want: &ModelId, cli: &CliVersion) -> Result<()> {
    let text = match std::fs::read_to_string(log_path) {
        Ok(t) => t,
        // The health classifier already treats a missing log as a non-completion.
        Err(_) => return Ok(()),
    };
    let Some(init) = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|r| r["type"] == "system" && r["subtype"] == "init")
    else {
        // Older transcripts predate the init record; nothing to compare against.
        return Ok(());
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
    Ok(())
}

pub struct Cli {
    pub session: Session,
    pub tool: crate::cli::Tool,
    pub backend: Backend,
    pub model: ModelId,
    pub cli_version: String,
    pub repo_root: std::path::PathBuf,
    pub enforcement: crate::io::sandbox::Enforcement,
}

impl Execute for Cli {
    fn execute(&self, work: &WorkDir, prompt: &str, log: &Path) -> Result<Ran> {
        let started = Instant::now();
        let cwd = work.translation();
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
        let settings = crate::io::sandbox::write_settings(crate::io::sandbox::Policy {
            repo_root: &self.repo_root,
            work_root: work.root(),
            enforcement: self.enforcement,
        })?;
        let mut command = match &self.backend {
            Backend::Claude => self
                .session
                .claude_command(&crate::agents::session::ClaudeRun {
                    cwd: &cwd,
                    prompt,
                    log,
                    settings: &settings,
                    model: &self.model,
                    agent_tmp: &agent_tmp,
                }),
            Backend::Codex { region } => {
                self.session
                    .codex_command(&cwd, prompt, log, self.model.as_str(), region)
            }
            Backend::Kiro => self.session.kiro_command(&cwd, prompt, log, &self.model),
            Backend::OpenCode { model_arg } => {
                self.session
                    .opencode_command(crate::agents::session::OpencodeRun {
                        cwd: &cwd,
                        prompt,
                        log,
                        model_arg,
                        xdg_config_home: &agent_tmp,
                    })
            }
        };
        let status = command
            .status()
            .with_context(|| format!("invoking the {} CLI", self.backend.name()))?;
        // Classified from the transcript, with the exit only as corroboration: every session pipes
        // through `tee`, so a killed agent reports a clean 0.
        let health =
            crate::agent_health::classify_log(log, self.backend.log_format(), observed(status));
        Ok(Ran {
            health,
            wall_secs: started.elapsed().as_secs(),
            cost_usd: crate::agent_health::cost_usd(log, self.backend.log_format()),
            cli: self.cli_version.clone(),
        })
    }
}
