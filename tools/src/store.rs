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

use crate::io::workdir::Roots;
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

/// The model the agent will actually use. Must be pinned before the run: `--agent
/// claude` passes no `--model`, so the resolved model appears only in the log's `init`
/// record, after the fact — and the CLI auto-updates, so an unkeyed model could hand
/// back output produced by a different one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        anyhow::ensure!(!s.trim().is_empty(), "model id must not be empty");
        // It reaches a `bash -c` command line double-quoted (`[1m]` is a bracket
        // glob), so refuse anything that could break out of those quotes.
        anyhow::ensure!(
            !s.contains('\'') && !s.contains('`') && !s.contains('$') && !s.contains('\n'),
            "model id contains shell metacharacters: {s}"
        );
        Ok(Self(s))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A model id rendered as one path component, losslessly.
///
/// Escaped rather than sanitised: `claude-opus-5[1m]` and `claude-opus-5(1m)` must not become one
/// directory, and a bracket is a glob to every shell that walks `results/`. Nothing is stripped --
/// dropping the vendor prefix is what let `openai/gpt-5.4` name a directory `4`. `~` is the escape, NOT
/// `%`: a percent anywhere in a path makes `rust-lld` fail to open its own output, so with `%5B1m%5D` in
/// the model level every cdylib case failed to build at grading time and scored 0. The alphabet is
/// `[A-Za-z0-9._~-]`, and escaping `~` itself keeps it injective.
pub fn model_dir(model: &str) -> String {
    let mut out = String::with_capacity(model.len());
    for b in model.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => out.push(b as char),
            _ => out.push_str(&format!("~{b:02x}")),
        }
    }
    out
}

/// A digest as one path component: `sha256:ab…` cannot be a directory name on every filesystem we
/// might land on, and hex carries no `-`, so the substitution stays injective.
fn digest_dir(d: &str) -> String {
    d.replace(':', "-")
}

/// Machine-specific paths, tokenised, so a prompt hashes the same on every machine.
///
/// Without it the scratch directory name is a nonce in the key and no entry ever hits: caching would
/// look enabled while never working.
pub fn normalise(text: &str, roots: &Roots) -> String {
    let mut out = text.to_string();
    // `to_str`, never `to_string_lossy`: lossy mapping sends every invalid byte to U+FFFD, so two
    // roots can produce the same substitution and two prompts the same digest -- a false cache *hit*,
    // the one failure mode this key exists to prevent. Skipping a non-UTF-8 root can only cost a miss.
    let mut subs: Vec<(&str, &str)> = [
        (Some(roots.work.as_path()), "$WORK"),
        (Some(roots.repo.as_path()), "$REPO"),
        // Longest-first below puts $REPO ahead of this, so a path under the repo is never
        // rewritten as `$REPOPARENT/ACTOR/...` and the checkout's own name stays out of the key.
        (roots.repo_parent.as_deref(), "$REPOPARENT"),
        (roots.work_base.as_deref(), "$WORKBASE"),
        (roots.home.as_deref(), "$HOME"),
    ]
    .into_iter()
    .filter_map(|(p, token)| p?.to_str().map(|s| (s, token)))
    .collect();
    // Longest first: a work root nested under the scratch base would otherwise be
    // rewritten as `$WORKBASE/harvest-work-AbCdEf`, putting the per-run name in the key.
    subs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    for (from, to) in subs {
        if !from.is_empty() {
            out = out.replace(from, to);
        }
    }
    out
}

/// The prompt as the key names it: the FINAL text, after substitution and path normalisation. The text
/// rides beside the hash because the entry stores it, so a change to normalisation is a re-key rather
/// than a cache wipe -- every entry holds the bytes its hash was taken over.
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
    /// Whether the CLI's own transcript confirmed the model it was pinned to. `default` because every
    /// entry written before the check was wired has no answer, and inventing `Confirmed` for those
    /// would be the claim this field exists to stop making.
    #[serde(default)]
    pub pin: crate::domain::health::PinReport,
    /// Whether the agent that produced this entry was confined to its working dir. `default` for the
    /// same reason: entries written before this was recorded have no answer.
    #[serde(default)]
    pub sandboxed: crate::io::sandbox::Sandboxed,
}

/// Whether the agent ran, CLASSIFIED from the transcript rather than read off an exit code: every
/// session pipes through `tee`, so a killed agent reported a clean 0 until `set -o pipefail` was
/// asserted, and a run can exit 0 having produced nothing.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Infra {
        reason: String,
        detail: String,
    },
    /// The agent used its whole wall-clock ceiling. Scored as a case failure, not as infra.
    Exhausted {
        secs: u64,
    },
    /// The provider declined on content grounds. The field is `refusal`, not `kind`, because `kind` is
    /// this enum's own serde tag and the two collide at compile time.
    Refused {
        refusal: crate::domain::health::RefusalKind,
        detail: String,
    },
    Unknown {
        why: String,
    },
}

impl Outcome {
    /// The TOOL'S OWN answer, settled and unchangeable by a re-run, so a replay may serve it.
    /// `lookup` serves only `Completed` -- right for `Infra`, which has no measurement, and wrong for
    /// these two, which this codebase calls "terminal and reproducible, unlike `Infra`": declining them
    /// made a definitive answer unreplayable and, `attests` being all-or-nothing, took the battery too.
    /// kiro's B01_synthetic served 83 of 85, and re-running one exhausts another 12-hour session.
    pub fn is_terminal_answer(&self) -> bool {
        match self {
            Outcome::Refused { .. } | Outcome::Exhausted { .. } => true,
            Outcome::Completed | Outcome::Infra { .. } | Outcome::Unknown { .. } => false,
        }
    }

    /// Whether another attempt could plausibly succeed. NOT a function of the tool: only claude's CLI
    /// exposes a retry setting, so leaving resilience to the CLIs made the harness's tolerance vary by
    /// backend. `Refused` is reproducible; `Exhausted` already spent its budget.
    pub fn is_transient(&self) -> bool {
        match self {
            Outcome::Infra { .. } => true,
            Outcome::Completed
            | Outcome::Exhausted { .. }
            | Outcome::Refused { .. }
            | Outcome::Unknown { .. } => false,
        }
    }
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
            Health::Exhausted { secs } => Outcome::Exhausted { secs: *secs },
            Health::Refused { kind, detail } => Outcome::Refused {
                refusal: kind.clone(),
                detail: detail.clone(),
            },
            Health::Unknown { why } => Outcome::Unknown { why: why.clone() },
        }
    }
}

/// One invocation's stored result: the tree it produced, what it cost to get, and the transcript.
pub struct Stored {
    pub after: Tree,
    pub record: AgentRecord,
    /// The entry's `run.log`. PRIVATE, and reachable only by [`Self::republish_transcript`], which
    /// takes a destination: a public path field is a path escape, and the architecture gate says so.
    transcript: Option<PathBuf>,
}

impl Stored {
    /// Put the stored transcript where the run would have written it, if the entry kept one.
    ///
    /// Returns whether it did. The store keeps entries at `0o444`, so the copy is made writable --
    /// otherwise the next run cannot overwrite it and every case after the first fails with EACCES.
    pub fn republish_transcript(&self, to: &Path) -> Result<bool> {
        let Some(from) = &self.transcript else {
            return Ok(false);
        };
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::remove_file(to) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("replacing {}", to.display())),
        }
        std::fs::copy(from, to)
            .with_context(|| format!("republishing {} to {}", from.display(), to.display()))?;
        crate::tree::set_read_only(to, crate::tree::Access::Writable)?;
        Ok(true)
    }
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
        // An entry that no longer reproduces its recorded digest is CORRUPT, and the mode decides:
        // `ReadWrite` treats it as a miss and re-pays, which is the design's own remedy; aborting the
        // case wedged it for ever once a digest RULE changed under it. `ReplayOnly` still errors.
        let after = match Tree::adopt_stored(dir.join("after"), recorded) {
            Ok(after) => after,
            Err(e) if self.mode == Mode::ReadWrite => {
                eprintln!("  cache: discarding a corrupt entry and re-running it: {e:#}");
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        self.count(|c| c.hits += 1);
        let transcript = dir.join("run.log");
        Ok(Some(Stored {
            after,
            record,
            transcript: transcript.is_file().then_some(transcript),
        }))
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

    /// Entry count and bytes on disk. An entry is a directory holding an `agent.json`, which is what
    /// `cache stats` reports -- counting directories would count the tree levels above them too.
    pub fn stats(&self) -> Result<(usize, u64)> {
        fn walk(dir: &Path, entries: &mut usize, bytes: &mut u64) -> Result<()> {
            if dir.join("agent.json").is_file() {
                *entries += 1;
            }
            for e in std::fs::read_dir(dir)?.filter_map(std::result::Result::ok) {
                let meta = e.metadata()?;
                if meta.is_dir() {
                    walk(&e.path(), entries, bytes)?;
                } else {
                    *bytes += meta.len();
                }
            }
            Ok(())
        }
        let (mut entries, mut bytes) = (0, 0);
        if self.root.is_dir() {
            walk(&self.root, &mut entries, &mut bytes)?;
        }
        Ok((entries, bytes))
    }

    /// The record here when [`Outcome::is_terminal_answer`] holds.
    pub fn terminal_record(&self, key: &Key<'_>) -> Result<Option<AgentRecord>> {
        let record_path = key.entry_dir(&self.root).join("agent.json");
        if !record_path.is_file() {
            return Ok(None);
        }
        let record: AgentRecord = serde_json::from_str(&std::fs::read_to_string(&record_path)?)
            .with_context(|| format!("reading {}", record_path.display()))?;
        Ok(record.outcome.is_terminal_answer().then_some(record))
    }

    /// Every entry whose run did not complete, as `(entry path AS TEXT, outcome)`. Text, not a
    /// `PathBuf`: a caller holding a path can run a command in the store. A walk over ordinary entries
    /// rather than a second tree -- which is what removed the `failed/` subtree and its walker.
    pub fn failures(&self) -> Result<Vec<(String, Outcome)>> {
        fn walk(dir: &Path, out: &mut Vec<(String, Outcome)>) -> Result<()> {
            let record = dir.join("agent.json");
            if record.is_file() {
                let parsed: AgentRecord = serde_json::from_str(&std::fs::read_to_string(&record)?)
                    .with_context(|| format!("reading {}", record.display()))?;
                if parsed.outcome != Outcome::Completed {
                    out.push((dir.display().to_string(), parsed.outcome));
                }
                return Ok(());
            }
            for e in std::fs::read_dir(dir)?.filter_map(std::result::Result::ok) {
                if e.metadata()?.is_dir() {
                    walk(&e.path(), out)?;
                }
            }
            Ok(())
        }
        let mut out = Vec::new();
        if self.root.is_dir() {
            walk(&self.root, &mut out)?;
        }
        Ok(out)
    }

    /// Re-hash every stored `before/` and `after/` against the digest it is filed under.
    ///
    /// The check the store was missing. `lookup` validates only `after` (via `Tree::adopt_stored`),
    /// and `write` treats `before/`'s mere EXISTENCE as proof it is complete:
    /// `if !before_at.exists() { before.copy_into(..) }`. `copy_carrying` copies file by file with no
    /// staging, so a sweep killed mid-copy -- Ctrl-C, OOM, the session's own `ulimit -f` -- leaves a
    /// truncated `before/`, and every later entry under that input tree takes the `!exists()` branch as
    /// false and skips the copy. The entry is then served for ever from a tree that does not hash to its
    /// own directory name, which is the definition of corrupt in this design: "a `before/` that no
    /// longer reproduces its own directory name is a corrupt entry".
    ///
    /// Zero agent invocations, so CI can run it on every push. Returns what it checked, because a
    /// verifier that inspects nothing must not report success.
    pub fn verify(&self) -> Result<(usize, Vec<String>)> {
        let mut checked = 0usize;
        let mut corrupt = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let entries: Vec<_> = match std::fs::read_dir(&dir) {
                Ok(rd) => rd.filter_map(std::result::Result::ok).collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("walking {}", dir.display())),
            };
            for e in entries {
                let at = e.path();
                if !at.is_dir() {
                    continue;
                }
                // A tree's own directory NAME is the digest it must reproduce: `before/` is filed under
                // the input digest one level up, and `after/`'s is recorded in `agent.json` beside it.
                let recorded = match at.file_name().and_then(|n| n.to_str()) {
                    Some("before") => dir.file_name().and_then(|n| n.to_str()).map(|d| {
                        // `digest_dir` is the only lossy step between a digest and a path.
                        d.replacen('-', ":", 1)
                    }),
                    Some("after") => {
                        let record = dir.join("agent.json");
                        match std::fs::read_to_string(&record) {
                            Ok(text) => {
                                serde_json::from_str::<AgentRecord>(&text)
                                    .with_context(|| format!("reading {}", record.display()))?
                                    .output_tree
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                            Err(e) => {
                                return Err(e)
                                    .with_context(|| format!("reading {}", record.display()))
                            }
                        }
                    }
                    _ => {
                        stack.push(at);
                        continue;
                    }
                };
                let Some(recorded) = recorded else { continue };
                checked += 1;
                if let Err(e) = Tree::adopt_stored(at.clone(), &recorded) {
                    corrupt.push(format!("{}: {e:#}", at.display()));
                }
            }
        }
        Ok((checked, corrupt))
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
mod corrupt_tests {
    use super::*;

    /// A corrupt entry is a MISS when the run may pay and an error when it may not. claude's macrodepth
    /// verify was hashed under the rule that included `target-cfg`; once that changed, `lookup` returning
    /// the error ABORTED the case rather than re-running it, so the battery could never heal.
    #[test]
    fn a_corrupt_entry_is_re_run_when_paying_is_allowed_and_refused_when_it_is_not() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let corpus = tmp.path().join("test_case");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(corpus.join("lib.c"), "int f(void);\n").unwrap();
        let before = crate::tree::WorkDir::assemble(&crate::tree::Corpus::at(&corpus).unwrap())
            .unwrap()
            .seal()
            .unwrap();

        let model = ModelId::new("global.anthropic.claude-opus-5[1m]").unwrap();
        let prompt = Prompt::new("verify");
        let key = Key {
            tool: "claude",
            model: model.as_str(),
            before: before.digest(),
            prompt: &prompt,
        };

        let store = Store::open(tmp.path(), Mode::ReadWrite).unwrap();
        let w = crate::tree::WorkDir::assemble(&crate::tree::Corpus::at(&corpus).unwrap()).unwrap();
        std::fs::write(w.translation().join("lib.rs"), "pub fn f() {}\n").unwrap();
        let after = w.seal().unwrap();
        let record = AgentRecord {
            pin: crate::domain::health::PinReport::Confirmed,
            sandboxed: crate::io::sandbox::Sandboxed::Enforced,
            outcome: Outcome::Completed,
            output_tree: Some(after.digest().as_str().to_string()),
            wall_secs: 1,
            cost_usd: None,
            cli: "fake".into(),
        };
        store
            .write(&key, &before, Some(&after), &record, None)
            .unwrap();
        assert!(
            store.lookup(&key).unwrap().is_some(),
            "fixture: an intact entry must serve, or the corruption proves nothing"
        );

        // Now make the stored bytes disagree with the recorded digest.
        let cache = tmp.path().join("results/.cache");
        let at = key
            .entry_dir(&cache)
            .join("after")
            .join(crate::tree::TRANSLATION)
            .join("lib.rs");
        let mut perms = std::fs::metadata(&at).unwrap().permissions();
        #[allow(
            clippy::permissions_set_readonly_false,
            reason = "the point of the test is to tamper with a stored tree, which is read-only"
        )]
        perms.set_readonly(false);
        std::fs::set_permissions(&at, perms).unwrap();
        std::fs::write(&at, "pub fn f() { /* edited after the fact */ }\n").unwrap();

        assert!(
            store.lookup(&key).unwrap().is_none(),
            "ReadWrite must treat it as a miss so the run replaces it"
        );
        let replay = Store::open(tmp.path(), Mode::ReplayOnly).unwrap();
        let err = match replay.lookup(&key) {
            Ok(_) => panic!("ReplayOnly cannot pay, so it must refuse rather than serve or shrug"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("records"), "{err:#}");
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn a_terminal_answer_is_servable_on_replay_and_a_lost_run_is_not() {
        for settled in [
            Outcome::Exhausted { secs: 43_200 },
            Outcome::Refused {
                refusal: crate::domain::health::RefusalKind::HighRiskCyberActivity,
                detail: String::new(),
            },
        ] {
            assert!(settled.is_terminal_answer(), "{settled:?} cannot change");
            assert!(!settled.is_transient(), "and must not be retried either");
        }
        for lost in [
            Outcome::Infra {
                reason: "timeout".into(),
                detail: String::new(),
            },
            Outcome::Unknown {
                why: "opaque log".into(),
            },
        ] {
            assert!(
                !lost.is_terminal_answer(),
                "{lost:?} has no measurement to serve, so a replay must still refuse it"
            );
        }
        // `Completed` is served by `lookup` and must not be reachable by this door as well.
        assert!(!Outcome::Completed.is_terminal_answer());
    }
}

#[cfg(test)]
mod attempt_tests {
    use super::*;

    /// Which outcomes are worth another attempt -- exhaustively, and with no `Tool` in sight. That
    /// absence is the point; retrying a `Refused` would also buy a reproducible answer twice.
    #[test]
    fn only_an_infra_outcome_is_worth_another_attempt() {
        assert!(Outcome::Infra {
            reason: "throttled".into(),
            detail: String::new()
        }
        .is_transient());

        for terminal in [
            Outcome::Completed,
            Outcome::Exhausted { secs: 43_200 },
            Outcome::Refused {
                refusal: crate::domain::health::RefusalKind::HighRiskCyberActivity,
                detail: String::new(),
            },
            Outcome::Unknown {
                why: "opaque log, agent exited 1".into(),
            },
        ] {
            assert!(
                !terminal.is_transient(),
                "{terminal:?} is an answer or a spent budget, not a blip"
            );
        }
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
        let w = crate::tree::WorkDir::assemble(&crate::tree::Corpus::at(&c).unwrap()).unwrap();
        std::fs::write(w.translation().join("lib.rs"), text).unwrap();
        (tmp, w.seal().unwrap())
    }

    fn record(outcome: Outcome, after: Option<&crate::tree::Tree>) -> AgentRecord {
        AgentRecord {
            pin: crate::domain::health::PinReport::Confirmed,
            sandboxed: crate::io::sandbox::Sandboxed::Enforced,
            outcome,
            output_tree: after.map(|t| t.digest().as_str().to_string()),
            wall_secs: 12,
            cost_usd: Some(0.5),
            cli: "claude 2.1.235".into(),
        }
    }

    /// A truncated `before/` is served for ever unless something re-hashes it.
    ///
    /// `write` is `if !before_at.exists() { before.copy_into(..) }` and `copy_carrying` copies file by
    /// file with no staging, so a sweep killed mid-copy (Ctrl-C, OOM, the session's own `ulimit -f`)
    /// leaves that directory PRESENT and incomplete -- and every later entry under the same input tree
    /// takes the `!exists()` branch as false and skips the copy. `lookup` validates only `after`, so
    /// nothing ever looked, and there was no `cache verify` to look with.
    ///
    /// Non-vacuity: the store is clean before the file is removed, so the failure is the damage and
    /// not the walk.
    #[test]
    fn a_truncated_stored_tree_is_detected_by_cache_verify() {
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
        store
            .write(
                &key,
                &before,
                Some(&after),
                &record(Outcome::Completed, Some(&after)),
                None,
            )
            .unwrap();

        let (checked, corrupt) = store.verify().unwrap();
        assert!(checked >= 2, "both trees must be inspected, saw {checked}");
        assert!(
            corrupt.is_empty(),
            "a freshly written store is clean: {corrupt:?}"
        );

        // Exactly what an interrupted `copy_into` leaves: the directory present, a file missing.
        let victim = key
            .input_dir(&repo.path().join("results/.cache"))
            .join("before/translation/lib.rs");
        crate::tree::set_read_only(victim.parent().unwrap(), crate::tree::Access::Writable)
            .unwrap();
        std::fs::remove_file(&victim).unwrap();

        let (checked, corrupt) = store.verify().unwrap();
        assert!(checked >= 2, "still inspecting both trees");
        assert_eq!(
            corrupt.len(),
            1,
            "the truncated tree must be named: {corrupt:?}"
        );
        assert!(
            corrupt[0].contains("before"),
            "and named as the BEFORE tree, which `lookup` never validates: {}",
            corrupt[0]
        );
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
        // Injective is not sufficient: `%` in a path makes `rust-lld` fail to open its own output.
        for d in &dirs {
            assert!(
                d.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b)),
                "{d} leaves the alphabet the filesystem, cargo and the linker all accept"
            );
        }
        // Non-vacuous: one of these ids must need escaping, or the rule above saw only plain ASCII.
        assert!(
            dirs.iter().any(|d| d.contains('~')),
            "no id in this set exercises the escape, so injectivity proves nothing here"
        );
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
