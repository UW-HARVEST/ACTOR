//! The backends: what actually runs when an invocation is not served from the store.
//!
//! Each implements [`crate::invocation::Execute`], so `invocation.rs` cannot know which one it got.
//! Classifying the transcript belongs here because only a backend knows its own log format -- kiro
//! writes prose, claude and codex write different JSON, and defaulting one into another's format is
//! how `LogFormat` once made seven agents' transcripts unreadable.

pub mod cli;

use crate::cli::Tool;
use crate::store::ModelId;
use anyhow::{Context, Result};

/// The model every claude invocation is pinned to.
///
/// Pinned before the run: the CLI auto-updates, so an unpinned run is attributed to whatever it
/// defaulted to that day, and the resolved model appears only in the transcript's `init` record --
/// after the money is spent.
pub const CLAUDE_MODEL_DEFAULT: &str = "global.anthropic.claude-opus-5[1m]";

/// kiro-cli's pin. It DOES take `--model`; a comment once said otherwise, nothing passed the flag,
/// and 0 files under `results/Test-Corpus/kiro/` name a model, so every kiro row ever published is
/// unattributable. Verified accepted by the CLI.
pub const KIRO_MODEL: &str = "claude-opus-5";

/// Kimi takes no `--model`; this is what its Bedrock call carries.
pub const KIMI_MODEL: &str = "moonshotai.kimi-k2.5";

/// Codex's default. The FIRST codex model methodologically comparable to claude: it runs the same
/// chain and reads `prompts/codex/`, whose protocol replaces Claude Code's Task-tool sub-agent one
/// with Codex's own single-session form. `gpt-5.4` and `gpt-5.5` are still reachable by `--model`,
/// and both are historical -- they were fed claude's prompts and had no verify step at all.
pub const CODEX_MODEL_DEFAULT: &str = "openai.gpt-5.6-sol";

/// The agent CLI build that will run, from `<program> --version`.
///
/// RECORDED, never keyed: the CLIs auto-update through a shim, and keying them stranded every entry
/// on each vendor release. So a replay does not depend on it and never probes for it.
pub struct CliVersion(String);

/// States the build when the CLI is not installed — replaying a stored cache without
/// one. The build must still be NAMED, so the key stays as specific as before, and
/// [`crate::agents::invocation::assert_pins_honoured`] still checks it against what the
/// transcript reports.
pub const ENV_CLI_VERSION: &str = "HARVEST_CLI_VERSION";

impl CliVersion {
    pub fn probe(program: &str) -> Result<Self> {
        Self::resolve(program, std::env::var(ENV_CLI_VERSION).ok())
    }

    /// The override is a parameter, not a read: mutating process env in a test races
    /// across test threads (see `crate::io::workdir::resolve_from`).
    fn resolve(program: &str, stated: Option<String>) -> Result<Self> {
        if let Some(v) = stated {
            let v = v.trim();
            anyhow::ensure!(!v.is_empty(), "{ENV_CLI_VERSION} is set but names no build");
            return Ok(Self(v.to_string()));
        }
        let out = std::process::Command::new(program)
            .arg("--version")
            .output()
            .with_context(|| format!("running `{program} --version`"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next().unwrap_or_default().trim();
        anyhow::ensure!(
            out.status.success() && !line.is_empty(),
            "`{program} --version` reported no build ({}), so the cache key cannot name \
             the one that would run and this run would be indistinguishable from a run \
             made by another. Install {program}, or set {ENV_CLI_VERSION} to replay a \
             stored cache without it.",
            out.status
        );
        Ok(Self(line.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the version the CLI reported in its own transcript is the one probed.
    /// A containment test because `--version` prints the number plus a product name.
    pub fn covers(&self, reported: &str) -> bool {
        self.0.contains(reported)
    }
}

/// THE model a tool will be asked for, resolved before anything runs.
///
/// One definition. `resolve_launch` and `verify_invocation` each carried this mapping, so a model
/// could differ between a chain's two steps and the key would record the divergence as two unrelated
/// entries.
pub fn resolve_model(tool: Tool, flag: Option<&str>) -> Result<Option<ModelId>> {
    let pinned = |s: &str| ModelId::new(s).map(Some);
    match tool {
        Tool::Claude => pinned(
            &std::env::var("HARVEST_CLAUDE_MODEL")
                .unwrap_or_else(|_| CLAUDE_MODEL_DEFAULT.to_string()),
        ),
        Tool::Kiro => pinned(KIRO_MODEL),
        Tool::Kimi => pinned(KIMI_MODEL),
        // Defaulted like claude's and kiro's: a codex run with no `--model` is still a pinned run,
        // and the two older models stay reachable by naming them.
        Tool::Codex => pinned(flag.unwrap_or(CODEX_MODEL_DEFAULT)),
        // The model IS the identity for these, and there is no canonical one to default to.
        Tool::Oneshot | Tool::OpenCode => {
            let flag = flag.with_context(|| {
                format!(
                    "--tool {} needs --model: the model is part of its identity, not a default",
                    crate::cli::tool_dir(tool)
                )
            })?;
            pinned(flag)
        }
        // No model is asked for, so none belongs in a key or a path.
        Tool::C2rust | Tool::Laertes | Tool::C2SaferRust | Tool::SmartC2Rust => Ok(None),
    }
}

/// What a tool's transcript can prove about completion.
///
/// ONE table, exhaustive, so a new tool decides rather than inheriting: defaulting codex into
/// claude's dialect made 7 of 17 agents classify `Unknown`, silencing the infra gate two files away.
pub fn log_format(tool: Tool) -> crate::domain::health::LogFormat {
    use crate::domain::health::LogFormat::{CodexJson, Opaque, StreamJson};
    match tool {
        Tool::Codex => CodexJson,
        Tool::Claude | Tool::OpenCode => StreamJson,
        // Prose or build output: kiro, the one-shot calls, and the docker baselines carry no terminal
        // record at all.
        Tool::Kiro
        | Tool::Kimi
        | Tool::Oneshot
        | Tool::C2rust
        | Tool::Laertes
        | Tool::C2SaferRust
        | Tool::SmartC2Rust => Opaque,
    }
}

/// A step's wall-clock ceiling.
///
/// One table for both roles and both datasets, so a chain's steps cannot be given ceilings that
/// disagree by accident. harvest-bench projects are whole libraries: `libpng` translate measured
/// 4.71 h, `mujs` verify 6.5 h and `zstd` verify 4.4 h, which is why theirs is a day and the
/// test-corpus cases' is hours.
pub fn ceiling(tool: Tool, role: crate::prompt::Role, dataset: crate::cli::Dataset) -> u64 {
    use crate::cli::Dataset;
    use crate::prompt::Role;
    match (dataset, role) {
        (Dataset::HarvestBench, _) => 86_400,
        // kiro's own session limits are far shorter than the others', and a ceiling above them only
        // turns a refusal into a silent truncation.
        (Dataset::TestCorpus, Role::Translate) if tool == Tool::Kiro => 5_400,
        (Dataset::TestCorpus, Role::Verify) if tool == Tool::Kiro => 2_700,
        (Dataset::TestCorpus, Role::Translate) => 10_800,
        (Dataset::TestCorpus, Role::Verify) => 43_200,
    }
}

/// Everything one step needs to execute, resolved BEFORE the agent starts.
///
/// Owns its parts so `Invocation` can borrow them: the model and the CLI build are key and record
/// components, and neither is honest after the fact -- the CLIs auto-update through a shim, and a
/// resolved model appears only in the transcript, once the money is spent.
pub struct Built {
    model: ModelId,
    inner: cli::Cli,
}

impl Built {
    pub fn as_runner(&self) -> crate::invocation::Runner<'_> {
        if crate::cli::is_agentic(self.tool()) {
            crate::invocation::Runner::Agent {
                model: &self.model,
                exec: &self.inner,
            }
        } else {
            crate::invocation::Runner::Baseline { exec: &self.inner }
        }
    }

    fn tool(&self) -> Tool {
        self.inner.tool
    }
}

/// Resolve the backend for one step. ONE place, where `resolve_launch` and `verify_invocation` each
/// carried half -- so a model or a session flag could differ between a chain's two steps and the key
/// would record the divergence as two unrelated entries.
pub fn build(paths: &crate::battery::Paths, role: crate::prompt::Role) -> Result<Built> {
    let model = paths.model.clone().with_context(|| {
        format!(
            "--tool {} resolves no model",
            crate::cli::tool_dir(paths.tool)
        )
    })?;
    let secs = ceiling(paths.tool, role, paths.dataset);
    let (session, backend, program) = match paths.tool {
        Tool::Claude => (
            crate::agents::session::Session::claude(secs),
            cli::Backend::Claude,
            "claude",
        ),
        Tool::Codex => (
            crate::agents::session::Session::codex(secs),
            cli::Backend::Codex {
                region: codex_region(model.as_str()),
            },
            "codex",
        ),
        Tool::Kiro => (
            crate::agents::session::Session::kiro(secs),
            cli::Backend::Kiro,
            "kiro-cli",
        ),
        Tool::OpenCode => {
            let parsed = crate::agents::opencode::parse_model(model.as_str())?;
            (
                crate::agents::session::Session::opencode(
                    match role {
                        crate::prompt::Role::Translate => crate::agents::opencode::Phase::Translate,
                        crate::prompt::Role::Verify => crate::agents::opencode::Phase::Verify,
                    },
                    secs,
                ),
                cli::Backend::OpenCode {
                    model_arg: parsed.as_arg(),
                },
                "opencode",
            )
        }
        other => anyhow::bail!(
            "--tool {} has no runner wired yet; the docker and single-shot backends are not part of \
             this landing",
            crate::cli::tool_dir(other)
        ),
    };
    Ok(Built {
        model: model.clone(),
        inner: cli::Cli {
            session,
            backend,
            tool: paths.tool,
            model,
            cli_version: CliVersion::probe(program)?.as_str().to_string(),
            repo_root: paths.repo_root.clone(),
            enforcement: paths.enforcement,
        },
    })
}

/// Which AWS region a codex model is served from. One table, so the two steps of a chain cannot ask
/// for different ones.
pub fn codex_region(model: &str) -> &'static str {
    match model {
        m if m.contains("gpt-5.4") => "us-west-2",
        _ => "us-east-2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_either_pins_a_model_or_demands_one() {
        // A run with no pin is attributed to whichever model the CLI defaulted to that day. That is
        // not a hypothetical: it is what happened to every published kiro row.
        for tool in [
            Tool::Claude,
            Tool::Codex,
            Tool::Kiro,
            Tool::OpenCode,
            Tool::Oneshot,
            Tool::Kimi,
        ] {
            let pinned = resolve_model(tool, None);
            let with_flag = resolve_model(tool, Some("some/model-1"));
            assert!(
                matches!(pinned, Ok(Some(_))) || matches!(with_flag, Ok(Some(_))),
                "{tool:?} resolves no model either way"
            );
            if pinned.is_err() {
                assert!(
                    with_flag.is_ok(),
                    "{tool:?} refuses without --model but also refuses with it"
                );
            }
        }
        for tool in [
            Tool::C2rust,
            Tool::Laertes,
            Tool::C2SaferRust,
            Tool::SmartC2Rust,
        ] {
            assert!(
                matches!(resolve_model(tool, None), Ok(None)),
                "{tool:?} runs no model, so it must resolve none rather than refusing"
            );
        }
    }

    /// A codex run with no `--model` must still be a PINNED run. It used to refuse, which meant a
    /// three-way sweep could not name codex without also naming a model -- and the two older codex
    /// models are historical: both were fed claude's prompts and had no verify step.
    #[test]
    fn codex_defaults_to_the_model_that_is_comparable_to_claude() {
        let defaulted = resolve_model(Tool::Codex, None).unwrap().expect("a pin");
        assert_eq!(defaulted.as_str(), CODEX_MODEL_DEFAULT);
        assert_eq!(codex_region(defaulted.as_str()), "us-east-2");
        // And naming one still overrides it, or the historical models become unreachable.
        let named = resolve_model(Tool::Codex, Some("openai.gpt-5.4"))
            .unwrap()
            .expect("a pin");
        assert_eq!(named.as_str(), "openai.gpt-5.4");
        assert_eq!(codex_region(named.as_str()), "us-west-2");
        // The two whose model IS their identity still refuse: there is no canonical one to default.
        for tool in [Tool::Oneshot, Tool::OpenCode] {
            assert!(
                resolve_model(tool, None).is_err(),
                "{tool:?} has no defensible default model"
            );
        }
    }

    #[test]
    fn a_codex_model_is_served_from_the_region_it_was_deployed_to() {
        assert_eq!(codex_region("openai.gpt-5.4"), "us-west-2");
        assert_eq!(codex_region("openai.gpt-5.6-sol"), "us-east-2");
    }
}
