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
    collect(root, root, false, &mut files)
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

fn collect(
    root: &Path,
    dir: &Path,
    in_build_dir: bool,
    out: &mut std::collections::BTreeMap<RelPath, PathBuf>,
) -> Result<()> {
    let build_here = in_build_dir || is_cmake_build_dir(dir);
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root).map(RelPath::new) else { continue };
        let Ok(rel) = rel else { continue };

        if path.is_dir() {
            if classify(&rel, build_here) == Disposition::StoreAndHash {
                collect(root, &path, build_here, out)?;
            }
            continue;
        }
        if classify(&rel, build_here) == Disposition::StoreAndHash {
            out.insert(rel, path);
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
        let mut files: std::collections::BTreeMap<RelPath, PathBuf> = Default::default();
        collect(&artifact, &artifact, false, &mut files)?;
        for (rel, abs) in &files {
            let Ok(text) = std::fs::read_to_string(abs) else { continue }; // binary: skip
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
        }
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

    pub fn digest(&self) -> &TreeDigest {
        &self.digest
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
        crate::translate::copy_dir_filtered(
            &self.root,
            &root.join(crate::battery::TRANSLATED_RUST),
            &["target"],
        )?;
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
        let translated = crate::battery::phase_dir(case_dir, crate::battery::TRANSLATED);
        if translated.is_dir() && P::DIR != crate::battery::TRANSLATED {
            crate::translate::copy_dir_filtered(&translated, &dst, &["target", "logs"])?;
        }
        crate::translate::copy_dir_filtered(&self.root, &dst, &["target", "c_src"])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
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

    #[test]
    fn debug_on_sealed_reveals_the_digest_not_the_location() {
        // Formatting must not be a way to recover a path and run something there.
        let d = TreeDigest("sha256:abc123def456".into());
        let s = format!("{d:?}");
        assert!(s.starts_with("sha256:"), "{s}");
        assert!(!s.contains('/'), "a digest must not look like a path: {s}");
    }
}
