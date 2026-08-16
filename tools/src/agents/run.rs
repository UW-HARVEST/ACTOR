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

/// The phases this driver runs. Carries nothing of its own — a phase's metrics file name is
/// a [`Phase`] constant, since the uncached translate paths write one too — but the bound is
/// still what makes porting a phase onto this driver deliberate rather than a call that
/// happens to compile.
pub trait Cached: Phase {}

impl Cached for Verify {}

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

    let Some(obtained) = obtained else {
        // Nothing published or stored, but the transcript is on disk (the invocation tees it
        // live), so the post-mortem survives and the "already done" skip check still sees
        // this case.
        let mut provenance = serde_json::json!({
            "agent": agent.as_str(),
            "duration_secs": start.elapsed().as_secs(),
            "success": false,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });
        // `agent_provenance` carries the observed exit where the phase publishes; this is the
        // only place it survives where it does not — and an opaque transcript cannot tell a
        // finished run from one killed at the wall clock, so dropped here the audit sees nothing.
        crate::agents::exit::merge_agent_exit(&mut provenance);
        write_phase_metrics::<P>(case_dir, &provenance, Recorded::Fresh { entry: None });
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
    let entry = obtained.key.as_str();
    write_phase_metrics::<P>(
        case_dir,
        &obtained.provenance,
        if obtained.replayed {
            Recorded::Replayed { entry }
        } else {
            Recorded::Fresh { entry: Some(entry) }
        },
    );
    Ok(Outcome::Published(obtained.sealed))
}

/// Whose invocation the `provenance` beside it describes: on a replay, the ORIGINAL one, so
/// a replay recorded as fresh reports that cost and timestamp as this run's spend. A named
/// enum rather than a `replayed: bool`, because `success` is already a bool in the same
/// object — and a `Replayed` naming no entry is then unrepresentable rather than unwritten.
pub(crate) enum Recorded<'a> {
    Fresh { entry: Option<&'a str> },
    Replayed { entry: &'a str },
}

/// THE writer of what a phase records beside its artifact, for both phases: a translate
/// record that omitted `replayed` would report a replayed translation's stored cost as this
/// run's spend the moment the translate cache lands.
pub(crate) fn write_phase_metrics<P: Phase>(
    case_dir: &Path,
    provenance: &serde_json::Value,
    recorded: Recorded<'_>,
) {
    let (replayed, entry) = match recorded {
        Recorded::Fresh { entry } => (false, entry),
        Recorded::Replayed { entry } => (true, Some(entry)),
    };
    let mut metrics = provenance.clone();
    metrics["replayed"] = serde_json::json!(replayed);
    if let Some(k) = entry {
        metrics["cache_key"] = serde_json::json!(k);
    }
    let path = crate::artifact::phase_metrics::<P>(case_dir);
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
    use crate::artifact::{phase_metrics, Translate};
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

    /// A replay carries the ORIGINAL invocation's cost, so recording one as a fresh run
    /// reports that spend as this run's. The whole-path test below reaches the verify half
    /// only: translate does not run through this driver yet, so nothing else covers what one
    /// writer for both phases now writes for `Translate`.
    #[test]
    fn a_replay_is_recorded_as_a_replay_whichever_phase_it_belongs_to() {
        fn record<P: Phase>(case: &Path, recorded: Recorded<'_>) -> serde_json::Value {
            write_phase_metrics::<P>(
                case,
                &serde_json::json!({"agent": "claude", "duration_secs": 42, "success": true}),
                recorded,
            );
            let path = phase_metrics::<P>(case);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            serde_json::from_str(&text).expect("metrics parse")
        }

        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("mujs");

        let fresh = record::<Translate>(&case, Recorded::Fresh { entry: None });
        assert_eq!(fresh["replayed"], serde_json::json!(false));
        assert!(
            fresh.get("cache_key").is_none(),
            "an uncached run stored no entry to name: {fresh}"
        );

        let replayed = record::<Verify>(
            &case,
            Recorded::Replayed {
                entry: "verify/abc123",
            },
        );
        assert_eq!(
            replayed["replayed"],
            serde_json::json!(true),
            "a replay recorded as fresh bills the original's spend to this run: {replayed}"
        );
        assert_eq!(replayed["cache_key"], serde_json::json!("verify/abc123"));
        assert_eq!(
            replayed["duration_secs"], 42,
            "and it still reports the ORIGINAL invocation's cost, not a blank"
        );

        // Each record lands beside its own artifact, so neither overwrote the other.
        assert!(phase_metrics::<Translate>(&case).is_file());
        assert_ne!(
            std::fs::read_to_string(phase_metrics::<Translate>(&case)).unwrap(),
            std::fs::read_to_string(phase_metrics::<Verify>(&case)).unwrap(),
        );
    }

    /// The writing half of the wall-clock kill: a phase that publishes nothing is where the
    /// observation matters, because an opaque transcript cannot tell a finished run from a killed
    /// one. Dropped here, `agent_health::recorded_exit` had nothing to read and the killed run
    /// audited as `Unknown` — the record shape asserted below is the one
    /// `agent_health::tests::a_wall_clock_killed_opaque_run_is_an_infra_failure` reads back.
    #[test]
    fn a_phase_that_published_nothing_still_records_how_the_agent_exited() {
        let f = fixture();
        let case = f.case.clone();
        let keys = Keys::new(&f.repo);
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let log = phase_dir(&case, VERIFIED).join("logs/verify.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "> reading c_src/parson.c\n").unwrap();

        crate::agents::exit::clear_agent_exit();
        let outcome = run_cached(
            phase_run(&case, &log, &keys, IsolatedWorkDir::new(&case).unwrap()),
            &store,
            |_| {
                // What `timeout` leaves behind when it kills the child, reported as the
                // pipeline's status by the session's `set -o pipefail`.
                crate::agents::exit::record_agent_exit(
                    std::process::Command::new("sh")
                        .arg("-c")
                        .arg("exit 124")
                        .status()
                        .unwrap(),
                );
                Ok(None)
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, Outcome::Nothing),
            "a killed run has nothing to publish"
        );

        let m = metrics_of(&case);
        assert_eq!(
            m["exit_code"],
            serde_json::json!(124),
            "the audit's only evidence for an opaque backend: {m}"
        );
        assert_eq!(m["timed_out"], serde_json::json!(true), "{m}");
        assert_eq!(
            m["success"],
            serde_json::json!(false),
            "and it is still not a result: {m}"
        );
    }

    fn metrics_of(case: &Path) -> serde_json::Value {
        let path = phase_metrics::<Verify>(case);
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
