//! What a run will be, resolved before it starts: whether the agent has this phase at all,
//! the model that will actually be asked for, and the CLI build that will ask for it.
//!
//! Model and build are cache-key components, and both are only honest if they are decided
//! before the money is spent — the model the CLI settled on otherwise appears for the first
//! time in the transcript, after the run.

use crate::cache::{CliVersion, ModelId};
use crate::cli::Agent;
use anyhow::Result;
use std::path::Path;

/// The model every `--agent claude` invocation is pinned to.
///
/// Must be pinned explicitly: the CLI auto-updates, so an unpinned run is attributed
/// to whatever model it defaulted to that day. The cache key also has to name the
/// model *before* the run, and the resolved model only appears in the transcript's
/// `init` record — after the money is spent.
///
/// A Bedrock inference-profile id because `CLAUDE_CODE_USE_BEDROCK` is set;
/// `HARVEST_CLAUDE_MODEL` overrides it for an environment routed differently.
pub const CLAUDE_MODEL_DEFAULT: &str = "global.anthropic.claude-opus-5[1m]";

pub fn claude_model() -> Result<ModelId> {
    let raw =
        std::env::var("HARVEST_CLAUDE_MODEL").unwrap_or_else(|_| CLAUDE_MODEL_DEFAULT.to_string());
    ModelId::new(raw)
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

/// Whether this agent has a verify phase at all.
///
/// The same partition of `Agent` as `verify::verify_invocation`, minus the parts that need
/// `Paths`. Two pins hold it there, because the two functions are in different files and
/// each covers only one of the ways they can drift:
/// `verify::tests::a_verify_backend_resolves_exactly_where_a_verify_phase_is_declared`
/// against the backend match, and
/// `translate::tests::a_verify_prompt_exists_exactly_where_a_verify_phase_does` against the
/// prompt table.
pub fn has_verify_phase(agent: Agent) -> bool {
    matches!(agent, Agent::Kiro | Agent::Claude | Agent::OpenCode)
}
