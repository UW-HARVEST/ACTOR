use anyhow::{Context, Result};
use clap::Parser;

/// The name `--agent` accepts, from clap's derived `ValueEnum` mapping.
///
/// The ONE spelling of an agent, so a hint, a results dir and a cache key cannot
/// disagree about it. Fallible only because `ValueEnum` permits a `#[value(skip)]`
/// variant with no name; none of these are.
pub fn cli_name(agent: Agent) -> Result<String> {
    use clap::ValueEnum;
    Ok(agent
        .to_possible_value()
        .context("agent variant has no --agent name")?
        .get_name()
        .to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    Kiro,
    Claude,
    /// Claude Code with combined translate+verify in a single session.
    /// Reads translate-and-verify-{exec,lib,shared}.md instead of separate prompts.
    /// Verify phase is skipped.
    ClaudeCombined,
    /// Claude Code with a minimal universal prompt (calibration baseline for
    /// prompt-sensitivity ablation). One prompt for all project types, no
    /// engineered guidance about cdylib, FFI types, namespace macros, etc.
    /// Verify phase is skipped.
    ClaudeMinimal,
    /// Claude Code with engineered prompts but no iteration-loop instructions
    /// (E3 prompt-sensitivity ablation). Same project-type dispatch and FFI
    /// guidance as `claude`, but the prompt does not tell the agent to run
    /// `cargo build`/`cargo test` and fix errors. Tests whether the iteration
    /// loop in the prompt is actually load-bearing.
    /// Verify phase is skipped.
    ClaudeNoIter,
    /// Claude Code with engineered prompts but no cmake-features → cargo-features
    /// guidance (E2 prompt-sensitivity ablation). Identical to `claude` except
    /// the shared-source prompt strips the build-time configurability section.
    /// Tests whether the cmake-features dispatch is what carries P01_sphincs_plus.
    /// Verify phase is skipped.
    ClaudeNoFeatures,
    /// Claude Code with engineered prompts but no subtask-decomposition
    /// guidance (E6 prompt-sensitivity ablation). Identical to `claude` except
    /// the shared-source prompt drops the "create a TODO list, work through
    /// subtasks one at a time" block. Tests whether explicit decomposition
    /// guidance is needed for large multi-file projects (P01_sphincs_plus).
    /// Verify phase is skipped.
    ClaudeNoSubtask,
    /// Claude Code with project-type prompts SWAPPED (E4 prompt-sensitivity
    /// ablation). Libraries get translate-executable.md, executables get
    /// translate-library.md. Directly answers Reviewer 2's question: "What
    /// happens when translate-library.md is used to translate an executable?"
    /// Tests whether the project-type dispatch is structurally necessary.
    /// Verify phase is skipped.
    ClaudeCrossPrompt,
    /// OpenAI Codex CLI on Amazon Bedrock with gpt-5.5 (us-east-2).  Same
    /// methodology as `claude`: same prompts, translate-then-verify pipeline.
    /// Used to validate that ACTOR is portable across multiple agentic
    /// harnesses (Kiro CLI, Claude Code, Codex).  All requests go through
    /// Bedrock; OpenAI telemetry/auth is disabled.
    CodexGpt55,
    /// OpenAI Codex CLI on Amazon Bedrock with gpt-5.4 (us-west-2).  Same
    /// methodology as `CodexGpt55`; different model version for cross-model
    /// comparison within the Codex harness.
    CodexGpt54,
    /// OpenCode CLI, with the model chosen by `--model <provider>/<model-id>`
    /// (e.g. `amazon-bedrock/us.anthropic.claude-sonnet-5`). Same methodology
    /// as `claude`: the SAME `prompts/claude/*.md` prompts and the same
    /// translate-then-verify pipeline, so a result is comparable to the Claude
    /// Code and Kiro runs. This is the backend that decouples ACTOR from any
    /// one vendor's CLI, letting any Bedrock-hosted model be evaluated.
    #[value(name = "opencode", alias = "oc")]
    OpenCode,
    C2rust,
    Laertes,
    /// C2SaferRust: post-processes C2Rust output with an LLM to reduce unsafe
    /// code. Runs in Docker from the pinned `c2saferrust/` submodule (our fork's
    /// `bedrock` branch), driven by gpt-5.4 via Amazon Bedrock. Like Laertes, it
    /// consumes this repo's c2rust `translated_rust_original` as input and has no
    /// separate C-as-oracle verify phase.
    #[value(name = "c2saferrust")]
    C2SaferRust,
    /// SmartC2Rust: from-C translator (segment + LLM + feedback repair), preserve
    /// FFI mode, run in Docker via Amazon Bedrock Claude. Translation is driven by
    /// an external fixture pipeline; harvest-tools is used here only to score the
    /// collected results, so it has no in-tool translate/verify phase.
    #[value(name = "smartc2rust")]
    SmartC2Rust,
    Kimi,
    Oneshot,
}

impl Agent {
    /// What this agent's log can prove about completion.
    ///
    /// The ONE table, matched exhaustively so a new variant is a compile error rather
    /// than a silent inheritance of the wrong classifier. Getting this wrong is not
    /// cosmetic: `crate::domain::health::classify` mints the proof
    /// `crate::artifact::Scrubbed::seal` demands, so an agent wrongly marked
    /// `StreamJson` can never publish, and one wrongly marked `Opaque` would be
    /// sealed on the strength of an exit code its own log contradicts.
    pub fn log_format(self) -> crate::domain::health::LogFormat {
        use crate::domain::health::LogFormat::{Opaque, StreamJson};
        match self {
            // Claude Code and the codex CLIs emit `--output-format stream-json`.
            Agent::Claude
            | Agent::ClaudeCombined
            | Agent::ClaudeMinimal
            | Agent::ClaudeNoIter
            | Agent::ClaudeNoFeatures
            | Agent::ClaudeNoSubtask
            | Agent::ClaudeCrossPrompt
            | Agent::CodexGpt55
            | Agent::CodexGpt54
            | Agent::OpenCode => StreamJson,
            // kiro-cli writes prose ("Credits: ..."); kimi and oneshot write prose;
            // c2rust writes cmake/cargo output; laertes, c2saferrust and smartc2rust
            // write docker output. None carries a terminal record.
            Agent::Kiro
            | Agent::C2rust
            | Agent::Laertes
            | Agent::C2SaferRust
            | Agent::SmartC2Rust
            | Agent::Kimi
            | Agent::Oneshot => Opaque,
        }
    }
}

/// Which benchmark dataset to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dataset {
    /// HARVEST test-corpus (MIT runtests).
    TestCorpus,
    /// harvest-bench: real C libraries scored by their upstream GoogleTest
    /// suites, linked against the translated cdylib by ABI. Targets are
    /// `HB/<project>` (e.g. `HB/libsodium`) or `HB` for all.
    HarvestBench,
}

impl Dataset {
    /// Auto-detect from target name: `HB/<project>` or `HB` → HarvestBench,
    /// else TestCorpus.
    pub fn detect(target: &str) -> Self {
        if target.eq_ignore_ascii_case("hb") || target.starts_with("HB/") {
            Self::HarvestBench
        } else {
            Self::TestCorpus
        }
    }

    /// Strip the "HB/" prefix if present, returning the inner target.
    pub fn strip_prefix(target: &str) -> &str {
        target.strip_prefix("HB/").unwrap_or(target)
    }
}

#[derive(Parser)]
// `version` is a function call, so it cannot be a derive attribute literal; it is
// applied in `parse_args` instead. See `Cli::parse_args`.
#[command(name = "harvest-tools", about = "C-to-Rust translation pipeline")]
pub struct Cli {
    /// Which LLM agent to use for translation
    #[arg(long, value_enum, default_value_t = Agent::Kiro)]
    pub agent: Agent,

    /// Model ID. Required with `--agent oneshot` (OpenRouter form, e.g.
    /// "openai/gpt-5.4") and with `--agent opencode` (OpenCode
    /// `<provider>/<model-id>` form, e.g.
    /// "amazon-bedrock/us.anthropic.claude-sonnet-5"). Rejected for any other
    /// agent, whose model is fixed by the agent variant.
    #[arg(long)]
    pub model: Option<String>,

    /// How to use the agent-invocation cache.
    #[arg(long, value_enum, default_value_t = CacheMode::On)]
    pub cache: CacheMode,

    /// Produce artifacts even though the code cannot be identified — an
    /// uncommitted tree, or a binary built from a different commit.
    ///
    /// Legitimate while iterating locally. The reason is printed and every artifact
    /// is stamped `<sha>-dirty`, so a run made this way cannot later be mistaken for
    /// a reproducible one. See `crate::provenance`.
    #[arg(long, global = true)]
    pub allow_dirty: bool,

    /// Launch agents even though the filesystem sandbox cannot be enforced.
    ///
    /// Without `bwrap` and `socat` the CLI degrades to unsandboxed, leaving the graded
    /// oracle and every sibling work dir readable. Artifacts are stamped so such a run
    /// cannot later be mistaken for a sandboxed one.
    #[arg(long, global = true)]
    pub allow_unsandboxed: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// The user-facing spelling of [`crate::cache::Mode`].
///
/// Kept separate from the internal enum so the CLI vocabulary and the store's
/// behaviour can be documented in their own terms, and so `clap` derives stay out
/// of the cache module.
#[derive(Copy, Clone, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum CacheMode {
    /// Reuse a stored result when every key input matches; store new ones.
    On,
    /// Ignore the cache entirely — neither read nor write. For sampling how much an
    /// agent varies between runs, which memoising would defeat.
    Off,
    /// Re-run even when a result is stored, and replace what was there. For when the
    /// stored artifact is suspect: leaving it in place would keep serving it. The
    /// replaced entry is kept under `results/.cache/quarantine/`, since it is the
    /// artifact being disputed.
    Refresh,
}

impl From<CacheMode> for crate::cache::Mode {
    fn from(m: CacheMode) -> Self {
        match m {
            CacheMode::On => crate::cache::Mode::ReadWrite,
            CacheMode::Off => crate::cache::Mode::Bypass,
            CacheMode::Refresh => crate::cache::Mode::Refresh,
        }
    }
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// Translate + verify + test (full pipeline)
    Run {
        /// Battery name or battery/case
        target: String,
        /// Skip verification phase
        #[arg(long)]
        no_verify: bool,
        /// Only process cases matching regex
        #[arg(long)]
        include_regex: Option<String>,
        /// Max parallel translations
        #[arg(long, default_value_t = 1)]
        parallel: usize,
    },
    /// Translate C to Rust
    Translate {
        target: String,
        #[arg(long)]
        include_regex: Option<String>,
        #[arg(long, default_value_t = 1)]
        parallel: usize,
    },
    /// C-as-oracle verification
    Verify {
        target: String,
        #[arg(long)]
        include_regex: Option<String>,
        /// Re-verify already-verified cases
        #[arg(long)]
        force: bool,
        #[arg(long, default_value_t = 1)]
        parallel: usize,
    },
    /// Inspect the agent-invocation cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Run MIT runtests
    Test {
        target: String,
        /// Update stored expected results
        #[arg(long)]
        update: bool,
        /// CI mode: compare against stored summary.json, exit 1 on mismatch
        #[arg(long, conflicts_with = "update")]
        check: bool,
        /// Score even though some agent runs died on infrastructure (expired
        /// credentials, rate limiting, a truncated log).
        ///
        /// Scoring refuses by default, because a case whose agent never ran has
        /// no measurement and reporting one is how 7 harvest-bench projects
        /// became "3/5 pass" on 2026-08-14. Pass this only when you know why the
        /// runs died and want the numbers anyway; the affected cases are listed
        /// and written to `INFRA_FAILURES.json` beside the results.
        #[arg(long)]
        allow_infra_failures: bool,
    },
    /// Backfill result.json with credits + unsafe counts (no tests, no LLM calls)
    Enrich { target: String },
    /// Generate markdown report tables from validated results into tables/
    Report,
}

// Execution is now driven by the `Benchmark` trait (see `benchmark.rs`): one
// lifecycle parameterized per dataset, rather than a plan enum + match ladder
// per phase. The old TranslatePlan / VerifyPlan / TestPlan enums are gone.

impl Cli {
    /// Parse argv, with `--version` reporting the commit, compiler and target.
    ///
    /// Built at run time rather than as a `#[command(version)]` literal, because the
    /// string is assembled from several `vergen` stamps. `--version` is how you audit
    /// a binary you did not build yourself, and it is the ecosystem's normal answer
    /// to "which code is this?" — the refusal in `crate::provenance` is the stricter,
    /// repo-specific half.
    ///
    /// The `expect` cannot fire, so this returns `Self` rather than a `Result`.
    /// `get_matches()` has already diagnosed and exited on every user-input error
    /// (unknown flag, bad value, and — because the non-`Option` `command` field
    /// makes the derive set `subcommand_required`/`arg_required_else_help` — a
    /// missing subcommand). What reaches `from_arg_matches` is therefore the
    /// derive's own `Command` matched against itself, with only `.version(..)`
    /// added, which does not touch the arg structure. It is that pairing the
    /// message records: a future `Self::command()` that dropped an arg or relaxed
    /// `subcommand_required` would be the bug, and it belongs at the top of a
    /// backtrace here rather than behind an error type `main` could not act on.
    pub fn parse_args() -> Self {
        use clap::{CommandFactory, FromArgMatches};
        let matches = Self::command()
            .version(crate::provenance::version_string())
            .get_matches();
        Self::from_arg_matches(&matches).expect("clap derive produces a valid parser")
    }
}

#[derive(clap::Subcommand)]
pub enum CacheAction {
    /// Entry count and size on disk.
    Stats,
}

impl Command {
    /// Whether this command writes an artifact that a paper number could rest on.
    ///
    /// The single source of truth for the provenance preflight. Exhaustive on
    /// purpose: a new subcommand cannot be added without deciding which side it is
    /// on, rather than defaulting to unchecked.
    pub fn produces_artifacts(&self) -> bool {
        match self {
            // Write into results/ or tables/.
            Command::Run { .. }
            | Command::Translate { .. }
            | Command::Verify { .. }
            | Command::Test { .. }
            | Command::Enrich { .. }
            | Command::Report => true,
            // Read-only introspection.
            Command::Cache { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::health::LogFormat;

    #[test]
    fn every_agent_declares_a_log_format_and_the_prose_ones_are_opaque() {
        // Guards the table against a new variant defaulting to the wrong classifier.
        for a in [
            Agent::Claude,
            Agent::ClaudeCombined,
            Agent::ClaudeMinimal,
            Agent::ClaudeNoIter,
            Agent::ClaudeNoFeatures,
            Agent::ClaudeNoSubtask,
            Agent::ClaudeCrossPrompt,
            Agent::CodexGpt55,
            Agent::CodexGpt54,
            Agent::OpenCode,
        ] {
            assert_eq!(a.log_format(), LogFormat::StreamJson, "{a:?}");
        }
        for a in [
            Agent::Kiro,
            Agent::C2rust,
            Agent::Laertes,
            Agent::C2SaferRust,
            Agent::SmartC2Rust,
            Agent::Kimi,
            Agent::Oneshot,
        ] {
            assert_eq!(a.log_format(), LogFormat::Opaque, "{a:?}");
        }
    }
}
