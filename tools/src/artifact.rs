//! Typed phase artifacts: what an agent produced, and what may be done with it.
//!
//! Three invariants are enforced by the compiler here, not by convention:
//!
//! * **Nothing runs in a published artifact.** [`Sealed`] exposes no `Path` and
//!   implements none of `AsRef<Path>` / `Deref` / `Borrow<Path>` / `Display`.
//!   Since `Command::current_dir` and `--target-dir` both take `impl AsRef<Path>`,
//!   "can obtain a path" *is* "can execute here" — so there is no expression in
//!   this crate that can run a command in a sealed tree. The only exit is a copy.
//!   Today the test phase does the opposite: `test.rs` symlinks
//!   `<case>/translated_rust` at the canonical phase dir and builds into
//!   `<phase>/target`, so scoring mutates the artifact it is scoring (1,702 MB of
//!   `target/` across 18 dirs in `results/` is the evidence). Fixing that needs
//!   the `c/`+`rust/` layout split and is deliberately not in this module yet.
//!
//! * **An infra-failed run cannot be sealed.** [`Scrubbed::seal`] requires
//!   [`crate::agent_health::Completed`], whose field is private to that module, so
//!   it can only be obtained by passing a real log through `classify_log`. On
//!   2026-08-14 seven harvest-bench agents died on expired credentials and their
//!   output was scored anyway; that is now a type error.
//!
//! * **A tree cannot be hashed before it is scrubbed.** [`Scrubbed`] is the only
//!   input to a digest. Agent output embeds the random scratch directory name —
//!   `c_src/build/CMakeCache.txt` records it for 3 of 7 harvest-bench projects —
//!   so hashing raw output yields a digest that changes every run.

use crate::agent_health::Completed;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

// ── Phase ──────────────────────────────────────────────────────────────────

mod sealed_trait {
    pub trait Sealed {}
}

/// A pipeline phase. Sealed: no phase can be defined outside this module, so
/// every phase-dependent constant lives here and cannot drift apart.
pub trait Phase: sealed_trait::Sealed + Copy + 'static {
    /// Directory name under a case dir.
    const DIR: &'static str;
}

/// What translation produced, pre-verify.
#[derive(Copy, Clone)]
pub struct Translate;
/// What the verify phase produced.
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

// ── Newtypes ───────────────────────────────────────────────────────────────

/// A `sha256:<hex>` tree digest. No `From<String>`: the only way to obtain one is
/// to hash something, so a digest cannot be confused with an arbitrary string or
/// with a digest of a different kind.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TreeDigest(String);

impl TreeDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Test-only constructor. There is deliberately no `From<String>`: outside
    /// tests, the only way to obtain a `TreeDigest` is to hash a real tree.
    #[cfg(test)]
    pub(crate) fn for_test(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Debug for TreeDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short form: the first 12 hex chars are plenty to compare by eye.
        let short: String = self.0.chars().take(19).collect();
        write!(f, "{short}…")
    }
}

/// A path guaranteed relative, with no `..` and no root component, so it can
/// never escape the tree it indexes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct RelPath(PathBuf);

impl RelPath {
    pub fn new(p: impl AsRef<Path>) -> Result<Self> {
        let p = p.as_ref();
        anyhow::ensure!(p.is_relative(), "path must be relative: {}", p.display());
        anyhow::ensure!(
            !p.components().any(|c| matches!(c, std::path::Component::ParentDir)),
            "path must not contain `..`: {}",
            p.display()
        );
        anyhow::ensure!(p.as_os_str() != "", "path must not be empty");
        Ok(Self(p.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

// ── Disposition: store vs hash vs ignore ───────────────────────────────────

/// What a copy carries, named by **purpose** rather than by exclusion list.
///
/// Every copy in the artifact lifecycle used to pass its own `&[&str]` of names to
/// skip. Two of those lists have to agree — the one writing a cache entry and the
/// one overlaying an artifact into the results tree — and they were kept in
/// agreement only by a comment saying so. That is not enforcement: while writing
/// this module I gave them different lists, which would have made a replayed
/// `verified/` differ from a freshly computed one in a way no test would catch.
///
/// Now the lists live here, once, defined against each other. A caller names the
/// purpose and cannot name the exclusions, so the two cannot drift apart.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Carry {
    /// Into an agent's work tree. `logs/` travels, because that is what the verify
    /// agent has always been able to see and narrowing it would silently change the
    /// experiment.
    ///
    /// Build output does not, and that IS a change: the previous name-based filter
    /// excluded a top-level `target/` but carried nested build trees, so three
    /// harvest-bench projects handed the verify agent a `c_src/build/CMakeCache.txt`
    /// recording a `/tmp/harvest-translate-*` directory that no longer exists. cmake
    /// refuses a cache whose `CMAKE_CACHEFILE_DIR` does not match where it now sits,
    /// so those files could only ever have broken a build the agent attempted in
    /// them. Dropping them removes 3.8 MB, and removes the only files that made
    /// [`WorkTree::scrub`] load-bearing.
    IntoWorkTree,
    /// Out of a sealed artifact — into the cache store, and out of the store into
    /// the results tree. ONE variant for both, deliberately: a replay re-assembles
    /// from the stored copy, so anything the store dropped that the results tree
    /// would have kept becomes a difference between a hit and a miss.
    ///
    /// It must exclude **nothing that [`classify`] hashes**, or a stored copy cannot
    /// re-derive the digest recorded beside it and every cache read fails its
    /// integrity check — a cache that looks enabled and never hits. An earlier draft
    /// of this dropped `c_src` here, on the reasoning that the assembly re-seeds it
    /// anyway; `a_digest_survives_the_round_trip_through_the_store` rejected it, and
    /// `from_artifact_keeps_everything_the_digest_covers` now pins the rule.
    ///
    /// The consequence is that the results-tree overlay also carries `c_src` over
    /// the copy seeded from the previous phase. That is a no-op in content, not a
    /// leniency: [`Scrubbed::seal`] refuses an artifact whose C oracle differs from
    /// the one the agent was given, so the two are byte-identical by the time
    /// anything is copied.
    FromArtifact,
    /// Seeding a tree from the preceding phase, so files the agent never touched
    /// are present. `logs/` stays behind: it is harness output, and the current
    /// phase's own log is being written live.
    FromPreviousPhase,
}

impl Carry {
    /// Whether a copy of this purpose carries a file of this disposition.
    ///
    /// This is the whole policy, and it is expressed against [`Disposition`] rather
    /// than as a list of directory names — which is what makes the failure mode
    /// unrepresentable rather than merely tested. Note that `StoreAndHash` has no
    /// arm returning `false`: **no copy can drop a file the digest covers**, so
    /// "export without `c_src`" is not a bug one can write here.
    fn admits(self, d: Disposition) -> bool {
        match d {
            // The invariant. Do not add a condition to this arm: an artifact whose
            // stored copy omits a hashed file cannot re-derive its own digest, so
            // every cache read fails validation and the cache silently never hits.
            Disposition::StoreAndHash => true,
            // Regenerable, nine times the bytes, and where per-run paths get baked
            // in. Dropped from every copy, including into the work tree — see the
            // note on `IntoWorkTree`.
            Disposition::BuildOutput => false,
            // Harness bookkeeping. Travels with the artifact so a work tree and a
            // stored entry keep the transcript, but is not re-seeded from the
            // previous phase, whose logs belong to that phase.
            Disposition::Ignore => self != Carry::FromPreviousPhase,
        }
    }
}

/// Copy the files `carry` admits, deciding with [`classify`] — the same policy the
/// digest uses.
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
    // `std::fs::copy` carries the source's mode bits. Cache entries are stored
    // read-only on purpose (see `set_read_only`), so without this a replay would
    // publish a read-only crate into the results tree and every later `cargo build`
    // would fail with EACCES — the protection leaking out of the place it protects.
    // Every `Carry` variant copies into somewhere that must be usable afterwards,
    // so this belongs here, at the single funnel, rather than at three call sites
    // where one would eventually be missed.
    set_read_only(dest, false)
}

/// Make a tree read-only, or writable again.
///
/// Types stop *this crate* from executing in a stored artifact, and the cache
/// store's placement stops a tree-walker from finding one. This is the third layer,
/// and the only one that also binds what the types cannot see: a shell-out, a stray
/// `cargo build --manifest-path`, a future refactor that has not read the comments.
/// With `0o555`/`0o444` a build inside a stored entry fails with `EACCES` instead of
/// quietly filling the store with `target/` directories and mutating the very
/// artifact it was reading.
pub(crate) fn set_read_only(root: &Path, ro: bool) -> Result<()> {
    fn perms(mode: u32) -> std::fs::Permissions {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(mode)
    }
    fn walk(p: &Path, ro: bool) -> Result<()> {
        let meta = std::fs::symlink_metadata(p)?;
        if meta.is_dir() {
            // Unlock a directory before touching its children, and lock it only
            // after: a `0o555` directory cannot have entries added or removed, so
            // the order is what makes this reversible.
            if !ro {
                std::fs::set_permissions(p, perms(0o755))?;
            }
            for e in std::fs::read_dir(p)? {
                walk(&e?.path(), ro)?;
            }
            if ro {
                std::fs::set_permissions(p, perms(0o555))?;
            }
        } else if !meta.file_type().is_symlink() {
            // Symlinks skipped: chmod follows them, so locking one would lock its
            // target, which may lie outside this tree.
            std::fs::set_permissions(p, perms(if ro { 0o444 } else { 0o644 }))?;
        }
        Ok(())
    }
    walk(root, ro).with_context(|| {
        format!("making {} {}writable", root.display(), if ro { "non-" } else { "" })
    })
}

/// What a file contributes to. Storage and hashing are different questions: the
/// agent's build output is legitimately its work, but it is regenerable, it is 9x
/// the bytes (4,536 MB vs 500 MB measured over `results/`), and it is where
/// per-run paths get baked in — so it is kept out of the digest.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Disposition {
    /// Agent source output: part of the artifact and part of its identity.
    StoreAndHash,
    /// Agent build output: regenerable, never hashed.
    BuildOutput,
    /// Harness bookkeeping or transient: neither.
    Ignore,
}

/// Directory names that are always build output.
const BUILD_DIRS: &[&str] = &[
    "target", "build", "c_build", "build_c", "artifacts", "gtest_build", "CMakeFiles", "e2e_out",
    "build_ffi", "fuzz_scripts",
];

/// Classify one entry.
///
/// `in_build_dir` must be true if any ancestor within the tree was itself
/// classified `BuildOutput` — including by the content sniff below, which is what
/// makes this future-proof. A name list catches `cbuild`, `gtest_build` and
/// `artifacts/cbuild_sub_7`; only the sniff catches `c_src/build`, which is
/// *nested* (so a top-level check walks past it) and which is precisely the
/// directory whose `CMakeCache.txt` records the random scratch path.
pub fn classify(rel: &RelPath, in_build_dir: bool) -> Disposition {
    let p = rel.as_path();
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();

    // Harness output and transients, at any depth.
    let ignored_file = matches!(
        name,
        "result.json" | "verification.json" | "translation.json"
            | "harvest_bench_report.json" | "harvest_batch_report.json"
    ) || name.ends_with(".log")
        || name.ends_with(".bak")
        || name.ends_with(".sha256");
    let in_logs = p.components().any(|c| c.as_os_str() == "logs");
    let in_claude = p.components().any(|c| c.as_os_str() == ".claude");
    if ignored_file || in_logs || in_claude {
        return Disposition::Ignore;
    }

    if in_build_dir || p.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        BUILD_DIRS.contains(&s.as_ref()) || s.starts_with("cbuild")
    }) {
        return Disposition::BuildOutput;
    }

    Disposition::StoreAndHash
}

/// Does this directory look like a cmake build tree, whatever it is called?
fn is_cmake_build_dir(dir: &Path) -> bool {
    dir.join("CMakeCache.txt").is_file() || dir.join("CMakeFiles").is_dir()
}

// ── Digest ─────────────────────────────────────────────────────────────────

/// Length-prefixed feed. The upstream `harvest_core::fs::hash_dir` separates
/// fields with bare NULs, so `("a\0b", "")` and `("a", "b")` collide once content
/// is binary. Prefixing with the length is injective.
fn feed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// Deterministic digest over the `StoreAndHash` files of a tree.
///
/// Ported from `harvest_core::fs::hash_dir` (harvest-agentic `core/src/fs/mod.rs`)
/// with three changes it needed to be usable here: a classification filter, the
/// length prefixing above, and following symlinks to hash content rather than
/// hashing the link target — the links that exist around phase dirs are staging
/// artifacts whose targets are per-run paths.
fn digest_tree(root: &Path) -> Result<TreeDigest> {
    let mut files: std::collections::BTreeMap<RelPath, PathBuf> = Default::default();
    visit(root, root, false, &|d| d == Disposition::StoreAndHash, &mut |rel, abs| {
        files.insert(rel.clone(), abs.to_path_buf());
        Ok(())
    })
    .with_context(|| format!("walking {} for a digest", root.display()))?;

    let mut h = Sha256::new();
    feed(&mut h, b"harvest-tree-v1");
    for (rel, abs) in &files {
        feed(&mut h, rel.as_path().to_string_lossy().as_bytes());
        let bytes = std::fs::read(abs).with_context(|| format!("reading {}", abs.display()))?;
        feed(&mut h, &bytes);
    }
    Ok(TreeDigest(format!("sha256:{:x}", h.finalize())))
}

/// **The** traversal of an artifact tree.
///
/// Hashing and copying both go through this, so "which files are part of this
/// artifact" has exactly one answer. Before, the digest walked with [`classify`] —
/// three-way, with a content sniff that catches a cmake build tree whatever it is
/// called — while copies walked a list of top-level directory names. Two policies
/// that had to agree, and twice did not: once dropping `c_src` from stored entries
/// so no entry could validate, once carrying `logs/` into a work tree and silently
/// changing what the agent saw.
///
/// `admits` decides both which files are emitted and which directories are
/// descended into, so a directory the caller does not want is not merely filtered
/// out file by file — it is never opened.
fn visit(
    root: &Path,
    dir: &Path,
    in_build_dir: bool,
    admits: &dyn Fn(Disposition) -> bool,
    emit: &mut dyn FnMut(&RelPath, &Path) -> Result<()>,
) -> Result<()> {
    // The sniff has to happen per directory, on the way down: `c_src/build` is
    // nested, so a check against top-level names walks straight past it.
    let build_here = in_build_dir || is_cmake_build_dir(dir);
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Ok(Ok(rel)) = path.strip_prefix(root).map(RelPath::new) else { continue };

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

// ── Scratch ────────────────────────────────────────────────────────────────

/// A disposable directory on a disk-backed filesystem (never tmpfs — see
/// [`crate::workdir`]). Removed on drop.
#[must_use]
pub struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    pub fn new(prefix: &str) -> Result<Self> {
        Ok(Self { dir: crate::workdir::tempdir(prefix)? })
    }
}

// ── WorkTree: the only runnable artifact ───────────────────────────────────

/// A materialised, writable copy. The ONLY artifact type that yields a `Path`.
pub struct WorkTree<P: Phase> {
    root: PathBuf,
    _scratch: Option<Scratch>, // kept alive so the tree outlives materialisation
    _phase: PhantomData<P>,
}

impl<P: Phase> WorkTree<P> {
    /// The scratch root. The single escape hatch in this module: everything that
    /// executes needs a path, and this is the only type that yields one.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The crate the agent works in: `<scratch>/translated_rust`.
    pub fn crate_dir(&self) -> PathBuf {
        self.root.join(crate::battery::TRANSLATED_RUST)
    }

    /// Read-only view of the C oracle source.
    pub fn c(&self) -> CDir {
        CDir(self.crate_dir().join("c_src"))
    }

    /// Rewrite per-run absolute paths to a stable token, then allow hashing.
    ///
    /// Consumes `self`: a `WorkTree` handle cannot be used after scrubbing, so
    /// the agent cannot run again against a tree that has been normalised for
    /// hashing.
    pub fn scrub(self) -> Result<Scrubbed<P>> {
        let base = crate::workdir::base()?;
        let needles = [self.root.to_string_lossy().into_owned(), base.to_string_lossy().into_owned()];
        let mut rewritten = Vec::new();

        let artifact = self.crate_dir();
        // Scrub exactly the files that will be hashed — the same traversal, so a
        // file cannot be hashed without having been offered for scrubbing.
        visit(&artifact, &artifact, false, &|d| d == Disposition::StoreAndHash, &mut |rel, abs| {
            let Ok(text) = std::fs::read_to_string(abs) else { return Ok(()) }; // binary: skip
            let mut out = text.clone();
            for n in &needles {
                if !n.is_empty() {
                    out = out.replace(n.as_str(), "$HARVEST_WORKDIR");
                }
            }
            if out != text {
                std::fs::write(abs, out).with_context(|| format!("scrubbing {}", abs.display()))?;
                rewritten.push(rel.clone());
            }
            Ok(())
        })?;
        Ok(Scrubbed { root: artifact, _scratch: self._scratch, rewritten, _phase: PhantomData })
    }
}

/// Read-only view of the C oracle. No method yields a `&Path` and none writes, so
/// this crate cannot modify the oracle. The agent is a subprocess holding
/// [`WorkTree::path`] and *can*, which is why [`Scrubbed::seal`] compares this
/// digest before and after the session.
pub struct CDir(PathBuf);

impl CDir {
    pub fn digest(&self) -> Result<TreeDigest> {
        if self.0.is_dir() {
            digest_tree(&self.0)
        } else {
            Ok(TreeDigest("sha256:absent".into()))
        }
    }
}

// ── Scrubbed: hashable, not yet trusted ────────────────────────────────────

/// Output whose per-run paths have been normalised. The only input to a digest.
pub struct Scrubbed<P: Phase> {
    root: PathBuf,
    _scratch: Option<Scratch>,
    rewritten: Vec<RelPath>,
    _phase: PhantomData<P>,
}

impl<P: Phase> Scrubbed<P> {
    /// Files whose embedded scratch paths were rewritten. Normally empty; 3 files
    /// in 345 cases in the current corpus.
    pub fn rewritten(&self) -> &[RelPath] {
        &self.rewritten
    }

    /// Seal the artifact. Requires proof the agent completed, and that the C
    /// oracle is byte-identical to what it was handed.
    pub fn seal(self, _proof: &Completed, c_before: &TreeDigest) -> Result<Sealed<P>> {
        let c_after = CDir(self.root.join("c_src")).digest()?;
        anyhow::ensure!(
            &c_after == c_before,
            "the agent modified the C oracle source: {} before, {} after. \
             The C side is the reference the translation is graded against; a run that \
             changes it has not been verified against the original program.",
            c_before.as_str(),
            c_after.as_str()
        );
        let digest = digest_tree(&self.root)?;
        Ok(Sealed { root: self.root, _scratch: self._scratch, digest, _phase: PhantomData })
    }
}

// ── Sealed: immutable, un-runnable ─────────────────────────────────────────

/// A finished artifact.
///
/// Deliberately implements NONE of `AsRef<Path>`, `Deref<Target = Path>`,
/// `Borrow<Path>` or `Display`, and has no `path()`. `Debug` prints the digest
/// rather than the location so the path cannot be recovered by formatting.
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
    /// Adopt an existing phase dir as a sealed artifact. Used for `translated/`,
    /// which was produced by an earlier run.
    pub fn adopt(case_dir: &Path) -> Result<Self> {
        let root = crate::battery::phase_dir(case_dir, P::DIR);
        anyhow::ensure!(root.is_dir(), "no {} phase dir at {}", P::DIR, root.display());
        let digest = digest_tree(&root)?;
        Ok(Self { root, _scratch: None, digest, _phase: PhantomData })
    }

    /// Re-adopt a tree the cache stored earlier.
    ///
    /// `pub(crate)` and named for its one caller: this is the only constructor that
    /// does not start from a phase dir, and widening it would be a way to
    /// manufacture a `Sealed` without a `Completed` proof, defeating I3.
    pub(crate) fn from_cache(code_dir: &Path) -> Result<Self> {
        anyhow::ensure!(code_dir.is_dir(), "cache entry has no code/ at {}", code_dir.display());
        let digest = digest_tree(code_dir)?;
        Ok(Self { root: code_dir.to_path_buf(), _scratch: None, digest, _phase: PhantomData })
    }

    pub fn digest(&self) -> &TreeDigest {
        &self.digest
    }

    /// Copy the artifact's contents into `dest`, for the cache to store.
    ///
    /// Takes a destination and returns nothing, so it does not widen I1: there is
    /// still no expression that yields a path *to* a sealed artifact.
    ///
    /// Uses [`Carry::FromArtifact`], the same variant the results-tree overlay
    /// uses, so what a replay reproduces cannot differ from what a fresh run
    /// produces. Nothing here can affect *identity* — `digest_tree` filters
    /// independently, via [`classify`] — only what a replay can reconstruct.
    pub fn export_into(&self, dest: &Path) -> Result<()> {
        copy_carrying(&self.root, dest, Carry::FromArtifact)
    }

    /// The only way to obtain something runnable: a writable copy elsewhere.
    #[must_use = "materialising and dropping the copy does nothing"]
    pub fn materialise_into<Q: Phase>(&self, scratch: Scratch) -> Result<WorkTree<Q>> {
        let root = scratch.dir.path().to_path_buf();
        // Skip ONLY `target`, matching the previous IsolatedWorkDir::new exactly.
        // `translated/logs/` therefore still reaches the agent's work dir, as it
        // always has — the agent's visible input must not change in this PR.
        // (Whether verify SHOULD see the translate log is a real question, since it
        // also drags result.json in; that is a separate change with its own
        // evaluation, not a side effect of a type refactor.)
        copy_carrying(&self.root, &root.join(crate::battery::TRANSLATED_RUST), Carry::IntoWorkTree)?;
        Ok(WorkTree { root, _scratch: Some(scratch), _phase: PhantomData })
    }

    /// Copy into `results/<case>/<P::DIR>`, preserving a live `logs/` dir.
    ///
    /// Seeds from `translated/` first so files the agent did not rewrite (notably
    /// `c_src/`) are present, then overlays the agent's output — the semantics the
    /// previous `IsolatedWorkDir::finish` had, kept deliberately.
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

    /// Seed from the previous phase, then overlay this artifact.
    ///
    /// Factored out of [`Self::publish`] so the assembly exists once. It is the
    /// definition of "what this phase's tree contains", and anything that needs
    /// such a tree — publishing it, or building a throwaway copy to compile-check
    /// — must get the same answer. Two implementations would be two answers.
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

    /// `Carry::FromArtifact` may not drop anything the digest covers.
    ///
    /// Stated as a property over a fixture rather than as a comparison of the two
    /// lists, because the lists are written in different vocabularies: `classify`
    /// decides per file, `Carry` names top-level directories. Comparing them
    /// directly would only restate the code; walking a tree asks the question that
    /// actually matters — after an export, is every hashed file still there?
    ///
    /// If this ever fails, the cache does not misbehave subtly: no entry can
    /// validate, so it silently never hits.
    #[test]
    fn from_artifact_keeps_everything_the_digest_covers() {
        let src = tempfile::tempdir().unwrap();
        tree(
            src.path(),
            &[
                ("Cargo.toml", "[package]"),
                ("src/lib.rs", "pub fn a() {}"),
                (".cargo/config.toml", "[build]"),      // a real build input
                ("c_src/src/lib.c", "int a(void){0;}"), // the oracle: hashed, so carried
                ("build.rs", "fn main() {}"),
                ("logs/verify.log", "transcript"),      // Ignore: need not survive
                ("target/debug/junk", "build output"),  // BuildOutput: must not survive
            ],
        );
        let dest = tempfile::tempdir().unwrap();
        let out = dest.path().join("code");
        copy_carrying(src.path(), &out, Carry::FromArtifact).unwrap();

        for hashed in ["Cargo.toml", "src/lib.rs", ".cargo/config.toml", "c_src/src/lib.c", "build.rs"] {
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
        assert!(!out.join("target").exists(), "build output must not be stored");

        // And the whole point: the digest is the same on both sides.
        assert_eq!(
            digest_tree(src.path()).unwrap(),
            digest_tree(&out).unwrap(),
            "an exported artifact must hash identically to the artifact it came from"
        );
    }

    /// A stale cmake cache must not reach the agent.
    ///
    /// Measured in the live tree: 3 of 7 harvest-bench projects carry a
    /// `translated/c_src/build/CMakeCache.txt` whose `CMAKE_CACHEFILE_DIR` points at
    /// a `/tmp/harvest-translate-*` scratch dir that no longer exists. cmake refuses
    /// such a cache, so copying it into the work tree could only break a build the
    /// agent tried there. The old top-level name filter carried it because it is
    /// nested; deciding by `Disposition` catches it via the content sniff.
    #[test]
    fn a_stale_cmake_cache_does_not_reach_the_agent() {
        let src = tempfile::tempdir().unwrap();
        tree(
            src.path(),
            &[
                ("Cargo.toml", "[package]"),
                ("c_src/src/lib.c", "int a(void){return 0;}"),
                ("c_src/build/CMakeCache.txt", "CMAKE_CACHEFILE_DIR:INTERNAL=/tmp/harvest-translate-w1nAAq/x"),
                ("c_src/build/CMakeFiles/junk", "cmake internals"),
                ("logs/translate.log", "transcript"),
            ],
        );
        let dest = tempfile::tempdir().unwrap();
        let out = dest.path().join("work");
        copy_carrying(src.path(), &out, Carry::IntoWorkTree).unwrap();

        assert!(out.join("c_src/src/lib.c").is_file(), "the C oracle must travel");
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
    fn relpath_rejects_escapes() {
        assert!(RelPath::new("a/b").is_ok());
        assert!(RelPath::new("/abs").is_err(), "absolute must be refused");
        assert!(RelPath::new("../up").is_err(), "parent traversal must be refused");
        assert!(RelPath::new("").is_err());
    }

    #[test]
    fn feed_is_injective_where_nul_separators_are_not() {
        // The upstream hash_dir separates with bare NULs, so these two collide
        // there once content is binary. Length prefixing distinguishes them.
        let mut a = Sha256::new();
        feed(&mut a, b"a\0b");
        feed(&mut a, b"");
        let mut b = Sha256::new();
        feed(&mut b, b"a");
        feed(&mut b, b"b");
        assert_ne!(format!("{:x}", a.finalize()), format!("{:x}", b.finalize()));
    }

    #[test]
    fn classify_ignores_harness_output_and_logs() {
        assert_eq!(classify(&rel("result.json"), false), Disposition::Ignore);
        assert_eq!(classify(&rel("verification.json"), false), Disposition::Ignore);
        assert_eq!(classify(&rel("logs/verify.log"), false), Disposition::Ignore);
        assert_eq!(classify(&rel("src/x.rs.bak"), false), Disposition::Ignore);
    }

    #[test]
    fn classify_treats_named_build_dirs_as_build_output() {
        for p in ["target/debug/x", "cbuild/a", "gtest_build/b", "artifacts/cbuild_sub_7/c"] {
            assert_eq!(classify(&rel(p), false), Disposition::BuildOutput, "{p}");
        }
    }

    #[test]
    fn classify_catches_nested_build_dirs_a_toplevel_check_would_miss() {
        // c_src/build is the one that matters: `build` is a known name but it is
        // NESTED, and its CMakeCache.txt records the random scratch path.
        assert_eq!(classify(&rel("c_src/build/CMakeCache.txt"), false), Disposition::BuildOutput);
        // And the sniff covers a name nobody has invented yet, via in_build_dir.
        assert_eq!(classify(&rel("weird_name/CMakeCache.txt"), true), Disposition::BuildOutput);
    }

    #[test]
    fn classify_keeps_source_and_dotfiles() {
        assert_eq!(classify(&rel("src/lib.rs"), false), Disposition::StoreAndHash);
        assert_eq!(classify(&rel("Cargo.lock"), false), Disposition::StoreAndHash);
        // .cargo/config.toml is a real build input in 16 corpus cases.
        assert_eq!(classify(&rel(".cargo/config.toml"), false), Disposition::StoreAndHash);
        assert_eq!(classify(&rel("c_src/src/lib.c"), false), Disposition::StoreAndHash);
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
        let a = tempfile::tempdir().unwrap();
        tree(a.path(), &[("src/lib.rs", "fn a() {}"), ("logs/verify.log", "noise")]);
        let b = tempfile::tempdir().unwrap();
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
        let a = tempfile::tempdir().unwrap();
        tree(a.path(), &[("src/lib.rs", "fn a() {}")]);
        let b = tempfile::tempdir().unwrap();
        tree(b.path(), &[("src/lib.rs", "fn b() {}")]);
        assert_ne!(digest_tree(a.path()).unwrap(), digest_tree(b.path()).unwrap());
    }

    #[test]
    fn digest_is_path_independent() {
        // Two different roots, identical content: equal digests. This is what lets
        // one phase's output key the next phase's lookup.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for r in [a.path(), b.path()] {
            tree(r, &[("src/lib.rs", "fn a() {}"), ("Cargo.toml", "[package]")]);
        }
        assert_eq!(digest_tree(a.path()).unwrap(), digest_tree(b.path()).unwrap());
    }

    /// The whole lifecycle, with a fake case dir instead of an agent. This is the
    /// test that would have caught a mistake in the plumbing: the 91 unit tests
    /// cover digest/classify in isolation, and `verify_case` cannot be unit-tested
    /// because it spawns an agent, so without this the refactor was unverified.
    #[test]
    fn lifecycle_round_trips_from_translated_to_verified() {
        let case = tempfile::tempdir().unwrap();
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

        // 1. adopt translated/ as a sealed artifact
        let sealed = Sealed::<Translate>::adopt(case.path()).expect("adopt");

        // 2. materialise a writable copy
        let scratch = Scratch::new("test-work-").unwrap();
        let work: WorkTree<Verify> = sealed.materialise_into(scratch).expect("materialise");
        let crate_dir = work.crate_dir();
        assert!(crate_dir.join("src/lib.rs").is_file(), "source must be copied");
        assert!(crate_dir.join("c_src/src/lib.c").is_file(), "C oracle must be copied");
        assert!(
            crate_dir.join("logs/translation.log").is_file(),
            "logs must still reach the agent — parity with the previous behaviour"
        );
        assert!(!crate_dir.join("target").exists(), "build output must NOT be copied");

        // 3. the agent edits the Rust, and leaves its scratch path in a note
        let c_before = work.c().digest().unwrap();
        std::fs::write(crate_dir.join("src/lib.rs"), "pub fn a() { /* verified */ }").unwrap();
        std::fs::write(
            crate_dir.join("SYMBOLS.md"),
            format!("built in {}\n", crate_dir.display()),
        )
        .unwrap();

        // 4. scrub, and confirm the per-run path was caught
        let scrubbed = work.scrub().expect("scrub");
        assert!(
            scrubbed.rewritten().iter().any(|r| r.as_path().ends_with("SYMBOLS.md")),
            "the embedded scratch path must be rewritten, else the digest varies per run"
        );

        // 5. seal with proof, then publish
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
        assert!(out.join("c_src/src/lib.c").is_file(), "c_src must be seeded from translated/");
        assert!(!out.join("target").exists(), "build output must not be published");
        assert_eq!(
            std::fs::read_to_string(translated.join("src/lib.rs")).unwrap(),
            "pub fn a() {}",
            "translated/ must be left untouched — verify is pure"
        );
    }

    #[test]
    fn seal_refuses_when_the_agent_modified_the_c_oracle() {
        let case = tempfile::tempdir().unwrap();
        tree(
            &case.path().join(crate::battery::TRANSLATED),
            &[("Cargo.toml", "[package]"), ("c_src/src/lib.c", "int a(void){return 0;}")],
        );
        let sealed = Sealed::<Translate>::adopt(case.path()).unwrap();
        let work: WorkTree<Verify> = sealed.materialise_into(Scratch::new("t-").unwrap()).unwrap();
        let c_before = work.c().digest().unwrap();

        // The agent "fixes" the reference implementation to match its translation.
        std::fs::write(work.crate_dir().join("c_src/src/lib.c"), "int a(void){return 1;}").unwrap();

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
