//! Conditions under which a run is not a measurement at all.
//!
//! Each is refused where it is detected. The type exists so the join site that
//! collects per-case results can still tell a refusal from an ordinary failure: a red
//! X is a result, a refusal is the absence of one, and only the first may be scored.

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The CLI resolved a model other than the one the run is keyed and attributed to.
    ModelSubstituted { asked: String, got: String },
    /// The agent changed the C sources its translation is graded against.
    OracleModified { change: OracleChange, file: String },
    /// A C reference the guard can see no file in: it would compare nothing to nothing, so
    /// any later change to the reference would pass. Having none at all is a different state.
    OracleEmpty { at: String },
    /// `RUSTUP_TOOLCHAIN` is set, so the compiler is not the pinned one.
    ToolchainOverridden { value: String },
}

/// What the agent did to one file of the C reference. Named rather than a digest pair: a
/// deletion used to read as "the digest moved", which says neither which file nor that it is
/// gone. `Added` is an addition that is NOT compiled output — building the reference is what
/// the translate prompt asks for, so its output is not a change to it. `Hidden` is the file
/// still on disk that the artifact no longer contains, which no stat can see; `Symlinked` a
/// path under `c_src` that the harness would resolve somewhere it does not own.
#[derive(Debug, PartialEq, Eq)]
pub enum OracleChange {
    Edited,
    Removed,
    Added,
    Hidden,
    Symlinked,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::ModelSubstituted { asked, got } => write!(
                f,
                "the CLI resolved a different model than the pin: asked for {asked}, got \
                 {got}. Refusing to attribute this run to {asked}."
            ),
            Refusal::OracleModified { change, file } => write!(
                f,
                "the agent modified the C oracle source: {file} was {}. The C side is the \
                 reference the translation is graded against; a run that changes it has not \
                 been verified against the original program.",
                match change {
                    OracleChange::Edited => "edited",
                    OracleChange::Removed => "removed",
                    OracleChange::Added => "added, and is not a compiled build product",
                    OracleChange::Hidden => {
                        "hidden from the artifact: a directory around it now reads as a build \
                         tree, so neither the sealed digest nor the published result contains \
                         the reference at all"
                    }
                    OracleChange::Symlinked => {
                        "reached through a symlink, so what it names need not be inside the tree \
                         being graded: the harness reads and deletes these paths itself, and a \
                         link is how that reaches something the run does not own"
                    }
                }
            ),
            Refusal::OracleEmpty { at } => write!(
                f,
                "the C reference at {at} holds no file this check can see, so it would \
                 compare nothing to nothing and any later change to the reference would \
                 pass. Refusing rather than grading against nothing."
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

/// Refusals seen this sweep. A refusal is not a case failure: the checks that raise one
/// exist to stop a bad measurement, so the sweep finishes (surviving cases are not
/// wasted) and then the command fails.
static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Fold a case's outcome into a pass/fail, remembering any refusal.
pub fn record(case: &str, outcome: anyhow::Result<bool>) -> bool {
    match outcome {
        Ok(ok) => ok,
        Err(e) => {
            if let Some(r) = e.downcast_ref::<Refusal>() {
                eprintln!("  ⛔ {case}: {r}");
                if let Ok(mut g) = SEEN.lock() {
                    g.push(format!("{case}: {r}"));
                }
            } else {
                eprintln!("  {case}: {e:#}");
            }
            false
        }
    }
}

/// Fail the command if anything refused.
pub fn bail_if_any() -> anyhow::Result<()> {
    let seen = SEEN.lock().map(|g| g.clone()).unwrap_or_default();
    anyhow::ensure!(
        seen.is_empty(),
        "{} case(s) refused rather than failed: {}\n  \
         A refusal means the measurement is invalid, not that the translation is bad — \
         fix the cause and re-run before scoring.",
        seen.len(),
        seen.join("; ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_stays_recognisable_under_the_context_a_call_stack_adds() {
        // The join site sees the error only after `?` has carried it up through
        // `.context(..)`; if downcast did not see through those, every refusal would
        // arrive as an ordinary failure again.
        let err = anyhow::Error::from(Refusal::ToolchainOverridden {
            value: "1.97.1".into(),
        })
        .context("verifying libpng")
        .context("harvest-bench sweep");
        assert_eq!(
            Refusal::in_chain(&err),
            Some(&Refusal::ToolchainOverridden {
                value: "1.97.1".into()
            }),
            "{err:#}"
        );
        assert!(format!("{err:#}").contains("RUSTUP_TOOLCHAIN"));
    }

    #[test]
    fn an_ordinary_failure_is_not_mistaken_for_a_refusal() {
        assert!(Refusal::in_chain(&anyhow::anyhow!("no Cargo.toml produced")).is_none());
    }
}
