//! THE traversal of a tree, and the digest taken over it.
//!
//! One walk, one digest, one answer to "which files are part of this tree". Hashing and copying
//! both route through [`visit`], so a file that travels is a file that is hashed — a copy that
//! omitted a hashed file would store a tree unable to re-derive its own digest, and every cache
//! read of it would fail validation.
//!
//! Split out of [`crate::artifact`], which owned this alongside the phase type-states: the
//! traversal is what the store is built on and has nothing to do with which phase produced a
//! tree. `src/domain/` would be the natural home for the arithmetic, but the walk reads the
//! filesystem and the pure layer may not.

use crate::domain::contents::{classify, Disposition};
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

    /// The one digest not taken over a tree: [`crate::artifact::OracleDir`] reports it where the
    /// C reference is absent. Both go when the oracle check does -- a restore makes the check
    /// unnecessary -- so this exists only until then.
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
/// `harvest_core::fs::hash_dir`, plus a classification filter, the length prefixing above,
/// and following symlinks to hash content rather than the link target — the links around
/// phase dirs are staging artifacts whose targets are per-run paths.
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
            // Resolved rather than emitted, because this traversal follows links deliberately
            // (see [`digest_tree`]); one whose per-run target is gone has nothing to follow, and
            // the shipped `results/` holds 17 of those, 16 inside a published phase dir.
            // NotFound only, propagating the rest. Swallowing every error here would drop an
            // unresolvable entry from BOTH the copy AND the digest, so the two would agree, the
            // store would validate the truncated tree, and nothing could report it -- where the
            // base behaviour was a loud refusal from `read`/`copy`. ELOOP is the input that
            // proves the difference: a symlink cycle must still refuse.
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
