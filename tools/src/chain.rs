//! The pipeline: a chain of agent invocations over every case in scope.
//!
//! One driver. This replaces `translate::run_test_corpus`, `verify::run_all`,
//! `verify::run_with_semaphore` and BOTH `run_harvest_bench` functions, which existed only because
//! translate and verify were modelled as different kinds of operation. They are the same function at
//! different prompts, so there is one loop:
//!
//! ```text
//! tree = assemble(corpus).seal()
//! for role in prompt::chain(tool, variant):
//!     tree = run_or_replay(invocation(role), tree)
//!     publish(tree, role); transform(published)
//! ```
//!
//! `prompt::chain` is the single declaration of chain length. Adding a third step needs no new type,
//! no new trait method and no new branch anywhere downstream.

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

/// A shared-source follower: this job's ONE translation, rebuilt under another CMake configuration.
/// Paths stated, not derived: deriving them is how followers came to be dropped entirely.
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

/// Run the chain for every case of one unit.
///
/// `steps` truncates the chain, which is what the old `translate`/`verify` subcommands were for: a
/// prefix of one pipeline rather than two pipelines.
pub fn run_unit(
    paths: &Paths,
    store: &Store,
    unit: &str,
    jobs: &[Job],
    steps: Option<usize>,
    pool: &crate::agents::Pool,
) -> Result<Ran> {
    let roles = prompt::chain(paths.tool, paths.variant);
    let roles = &roles[..steps.map_or(roles.len(), |n| n.min(roles.len()))];

    // Concurrent, bounded by the pool's width: the loop here was sequential, so `--parallel` bought
    // nothing at all -- a permit acquired inside a sequential loop is never contended. Workers pull
    // from one queue rather than a thread per case, so 338 cases do not become 338 threads.
    let queue = std::sync::Mutex::new(jobs.iter());
    let collected: std::sync::Mutex<Ran> = std::sync::Mutex::new(Ran {
        resolved: Resolved::new(),
        failures: Vec::new(),
        refused: Vec::new(),
    });

    std::thread::scope(|scope| {
        for _ in 0..pool.width().max(1) {
            scope.spawn(|| loop {
                let Some(job) = queue.lock().expect("case queue").next() else {
                    return;
                };
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
        }
    });

    Ok(collected.into_inner().expect("collected results"))
}

/// The per-case parameters.
struct RunCase<'a> {
    paths: &'a Paths,
    store: &'a Store,
    roles: &'a [Role],
    job: &'a Job,
    pool: &'a crate::agents::Pool,
}

/// One case, all the way along its chain.
///
/// The tree returned by each step is the tree handed to the next. Nothing consults the filesystem to
/// find the previous step's output: reading `verified/` off disk is what once scored a five-day-old
/// artifact as this run's.
/// What one case's chain produced. A struct rather than a tuple of two `Vec`s, which transpose
/// silently.
struct CaseOutcome {
    published: Vec<(PathBuf, Tree)>,
    refused: Vec<String>,
}

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
