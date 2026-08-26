//! THE cached execution path for an agent phase: key, obtain, publish, record.
//!
//! [`crate::cache::Store::obtain`] is called here and nowhere else in the crate — a rule in
//! `tests/architecture.rs` asserts it — so a replay and a fresh run leave by the same path:
//! one publish, one metrics write, and no "cached" branch to keep in step with an uncached
//! one.

use crate::agents::work::IsolatedWorkDir;
use crate::artifact::{Phase, Publishing, Translate, Verify};
use crate::cache::{
    AgentKey, Attempt, CliVersion, Failure, KeyInputs, Mode, ModelId, PromptDigest, RecipeDigest,
    Resolved, Store, ToolchainId,
};
use anyhow::Result;
use std::path::Path;

/// The phases this driver runs. Carries nothing of its own — a phase's metrics file name is
/// a [`Phase`] constant, since the uncached translate paths write one too — but the bound is
/// still what makes porting a phase onto this driver deliberate rather than a call that
/// happens to compile.
pub trait Cached: Phase {}

impl Cached for Translate {}
impl Cached for Verify {}

/// What may answer "has this phase already run for this case?", named rather than a `keyed: bool`:
/// the store names the model, prompt, CLI and toolchain an artifact came from and a published crate
/// names none — which reported all seven harvest-bench projects done, unrun, on 2026-08-15.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum SkipCheck {
    Keyed,
    /// All there is to ask wherever no key can be asked about — the honest limit of a bypass.
    WhateverIsPublished,
}

impl SkipCheck {
    /// Keyedness needs a keyed launch AND a store that reads one: [`Mode::Bypass`] neither loads
    /// nor stores, so `Keyed` there deletes a path's only check. `Refresh` and `ReplayOnly` read.
    pub(crate) fn through(self, store: Mode) -> Self {
        match store {
            Mode::Bypass => SkipCheck::WhateverIsPublished,
            Mode::ReadWrite | Mode::Refresh | Mode::ReplayOnly => self,
        }
    }

    /// The keyed answer needs the key, which [`run_cached`] resolves; only `published` is local.
    pub(crate) fn already_done(self, published: impl FnOnce() -> bool) -> bool {
        match self {
            SkipCheck::Keyed => false,
            SkipCheck::WhateverIsPublished => published(),
        }
    }
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
    /// The bytes `prompt` was hashed from, and the recipe `recipe` was hashed from. Carried BESIDE
    /// the digests, not instead of them: the digests are what the key names, these are only
    /// recorded in the entry so a change to `normalise` or to `Recipe::digest`'s framing becomes a
    /// re-key rather than a cache wipe.
    pub prompt_text: &'a str,
    /// Owned, from [`crate::cache::Recipe::shape_record`], because `Recipe` borrows the `Session`
    /// beside it and a borrowed field here would make the type unconstructible in a test.
    pub recipe_record: serde_json::Value,
}

/// What the phase left in the results tree.
pub enum Outcome<P: Phase> {
    /// Published under `<case>/<P::DIR>/` and not yet digested: see [`crate::artifact::Publishing`].
    Published(Publishing<P>),
    /// Nothing worth keeping, so nothing was stored either.
    Nothing,
    /// No stored artifact, and [`Mode::ReplayOnly`] forbade paying: there is no run to record.
    Unavailable,
}

/// Run one agent phase, or replay it. `compute` returning [`Attempt::Nothing`] is "nothing worth
/// keeping" — an infra failure, or a crate that does not compile; see [`Store::record_failure`].
pub fn run_cached<P, F>(run: PhaseRun<'_, P>, store: &Store, compute: F) -> Result<Outcome<P>>
where
    P: Cached,
    F: FnOnce(IsolatedWorkDir<P>) -> Result<Attempt<P>>,
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
        prompt_text,
        recipe_record,
    } = run;
    let start = std::time::Instant::now();
    let input_tree = work.input_digest().clone();
    // Taken before `work` moves into `compute`.
    let seed = work.seed().clone();
    // Two reads of one table agree by construction, but a key naming a model the agent did not run
    // is silent corruption. Checked, not assumed.
    anyhow::ensure!(
        agent.model() == Some(model),
        "the key would record model {:?} while the agent runs {}",
        agent.model().map(|m| m.as_str()),
        model.as_str()
    );
    let inputs = KeyInputs {
        // From the phase itself, never a `&str` the caller passes: a literal that disagreed
        // with the `P` the store writes the entry under would key one phase as another.
        phase: P::DIR,
        agent,
        toolchain,
        prompt,
        recipe,
        input_tree: &input_tree,
    };

    // `cli` travels beside the key inputs, not inside them: it is recorded in the entry for
    // audit and deliberately not keyed, because the agent CLIs auto-update through a shim and
    // keying them stranded every entry on each vendor release.
    let record = crate::cache::Preimage {
        seed,
        prompt: prompt_text.to_string(),
        recipe: recipe_record,
    };
    let obtained = match store.obtain(&inputs, cli, &record, || compute(work))? {
        Resolved::Obtained(obtained) => obtained,
        Resolved::Unavailable => return Ok(Outcome::Unavailable),
        Resolved::Nothing(why) => {
            // The transcript stays in the phase dir, teed there live: it is the post-mortem and all
            // the infra gate reads this case through. It has also truncated the PREVIOUS run's, whose
            // ARTIFACT is beside it and would be stamped with this log's model and cost — so that
            // artifact moves, after the run, and is moved rather than deleted: it was paid for.
            displace_and_warn::<P>(case_dir)?;
            let mut provenance = serde_json::json!({
                "agent": agent.as_str(),
                "duration_secs": start.elapsed().as_secs(),
                "success": false,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });
            // `agent_provenance` carries the observed exit where the phase publishes; this is the only
            // place it survives where it does not, and an opaque transcript cannot tell a kill apart.
            crate::agents::exit::merge_agent_exit(&mut provenance);
            // Reported, not propagated: an unwritable record must not cost the `<phase>.json`.
            if let Err(e) = store.record_failure(
                &inputs,
                cli,
                &inputs.key(),
                &Failure::new(&why, log_path, &provenance),
            ) {
                eprintln!("  cache: the failed run was NOT recorded: {e:#}");
            }
            write_phase_metrics::<P>(case_dir, &provenance, Recorded::Fresh { entry: None });
            return Ok(Outcome::Nothing);
        }
    };

    if obtained.replayed {
        println!(
            "  ♻️  replayed a stored {}/ ({:?})",
            P::DIR,
            obtained.sealed.digest()
        );
        // A replay must leave behind the same log a fresh run tees, or the skip check
        // misses this case and the next sweep pays for it again.
        store.restore_log(&inputs, &obtained.key, log_path)?;
    }

    let publishing = obtained.sealed.publish(case_dir)?;

    // After the publish, which clears the phase dir but `logs`. `P::METRICS` is `Ignore`-class at
    // the artifact root, so writing it before `finish` cannot move the digest.
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
    Ok(Outcome::Published(publishing))
}

/// [`crate::artifact::displace_phase`] and the operator's only notice that it happened, in one
/// place: this is the sole caller, so the keyed failure path here and the unkeyed one in
/// `translate::run_and_record` cannot come to disagree on either half. Called AFTER the run,
/// never before — a displacement up front would move aside a crate this run then republishes.
pub(crate) fn displace_and_warn<P: Phase>(case_dir: &Path) -> Result<()> {
    if let Some(aside) = crate::artifact::displace_phase::<P>(case_dir)? {
        eprintln!(
            "  ⚠️  this run published nothing; the previous {}/ is at {}",
            P::DIR,
            aside.display()
        );
    }
    Ok(())
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
    use crate::artifact::{phase_metrics, Published, Sealed};
    use crate::battery::{has_crate, phase_dir, TRANSLATED, VERIFIED};
    use crate::cache::tests::fixture;
    use crate::cache::{fake_program, prompt_digest, Counts, Mode, NotProduced, Produced, Recipe};
    use crate::cli::{honouring, Agent, Reuse};
    use crate::domain::health::Completed;
    use crate::io::workdir::Roots;
    use crate::tree::TreeDigest;
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
        /// Held so `PhaseRun` can carry the preimages beside the digests, as the driver does.
        prompt_text: String,
        recipe_record: serde_json::Value,
    }

    impl Keys {
        fn new(repo: &Path) -> Self {
            let roots = Roots {
                work: PathBuf::from("/w"),
                repo_parent: repo.parent().map(|p| p.to_path_buf()),
                repo: repo.to_path_buf(),
                work_base: None,
                home: None,
            };
            // Resolved ONCE and shared: the fixture had `claude-opus-5[1m]` beside a key resolving
            // `global.anthropic.claude-opus-5[1m]`, which `inputs` now refuses.
            let resolved = crate::agents::invocation::resolved_model(Agent::Claude, None)
                .unwrap()
                .expect("claude runs a model");
            Self {
                agent: AgentKey::new(Agent::Claude, None, Some(resolved.clone())).unwrap(),
                model: resolved,
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
                prompt_text: crate::cache::normalise("verify the crate at $WORK", &roots),
                recipe_record: Recipe::new(&Session::claude(10_800), Some("deny=$REPO".into()))
                    .unwrap()
                    .shape_record(),
            }
        }

        /// The key the driver computes for `P`, varying nothing but the phase and the tree.
        fn inputs<'a, P: Cached>(&'a self, input_tree: &'a TreeDigest) -> KeyInputs<'a> {
            KeyInputs {
                phase: P::DIR,
                agent: &self.agent,
                toolchain: &self.toolchain,
                prompt: &self.prompt,
                recipe: &self.recipe,
                input_tree,
            }
        }
    }

    /// Taken off the fixture's phase dir: these tests are about the STORE, not minting.
    fn translated(case: &Path) -> Published<Translate> {
        Published::<Translate>::unkeyed_from_phase_dir(case)
            .expect("the fixture wrote a translated/ crate")
    }

    fn phase_run<'a, P: Cached>(
        case: &'a Path,
        log: &'a Path,
        keys: &'a Keys,
        work: IsolatedWorkDir<P>,
    ) -> PhaseRun<'a, P> {
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
            prompt_text: &keys.prompt_text,
            recipe_record: keys.recipe_record.clone(),
        }
    }

    /// What every caller does between publish and finish: `post_process_independent`'s idempotent
    /// edit. Load-bearing — the phase dir then holds a tree no entry's digest describes (0 of 84
    /// stamped cases match), so a publish-time check against the entry compares trees never equal.
    fn post_processed(publishing: Publishing<Translate>) -> Published<Translate> {
        publishing
            .edited(|tree| {
                let cargo = tree.join("Cargo.toml");
                let text = std::fs::read_to_string(&cargo)?;
                if !text.contains("[workspace]") {
                    std::fs::write(&cargo, format!("{text}\n[workspace]\n"))?;
                }
                Ok(())
            })
            .finish()
            .expect("a published tree must digest")
    }

    /// The `bool` is whether the agent ran: outside, a replay and a re-run differ in nothing else.
    fn translate_once(
        case: &Path,
        corpus: &Path,
        log: &Path,
        keys: &Keys,
        store: &Store,
        body: &str,
    ) -> (Published<Translate>, bool) {
        let ran = std::cell::Cell::new(false);
        let outcome = run_cached(
            phase_run(
                case,
                log,
                keys,
                IsolatedWorkDir::<Translate>::from_corpus(corpus).unwrap(),
            ),
            store,
            |work| {
                ran.set(true);
                let crate_dir = work.translated_rust();
                std::fs::create_dir_all(crate_dir.join("src"))?;
                std::fs::write(crate_dir.join("Cargo.toml"), "[package]\nname=\"x\"")?;
                std::fs::write(crate_dir.join("src/lib.rs"), body)?;
                Ok(Attempt::Produced(Produced::new(
                    work.finish(&Completed::for_test())?,
                    log.to_path_buf(),
                    serde_json::json!({"agent": "claude", "duration_secs": 42}),
                )))
            },
        )
        .unwrap();
        let Outcome::Published(publishing) = outcome else {
            panic!("a completed run must publish");
        };
        (post_processed(publishing), ran.get())
    }

    /// THE headline criterion, at the scale a test can state it: one pass pays for both phases and
    /// the SECOND is two hits with no agent invocation, both through ONE store.
    #[test]
    fn a_second_pass_over_a_case_is_two_cache_hits_and_no_agent_invocation() {
        let f = fixture();
        let case = f.case.clone();
        let keys = Keys::new(&f.repo);
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let corpus = corpus(&f.repo.join("corpus"), "upstream");
        let tlog = phase_dir(&case, TRANSLATED).join("logs/translation.log");
        let vlog = phase_dir(&case, VERIFIED).join("logs/verify.log");
        for log in [&tlog, &vlog] {
            std::fs::create_dir_all(log.parent().unwrap()).unwrap();
            std::fs::write(log, "the transcript the invocation teed\n").unwrap();
        }

        // A PARAMETER, not captured: a closure would keep counting into the FIRST store.
        let verify_once = |store: &Store, ran: &std::cell::Cell<bool>| {
            let outcome = run_cached(
                phase_run(
                    &case,
                    &vlog,
                    &keys,
                    IsolatedWorkDir::new(&translated(&case)).unwrap(),
                ),
                store,
                |work| {
                    ran.set(true);
                    std::fs::write(
                        work.translated_rust().join("src/lib.rs"),
                        "pub fn a() { /* verified */ }",
                    )?;
                    Ok(Attempt::Produced(Produced::new(
                        work.finish(&Completed::for_test())?,
                        vlog.clone(),
                        serde_json::json!({"agent": "claude", "duration_secs": 42}),
                    )))
                },
            )
            .unwrap();
            assert!(matches!(outcome, Outcome::Published(_)));
        };

        let (_, ran) = translate_once(
            &case,
            &corpus,
            &tlog,
            &keys,
            &store,
            "pub fn a() { /* translated */ }",
        );
        assert!(ran, "nothing is stored yet, so the first pass pays");
        let ran = std::cell::Cell::new(false);
        verify_once(&store, &ran);
        assert!(ran.get(), "and so does verify");
        assert_eq!(
            store.tally(),
            [
                (
                    TRANSLATED,
                    Counts {
                        hits: 0,
                        invocations: 1
                    }
                ),
                (
                    VERIFIED,
                    Counts {
                        hits: 0,
                        invocations: 1
                    }
                ),
            ]
            .into_iter()
            .collect(),
            "the first pass is two invocations: {}",
            store.tally_line().unwrap_or_default()
        );

        let second = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (_, ran) = translate_once(
            &case,
            &corpus,
            &tlog,
            &keys,
            &second,
            "pub fn a() { /* never reached */ }",
        );
        assert!(!ran, "the store holds this key");
        let ran = std::cell::Cell::new(false);
        verify_once(&second, &ran);
        assert!(!ran.get(), "and this one");
        let store = second;
        assert_eq!(
            store.tally(),
            [
                (
                    TRANSLATED,
                    Counts {
                        hits: 1,
                        invocations: 0
                    }
                ),
                (
                    VERIFIED,
                    Counts {
                        hits: 1,
                        invocations: 0
                    }
                ),
            ]
            .into_iter()
            .collect(),
            "two hits per case and zero agent invocations is the whole design: {}",
            store.tally_line().unwrap_or_default()
        );
        assert_eq!(
            store.tally_line().as_deref(),
            Some("🗃️  cache: translated 1 hit / 0 run, verified 1 hit / 0 run (0 agent invocation(s))"),
            "and it is PRINTED, so 'two hits per case' is observable rather than inferred"
        );
        assert_eq!(
            Store::open(&f.repo, Mode::ReadWrite).unwrap().tally_line(),
            None,
            "while a store nothing went through says nothing: a table of zeroes would read as \
             'everything was cached' for a sweep that never consulted it"
        );
    }

    /// The other half of the same defect: while `has_crate` answered "done" on a populated tree,
    /// the store was never asked, so the cache was inert on the very sweep it exists for.
    #[test]
    fn a_populated_results_tree_still_replays_from_the_store_instead_of_re_running() {
        let f = fixture();
        let case = f.case.clone();
        let mut keys = Keys::new(&f.repo);
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let corpus = corpus(&f.repo.join("corpus"), "upstream");
        let log = phase_dir(&case, TRANSLATED).join("logs/translation.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "the transcript the invocation teed\n").unwrap();

        const FIXED: &str = "pub fn a() { /* translated */ }";
        let (first, ran) = translate_once(&case, &corpus, &log, &keys, &store, FIXED);
        assert!(ran, "the first sweep has nothing stored, so the agent runs");

        let published = phase_dir(&case, TRANSLATED);
        assert!(
            has_crate(&published),
            "the tree must be populated, or this is not the case under test"
        );

        let replayed = run_cached(
            phase_run(
                &case,
                &log,
                &keys,
                IsolatedWorkDir::<Translate>::from_corpus(&corpus).unwrap(),
            ),
            &store,
            |_| panic!("the store holds this exact key, so no agent may be paid for it"),
        )
        .unwrap();
        let Outcome::Published(replayed) = replayed else {
            panic!("a hit must publish the stored artifact");
        };
        assert_eq!(
            post_processed(replayed).digest(),
            first.digest(),
            "a replay must publish the artifact that was stored"
        );
        assert_eq!(
            std::fs::read_to_string(published.join("src/lib.rs")).unwrap(),
            FIXED
        );

        // Non-vacuity: the entry served was the one THIS key names, not whatever was stored.
        // Both, and consistently: the key reads the model off the agent, and `inputs` refuses a
        // PhaseRun whose model disagrees with it.
        keys.model = ModelId::new("claude-sonnet-5").unwrap();
        keys.agent = AgentKey::for_test(keys.agent.as_str(), "claude-sonnet-5").unwrap();
        const OTHER: &str = "pub fn a() { /* another model translated this */ }";
        let (second, ran) = translate_once(&case, &corpus, &log, &keys, &store, OTHER);
        assert!(
            ran,
            "a translation by another model is not this model's result, so the agent must run"
        );
        assert_ne!(
            first.digest(),
            second.digest(),
            "the two runs must differ, or 'it ran' proves nothing about what it published"
        );
        assert_eq!(
            std::fs::read_to_string(published.join("src/lib.rs")).unwrap(),
            OTHER,
            "and what is published is the model that was asked for"
        );
    }

    /// `--force` is "do not reuse a previous result", which a keyed skip check cannot honour — it
    /// answers `false` anyway — so left there, the operator is handed that entry back, replayed.
    #[test]
    fn a_forced_run_pays_the_agent_again_rather_than_replaying_the_entry_it_distrusts() {
        let f = fixture();
        let case = f.case.clone();
        let keys = Keys::new(&f.repo);
        let log = phase_dir(&case, VERIFIED).join("logs/verify.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "the transcript the invocation teed\n").unwrap();

        let verify_once = |store: &Store, body: &str| -> bool {
            let ran = std::cell::Cell::new(false);
            let outcome = run_cached(
                phase_run(
                    &case,
                    &log,
                    &keys,
                    IsolatedWorkDir::new(&translated(&case)).unwrap(),
                ),
                store,
                |work| {
                    ran.set(true);
                    std::fs::write(work.translated_rust().join("src/lib.rs"), body)?;
                    Ok(Attempt::Produced(Produced::new(
                        work.finish(&Completed::for_test())?,
                        log.to_path_buf(),
                        serde_json::json!({"agent": "claude", "duration_secs": 42}),
                    )))
                },
            )
            .unwrap();
            assert!(
                matches!(outcome, Outcome::Published(_)),
                "a completed run must publish"
            );
            ran.get()
        };

        const DISPUTED: &str = "pub fn a() { /* the result the operator distrusts */ }";
        let reusing = Store::open(&f.repo, honouring(Mode::ReadWrite, Reuse::Permitted)).unwrap();
        assert!(verify_once(&reusing, DISPUTED), "nothing is stored yet");
        assert!(
            !verify_once(&reusing, "pub fn a() { /* never reached */ }"),
            "non-vacuity: this key IS a hit without the flag, so `Refused` below is the only \
             thing that can change the answer"
        );

        const RERUN: &str = "pub fn a() { /* what a second look produced */ }";
        let forced = Store::open(&f.repo, honouring(Mode::ReadWrite, Reuse::Refused)).unwrap();
        assert!(
            verify_once(&forced, RERUN),
            "--force must reach the store, or it changes nothing a keyed phase does"
        );
        assert_eq!(
            std::fs::read_to_string(phase_dir(&case, VERIFIED).join("src/lib.rs")).unwrap(),
            RERUN,
            "and the published crate is this run's, not the entry that was replaced"
        );

        assert_eq!(
            honouring(Mode::Bypass, Reuse::Refused),
            Mode::Bypass,
            "while an operator who asked for no cache must not be given one: there --force \
             overrides the published-log check instead, which is all that path has"
        );
    }

    /// `doc/footer.html.bak` is real in 26 stored cases, and is what the root rules drop.
    fn corpus(at: &Path, bak: &str) -> PathBuf {
        for (rel, body) in [
            ("src/lib.c", "int a(void){return 0;}"),
            ("doc/footer.html.bak", bak),
        ] {
            let p = at.join(rel);
            std::fs::create_dir_all(p.parent().expect("a parent")).unwrap();
            std::fs::write(p, body).unwrap();
        }
        at.to_path_buf()
    }

    /// A replay carries the ORIGINAL invocation's cost, so recording one as a fresh run
    /// reports that spend as this run's. Where the whole-path tests below exercise one phase
    /// each, this is what asserts the two records land beside their own artifact rather than
    /// one overwriting the other.
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
            phase_run(
                &case,
                &log,
                &keys,
                IsolatedWorkDir::new(&translated(&case)).unwrap(),
            ),
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
                Ok(Attempt::Nothing(NotProduced::DidNotComplete {
                    health: "Infra { reason: \"timeout\" }".into(),
                }))
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, Outcome::Nothing),
            "a killed run has nothing to publish"
        );

        let m = metrics_of::<Verify>(&case);
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

        // The record the DRIVER filed, not one a test hand-called: a `Resolved::Nothing` arm that
        // skipped `record_failure` would lose the transcript the next attempt tees over, silently.
        let filed = store.failures().unwrap();
        assert_eq!(filed.len(), 1, "one failed run, one record: {filed:?}");
        let (phase, who, key, attempt) = &filed[0];
        let run_dir = keys.agent.dir();
        assert_eq!(
            (phase.as_str(), who.as_str(), attempt.as_str()),
            (VERIFIED, run_dir.as_str(), "1"),
            "filed under the phase and RUN that failed, as attempt 1: {filed:?}"
        );
        let kept = f
            .repo
            .join("results/.cache")
            .join(crate::cache::SCHEMA.to_string())
            .join(crate::cache::FAILED)
            .join(phase)
            .join(who)
            .join(key)
            .join(attempt)
            .join("agent/run.log");
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap_or_default(),
            "> reading c_src/parson.c\n",
            "and the transcript itself is what was kept, at {}",
            kept.display()
        );
    }

    /// "Refuse before the money", whole: a hit is served as read-write serves it, and a miss reaches
    /// no agent, displaces nothing and files no failure. `verify` seeds every case this way.
    #[test]
    fn a_replay_only_phase_serves_a_stored_translation_and_refuses_to_pay_for_a_miss() {
        let f = fixture();
        let case = f.case.clone();
        let keys = Keys::new(&f.repo);
        let corpus = corpus(&f.repo.join("corpus"), "upstream");
        let log = phase_dir(&case, TRANSLATED).join("logs/translation.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "the transcript the invocation teed\n").unwrap();
        const PAID_FOR: &str = "pub fn a() { /* the crate the operator paid for */ }";
        std::fs::write(phase_dir(&case, TRANSLATED).join("src/lib.rs"), PAID_FOR).unwrap();

        let replaying = Store::open(&f.repo, Mode::ReplayOnly).unwrap();
        let refused = run_cached(
            phase_run(
                &case,
                &log,
                &keys,
                IsolatedWorkDir::<Translate>::from_corpus(&corpus).unwrap(),
            ),
            &replaying,
            |_| panic!("nothing is stored, and paying for it is what this mode exists to refuse"),
        )
        .unwrap();
        assert!(
            matches!(refused, Outcome::Unavailable),
            "a miss here is 'no artifact', not 'a run that produced none'"
        );
        assert!(
            !case.join(format!("{TRANSLATED}.displaced")).exists(),
            "no run happened, so there is nothing to displace a paid crate for"
        );
        assert_eq!(
            std::fs::read_to_string(phase_dir(&case, TRANSLATED).join("src/lib.rs")).unwrap(),
            PAID_FOR,
            "and the artifact the command was asked to check is still the one on disk"
        );
        assert!(
            replaying.failures().unwrap().is_empty(),
            "nor may a refusal be filed as a failed run: no agent exited"
        );
        assert!(
            !phase_metrics::<Translate>(&case).exists(),
            "and no record may claim an attempt this run never made"
        );
        assert_eq!(replaying.tally(), Default::default(), "nothing to report");

        let paying = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (stored, ran) = translate_once(&case, &corpus, &log, &keys, &paying, "pub fn a() {}");
        assert!(ran, "the entry has to be paid for once");
        let replaying = Store::open(&f.repo, Mode::ReplayOnly).unwrap();
        let served = run_cached(
            phase_run(
                &case,
                &log,
                &keys,
                IsolatedWorkDir::<Translate>::from_corpus(&corpus).unwrap(),
            ),
            &replaying,
            |_| panic!("the store holds this key, so no agent may be paid for it"),
        )
        .unwrap();
        let Outcome::Published(served) = served else {
            panic!("a hit must publish the stored artifact");
        };
        assert_eq!(
            post_processed(served).digest(),
            stored.digest(),
            "a replay-only hit publishes what was stored, byte for byte"
        );
        assert_eq!(
            replaying
                .tally()
                .get(TRANSLATED)
                .copied()
                .unwrap_or_default(),
            Counts {
                hits: 1,
                invocations: 0
            },
            "counted as a hit and no invocation: {}",
            replaying.tally_line().unwrap_or_default()
        );
    }

    /// `verify` seeds itself by REPUBLISHING each case's stored translation, and
    /// `Translate::INVALIDATES` names `verified/` — so the seeding leg deleted the verification the
    /// command was asked to check, crate, `logs/` and `verification.json`, before its agent had
    /// produced a replacement. 248 of them are in the shipped submodule, replayable from nowhere.
    #[test]
    fn seeding_a_verify_sweep_from_a_stored_translation_keeps_the_verification_it_checks() {
        let f = fixture();
        let case = f.case.clone();
        let keys = Keys::new(&f.repo);
        let corpus = corpus(&f.repo.join("corpus"), "upstream");
        let tlog = phase_dir(&case, TRANSLATED).join("logs/translation.log");
        std::fs::create_dir_all(tlog.parent().unwrap()).unwrap();
        std::fs::write(&tlog, "the transcript the invocation teed\n").unwrap();

        let paid = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let (stored, ran) = translate_once(&case, &corpus, &tlog, &keys, &paid, "pub fn a() {}");
        assert!(ran, "the translate entry has to be paid for once");

        // The verification standing beside it: crate, score, record, and its transcript.
        const VERIFICATION: &str = "pub fn a() { /* verified */ }";
        let verified = phase_dir(&case, VERIFIED);
        std::fs::create_dir_all(verified.join("src")).unwrap();
        for (rel, body) in [
            ("Cargo.toml", "[package]\nname=\"x\""),
            ("src/lib.rs", VERIFICATION),
            ("result.json", r#"{"tests_passed": 5}"#),
            ("verification.json", r#"{"success": true}"#),
        ] {
            std::fs::write(verified.join(rel), body).unwrap();
        }
        let vlog = crate::artifact::phase_log::<Verify>(&case);
        std::fs::create_dir_all(vlog.parent().unwrap()).unwrap();
        std::fs::write(&vlog, "the verification's own transcript\n").unwrap();

        // The seeding leg of `harvest-tools verify`: replay-only, and this key hits.
        let seeding = Store::open(&f.repo, Mode::ReplayOnly).unwrap();
        let served = run_cached(
            phase_run(
                &case,
                &tlog,
                &keys,
                IsolatedWorkDir::<Translate>::from_corpus(&corpus).unwrap(),
            ),
            &seeding,
            |_| panic!("the store holds this key, so no agent may be paid for it"),
        )
        .unwrap();
        let Outcome::Published(served) = served else {
            panic!("a hit must publish the stored artifact");
        };
        let served = post_processed(served);
        assert_eq!(
            served.digest(),
            stored.digest(),
            "fixture: the republished translation IS the one already there, so nothing keyed on \
             it moved and the verification beside it is as valid as it was"
        );
        assert!(
            std::fs::read_to_string(phase_dir(&case, TRANSLATED).join("Cargo.toml"))
                .unwrap()
                .contains("[workspace]"),
            "fixture: the post-publish edit must have landed, or the trap is absent — it is what \
             makes the phase dir differ from the tree the entry stores, so an identity check made \
             at publish time against the entry would compare trees that are never equal"
        );
        for (rel, body) in [
            ("src/lib.rs", VERIFICATION),
            ("result.json", r#"{"tests_passed": 5}"#),
            ("verification.json", r#"{"success": true}"#),
        ] {
            assert_eq!(
                std::fs::read_to_string(verified.join(rel)).unwrap_or_default(),
                body,
                "seeding a verify sweep deleted verified/{rel}, which no key asked to replace"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&vlog).unwrap_or_default(),
            "the verification's own transcript\n",
            "nor its transcript, which is the entire post-mortem"
        );

        // And when the verify key MISSES and its agent produces nothing, it stays recoverable.
        crate::agents::exit::clear_agent_exit();
        let paying = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let outcome = run_cached(
            phase_run(&case, &vlog, &keys, IsolatedWorkDir::new(&served).unwrap()),
            &paying,
            |_| Ok(Attempt::Nothing(NotProduced::DoesNotCompile)),
        )
        .unwrap();
        assert!(
            matches!(outcome, Outcome::Nothing),
            "a run that produced nothing has nothing to publish"
        );
        let aside = case.join(format!("{VERIFIED}.displaced"));
        assert_eq!(
            std::fs::read_to_string(aside.join("src/lib.rs")).unwrap_or_default(),
            VERIFICATION,
            "a paid-for verification must be recoverable at {}",
            aside.display()
        );
    }

    const RUN_A: &str = "pub fn a() { /* run A's verification */ }";
    const RUN_B_LOG: &str = "run B's transcript, teed over run A's\n";

    /// Run A publishes a verified crate and is scored; run B publishes nothing. Both invariants.
    fn a_run_that_published_nothing(f: &crate::cache::tests::Fixture) -> PathBuf {
        let case = &f.case;
        let verified = phase_dir(case, VERIFIED);
        std::fs::create_dir_all(verified.join("src")).unwrap();
        for (rel, body) in [
            ("Cargo.toml", "[package]\nname=\"x\""),
            ("src/lib.rs", RUN_A),
            ("result.json", r#"{"tests_passed": 5}"#),
        ] {
            std::fs::write(verified.join(rel), body).unwrap();
        }
        assert!(
            has_crate(&verified),
            "the fixture must hold a complete verified/ crate, or there is no corruption to \
             leave and nothing to lose"
        );

        let log = verified.join("logs/verify.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, RUN_B_LOG).unwrap();

        crate::agents::exit::clear_agent_exit();
        let keys = Keys::new(&f.repo);
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let outcome = run_cached(
            phase_run(
                case,
                &log,
                &keys,
                IsolatedWorkDir::new(&translated(case)).unwrap(),
            ),
            &store,
            |_| Ok(Attempt::Nothing(NotProduced::DoesNotCompile)),
        )
        .unwrap();
        assert!(
            matches!(outcome, Outcome::Nothing),
            "a run that produced nothing has nothing to publish"
        );
        verified
    }

    /// INVARIANT 1. The failed run's transcript has replaced run A's, so run A's crate beside it
    /// is scored as this run's result, with this run's model and cost stamped on by the enrichers.
    #[test]
    fn a_phase_that_published_nothing_leaves_no_earlier_crate_beside_its_transcript() {
        let f = fixture();
        let case = f.case.clone();
        let verified = a_run_that_published_nothing(&f);

        assert!(
            !has_crate(&verified),
            "run A's crate cannot stand beside run B's transcript: it would be scored as run \
             B's result, with run B's model and cost"
        );
        assert!(
            !verified.join("result.json").exists(),
            "nor its score, which is what the enrichers rewrite in place"
        );
        assert!(
            !has_crate(&phase_dir(&case, VERIFIED)),
            "so this run resolved no verified artifact at all, and a score can only cover the \
             artifacts it resolved (see `crate::eval`)"
        );
        assert_eq!(
            std::fs::read_to_string(crate::artifact::phase_log::<Verify>(&case)).unwrap(),
            RUN_B_LOG,
            "while the transcript itself survives — it is the entire post-mortem, and the \
             infra gate reads this case through it"
        );
        assert_eq!(
            metrics_of::<Verify>(&case)["success"],
            serde_json::json!(false),
            "and the record of the failed run is written after the artifact moves, not into it"
        );
    }

    /// INVARIANT 2, at the same time: the crate run B could not replace was paid for and nothing
    /// replays it — `--cache off` stores none — so deleting it makes one Ctrl-C a permanent loss.
    #[test]
    fn a_run_that_publishes_nothing_leaves_the_artifact_it_could_not_replace_on_disk() {
        let f = fixture();
        let case = f.case.clone();
        a_run_that_published_nothing(&f);

        let aside = case.join(format!("{VERIFIED}.displaced"));
        assert!(
            has_crate(&aside),
            "run A's crate must still be on disk, whole: {}",
            aside.display()
        );
        assert_eq!(
            std::fs::read_to_string(aside.join("src/lib.rs")).unwrap(),
            RUN_A,
            "and it must be run A's crate rather than an empty shell of one"
        );
        assert!(
            aside.join("result.json").is_file(),
            "with the score it was measured at, since that score is run A's too"
        );

        // A second failure must not eat the first one's evidence.
        let keys = Keys::new(&f.repo);
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let outcome = run_cached(
            phase_run(
                &case,
                &crate::artifact::phase_log::<Verify>(&case),
                &keys,
                IsolatedWorkDir::new(&translated(&case)).unwrap(),
            ),
            &store,
            |_| Ok(Attempt::Nothing(NotProduced::DoesNotCompile)),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Nothing));
        assert_eq!(
            std::fs::read_to_string(aside.join("src/lib.rs")).unwrap(),
            RUN_A,
            "a second failed run must not displace a metrics file over the crate"
        );
    }

    fn metrics_of<P: Phase>(case: &Path) -> serde_json::Value {
        let path = phase_metrics::<P>(case);
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
            phase_run(
                &case,
                &log,
                &keys,
                IsolatedWorkDir::new(&translated(&case)).unwrap(),
            ),
            &store,
            |work| {
                std::fs::write(work.translated_rust().join("src/lib.rs"), FIXED)?;
                Ok(Attempt::Produced(Produced::new(
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
        let fresh = fresh.finish().unwrap();
        let published = phase_dir(&case, VERIFIED).join("src/lib.rs");
        assert_eq!(std::fs::read_to_string(&published).unwrap(), FIXED);
        assert_eq!(
            metrics_of::<Verify>(&case)["replayed"],
            serde_json::json!(false)
        );

        // The evidence for the restore: a fresh run tees this file, a replay never runs the
        // agent, so its reappearance can only be the store putting it back.
        std::fs::remove_file(&log).unwrap();
        assert!(
            !log.exists(),
            "the fixture must remove what the replay restores"
        );

        let replayed = run_cached(
            phase_run(
                &case,
                &log,
                &keys,
                IsolatedWorkDir::new(&translated(&case)).unwrap(),
            ),
            &store,
            |_| panic!("the agent must NOT run on a hit — that is the entire point"),
        )
        .unwrap();
        let Outcome::Published(replayed) = replayed else {
            panic!("a hit must publish the stored artifact");
        };
        assert_eq!(
            replayed.finish().unwrap().digest(),
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

        let m = metrics_of::<Verify>(&case);
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

    /// THE false-hit hazard: nothing substitutes a case into a translate prompt, so `input_tree`
    /// is the only per-case component of its key — and digested as a phase dir, the root-anchored
    /// rules drop every `*.bak`, `*.log` and `*.sha256`. Every case of a battery would then
    /// collide on one key and be served another's translation, with nothing downstream to notice.
    #[test]
    fn two_corpora_differing_only_in_an_ignored_file_do_not_share_a_translate_key() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let keys = Keys::new(tmp.path());
        let (a, b) = (
            corpus(&tmp.path().join("a"), "one"),
            corpus(&tmp.path().join("b"), "two"),
        );
        let digest_of = |c: &Path| {
            IsolatedWorkDir::<Translate>::from_corpus(c)
                .unwrap()
                .input_digest()
                .clone()
        };
        let (da, db) = (digest_of(&a), digest_of(&b));
        assert_ne!(
            keys.inputs::<Translate>(&da).key(),
            keys.inputs::<Translate>(&db).key(),
            "an ignored-at-root file still changes what the agent is given to translate"
        );

        // ...and the naive spelling really would have collided, so this cannot pass
        // vacuously: `from_cache` is `digest_tree` over a path, reached from here.
        assert_eq!(
            Sealed::<Translate>::from_cache(&a).unwrap().digest(),
            Sealed::<Translate>::from_cache(&b).unwrap().digest(),
            "fixture assumption: digest_tree is the hashing that drops the .bak"
        );
    }

    /// `phase` comes from `P::DIR` and not a `&str` the caller passes, so no otherwise identical
    /// request crosses phases: a verify sweep replaying its own translations would publish the
    /// pre-verify crate as `verified/` and score it as verified.
    #[test]
    fn a_translate_entry_cannot_serve_a_verify_request() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let keys = Keys::new(tmp.path());
        let input = TreeDigest::for_test("sha256:the-same-tree");
        assert_ne!(
            keys.inputs::<Translate>(&input).key(),
            keys.inputs::<Verify>(&input).key(),
            "the phase must separate two requests that agree on everything else"
        );
        assert_eq!(
            keys.inputs::<Verify>(&input).key(),
            keys.inputs::<Verify>(&input).key(),
            "the builder varies nothing but the phase, so the inequality above IS the phase"
        );
    }

    /// The whole path a translation takes, twice: corpus → agent → seal → store → publish →
    /// metrics, then the same inputs again. This is the $795.59 a harvest-bench sweep re-paid per
    /// pass; a replay leaving no `translation.log` behind makes the next sweep pay it again, and
    /// one omitting `replayed`/`cache_key` reports the original's cost as this run's.
    #[test]
    fn a_replayed_translation_publishes_and_restores_the_transcript_a_fresh_run_would_have_teed() {
        let f = fixture();
        let case = f.case.clone();
        let keys = Keys::new(&f.repo);
        let store = Store::open(&f.repo, Mode::ReadWrite).unwrap();
        let corpus = corpus(&f.repo.join("corpus"), "upstream");
        let log = phase_dir(&case, TRANSLATED).join("logs/translation.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "the transcript the invocation teed\n").unwrap();

        const FIXED: &str = "pub fn a() { /* translated */ }";
        let translate = |work: IsolatedWorkDir<Translate>| -> Result<Attempt<Translate>> {
            let crate_dir = work.translated_rust();
            std::fs::create_dir_all(crate_dir.join("src"))?;
            std::fs::write(crate_dir.join("Cargo.toml"), "[package]\nname=\"x\"")?;
            std::fs::write(crate_dir.join("src/lib.rs"), FIXED)?;
            // Where a real `cargo build` leaves it: the tree that gets sealed. Planted in
            // `translated/` it proves nothing — `publish` clears that dir before it copies.
            std::fs::create_dir_all(crate_dir.join("target/debug"))?;
            std::fs::write(crate_dir.join("target/debug/junk"), "build output")?;
            Ok(Attempt::Produced(Produced::new(
                work.finish(&Completed::for_test())?,
                log.clone(),
                serde_json::json!({"agent": "claude", "duration_secs": 42}),
            )))
        };
        let fresh = run_cached(
            phase_run(
                &case,
                &log,
                &keys,
                IsolatedWorkDir::<Translate>::from_corpus(&corpus).unwrap(),
            ),
            &store,
            translate,
        )
        .unwrap();
        let Outcome::Published(fresh) = fresh else {
            panic!("a completed run must publish");
        };
        let fresh = post_processed(fresh);
        let published = phase_dir(&case, TRANSLATED).join("src/lib.rs");
        assert_eq!(std::fs::read_to_string(&published).unwrap(), FIXED);
        assert!(
            phase_dir(&case, TRANSLATED)
                .join("c_src/doc/footer.html.bak")
                .is_file(),
            "the corpus travels into the translation: it is the oracle the scorer builds"
        );
        assert!(
            !phase_dir(&case, TRANSLATED).join("target").exists(),
            "build output is regenerable and bakes in a dead scratch path, so neither a \
             fresh run nor a replay may publish it"
        );
        let first = metrics_of::<Translate>(&case);
        assert_eq!(first["replayed"], serde_json::json!(false));

        std::fs::remove_file(&log).unwrap();
        assert!(
            !log.exists(),
            "the fixture must remove what the replay restores"
        );

        let replayed = run_cached(
            phase_run(
                &case,
                &log,
                &keys,
                IsolatedWorkDir::<Translate>::from_corpus(&corpus).unwrap(),
            ),
            &store,
            |_| panic!("the agent must NOT run on a hit — that is the entire point"),
        )
        .unwrap();
        let Outcome::Published(replayed) = replayed else {
            panic!("a hit must publish the stored artifact");
        };
        assert_eq!(
            post_processed(replayed).digest(),
            fresh.digest(),
            "a replay must publish the artifact that was stored"
        );
        assert_eq!(std::fs::read_to_string(&published).unwrap(), FIXED);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap_or_default(),
            "the transcript the invocation teed\n",
            "without the restored transcript the skip check misses this case and the next \
             sweep pays for it again"
        );

        let m = metrics_of::<Translate>(&case);
        assert_eq!(
            m["replayed"],
            serde_json::json!(true),
            "a replay recorded as a fresh run reports the original's spend as this run's: {m}"
        );
        assert_eq!(
            m["cache_key"], first["cache_key"],
            "a replay must name the entry the original wrote: {m}"
        );
        assert!(
            m["cache_key"].as_str().is_some_and(|k| !k.is_empty()),
            "and that entry must actually be named: {m}"
        );
        assert_eq!(
            m["duration_secs"], 42,
            "a replay reports the ORIGINAL invocation's cost, not a blank"
        );
    }
}
