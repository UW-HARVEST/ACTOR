//! The deterministic edges of the pipeline: harness transforms, never cached.
//!
//! An AGENT RUN is nondeterministic and expensive, so it is keyed and stored. A HARNESS TRANSFORM is
//! deterministic and cheap, so it is recomputed every time and never stored -- and it must stay
//! OUTSIDE the cache, or harness logic gets baked into the agent's artifact and changing it
//! invalidates runs that are still perfectly good.
//!
//! That separation is why the next step's `before` is `transform(previous after)` rather than the
//! previous `after` itself. It used to be neither: the transform ran between the cache write and the
//! digest the next phase keyed on, so an entry's `output_tree` described a tree nothing downstream
//! used -- 216 of 216 paired entries differed, and `agents/run.rs` recorded 0 of 84 matching from the
//! other direction.

use crate::analyse::cargo_toml::{self, CargoToml};
use crate::prompt::Shape;
use crate::tree::{Tree, C_SRC, TRANSLATION};
use anyhow::{Context, Result};
use std::path::Path;

/// What the scorer's own discovery requires of a case directory: `runtests`' `discovery/rust.py`
/// tests `(p / "translated_rust").exists() and (p / "test_vectors").exists()` and reads nothing else.
pub const SCORED_CRATE_DIR: &str = "translated_rust";
pub const VECTORS_DIR: &str = "test_vectors";

/// Normalise a published crate so the scorer can build it.
///
/// Deterministic from the case: the lib name comes from the corpus runner, not from the agent, and
/// the `[workspace]` stanza stops cargo absorbing the crate into a parent workspace. Idempotent, so
/// applying it twice is applying it once.
pub fn post_process(crate_dir: &Path, shape: Shape, lib_name: &str) -> Result<()> {
    let cargo_path = crate_dir.join("Cargo.toml");
    if !cargo_path.exists() {
        return Ok(());
    }
    let mut cargo = CargoToml::open(&cargo_path)?;
    cargo.add_workspace();
    match shape {
        Shape::Library | Shape::Shared => {
            cargo.remove_bin();
            cargo.set_lib(lib_name);
            cargo.save()?;
            cargo_toml::strip_for_lib(crate_dir)?;
        }
        Shape::Executable => {
            cargo.set_bin_driver();
            cargo.save()?;
        }
    }
    Ok(())
}

/// Assemble the tree the scorer grades: the translation and the corpus's vectors, and **no C**.
///
/// This is the structural fix for linking the original library. `runtests` never reads `c_src/`, so
/// leaving it out costs the measurement nothing and makes the shortcut fail to build rather than
/// something to detect: one published artifact CMake-built libsodium out of its own `c_src/`,
/// `objcopy`-renamed all 881 public symbols and jumped to them from naked asm, scoring full marks at
/// 1,013 lines against another agent's 27,044. Agents are misaligned; this is a shape, not a policy.
pub fn eval_case(dest: &Path, tree: &Tree, corpus_case: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    tree.copy_subtree_into(TRANSLATION, &dest.join(SCORED_CRATE_DIR))
        .with_context(|| format!("assembling {} for scoring", dest.display()))?;
    let vectors = corpus_case.join(VECTORS_DIR);
    anyhow::ensure!(
        vectors.is_dir(),
        "no {VECTORS_DIR}/ at {} -- the oracle is what grades the translation, so a case without \
         one cannot be scored and must not be reported as a zero",
        vectors.display()
    );
    crate::tree::copy_plain(&vectors, &dest.join(VECTORS_DIR))?;
    // The runner drives the vectors against the built crate. From the corpus, like the vectors: it is
    // the oracle's own harness and an agent never sees it.
    let runner = corpus_case.join("runner");
    if runner.is_dir() {
        crate::tree::copy_plain(&runner, &dest.join("runner"))?;
        repoint_runner(&dest.join("runner/Cargo.toml"), corpus_case)?;
    }
    debug_assert!(
        !dest.join(SCORED_CRATE_DIR).join(C_SRC).exists(),
        "the graded tree must never carry the C"
    );
    Ok(())
}

/// `cando2` is a relative path in the corpus layout, and the graded tree sits at another depth.
fn repoint_runner(manifest: &Path, corpus_case: &Path) -> Result<()> {
    if !manifest.is_file() {
        return Ok(());
    }
    let Some(root) = corpus_case
        .ancestors()
        .find(|a| a.join("tools/cando2").is_dir())
        .map(|a| a.join("tools/cando2"))
    else {
        return Ok(());
    };
    let content = std::fs::read_to_string(manifest)?;
    std::fs::write(
        manifest,
        content.replace(
            "path = \"../../../../tools/cando2\"",
            &format!("path = \"{}\"", root.display()),
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::WorkDir;

    fn corpus() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("pow43_lib");
        std::fs::create_dir_all(case.join("test_case/src")).unwrap();
        std::fs::write(case.join("test_case/src/lib.c"), "int f(void){return 1;}\n").unwrap();
        std::fs::create_dir_all(case.join(VECTORS_DIR)).unwrap();
        std::fs::write(case.join(VECTORS_DIR).join("t1.txt"), "vector\n").unwrap();
        (tmp, case)
    }

    fn translated(case: &Path) -> Tree {
        let w = WorkDir::assemble(&case.join("test_case")).unwrap();
        std::fs::create_dir_all(w.translation().join("src")).unwrap();
        std::fs::write(
            w.translation().join("Cargo.toml"),
            "[package]\nname = \"translated_rust\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            w.translation().join("src/lib.rs"),
            "pub fn f() -> i32 { 1 }\n",
        )
        .unwrap();
        w.seal().unwrap()
    }

    #[test]
    fn the_graded_tree_holds_the_translation_and_the_vectors_and_no_c() {
        // An artifact that links the original C can only do so if the C is there to link. Leaving it
        // out is what makes that unrepresentable rather than detected.
        let (_tmp, case) = corpus();
        let tree = translated(&case);
        let out = crate::io::workdir::test_tempdir().unwrap();
        let dest = out.path().join("pow43_lib");
        eval_case(&dest, &tree, &case).unwrap();

        assert!(dest.join(SCORED_CRATE_DIR).join("src/lib.rs").is_file());
        assert!(dest.join(VECTORS_DIR).join("t1.txt").is_file());
        assert!(
            !dest.join(SCORED_CRATE_DIR).join(C_SRC).exists() && !dest.join(C_SRC).exists(),
            "the C must not reach the graded tree at any depth"
        );
        // Non-vacuous: the tree it came FROM really did carry the C.
        let work = tree.materialise(&case.join("test_case")).unwrap();
        assert!(
            work.root().join(C_SRC).join("src/lib.c").is_file(),
            "otherwise this test proves nothing about exclusion"
        );
    }

    #[test]
    fn a_case_with_no_oracle_is_refused_rather_than_scored_zero() {
        let (_tmp, case) = corpus();
        std::fs::remove_dir_all(case.join(VECTORS_DIR)).unwrap();
        let tree = translated(&case);
        let out = crate::io::workdir::test_tempdir().unwrap();
        let err = eval_case(&out.path().join("c"), &tree, &case)
            .expect_err("no vectors means nothing grades it");
        assert!(format!("{err:#}").contains(VECTORS_DIR));
    }

    #[test]
    fn post_processing_names_the_lib_from_the_corpus_and_is_idempotent() {
        let (_tmp, case) = corpus();
        let tree = translated(&case);
        let work = tree.materialise(&case.join("test_case")).unwrap();
        let crate_dir = work.translation();

        post_process(&crate_dir, Shape::Library, "pow43_lib").unwrap();
        let once = std::fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap();
        assert!(once.contains("pow43_lib"), "{once}");
        assert!(once.contains("[workspace]"), "{once}");

        post_process(&crate_dir, Shape::Library, "pow43_lib").unwrap();
        let twice = std::fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap();
        assert_eq!(once, twice, "the transform must be idempotent");
    }
}
