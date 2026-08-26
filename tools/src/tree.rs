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

/// The two subtrees every working dir has. A first invocation differs from a later one only in that
/// its translation is empty -- not in shape, not in layout, not in how it is hashed.
///
/// Siblings, not nested: with the C inside the crate a `build.rs` can CMake-build the original
/// library and link it, and one published artifact did exactly that -- 881 `objcopy`-renamed symbols
/// reached by naked-asm `jmp`, full marks at 1,013 lines against another agent's 27,044.
pub const C_SRC: &str = "c_src";
pub const TRANSLATION: &str = "translation";

/// A directory an agent may run in.
///
/// Remembers the corpus its C came from, so [`Self::seal`] restores from the SAME reference the
/// agent was given: a caller free to pass another at seal time could swap the oracle silently.
pub struct WorkDir {
    root: PathBuf,
    corpus_c: PathBuf,
    /// Shared, so N cases can be cut from one scratch root rather than N, and held so the
    /// directory outlives every path taken from it.
    _scratch: std::sync::Arc<tempfile::TempDir>,
}

impl WorkDir {
    /// The first work dir of a chain: the C from the corpus, an empty translation.
    pub fn assemble(corpus_c: &Path) -> Result<Self> {
        let scratch = std::sync::Arc::new(crate::io::workdir::tempdir("harvest-work-")?);
        let root = scratch.path().to_path_buf();
        Self::lay_out(&root, corpus_c)?;
        std::fs::create_dir_all(root.join(TRANSLATION))?;
        Ok(Self {
            root,
            corpus_c: corpus_c.to_path_buf(),
            _scratch: scratch,
        })
    }

    fn lay_out(root: &Path, corpus_c: &Path) -> Result<()> {
        anyhow::ensure!(
            corpus_c.is_dir(),
            "no C reference at {} -- a working dir cannot be laid out without one",
            corpus_c.display()
        );
        copy_carrying(corpus_c, &root.join(C_SRC), Carry::IntoWorkTree, None)
    }

    /// Where the agent runs.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The crate root: what gets built, and the only part an agent is asked to produce.
    pub fn translation(&self) -> PathBuf {
        self.root.join(TRANSLATION)
    }

    /// RESTORE, SCRUB, DIGEST -- the three steps that used to hide inside one `seal`. The restore
    /// is why no tamper check is needed: the agent may do what it likes to `c_src/`, and the tree
    /// that is hashed, stored and handed on always holds the corpus's C.
    pub fn seal(self) -> Result<Tree> {
        std::fs::remove_dir_all(self.root.join(C_SRC)).or_else(|e| {
            (e.kind() == std::io::ErrorKind::NotFound)
                .then_some(())
                .ok_or(e)
        })?;
        Self::lay_out(&self.root, &self.corpus_c)?;
        scrub(&self.root)?;
        Ok(Tree {
            digest: digest_tree(&self.root)?,
            at: self.root,
            _scratch: Some(self._scratch),
        })
    }
}

/// Rewrite per-run absolute paths to a stable token, so a digest of agent output does not change
/// every run. Must precede any digest: the scratch directory name is random.
fn scrub(root: &Path) -> Result<()> {
    let base = crate::io::workdir::base()?;
    // Read as UTF-8, so a non-UTF-8 path cannot occur in one of these files and there is nothing to
    // rewrite -- whereas its lossy form (U+FFFD per invalid byte) can, and would rewrite text that
    // is not a path.
    let needles: Vec<String> = [root, base.as_path()]
        .into_iter()
        .filter_map(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    // The same predicate the digest uses: nothing can be hashed unscrubbed.
    visit(
        root,
        root,
        false,
        &|d| d == Disposition::StoreAndHash,
        &mut |_rel, abs| {
            let Ok(text) = std::fs::read_to_string(abs) else {
                return Ok(()); // binary: skip
            };
            let mut out = text.clone();
            for n in &needles {
                out = out.replace(n.as_str(), "$HARVEST_WORKDIR");
            }
            if out != text {
                std::fs::write(abs, out).with_context(|| format!("scrubbing {}", abs.display()))?;
            }
            Ok(())
        },
    )
}

/// A sealed working dir: content-addressed, and not constructible from a path alone.
///
/// Yields no path, so nothing runs in one -- [`Self::materialise`] makes a fresh copy. That
/// unforgeability replaces `SeededBy`: step order is no longer typed, but a tree nothing produced
/// cannot be fed to a step.
pub struct Tree {
    digest: TreeDigest,
    at: PathBuf,
    /// Held where the bytes live in scratch; `None` where they live in the store.
    _scratch: Option<std::sync::Arc<tempfile::TempDir>>,
}

impl fmt::Debug for Tree {
    /// The digest, never the location: a `Tree` that could be formatted into a path would be a
    /// working directory by another name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tree({:?})", self.digest)
    }
}

impl Tree {
    pub fn digest(&self) -> &TreeDigest {
        &self.digest
    }

    #[allow(
        dead_code,
        reason = "the store is wired to this in the next step; the tests here already exercise it"
    )]
    /// The store's only door. The bytes must hash to what the entry recorded, or the entry is
    /// corrupt -- which is the check the whole design rests on, since a `before` that no longer
    /// reproduces its own name cannot be re-keyed.
    pub(crate) fn adopt_stored(at: PathBuf, recorded: &TreeDigest) -> Result<Self> {
        let digest = digest_tree(&at)?;
        anyhow::ensure!(
            &digest == recorded,
            "the stored tree at {} hashes to {digest:?} but its entry records {recorded:?}",
            at.display()
        );
        Ok(Self {
            digest,
            at,
            _scratch: None,
        })
    }

    /// A fresh working dir holding this tree's bytes. `corpus_c` is stated rather than remembered
    /// because a stored tree does not know which corpus it came from.
    pub fn materialise(&self, corpus_c: &Path) -> Result<WorkDir> {
        let scratch = std::sync::Arc::new(crate::io::workdir::tempdir("harvest-work-")?);
        let root = scratch.path().to_path_buf();
        copy_carrying(&self.at, &root, Carry::FromPreviousPhase, None)?;
        std::fs::create_dir_all(root.join(TRANSLATION))?;
        Ok(WorkDir {
            root,
            corpus_c: corpus_c.to_path_buf(),
            _scratch: scratch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corpus C reference: two files, one of them a `.bak` that only hashes because
    /// `is_ignored` exempts everything under `c_src/`.
    fn corpus() -> (tempfile::TempDir, PathBuf) {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let c = tmp.path().join("test_case");
        std::fs::create_dir_all(c.join("src")).unwrap();
        std::fs::write(c.join("src/lib.c"), "int f(void){return 1;}\n").unwrap();
        std::fs::write(c.join("doc.html.bak"), "reference\n").unwrap();
        (tmp, c)
    }

    #[test]
    fn a_step_hands_the_next_one_the_tree_it_sealed() {
        // The whole path, with no phase anywhere in it.
        let (_tmp, c) = corpus();
        let w = WorkDir::assemble(&c).unwrap();
        assert!(w.root().join(C_SRC).join("src/lib.c").is_file());
        assert!(
            w.translation().is_dir(),
            "translation must exist, even empty"
        );
        std::fs::write(w.translation().join("lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

        let sealed = w.seal().unwrap();
        let next = sealed.materialise(&c).unwrap();
        assert_eq!(
            std::fs::read_to_string(next.translation().join("lib.rs")).unwrap(),
            "pub fn f() -> i32 { 1 }\n",
            "the next step must receive the translation the previous one produced"
        );
        assert!(
            next.root().join(C_SRC).join("src/lib.c").is_file(),
            "and the C beside it"
        );
        assert_eq!(
            next.seal().unwrap().digest(),
            sealed.digest(),
            "materialise then seal must round-trip to the same digest"
        );
    }

    #[test]
    fn an_edit_to_the_c_reference_cannot_survive_a_seal() {
        // Replaces the tamper check: restoring makes a linked-original-C artifact (see `C_SRC`)
        // unable to persist rather than something to detect.
        let (_tmp, c) = corpus();
        let clean = {
            let w = WorkDir::assemble(&c).unwrap();
            std::fs::write(w.translation().join("lib.rs"), "x\n").unwrap();
            w.seal().unwrap().digest().clone()
        };

        let w = WorkDir::assemble(&c).unwrap();
        std::fs::write(w.translation().join("lib.rs"), "x\n").unwrap();
        let victim = w.root().join(C_SRC).join("src/lib.c");
        std::fs::write(&victim, "int f(void){return 999;}\n").unwrap();
        std::fs::write(w.root().join(C_SRC).join("extra.c"), "added\n").unwrap();
        assert_ne!(
            std::fs::read_to_string(&victim).unwrap(),
            "int f(void){return 1;}\n",
            "non-vacuous only if the fixture really tampered"
        );

        let sealed = w.seal().unwrap();
        assert_eq!(
            sealed.digest(),
            &clean,
            "a tampered C reference must hash identically to a pristine one"
        );
        let after = sealed.materialise(&c).unwrap();
        assert!(
            !after.root().join(C_SRC).join("extra.c").exists(),
            "and the added file must be gone, not merely unhashed"
        );
    }

    #[test]
    fn the_scratch_directory_name_never_reaches_a_digest() {
        // Unscrubbed, no entry would ever hit: caching would look enabled while never working.
        let (_tmp, c) = corpus();
        let seal_one = || {
            let w = WorkDir::assemble(&c).unwrap();
            let embedded = format!("// built in {}\n", w.root().display());
            std::fs::write(w.translation().join("lib.rs"), &embedded).unwrap();
            (w.seal().unwrap(), embedded)
        };
        let (a, text_a) = seal_one();
        let (b, text_b) = seal_one();
        assert_ne!(
            text_a, text_b,
            "non-vacuous only if the two runs really embedded different paths"
        );
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn a_stored_tree_that_does_not_hash_to_its_record_is_refused() {
        // A `before` that no longer reproduces its own name cannot be re-keyed: that is corruption,
        // and serving it publishes a number from bytes nothing attests.
        let (_tmp, c) = corpus();
        let w = WorkDir::assemble(&c).unwrap();
        std::fs::write(w.translation().join("lib.rs"), "one\n").unwrap();
        let sealed = w.seal().unwrap();
        let recorded = sealed.digest().clone();

        let store = crate::io::workdir::test_tempdir().unwrap();
        let at = store.path().join("before");
        copy_carrying(&sealed.at, &at, Carry::FromPreviousPhase, None).unwrap();
        Tree::adopt_stored(at.clone(), &recorded).expect("an intact copy is adoptable");

        std::fs::write(at.join(TRANSLATION).join("lib.rs"), "two\n").unwrap();
        let err = Tree::adopt_stored(at, &recorded).expect_err("a mutated copy must be refused");
        assert!(
            format!("{err:#}").contains("records"),
            "and must name both digests: {err:#}"
        );
    }

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
