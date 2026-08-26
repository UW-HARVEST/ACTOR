//! The agent-invocation cache: one key, one entry, and the path IS the key.
//!
//! `.cache/<tool>/<model>/<before>/<prompt>/` carries every component the key is made of, so there
//! is no key hash to keep in agreement with the directory it names. That only holds if every level
//! is INJECTIVE, which is why [`model_dir`] encodes rather than abbreviates: the previous rendering
//! stripped a vendor prefix as everything-before-the-first-dot and turned `openai/gpt-5.4` into a
//! directory called `4`.
//!
//! One entry per key. Several attempts are not representable, so a table's numbers follow from the
//! key alone and there is no selection rule, no recorded pin and no tie to break.

use crate::tree::{Tree, TreeDigest};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// What may answer "has this invocation already run?", and what a miss costs.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    ReadWrite,
    /// A miss is a refusal, never an invocation: what `reproduce.sh` needs to be incapable of
    /// spending money.
    ReplayOnly,
}

/// A model id rendered as one path component, losslessly.
///
/// Percent-encoded rather than sanitised: `claude-opus-5[1m]` and `claude-opus-5(1m)` must not
/// become the same directory, and a bracket in a directory name is a glob to every shell that walks
/// `results/`. Nothing is stripped -- the vendor and region prefixes are part of the id, and
/// dropping them is what let two ids collide.
pub fn model_dir(model: &str) -> String {
    let mut out = String::with_capacity(model.len());
    for b in model.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A digest as one path component: `sha256:ab…` cannot be a directory name on every filesystem we
/// might land on, and hex carries no `-`, so the substitution stays injective.
fn digest_dir(d: &str) -> String {
    d.replace(':', "-")
}

/// The prompt as the key names it: the FINAL text, after substitution and path normalisation.
///
/// Carries the text beside the hash because the text is stored in the entry -- a change to how
/// prompts are normalised then becomes a re-key rather than a cache wipe, since every entry holds
/// the bytes its hash was taken over.
pub struct Prompt {
    text: String,
    hash: String,
}

impl Prompt {
    /// `normalised` must already have machine-specific paths tokenised, or the hash is a per-run
    /// nonce and no entry ever hits. Normalising is the caller's edge, not this type's.
    pub fn new(normalised: impl Into<String>) -> Self {
        let text = normalised.into();
        let mut h = Sha256::new();
        h.update((text.len() as u64).to_le_bytes());
        h.update(text.as_bytes());
        Self {
            hash: format!("sha256:{:x}", h.finalize()),
            text,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }
}

/// WHERE an entry lives, which is also WHAT it is.
///
/// Borrowed rather than owned so a caller cannot pair one invocation's tool with another's model and
/// have the mismatch stored as a legitimate entry.
pub struct Key<'a> {
    pub tool: &'a str,
    pub model: &'a str,
    pub before: &'a TreeDigest,
    pub prompt: &'a Prompt,
}

impl Key<'_> {
    /// `<tool>/<model>/<before>` — shared by every prompt run against that working dir, which is
    /// why `before/` is stored once at this level and not per prompt.
    fn input_dir(&self, root: &Path) -> PathBuf {
        root.join(self.tool)
            .join(model_dir(self.model))
            .join(digest_dir(self.before.as_str()))
    }

    fn entry_dir(&self, root: &Path) -> PathBuf {
        self.input_dir(root).join(digest_dir(self.prompt.hash()))
    }
}

/// What the run did, and nothing else. Everything the path already carries -- tool, model, input
/// tree, prompt -- is deliberately absent: recording it twice is what lets two records disagree.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct AgentRecord {
    pub outcome: Outcome,
    /// The digest of the `after/` beside this record. Not bookkeeping: it is what makes a corrupted
    /// tree detectable rather than silently served.
    pub output_tree: Option<String>,
    pub wall_secs: u64,
    /// `None` where the transcript carries no spend: kiro writes prose, so there is nothing to read
    /// it from, and a `0` there would be a measurement nobody made.
    pub cost_usd: Option<f64>,
    pub cli: String,
}

/// Whether the agent ran, CLASSIFIED from the transcript rather than read off an exit code: every
/// session pipes through `tee`, so a killed agent reported a clean 0 until `set -o pipefail` was
/// asserted, and a run can exit 0 having produced nothing.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Infra { reason: String, detail: String },
    Unknown { why: String },
}

impl From<&crate::domain::health::Health> for Outcome {
    fn from(h: &crate::domain::health::Health) -> Self {
        use crate::domain::health::Health;
        match h {
            Health::Completed => Outcome::Completed,
            Health::Infra { reason, detail } => Outcome::Infra {
                reason: reason.clone(),
                detail: detail.clone(),
            },
            Health::Unknown { why } => Outcome::Unknown { why: why.clone() },
        }
    }
}

/// One invocation's stored result: the tree it produced, and what it cost to get.
pub struct Stored {
    pub after: Tree,
    pub record: AgentRecord,
}

#[derive(Default)]
pub struct Counts {
    pub hits: usize,
    pub invocations: usize,
}

pub struct Store {
    root: PathBuf,
    mode: Mode,
    counts: std::sync::Mutex<Counts>,
}

impl Store {
    pub fn open(repo_root: &Path, mode: Mode) -> Result<Self> {
        let root = repo_root.join("results/.cache");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating the store at {}", root.display()))?;
        Ok(Self {
            root,
            mode,
            counts: std::sync::Mutex::new(Counts::default()),
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The stored result of this invocation, if there is one worth serving.
    ///
    /// An entry whose outcome is not `Completed` is a RECORD, not a hit: it is kept as evidence of
    /// what went wrong and a re-run replaces it. Returning it would publish a number from a run
    /// that never finished.
    pub fn lookup(&self, key: &Key<'_>) -> Result<Option<Stored>> {
        let dir = key.entry_dir(&self.root);
        let record_path = dir.join("agent.json");
        if !record_path.is_file() {
            return Ok(None);
        }
        let record: AgentRecord = serde_json::from_str(&std::fs::read_to_string(&record_path)?)
            .with_context(|| format!("reading {}", record_path.display()))?;
        if record.outcome != Outcome::Completed {
            return Ok(None);
        }
        let Some(recorded) = &record.output_tree else {
            anyhow::bail!(
                "{} says the run completed but records no output tree",
                record_path.display()
            );
        };
        let after = Tree::adopt_stored(dir.join("after"), recorded)?;
        self.count(|c| c.hits += 1);
        Ok(Some(Stored { after, record }))
    }

    /// Write the entry. `before` is written once per input tree and shared by every prompt beneath
    /// it, which is the whole reason the path splits where it does.
    pub fn write(
        &self,
        key: &Key<'_>,
        before: &Tree,
        after: Option<&Tree>,
        record: &AgentRecord,
        log: Option<&Path>,
    ) -> Result<()> {
        let input_dir = key.input_dir(&self.root);
        let before_at = input_dir.join("before");
        if !before_at.exists() {
            before.copy_into(&before_at)?;
        }
        let dir = key.entry_dir(&self.root);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("prompt.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "digest": key.prompt.hash(),
                "text": key.prompt.text(),
            }))?,
        )?;
        if let Some(after) = after {
            let at = dir.join("after");
            if at.exists() {
                std::fs::remove_dir_all(&at)?;
            }
            after.copy_into(&at)?;
        }
        if let Some(log) = log {
            if log.is_file() {
                std::fs::copy(log, dir.join("run.log"))?;
            }
        }
        std::fs::write(
            dir.join("agent.json"),
            serde_json::to_string_pretty(record)?,
        )?;
        Ok(())
    }

    pub fn count(&self, f: impl FnOnce(&mut Counts)) {
        if let Ok(mut c) = self.counts.lock() {
            f(&mut c);
        }
    }

    /// The line `reproduce.sh` greps: it must say `0 run` and `0 agent invocation(s)` for every
    /// phase, or the run paid for something and reproduced nothing.
    pub fn tally_line(&self) -> String {
        let c = self.counts.lock().expect("counts");
        format!(
            "\u{1f5c3}\u{fe0f}  cache: {} hit / {} run ({} agent invocation(s))",
            c.hits, c.invocations, c.invocations
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed_tree(text: &str) -> (tempfile::TempDir, crate::tree::Tree) {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let c = tmp.path().join("test_case");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(c.join("lib.c"), "int f(void);\n").unwrap();
        let w = crate::tree::WorkDir::assemble(&c).unwrap();
        std::fs::write(w.translation().join("lib.rs"), text).unwrap();
        (tmp, w.seal().unwrap())
    }

    fn record(outcome: Outcome, after: Option<&crate::tree::Tree>) -> AgentRecord {
        AgentRecord {
            outcome,
            output_tree: after.map(|t| t.digest().as_str().to_string()),
            wall_secs: 12,
            cost_usd: Some(0.5),
            cli: "claude 2.1.235".into(),
        }
    }

    #[test]
    fn an_entry_written_is_an_entry_served() {
        let repo = crate::io::workdir::test_tempdir().unwrap();
        let store = Store::open(repo.path(), Mode::ReadWrite).unwrap();
        let (_b, before) = sealed_tree("before\n");
        let (_a, after) = sealed_tree("after\n");
        let prompt = Prompt::new("do the thing");
        let key = Key {
            tool: "claude",
            model: "global.anthropic.claude-opus-5[1m]",
            before: before.digest(),
            prompt: &prompt,
        };

        assert!(store.lookup(&key).unwrap().is_none(), "empty store, no hit");
        store
            .write(
                &key,
                &before,
                Some(&after),
                &record(Outcome::Completed, Some(&after)),
                None,
            )
            .unwrap();

        let got = store.lookup(&key).unwrap().expect("the entry just written");
        assert_eq!(got.after.digest(), after.digest());
        assert_eq!(got.record.wall_secs, 12);
        // `before/` is stored once at the input level, shared by every prompt beneath it.
        let shared = key.input_dir(&store.root).join("before");
        assert!(
            shared.join("c_src/lib.c").is_file(),
            "before/ must hold the C"
        );
        assert!(
            shared
                .join(crate::tree::TRANSLATION)
                .join("lib.rs")
                .is_file(),
            "and the translation it was handed"
        );
    }

    #[test]
    fn an_entry_that_did_not_complete_is_a_record_and_not_a_hit() {
        // Serving one would publish a number from a run that never finished. Keeping it is how the
        // failure stays inspectable without a separate `failed/` subtree.
        let repo = crate::io::workdir::test_tempdir().unwrap();
        let store = Store::open(repo.path(), Mode::ReadWrite).unwrap();
        let (_b, before) = sealed_tree("before\n");
        let prompt = Prompt::new("do the thing");
        let key = Key {
            tool: "claude",
            model: "m",
            before: before.digest(),
            prompt: &prompt,
        };
        let infra = Outcome::Infra {
            reason: "api_error".into(),
            detail: "terminal_reason=api_error".into(),
        };
        store
            .write(&key, &before, None, &record(infra, None), None)
            .unwrap();

        assert!(
            store.lookup(&key).unwrap().is_none(),
            "an infra failure must not answer a lookup"
        );
        assert!(
            key.entry_dir(&store.root).join("agent.json").is_file(),
            "but the record must still be there to read"
        );
    }

    #[test]
    fn a_model_directory_is_injective() {
        // The previous rendering stripped "the vendor prefix" as everything-before-the-first-dot,
        // so `openai/gpt-5.4` became the directory `4`. Anything lossy used as key material makes
        // two runs share an entry, which is silent corruption rather than a visible failure.
        let ids = [
            "global.anthropic.claude-opus-5[1m]",
            "global.anthropic.claude-opus-5(1m)",
            "anthropic.claude-opus-5[1m]",
            "openai/gpt-5.4",
            "openai.gpt-5.4",
            "us.anthropic.claude-sonnet-5",
            "eu.anthropic.claude-sonnet-5",
            "unpinned:kiro-cli-default",
            "moonshotai.kimi-k2.5",
        ];
        let dirs: Vec<String> = ids.iter().map(|i| model_dir(i)).collect();
        let unique: std::collections::BTreeSet<&String> = dirs.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "two model ids share a directory: {dirs:#?}"
        );
        for d in &dirs {
            assert!(
                !d.contains('/') && !d.contains('[') && !d.contains(':'),
                "{d} is not a single safe path component"
            );
        }
    }

    #[test]
    fn the_path_carries_every_component_of_the_key() {
        // If a component were missing from the path, two different invocations would address the
        // same directory -- and with no key hash beside it, nothing would notice.
        let d = TreeDigest::for_test("sha256:aaa");
        let p = Prompt::new("translate this");
        let root = Path::new("/r");
        let base = Key {
            tool: "claude",
            model: "m1",
            before: &d,
            prompt: &p,
        }
        .entry_dir(root);

        let other_tree = TreeDigest::for_test("sha256:bbb");
        let other_prompt = Prompt::new("verify this");
        for (what, k) in [
            (
                "tool",
                Key {
                    tool: "codex",
                    model: "m1",
                    before: &d,
                    prompt: &p,
                },
            ),
            (
                "model",
                Key {
                    tool: "claude",
                    model: "m2",
                    before: &d,
                    prompt: &p,
                },
            ),
            (
                "before",
                Key {
                    tool: "claude",
                    model: "m1",
                    before: &other_tree,
                    prompt: &p,
                },
            ),
            (
                "prompt",
                Key {
                    tool: "claude",
                    model: "m1",
                    before: &d,
                    prompt: &other_prompt,
                },
            ),
        ] {
            assert_ne!(base, k.entry_dir(root), "{what} must change the entry dir");
        }
    }

    #[test]
    fn a_prompt_hash_is_stable_and_length_prefixed() {
        assert_eq!(Prompt::new("abc").hash(), Prompt::new("abc").hash());
        assert_ne!(Prompt::new("abc").hash(), Prompt::new("abd").hash());
        // Length-prefixed, so concatenation cannot collide: ("a","bc") and ("ab","c") would hash
        // alike without it.
        assert_ne!(Prompt::new("a\0bc").hash(), Prompt::new("ab\0c").hash());
    }
}
