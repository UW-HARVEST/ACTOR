//! Which code produced a result — and a refusal to measure when we cannot say.
//!
//! # The failure this exists to prevent
//!
//! On 2026-08-14 a harvest-bench sweep ran for twelve hours and billed $625 against
//! a binary built from `prompt-merge-62-63`, seven commits behind `main`. It
//! therefore contained none of the infra-failure gate (#67), the typed artifacts
//! (#73) or the agent cache (#74) — the very things the run was supposed to
//! exercise. A `strings` probe of that binary found zero references to
//! `INFRA_FAILURES` or the cache.
//!
//! Nothing caught it, for one reason: **the driver referenced a path, and a path has
//! no identity.** `BIN=./tools/target/release/harvest-tools` names a file, not a
//! commit, and nothing compared the two.
//!
//! This is the same defect class as an unpinned model, which
//! [`crate::translate::CLAUDE_MODEL_DEFAULT`] fixes: *an artifact that does not
//! record what produced it.* The remedy has the same two halves — stamp the
//! identity, then refuse to proceed when it cannot be confirmed.
//!
//! # Why refusal, and not just recording
//!
//! Recording alone would have told us afterwards. Refusing tells us before the
//! money. So [`require_reproducible`] runs as a startup preflight, in the same
//! spirit as `ToolchainId::detect` refusing a `RUSTUP_TOOLCHAIN` override: fail in
//! the first second, not the twelfth hour.

use anyhow::{Context, Result};

/// The commit this binary was compiled from, stamped by `build.rs` via `vergen`.
///
/// Outside a git tree vergen emits its placeholder rather than failing the build, so
/// callers must go through [`built_from`], which normalises that to `"unknown"`.
const VERGEN_SHA: &str = env!("VERGEN_GIT_SHA");

/// vergen's stand-in for a value it could not determine. Matching on it here rather
/// than anywhere else keeps the placeholder an implementation detail of this module.
const VERGEN_PLACEHOLDER: &str = "VERGEN_IDEMPOTENT_OUTPUT";

/// The commit this binary was compiled from, or `"unknown"`.
pub fn built_from() -> &'static str {
    if VERGEN_SHA.is_empty() || VERGEN_SHA == VERGEN_PLACEHOLDER {
        "unknown"
    } else {
        VERGEN_SHA
    }
}

/// Whether the tree was dirty when this binary was *built*.
///
/// Distinct from the runtime check in [`require_reproducible`], which is
/// authoritative because the tree can be edited after a build. This one matters for
/// `--version`: a binary handed to someone else still reports that it came from an
/// uncommitted tree.
pub fn built_dirty() -> bool {
    env!("VERGEN_GIT_DIRTY") == "true"
}

/// Full identity for `--version`: commit, compiler and target.
///
/// The compiler is here for the same reason it is in the cache key — `build_ok` is a
/// function of it — and the target triple because a result is not portable across
/// architectures.
/// Returns `&'static str` rather than `String` because that is what
/// `clap::Command::version` accepts. Backed by a `OnceLock` instead of leaking a
/// `Box`, so the allocation is bounded and obvious.
pub fn version_string() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        format!(
            "{} ({}{}, rustc {}, {})",
            env!("CARGO_PKG_VERSION"),
            built_from(),
            if built_dirty() { " dirty" } else { "" },
            env!("VERGEN_RUSTC_SEMVER"),
            env!("VERGEN_CARGO_TARGET_TRIPLE"),
        )
    })
}

/// Why a tree is not fit to produce a measurement.
#[derive(Debug, PartialEq, Eq)]
pub enum Unreproducible {
    /// The binary was built from a different commit than the one checked out — the
    /// 2026-08-14 failure exactly.
    StaleBinary { built_from: String, head: String },
    /// Tracked files differ from HEAD, so the commit does not describe the code.
    DirtyTree { files: usize },
    /// Built with no git metadata, so there is nothing to compare against.
    UnknownProvenance,
}

impl std::fmt::Display for Unreproducible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unreproducible::StaleBinary { built_from, head } => write!(
                f,
                "this binary was built from {built_from} but HEAD is {head}.\n  \
                 The results would be attributed to code that did not produce them. \
                 Rebuild (`cargo build --release --manifest-path tools/Cargo.toml`), \
                 or invoke via `cargo run` so the build cannot go stale."
            ),
            Unreproducible::DirtyTree { files } => write!(
                f,
                "{files} tracked file(s) differ from HEAD, so no commit describes the \
                 code that would run. Commit or stash first."
            ),
            Unreproducible::UnknownProvenance => write!(
                f,
                "this binary carries no commit stamp (built outside a git tree), so \
                 the code that produced the results cannot be identified."
            ),
        }
    }
}

/// The decision, as a pure function.
///
/// Split out from the git plumbing for the same reason
/// [`crate::workdir::resolve_from`] is: the interesting behaviour is testable
/// without a repository, a subprocess, or mutating the process environment.
pub fn assess(built_from: &str, head: Option<&str>, dirty_files: usize) -> Option<Unreproducible> {
    if built_from == "unknown" {
        return Some(Unreproducible::UnknownProvenance);
    }
    if dirty_files > 0 {
        return Some(Unreproducible::DirtyTree { files: dirty_files });
    }
    match head {
        // Prefix equality, not equality: the stamp is git's *short* SHA while
        // `rev-parse HEAD` is the full 40 chars. Comparing them directly would
        // reject every single run as stale.
        Some(h) if !h.starts_with(built_from) => Some(Unreproducible::StaleBinary {
            built_from: built_from.to_string(),
            head: h.chars().take(12).collect(),
        }),
        // No HEAD readable (not a repo at run time) but a stamp exists: nothing to
        // contradict, so allow. The stamp still lands in the recorded provenance.
        _ => None,
    }
}

/// Human-readable identity for the provenance record: `abc123def456`, or
/// `abc123def456-dirty` when the tree has uncommitted changes.
pub fn harness_id() -> String {
    match dirty_file_count() {
        Ok(n) if n > 0 => format!("{}-dirty", built_from()),
        _ => built_from().to_string(),
    }
}

/// Refuse to start a measurement whose provenance cannot be established.
///
/// Called for the phases that produce recorded artifacts. `--allow-dirty`
/// downgrades it to a warning, because iterating locally is legitimate — but then
/// the reason appears in the log, and `harness_id` carries `-dirty` into every
/// artifact, so a downgraded run cannot later be mistaken for a clean one.
pub fn require_reproducible(allow_dirty: bool) -> Result<()> {
    let head = head_sha();
    let dirty = dirty_file_count().unwrap_or(0);
    match assess(built_from(), head.as_deref(), dirty) {
        None => Ok(()),
        Some(problem) => {
            if allow_dirty {
                eprintln!("⚠️  --allow-dirty: {problem}");
                eprintln!(
                    "    Artifacts will be stamped `{}`; treat any number from this run as \
                     unpublishable.",
                    harness_id()
                );
                Ok(())
            } else {
                anyhow::bail!(
                    "refusing to run: {problem}\n  \
                     Pass --allow-dirty to proceed anyway (the run will be stamped as such)."
                )
            }
        }
    }
}

fn git(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    anyhow::ensure!(out.status.success(), "git {} failed", args.join(" "));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn head_sha() -> Option<String> {
    git(&["rev-parse", "HEAD"]).ok().filter(|s| !s.is_empty())
}

/// Count tracked files differing from HEAD. Untracked files are deliberately
/// ignored: `results/` alone carries thousands of them, and an untracked file
/// cannot change the behaviour of the compiled binary.
fn dirty_file_count() -> Result<usize> {
    Ok(git(&["status", "--porcelain", "--untracked-files=no"])?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_binary_is_refused() {
        // The 2026-08-14 failure: a binary from one commit, a checkout at another.
        let p = assess("1d2afa1aaaaa", Some("2cdc503bbbbbccccddddeeeeffff000011112222"), 0);
        assert_eq!(
            p,
            Some(Unreproducible::StaleBinary {
                built_from: "1d2afa1aaaaa".into(),
                head: "2cdc503bbbbb".into()
            })
        );
        assert!(format!("{}", p.unwrap()).contains("did not produce them"));
    }

    #[test]
    fn a_matching_prefix_is_accepted() {
        // build.rs stamps --short=12; rev-parse HEAD is 40 chars. Equality has to be
        // prefix equality or every single run would be refused as stale.
        assert_eq!(assess("2cdc503bbbbb", Some("2cdc503bbbbbccccddddeeeeffff000011112222"), 0), None);
    }

    #[test]
    fn a_dirty_tree_is_refused_and_says_how_many() {
        assert_eq!(assess("abc123abc123", Some("abc123abc123ff"), 4), Some(Unreproducible::DirtyTree { files: 4 }));
    }

    #[test]
    fn dirtiness_outranks_staleness() {
        // Both wrong: report the one the operator must fix first. A rebuild cannot
        // help while the tree is dirty, because there is no commit to rebuild to.
        let p = assess("aaaaaaaaaaaa", Some("bbbbbbbbbbbb"), 2);
        assert!(matches!(p, Some(Unreproducible::DirtyTree { .. })), "got {p:?}");
    }

    #[test]
    fn an_unstamped_binary_is_refused_rather_than_assumed_fine() {
        assert_eq!(assess("unknown", Some("abc123abc123"), 0), Some(Unreproducible::UnknownProvenance));
        // ...and stays refused even with a clean tree and no HEAD to compare.
        assert_eq!(assess("unknown", None, 0), Some(Unreproducible::UnknownProvenance));
    }

    #[test]
    fn a_stamped_binary_outside_a_repo_is_allowed() {
        // Deployed elsewhere: the stamp cannot be contradicted, and it still reaches
        // the artifact. Refusing here would make the tool unusable off a dev box.
        assert_eq!(assess("abc123abc123", None, 0), None);
    }

    #[test]
    fn the_binary_under_test_carries_a_real_stamp() {
        // Guards build.rs itself: if the stamp silently became empty, every check
        // above would still pass while proving nothing about this binary.
        assert!(!built_from().is_empty());
        assert_ne!(built_from(), "unknown", "built inside the repo, so a SHA must be stamped");
        // vergen's placeholder must never leak out as if it were a commit.
        assert_ne!(built_from(), VERGEN_PLACEHOLDER);
        // And --version must render something a human can act on.
        let v = version_string();
        assert!(v.contains(built_from()) && v.contains("rustc"), "{v}");
    }
}
