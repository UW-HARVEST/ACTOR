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

use crate::battery::{self, Case, Paths};
use crate::eval::Resolved;
use crate::invocation::{run_or_replay, Invocation, Produced};
use crate::prompt::{self, Role, Shape};
use crate::store::{Prompt, Store};
use crate::tree::{Tree, WorkDir, TRANSLATION};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

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
    filter: Option<&str>,
    steps: Option<usize>,
    pool: &crate::agents::Pool,
) -> Result<Ran> {
    let discovered = battery::discover(&paths.corpus_dir, unit, filter)
        .with_context(|| format!("discovering the cases of {unit}"))?;
    let roles = prompt::chain(paths.tool, paths.variant);
    let roles = &roles[..steps.map_or(roles.len(), |n| n.min(roles.len()))];

    // Concurrent, bounded by the pool's width: the loop here was sequential, so `--parallel` bought
    // nothing at all -- a permit acquired inside a sequential loop is never contended. Workers pull
    // from one queue rather than a thread per case, so 338 cases do not become 338 threads.
    let queue = std::sync::Mutex::new(discovered.cases.iter());
    let collected: std::sync::Mutex<Ran> = std::sync::Mutex::new(Ran {
        resolved: Resolved::new(),
        failures: Vec::new(),
        refused: Vec::new(),
    });

    std::thread::scope(|scope| {
        for _ in 0..pool.width().max(1) {
            scope.spawn(|| loop {
                let Some(case) = queue.lock().expect("case queue").next() else {
                    return;
                };
                // A case the corpus cannot describe is that case's failure, not the run's: the
                // other workers keep going and this one is reported by name.
                let described = match describe(case, paths, unit) {
                    Ok(d) => d,
                    Err(e) => {
                        let mut out = collected.lock().expect("collected results");
                        println!("  \u{274c} describing a case of {unit}: {e:#}");
                        out.failures.push(format!("{unit}/<undescribable>"));
                        continue;
                    }
                };
                let (name, shape, artifact, followers) = described;
                // `CMakePresets.json` sits beside `test_case/`, so the prompt's build flags are read
                // from the parent.
                let case_inputs = paths.input_dir(unit).join(&name);
                let corpus_case = case_inputs.join("test_case");
                let case_dir = paths.case_dir(unit, &name);
                let outcome = run_case(RunCase {
                    paths,
                    store,
                    roles,
                    name: &name,
                    shape,
                    artifact: &artifact,
                    case_inputs: &case_inputs,
                    corpus_case: &corpus_case,
                    case_dir: &case_dir,
                    followers: &followers,
                    unit,
                    pool,
                });
                let mut out = collected.lock().expect("collected results");
                match outcome {
                    Ok(done) => {
                        out.resolved.extend(done.published);
                        out.refused.extend(done.refused);
                    }
                    Err(e) => {
                        println!("  \u{274c} {name}: {e:#}");
                        out.failures.push(name);
                    }
                }
            });
        }
    });

    Ok(collected.into_inner().expect("collected results"))
}

/// The per-case parameters. A struct because half of them are `&Path` and positional arguments of the
/// same type transpose silently.
struct RunCase<'a> {
    paths: &'a Paths,
    store: &'a Store,
    roles: &'a [Role],
    name: &'a str,
    shape: Shape,
    artifact: &'a crate::transform::Artifact,
    case_inputs: &'a Path,
    corpus_case: &'a Path,
    case_dir: &'a Path,
    followers: &'a [battery::Config],
    unit: &'a str,
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
    let work_base = crate::io::workdir::base()?;
    let mut tree = WorkDir::assemble(c.corpus_case)
        .with_context(|| format!("laying out a working dir for {}", c.name))?
        .seal()?;
    let mut published = Vec::new();
    let mut refused: Vec<String> = Vec::new();

    for &role in c.roles {
        let text = prompt::read(
            &c.paths.repo_root,
            c.paths.tool,
            c.paths.variant,
            role,
            c.shape,
            c.case_inputs,
        )?
        .with_context(|| {
            format!(
                "{:?}/{:?} has no {role:?} prompt for a {:?} case, yet the chain schedules one",
                c.paths.tool, c.paths.variant, c.shape
            )
        })?;
        let roots = crate::io::workdir::Roots::resolve(&work_base, &c.paths.repo_root);
        let prompt = Prompt::new(crate::store::normalise(&text, &roots));
        let phase_dir = c.case_dir.join(role.dir());
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
            run_or_replay(&invocation, &tree, c.store, c.corpus_case, &log)?
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
                    c.name, record.outcome
                );
                let empty = reseal(&phase_dir, c.corpus_case)?;
                published.push((phase_dir.clone(), empty));
                refused.push(format!("{}/{role:?}", c.name));
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
                c.name
            ),
        };

        // Publish, then transform. The transform is deterministic and outside the cache, so the tree
        // the NEXT step keys on is `transform(after)` and not `after` itself.
        publish(&after, &phase_dir, c.corpus_case)?;
        crate::transform::post_process(&phase_dir, c.artifact)?;
        tree = reseal(&phase_dir, c.corpus_case)?;
        published.push((phase_dir.clone(), tree.clone()));

        // Per step: `attests` is checked per role, so a follower must be served for each one.
        for cfg in c.followers {
            let follower_dir = c.paths.case_dir(c.unit, &cfg.name).join(role.dir());
            crate::transform::propagate_config(&phase_dir, &follower_dir, cfg)
                .with_context(|| format!("deriving {} from {} for {role:?}", cfg.name, c.name))?;
            let follower_corpus = c.paths.input_dir(c.unit).join(&cfg.name).join("test_case");
            let follower_tree = reseal(&follower_dir, &follower_corpus)?;
            published.push((follower_dir, follower_tree));
        }
    }
    Ok(CaseOutcome { published, refused })
}

/// Write the step's crate into the results tree. Only the translation: `c_src/` is the pinned corpus
/// and re-derived wherever a working dir is assembled, so publishing it would store the same bytes
/// once per case per step.
fn publish(tree: &Tree, phase_dir: &Path, _corpus_case: &Path) -> Result<()> {
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

/// Name, prompt shape, ARTIFACT shape, and the followers this case stands for.
///
/// Two separate shapes on purpose. Dropping the followers here is why any battery with a
/// shared-source group published nothing, and collapsing the two shapes is why every such group lost
/// its `driver`.
fn describe(
    case: &Case,
    paths: &Paths,
    unit: &str,
) -> Result<(
    String,
    Shape,
    crate::transform::Artifact,
    Vec<battery::Config>,
)> {
    let input_dir = paths.input_dir(unit);
    Ok(match case {
        Case::Independent(c) => {
            let artifact = if c.is_lib {
                // The case-dir name IS the right lib name where the corpus runner names no other:
                // `cando2`'s short-form `harness!` resolves `lib<case>.so`.
                crate::transform::Artifact::Cdylib {
                    lib_name: battery::extract_lib_name(&input_dir, &c.name)
                        .unwrap_or_else(|| c.name.clone()),
                }
            } else {
                crate::transform::Artifact::Driver
            };
            (
                c.name.clone(),
                Shape::of(c.is_lib, false),
                artifact,
                Vec::new(),
            )
        }
        // One invocation for the real case; its followers are derived by a transform, not re-run --
        // and the real case is the TEMPLATE they are derived from, so it keeps both targets.
        Case::SharedSource(g) => (
            g.real_case.clone(),
            Shape::Shared,
            crate::transform::Artifact::Template {
                default_features: battery::extract_features_from_path(
                    &input_dir.join(&g.real_case).join("CMakePresets.json"),
                )?,
                needs_driver: !g.real_case.ends_with("_lib"),
            },
            g.configs.clone(),
        ),
    })
}
