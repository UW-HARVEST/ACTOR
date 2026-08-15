//! Typed phase artifacts: what an agent produced, and what may be done with it.
//!
//! Three invariants are enforced by the compiler, not by convention:
//! * Nothing runs in a published artifact: `Command::current_dir` and `--target-dir`
//!   both take `impl AsRef<Path>`, so "can obtain a path" *is* "can execute here", and
//!   [`Sealed`] yields no path in any form. (`test.rs` still builds inside the tree it
//!   scores; fixing that needs the `c/`+`rust/` layout split.)
//! * An infra-failed run cannot be sealed: [`Scrubbed::seal`] demands a
//!   [`crate::agent_health::Completed`], which only `classify_log` can mint.
//! * A tree cannot be hashed before it is scrubbed: agent output embeds the random
//!   scratch directory name, so a digest of raw output changes every run.

use crate::agent_health::Completed;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

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

/// Relative, non-empty, no `..`: cannot escape the tree it indexes.
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

/// What a copy carries, named by **purpose** rather than by exclusion list: a caller
/// names the purpose and cannot name the exclusions, so the list used to write a cache
/// entry and the one used to overlay a results tree cannot drift apart.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Carry {
    /// Into an agent's work tree. `logs/` travels, because that is what the verify agent
    /// has always been able to see and narrowing it would silently change the experiment.
    /// Build output does not, unlike the earlier top-level-name filter: cmake refuses a
    /// cache whose `CMAKE_CACHEFILE_DIR` no longer matches, and a nested
    /// `c_src/build/CMakeCache.txt` naming a dead scratch dir could only break a build
    /// the agent attempted there.
    IntoWorkTree,
    /// Out of a sealed artifact — into the cache store, and out of the store into the
    /// results tree. ONE variant for both, deliberately: a replay re-assembles from the
    /// stored copy, so anything the store dropped becomes a hit/miss difference. It must
    /// exclude nothing [`classify`] hashes, or a stored copy cannot re-derive the digest
    /// recorded beside it and every cache read fails validation — a cache that looks
    /// enabled and never hits. Re-carrying `c_src` over the copy seeded from the previous
    /// phase is therefore a no-op: [`Scrubbed::seal`] refuses an artifact whose C oracle
    /// differs from the one the agent was given.
    FromArtifact,
    /// Seeding a tree from the preceding phase. `logs/` stays behind: it is harness
    /// output, and the current phase's own log is being written live.
    FromPreviousPhase,
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
    set_read_only(dest, false)
}

/// Types stop *this crate* from executing in a stored artifact; `0o555`/`0o444` also
/// binds what the types cannot see — a shell-out, a stray `cargo build --manifest-path` —
/// which then fails with `EACCES` instead of quietly filling the store with `target/`
/// dirs and mutating the artifact it was reading.
pub(crate) fn set_read_only(root: &Path, ro: bool) -> Result<()> {
    fn perms(mode: u32) -> std::fs::Permissions {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(mode)
    }
    fn walk(p: &Path, ro: bool) -> Result<()> {
        let meta = std::fs::symlink_metadata(p)?;
        if meta.is_dir() {
            // A `0o555` directory cannot have entries added or removed, so unlocking on
            // the way down and locking on the way out is what makes this reversible.
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
            // chmod follows symlinks: locking one would lock a target outside this tree.
            std::fs::set_permissions(p, perms(if ro { 0o444 } else { 0o644 }))?;
        }
        Ok(())
    }
    walk(root, ro).with_context(|| {
        format!("making {} {}writable", root.display(), if ro { "non-" } else { "" })
    })
}

/// What a file contributes to. The agent's build output is legitimately its work, but it
/// is regenerable, 9x the bytes (4,536 MB vs 500 MB over `results/`), and where per-run
/// paths get baked in — so it is neither carried nor hashed.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Disposition {
    StoreAndHash,
    BuildOutput,
    Ignore,
}

const BUILD_DIRS: &[&str] = &[
    "target", "build", "c_build", "build_c", "artifacts", "gtest_build", "CMakeFiles", "e2e_out",
    "build_ffi", "fuzz_scripts",
];

/// Harness bookkeeping. Matched only at the artifact ROOT: deeper down the same name
/// is the translated program's own file, and must be inside its digest.
const ROOT_ONLY_IGNORED: &[&str] = &[
    "result.json", "verification.json", "translation.json",
    "harvest_bench_report.json", "harvest_batch_report.json",
];

/// Likewise root-anchored: `src/logs/` is source, and matching `logs` at any depth hid
/// it from the digest entirely.
const ROOT_ONLY_IGNORED_DIRS: &[&str] = &["logs", ".claude"];

/// The C oracle. Nothing under it is ever [`Disposition::Ignore`]: [`Scrubbed::seal`]
/// grades a run by comparing this subtree's digest before and after, so a rule firing
/// inside it hides a change to the reference. 26 real `c_src/doc/footer.html.bak` files
/// sat in that blind spot.
const C_ORACLE_DIR: &str = "c_src";

/// `in_build_dir` must be true if any ancestor within the tree was itself classified
/// `BuildOutput`, including by the content sniff in `visit`: the name check below misses
/// `c_src/build`, which is *nested* (so a top-level check walks past it) and which is
/// precisely the directory whose `CMakeCache.txt` records the random scratch path.
pub fn classify(rel: &RelPath, in_build_dir: bool) -> Disposition {
    let p = rel.as_path();

    if is_ignored(p) {
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

fn is_ignored(p: &Path) -> bool {
    let mut components = p.components().map(|c| c.as_os_str());
    let Some(first) = components.next() else { return false };
    if first == C_ORACLE_DIR {
        return false;
    }
    if ROOT_ONLY_IGNORED_DIRS.iter().any(|d| first == *d) {
        return true;
    }
    let at_root = components.next().is_none();
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    (at_root && ROOT_ONLY_IGNORED.contains(&name))
        || name.ends_with(".log")
        || name.ends_with(".bak")
        || name.ends_with(".sha256")
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
        feed(&mut h, rel.as_path().to_string_lossy().as_bytes());
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

/// Disk-backed, never tmpfs (see [`crate::workdir`]); removed on drop.
#[must_use]
pub struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    pub fn new(prefix: &str) -> Result<Self> {
        Ok(Self { dir: crate::workdir::tempdir(prefix)? })
    }
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
        let base = crate::workdir::base()?;
        let needles = [self.root.to_string_lossy().into_owned(), base.to_string_lossy().into_owned()];
        let mut rewritten = Vec::new();

        let artifact = self.crate_dir();
        // The same predicate the digest uses: nothing can be hashed unscrubbed.
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
        Ok(Sealed { root: self.root, _scratch: self._scratch, digest, _phase: PhantomData })
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
        anyhow::ensure!(root.is_dir(), "no {} phase dir at {}", P::DIR, root.display());
        let digest = digest_tree(&root)?;
        Ok(Self { root, _scratch: None, digest, _phase: PhantomData })
    }

    /// Re-adopt a tree the cache stored earlier. Kept `pub(crate)`: widening it would be
    /// a way to manufacture a `Sealed` without a `Completed` proof.
    pub(crate) fn from_cache(code_dir: &Path) -> Result<Self> {
        anyhow::ensure!(code_dir.is_dir(), "cache entry has no code/ at {}", code_dir.display());
        let digest = digest_tree(code_dir)?;
        Ok(Self { root: code_dir.to_path_buf(), _scratch: None, digest, _phase: PhantomData })
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
    #[must_use = "materialising and dropping the copy does nothing"]
    pub fn materialise_into<Q: Phase>(&self, scratch: Scratch) -> Result<WorkTree<Q>> {
        let root = scratch.dir.path().to_path_buf();
        copy_carrying(&self.root, &root.join(crate::battery::TRANSLATED_RUST), Carry::IntoWorkTree)?;
        Ok(WorkTree { root, _scratch: Some(scratch), _phase: PhantomData })
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
        let src = tempfile::tempdir().unwrap();
        tree(
            src.path(),
            &[
                ("Cargo.toml", "[package]"),
                ("src/lib.rs", "pub fn a() {}"),
                (".cargo/config.toml", "[build]"),      // a real build input
                ("c_src/src/lib.c", "int a(void){0;}"), // the oracle: hashed, so carried
                ("c_src/doc/footer.html.bak", "upstream"), // hashed too: it is oracle source
                ("build.rs", "fn main() {}"),
                ("logs/verify.log", "transcript"),      // Ignore: need not survive
                ("target/debug/junk", "build output"),  // BuildOutput: must not survive
            ],
        );
        let dest = tempfile::tempdir().unwrap();
        let out = dest.path().join("code");
        copy_carrying(src.path(), &out, Carry::FromArtifact).unwrap();

        for hashed in ["Cargo.toml", "src/lib.rs", ".cargo/config.toml", "c_src/src/lib.c",
                       "c_src/doc/footer.html.bak", "build.rs"] {
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

    /// The harness writes its bookkeeping at the phase-dir root. The same NAME deeper in
    /// the tree is the translated program's own file and must be inside its digest.
    #[test]
    fn harness_bookkeeping_is_ignored_only_at_the_artifact_root() {
        for name in ROOT_ONLY_IGNORED {
            assert_eq!(classify(&rel(name), false), Disposition::Ignore, "{name} at the root");
            let nested = format!("src/data/{name}");
            assert_eq!(
                classify(&rel(&nested), false),
                Disposition::StoreAndHash,
                "{nested} belongs to the translation, not the harness"
            );
        }
        assert_eq!(classify(&rel("src/logs/mod.rs"), false), Disposition::StoreAndHash,
            "a source module named logs/ is source");
    }

    /// `Scrubbed::seal` grades a run on the c_src digest before vs after. Anything
    /// excluded there is a change to the reference that nothing detects — and
    /// `c_src/doc/footer.html.bak` is a real upstream file in 26 stored cases.
    #[test]
    fn nothing_under_the_c_oracle_is_ignored() {
        for p in [
            "c_src/doc/footer.html.bak",
            "c_src/tests/expected.log",
            "c_src/lib.c.sha256",
            "c_src/logs/note.txt",
            "c_src/result.json",
        ] {
            assert_eq!(classify(&rel(p), false), Disposition::StoreAndHash, "{p}");
        }
        // Build output under the oracle stays build output: its CMakeCache.txt names a
        // dead scratch dir, which is why it is neither carried nor hashed.
        assert_eq!(classify(&rel("c_src/build/CMakeCache.txt"), false), Disposition::BuildOutput);
    }

    /// The oracle guard walks `c_src` as its OWN root, where the root-anchored rules in
    /// `classify` cannot see the `c_src` prefix. Both walks must cover the same files or
    /// the guard is blind to exactly what the artifact digest records.
    #[test]
    fn the_oracle_guard_sees_a_bak_file_change() {
        let tmp = tempfile::tempdir().unwrap();
        let c = tmp.path().join("c_src");
        tree(&c, &[("src/lib.c", "int a(void){return 0;}"), ("doc/footer.html.bak", "upstream")]);

        let before = CDir(c.clone()).digest().unwrap();
        std::fs::write(c.join("doc/footer.html.bak"), "the agent edited the oracle").unwrap();
        assert_ne!(before, CDir(c.clone()).digest().unwrap(), "a .bak edit must move the digest");

        assert_eq!(
            classify(&rel("c_src/doc/footer.html.bak"), false),
            Disposition::StoreAndHash,
            "and the artifact digest must cover the same file",
        );
    }

    #[test]
    fn classify_treats_named_build_dirs_as_build_output() {
        for p in ["target/debug/x", "cbuild/a", "gtest_build/b", "artifacts/cbuild_sub_7/c"] {
            assert_eq!(classify(&rel(p), false), Disposition::BuildOutput, "{p}");
        }
    }

    #[test]
    fn classify_catches_nested_build_dirs_a_toplevel_check_would_miss() {
        assert_eq!(classify(&rel("c_src/build/CMakeCache.txt"), false), Disposition::BuildOutput);
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
        // Path independence is what lets one phase's output key the next phase's lookup.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for r in [a.path(), b.path()] {
            tree(r, &[("src/lib.rs", "fn a() {}"), ("Cargo.toml", "[package]")]);
        }
        assert_eq!(digest_tree(a.path()).unwrap(), digest_tree(b.path()).unwrap());
    }

    /// `verify_case` spawns a real agent and cannot be unit-tested, so the plumbing
    /// between the phases is covered nowhere else.
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

        let sealed = Sealed::<Translate>::adopt(case.path()).expect("adopt");

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

        // Stand in for the agent: edit the Rust, and bake a scratch path into a note.
        let c_before = work.c().digest().unwrap();
        std::fs::write(crate_dir.join("src/lib.rs"), "pub fn a() { /* verified */ }").unwrap();
        std::fs::write(
            crate_dir.join("SYMBOLS.md"),
            format!("built in {}\n", crate_dir.display()),
        )
        .unwrap();

        let scrubbed = work.scrub().expect("scrub");
        assert!(
            scrubbed.rewritten().iter().any(|r| r.as_path().ends_with("SYMBOLS.md")),
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
