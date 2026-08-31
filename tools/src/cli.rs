use clap::Parser;
/// WHICH program runs an invocation.
///
/// Ten variants, not eighteen. The six `claude-*` ablations were the same tool at a different
/// prompt, so they are [`Variant`]s now; the three codex entries were the same tool at a different
/// model, so they are `--model` values. Neither distinction ever belonged in this enum -- the key
/// carries tool, model and prompt separately, so encoding two of them in the third could only make
/// them disagree.
#[derive(clap::ValueEnum, Copy, Clone, PartialEq, Eq, Debug)]
pub enum Tool {
    /// Claude Code.
    Claude,
    /// OpenAI Codex CLI on Amazon Bedrock. The model is `--model`: gpt-5.4, gpt-5.5 and gpt-5.6-sol
    /// were three enum variants and are one tool at three models.
    Codex,
    /// kiro-cli. Takes `--model` like every other agent -- a comment once claimed otherwise, and
    /// because nothing passed the flag, every kiro row ever published is unattributable.
    Kiro,
    /// OpenCode CLI; `--model <provider>/<model-id>` chooses the backend model.
    #[value(name = "opencode", alias = "oc")]
    OpenCode,
    /// One-shot LLM call, no agentic loop. `--model` is its identity.
    Oneshot,
    /// Kimi via Bedrock, one shot.
    Kimi,
    /// c2rust: a transpiler. Deterministic, so nothing is gained by keying it.
    C2rust,
    Laertes,
    #[value(name = "c2saferrust")]
    C2SaferRust,
    #[value(name = "smartc2rust")]
    SmartC2Rust,
}

/// WHICH prompt an invocation is handed, where a tool has more than one.
///
/// These were `Agent` variants (`claude-no-iter` and friends). They are prompts: the tool, the model
/// and the CLI are identical, and only the text differs -- which the cache key already separates by
/// prompt hash. Naming them here instead lets one tool carry an experiment without a new backend.
#[derive(clap::ValueEnum, Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Variant {
    #[default]
    Default,
    /// Translate and verify in one session; the chain is one step because the prompt does both.
    Combined,
    /// One minimal prompt for every shape -- the calibration baseline.
    Minimal,
    /// Engineered prompts with the iteration loop removed (E3).
    NoIter,
    /// No cmake-features to cargo-features guidance (E2).
    NoFeatures,
    /// No subtask-decomposition guidance (E6).
    NoSubtask,
    /// Shapes deliberately swapped: a library gets the executable prompt (E4).
    CrossPrompt,
    /// The library prompt with the C-wrapping loophole closed: every exported symbol must be Rust.
    /// codex answered all seven harvest-bench projects by compiling the C from `build.rs`, so `default`
    /// measures that and this measures translation. A variant, not an edit to the shared prompt that
    /// Test-Corpus library cases also read.
    NoShim,
}

impl Variant {
    /// The results directory level. `default` is spelled out rather than omitted: a level that
    /// appears only sometimes is the ragged tree the model level already taught us to avoid.
    pub fn dir(self) -> &'static str {
        match self {
            Variant::Default => "default",
            Variant::Combined => "combined",
            Variant::Minimal => "minimal",
            Variant::NoIter => "no-iter",
            Variant::NoFeatures => "no-features",
            Variant::NoSubtask => "no-subtask",
            Variant::CrossPrompt => "cross-prompt",
            Variant::NoShim => "no-shim",
        }
    }
}

/// The directory level ABOVE the model, and the spelling `--tool` accepts, so a path cannot drift
/// from the CLI surface. Named `tool_dir`, not `harness_dir`: "harness" means the ACTOR commit
/// everywhere else in this repo.
pub fn tool_dir(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "claude",
        Tool::Codex => "codex",
        Tool::Kiro => "kiro",
        Tool::OpenCode => "opencode",
        Tool::Oneshot => "oneshot",
        Tool::Kimi => "kimi",
        Tool::C2rust => "c2rust",
        Tool::Laertes => "laertes",
        Tool::C2SaferRust => "c2saferrust",
        Tool::SmartC2Rust => "smartc2rust",
    }
}

/// Whether this tool runs an agentic loop, and so whether the store may name its runs.
///
/// A transpiler is deterministic and a one-shot call is cheap; neither is worth memoising, and
/// neither has the iterating session that makes a cache entry valuable.
pub fn is_agentic(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::Claude | Tool::Codex | Tool::Kiro | Tool::OpenCode
    )
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
    /// Which program runs the invocations. Comma-separated to run several at once:
    /// `--tool claude,codex,kiro`.
    ///
    /// Each tool gets its own results tree, its own store prefix and its own concurrency budget, so
    /// `--parallel 3` with three tools is three in flight PER TOOL. Their tables are written once,
    /// from the three attestations merged -- one run per tool would have each rewrite `tables/` from
    /// its own rows and blank the others'.
    #[arg(long, value_enum, value_delimiter = ',', default_value = "claude")]
    pub tool: Vec<Tool>,

    /// Which prompt set. An ablation is a prompt, not a tool: the program, the model and the CLI are
    /// identical and only the text differs, which the key separates by prompt hash.
    #[arg(long = "prompt", value_enum, default_value_t = Variant::Default)]
    pub variant: Variant,

    /// Max concurrent invocations PER TOOL.
    ///
    /// Beside `--tool` because it is that list's budget: with three tools this is three in flight
    /// each, nine in total. `global`, so either position parses -- without it `run HB --parallel 3` is
    /// a clap error, which is how a sweep launcher died on `unexpected argument`.
    #[arg(long, global = true, default_value_t = 1)]
    pub parallel: usize,

    /// Model id. Required for `--tool oneshot` and `opencode`, where the model IS the
    /// identity; fixed by the tool otherwise.
    #[arg(long)]
    pub model: Option<String>,

    /// Replay stored results and refuse to invoke an agent: a cache miss is an error, not a run.
    ///
    /// What a reproducibility check needs. `tools/reproduce.sh` re-derives the numbers from the store
    /// and must be incapable of spending money, so a miss stops the sweep by name instead of quietly
    /// paying for the case and reporting a figure nobody stored. A prompt or model change moves the
    /// key and therefore misses -- deliberately, because the stored results no longer answer the
    /// question being asked.
    ///
    /// There is no "ignore the cache" or "replace the entry" flag. One key maps to one entry, so
    /// sampling variance has nowhere to be stored, and the two bools that used to mean opposite
    /// things to the store are gone with the modes they selected.
    #[arg(long)]
    pub replay_only: bool,

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

    /// Leave `.eval/` on disk after scoring, for a post-mortem of a build failure. It is otherwise
    /// removed: a tree left standing is one a later run could read instead of materialising its own.
    #[arg(long, global = true)]
    pub keep_eval_tree: bool,

    /// Score the cases that DID run, even though some agent run died for infrastructure reasons.
    ///
    /// An infrastructure failure is not a result, so a score refuses by default and the unit goes
    /// unpublished. The escape hatch for a sweep that lost its credentials hours in; what it lets
    /// through is announced as unsupported. Two refusal messages named this flag while nothing read it.
    #[arg(long, global = true)]
    pub allow_infra_failures: bool,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// THE mapping from what the operator typed to what the store does. One flag, two modes: there
    /// is nothing to resolve by precedence now that "ignore the cache" and "replace the entry" are
    /// gone with the modes they selected.
    pub fn cache_mode(&self) -> crate::store::Mode {
        if self.replay_only {
            crate::store::Mode::ReplayOnly
        } else {
            crate::store::Mode::ReadWrite
        }
    }
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// Run the chain: every step the tool's prompt variant declares, then score, then tables.
    ///
    /// The only pipeline. `translate` and `verify` were separate subcommands because the harness
    /// modelled them as different kinds of operation; they are one function at two prompts, so
    /// `--steps` takes a prefix of the chain instead.
    Run {
        /// Battery name, `battery/case`, `all`, or `HB`.
        target: String,
        /// Run only the first N steps of the chain.
        #[arg(long)]
        steps: Option<usize>,
        /// Only process cases matching this regex.
        #[arg(long)]
        include_regex: Option<String>,
    },
    /// Inspect the agent-invocation cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Backfill result.json with credits + unsafe counts (no tests, no LLM calls)
    Enrich { target: String },
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
    /// Entries whose run did not complete. Recorded and never served: a failure is an entry with a
    /// non-`Completed` outcome, not a separate tree.
    Failures,
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
            Command::Run { .. } | Command::Enrich { .. } => true,
            // Read-only introspection.
            Command::Cache { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--replay-only` is the flag `tools/reproduce.sh` relies on to be unable to spend money, so
    /// the thing under test is the MAPPING from typed flags to store mode -- not a hand-built `Mode`.
    /// Parsed through `clap`, because a hand-built `Cli` would skip the parsing being asserted.
    #[test]
    fn replay_only_is_the_only_flag_that_stops_the_store_paying() {
        use clap::Parser;
        let parse = |args: &[&str]| -> anyhow::Result<crate::store::Mode> {
            let mut argv = vec!["harvest-tools"];
            argv.extend_from_slice(args);
            argv.extend_from_slice(&["run", "B01"]);
            Ok(Cli::try_parse_from(argv)?.cache_mode())
        };
        assert_eq!(parse(&[]).unwrap(), crate::store::Mode::ReadWrite);
        assert_eq!(
            parse(&["--replay-only"]).unwrap(),
            crate::store::Mode::ReplayOnly
        );
    }

    /// `--tool claude,codex,kiro --parallel 3` is the form a three-way sweep is launched with, so the
    /// parse is worth pinning: a `--tool` that took only the LAST value would silently run one tool
    /// and report it as three.
    #[test]
    fn one_tool_flag_names_every_tool_a_sweep_runs() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "harvest-tools",
            "--tool",
            "claude,codex,kiro",
            "--parallel",
            "3",
            "run",
            "all",
        ])
        .expect("the comma-separated form must parse");
        assert_eq!(cli.tool, vec![Tool::Claude, Tool::Codex, Tool::Kiro]);
        assert_eq!(
            cli.parallel, 3,
            "and the budget is three PER tool, not three shared"
        );

        // AFTER the subcommand too, where an operator naturally puts a per-run budget.
        let trailing = Cli::try_parse_from([
            "harvest-tools",
            "--tool",
            "claude",
            "run",
            "HB",
            "--parallel",
            "3",
            "--allow-infra-failures",
        ])
        .expect("both flags are global, so either position parses");
        assert_eq!(trailing.parallel, 3);
        assert!(
            trailing.allow_infra_failures,
            "a flag parsed and then discarded is this repo's most repeated defect; two refusal \
             messages named this one while nothing read it"
        );
        // Repeating the flag is the other spelling of the same thing.
        let repeated = Cli::try_parse_from([
            "harvest-tools",
            "--tool",
            "claude",
            "--tool",
            "kiro",
            "run",
            "all",
        ])
        .expect("repeated flags must parse");
        assert_eq!(repeated.tool, vec![Tool::Claude, Tool::Kiro]);
        // And the default is still a single tool, not an empty list.
        assert_eq!(
            Cli::try_parse_from(["harvest-tools", "run", "all"])
                .unwrap()
                .tool,
            vec![Tool::Claude]
        );
    }
}
