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

    let mut out = Ran {
        resolved: Resolved::new(),
        failures: Vec::new(),
    };
    for case in &discovered.cases {
        let (name, shape, lib_name) = describe(case, paths, unit);
        let corpus_case = paths.input_dir(unit).join(&name).join("test_case");
        let case_dir = paths.case_dir(unit, &name);
        match run_case(RunCase {
            paths,
            store,
            roles,
            name: &name,
            shape,
            lib_name: lib_name.as_deref(),
            corpus_case: &corpus_case,
            case_dir: &case_dir,
            pool,
        }) {
            Ok(published) => out.resolved.extend(published),
            Err(e) => {
                println!("  \u{274c} {name}: {e:#}");
                out.failures.push(name);
            }
        }
    }
    Ok(out)
}

/// The per-case parameters. A struct because half of them are `&Path` and positional arguments of the
/// same type transpose silently.
struct RunCase<'a> {
    paths: &'a Paths,
    store: &'a Store,
    roles: &'a [Role],
    name: &'a str,
    shape: Shape,
    lib_name: Option<&'a str>,
    corpus_case: &'a Path,
    case_dir: &'a Path,
    pool: &'a crate::agents::Pool,
}

/// One case, all the way along its chain.
///
/// The tree returned by each step is the tree handed to the next. Nothing consults the filesystem to
/// find the previous step's output: reading `verified/` off disk is what once scored a five-day-old
/// artifact as this run's.
fn run_case(c: RunCase<'_>) -> Result<Vec<(PathBuf, Tree)>> {
    let work_base = crate::io::workdir::base()?;
    let mut tree = WorkDir::assemble(c.corpus_case)
        .with_context(|| format!("laying out a working dir for {}", c.name))?
        .seal()?;
    let mut published = Vec::new();

    for &role in c.roles {
        let text = prompt::read(
            &c.paths.repo_root,
            c.paths.tool,
            c.paths.variant,
            role,
            c.shape,
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
        crate::transform::post_process(&phase_dir, c.shape, c.lib_name.unwrap_or(c.name))?;
        tree = reseal(&phase_dir, c.corpus_case)?;
        published.push((phase_dir, tree.clone()));
    }
    Ok(published)
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

/// Name, shape and the lib name the oracle's runner expects, per case.
fn describe(case: &Case, paths: &Paths, unit: &str) -> (String, Shape, Option<String>) {
    match case {
        Case::Independent(c) => (
            c.name.clone(),
            Shape::of(c.is_lib, false),
            battery::extract_lib_name(&paths.input_dir(unit), &c.name),
        ),
        // One invocation for the real case; its followers are derived by a transform, not re-run.
        Case::SharedSource(g) => (
            g.real_case.clone(),
            Shape::Shared,
            battery::extract_lib_name(&paths.input_dir(unit), &g.real_case),
        ),
    }
}
