//! Which commit produced a result, and refusal to measure when that cannot be said:
//! a driver naming a binary path names a file, not a commit, and a stale binary once
//! billed a twelve-hour, $625 sweep against code lacking the features under test.
//! Hence [`require_reproducible`] as a startup preflight, not a post-hoc record.

use anyhow::{Context, Result};

/// Stamped by `build.rs` via `vergen`. Outside a git tree vergen emits a placeholder
/// rather than failing the build, so read it through [`built_from`].
const VERGEN_SHA: &str = env!("VERGEN_GIT_SHA");

/// vergen's stand-in for a value it could not determine.
const VERGEN_PLACEHOLDER: &str = "VERGEN_IDEMPOTENT_OUTPUT";

pub fn built_from() -> &'static str {
    if VERGEN_SHA.is_empty() || VERGEN_SHA == VERGEN_PLACEHOLDER {
        "unknown"
    } else {
        VERGEN_SHA
    }
}

/// Dirty at *build* time; the runtime check in [`require_reproducible`] is the
/// authoritative one, since the tree can be edited after a build.
pub fn built_dirty() -> bool {
    env!("VERGEN_GIT_DIRTY") == "true"
}

/// Returns `&'static str` rather than `String` because that is what
/// `clap::Command::version` accepts.
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

#[derive(Debug, PartialEq, Eq)]
pub enum Unreproducible {
    StaleBinary { built_from: String, head: String },
    DirtyTree { files: usize },
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

/// Split from the git plumbing so the decision is testable without a repository.
pub fn assess(built_from: &str, head: Option<&str>, dirty_files: usize) -> Option<Unreproducible> {
    if built_from == "unknown" {
        return Some(Unreproducible::UnknownProvenance);
    }
    if dirty_files > 0 {
        return Some(Unreproducible::DirtyTree { files: dirty_files });
    }
    match head {
        // Prefix equality, not equality: the stamp is git's short SHA while
        // `rev-parse HEAD` is the full 40 chars.
        Some(h) if !h.starts_with(built_from) => Some(Unreproducible::StaleBinary {
            built_from: built_from.to_string(),
            head: h.chars().take(12).collect(),
        }),
        // No HEAD readable (not a repo at run time): nothing can contradict the stamp,
        // which still reaches the recorded provenance.
        _ => None,
    }
}

pub fn harness_id() -> String {
    match dirty_file_count() {
        Ok(n) if n > 0 => format!("{}-dirty", built_from()),
        _ => built_from().to_string(),
    }
}

/// `--allow-dirty` downgrades the refusal to a warning for local iteration;
/// `harness_id` then carries `-dirty` into every artifact, so a downgraded run cannot
/// later be mistaken for a clean one.
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

/// Untracked files are deliberately ignored: `results/` alone carries thousands, and
/// an untracked file cannot change the behaviour of the compiled binary.
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
        // build.rs stamps --short=12; rev-parse HEAD is 40 chars.
        assert_eq!(assess("2cdc503bbbbb", Some("2cdc503bbbbbccccddddeeeeffff000011112222"), 0), None);
    }

    #[test]
    fn a_dirty_tree_is_refused_and_says_how_many() {
        assert_eq!(assess("abc123abc123", Some("abc123abc123ff"), 4), Some(Unreproducible::DirtyTree { files: 4 }));
    }

    #[test]
    fn dirtiness_outranks_staleness() {
        // A rebuild cannot help while the tree is dirty: no commit to rebuild to.
        let p = assess("aaaaaaaaaaaa", Some("bbbbbbbbbbbb"), 2);
        assert!(matches!(p, Some(Unreproducible::DirtyTree { .. })), "got {p:?}");
    }

    #[test]
    fn an_unstamped_binary_is_refused_rather_than_assumed_fine() {
        assert_eq!(assess("unknown", Some("abc123abc123"), 0), Some(Unreproducible::UnknownProvenance));
        assert_eq!(assess("unknown", None, 0), Some(Unreproducible::UnknownProvenance));
    }

    #[test]
    fn a_stamped_binary_outside_a_repo_is_allowed() {
        // Refusing here would make the tool unusable off a dev box.
        assert_eq!(assess("abc123abc123", None, 0), None);
    }

    #[test]
    fn the_binary_under_test_carries_a_real_stamp() {
        // Guards build.rs itself: if the stamp silently became empty, every check
        // above would still pass while proving nothing.
        assert!(!built_from().is_empty());
        assert_ne!(built_from(), "unknown", "built inside the repo, so a SHA must be stamped");
        assert_ne!(built_from(), VERGEN_PLACEHOLDER);
        let v = version_string();
        assert!(v.contains(built_from()) && v.contains("rustc"), "{v}");
    }
}
