//! Stamp the binary with the commit, compiler and target it was built from, so a
//! result can be traced to the code that produced it.
//!
//! `vergen` rather than hand-rolled `git rev-parse`: inside a git worktree `.git` is a
//! file and HEAD lives under `.git/worktrees/<name>/`; vergen aims `rerun-if-changed`
//! at that real path, so the stamp cannot go stale, and with no git metadata it emits a
//! placeholder ([`crate::provenance`] then refuses to measure) instead of failing.
//!
//! No timestamps: they would make two builds of the same commit differ. Pinned to
//! vergen-gitcl 9.x — v10 requires rustc 1.96 and `rust-toolchain.toml` pins 1.94.0.

use vergen_gitcl::{CargoBuilder, Emitter, GitclBuilder, RustcBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Selective rather than `all_git()`, which also bakes in committer name and message.
    let git = GitclBuilder::default()
        .sha(true) // short SHA; `provenance` compares it as a prefix
        .dirty(false) // tracked files only — matches the runtime check
        .build()?;
    let rustc = RustcBuilder::default().semver(true).build()?;
    let cargo = CargoBuilder::default().target_triple(true).build()?;

    Emitter::default()
        .add_instructions(&git)?
        .add_instructions(&rustc)?
        .add_instructions(&cargo)?
        .emit()?;

    // `.cargo/config.toml` points TMPDIR off the /tmp tmpfs at this directory, and cargo
    // sets the variable without creating it, so every child of a `cargo run` would be
    // handed a TMPDIR it cannot use. Derived from CARGO_MANIFEST_DIR rather than read back
    // from TMPDIR: a build script may only write inside its own package, and the ambient
    // value can name anywhere — with `TMPDIR=/nonexistent` exported, creating it aborted
    // the build with a bare EACCES that named neither the variable nor this script.
    let tmp = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR")?).join("target/tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| {
        format!(
            "creating {} for the TMPDIR in .cargo/config.toml: {e}",
            tmp.display()
        )
    })?;
    Ok(())
}
