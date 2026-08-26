//! THE traversal of a tree, and the digest taken over it.
//!
//! Hashing and copying both route through [`visit`], so a file that travels is a file that is
//! hashed: a copy omitting a hashed file would store a tree unable to re-derive its own digest,
//! and every cache read of it would fail validation. Split out of [`crate::artifact`], which owned
//! this only because it also owned the phase type-states.

use crate::domain::contents::{classify, Carry, Disposition};
use crate::domain::relpath::RelPath;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};

/// A `sha256:<hex>` tree digest. No `From<String>`: the only way to obtain one is to
/// hash a tree, so it cannot be confused with an arbitrary string.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TreeDigest(String);

impl TreeDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The one digest not taken over a tree: [`crate::artifact::OracleDir`] reports it where the C
    /// reference is absent. Goes when the oracle check does, a restore making it unnecessary.
    pub(crate) fn absent() -> Self {
        Self("sha256:absent".to_string())
    }

    #[cfg(test)]
    pub(crate) fn for_test(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Debug for TreeDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 19 = `sha256:` plus 12 hex chars, enough to compare by eye.
        let short: String = self.0.chars().take(19).collect();
        write!(f, "{short}…")
    }
}

/// Length-prefixed, hence injective: the upstream `harvest_core::fs::hash_dir` separates
/// fields with bare NULs, so `("a\0b", "")` and `("a", "b")` collide there on binary.
pub(crate) fn feed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

pub(crate) fn is_cmake_build_dir(dir: &Path) -> bool {
    dir.join("CMakeCache.txt").is_file() || dir.join("CMakeFiles").is_dir()
}

/// Deterministic digest over the `StoreAndHash` files of a tree. Ported from
/// `harvest_core::fs::hash_dir`, plus a classification filter and the length prefixing above.
pub(crate) fn digest_tree(root: &Path) -> Result<TreeDigest> {
    hash_tree(root, &|d| d == Disposition::StoreAndHash)
}

pub(crate) fn hash_tree(root: &Path, admits: &dyn Fn(Disposition) -> bool) -> Result<TreeDigest> {
    let mut files: std::collections::BTreeMap<RelPath, PathBuf> = Default::default();
    visit(root, root, false, admits, &mut |rel, abs| {
        files.insert(rel.clone(), abs.to_path_buf());
        Ok(())
    })
    .with_context(|| format!("walking {} for a digest", root.display()))?;

    let mut h = Sha256::new();
    feed(&mut h, b"harvest-tree-v1");
    for (rel, abs) in &files {
        // `RelPath::new` validates relative/no-`..`/non-empty but NOT UTF-8, and a lossy
        // name collapses every invalid byte to U+FFFD — so `a\xFF` and `a\xFE` would hash
        // alike, losing the injectivity the rest of this digest rests on.
        feed(&mut h, rel.as_path().as_os_str().as_encoded_bytes());
        let bytes = std::fs::read(abs).with_context(|| format!("reading {}", abs.display()))?;
        feed(&mut h, &bytes);
    }
    Ok(TreeDigest(format!("sha256:{:x}", h.finalize())))
}

/// `reserved` is a relative path the artifact may NOT overwrite, because the harness owns it.
///
/// Exactly one file needs this: the phase transcript at `logs/<P::LOG>`. `logs/` is
/// `Disposition::Ignore`, which `Carry::FromArtifact` admits, so an agent that happens to create
/// `logs/verify.log` in its own work tree had that file published straight over the transcript --
/// turning a run whose stored copy shows `turn.completed` into one the audit reads as truncated and
/// refuses. Skipped rather than renamed-and-kept only because the transcript is proof of completion
/// and the agent's same-named log is a duplicate of output it also wrote elsewhere; the skip is
/// announced so it is never silent.
pub(crate) fn copy_carrying(
    src: &Path,
    dest: &Path,
    carry: Carry,
    reserved: Option<&Path>,
) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    visit(src, src, false, &|d| carry.admits(d), &mut |rel, abs| {
        if Some(rel.as_path()) == reserved {
            println!(
                "   \u{26a0}\u{fe0f}  not carrying {} from the artifact: the harness's transcript lives there",
                rel.as_path().display()
            );
            return Ok(());
        }
        let to = dest.join(rel.as_path());
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(abs, &to)
            .with_context(|| format!("copying {} to {}", abs.display(), to.display()))?;
        Ok(())
    })
    .with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
    // `std::fs::copy` carries mode bits and cache entries are stored read-only, so
    // without this a replay would publish a read-only crate and later builds hit EACCES.
    set_read_only(dest, Access::Writable)
}

/// Which way [`set_read_only`] goes. As a `bool` the call site read `set_read_only(dest,
/// false)`, where `false` says nothing about which; backwards, it either leaves the store
/// writable or publishes a crate later builds cannot write to.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(crate) enum Access {
    ReadOnly,
    Writable,
}

impl Access {
    fn locked(self) -> bool {
        self == Access::ReadOnly
    }
}

/// Types stop *this crate* from executing in a stored artifact; `0o555`/`0o444` also
/// binds what the types cannot see — a shell-out, a stray `cargo build --manifest-path` —
/// which then fails with `EACCES` instead of quietly filling the store with `target/`
/// dirs and mutating the artifact it was reading.
pub(crate) fn set_read_only(root: &Path, access: Access) -> Result<()> {
    fn perms(mode: u32) -> std::fs::Permissions {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(mode)
    }
    fn walk(p: &Path, access: Access) -> Result<()> {
        let meta = std::fs::symlink_metadata(p)?;
        if meta.is_dir() {
            // A `0o555` directory cannot have entries added or removed, so unlocking on
            // the way down and locking on the way out is what makes this reversible.
            if !access.locked() {
                std::fs::set_permissions(p, perms(0o755))?;
            }
            for e in std::fs::read_dir(p)? {
                walk(&e?.path(), access)?;
            }
            if access.locked() {
                std::fs::set_permissions(p, perms(0o555))?;
            }
        } else if !meta.file_type().is_symlink() {
            // chmod follows symlinks: locking one would lock a target outside this tree.
            std::fs::set_permissions(p, perms(if access.locked() { 0o444 } else { 0o644 }))?;
        }
        Ok(())
    }
    walk(root, access).with_context(|| {
        format!(
            "making {} {}writable",
            root.display(),
            if access.locked() { "non-" } else { "" }
        )
    })
}

/// **The** traversal of a tree: hashing and copying both go through it, so
/// "which files are part of this tree" has exactly one answer. `admits` gates descent
/// as well as emission, so a directory the caller does not want is never opened.
pub(crate) fn visit(
    root: &Path,
    dir: &Path,
    in_build_dir: bool,
    admits: &dyn Fn(Disposition) -> bool,
    emit: &mut dyn FnMut(&RelPath, &Path) -> Result<()>,
) -> Result<()> {
    let build_here = in_build_dir || is_cmake_build_dir(dir);
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Ok(Ok(rel)) = path.strip_prefix(root).map(RelPath::new) else {
            continue;
        };

        if !admits(classify(&rel, build_here)) {
            continue;
        }
        let ft = entry.file_type()?;
        let ft = if ft.is_symlink() {
            // Links are followed deliberately (the shipped `results/` holds 17 broken ones, 16
            // inside a published phase dir). NotFound only: swallowing every error would drop an
            // entry from BOTH the copy and the digest, so the two would agree and the store would
            // validate a truncated tree. ELOOP is the input that proves the difference.
            match std::fs::metadata(&path) {
                Ok(m) => m.file_type(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("resolving {}", path.display())),
            }
        } else {
            ft
        };
        if ft.is_dir() {
            visit(root, &path, build_here, admits, emit)?;
        } else if ft.is_file() {
            emit(&rel, &path)?;
        }
        // Anything else is skipped. Agent workspaces hold non-regular files — CRUST's
        // `impcheck` creates `.pipe` FIFOs — and `std::fs::copy`/`std::fs::read` open before
        // they stat, so a FIFO blocks until a writer appears: one stray pipe hangs a publish,
        // a digest, and the sweep worker whose permit is held across both.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_is_injective_where_nul_separators_are_not() {
        let mut a = Sha256::new();
        feed(&mut a, b"a\0b");
        feed(&mut a, b"");
        let mut b = Sha256::new();
        feed(&mut b, b"a");
        feed(&mut b, b"b");
        assert_ne!(format!("{:x}", a.finalize()), format!("{:x}", b.finalize()));
    }
}
