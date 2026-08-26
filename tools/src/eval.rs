//! The tree a score is taken from: created empty every run, filled only from the artifacts that run
//! resolved plus the corpus, deleted afterwards. No old file is read because none is present.
//! `translated_rust/` must be a REAL directory: `rust.py` pins the build to
//! `(case_root / "translated_rust").resolve() / "target"`, so the symlink this replaces put 666
//! `target/` dirs inside published phase dirs while both tests asserting `target/` is absent passed.

use crate::artifact::{Phase, Published, Seed};
use crate::battery::Paths;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Never `/tmp`: tmpfs here, so the scoring build would be resident RAM.
pub const EVAL_DIR: &str = ".eval";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Keep {
    Discard,
    ForPostMortem,
}

impl Keep {
    pub fn from_keep_eval_tree_flag(flag: bool) -> Self {
        if flag {
            Self::ForPostMortem
        } else {
            Self::Discard
        }
    }
}

/// What a score is taken from: the artifacts THIS RUN resolved, and nothing else. It was an enum whose
/// `Archive` variant read a phase dir with no key; measured when that went, 16 of 17 published agents
/// had no cached agent call at all, so ~95% of the numbers rested on artifacts nothing could attest.
#[derive(Copy, Clone)]
pub struct Source<'a> {
    pub translate: &'a crate::translate::Translations,
    pub verify: &'a crate::verify::Verifications,
}

pub fn artifact_root<P: Phase>(artifact: &Published<P>) -> PathBuf {
    match artifact.as_seed() {
        Seed::FromCorpus(root) | Seed::FromArtifact(root) => root,
    }
}

pub struct Tree {
    root: PathBuf,
    keep: Keep,
}

impl Tree {
    pub fn create_empty(paths: &Paths, keep: Keep) -> Result<Self> {
        let root = paths.repo_root.join(EVAL_DIR).join(paths.agent_key.dir());
        if root.exists() {
            std::fs::remove_dir_all(&root)
                .with_context(|| format!("emptying {}", root.display()))?;
        }
        std::fs::create_dir_all(&root)?;
        Ok(Self { root, keep })
    }

    pub fn scope(&self, name: &str) -> Result<Scope<'_>> {
        let root = if name.is_empty() {
            self.root.clone()
        } else {
            self.root.join(name)
        };
        std::fs::create_dir_all(&root)?;
        Ok(Scope {
            _tree: self,
            root,
            cases: Vec::new(),
        })
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        match self.keep {
            Keep::ForPostMortem => println!(
                "  evaluation tree kept for post-mortem at {}",
                self.root.display()
            ),
            Keep::Discard => {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
    }
}

pub struct Case {
    pub name: String,
    pub record_into: PathBuf,
    pub case_dir: PathBuf,
}

pub struct Scope<'t> {
    _tree: &'t Tree,
    root: PathBuf,
    cases: Vec<Case>,
}

impl Scope<'_> {
    pub fn materialise<P: Phase>(
        &mut self,
        name: &str,
        artifact: &Published<P>,
        corpus_case: &Path,
    ) -> Result<()> {
        let case = self.root.join(name);
        let from = artifact_root(artifact);
        artifact
            .as_seed()
            .export_into(&case.join(crate::battery::TRANSLATED_RUST))
            .with_context(|| format!("materialising {name} from {}", from.display()))?;

        for dir in ["test_vectors", "runner"] {
            let src = corpus_case.join(dir);
            if src.is_dir() {
                crate::translate::copy_dir_all(&src, &case.join(dir))?;
            }
        }
        self.repoint_runner_dependency(name, corpus_case)?;

        self.cases.push(Case {
            name: name.to_string(),
            record_into: from.clone(),
            case_dir: from.parent().unwrap_or(&from).to_path_buf(),
        });
        Ok(())
    }

    /// `cando2` is relative to the corpus layout and this tree sits at another depth.
    fn repoint_runner_dependency(&self, name: &str, corpus_case: &Path) -> Result<()> {
        let manifest = self.root.join(name).join("runner/Cargo.toml");
        if !manifest.is_file() {
            return Ok(());
        }
        let Some(cando2) = corpus_root(corpus_case).map(|r| r.join("tools/cando2")) else {
            return Ok(());
        };
        if !cando2.is_dir() {
            return Ok(());
        }
        let content = std::fs::read_to_string(&manifest)?;
        std::fs::write(
            &manifest,
            content.replace(
                "path = \"../../../../tools/cando2\"",
                &format!("path = \"{}\"", cando2.display()),
            ),
        )?;
        Ok(())
    }

    pub fn finish(self) -> Result<Materialised> {
        let mut members: Vec<String> = Vec::new();
        for case in &self.cases {
            if self
                .root
                .join(&case.name)
                .join("runner/Cargo.toml")
                .is_file()
            {
                members.push(format!("    \"{}/runner\"", case.name));
            }
        }
        if !members.is_empty() {
            members.sort();
            std::fs::write(
                self.root.join("Cargo.toml"),
                format!(
                    "[workspace]\nmembers = [\n{},\n]\nresolver = \"2\"\n",
                    members.join(",\n")
                ),
            )?;
        }
        Ok(Materialised {
            root: self.root,
            cases: self.cases,
        })
    }
}

pub struct Materialised {
    root: PathBuf,
    cases: Vec<Case>,
}

impl Materialised {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cases(&self) -> &[Case] {
        &self.cases
    }

    pub fn crate_root(&self, case: &str) -> PathBuf {
        self.root.join(case).join(crate::battery::TRANSLATED_RUST)
    }

    /// `_is_case_dir_rust` requires BOTH `translated_rust/` and `test_vectors/`, and a case missing
    /// either is silently NOT discovered — so the denominator is checked, not trusted.
    pub fn reconcile(&self, discovered: usize, scored: &BTreeSet<String>) -> Result<()> {
        let materialised: BTreeSet<&str> = self.cases.iter().map(|c| c.name.as_str()).collect();
        let missing: Vec<&str> = materialised
            .iter()
            .filter(|n| !scored.contains(**n))
            .copied()
            .collect();
        let extra: Vec<&str> = scored
            .iter()
            .filter(|n| !materialised.contains(n.as_str()))
            .map(String::as_str)
            .collect();
        anyhow::ensure!(
            discovered == materialised.len() && missing.is_empty() && extra.is_empty(),
            "the score covers {discovered} case(s) but {} were materialised at {}.\n  \
             materialised yet unscored: {:?}\n  scored yet not materialised: {:?}\n\
             A case the oracle did not discover is a smaller denominator nobody asked for, \
             so this is a refusal and not a warning.",
            materialised.len(),
            self.root.display(),
            missing,
            extra,
        );
        Ok(())
    }
}

fn corpus_root(corpus_case: &Path) -> Option<PathBuf> {
    Some(corpus_case.parent()?.parent()?.parent()?.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Translate;
    use std::fs;

    fn paths_at(repo_root: &Path) -> Paths {
        Paths::new(
            repo_root,
            crate::cli::Agent::Claude,
            crate::cli::Dataset::TestCorpus,
            None,
            crate::cache::Mode::Bypass,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap()
    }

    fn published(case_dir: &Path, body: &str) -> Published<Translate> {
        let dir = case_dir.join("translated");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]\n[workspace]\n").unwrap();
        fs::write(dir.join("src/main.rs"), body).unwrap();
        Published::<Translate>::unkeyed_from_phase_dir(case_dir).unwrap()
    }

    fn corpus_case(repo_root: &Path, battery: &str, case: &str) -> PathBuf {
        let dir = repo_root
            .join("test-corpus/Public-Tests")
            .join(battery)
            .join(case);
        fs::create_dir_all(dir.join("test_vectors")).unwrap();
        fs::write(dir.join("test_vectors/test1.txt"), "vector").unwrap();
        dir
    }

    #[test]
    fn the_tree_is_created_empty_and_the_crate_in_it_is_no_symlink_into_results() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("results")).unwrap();
        fs::create_dir_all(tmp.path().join("test-corpus")).unwrap();
        let paths = paths_at(tmp.path());

        let planted = tmp
            .path()
            .join(EVAL_DIR)
            .join(paths.agent_key.dir())
            .join("B01/leftover/translated_rust");
        fs::create_dir_all(&planted).unwrap();
        fs::write(planted.join("Cargo.toml"), "[package]").unwrap();
        assert!(
            planted.join("Cargo.toml").is_file(),
            "fixture must plant it"
        );

        let tree = Tree::create_empty(&paths, Keep::ForPostMortem).unwrap();
        assert!(!planted.exists(), "the leftover case must be gone");

        let case_dir = paths.case_dir("B01", "001");
        let artifact = published(&case_dir, "fn main() {}");
        let mut scope = tree.scope("B01").unwrap();
        scope
            .materialise("001", &artifact, &corpus_case(tmp.path(), "B01", "001"))
            .unwrap();
        let done = scope.finish().unwrap();
        assert_eq!(done.cases().len(), 1);
        assert!(done.root().join("001/test_vectors/test1.txt").is_file());

        let crate_root = done.root().join("001/translated_rust");
        assert!(!crate_root.is_symlink() && crate_root.is_dir());
        assert!(crate_root.join("Cargo.toml").is_file());
        fs::write(crate_root.join("src/main.rs"), "fn main() { /* built */ }").unwrap();
        assert_eq!(
            fs::read_to_string(case_dir.join("translated/src/main.rs")).unwrap(),
            "fn main() {}",
            "the published artifact must be untouched by the scoring build"
        );
    }

    #[test]
    fn a_case_the_oracle_did_not_discover_is_refused_not_averaged_away() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("results")).unwrap();
        fs::create_dir_all(tmp.path().join("test-corpus")).unwrap();
        let paths = paths_at(tmp.path());
        let tree = Tree::create_empty(&paths, Keep::ForPostMortem).unwrap();
        let mut scope = tree.scope("B01").unwrap();
        for case in ["001", "002"] {
            let artifact = published(&paths.case_dir("B01", case), "fn main() {}");
            scope
                .materialise(case, &artifact, &corpus_case(tmp.path(), "B01", case))
                .unwrap();
        }
        let done = scope.finish().unwrap();

        let both: BTreeSet<String> = ["001", "002"].iter().map(|s| s.to_string()).collect();
        done.reconcile(2, &both).expect("agreeing counts pass");

        let one: BTreeSet<String> = ["001"].iter().map(|s| s.to_string()).collect();
        let err = done
            .reconcile(1, &one)
            .expect_err("a case that vanished from the denominator must refuse");
        let text = format!("{err:#}");
        assert!(text.contains("002"), "and must name the case: {text}");
    }

    #[test]
    fn the_tree_is_removed_when_the_run_does_not_ask_to_keep_it() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("results")).unwrap();
        fs::create_dir_all(tmp.path().join("test-corpus")).unwrap();
        let paths = paths_at(tmp.path());
        let root = {
            let tree = Tree::create_empty(&paths, Keep::Discard).unwrap();
            let mut scope = tree.scope("B01").unwrap();
            let artifact = published(&paths.case_dir("B01", "001"), "fn main() {}");
            scope
                .materialise("001", &artifact, &corpus_case(tmp.path(), "B01", "001"))
                .unwrap();
            let done = scope.finish().unwrap();
            let root = done.root().to_path_buf();
            assert!(root.join("001/translated_rust").is_dir(), "fixture");
            root
        };
        assert!(
            !root.exists(),
            "a tree left standing is one the next run could read"
        );
    }
}
