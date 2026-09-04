//! ONE agent invocation: a working dir and a prompt in, a working dir out.
//!
//! Nothing here knows where in a chain it sits. Translate and verify are not different kinds of
//! operation -- they differ only in the prompt handed to them -- which is why `phase` is not key
//! material and why adding a third step needs no new concept.

use crate::domain::health::Health;
use crate::store::{AgentRecord, Key, Mode, ModelId, Outcome, Prompt, Store};
use crate::tree::{Tree, WorkDir};
use anyhow::Result;
use std::path::Path;

/// What a backend does with a working dir.
///
/// A trait, so this module cannot know which backend it got: the CLI sessions, the docker baselines
/// and the single-shot API calls all live in `runners/` and differ in nothing visible from here.
pub trait Execute {
    /// Run, teeing the transcript to `log`. Classifying it is the backend's job because only the
    /// backend knows its own log format -- kiro writes prose, codex and claude write different JSON.
    fn execute(&self, work: &WorkDir, prompt: &str, log: &Path) -> Result<Ran>;
}

/// What one execution reported about itself, all of it read from the transcript rather than from an
/// exit code: every session pipes through `tee`, so a killed agent reports a clean 0.
pub struct Ran {
    pub health: Health,
    pub wall_secs: u64,
    pub cost_usd: Option<f64>,
    pub cli: String,
    /// Required, with no default, so a backend cannot return a `Ran` without having checked that the
    /// CLI honoured its model pin -- see [`crate::domain::health::PinReport`].
    pub pin: crate::domain::health::PinReport,
}

/// How an invocation executes. A struct, not an enum: the `Baseline` arm carried no model, so
/// `Invocation::key` returned `Option<Key>` and every store call had to handle a `None` that only the
/// transpilers and docker baselines could produce. They are gone, so EVERY run is keyed and the
/// `Option` with it -- "an unkeyed run cannot be written to the store" is now true because an unkeyed
/// run cannot be spelled.
pub struct Runner<'a> {
    pub model: &'a ModelId,
    pub exec: &'a dyn Execute,
}

pub struct Invocation<'a> {
    pub tool: &'a str,
    pub prompt: &'a Prompt,
    pub runner: Runner<'a>,
}

impl<'a> Invocation<'a> {
    fn key(&'a self, before: &'a Tree) -> Key<'a> {
        Key {
            tool: self.tool,
            model: self.runner.model.as_str(),
            before: before.digest(),
            prompt: self.prompt,
        }
    }
}

/// The outcome of one step of a chain.
pub enum Produced {
    /// There is a tree to hand to the next step, whether it was served or paid for.
    Done {
        after: Tree,
        record: AgentRecord,
        replayed: bool,
    },
    /// The agent ran and did not finish. RECORDED, so the failure stays inspectable, but not
    /// served: a tree from a run that never completed is not a measurement.
    DidNotComplete(AgentRecord),
    /// No stored entry and [`Mode::ReplayOnly`] forbade paying, so there is no run to record.
    Unavailable,
}

/// Look the invocation up; run it only if the store cannot answer.
///
/// One branch, on whether a key exists at all. A replay and a fresh run leave by the same path, so
/// there is no "cached" variant to keep in step with an uncached one.
pub fn run_or_replay(
    inv: &Invocation<'_>,
    before: &Tree,
    store: &Store,
    corpus_c: &crate::tree::Corpus,
    log: &Path,
) -> Result<Produced> {
    let key = inv.key(before);

    if let Some(hit) = store.lookup(&key)? {
        // Put the transcript back where the run would have written it. Without this a replay left
        // `logs/<role>.log` as whatever filesystem history happened to hold -- the ONE part of
        // `results/` that was not a function of the store, and the reason a fresh checkout's published
        // logs came from git rather than from the entry being replayed. `agent_health::audit` reads
        // exactly these files, so it was auditing a different run's evidence.
        hit.republish_transcript(log)?;
        return Ok(Produced::Done {
            after: hit.after,
            record: hit.record,
            replayed: true,
        });
    }
    // Above the execution, which is where the money is: refusing instead IS the mode.
    if store.mode() == Mode::ReplayOnly {
        // A TERMINAL answer replays as itself; `Infra`/`Unknown` have nothing to serve.
        if let Some(record) = store.terminal_record(&key)? {
            return Ok(Produced::DidNotComplete(record));
        }
        return Ok(Produced::Unavailable);
    }

    let work = before.materialise(corpus_c)?;
    let ran = inv.runner.exec.execute(&work, inv.prompt.text(), log)?;
    store.count(|c| c.invocations += 1);
    let outcome = Outcome::from(&ran.health);

    if outcome != Outcome::Completed {
        let record = AgentRecord {
            outcome,
            output_tree: None,
            wall_secs: ran.wall_secs,
            cost_usd: ran.cost_usd,
            cli: ran.cli,
            pin: ran.pin,
        };
        // Recorded even though it is not servable: a re-run replaces it, and until then it is the
        // only account of what went wrong. This is what removes the `failed/` subtree.
        if let Err(e) = store.write(&key, before, None, &record, Some(log)) {
            eprintln!("  cache: the failed run was NOT recorded: {e:#}");
        }
        return Ok(Produced::DidNotComplete(record));
    }

    let after = work.seal()?;
    let record = AgentRecord {
        outcome,
        output_tree: Some(after.digest().as_str().to_string()),
        wall_secs: ran.wall_secs,
        cost_usd: ran.cost_usd,
        cli: ran.cli,
        pin: ran.pin,
    };
    // LOUD BUT NOT FATAL. By this line the agent has run and the money is spent -- a measured
    // $795.59 per harvest-bench sweep -- and `after` exists. Propagating a store failure would
    // discard a paid artifact to protect an optimisation. Storing is the optimisation; the tree is
    // the deliverable, and a failed store costs exactly one future miss.
    if let Err(e) = store.write(&key, before, Some(&after), &record, Some(log)) {
        eprintln!("  cache: NOT stored, continuing anyway: {e:#}");
    }
    Ok(Produced::Done {
        after,
        record,
        replayed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A backend that writes a fixed translation and counts how often it was asked to.
    struct Fake {
        calls: AtomicUsize,
        health: fn() -> Health,
        writes: &'static str,
    }

    impl Fake {
        fn completing(writes: &'static str) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                health: || Health::Completed,
                writes,
            }
        }

        fn exhausted() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                health: || Health::Exhausted { secs: 43_200 },
                writes: "partial\n",
            }
        }

        fn failing() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                health: || Health::Infra {
                    reason: "api_error".into(),
                    detail: "terminal_reason=api_error".into(),
                },
                writes: "partial\n",
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Execute for Fake {
        fn execute(&self, work: &WorkDir, _prompt: &str, log: &Path) -> Result<Ran> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(work.translation().join("lib.rs"), self.writes)?;
            std::fs::write(log, "transcript\n")?;
            Ok(Ran {
                pin: crate::domain::health::PinReport::NotReported,
                health: (self.health)(),
                wall_secs: 7,
                cost_usd: Some(1.25),
                cli: "fake 1.0".into(),
            })
        }
    }

    struct Fixture {
        _repo: tempfile::TempDir,
        _corpus: tempfile::TempDir,
        corpus_c: crate::tree::Corpus,
        log: std::path::PathBuf,
        first: Tree,
    }

    fn fixture() -> Fixture {
        let repo = crate::io::workdir::test_tempdir().unwrap();
        let corpus = crate::io::workdir::test_tempdir().unwrap();
        let corpus_dir = corpus.path().join("test_case");
        std::fs::create_dir_all(&corpus_dir).unwrap();
        std::fs::write(corpus_dir.join("lib.c"), "int f(void);\n").unwrap();
        let corpus_c = crate::tree::Corpus::at(&corpus_dir).unwrap();
        let log = repo.path().join("run.log");
        let first = WorkDir::assemble(&corpus_c).unwrap().seal().unwrap();
        Fixture {
            _repo: repo,
            _corpus: corpus,
            corpus_c,
            log,
            first,
        }
    }

    fn model() -> ModelId {
        ModelId::new("global.anthropic.claude-opus-5[1m]").unwrap()
    }

    #[test]
    fn the_second_run_of_one_invocation_serves_the_stored_tree_and_pays_nothing() {
        let f = fixture();
        let store = Store::open(f._repo.path(), Mode::ReadWrite).unwrap();
        let exec = Fake::completing("pub fn f() {}\n");
        let m = model();
        let prompt = Prompt::new("translate");
        let inv = Invocation {
            tool: "claude",
            prompt: &prompt,
            runner: Runner {
                model: &m,
                exec: &exec,
            },
        };

        let first = run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap();
        let Produced::Done {
            after, replayed, ..
        } = first
        else {
            panic!("the first run must produce a tree")
        };
        assert!(!replayed);
        assert_eq!(exec.calls(), 1);

        let second = run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap();
        let Produced::Done {
            after: again,
            replayed,
            ..
        } = second
        else {
            panic!("the second run must be served")
        };
        assert!(replayed, "and must say so");
        assert_eq!(exec.calls(), 1, "the agent must NOT have run again");
        assert_eq!(again.digest(), after.digest());
    }

    /// A settled answer replays without paying; a lost run still refuses.
    #[test]
    fn a_stored_terminal_answer_replays_and_a_stored_infra_failure_does_not() {
        let f = fixture();
        let m = model();
        let prompt = Prompt::new("translate");

        for (exec, servable) in [(Fake::exhausted(), true), (Fake::failing(), false)] {
            let store = Store::open(f._repo.path(), Mode::ReadWrite).unwrap();
            let inv = Invocation {
                tool: "kiro",
                prompt: &prompt,
                runner: Runner {
                    model: &m,
                    exec: &exec,
                },
            };
            let first = run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap();
            assert!(
                matches!(first, Produced::DidNotComplete(_)),
                "the fixture must not complete"
            );
            assert_eq!(exec.calls(), 1);

            // Same key, now under the mode `reproduce.sh` uses: nothing may be paid for.
            let replay = Store::open(f._repo.path(), Mode::ReplayOnly).unwrap();
            let again = run_or_replay(&inv, &f.first, &replay, &f.corpus_c, &f.log).unwrap();
            assert_eq!(exec.calls(), 1, "a replay must never invoke the agent");
            if servable {
                let Produced::DidNotComplete(record) = again else {
                    panic!("a terminal answer must replay as itself")
                };
                assert!(matches!(
                    record.outcome,
                    crate::store::Outcome::Exhausted { .. }
                ));
            } else {
                assert!(
                    matches!(again, Produced::Unavailable),
                    "a lost run has no measurement to serve"
                );
            }
            std::fs::remove_dir_all(f._repo.path().join("results/.cache")).ok();
        }
    }

    #[test]
    fn a_different_prompt_against_the_same_tree_is_a_different_invocation() {
        // This is the whole reason `phase` is not key material: a chain's second step is the same
        // function at a different prompt, and the key already separates them.
        let f = fixture();
        let store = Store::open(f._repo.path(), Mode::ReadWrite).unwrap();
        let exec = Fake::completing("pub fn f() {}\n");
        let m = model();
        let (p1, p2) = (Prompt::new("translate"), Prompt::new("verify"));
        let mut inv = Invocation {
            tool: "claude",
            prompt: &p1,
            runner: Runner {
                model: &m,
                exec: &exec,
            },
        };
        run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap();
        inv.prompt = &p2;
        run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap();
        assert_eq!(exec.calls(), 2, "each prompt is its own entry");
    }

    #[test]
    fn replay_only_refuses_a_miss_rather_than_paying_for_it() {
        // `reproduce.sh` must be incapable of spending money: a miss there means the stored results
        // no longer answer the question, not that it should go and ask again.
        let f = fixture();
        let store = Store::open(f._repo.path(), Mode::ReplayOnly).unwrap();
        let exec = Fake::completing("pub fn f() {}\n");
        let m = model();
        let prompt = Prompt::new("translate");
        let inv = Invocation {
            tool: "claude",
            prompt: &prompt,
            runner: Runner {
                model: &m,
                exec: &exec,
            },
        };
        assert!(matches!(
            run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap(),
            Produced::Unavailable
        ));
        assert_eq!(exec.calls(), 0, "no agent may be invoked");
    }

    #[test]
    fn a_run_that_did_not_complete_is_recorded_and_retried_rather_than_served() {
        let f = fixture();
        let store = Store::open(f._repo.path(), Mode::ReadWrite).unwrap();
        let failing = Fake::failing();
        let m = model();
        let prompt = Prompt::new("translate");
        let inv = Invocation {
            tool: "claude",
            prompt: &prompt,
            runner: Runner {
                model: &m,
                exec: &failing,
            },
        };
        let out = run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap();
        let Produced::DidNotComplete(record) = out else {
            panic!("an infra failure must not produce a tree")
        };
        assert!(matches!(record.outcome, Outcome::Infra { .. }));
        assert_eq!(record.output_tree, None);

        run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap();
        assert_eq!(
            failing.calls(),
            2,
            "the recorded failure must not answer the next lookup"
        );
    }
}
