use clap::Parser;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    Kiro,
    Claude,
    C2rust,
}

/// Which benchmark dataset to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dataset {
    /// HARVEST test-corpus (MIT runtests).
    TestCorpus,
    /// CRUST-bench (cargo test).
    Crust,
}

impl Dataset {
    /// Auto-detect from target name: "CRUST/<project>" or "CRUST" → Crust, else TestCorpus.
    pub fn detect(target: &str) -> Self {
        if target.eq_ignore_ascii_case("crust") || target.starts_with("CRUST/") {
            Self::Crust
        } else {
            Self::TestCorpus
        }
    }

    /// Strip the "CRUST/" prefix if present, returning the inner target.
    pub fn strip_prefix(target: &str) -> &str {
        target.strip_prefix("CRUST/").unwrap_or(target)
    }
}

#[derive(Parser)]
#[command(name = "harvest-tools", about = "C-to-Rust translation pipeline")]
pub struct Cli {
    /// Which LLM agent to use for translation
    #[arg(long, value_enum, default_value_t = Agent::Kiro)]
    pub agent: Agent,

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
        /// Max number of cases to process
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Translate C to Rust
    Translate {
        target: String,
        #[arg(long)]
        include_regex: Option<String>,
        #[arg(long, default_value_t = 1)]
        parallel: usize,
        /// Max number of cases to process
        #[arg(long)]
        limit: Option<usize>,
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
    },
}

// ── Type-safe execution plans ──────────────────────────────────────────
// Each variant carries only the parameters valid for that dataset.
// Invalid combinations (e.g. limit on TestCorpus) are unrepresentable.

/// Plan for translation. Constructed once, consumed by translate module.
pub enum TranslatePlan {
    TestCorpus {
        batteries: Vec<String>,
        parallel: usize,
    },
    Crust {
        projects: Vec<super::battery::CrustProject>,
        parallel: usize,
    },
}

/// Plan for verification. CRUST has no verify step.
pub enum VerifyPlan {
    TestCorpus {
        batteries: Vec<String>,
        parallel: usize,
        force: bool,
    },
    Skip,
}

/// Plan for testing.
pub enum TestPlan {
    TestCorpus {
        batteries: Vec<String>,
        mode: super::test::TestMode,
    },
    Crust {
        projects: Vec<super::battery::CrustProject>,
        mode: super::test::TestMode,
    },
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
