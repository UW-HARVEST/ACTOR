//! The pipeline: a chain of agent invocations over every case in scope.
//!
//! One driver. It replaces `translate::run_test_corpus`, `verify::run_all`,
//! `verify::run_with_semaphore` and BOTH `run_harvest_bench` functions, which existed only because
//! translate and verify were modelled as different kinds of operation. They are the same function at
//! different prompts, so there is one loop -- assemble, then per role replay-or-run, publish and
//! transform. `prompt::chain` is the single declaration of chain length: a third step needs no new
//! type, no new trait method and no new branch anywhere downstream.

use crate::battery::{self, Paths};
use crate::eval::Resolved;
use crate::invocation::{run_or_replay, Invocation, Produced};
use crate::prompt::{self, Role, Shape};
use crate::store::{Prompt, Store};
use crate::tree::{Corpus, Tree, WorkDir, TRANSLATION};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Everything the chain needs about one case, stated by the dataset that knows its layout. The driver
/// derived it itself, Test-Corpus-shaped -- see [`crate::benchmark::Benchmark::jobs`].
pub struct Job {
    pub name: String,
    /// The case root: `test_case/` and, for Test-Corpus, `CMakePresets.json` sit directly inside it.
    pub case_inputs: PathBuf,
    /// The pinned C, checked once where the dataset derives it. A [`Corpus`] rather than a path
    /// because `seal` restores `c_src/` from it, so substituting it is silent -- see the type.
    pub corpus: Corpus,
    /// Where each role publishes. Not derivable from `case_inputs`: only one dataset nests.
    pub case_dir: PathBuf,
    pub shape: Shape,
    /// What the ORACLE expects to build -- a different question from the prompt's shape.
    pub artifact: crate::transform::Artifact,
    pub followers: Vec<Follower>,
}

/// A shared-source follower: this job's ONE translation under another CMake configuration. Paths
/// stated, not derived -- deriving them is how followers came to be dropped entirely.
pub struct Follower {
    pub cfg: battery::Config,
    pub case_inputs: PathBuf,
    pub corpus: Corpus,
    pub case_dir: PathBuf,
}

/// Attempts per invocation on a transient provider failure, and the backoff. Per HARNESS, not per tool.
const TRANSIENT_ATTEMPTS: usize = 3;
const BACKOFF_SECS: u64 = 30;

/// Every directory a unit's jobs publish into, FOLLOWERS INCLUDED.
///
/// One definition, because "did the store serve this unit" and "which records does enrich backfill"
/// need the same answer. Leaving followers out of the first voided B02_synthetic and P01 entirely.
pub fn case_dirs(jobs: &[Job]) -> Vec<&Path> {
    jobs.iter()
        .flat_map(|j| {
            std::iter::once(j.case_dir.as_path())
                .chain(j.followers.iter().map(|f| f.case_dir.as_path()))
        })
        .collect()
}

/// What one case's chain produced, per role, keyed by the directory it was published into.
pub struct Ran {
    pub resolved: Resolved,
    pub failures: Vec<String>,
    /// Cases whose provider declined on content grounds. Scored as failures, counted separately: a
    /// refusal is a fact about the provider's policy, not about the translation.
    pub refused: Vec<String>,
}

/// Run the chain for every case of every unit in scope.
///
/// ONE queue over all of them, not a loop over units each with its own queue: harvest-bench's unit is
/// a project holding ONE case, so `--parallel 3` ran three workers over a queue of one, seven times
/// over, and the dataset went one project at a time per tool. Test-Corpus gains the same way at each
/// battery boundary. `steps` truncates the chain -- a prefix of one pipeline, not two pipelines.
pub fn run_all(
    paths: &Paths,
    store: &Store,
    units: &[(String, Vec<Job>)],
    steps: Option<usize>,
    pool: &crate::agents::Pool,
) -> Result<Ran> {
    let roles = prompt::chain(paths.variant);
    let roles = &roles[..steps.map_or(roles.len(), |n| n.min(roles.len()))];

    let queue: Vec<(&str, &Job)> = units
        .iter()
        .flat_map(|(unit, jobs)| jobs.iter().map(move |j| (unit.as_str(), j)))
        .collect();
    let collected: std::sync::Mutex<Ran> = std::sync::Mutex::new(Ran {
        resolved: Resolved::new(),
        failures: Vec::new(),
        refused: Vec::new(),
    });

    in_flight(&queue, pool.width(), |&(unit, job)| {
        let outcome = run_case(RunCase {
            paths,
            store,
            roles,
            job,
            pool,
        });
        let mut out = collected.lock().expect("collected results");
        match outcome {
            Ok(done) => {
                out.resolved.extend(done.published);
                out.refused.extend(done.refused);
            }
            Err(e) => {
                println!("  \u{274c} {unit}/{}: {e:#}", job.name);
                out.failures.push(job.name.clone());
            }
        }
    });

    Ok(collected.into_inner().expect("collected results"))
}

/// Apply `f` to every item with at most `width` in flight. Workers PULL from one queue rather than a
/// thread per item, so 338 cases do not become 338 threads.
fn in_flight<T: Sync>(items: &[T], width: usize, f: impl Fn(&T) + Sync) {
    let queue = std::sync::Mutex::new(items.iter());
    std::thread::scope(|scope| {
        for _ in 0..width.max(1) {
            scope.spawn(|| loop {
                let Some(item) = queue.lock().expect("work queue").next() else {
                    return;
                };
                f(item);
            });
        }
    });
}

/// The per-case parameters.
struct RunCase<'a> {
    paths: &'a Paths,
    store: &'a Store,
    roles: &'a [Role],
    job: &'a Job,
    pool: &'a crate::agents::Pool,
}

/// What one case's chain produced. A struct, not a tuple of two `Vec`s, which transpose silently.
struct CaseOutcome {
    published: Vec<(PathBuf, Tree)>,
    refused: Vec<String>,
}

/// One case, all the way along its chain: the tree each step returns is the tree the next is handed.
/// Nothing consults the filesystem for the previous step's output -- reading `verified/` off disk is
/// what once scored a five-day-old artifact as this run's.
fn run_case(c: RunCase<'_>) -> Result<CaseOutcome> {
    let job = c.job;
    let work_base = crate::io::workdir::base()?;
    let mut tree = WorkDir::assemble(&job.corpus)
        .with_context(|| format!("laying out a working dir for {}", job.name))?
        .seal()?;
    let mut published = Vec::new();
    let mut refused: Vec<String> = Vec::new();

    for &role in c.roles {
        let text = prompt::read(
            &c.paths.repo_root,
            c.paths.tool,
            c.paths.variant,
            role,
            job.shape,
            &job.case_inputs,
        )?;
        let roots = crate::io::workdir::Roots::resolve(&work_base, &c.paths.repo_root);
        let prompt = Prompt::new(crate::store::normalise(&text, &roots));
        // Opened, which CLEARS it: a phase dir is output, and nothing ever emptied it, so sweep 1's
        // files survived sweep 2's publish.
        let publish = Publish::open(&job.case_dir.join(role.dir()))?;
        let log = publish.log(role)?;

        let runner = crate::runners::build(c.paths, role)?;
        let invocation = Invocation {
            tool: crate::cli::tool_dir(c.paths.tool),
            prompt: &prompt,
            runner: runner.as_runner(),
        };
        // Retried HERE, uniformly, because the CLIs cannot be: only claude's exposes a retry setting,
        // so resilience to a throttle was a property of the backend. This sweep lost kiro/pcre2 and
        // claude/libpng to transients, and a lost harvest-bench project is a row that never appears.
        // The permit spans ONE attempt, so a backoff holds no slot; `run_or_replay` re-runs rather than
        // serving a non-completed record, so the retry writes over it -- one key, still one entry.
        let mut produced;
        let mut attempt = 1;
        loop {
            produced = {
                let _permit = c.pool.acquire();
                run_or_replay(&invocation, &tree, c.store, &job.corpus, &log)?
            };
            let transient =
                matches!(&produced, Produced::DidNotComplete(r) if r.outcome.is_transient());
            if !transient || attempt >= TRANSIENT_ATTEMPTS {
                break;
            }
            println!(
                "  \u{27f3} {}: {role:?} attempt {attempt} of {TRANSIENT_ATTEMPTS} hit a transient \
                 provider failure, retrying",
                job.name
            );
            std::thread::sleep(std::time::Duration::from_secs(
                BACKOFF_SECS * attempt as u64,
            ));
            attempt += 1;
        }
        let after = match produced {
            Produced::Done { after, .. } => after,
            // A provider refusal is an ANSWER, not a lost entry. It is terminal and reproducible, so
            // the case is scored as a failure rather than voiding the whole battery the way a
            // transport blip does: one codex refusal discarded all 85 of its B01_synthetic cases.
            // Publishing nothing is what makes it a failure -- the oracle finds `translated_rust/`
            // with no crate in it, records a build failure, and the denominator stays whole.
            Produced::DidNotComplete(record)
                if matches!(
                    record.outcome,
                    crate::store::Outcome::Refused { .. } | crate::store::Outcome::Exhausted { .. }
                ) =>
            {
                println!(
                    "  \u{1f6ab} {}: {role:?} answered no: {:?}",
                    job.name, record.outcome
                );
                // The tree for a step that produced nothing is the FIRST tree of a chain: the corpus's
                // C and an empty translation. Derived from the corpus, not by reading the phase dir
                // back -- which is what `reseal` did, and which picked up whatever crate an earlier
                // sweep had left there, so a refusal was scored on someone else's artifact.
                let empty = WorkDir::assemble(&job.corpus)?.seal()?;
                published.push((publish.at().to_path_buf(), empty));
                refused.push(format!("{}/{role:?}", job.name));
                break;
            }
            Produced::DidNotComplete(record) => {
                anyhow::bail!(
                    "the {role:?} step did not complete: {:?}",
                    record.outcome
                )
            }
            Produced::Unavailable => anyhow::bail!(
                "--replay-only: no stored entry for the {role:?} step of {}, so nothing here is \
                 measured. The store does not cover this case at these inputs -- most often because \
                 the prompt or the model moved, and both are key components.",
                job.name
            ),
        };

        // Publish for the scorer and for a human, then derive the next step's input from the STORED
        // artifact -- never by reading the tree we just wrote. Output must not feed the next key.
        publish.write(&after)?;
        publish.transform(&job.artifact)?;
        tree = next_input(&after, &job.artifact, &job.corpus)?;
        published.push((publish.at().to_path_buf(), tree.clone()));

        // Per step: publishability is checked per role, so each must serve its followers. Each is
        // `transform(leader's STORED artifact, its own config)`, computed in scratch -- not a copy of
        // the leader's published directory, which carried whatever else was in it into 129 graded
        // trees.
        for follower in &job.followers {
            let into = Publish::open(&follower.case_dir.join(role.dir()))?;
            let derived =
                follower_input(&after, &follower.cfg, &follower.corpus).with_context(|| {
                    format!(
                        "deriving {} from {} for {role:?}",
                        follower.cfg.name, job.name
                    )
                })?;
            into.write(&derived)?;
            published.push((into.at().to_path_buf(), derived));
        }
    }
    Ok(CaseOutcome { published, refused })
}

/// Where a step publishes: WRITE-ONLY, by construction.
///
/// It yields no readable path -- `at` returns one for the `Resolved` map and for `logs/`, and
/// nothing here reads the directory's CONTENTS. That is the same trick [`Tree`] uses ("yields no path,
/// so nothing runs in one"), and it is what makes the defect this replaces unspellable: `reseal(&Path)
/// -> Tree` turned a published OUTPUT directory back into the next step's INPUT, its digest became a
/// cache key, and because `results/.gitignore` drops two files the digest covers, a fresh checkout
/// computed a different key -- codex 332/338 -> 204/338, all 128 P01 cases unservable.
///
/// `open` CLEARS the crate. Nothing ever did, so a re-run's publish overwrote what it had and left the
/// rest: sweep 1's `src/aes.rs` survived sweep 2, inflating `enrich`'s LOC and `unsafe` counts over
/// files no `lib.rs` declares. `logs/` survives, because the transcript is written before the artifact.
pub struct Publish(PathBuf);

impl Publish {
    pub fn open(dir: &Path) -> Result<Self> {
        for entry in std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
        {
            if entry.file_name() == "logs" {
                continue;
            }
            let at = entry.path();
            let removed = if entry.file_type().is_ok_and(|t| t.is_dir()) {
                std::fs::remove_dir_all(&at)
            } else {
                std::fs::remove_file(&at)
            };
            removed.with_context(|| format!("clearing {}", at.display()))?;
        }
        std::fs::create_dir_all(dir.join("logs"))
            .with_context(|| format!("preparing {}", dir.display()))?;
        Ok(Self(dir.to_path_buf()))
    }

    /// Where the transcript goes. From the one definition that names it.
    fn log(&self, role: Role) -> Result<PathBuf> {
        Ok(role.log_in(&self.0))
    }

    /// The directory itself, for the `Resolved` map the oracle looks a case up in. Not a licence to
    /// read it: no caller does, and `reseal` is gone.
    fn at(&self) -> &Path {
        &self.0
    }

    /// Only the translation: `c_src/` is the pinned corpus and re-derived wherever a working dir is
    /// assembled, so publishing it would store the same bytes once per case per step.
    fn write(&self, tree: &Tree) -> Result<()> {
        tree.copy_subtree_into(TRANSLATION, &self.0)
            .with_context(|| format!("publishing into {}", self.0.display()))
    }

    /// The harness transform, applied to what was just written. Deterministic and OUTSIDE the cache,
    /// so changing it never invalidates an entry that is still good.
    fn transform(&self, artifact: &crate::transform::Artifact) -> Result<()> {
        crate::transform::post_process(&self.0, artifact)
    }
}

/// The next step's input: `transform(stored artifact)`, computed in a scratch dir.
///
/// Derived from the STORE, never from the results tree. Reading the published dir back made the key
/// depend on whatever else happened to sit there -- residue from an earlier sweep, or a file the seal
/// hashes that `.gitignore` skips. Measured: codex lost all 128 P01_sphincs_plus cases in CI, 332/338
/// -> 204/338, because its artifact writes `.cargo/config.toml` and `Cargo.lock` and both were ignored,
/// so a fresh checkout hashed a different tree and the verify entry stopped being servable. A results
/// tree is OUTPUT; letting it feed the next key makes reproduction depend on filesystem history.
fn next_input(
    after: &Tree,
    artifact: &crate::transform::Artifact,
    corpus: &Corpus,
) -> Result<Tree> {
    let work = after.materialise(corpus)?;
    crate::transform::post_process(&work.translation(), artifact)?;
    work.seal()
}

/// A follower's tree: the leader's ONE translation under the follower's own CMake configuration.
///
/// A function of the leader's STORED artifact, computed in scratch. It used to be
/// `propagate_config(leader's published dir, ..)` followed by reading that directory back, so anything
/// else sitting in the leader's `translated/` reached 129 followers' graded trees.
fn follower_input(after: &Tree, cfg: &battery::Config, corpus: &Corpus) -> Result<Tree> {
    let work = after.materialise(corpus)?;
    crate::transform::apply_config(&work.translation(), cfg)?;
    work.seal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    /// A results tree is OUTPUT: residue in it must reach neither the next step's key nor the score,
    /// and a step must not inherit an earlier sweep's files.
    ///
    /// `reseal(&Path) -> Tree` read the published dir back, so any file the seal hashes but
    /// `.gitignore` skips changed the hash in a fresh checkout: codex lost all 128 P01_sphincs_plus
    /// cases in CI, 332/338 -> 204/338, over `.cargo/config.toml` and `Cargo.lock`. That function is
    /// gone and cannot be rewritten -- [`Publish`] yields no readable contents -- so what is left to
    /// pin is the other half: `Publish::open` CLEARS, and the next input is a function of the store.
    ///
    /// Non-vacuity is asserted twice: the residue is really there before `open`, and it is really
    /// hashed (the same bytes inside the tree DO move a digest), so neither claim is about a file the
    /// digest ignores.
    #[test]
    fn residue_in_a_published_dir_reaches_neither_the_next_key_nor_the_score() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let corpus_dir = tmp.path().join("test_case");
        std::fs::create_dir_all(&corpus_dir).unwrap();
        std::fs::write(corpus_dir.join("lib.c"), "int f(void){return 1;}\n").unwrap();
        let corpus = Corpus::at(&corpus_dir).unwrap();

        let work = WorkDir::assemble(&corpus).unwrap();
        std::fs::create_dir_all(work.translation().join("src")).unwrap();
        std::fs::write(
            work.translation().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(work.translation().join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let after = work.seal().unwrap();

        let artifact = crate::transform::Artifact::Cdylib {
            lib_name: "x".to_string(),
        };
        let dir = tmp.path().join("published");

        // An earlier sweep's output, including exactly the class of file `.gitignore` skipped and the
        // seal hashed, plus a source file no later translation has.
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join(".cargo/config.toml"), "[net]\noffline = true\n").unwrap();
        std::fs::write(dir.join("src/aes.rs"), "pub fn stale() {}\n").unwrap();
        std::fs::write(dir.join("Cargo.lock"), "# from sweep 1\n").unwrap();
        std::fs::write(dir.join("logs/translation.log"), "sweep 1\n").unwrap();
        assert!(dir.join("src/aes.rs").is_file(), "the trap must be present");

        let publish = Publish::open(&dir).unwrap();
        publish.write(&after).unwrap();
        publish.transform(&artifact).unwrap();

        // What the scorer will grade holds the published crate and NOTHING from sweep 1 --
        assert!(
            !dir.join("src/aes.rs").exists(),
            "a stale source file survived the publish"
        );
        assert!(!dir.join(".cargo").exists(), "stale build config survived");
        assert!(
            !dir.join("Cargo.lock").exists(),
            "a stale lock file survived"
        );
        assert!(
            dir.join("src/lib.rs").is_file(),
            "the new crate was published"
        );
        // -- except the transcript, which is written BEFORE the artifact and must survive.
        assert!(
            dir.join("logs/translation.log").is_file(),
            "clearing must not take the transcript with it"
        );

        // And the next step's input is a function of the STORE, not of that directory: putting the
        // residue back changes nothing.
        let before_residue = next_input(&after, &artifact, &corpus).unwrap();
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(dir.join(".cargo/config.toml"), "[net]\noffline = true\n").unwrap();
        assert_eq!(
            next_input(&after, &artifact, &corpus).unwrap().digest(),
            before_residue.digest(),
            "the next input must be a function of the store, not of the results tree"
        );

        // Non-vacuity: those same bytes INSIDE a tree really do move a digest, so the equality above
        // is not over a file the digest ignores.
        let moved = WorkDir::assemble(&corpus).unwrap();
        crate::tree::copy_plain(&dir, &moved.translation()).unwrap();
        std::fs::create_dir_all(moved.translation().join(".cargo")).unwrap();
        std::fs::write(
            moved.translation().join(".cargo/config.toml"),
            "[net]\noffline = true\n",
        )
        .unwrap();
        assert_ne!(
            moved.seal().unwrap().digest(),
            before_residue.digest(),
            "non-vacuity: this residue must really move a seal taken over it"
        );
    }

    /// The oracle must look for a transcript where the runner writes it, and the eval tree is not
    /// where either lives.
    ///
    /// `runtests` and `gtest` both derived the log path from `Materialised::crate_root`, an EVAL-TREE
    /// path. The eval tree is assembled by `copy_carrying`, which admits only
    /// `Disposition::StoreAndHash`, and every `*.log` is `Ignore` -- so `translated_rust/logs/` cannot
    /// exist, `extract_agent_meta` returned `None` (absence, not error), and 316 committed
    /// `result.json` files carry no cost, no model and no turn count. The second assertion is the
    /// non-vacuity: it proves the old derivation could never have worked, rather than merely that the
    /// new one does.
    #[test]
    fn the_oracle_reads_a_transcript_where_the_runner_wrote_it_and_never_from_the_eval_tree() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case_dir = tmp.path().join("B01/case_x");

        for role in [Role::Translate, Role::Verify] {
            let publish = Publish::open(&case_dir.join(role.dir())).unwrap();
            let written = publish.log(role).unwrap();
            std::fs::write(&written, "transcript\n").unwrap();
            assert_eq!(
                written,
                role.transcript_in(&case_dir),
                "{role:?}: the writer and the reader disagree about where the transcript is"
            );
            assert!(written.is_file(), "{role:?}: and it must really be there");
        }

        // Non-vacuity: a log cannot travel into a tree, so no eval-tree path can ever hold one.
        let corpus_dir = tmp.path().join("test_case");
        std::fs::create_dir_all(&corpus_dir).unwrap();
        std::fs::write(corpus_dir.join("lib.c"), "int f(void);\n").unwrap();
        let corpus = Corpus::at(&corpus_dir).unwrap();
        let work = WorkDir::assemble(&corpus).unwrap();
        std::fs::create_dir_all(work.translation().join("logs")).unwrap();
        std::fs::write(
            work.translation().join("logs").join(Role::Verify.log()),
            "transcript\n",
        )
        .unwrap();
        let sealed = work.seal().unwrap();
        let elsewhere = tmp.path().join("eval-crate");
        sealed.copy_subtree_into(TRANSLATION, &elsewhere).unwrap();
        assert!(
            !elsewhere.join("logs").exists(),
            "a transcript reached a tree, so deriving its path from one is not obviously wrong"
        );
    }

    /// Three items and a width of three are really in flight TOGETHER.
    ///
    /// No count can see this failure, so it measures the PEAK inside `f`, which sequential execution
    /// cannot raise above 1 however many items drain. Bounded rather than a barrier: a regression
    /// fails in a second instead of hanging the suite.
    #[test]
    fn items_are_in_flight_together_up_to_the_width() {
        let items: Vec<usize> = (0..3).collect();
        let (inside, peak, ran) = (
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        );
        in_flight(&items, 3, |_| {
            let now = inside.fetch_add(1, SeqCst) + 1;
            peak.fetch_max(now, SeqCst);
            for _ in 0..200 {
                if inside.load(SeqCst) >= 3 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            inside.fetch_sub(1, SeqCst);
            ran.fetch_add(1, SeqCst);
        });
        assert_eq!(
            peak.load(SeqCst),
            3,
            "three workers, three items, one queue"
        );
        assert_eq!(
            ran.load(SeqCst),
            3,
            "and every item still runs exactly once"
        );

        // More items than width still drains: the peak above is what proves concurrency, not this.
        let many: Vec<usize> = (0..8).collect();
        let count = AtomicUsize::new(0);
        in_flight(&many, 3, |_| {
            count.fetch_add(1, SeqCst);
        });
        assert_eq!(count.into_inner(), 8);
    }
}
