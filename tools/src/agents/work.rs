//! The tree an agent is given to work in, and what it takes to get its output back out.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// One phase's working copy, expressed in terms of [`crate::artifact`].
///
/// Materialises the previous phase's artifact into disk-backed scratch, hands the agent a
/// [`crate::artifact::WorkTree`] (the only artifact type that yields a path, hence the only
/// one anything can execute in), and on `finish` requires proof that the agent completed
/// before the result may reach the phase dir.
///
/// The input digest and the oracle travel WITH the tree rather than beside it, so
/// [`crate::agents::run::run_cached`] reads the key's input component off the very tree it
/// hands the agent: a key naming another tree is unrepresentable rather than avoided.
pub struct IsolatedWorkDir<P: crate::artifact::Phase> {
    work: crate::artifact::WorkTree<P>,
    /// Digest of the artifact or corpus this was materialised from.
    input: crate::artifact::TreeDigest,
    /// WHERE that artifact or corpus is, kept so the store can record the tree the key was
    /// computed from and not only its digest. Both constructors were handed this path and threw
    /// it away, which is why an entry could never re-derive its own input.
    seed: crate::artifact::Seed,
    /// The C oracle as handed to the agent, compared file by file on `finish`: the C
    /// side is the reference being graded against, so a run that modified it has not
    /// been verified against the original program. `verify.md` contains no rule
    /// forbidding that, so this check is the only thing catching it. A file set rather than
    /// a digest because building the oracle is instructed, and only the file set tells the
    /// build's output apart from an edit to the reference.
    c_before: crate::artifact::Oracle,
}

/// Translate is the first phase, so it is seeded from the C corpus — an INPUT, and the only
/// per-case component of its key. Through [`crate::artifact::Corpus`], whose digest is rooted at
/// the corpus: the root-anchored rules would otherwise drop every `*.bak`, `*.log` and `*.sha256`
/// that IS hashed once seeded under `c_src/`, so two corpora could replay each other's work.
impl IsolatedWorkDir<crate::artifact::Translate> {
    pub fn from_corpus(corpus_dir: &Path) -> Result<Self> {
        let corpus = crate::artifact::Corpus::adopt(corpus_dir)?;
        let scratch = crate::artifact::Scratch::new("harvest-translate-")?;
        let input = corpus.digest()?;
        let seed = corpus.as_seed();
        let work = corpus.materialise_into::<crate::artifact::Translate>(scratch)?;
        let c_before = work.c().snapshot()?;
        Ok(Self {
            work,
            input,
            seed,
            c_before,
        })
    }
}

/// Seeding a verification from a sealed translation is the other transition this type
/// materialises; [`crate::artifact::SeededBy`] is what stops any other pair compiling.
impl IsolatedWorkDir<crate::artifact::Verify> {
    pub fn new(case_dir: &Path) -> Result<Self> {
        let translated = crate::artifact::Sealed::<crate::artifact::Translate>::adopt(case_dir)
            .context("adopting translated/ as a sealed artifact")?;
        let scratch = crate::artifact::Scratch::new("harvest-work-")?;
        let input = translated.digest().clone();
        let seed = translated.as_seed();
        let work = translated.materialise_into::<crate::artifact::Verify>(scratch)?;
        let c_before = work.c().snapshot()?;
        Ok(Self {
            work,
            input,
            seed,
            c_before,
        })
    }
}

impl<P: crate::artifact::Phase> IsolatedWorkDir<P> {
    pub fn translated_rust(&self) -> PathBuf {
        self.work.crate_dir()
    }

    /// Scratch root: current_dir, the sandbox policy and the agent's TMPDIR.
    pub fn root(&self) -> &Path {
        self.work.path()
    }

    /// The cache key's input component. Taken here rather than recomputed later
    /// because it must describe what the agent was actually given.
    /// The tree the key names, so [`crate::cache::Store`] can copy it into the entry.
    pub fn seed(&self) -> &crate::artifact::Seed {
        &self.seed
    }

    pub fn input_digest(&self) -> &crate::artifact::TreeDigest {
        &self.input
    }

    /// Seal the agent's output.
    ///
    /// Requires `&Completed` so an infra-failed run cannot be sealed. Returns the
    /// artifact rather than publishing it: publication happens once on the far side
    /// of the cache, so replayed and fresh results travel the same path.
    pub fn finish(
        self,
        proof: &crate::domain::health::Completed,
    ) -> Result<crate::artifact::Sealed<P>> {
        let scrubbed = self.work.scrub()?;
        // Reported rather than silent: a file that embedded the scratch path is a
        // file whose content varied per run (3 files across 345 cases).
        for rel in scrubbed.rewritten() {
            eprintln!("  scrubbed per-run path from {}", rel.as_path().display());
        }
        scrubbed.seal(proof, &self.c_before)
    }
}
