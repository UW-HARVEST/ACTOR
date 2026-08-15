//! Memoisation of agent phases. There is no cached-vs-uncached fork: every phase
//! runs through [`Store::obtain`], and `--no-cache` is a [`Mode`] of the store rather
//! than an `if` at the call site, so the two paths cannot drift.
//!
//! [`Produced`] is constructible only from a [`Sealed`], which requires
//! [`crate::agent_health::Completed`], so "never cache an infra failure" is
//! unrepresentable rather than checked.
//!
//! Keys are machine-independent: paths are rewritten to `$WORK` / `$REPO` tokens
//! before hashing, because a leaked absolute path would make a colleague's cache
//! silently never hit, indistinguishable from "caching does not help here".
//! `OPENSSL_DIR` is deliberately excluded: it can only influence `build_ok`, which is
//! decided in the test phase, and the test phase is not cached.

use crate::artifact::{Phase, Sealed, TreeDigest};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Bump to invalidate every entry, e.g. if the key composition changes.
pub const SCHEMA: u32 = 1;

macro_rules! digest_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);
        impl $name {
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0.chars().take(19).collect::<String>())
            }
        }
    };
}

digest_newtype! {
    /// Digest of the FINAL prompt text sent to the agent, after substitution and
    /// path normalisation. The template alone is not enough: a case's cmake flags are
    /// interpolated in, so two cases sharing `verify.md` can be different invocations.
    PromptDigest
}
digest_newtype! {
    RecipeDigest
}
digest_newtype! {
    /// The resolved compiler, from `rustc -vV`, host triple included: `build_ok` is a
    /// function of it and the agent iterates with `cargo build` during verify, so
    /// entries must not be shared across compiler versions or architectures.
    ToolchainId
}
digest_newtype! {
    /// The content-addressed identity of one agent invocation.
    CacheKey
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
        // It reaches a `bash -lc` command line single-quoted (`[1m]` is a bracket
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

impl ToolchainId {
    /// Refuses if `RUSTUP_TOOLCHAIN` is set, because it silently overrides
    /// `rust-toolchain.toml` — the current results tree holds 676 crates built with
    /// 1.97.1 next to 11 built with the pinned 1.94.0.
    pub fn detect() -> Result<Self> {
        anyhow::ensure!(
            std::env::var_os("RUSTUP_TOOLCHAIN").is_none(),
            "RUSTUP_TOOLCHAIN is set, which silently overrides rust-toolchain.toml. \
             Unset it (`env -u RUSTUP_TOOLCHAIN`) so the pinned compiler is used and \
             the cache key reflects it."
        );
        let out = std::process::Command::new("rustc")
            .arg("-vV")
            .output()
            .context("running `rustc -vV`")?;
        anyhow::ensure!(out.status.success(), "`rustc -vV` failed");
        let text = String::from_utf8_lossy(&out.stdout);
        let pick = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k))
                .map(str::trim)
                .unwrap_or("?")
                .to_string()
        };
        Ok(Self(format!("{} {}", pick("release:"), pick("host:"))))
    }
}

/// Rewrite machine-specific paths to stable tokens. Applied to everything that enters
/// a digest, so the same work yields the same key on another machine.
pub fn normalise(text: &str, work_root: &Path, repo_root: &Path) -> String {
    let mut out = text.to_string();
    // Longest first: the work root usually lives under the scratch base.
    let mut subs: Vec<(String, &str)> = vec![
        (work_root.to_string_lossy().into_owned(), "$WORK"),
        (repo_root.to_string_lossy().into_owned(), "$REPO"),
    ];
    if let Ok(base) = crate::workdir::base() {
        subs.push((base.to_string_lossy().into_owned(), "$WORKBASE"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        subs.push((home.to_string_lossy().into_owned(), "$HOME"));
    }
    subs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    for (from, to) in subs {
        if !from.is_empty() {
            out = out.replace(&from, to);
        }
    }
    out
}

fn feed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

pub fn prompt_digest(prompt: &str, work_root: &Path, repo_root: &Path) -> PromptDigest {
    let mut h = Sha256::new();
    feed(&mut h, b"prompt-v1");
    feed(&mut h, normalise(prompt, work_root, repo_root).as_bytes());
    PromptDigest(format!("sha256:{:x}", h.finalize()))
}

/// How the agent is invoked. An explicit struct rather than the raw argv, because argv
/// contains the scratch path — a nonce — and hashing it would make every key unique.
pub struct Recipe {
    pub max_turns: u32,
    pub permission_mode: &'static str,
    pub timeout_secs: u64,
    pub ulimit_fsize_blocks: u64,
    pub ulimit_data_kb: u64,
    pub agents_json: &'static str,
    /// Shape of the sandbox policy with paths tokenised.
    pub sandbox_shape: String,
    /// Agent-runtime environment (retries, request timeouts). Keyed because retry
    /// policy changes how a throttled session ends. See [`crate::translate::AGENT_ENV`].
    pub agent_env: &'static [(&'static str, &'static str)],
}

impl Recipe {
    /// Reads the same constants the invocation uses rather than restating them, so
    /// raising a resource cap or the turn limit changes the key instead of silently
    /// reusing output produced under the old limits.
    pub fn for_verify(paths: &crate::battery::Paths, work_root: &Path) -> Self {
        Self {
            max_turns: 1000,
            permission_mode: "bypassPermissions",
            timeout_secs: crate::verify::VERIFY_TIMEOUT_SECS,
            ulimit_fsize_blocks: crate::workdir::AGENT_FSIZE_BLOCKS,
            ulimit_data_kb: crate::workdir::AGENT_DATA_KB,
            agents_json: crate::translate::CLAUDE_PLAIN_AGENT_JSON,
            agent_env: crate::translate::AGENT_ENV,
            // The real policy, tokenised: a hand-written summary would drift, and the
            // literal directory names are machine-specific and must not enter the key.
            sandbox_shape: normalise(
                &crate::sandbox::settings_json(&paths.repo_root, work_root)
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                work_root,
                &paths.repo_root,
            ),
        }
    }

    pub fn digest(&self) -> RecipeDigest {
        let mut h = Sha256::new();
        feed(&mut h, b"recipe-v1");
        feed(&mut h, &self.max_turns.to_le_bytes());
        feed(&mut h, self.permission_mode.as_bytes());
        feed(&mut h, &self.timeout_secs.to_le_bytes());
        feed(&mut h, &self.ulimit_fsize_blocks.to_le_bytes());
        feed(&mut h, &self.ulimit_data_kb.to_le_bytes());
        feed(&mut h, self.agents_json.as_bytes());
        feed(&mut h, self.sandbox_shape.as_bytes());
        // Sorted, so a reordering of the constant is not a different recipe.
        let mut env: Vec<_> = self.agent_env.to_vec();
        env.sort_unstable();
        for (k, v) in env {
            feed(&mut h, k.as_bytes());
            feed(&mut h, v.as_bytes());
        }
        RecipeDigest(format!("sha256:{:x}", h.finalize()))
    }
}

/// Every input to a key. No `Default`, so adding a component is a compile error at
/// every construction site: a forgotten one would let two different invocations share
/// an entry, which is silent corruption rather than a visible failure.
pub struct KeyInputs<'a> {
    pub phase: &'static str,
    pub agent: &'a str,
    pub model: &'a ModelId,
    pub toolchain: &'a ToolchainId,
    pub prompt: &'a PromptDigest,
    pub recipe: &'a RecipeDigest,
    pub input_tree: &'a TreeDigest,
}

impl KeyInputs<'_> {
    pub fn key(&self) -> CacheKey {
        let mut h = Sha256::new();
        feed(&mut h, b"key-v1");
        feed(&mut h, &SCHEMA.to_le_bytes());
        for part in [
            self.phase,
            self.agent,
            self.model.as_str(),
            self.toolchain.as_str(),
            self.prompt.as_str(),
            self.recipe.as_str(),
            self.input_tree.as_str(),
        ] {
            feed(&mut h, part.as_bytes());
        }
        CacheKey(format!("{:x}", h.finalize()))
    }

    fn meta(&self, key: &CacheKey) -> serde_json::Value {
        serde_json::json!({
            "schema": SCHEMA,
            "key": key.as_str(),
            "phase": self.phase,
            "agent": self.agent,
            "model": self.model.as_str(),
            "toolchain": self.toolchain.as_str(),
            "prompt": self.prompt.as_str(),
            "recipe": self.recipe.as_str(),
            "input_tree": self.input_tree.as_str(),
            // Recorded for audit, deliberately NOT keyed and not among the fields
            // `load` re-compares: every harness commit would otherwise empty the cache,
            // including commits that cannot affect an artifact. When a change genuinely
            // alters what an artifact IS, bump SCHEMA by hand.
            "harness": crate::provenance::harness_id(),
        })
    }
}

/// How the store behaves. Chosen once, at the top level, so no call site branches.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// The default.
    ReadWrite,
    /// For sampling an agent's variance, where memoising would defeat the point.
    Bypass,
    /// Never read, but DO write, replacing any existing entry: `--force` means the
    /// stored artifact is untrustworthy, so leaving the old one would be wrong.
    Refresh,
}

/// Constructible only from a [`Sealed`], hence only with a `Completed` proof.
pub struct Produced<P: Phase> {
    pub sealed: Sealed<P>,
    /// Private: a public path field is a path escaping the module, and the shape rule
    /// in tests/architecture.rs treats it as one.
    log: PathBuf,
    pub provenance: serde_json::Value,
}

impl<P: Phase> Produced<P> {
    pub fn new(sealed: Sealed<P>, log: PathBuf, provenance: serde_json::Value) -> Self {
        Self {
            sealed,
            log,
            provenance,
        }
    }
}

pub struct Obtained<P: Phase> {
    pub sealed: Sealed<P>,
    /// True if a stored result was replayed and no agent ran. Callers must not report a
    /// replay as fresh spend: the provenance records what the ORIGINAL run cost.
    pub replayed: bool,
    pub key: CacheKey,
    /// Provenance of the producing invocation: this run on a miss, the original on a
    /// replay.
    pub provenance: serde_json::Value,
}

struct Loaded<P: Phase> {
    sealed: Sealed<P>,
    provenance: serde_json::Value,
}

pub struct Store {
    /// Never escapes this module: `entry_dir` is private, `load` hands back a
    /// [`Sealed`] (which yields no path), and `stats` hands back numbers — so no
    /// expression outside `cache.rs` can produce a path into the store, and therefore
    /// none can run a command in one.
    root: PathBuf,
    mode: Mode,
}

use crate::artifact::set_read_only;

impl Store {
    /// Open the store at `<repo>/results/.cache/`.
    ///
    /// The level is load-bearing: every tree-walker is handed
    /// `results/<dataset>/<agent>` or deeper (`test::discover_batteries` treats each
    /// child as a battery, `stage_phase_for_runtests` symlinks each grandchild), so
    /// sitting two levels above them all is what stops a cached crate being staged,
    /// built, or graded as if it were a case.
    pub fn open(repo_root: &Path, mode: Mode) -> Result<Self> {
        let root = repo_root.join("results").join(".cache");
        std::fs::create_dir_all(root.join("tmp"))
            .with_context(|| format!("creating cache at {}", root.display()))?;
        Ok(Self { root, mode })
    }

    fn entry_dir(&self, inputs: &KeyInputs<'_>, key: &CacheKey) -> PathBuf {
        self.root
            .join(SCHEMA.to_string())
            .join(inputs.phase)
            .join(inputs.agent)
            .join(key.as_str())
    }

    /// **The** execution path for an agent phase: a hit and a miss return the same type
    /// and are published identically, so the two cannot drift.
    ///
    /// `compute` returning `Ok(None)` — the agent did not complete, or this agent has no
    /// such phase — stores nothing at all, deliberately: a failure is a property of the
    /// moment (an API outage, a timeout), not of the inputs, so memoising it would make
    /// a transient failure permanent and identical on every future run.
    pub fn obtain<P: Phase>(
        &self,
        inputs: &KeyInputs<'_>,
        compute: impl FnOnce() -> Result<Option<Produced<P>>>,
    ) -> Result<Option<Obtained<P>>> {
        let key = inputs.key();
        let dir = self.entry_dir(inputs, &key);

        if self.mode == Mode::ReadWrite {
            match self.load(inputs, &key, &dir) {
                Ok(Some(Loaded { sealed, provenance })) => {
                    return Ok(Some(Obtained {
                        sealed,
                        replayed: true,
                        key,
                        provenance,
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    // A damaged entry is a miss, loudly — and quarantined rather than
                    // deleted, so the corruption can still be examined.
                    eprintln!("  cache: ignoring unusable entry {}: {e:#}", key.as_str());
                    let bad = self.root.join("quarantine").join(key.as_str());
                    if let Some(p) = bad.parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    let _ = std::fs::rename(&dir, &bad);
                }
            }
        }

        let Some(produced) = compute()? else {
            return Ok(None);
        };
        if self.mode != Mode::Bypass {
            self.store(inputs, &key, &dir, &produced)
                .with_context(|| format!("storing cache entry {}", key.as_str()))?;
        }
        Ok(Some(Obtained {
            sealed: produced.sealed,
            replayed: false,
            key,
            provenance: produced.provenance,
        }))
    }

    /// Load and VALIDATE an entry. Re-comparing every key component against the recorded
    /// meta catches key-construction bugs: if two genuinely different invocations ever
    /// compute the same key, this is a loud error naming the field that differs instead
    /// of silent cross-contamination. The stored artifact's digest is re-derived too, so
    /// a corrupted or hand-edited entry cannot be served as if it were the original.
    fn load<P: Phase>(
        &self,
        inputs: &KeyInputs<'_>,
        key: &CacheKey,
        dir: &Path,
    ) -> Result<Option<Loaded<P>>> {
        if !dir.join("meta.json").is_file() {
            return Ok(None);
        }
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json"))?)
                .context("parsing meta.json")?;
        let want = inputs.meta(key);
        for k in [
            "schema",
            "phase",
            "agent",
            "model",
            "toolchain",
            "prompt",
            "recipe",
            "input_tree",
        ] {
            anyhow::ensure!(
                meta.get(k) == want.get(k),
                "meta.json disagrees on {k}: stored {:?}, computed {:?}",
                meta.get(k),
                want.get(k)
            );
        }
        let code = dir.join("code");
        anyhow::ensure!(code.is_dir(), "entry has no code/ directory");
        let sealed = Sealed::<P>::from_cache(&code)?;
        let stored_output = meta.get("output_tree").and_then(|v| v.as_str());
        anyhow::ensure!(
            stored_output == Some(sealed.digest().as_str()),
            "code/ does not match its recorded digest (stored {stored_output:?}, \
             recomputed {:?}) — the entry has been corrupted or edited",
            sealed.digest().as_str()
        );
        let provenance = std::fs::read_to_string(dir.join("agent").join("run.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        Ok(Some(Loaded { sealed, provenance }))
    }

    /// The "already verified" skip check keys on `verified/logs/verify.log` existing, so
    /// a replay must leave the same files behind as a fresh run or the next run redoes
    /// the work.
    pub fn restore_log(&self, inputs: &KeyInputs<'_>, key: &CacheKey, dest: &Path) -> Result<()> {
        let src = self.entry_dir(inputs, key).join("agent").join("run.log");
        if !src.is_file() {
            return Ok(());
        }
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(&src, dest)
            .with_context(|| format!("restoring cached transcript to {}", dest.display()))?;
        Ok(())
    }

    /// Write an entry ATOMICALLY: stage under `tmp/`, then rename. A killed run leaves an
    /// orphan under `tmp/`, never a half-written entry that a later read would trust —
    /// killed runs have already produced truncated logs that were then scored.
    fn store<P: Phase>(
        &self,
        inputs: &KeyInputs<'_>,
        key: &CacheKey,
        dir: &Path,
        produced: &Produced<P>,
    ) -> Result<()> {
        let staging = self
            .root
            .join("tmp")
            .join(format!("{}.partial", key.as_str()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(staging.join("agent"))?;

        produced.sealed.export_into(&staging.join("code"))?;
        if produced.log.is_file() {
            std::fs::copy(&produced.log, staging.join("agent").join("run.log"))?;
        }
        std::fs::write(
            staging.join("agent").join("run.json"),
            serde_json::to_string_pretty(&produced.provenance)? + "\n",
        )?;
        // `output_tree` is the result, so it cannot be a key component; it is recorded so
        // a read can prove the artifact is the one that was written.
        let mut meta = inputs.meta(key);
        meta["output_tree"] = serde_json::json!(produced.sealed.digest().as_str());
        std::fs::write(
            staging.join("meta.json"),
            serde_json::to_string_pretty(&meta)? + "\n",
        )?;

        // Lock the CONTENTS before the rename, so no file is ever writable while visible
        // at the entry's final path — but leave the staging root itself writable, because
        // `rename(2)` on a directory must update that directory's own `..` entry and
        // fails with EACCES otherwise. The root is locked right after the move instead.
        for e in std::fs::read_dir(&staging)? {
            set_read_only(&e?.path(), true)?;
        }

        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A concurrent writer with the same key wrote identical content by construction,
        // so last-writer-wins is safe and a lock would only add a stale-lock failure mode
        // after a kill. The old entry must be made writable to be removable at all.
        if dir.exists() {
            set_read_only(dir, false)?;
            let _ = std::fs::remove_dir_all(dir);
        }
        std::fs::rename(&staging, dir)
            .with_context(|| format!("renaming {} into place", staging.display()))?;
        // In place and needing no further moves, so the root can be closed now too.
        set_read_only(dir, true)?;
        Ok(())
    }

    /// Entry count and byte size, for `harvest-tools cache stats`.
    pub fn stats(&self) -> Result<(usize, u64)> {
        fn walk(p: &Path, files: &mut u64) -> u64 {
            let mut total = 0;
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        total += walk(&path, files);
                    } else if let Ok(m) = e.metadata() {
                        *files += 1;
                        total += m.len();
                    }
                }
            }
            total
        }
        let mut entries = 0usize;
        let mut files = 0u64;
        let mut bytes = 0u64;
        let schema_dir = self.root.join(SCHEMA.to_string());
        if let Ok(phases) = std::fs::read_dir(&schema_dir) {
            for phase in phases.flatten() {
                if let Ok(agents) = std::fs::read_dir(phase.path()) {
                    for agent in agents.flatten() {
                        if let Ok(keys) = std::fs::read_dir(agent.path()) {
                            for k in keys.flatten() {
                                entries += 1;
                                bytes += walk(&k.path(), &mut files);
                            }
                        }
                    }
                }
            }
        }
        Ok((entries, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(
        m: &'a ModelId,
        t: &'a ToolchainId,
        p: &'a PromptDigest,
        r: &'a RecipeDigest,
        i: &'a TreeDigest,
    ) -> KeyInputs<'a> {
        KeyInputs {
            phase: "verify",
            agent: "claude",
            model: m,
            toolchain: t,
            prompt: p,
            recipe: r,
            input_tree: i,
        }
    }

    fn fixtures() -> (ModelId, ToolchainId, RecipeDigest) {
        (
            ModelId::new("claude-opus-5[1m]").unwrap(),
            ToolchainId("1.94.0 x86_64-unknown-linux-gnu".into()),
            Recipe {
                max_turns: 1000,
                permission_mode: "bypassPermissions",
                timeout_secs: 10_800,
                ulimit_fsize_blocks: 4 * 1024 * 1024,
                ulimit_data_kb: 6 * 1024 * 1024,
                agents_json: "{}",
                sandbox_shape: "deny=$REPO,$WORKBASE allow=$WORK".into(),
                agent_env: &[("CLAUDE_CODE_MAX_RETRIES", "20")],
            }
            .digest(),
        )
    }

    #[test]
    fn model_id_refuses_shell_metacharacters() {
        assert!(ModelId::new("claude-opus-5[1m]").is_ok());
        assert!(ModelId::new("").is_err());
        assert!(ModelId::new("x$(whoami)").is_err());
        assert!(ModelId::new("x'y").is_err());
    }

    #[test]
    fn normalise_removes_every_machine_specific_path() {
        let work = Path::new("/home/alice/.harvest/work/harvest-work-AbCdEf");
        let repo = Path::new("/home/alice/src/ACTOR");
        let text = format!("cd {} && ls {}/prompts", work.display(), repo.display());
        let n = normalise(&text, work, repo);
        assert!(!n.contains("alice"), "no username may survive: {n}");
        assert!(
            !n.contains("harvest-work-AbCdEf"),
            "no scratch name may survive: {n}"
        );
        assert!(n.contains("$WORK") && n.contains("$REPO"), "{n}");
    }

    #[test]
    fn prompt_digest_is_machine_independent() {
        // A leak here means a colleague's cache silently never hits.
        let a = prompt_digest(
            "work in /home/alice/.harvest/work/w-1 on /home/alice/src/ACTOR",
            Path::new("/home/alice/.harvest/work/w-1"),
            Path::new("/home/alice/src/ACTOR"),
        );
        let b = prompt_digest(
            "work in /local/home/bob/.harvest/work/w-2 on /local/home/bob/repo/ACTOR",
            Path::new("/local/home/bob/.harvest/work/w-2"),
            Path::new("/local/home/bob/repo/ACTOR"),
        );
        assert_eq!(
            a, b,
            "prompt digest must not depend on where anything lives"
        );
    }

    #[test]
    fn prompt_digest_changes_when_the_prompt_changes() {
        let w = Path::new("/w");
        let r = Path::new("/r");
        assert_ne!(
            prompt_digest("verify X", w, r),
            prompt_digest("verify Y", w, r)
        );
    }

    #[test]
    fn key_changes_with_every_component() {
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:i");
        let base = inputs(&m, &t, &p, &r, &i).key();

        let m2 = ModelId::new("claude-sonnet-5").unwrap();
        assert_ne!(base, inputs(&m2, &t, &p, &r, &i).key(), "model must matter");

        let t2 = ToolchainId("1.97.1 x86_64-unknown-linux-gnu".into());
        assert_ne!(
            base,
            inputs(&m, &t2, &p, &r, &i).key(),
            "toolchain must matter"
        );

        let p2 = PromptDigest("sha256:p2".into());
        assert_ne!(
            base,
            inputs(&m, &t, &p2, &r, &i).key(),
            "prompt must matter"
        );

        let i2 = TreeDigest::for_test("sha256:i2");
        assert_ne!(
            base,
            inputs(&m, &t, &p, &r, &i2).key(),
            "input tree must matter"
        );

        let mut ki = inputs(&m, &t, &p, &r, &i);
        ki.phase = "translate";
        assert_ne!(base, ki.key(), "phase must matter");
    }

    #[test]
    fn key_is_stable_for_identical_inputs() {
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:i");
        assert_eq!(
            inputs(&m, &t, &p, &r, &i).key(),
            inputs(&m, &t, &p, &r, &i).key()
        );
    }

    #[test]
    fn recipe_digest_changes_with_the_resource_caps() {
        let (_, _, base) = fixtures();
        let changed = Recipe {
            max_turns: 1000,
            permission_mode: "bypassPermissions",
            timeout_secs: 10_800,
            ulimit_fsize_blocks: 4 * 1024 * 1024,
            ulimit_data_kb: 2 * 1024 * 1024, // a different heap cap
            agents_json: "{}",
            sandbox_shape: "deny=$REPO,$WORKBASE allow=$WORK".into(),
            agent_env: &[("CLAUDE_CODE_MAX_RETRIES", "20")],
        }
        .digest();
        assert_ne!(
            base, changed,
            "a different cap can change what the agent produces"
        );
    }

    // These need a real `Sealed<Verify>`, which needs a `Completed` proof, so the
    // fixtures below walk the actual lifecycle rather than fabricating one.

    use crate::artifact::{Scratch, Sealed, Translate, Verify, WorkTree};

    struct Fixture {
        _repo: tempfile::TempDir,
        repo: PathBuf,
        case: PathBuf,
    }

    /// A results tree with one case, laid out as `Store::open` and `assemble_into`
    /// expect to find it.
    fn fixture() -> Fixture {
        let repo = tempfile::tempdir().unwrap();
        let case = repo.path().join("results/Test-Corpus/claude/P00_case");
        for (rel, body) in [
            ("Cargo.toml", "[package]\nname=\"x\""),
            ("src/lib.rs", "pub fn a() {}"),
            ("c_src/src/lib.c", "int a(void){return 0;}"),
            ("target/debug/junk", "build output"),
        ] {
            let p = case.join(crate::battery::TRANSLATED).join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let path = repo.path().to_path_buf();
        Fixture {
            _repo: repo,
            repo: path,
            case,
        }
    }

    /// `edit` stands in for the agent's change to the crate.
    fn seal_verify(case: &Path, edit: &str) -> Sealed<Verify> {
        let translated = Sealed::<Translate>::adopt(case).unwrap();
        let work: WorkTree<Verify> = translated
            .materialise_into(Scratch::new("cache-test-").unwrap())
            .unwrap();
        let c_before = work.c().digest().unwrap();
        std::fs::write(work.crate_dir().join("src/lib.rs"), edit).unwrap();
        work.scrub()
            .unwrap()
            .seal(&crate::agent_health::Completed::for_test(), &c_before)
            .unwrap()
    }

    fn produced(case: &Path, edit: &str) -> Produced<Verify> {
        Produced {
            sealed: seal_verify(case, edit),
            log: PathBuf::from("/nonexistent"),
            provenance: serde_json::json!({"agent": "claude", "duration_secs": 42}),
        }
    }

    fn key_inputs<'a>(
        m: &'a ModelId,
        t: &'a ToolchainId,
        p: &'a PromptDigest,
        r: &'a RecipeDigest,
        i: &'a TreeDigest,
    ) -> KeyInputs<'a> {
        KeyInputs {
            phase: crate::battery::VERIFIED,
            agent: "claude",
            model: m,
            toolchain: t,
            prompt: p,
            recipe: r,
            input_tree: i,
        }
    }

    /// THE load-bearing property: if the recorded digest and the one recomputed from the
    /// exported copy disagree, every hit fails validation, gets quarantined, and the cache
    /// silently never works while looking like it is enabled.
    #[test]
    fn a_digest_survives_the_round_trip_through_the_store() {
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let sealed = seal_verify(&f.case, "pub fn a() { /* fixed */ }");
        let want = sealed.digest().clone();

        let i = TreeDigest::for_test("sha256:in");
        let inputs = key_inputs(&m, &t, &p, &r, &i);
        let key = inputs.key();
        store
            .store(
                &inputs,
                &key,
                &store.entry_dir(&inputs, &key),
                &Produced {
                    sealed,
                    log: PathBuf::from("/nonexistent"),
                    provenance: serde_json::Value::Null,
                },
            )
            .unwrap();

        let loaded = store
            .load::<Verify>(&inputs, &key, &store.entry_dir(&inputs, &key))
            .expect("a freshly written entry must validate")
            .expect("and must be found");
        assert_eq!(
            loaded.sealed.digest(),
            &want,
            "the exported copy must hash to what was recorded, or no hit ever validates"
        );
    }

    #[test]
    fn obtain_computes_once_and_replays_thereafter() {
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:in");
        let inputs = key_inputs(&m, &t, &p, &r, &i);

        let mut runs = 0;
        let first = store
            .obtain(&inputs, || {
                runs += 1;
                Ok(Some(produced(&f.case, "pub fn a() { /* v1 */ }")))
            })
            .unwrap()
            .unwrap();
        assert!(!first.replayed);
        assert_eq!(runs, 1);

        let second = store
            .obtain::<Verify>(&inputs, || {
                runs += 1;
                panic!("the agent must NOT be invoked on a hit — that is the entire point");
            })
            .unwrap()
            .unwrap();
        assert!(
            second.replayed,
            "the second obtain must be served from the store"
        );
        assert_eq!(runs, 1, "compute must have run exactly once");
        assert_eq!(first.sealed.digest(), second.sealed.digest());
        assert_eq!(
            second.provenance["duration_secs"], 42,
            "a replay must carry the ORIGINAL invocation's provenance, not a blank"
        );
    }

    #[test]
    fn a_failed_invocation_is_not_stored() {
        // An API outage is a property of the moment, not of the inputs; memoising it
        // would make one bad afternoon a permanent, silent, identical failure.
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:in");
        let inputs = key_inputs(&m, &t, &p, &r, &i);

        let out = store.obtain::<Verify>(&inputs, || Ok(None)).unwrap();
        assert!(out.is_none());
        assert_eq!(
            store.stats().unwrap().0,
            0,
            "nothing may be stored for a failure"
        );

        let mut ran = false;
        store
            .obtain(&inputs, || {
                ran = true;
                Ok(Some(produced(&f.case, "pub fn a() { /* recovered */ }")))
            })
            .unwrap()
            .unwrap();
        assert!(
            ran,
            "a later run must get another chance, not a cached failure"
        );
    }

    #[test]
    fn bypass_neither_reads_nor_writes() {
        let f = fixture();
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:in");
        let inputs = key_inputs(&m, &t, &p, &r, &i);

        let rw = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        rw.obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}"))))
            .unwrap();
        assert_eq!(rw.stats().unwrap().0, 1);

        let off = Store::open(&f.repo, Mode::Bypass).unwrap();
        let mut ran = false;
        let got = off
            .obtain(&inputs, || {
                ran = true;
                Ok(Some(produced(&f.case, "pub fn a() { /* again */ }")))
            })
            .unwrap()
            .unwrap();
        assert!(ran, "bypass must not read");
        assert!(!got.replayed);
        assert_eq!(
            off.stats().unwrap().0,
            1,
            "bypass must not write, so the count is unchanged"
        );
    }

    #[test]
    fn refresh_ignores_the_stored_entry_and_replaces_it() {
        let f = fixture();
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:in");
        let inputs = key_inputs(&m, &t, &p, &r, &i);

        let rw = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let old = rw
            .obtain(&inputs, || {
                Ok(Some(produced(&f.case, "pub fn a() { /* old */ }")))
            })
            .unwrap()
            .unwrap();

        let refresh = Store::open(&f.repo, Mode::Refresh).unwrap();
        let new = refresh
            .obtain(&inputs, || {
                Ok(Some(produced(&f.case, "pub fn a() { /* new */ }")))
            })
            .unwrap()
            .unwrap();
        assert!(!new.replayed, "refresh must re-run");
        assert_ne!(old.sealed.digest(), new.sealed.digest());

        // The point of --cache refresh is that the suspect entry is GONE, not shadowed.
        let after = rw
            .obtain::<Verify>(&inputs, || panic!("must hit"))
            .unwrap()
            .unwrap();
        assert_eq!(
            after.sealed.digest(),
            new.sealed.digest(),
            "the replacement must be what is served afterwards"
        );
    }

    #[test]
    fn a_stored_entry_is_read_only_on_disk() {
        // Binds what the types cannot see: a shell-out, a stray `cargo build
        // --manifest-path`, a future refactor.
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:in");
        let inputs = key_inputs(&m, &t, &p, &r, &i);
        let key = inputs.key();
        store
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}"))))
            .unwrap();

        let code = store.entry_dir(&inputs, &key).join("code");
        assert!(
            std::fs::write(code.join("intruder.rs"), "fn main() {}").is_err(),
            "a cache entry must refuse new files — this is what stops a build in it"
        );
        assert!(
            std::fs::write(code.join("Cargo.toml"), "tampered").is_err(),
            "and must refuse edits to the files it holds"
        );
    }

    #[test]
    fn published_files_are_writable_even_though_the_store_is_not() {
        // The read-only store must not leak its permissions into the results tree:
        // scoring builds there, and a 0o444 Cargo.toml would fail with EACCES.
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:in");
        let inputs = key_inputs(&m, &t, &p, &r, &i);
        store
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}"))))
            .unwrap();

        let replay = store
            .obtain::<Verify>(&inputs, || panic!("must hit"))
            .unwrap()
            .unwrap();
        assert!(replay.replayed);
        replay.sealed.publish(&f.case).unwrap();

        let published = crate::battery::phase_dir(&f.case, crate::battery::VERIFIED);
        std::fs::write(
            published.join("src/lib.rs"),
            "pub fn a() { /* editable */ }",
        )
        .expect("a replayed artifact must be writable once published, or scoring cannot build");
    }

    #[test]
    fn a_tampered_entry_is_a_miss_rather_than_a_lie() {
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:in");
        let inputs = key_inputs(&m, &t, &p, &r, &i);
        let key = inputs.key();
        store
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}"))))
            .unwrap();

        // Defeat the read-only bit first: this is the scenario the digest check exists for.
        let dir = store.entry_dir(&inputs, &key);
        crate::artifact::set_read_only(&dir, false).unwrap();
        std::fs::write(dir.join("code/src/lib.rs"), "pub fn a() { /* smuggled */ }").unwrap();

        let mut ran = false;
        let got = store
            .obtain(&inputs, || {
                ran = true;
                Ok(Some(produced(&f.case, "pub fn a() { /* honest */ }")))
            })
            .unwrap()
            .unwrap();
        assert!(ran, "a corrupted entry must be recomputed, never served");
        assert!(!got.replayed);
        assert!(
            f.repo
                .join("results/.cache/quarantine")
                .join(key.as_str())
                .exists(),
            "and the corruption must be preserved for inspection, not destroyed"
        );
    }

    #[test]
    fn the_store_sits_outside_every_tree_walker() {
        // `discover_batteries` treats each child of `results/<dataset>/<agent>` as a
        // battery, so a store living under it could be graded as if it were a case.
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:in");
        let inputs = key_inputs(&m, &t, &p, &r, &i);
        store
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}"))))
            .unwrap();

        let per_agent = f.repo.join("results/Test-Corpus/claude");
        let walked: Vec<String> = std::fs::read_dir(&per_agent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !walked.iter().any(|n| n.contains("cache")),
            "the store must not be reachable from the per-agent results dir: {walked:?}"
        );
        assert!(
            f.repo.join("results/.cache").is_dir(),
            "it lives two levels above instead"
        );
    }

    fn recipe_with_env(env: &'static [(&'static str, &'static str)]) -> RecipeDigest {
        Recipe {
            max_turns: 1000,
            permission_mode: "bypassPermissions",
            timeout_secs: 10_800,
            ulimit_fsize_blocks: 4 * 1024 * 1024,
            ulimit_data_kb: 6 * 1024 * 1024,
            agents_json: "{}",
            sandbox_shape: "s".into(),
            agent_env: env,
        }
        .digest()
    }

    #[test]
    fn recipe_digest_covers_the_agent_runtime_env() {
        // Retry count changes how a throttled session ends, so it changes what the agent
        // produces; these once lived in a shell driver where the key could not see them.
        let twenty = recipe_with_env(&[("CLAUDE_CODE_MAX_RETRIES", "20")]);
        let one = recipe_with_env(&[("CLAUDE_CODE_MAX_RETRIES", "1")]);
        assert_ne!(twenty, one, "retry policy must change the key");

        let extra = recipe_with_env(&[("CLAUDE_CODE_MAX_RETRIES", "20"), ("API_TIMEOUT_MS", "5")]);
        assert_ne!(twenty, extra, "adding a setting must change the key");
    }

    #[test]
    fn recipe_digest_is_insensitive_to_env_ordering() {
        // If reordering the constant were a different key, a cosmetic edit would
        // silently invalidate every stored entry.
        let a = recipe_with_env(&[("A", "1"), ("B", "2")]);
        let b = recipe_with_env(&[("B", "2"), ("A", "1")]);
        assert_eq!(a, b);
    }

    #[test]
    fn harness_stamp_is_recorded_but_not_keyed() {
        // Recorded so a result is traceable to code, but NOT keyed, or every harness
        // commit would empty the cache.
        let (m, t, r) = fixtures();
        let p = PromptDigest("sha256:p".into());
        let i = TreeDigest::for_test("sha256:i");
        let ki = inputs(&m, &t, &p, &r, &i);
        let meta = ki.meta(&ki.key());
        assert!(
            meta.get("harness")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()),
            "the producing commit must be recorded: {meta:?}"
        );
        // `load` re-compares this exact list; `harness` must not be on it.
        for k in [
            "schema",
            "phase",
            "agent",
            "model",
            "toolchain",
            "prompt",
            "recipe",
            "input_tree",
        ] {
            assert!(meta.get(k).is_some(), "{k} must be recorded");
        }
    }

    #[test]
    fn toolchain_detect_refuses_an_overriding_env() {
        // The 676-vs-11 compiler split in the results tree came from this variable.
        let prev = std::env::var_os("RUSTUP_TOOLCHAIN");
        std::env::set_var("RUSTUP_TOOLCHAIN", "1.97.1");
        let got = ToolchainId::detect();
        match prev {
            Some(v) => std::env::set_var("RUSTUP_TOOLCHAIN", v),
            None => std::env::remove_var("RUSTUP_TOOLCHAIN"),
        }
        let err = format!("{:#}", got.expect_err("must refuse"));
        assert!(err.contains("RUSTUP_TOOLCHAIN"), "{err}");
    }
}
