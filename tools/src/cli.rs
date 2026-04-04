use clap::Parser;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    Kiro,
    Claude,
    C2rust,
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
    },
    /// Translate C to Rust
    Translate {
        target: String,
        #[arg(long)]
        include_regex: Option<String>,
    },
    /// C-as-oracle verification
    Verify {
        target: String,
        #[arg(long)]
        include_regex: Option<String>,
        /// Re-verify already-verified cases
        #[arg(long)]
        force: bool,
    },
    /// Run MIT runtests
    Test {
        target: String,
        /// Update stored expected results
        #[arg(long)]
        update: bool,
    },
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
