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
    let roles = prompt::chain(paths.tool, paths.variant);
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
        )?
        .with_context(|| {
            format!(
                "{:?}/{:?} has no {role:?} prompt for a {:?} case, yet the chain schedules one",
                c.paths.tool, c.paths.variant, job.shape
            )
        })?;
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
        // The permit spans the invocation only. Held across scoring it would serialise a sweep on the
        // slowest case's build rather than on its agent.
        let produced = {
            let _permit = c.pool.acquire();
            run_or_replay(&invocation, &tree, c.store, &corpus_c, &log)?
        };
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

        // Publish, then transform. The transform is deterministic and outside the cache, so the tree
        // the NEXT step keys on is `transform(after)` and not `after` itself.
        publish(&after, &phase_dir)?;
        crate::transform::post_process(&phase_dir, &job.artifact)?;
        tree = reseal(&phase_dir, &corpus_c)?;
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

/// Re-seal the published-and-transformed crate, so the next step's input tree is exactly what is on
/// disk after the transform rather than what the agent left.
fn reseal(phase_dir: &Path, corpus_case: &Path) -> Result<Tree> {
    let work = WorkDir::assemble(corpus_case)?;
    crate::tree::copy_plain(phase_dir, &work.translation())?;
    work.seal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

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
