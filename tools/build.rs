//! Stamp the binary with the commit it was built from.
//!
//! On 2026-08-14 a twelve-hour, $625 harvest-bench sweep ran a binary built from a
//! branch seven commits behind `main`, so it contained none of the infra gate, the
//! typed artifacts or the cache it was meant to exercise. Nothing detected this,
//! because nothing recorded which code produced a result. The driver script
//! referenced `./tools/target/release/harvest-tools` — a path, with no identity.
//!
//! A stamp alone would not have caught it either; the binary has to *refuse to run*
//! when its stamp disagrees with `HEAD`. See `crate::provenance`.

use std::process::Command;

fn main() {
    // Re-stamp whenever HEAD moves, or a commit that changes no source file leaves
    // the binary carrying a stale SHA — which is precisely the failure being fixed.
    // `--git-path` is used rather than a literal `../.git/HEAD` because in a git
    // worktree `.git` is a file and HEAD lives under `.git/worktrees/<name>/`.
    for p in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");

    // `unknown` is a legitimate answer: a source tarball has no .git, and the
    // runtime check treats an unknown stamp as "cannot prove reproducibility"
    // rather than pretending it matched.
    let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=HARVEST_GIT_SHA={sha}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}
