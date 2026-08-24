use crate::agents::exit::{
    agent_provenance, clear_agent_exit, merge_agent_exit, observed_exit, record_agent_exit,
};
use crate::agents::invocation::{
    assert_pins_honoured, claude_model, Backend, Invocation, KIRO_UNPINNED_MODEL,
};
use crate::agents::run::{
    displace_and_warn, run_cached, write_phase_metrics, Outcome, PhaseRun, Recorded, SkipCheck,
};
use crate::agents::session::{ClaudeRun, Session};
use crate::agents::work::IsolatedWorkDir;
use crate::analyse::cargo_toml::{self, CargoToml};
use crate::artifact::{Published, Publishing, Translate};
use crate::battery::{self, Case, Paths};
use crate::cache::{self, Attempt, CliVersion, ModelId, Produced, Store};
use crate::cli::Agent;
use crate::io::workdir::Roots;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What a translate sweep RESOLVED, by case dir — a case's only collision-free name. THE hand-off
/// medium: a case absent from it had no translation resolved THIS RUN, a `None`, not a `stat`.
pub type Translations = HashMap<PathBuf, Published<Translate>>;

/// How many resolved translations under `prefix` no key names, so the phase that DERIVES A NUMBER from
/// them can say so — nothing in a phase dir records the model or prompt behind it.
pub fn unkeyed_seeds(translations: &Translations, prefix: &Path) -> usize {
    translations
        .iter()
        .filter(|(dir, t)| {
            dir.starts_with(prefix) && t.keying() == crate::artifact::Keying::Unkeyable
        })
        .count()
}

/// What one translate path produced: a [`Publishing`], since post-processing is per-case.
struct Translated {
    recorded: RecordedBy,
    publishing: Publishing<Translate>,
}

/// What one translate path RESOLVED: `Recorded` ran and left its record, `Unavailable` never ran.
enum Resolution {
    Published(Translated),
    Recorded,
    Unavailable,
}

/// Wall-clock cap on one agentic session. Reaches the command through
/// [`crate::agents::session::Session`], which is also what the cache key records.
const TRANSLATE_TIMEOUT_SECS: u64 = 10800;

/// libpng's translate died on the 3 h ceiling at exactly 10802 s (`exit_code: 124`) after five others
/// finished inside 1.3 h; raised to 24 h on the operator's instruction.
const HB_TRANSLATE_TIMEOUT_SECS: u64 = 86400;

/// Scoped BY DATASET because `timeout=` is in the translate recipe: one ceiling would move all 209 stored
/// Test-Corpus keys. Harvest-bench's own entries did move -- a keyed component cannot be raised without it.
fn translate_ceiling(dataset: crate::cli::Dataset) -> u64 {
    match dataset {
        crate::cli::Dataset::TestCorpus => TRANSLATE_TIMEOUT_SECS,
        crate::cli::Dataset::HarvestBench => HB_TRANSLATE_TIMEOUT_SECS,
    }
}

const KIRO_TRANSLATE_TIMEOUT_SECS: u64 = 5400;

fn opencode_model(paths: &Paths) -> Result<crate::agents::opencode::Model> {
    let raw = paths.model.as_deref().context(
        "--agent opencode requires --model <provider>/<model-id> (should have been \
         rejected at startup)",
    )?;
    crate::agents::opencode::parse_model(raw)
}

struct CaseResult {
    name: String,
    elapsed_secs: u64,
    success: bool,
    error: Option<String>,
    skipped: bool,
}

impl CaseResult {
    /// A worker that unwound rather than returned: `run_and_record` already maps `Err` to
    /// a failed case, so this is an outright panic, and it must fail only its own case
    /// instead of escaping `thread::scope` and discarding every sibling's hours of work.
    /// `case_dir` is taken so the panic is recorded on disk too. Every other failure
    /// path writes `translation.json` via `run_and_record`; without this, a panicked case
    /// is the only one that leaves no trace and so reads as never attempted.
    fn panicked(
        name: String,
        case_dir: &Path,
        agent: &crate::cache::AgentKey,
        payload: Box<dyn std::any::Any + Send>,
    ) -> Self {
        let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "panic with a non-string payload".to_owned()
        };
        // Runs on the JOINING thread, whose `LAST_AGENT_EXIT` belongs to whichever case
        // that thread last ran, and a panic after `run_and_record` returned must not
        // overwrite the real record with a zero-duration failure.
        clear_agent_exit();
        if !crate::artifact::phase_metrics::<Translate>(case_dir).is_file() {
            write_translation_metrics(case_dir, agent, 0, false);
        }
        CaseResult {
            name,
            elapsed_secs: 0,
            success: false,
            error: Some(format!("worker thread panicked: {msg}")),
            skipped: false,
        }
    }
}

/// Zero discovered cases almost always means a case dir was passed where a battery
/// was expected, so the message spells the layout out rather than reporting
/// "0/0 translated".
fn ensure_cases_found(count: usize, paths: &Paths, battery_name: &str) -> Result<()> {
    if count > 0 {
        return Ok(());
    }
    let input_dir = paths.input_dir(battery_name);
    let agent = crate::cli::cli_name(paths.agent)?;
    anyhow::bail!(
        "No translatable cases found under battery '{battery_name}' ({}).\n\
         A translate target is a BATTERY; each case must be one level deeper as \
         `<battery>/<case>/`, with BOTH a `test_case/` (your C sources) and a \
         `test_vectors/` dir (may be empty) inside it.\n\
         If '{battery_name}' is itself your case, nest it under a battery, e.g.:\n  \
           test-corpus/Public-Tests/mycases/{battery_name}/test_case/     (your .c/.h)\n  \
           test-corpus/Public-Tests/mycases/{battery_name}/test_vectors/  (may be empty)\n\
         then run:  harvest-tools --agent {agent} translate mycases/{battery_name}",
        input_dir.display(),
    );
}

pub fn run_test_corpus(
    paths: &Paths,
    battery_name: &str,
    filter: Option<&str>,
    pool: &crate::agents::Pool,
) -> Result<Translations> {
    preflight_check(paths.agent, paths.cache_mode)?;
    let skip = translate_skip_check(paths);

    let battery = battery::discover(&paths.corpus_dir, battery_name, filter)?;
    let output_dir = paths.output_dir(battery_name);
    std::fs::create_dir_all(&output_dir)?;
    let store = cache::Store::open(&paths.repo_root, paths.cache_mode)?;

    let total = count_cases(&battery);
    ensure_cases_found(total, paths, battery_name)?;

    let mut independent: Vec<&battery::IndependentCase> = Vec::new();
    let mut shared: Vec<&battery::SharedSourceGroup> = Vec::new();
    for case in &battery.cases {
        match case {
            Case::Independent(c) => independent.push(c),
            Case::SharedSource(g) => shared.push(g),
        }
    }

    let mut resolved: Translations = HashMap::new();
    let ind_results: Vec<CaseResult> = std::thread::scope(|s| {
        let handles: Vec<(String, _)> = independent
            .iter()
            .map(|c| {
                // Paired with the handle so a panicked join still names its case.
                (
                    c.name.clone(),
                    s.spawn(|| {
                        let _permit = pool.acquire();
                        translate_one_independent(paths, &output_dir, battery_name, c, &store, skip)
                    }),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(name, h)| {
                let case_dir = output_dir.join(&name);
                match h.join() {
                    Ok((r, published)) => {
                        if let Some(p) = published {
                            resolved.insert(case_dir, p);
                        }
                        r
                    }
                    Err(e) => CaseResult::panicked(name, &case_dir, &paths.agent_key, e),
                }
            })
            .collect()
    });

    let mut translated = 0usize;
    let mut failed = 0usize;
    let mut current = 0usize;

    for r in &ind_results {
        current += 1;
        if r.skipped {
            translated += 1;
            println!("[{current}/{total}] ⏭️  {} (already done)", r.name);
        } else if r.success {
            translated += 1;
            println!(
                "  ✅ {} ({}s) [{translated} translated, {failed} failed of {current}/{total}]",
                r.name, r.elapsed_secs
            );
        } else {
            failed += 1;
            let err = r.error.as_deref().unwrap_or("unknown error");
            println!("  ❌ {} — {err} ({}s) [{translated} translated, {failed} failed of {current}/{total}]", r.name, r.elapsed_secs);
        }
    }

    for group in &shared {
        current += 1;
        let real_dir = output_dir.join(&group.real_case);
        let (r, published) =
            translate_one_shared(paths, &output_dir, battery_name, group, &store, skip);
        if let Some(p) = published {
            resolved.insert(real_dir.clone(), p);
        }

        if r.skipped {
            translated += 1;
            println!("[{current}/{total}] ⏭️  {} (already done)", group.real_case);
        } else if r.success {
            translated += 1;
            println!("  ✅ {} ({}s)", group.real_case, r.elapsed_secs);
        } else {
            failed += 1;
            let err = r.error.as_deref().unwrap_or("unknown error");
            println!("  ❌ {} — {err} ({}s)", group.real_case, r.elapsed_secs);
            current += group.configs.len();
            continue;
        }

        // Propagated every run: a crate already there is a PREVIOUS run's derivation of this one.
        for cfg in &group.configs {
            current += 1;
            let cfg_dir = output_dir.join(&cfg.name);
            // The borrow ends before the insert below; a follower is derived from the real case's
            // artifact, so the group's key is what attests it.
            let derived = match resolved.get(&real_dir) {
                Some(source) => {
                    propagate_config(paths, battery_name, &group.real_case, cfg, source)
                }
                None => Err(anyhow::anyhow!(
                    "no published translation for {} to derive from",
                    group.real_case
                )),
            };
            match derived {
                Ok(published) => {
                    translated += 1;
                    resolved.insert(cfg_dir, published);
                    println!("[{current}/{total}] 🔗 {} → {}", cfg.name, group.real_case);
                }
                Err(e) => {
                    failed += 1;
                    println!("[{current}/{total}] ❌ {} — {e:#}", cfg.name);
                }
            }
        }
    }

    println!();
    println!("Done: {translated}/{total} translated, {failed} failed");
    if let Some(line) = store.tally_line() {
        println!("{line}");
    }
    paths
        .cache_mode
        .require_every_case_resolved(failed, total)?;
    Ok(resolved)
}

/// Can this sweep PAY for a translation? Not under [`cache::Mode::ReplayOnly`]. Decided once, here.
fn resolvable(paths: &Paths, skip: SkipCheck) -> bool {
    paths.cache_mode != cache::Mode::ReplayOnly || skip == SkipCheck::Keyed
}

fn unavailable(
    name: &str,
    case_dir: &Path,
    agent: &crate::cache::AgentKey,
) -> (CaseResult, Option<Published<Translate>>) {
    run_and_record(
        name,
        case_dir,
        agent,
        || Ok(Resolution::Unavailable),
        |_| Ok(()),
    )
}

/// Already published, and taken as this run's translation because NO KEY EXISTS to ask for a better
/// one: an unkeyed backend. Reached only where [`SkipCheck::WhateverIsPublished`] is all that can be
/// asked, so a keyed phase still resolves through the store or not at all.
///
/// Resolving NOTHING here left `--agent opencode` and every symlinked-config battery unable to
/// verify at all: neither has an entry to replay, ever.
fn from_published_tree(name: &str, case_dir: &Path) -> (CaseResult, Option<Published<Translate>>) {
    let mut result = CaseResult {
        name: name.to_owned(),
        elapsed_secs: 0,
        success: true,
        error: None,
        skipped: true,
    };
    match Published::<Translate>::unkeyed_from_phase_dir(case_dir) {
        Ok(published) => {
            // Per case, beside the other measurement caveats: nothing here names what produced it.
            eprintln!(
                "  ⚠️  {name}: seeded from the {} already published — no key names it, so the \
                 freshness guarantee does not cover this case",
                crate::battery::TRANSLATED
            );
            (result, Some(published))
        }
        // A skip reporting success while handing on nothing is what left verify with no seed.
        Err(e) => {
            result.success = false;
            result.skipped = false;
            result.error = Some(format!(
                "the published {} cannot be digested, so no phase can be seeded from it: {e:#}",
                crate::battery::TRANSLATED
            ));
            (result, None)
        }
    }
}

fn translate_one_independent(
    paths: &Paths,
    output_dir: &Path,
    battery_name: &str,
    case: &battery::IndependentCase,
    store: &Store,
    skip: SkipCheck,
) -> (CaseResult, Option<Published<Translate>>) {
    let case_dir = output_dir.join(&case.name);
    if skip.already_done(|| {
        crate::battery::has_crate(&crate::battery::phase_dir(
            &case_dir,
            crate::battery::TRANSLATED,
        ))
    }) {
        return from_published_tree(&case.name, &case_dir);
    }
    if !resolvable(paths, skip) {
        return unavailable(&case.name, &case_dir, &paths.agent_key);
    }

    run_and_record(
        &case.name,
        &case_dir,
        &paths.agent_key,
        || dispatch_translate(paths, battery_name, &case.name, case.is_lib, store),
        |tree| {
            if paths.agent == Agent::ClaudeCrossPrompt {
                // E4: the agent's lib-vs-bin choice IS the experiment, so it must not
                // be overridden here; `[workspace]` is still needed so cargo does not
                // absorb each case into a parent workspace.
                let cargo_path = tree.join("Cargo.toml");
                if cargo_path.exists() {
                    if let Ok(mut cargo) = CargoToml::open(&cargo_path) {
                        cargo.add_workspace();
                        let _ = cargo.save();
                    }
                }
                Ok(())
            } else {
                post_process_independent(paths, battery_name, &case.name, case.is_lib, tree)
            }
        },
    )
}

/// Keyed like any other case: one entry names the shared source and every follower is `Derived` from
/// it. Bypassing the store here kept `B02_synthetic` and `P01_sphincs_plus` out of `tables/`.
fn translate_one_shared(
    paths: &Paths,
    output_dir: &Path,
    battery_name: &str,
    group: &battery::SharedSourceGroup,
    store: &Store,
    skip: SkipCheck,
) -> (CaseResult, Option<Published<Translate>>) {
    let real_dir = output_dir.join(&group.real_case);
    if skip.already_done(|| {
        crate::battery::has_crate(&crate::battery::phase_dir(
            &real_dir,
            crate::battery::TRANSLATED,
        ))
    }) {
        return from_published_tree(&group.real_case, &real_dir);
    }
    if !resolvable(paths, skip) {
        return unavailable(&group.real_case, &real_dir, &paths.agent_key);
    }

    println!(
        "Translating: {} (shared-source, {} configs)",
        group.real_case,
        group.configs.len()
    );
    run_and_record(
        &group.real_case,
        &real_dir,
        &paths.agent_key,
        || dispatch_translate_shared(paths, battery_name, &group.real_case, store),
        |tree| {
            if let Ok(mut cargo) = CargoToml::open(&tree.join("Cargo.toml")) {
                cargo.add_workspace();
                let features = battery::extract_features_from_path(
                    &paths
                        .input_dir(battery_name)
                        .join(&group.real_case)
                        .join("CMakePresets.json"),
                )
                .unwrap_or_default();
                let resolved = battery::resolve_features(&tree.join("Cargo.toml"), &features)
                    .unwrap_or_default();
                if !resolved.is_empty() {
                    cargo.set_default_features(&resolved);
                }
                let _ = cargo.save();
            }
            Ok(())
        },
    )
}

/// Who wrote `translation.json`: [`run_cached`] does for the phase it drives, carrying the entry
/// a replay served and the exit code the infra audit reads — a second write would blank both.
enum RecordedBy {
    Driver,
    Caller,
}

fn run_and_record(
    name: &str,
    case_dir: &Path,
    agent: &crate::cache::AgentKey,
    translate_fn: impl FnOnce() -> Result<Resolution>,
    post_process_fn: impl FnOnce(&Path) -> Result<()>,
) -> (CaseResult, Option<Published<Translate>>) {
    // A thread may be re-used across cases; without this, a non-CLI agent would
    // inherit the previous case's exit code.
    clear_agent_exit();
    let start = Instant::now();
    match translate_fn() {
        // The driver published nothing, having already displaced, recorded and written: repeating any of
        // it overwrites that, and `merge_agent_exit` consumes the exit `exit_code` needs.
        Ok(Resolution::Recorded) => (
            CaseResult {
                name: name.to_owned(),
                elapsed_secs: start.elapsed().as_secs(),
                success: false,
                error: Some(format!(
                    "no translation was published; the driver recorded why in {}",
                    crate::artifact::phase_metrics::<Translate>(case_dir).display()
                )),
                skipped: false,
            },
            None,
        ),
        // Nothing to displace or record: no run happened, so the case is absent from the hand-off.
        Ok(Resolution::Unavailable) => (
            CaseResult {
                name: name.to_owned(),
                elapsed_secs: 0,
                success: false,
                error: Some(
                    "no stored translation for this run's key, and this sweep may not pay for one \
                     — either `verify`, which resolves translations read-only, or `--replay-only`. \
                     Translate it deliberately (`harvest-tools translate <battery>`, or `run`) and \
                     verify after."
                        .to_owned(),
                ),
                skipped: false,
            },
            None,
        ),
        Ok(Resolution::Published(Translated {
            recorded,
            publishing,
        })) => {
            let elapsed = start.elapsed().as_secs();
            if matches!(recorded, RecordedBy::Caller) {
                write_translation_metrics(case_dir, agent, elapsed, true);
            }
            let published = match publishing.edited(post_process_fn).finish() {
                Ok(p) => Some(p),
                // Published and staying published; what failed is HASHING it, which takes a symlink
                // cycle — and nothing may be seeded from an undescribed tree.
                Err(e) => {
                    eprintln!(
                        "  ⚠️  {name}: published, but could not be digested, so no phase can be \
                         seeded from it: {e:#}"
                    );
                    None
                }
            };
            (
                CaseResult {
                    name: name.to_owned(),
                    elapsed_secs: elapsed,
                    success: true,
                    error: None,
                    skipped: false,
                },
                published,
            )
        }
        Err(e) => {
            let elapsed = start.elapsed().as_secs();
            // The earlier artifact leaves BEFORE this run's record arrives. Every reader of
            // `Phase::METRICS` — `agent_health::audit`, the `oracle/` enrichers,
            // `battery::extract_agent_meta` — resolves it from the phase dir, and the
            // transcript was teed there before the outcome was known, so a `translation.json`
            // written next to the previous run's crate is that crate scored as this run's
            // result. Moved, never deleted: an unkeyed backend has no store entry to replay.
            // Staging the transcript instead does not remove the need: this record lands in the
            // phase dir either way, and `audit` finds a case only by a transcript already there,
            // so a staged one blinds the infra gate on exactly the runs that fail.
            let error = match displace_and_warn::<Translate>(case_dir) {
                Ok(()) => {
                    write_translation_metrics(case_dir, agent, elapsed, false);
                    e.to_string()
                }
                // Neither swallowed nor recorded: a record written now would be the very
                // corruption the move exists to prevent, so the failure travels out as this
                // case's error instead of as a silent `if let Ok(..)`.
                Err(moving) => format!(
                    "{e}; and the earlier {} could not be moved aside, so this run's \
                     record was withheld rather than written beside it: {moving:#}",
                    crate::battery::TRANSLATED
                ),
            };
            (
                CaseResult {
                    name: name.to_owned(),
                    elapsed_secs: elapsed,
                    success: false,
                    error: Some(error),
                    skipped: false,
                },
                None,
            )
        }
    }
}

/// Which prompt a phase needs. `Library`/`Executable` is the project-type dispatch;
/// `Shared` is a shared-source group's real case.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PromptKind {
    Library,
    Executable,
    Shared,
    Verify,
}

impl PromptKind {
    pub fn independent(is_lib: bool) -> Self {
        if is_lib {
            PromptKind::Library
        } else {
            PromptKind::Executable
        }
    }

    #[cfg(test)]
    const ALL: &'static [PromptKind] = &[
        PromptKind::Library,
        PromptKind::Executable,
        PromptKind::Shared,
        PromptKind::Verify,
    ];
}

/// The one place a prompt file is chosen, relative to [`Paths::prompts_dir`].
///
/// `None` means this agent reads no prompt of that kind at all: c2rust and the
/// docker-driven translators are given none, and the ablations plus the one-shot LLM
/// arms have no verify phase (`agents::invocation::has_verify_phase` must agree — a test
/// asserts it). Returning the *name* rather than the text is what lets a test check the
/// choice against the files on disk, so a renamed prompt fails in CI instead of reaching
/// a paid agent run as an empty one.
pub fn prompt_file_for(agent: Agent, kind: PromptKind) -> Option<&'static str> {
    use PromptKind::{Executable, Library, Shared, Verify};
    match agent {
        // One arm on purpose: the backend varies, the methodology does not.
        Agent::Kiro | Agent::Claude | Agent::OpenCode => Some(match kind {
            Library => "translate-library.md",
            Executable => "translate-executable.md",
            Shared => "translate-shared.md",
            Verify => "verify.md",
        }),
        Agent::ClaudeCombined => match kind {
            Library => Some("ablations/translate-and-verify-library.md"),
            Executable => Some("ablations/translate-and-verify-executable.md"),
            Shared => Some("ablations/translate-and-verify-shared.md"),
            Verify => None,
        },
        Agent::ClaudeMinimal => match kind {
            // One prompt for every project type — that is the ablation.
            Library | Executable | Shared => Some("ablations/translate-minimal.md"),
            Verify => None,
        },
        Agent::ClaudeNoIter => match kind {
            Library => Some("ablations/translate-no-iter-library.md"),
            Executable => Some("ablations/translate-no-iter-executable.md"),
            Shared => Some("ablations/translate-no-iter-shared.md"),
            Verify => None,
        },
        // E2 and E6 differ from `claude` on shared-source cases only, so their
        // independent cases deliberately read the unmodified prompts.
        Agent::ClaudeNoFeatures => match kind {
            Library => Some("translate-library.md"),
            Executable => Some("translate-executable.md"),
            Shared => Some("ablations/translate-no-features-shared.md"),
            Verify => None,
        },
        Agent::ClaudeNoSubtask => match kind {
            Library => Some("translate-library.md"),
            Executable => Some("translate-executable.md"),
            Shared => Some("ablations/translate-no-subtask-shared.md"),
            Verify => None,
        },
        Agent::ClaudeCrossPrompt => match kind {
            // E4: the mismatch IS the experiment — a library gets the executable
            // prompt and vice versa. Shared-source cases have no counterpart to swap
            // with, so they read the standard shared prompt.
            Library => Some("translate-executable.md"),
            Executable => Some("translate-library.md"),
            Shared => Some("translate-shared.md"),
            Verify => None,
        },
        // Same filenames as claude's, read from `prompts/codex/`: the methodology is
        // identical and only the sub-agent protocol differs, Codex having no Task tool.
        Agent::CodexGpt56Sol => Some(match kind {
            Library => "translate-library.md",
            Executable => "translate-executable.md",
            Shared => "translate-shared.md",
            Verify => "verify.md",
        }),
        Agent::CodexGpt55 | Agent::CodexGpt54 => match kind {
            Library => Some("translate-library.md"),
            Executable => Some("translate-executable.md"),
            Shared => Some("translate-shared.md"),
            Verify => None,
        },
        Agent::Kimi | Agent::Oneshot => match kind {
            // A single LLM call with the project-type prompt as its system message.
            // `oneshot_llm_translate` detects the type from CMakeLists even for a
            // shared-source group, so neither Shared nor Verify is ever asked for.
            Library => Some("translate-library.md"),
            Executable => Some("translate-executable.md"),
            Shared | Verify => None,
        },
        // Non-LLM translators: c2rust transpiles, and laertes/c2saferrust/smartc2rust
        // are driven by their own docker pipelines.
        Agent::C2rust | Agent::Laertes | Agent::C2SaferRust | Agent::SmartC2Rust => None,
    }
}

/// The text of the prompt for `kind`, or `None` when this agent reads none.
///
/// A prompt that IS named but missing on disk is an error, never an empty string: an
/// empty prompt invokes the agent with nothing to do and the result is then recorded
/// as a measurement.
pub fn read_prompt(paths: &Paths, kind: PromptKind) -> Result<Option<String>> {
    let Some(file) = prompt_file_for(paths.agent, kind) else {
        return Ok(None);
    };
    let path = paths.prompts_dir.join(file);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading the {kind:?} prompt {}", path.display()))?;
    anyhow::ensure!(
        !text.trim().is_empty(),
        "the prompt {} is empty",
        path.display()
    );
    Ok(Some(text))
}

/// [`read_prompt`] where the phase cannot run without one.
pub fn require_prompt(paths: &Paths, kind: PromptKind) -> Result<String> {
    read_prompt(paths, kind)?.with_context(|| {
        format!(
            "--agent {} has no {kind:?} prompt, so this phase does not exist for it",
            paths.agent_key.as_str()
        )
    })
}

fn uncached(r: Result<Publishing<Translate>>) -> Result<Resolution> {
    r.map(|publishing| {
        Resolution::Published(Translated {
            recorded: RecordedBy::Caller,
            publishing,
        })
    })
}

fn dispatch_translate(
    paths: &Paths,
    battery: &str,
    name: &str,
    is_lib: bool,
    store: &Store,
) -> Result<Resolution> {
    match paths.agent {
        Agent::Laertes => uncached(laertes_translate_case(paths, battery, name)),
        Agent::C2SaferRust => uncached(c2saferrust_translate_case(paths, battery, name, is_lib)),
        Agent::SmartC2Rust => anyhow::bail!("smartc2rust is translated via the external fixture pipeline (docs), not in-tool; harvest-tools only scores its results"),
        Agent::Kimi => uncached(kimi_translate_case(paths, battery, name, is_lib)),
        Agent::Oneshot => uncached(oneshot_translate_case(paths, battery, name, is_lib)),
        Agent::C2rust => translate_case(paths, battery, name, "", store),
        // Every remaining agent differs only in which prompt file it reads.
        Agent::Kiro | Agent::Claude | Agent::OpenCode | Agent::ClaudeCombined
        | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures
        | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt
        | Agent::CodexGpt55 | Agent::CodexGpt54 | Agent::CodexGpt56Sol => {
            let prompt = require_prompt(paths, PromptKind::independent(is_lib))?;
            translate_case(paths, battery, name, &prompt, store)
        }
    }
}

/// One invocation serves N configs derived from it, so the group holds ONE entry.
fn dispatch_translate_shared(
    paths: &Paths,
    battery: &str,
    name: &str,
    store: &Store,
) -> Result<Resolution> {
    match paths.agent {
        Agent::Laertes => uncached(laertes_translate_case(paths, battery, name)),
        Agent::C2SaferRust => uncached(c2saferrust_translate_case(paths, battery, name, true)),
        Agent::SmartC2Rust => anyhow::bail!("smartc2rust is translated via the external fixture pipeline (docs), not in-tool; harvest-tools only scores its results"),
        Agent::Kimi => uncached(kimi_translate_case(paths, battery, name, true)),
        Agent::Oneshot => uncached(oneshot_translate_case(paths, battery, name, true)),
        Agent::C2rust => translate_case(paths, battery, name, "", store),
        Agent::Kiro | Agent::Claude | Agent::OpenCode | Agent::ClaudeCombined
        | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures
        | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt
        | Agent::CodexGpt55 | Agent::CodexGpt54 | Agent::CodexGpt56Sol => {
            let prompt = require_prompt(paths, PromptKind::Shared)?;
            translate_case(paths, battery, name, &prompt, store)
        }
    }
}

/// Translate every harvest-bench project's `test_case/` into a Rust crate that
/// builds a cdylib with the same C ABI: the test phase builds it into a `.so` and
/// runs the upstream gtest suite against it.
pub fn run_harvest_bench(
    paths: &Paths,
    projects: &[battery::HarvestBenchProject],
    pool: &crate::agents::Pool,
) -> Result<Translations> {
    // Ahead of `preflight_check`, so an agent with no translate phase here refuses once
    // before a CLI it will never run is probed, rather than panicking once per project.
    anyhow::ensure!(
        in_tool_translate(paths.agent).is_some(),
        "{}",
        no_in_tool_translate(&paths.agent_key)
    );
    preflight_check(paths.agent, paths.cache_mode)?;
    let skip = translate_skip_check(paths);

    // A harvest-bench test_case/ is always a C library the suite links by ABI, so the
    // library prompt applies to every project — no project-type dispatch. Empty only
    // for the agents that are handed no prompt at all (see `prompt_file_for`).
    let prompt = read_prompt(paths, PromptKind::Library)?.unwrap_or_default();

    anyhow::ensure!(
        !projects.is_empty(),
        "No harvest-bench projects to translate. Targets are `HB` (all) or \
         `HB/<project>`; each project is a dir under harvest-bench/tests/ with \
         both a `test_case/` and a `gtest_suite/`. Did you `git submodule update --init`?"
    );
    let total = projects.len();
    let store = cache::Store::open(&paths.repo_root, paths.cache_mode)?;

    let mut resolved: Translations = HashMap::new();
    let results: Vec<CaseResult> = std::thread::scope(|s| {
        let handles: Vec<(String, _)> = projects
            .iter()
            .map(|p| {
                let prompt = &prompt;
                let store = &store;
                let name = p.name().to_owned(); // see run_test_corpus
                (
                    name,
                    s.spawn(move || {
                        let _permit = pool.acquire();
                        translate_one_harvest_bench(paths, p, prompt, store, skip)
                    }),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(name, h)| {
                let case_dir = paths.output_dir(&name);
                match h.join() {
                    Ok((r, published)) => {
                        if let Some(p) = published {
                            resolved.insert(case_dir, p);
                        }
                        r
                    }
                    Err(e) => CaseResult::panicked(name, &case_dir, &paths.agent_key, e),
                }
            })
            .collect()
    });

    let mut translated = 0usize;
    let mut failed = 0usize;
    for (i, r) in results.iter().enumerate() {
        let n = i + 1;
        if r.skipped {
            translated += 1;
            println!("[{n}/{total}] ⏭️  {} (already done)", r.name);
        } else if r.success {
            translated += 1;
            println!("[{n}/{total}] ✅ {} ({}s)", r.name, r.elapsed_secs);
        } else {
            failed += 1;
            let err = r.error.as_deref().unwrap_or("unknown");
            println!("[{n}/{total}] ❌ {} — {err} ({}s)", r.name, r.elapsed_secs);
        }
    }

    println!("\nDone: {translated}/{total} translated, {failed} failed");
    if let Some(line) = store.tally_line() {
        println!("{line}");
    }
    paths
        .cache_mode
        .require_every_case_resolved(failed, total)?;
    Ok(resolved)
}

fn translate_one_harvest_bench(
    paths: &Paths,
    project: &battery::HarvestBenchProject,
    prompt: &str,
    store: &Store,
    skip: SkipCheck,
) -> (CaseResult, Option<Published<Translate>>) {
    let name = project.name();
    let case_dir = paths.output_dir(name);

    if skip.already_done(|| {
        crate::battery::has_crate(&crate::battery::phase_dir(
            &case_dir,
            crate::battery::TRANSLATED,
        ))
    }) {
        return from_published_tree(name, &case_dir);
    }
    if !resolvable(paths, skip) {
        return unavailable(name, &case_dir, &paths.agent_key);
    }

    run_and_record(
        name,
        &case_dir,
        &paths.agent_key,
        || translate_case_at(paths, project.test_case(), &case_dir, prompt, store),
        |tree| {
            // The lib name must be the project name: the suite links `lib<name>.so`
            // by ABI, not by crate name.
            let cargo_path = tree.join("Cargo.toml");
            if cargo_path.exists() {
                let mut cargo = CargoToml::open(&cargo_path)?;
                cargo.add_workspace();
                cargo.remove_bin();
                cargo.set_lib(name);
                cargo.save()?;
                cargo_toml::strip_for_lib(tree)?;
            }
            Ok(())
        },
    )
}

/// Whether this sweep can launch an agent, so whether its CLI must be on PATH. A pure mapping so a
/// test can assert it exhaustively: `ReplayOnly` refuses above `compute`, so demanding a CLI only
/// stops a runner that was never going to run it. `cli` is not a key component (#109) either.
fn probes_the_agent_cli(mode: cache::Mode) -> bool {
    match mode {
        cache::Mode::ReadWrite | cache::Mode::Bypass | cache::Mode::Refresh => true,
        cache::Mode::ReplayOnly => false,
    }
}

fn preflight_check(agent: Agent, mode: cache::Mode) -> Result<()> {
    if !probes_the_agent_cli(mode) {
        return Ok(());
    }
    let (cmd, version_args): (&str, &[&str]) = match agent {
        Agent::Kiro => ("kiro-cli", &["--version"]),
        Agent::Claude
        | Agent::ClaudeCombined
        | Agent::ClaudeMinimal
        | Agent::ClaudeNoIter
        | Agent::ClaudeNoFeatures
        | Agent::ClaudeNoSubtask
        | Agent::ClaudeCrossPrompt => ("claude", &["--version"]),
        Agent::CodexGpt55 | Agent::CodexGpt54 | Agent::CodexGpt56Sol => ("codex", &["--version"]),
        Agent::OpenCode => ("opencode", &["--version"]),
        Agent::C2rust => ("c2rust", &["--version"]),
        Agent::Laertes => ("docker", &["--version"]),
        Agent::C2SaferRust => ("docker", &["--version"]),
        Agent::SmartC2Rust => ("docker", &["--version"]),
        Agent::Kimi => ("aws", &["sts", "get-caller-identity"]),
        Agent::Oneshot => ("curl", &["--version"]),
    };

    let output = Command::new("bash")
        .arg("-lc")
        .arg(format!("which {cmd} && {cmd} {}", version_args.join(" ")))
        .output()
        .with_context(|| format!("{cmd} not found — is it on PATH?"))?;

    if !output.status.success() {
        anyhow::bail!(
            "{cmd} not found on PATH. Subprocess shell may not source ~/.bashrc.\n\
             Try: export PATH=\"$PATH:$(dirname $(which {cmd}))\" in ~/.profile or ~/.bash_profile"
        );
    }

    let info = String::from_utf8_lossy(&output.stdout);
    for line in info.lines() {
        println!("  {line}");
    }

    if matches!(
        agent,
        Agent::Claude
            | Agent::ClaudeCombined
            | Agent::ClaudeMinimal
            | Agent::ClaudeNoIter
            | Agent::ClaudeNoFeatures
            | Agent::ClaudeNoSubtask
            | Agent::ClaudeCrossPrompt
    ) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // `claude --version` prints either "2.1.150.280 ..." or "claude 2.1.158.312 ...",
        // depending on version, so scan for the first line carrying digits.
        let version_str = stdout
            .lines()
            .find(|l| l.chars().any(|c| c.is_ascii_digit()))
            .unwrap_or("");
        let parts: Vec<u32> = version_str
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        let (major, minor) = (
            parts.first().copied().unwrap_or(0),
            parts.get(1).copied().unwrap_or(0),
        );
        if major < 2 || (major == 2 && minor < 1) {
            anyhow::bail!(
                "Claude Code version {version_str} is too old (need >= 2.1).\n\
                 Subprocess resolved: {}",
                stdout.lines().next().unwrap_or("unknown"),
            );
        }
    }

    // REFUSE BEFORE THE MONEY. `ToolchainId::detect` is a key component and it refuses when
    // `RUSTUP_TOOLCHAIN` disagrees with `rust-toolchain.toml`. Resolved per case it would let a
    // sweep translate case 1, spend real money, and then refuse case 2 for a condition that was
    // already true at launch -- which is the exact failure CLAUDE.md records ("a 3h20m sweep
    // completed and then had all seven verifications refused for a variable that was already set
    // at launch"). Probing it here costs one subprocess and moves that refusal to before the first
    // invocation. The per-case call stays: it is what the key is built from, and it now cannot be
    // the first thing to notice.
    let toolchain = crate::cache::ToolchainId::detect()
        .context("the pinned Rust toolchain must be resolvable before any agent is invoked")?;
    println!("  toolchain {}", toolchain.as_str());

    Ok(())
}

/// Which CLI [`translate_case_at`] drives, resolved from `--agent` before anything runs.
/// Codex's model and region travel with the arm that chose it, where a second
/// `match paths.agent` picked them behind a `_ => unreachable!()` that was sound only because
/// of the first — a reachability argument spread over two matches.
#[derive(Copy, Clone)]
enum InTool {
    Kiro,
    Claude,
    /// Carries nothing: `invocation::codex_model` is the ONE table verify reads too.
    Codex,
    OpenCode,
    C2rust,
}

/// `None` is "this agent has no in-tool translate phase", the counterpart of
/// `verify::verify_invocation`'s `Ok(None)`. The five it answers `None` for were
/// `unreachable!()` arms of the invocation match, and they are reachable: harvest-bench calls
/// `translate_case_at` for whatever `--agent` was passed, so `--agent laertes translate
/// HB/<project>` panicked there and `CaseResult::panicked` reported an ordinary ❌.
fn in_tool_translate(agent: Agent) -> Option<InTool> {
    Some(match agent {
        Agent::Kiro => InTool::Kiro,
        Agent::Claude
        | Agent::ClaudeCombined
        | Agent::ClaudeMinimal
        | Agent::ClaudeNoIter
        | Agent::ClaudeNoFeatures
        | Agent::ClaudeNoSubtask
        | Agent::ClaudeCrossPrompt => InTool::Claude,
        Agent::CodexGpt55 | Agent::CodexGpt54 | Agent::CodexGpt56Sol => InTool::Codex,
        Agent::OpenCode => InTool::OpenCode,
        Agent::C2rust => InTool::C2rust,
        // Each is driven by its own docker pipeline or single API call, reached from
        // `dispatch_translate` and never from here.
        Agent::Laertes | Agent::C2SaferRust | Agent::SmartC2Rust | Agent::Kimi | Agent::Oneshot => {
            return None
        }
    })
}

fn no_in_tool_translate(agent: &crate::cache::AgentKey) -> String {
    format!(
        "--agent {} has no in-tool translate phase: its translation comes from an external \
         docker pipeline or a single API call (see docs), which `dispatch_translate` reaches \
         on Test-Corpus only. There is nothing to run here — but harvest-tools can still \
         verify, test and score results that pipeline produced.",
        agent.as_str()
    )
}

fn translate_case(
    paths: &Paths,
    battery: &str,
    name: &str,
    prompt: &str,
    store: &Store,
) -> Result<Resolution> {
    let input_test_case = paths.input_dir(battery).join(name).join("test_case");
    let case_dir = paths.case_dir(battery, name);
    translate_case_at(paths, &input_test_case, &case_dir, prompt, store)
}

/// How the agent will be launched, resolved from `--agent` before anything runs. Only
/// [`Launch::Keyed`] reaches the store, and the other three each say why they cannot: a wrong
/// key is worse than no cache.
enum Launch {
    Keyed(Invocation),
    /// `opencode run --format json` is not claude's `--output-format stream-json`, whose
    /// terminal records are all [`crate::domain::health::classify`] reads, so nothing mints the
    /// `Completed` the driver seals with and keying it would refuse every healthy run. Teaching
    /// the classifier this format changes what verify publishes too — its own change.
    OpenCode {
        model: crate::agents::opencode::Model,
        session: Session,
    },
    /// No model to name, and no agent exit recorded, so an opaque log is `Unknown` and mints no
    /// `Completed` either. Also the one backend whose spend a cache would not save.
    C2rust,
}

/// The CLI build is probed and the model pinned HERE, before the agent runs: both are key
/// components, and neither is honest after the fact — the CLIs auto-update through a shim, and
/// the resolved model appears only in the transcript.
fn resolve_launch(paths: &Paths, backend: InTool) -> Result<Launch> {
    Ok(match backend {
        InTool::Kiro => Launch::Keyed(Invocation {
            backend: Backend::Kiro,
            model: ModelId::new(KIRO_UNPINNED_MODEL)?,
            cli: CliVersion::probe("kiro-cli")?,
            session: Session::kiro(KIRO_TRANSLATE_TIMEOUT_SECS),
        }),
        InTool::Claude => Launch::Keyed(Invocation {
            backend: Backend::Claude,
            model: claude_model()?,
            cli: CliVersion::probe("claude")?,
            // The same builder verify uses, so the phases cannot drift on flags for one CLI.
            session: Session::claude(translate_ceiling(paths.dataset)),
        }),
        InTool::OpenCode => Launch::OpenCode {
            model: opencode_model(paths)?,
            session: Session::opencode(
                crate::agents::opencode::Phase::Translate,
                translate_ceiling(paths.dataset),
            ),
        },
        // Keyed like every other in-tool backend. Its own unkeyed variant is why no codex
        // sweep ever left an entry to replay.
        InTool::Codex => Launch::Keyed(Invocation {
            backend: Backend::Codex,
            model: ModelId::new(
                crate::agents::invocation::codex_model(paths.agent)
                    .context("a Codex launch resolved for a non-codex agent")?
                    .0,
            )?,
            cli: CliVersion::probe("codex")?,
            session: Session::codex(translate_ceiling(paths.dataset)),
        }),
        InTool::C2rust => Launch::C2rust,
    })
}

/// Which backends have a key at all: exactly those [`resolve_launch`] answers [`Launch::Keyed`]
/// for, which is why it probes a CLI for those two and no other. Read off [`InTool`] so the skip
/// decision stays pure and testable; both matches are exhaustive, so a new backend decides in both.
fn skip_check(backend: InTool) -> SkipCheck {
    match backend {
        InTool::Kiro | InTool::Claude | InTool::Codex => SkipCheck::Keyed,
        InTool::OpenCode | InTool::C2rust => SkipCheck::WhateverIsPublished,
    }
}

/// THE whole of "already translated?": the backend's half and the store's, narrowed here so a sweep
/// has one value to pass on and no mode of its own to get wrong. `None` is a docker pipeline or a
/// single API call — no [`Launch`] at all, so no key.
fn translate_skip_check(paths: &Paths) -> SkipCheck {
    match in_tool_translate(paths.agent) {
        None => SkipCheck::WhateverIsPublished,
        Some(backend) => skip_check(backend),
    }
    .through(paths.cache_mode)
}

/// The agentic-translation core: materialises the C corpus into an isolated work tree, invokes
/// the agent there, and publishes the sealed crate to `<case_dir>/translated`. Paths are
/// explicit so it serves any dataset layout, and it is memoised through [`run_cached`] wherever
/// [`resolve_launch`] yields a [`Launch::Keyed`]. Nothing substitutes a case into a translate
/// prompt, so `input_tree` is the ONLY per-case component of that key — hence
/// [`IsolatedWorkDir::from_corpus`], never a phase dir.
fn translate_case_at(
    paths: &Paths,
    input_test_case: &Path,
    case_dir: &Path,
    prompt: &str,
    store: &Store,
) -> Result<Resolution> {
    let backend =
        in_tool_translate(paths.agent).with_context(|| no_in_tool_translate(&paths.agent_key))?;

    // Created before the agent runs so `tee` can write its log there live; `clear_phase`
    // keeps `logs/` for exactly that reason.
    let logs_dir = crate::artifact::phase_logs::<Translate>(case_dir);
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = crate::artifact::phase_log::<Translate>(case_dir);

    // Before the store is consulted: the OpenCode contract below embeds this root and the prompt
    // is keyed, so on a hit the copy is wasted but the prompt hashed is the prompt shown.
    let work = IsolatedWorkDir::<Translate>::from_corpus(input_test_case)?;

    // OpenCode's appended filesystem-boundary contract names the work root, so the final
    // prompt is only known once the tree exists.
    let prompt = match backend {
        InTool::OpenCode => format!(
            "{prompt}{}",
            crate::agents::opencode::prompt_suffix(work.root())
        ),
        _ => prompt.to_string(),
    };

    // Prompt files evolve, so the filename alone does not say what a past run saw:
    // store the rendered text, after any per-agent suffix. Empty for non-prompt
    // agents such as c2rust.
    if !prompt.is_empty() {
        let _ = std::fs::write(logs_dir.join("prompt.md"), &prompt);
    }

    let launch = resolve_launch(paths, backend)?;
    let Launch::Keyed(inv) = &launch else {
        // Not sealed: sealing demands a `Completed` none of these three transcripts can mint
        // (see [`Launch`]), so routing them through the driver would refuse every healthy run
        // rather than cache one. The TREE they publish is the sealed path's, though, which is
        // what `publish_unsealed` is for.
        //
        // Repeated from the sweeps at the invocation itself: this arm reaches no store to refuse for it.
        if !resolvable(paths, SkipCheck::WhateverIsPublished) {
            return Ok(Resolution::Unavailable);
        }
        run_translate_agent(paths, &launch, &work, &prompt, &log_path)?;
        return Ok(Resolution::Published(Translated {
            recorded: RecordedBy::Caller,
            publishing: crate::artifact::publish_unsealed::<Translate>(
                &work.translated_rust(),
                case_dir,
            )?,
        }));
    };

    // Resolved once, so the digests below cannot disagree about what machine they were taken on.
    let roots = Roots::resolve(work.root(), &paths.repo_root);
    let rendered = cache::prompt(&prompt, &roots);
    let prompt_digest = rendered.digest.clone();
    let policy = inv.backend.policy_shape(paths.enforcement, &roots)?;
    let recipe_shape = cache::Recipe::new(&inv.session, policy)?;
    let recipe = recipe_shape.digest();
    let toolchain = cache::ToolchainId::detect()?;

    let outcome = run_cached(
        PhaseRun {
            work,
            case_dir,
            log_path: &log_path,
            agent: &paths.agent_key,
            model: &inv.model,
            cli: &inv.cli,
            toolchain: &toolchain,
            prompt: &prompt_digest,
            recipe: &recipe,
            prompt_text: &rendered.normalised,
            recipe_record: recipe_shape.shape_record(),
        },
        store,
        |work| {
            let start = Instant::now();
            run_translate_agent(paths, &launch, &work, &prompt, &log_path)?;
            // `Attempt::Nothing`, not an `Err`: an incomplete agent is a run the driver must RECORD, with
            // its transcript, and an error carries no reason it can file — this is where translate's
            // `api_error` attempts used to become unexaminable.
            let health = completion_proof(paths, &log_path);
            let Some(proof) = health.completed() else {
                return Ok(Attempt::Nothing(cache::NotProduced::DidNotComplete {
                    health: format!("{health:?}"),
                }));
            };
            let sealed = work.finish(&proof)?;
            // Once per invocation and inside `compute`: `agent_provenance` CONSUMES the observed
            // exit, so in the caller a replay would report the previous case's exit as this one's.
            Ok(Attempt::Produced(Produced::new(
                sealed,
                log_path.clone(),
                agent_provenance(&paths.agent_key, start.elapsed().as_secs()),
            )))
        },
    )?;
    Ok(match outcome {
        Outcome::Published(publishing) => Resolution::Published(Translated {
            recorded: RecordedBy::Driver,
            publishing,
        }),
        // Not an error: see `Resolution::Recorded`'s arm in `run_and_record`.
        Outcome::Nothing => Resolution::Recorded,
        Outcome::Unavailable => Resolution::Unavailable,
    })
}

/// The classification [`IsolatedWorkDir::finish`]'s proof is minted from, by the same discriminator the
/// scoring gate uses. Returned rather than an error: the caller records the reason.
fn completion_proof(paths: &Paths, log_path: &Path) -> crate::domain::health::Health {
    crate::agent_health::classify_log(log_path, paths.agent.log_format(), observed_exit())
}

/// Borrows the work tree rather than sealing it: the bypassed caller must leave the observed exit
/// for [`write_translation_metrics`], the only thing putting `exit_code` in front of the audit.
fn run_translate_agent(
    paths: &Paths,
    launch: &Launch,
    work: &IsolatedWorkDir<Translate>,
    prompt: &str,
    log_path: &Path,
) -> Result<()> {
    // Cleared so an absent CLI run records no spend; a replay never reaches this function.
    clear_agent_exit();
    let cwd = work.translated_rust();

    match launch {
        Launch::Keyed(inv) => match &inv.backend {
            Backend::Kiro => {
                let status = inv
                    .session
                    .kiro_command(&cwd, prompt, log_path)
                    .status()
                    .context("invoking kiro-cli")?;
                record_agent_exit(status);
            }
            Backend::Codex => {
                let (model, region) = crate::agents::invocation::codex_model(paths.agent)
                    .context("a Codex backend resolved for a non-codex agent")?;
                // The retry wrapper, not a bare status: codex exits 0 after Bedrock drops the
                // conversation, so without it an empty translation records as a clean success.
                invoke_codex_with_retry(
                    RetrySession {
                        prompt,
                        log_path,
                        work_dir: &cwd,
                        context_label: "translate",
                    },
                    &inv.session,
                    model,
                    region,
                )?;
            }
            Backend::Claude => {
                let settings = crate::io::sandbox::write_settings(crate::io::sandbox::Policy {
                    repo_root: &paths.repo_root,
                    work_root: work.root(),
                    enforcement: paths.enforcement,
                })?;
                let status = inv
                    .session
                    .claude_command(&ClaudeRun {
                        cwd: &cwd,
                        prompt,
                        log: log_path,
                        settings: &settings,
                        agent_tmp: &crate::io::workdir::agent_tmp(work.root())?,
                        model: &inv.model,
                    })
                    .status()
                    .context("invoking claude")?;
                record_agent_exit(status);
                // An unhonoured pin misattributes the artifact, `verified/` is built on top of
                // it, and it would now be stored under a key naming a model that never ran.
                assert_pins_honoured(log_path, &inv.model, &inv.cli)?;
            }
            // `resolve_launch` resolves no opencode `Invocation`; refused here rather than at the
            // seal two functions away, which is where an opencode transcript would fail instead.
            Backend::OpenCode(_) => anyhow::bail!(
                "translate keys no opencode invocation: its transcript mints no `Completed` \
                 for the driver to seal with (see `Launch::OpenCode`)"
            ),
        },
        Launch::OpenCode { model, session } => {
            crate::agents::opencode::materialize_config(
                work.root(),
                crate::agents::opencode::Phase::Translate,
                model,
            )?;
            invoke_opencode_with_retry(
                RetrySession {
                    prompt,
                    log_path,
                    work_dir: &cwd,
                    context_label: "translate",
                },
                session,
                work.root(),
                model,
            )?;
        }
        Launch::C2rust => c2rust_translate(&cwd, log_path)?,
    }

    anyhow::ensure!(crate::battery::has_crate(&cwd), "no Cargo.toml produced");
    Ok(())
}

/// `tree` is handed in by [`crate::artifact::Publishing::edited`], not recomputed from `case_dir`:
/// this body used to reopen the published `Cargo.toml` AFTER the seal (site 17).
fn post_process_independent(
    paths: &Paths,
    battery: &str,
    name: &str,
    is_lib: bool,
    tree: &Path,
) -> Result<()> {
    let cargo_path = tree.join("Cargo.toml");
    if !cargo_path.exists() {
        return Ok(());
    }
    let mut cargo = CargoToml::open(&cargo_path)?;
    cargo.add_workspace();

    if is_lib {
        cargo.remove_bin();
        let lib_name = battery::extract_lib_name(&paths.input_dir(battery), name);
        cargo.set_lib(lib_name.as_deref().unwrap_or(name));
        cargo.save()?;
        cargo_toml::strip_for_lib(tree)?;
    } else {
        cargo.set_bin_driver();
        cargo.save()?;
    }
    Ok(())
}

/// Propagate the real case's crate to a shared-source config follower.
///
/// Verify re-propagates the `verified/` phase after fixing the real case, so each
/// follower carries the same post-verify crate; without that, runtests scores only
/// the real case as verified.
///
/// The phase is a type parameter, not a `&str`: `only_the_pipeline_names_a_phase_directory` lexes the
/// names, so a caller passing one would have a way past it.
pub fn propagate_config_phase<P: crate::artifact::Phase>(
    paths: &Paths,
    battery: &str,
    real_case: &str,
    cfg: &battery::Config,
    source: &Published<P>,
) -> Result<Option<Published<P>>> {
    let real_dir = crate::battery::phase_dir(&paths.case_dir(battery, real_case), P::DIR);
    // An agent with no verify phase produces no verified/ to copy.
    if !real_dir.is_dir() {
        return Ok(None);
    }
    let cfg_dir = paths.case_dir(battery, &cfg.name);

    // Published, not written file by file: that is what clears the destination and what invalidates
    // the follower's dependent phases only when the derived tree actually moved.
    let publishing = crate::artifact::publish_derived::<P>(&real_dir, &cfg_dir, source)?;
    let translated = crate::battery::phase_dir(&cfg_dir, P::DIR);
    std::fs::create_dir_all(translated.join("logs"))?;

    let cargo_path = translated.join("Cargo.toml");
    let mut cargo = CargoToml::open(&cargo_path)?;

    let resolved = battery::resolve_features(&cargo_path, &cfg.features)?;
    if !resolved.is_empty() {
        cargo.set_default_features(&resolved);
    }

    if cfg.is_lib {
        cargo.remove_bin();
        if let Some(ref ln) = cfg.lib_name {
            cargo.set_lib(ln);
        }
        cargo.save()?;
        cargo_toml::strip_for_lib(&translated)?;
    } else {
        cargo.save()?;
    }

    Ok(Some(publishing.finish()?))
}

pub fn propagate_config(
    paths: &Paths,
    battery: &str,
    real_case: &str,
    cfg: &battery::Config,
    source: &Published<Translate>,
) -> Result<Published<Translate>> {
    propagate_config_phase::<Translate>(paths, battery, real_case, cfg, source)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{real_case} published no {}, so nothing can be derived from it",
            crate::battery::TRANSLATED
        )
    })
}

/// For the paths that do NOT reach the store (see [`RecordedBy`]), so it is always fresh.
fn write_translation_metrics(
    case_dir: &Path,
    agent: &crate::cache::AgentKey,
    duration_secs: u64,
    success: bool,
) {
    let mut provenance = serde_json::json!({
        "agent": agent.as_str(),
        "duration_secs": duration_secs,
        "success": success,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    merge_agent_exit(&mut provenance);
    write_phase_metrics::<Translate>(case_dir, &provenance, Recorded::Fresh { entry: None });
}

fn count_cases(battery: &battery::Battery) -> usize {
    battery
        .cases
        .iter()
        .map(|c| match c {
            Case::Independent(_) => 1,
            Case::SharedSource(g) => 1 + g.configs.len(),
        })
        .sum()
}

pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading dir {}", src.display()))? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::metadata(entry.path());
            match target {
                Ok(m) if m.is_dir() => copy_dir_all(&entry.path(), &dst_path)?,
                Ok(m) if m.is_file() => {
                    std::fs::copy(entry.path(), &dst_path)?;
                }
                Ok(_) => continue,  // non-regular target (pipe, socket, etc.)
                Err(_) => continue, // dangling symlink
            }
        } else if ft.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else if ft.is_file() {
            std::fs::copy(entry.path(), &dst_path)?;
        }
        // Skip non-regular files: FIFOs, sockets, char/block devices.
    }
    Ok(())
}

/// `skip` applies to top-level directories only.
pub fn copy_dir_filtered(src: &Path, dst: &Path, skip: &[&str]) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading dir {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let dst_path = dst.join(&name);
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::metadata(entry.path());
            match target {
                Ok(m) if m.is_dir() => {
                    if !skip.iter().any(|s| *s == &*name_str) {
                        copy_dir_all(&entry.path(), &dst_path)?;
                    }
                }
                Ok(m) if m.is_file() => {
                    std::fs::copy(entry.path(), &dst_path)?;
                }
                Ok(_) => continue, // non-regular target (pipe, socket, etc.), skip
                Err(_) => continue, // dangling symlink, skip
            }
        } else if ft.is_dir() {
            if skip.iter().any(|s| *s == &*name_str) {
                continue;
            }
            copy_dir_all(&entry.path(), &dst_path)?;
        } else if ft.is_file() {
            std::fs::copy(entry.path(), &dst_path)?;
        }
        // Non-regular files (FIFOs, sockets, devices) appear in agent workspaces —
        // impcheck creates .pipe FIFOs — and std::fs::copy blocks forever on them.
    }
    Ok(())
}

// ── c2rust ─────────────────────────────────────────────────────────────

/// c2rust names the crate after the dir it transpiled (`translated_rust*`); the harness
/// expects `driver`. `static` so the literal pattern compiles once, not once per case.
static C2RUST_CRATE_NAME_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"name = "translated_rust[^"]*""#).expect("literal pattern")
});

fn c2rust_translate(work_dir: &Path, log_path: &Path) -> Result<()> {
    let c_src = work_dir.join("c_src");
    let build_dir = c_src.join("build");
    std::fs::create_dir_all(&build_dir)?;

    let mut log = std::fs::File::create(log_path)?;
    use std::io::Write;

    let cmake_out = Command::new("cmake")
        .args(["..", "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON"])
        .current_dir(&build_dir)
        .output()
        .context("running cmake")?;
    log.write_all(&cmake_out.stdout)?;
    log.write_all(&cmake_out.stderr)?;
    if !cmake_out.status.success() {
        anyhow::bail!(
            "cmake failed: {}",
            String::from_utf8_lossy(&cmake_out.stderr)
        );
    }

    let cc_json = build_dir.join("compile_commands.json");
    if !cc_json.exists() {
        anyhow::bail!("cmake did not produce compile_commands.json");
    }

    let c2r_out = Command::new("c2rust")
        .args([
            "transpile",
            "--emit-build-files",
            "--binary",
            "main",
            &cc_json.to_string_lossy(),
            "--output-dir",
            &work_dir.to_string_lossy(),
        ])
        .output()
        .context("running c2rust transpile")?;
    log.write_all(&c2r_out.stdout)?;
    log.write_all(&c2r_out.stderr)?;
    if !c2r_out.status.success() {
        anyhow::bail!(
            "c2rust transpile failed: {}",
            String::from_utf8_lossy(&c2r_out.stderr)
        );
    }

    let cargo_path = work_dir.join("Cargo.toml");
    if cargo_path.exists() {
        let mut cargo = std::fs::read_to_string(&cargo_path)?;
        cargo = cargo.replace("name = \"main\"", "name = \"driver\"");
        cargo = cargo.replace("name = \"rust_out\"", "name = \"driver\"");
        cargo = C2RUST_CRATE_NAME_RE
            .replace_all(&cargo, r#"name = "driver""#)
            .into_owned();
        for entry in walkdir(work_dir)? {
            if entry.extension().is_some_and(|e| e == "rs") {
                let content = std::fs::read_to_string(&entry)?;
                if content.contains("translated_rust") {
                    std::fs::write(&entry, content.replace("translated_rust", "driver"))?;
                }
            }
        }
        if !cargo.contains("libc") {
            cargo = cargo.replace("[dependencies]", "[dependencies]\nlibc = \"0.2\"");
        }
        if !cargo.contains("[workspace]") {
            cargo.push_str("\n[workspace]\n");
        }
        std::fs::write(&cargo_path, cargo)?;
    }

    std::fs::write(
        work_dir.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly\"\n",
    )?;

    let build_out = Command::new("cargo")
        .args(["+nightly", "build", "--release"])
        .current_dir(work_dir)
        .output()
        .context("cargo build")?;
    log.write_all(&build_out.stdout)?;
    log.write_all(&build_out.stderr)?;
    writeln!(
        log,
        "\nc2rust translation {}",
        if build_out.status.success() {
            "succeeded"
        } else {
            "FAILED to compile"
        }
    )?;

    Ok(())
}

// ── Kimi one-shot LLM translation (harvest methodology) ───────────────

struct LlmResponse {
    content: String,
    input_tokens: u64,
    output_tokens: u64,
}

/// The two halves of a one-shot prompt, named. As two adjacent `&str` parameters they are
/// transposable in silence: the model still answers, the run still records a translation,
/// and the only evidence is that a different question was asked — in the path that
/// produced the committed `oneshot` result files.
#[derive(Copy, Clone)]
struct Conversation<'a> {
    system: &'a str,
    user: &'a str,
}

const BEDROCK_MODEL_ID: &str = "moonshotai.kimi-k2.5";
const BEDROCK_REGION: &str = "us-east-1";
const BEDROCK_MAX_TOKENS: u32 = 16384;

fn kimi_translate_case(
    paths: &Paths,
    battery: &str,
    name: &str,
    is_lib_hint: bool,
) -> Result<Publishing<Translate>> {
    oneshot_llm_translate(paths, battery, name, is_lib_hint, None, bedrock_converse)
}

fn oneshot_translate_case(
    paths: &Paths,
    battery: &str,
    name: &str,
    is_lib_hint: bool,
) -> Result<Publishing<Translate>> {
    // As in `opencode_model`: main rejects a missing --model, but this runs per case.
    let model = paths.model.as_deref().context(
        "--agent oneshot requires --model <provider>/<model-id> (should have been \
         rejected at startup)",
    )?;
    oneshot_llm_translate(
        paths,
        battery,
        name,
        is_lib_hint,
        Some(model),
        |convo, log| openrouter_converse(model, convo, log),
    )
}

fn oneshot_llm_translate(
    paths: &Paths,
    battery: &str,
    name: &str,
    is_lib_hint: bool,
    model: Option<&str>,
    invoke_llm: impl FnOnce(Conversation<'_>, &Path) -> Result<LlmResponse>,
) -> Result<Publishing<Translate>> {
    let case_dir = paths.case_dir(battery, name);
    let logs_dir = crate::artifact::phase_logs::<Translate>(&case_dir);
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = crate::artifact::phase_log::<Translate>(&case_dir);

    let input_test_case = paths.input_dir(battery).join(name).join("test_case");
    let files_json = collect_c_files_json(&input_test_case)?;
    let is_lib = detect_is_library(&input_test_case).unwrap_or(is_lib_hint);
    let system_prompt = require_prompt(paths, PromptKind::independent(is_lib))?;

    // As on the CLI-agent path: prompt files change, so store the text that ran.
    let _ = std::fs::write(logs_dir.join("prompt.md"), &system_prompt);

    let user_msg = format!(
        "Please translate the following C project into a Rust project including Cargo manifest:\n\n{files_json}\n\nreturn as JSON"
    );

    let resp = invoke_llm(
        Conversation {
            system: &system_prompt,
            user: &user_msg,
        },
        &log_path,
    )?;

    let mut usage = serde_json::json!({
        "input_tokens": resp.input_tokens,
        "output_tokens": resp.output_tokens,
    });
    if let Some(m) = model {
        usage["model"] = serde_json::json!(m);
    }
    let _ = std::fs::write(
        logs_dir.join("usage.json"),
        serde_json::to_string_pretty(&usage).unwrap_or_default() + "\n",
    );

    // Assembled in scratch first, as on the CLI-agent path: a response that parses to no
    // crate must not be what replaces the previous translation. `c_src` because the test
    // harness expects the C sources beside the crate.
    let staged = crate::io::workdir::tempdir("harvest-oneshot-")
        .context("creating a staging dir for the LLM response")?;
    copy_dir_all(&input_test_case, &staged.path().join("c_src"))?;
    write_llm_files(&resp.content, staged.path())?;
    if !crate::battery::has_crate(staged.path()) {
        anyhow::bail!("no Cargo.toml in LLM response");
    }

    crate::artifact::publish_unsealed::<Translate>(staged.path(), &case_dir)
}

/// Collect all files under `dir` as a JSON object: `{"files": [{"path": "...", "contents": "..."}]}`.
fn collect_c_files_json(dir: &Path) -> Result<String> {
    #[derive(serde::Serialize)]
    struct FileEntry {
        path: String,
        contents: String,
    }
    #[derive(serde::Serialize)]
    struct FilesPayload {
        files: Vec<FileEntry>,
    }

    let mut files = Vec::new();
    for path in walkdir(dir)? {
        let rel = path.strip_prefix(dir)?.to_string_lossy().to_string();
        let contents =
            std::fs::read_to_string(&path).unwrap_or_else(|_| String::from("<binary file>"));
        files.push(FileEntry {
            path: rel,
            contents,
        });
    }
    Ok(serde_json::to_string(&FilesPayload { files })?)
}

fn detect_is_library(dir: &Path) -> Option<bool> {
    let cmake = std::fs::read_to_string(dir.join("CMakeLists.txt")).ok()?;
    if cmake
        .lines()
        .any(|l| l.trim_start().starts_with("add_library("))
    {
        Some(true)
    } else if cmake
        .lines()
        .any(|l| l.trim_start().starts_with("add_executable("))
    {
        Some(false)
    } else {
        None
    }
}

fn bedrock_converse(convo: Conversation<'_>, log_path: &Path) -> Result<LlmResponse> {
    let request = serde_json::json!({
        "modelId": BEDROCK_MODEL_ID,
        "system": [{"text": convo.system}],
        "messages": [{"role": "user", "content": [{"text": convo.user}]}],
        "inferenceConfig": {"maxTokens": BEDROCK_MAX_TOKENS, "temperature": 0.0}
    });

    let request_file = log_path.with_extension("request.json");
    std::fs::write(&request_file, serde_json::to_string_pretty(&request)?)?;

    let output = Command::new("aws")
        .args([
            "bedrock-runtime",
            "converse",
            "--region",
            BEDROCK_REGION,
            "--cli-read-timeout",
            "300",
            "--cli-input-json",
            &format!("file://{}", request_file.display()),
        ])
        .output()
        .context("failed to invoke aws bedrock-runtime converse")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Sibling of the log, as `with_extension` above, and total where
    // `parent().unwrap().join()` panics on a `log_path` with no directory component.
    let response_file = log_path.with_file_name("translation.response.json");
    let _ = std::fs::write(&response_file, &stdout);

    let log_content = format!(
        "=== BEDROCK REQUEST ===\nModel: {BEDROCK_MODEL_ID}\nRegion: {BEDROCK_REGION}\n\n\
         === SYSTEM PROMPT ===\n{}\n\n\
         === USER MESSAGE (first 2000 chars) ===\n{}\n\n\
         === STDERR ===\n{stderr}",
        convo.system,
        truncated(convo.user, 2000)
    );
    let _ = std::fs::write(log_path, &log_content);

    if !output.status.success() {
        anyhow::bail!("bedrock converse failed: {stderr}");
    }

    let resp: serde_json::Value = serde_json::from_str(&stdout).with_context(|| {
        format!(
            "failed to parse Bedrock response: {}",
            truncated(&stdout, 500)
        )
    })?;

    let content = resp["output"]["message"]["content"][0]["text"]
        .as_str()
        .context("no text in Bedrock response")?
        .trim()
        .to_string();

    let input_tokens = resp["usage"]["inputTokens"].as_u64().unwrap_or(0);
    let output_tokens = resp["usage"]["outputTokens"].as_u64().unwrap_or(0);

    Ok(LlmResponse {
        content,
        input_tokens,
        output_tokens,
    })
}

fn openrouter_converse(
    model: &str,
    convo: Conversation<'_>,
    log_path: &Path,
) -> Result<LlmResponse> {
    let api_key =
        std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY env var not set")?;

    let request = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": convo.system},
            {"role": "user", "content": convo.user}
        ],
        "temperature": 0,
        "response_format": {"type": "json_object"}
    });

    let request_file = log_path.with_extension("request.json");
    std::fs::write(&request_file, serde_json::to_string_pretty(&request)?)?;

    let output = Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "600",
            "-X",
            "POST",
            "https://openrouter.ai/api/v1/chat/completions",
            "-H",
            &format!("Authorization: Bearer {api_key}"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &format!("@{}", request_file.display()),
        ])
        .output()
        .context("failed to invoke curl for OpenRouter")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    let response_file = log_path.with_file_name("translation.response.json"); // see bedrock_converse
    let _ = std::fs::write(&response_file, &stdout);

    let log_content = format!(
        "=== OPENROUTER REQUEST ===\nModel: {model}\n\n\
         === SYSTEM PROMPT ===\n{}\n\n\
         === USER MESSAGE (first 2000 chars) ===\n{}\n\n\
         === RAW RESPONSE (first 2000 chars) ===\n{}",
        convo.system,
        truncated(convo.user, 2000),
        truncated(&stdout, 2000)
    );
    let _ = std::fs::write(log_path, &log_content);

    if !output.status.success() {
        anyhow::bail!("curl failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let resp: serde_json::Value = serde_json::from_str(&stdout).with_context(|| {
        format!(
            "failed to parse OpenRouter response: {}",
            truncated(&stdout, 500)
        )
    })?;

    if let Some(err) = resp.get("error") {
        anyhow::bail!("OpenRouter error: {err}");
    }

    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_string())
        .context("no content in OpenRouter response")?;

    let input_tokens = resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
    let output_tokens = resp["usage"]["completion_tokens"].as_u64().unwrap_or(0);

    Ok(LlmResponse {
        content,
        input_tokens,
        output_tokens,
    })
}

/// Truncate to at most `max` **bytes**, ending on a UTF-8 character boundary.
///
/// The obvious `&s[..s.len().min(max)]` panics when byte `max` lands mid-character,
/// which for these call sites (error messages about unparseable LLM output, routinely
/// non-ASCII) would replace the diagnostic being printed.
/// `str::floor_char_boundary` would do this directly but is still unstable.
fn truncated(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Parse a JSON response `{"files": [{"path": "...", "contents": "..."}]}` and write files.
fn write_llm_files(json_response: &str, output_dir: &Path) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct FileEntry {
        path: String,
        contents: String,
    }
    #[derive(serde::Deserialize)]
    struct FilesPayload {
        files: Vec<FileEntry>,
    }

    // Models wrap the object in prose or a markdown fence, so cut to the outermost
    // brace pair rather than parsing the response as-is.
    let json_str = if let Some(start) = json_response.find('{') {
        let from_brace = &json_response[start..];
        let mut depth = 0;
        let mut end = from_brace.len();
        for (i, c) in from_brace.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        &from_brace[..end]
    } else {
        json_response
    };

    let payload: FilesPayload = serde_json::from_str(json_str).with_context(|| {
        format!(
            "failed to parse LLM JSON response: {}",
            truncated(json_str, 500)
        )
    })?;

    for file in &payload.files {
        let dest = output_dir.join(&file.path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &file.contents)?;
    }

    println!("    LLM produced {} files", payload.files.len());
    Ok(())
}

// ── Laertes (c2rust → resolve-imports → resolve-lifetimes) ─────────────

const LAERTES_DOCKER_IMAGE: &str = "laertes-ready";

/// Requires the project bind-mounted read-write at /mnt/project.
const LAERTES_DOCKER_SCRIPT: &str = r#"
set -e
export PATH=$HOME/.cargo/bin:$PATH
export LD_LIBRARY_PATH=$HOME/.rustup/toolchains/nightly-2020-10-15-x86_64-unknown-linux-gnu/lib
export RUST_LOG=off
cd $HOME/lab/laertes

rm -rf rewrite-workspace/project rewrite-invocations/project
cp -r /mnt/project rewrite-workspace/project
echo "$HOME/lab/laertes/rewrite-workspace/project/lib.rs" > rewrite-invocations/project

echo "=== resolve-imports ==="
target/release/resolve-imports $(cat rewrite-invocations/project) 2>&1

echo "=== resolve-lifetimes ==="
timeout 120 target/release/resolve-lifetimes -f $(cat rewrite-invocations/project) 2>&1 \
    || echo "resolve-lifetimes failed or timed out, continuing with RI-only output"

echo "=== resolve-imports (cleanup) ==="
target/release/resolve-imports $(cat rewrite-invocations/project) 2>&1

# Copy rewritten sources back (only .rs files, preserve mount structure)
find rewrite-workspace/project -name '*.rs' | while read -r f; do
    rel="${f#rewrite-workspace/project/}"
    mkdir -p "/mnt/project/$(dirname "$rel")"
    cp "$f" "/mnt/project/$rel"
done
"#;

fn laertes_translate_case(
    paths: &Paths,
    battery: &str,
    name: &str,
) -> Result<Publishing<Translate>> {
    use std::io::Write;

    // c2rust never runs a verify phase, so its translated/ IS the crate to consume.
    let c2rust_case = paths
        .results_dir
        .parent()
        .context("no parent for results_dir")?
        .join("c2rust")
        .join(battery)
        .join(name);
    let c2rust_original = crate::battery::phase_dir(&c2rust_case, crate::battery::TRANSLATED);
    anyhow::ensure!(
        c2rust_original.is_dir(),
        "c2rust translated/ crate not found: {}",
        c2rust_original.display()
    );

    let case_dir = paths.case_dir(battery, name);
    std::fs::create_dir_all(crate::artifact::phase_logs::<Translate>(&case_dir))?;
    let log_path = crate::artifact::phase_log::<Translate>(&case_dir);

    // Staged in scratch like its c2saferrust neighbour: the container gets this mounted
    // read-write, and the compile check writes `target/` plus a `Cargo.lock` that is part
    // of the hashed artifact — so in `translated/` the arm mutated its own result.
    let tmp = crate::io::workdir::tempdir("harvest-laertes-")
        .context("creating laertes temp workspace")?;
    let work = tmp.path().join("project");
    copy_dir_filtered(&c2rust_original, &work, &["target"])?;

    let mut log = std::fs::File::create(&log_path)?;
    writeln!(log, "source: {}", c2rust_original.display())?;

    writeln!(log, "\n=== Laertes pre-process ===")?;
    laertes_preprocess(&work)?;
    writeln!(log, "done")?;

    writeln!(log, "\n=== Laertes Docker ===")?;
    let mount = format!("{}:/mnt/project", work.display());
    let docker_out = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &mount,
            LAERTES_DOCKER_IMAGE,
            "bash",
            "-c",
            LAERTES_DOCKER_SCRIPT,
        ])
        .output()
        .context("running laertes docker container")?;
    log.write_all(&docker_out.stdout)?;
    log.write_all(&docker_out.stderr)?;
    // Unchecked, a container that never ran (missing image, dead daemon) published the
    // untouched c2rust input as this arm's translation, and the comparison silently
    // measured c2rust twice.
    anyhow::ensure!(
        docker_out.status.success(),
        "laertes container failed ({}), so this case has no laertes translation; see {}",
        docker_out.status,
        log_path.display()
    );

    writeln!(log, "\n=== Laertes post-process ===")?;
    laertes_postprocess(&work)?;

    let build = Command::new("cargo")
        .args(["+nightly", "build", "--release"])
        .env("RUSTFLAGS", "-Awarnings")
        .current_dir(&work)
        .output()
        .context("cargo build after laertes")?;
    log.write_all(&build.stdout)?;
    log.write_all(&build.stderr)?;
    let ok = build.status.success();
    writeln!(
        log,
        "\nlaertes translation {}",
        if ok {
            "succeeded"
        } else {
            "FAILED to compile (non-fatal)"
        }
    )?;

    crate::artifact::publish_unsealed::<Translate>(&work, &case_dir)
}

// ── C2SaferRust (c2rust output -> LLM unsafe-reduction via Bedrock) ────────

const C2SAFERRUST_DOCKER_IMAGE: &str = "c2saferrust:latest";
const C2SAFERRUST_MODEL: &str = "bedrock-gpt54";
const C2SAFERRUST_DEFAULT_BASE_URL: &str = "https://bedrock-mantle.us-west-2.api.aws/openai/v1";

/// Runs against the workspace bind-mounted at /work, reshaped crate at /work/rust,
/// result at /work/rust_WIP. The non-root container user needs a writable HOME +
/// CARGO_HOME, and the registry is seeded from the image so the pinned-nightly build
/// needs no network.
const C2SAFERRUST_DOCKER_SCRIPT: &str = r#"
set -e
mkdir -p /work/home /work/cargo
cp -r /opt/cargo/registry /work/cargo/ 2>/dev/null || true
export HOME=/work/home
export CARGO_HOME=/work/cargo
export C2SR_MODEL=bedrock-gpt54
cd /opt/c2saferrust
# Per-case wall-clock cap: table-heavy functions (large CRC/float lookup tables)
# make gpt-5.4 regenerate thousands of entries in one call, which can run for
# many minutes, hit the per-call timeout, retry, and monopolize a parallel slot.
# 900s lets normal cases finish comfortably while failing pathological ones fast
# so they free their slot instead of wedging the batch. `timeout` exits 124 on
# expiry; the harness then sees no rust_WIP and records the case as failed.
timeout 900 python3 translate.py --code_dir /work/rust 2>&1
"#;

// Minted tokens live 12h; refreshing at half that leaves a long batch run unable to
// outlive its token.
static BEDROCK_TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
const BEDROCK_TOKEN_REFRESH_AFTER: Duration = Duration::from_secs(6 * 3600);

/// Env override first (CI / manual injection), then the process cache, then a freshly
/// minted token.
fn bedrock_token(region: &str) -> Result<String> {
    if let Ok(t) = std::env::var("BEDROCK_API_KEY") {
        if !t.trim().is_empty() {
            return Ok(t);
        }
    }
    if let Ok(t) = std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
        if !t.trim().is_empty() {
            return Ok(t);
        }
    }

    // Replaced only wholesale, so no panic can leave it half-written; propagating the
    // poison would instead fail every remaining case while holding a valid token.
    let mut guard = BEDROCK_TOKEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((tok, born)) = guard.as_ref() {
        if born.elapsed() < BEDROCK_TOKEN_REFRESH_AFTER {
            return Ok(tok.clone());
        }
    }

    let tok = mint_bedrock_token(region)?;
    *guard = Some((tok.clone(), Instant::now()));
    Ok(tok)
}

/// Mints a 12h token. AWS_PROFILE/AWS_DEFAULT_PROFILE are stripped so it is issued
/// for the operator's `default` (ada) profile and not a session profile Claude Code
/// or other tooling exported — that mismatch produced wrong-account 401s.
fn mint_bedrock_token(region: &str) -> Result<String> {
    let py = "import sys; from aws_bedrock_token_generator import provide_token; \
              sys.stdout.write(provide_token(region=sys.argv[1]))";
    let out = Command::new("python3")
        .args(["-c", py, region])
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_DEFAULT_PROFILE")
        .output()
        .context(
            "minting Bedrock token (is aws_bedrock_token_generator installed \
                  and are `default`-profile creds valid? run `aws-creds <account>`)",
        )?;
    anyhow::ensure!(
        out.status.success(),
        "Bedrock token mint failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tok = String::from_utf8(out.stdout)?.trim().to_string();
    anyhow::ensure!(
        tok.starts_with("bedrock-api-key-"),
        "unexpected token format from provide_token (got {} chars)",
        tok.len()
    );
    Ok(tok)
}

/// Reduce unsafe code in this repo's c2rust output with the pinned submodule tool in
/// Docker, driven by gpt-5.4 via Bedrock.
///
/// Blind by design (no `--test_dir`): compile-gated only, which is what makes its
/// numbers comparable to ACTOR self-verified. `BEDROCK_BASE_URL` may override the
/// default us-west-2 mantle endpoint.
fn c2saferrust_translate_case(
    paths: &Paths,
    battery: &str,
    name: &str,
    _is_lib: bool,
) -> Result<Publishing<Translate>> {
    use std::io::Write;

    // c2rust never runs a verify phase, so its translated/ IS the crate to consume.
    let c2rust_case = paths
        .results_dir
        .parent()
        .context("no parent for results_dir")?
        .join("c2rust")
        .join(battery)
        .join(name);
    let c2rust_original = crate::battery::phase_dir(&c2rust_case, crate::battery::TRANSLATED);
    anyhow::ensure!(
        c2rust_original.is_dir(),
        "c2rust translated/ crate not found (run the c2rust agent first): {}",
        c2rust_original.display()
    );

    let base_url = std::env::var("BEDROCK_BASE_URL")
        .unwrap_or_else(|_| C2SAFERRUST_DEFAULT_BASE_URL.to_string());
    let region = base_url
        .split("bedrock-mantle.")
        .nth(1)
        .and_then(|s| s.split('.').next())
        .unwrap_or("us-west-2")
        .to_string();
    let token = bedrock_token(&region)?;

    let case_dir = paths.case_dir(battery, name);
    let logs_dir = crate::artifact::phase_logs::<Translate>(&case_dir);
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = crate::artifact::phase_log::<Translate>(&case_dir);
    let translated = crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED);

    // Bind-mounted into the container: the tool reshapes <work>/rust in place and
    // writes <work>/rust_WIP.
    let tmp = crate::io::workdir::tempdir("harvest-c2sr-")
        .context("creating c2saferrust temp workspace")?;
    let work_rust = tmp.path().join("rust");
    copy_dir_filtered(&c2rust_original, &work_rust, &["target", "c_src"])?;
    let _ = std::fs::remove_file(work_rust.join("Cargo.lock"));

    let mut log = std::fs::File::create(&log_path)?;
    writeln!(log, "source: {}", c2rust_original.display())?;
    writeln!(log, "model: {} via {}", C2SAFERRUST_MODEL, base_url)?;

    writeln!(log, "\n=== C2SaferRust pre-process ===")?;
    c2saferrust_preprocess(&work_rust)?;
    writeln!(log, "done")?;

    // Runs as the host user so the outputs are not root-owned.
    writeln!(log, "\n=== C2SaferRust Docker (gpt-5.4 via Bedrock) ===")?;
    let uid = unsafe { libc_getuid() };
    let gid = unsafe { libc_getgid() };
    let mount = format!("{}:/work", tmp.path().display());
    let docker_out = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--user",
            &format!("{uid}:{gid}"),
            "-e",
            "C2SR_MODEL",
            "-e",
            &format!("BEDROCK_API_KEY={token}"),
            "-e",
            &format!("BEDROCK_BASE_URL={base_url}"),
            "-v",
            &mount,
            C2SAFERRUST_DOCKER_IMAGE,
            "bash",
            "-c",
            C2SAFERRUST_DOCKER_SCRIPT,
        ])
        .env("C2SR_MODEL", C2SAFERRUST_MODEL)
        .output()
        .context("running c2saferrust docker container")?;
    log.write_all(&docker_out.stdout)?;
    log.write_all(&docker_out.stderr)?;

    // No rust_WIP means the c2rust input did not compile under the pinned nightly
    // (e.g. SPHINCS+, whose duplicate `randombytes` symbol is a hard error on
    // nightly-2022-08-08). Emitting the unmodified input then keeps the case counted
    // and failing at test time instead of silently vanishing from the totals.
    let wip = tmp.path().join("rust_WIP");
    let source_dir = if crate::battery::has_crate(&wip) {
        writeln!(log, "\nrust_WIP produced; collecting C2SaferRust output")?;
        wip.clone()
    } else {
        writeln!(
            log,
            "\nNo rust_WIP produced (C2Rust input failed to compile under \
                       nightly-2022-08-08, or translation failed). Falling back to the \
                       unmodified C2Rust input so the case is counted as a failure."
        )?;
        work_rust.clone()
    };
    // Held rather than dropped: the tidying below edits the published tree before the digest.
    let publishing = crate::artifact::publish_unsealed::<Translate>(&source_dir, &case_dir)?;
    for junk in [
        "callgraph.dot",
        "callgraph.pdf",
        "slices.json",
        "log.txt",
        "prompts.txt",
        "ordering.txt",
    ] {
        let _ = std::fs::remove_file(translated.join(junk));
    }
    // The tool leaves .old rollback files behind.
    if let Ok(entries) = std::fs::read_dir(&translated) {
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "old") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    // The nightly-2022-08-08 pin exists only for the tool's slicer; downstream
    // testing must use the same toolchain as every other agent.
    c2saferrust_postprocess(&translated)?;

    if wip.join("log.txt").exists() {
        let _ = std::fs::copy(wip.join("log.txt"), logs_dir.join("c2saferrust_log.txt"));
    }

    writeln!(
        log,
        "\nc2saferrust translation collected into {}",
        translated.display()
    )?;
    Ok(publishing)
}

/// Whatever `crate-type` c2rust emitted, replaced wholesale. `static`: as
/// [`C2RUST_CRATE_NAME_RE`].
static CRATE_TYPE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"crate-type\s*=\s*\[[^\]]*\]"#).expect("literal pattern")
});

/// Reshape a c2rust crate so the C2SaferRust slicer can build it as a library.
fn c2saferrust_preprocess(work_dir: &Path) -> Result<()> {
    // The slicer needs an rlib; c2rust emits cdylib for _lib cases.
    let cargo = work_dir.join("Cargo.toml");
    if cargo.exists() {
        let mut s = std::fs::read_to_string(&cargo)?;
        if CRATE_TYPE_RE.is_match(&s) {
            s = CRATE_TYPE_RE
                .replace(&s, r#"crate-type = ["staticlib","rlib"]"#)
                .into_owned();
        }
        std::fs::write(&cargo, s)?;
    }
    // The toolchain the slicer and its metrics were built against.
    std::fs::write(
        work_dir.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2022-08-08\"\ncomponents = [\"rustfmt\", \"rustc-dev\", \"rust-src\", \"llvm-tools-preview\"]\n",
    )?;
    Ok(())
}

/// Undo [`c2saferrust_preprocess`]'s toolchain pin so downstream build/test matches
/// the other agents.
fn c2saferrust_postprocess(work_dir: &Path) -> Result<()> {
    std::fs::write(
        work_dir.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly\"\n",
    )?;
    Ok(())
}

extern "C" {
    fn getuid() -> u32;
    fn getgid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    getuid()
}
unsafe fn libc_getgid() -> u32 {
    getgid()
}

/// Adapt c2rust output for Laertes' nightly-2020-10-15 toolchain.
fn laertes_preprocess(work_dir: &Path) -> Result<()> {
    for path in walkdir(work_dir)? {
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let mut src = std::fs::read_to_string(&path)?;
        let changed = src.contains("::core::ffi::")
            || src.contains("::core::ptr")
            || src.contains("::core::mem");
        if !changed && !path.ends_with("lib.rs") {
            continue;
        }

        src = src.replace("::core::ffi::", "libc::");
        src = src.replace("::core::ptr", "std::ptr");
        src = src.replace("::core::mem", "std::mem");

        if src.contains("libc::") && !src.contains("extern crate libc") {
            src.insert_str(0, "extern crate libc;\n");
        }
        std::fs::write(&path, src)?;
    }

    let lib_rs = work_dir.join("lib.rs");
    if lib_rs.exists() {
        let mut src = std::fs::read_to_string(&lib_rs)?;
        if !src.contains("rustc_private") {
            src.insert_str(0, "#![feature(rustc_private)]\n");
        }
        std::fs::write(&lib_rs, src)?;
    }

    // libc must be pinned exactly: the 2020 resolver cannot handle newer releases.
    let cargo = work_dir.join("Cargo.toml");
    if cargo.exists() {
        let mut s = std::fs::read_to_string(&cargo)?;
        s = s.replace("edition = \"2021\"", "edition = \"2018\"");
        s = s.replace("libc = \"0.2\"", "libc = \"=0.2.126\"");
        std::fs::write(&cargo, s)?;
    }
    Ok(())
}

/// Collapses `libc::unix::linux_like::open` to `libc::open`: Laertes' 2020 nightly
/// resolves internal module paths modern libc does not expose. `static`: applied per
/// file, so recompiling it per call was the costliest of the three.
static LIBC_INTERNAL_PATH_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"libc::(?:[a-z_0-9]+::)+([a-z_0-9]+)").expect("literal pattern")
});

/// Restore modern-toolchain compatibility after Laertes rewrites.
fn laertes_postprocess(work_dir: &Path) -> Result<()> {
    for path in walkdir(work_dir)? {
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path)?;
        let mut out = src.replace("extern crate libc;\n", "");
        out = LIBC_INTERNAL_PATH_RE
            .replace_all(&out, "libc::$1")
            .into_owned();
        if out != src {
            std::fs::write(&path, out)?;
        }
    }

    let lib_rs = work_dir.join("lib.rs");
    if lib_rs.exists() {
        let src = std::fs::read_to_string(&lib_rs)?;
        std::fs::write(&lib_rs, src.replace("#![feature(rustc_private)]\n", ""))?;
    }

    let cargo = work_dir.join("Cargo.toml");
    if cargo.exists() {
        let mut s = std::fs::read_to_string(&cargo)?;
        s = s.replace("edition = \"2018\"", "edition = \"2021\"");
        s = s.replace("libc = \"=0.2.126\"", "libc = \"0.2\"");
        std::fs::write(&cargo, s)?;
    }
    Ok(())
}

fn walkdir(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

/// The inputs both retrying agent invocations share, regardless of backend.
pub(crate) struct RetrySession<'a> {
    pub(crate) prompt: &'a str,
    pub(crate) log_path: &'a Path,
    pub(crate) work_dir: &'a Path,
    /// Identifies the case in retry/abort diagnostics.
    pub(crate) context_label: &'a str,
}

/// Retry-aware codex invocation: codex exits 0 after a transient Bedrock error
/// mid-conversation, so without this a Bedrock failure counts as a successful (but
/// empty) translation. Each retry is a fresh invocation, discarding partial state.
pub(crate) fn invoke_codex_with_retry(
    session: RetrySession<'_>,
    invocation: &crate::agents::session::Session,
    model: &str,
    region: &str,
) -> Result<()> {
    let RetrySession {
        prompt,
        log_path,
        work_dir,
        context_label,
    } = session;
    const MAX_RETRIES: u32 = 3;
    const RETRY_BACKOFF_SECS: u64 = 30;

    for attempt in 1..=MAX_RETRIES {
        let status = invocation
            .codex_command(work_dir, prompt, log_path, model, region)
            .status()
            .with_context(|| format!("invoking codex for {context_label}"))?;
        record_agent_exit(status);

        match scan_codex_log_for_transient_error(log_path) {
            None => return Ok(()), // success or non-transient
            Some(err) if attempt < MAX_RETRIES => {
                eprintln!(
                    "  codex transient error ({err}) on attempt {attempt}/{MAX_RETRIES}, retrying in {RETRY_BACKOFF_SECS}s..."
                );
                std::thread::sleep(std::time::Duration::from_secs(RETRY_BACKOFF_SECS));
            }
            Some(err) => {
                eprintln!(
                    "  codex transient error ({err}) on final attempt {attempt}/{MAX_RETRIES} — giving up"
                );
                return Ok(()); // let caller's "no Cargo.toml" check fail it
            }
        }
    }

    Ok(())
}

/// Transient Bedrock failures worth retrying, as (log needle, short label).
/// Shared by every Bedrock-backed agent (codex, opencode) because the failures
/// come from Bedrock, not from the CLI wrapping it.
const TRANSIENT_BEDROCK_PATTERNS: &[(&str, &str)] = &[
    ("Engine not found", "bedrock 404"),
    ("stream disconnected", "stream disconnected"),
    ("server had an error", "server error"),
    ("ThrottlingException", "throttled"),
    ("RequestTimeout", "request timeout"),
    ("InternalServerError", "internal server error"),
    ("503 Service Unavailable", "503"),
];

fn first_transient_pattern(content: &str) -> Option<String> {
    TRANSIENT_BEDROCK_PATTERNS
        .iter()
        .find(|(needle, _)| content.contains(needle))
        .map(|(_, label)| (*label).to_string())
}

/// Returns Some(reason) if the log indicates a transient Bedrock failure.
fn scan_codex_log_for_transient_error(log_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(log_path).ok()?;

    // `"type":"error"` alone is not enough: the run must have aborted
    // (`turn.failed`) and never recovered (no `turn.completed`).
    if !content.contains(r#""type":"turn.failed""#) {
        return None;
    }
    if content.contains(r#""type":"turn.completed""#) {
        return None;
    }

    first_transient_pattern(&content)
}

/// Same motivation as the codex scanner: the CLI can exit 0 after Bedrock drops the
/// conversation, which would otherwise be recorded as a legitimately-empty
/// translation.
fn scan_opencode_log_for_transient_error(log_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(log_path).ok()?;

    let has_error = content.contains(r#""type":"error""#)
        || content.contains(r#""error":"#)
        || content.contains("APICallError");
    if !has_error {
        return None;
    }
    // Assistant or tool activity means the session recovered; retrying from here
    // would discard real work.
    if content.contains(r#""type":"tool"#) || content.contains(r#""role":"assistant""#) {
        return None;
    }

    first_transient_pattern(&content)
}

/// Mirrors [`invoke_codex_with_retry`]: a Bedrock throttle can leave the CLI exiting
/// 0 with nothing written, indistinguishable from a genuine translation failure.
fn invoke_opencode_with_retry(
    retry: RetrySession<'_>,
    session: &crate::agents::session::Session,
    tmp_root: &Path,
    model: &crate::agents::opencode::Model,
) -> Result<()> {
    let RetrySession {
        prompt,
        log_path,
        work_dir,
        context_label,
    } = retry;
    const MAX_RETRIES: u32 = 3;
    const RETRY_BACKOFF_SECS: u64 = 30;

    for attempt in 1..=MAX_RETRIES {
        crate::agents::opencode::invoke(session, prompt, log_path, work_dir, tmp_root, model)
            .with_context(|| format!("invoking opencode ({context_label})"))?;

        match scan_opencode_log_for_transient_error(log_path) {
            None => return Ok(()), // success or non-transient
            Some(err) if attempt < MAX_RETRIES => {
                eprintln!(
                    "  opencode transient error ({err}) on attempt {attempt}/{MAX_RETRIES}, retrying in {RETRY_BACKOFF_SECS}s..."
                );
                std::thread::sleep(Duration::from_secs(RETRY_BACKOFF_SECS));
            }
            Some(err) => {
                eprintln!(
                    "  opencode transient error ({err}) on final attempt {attempt}/{MAX_RETRIES} — giving up"
                );
                return Ok(()); // let the caller's artifact check fail it
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    /// A runner replaying a stored battery has no `claude` on PATH and needs none. Asserted over
    /// every mode so a new one decides here rather than defaulting into probing a CLI it never runs.
    #[test]
    fn only_a_sweep_that_can_launch_an_agent_demands_its_cli() {
        for mode in [
            crate::cache::Mode::ReadWrite,
            crate::cache::Mode::Bypass,
            crate::cache::Mode::Refresh,
        ] {
            assert!(
                probes_the_agent_cli(mode),
                "{mode:?} can reach compute, so the CLI must exist"
            );
        }
        assert!(
            !probes_the_agent_cli(crate::cache::Mode::ReplayOnly),
            "a replay cannot launch an agent, so requiring its CLI only breaks the runner"
        );

        // Non-vacuity: the skip is what makes the check reachable with nothing on PATH at all.
        // `Agent::Claude` names a `claude` binary this test does not provide.
        preflight_check(Agent::Claude, crate::cache::Mode::ReplayOnly)
            .expect("a replay must not probe a CLI it will never invoke");
    }

    use super::*;
    use std::process::Command;

    /// ExitStatus cannot be constructed directly, so shell out for a real one.
    /// main's tests predate `AgentKey`; this is the one spelling they share.
    fn test_agent_key() -> crate::cache::AgentKey {
        crate::cache::AgentKey::new(Agent::Claude, None).expect("claude has a fixed name")
    }

    fn exit_status(code: i32) -> std::process::ExitStatus {
        Command::new("sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .status()
            .unwrap()
    }

    #[test]
    fn truncated_never_splits_a_character() {
        let s = "é".repeat(10); // 20 bytes, 10 chars; every odd byte index is mid-char
        for max in 0..=s.len() {
            let t = truncated(&s, max); // must not panic for ANY cut point
            assert!(t.len() <= max);
            assert!(s.starts_with(t), "must be a prefix: {t:?}");
        }
        // A cut landing mid-character steps back to the boundary, losing that char.
        assert_eq!(truncated(&s, 3), "é");
        assert_eq!(truncated("ok", 500), "ok");
        assert_eq!(truncated(&s, 4), "éé");
    }

    /// THE ORDERING BUG: all four translate paths wiped the case dir before invoking the
    /// agent, so an outage, a timeout or a crash left the case holding nothing where a
    /// complete result had been. Driven through the oneshot path because its LLM call is the
    /// one injectable failure of the four; the wipe was identical at every site.
    #[test]
    fn a_translation_that_fails_leaves_the_previous_result_standing() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let root = tmp.path();
        // `require_prompt` refuses an absent one, failing ahead of the code under test.
        let prompts = root.join("prompts/oneshot");
        std::fs::create_dir_all(&prompts).unwrap();
        for f in ["translate-library.md", "translate-executable.md"] {
            std::fs::write(
                prompts.join(f),
                "translate this C project into Rust.".repeat(8),
            )
            .unwrap();
        }
        let paths = Paths::new(
            root,
            Agent::Oneshot,
            crate::cli::Dataset::TestCorpus,
            Some("openai/gpt-5.4"),
            crate::cache::Mode::Bypass,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .expect("Paths");
        let (battery, name) = ("B01_organic", "strcmp");
        let test_case = paths.input_dir(battery).join(name).join("test_case");
        std::fs::create_dir_all(&test_case).unwrap();
        std::fs::write(test_case.join("main.c"), "int main(void){return 0;}").unwrap();

        // What the last sweep left: a translation, the verification built on it, and the
        // test artifacts staged beside both.
        let case = paths.case_dir(battery, name);
        let translated = crate::battery::phase_dir(&case, crate::battery::TRANSLATED);
        let verified = crate::battery::phase_dir(&case, crate::battery::VERIFIED);
        for dir in [&translated, &verified] {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
            std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}").unwrap();
        }
        std::fs::create_dir_all(case.join("test_vectors")).unwrap();
        std::fs::write(case.join("test_vectors/in.txt"), "1 2").unwrap();

        let err = oneshot_llm_translate(&paths, battery, name, false, None, |_, log| {
            // The transcript is teed live, as the real backends do, and then the call dies.
            std::fs::write(log, "=== OPENROUTER REQUEST ===\n").unwrap();
            anyhow::bail!("api_error 403: the security token included in the request is expired")
        })
        .expect_err("the injected outage must fail the translation");
        assert!(format!("{err:#}").contains("403"), "{err:#}");

        for f in [
            translated.join("src/lib.rs"),
            verified.join("Cargo.toml"),
            case.join("test_vectors/in.txt"),
        ] {
            assert!(
                f.is_file(),
                "{} was destroyed by a run that produced no translation to replace it",
                f.display()
            );
        }

        // Non-vacuity: publishing a DIFFERENT translation does replace the first two, so the
        // assertions above hold because the wipe moved to publish time, not because nothing clears.
        let fresh = tmp.path().join("a-different-translation");
        std::fs::create_dir_all(fresh.join("src")).unwrap();
        std::fs::write(fresh.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(fresh.join("src/lib.rs"), "pub fn b() {}").unwrap();
        crate::artifact::publish_unsealed::<Translate>(&fresh, &case)
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(translated.join("src/lib.rs")).unwrap(),
            "pub fn b() {}",
            "a publish must replace the old crate"
        );
        assert!(
            !verified.exists(),
            "and must invalidate the verification built on it"
        );
        assert!(
            crate::artifact::phase_log::<Translate>(&case).is_file(),
            "while the transcript teed into the phase dir survives"
        );
        assert!(
            case.join("test_vectors/in.txt").is_file(),
            "and what translate does not own is never its to delete"
        );
    }

    /// A `Paths` in a tempdir (`paths_for` names the real repo root). Agent AND mode are parameters:
    /// they are what [`translate_skip_check`] answers from, and fixing one hides the other's error.
    fn paths_at(
        root: &Path,
        agent: Agent,
        dataset: crate::cli::Dataset,
        cache: cache::Mode,
    ) -> Paths {
        let model = match agent {
            Agent::OpenCode => Some("amazon-bedrock/us.anthropic.claude-sonnet-5"),
            Agent::Oneshot | Agent::Kimi => Some("openai/gpt-5.4"),
            _ => None,
        };
        Paths::new(
            root,
            agent,
            dataset,
            model,
            cache,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .expect("Paths")
    }

    struct Site<'a> {
        what: &'a str,
        published: std::path::PathBuf,
        run: &'a dyn Fn(SkipCheck) -> (CaseResult, Option<Published<Translate>>),
    }

    fn publish_a_crate(case_dir: &Path) -> std::path::PathBuf {
        let dir = crate::battery::phase_dir(case_dir, crate::battery::TRANSLATED);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        dir
    }

    /// A follower's number rests on the group's key PLUS a deterministic derivation, so both halves are
    /// pinned here, and a re-derivation must clear what it does not produce (`spec-7c.md`).
    #[test]
    fn a_follower_is_derived_from_the_group_and_derives_the_same_tree_every_time() {
        let (_tmp, paths, battery, real, real_dir, published) = group_fixture();
        let source = crate::artifact::Publishing::<Translate>::for_test(&real_dir)
            .finish()
            .unwrap();
        assert_eq!(
            source.keying(),
            crate::artifact::Keying::Keyed,
            "fixture: the group is what holds the key"
        );
        let _ = published;

        let cfg = battery::Config {
            name: "macrodepth_add_0".to_owned(),
            features: Vec::new(),
            is_lib: false,
            lib_name: None,
        };
        let first = propagate_config(&paths, battery, real, &cfg, &source).unwrap();
        assert_eq!(
            first.keying(),
            crate::artifact::Keying::Derived,
            "a follower is attributable through the group's key, so it is not Unkeyable"
        );

        let follower = crate::battery::phase_dir(
            &paths.case_dir(battery, &cfg.name),
            crate::battery::TRANSLATED,
        );
        std::fs::write(follower.join("stale.rs"), "left by an earlier derivation").unwrap();
        let again = propagate_config(&paths, battery, real, &cfg, &source).unwrap();
        assert!(
            !follower.join("stale.rs").is_file(),
            "a derivation must clear what it does not produce"
        );
        assert_eq!(
            first.digest(),
            again.digest(),
            "and derive the identical tree, or the group's key names nothing about its followers"
        );
    }

    /// PR 21's rule one level down, and the defect that shelved `spec-7c`: without BOTH directions the
    /// group straddles two phases and the battery's headline silently loses a case.
    #[test]
    fn a_follower_keeps_its_verification_only_while_its_derivation_does_not_move() {
        let (_tmp, paths, battery, real, real_dir, published) = group_fixture();
        let cfg = battery::Config {
            name: "macrodepth_add_0".to_owned(),
            features: Vec::new(),
            is_lib: false,
            lib_name: None,
        };
        let keyed = || {
            crate::artifact::Publishing::<Translate>::for_test(&real_dir)
                .finish()
                .unwrap()
        };
        propagate_config(&paths, battery, real, &cfg, &keyed()).unwrap();

        let verified = crate::battery::phase_dir(
            &paths.case_dir(battery, &cfg.name),
            crate::battery::VERIFIED,
        );
        std::fs::create_dir_all(&verified).unwrap();
        std::fs::write(verified.join("Cargo.toml"), "[package]").unwrap();

        propagate_config(&paths, battery, real, &cfg, &keyed()).unwrap();
        assert!(
            verified.join("Cargo.toml").is_file(),
            "an identical re-derivation must not delete the follower's verification"
        );

        std::fs::write(published.join("src/lib.rs"), "pub fn a() { /* fixed */ }").unwrap();
        propagate_config(&paths, battery, real, &cfg, &keyed()).unwrap();
        assert!(
            !verified.exists(),
            "and a derivation that MOVED must invalidate it, or the group straddles two phases"
        );
    }

    fn group_fixture() -> (
        tempfile::TempDir,
        Paths,
        &'static str,
        &'static str,
        PathBuf,
        PathBuf,
    ) {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let paths = paths_at(
            tmp.path(),
            Agent::Claude,
            crate::cli::Dataset::TestCorpus,
            cache::Mode::ReadWrite,
        );
        let (battery, real) = ("B02_synthetic", "macrodepth_add_5");
        let real_dir = paths.case_dir(battery, real);
        let published = publish_a_crate(&real_dir);
        std::fs::create_dir_all(published.join("src")).unwrap();
        std::fs::write(published.join("src/lib.rs"), "pub fn a() {}").unwrap();
        (tmp, paths, battery, real, real_dir, published)
    }

    /// A published `translated/` records nothing about the invocation that wrote it, so a skip check
    /// reading one accepts another model's crate as this model's, and publishes numbers. Both values
    /// go to BOTH keyed sites: a site that stops consulting the one it was given is the same bug.
    #[test]
    fn a_published_translation_from_a_different_model_is_not_accepted_as_done() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let corpus = paths_at(
            tmp.path(),
            Agent::Oneshot,
            crate::cli::Dataset::TestCorpus,
            cache::Mode::Bypass,
        );
        let hb = paths_at(
            tmp.path(),
            Agent::Oneshot,
            crate::cli::Dataset::HarvestBench,
            cache::Mode::Bypass,
        );
        let store = cache::Store::open(tmp.path(), corpus.cache_mode).unwrap();

        let battery = "B01_organic";
        let output_dir = corpus.output_dir(battery);
        let case = battery::IndependentCase {
            name: "strcmp".to_owned(),
            is_lib: true,
        };
        for sub in ["test_case", "gtest_suite"] {
            std::fs::create_dir_all(hb.corpus_dir.join("libpng").join(sub)).unwrap();
        }
        let project = battery::HarvestBenchProject::resolve(&hb.corpus_dir, "libpng").unwrap();

        let one_ind =
            |skip| translate_one_independent(&corpus, &output_dir, battery, &case, &store, skip);
        let one_bench = |skip| translate_one_harvest_bench(&hb, &project, "", &store, skip);
        let sites = [
            Site {
                what: "independent",
                published: publish_a_crate(&output_dir.join(&case.name)),
                run: &one_ind,
            },
            Site {
                what: "harvest-bench",
                published: publish_a_crate(&hb.output_dir(project.name())),
                run: &one_bench,
            },
        ];

        for site in sites {
            let what = site.what;
            assert!(
                (site.run)(SkipCheck::WhateverIsPublished).0.skipped,
                "{what}: the fixture must be the tree the old check answered done on, or \
                 nothing here is being tested"
            );
            let (keyed, resolved) = (site.run)(SkipCheck::Keyed);
            assert!(
                resolved.is_none(),
                "{what}: and it resolved no artifact, so nothing downstream can be seeded from \
                 this case at all"
            );
            assert!(
                !keyed.skipped,
                "{what}: a `translated/` names no model, prompt, CLI or toolchain, so it \
                 cannot say WHICH invocation produced it and must not answer for the store"
            );
            assert!(
                !keyed.success,
                "{what}: and it reached the backend, which has no corpus here: {:?}",
                keyed.error
            );
            let aside = displaced(&site.published);
            assert!(
                crate::battery::has_crate(&aside),
                "{what}: a run that produced nothing may not destroy the previous translation, \
                 which nothing replays: {}",
                aside.display()
            );
            assert!(
                !crate::battery::has_crate(&site.published),
                "{what}: nor leave it in the phase dir, where this run's record now says the \
                 translation failed"
            );
        }
    }

    /// Where [`crate::artifact::displace_phase`] leaves what a failed run could not replace.
    fn displaced(phase_dir: &Path) -> std::path::PathBuf {
        phase_dir.with_file_name(format!("{}.displaced", crate::battery::TRANSLATED))
    }

    fn translated(case_dir: &Path) -> std::path::PathBuf {
        crate::battery::phase_dir(case_dir, crate::battery::TRANSLATED)
    }

    /// The battery and case every fixture below uses.
    const A_CASE: (&str, &str) = ("B01_organic", "strcmp");

    const RUN_A_CRATE: &str = "pub fn a() { /* run A's translation */ }";
    const RUN_A_LOG: &str = "run A's transcript\n";
    const RUN_B_LOG: &str = "=== OPENROUTER REQUEST ===\nrun B's transcript, over run A's\n";

    /// The state a failing run arrives into: the previous sweep's crate, the `result.json` the
    /// enrichers rewrite in place, that run's own record, and its transcript.
    fn a_previous_translation(case_dir: &Path) {
        let dir = crate::battery::phase_dir(case_dir, crate::battery::TRANSLATED);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for (rel, body) in [
            ("Cargo.toml", "[package]\nname=\"x\""),
            ("src/lib.rs", RUN_A_CRATE),
            ("result.json", r#"{"tests_passed":5}"#),
            ("translation.json", r#"{"agent":"oneshot","success":true}"#),
        ] {
            std::fs::write(dir.join(rel), body).unwrap();
        }
        let log = crate::artifact::phase_log::<Translate>(case_dir);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, RUN_A_LOG).unwrap();
        assert!(
            crate::battery::has_crate(&dir),
            "the fixture must hold a complete crate, or there is no wrong number to leave"
        );
    }

    /// The unkeyed translate paths, by the two shapes their failures take: the docker arms refuse
    /// before they tee anything, and the one-shot LLM arms tee over the previous transcript first.
    #[derive(Copy, Clone)]
    enum Unkeyed {
        Docker,
        OneShotLlm,
    }

    impl Unkeyed {
        const ALL: &'static [Unkeyed] = &[Unkeyed::Docker, Unkeyed::OneShotLlm];

        fn what(self) -> &'static str {
            match self {
                Unkeyed::Docker => "laertes",
                Unkeyed::OneShotLlm => "oneshot",
            }
        }

        fn paths(self, root: &Path) -> Paths {
            let paths = paths_at(
                root,
                match self {
                    Unkeyed::Docker => Agent::Laertes,
                    Unkeyed::OneShotLlm => Agent::Oneshot,
                },
                crate::cli::Dataset::TestCorpus,
                cache::Mode::Bypass,
            );
            // `require_prompt` refuses an absent one, which would fail ahead of the tee.
            std::fs::create_dir_all(&paths.prompts_dir).unwrap();
            for f in ["translate-library.md", "translate-executable.md"] {
                std::fs::write(
                    paths.prompts_dir.join(f),
                    "translate this C project into Rust.".repeat(8),
                )
                .unwrap();
            }
            paths
        }

        /// The transcript in the phase dir once this arm has failed.
        fn teed(self) -> &'static str {
            match self {
                Unkeyed::Docker => RUN_A_LOG,
                Unkeyed::OneShotLlm => RUN_B_LOG,
            }
        }

        /// `dispatch_translate`'s own composition for this agent, wrapped as the sweep wraps it.
        /// Only the LLM call is stood in for — as in
        /// `a_translation_that_fails_leaves_the_previous_result_standing`, it is the one
        /// injectable failure of the four, and the docker arm needs none.
        fn fail(self, paths: &Paths) -> (CaseResult, Option<Published<Translate>>) {
            let (battery, name) = A_CASE;
            run_and_record(
                name,
                &paths.case_dir(battery, name),
                &paths.agent_key,
                || match self {
                    Unkeyed::Docker => uncached(laertes_translate_case(paths, battery, name)),
                    Unkeyed::OneShotLlm => uncached(oneshot_llm_translate(
                        paths,
                        battery,
                        name,
                        false,
                        None,
                        |_, log| {
                            std::fs::write(log, RUN_B_LOG).unwrap();
                            anyhow::bail!(
                                "api_error 403: the security token included in the request is \
                                 expired"
                            )
                        },
                    )),
                },
                |_| Ok(()),
            )
        }
    }

    /// Run A published a translation; run B is one of the unkeyed paths, failing the way that
    /// backend really fails.
    fn a_failed_unkeyed_translation(root: &Path, backend: Unkeyed) -> (Paths, std::path::PathBuf) {
        let paths = backend.paths(root);
        let (battery, name) = A_CASE;
        let test_case = paths.input_dir(battery).join(name).join("test_case");
        std::fs::create_dir_all(&test_case).unwrap();
        std::fs::write(test_case.join("main.c"), "int main(void){return 0;}").unwrap();
        let case = paths.case_dir(battery, name);
        a_previous_translation(&case);

        let (r, resolved) = backend.fail(&paths);
        assert!(
            !r.success,
            "{}: the injected failure must fail the case: {:?}",
            backend.what(),
            r.error
        );
        assert!(
            resolved.is_none(),
            "{}: and an unkeyed path that failed resolves nothing either",
            backend.what()
        );
        (paths, case)
    }

    /// INVARIANT 1 on the five unkeyed paths. `run_and_record`'s `Err` arm writes `translation.json`
    /// into the phase dir, so with run A's crate still standing the enrichers would stamp run B's
    /// agent, model, cost and timestamp onto its `result.json`.
    #[test]
    fn a_failed_unkeyed_translation_does_not_leave_its_metrics_beside_an_earlier_crate() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        for backend in Unkeyed::ALL {
            let what = backend.what();
            let (_, case) = a_failed_unkeyed_translation(tmp.path(), *backend);
            let dir = translated(&case);

            let m: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(crate::artifact::phase_metrics::<Translate>(&case))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                m["success"],
                serde_json::json!(false),
                "{what}: the failure is recorded rather than withheld: {m}"
            );
            assert!(
                !crate::battery::has_crate(&dir),
                "{what}: and run A's crate is not beside that record, where it would be scored \
                 as run B's result with run B's agent, model and cost"
            );
            assert!(
                !dir.join("result.json").exists(),
                "{what}: nor its score, which is what the enrichers rewrite in place"
            );
            assert!(
                !crate::battery::has_crate(&crate::battery::phase_dir(
                    &case,
                    crate::battery::VERIFIED
                )),
                "{what}: and no other phase dir holds one either, so this run resolved nothing \
                 for this case rather than the wrong thing"
            );
            assert_eq!(
                std::fs::read_to_string(crate::artifact::phase_log::<Translate>(&case)).unwrap(),
                backend.teed(),
                "{what}: while whatever transcript the phase dir holds stays — it is the \
                 post-mortem, and all the infra gate reads this case through"
            );
        }
    }

    /// INVARIANT 2 where there is no store entry to fall back on: an unkeyed backend writes none,
    /// so a delete here turns one 403 into the permanent loss of a paid crate.
    #[test]
    fn a_failed_unkeyed_translation_does_not_make_the_earlier_crate_unrecoverable() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        for backend in Unkeyed::ALL {
            let what = backend.what();
            let (paths, case) = a_failed_unkeyed_translation(tmp.path(), *backend);

            let aside = displaced(&translated(&case));
            assert!(
                crate::battery::has_crate(&aside),
                "{what}: run A's crate must still be on disk, whole: {}",
                aside.display()
            );
            assert_eq!(
                std::fs::read_to_string(aside.join("src/lib.rs")).unwrap(),
                RUN_A_CRATE,
                "{what}: and be run A's crate rather than an empty shell of one"
            );
            for kept in ["result.json", "translation.json"] {
                assert!(
                    aside.join(kept).is_file(),
                    "{what}: with the {kept} it was measured at, which is run A's too"
                );
            }
            assert_eq!(
                cache::Store::open(&paths.repo_root, paths.cache_mode)
                    .unwrap()
                    .stats()
                    .unwrap()
                    .0,
                0,
                "{what}: non-vacuity for this whole test — these paths store no entry, so the \
                 copy that was moved is the only one there is"
            );
        }
    }

    /// The consequence that reaches a published number. `SkipCheck::WhateverIsPublished` is all
    /// these paths have, so a phase dir left holding run A's crate reads as done on the next
    /// sweep: the case is never re-run and the mismatched pair is scored as a result for good.
    #[test]
    fn a_case_the_previous_sweep_failed_is_not_read_as_done() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        // The docker arm, because it fails with no network and no CLI, so the sweep's own entry
        // point can be driven whole rather than stood in for.
        let backend = Unkeyed::Docker;
        let paths = backend.paths(tmp.path());
        let (battery, name) = A_CASE;
        let output_dir = paths.output_dir(battery);
        let case = battery::IndependentCase {
            name: name.to_owned(),
            is_lib: true,
        };
        let store = cache::Store::open(&paths.repo_root, paths.cache_mode).unwrap();
        // The resolver's own answer, not a literal: `Keyed` here would skip nothing whatever the
        // phase dir holds, and this test would pass while asserting nothing.
        let sweep = || {
            translate_one_independent(
                &paths,
                &output_dir,
                battery,
                &case,
                &store,
                translate_skip_check(&paths),
            )
        };

        a_previous_translation(&paths.case_dir(battery, name));
        assert!(
            sweep().0.skipped,
            "non-vacuity: a published crate IS read as done here — that is the whole of what \
             this path's skip check can ask"
        );

        assert!(!backend.fail(&paths).0.success);
        let (next, _) = sweep();
        assert!(
            !next.skipped,
            "a case whose last run failed must be translated again, not counted as translated"
        );
        assert!(
            !next.success,
            "and it really reached the backend: {:?}",
            next.error
        );
    }

    const DRIVER_CRATE: &str = "pub fn a() { /* what the driver published */ }";
    const DRIVER_ENTRY: &str = "translate/deadbeef";

    /// What [`run_cached`] leaves behind for a completed run or a hit: a sealed crate published
    /// into the phase dir, and the record naming the entry it came from.
    fn a_driver_published_translation(root: &Path, case_dir: &Path) {
        let corpus = root.join("corpus");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(corpus.join("lib.c"), "int a(void){return 0;}").unwrap();
        let work = IsolatedWorkDir::<Translate>::from_corpus(&corpus).unwrap();
        let crate_dir = work.translated_rust();
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        std::fs::write(crate_dir.join("src/lib.rs"), DRIVER_CRATE).unwrap();
        work.finish(&crate::domain::health::Completed::for_test())
            .unwrap()
            .publish(case_dir)
            .unwrap()
            .finish()
            .unwrap();
        write_phase_metrics::<Translate>(
            case_dir,
            &serde_json::json!({"agent": "claude", "duration_secs": 42, "success": true}),
            Recorded::Fresh {
                entry: Some(DRIVER_ENTRY),
            },
        );
    }

    /// The hazard in giving the unkeyed paths a displacement: [`run_and_record`] wraps the KEYED
    /// path too, where the driver has already published this run's crate and recorded the entry it
    /// came from. Displaced unconditionally — up front, or in both arms — that moves a paid,
    /// scoreable result out of the tree the scorer reads.
    #[test]
    fn a_driver_published_artifact_is_never_displaced() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        // Over the whole of `RecordedBy` rather than one hand-picked variant: the decision is on
        // the `Result`, so every `Ok` must leave the artifact where its publisher put it.
        for (what, recorded) in [
            ("driver", RecordedBy::Driver),
            ("caller", RecordedBy::Caller),
        ] {
            let case = tmp.path().join(what);
            a_driver_published_translation(tmp.path(), &case);

            let (r, resolved) = run_and_record(
                A_CASE.1,
                &case,
                &test_agent_key(),
                || {
                    Ok(Resolution::Published(Translated {
                        recorded,
                        publishing: Publishing::for_test(&case),
                    }))
                },
                |_| Ok(()),
            );
            assert!(r.success, "{what}: {:?}", r.error);
            assert!(
                resolved.is_some(),
                "{what}: a published translation must be handed on, or verify has nothing to be \
                 seeded from"
            );
            assert_eq!(
                std::fs::read_to_string(translated(&case).join("src/lib.rs")).unwrap_or_default(),
                DRIVER_CRATE,
                "{what}: the crate must stay in the phase dir it was published into"
            );
            assert!(
                !displaced(&translated(&case)).exists(),
                "{what}: and nothing may be moved aside"
            );
        }

        let entry_of = |what: &str| -> serde_json::Value {
            let p = crate::artifact::phase_metrics::<Translate>(&tmp.path().join(what));
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(p).unwrap()).unwrap()
                ["cache_key"]
                .clone()
        };
        assert_eq!(
            entry_of("driver"),
            serde_json::json!(DRIVER_ENTRY),
            "the driver's record carries the entry that was served; a second write blanks it"
        );
        assert_eq!(
            entry_of("caller"),
            serde_json::Value::Null,
            "while a caller-recorded run reached no store and has no entry to name, which is what \
             the two variants are for"
        );

        // Non-vacuity: the same fixture, failing, IS displaced — so the assertions above hold
        // because the run succeeded and not because nothing here ever moves.
        let case = tmp.path().join("failed");
        a_driver_published_translation(tmp.path(), &case);
        let (r, _) = run_and_record(
            A_CASE.1,
            &case,
            &test_agent_key(),
            || anyhow::bail!("the agent did not complete"),
            |_| Ok(()),
        );
        assert!(!r.success);
        assert!(!crate::battery::has_crate(&translated(&case)));
        assert!(crate::battery::has_crate(&displaced(&translated(&case))));

        // And the THIRD arm: the driver published nothing, having already displaced the crate, written
        // its record and filed the failed run. Re-recording blanks the `cache_key` and drops `exit_code`.
        let case = tmp.path().join("driver-published-nothing");
        a_driver_published_translation(tmp.path(), &case);
        let (r, resolved) = run_and_record(
            A_CASE.1,
            &case,
            &test_agent_key(),
            || Ok(Resolution::Recorded),
            |_| Ok(()),
        );
        assert!(
            !r.success,
            "the case has no result and must not be reported as one"
        );
        assert!(
            resolved.is_none(),
            "and nothing downstream may be seeded from it"
        );
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("translation.json")),
            "the operator is pointed at the record the driver wrote: {:?}",
            r.error
        );
        assert_eq!(
            entry_of("driver-published-nothing"),
            serde_json::json!(DRIVER_ENTRY),
            "which must still name the entry the driver recorded, not a second, blanker write"
        );
    }

    /// Exhaustive over `Dataset`: the value is in the translate recipe, so a dataset on the wrong ceiling
    /// either gets a limit measured for something smaller or re-keys the other dataset's entries.
    #[test]
    fn each_dataset_gets_its_own_translate_ceiling_and_test_corpus_does_not_move() {
        for (dataset, want) in [
            (crate::cli::Dataset::TestCorpus, 10_800),
            (crate::cli::Dataset::HarvestBench, 86_400),
        ] {
            assert_eq!(translate_ceiling(dataset), want, "{dataset:?}");
        }
        assert_ne!(
            Session::claude(translate_ceiling(crate::cli::Dataset::TestCorpus)).shape(),
            Session::claude(translate_ceiling(crate::cli::Dataset::HarvestBench)).shape(),
            "the two must be different recipes, or scoping bought nothing"
        );
    }

    /// The honest limit of a keyless BACKEND: where no key can be asked about, "is something published
    /// here" is all there is to ask, and it stays. A shared-source group used to be the other half of
    /// this and no longer is — it is keyed like any other case, so it may not adopt what it finds.
    #[test]
    fn a_bypassed_backend_still_skips_on_a_published_crate() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        // `ReadWrite`, so a store that never reads is not what makes the answers below unkeyed.
        let corpus = crate::cli::Dataset::TestCorpus;
        let paths = paths_at(tmp.path(), Agent::Oneshot, corpus, cache::Mode::ReadWrite);
        let (battery, name) = ("B01_organic", "strcmp");
        let output_dir = paths.output_dir(battery);
        let translated = publish_a_crate(&output_dir.join(name));
        let store = cache::Store::open(&paths.repo_root, paths.cache_mode).unwrap();
        let case = battery::IndependentCase {
            name: name.to_owned(),
            is_lib: true,
        };

        let (r, _) = translate_one_independent(
            &paths,
            &output_dir,
            battery,
            &case,
            &store,
            translate_skip_check(&paths),
        );
        assert!(
            r.skipped && r.success,
            "a single API call resolves no `Launch`, so there is nothing else to ask: {:?}",
            r.error
        );
        assert!(
            crate::battery::has_crate(&translated),
            "and the crate it answered from is untouched"
        );

        // A group takes no such licence any more: keyed, it must ask the store, and a store that may
        // only replay and holds no entry refuses rather than adopting the crate already there.
        let replay = paths_at(tmp.path(), Agent::Claude, corpus, cache::Mode::ReplayOnly);
        let group = battery::SharedSourceGroup {
            real_case: "macrodepth_add_5".to_owned(),
            configs: Vec::new(),
        };
        let real = publish_a_crate(&output_dir.join(&group.real_case));
        let group_store = cache::Store::open(&replay.repo_root, replay.cache_mode).unwrap();
        let (g, resolved) = translate_one_shared(
            &replay,
            &output_dir,
            battery,
            &group,
            &group_store,
            translate_skip_check(&replay),
        );
        assert!(
            !g.skipped,
            "a keyed group may not adopt the crate already published: {:?}",
            g.error
        );
        assert!(resolved.is_none() && !g.success, "and it resolves nothing");
        assert!(
            !crate::battery::has_crate(&real)
                && output_dir
                    .join(&group.real_case)
                    .join(format!("{}.displaced", crate::battery::TRANSLATED))
                    .join("Cargo.toml")
                    .is_file(),
            "the crate it would once have adopted is DISPLACED, not deleted and not left in place \
             for the next run to read"
        );

        // THE WHOLE DECISION, both halves, as a sweep resolves it — here, because a sweep needs a CLI
        // and a corpus. Keyed without a key re-bills every case; keyed through a store that never
        // reads deletes that path's only check; unkeyed claude by default reverts this PR.
        for (agent, mode, expected) in [
            (Agent::Claude, cache::Mode::ReadWrite, SkipCheck::Keyed),
            (Agent::Claude, cache::Mode::Refresh, SkipCheck::Keyed),
            (
                Agent::Claude,
                cache::Mode::Bypass,
                SkipCheck::WhateverIsPublished,
            ),
            (Agent::Kiro, cache::Mode::ReadWrite, SkipCheck::Keyed),
            (
                Agent::OpenCode,
                cache::Mode::ReadWrite,
                SkipCheck::WhateverIsPublished,
            ),
            // Keyed since codex became a real backend: it used to be its own unkeyed `Launch`
            // variant, which is why no codex sweep ever left an entry to replay.
            (Agent::CodexGpt55, cache::Mode::ReadWrite, SkipCheck::Keyed),
            (
                Agent::CodexGpt56Sol,
                cache::Mode::ReadWrite,
                SkipCheck::Keyed,
            ),
            (
                Agent::C2rust,
                cache::Mode::ReadWrite,
                SkipCheck::WhateverIsPublished,
            ),
            (
                Agent::Oneshot,
                cache::Mode::ReadWrite,
                SkipCheck::WhateverIsPublished,
            ),
        ] {
            assert_eq!(
                translate_skip_check(&paths_at(tmp.path(), agent, corpus, mode)),
                expected,
                "--agent {agent:?} --cache {mode:?}"
            );
        }

        // And the pairing: a backend `skip_check` calls keyless must be one `resolve_launch` answers
        // no `Launch::Keyed` for. Only these two need no CLI on PATH — which is the same line, and
        // is why codex left this list when it gained a key.
        for backend in [InTool::OpenCode, InTool::C2rust] {
            let launch = resolve_launch(
                &paths_at(tmp.path(), Agent::OpenCode, corpus, cache::Mode::ReadWrite),
                backend,
            )
            .unwrap();
            assert!(
                !matches!(launch, Launch::Keyed(_)),
                "{:?} is skipped on its published crate, so it must reach the store under no key",
                skip_check(backend)
            );
        }
    }

    /// The two configurations no key can name could not complete a `verify` sweep AT ALL: `--agent
    /// opencode`, and every battery of symlinked configs (B02_synthetic, P01_sphincs_plus, both
    /// published). What is published is now the seed, recorded
    /// [`crate::artifact::Keying::Unkeyable`]; what is NOT published is still refused, and so is
    /// anything keyed — which is 014.
    #[test]
    fn a_sweep_that_may_not_pay_seeds_what_is_published_and_refuses_what_is_not() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let corpus = crate::cli::Dataset::TestCorpus;
        // From the production mapping: a literal would pass while `cli::seeding` sent a sweep elsewhere.
        let replay = crate::cli::seeding(cache::Mode::ReadWrite).unwrap();
        let paths = paths_at(tmp.path(), Agent::Oneshot, corpus, replay);
        let battery = "B01_organic";
        let output_dir = paths.output_dir(battery);
        let store = cache::Store::open(&paths.repo_root, paths.cache_mode).unwrap();
        let one = |name: &str, skip| {
            translate_one_independent(
                &paths,
                &output_dir,
                battery,
                &battery::IndependentCase {
                    name: name.to_owned(),
                    is_lib: true,
                },
                &store,
                skip,
            )
        };

        let translated = publish_a_crate(&output_dir.join("strcmp"));
        let (r, resolved) = one("strcmp", translate_skip_check(&paths));
        assert_eq!(
            resolved.map(|p| p.keying()),
            Some(crate::artifact::Keying::Unkeyable),
            "a single API call has no entry to replay, so the tree already published is the only \
             translation there is — and the sweep must say no key names it: {:?}",
            r.error
        );
        assert!(r.skipped && r.success, "{:?}", r.error);
        assert!(
            crate::battery::has_crate(&translated)
                && !crate::artifact::phase_metrics::<Translate>(&output_dir.join("strcmp"))
                    .exists(),
            "no agent ran, so the crate is untouched and no record claims an attempt"
        );

        // The same path with NOTHING published: still refused, or `refuse_absent` could not fire.
        let (empty, resolved) = one("memcmp", translate_skip_check(&paths));
        assert!(
            resolved.is_none() && !empty.success && !empty.skipped,
            "{:?}",
            empty.error
        );
        assert!(
            empty
                .error
                .as_deref()
                .is_some_and(|e| e.contains("read-only") && e.contains("translate")),
            "and the operator is told what to run instead: {:?}",
            empty.error
        );

        // A KEYED sweep (claude, kiro): `SkipCheck::Keyed` never answers from a phase dir, so with
        // no entry the case resolves nothing — 014's shape. c2rust, to skip the CLI probe.
        let unkeyed_launch = paths_at(tmp.path(), Agent::C2rust, corpus, replay);
        std::fs::create_dir_all(unkeyed_launch.input_dir(battery).join("keyed/test_case")).unwrap();
        std::fs::write(
            unkeyed_launch
                .input_dir(battery)
                .join("keyed/test_case/lib.c"),
            "int a(void){return 0;}",
        )
        .unwrap();
        let keyed_crate = publish_a_crate(&output_dir.join("keyed"));
        let (k, resolved) = translate_one_independent(
            &unkeyed_launch,
            &output_dir,
            battery,
            &battery::IndependentCase {
                name: "keyed".to_owned(),
                is_lib: true,
            },
            &store,
            SkipCheck::Keyed,
        );
        assert!(
            resolved.is_none() && !k.success && !k.skipped,
            "a keyed check may not resolve a case from the crate sitting in its phase dir: {:?}",
            k.error
        );
        assert!(crate::battery::has_crate(&keyed_crate));
        assert_eq!(
            translate_skip_check(&paths_at(tmp.path(), Agent::Claude, corpus, replay)),
            SkipCheck::Keyed,
            "and claude IS keyed under the mode `verify` seeds with, so that is the branch it takes"
        );

        // A group, with a KEYED agent: keyed like any other case now, so a sweep that may not pay and
        // has no entry refuses it — whether or not a crate is already published there.
        let keyed = paths_at(tmp.path(), Agent::Claude, corpus, replay);
        let keyed_store = cache::Store::open(&keyed.repo_root, keyed.cache_mode).unwrap();
        for real_case in ["macrodepth_add_5", "macrodepth_add_9"] {
            let group = battery::SharedSourceGroup {
                real_case: real_case.to_owned(),
                configs: Vec::new(),
            };
            if real_case.ends_with('5') {
                publish_a_crate(&output_dir.join(real_case));
            }
            let (g, resolved) = translate_one_shared(
                &keyed,
                &output_dir,
                battery,
                &group,
                &keyed_store,
                translate_skip_check(&keyed),
            );
            assert!(
                resolved.is_none() && !g.success,
                "{real_case}: a group with no entry must refuse, published crate or not: {:?}",
                g.error
            );
        }

        // THE mapping, over every mode a `Paths` can carry: only one refuses, and under it only a
        // keyed check resolves — without both halves, a predicate answering alike would pass too.
        for (mode, skip, expected) in [
            (cache::Mode::ReplayOnly, SkipCheck::Keyed, true),
            (
                cache::Mode::ReplayOnly,
                SkipCheck::WhateverIsPublished,
                false,
            ),
            (cache::Mode::ReadWrite, SkipCheck::WhateverIsPublished, true),
            (cache::Mode::Refresh, SkipCheck::WhateverIsPublished, true),
            (cache::Mode::Bypass, SkipCheck::WhateverIsPublished, true),
        ] {
            assert_eq!(
                resolvable(&paths_at(tmp.path(), Agent::Claude, corpus, mode), skip),
                expected,
                "--cache {mode:?} with {skip:?}"
            );
        }
    }

    /// `--agent laertes translate HB/<project>` reached `translate_case_at`, hit an
    /// `unreachable!()` and was caught by `CaseResult::panicked`, so every project read as an
    /// ordinary ❌ — indistinguishable from a translation that genuinely failed.
    #[test]
    fn an_agent_with_no_in_tool_translate_phase_refuses_instead_of_panicking() {
        let paths = paths_for(Agent::Laertes, crate::cli::Dataset::HarvestBench);
        let err = run_harvest_bench(&paths, &[], &crate::agents::Pool::for_run(1))
            .expect_err("must refuse, not panic");
        assert!(
            format!("{err:#}").contains("no in-tool translate phase"),
            "{err:#}"
        );
        // ...and the refusal discriminates: the agents that have one are not refused.
        assert!(in_tool_translate(Agent::Claude).is_some());
        assert!(in_tool_translate(Agent::Laertes).is_none());
    }

    /// The writer must land where `agent_health::collect` and `battery.rs` look, or the
    /// timeout signal (`timeout` exits 124) is written and read by nobody.
    #[test]
    fn translation_metrics_land_where_the_readers_look() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("mujs");
        clear_agent_exit();
        record_agent_exit(exit_status(124));
        write_translation_metrics(&case, &test_agent_key(), 10_800, false);

        let expected = case
            .join(crate::battery::TRANSLATED)
            .join("translation.json");
        assert!(
            expected.is_file(),
            "the reader path must be the writer path"
        );
        assert!(
            !case.join("translation.json").exists(),
            "and nothing may be left at the case root"
        );
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&expected).unwrap()).unwrap();
        assert_eq!(m["exit_code"], serde_json::json!(124));
        assert_eq!(m["timed_out"], serde_json::json!(true));
        assert_eq!(
            crate::agent_health::exit_code(&expected),
            Some(124),
            "the reader must now actually reach it",
        );
    }

    #[test]
    fn a_panic_after_the_metrics_were_written_does_not_overwrite_them() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("libpng");
        clear_agent_exit();
        record_agent_exit(exit_status(124));
        write_translation_metrics(&case, &test_agent_key(), 10_800, false);
        let before =
            std::fs::read_to_string(crate::artifact::phase_metrics::<Translate>(&case)).unwrap();

        // The joining thread carries some OTHER case's exit; it must not be borrowed.
        record_agent_exit(exit_status(0));
        let r = CaseResult::panicked("libpng".into(), &case, &test_agent_key(), Box::new("boom"));

        assert!(!r.success);
        assert_eq!(
            std::fs::read_to_string(crate::artifact::phase_metrics::<Translate>(&case)).unwrap(),
            before,
            "the real 3h/124 record must survive the panic report",
        );
    }

    #[test]
    fn a_panic_with_no_record_yet_writes_one_without_borrowing_an_exit_code() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("jansson");
        clear_agent_exit();
        // Belongs to whatever case this thread ran before, NOT to jansson.
        record_agent_exit(exit_status(0));

        CaseResult::panicked("jansson".into(), &case, &test_agent_key(), Box::new("boom"));

        let m: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(crate::artifact::phase_metrics::<Translate>(&case)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            m["success"],
            serde_json::json!(false),
            "the panic must leave a trace"
        );
        assert!(
            m.get("exit_code").is_none(),
            "another case's exit must not be attributed here"
        );
        assert!(m.get("timed_out").is_none());
    }

    fn write_log(body: &str) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), body).unwrap();
        f
    }

    #[test]
    fn opencode_scanner_retries_a_throttle_that_produced_nothing() {
        // Otherwise the empty result would be scored as a real failure.
        let log = write_log(
            r#"{"type":"error","error":{"message":"ThrottlingException: rate exceeded"}}"#,
        );
        assert_eq!(
            scan_opencode_log_for_transient_error(log.path()).as_deref(),
            Some("throttled"),
        );
    }

    #[test]
    fn opencode_scanner_does_not_retry_after_real_work() {
        // Retrying after the session recovered would DISCARD a completed translation.
        let log = write_log(
            "{\"type\":\"error\",\"error\":{\"message\":\"ThrottlingException\"}}\n\
             {\"type\":\"tool\",\"name\":\"write\"}\n",
        );
        assert_eq!(scan_opencode_log_for_transient_error(log.path()), None);
    }

    #[test]
    fn opencode_scanner_ignores_clean_and_nontransient_logs() {
        let clean = write_log(r#"{"type":"step","name":"done"}"#);
        assert_eq!(scan_opencode_log_for_transient_error(clean.path()), None);
        // A non-transient error must not be retried: retrying cannot fix it and
        // burns hours.
        let hard =
            write_log(r#"{"type":"error","error":{"message":"ValidationException: bad request"}}"#);
        assert_eq!(scan_opencode_log_for_transient_error(hard.path()), None);
    }

    #[test]
    fn both_bedrock_backends_share_one_transient_pattern_table() {
        // The failures come from Bedrock, not the CLI wrapping it, so both scanners
        // must agree on what is retryable.
        for (needle, label) in TRANSIENT_BEDROCK_PATTERNS {
            assert_eq!(first_transient_pattern(needle).as_deref(), Some(*label));
        }
    }

    fn paths_for(agent: Agent, dataset: crate::cli::Dataset) -> Paths {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tools/ has a parent");
        let model = match agent {
            Agent::Oneshot => Some("openai/gpt-5.4"),
            Agent::OpenCode => Some("amazon-bedrock/us.anthropic.claude-sonnet-5"),
            _ => None,
        };
        Paths::new(
            root,
            agent,
            dataset,
            model,
            crate::cache::Mode::Bypass,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .expect("Paths")
    }

    #[test]
    fn every_prompt_the_matrix_names_is_on_disk() {
        // The whole point of naming the file separately from reading it: a rename used
        // to surface as an empty prompt handed to a live agent, mid-sweep.
        use clap::ValueEnum;
        for dataset in [
            crate::cli::Dataset::TestCorpus,
            crate::cli::Dataset::HarvestBench,
        ] {
            for agent in Agent::value_variants() {
                let paths = paths_for(*agent, dataset);
                for kind in PromptKind::ALL {
                    if prompt_file_for(*agent, *kind).is_none() {
                        continue;
                    }
                    let text = read_prompt(&paths, *kind)
                        .unwrap_or_else(|e| panic!("{agent:?} {kind:?} {dataset:?}: {e:#}"));
                    assert!(
                        text.is_some_and(|t| t.len() > 100),
                        "{agent:?} {kind:?}: too short to be a prompt"
                    );
                }
            }
        }
    }

    #[test]
    fn a_verify_prompt_exists_exactly_where_a_verify_phase_does() {
        use clap::ValueEnum;
        for agent in Agent::value_variants() {
            assert_eq!(
                prompt_file_for(*agent, PromptKind::Verify).is_some(),
                crate::agents::invocation::has_verify_phase(*agent),
                "{agent:?}: the verify prompt and the verify backend disagree, so either a \
                 phase would run with no prompt or a prompt is named for a phase that never runs"
            );
        }
    }

    #[test]
    fn each_ablation_differs_from_claude_exactly_where_its_experiment_says() {
        use PromptKind::{Executable, Library, Shared};
        let claude = |k| prompt_file_for(Agent::Claude, k);

        // E4: libraries get the executable prompt and vice versa — the swap IS the arm.
        assert_eq!(
            prompt_file_for(Agent::ClaudeCrossPrompt, Library),
            claude(Executable)
        );
        assert_eq!(
            prompt_file_for(Agent::ClaudeCrossPrompt, Executable),
            claude(Library)
        );
        assert_eq!(
            prompt_file_for(Agent::ClaudeCrossPrompt, Shared),
            claude(Shared)
        );

        // E2 and E6 vary the shared-source prompt only; their independent cases must be
        // byte-identical to claude's or the arm measures more than one change.
        for (agent, shared) in [
            (
                Agent::ClaudeNoFeatures,
                "ablations/translate-no-features-shared.md",
            ),
            (
                Agent::ClaudeNoSubtask,
                "ablations/translate-no-subtask-shared.md",
            ),
        ] {
            assert_eq!(
                prompt_file_for(agent, Library),
                claude(Library),
                "{agent:?}"
            );
            assert_eq!(
                prompt_file_for(agent, Executable),
                claude(Executable),
                "{agent:?}"
            );
            assert_eq!(prompt_file_for(agent, Shared), Some(shared));
        }

        // The calibration baseline is one universal prompt for every project type.
        for k in [Library, Executable, Shared] {
            assert_eq!(
                prompt_file_for(Agent::ClaudeMinimal, k),
                Some("ablations/translate-minimal.md")
            );
        }

        // Kiro, OpenCode and Codex are portability arms: same prompts, different harness.
        for agent in [
            Agent::Kiro,
            Agent::OpenCode,
            Agent::CodexGpt55,
            Agent::CodexGpt54,
        ] {
            for k in [Library, Executable, Shared] {
                assert_eq!(prompt_file_for(agent, k), claude(k), "{agent:?} {k:?}");
            }
        }
    }
}
