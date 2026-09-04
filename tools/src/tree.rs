//! THE traversal of a tree, and the digest taken over it.
//!
//! Hashing and copying both route through `visit`, so a file that travels is a file that is
//! hashed: a copy omitting a hashed file would store a tree unable to re-derive its own digest,
//! and every cache read of it would fail validation. Split out of `artifact`, which owned
//! this only because it also owned the phase type-states.

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
///
/// PRIVATE to this module, and reached only through [`Scrubbed`]. That is what makes the seal's
/// restore -> widen -> scrub -> digest order a compile-time property instead of a comment: there is no
/// other door, so a digest taken over an unscrubbed tree -- every key a per-run nonce -- cannot be
/// written from anywhere in the crate.
fn digest_tree(root: &Path) -> Result<TreeDigest> {
    hash_tree(root, &|d| d == Disposition::StoreAndHash)
}

fn hash_tree(root: &Path, admits: &dyn Fn(Disposition) -> bool) -> Result<TreeDigest> {
    let mut files: std::collections::BTreeMap<RelPath, PathBuf> = Default::default();
    visit(root, root, false, admits, &mut |rel, abs| {
        files.insert(rel.clone(), abs.to_path_buf());
        Ok(())
    })
    .with_context(|| format!("walking {} for a digest", root.display()))?;

    let mut h = Sha256::new();
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

/// Copy exactly what [`digest_tree`] hashes, and nothing else.
///
/// ONE policy, because there is one kind of tree. The three `Carry` variants this replaces differed
/// only in whether `Disposition::Ignore` files travelled, which existed so a work tree carried the
/// previous phase's `logs/`. A transcript is harness output: it lives in the cache entry as `run.log`
/// and never inside a tree, so nothing needs to carry it and nothing needs protecting from being
/// overwritten by it.
pub(crate) fn copy_carrying(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    visit(
        src,
        src,
        false,
        &|d| d == Disposition::StoreAndHash,
        &mut |rel, abs| {
            let to = dest.join(rel.as_path());
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(abs, &to)
                .with_context(|| format!("copying {} to {}", abs.display(), to.display()))?;
            Ok(())
        },
    )
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
pub const C_SRC: &str = crate::domain::contents::C_ORACLE_DIR;
pub const TRANSLATION: &str = crate::domain::contents::TRANSLATION_DIR;

/// The PINNED C a case is translated from, checked once where it is derived.
///
/// A newtype because every function that touches a working dir took a bare `&Path` for it, and the
/// C reference is the one path whose substitution is SILENT: `seal` restores `c_src/` from it, so a
/// results directory or a scratch dir passed here would be hashed into the key as though it were the
/// corpus, and the entry would look ordinary. `Roots` (`io/workdir.rs`) is the same move, made after
/// a transposed root produced keys no second machine could reproduce.
#[derive(Clone, Debug)]
pub struct Corpus(PathBuf);

impl Corpus {
    /// The one construction point. Existence is checked HERE rather than at the first copy, so a
    /// missing corpus refuses before a scratch tree is cut for it.
    pub fn at(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        anyhow::ensure!(
            dir.is_dir(),
            "no C reference at {} -- a working dir cannot be laid out without one",
            dir.display()
        );
        Ok(Self(dir))
    }
}

/// A directory an agent may run in.
///
/// Remembers the corpus its C came from, so [`Self::seal`] restores from the SAME reference the
/// agent was given: a caller free to pass another at seal time could swap the oracle silently.
pub struct WorkDir {
    root: PathBuf,
    corpus_c: Corpus,
    /// Shared, so N cases can be cut from one scratch root rather than N, and held so the
    /// directory outlives every path taken from it.
    _scratch: std::sync::Arc<tempfile::TempDir>,
}

impl WorkDir {
    /// The first work dir of a chain: the C from the corpus, an empty translation.
    pub fn assemble(corpus_c: &Corpus) -> Result<Self> {
        let scratch = std::sync::Arc::new(crate::io::workdir::tempdir("harvest-work-")?);
        let root = scratch.path().to_path_buf();
        Self::lay_out(&root, corpus_c)?;
        std::fs::create_dir_all(root.join(TRANSLATION))?;
        Ok(Self {
            root,
            corpus_c: corpus_c.clone(),
            _scratch: scratch,
        })
    }

    fn lay_out(root: &Path, corpus_c: &Corpus) -> Result<()> {
        // The field, not an accessor: `Corpus` hands its location to nobody. The only reader is this
        // copy, in the same module, and a `pub(crate) fn as_path` was still a path escape the
        // architecture gate refused -- rightly, since a caller holding it could run a command in the
        // pinned corpus.
        copy_carrying(&corpus_c.0, &root.join(C_SRC))
    }

    /// Where the agent runs.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The crate root: what gets built, and the only part an agent is asked to produce.
    pub fn translation(&self) -> PathBuf {
        self.root.join(TRANSLATION)
    }

    /// RESTORE, WIDEN, SCRUB, DIGEST -- and the ORDER IS TYPED, not commented.
    ///
    /// Each step consumes the previous step's witness, so the sequence cannot be reordered or a step
    /// dropped without the compiler objecting. It was four statements in a row whose order was
    /// load-bearing and enforced by prose: "Must precede any digest" on `scrub`, and nothing at all on
    /// the widening. Both matter. A digest taken before the scrub carries the scratch directory's
    /// random name, so every key is a nonce; a scrub taken before the widening SILENTLY SKIPS what it
    /// cannot read, so an agent's own `0o000` fixture reaches the digest unrewritten.
    ///
    /// The restore is why no tamper check is needed: the agent may do what it likes to `c_src/`, and
    /// the tree that is hashed, stored and handed on always holds the corpus's C.
    pub fn seal(self) -> Result<Tree> {
        let digest = self.restore()?.widen()?.scrub()?.digest()?;
        Ok(Tree {
            digest,
            at: self.root,
            _scratch: Some(self._scratch),
        })
    }

    fn restore(&self) -> Result<Restored<'_>> {
        std::fs::remove_dir_all(self.root.join(C_SRC)).or_else(|e| {
            (e.kind() == std::io::ErrorKind::NotFound)
                .then_some(())
                .ok_or(e)
        })?;
        Self::lay_out(&self.root, &self.corpus_c)?;
        Ok(Restored(&self.root))
    }
}

/// `c_src/` is the corpus's again, whatever the agent did to it.
struct Restored<'a>(&'a Path);
/// Every file the digest will read can be read. See [`widen`].
struct Readable<'a>(&'a Path);
/// No absolute path from this run survives in any hashed file. See [`scrub`].
struct Scrubbed<'a>(&'a Path);

impl<'a> Restored<'a> {
    fn widen(self) -> Result<Readable<'a>> {
        widen(self.0)?;
        Ok(Readable(self.0))
    }
}

impl<'a> Readable<'a> {
    fn scrub(self) -> Result<Scrubbed<'a>> {
        scrub(self.0)?;
        Ok(Scrubbed(self.0))
    }
}

impl Scrubbed<'_> {
    /// The ONLY way to obtain a digest of a tree on disk.
    ///
    /// A stored tree is the one legitimate exception, and it says so by name -- see
    /// [`Scrubbed::already_stored`].
    fn digest(&self) -> Result<TreeDigest> {
        digest_tree(self.0)
    }

    /// A tree the store already holds: scrubbed and widened when it was WRITTEN, and read-only since.
    /// Named for what it asserts, so the exception is visible at the call site rather than being a
    /// second unlabelled door into `digest_tree`.
    fn already_stored(at: &Path) -> Scrubbed<'_> {
        Scrubbed(at)
    }
}

/// Widen permissions on what the seal is about to read. Reached only through [`Restored::widen`]. Agents leave files unreadable ON PURPOSE --
/// `static-vars-fpts` left `_ref/data/noperm.txt` at `0o000` to exercise a failing open -- and one
/// breaks both steps below: `digest_tree` fails the case with EACCES, voiding all 42 of claude's
/// B02_synthetic, and `scrub` SILENTLY skips what it cannot read, so an unscrubbed scratch path
/// reaches the digest as a per-run nonce. No key moves: the digest feeds paths and bytes, not modes.
///
/// Its own walk, though `visit` is THE traversal elsewhere: `visit` must `read_dir` a directory to
/// reach what is beneath it, so an unreadable DIRECTORY defeats it before it can emit -- the
/// condition being repaired. Drift from its pruning is harmless: widening a file the digest ignores
/// costs nothing, and one this skips is one nothing reads.
fn widen(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fn walk(root: &Path, dir: &Path) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let Ok(Ok(rel)) = path.strip_prefix(root).map(RelPath::new) else {
                continue;
            };
            if classify(&rel, false) == Disposition::BuildOutput {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path)?;
            // chmod follows symlinks, so widening one would widen a target outside this tree.
            if meta.file_type().is_symlink() {
                continue;
            }
            let is_dir = meta.is_dir();
            let mode = meta.permissions().mode() & 0o777;
            // Writable too, not merely readable: `scrub` rewrites the files it finds a path in.
            let want = mode | if is_dir { 0o700 } else { 0o600 };
            if want != mode {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(want))
                    .with_context(|| format!("widening permissions on {}", path.display()))?;
            }
            if is_dir {
                walk(root, &path)?;
            }
        }
        Ok(())
    }
    walk(root, root).with_context(|| format!("widening permissions under {}", root.display()))
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
#[derive(Clone)]
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
    pub(crate) fn adopt_stored(at: PathBuf, recorded: &str) -> Result<Self> {
        let digest = Scrubbed::already_stored(&at).digest()?;
        anyhow::ensure!(
            digest.as_str() == recorded,
            "the stored tree at {} hashes to {digest:?} but its entry records {recorded}",
            at.display()
        );
        Ok(Self {
            digest,
            at,
            _scratch: None,
        })
    }

    /// Copy the bytes out, for the store alone. Not a path escape: the caller states WHERE, and
    /// what it receives is a copy it owns rather than a handle on the tree.
    pub(crate) fn copy_into(&self, dest: &Path) -> Result<()> {
        copy_carrying(&self.at, dest)
    }

    /// Test-only: adopt a directory as a tree by hashing it. Production has only `seal` and
    /// `adopt_stored`, which is the point -- a tree nothing produced cannot be fed to a step.
    #[cfg(test)]
    pub(crate) fn for_test(at: &Path) -> Result<Self> {
        Ok(Self {
            digest: Scrubbed::already_stored(at).digest()?,
            at: at.to_path_buf(),
            _scratch: None,
        })
    }

    /// Copy ONE subtree out. The graded tree gets the translation and never `c_src/`, which is what
    /// makes an artifact that links the original library fail to build rather than score.
    pub(crate) fn copy_subtree_into(&self, subtree: &str, dest: &Path) -> Result<()> {
        let from = self.at.join(subtree);
        anyhow::ensure!(
            from.is_dir(),
            "the tree has no {subtree}/ to copy: a working dir always has both subtrees, so this is \
             a corrupt entry rather than an empty step"
        );
        copy_carrying(&from, dest)
    }

    /// A fresh working dir holding this tree's bytes. `corpus_c` is stated rather than remembered
    /// because a stored tree does not know which corpus it came from.
    pub fn materialise(&self, corpus_c: &Corpus) -> Result<WorkDir> {
        let scratch = std::sync::Arc::new(crate::io::workdir::tempdir("harvest-work-")?);
        let root = scratch.path().to_path_buf();
        copy_carrying(&self.at, &root)?;
        std::fs::create_dir_all(root.join(TRANSLATION))?;
        Ok(WorkDir {
            root,
            corpus_c: corpus_c.clone(),
            _scratch: scratch,
        })
    }
}

/// Copy a directory that is NOT a tree -- the corpus's test vectors, which the digest never covers.
///
/// Separate from [`copy_carrying`] on purpose: that one copies exactly what is hashed, and reusing it
/// here would silently apply the tree's classification rules to something that is not a tree.
pub(crate) fn copy_plain(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_plain(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to).with_context(|| {
                format!("copying {} to {}", entry.path().display(), to.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corpus C reference: two files, one of them a `.bak` that only hashes because
    /// `is_ignored` exempts everything under `c_src/`.
    fn corpus() -> (tempfile::TempDir, Corpus) {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let at = tmp.path().join("test_case");
        std::fs::create_dir_all(at.join("src")).unwrap();
        std::fs::write(at.join("src/lib.c"), "int f(void){return 1;}\n").unwrap();
        std::fs::write(at.join("doc.html.bak"), "reference\n").unwrap();
        let c = Corpus::at(&at).unwrap();
        (tmp, c)
    }

    /// An agent's own `0o000` fixture must cost it neither its case nor the stability of its key --
    /// both failures [`make_readable`] names, asserted where each would be observed.
    #[test]
    fn an_unreadable_file_the_agent_left_loses_neither_the_case_nor_the_key() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, c) = corpus();
        let seal_leaving_noperm = |payload: &str| {
            let w = WorkDir::assemble(&c).unwrap();
            std::fs::write(w.translation().join("lib.rs"), "pub fn f() {}\n").unwrap();
            let at = w.root().join("_ref/data");
            std::fs::create_dir_all(&at).unwrap();
            let f = at.join("noperm.txt");
            std::fs::write(&f, format!("{payload} at {}\n", w.root().display())).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
            let sealed = w
                .seal()
                .expect("an unreadable fixture must not lose the case");
            let text = std::fs::read_to_string(sealed.at.join("_ref/data/noperm.txt")).unwrap();
            (sealed.digest().as_str().to_string(), text)
        };

        let (first, text) = seal_leaving_noperm("one");
        assert!(
            text.contains("$HARVEST_WORKDIR") && !text.contains("/harvest-work-"),
            "the scrub must have reached it, or its key carries a per-run nonce: {text}"
        );
        // Non-vacuity: the file is HASHED, so the two assertions above are about a file that is
        // really part of the tree rather than one the digest never looked at.
        let (second, _) = seal_leaving_noperm("two");
        assert_ne!(
            first, second,
            "an unreadable file's bytes must still reach the digest"
        );
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
        copy_carrying(&sealed.at, &at).unwrap();
        Tree::adopt_stored(at.clone(), recorded.as_str()).expect("an intact copy is adoptable");

        std::fs::write(at.join(TRANSLATION).join("lib.rs"), "two\n").unwrap();
        let err =
            Tree::adopt_stored(at, recorded.as_str()).expect_err("a mutated copy must be refused");
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
