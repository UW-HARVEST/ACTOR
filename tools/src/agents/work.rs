//! The tree an agent is given to work in, and what it takes to get its output back out.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The verify phase's working copy, expressed in terms of [`crate::artifact`].
///
/// Materialises `translated/` into disk-backed scratch, hands the agent a
/// [`crate::artifact::WorkTree`] (the only artifact type that yields a path, hence
/// the only one anything can execute in), and on `finish` requires proof that the
/// agent completed before the result may reach `verified/`.
pub struct IsolatedWorkDir {
    work: crate::artifact::WorkTree<crate::artifact::Verify>,
    /// Digest of the `translated/` artifact this was materialised from.
    input: crate::artifact::TreeDigest,
    /// Digest of the C oracle as handed to the agent, compared on `finish`: the C
    /// side is the reference being graded against, so a run that modified it has not
    /// been verified against the original program. `verify.md` contains no rule
    /// forbidding that, so this check is the only thing catching it.
    c_before: crate::artifact::TreeDigest,
}

impl IsolatedWorkDir {
    pub fn new(case_dir: &Path) -> Result<Self> {
        let translated = crate::artifact::Sealed::<crate::artifact::Translate>::adopt(case_dir)
            .context("adopting translated/ as a sealed artifact")?;
        let scratch = crate::artifact::Scratch::new("harvest-work-")?;
        let input = translated.digest().clone();
        let work = translated.materialise_into::<crate::artifact::Verify>(scratch)?;
        let c_before = work.c().digest()?;
        Ok(Self {
            work,
            input,
            c_before,
        })
    }

    pub fn translated_rust(&self) -> PathBuf {
        self.work.crate_dir()
    }

    /// Scratch root: current_dir, the sandbox policy and the agent's TMPDIR.
    pub fn root(&self) -> &Path {
        self.work.path()
    }

    /// The cache key's input component. Taken here rather than recomputed later
    /// because it must describe what the agent was actually given.
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
    ) -> Result<crate::artifact::Sealed<crate::artifact::Verify>> {
        let scrubbed = self.work.scrub()?;
        // Reported rather than silent: a file that embedded the scratch path is a
        // file whose content varied per run (3 files across 345 cases).
        for rel in scrubbed.rewritten() {
            eprintln!("  scrubbed per-run path from {}", rel.as_path().display());
        }
        scrubbed.seal(proof, &self.c_before)
    }
}
