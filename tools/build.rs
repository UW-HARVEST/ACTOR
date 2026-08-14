//! Stamp the binary with the commit, compiler and target it was built from.
//!
//! # Why this exists
//!
//! On 2026-08-14 a twelve-hour, $625 harvest-bench sweep ran a binary built from a
//! branch seven commits behind `main`, so it contained none of the infra gate, the
//! typed artifacts or the cache it was meant to exercise. Nothing detected it,
//! because nothing recorded which code produced a result: the driver referenced
//! `./tools/target/release/harvest-tools`, and a path names a file, not a commit.
//!
//! # Why `vergen` rather than hand-rolled git calls
//!
//! The first version of this file shelled out to `git rev-parse` directly. That is a
//! worse reimplementation of a subset of `vergen`, which is what the ecosystem uses
//! for exactly this. It gets two things right that hand-rolling has to rediscover:
//!
//! * **`rerun-if-changed` on the right paths.** A commit that changes no source file
//!   would otherwise leave the binary carrying a stale SHA — reintroducing the very
//!   bug being fixed — and the naive path (`../.git/HEAD`) is wrong inside a git
//!   worktree, where `.git` is a file and HEAD lives under `.git/worktrees/<name>/`.
//!   Every PR in this repo is developed in a worktree, so the naive form would be
//!   wrong essentially always. vergen resolves it: the emitted `rerun-if-changed`
//!   points at `.git/worktrees/<name>/HEAD`.
//! * **Graceful absence.** With no git metadata it emits a placeholder and warns
//!   rather than failing the build, so a source export still compiles;
//!   [`crate::provenance`] maps that placeholder to "cannot prove provenance" and
//!   refuses to *measure*, which is the right place for that decision.
//!
//! # What is deliberately not stamped
//!
//! No build timestamp and no commit timestamp. `idempotent()` would blank them
//! anyway (vergen suppresses time-varying values so rebuilds stay reproducible), and
//! a build timestamp is actively harmful: it makes two builds of the same commit
//! differ, which is the opposite of the property being established here. The commit
//! SHA identifies everything; `git show` supplies the date to anyone who wants it.
//!
//! Pinned to vergen-gitcl 9.x — v10 requires rustc 1.96 and `rust-toolchain.toml`
//! pins 1.94.0.

use vergen_gitcl::{CargoBuilder, Emitter, GitclBuilder, RustcBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Selective rather than `all_git()`: that would also bake the committer's name
    // and email and the full commit message into the binary — noise in an artifact
    // whose provenance record is published alongside a paper.
    let git = GitclBuilder::default()
        .sha(true) // VERGEN_GIT_SHA (short)
        .dirty(false) // VERGEN_GIT_DIRTY, tracked files only — matches the runtime check
        .build()?;
    // The compiler is part of a measurement's provenance for the same reason the
    // model is; `cache::ToolchainId` also puts it in the cache key.
    let rustc = RustcBuilder::default().semver(true).build()?;
    let cargo = CargoBuilder::default().target_triple(true).build()?;

    Emitter::default()
        .add_instructions(&git)?
        .add_instructions(&rustc)?
        .add_instructions(&cargo)?
        .emit()?;
    Ok(())
}
