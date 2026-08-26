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
}

/// How an invocation executes, and whether the store may name it.
///
/// The model lives in the `Agent` arm, so a `Baseline` has nothing to key on and
/// [`Invocation::key`] cannot produce one for it: storing an unkeyed run is unrepresentable rather
/// than something a caller must remember not to do. Only agentic runs are worth memoising -- a
/// transpiler is deterministic, and a docker baseline is cheap next to an iterating agent.
pub enum Runner<'a> {
    Agent {
        model: &'a ModelId,
        exec: &'a dyn Execute,
    },
    Baseline {
        exec: &'a dyn Execute,
    },
}

impl<'a> Runner<'a> {
    fn exec(&self) -> &'a dyn Execute {
        match self {
            Runner::Agent { exec, .. } | Runner::Baseline { exec } => *exec,
        }
    }
}

pub struct Invocation<'a> {
    pub tool: &'a str,
    pub prompt: &'a Prompt,
    pub runner: Runner<'a>,
}

impl<'a> Invocation<'a> {
    /// `None` where the runner is not keyed. There is no second way to reach the store, so an
    /// unkeyed run cannot be written to it.
    fn key(&'a self, before: &'a Tree) -> Option<Key<'a>> {
        match &self.runner {
            Runner::Agent { model, .. } => Some(Key {
                tool: self.tool,
                model: model.as_str(),
                before: before.digest(),
                prompt: self.prompt,
            }),
            Runner::Baseline { .. } => None,
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
    corpus_c: &Path,
    log: &Path,
) -> Result<Produced> {
    let key = inv.key(before);

    if let Some(key) = &key {
        if let Some(hit) = store.lookup(key)? {
            return Ok(Produced::Done {
                after: hit.after,
                record: hit.record,
                replayed: true,
            });
        }
        // Above the execution, which is where the money is: refusing instead IS the mode.
        if store.mode() == Mode::ReplayOnly {
            return Ok(Produced::Unavailable);
        }
    }

    let work = before.materialise(corpus_c)?;
    let ran = inv.runner.exec().execute(&work, inv.prompt.text(), log)?;
    store.count(|c| c.invocations += 1);
    let outcome = Outcome::from(&ran.health);

    if outcome != Outcome::Completed {
        let record = AgentRecord {
            outcome,
            output_tree: None,
            wall_secs: ran.wall_secs,
            cost_usd: ran.cost_usd,
            cli: ran.cli,
        };
        // Recorded even though it is not servable: a re-run replaces it, and until then it is the
        // only account of what went wrong. This is what removes the `failed/` subtree.
        if let Some(key) = &key {
            if let Err(e) = store.write(key, before, None, &record, Some(log)) {
                eprintln!("  cache: the failed run was NOT recorded: {e:#}");
            }
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
    };
    // LOUD BUT NOT FATAL. By this line the agent has run and the money is spent -- a measured
    // $795.59 per harvest-bench sweep -- and `after` exists. Propagating a store failure would
    // discard a paid artifact to protect an optimisation. Storing is the optimisation; the tree is
    // the deliverable, and a failed store costs exactly one future miss.
    if let Some(key) = &key {
        if let Err(e) = store.write(key, before, Some(&after), &record, Some(log)) {
            eprintln!("  cache: NOT stored, continuing anyway: {e:#}");
        }
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
        corpus_c: std::path::PathBuf,
        log: std::path::PathBuf,
        first: Tree,
    }

    fn fixture() -> Fixture {
        let repo = crate::io::workdir::test_tempdir().unwrap();
        let corpus = crate::io::workdir::test_tempdir().unwrap();
        let corpus_c = corpus.path().join("test_case");
        std::fs::create_dir_all(&corpus_c).unwrap();
        std::fs::write(corpus_c.join("lib.c"), "int f(void);\n").unwrap();
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
            runner: Runner::Agent {
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
            runner: Runner::Agent {
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
    fn an_unkeyed_runner_is_never_stored_and_never_replayed() {
        // Only agentic runs are keyed. A baseline has no model to key on, so `key` yields nothing
        // and there is no path from here into the store.
        let f = fixture();
        let store = Store::open(f._repo.path(), Mode::ReadWrite).unwrap();
        let exec = Fake::completing("pub fn f() {}\n");
        let prompt = Prompt::new("transpile");
        let inv = Invocation {
            tool: "c2rust",
            prompt: &prompt,
            runner: Runner::Baseline { exec: &exec },
        };
        run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap();
        run_or_replay(&inv, &f.first, &store, &f.corpus_c, &f.log).unwrap();
        assert_eq!(exec.calls(), 2, "an unkeyed run cannot be replayed");
        let entries = std::fs::read_dir(f._repo.path().join("results/.cache"))
            .unwrap()
            .count();
        assert_eq!(entries, 0, "and must leave the store empty");
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
            runner: Runner::Agent {
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
            runner: Runner::Agent {
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
