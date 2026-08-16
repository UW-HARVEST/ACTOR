//! Which commit produced a result, and refusal to measure when that cannot be said:
//! a driver naming a binary path names a file, not a commit, and a stale binary once
//! billed a twelve-hour, $625 sweep against code lacking the features under test.
//! Hence [`require_reproducible`] as a startup preflight, not a post-hoc record.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
                "{files} tracked file(s) under {} differ from HEAD, so no commit describes \
                 the code that would run. Commit or stash first.",
                BEHAVIOUR_PATHS.join(" ")
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
        Some(n) if n > 0 => format!("{}-dirty", built_from()),
        _ => built_from().to_string(),
    }
}

/// What an unreproducible tree costs. Named rather than `bool` because this is the one
/// switch that decides whether a run whose code no commit describes is allowed to produce
/// numbers at all, and `require_reproducible(true)` at a call site says nothing about
/// which way `true` points.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum OnUnreproducible {
    Refuse,
    WarnAndStamp,
}

/// `--allow-dirty` downgrades the refusal to a warning for local iteration;
/// `harness_id` then carries `-dirty` into every artifact, so a downgraded run cannot
/// later be mistaken for a clean one.
pub fn require_reproducible(on_problem: OnUnreproducible) -> Result<()> {
    let head = head_sha();
    let dirty = dirty_file_count().unwrap_or(0);
    match assess(built_from(), head.as_deref(), dirty) {
        None => Ok(()),
        Some(problem) => {
            if on_problem == OnUnreproducible::WarnAndStamp {
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

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    anyhow::ensure!(out.status.success(), "git {} failed", args.join(" "));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Every git call is anchored here, because the process may be several levels deep
/// inside `results/` — a submodule, whose HEAD and dirtiness are not this repo's.
fn repo_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let cwd = PathBuf::from(".");
        let super_tree = git(&cwd, &["rev-parse", "--show-superproject-working-tree"]);
        match super_tree {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => git(&cwd, &["rev-parse", "--show-toplevel"])
                .map(PathBuf::from)
                .unwrap_or(cwd),
        }
    })
    .clone()
}

fn head_sha() -> Option<String> {
    git(&repo_root(), &["rev-parse", "HEAD"])
        .ok()
        .filter(|s| !s.is_empty())
}

/// The paths an edit to which can change what this binary does: its own sources, the
/// prompts the agents are handed, the toolchain the crates are built with.
///
/// Scoped rather than whole-tree because `results/` and `tables/` are this harness's
/// own OUTPUT, which a half-finished sweep has necessarily written — counting those
/// refused the documented resume path and left `--allow-dirty`, which stamps every
/// artifact unpublishable, as the only way through.
const BEHAVIOUR_PATHS: &[&str] = &[
    "tools/",
    "prompts/",
    "rust-toolchain.toml",
    ".cargo/config.toml",
];

/// `None` when git could not answer, which `assess` already treats as "outside a
/// repository" — the same case as an absent HEAD.
///
/// Memoised: it is consulted once per produced artifact through [`harness_id`], and
/// the state that matters is the tree the running binary was loaded from.
fn dirty_file_count() -> Option<usize> {
    static COUNT: OnceLock<Option<usize>> = OnceLock::new();
    *COUNT.get_or_init(|| count_dirty_in(&repo_root()).ok())
}

/// Untracked files are deliberately ignored: `results/` alone carries thousands, and
/// an untracked file cannot change the behaviour of the compiled binary. Submodules
/// likewise: none of the four is a build input of this crate.
fn count_dirty_in(dir: &Path) -> Result<usize> {
    let mut args = vec![
        "status",
        "--porcelain",
        "--untracked-files=no",
        "--ignore-submodules=all",
        "--",
    ];
    args.extend_from_slice(BEHAVIOUR_PATHS);
    Ok(git(dir, &args)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_binary_is_refused() {
        let p = assess(
            "1d2afa1aaaaa",
            Some("2cdc503bbbbbccccddddeeeeffff000011112222"),
            0,
        );
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
        assert_eq!(
            assess(
                "2cdc503bbbbb",
                Some("2cdc503bbbbbccccddddeeeeffff000011112222"),
                0
            ),
            None
        );
    }

    #[test]
    fn a_dirty_tree_is_refused_and_says_how_many() {
        assert_eq!(
            assess("abc123abc123", Some("abc123abc123ff"), 4),
            Some(Unreproducible::DirtyTree { files: 4 })
        );
    }

    #[test]
    fn dirtiness_outranks_staleness() {
        // A rebuild cannot help while the tree is dirty: no commit to rebuild to.
        let p = assess("aaaaaaaaaaaa", Some("bbbbbbbbbbbb"), 2);
        assert!(
            matches!(p, Some(Unreproducible::DirtyTree { .. })),
            "got {p:?}"
        );
    }

    #[test]
    fn an_unstamped_binary_is_refused_rather_than_assumed_fine() {
        assert_eq!(
            assess("unknown", Some("abc123abc123"), 0),
            Some(Unreproducible::UnknownProvenance)
        );
        assert_eq!(
            assess("unknown", None, 0),
            Some(Unreproducible::UnknownProvenance)
        );
    }

    #[test]
    fn a_stamped_binary_outside_a_repo_is_allowed() {
        // Refusing here would make the tool unusable off a dev box.
        assert_eq!(assess("abc123abc123", None, 0), None);
    }

    #[test]
    fn every_scoped_path_is_still_in_the_checkout() {
        // A rename or a move would leave the gate silently covering nothing.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tools/ has a parent");
        for p in BEHAVIOUR_PATHS {
            assert!(
                root.join(p).exists(),
                "{p} is gone; the dirty-tree gate now covers nothing"
            );
        }
    }

    #[test]
    fn the_scope_catches_code_and_ignores_harness_output() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let root = tmp.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "t@example.invalid"]);
        run(&["config", "user.name", "t"]);
        let files = [
            "tools/src/verify.rs",
            "prompts/claude/verify.md",
            "rust-toolchain.toml",
            "tables/results.md",
        ];
        for f in files {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "before\n").unwrap();
        }
        // --force: a developer's global excludes file may ignore `rust-toolchain.toml`,
        // which would leave it untracked here and the assertion below vacuous.
        run(&["add", "--force", "-A"]);
        run(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ]);
        assert_eq!(count_dirty_in(root).unwrap(), 0, "a clean tree");

        // Five of the nine lines that refused the resume path were regenerated tables.
        std::fs::write(root.join("tables/results.md"), "after\n").unwrap();
        assert_eq!(
            count_dirty_in(root).unwrap(),
            0,
            "harness output must not refuse a resume"
        );

        for f in &files[..3] {
            std::fs::write(root.join(f), "after\n").unwrap();
        }
        assert_eq!(
            count_dirty_in(root).unwrap(),
            3,
            "code, prompts and the toolchain pin must still be caught"
        );
    }

    #[test]
    fn the_binary_under_test_carries_a_real_stamp() {
        // Guards build.rs itself: if the stamp silently became empty, every check
        // above would still pass while proving nothing.
        assert!(!built_from().is_empty());
        assert_ne!(
            built_from(),
            "unknown",
            "built inside the repo, so a SHA must be stamped"
        );
        assert_ne!(built_from(), VERGEN_PLACEHOLDER);
        let v = version_string();
        assert!(v.contains(built_from()) && v.contains("rustc"), "{v}");
    }
}
