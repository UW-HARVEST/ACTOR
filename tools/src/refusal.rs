//! Conditions under which a run is not a measurement at all.
//!
//! Each is refused where it is detected, and each message has to tell an operator what to change.
//!
//! The oracle-tamper variants are gone with the check itself: a working dir is assembled with the
//! corpus's own `c_src`, so tampering cannot persist to be detected (CLAUDE.md, "Restore rather than
//! detect"). The per-sweep refusal registry went with the two phase drivers.

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The CLI resolved a model other than the one the run is keyed and attributed to.
    ModelSubstituted { asked: String, got: String },
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
            Refusal::ToolchainOverridden { value } => write!(
                f,
                "RUSTUP_TOOLCHAIN is set ({value}), which silently overrides \
                 rust-toolchain.toml. Unset it (`env -u RUSTUP_TOOLCHAIN`) so the pinned \
                 compiler is the one that builds what gets scored."
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Refuse a run whose compiler is not the pinned one.
///
/// [`Refusal::ToolchainOverridden`] had nothing constructing it: the check went with the two phase
/// drivers, leaving one shell script's `unset` line as the only guard. This box's shell exports it.
pub fn require_pinned_toolchain() -> anyhow::Result<()> {
    match std::env::var("RUSTUP_TOOLCHAIN") {
        Ok(value) if !value.trim().is_empty() => Err(Refusal::ToolchainOverridden { value }.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal an operator cannot act on is a dead end: both of these name the one thing to change.
    #[test]
    fn every_refusal_names_what_to_change() {
        let toolchain = Refusal::ToolchainOverridden {
            value: "1.97.1".into(),
        }
        .to_string();
        assert!(toolchain.contains("RUSTUP_TOOLCHAIN") && toolchain.contains("1.97.1"));
        let model = Refusal::ModelSubstituted {
            asked: "claude-opus-5".into(),
            got: "claude-sonnet-4".into(),
        }
        .to_string();
        assert!(model.contains("claude-opus-5") && model.contains("claude-sonnet-4"));
    }
}
