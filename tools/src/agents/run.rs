//! THE cached execution path for an agent phase: key, obtain, publish, record.
//!
//! [`crate::cache::Store::obtain`] is called here and nowhere else in the crate — a rule in
//! `tests/architecture.rs` asserts it — so a replay and a fresh run leave by the same path:
//! one publish, one metrics write, and no "cached" branch to keep in step with an uncached
//! one.

use crate::agents::work::IsolatedWorkDir;
use crate::artifact::{Phase, Sealed, Verify};
use crate::cache::{
    AgentKey, CliVersion, KeyInputs, ModelId, Produced, PromptDigest, RecipeDigest, Store,
    ToolchainId,
};
use anyhow::Result;
use std::path::Path;

/// What a phase leaves in its phase dir besides the artifact. On the phase MARKER rather
/// than in [`PhaseRun`]: a caller cannot pass another phase's file name, and a phase ported
/// onto this driver cannot forget to say what its metrics are called, because the missing
/// impl is what stops `run_cached::<P>` compiling at all.
pub trait Cached: Phase {
    const METRICS: &'static str;
}

impl Cached for Verify {
    const METRICS: &'static str = "verification.json";
}

/// Everything one cached phase needs, all of it resolved BEFORE the agent runs so the key
/// can name it.
///
/// A struct rather than positional parameters: `case_dir` and `log_path` are both `&Path`,
/// so positional args transpose silently. The key's input digest is deliberately NOT a
/// field — it is read from `work`, so the tree the key names is the tree the agent is
/// handed, and a transposed digest (a wrong key, silently) has nowhere to come from.
pub struct PhaseRun<'a, P: Cached> {
    pub work: IsolatedWorkDir<P>,
    pub case_dir: &'a Path,
    /// Where the invocation tees its transcript, and where a replay restores it.
    pub log_path: &'a Path,
    pub agent: &'a AgentKey,
    pub model: &'a ModelId,
    pub cli: &'a CliVersion,
    pub toolchain: &'a ToolchainId,
    pub prompt: &'a PromptDigest,
    pub recipe: &'a RecipeDigest,
}

/// What the phase left in the results tree.
pub enum Outcome<P: Phase> {
    /// Published under `<case>/<P::DIR>/`, by this run or from the store.
    Published(Sealed<P>),
    /// Nothing worth keeping, so nothing was stored either.
    Nothing,
}

/// Run one agent phase, or replay it.
///
/// `compute` returning `Ok(None)` means "nothing worth keeping" — an infra failure, or a
/// crate that does not compile. The store keeps no entry for it, so a transient failure is
/// never memoised into a permanent one.
pub fn run_cached<P, F>(run: PhaseRun<'_, P>, store: &Store, compute: F) -> Result<Outcome<P>>
where
    P: Cached,
    F: FnOnce(IsolatedWorkDir<P>) -> Result<Option<Produced<P>>>,
{
    let PhaseRun {
        work,
        case_dir,
        log_path,
        agent,
        model,
        cli,
        toolchain,
        prompt,
        recipe,
    } = run;
    let start = std::time::Instant::now();
    let input_tree = work.input_digest().clone();
    let inputs = KeyInputs {
        // From the phase itself, never a `&str` the caller passes: a literal that disagreed
        // with the `P` the store writes the entry under would key one phase as another.
        phase: P::DIR,
        agent,
        model,
        cli,
        toolchain,
        prompt,
        recipe,
        input_tree: &input_tree,
    };

    let obtained = store.obtain(&inputs, || compute(work))?;
    let metrics = crate::battery::phase_dir(case_dir, P::DIR).join(P::METRICS);

    let Some(obtained) = obtained else {
        // Nothing published or stored, but the transcript is on disk (the invocation tees it
        // live), so the post-mortem survives and the "already done" skip check still sees
        // this case.
        write_metrics(
            &metrics,
            &serde_json::json!({
                "agent": agent.as_str(),
                "duration_secs": start.elapsed().as_secs(),
                "success": false,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            false,
            None,
        );
        return Ok(Outcome::Nothing);
    };

    if obtained.replayed {
        println!(
            "  ♻️  replayed a stored verification ({:?})",
            obtained.sealed.digest()
        );
        // A replay must leave behind the same log a fresh run tees, or the skip check
        // misses this case and the next sweep pays for it again.
        store.restore_log(&inputs, &obtained.key, log_path)?;
    }

    obtained.sealed.publish(case_dir)?;

    // After the publish, which clears everything in the phase dir but `logs`.
    write_metrics(
        &metrics,
        &obtained.provenance,
        obtained.replayed,
        Some(obtained.key.as_str()),
    );
    Ok(Outcome::Published(obtained.sealed))
}

/// `provenance` describes the invocation that produced the artifact, which on a replay is
/// the ORIGINAL one — so `replayed` and `cache_key` are what stop its cost and timestamp
/// being read as this run's spend.
fn write_metrics(
    path: &Path,
    provenance: &serde_json::Value,
    replayed: bool,
    cache_key: Option<&str>,
) {
    let mut metrics = provenance.clone();
    metrics["replayed"] = serde_json::json!(replayed);
    if let Some(k) = cache_key {
        metrics["cache_key"] = serde_json::json!(k);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        path,
        serde_json::to_string_pretty(&metrics).unwrap_or_default() + "\n",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::session::Session;
    use crate::battery::{phase_dir, TRANSLATED, VERIFIED};
    use crate::cache::tests::fixture;
    use crate::cache::{fake_program, prompt_digest, Mode, Recipe};
    use crate::cli::Agent;
    use crate::domain::health::Completed;
    use crate::io::workdir::Roots;
    use std::path::PathBuf;

    /// Owns every key component, so two runs can borrow provably identical ones and the
    /// second is a hit for the reason under test rather than by luck.
    struct Keys {
        agent: AgentKey,
        model: ModelId,
        cli: CliVersion,
        toolchain: ToolchainId,
        prompt: PromptDigest,
        recipe: RecipeDigest,
    }

    impl Keys {
        fn new(repo: &Path) -> Self {
            let roots = Roots {
                work: PathBuf::from("/w"),
                repo: repo.to_path_buf(),
                work_base: None,
                home: None,
            };
            Self {
                agent: AgentKey::new(Agent::Claude, None).unwrap(),
                model: ModelId::new("claude-opus-5[1m]").unwrap(),
                // Probed from a fake CLI rather than fabricated: `CliVersion` exists to
                // name a build that was observed.
                cli: CliVersion::probe(&fake_program(
                    repo,
                    "claude",
                    "echo '2.1.231.653 (Claude Code)'",
                ))
                .unwrap(),
                toolchain: ToolchainId::for_test("1.94.0 x86_64-unknown-linux-gnu"),
                prompt: prompt_digest("verify the crate at $WORK", &roots),
                recipe: Recipe::new(&Session::claude(10_800), Some("deny=$REPO".into()))
                    .unwrap()
                    .digest(),
            }
        }
    }

    fn phase_run<'a>(
        case: &'a Path,
        log: &'a Path,
        keys: &'a Keys,
        work: IsolatedWorkDir<Verify>,
    ) -> PhaseRun<'a, Verify> {
        PhaseRun {
            work,
            case_dir: case,
            log_path: log,
            agent: &keys.agent,
            model: &keys.model,
            cli: &keys.cli,
            toolchain: &keys.toolchain,
            prompt: &keys.prompt,
            recipe: &keys.recipe,
        }
    }

    fn metrics_of(case: &Path) -> serde_json::Value {
        let path = phase_dir(case, VERIFIED).join(Verify::METRICS);
        serde_json::from_str(
            &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
        )
        .expect("metrics parse")
    }

    /// The whole path a phase takes, twice: translated tree → agent → seal → store →
    /// publish → metrics, then the same inputs again. A replay that publishes but leaves
    /// no `verify.log` behind makes the next sweep pay for the case again, and one that
    /// omits `replayed`/`cache_key` reports the original invocation's cost as this run's.
    #[test]
    fn a_replayed_phase_publishes_and_restores_the_transcript_a_fresh_run_would_have_teed() {
        let f = fixture();
        let case = f.case.clone();
        let keys = Keys::new(&f.repo);
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let log = phase_dir(&case, VERIFIED).join("logs/verify.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "the transcript the invocation teed\n").unwrap();

        const FIXED: &str = "pub fn a() { /* verified */ }";
        let fresh = run_cached(
            phase_run(&case, &log, &keys, IsolatedWorkDir::new(&case).unwrap()),
            &store,
            |work| {
                std::fs::write(work.translated_rust().join("src/lib.rs"), FIXED)?;
                Ok(Some(Produced::new(
                    work.finish(&Completed::for_test())?,
                    log.clone(),
                    serde_json::json!({"agent": "claude", "duration_secs": 42}),
                )))
            },
        )
        .unwrap();
        let Outcome::Published(fresh) = fresh else {
            panic!("a completed run must publish");
        };
        let published = phase_dir(&case, VERIFIED).join("src/lib.rs");
        assert_eq!(std::fs::read_to_string(&published).unwrap(), FIXED);
        assert_eq!(metrics_of(&case)["replayed"], serde_json::json!(false));

        // The evidence for the restore: a fresh run tees this file, a replay never runs the
        // agent, so its reappearance can only be the store putting it back.
        std::fs::remove_file(&log).unwrap();
        assert!(
            !log.exists(),
            "the fixture must remove what the replay restores"
        );

        let replayed = run_cached(
            phase_run(&case, &log, &keys, IsolatedWorkDir::new(&case).unwrap()),
            &store,
            |_| panic!("the agent must NOT run on a hit — that is the entire point"),
        )
        .unwrap();
        let Outcome::Published(replayed) = replayed else {
            panic!("a hit must publish the stored artifact");
        };
        assert_eq!(
            replayed.digest(),
            fresh.digest(),
            "a replay must publish the artifact that was stored"
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap_or_default(),
            "the transcript the invocation teed\n",
            "without the restored transcript the skip check misses this case and the next \
             sweep pays for it again"
        );
        assert_eq!(std::fs::read_to_string(&published).unwrap(), FIXED);
        assert!(
            phase_dir(&case, TRANSLATED)
                .join("target/debug/junk")
                .is_file(),
            "the fixture must contain the build output whose absence is asserted next"
        );
        assert!(
            !phase_dir(&case, VERIFIED).join("target").exists(),
            "build output is regenerable and bakes in a dead scratch path, so neither a \
             fresh run nor a replay may publish it"
        );

        let m = metrics_of(&case);
        assert_eq!(
            m["replayed"],
            serde_json::json!(true),
            "a replay recorded as a fresh run reports the original's spend as this run's: {m}"
        );
        assert!(
            m["cache_key"].as_str().is_some_and(|k| !k.is_empty()),
            "the entry replayed must be named: {m}"
        );
        assert_eq!(
            m["duration_secs"], 42,
            "a replay reports the ORIGINAL invocation's cost, not a blank"
        );
    }
}
