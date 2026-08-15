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

use crate::artifact::{Access, Phase, Sealed, TreeDigest};
use crate::cli::Agent;
use crate::session::Session;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Bump to invalidate every entry, e.g. if the key composition changes.
pub const SCHEMA: u32 = 2;

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

/// Which agent produced an artifact, as the results tree, the cache and the recorded
/// provenance all spell it.
///
/// Never `format!("{agent:?}")`: `Debug` is not a serialization contract, and the
/// failure has already happened — 208 files under `results/*/codex-gpt55/` record
/// `"agent": "codex"`, a variant that no longer exists, and 418 record
/// `"agent": "oneshot"` for two different models. clap's `ValueEnum` name is the one
/// spelling that cannot drift from what `--agent` accepts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AgentKey(String);

impl AgentKey {
    /// `model` is required for the variants where one agent covers many models, since
    /// there the model is part of the identity rather than a parameter of it.
    pub fn new(agent: Agent, model: Option<&str>) -> Result<Self> {
        let name = match agent {
            Agent::OpenCode => {
                let raw = model.context(
                    "--agent opencode needs --model <provider>/<model-id>: it names the results dir",
                )?;
                crate::opencode::results_slug(&crate::opencode::parse_model(raw)?)
            }
            Agent::Oneshot => model
                .map(|m| m.rsplit_once('/').map_or(m, |(_, last)| last).to_string())
                .context(
                    "--agent oneshot needs --model <provider>/<model-id>: it names the results dir",
                )?,
            // Listed rather than `_`, so a new backend has to decide whether its model
            // is part of its identity instead of defaulting to "no".
            Agent::Kiro
            | Agent::Claude
            | Agent::ClaudeCombined
            | Agent::ClaudeMinimal
            | Agent::ClaudeNoIter
            | Agent::ClaudeNoFeatures
            | Agent::ClaudeNoSubtask
            | Agent::ClaudeCrossPrompt
            | Agent::CodexGpt55
            | Agent::CodexGpt54
            | Agent::C2rust
            | Agent::Laertes
            | Agent::C2SaferRust
            | Agent::SmartC2Rust
            | Agent::Kimi => crate::cli::cli_name(agent)?,
        };
        // It names a directory under `results/` and under the store, and for the two
        // model-derived variants it comes from `--model`.
        anyhow::ensure!(
            !name.is_empty()
                && name != "."
                && name != ".."
                && !name.contains('/')
                && !name.starts_with('-'),
            "agent key must be a single path component, got {name:?}"
        );
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The agent CLI build that will run, from `<program> --version`.
///
/// Keyed because the CLIs auto-update through a shim: two claude builds (2.1.231.653
/// and 2.1.232.657) are installed under `~/.toolbox/tools/claude-code/` on this
/// machine, so without this one entry spans two binaries. It cannot be read from the
/// transcript, which reports it only after the money is spent.
///
/// Probed per case rather than memoised, so an upgrade part-way through a sweep is
/// caught before the next case is attributed to the old build.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CliVersion(String);

/// States the build when the CLI is not installed — replaying a stored cache without
/// one. The build must still be NAMED, so the key stays as specific as before, and
/// [`crate::translate::assert_pins_honoured`] still checks it against what the
/// transcript reports.
pub const ENV_CLI_VERSION: &str = "HARVEST_CLI_VERSION";

impl CliVersion {
    pub fn probe(program: &str) -> Result<Self> {
        Self::resolve(program, std::env::var(ENV_CLI_VERSION).ok())
    }

    /// The override is a parameter, not a read: mutating process env in a test races
    /// across test threads (see `crate::workdir::resolve_from`).
    fn resolve(program: &str, stated: Option<String>) -> Result<Self> {
        if let Some(v) = stated {
            let v = v.trim();
            anyhow::ensure!(!v.is_empty(), "{ENV_CLI_VERSION} is set but names no build");
            return Ok(Self(v.to_string()));
        }
        let out = std::process::Command::new(program)
            .arg("--version")
            .output()
            .with_context(|| format!("running `{program} --version`"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next().unwrap_or_default().trim();
        anyhow::ensure!(
            out.status.success() && !line.is_empty(),
            "`{program} --version` reported no build ({}), so the cache key cannot name \
             the one that would run and this run would be indistinguishable from a run \
             made by another. Install {program}, or set {ENV_CLI_VERSION} to replay a \
             stored cache without it.",
            out.status
        );
        Ok(Self(line.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the version the CLI reported in its own transcript is the one probed.
    /// A containment test because `--version` prints the number plus a product name.
    pub fn covers(&self, reported: &str) -> bool {
        self.0.contains(reported)
    }
}

impl ToolchainId {
    /// Refuses if `RUSTUP_TOOLCHAIN` is set, because it silently overrides
    /// `rust-toolchain.toml` — the current results tree holds 676 crates built with
    /// 1.97.1 next to 11 built with the pinned 1.94.0.
    pub fn detect() -> Result<Self> {
        if let Some(value) = std::env::var_os("RUSTUP_TOOLCHAIN") {
            return Err(crate::refusal::Refusal::ToolchainOverridden {
                value: value.to_string_lossy().into_owned(),
            }
            .into());
        }
        let out = std::process::Command::new("rustc")
            .arg("-vV")
            .output()
            .context("running `rustc -vV`")?;
        anyhow::ensure!(out.status.success(), "`rustc -vV` failed");
        Ok(Self(parse_rustc_vv(&String::from_utf8_lossy(&out.stdout))?))
    }
}

/// Refuses output it cannot parse rather than substituting a placeholder, which would key
/// two unidentifiable compilers alike — the failure this whole type exists to prevent.
fn parse_rustc_vv(text: &str) -> Result<String> {
    let pick = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k))
            .map(str::trim)
            .with_context(|| format!("`rustc -vV` printed no `{k}` line"))
    };
    Ok(format!("{} {}", pick("release:")?, pick("host:")?))
}

/// Rewrite machine-specific paths to stable tokens. Applied to everything that enters
/// a digest, so the same work yields the same key on another machine.
pub fn normalise(text: &str, work_root: &Path, repo_root: &Path) -> String {
    let mut out = text.to_string();
    // `to_str`, never `to_string_lossy`: lossy mapping sends every invalid byte to
    // U+FFFD, so two different roots can produce the same substitution string and two
    // different prompts the same digest — a false cache *hit*, the one failure mode
    // this key exists to prevent. Skipping a non-UTF-8 root instead leaves the literal
    // path in the normalised text, which can only cost a miss.
    let mut roots: Vec<(PathBuf, &str)> =
        vec![(work_root.to_path_buf(), "$WORK"), (repo_root.to_path_buf(), "$REPO")];
    if let Ok(base) = crate::workdir::base() {
        roots.push((base, "$WORKBASE"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        roots.push((PathBuf::from(home), "$HOME"));
    }
    let mut subs: Vec<(String, &str)> = roots
        .iter()
        .filter_map(|(p, token)| p.to_str().map(|s| (s.to_owned(), *token)))
        .collect();
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

/// How the agent was invoked, per backend.
///
/// Borrows the [`Session`] that renders the actual command instead of restating its
/// values, so raising a cap or the turn limit changes the key rather than silently
/// reusing output produced under the old limits — and so the recipe cannot describe a
/// run that did not happen, which it did for every backend but claude.
///
/// Not the raw argv, which contains the scratch path — a nonce that would make every
/// key unique.
pub struct Recipe<'a> {
    session: &'a Session,
    /// The sandbox policy actually applied, paths tokenised. `None` where the backend
    /// applies none: kiro-cli runs `--trust-all-tools` with no policy file at all.
    policy_shape: Option<String>,
}

impl<'a> Recipe<'a> {
    pub fn new(session: &'a Session, policy_shape: Option<String>) -> Result<Self> {
        session.assert_declares()?;
        Ok(Self { session, policy_shape })
    }

    /// Opens with an EXHAUSTIVE pattern, deliberately. Adding a field to `Recipe` then
    /// fails to compile here (E0027) instead of silently leaving the key unchanged, and
    /// binding one without feeding it is an `unused variable` error under
    /// `warnings = "deny"`. Do not reintroduce `..`.
    pub fn digest(&self) -> RecipeDigest {
        let Self { session, policy_shape } = self;
        let mut h = Sha256::new();
        feed(&mut h, b"recipe-v2");
        feed(&mut h, session.shape().as_bytes());
        // Tagged, so "no policy" cannot hash the same as an empty one.
        match policy_shape {
            Some(p) => {
                feed(&mut h, b"policy");
                feed(&mut h, p.as_bytes());
            }
            None => feed(&mut h, b"no-policy"),
        }
        RecipeDigest(format!("sha256:{:x}", h.finalize()))
    }
}

/// Every input to a key. No `Default`, so adding a component is a compile error at
/// every construction site: a forgotten one would let two different invocations share
/// an entry, which is silent corruption rather than a visible failure.
pub struct KeyInputs<'a> {
    pub phase: &'static str,
    pub agent: &'a AgentKey,
    /// The model the backend named in this run will actually be asked for — resolved
    /// per backend, because a key naming claude's model for an opencode run makes two
    /// sweeps at different `--model` values share an entry.
    pub model: &'a ModelId,
    pub cli: &'a CliVersion,
    pub toolchain: &'a ToolchainId,
    pub prompt: &'a PromptDigest,
    pub recipe: &'a RecipeDigest,
    pub input_tree: &'a TreeDigest,
}

impl KeyInputs<'_> {
    /// Exhaustive pattern, for the reason on `Recipe::digest`: a component added to
    /// `KeyInputs` and forgotten here would let two different invocations share an entry.
    pub fn key(&self) -> CacheKey {
        let Self { phase, agent, model, cli, toolchain, prompt, recipe, input_tree } = self;
        let mut h = Sha256::new();
        feed(&mut h, b"key-v1");
        feed(&mut h, &SCHEMA.to_le_bytes());
        for part in [
            *phase,
            agent.as_str(),
            model.as_str(),
            cli.as_str(),
            toolchain.as_str(),
            prompt.as_str(),
            recipe.as_str(),
            input_tree.as_str(),
        ] {
            feed(&mut h, part.as_bytes());
        }
        CacheKey(format!("{:x}", h.finalize()))
    }

    /// The fields `load` re-compares, derived from this one function rather than
    /// hand-listed a second and third time. A component added to `key()` but not here
    /// silently removes the backstop that turns a forgotten field into a loud
    /// "meta.json disagrees on X".
    pub(crate) const VALIDATED: &'static [&'static str] = &[
        "schema", "phase", "agent", "model", "cli", "toolchain", "prompt", "recipe",
        "input_tree",
    ];

    fn meta(&self, key: &CacheKey) -> serde_json::Value {
        let Self { phase, agent, model, cli, toolchain, prompt, recipe, input_tree } = self;
        serde_json::json!({
            "schema": SCHEMA,
            "key": key.as_str(),
            "phase": phase,
            "agent": agent.as_str(),
            "model": model.as_str(),
            "cli": cli.as_str(),
            "toolchain": toolchain.as_str(),
            "prompt": prompt.as_str(),
            "recipe": recipe.as_str(),
            "input_tree": input_tree.as_str(),
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
    /// stored artifact is untrustworthy, so leaving the old one would be wrong. The
    /// replaced entry is quarantined, not deleted — it is the reason the operator
    /// reached for `refresh`, so it is evidence.
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
        Self { sealed, log, provenance }
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
            .join(inputs.agent.as_str())
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

        match self.mode {
            Mode::ReadWrite => match self.load(inputs, &key, &dir) {
                Ok(Some(Loaded { sealed, provenance })) => {
                    return Ok(Some(Obtained { sealed, replayed: true, key, provenance }));
                }
                Ok(None) => {}
                Err(e) => {
                    // A damaged entry is a miss, loudly — and quarantined rather than
                    // deleted, so the corruption can still be examined.
                    eprintln!("  cache: ignoring unusable entry {}: {e:#}", key.as_str());
                    self.quarantine(&key, &dir)?;
                }
            },
            Mode::Refresh => self.quarantine(&key, &dir)?,
            Mode::Bypass => {}
        }

        let Some(produced) = compute()? else { return Ok(None) };
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

    /// Move a suspect entry aside, keeping it readable for a post-mortem. Reports failure
    /// rather than swallowing it: written as `let _ = rename(..)` this could not have
    /// succeeded once — the entry is `0o555`, and a rename ACROSS parents needs write
    /// permission on the moved directory itself, to update its `..`.
    fn quarantine(&self, key: &CacheKey, dir: &Path) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        let holding = self.root.join("quarantine");
        std::fs::create_dir_all(&holding)
            .with_context(|| format!("creating {}", holding.display()))?;
        // A second copy of the same key must not land on the first: `rename` onto a
        // non-empty directory fails, and the earlier copy is evidence too.
        let dest = (1..1000)
            .map(|n| match n {
                1 => holding.join(key.as_str()),
                n => holding.join(format!("{}.{n}", key.as_str())),
            })
            .find(|p| !p.exists())
            .with_context(|| {
                format!("{} already holds 999 copies of {}", holding.display(), key.as_str())
            })?;
        set_read_only(dir, Access::Writable)?;
        std::fs::rename(dir, &dest)
            .with_context(|| format!("quarantining {} to {}", dir.display(), dest.display()))?;
        eprintln!("  cache: quarantined the entry at {}", dest.display());
        set_read_only(&dest, Access::ReadOnly)
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
        for k in KeyInputs::VALIDATED {
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
        // `store` always writes this, so a missing or unreadable one is a damaged entry —
        // and defaulting to `Null` publishes a replay whose metrics.json has no duration or
        // cost at all, which reads as a free run rather than as a broken store.
        let provenance = serde_json::from_str(
            &std::fs::read_to_string(dir.join("agent").join("run.json"))
                .context("reading agent/run.json")?,
        )
        .context("parsing agent/run.json")?;
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
        // `fs::copy` carries the source's mode and the source is `0o444` in the store: the
        // second unlock keeps the results tree writable for the next run's tee, and the
        // first stops this failing EACCES on the second replay of the same case.
        if dest.exists() {
            set_read_only(dest, Access::Writable)?;
        }
        std::fs::copy(&src, dest)
            .with_context(|| format!("restoring cached transcript to {}", dest.display()))?;
        set_read_only(dest, Access::Writable)
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
        let staging = self.root.join("tmp").join(format!("{}.partial", key.as_str()));
        if staging.exists() {
            // A run killed between the lock below and the rename leaves a `0o555` orphan,
            // and entries inside a `0o555` directory cannot be removed: ignoring that
            // poisons this key for good, every later write failing EACCES on `code/`.
            set_read_only(&staging, Access::Writable)?;
            std::fs::remove_dir_all(&staging)
                .with_context(|| format!("clearing stale staging dir {}", staging.display()))?;
        }
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
        std::fs::write(staging.join("meta.json"), serde_json::to_string_pretty(&meta)? + "\n")?;

        // Lock the CONTENTS before the rename, so no file is ever writable while visible
        // at the entry's final path — but leave the staging root itself writable, because
        // `rename(2)` on a directory must update that directory's own `..` entry and
        // fails with EACCES otherwise. The root is locked right after the move instead.
        for e in std::fs::read_dir(&staging)? {
            set_read_only(&e?.path(), Access::ReadOnly)?;
        }

        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A concurrent writer with the same key wrote identical content by construction,
        // so last-writer-wins is safe and a lock would only add a stale-lock failure mode
        // after a kill. The old entry must be made writable to be removable at all.
        if dir.exists() {
            set_read_only(dir, Access::Writable)?;
            if let Err(e) = std::fs::remove_dir_all(dir) {
                // NotFound is such a writer getting there first; anything else would
                // resurface below as a puzzling ENOTEMPTY from the rename.
                anyhow::ensure!(
                    e.kind() == std::io::ErrorKind::NotFound,
                    "removing the entry being replaced at {}: {e}",
                    dir.display()
                );
            }
        }
        std::fs::rename(&staging, dir)
            .with_context(|| format!("renaming {} into place", staging.display()))?;
        // In place and needing no further moves, so the root can be closed now too.
        set_read_only(dir, Access::ReadOnly)?;
        Ok(())
    }

    /// Entry count and byte size of the servable entries, for `harvest-tools cache stats`:
    /// quarantined and half-written trees sit outside the schema dir and are not counted.
    /// A read failure is reported, never counted as zero — an unreadable store is not an
    /// empty one.
    pub fn stats(&self) -> Result<(usize, u64)> {
        /// A missing directory is an empty store, not a fault: nothing has been stored yet.
        fn children(p: &Path) -> Result<Vec<PathBuf>> {
            let rd = match std::fs::read_dir(p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                other => other.with_context(|| format!("reading {}", p.display()))?,
            };
            rd.map(|e| Ok(e?.path()))
                .collect::<Result<Vec<_>>>()
                .with_context(|| format!("reading {}", p.display()))
        }
        fn bytes(p: &Path) -> Result<u64> {
            let mut total = 0;
            for child in children(p)? {
                // `symlink_metadata`: a link to a directory must not send this into a loop.
                let meta = std::fs::symlink_metadata(&child)
                    .with_context(|| format!("reading {}", child.display()))?;
                total += if meta.is_dir() { bytes(&child)? } else { meta.len() };
            }
            Ok(total)
        }
        let mut entries = 0usize;
        let mut total = 0u64;
        for phase in children(&self.root.join(SCHEMA.to_string()))? {
            for agent in children(&phase)? {
                for key in children(&agent)? {
                    entries += 1;
                    total += bytes(&key)?;
                }
            }
        }
        Ok((entries, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(session: &Session, policy: Option<&str>) -> RecipeDigest {
        Recipe::new(session, policy.map(str::to_string)).unwrap().digest()
    }

    /// Owns every key component, so a test can vary exactly one and borrow a
    /// [`KeyInputs`] from the result.
    struct Inputs {
        phase: &'static str,
        agent: AgentKey,
        model: ModelId,
        cli: CliVersion,
        toolchain: ToolchainId,
        prompt: PromptDigest,
        recipe: RecipeDigest,
        tree: TreeDigest,
    }

    impl Inputs {
        fn new() -> Self {
            Self {
                phase: crate::battery::VERIFIED,
                agent: AgentKey::new(Agent::Claude, None).unwrap(),
                model: ModelId::new("claude-opus-5[1m]").unwrap(),
                cli: CliVersion("2.1.231.653 (Claude Code)".into()),
                toolchain: ToolchainId("1.94.0 x86_64-unknown-linux-gnu".into()),
                prompt: PromptDigest("sha256:p".into()),
                recipe: recipe(&Session::claude(10_800), Some("deny=$REPO allow=$WORK")),
                tree: TreeDigest::for_test("sha256:in"),
            }
        }

        fn key_inputs(&self) -> KeyInputs<'_> {
            KeyInputs {
                phase: self.phase,
                agent: &self.agent,
                model: &self.model,
                cli: &self.cli,
                toolchain: &self.toolchain,
                prompt: &self.prompt,
                recipe: &self.recipe,
                input_tree: &self.tree,
            }
        }

        fn key(&self) -> CacheKey {
            self.key_inputs().key()
        }
    }

    #[test]
    fn the_agent_key_is_the_cli_spelling_not_the_debug_spelling() {
        // `format!("{agent:?}").to_lowercase()` produced "claudenosubtask" and
        // "codexgpt55" — and, for the renamed variant, "codex", which no `--agent`
        // value has spelled since. These are also the published results dir names.
        for (agent, want) in [
            (Agent::Kiro, "kiro"),
            (Agent::Claude, "claude"),
            (Agent::ClaudeCombined, "claude-combined"),
            (Agent::ClaudeMinimal, "claude-minimal"),
            (Agent::ClaudeNoIter, "claude-no-iter"),
            (Agent::ClaudeNoFeatures, "claude-no-features"),
            (Agent::ClaudeNoSubtask, "claude-no-subtask"),
            (Agent::ClaudeCrossPrompt, "claude-cross-prompt"),
            (Agent::CodexGpt55, "codex-gpt55"),
            (Agent::CodexGpt54, "codex-gpt54"),
            (Agent::C2rust, "c2rust"),
            (Agent::Laertes, "laertes"),
            (Agent::C2SaferRust, "c2saferrust"),
            (Agent::SmartC2Rust, "smartc2rust"),
            (Agent::Kimi, "kimi"),
        ] {
            assert_eq!(AgentKey::new(agent, None).unwrap().as_str(), want);
        }
    }

    #[test]
    fn the_agent_key_distinguishes_models_where_one_variant_covers_many() {
        // 418 files record `"agent": "oneshot"` for two different models, and every
        // opencode run would record plain "opencode".
        let gpt = AgentKey::new(Agent::Oneshot, Some("openai/gpt-5.4")).unwrap();
        let gemini = AgentKey::new(Agent::Oneshot, Some("google/gemini-3.1-pro-preview")).unwrap();
        assert_eq!(gpt.as_str(), "gpt-5.4");
        assert_eq!(gemini.as_str(), "gemini-3.1-pro-preview");
        assert_ne!(gpt, gemini);

        let oc = AgentKey::new(Agent::OpenCode, Some("amazon-bedrock/us.anthropic.claude-sonnet-5"));
        assert_eq!(oc.unwrap().as_str(), "opencode-claude-sonnet-5");
    }

    #[test]
    fn the_agent_key_refuses_a_model_that_would_name_another_directory() {
        // It is a directory component under `results/` and under the store, and for
        // these two variants it comes straight from `--model`.
        assert!(AgentKey::new(Agent::Oneshot, Some("openai/..")).is_err());
        assert!(AgentKey::new(Agent::Oneshot, Some("openai/")).is_err());
        assert!(AgentKey::new(Agent::Oneshot, None).is_err());
        assert!(AgentKey::new(Agent::OpenCode, None).is_err());
    }

    /// A stand-in for an agent CLI, so the probe is exercised without running one.
    fn fake_cli(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn a_cli_version_must_be_observed_rather_than_assumed() {
        let d = tempfile::tempdir().unwrap();
        let ok = fake_cli(d.path(), "ok", "echo '2.1.232.657 (Claude Code)'; echo 'trailing note'");
        assert_eq!(
            CliVersion::probe(&ok).unwrap().as_str(),
            "2.1.232.657 (Claude Code)",
            "the first line only, so a changelog banner is not the version"
        );

        assert!(CliVersion::probe("harvest-no-such-program").is_err(), "a missing CLI must refuse");
        let broken = fake_cli(d.path(), "broken", "echo 1.0; exit 3");
        assert!(CliVersion::probe(&broken).is_err(), "a failing probe must refuse");
        let mute = fake_cli(d.path(), "mute", "exit 0");
        assert!(CliVersion::probe(&mute).is_err(), "an unreportable version must refuse");
    }

    #[test]
    fn a_stated_cli_version_replaces_the_probe_but_not_the_naming() {
        // For replaying a stored cache without the CLI installed. It must still name one
        // build, so a hit is as specific as it was when the entry was written.
        let stated = CliVersion::resolve("harvest-no-such-program", Some(" 2.1.231.653 ".into()))
            .expect("a stated build needs no CLI");
        assert_eq!(stated.as_str(), "2.1.231.653");
        assert!(
            CliVersion::resolve("harvest-no-such-program", Some("  ".into())).is_err(),
            "a blank statement is not a build name"
        );
    }

    #[test]
    fn a_cli_version_covers_the_build_the_transcript_reports() {
        let probed = CliVersion("2.1.232.657 (Claude Code)".into());
        assert!(probed.covers("2.1.232.657"));
        assert!(!probed.covers("2.1.231.653"), "the other build on this machine");
    }

    #[test]
    fn model_id_refuses_shell_metacharacters() {
        assert!(ModelId::new("claude-opus-5[1m]").is_ok());
        assert!(ModelId::new("").is_err());
        assert!(ModelId::new("x$(whoami)").is_err());
        assert!(ModelId::new("x'y").is_err());
    }

    #[test]
    fn toolchain_id_refuses_output_it_cannot_parse() {
        // A placeholder would give two unidentifiable compilers the same key.
        let real = "rustc 1.94.0\nhost: x86_64-unknown-linux-gnu\nrelease: 1.94.0\n";
        assert_eq!(parse_rustc_vv(real).unwrap(), "1.94.0 x86_64-unknown-linux-gnu");
        let err = format!("{:#}", parse_rustc_vv("rustc 1.94.0\n").expect_err("must refuse"));
        assert!(err.contains("release:"), "{err}");
    }

    #[test]
    fn normalise_removes_every_machine_specific_path() {
        let work = Path::new("/home/alice/.harvest/work/harvest-work-AbCdEf");
        let repo = Path::new("/home/alice/src/ACTOR");
        let text = format!("cd {} && ls {}/prompts", work.display(), repo.display());
        let n = normalise(&text, work, repo);
        assert!(!n.contains("alice"), "no username may survive: {n}");
        assert!(!n.contains("harvest-work-AbCdEf"), "no scratch name may survive: {n}");
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
        assert_eq!(a, b, "prompt digest must not depend on where anything lives");
    }

    #[test]
    fn prompt_digest_changes_when_the_prompt_changes() {
        let w = Path::new("/w");
        let r = Path::new("/r");
        assert_ne!(prompt_digest("verify X", w, r), prompt_digest("verify Y", w, r));
    }

    #[test]
    fn key_changes_with_every_component() {
        let base = Inputs::new().key();

        let mut v = Inputs::new();
        v.model = ModelId::new("claude-sonnet-5").unwrap();
        assert_ne!(base, v.key(), "model must matter");

        let mut v = Inputs::new();
        v.agent = AgentKey::new(Agent::Kiro, None).unwrap();
        assert_ne!(base, v.key(), "agent must matter");

        let mut v = Inputs::new();
        v.cli = CliVersion("2.1.232.657 (Claude Code)".into());
        assert_ne!(base, v.key(), "the CLI build must matter");

        let mut v = Inputs::new();
        v.toolchain = ToolchainId("1.97.1 x86_64-unknown-linux-gnu".into());
        assert_ne!(base, v.key(), "toolchain must matter");

        let mut v = Inputs::new();
        v.prompt = PromptDigest("sha256:p2".into());
        assert_ne!(base, v.key(), "prompt must matter");

        let mut v = Inputs::new();
        v.recipe = recipe(&Session::kiro(2_700), None);
        assert_ne!(base, v.key(), "recipe must matter");

        let mut v = Inputs::new();
        v.tree = TreeDigest::for_test("sha256:i2");
        assert_ne!(base, v.key(), "input tree must matter");

        let mut v = Inputs::new();
        v.phase = "translate";
        assert_ne!(base, v.key(), "phase must matter");
    }

    #[test]
    fn key_is_stable_for_identical_inputs() {
        assert_eq!(Inputs::new().key(), Inputs::new().key());
    }

    #[test]
    fn two_opencode_sweeps_at_different_models_do_not_share_an_entry() {
        // THE defect: the key took claude's model for every backend, so two opencode
        // sweeps computed an identical key and the second published the first's
        // artifact, stamped `replayed: true`, with every log and check agreeing.
        let mut sonnet = Inputs::new();
        sonnet.agent = AgentKey::new(Agent::OpenCode, Some("amazon-bedrock/us.anthropic.claude-sonnet-5")).unwrap();
        sonnet.model = ModelId::new("amazon-bedrock/us.anthropic.claude-sonnet-5").unwrap();

        let mut gpt = Inputs::new();
        gpt.agent = AgentKey::new(Agent::OpenCode, Some("amazon-bedrock/openai.gpt-5.5")).unwrap();
        gpt.model = ModelId::new("amazon-bedrock/openai.gpt-5.5").unwrap();

        assert_ne!(sonnet.key(), gpt.key());
    }

    #[test]
    fn recipe_digest_changes_with_the_resource_caps() {
        // A different cap can change what the agent produces. The `ulimit` caps are
        // constants of the session, so `Session::shape` is what proves the digest reads
        // them (session.rs); the wall clock is the one this layer can vary.
        let base = recipe(&Session::claude(10_800), None);
        assert_ne!(base, recipe(&Session::claude(2_700), None));
    }

    #[test]
    fn a_recipe_names_the_backend_that_actually_ran() {
        // It used to record claude's 10800s / 1000-turn / bypassPermissions invocation
        // for kiro, which really runs `timeout 2700` with no turn limit.
        let claude = recipe(&Session::claude(10_800), Some("deny=$REPO allow=$WORK"));
        let kiro = recipe(&Session::kiro(2_700), None);
        let oc = recipe(&Session::opencode(crate::opencode::Phase::Verify, 10_800), Some("allow=$WORK"));
        assert_ne!(claude, kiro);
        assert_ne!(claude, oc);
        assert_ne!(kiro, oc);
    }

    #[test]
    fn recipe_digest_covers_the_sandbox_policy() {
        let narrow = recipe(&Session::claude(10_800), Some("allow=$WORK"));
        let wide = recipe(&Session::claude(10_800), Some("allow=$WORK,$REPO"));
        assert_ne!(narrow, wide, "a wider sandbox can change what the agent produces");
        assert_ne!(
            recipe(&Session::claude(10_800), None),
            recipe(&Session::claude(10_800), Some("")),
            "no policy must not hash the same as an empty one"
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
        Fixture { _repo: repo, repo: path, case }
    }

    /// `edit` stands in for the agent's change to the crate.
    fn seal_verify(case: &Path, edit: &str) -> Sealed<Verify> {
        let translated = Sealed::<Translate>::adopt(case).unwrap();
        let work: WorkTree<Verify> =
            translated.materialise_into(Scratch::new("cache-test-").unwrap()).unwrap();
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

    /// THE load-bearing property: if the recorded digest and the one recomputed from the
    /// exported copy disagree, every hit fails validation, gets quarantined, and the cache
    /// silently never works while looking like it is enabled.
    #[test]
    fn a_digest_survives_the_round_trip_through_the_store() {
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let sealed = seal_verify(&f.case, "pub fn a() { /* fixed */ }");
        let want = sealed.digest().clone();

        let owned = Inputs::new();
        let inputs = owned.key_inputs();
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
        let owned = Inputs::new();
        let inputs = owned.key_inputs();

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
        assert!(second.replayed, "the second obtain must be served from the store");
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
        let owned = Inputs::new();
        let inputs = owned.key_inputs();

        let out = store.obtain::<Verify>(&inputs, || Ok(None)).unwrap();
        assert!(out.is_none());
        assert_eq!(store.stats().unwrap().0, 0, "nothing may be stored for a failure");

        let mut ran = false;
        store
            .obtain(&inputs, || {
                ran = true;
                Ok(Some(produced(&f.case, "pub fn a() { /* recovered */ }")))
            })
            .unwrap()
            .unwrap();
        assert!(ran, "a later run must get another chance, not a cached failure");
    }

    #[test]
    fn bypass_neither_reads_nor_writes() {
        let f = fixture();
        let owned = Inputs::new();
        let inputs = owned.key_inputs();

        let rw = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        rw.obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}")))).unwrap();
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
        assert_eq!(off.stats().unwrap().0, 1, "bypass must not write, so the count is unchanged");
    }

    #[test]
    fn refresh_ignores_the_stored_entry_and_replaces_it() {
        let f = fixture();
        let owned = Inputs::new();
        let inputs = owned.key_inputs();

        let rw = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let old = rw.obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() { /* old */ }"))))
            .unwrap().unwrap();

        let refresh = Store::open(&f.repo, Mode::Refresh).unwrap();
        let new = refresh
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() { /* new */ }"))))
            .unwrap()
            .unwrap();
        assert!(!new.replayed, "refresh must re-run");
        assert_ne!(old.sealed.digest(), new.sealed.digest());

        // The point of --cache refresh is that the suspect entry is GONE, not shadowed.
        let after = rw.obtain::<Verify>(&inputs, || panic!("must hit")).unwrap().unwrap();
        assert_eq!(
            after.sealed.digest(),
            new.sealed.digest(),
            "the replacement must be what is served afterwards"
        );
    }

    #[test]
    fn refresh_keeps_the_entry_it_replaced() {
        // An operator reaches for `--cache refresh` because they doubt the stored artifact,
        // which makes it the evidence: deleting it destroys the only copy of the thing
        // being disputed.
        let f = fixture();
        // Ported onto this branch's `Inputs` builder, which replaced the free
        // fixtures()/key_inputs() helpers; `held` must outlive `inputs`, which borrows it.
        let held = Inputs::new();
        let inputs = held.key_inputs();
        let key = inputs.key();

        let rw = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let old = rw
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() { /* suspect */ }"))))
            .unwrap()
            .unwrap();

        Store::open(&f.repo, Mode::Refresh)
            .unwrap()
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() { /* new */ }"))))
            .unwrap()
            .unwrap();

        let kept = f.repo.join("results/.cache/quarantine").join(key.as_str());
        assert!(kept.is_dir(), "the replaced entry must be kept for comparison: {kept:?}");
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(kept.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(
            meta["output_tree"].as_str(),
            Some(old.sealed.digest().as_str()),
            "and it must be the entry that was replaced, not the replacement"
        );

        // A second refresh must not land on the first copy, nor lose it.
        Store::open(&f.repo, Mode::Refresh)
            .unwrap()
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() { /* newer */ }"))))
            .unwrap()
            .unwrap();
        assert!(kept.is_dir(), "the first copy must survive a second refresh");
        assert!(
            f.repo.join("results/.cache/quarantine").join(format!("{}.2", key.as_str())).is_dir(),
            "and the second must be kept beside it"
        );
    }

    #[test]
    fn a_quarantined_entry_is_read_only_and_uncounted() {
        let f = fixture();
        // Ported onto this branch's `Inputs` builder, which replaced the free
        // fixtures()/key_inputs() helpers; `held` must outlive `inputs`, which borrows it.
        let held = Inputs::new();
        let inputs = held.key_inputs();
        let key = inputs.key();

        let rw = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        rw.obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}")))).unwrap();
        Store::open(&f.repo, Mode::Refresh)
            .unwrap()
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() { /* new */ }"))))
            .unwrap();

        let kept = f.repo.join("results/.cache/quarantine").join(key.as_str());
        // Without this the write below fails merely because nothing is there, which is how
        // the quarantine came to be asserted by a test that never exercised it.
        assert!(kept.join("code/Cargo.toml").is_file(), "nothing was quarantined: {kept:?}");
        assert!(
            std::fs::write(kept.join("code/Cargo.toml"), "tampered").is_err(),
            "evidence must not be editable in place"
        );
        assert_eq!(
            rw.stats().unwrap().0,
            1,
            "and must not be counted as a servable entry, or `cache stats` overstates the store"
        );
    }

    #[test]
    fn a_restored_transcript_is_writable_and_survives_a_second_replay() {
        // `fs::copy` carries the store's 0o444 across, which left a read-only verify.log in
        // the results tree and then failed EACCES on the next replay of the same case.
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        // Ported onto this branch's `Inputs` builder, which replaced the free
        // fixtures()/key_inputs() helpers; `held` must outlive `inputs`, which borrows it.
        let held = Inputs::new();
        let inputs = held.key_inputs();
        let key = inputs.key();

        let log = f.repo.join("live-run.log");
        std::fs::write(&log, "the transcript the invocation teed\n").unwrap();
        store
            .obtain(&inputs, || {
                Ok(Some(Produced {
                    sealed: seal_verify(&f.case, "pub fn a() {}"),
                    log: log.clone(),
                    provenance: serde_json::Value::Null,
                }))
            })
            .unwrap();

        let dest = crate::battery::phase_dir(&f.case, crate::battery::VERIFIED)
            .join("logs")
            .join("verify.log");
        store.restore_log(&inputs, &key, &dest).unwrap();
        store
            .restore_log(&inputs, &key, &dest)
            .expect("a second replay must not trip over the log the first one restored");
        std::fs::write(&dest, "the next run tees over it")
            .expect("a restored transcript must stay writable, or the next run cannot tee to it");
    }

    #[test]
    fn an_entry_without_provenance_is_a_miss_rather_than_a_free_run() {
        // A replay carrying `null` provenance publishes a metrics.json with no duration and
        // no cost, which reads as an agent invocation that cost nothing.
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        // Ported onto this branch's `Inputs` builder, which replaced the free
        // fixtures()/key_inputs() helpers; `held` must outlive `inputs`, which borrows it.
        let held = Inputs::new();
        let inputs = held.key_inputs();
        let key = inputs.key();
        store.obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}")))).unwrap();

        let dir = store.entry_dir(&inputs, &key);
        crate::artifact::set_read_only(&dir, Access::Writable).unwrap();
        std::fs::remove_file(dir.join("agent/run.json")).unwrap();
        crate::artifact::set_read_only(&dir, Access::ReadOnly).unwrap();

        let mut ran = false;
        let got = store
            .obtain(&inputs, || {
                ran = true;
                Ok(Some(produced(&f.case, "pub fn a() { /* honest */ }")))
            })
            .unwrap()
            .unwrap();
        assert!(ran, "an entry that cannot say what it cost must be recomputed");
        assert_eq!(got.provenance["duration_secs"], 42);
    }

    #[test]
    fn a_killed_write_does_not_poison_the_key() {
        // A run killed between locking the staging tree and renaming it leaves a 0o555
        // orphan, and entries inside a 0o555 directory cannot be removed.
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        // Ported onto this branch's `Inputs` builder, which replaced the free
        // fixtures()/key_inputs() helpers; `held` must outlive `inputs`, which borrows it.
        let held = Inputs::new();
        let inputs = held.key_inputs();
        let key = inputs.key();

        let staging =
            f.repo.join("results/.cache/tmp").join(format!("{}.partial", key.as_str()));
        std::fs::create_dir_all(staging.join("code/src")).unwrap();
        std::fs::write(staging.join("code/src/lib.rs"), "half-written").unwrap();
        crate::artifact::set_read_only(&staging, Access::ReadOnly).unwrap();

        store
            .obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}"))))
            .expect("a stale locked staging dir must not block the write")
            .unwrap();
        assert!(store.obtain::<Verify>(&inputs, || panic!("must hit")).unwrap().unwrap().replayed);
    }

    #[test]
    fn a_stored_entry_is_read_only_on_disk() {
        // Binds what the types cannot see: a shell-out, a stray `cargo build
        // --manifest-path`, a future refactor.
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let owned = Inputs::new();
        let inputs = owned.key_inputs();
        let key = inputs.key();
        store.obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}")))).unwrap();

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
        let owned = Inputs::new();
        let inputs = owned.key_inputs();
        store.obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}")))).unwrap();

        let replay = store.obtain::<Verify>(&inputs, || panic!("must hit")).unwrap().unwrap();
        assert!(replay.replayed);
        replay.sealed.publish(&f.case).unwrap();

        let published = crate::battery::phase_dir(&f.case, crate::battery::VERIFIED);
        std::fs::write(published.join("src/lib.rs"), "pub fn a() { /* editable */ }")
            .expect("a replayed artifact must be writable once published, or scoring cannot build");
    }

    #[test]
    fn a_tampered_entry_is_a_miss_rather_than_a_lie() {
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let owned = Inputs::new();
        let inputs = owned.key_inputs();
        let key = inputs.key();
        store.obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}")))).unwrap();

        // Defeat the read-only bit first: this is the scenario the digest check exists for.
        let dir = store.entry_dir(&inputs, &key);
        crate::artifact::set_read_only(&dir, Access::Writable).unwrap();
        std::fs::write(dir.join("code/src/lib.rs"), "pub fn a() { /* smuggled */ }").unwrap();
        // RE-LOCK: an entry found in the wild is 0o555, and quarantining one is a
        // cross-parent rename, which needs write permission on the moved directory. Left
        // unlocked, this test passes without the quarantine ever having worked.
        crate::artifact::set_read_only(&dir, Access::ReadOnly).unwrap();

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
            f.repo.join("results/.cache/quarantine").join(key.as_str()).exists(),
            "and the corruption must be preserved for inspection, not destroyed"
        );
    }

    #[test]
    fn the_store_sits_outside_every_tree_walker() {
        // `discover_batteries` treats each child of `results/<dataset>/<agent>` as a
        // battery, so a store living under it could be graded as if it were a case.
        let f = fixture();
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let owned = Inputs::new();
        let inputs = owned.key_inputs();
        store.obtain(&inputs, || Ok(Some(produced(&f.case, "pub fn a() {}")))).unwrap();

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
        assert!(f.repo.join("results/.cache").is_dir(), "it lives two levels above instead");
    }

    #[test]
    fn harness_stamp_is_recorded_but_not_keyed() {
        // Recorded so a result is traceable to code, but NOT keyed, or every harness
        // commit would empty the cache.
        let owned = Inputs::new();
        let ki = owned.key_inputs();
        let meta = ki.meta(&ki.key());
        assert!(
            meta.get("harness").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()),
            "the producing commit must be recorded: {meta:?}"
        );
        // `load` re-compares this exact list; `harness` must not be on it.
        for k in KeyInputs::VALIDATED {
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
