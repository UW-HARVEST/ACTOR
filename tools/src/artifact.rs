//! Typed phase artifacts: what an agent produced, and what may be done with it.
//!
//! Three invariants are enforced by the compiler, not by convention:
//! * Nothing runs in a published artifact: `Command::current_dir` and `--target-dir`
//!   both take `impl AsRef<Path>`, so "can obtain a path" *is* "can execute here", and
//!   [`Sealed`] yields no path in any form. (`oracle/` still builds inside the tree it
//!   scores. No layout *inside* `results/` fixes that: MIT `runtests` resolves each
//!   crate at `<case>/translated_rust` and pins its build output to
//!   `<that>/target` with an explicit `--target-dir`, and its `cando2` runner bakes
//!   `CARGO_MANIFEST_DIR`. The build has to leave the tree, which is what
//!   [`Scratch::subdir`] + [`Sealed::materialise_at`] exist for.)
//! * An infra-failed run cannot be sealed: [`Scrubbed::seal`] demands a
//!   [`crate::agent_health::Completed`], mintable only from a completed run.
//! * A tree cannot be hashed before it is scrubbed: agent output embeds the random
//!   scratch directory name, so a digest of raw output changes every run.

use crate::agent_health::Completed;
use crate::domain::contents::{classify, Carry, Disposition, C_ORACLE_DIR};
use crate::domain::relpath::RelPath;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod sealed_trait {
    pub trait Sealed {}
}

/// Sealed, so that every phase-dependent constant lives here and cannot drift apart.
pub trait Phase: sealed_trait::Sealed + Copy + 'static {
    const DIR: &'static str;
}

#[derive(Copy, Clone)]
pub struct Translate;
#[derive(Copy, Clone)]
pub struct Verify;

impl sealed_trait::Sealed for Translate {}
impl sealed_trait::Sealed for Verify {}

impl Phase for Translate {
    const DIR: &'static str = crate::battery::TRANSLATED;
}
impl Phase for Verify {
    const DIR: &'static str = crate::battery::VERIFIED;
}

/// Where a seed's contents land inside a work tree. Swapping these is silent: a corpus
/// at the crate root would present C sources as the Rust crate, and a crate under
/// `c_src/` would be graded as its own oracle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SeedAt {
    CrateRoot,
    COracle,
}

/// `Self` is a phase whose work tree may be seeded from `S`. The impls below are the only
/// legal transitions. On the phase MARKERS, never on [`Sealed`] (which implements only
/// `Debug`).
pub trait SeededBy<S>: Phase {
    const AT: SeedAt;
}

impl SeededBy<Corpus> for Translate {
    const AT: SeedAt = SeedAt::COracle;
}

/// Only from a *translation*: re-verifying a verification does not compile.
impl SeededBy<Sealed<Translate>> for Verify {
    const AT: SeedAt = SeedAt::CrateRoot;
}

/// The C sources an agent translates: an INPUT, never an output. Not a [`Phase`]: a
/// `Sealed<Corpus>` would inherit [`Sealed::publish`], and with `DIR = "test_case"` that
/// deletes the experiment's own input.
pub struct Corpus {
    c: CDir,
}

impl Corpus {
    /// The only constructor. `is_dir` is what keeps [`CDir::digest`]'s fabricated
    /// `sha256:absent` unreachable from a cache key.
    pub fn adopt(dir: &Path) -> Result<Self> {
        anyhow::ensure!(
            dir.is_dir(),
            "no C corpus at {}: an absent input would otherwise be keyed as a real one",
            dir.display()
        );
        Ok(Self {
            c: CDir(dir.to_path_buf()),
        })
    }

    /// The corpus as the agent will see it. Through [`CDir`], never `digest_tree`: with
    /// the corpus as hash root, `is_ignored` drops `*.bak`/`*.log`/`*.sha256` that ARE
    /// hashed once seeded under `c_src/`. `doc/footer.html.bak` is real here, so two
    /// corpora could otherwise share a digest and replay each other's translation.
    pub fn digest(&self) -> Result<TreeDigest> {
        self.c.digest()
    }

    #[must_use = "materialising and dropping the copy does nothing"]
    pub fn materialise_into<Q>(&self, scratch: Scratch) -> Result<WorkTree<Q>>
    where
        Q: SeededBy<Corpus>,
    {
        let root = scratch.dir.path().to_path_buf();
        seed(&self.c.0, root, scratch, Q::AT)
    }

    /// As [`Self::materialise_into`], into one slot of a scratch root the caller keeps.
    #[must_use = "materialising and dropping the copy does nothing"]
    pub fn materialise_at<Q>(&self, at: ScratchPath) -> Result<WorkTree<Q>>
    where
        Q: SeededBy<Corpus>,
    {
        let ScratchPath { root, keep } = at;
        seed(&self.c.0, root, Scratch { dir: keep }, Q::AT)
    }
}

/// THE seeding body: every way of obtaining a [`WorkTree`] routes here.
fn seed<Q: Phase>(src: &Path, root: PathBuf, keep: Scratch, at: SeedAt) -> Result<WorkTree<Q>> {
    let crate_root = root.join(crate::battery::TRANSLATED_RUST);
    let dest = match at {
        SeedAt::CrateRoot => crate_root,
        SeedAt::COracle => crate_root.join(C_ORACLE_DIR),
    };
    copy_carrying(src, &dest, Carry::IntoWorkTree)?;
    Ok(WorkTree {
        root,
        _scratch: Some(keep),
        _phase: PhantomData,
    })
}

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

impl Carry {
    fn admits(self, d: Disposition) -> bool {
        match d {
            // No arm here may return false: an artifact whose stored copy omits a hashed
            // file cannot re-derive its own digest, so every cache read fails validation.
            Disposition::StoreAndHash => true,
            Disposition::BuildOutput => false,
            Disposition::Ignore => self != Carry::FromPreviousPhase,
        }
    }
}

fn copy_carrying(src: &Path, dest: &Path, carry: Carry) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    visit(src, src, false, &|d| carry.admits(d), &mut |rel, abs| {
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

fn is_cmake_build_dir(dir: &Path) -> bool {
    dir.join("CMakeCache.txt").is_file() || dir.join("CMakeFiles").is_dir()
}

/// Length-prefixed, hence injective: the upstream `harvest_core::fs::hash_dir` separates
/// fields with bare NULs, so `("a\0b", "")` and `("a", "b")` collide there on binary.
fn feed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// Deterministic digest over the `StoreAndHash` files of a tree. Ported from
/// `harvest_core::fs::hash_dir`, plus a classification filter, the length prefixing above,
/// and following symlinks to hash content rather than the link target — the links around
/// phase dirs are staging artifacts whose targets are per-run paths.
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

/// **The** traversal of an artifact tree: hashing and copying both go through it, so
/// "which files are part of this artifact" has exactly one answer. `admits` gates descent
/// as well as emission, so a directory the caller does not want is never opened.
fn visit(
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
        if path.is_dir() {
            visit(root, &path, build_here, admits, emit)?;
        } else {
            emit(&rel, &path)?;
        }
    }
    Ok(())
}

/// Disk-backed, never tmpfs (see [`crate::io::workdir`]); removed once the last handle to
/// it drops. Shared rather than solely owned so that a [`ScratchPath`] cut from it
/// cannot name a directory the tempdir has already deleted.
#[must_use]
pub struct Scratch {
    dir: Arc<tempfile::TempDir>,
}

impl Scratch {
    pub fn new(prefix: &str) -> Result<Self> {
        Ok(Self {
            dir: Arc::new(crate::io::workdir::tempdir(prefix)?),
        })
    }

    /// Room for ONE case inside a root shared by many. [`Sealed::materialise_into`]
    /// cannot serve a whole battery: it consumes the `Scratch` and roots the work tree
    /// at the tempdir itself, so N cases would need N roots.
    pub fn subdir(&self, name: impl AsRef<Path>) -> Result<ScratchPath> {
        let rel = RelPath::new(name)?;
        let root = self.dir.path().join(rel.as_path());
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating scratch subdir {}", root.display()))?;
        Ok(ScratchPath {
            root,
            keep: Arc::clone(&self.dir),
        })
    }
}

/// Where a case may be materialised. Hands out no path of its own, so the only thing a
/// caller can do with one is pass it to [`Sealed::materialise_at`] — which is what stops
/// that destination from being the results-tree phase dir being measured.
#[must_use]
pub struct ScratchPath {
    root: PathBuf,
    keep: Arc<tempfile::TempDir>,
}

/// A materialised, writable copy. The ONLY artifact type that yields a `Path`.
pub struct WorkTree<P: Phase> {
    root: PathBuf,
    _scratch: Option<Scratch>, // kept alive so the tree outlives materialisation
    _phase: PhantomData<P>,
}

impl<P: Phase> WorkTree<P> {
    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn crate_dir(&self) -> PathBuf {
        self.root.join(crate::battery::TRANSLATED_RUST)
    }

    pub fn c(&self) -> CDir {
        CDir(self.crate_dir().join(C_ORACLE_DIR))
    }

    /// Rewrite per-run absolute paths to a stable token, then allow hashing. Consumes
    /// `self`, so nothing can run again against a tree normalised for hashing.
    pub fn scrub(self) -> Result<Scrubbed<P>> {
        let base = crate::io::workdir::base()?;
        // The files below are read as UTF-8, so a non-UTF-8 path cannot occur in one and
        // there is nothing to rewrite — whereas its lossy form (U+FFFD per invalid byte)
        // can occur, and would rewrite text that is not a path.
        let needles: Vec<String> = [self.root.as_path(), base.as_path()]
            .into_iter()
            .filter_map(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let mut rewritten = Vec::new();

        let artifact = self.crate_dir();
        // The same predicate the digest uses: nothing can be hashed unscrubbed.
        visit(
            &artifact,
            &artifact,
            false,
            &|d| d == Disposition::StoreAndHash,
            &mut |rel, abs| {
                let Ok(text) = std::fs::read_to_string(abs) else {
                    return Ok(());
                }; // binary: skip
                let mut out = text.clone();
                for n in &needles {
                    out = out.replace(n.as_str(), "$HARVEST_WORKDIR");
                }
                if out != text {
                    std::fs::write(abs, out)
                        .with_context(|| format!("scrubbing {}", abs.display()))?;
                    rewritten.push(rel.clone());
                }
                Ok(())
            },
        )?;
        Ok(Scrubbed {
            root: artifact,
            _scratch: self._scratch,
            rewritten,
            _phase: PhantomData,
        })
    }
}

/// This crate cannot modify the oracle: no `&Path`, no writes. The agent subprocess holds
/// [`WorkTree::path`] and *can*, hence the before/after compare in [`Scrubbed::seal`].
pub struct CDir(PathBuf);

impl CDir {
    /// Everything but build output — the same files [`classify`] keeps under `c_src/`
    /// when reached from the artifact root. This walk sees `c_src` as its own root, where
    /// the root-anchored rules cannot recognise the prefix, so the predicate is spelled
    /// here instead.
    pub fn digest(&self) -> Result<TreeDigest> {
        if self.0.is_dir() {
            hash_tree(&self.0, &|d| d != Disposition::BuildOutput)
        } else {
            Ok(TreeDigest("sha256:absent".into()))
        }
    }
}

/// Output whose per-run paths have been normalised. The only input to a digest.
pub struct Scrubbed<P: Phase> {
    root: PathBuf,
    _scratch: Option<Scratch>,
    rewritten: Vec<RelPath>,
    _phase: PhantomData<P>,
}

impl<P: Phase> Scrubbed<P> {
    pub fn rewritten(&self) -> &[RelPath] {
        &self.rewritten
    }

    pub fn seal(self, _proof: &Completed, c_before: &TreeDigest) -> Result<Sealed<P>> {
        let c_after = CDir(self.root.join("c_src")).digest()?;
        if &c_after != c_before {
            return Err(crate::refusal::Refusal::OracleModified {
                before: c_before.as_str().to_string(),
                after: c_after.as_str().to_string(),
            }
            .into());
        }
        let c_after = CDir(self.root.join(C_ORACLE_DIR)).digest()?;
        anyhow::ensure!(
            &c_after == c_before,
            "the agent modified the C oracle source: {} before, {} after. \
             The C side is the reference the translation is graded against; a run that \
             changes it has not been verified against the original program.",
            c_before.as_str(),
            c_after.as_str()
        );
        let digest = digest_tree(&self.root)?;
        Ok(Sealed {
            root: self.root,
            _scratch: self._scratch,
            digest,
            _phase: PhantomData,
        })
    }
}

/// A finished artifact. Deliberately implements NONE of `AsRef<Path>`,
/// `Deref<Target = Path>`, `Borrow<Path>` or `Display`, and has no `path()`; `Debug`
/// prints the digest so the location cannot be recovered by formatting.
pub struct Sealed<P: Phase> {
    root: PathBuf,
    _scratch: Option<Scratch>,
    digest: TreeDigest,
    _phase: PhantomData<P>,
}

impl<P: Phase> fmt::Debug for Sealed<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sealed<{}>({:?})", P::DIR, self.digest)
    }
}

impl<P: Phase> Sealed<P> {
    pub fn adopt(case_dir: &Path) -> Result<Self> {
        let root = crate::battery::phase_dir(case_dir, P::DIR);
        anyhow::ensure!(
            root.is_dir(),
            "no {} phase dir at {}",
            P::DIR,
            root.display()
        );
        let digest = digest_tree(&root)?;
        Ok(Self {
            root,
            _scratch: None,
            digest,
            _phase: PhantomData,
        })
    }

    /// Re-adopt a tree the cache stored earlier. Kept `pub(crate)`: widening it would be
    /// a way to manufacture a `Sealed` without a `Completed` proof.
    pub(crate) fn from_cache(code_dir: &Path) -> Result<Self> {
        anyhow::ensure!(
            code_dir.is_dir(),
            "cache entry has no code/ at {}",
            code_dir.display()
        );
        let digest = digest_tree(code_dir)?;
        Ok(Self {
            root: code_dir.to_path_buf(),
            _scratch: None,
            digest,
            _phase: PhantomData,
        })
    }

    pub fn digest(&self) -> &TreeDigest {
        &self.digest
    }

    /// Takes a destination and returns nothing, so there is still no expression that
    /// yields a path *to* a sealed artifact. Uses the same [`Carry`] variant as the
    /// results-tree overlay, so a replay cannot differ from a fresh run.
    pub fn export_into(&self, dest: &Path) -> Result<()> {
        copy_carrying(&self.root, dest, Carry::FromArtifact)
    }

    /// The only way to obtain something runnable: a writable copy elsewhere.
    /// `Q: SeededBy<Self>` is the pairing constraint — see [`SeededBy`].
    #[must_use = "materialising and dropping the copy does nothing"]
    pub fn materialise_into<Q>(&self, scratch: Scratch) -> Result<WorkTree<Q>>
    where
        Q: SeededBy<Self>,
    {
        let root = scratch.dir.path().to_path_buf();
        seed(&self.root, root, scratch, Q::AT)
    }

    /// As [`Self::materialise_into`], but into one slot of a scratch root the caller
    /// keeps, so a battery of N cases needs one root and not N.
    #[must_use = "materialising and dropping the copy does nothing"]
    pub fn materialise_at<Q>(&self, at: ScratchPath) -> Result<WorkTree<Q>>
    where
        Q: SeededBy<Self>,
    {
        let ScratchPath { root, keep } = at;
        seed(&self.root, root, Scratch { dir: keep }, Q::AT)
    }

    pub fn publish(&self, case_dir: &Path) -> Result<()> {
        let dst = crate::battery::phase_dir(case_dir, P::DIR);
        if dst.exists() {
            for entry in std::fs::read_dir(&dst)? {
                let entry = entry?;
                if entry.file_name() == "logs" {
                    continue; // verify.log is written here live
                }
                let p = entry.path();
                if entry.file_type()?.is_dir() {
                    std::fs::remove_dir_all(&p)?;
                } else {
                    std::fs::remove_file(&p)?;
                }
            }
        }
        self.assemble_into(case_dir, &dst)
    }

    /// Factored out of [`Self::publish`] so that "what this phase's tree contains" —
    /// published, or copied to compile-check — has exactly one answer.
    pub fn assemble_into(&self, case_dir: &Path, dst: &Path) -> Result<()> {
        let translated = crate::battery::phase_dir(case_dir, crate::battery::TRANSLATED);
        if translated.is_dir() && P::DIR != crate::battery::TRANSLATED {
            copy_carrying(&translated, dst, Carry::FromPreviousPhase)?;
        }
        copy_carrying(&self.root, dst, Carry::FromArtifact)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    /// `Carry::FromArtifact` may not drop anything the digest covers. If this fails the
    /// cache does not misbehave subtly: no entry can validate, so it silently never hits.
    #[test]
    fn from_artifact_keeps_everything_the_digest_covers() {
        let src = crate::io::workdir::test_tempdir().unwrap();
        tree(
            src.path(),
            &[
                ("Cargo.toml", "[package]"),
                ("src/lib.rs", "pub fn a() {}"),
                (".cargo/config.toml", "[build]"), // a real build input
                ("c_src/src/lib.c", "int a(void){0;}"), // the oracle: hashed, so carried
                ("c_src/doc/footer.html.bak", "upstream"), // hashed too: it is oracle source
                ("build.rs", "fn main() {}"),
                ("logs/verify.log", "transcript"), // Ignore: need not survive
                ("target/debug/junk", "build output"), // BuildOutput: must not survive
            ],
        );
        let dest = crate::io::workdir::test_tempdir().unwrap();
        let out = dest.path().join("code");
        copy_carrying(src.path(), &out, Carry::FromArtifact).unwrap();

        for hashed in [
            "Cargo.toml",
            "src/lib.rs",
            ".cargo/config.toml",
            "c_src/src/lib.c",
            "c_src/doc/footer.html.bak",
            "build.rs",
        ] {
            assert_eq!(
                classify(&rel(hashed), false),
                Disposition::StoreAndHash,
                "fixture assumption for {hashed}"
            );
            assert!(
                out.join(hashed).is_file(),
                "{hashed} is hashed, so an export that drops it makes the digest unverifiable"
            );
        }
        assert!(
            !out.join("target").exists(),
            "build output must not be stored"
        );

        assert_eq!(
            digest_tree(src.path()).unwrap(),
            digest_tree(&out).unwrap(),
            "an exported artifact must hash identically to the artifact it came from"
        );
    }

    /// cmake refuses a cache whose `CMAKE_CACHEFILE_DIR` names a scratch dir that no
    /// longer exists, and being nested, only the content sniff catches it.
    #[test]
    fn a_stale_cmake_cache_does_not_reach_the_agent() {
        let src = crate::io::workdir::test_tempdir().unwrap();
        tree(
            src.path(),
            &[
                ("Cargo.toml", "[package]"),
                ("c_src/src/lib.c", "int a(void){return 0;}"),
                (
                    "c_src/build/CMakeCache.txt",
                    "CMAKE_CACHEFILE_DIR:INTERNAL=/tmp/harvest-translate-w1nAAq/x",
                ),
                ("c_src/build/CMakeFiles/junk", "cmake internals"),
                ("logs/translate.log", "transcript"),
            ],
        );
        let dest = crate::io::workdir::test_tempdir().unwrap();
        let out = dest.path().join("work");
        copy_carrying(src.path(), &out, Carry::IntoWorkTree).unwrap();

        assert!(
            out.join("c_src/src/lib.c").is_file(),
            "the C oracle must travel"
        );
        assert!(
            out.join("logs/translate.log").is_file(),
            "logs must still travel — the agent has always been able to read them"
        );
        assert!(
            !out.join("c_src/build").exists(),
            "a nested cmake build tree must NOT travel: its cache names a dead scratch dir"
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

    /// Pinned from the pre-`as_encoded_bytes` implementation. On Unix an `OsStr` *is* its
    /// encoded bytes and lossy conversion of valid UTF-8 is the identity, so every digest
    /// in `results/` must survive; this measures that rather than assuming it.
    #[test]
    fn lossless_path_encoding_does_not_change_an_existing_digest() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let r = tmp.path();
        std::fs::create_dir_all(r.join("src")).unwrap();
        std::fs::write(r.join("Cargo.toml"), b"[package]\nname=\"x\"\n").unwrap();
        std::fs::write(r.join("src/lib.rs"), b"pub fn f() {}\n").unwrap();
        std::fs::create_dir_all(r.join("c_src/nested")).unwrap();
        std::fs::write(r.join("c_src/nested/a.c"), b"int a;\n").unwrap();
        assert_eq!(
            digest_tree(r).unwrap().as_str(),
            "sha256:bb95dbd7dfe0089c8568c8421bcca59e4d32bc0f406fb65d9f3bd8ab6302a6df"
        );
    }

    /// The hole the lossy encoding left. `RelPath` does not validate UTF-8, so this is
    /// reachable, and both trees used to digest alike — a cache hit across different work.
    #[test]
    fn paths_differing_only_outside_utf8_digest_differently() {
        use std::os::unix::ffi::OsStrExt;
        let digest_of = |name: &[u8]| {
            let tmp = crate::io::workdir::test_tempdir().unwrap();
            let f = tmp.path().join(std::ffi::OsStr::from_bytes(name));
            std::fs::write(&f, b"same content").unwrap();
            digest_tree(tmp.path()).unwrap().as_str().to_owned()
        };
        assert_ne!(digest_of(b"a\xff"), digest_of(b"a\xfe"));
    }

    /// The oracle guard walks `c_src` as its OWN root, where the root-anchored rules in
    /// `classify` cannot see the `c_src` prefix. Both walks must cover the same files or
    /// the guard is blind to exactly what the artifact digest records.
    #[test]
    fn the_oracle_guard_sees_a_bak_file_change() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let c = tmp.path().join("c_src");
        tree(
            &c,
            &[
                ("src/lib.c", "int a(void){return 0;}"),
                ("doc/footer.html.bak", "upstream"),
            ],
        );

        let before = CDir(c.clone()).digest().unwrap();
        std::fs::write(c.join("doc/footer.html.bak"), "the agent edited the oracle").unwrap();
        assert_ne!(
            before,
            CDir(c.clone()).digest().unwrap(),
            "a .bak edit must move the digest"
        );

        assert_eq!(
            classify(&rel("c_src/doc/footer.html.bak"), false),
            Disposition::StoreAndHash,
            "and the artifact digest must cover the same file",
        );
    }

    #[test]
    fn an_absent_corpus_is_refused_rather_than_keyed_as_absent() {
        // Otherwise `sha256:absent` becomes a key every missing corpus shares.
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let err = Corpus::adopt(&tmp.path().join("nope"))
            .err()
            .expect("must refuse an absent corpus");
        assert!(format!("{err:#}").contains("no C corpus at"), "{err:#}");
    }

    #[test]
    fn two_corpora_differing_only_in_an_ignored_file_get_different_digests() {
        // THE FALSE-HIT HAZARD — see `Corpus::digest`.
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        tree(
            &a,
            &[
                ("src/lib.c", "int f(void){return 1;}"),
                ("doc/footer.html.bak", "one"),
            ],
        );
        tree(
            &b,
            &[
                ("src/lib.c", "int f(void){return 1;}"),
                ("doc/footer.html.bak", "two"),
            ],
        );

        let da = Corpus::adopt(&a).unwrap().digest().unwrap();
        let db = Corpus::adopt(&b).unwrap().digest().unwrap();
        assert_ne!(
            da, db,
            "an ignored-at-root file still changes what the agent sees"
        );

        // ...and the naive spelling really would have collided, so this is not vacuous.
        assert_eq!(
            digest_tree(&a).unwrap(),
            digest_tree(&b).unwrap(),
            "fixture assumption: digest_tree is the hashing that drops it"
        );
    }

    #[test]
    fn a_corpus_seeds_the_oracle_and_a_sealed_artifact_seeds_the_crate_root() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let src = tmp.path().join("corpus");
        tree(&src, &[("src/lib.c", "int f(void){return 1;}")]);

        let work: WorkTree<Translate> = Corpus::adopt(&src)
            .unwrap()
            .materialise_into(Scratch::new("t-").unwrap())
            .unwrap();
        assert!(
            work.c().0.join("src/lib.c").is_file(),
            "corpus lands under c_src/"
        );
        assert!(
            !work.crate_dir().join("src/lib.c").is_file(),
            "not at the crate root"
        );
        assert_eq!(<Translate as SeededBy<Corpus>>::AT, SeedAt::COracle);
        assert_eq!(
            <Verify as SeededBy<Sealed<Translate>>>::AT,
            SeedAt::CrateRoot
        );
    }

    fn tree(root: &Path, files: &[(&str, &str)]) {
        for (p, c) in files {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
        }
    }

    #[test]
    fn digest_ignores_build_output_and_logs() {
        let a = crate::io::workdir::test_tempdir().unwrap();
        tree(
            a.path(),
            &[("src/lib.rs", "fn a() {}"), ("logs/verify.log", "noise")],
        );
        let b = crate::io::workdir::test_tempdir().unwrap();
        tree(
            b.path(),
            &[
                ("src/lib.rs", "fn a() {}"),
                ("logs/verify.log", "COMPLETELY different noise"),
                ("target/debug/blob", "junk"),
                ("c_src/build/CMakeCache.txt", "/tmp/harvest-translate-XXXX"),
            ],
        );
        assert_eq!(
            digest_tree(a.path()).unwrap(),
            digest_tree(b.path()).unwrap(),
            "logs and build output must not affect identity"
        );
    }

    #[test]
    fn digest_changes_when_a_source_byte_changes() {
        let a = crate::io::workdir::test_tempdir().unwrap();
        tree(a.path(), &[("src/lib.rs", "fn a() {}")]);
        let b = crate::io::workdir::test_tempdir().unwrap();
        tree(b.path(), &[("src/lib.rs", "fn b() {}")]);
        assert_ne!(
            digest_tree(a.path()).unwrap(),
            digest_tree(b.path()).unwrap()
        );
    }

    #[test]
    fn digest_is_path_independent() {
        // Path independence is what lets one phase's output key the next phase's lookup.
        let a = crate::io::workdir::test_tempdir().unwrap();
        let b = crate::io::workdir::test_tempdir().unwrap();
        for r in [a.path(), b.path()] {
            tree(
                r,
                &[("src/lib.rs", "fn a() {}"), ("Cargo.toml", "[package]")],
            );
        }
        assert_eq!(
            digest_tree(a.path()).unwrap(),
            digest_tree(b.path()).unwrap()
        );
    }

    /// `verify_case` spawns a real agent and cannot be unit-tested, so the plumbing
    /// between the phases is covered nowhere else.
    #[test]
    fn lifecycle_round_trips_from_translated_to_verified() {
        let case = crate::io::workdir::test_tempdir().unwrap();
        let translated = case.path().join(crate::battery::TRANSLATED);
        tree(
            &translated,
            &[
                ("Cargo.toml", "[package]\nname=\"x\""),
                ("src/lib.rs", "pub fn a() {}"),
                ("c_src/src/lib.c", "int a(void){return 0;}"),
                ("logs/translation.log", "agent transcript"),
                ("target/debug/junk", "build output"),
            ],
        );

        let sealed = Sealed::<Translate>::adopt(case.path()).expect("adopt");

        let scratch = Scratch::new("test-work-").unwrap();
        let work: WorkTree<Verify> = sealed.materialise_into(scratch).expect("materialise");
        let crate_dir = work.crate_dir();
        assert!(
            crate_dir.join("src/lib.rs").is_file(),
            "source must be copied"
        );
        assert!(
            crate_dir.join("c_src/src/lib.c").is_file(),
            "C oracle must be copied"
        );
        assert!(
            crate_dir.join("logs/translation.log").is_file(),
            "logs must still reach the agent — parity with the previous behaviour"
        );
        assert!(
            !crate_dir.join("target").exists(),
            "build output must NOT be copied"
        );

        // Stand in for the agent: edit the Rust, and bake a scratch path into a note.
        let c_before = work.c().digest().unwrap();
        std::fs::write(
            crate_dir.join("src/lib.rs"),
            "pub fn a() { /* verified */ }",
        )
        .unwrap();
        std::fs::write(
            crate_dir.join("SYMBOLS.md"),
            format!("built in {}\n", crate_dir.display()),
        )
        .unwrap();

        let scrubbed = work.scrub().expect("scrub");
        assert!(
            scrubbed
                .rewritten()
                .iter()
                .any(|r| r.as_path().ends_with("SYMBOLS.md")),
            "the embedded scratch path must be rewritten, else the digest varies per run"
        );

        let verified = scrubbed
            .seal(&crate::agent_health::Completed::for_test(), &c_before)
            .expect("seal");
        verified.publish(case.path()).expect("publish");

        let out = case.path().join(crate::battery::VERIFIED);
        assert_eq!(
            std::fs::read_to_string(out.join("src/lib.rs")).unwrap(),
            "pub fn a() { /* verified */ }",
            "the agent's edit must reach verified/"
        );
        assert!(
            out.join("c_src/src/lib.c").is_file(),
            "c_src must be seeded from translated/"
        );
        assert!(
            !out.join("target").exists(),
            "build output must not be published"
        );
        assert_eq!(
            std::fs::read_to_string(translated.join("src/lib.rs")).unwrap(),
            "pub fn a() {}",
            "translated/ must be left untouched — verify is pure"
        );
    }

    /// One root, many cases — the shape `materialise_into` cannot express, and the
    /// prerequisite for scoring a battery outside the tree it scores.
    #[test]
    fn one_scratch_root_holds_many_cases_and_outlives_each_of_them() {
        let case = crate::io::workdir::test_tempdir().unwrap();
        tree(
            &case.path().join(crate::battery::TRANSLATED),
            &[("Cargo.toml", "[package]"), ("src/lib.rs", "pub fn a() {}")],
        );
        let sealed = Sealed::<Translate>::adopt(case.path()).unwrap();

        let root = Scratch::new("test-battery-").unwrap();
        let a: WorkTree<Verify> = sealed
            .materialise_at(root.subdir("case-a").unwrap())
            .unwrap();
        let b: WorkTree<Verify> = sealed
            .materialise_at(root.subdir("case-b").unwrap())
            .unwrap();

        assert_eq!(
            a.path().parent(),
            b.path().parent(),
            "both cases must share the one root"
        );
        assert_ne!(
            a.path(),
            b.path(),
            "each case still needs a crate of its own to build in"
        );
        for w in [&a, &b] {
            assert!(
                w.crate_dir().join("src/lib.rs").is_file(),
                "the crate must be copied"
            );
        }

        let b_crate = b.crate_dir();
        drop(b);
        assert!(
            b_crate.join("src/lib.rs").is_file(),
            "one case ending must not delete the root the others are still building in"
        );

        assert!(
            root.subdir("/abs").is_err(),
            "a destination outside the root must be refused"
        );
        assert!(root.subdir("../up").is_err());
    }

    #[test]
    fn seal_refuses_when_the_agent_modified_the_c_oracle() {
        let case = crate::io::workdir::test_tempdir().unwrap();
        tree(
            &case.path().join(crate::battery::TRANSLATED),
            &[
                ("Cargo.toml", "[package]"),
                ("c_src/src/lib.c", "int a(void){return 0;}"),
            ],
        );
        let sealed = Sealed::<Translate>::adopt(case.path()).unwrap();
        let work: WorkTree<Verify> = sealed
            .materialise_into(Scratch::new("t-").unwrap())
            .unwrap();
        let c_before = work.c().digest().unwrap();

        std::fs::write(
            work.crate_dir().join("c_src/src/lib.c"),
            "int a(void){return 1;}",
        )
        .unwrap();

        let err = work
            .scrub()
            .unwrap()
            .seal(&crate::agent_health::Completed::for_test(), &c_before)
            .expect_err("modifying the oracle must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("modified the C oracle"), "{msg}");
    }

    #[test]
    fn debug_on_sealed_reveals_the_digest_not_the_location() {
        // Formatting must not be a way to recover a path and run something there.
        let d = TreeDigest("sha256:abc123def456".into());
        let s = format!("{d:?}");
        assert!(s.starts_with("sha256:"), "{s}");
        assert!(!s.contains('/'), "a digest must not look like a path: {s}");
    }
}
