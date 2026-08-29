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
use crate::tree::{Tree, C_SRC, TRANSLATION};
use anyhow::{Context, Result};
use std::path::Path;

/// What the scorer's own discovery requires of a case directory: `runtests`' `discovery/rust.py`
/// tests `(p / "translated_rust").exists() and (p / "test_vectors").exists()` and reads nothing else.
pub const SCORED_CRATE_DIR: &str = "translated_rust";
pub const VECTORS_DIR: &str = "test_vectors";

/// What the ORACLE expects to find, which is a different question from which prompt the case gets.
///
/// `Shape` answers the prompt question and must keep `Shared`; this answers the artifact question.
/// Collapsing the two is what stripped `[[bin]] name = "driver"` from every shared-source group's
/// crate: `runtests` picks lib-vs-exec from the case-dir's `_lib` suffix alone, so one group's crate
/// is graded by BOTH runners and 48 of P01's 128 cases plus all 3 macrodepth cases died on a missing
/// `driver`.
pub enum Artifact {
    /// One cdylib, named by the corpus runner rather than by the agent.
    Cdylib { lib_name: String },
    /// The oracle runs `target/release/driver`.
    Driver,
    /// A shared-source group's real case: the template every follower is derived from, so it carries
    /// BOTH targets and keeps the agent's own `crate-type`. Ported from the pre-rewrite
    /// `translate_one_shared`, which deliberately did not reshape it.
    Template {
        default_features: Vec<String>,
        needs_driver: bool,
    },
}

/// Normalise a published crate so the scorer can build it.
///
/// Deterministic from the case: the lib name comes from the corpus runner, not the agent, and
/// `[workspace]` stops cargo absorbing the crate into a parent. Idempotent.
pub fn post_process(crate_dir: &Path, artifact: &Artifact) -> Result<()> {
    let cargo_path = crate_dir.join("Cargo.toml");
    if !cargo_path.exists() {
        return Ok(());
    }
    let mut cargo = CargoToml::open(&cargo_path)?;
    cargo.add_workspace();
    match artifact {
        Artifact::Cdylib { lib_name } => {
            cargo.remove_bin();
            cargo.set_lib(lib_name);
            cargo.save()?;
            cargo_toml::strip_for_lib(crate_dir)?;
        }
        Artifact::Driver => {
            cargo.set_bin_driver();
            cargo.save()?;
        }
        Artifact::Template {
            default_features,
            needs_driver,
        } => {
            // The real case's OWN configuration. Without it the crate is published under whichever
            // features the agent defaulted to and graded against another configuration's vectors --
            // live, P01's real case carried `default = ["haraka","robust","128s"]` against
            // blake/simple/128f test vectors.
            let resolved = crate::battery::resolve_features(&cargo_path, default_features)?;
            if !resolved.is_empty() {
                cargo.set_default_features(&resolved);
            }
            // Never `remove_bin`/`set_lib`/`strip_for_lib` here: not renaming the lib is what keeps
            // the agent's `crate-type = ["cdylib", "rlib"]`, and the bin needs the rlib to link.
            if *needs_driver {
                cargo.set_bin_driver();
            }
            cargo.save()?;
        }
    }
    Ok(())
}

/// Derive a shared-source follower from the crate its group's real case produced: one translation,
/// rebuilt under each follower's CMake features and graded against its own vectors. Deterministic, so
/// it stays outside the cache. Without it `attests` voids the battery -- 2 followers cost B02_synthetic
/// all three tools, and P01_sphincs_plus is 127 of 128 cases.
pub fn propagate_config(
    real_dir: &Path,
    follower_dir: &Path,
    cfg: &crate::battery::Config,
) -> Result<()> {
    anyhow::ensure!(
        real_dir != follower_dir,
        "refusing to derive {} onto itself",
        real_dir.display()
    );
    anyhow::ensure!(
        real_dir.join("Cargo.toml").is_file(),
        "{} published no crate, so nothing can be derived from it",
        real_dir.display()
    );
    // Replace, not merge: a stale follower would keep files this translation no longer has.
    if follower_dir.exists() {
        std::fs::remove_dir_all(follower_dir)
            .with_context(|| format!("clearing {}", follower_dir.display()))?;
    }
    crate::tree::copy_plain(real_dir, follower_dir)
        .with_context(|| format!("deriving {}", follower_dir.display()))?;

    let cargo_path = follower_dir.join("Cargo.toml");
    let mut cargo = CargoToml::open(&cargo_path)?;
    // Only features the crate defines: a CMake variable the agent did not model as a cargo feature
    // would otherwise fail the build for a reason that is not the translation's fault.
    let resolved = crate::battery::resolve_features(&cargo_path, &cfg.features)?;
    if !resolved.is_empty() {
        cargo.set_default_features(&resolved);
    }
    if cfg.is_lib {
        cargo.remove_bin();
        if let Some(lib) = &cfg.lib_name {
            cargo.set_lib(lib);
        }
        cargo.save()?;
        cargo_toml::strip_for_lib(follower_dir)?;
    } else {
        cargo.save()?;
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

    /// The follower must be the SAME crate with its own features selected -- and must exist at all,
    /// which is what was missing: `attests` voided B02_synthetic over 2 followers, P01 over 127.
    /// A shared-source group's real case is the TEMPLATE its followers are derived from, and one crate
    /// is graded by both runners -- `runtests` picks lib-vs-exec from the case-dir's `_lib` suffix, not
    /// from anything in the crate. Reshaping it as a library stripped `[[bin]] name = "driver"` and
    /// `src/main.rs`, so 48 of P01's 128 cases and all 3 macrodepth cases died on a missing `driver`.
    #[test]
    fn a_shared_source_template_keeps_both_targets_and_its_own_features() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let dir = tmp.path().join("005_real/translated");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"sphincs_plus\"\nedition = \"2021\"\n\n             [lib]\nname = \"sphincs_plus\"\ncrate-type = [\"cdylib\", \"rlib\"]\n\n             [[bin]]\nname = \"driver\"\npath = \"src/main.rs\"\n\n             [features]\ndefault = [\"haraka\"]\nblake = []\nsimple = []\nharaka = []\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/t.rs"), "#[test] fn t() {}\n").unwrap();

        post_process(
            &dir,
            &Artifact::Template {
                default_features: vec!["blake".into(), "simple".into()],
                needs_driver: true,
            },
        )
        .unwrap();

        let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        let doc: toml_edit::DocumentMut = toml.parse().unwrap();
        assert!(
            doc.get("bin").is_some(),
            "the driver bin must survive: {toml}"
        );
        assert!(
            dir.join("src/main.rs").is_file(),
            "and so must its source -- strip_for_lib deletes it"
        );
        let ct: Vec<String> = doc["lib"]["crate-type"]
            .as_array()
            .expect("crate-type")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            ct.contains(&"rlib".to_string()),
            "the bin links the lib, so rlib must survive: {ct:?}"
        );
        assert_eq!(
            doc["lib"]["name"].as_str().unwrap(),
            "sphincs_plus",
            "the lib must NOT be renamed to the case dir: `005_real` is not a Rust identifier"
        );
        let default: Vec<String> = doc["features"]["default"]
            .as_array()
            .expect("default")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            default,
            vec!["blake".to_string(), "simple".to_string()],
            "the real case's OWN preset features must be selected, not the agent's default"
        );

        // Non-vacuous: the Cdylib arm really does strip all of that, which is what was happening.
        let lib_dir = tmp.path().join("as_lib/translated");
        crate::tree::copy_plain(&dir, &lib_dir).unwrap();
        post_process(
            &lib_dir,
            &Artifact::Cdylib {
                lib_name: "renamed".into(),
            },
        )
        .unwrap();
        let lt: toml_edit::DocumentMut = std::fs::read_to_string(lib_dir.join("Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(lt.get("bin").is_none() && !lib_dir.join("src/main.rs").exists());
    }

    #[test]
    fn a_follower_is_the_real_crate_rebuilt_under_its_own_features() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let real = tmp.path().join("macrodepth_add_5/translated");
        std::fs::create_dir_all(real.join("src")).unwrap();
        std::fs::write(
            real.join("Cargo.toml"),
            "[package]\nname = \"t\"\nedition = \"2021\"\n\n[features]\ndefault = []\nop_add = []\nop_mul = []\n",
        )
        .unwrap();
        std::fs::write(real.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

        let follower = tmp.path().join("macrodepth_mul_4/translated");
        let cfg = crate::battery::Config {
            name: "macrodepth_mul_4".into(),
            features: vec!["op_mul".into(), "op_bogus".into()],
            is_lib: true,
            lib_name: Some("macrodepth_mul_4".into()),
        };
        propagate_config(&real, &follower, &cfg).unwrap();

        let toml = std::fs::read_to_string(follower.join("Cargo.toml")).unwrap();
        let doc: toml_edit::DocumentMut = toml.parse().unwrap();
        let default: Vec<String> = doc["features"]["default"]
            .as_array()
            .expect("default feature list")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            default,
            vec!["op_mul".to_string()],
            "the follower's feature must be SELECTED as default, and the undefined one dropped"
        );
        assert!(
            toml.contains("macrodepth_mul_4"),
            "the lib is renamed for the oracle: {toml}"
        );
        assert_eq!(
            std::fs::read_to_string(follower.join("src/lib.rs")).unwrap(),
            "pub fn f() -> i32 { 1 }\n",
            "the follower is the SAME translation, not a re-translation"
        );

        let once = std::fs::read_to_string(follower.join("Cargo.toml")).unwrap();
        propagate_config(&real, &follower, &cfg).unwrap();
        assert_eq!(
            once,
            std::fs::read_to_string(follower.join("Cargo.toml")).unwrap()
        );

        let err = propagate_config(&real, &real, &cfg).expect_err("self-derivation must refuse");
        assert!(format!("{err:#}").contains("onto itself"));
        assert!(
            real.join("src/lib.rs").is_file(),
            "and the real crate survives the refusal"
        );
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

        let artifact = Artifact::Cdylib {
            lib_name: "pow43_lib".into(),
        };
        post_process(&crate_dir, &artifact).unwrap();
        let once = std::fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap();
        assert!(once.contains("pow43_lib"), "{once}");
        assert!(once.contains("[workspace]"), "{once}");

        let artifact = Artifact::Cdylib {
            lib_name: "pow43_lib".into(),
        };
        post_process(&crate_dir, &artifact).unwrap();
        let twice = std::fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap();
        assert_eq!(once, twice, "the transform must be idempotent");
    }
}
