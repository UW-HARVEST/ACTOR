//! Typed phase artifacts: what an agent produced, and what may be done with it.
//!
//! Four invariants are enforced by the compiler, not by convention:
//! * Nothing runs in a published artifact: `Command::current_dir` and `--target-dir`
//!   both take `impl AsRef<Path>`, so "can obtain a path" *is* "can execute here", and
//!   [`Sealed`] yields no path in any form; the scorers build in [`crate::eval`]'s tree, not in one.
//! * An infra-failed run cannot be sealed: [`Scrubbed::seal`] demands a
//!   [`crate::domain::health::Completed`], mintable only from a completed run.
//! * A tree cannot be hashed before it is scrubbed: agent output embeds the random
//!   scratch directory name, so a digest of raw output changes every run.
//! * A published tree is digested AFTER its phase edited it: [`Publishing::finish`] is its only exit.

use crate::domain::contents::{classify, Carry, Disposition, C_ORACLE_DIR};
use crate::domain::health::Completed;
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
    /// The transcript this phase tees, under `<DIR>/logs/`.
    const LOG: &'static str;
    /// What this phase records beside its artifact, in the phase dir with the log. Every
    /// reader resolves both from the phase dir — `agent_health::audit`, the `oracle/`
    /// enrichers, `battery::extract_agent_meta` — and three of translate's four paths wrote
    /// the case ROOT, so an infra failure on those backends was scored as a result and
    /// `exit_code`/`timed_out` (the 124 a wall-clock kill leaves) were read by nothing.
    const METRICS: &'static str;
    /// Phase dirs a CHANGED result of this phase makes stale, which [`Publishing::finish`] removes
    /// whole. A *different* translation invalidates `verified/`: the verification it holds was
    /// performed on the previous one, and the "already verified" skip keys on
    /// `verified/logs/verify.log`, so keeping just its logs makes verify skip the case.
    const INVALIDATES: &'static [&'static str];
}

#[derive(Copy, Clone)]
pub struct Translate;
#[derive(Copy, Clone)]
pub struct Verify;

impl sealed_trait::Sealed for Translate {}
impl sealed_trait::Sealed for Verify {}

impl Phase for Translate {
    const DIR: &'static str = crate::battery::TRANSLATED;
    const LOG: &'static str = "translation.log";
    const METRICS: &'static str = "translation.json";
    const INVALIDATES: &'static [&'static str] = &[crate::battery::VERIFIED];
}
impl Phase for Verify {
    const DIR: &'static str = crate::battery::VERIFIED;
    const LOG: &'static str = "verify.log";
    const METRICS: &'static str = "verification.json";
    const INVALIDATES: &'static [&'static str] = &[];
}

/// Verify has no dependent to invalidate; asserted rather than assumed, since [`Publishing::finish`] acts.
const _: () = assert!(Verify::INVALIDATES.is_empty());

/// Inside the phase's own dir, which is what lets [`clear_phase`] keep the transcript while
/// replacing the artifact around it.
pub(crate) fn phase_logs<P: Phase>(case_dir: &Path) -> PathBuf {
    crate::battery::phase_dir(case_dir, P::DIR).join("logs")
}

/// THE log path of a phase: a function of the phase, so it cannot have four homes again.
pub(crate) fn phase_log<P: Phase>(case_dir: &Path) -> PathBuf {
    phase_logs::<P>(case_dir).join(P::LOG)
}

pub(crate) fn phase_metrics<P: Phase>(case_dir: &Path) -> PathBuf {
    crate::battery::phase_dir(case_dir, P::DIR).join(P::METRICS)
}

/// Make room for a fresh result of this phase — THIS phase's dir only. `INVALIDATES` is
/// [`Publishing::finish`]'s: whether a dependent went stale is unknown until the tree is digested.
///
/// Call it immediately before the new output is written, never before the agent starts: the
/// four translate paths used to `remove_dir_all` the whole case up front, so a crash, an API
/// outage or a timeout left the case holding *nothing* where a complete result had been, and
/// took `verified/` plus the staged `test_vectors/` and `runner/`, which translate does not
/// own. `logs/` survives, because the transcript is teed there while the agent runs.
///
/// `pub(crate)`, not `pub`: a recursive delete parameterised by a caller-supplied `case_dir`
/// is not crate-external API, and `no_public_path_escapes_the_artifact_modules` reads only
/// impls and structs, so nothing would have caught it escaping.
pub(crate) fn clear_phase<P: Phase>(case_dir: &Path) -> Result<()> {
    for entry in artifact_children::<P>(case_dir)? {
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        }
        .with_context(|| format!("clearing {}", p.display()))?;
    }
    Ok(())
}

/// Remove what a CHANGED result of this phase makes stale. Deferred out of [`clear_phase`] to
/// [`Publishing::finish`]: `verify` seeds itself by REPUBLISHING each case's stored translation, so
/// invalidating on every publish deleted the verification the command was asked to check — crate,
/// `logs/` and `verification.json`, 248 in the shipped tree — for a translation that had not moved.
fn invalidate_dependents<P: Phase>(case_dir: &Path) -> Result<()> {
    for stale in P::INVALIDATES {
        let dir = crate::battery::phase_dir(case_dir, stale);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("removing the stale {}", dir.display()))?;
        }
    }
    Ok(())
}

/// What the phase dir holds NOW, measured as the phase seeded from it keys it — verify's `input_tree`
/// IS [`Published::digest`] of the translation — so "the tree changed" and "the key moved" are one
/// comparison, post-seal edits on both sides. `None` (no dir, or unhashable) keeps the invalidation.
fn phase_dir_digest<P: Phase>(case_dir: &Path) -> Option<TreeDigest> {
    let dir = crate::battery::phase_dir(case_dir, P::DIR);
    if !dir.is_dir() {
        return None;
    }
    digest_tree(&dir).ok()
}

/// What a fresh result of this phase replaces: everything in the phase dir but `logs/`, which holds
/// the transcript being written live. Shared, so [`clear_phase`] and [`displace_phase`] agree.
fn artifact_children<P: Phase>(case_dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let dst = crate::battery::phase_dir(case_dir, P::DIR);
    children_but_logs(std::fs::read_dir(&dst), &dst)
}

/// ONLY `NotFound` is an empty phase. A `--parallel` sweep at the process fd limit fails `read_dir`
/// on a dir that IS there, and calling that "no artifact" makes [`clear_phase`] remove nothing while
/// the new files land on top of the old, publishing a union that hashes as neither run. Takes the
/// `io::Result` because a test cannot reach the fd limit.
fn children_but_logs(
    entries: std::io::Result<std::fs::ReadDir>,
    dst: &Path,
) -> Result<Vec<std::fs::DirEntry>> {
    let entries = match entries {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other.with_context(|| format!("reading the phase dir {}", dst.display()))?,
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading an entry of {}", dst.display()))?;
        if entry.file_name() != "logs" {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Move this phase's artifact OUT of the phase dir, keeping `logs/`, and say where it went.
///
/// [`clear_phase`]'s counterpart for a run that published nothing. It may not delete: an artifact
/// from a `--cache off` sweep, or from before the store existed, is replayable from nowhere, so a
/// delete makes a transient failure permanent. It may not leave it either — the failed run's
/// transcript and metrics are in that phase dir already, so the enrichers would take the earlier
/// crate for this run's. A sibling is in no digest, no reader's path and no `INVALIDATES` list.
pub(crate) fn displace_phase<P: Phase>(case_dir: &Path) -> Result<Option<PathBuf>> {
    // The crate, because that is what a reader scores; a metrics file is not an artifact, and moving
    // one aside would overwrite what an earlier displacement left here.
    let dir = crate::battery::phase_dir(case_dir, P::DIR);
    if !crate::battery::has_crate(&dir) {
        return Ok(None);
    }
    let moving = artifact_children::<P>(case_dir)?;
    ensure_displaceable(&dir, moving.len())?;
    let aside = case_dir.join(format!("{}.displaced", P::DIR));
    if aside.exists() {
        std::fs::remove_dir_all(&aside).with_context(|| format!("clearing {}", aside.display()))?;
    }
    std::fs::create_dir_all(&aside).with_context(|| format!("creating {}", aside.display()))?;
    for entry in moving {
        let from = entry.path();
        let to = aside.join(entry.file_name());
        std::fs::rename(&from, &to)
            .with_context(|| format!("moving {} aside to {}", from.display(), to.display()))?;
    }
    Ok(Some(aside))
}

/// `has_crate` is a `stat` and the walk is a `read_dir`, so they can disagree. Checked before the
/// sibling is created, because a caller told "the previous verified/ is at <aside>" with the crate
/// still in the phase dir beside the failed run's transcript is worse off than one told nothing.
fn ensure_displaceable(dir: &Path, moving: usize) -> Result<()> {
    anyhow::ensure!(
        moving > 0,
        "{} holds a crate but walking it found nothing to move aside",
        dir.display()
    );
    Ok(())
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

/// Only from a *published translation*: re-verifying a verification does not compile, and neither
/// does seeding from a [`Sealed`] — [`Publishing::edited`] rewrites `Cargo.toml` in between.
impl SeededBy<Published<Translate>> for Verify {
    const AT: SeedAt = SeedAt::CrateRoot;
}

/// The tree a work dir was materialised FROM, kept so an entry can record the input its key names
/// and not merely a digest of it. Two variants, not one path plus a flag: the two phases hash their
/// seed through DIFFERENT predicates, and an export that does not match the digest's predicate
/// cannot re-derive `meta.input_tree`. Putting the predicate in the variant is what makes that
/// impossible to get wrong at a call site.
#[derive(Clone, Debug)]
pub enum Seed {
    /// Translate's input: the C corpus, hashed by the oracle predicate: everything but build output.
    FromCorpus(PathBuf),
    /// Verify's input: a sealed translation, hashed by the artifact predicate: StoreAndHash only.
    FromArtifact(PathBuf),
}

impl Seed {
    /// Copy the seed somewhere the store can keep it. The `Carry` per variant admits at least
    /// everything the matching digest hashes, and MORE than that on purpose: the digest covers only
    /// part of the tree, and a stored input that dropped the rest would re-hash correctly while
    /// being useless for the future re-key this exists for.
    pub fn export_into(&self, dest: &Path) -> Result<()> {
        match self {
            Seed::FromCorpus(root) => copy_carrying(root, dest, Carry::IntoWorkTree),
            Seed::FromArtifact(root) => copy_carrying(root, dest, Carry::FromArtifact),
        }
    }

    /// Re-derive, from a COPY of this seed, the digest the original contributed to the key. If this
    /// does not equal `meta.input_tree`, the export and the digest disagree and the entry cannot be
    /// re-keyed.
    pub fn digest_at(&self, root: &Path) -> Result<TreeDigest> {
        match self {
            Seed::FromCorpus(_) => hash_tree(root, &oracle_admits),
            Seed::FromArtifact(_) => digest_tree(root),
        }
    }

    /// Which algorithm produced the digest, recorded in the entry so a future re-key knows what it
    /// is converting FROM.
    pub fn algorithm(&self) -> &'static str {
        match self {
            Seed::FromCorpus(_) => ORACLE_TREE_ALGORITHM,
            Seed::FromArtifact(_) => TREE_ALGORITHM,
        }
    }
}

/// The C sources an agent translates: an INPUT, never an output. Not a [`Phase`]: a
/// `Sealed<Corpus>` would inherit [`Sealed::publish`], and with `DIR = "test_case"` that
/// deletes the experiment's own input.
pub struct Corpus {
    c: OracleDir,
}

impl Corpus {
    /// The only constructor. `is_dir` is what keeps [`OracleDir::digest`]'s fabricated
    /// `sha256:absent` unreachable from a cache key.
    pub fn adopt(dir: &Path) -> Result<Self> {
        anyhow::ensure!(
            dir.is_dir(),
            "no C corpus at {}: an absent input would otherwise be keyed as a real one",
            dir.display()
        );
        Ok(Self {
            c: OracleDir(dir.to_path_buf()),
        })
    }

    /// Where this corpus lives, so the store can record the tree the key was computed from.
    pub fn as_seed(&self) -> Seed {
        Seed::FromCorpus(self.c.0.clone())
    }

    /// The corpus as the agent will see it. Through [`OracleDir`], never `digest_tree`: with
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
/// Named so the entry's record and the hasher cannot disagree about which algorithm ran.
pub const TREE_ALGORITHM: &str = "harvest-tree-v1";
/// The oracle walk hashes a different file set, so it is a different algorithm.
pub const ORACLE_TREE_ALGORITHM: &str = "harvest-oracle-tree-v1";

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
        let ft = entry.file_type()?;
        let ft = if ft.is_symlink() {
            // Resolved rather than emitted, because this traversal follows links deliberately
            // (see [`digest_tree`]); one whose per-run target is gone has nothing to follow, and
            // the shipped `results/` holds 17 of those, 16 inside a published phase dir.
            // NotFound only, propagating the rest, mirroring `children_but_logs` above. Swallowing
            // every error here would drop an unresolvable entry from BOTH the copy AND the digest,
            // so the two would agree, the store would validate the truncated tree, and nothing
            // could report it -- where the base behaviour was a loud refusal from `read`/`copy`.
            // ELOOP is the input that proves the difference: a symlink cycle must still refuse.
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

    /// Room for ONE case inside a root shared by many. [`Published::materialise_into`]
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
/// caller can do with one is pass it to [`Published::materialise_at`] — which is what stops
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

    pub fn c(&self) -> OracleDir {
        OracleDir(self.crate_dir().join(C_ORACLE_DIR))
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

/// Hands out no `&Path` and writes nothing, so nothing reached through it can alter the
/// reference. The agent subprocess holds [`WorkTree::path`] and *can*, hence the compare in
/// [`Scrubbed::seal`] — which then deletes only what that compare judged to be build output.
/// The C reference we grade against, as a directory. Named for its JOB rather than its contents:
/// `CDir` said "a directory of C", which is also true of a work tree's sources.
pub struct OracleDir(PathBuf);

/// The oracle's traversal predicate, spelled once for [`OracleDir::digest`], which keys the translate
/// cache, and for the seal guard. Wider than [`digest_tree`]'s: from `c_src` as its own root the
/// root-anchored rules cannot see the prefix, so [`classify`] calls `.bak`/`.log`/`.sha256`
/// `Ignore` although they are reference source — narrowing it loses 26 stored cases' files.
fn oracle_admits(d: Disposition) -> bool {
    d != Disposition::BuildOutput
}

impl OracleDir {
    /// Everything but build output — the files [`classify`] keeps under `c_src/` from the root.
    pub fn digest(&self) -> Result<TreeDigest> {
        if self.0.is_dir() {
            hash_tree(&self.0, &oracle_admits)
        } else {
            Ok(TreeDigest("sha256:absent".into()))
        }
    }

    /// The oracle as a file set, not one number, which is what [`Scrubbed::seal`] compares.
    pub fn snapshot(&self) -> Result<Oracle> {
        if !self.0.is_dir() {
            return Ok(Oracle(Reference::Ungraded));
        }
        let mut files = std::collections::BTreeMap::new();
        for (rel, abs) in self.contents()? {
            files.insert(rel, file_digest(&abs)?);
        }
        if files.is_empty() {
            return Err(crate::refusal::Refusal::OracleEmpty {
                at: self.0.display().to_string(),
            }
            .into());
        }
        Ok(Oracle(Reference::Graded(OracleFiles(files))))
    }

    /// The files [`Self::digest`] hashes, by path.
    fn contents(&self) -> Result<std::collections::BTreeMap<RelPath, PathBuf>> {
        let mut files = std::collections::BTreeMap::new();
        if !self.0.is_dir() {
            return Ok(files);
        }
        refuse_symlink(&self.0, Path::new(C_ORACLE_DIR))?;
        self.walk(&self.0, false, &mut files)?;
        Ok(files)
    }

    /// Not [`visit`], which sniffs the directory it STARTS in — a `CMakeCache.txt` at the oracle
    /// root would read the whole reference as build output and hide every addition beside it, while
    /// sub-directories must still be sniffed. It also descends on `is_dir()`, which follows:
    /// refusing a link at DESCENT, not at emission, is what covers a directory link whose subtree
    /// emits nothing — an empty target, or one holding only `build/` — and is otherwise unnamed.
    fn walk(
        &self,
        dir: &Path,
        in_build_dir: bool,
        files: &mut std::collections::BTreeMap<RelPath, PathBuf>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let Ok(Ok(rel)) = path.strip_prefix(&self.0).map(RelPath::new) else {
                continue;
            };
            if !oracle_admits(classify(&rel, in_build_dir)) {
                continue;
            }
            refuse_symlink(&path, rel.as_path())?;
            if path.is_dir() {
                self.walk(&path, in_build_dir || is_cmake_build_dir(&path), files)?;
            } else {
                files.insert(rel, path);
            }
        }
        Ok(())
    }
}

/// Refused, not skipped, and only for the oracle root and the admitted paths beneath it: the rest
/// of the `WorkTree` the agent holds is uncovered and needs none, since nothing here reads or
/// unlinks outside `c_src`. [`drop_build_products`] unlinks what the walk named and `remove_file`
/// resolves all but the last component, so a link anywhere on a path deletes what we do not own.
fn refuse_symlink(abs: &Path, named: &Path) -> Result<()> {
    if std::fs::symlink_metadata(abs)
        .with_context(|| format!("inspecting {}", abs.display()))?
        .file_type()
        .is_symlink()
    {
        return Err(crate::refusal::Refusal::OracleModified {
            change: crate::refusal::OracleChange::Symlinked,
            file: named.display().to_string(),
        }
        .into());
    }
    Ok(())
}

/// The C reference as the agent was handed it, file by file. A whole-directory digest answers
/// "did anything differ", the wrong question: the translate prompt tells the agent to build the C
/// library and `nm -D` the resulting `.so`, so a difference is what compliance looks like — 532 of
/// 6,312 stored `translated/*/c_src` trees hold 3,276 compiled artefacts at paths this walk
/// covers. The invariant is "the reference we grade against is the one we shipped", not "nothing
/// differs". Never empty, `NONE` aside: [`OracleDir::snapshot`] refuses rather than record nothing.
#[derive(Debug)]
pub struct OracleFiles(std::collections::BTreeMap<RelPath, [u8; 32]>);

/// Proof that the oracle was READ, and the only thing [`Scrubbed::seal`] grades against. The
/// private field IS the invariant: a public field-less variant compiled from outside the crate, and
/// a caller passing it sealed an edited or deleted reference silently, where a digest refused loud.
#[derive(Debug)]
pub struct Oracle(Reference);

/// Whether the tree handed to the agent HELD a reference to grade against. An empty file set stood
/// for both "nothing to check" and "the check saw nothing", and the second cannot fire: with no
/// recorded file, no edit, deletion or hiding is reachable and [`Scrubbed::seal`] seals whatever it
/// is handed. Both are real — 678 of 6,990 stored translations hold no `c_src` at all: c2saferrust
/// and smartc2rust entirely (338 each, seeded from a sibling backend's tree, not the corpus), plus
/// `CRUST/kiro/impcheck` and one Test-Corpus case — so only the second refuses.
#[derive(Debug)]
enum Reference {
    Graded(OracleFiles),
    /// No reference was handed over, so the one rule left is that the run may not invent one.
    Ungraded,
}

impl Oracle {
    fn judge(&self, artifact: &Path) -> Result<OracleVerdict> {
        match &self.0 {
            Reference::Graded(files) => files.judge(artifact),
            Reference::Ungraded => OracleFiles::NONE.judge(artifact),
        }
    }
}

enum OracleVerdict {
    Tampered(crate::refusal::Refusal),
    /// What building the reference left — absolute, because [`drop_build_products`] deletes it.
    BuiltInPlace(Vec<PathBuf>),
}

impl OracleFiles {
    /// Only `Reference::Ungraded` reaches it: judging additions against nothing IS "invent none".
    const NONE: Self = Self(std::collections::BTreeMap::new());

    /// Gone, edited or no longer carried is tampering; so is an addition where the reference stood.
    fn judge(&self, artifact: &Path) -> Result<OracleVerdict> {
        use crate::refusal::{OracleChange, Refusal};
        let tampered = |change, rel: &RelPath| {
            Ok(OracleVerdict::Tampered(Refusal::OracleModified {
                change,
                file: rel.as_path().display().to_string(),
            }))
        };
        let now = OracleDir(artifact.join(C_ORACLE_DIR));
        let carried = oracle_paths_in_artifact(artifact)?;
        for (rel, before) in &self.0 {
            let abs = now.0.join(rel.as_path());
            if !abs.is_file() {
                return tampered(OracleChange::Removed, rel);
            }
            if !carried.contains(rel) {
                return tampered(OracleChange::Hidden, rel);
            }
            if &file_digest(&abs)? != before {
                return tampered(OracleChange::Edited, rel);
            }
        }
        let mut built = Vec::new();
        for (rel, abs) in now.contents()? {
            if self.0.contains_key(&rel) {
                continue;
            }
            if !is_build_product(&rel, &head(&abs)?) && self.occupied_dir_of(&rel) {
                return tampered(OracleChange::Added, &rel);
            }
            built.push(abs);
        }
        Ok(OracleVerdict::BuiltInPlace(built))
    }

    /// Did the reference we were handed occupy this path's directory? One it never occupied is one the
    /// build created, so nothing inside it can be a change to the reference — the question `BUILD_DIRS`
    /// approximated by NAME, one project at a time, until SPHINCS+'s plain-Makefile `build-<variant>/`
    /// would have made it twelve (`docs/prs/spec-25.md`). Empty is [`Reference::Ungraded`]: a run that
    /// was handed no reference may not invent one, so nothing there is a build's either.
    fn occupied_dir_of(&self, rel: &RelPath) -> bool {
        let dir = rel.as_path().parent().unwrap_or(Path::new(""));
        self.0.is_empty() || self.0.keys().any(|k| k.as_path().starts_with(dir))
    }
}

/// The reference as the artifact will actually contain it, relative to the oracle. Walked from the
/// artifact root, where [`digest_tree`] and [`copy_carrying`] start and every directory is sniffed
/// as they descend: one empty `c_src/CMakeFiles/` leaves the reference on disk but out of the seal.
fn oracle_paths_in_artifact(artifact: &Path) -> Result<std::collections::BTreeSet<RelPath>> {
    let mut under_oracle = std::collections::BTreeSet::new();
    visit(
        artifact,
        artifact,
        false,
        &|d| Carry::FromArtifact.admits(d),
        &mut |rel, _| {
            if let Ok(Ok(rel)) = rel.as_path().strip_prefix(C_ORACLE_DIR).map(RelPath::new) {
                under_oracle.insert(rel);
            }
            Ok(())
        },
    )?;
    Ok(under_oracle)
}

/// Delete what building the reference left rather than hash and store it: a build product is where
/// the per-run scratch path gets baked in (22 of 201 stored `.o` files at digest-covered paths hold
/// one) and `scrub` cannot reach it, reading UTF-8 and skipping a binary silently — so the digest of
/// byte-identical Rust would differ every run and the verify entry it keys never hit. Deleted, not
/// reclassified: [`classify`] is what the pinned golden digests measure. Reported, not silent, and
/// unlinked by this process rather than the agent, so every path came through [`refuse_symlink`].
fn drop_build_products(products: &[PathBuf]) -> Result<()> {
    for abs in products {
        std::fs::remove_file(abs)
            .with_context(|| format!("removing the build product {}", abs.display()))?;
    }
    if !products.is_empty() {
        eprintln!(
            "  dropped {} file(s) that building the C reference left under {C_ORACLE_DIR}/",
            products.len()
        );
    }
    Ok(())
}

fn file_digest(path: &Path) -> Result<[u8; 32]> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Sha256::digest(&bytes).into())
}

/// The longest magic below. Not `fs::read`: an added `.so` runs to megabytes.
const MAGIC_BYTES: u64 = 8;

fn head(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .take(MAGIC_BYTES)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(bytes)
}

/// What a C build leaves beside the sources it compiled: `.d`/`.la` text, `.gcda`/`.gcno`
/// coverage counters, all of it generated.
const BUILD_PRODUCT_EXTS: &[&str] = &[
    "o", "a", "so", "lo", "la", "obj", "d", "gch", "pch", "dylib", "gcda", "gcno",
];

/// ELF, an `ar` archive, Mach-O in both byte orders plus its fat header, and gcov's own pair.
const BUILD_PRODUCT_MAGIC: &[&[u8]] = &[
    b"\x7fELF",
    b"!<arch>\n",
    &[0xfe, 0xed, 0xfa, 0xce],
    &[0xce, 0xfa, 0xed, 0xfe],
    &[0xfe, 0xed, 0xfa, 0xcf],
    &[0xcf, 0xfa, 0xed, 0xfe],
    &[0xca, 0xfe, 0xba, 0xbe],
    b"adcg",
    b"oncg",
];

/// Did building the reference produce this file, rather than the reference shipping it?
/// Sniffed as well as matched on the extension: of the 532 stored trees that built the oracle
/// in place, 294 hold a product the extension list cannot name (`c_src/test_runner`,
/// `c_src/main` — compiled, and with no extension at all).
fn is_build_product(rel: &RelPath, head: &[u8]) -> bool {
    if BUILD_PRODUCT_MAGIC.iter().any(|m| head.starts_with(m)) {
        return true;
    }
    let Some(name) = rel.as_path().file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let mut rest = name;
    // `libfoo.so.1.2.3`: strip the version tail, then judge the extension under it.
    while let Some((stem, last)) = rest.rsplit_once('.') {
        if !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()) {
            rest = stem;
            continue;
        }
        return BUILD_PRODUCT_EXTS.contains(&last);
    }
    false
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

    pub fn seal(self, _proof: &Completed, c_before: &Oracle) -> Result<Sealed<P>> {
        match c_before.judge(&self.root)? {
            OracleVerdict::Tampered(refusal) => return Err(refusal.into()),
            OracleVerdict::BuiltInPlace(products) => drop_build_products(&products)?,
        }
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

    /// Consumes `self`: [`Publishing::edited`] changes the tree, so a `Sealed` usable afterwards would
    /// be one whose digest describes a tree that no longer exists.
    pub fn publish(self, case_dir: &Path) -> Result<Publishing<P>> {
        let replaced = phase_dir_digest::<P>(case_dir);
        clear_phase::<P>(case_dir)?;
        let dst = crate::battery::phase_dir(case_dir, P::DIR);
        self.assemble_into(case_dir, &dst)?;
        Ok(Publishing {
            root: dst,
            case_dir: case_dir.to_path_buf(),
            replaced,
            keying: Keying::Keyed,
            _phase: PhantomData,
        })
    }

    /// Factored out of [`Self::publish`]: "what this phase's tree contains" has one answer.
    pub fn assemble_into(&self, case_dir: &Path, dst: &Path) -> Result<()> {
        assemble::<P>(&self.root, case_dir, dst)
    }
}

/// A phase dir this run has just written, whose only exit is [`Self::finish`] — which digests the tree AS
/// IT THEN STANDS. Post-processing edited `translated/Cargo.toml` after `publish` wrote it, so what was
/// scored was not what was sealed: for every case, always.
#[must_use = "a published phase dir that is never finished has no digest, so nothing can name it"]
pub struct Publishing<P: Phase> {
    root: PathBuf,
    case_dir: PathBuf,
    /// What the phase dir held before, so [`Self::finish`] can tell a republish from a new tree.
    replaced: Option<TreeDigest>,
    keying: Keying,
    _phase: PhantomData<P>,
}

/// Whether a cache key names this artifact, so "its inputs are this run's inputs" is a guarantee and
/// not a hope about bytes on disk. Only a [`Sealed`] — which needs a [`Completed`] — can be `Keyed`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Keying {
    Keyed,
    Unkeyable,
}

impl<P: Phase> fmt::Debug for Publishing<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Publishing<{}>", P::DIR)
    }
}

impl<P: Phase> Publishing<P> {
    #[cfg(test)]
    pub(crate) fn for_test(case_dir: &Path) -> Self {
        Self {
            root: crate::battery::phase_dir(case_dir, P::DIR),
            case_dir: case_dir.to_path_buf(),
            replaced: None,
            keying: Keying::Keyed,
            _phase: PhantomData,
        }
    }

    /// The edits this phase makes to its OWN published tree, before anything digests it. Handed the tree
    /// and not a case dir, so the destination cannot be another phase's, and consuming `self` makes an
    /// edit provably precede [`Self::finish`]. Best-effort, as it has always been.
    pub fn edited(self, edit: impl FnOnce(&Path) -> Result<()>) -> Self {
        if let Err(e) = edit(&self.root) {
            eprintln!(
                "  ⚠️  post-processing {} failed; published unedited: {e:#}",
                self.root.display()
            );
        }
        self
    }

    pub fn finish(self) -> Result<Published<P>> {
        let digest = digest_tree(&self.root);
        // Only a republish PROVEN identical skips the invalidation: a tree that cannot be digested, or
        // a phase dir that was not there to compare with, may not keep a dependent phase alive.
        if !matches!((&self.replaced, &digest), (Some(before), Ok(after)) if before == after) {
            invalidate_dependents::<P>(&self.case_dir)?;
        }
        Ok(Published {
            root: self.root,
            digest: digest?,
            keying: self.keying,
            _phase: PhantomData,
        })
    }
}

/// A phase's artifact as this run published it: a stable path, a digest of it, and no scratch
/// lifetime, so a battery's translations can be held while verify runs over them. THE hand-off
/// medium, taken BY VALUE: "seeded from a translation this run did not produce" does not compile.
pub struct Published<P: Phase> {
    root: PathBuf,
    digest: TreeDigest,
    keying: Keying,
    _phase: PhantomData<P>,
}

impl<P: Phase> fmt::Debug for Published<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Published<{}>({:?})", P::DIR, self.digest)
    }
}

impl<P: Phase> Published<P> {
    /// The tree already in the phase dir, taken as this run's artifact because NO KEY EXISTS to ask
    /// for a better one: an unkeyed backend, or a phase whose store is bypassed — refusing those
    /// left `--agent opencode` and every battery of symlinked configs unable to verify under any
    /// flag. `Sealed::adopt`'s shape without its power: it mints no [`Sealed`], so nothing here
    /// becomes a cache entry or stands in for a [`Completed`], and [`Keying::Unkeyable`] is
    /// recorded rather than a guarantee implied. A CRATE, not merely a directory: [`displace_phase`] keeps `logs/` when a run publishes nothing, so
    /// a transcript alone would be adopted as this run's artifact and scored a BUILD FAILURE, not absent.
    pub(crate) fn unkeyed_from_phase_dir(case_dir: &Path) -> Result<Self> {
        let root = crate::battery::phase_dir(case_dir, P::DIR);
        anyhow::ensure!(
            crate::battery::has_crate(&root),
            "no {} crate at {}",
            P::DIR,
            root.display()
        );
        Ok(Self {
            digest: digest_tree(&root)?,
            root,
            keying: Keying::Unkeyable,
            _phase: PhantomData,
        })
    }

    pub fn digest(&self) -> &TreeDigest {
        &self.digest
    }

    /// Printed per case by the sweeps: a number derived from an unkeyed artifact is a weaker claim.
    pub(crate) fn keying(&self) -> Keying {
        self.keying
    }

    /// Where this artifact lives, so a work dir seeded from it can tell the store what its key
    /// named. A [`Seed`], not a path: `sealed_has_no_path`'s property holds for this type too.
    pub fn as_seed(&self) -> Seed {
        Seed::FromArtifact(self.root.clone())
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
}

/// A published tree, MEASURED, and nothing else. `Sealed::adopt` served this — the golden
/// fingerprint pins `digest_tree` over 40 shipped `translated/` dirs — and served a whole artifact
/// with it: a `Sealed<P>` with no [`Completed`], which then seeded verify. A `String` mints
/// nothing.
pub struct Fingerprint(String);

impl Fingerprint {
    pub fn of_phase_dir<P: Phase>(case_dir: &Path) -> Result<Self> {
        let root = crate::battery::phase_dir(case_dir, P::DIR);
        anyhow::ensure!(
            root.is_dir(),
            "no {} phase dir at {}",
            P::DIR,
            root.display()
        );
        Ok(Self(digest_tree(&root)?.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// THE answer to "what does this phase's tree contain", for a sealed artifact and for the
/// unkeyed trees [`publish_unsealed`] takes — so `translated/` cannot mean a different file
/// set depending on which backend produced it.
fn assemble<P: Phase>(from: &Path, case_dir: &Path, dst: &Path) -> Result<()> {
    let translated = crate::battery::phase_dir(case_dir, crate::battery::TRANSLATED);
    if translated.is_dir() && P::DIR != crate::battery::TRANSLATED {
        copy_carrying(&translated, dst, Carry::FromPreviousPhase)?;
    }
    copy_carrying(from, dst, Carry::FromArtifact)
}

/// Publish a tree that no [`Completed`] can prove, carrying exactly what [`Sealed::publish`]
/// would have carried FROM THE SAME INPUT -- which is not the same as publishing the same tree,
/// and the difference is measured below.
///
/// Translate's unkeyed backends cannot become a [`Sealed`]: opencode's `run --format json`
/// mints no `Completed`, c2rust records no exit for the classifier to read, and the docker and
/// one-shot arms assemble their output in scratch with no [`WorkTree`] to scrub. So they publish
/// by copy — and each copy chose its own exclusions, `copy_dir_all` carrying `target/` and a
/// `skip = ["target"]` filter still carrying `c_src/build/`, which made `translated/` a
/// different file set per backend and the comparison across backends a comparison of two
/// shapes. Grants nothing a seal grants: no digest, no store entry, and no `Sealed` to
/// materialise or export from, so the three invariants in this module's header are untouched.
///
/// ONE DIVERGENCE REMAINS, and it is not closable here. The keyed path reaches this copy only
/// after [`Scrubbed::seal`], which runs [`drop_build_products`] over `c_src/` first, so a keyed
/// publish drops build products left among the oracle sources and an unkeyed one keeps them.
/// Measured with a probe: a work tree carrying `c_src/test_runner` (ELF magic) and `c_src/lib.o`
/// publishes them as absent on the keyed path and present on the unkeyed one. It is not a corner
/// case -- [`is_build_product`] records 294 of 532 stored trees holding `c_src/test_runner` or
/// `c_src/main`, and the shipped tree has 12 `.o`/`.a` files under
/// `CRUST/codex-gpt54/*/translated/c_src/`. So `translated/` still depends on which backend
/// produced it: Kiro and Claude drop that class, every other arm keeps it.
///
/// It cannot be fixed by calling `drop_build_products` here, because that step takes an
/// [`Oracle`] and an unkeyed path has no `Completed` with which to obtain one. Closing it means
/// giving the unkeyed backends a completion proof -- backlog #38 -- not widening this function.
///
/// It yields a [`Publishing`] and therefore a [`Published`], since it can digest what it just
/// wrote; what it cannot mint is a [`Sealed`], having no [`Completed`] — so STALENESS holds for all
/// seven translate arms while CACHING covers only the keyed two.
pub(crate) fn publish_unsealed<P: Phase>(from: &Path, case_dir: &Path) -> Result<Publishing<P>> {
    let replaced = phase_dir_digest::<P>(case_dir);
    clear_phase::<P>(case_dir)?;
    let dst = crate::battery::phase_dir(case_dir, P::DIR);
    assemble::<P>(from, case_dir, &dst)?;
    Ok(Publishing {
        root: dst,
        case_dir: case_dir.to_path_buf(),
        replaced,
        keying: Keying::Unkeyable,
        _phase: PhantomData,
    })
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

    /// The guard walks `c_src` as its OWN root, where the root-anchored rules in `classify`
    /// cannot see the `c_src` prefix, so oracle source reads `Ignore` there and
    /// `StoreAndHash` from the artifact root that carries it. Narrow the guard's predicate to
    /// the digest's and it stops recording the file, and an agent may rewrite the reference.
    #[test]
    fn the_oracle_guard_sees_a_bak_file_change() {
        let (work, c_before, _) = seeded_oracle();
        assert_eq!(
            (
                classify(&rel("doc/footer.html.bak"), false),
                classify(&rel("c_src/doc/footer.html.bak"), false),
            ),
            (Disposition::Ignore, Disposition::StoreAndHash),
            "fixture assumption: the same file, classified from the two roots"
        );

        std::fs::write(
            work.c().0.join("doc/footer.html.bak"),
            "the agent edited the oracle",
        )
        .unwrap();

        let err = seal_as_completed(work, &c_before).expect_err("an edited reference must refuse");
        assert_eq!(
            crate::refusal::Refusal::in_chain(&err),
            Some(&crate::refusal::Refusal::OracleModified {
                change: crate::refusal::OracleChange::Edited,
                file: "doc/footer.html.bak".into(),
            }),
            "{err:#}"
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
    fn a_corpus_seeds_the_oracle_and_a_published_translation_seeds_the_crate_root() {
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
            <Verify as SeededBy<Published<Translate>>>::AT,
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

    fn chmod(dir: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn a_crate_at(dir: &Path) {
        tree(
            dir,
            &[("Cargo.toml", "[package]"), ("src/lib.rs", "pub fn a() {}")],
        );
    }

    /// [`Sealed::publish`] clears the phase dir and then copies onto it, so a `read_dir` failure read
    /// as "nothing here" publishes the UNION of two runs: a tree whose digest matches neither.
    #[test]
    fn a_phase_dir_that_cannot_be_read_is_not_cleared_to_nothing() {
        let case = crate::io::workdir::test_tempdir().unwrap();
        let dir = case.path().join(crate::battery::TRANSLATED);
        a_crate_at(&dir);

        chmod(&dir, 0o000);
        let cleared = clear_phase::<Translate>(case.path());
        chmod(&dir, 0o755);

        let err = cleared.expect_err("an unreadable phase dir must not pass for an empty one");
        assert!(
            format!("{err:#}").contains("reading the phase dir"),
            "{err:#}"
        );
        assert!(
            dir.join("Cargo.toml").is_file(),
            "and the crate is still there, so reporting the clear as done was the lie"
        );
    }

    /// The half that must not regress with it: a case's first run has no phase dir at all, and
    /// refusing there would refuse every fresh case instead of the corrupt ones.
    #[test]
    fn an_absent_phase_dir_is_nothing_to_clear_and_nothing_to_displace() {
        let case = crate::io::workdir::test_tempdir().unwrap();
        let dir = case.path().join(crate::battery::VERIFIED);
        assert!(!dir.exists(), "fixture: nothing has run for this case yet");

        clear_phase::<Verify>(case.path()).expect("a first run must have room made for it");
        assert!(displace_phase::<Verify>(case.path()).unwrap().is_none());
    }

    /// `has_crate` is a `stat`, which answers yes on a dir the walk cannot read (mode `0o111` here,
    /// the fd limit in a sweep). Reported as a move, the operator is told the phase dir is clear
    /// while the earlier crate sits in it beside this run's transcript and `success: false`.
    #[test]
    fn a_displacement_that_moved_nothing_is_refused_rather_than_announced() {
        let case = crate::io::workdir::test_tempdir().unwrap();
        let dir = case.path().join(crate::battery::VERIFIED);
        a_crate_at(&dir);

        chmod(&dir, 0o111);
        let stat_says_crate = crate::battery::has_crate(&dir);
        let displaced = displace_phase::<Verify>(case.path());
        chmod(&dir, 0o755);

        assert!(stat_says_crate, "fixture: the stat must still answer yes");
        let err = displaced.expect_err("a displacement that moved nothing must refuse");
        assert!(
            format!("{err:#}").contains("reading the phase dir"),
            "{err:#}"
        );
        assert!(
            !case.path().join("verified.displaced").exists(),
            "and no empty sibling is left claiming to hold the crate"
        );
        assert!(
            dir.join("Cargo.toml").is_file(),
            "which is still here, {err:#}"
        );

        // Unreachable from the filesystem now that the walk propagates, so it is pinned here.
        assert!(ensure_displaceable(&dir, 0).is_err());
        ensure_displaceable(&dir, 1).expect("one child is a displacement");
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

        let published =
            Published::<Translate>::unkeyed_from_phase_dir(case.path()).expect("the fixture tree");

        let scratch = Scratch::new("test-work-").unwrap();
        let work: WorkTree<Verify> = published.materialise_into(scratch).expect("materialise");
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
        let c_before = work.c().snapshot().unwrap();
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
            .seal(&crate::domain::health::Completed::for_test(), &c_before)
            .expect("seal");
        verified
            .publish(case.path())
            .expect("publish")
            .finish()
            .expect("digest the published tree");

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

    /// SITE 17, and the probe that no key moved when it closed: post-processing edited the tree after
    /// the seal, so the recorded `output_tree` described 0 of 84 published trees. The edits now run on
    /// the way to [`Published`], so WHERE the digest is taken moves and not what it covers.
    #[test]
    fn a_published_translation_is_digested_after_its_post_processing() {
        let case = crate::io::workdir::test_tempdir().unwrap();
        let (work, c_before, _) = seeded_oracle();
        let sealed = work
            .scrub()
            .unwrap()
            .seal(&crate::domain::health::Completed::for_test(), &c_before)
            .unwrap();
        let sealed_digest = sealed.digest().clone();

        // `add_workspace`'s effect in one line: the ORDER is what is under test, not the rewrite.
        let published = sealed
            .publish(case.path())
            .unwrap()
            .edited(|tree| {
                let manifest = tree.join("Cargo.toml");
                let mut text = std::fs::read_to_string(&manifest)?;
                text.push_str("\n[workspace]\n");
                std::fs::write(&manifest, text)?;
                Ok(())
            })
            .finish()
            .unwrap();

        let manifest =
            crate::battery::phase_dir(case.path(), crate::battery::TRANSLATED).join("Cargo.toml");
        assert!(
            std::fs::read_to_string(&manifest)
                .unwrap()
                .contains("[workspace]"),
            "the edit must reach the published tree, or the ordering below is about nothing"
        );
        assert_ne!(
            &sealed_digest,
            published.digest(),
            "non-vacuity: the edit really does move the digest, which is why taking it at the \
             seal described a tree that no longer existed by the time anything read it"
        );
        assert_eq!(
            published.digest().as_str(),
            Fingerprint::of_phase_dir::<Translate>(case.path())
                .unwrap()
                .as_str(),
            "and what `Published` states is the digest OF THE PUBLISHED TREE — the same function \
             the 40 golden fingerprints pin, so verify's `input_tree` is unchanged"
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
        let published = Published::<Translate>::unkeyed_from_phase_dir(case.path()).unwrap();

        let root = Scratch::new("test-battery-").unwrap();
        let a: WorkTree<Verify> = published
            .materialise_at(root.subdir("case-a").unwrap())
            .unwrap();
        let b: WorkTree<Verify> = published
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
    fn an_edited_oracle_source_is_still_refused() {
        let case = crate::io::workdir::test_tempdir().unwrap();
        tree(
            &case.path().join(crate::battery::TRANSLATED),
            &[
                ("Cargo.toml", "[package]"),
                ("c_src/src/lib.c", "int a(void){return 0;}"),
            ],
        );
        let published = Published::<Translate>::unkeyed_from_phase_dir(case.path()).unwrap();
        let work: WorkTree<Verify> = published
            .materialise_into(Scratch::new("t-").unwrap())
            .unwrap();
        let c_before = work.c().snapshot().unwrap();
        assert!(
            matches!(&c_before.0, Reference::Graded(f) if f.0.contains_key(&rel("src/lib.c"))),
            "non-vacuity: the file this test edits must be one the snapshot recorded"
        );

        std::fs::write(
            work.crate_dir().join("c_src/src/lib.c"),
            "int a(void){return 1;}",
        )
        .unwrap();

        let err = work
            .scrub()
            .unwrap()
            .seal(&crate::domain::health::Completed::for_test(), &c_before)
            .expect_err("modifying the oracle must be refused");
        assert_eq!(
            crate::refusal::Refusal::in_chain(&err),
            Some(&crate::refusal::Refusal::OracleModified {
                change: crate::refusal::OracleChange::Edited,
                file: "src/lib.c".into(),
            }),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains("modified the C oracle"),
            "{err:#}"
        );
    }

    /// A translate work tree as handed over: the oracle as a file set, and as the digest it used.
    fn seeded_oracle() -> (WorkTree<Translate>, Oracle, TreeDigest) {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let src = tmp.path().join("corpus");
        tree(
            &src,
            &[
                ("src/lib.c", "int a(void){return 0;}"),
                ("include/a.h", "int a(void);"),
                // Reference source that reads as `Ignore` from the oracle root: 26 stored cases
                // hold this file, and only a predicate wider than the digest's records it.
                ("doc/footer.html.bak", "upstream"),
            ],
        );
        let work: WorkTree<Translate> = Corpus::adopt(&src)
            .unwrap()
            .materialise_into(Scratch::new("t-").unwrap())
            .unwrap();
        let files = work.c().snapshot().unwrap();
        let digest = work.c().digest().unwrap();
        assert!(
            matches!(&files.0, Reference::Graded(f) if f.0.len() == 3),
            "non-vacuity for every test below: they all judge against THIS snapshot, and one \
             that recorded nothing would make each of them green while inspecting no file"
        );
        tree(&work.crate_dir(), &[("Cargo.toml", "[package]")]);
        (work, files, digest)
    }

    fn seal_as_completed(work: WorkTree<Translate>, c_before: &Oracle) -> Result<()> {
        work.scrub()?
            .seal(&crate::domain::health::Completed::for_test(), c_before)
            .map(|_| ())
    }

    /// The prompt tells the agent to build the C library and `nm -D` the `.so`, so an in-tree
    /// `make` is compliance: 71 of 6,312 stored trees hold a `.o`, `.a` or `.so` it refused.
    /// The product is accepted and then dropped, so it never reaches the digest.
    #[test]
    fn an_added_object_file_beside_its_source_is_not_an_oracle_modification() {
        let (work, c_before, digest_before) = seeded_oracle();
        tree(
            &work.c().0,
            &[("src/foo.o", "not even ELF, just object bytes")],
        );
        assert_ne!(
            digest_before,
            work.c().digest().unwrap(),
            "fixture assumption: the digest the old check compared really did move"
        );
        seal_as_completed(work, &c_before).expect("a compiled artefact is not tampering");
    }

    /// The measured case `c_src/test_runner`: a bare ELF with no extension to match. 294 of the
    /// 532 affected trees hold one, so an extension list on its own would miss most of them.
    #[test]
    fn an_extensionless_compiled_binary_at_the_oracle_root_is_not_an_oracle_modification() {
        let (work, c_before, digest_before) = seeded_oracle();
        std::fs::write(
            work.c().0.join("test_runner"),
            b"\x7fELF\x02\x01\x01\x00 and the rest of a binary",
        )
        .unwrap();
        assert_ne!(
            digest_before,
            work.c().digest().unwrap(),
            "fixture assumption: the digest the old check compared really did move"
        );
        assert!(
            !is_build_product(&rel("test_runner"), b"#!/bin/sh\n"),
            "and the acceptance must come from the content, not from the bare name"
        );
        seal_as_completed(work, &c_before).expect("building the oracle is what the prompt asks");
    }

    /// Coverage output is as much "the reference's own build produced this" as an object file is,
    /// and in 203 stored trees it is the ONLY build product present. gcov names its files after the
    /// object rather than the source, so the extension list alone can miss them; both magics match.
    #[test]
    fn gcov_output_from_building_the_reference_is_not_an_oracle_modification() {
        for (name, magic) in [
            ("src/lib.gcda", &b"adcg\x42\x30\x37\x2a"[..]),
            ("src/lib-cov-2", b"oncg\x42\x30\x37\x2a"),
        ] {
            let (work, c_before, digest_before) = seeded_oracle();
            std::fs::write(work.c().0.join(name), magic).unwrap();
            assert_ne!(
                digest_before,
                work.c().digest().unwrap(),
                "fixture assumption for {name}: the digest the old check compared did move"
            );
            assert_eq!(
                is_build_product(&rel(name), b"whatever text"),
                name.ends_with(".gcda"),
                "{name}: the second case must be carried by the magic, not by its name"
            );
            seal_as_completed(work, &c_before).expect(name);
        }
    }

    /// A deletion was only ever implied by a digest difference, which names no file and no kind.
    #[test]
    fn a_deleted_oracle_source_is_refused_and_says_so() {
        let (work, c_before, _) = seeded_oracle();
        std::fs::remove_file(work.c().0.join("include/a.h")).unwrap();
        let err = seal_as_completed(work, &c_before).expect_err("a deleted reference must refuse");
        assert_eq!(
            crate::refusal::Refusal::in_chain(&err),
            Some(&crate::refusal::Refusal::OracleModified {
                change: crate::refusal::OracleChange::Removed,
                file: "include/a.h".into(),
            }),
            "{err:#}"
        );
        assert!(format!("{err:#}").contains("was removed"), "{err:#}");
    }

    /// Which additions under `c_src` are the build's own and which are a changed reference. Refusing is
    /// the shadowing hazard: a new `.h` pre-empts an include, a new `.c` is swept up by a glob. Accepting
    /// is why no name list is needed, and cost a verify ~3h of paid work when it refused
    /// (`docs/prs/spec-25.md`). Accepted rows compare digests, because `Sealed` exposes no path.
    #[test]
    fn an_addition_is_the_builds_own_only_where_the_reference_was_never_present() {
        let sealed = |work: WorkTree<Translate>, c: &Oracle| -> Result<TreeDigest> {
            work.scrub()?
                .seal(&crate::domain::health::Completed::for_test(), c)
                .map(|s| s.digest().clone())
        };
        let (clean, reference, _) = seeded_oracle();
        let untouched = sealed(clean, &reference).expect("the fixture itself seals");

        for (added, refused) in [
            // The reference occupies `.`, `src/`, `include/` and `doc/`; the build made the rest.
            ("evil.c", true),
            ("src/extra.c", true),
            ("include/shim.h", true),
            ("doc/note.bak", true),
            ("build-blake-robust-128f/build.log", false),
            ("obj/notes.txt", false),
        ] {
            let (work, reference, _) = seeded_oracle();
            tree(
                &work.c().0,
                &[(added, "the build's own, or a changed reference")],
            );
            match sealed(work, &reference) {
                Err(err) => {
                    assert!(
                        refused,
                        "{added} is the build's own, not tampering: {err:#}"
                    );
                    assert_eq!(
                        crate::refusal::Refusal::in_chain(&err),
                        Some(&crate::refusal::Refusal::OracleModified {
                            change: crate::refusal::OracleChange::Added,
                            file: added.into(),
                        }),
                        "{added}: {err:#}"
                    );
                }
                Ok(digest) => {
                    assert!(
                        !refused,
                        "{added} changes what gets built, so it must refuse"
                    );
                    assert_eq!(digest, untouched, "{added} must be dropped, not sealed in");
                }
            }
        }
    }

    /// `is_cmake_build_dir` is an OR, and either leg blinds the walk that hashes and publishes:
    /// `digest_tree` and `copy_carrying` sniff every directory as they descend into `c_src`, so a
    /// recorded file the guard just stat'd can be absent from the artifact it seals. A stored case
    /// does this: `B02_synthetic/tu_linkage` holds both legs at the oracle root, and of the 13 files
    /// the guard records there — 9 corpus reference source, the rest cmake's own — none is carried.
    #[test]
    fn a_run_that_makes_the_reference_read_as_build_output_is_refused() {
        for (left_behind, hidden) in [
            ("CMakeFiles/TargetDirectories.txt", "doc/footer.html.bak"),
            ("CMakeCache.txt", "doc/footer.html.bak"),
            ("src/CMakeFiles/dep.make", "src/lib.c"),
            ("src/CMakeCache.txt", "src/lib.c"),
        ] {
            let (work, c_before, _) = seeded_oracle();
            tree(&work.c().0, &[(left_behind, "cmake internals")]);
            assert!(
                work.c().0.join(hidden).is_file(),
                "{left_behind}: the reference is still on disk, so no stat can catch this"
            );
            let out = crate::io::workdir::test_tempdir().unwrap();
            let published = out.path().join("published");
            copy_carrying(&work.crate_dir(), &published, Carry::FromArtifact).unwrap();
            assert!(
                !published.join(C_ORACLE_DIR).join(hidden).exists(),
                "fixture assumption for {left_behind}: the artifact really does lose it"
            );

            let err = seal_as_completed(work, &c_before).expect_err(left_behind);
            assert_eq!(
                crate::refusal::Refusal::in_chain(&err),
                Some(&crate::refusal::Refusal::OracleModified {
                    change: crate::refusal::OracleChange::Hidden,
                    file: hidden.into(),
                }),
                "{left_behind}: {err:#}"
            );
        }
    }

    /// Nothing can scrub a build product — `scrub` reads UTF-8 and skips a binary silently — so one
    /// inside the digest makes two runs whose Rust is byte-identical seal differently, and the
    /// verify entry that digest keys never hits. Measured: 22 of 201 stored `.o` files hold one.
    #[test]
    fn a_build_product_the_scrub_cannot_reach_reaches_neither_digest_nor_store() {
        let seal_after_building = |built: bool| -> Sealed<Translate> {
            let (work, c_before, _) = seeded_oracle();
            if built {
                let mut object = b"\x7fELF\x02\x01\x01\x00\xff".to_vec();
                object.extend_from_slice(work.path().as_os_str().as_encoded_bytes());
                object.extend_from_slice(b"/c_src/src/lib.gcda");
                std::fs::write(work.c().0.join("src/lib.o"), object).unwrap();
            }
            work.scrub()
                .unwrap()
                .seal(&crate::domain::health::Completed::for_test(), &c_before)
                .unwrap()
        };
        let one = seal_after_building(true);
        assert_eq!(
            seal_after_building(true).digest(),
            one.digest(),
            "each run bakes its own scratch path into the object file, so two runs that \
             both built the reference must still seal alike"
        );
        assert_eq!(
            seal_after_building(false).digest(),
            one.digest(),
            "and building the reference must not change the artifact's identity at all"
        );

        let out = crate::io::workdir::test_tempdir().unwrap();
        let stored = out.path().join("code");
        one.export_into(&stored).unwrap();
        assert!(
            stored.join("c_src/src/lib.c").is_file(),
            "the reference itself must still be stored"
        );
        assert!(
            !stored.join("c_src/src/lib.o").exists(),
            "the store must not receive a file holding this host's scratch path"
        );
    }

    /// The harness, not the sandboxed agent, unlinks what building the reference left, and the walk
    /// naming those files descends on `is_dir()` — which follows. So a directory link the agent
    /// plants is a delete of somebody else's file. Four shapes: inside the oracle, the oracle root
    /// replaced by one (no component for the walk to reach), and two whose subtree emits NOTHING —
    /// an empty target and one holding only `build/` — where a guard at emission never runs at all.
    #[test]
    fn a_symlink_inside_the_oracle_is_refused_rather_than_followed() {
        type Plant = fn(&Path, &Path);
        let below_the_root: Plant = |oracle, elsewhere| {
            std::os::unix::fs::symlink(elsewhere, oracle.join("out")).unwrap();
        };
        let at_the_root: Plant = |oracle, elsewhere| {
            copy_carrying(oracle, elsewhere, Carry::IntoWorkTree).unwrap();
            std::fs::remove_dir_all(oracle).unwrap();
            std::os::unix::fs::symlink(elsewhere, oracle).unwrap();
        };

        let theirs = &[("keep.o", "somebody else's file")][..];
        let unadmitted = &[("build/keep.o", "in a BUILD_DIR, so never emitted")][..];
        for (plant, named, behind) in [
            (below_the_root, "out", theirs),
            (at_the_root, C_ORACLE_DIR, theirs),
            (below_the_root, "out", unadmitted),
            (below_the_root, "out", &[][..]),
        ] {
            let outside = crate::io::workdir::test_tempdir().unwrap();
            let elsewhere = outside.path().join("not-ours");
            std::fs::create_dir_all(&elsewhere).unwrap();
            tree(&elsewhere, behind);

            let (work, c_before, _) = seeded_oracle();
            assert!(
                !elsewhere.starts_with(work.path()),
                "fixture assumption: the link target is outside the work tree"
            );
            plant(&work.c().0, &elsewhere);

            let outcome = seal_as_completed(work, &c_before);
            assert!(
                elsewhere.is_dir() && behind.iter().all(|(f, _)| elsewhere.join(f).is_file()),
                "{named}: the harness reached through the link into {}",
                elsewhere.display()
            );
            let err = outcome.expect_err(named);
            assert_eq!(
                crate::refusal::Refusal::in_chain(&err),
                Some(&crate::refusal::Refusal::OracleModified {
                    change: crate::refusal::OracleChange::Symlinked,
                    file: named.into(),
                }),
                "{named}: {err:#}"
            );
        }
    }

    /// `judge` compares recorded files, so an empty record reports nothing and seals whatever
    /// it was handed. No such value exists to seal with: finding no file refuses instead.
    #[test]
    fn an_empty_oracle_snapshot_cannot_seal() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let src = tmp.path().join("corpus");
        std::fs::create_dir_all(&src).unwrap();
        let work: WorkTree<Translate> = Corpus::adopt(&src)
            .unwrap()
            .materialise_into(Scratch::new("t-").unwrap())
            .unwrap();
        let c = work.c();
        assert!(
            c.0.is_dir() && c.contents().unwrap().is_empty(),
            "fixture assumption: the oracle dir exists and the walk sees nothing in it"
        );

        let err = c
            .snapshot()
            .expect_err("an oracle with no files must refuse");
        assert_eq!(
            crate::refusal::Refusal::in_chain(&err),
            Some(&crate::refusal::Refusal::OracleEmpty {
                at: c.0.display().to_string(),
            }),
            "{err:#}"
        );

        tree(&src, &[("src/lib.c", "int a(void){return 0;}")]);
        let work: WorkTree<Translate> = Corpus::adopt(&src)
            .unwrap()
            .materialise_into(Scratch::new("t-").unwrap())
            .unwrap();
        tree(&work.crate_dir(), &[("Cargo.toml", "[package]")]);
        let c_before = work.c().snapshot().unwrap();
        seal_as_completed(work, &c_before)
            .expect("non-vacuity: the same corpus seals once its oracle has one file in it");
    }

    /// The other state an empty record stood for: 678 of 6,990 stored translations hold no
    /// `c_src` at all, so they must still seal — and the rule left is that they invent none.
    #[test]
    fn a_tree_that_never_had_an_oracle_seals_but_may_not_invent_one() {
        let seal_after = |invents: bool| -> Result<()> {
            let case = crate::io::workdir::test_tempdir().unwrap();
            tree(
                &case.path().join(crate::battery::TRANSLATED),
                &[("Cargo.toml", "[package]"), ("src/lib.rs", "pub fn a() {}")],
            );
            let work: WorkTree<Verify> =
                Published::<Translate>::unkeyed_from_phase_dir(case.path())
                    .unwrap()
                    .materialise_into(Scratch::new("t-").unwrap())
                    .unwrap();
            let c_before = work.c().snapshot().unwrap();
            assert!(
                matches!(c_before.0, Reference::Ungraded),
                "fixture assumption: a translation with no c_src is handed no reference"
            );
            if invents {
                tree(&work.c().0, &[("src/mine.c", "int a(void){return 1;}")]);
            }
            work.scrub()?
                .seal(&crate::domain::health::Completed::for_test(), &c_before)
                .map(|_| ())
        };
        seal_after(false).expect("a translation that never had a reference must still seal");
        let err = seal_after(true).expect_err("inventing the reference must refuse");
        assert_eq!(
            crate::refusal::Refusal::in_chain(&err),
            Some(&crate::refusal::Refusal::OracleModified {
                change: crate::refusal::OracleChange::Added,
                file: "src/mine.c".into(),
            }),
            "{err:#}"
        );
    }

    /// Every file under `root`, admitting everything, so the comparison below cannot pass by
    /// both sides sharing an exclusion the walk also applies.
    fn file_set(root: &Path) -> std::collections::BTreeSet<RelPath> {
        let mut out = std::collections::BTreeSet::new();
        visit(root, root, false, &|_| true, &mut |rel, _| {
            out.insert(rel.clone());
            Ok(())
        })
        .unwrap();
        out
    }

    /// A FIFO, a link whose target is gone, and a link to a file. `mkfifo`, because no std call
    /// makes one: a `.pipe` is what `impcheck` leaves in a work tree, and it is the entry an
    /// unguarded walk hangs on. `tmp/symlink -> ./tmp/file` is the shape of every one of the 17
    /// links in the shipped `results/` tree, all of which dangle.
    fn non_regular_entries(root: &Path) {
        use std::os::unix::fs::FileTypeExt;
        let pipe = root.join("c_src/tests/x.pipe");
        std::fs::create_dir_all(pipe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&pipe)
                .status()
                .unwrap()
                .success()
                && std::fs::symlink_metadata(&pipe)
                    .unwrap()
                    .file_type()
                    .is_fifo(),
            "the fixture must hold a real FIFO, or nothing here is being guarded against"
        );
        std::os::unix::fs::symlink("./tmp/file", root.join("tmp/symlink")).unwrap();
        std::os::unix::fs::symlink("lib.rs", root.join("src/linked.rs")).unwrap();
        assert!(
            std::fs::metadata(root.join("tmp/symlink")).is_err()
                && std::fs::metadata(root.join("src/linked.rs"))
                    .unwrap()
                    .is_file(),
            "and one link that resolves to a file beside one that resolves to nothing"
        );
    }

    /// What `translated/` CONTAINS may not depend on which backend produced it: the keyed path
    /// publishes through [`Sealed::publish`] and the unkeyed ones through [`publish_unsealed`],
    /// and while those were two hand-rolled copies — `copy_dir_all` carrying `target/`, a
    /// `skip = ["target"]` filter still carrying `c_src/build/` — comparing two backends
    /// compared two file sets.
    ///
    /// Also that BOTH publishes return at all: the copy helpers the unkeyed sites gave up skipped
    /// non-regular files because `std::fs::copy` on a FIFO blocks until a writer appears, and
    /// [`digest_tree`]'s `std::fs::read` blocks the keyed side identically — so this fixture holds
    /// one, and a walk that emits it never reaches an assertion.
    #[test]
    fn both_publish_paths_share_one_copy_body_and_one_exclusion_set() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let produced = tmp.path().join("produced");
        tree(
            &produced,
            &[
                ("Cargo.toml", "[package]"),
                ("src/lib.rs", "pub fn a() {}"),
                (".cargo/config.toml", "[build]"),
                ("c_src/src/lib.c", "int a(void){return 0;}"),
                ("c_src/doc/footer.html.bak", "upstream"),
                ("target/debug/junk", "build output"),
                ("c_src/build/CMakeCache.txt", "/dead/scratch/path"),
            ],
        );
        non_regular_entries(&produced);

        // Both FINISHED here: the file sets below catch a difference the digest cannot see.
        type Publish = fn(&Path, &Path) -> Result<Published<Translate>>;
        let mut shapes = Vec::new();
        let mut digests = Vec::new();
        let sealed: Publish = |from, case| {
            Sealed::<Translate>::from_cache(from)?
                .publish(case)?
                .finish()
        };
        let copied: Publish = |from, case| publish_unsealed::<Translate>(from, case)?.finish();
        let mut keyings = Vec::new();
        for (what, publish) in [("sealed", sealed), ("copied", copied)] {
            let case = tmp.path().join(what);
            let published = publish(&produced, &case).unwrap();
            keyings.push(published.keying());
            digests.push(published.digest().clone());
            let dst = crate::battery::phase_dir(&case, crate::battery::TRANSLATED);
            for admitted in [
                "Cargo.toml",
                "src/lib.rs",
                ".cargo/config.toml",
                "c_src/src/lib.c",
                "c_src/doc/footer.html.bak",
                // Followed, not skipped by file type: the links around phase dirs are staging
                // artifacts, and dropping them would drop content the digest counts.
                "src/linked.rs",
            ] {
                assert!(
                    dst.join(admitted).is_file(),
                    "{what}: {admitted} is hashed, so a publish that drops it publishes a \
                     different crate from the other path's"
                );
            }
            for excluded in ["target", "c_src/build"] {
                assert!(
                    !dst.join(excluded).exists(),
                    "{what}: {excluded} bakes in a dead scratch path and is 9x the bytes"
                );
            }
            for skipped in ["c_src/tests/x.pipe", "tmp/symlink"] {
                assert!(
                    std::fs::symlink_metadata(dst.join(skipped)).is_err(),
                    "{what}: {skipped} has no content to publish, and copying it either blocks \
                     forever or fails the whole publish"
                );
            }
            shapes.push(file_set(&dst));
        }

        assert!(
            produced.join("target/debug/junk").is_file()
                && produced.join("c_src/build/CMakeCache.txt").is_file(),
            "fixture assumption: the tree really holds what both publishes must drop"
        );
        assert_eq!(
            shapes[0], shapes[1],
            "the two publishes must agree file for file, or `translated/` means one thing for \
             a keyed backend and another for an unkeyed one"
        );
        assert_eq!(
            digests[0], digests[1],
            "and they must digest alike, because that digest is what the next phase's key names"
        );
        // Every way a `Published` is minted: identical trees, and a KEYING that differs, because
        // reporting the unkeyed two as covered claims a guarantee nothing has.
        assert_eq!(
            (keyings[0], keyings[1]),
            (Keying::Keyed, Keying::Unkeyable),
            "only a `Sealed`, which needs a `Completed`, is what the store can key"
        );
        assert_eq!(
            Published::<Translate>::unkeyed_from_phase_dir(&tmp.path().join("sealed"))
                .unwrap()
                .keying(),
            Keying::Unkeyable,
            "and a tree re-adopted from its phase dir is unkeyable however it was first published: \
             the adoption asks no key, so nothing there names the model, prompt or toolchain"
        );
    }

    /// The pcre2 shape, and what verify leaves when its crate fails `cargo check`: nothing published,
    /// the crate displaced, `verified/` a transcript alone — scored as a build failure, not absent.
    #[test]
    fn a_phase_dir_holding_only_a_transcript_is_not_an_artifact() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("pcre2");
        let verified = crate::battery::phase_dir(&case, crate::battery::VERIFIED);
        std::fs::create_dir_all(verified.join("logs")).unwrap();
        std::fs::write(phase_log::<Verify>(&case), "the run that published nothing").unwrap();
        std::fs::write(verified.join(Verify::METRICS), "{}").unwrap();

        let err = Published::<Verify>::unkeyed_from_phase_dir(&case)
            .expect_err("a transcript is not a verification");
        let text = format!("{err:#}");
        assert!(
            text.contains("no verified crate"),
            "naming what is missing: {text}"
        );

        std::fs::write(verified.join("Cargo.toml"), "[package]\n").unwrap();
        Published::<Verify>::unkeyed_from_phase_dir(&case)
            .expect("a real crate in the same dir is still adopted, or this pins nothing");
    }

    /// A symlink CYCLE is the input that separates "skip what cannot be resolved" from "swallow
    /// every resolution error". A dangling link has nothing to follow and is skipped; a cycle
    /// returns ELOOP, and swallowing it would drop the entry from BOTH the copy and the digest --
    /// so the two would agree, the store would validate a truncated tree, and nothing could report
    /// it. It must refuse instead.
    #[test]
    fn a_symlink_cycle_is_refused_rather_than_silently_dropped() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let root = tmp.path().join("cycle");
        tree(&root, &[("Cargo.toml", "[package]")]);
        std::os::unix::fs::symlink("b.rs", root.join("src/a.rs")).ok();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::os::unix::fs::symlink("b.rs", root.join("src/a.rs")).unwrap();
        std::os::unix::fs::symlink("a.rs", root.join("src/b.rs")).unwrap();
        assert!(
            std::fs::metadata(root.join("src/a.rs")).is_err(),
            "the fixture must really be an unresolvable cycle, not a dangling link"
        );

        let err = digest_tree(&root).expect_err("a cycle must refuse, not hash a tree without it");
        let text = format!("{err:#}");
        assert!(
            text.contains("resolving") || text.contains("a.rs") || text.contains("b.rs"),
            "and must name what it could not resolve: {text}"
        );
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
