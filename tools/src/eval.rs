//! The tree a score is taken from: created empty every run, filled only from the artifacts that run
//! resolved plus the corpus, deleted afterwards. No old file is read because none is present.
//! `translated_rust/` must be a REAL directory: `rust.py` pins the build to
//! `(case_root / "translated_rust").resolve() / "target"`, so the symlink this replaces put 666
//! `target/` dirs inside published phase dirs while both tests asserting `target/` is absent passed.

use crate::battery::Paths;
use crate::tree::Tree;
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

/// What a score is taken from: the trees THIS RUN resolved, and nothing else. It was an enum whose
/// `Archive` variant read a phase dir with no key; measured when that went, 16 of 17 published agents
/// had no cached agent call at all, so ~95% of the numbers rested on artifacts nothing could attest.
///
/// Keyed by role name rather than by phase type: a chain of three steps needs no new field.
pub type Resolved = std::collections::HashMap<PathBuf, Tree>;

pub struct EvalTree {
    root: PathBuf,
    /// `.eval` itself, so [`Drop`] prunes the harness level without walking out of it.
    eval_root: PathBuf,
    keep: Keep,
}

impl EvalTree {
    pub fn create_empty(paths: &Paths, target: &str, keep: Keep) -> Result<Self> {
        let eval_root = paths.repo_root.join(EVAL_DIR);
        // `<tool>/<model>/<variant>/<target>`, and the TARGET level is what lets two batteries of the
        // SAME tool run at once. Without it the root was the tool level and `remove_dir_all` below
        // wiped it wholesale, so a second battery deleted the first one's tree mid-score -- which is
        // why every same-tool leg had to be serialised. `Drop` prunes upward with `remove_dir`, which
        // fails on a non-empty directory, so a sibling battery's tree stops the prune.
        let root = eval_root
            .join(
                paths
                    .results_dir
                    .strip_prefix(
                        paths
                            .results_dir
                            .ancestors()
                            .nth(3)
                            .unwrap_or(&paths.results_dir),
                    )
                    .unwrap_or(&paths.results_dir),
            )
            // A target may name a case (`B01_organic/bin2hex_lib`), which must not become two levels.
            .join(target.replace('/', "~"));
        if root.exists() {
            std::fs::remove_dir_all(&root)
                .with_context(|| format!("emptying {}", root.display()))?;
        }
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            eval_root,
            keep,
        })
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

impl Drop for EvalTree {
    fn drop(&mut self) {
        match self.keep {
            Keep::ForPostMortem => println!(
                "  evaluation tree kept for post-mortem at {}",
                self.root.display()
            ),
            Keep::Discard => {
                let _ = std::fs::remove_dir_all(&self.root);
                // The root is `<harness>/<model>`, so removing it leaves the harness level standing
                // and `reproduce.sh` refuses that. `remove_dir` only succeeds on an empty directory.
                let mut dir = self.root.parent();
                while let Some(d) = dir {
                    if d == self.eval_root || std::fs::remove_dir(d).is_err() {
                        break;
                    }
                    dir = d.parent();
                }
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
    _tree: &'t EvalTree,
    root: PathBuf,
    cases: Vec<Case>,
}

impl Scope<'_> {
    /// Assemble one case for scoring. `record_into` is where its `result.json` goes -- stated by the
    /// caller, which published it, rather than recovered from a path inside the tree.
    pub fn materialise(
        &mut self,
        name: &str,
        tree: &Tree,
        graded: &crate::transform::Graded,
        record_into: &Path,
    ) -> Result<()> {
        let case = self.root.join(name);
        crate::transform::eval_case(&case, tree, graded)
            .with_context(|| format!("assembling {name} for scoring"))?;
        self.cases.push(Case {
            name: name.to_string(),
            record_into: record_into.to_path_buf(),
            case_dir: record_into.parent().unwrap_or(record_into).to_path_buf(),
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The `<tool>/<model>/<variant>` tail the eval tree mirrors.
    fn results_tail(paths: &Paths) -> PathBuf {
        paths
            .results_dir
            .strip_prefix(
                paths
                    .results_dir
                    .ancestors()
                    .nth(3)
                    .unwrap_or(&paths.results_dir),
            )
            .unwrap_or(&paths.results_dir)
            .to_path_buf()
    }

    fn paths_at(repo_root: &Path) -> Paths {
        Paths::new(
            repo_root,
            crate::cli::Tool::Claude,
            crate::cli::Variant::Default,
            crate::cli::Dataset::TestCorpus,
            None,
            crate::store::Mode::ReadWrite,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap()
    }

    /// A tree shaped like a step's output: `c_src/` beside `translation/`, which is what every
    /// working dir is.
    fn published(case_dir: &Path, body: &str) -> Tree {
        let dir = case_dir.join(crate::tree::TRANSLATION);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]\n[workspace]\n").unwrap();
        fs::write(dir.join("src/main.rs"), body).unwrap();
        fs::create_dir_all(case_dir.join(crate::tree::C_SRC)).unwrap();
        fs::write(
            case_dir.join(crate::tree::C_SRC).join("lib.c"),
            "int f(void);\n",
        )
        .unwrap();
        Tree::for_test(case_dir).unwrap()
    }

    fn vectors_of(repo_root: &Path, case: &str) -> crate::transform::Graded {
        crate::transform::Graded::Vectors {
            corpus_case: corpus_case(repo_root, "B01", case),
        }
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
    fn a_discarded_tree_leaves_nothing_at_all_under_eval() {
        // `remove_dir_all(root)` left the harness level standing the moment the root became
        // `<harness>/<model>`, so a run that replayed every phase failed its own cleanliness check.
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("results")).unwrap();
        fs::create_dir_all(tmp.path().join("test-corpus")).unwrap();
        let paths = paths_at(tmp.path());
        let eval = tmp.path().join(EVAL_DIR);

        let tree = EvalTree::create_empty(&paths, "T", Keep::Discard).unwrap();
        tree.scope("B01").unwrap();
        assert!(
            results_tail(&paths).to_string_lossy().contains('/'),
            "non-vacuous only if the fixture's key really has a model level: {}",
            results_tail(&paths).display()
        );
        assert!(
            eval.join(results_tail(&paths)).is_dir(),
            "fixture must build it"
        );
        drop(tree);

        let left: Vec<PathBuf> = fs::read_dir(&eval)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "left standing under .eval/: {left:?}");
    }

    #[test]
    fn the_tree_is_created_empty_and_the_crate_in_it_is_no_symlink_into_results() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("results")).unwrap();
        fs::create_dir_all(tmp.path().join("test-corpus")).unwrap();
        let paths = paths_at(tmp.path());

        // Under THIS target: a stale case from an earlier run of the same target must go, or scoring
        // could read it instead of materialising its own.
        let planted = tmp
            .path()
            .join(EVAL_DIR)
            .join(results_tail(&paths))
            .join("T/B01/leftover/translated_rust");
        fs::create_dir_all(&planted).unwrap();
        fs::write(planted.join("Cargo.toml"), "[package]").unwrap();
        assert!(
            planted.join("Cargo.toml").is_file(),
            "fixture must plant it"
        );
        // Under ANOTHER target: must SURVIVE. This is what lets two batteries of the same tool run at
        // once; when the root was the tool level, the second battery wiped the first one's tree
        // mid-score, which is why every same-tool leg had to be serialised.
        let sibling = tmp
            .path()
            .join(EVAL_DIR)
            .join(results_tail(&paths))
            .join("OTHER/B02/inflight/translated_rust");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("Cargo.toml"), "[package]").unwrap();

        let tree = EvalTree::create_empty(&paths, "T", Keep::ForPostMortem).unwrap();
        assert!(!planted.exists(), "the leftover case must be gone");
        assert!(
            sibling.join("Cargo.toml").is_file(),
            "a concurrent battery's tree must survive: {}",
            sibling.display()
        );

        let case_dir = paths.case_dir("B01", "001");
        let artifact = published(&case_dir, "fn main() {}");
        let mut scope = tree.scope("B01").unwrap();
        scope
            .materialise("001", &artifact, &vectors_of(tmp.path(), "001"), &case_dir)
            .unwrap();
        let done = scope.finish().unwrap();
        assert_eq!(done.cases().len(), 1);
        assert!(done.root().join("001/test_vectors/test1.txt").is_file());

        let crate_root = done.root().join("001/translated_rust");
        assert!(!crate_root.is_symlink() && crate_root.is_dir());
        assert!(crate_root.join("Cargo.toml").is_file());
        fs::write(crate_root.join("src/main.rs"), "fn main() { /* built */ }").unwrap();
        assert_eq!(
            fs::read_to_string(case_dir.join(crate::tree::TRANSLATION).join("src/main.rs"))
                .unwrap(),
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
        let tree = EvalTree::create_empty(&paths, "T", Keep::ForPostMortem).unwrap();
        let mut scope = tree.scope("B01").unwrap();
        for case in ["001", "002"] {
            let case_dir = paths.case_dir("B01", case);
            let artifact = published(&case_dir, "fn main() {}");
            scope
                .materialise(case, &artifact, &vectors_of(tmp.path(), case), &case_dir)
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
            let tree = EvalTree::create_empty(&paths, "T", Keep::Discard).unwrap();
            let mut scope = tree.scope("B01").unwrap();
            let case_dir = paths.case_dir("B01", "001");
            let artifact = published(&case_dir, "fn main() {}");
            scope
                .materialise("001", &artifact, &vectors_of(tmp.path(), "001"), &case_dir)
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
