use clap::Parser;

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
    /// Auto-detect from target name: "HB/<project>" or "HB" → HarvestBench,
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

    #[command(subcommand)]
    pub command: Command,
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
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
