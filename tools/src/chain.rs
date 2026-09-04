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
use crate::tree::{Tree, WorkDir, TRANSLATION};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Everything the chain needs about one case, stated by the dataset that knows its layout. The driver
/// derived it itself, Test-Corpus-shaped -- see [`crate::benchmark::Benchmark::jobs`].
pub struct Job {
    pub name: String,
    /// The case root: `test_case/` and, for Test-Corpus, `CMakePresets.json` sit directly inside it.
    pub case_inputs: PathBuf,
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
    pub case_dir: PathBuf,
}

/// Attempts per invocation on a transient provider failure, and the backoff. Per HARNESS, not per tool.
const TRANSIENT_ATTEMPTS: usize = 3;
const BACKOFF_SECS: u64 = 30;

/// Both datasets spell the C the same way.
fn corpus_case(case_inputs: &Path) -> PathBuf {
    case_inputs.join("test_case")
}

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
    let corpus_c = corpus_case(&job.case_inputs);
    let work_base = crate::io::workdir::base()?;
    let mut tree = WorkDir::assemble(&corpus_c)
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
        let phase_dir = job.case_dir.join(role.dir());
        std::fs::create_dir_all(phase_dir.join("logs"))?;
        let log = phase_dir.join("logs").join(role.log());

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
                run_or_replay(&invocation, &tree, c.store, &corpus_c, &log)?
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
                let empty = reseal(&phase_dir, &corpus_c)?;
                published.push((phase_dir.clone(), empty));
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
        publish(&after, &phase_dir)?;
        crate::transform::post_process(&phase_dir, &job.artifact)?;
        tree = next_input(&after, &job.artifact, &corpus_c)?;
        published.push((phase_dir.clone(), tree.clone()));

        // Per step: publishability is checked per role, so each must serve its followers.
        for follower in &job.followers {
            let follower_dir = follower.case_dir.join(role.dir());
            crate::transform::propagate_config(&phase_dir, &follower_dir, &follower.cfg)
                .with_context(|| {
                    format!(
                        "deriving {} from {} for {role:?}",
                        follower.cfg.name, job.name
                    )
                })?;
            let follower_tree = reseal(&follower_dir, &corpus_case(&follower.case_inputs))?;
            published.push((follower_dir, follower_tree));
        }
    }
    Ok(CaseOutcome { published, refused })
}

/// Write the step's crate into the results tree. Only the translation: `c_src/` is the pinned corpus
/// and re-derived wherever a working dir is assembled, so publishing it would store the same bytes
/// once per case per step.
fn publish(tree: &Tree, phase_dir: &Path) -> Result<()> {
    tree.copy_subtree_into(TRANSLATION, phase_dir)
        .with_context(|| format!("publishing into {}", phase_dir.display()))
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
    corpus_case: &Path,
) -> Result<Tree> {
    let work = after.materialise(corpus_case)?;
    crate::transform::post_process(&work.translation(), artifact)?;
    work.seal()
}

/// Seal what is on disk in `phase_dir`. Only for a REFUSED or EXHAUSTED step, which publishes no
/// artifact and therefore has no stored tree to derive one from.
fn reseal(phase_dir: &Path, corpus_case: &Path) -> Result<Tree> {
    let work = WorkDir::assemble(corpus_case)?;
    crate::tree::copy_plain(phase_dir, &work.translation())?;
    work.seal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    /// A results tree is OUTPUT: residue in it must not move the next step's key.
    ///
    /// `reseal` read the published dir back, so any file the seal hashes but `.gitignore` skips changed
    /// the hash in a fresh checkout. Measured: codex lost all 128 P01_sphincs_plus cases in CI,
    /// 332/338 -> 204/338, over `.cargo/config.toml` and `Cargo.lock`. Non-vacuity asserts the same
    /// residue really does move a disk-derived seal, so the fixture carries the trap it claims to.
    #[test]
    fn residue_in_the_published_tree_cannot_move_the_next_steps_input() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let corpus_case = tmp.path().join("test_case");
        std::fs::create_dir_all(&corpus_case).unwrap();
        std::fs::write(corpus_case.join("lib.c"), "int f(void){return 1;}\n").unwrap();

        let work = WorkDir::assemble(&corpus_case).unwrap();
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
        let phase_dir = tmp.path().join("published");
        publish(&after, &phase_dir).unwrap();
        crate::transform::post_process(&phase_dir, &artifact).unwrap();

        let from_store = next_input(&after, &artifact, &corpus_case).unwrap();
        let from_disk = reseal(&phase_dir, &corpus_case).unwrap();

        // Exactly the class of file `.gitignore` skipped and the seal hashed.
        std::fs::create_dir_all(phase_dir.join(".cargo")).unwrap();
        std::fs::write(
            phase_dir.join(".cargo/config.toml"),
            "[net]\noffline = true\n",
        )
        .unwrap();

        assert_eq!(
            next_input(&after, &artifact, &corpus_case)
                .unwrap()
                .digest(),
            from_store.digest(),
            "the next input must be a function of the STORE, not of the results tree"
        );
        assert_ne!(
            reseal(&phase_dir, &corpus_case).unwrap().digest(),
            from_disk.digest(),
            "non-vacuity: this residue must really move a disk-derived seal"
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
