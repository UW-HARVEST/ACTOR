use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "harvest-benchmark")]
#[command(
    about = "Runs all benchmarks by translating C projects to Rust and validating them with test vectors"
)]
pub struct Args {
    /// Input directory containing subdirectories with benchmarks
    #[arg(
        help = "Path to the directory containing example subdirectories (each with test_case/ and test_vectors/)"
    )]
    pub input_dir: PathBuf,

    /// Output directory where the translated Rust projects will be written
    #[arg(help = "Path to the output directory for all translated Rust projects")]
    pub output_dir: PathBuf,

    /// Use modular translation rather than standard all-at-once translation.
    #[arg(long)]
    pub modular: bool,

    /// Set a configuration value; format $NAME=$VALUE.
    #[arg(long, short)]
    pub config: Vec<String>,

    /// Timeout in seconds for running test cases
    #[arg(long, default_value = "10")]
    pub timeout: u64,

    /// Filter benchmarks by regex pattern on directory names (keeps matching directories).
    /// Examples: ".*_lib$" (only libraries)
    /// Cannot be used together with --exclude.
    #[arg(long, conflicts_with = "exclude")]
    pub filter: Option<String>,

    /// Exclude benchmarks by regex pattern on directory names (removes matching directories).
    /// Examples: ".*_lib$" (exclude libraries)
    /// Cannot be used together with --filter.
    #[arg(long, conflicts_with = "filter")]
    pub exclude: Option<String>,
    /// Use pre-translated results instead of running translation.
    /// When set, input_dir is the test corpus and output_dir has existing translations.
    #[arg(long)]
    pub pre_translated: bool,

    /// Include test vectors with undefined behavior (skipped by default).
    #[arg(long)]
    pub include_ub: bool,
}
