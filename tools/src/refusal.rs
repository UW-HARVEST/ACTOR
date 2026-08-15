//! Conditions under which a run is not a measurement at all.
//!
//! Each is refused where it is detected. The type exists so the join site that
//! collects per-case results can still tell a refusal from an ordinary failure: a red
//! X is a result, a refusal is the absence of one, and only the first may be scored.

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The CLI resolved a model other than the one the run is keyed and attributed to.
    ModelSubstituted { asked: String, got: String },
    /// The agent edited the C sources its translation is graded against.
    OracleModified { before: String, after: String },
    /// `RUSTUP_TOOLCHAIN` is set, so the compiler is not the pinned one.
    ToolchainOverridden { value: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::ModelSubstituted { asked, got } => write!(
                f,
                "the CLI resolved a different model than the pin: asked for {asked}, got \
                 {got}. Refusing to attribute this run to {asked}."
            ),
            Refusal::OracleModified { before, after } => write!(
                f,
                "the agent modified the C oracle source: {before} before, {after} after. \
                 The C side is the reference the translation is graded against; a run that \
                 changes it has not been verified against the original program."
            ),
            Refusal::ToolchainOverridden { value } => write!(
                f,
                "RUSTUP_TOOLCHAIN is set ({value}), which silently overrides \
                 rust-toolchain.toml. Unset it (`env -u RUSTUP_TOOLCHAIN`) so the pinned \
                 compiler is used and the cache key reflects it."
            ),
        }
    }
}

impl std::error::Error for Refusal {}

impl Refusal {
    /// The refusal in `err`'s chain, if any.
    pub fn in_chain(err: &anyhow::Error) -> Option<&Refusal> {
        err.downcast_ref::<Refusal>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_stays_recognisable_under_the_context_a_call_stack_adds() {
        // The join site sees the error only after `?` has carried it up through
        // `.context(..)`; if downcast did not see through those, every refusal would
        // arrive as an ordinary failure again.
        let err = anyhow::Error::from(Refusal::ToolchainOverridden { value: "1.97.1".into() })
            .context("verifying libpng")
            .context("harvest-bench sweep");
        assert_eq!(
            Refusal::in_chain(&err),
            Some(&Refusal::ToolchainOverridden { value: "1.97.1".into() }),
            "{err:#}"
        );
        assert!(format!("{err:#}").contains("RUSTUP_TOOLCHAIN"));
    }

    #[test]
    fn an_ordinary_failure_is_not_mistaken_for_a_refusal() {
        assert!(Refusal::in_chain(&anyhow::anyhow!("no Cargo.toml produced")).is_none());
    }
}
