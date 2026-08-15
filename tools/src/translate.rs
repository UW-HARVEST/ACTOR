use crate::battery::{self, Case, Paths};
use crate::cargo_toml::{self, CargoToml};
use crate::cli::Agent;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

// ── claude_plain agent (mirrors kiro_plain) ────────────────────────────
// Deliberately matches kiro_plain.json (built-in tools only, no skills/plugins/
// MCP, no extra system prompt) so the two agents are compared on equal footing.
pub const CLAUDE_PLAIN_AGENT_JSON: &str = r#"{"claude_plain":{"description":"Bare-bones agent matching kiro_plain","prompt":"You are a coding assistant. Use the available tools to complete the user's task.","tools":["Bash","Edit","Read","Write","Task"]}}"#;

/// Agent-runtime settings that materially change how a session behaves.
///
/// They belong here, not in the shell driver as bare `export`s, so
/// [`crate::cache::Recipe`] hashes them by construction: otherwise two sweeps with
/// different retry policy would share a cache entry.
pub const AGENT_ENV: &[(&str, &str)] = &[
    // A single request may legitimately run this long on a large project; the
    // per-session wall clock is bounded separately by `timeout`.
    ("API_TIMEOUT_MS", "1200000"),
    ("API_FORCE_IDLE_TIMEOUT", "0"),
    // Bedrock throttles under concurrency; without generous retries a throttle
    // becomes a dead case, which #67 then correctly refuses to score.
    ("CLAUDE_CODE_MAX_RETRIES", "20"),
    ("CLAUDE_CODE_RETRY_WATCHDOG", "1"),
];

/// The model every `--agent claude` invocation is pinned to.
///
/// Must be pinned explicitly: the CLI auto-updates, so an unpinned run is attributed
/// to whatever model it defaulted to that day. The cache key also has to name the
/// model *before* the run, and the resolved model only appears in the transcript's
/// `init` record — after the money is spent.
///
/// A Bedrock inference-profile id because `CLAUDE_CODE_USE_BEDROCK` is set;
/// `HARVEST_CLAUDE_MODEL` overrides it for an environment routed differently.
pub const CLAUDE_MODEL_DEFAULT: &str = "global.anthropic.claude-opus-5[1m]";

pub fn claude_model() -> Result<crate::cache::ModelId> {
    let raw = std::env::var("HARVEST_CLAUDE_MODEL")
        .unwrap_or_else(|_| CLAUDE_MODEL_DEFAULT.to_string());
    crate::cache::ModelId::new(raw)
}

/// Fail loudly if the CLI did not honour the model pin, or is not the build the key
/// was computed for.
///
/// `--model` is a request, not a guarantee: an unrecognised id can be silently
/// substituted, and the sweep would then be cached under a key naming a model that
/// never ran. The CLI build has the same problem from the other end — it auto-updates
/// through a shim, so the binary can change between the probe and the run. The
/// transcript's `init` record is the CLI's own report of both.
pub fn assert_pins_honoured(
    log_path: &Path,
    want: &crate::cache::ModelId,
    cli: &crate::cache::CliVersion,
) -> Result<()> {
    let text = match std::fs::read_to_string(log_path) {
        Ok(t) => t,
        // The health classifier already treats a missing log as a non-completion.
        Err(_) => return Ok(()),
    };
    let Some(init) = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|r| r["type"] == "system" && r["subtype"] == "init")
    else {
        // Older transcripts predate the init record; nothing to compare against.
        return Ok(());
    };
    // A typed refusal, not an ensure!: the sweep collects these and fails the command,
    // where an anyhow error would have been folded into an ordinary red X.
    if let Some(got) = init["model"].as_str() {
        if got != want.as_str() {
            return Err(crate::refusal::Refusal::ModelSubstituted {
                asked: want.as_str().to_string(),
                got: got.to_string(),
            }
            .into());
        }
    }
    if let Some(got) = init["claude_code_version"].as_str() {
        anyhow::ensure!(
            cli.covers(got),
            "the CLI upgraded under the run: the key names {}, the session reports {got}. \
             Refusing to store this artifact under a key naming another build.",
            cli.as_str()
        );
    }
    Ok(())
}

/// Wall-clock cap on one agentic session. Reaches the command through
/// [`crate::session::Session`], which is also what the cache key records.
const TRANSLATE_TIMEOUT_SECS: u64 = 10800;

const KIRO_TRANSLATE_TIMEOUT_SECS: u64 = 5400;

fn opencode_model(paths: &Paths) -> Result<crate::opencode::Model> {
    let raw = paths.model.as_deref().context(
        "--agent opencode requires --model <provider>/<model-id> (should have been \
         rejected at startup)",
    )?;
    crate::opencode::parse_model(raw)
}

// ── Semaphore ──────────────────────────────────────────────────────────

pub struct Semaphore {
    state: Mutex<usize>,
    cvar: Condvar,
    max: usize,
}

impl Semaphore {
    pub fn new(max: usize) -> Self {
        Self { state: Mutex::new(0), cvar: Condvar::new(), max }
    }
    /// Poison is recovered, not propagated: a `usize` is never half-updated and the
    /// panicking worker's guard still ran, so the count is sound — while propagating
    /// would panic every sibling worker that next acquires.
    pub fn acquire(&self) -> SemaphoreGuard<'_> {
        let mut count = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *count >= self.max {
            count = self.cvar.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count += 1;
        SemaphoreGuard(self)
    }
}

pub struct SemaphoreGuard<'a>(&'a Semaphore);

impl Drop for SemaphoreGuard<'_> {
    fn drop(&mut self) {
        // Runs while unwinding, where a second panic aborts the process; see `acquire`.
        *self.0.state.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.cvar.notify_one();
    }
}

// ── Result type ────────────────────────────────────────────────────────

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
        if !translation_metrics_path(case_dir).is_file() {
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

// ── Public entry point ─────────────────────────────────────────────────

/// Zero discovered cases almost always means a case dir was passed where a battery
/// was expected, so the message spells the layout out rather than reporting
/// "0/0 translated".
fn ensure_cases_found(count: usize, paths: &Paths, battery_name: &str) -> Result<()> {
    if count > 0 { return Ok(()); }
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

pub fn run_test_corpus(paths: &Paths, battery_name: &str, filter: Option<&str>, parallel: usize) -> Result<()> {
    preflight_check(paths.agent)?;

    let battery = battery::discover(&paths.corpus_dir, battery_name, filter)?;
    let output_dir = paths.output_dir(battery_name);
    std::fs::create_dir_all(&output_dir)?;

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

    // ── Parallel: independent cases ────────────────────────────────────
    let sem = Semaphore::new(parallel);
    let ind_results: Vec<CaseResult> = std::thread::scope(|s| {
        let handles: Vec<(String, _)> = independent.iter().map(|c| {
            // Paired with the handle so a panicked join still names its case.
            (c.name.clone(), s.spawn(|| {
                let _permit = sem.acquire();
                translate_one_independent(paths, &output_dir, battery_name, c)
            }))
        }).collect();
        handles.into_iter()
            .map(|(name, h)| {
                let case_dir = output_dir.join(&name);
                h.join().unwrap_or_else(|e| CaseResult::panicked(name, &case_dir, &paths.agent_key, e))
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
            println!("  ✅ {} ({}s) [{translated} translated, {failed} failed of {current}/{total}]", r.name, r.elapsed_secs);
        } else {
            failed += 1;
            let err = r.error.as_deref().unwrap_or("unknown error");
            println!("  ❌ {} — {err} ({}s) [{translated} translated, {failed} failed of {current}/{total}]", r.name, r.elapsed_secs);
        }
    }

    // ── Sequential: shared-source groups ───────────────────────────────
    for group in &shared {
        current += 1;
        let r = translate_one_shared(paths, &output_dir, battery_name, group);

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

        for cfg in &group.configs {
            current += 1;
            if crate::battery::has_crate(&crate::battery::phase_dir(&output_dir.join(&cfg.name), crate::battery::TRANSLATED)) {
                translated += 1;
                println!("[{current}/{total}] ⏭️  {} (already done)", cfg.name);
                continue;
            }
            match propagate_config(paths, battery_name, &group.real_case, cfg) {
                Ok(()) => {
                    translated += 1;
                    println!("[{current}/{total}] 🔗 {} → {}", cfg.name, group.real_case);
                }
                Err(e) => {
                    failed += 1;
                    println!("[{current}/{total}] ❌ {} — {e}", cfg.name);
                }
            }
        }
    }

    println!();
    println!("Done: {translated}/{total} translated, {failed} failed");
    Ok(())
}

// ── Per-case translation (no shared state) ─────────────────────────────

fn translate_one_independent(
    paths: &Paths,
    output_dir: &Path,
    battery_name: &str,
    case: &battery::IndependentCase,
) -> CaseResult {
    if crate::battery::has_crate(&crate::battery::phase_dir(&output_dir.join(&case.name), crate::battery::TRANSLATED)) {
        return CaseResult { name: case.name.clone(), elapsed_secs: 0, success: true, error: None, skipped: true };
    }

    run_and_record(&case.name, &output_dir.join(&case.name), &paths.agent_key,
        || dispatch_translate(paths, battery_name, &case.name, case.is_lib),
        || {
            if paths.agent == Agent::ClaudeCrossPrompt {
                // E4: the agent's lib-vs-bin choice IS the experiment, so it must not
                // be overridden here; `[workspace]` is still needed so cargo does not
                // absorb each case into a parent workspace.
                let cargo_path = crate::battery::phase_dir(&paths.case_dir(battery_name, &case.name), crate::battery::TRANSLATED).join("Cargo.toml");
                if cargo_path.exists() {
                    if let Ok(mut cargo) = CargoToml::open(&cargo_path) {
                        cargo.add_workspace();
                        let _ = cargo.save();
                    }
                }
                Ok(())
            } else {
                post_process_independent(paths, battery_name, &case.name, case.is_lib)
            }
        },
    )
}

fn translate_one_shared(
    paths: &Paths,
    output_dir: &Path,
    battery_name: &str,
    group: &battery::SharedSourceGroup,
) -> CaseResult {
    let real_dir = output_dir.join(&group.real_case);
    if crate::battery::has_crate(&crate::battery::phase_dir(&real_dir, crate::battery::TRANSLATED)) {
        return CaseResult { name: group.real_case.clone(), elapsed_secs: 0, success: true, error: None, skipped: true };
    }

    println!("Translating: {} (shared-source, {} configs)", group.real_case, group.configs.len());
    run_and_record(&group.real_case, &real_dir, &paths.agent_key,
        || dispatch_translate_shared(paths, battery_name, &group.real_case),
        || {
            if let Ok(mut cargo) = CargoToml::open(&crate::battery::phase_dir(&real_dir, crate::battery::TRANSLATED).join("Cargo.toml")) {
                cargo.add_workspace();
                let features = battery::extract_features_from_path(
                    &paths.input_dir(battery_name).join(&group.real_case).join("CMakePresets.json"),
                ).unwrap_or_default();
                let resolved = battery::resolve_features(
                    &crate::battery::phase_dir(&real_dir, crate::battery::TRANSLATED).join("Cargo.toml"), &features,
                ).unwrap_or_default();
                if !resolved.is_empty() {
                    cargo.set_default_features(&resolved);
                }
                let _ = cargo.save();
            }
            Ok(())
        },
    )
}

// ── DRY dispatch helpers ───────────────────────────────────────────────

fn run_and_record(
    name: &str,
    case_dir: &Path,
    agent: &crate::cache::AgentKey,
    translate_fn: impl FnOnce() -> Result<()>,
    post_process_fn: impl FnOnce() -> Result<()>,
) -> CaseResult {
    // A thread may be re-used across cases; without this, a non-CLI agent would
    // inherit the previous case's exit code.
    clear_agent_exit();
    let start = Instant::now();
    match translate_fn() {
        Ok(()) => {
            let elapsed = start.elapsed().as_secs();
            write_translation_metrics(case_dir, agent, elapsed, true);
            let _ = post_process_fn();
            CaseResult { name: name.to_owned(), elapsed_secs: elapsed, success: true, error: None, skipped: false }
        }
        Err(e) => {
            let elapsed = start.elapsed().as_secs();
            write_translation_metrics(case_dir, agent, elapsed, false);
            CaseResult { name: name.to_owned(), elapsed_secs: elapsed, success: false, error: Some(e.to_string()), skipped: false }
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
        if is_lib { PromptKind::Library } else { PromptKind::Executable }
    }

    #[cfg(test)]
    const ALL: &'static [PromptKind] =
        &[PromptKind::Library, PromptKind::Executable, PromptKind::Shared, PromptKind::Verify];
}

/// The one place a prompt file is chosen, relative to [`Paths::prompts_dir`].
///
/// `None` means this agent reads no prompt of that kind at all: c2rust and the
/// docker-driven translators are given none, and the ablations plus the one-shot LLM
/// arms have no verify phase (`verify::has_verify_phase` must agree — a test asserts
/// it). Returning the *name* rather than the text is what lets a test check the choice
/// against the files on disk, so a renamed prompt fails in CI instead of reaching a
/// paid agent run as an empty one.
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
    let Some(file) = prompt_file_for(paths.agent, kind) else { return Ok(None) };
    let path = paths.prompts_dir.join(file);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading the {kind:?} prompt {}", path.display()))?;
    anyhow::ensure!(!text.trim().is_empty(), "the prompt {} is empty", path.display());
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

fn dispatch_translate(paths: &Paths, battery: &str, name: &str, is_lib: bool) -> Result<()> {
    match paths.agent {
        Agent::Laertes => laertes_translate_case(paths, battery, name),
        Agent::C2SaferRust => c2saferrust_translate_case(paths, battery, name, is_lib),
        Agent::SmartC2Rust => anyhow::bail!("smartc2rust is translated via the external fixture pipeline (docs), not in-tool; harvest-tools only scores its results"),
        Agent::Kimi => kimi_translate_case(paths, battery, name, is_lib),
        Agent::Oneshot => oneshot_translate_case(paths, battery, name, is_lib),
        Agent::C2rust => translate_case(paths, battery, name, ""),
        // Every remaining agent differs only in which prompt file it reads.
        Agent::Kiro | Agent::Claude | Agent::OpenCode | Agent::ClaudeCombined
        | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures
        | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt
        | Agent::CodexGpt55 | Agent::CodexGpt54 => {
            let prompt = require_prompt(paths, PromptKind::independent(is_lib))?;
            translate_case(paths, battery, name, &prompt)
        }
    }
}

fn dispatch_translate_shared(paths: &Paths, battery: &str, name: &str) -> Result<()> {
    match paths.agent {
        Agent::Laertes => laertes_translate_case(paths, battery, name),
        Agent::C2SaferRust => c2saferrust_translate_case(paths, battery, name, true),
        Agent::SmartC2Rust => anyhow::bail!("smartc2rust is translated via the external fixture pipeline (docs), not in-tool; harvest-tools only scores its results"),
        Agent::Kimi => kimi_translate_case(paths, battery, name, true),
        Agent::Oneshot => oneshot_translate_case(paths, battery, name, true),
        Agent::C2rust => translate_case(paths, battery, name, ""),
        Agent::Kiro | Agent::Claude | Agent::OpenCode | Agent::ClaudeCombined
        | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures
        | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt
        | Agent::CodexGpt55 | Agent::CodexGpt54 => {
            let prompt = require_prompt(paths, PromptKind::Shared)?;
            translate_case(paths, battery, name, &prompt)
        }
    }
}

// ── harvest-bench translation ──────────────────────────────────────────

/// Translate every harvest-bench project's `test_case/` into a Rust crate that
/// builds a cdylib with the same C ABI: the test phase builds it into a `.so` and
/// runs the upstream gtest suite against it.
pub fn run_harvest_bench(paths: &Paths, projects: &[battery::HarvestBenchProject], parallel: usize) -> Result<()> {
    preflight_check(paths.agent)?;

    // A harvest-bench test_case/ is always a C library the suite links by ABI, so the
    // library prompt applies to every project — no project-type dispatch. Empty only
    // for the agents that are handed no prompt at all (see `prompt_file_for`).
    let prompt = read_prompt(paths, PromptKind::Library)?.unwrap_or_default();

    anyhow::ensure!(!projects.is_empty(),
        "No harvest-bench projects to translate. Targets are `HB` (all) or \
         `HB/<project>`; each project is a dir under harvest-bench/tests/ with \
         both a `test_case/` and a `gtest_suite/`. Did you `git submodule update --init`?");
    let total = projects.len();
    let sem = Semaphore::new(parallel);

    let results: Vec<CaseResult> = std::thread::scope(|s| {
        let handles: Vec<(String, _)> = projects.iter().map(|p| {
            let prompt = &prompt;
            let sem = &sem;
            let name = p.name().to_owned(); // see run_test_corpus
            (name, s.spawn(move || {
                let _permit = sem.acquire();
                translate_one_harvest_bench(paths, p, prompt)
            }))
        }).collect();
        handles.into_iter()
            .map(|(name, h)| {
                let case_dir = paths.output_dir(&name);
                h.join().unwrap_or_else(|e| CaseResult::panicked(name, &case_dir, &paths.agent_key, e))
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
    Ok(())
}

fn translate_one_harvest_bench(paths: &Paths, project: &battery::HarvestBenchProject, prompt: &str) -> CaseResult {
    let name = project.name();
    let case_dir = paths.output_dir(name);

    if crate::battery::has_crate(&crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED)) {
        return CaseResult { name: name.into(), elapsed_secs: 0, success: true, error: None, skipped: true };
    }

    run_and_record(name, &case_dir, &paths.agent_key,
        || translate_case_at(paths, project.test_case(), &case_dir, prompt),
        || {
            // The lib name must be the project name: the suite links `lib<name>.so`
            // by ABI, not by crate name.
            let cargo_path = crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED).join("Cargo.toml");
            if cargo_path.exists() {
                let mut cargo = CargoToml::open(&cargo_path)?;
                cargo.add_workspace();
                cargo.remove_bin();
                cargo.set_lib(name);
                cargo.save()?;
                cargo_toml::strip_for_lib(&crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED))?;
            }
            Ok(())
        },
    )
}

// ── Preflight ──────────────────────────────────────────────────────────

fn preflight_check(agent: Agent) -> Result<()> {
    let (cmd, version_args): (&str, &[&str]) = match agent {
        Agent::Kiro => ("kiro-cli", &["--version"]),
        Agent::Claude | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt => ("claude", &["--version"]),
        Agent::CodexGpt55 | Agent::CodexGpt54 => ("codex", &["--version"]),
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

    if matches!(agent, Agent::Claude | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // `claude --version` prints either "2.1.150.280 ..." or "claude 2.1.158.312 ...",
        // depending on version, so scan for the first line carrying digits.
        let version_str = stdout.lines()
            .find(|l| l.chars().any(|c| c.is_ascii_digit()))
            .unwrap_or("");
        let parts: Vec<u32> = version_str
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        let (major, minor) = (parts.first().copied().unwrap_or(0), parts.get(1).copied().unwrap_or(0));
        if major < 2 || (major == 2 && minor < 1) {
            anyhow::bail!(
                "Claude Code version {version_str} is too old (need >= 2.1).\n\
                 Subprocess resolved: {}",
                stdout.lines().next().unwrap_or("unknown"),
            );
        }
    }

    Ok(())
}

// ── Core translation ───────────────────────────────────────────────────

fn translate_case(paths: &Paths, battery: &str, name: &str, prompt: &str) -> Result<()> {
    let input_test_case = paths.input_dir(battery).join(name).join("test_case");
    let case_dir = paths.case_dir(battery, name);
    translate_case_at(paths, &input_test_case, &case_dir, prompt)
}

/// The agentic-translation core: copies the C source into an isolated temp
/// workspace, invokes the agent there, and on success copies the produced crate to
/// `<out_case_dir>/translated_rust`. Paths are explicit so it serves any dataset
/// layout.
pub fn translate_case_at(paths: &Paths, input_test_case: &Path, out_case_dir: &Path, prompt: &str) -> Result<()> {
    let case_dir = out_case_dir;

    if case_dir.exists() {
        std::fs::remove_dir_all(case_dir)?;
    }

    // Created before the agent runs so `tee` can write its log there live; the
    // crate copy-back below merges into this dir rather than replacing it.
    let translated_dir = crate::battery::phase_dir(case_dir, crate::battery::TRANSLATED);
    let logs_dir = translated_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;

    let log_path = logs_dir.join("translation.log");
    let openssl_dir = std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into());

    // The sandbox, settings path and OpenCode contract are all rooted at the temp dir, so
    // it leaves the match directly: recovering it as `work_dir.parent()` turned a known
    // root into an `Option` that three call sites below then had to guess at.
    let (work_root, work_dir, _tmp_guard) = match paths.agent {
        Agent::Kiro | Agent::Claude | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt | Agent::CodexGpt55 | Agent::CodexGpt54 | Agent::OpenCode | Agent::C2rust | Agent::Laertes | Agent::C2SaferRust | Agent::SmartC2Rust | Agent::Kimi | Agent::Oneshot => {
            let tmp = crate::workdir::tempdir("harvest-translate-")
                .context("creating isolated temp dir")?;
            let work = tmp.path().join(crate::battery::TRANSLATED_RUST);
            let c_src = work.join("c_src");
            std::fs::create_dir_all(&c_src)?;
            copy_dir_all(input_test_case, &c_src)?;

            if matches!(paths.agent, Agent::Claude | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt) {
                crate::sandbox::write_settings(&paths.repo_root, tmp.path(), tmp.path(), paths.allow_unsandboxed)?;
            }

            if matches!(paths.agent, Agent::OpenCode) {
                // OpenCode's equivalent of the .claude/settings.json sandbox above.
                crate::opencode::materialize_config(
                    tmp.path(),
                    crate::opencode::Phase::Translate,
                    &opencode_model(paths)?,
                )?;
            }

            (tmp.path().to_path_buf(), work, Some(tmp))
        }
    };

    // OpenCode's appended filesystem-boundary contract names the temp dir, so the
    // final prompt is only known once the workspace exists.
    let prompt: &str = &match paths.agent {
        Agent::OpenCode => format!("{prompt}{}", crate::opencode::prompt_suffix(&work_root)),
        _ => prompt.to_string(),
    };

    // Prompt files evolve, so the filename alone does not say what a past run saw:
    // store the rendered text, after any per-agent suffix. Empty for non-prompt
    // agents such as c2rust.
    if !prompt.is_empty() {
        let _ = std::fs::write(logs_dir.join("prompt.md"), prompt);
    }

    match paths.agent {
        Agent::Kiro => {
            let status = crate::session::Session::kiro(KIRO_TRANSLATE_TIMEOUT_SECS)
                .kiro_command(&work_dir, prompt, &log_path)
                .status()
                .context("invoking kiro-cli")?;
            record_agent_exit(status);
        }
        Agent::Claude | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt => {
            // The same builder verify uses, so the two phases cannot drift into
            // different flags for the same CLI.
            let model = claude_model()?;
            let cli = crate::cache::CliVersion::probe("claude")?;
            let status = crate::session::Session::claude(TRANSLATE_TIMEOUT_SECS)
                .claude_command(&crate::session::ClaudeRun {
                    cwd: &work_dir,
                    prompt,
                    log: &log_path,
                    settings: &work_root.join(".claude/settings.json"),
                    agent_tmp: &crate::workdir::agent_tmp(&work_root)?,
                    model: &model,
                })
                .status()
                .context("invoking claude")?;
            record_agent_exit(status);
            // Translate is not cached, but an unhonoured pin still misattributes the
            // artifact — and `verified/` is built on top of it.
            assert_pins_honoured(&log_path, &model, &cli)?;
        }
        Agent::CodexGpt55 | Agent::CodexGpt54 => {
            // Model and region are passed as `-c` overrides so the run does not
            // depend on a global ~/.codex/config.toml.
            let (model, region) = match paths.agent {
                Agent::CodexGpt55 => ("openai.gpt-5.5", "us-east-2"),
                Agent::CodexGpt54 => ("openai.gpt-5.4", "us-west-2"),
                _ => unreachable!(),
            };
            invoke_codex_with_retry(
                RetrySession {
                    prompt,
                    log_path: &log_path,
                    work_dir: &work_dir,
                    context_label: "translate",
                },
                model,
                region,
                &openssl_dir,
            )?;
        }
        Agent::OpenCode => {
            invoke_opencode_with_retry(
                RetrySession {
                    prompt,
                    log_path: &log_path,
                    work_dir: &work_dir,
                    context_label: "translate",
                },
                &crate::session::Session::opencode(
                    crate::opencode::Phase::Translate, TRANSLATE_TIMEOUT_SECS,
                ),
                &work_root,
                &opencode_model(paths)?,
            )?;
        }
        Agent::C2rust => {
            c2rust_translate(&work_dir, &log_path)?;
        }
        Agent::Laertes => unreachable!("laertes uses laertes_translate_case"),
        Agent::C2SaferRust => unreachable!("c2saferrust uses c2saferrust_translate_case"),
        Agent::SmartC2Rust => unreachable!("smartc2rust is not translated in-tool"),
        Agent::Kimi => unreachable!("kimi uses kimi_translate_case"),
        Agent::Oneshot => unreachable!("oneshot uses oneshot_translate_case"),
    };

    if !crate::battery::has_crate(&work_dir) {
        anyhow::bail!("no Cargo.toml produced");
    }

    // copy_dir_all merges, so the logs/ already written into `translated/` survive.
    copy_dir_all(&work_dir, &translated_dir)?;

    Ok(())
}

// ── Post-processing ────────────────────────────────────────────────────

fn post_process_independent(paths: &Paths, battery: &str, name: &str, is_lib: bool) -> Result<()> {
    let cargo_path = crate::battery::phase_dir(&paths.case_dir(battery, name), crate::battery::TRANSLATED).join("Cargo.toml");
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
        cargo_toml::strip_for_lib(&crate::battery::phase_dir(&paths.case_dir(battery, name), crate::battery::TRANSLATED))?;
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
pub fn propagate_config_phase(
    paths: &Paths,
    battery: &str,
    real_case: &str,
    cfg: &battery::Config,
    phase: &str,
) -> Result<()> {
    let real_dir = crate::battery::phase_dir(&paths.case_dir(battery, real_case), phase);
    // An agent with no verify phase produces no verified/ to copy.
    if !real_dir.is_dir() { return Ok(()); }
    let cfg_dir = paths.case_dir(battery, &cfg.name);
    let translated = crate::battery::phase_dir(&cfg_dir, phase);

    std::fs::create_dir_all(&translated)?;
    std::fs::create_dir_all(translated.join("logs"))?;

    let src_dst = translated.join("src");
    if src_dst.exists() {
        std::fs::remove_dir_all(&src_dst)?;
    }
    copy_dir_all(&real_dir.join("src"), &src_dst)?;

    std::fs::copy(real_dir.join("Cargo.toml"), translated.join("Cargo.toml"))?;

    for entry in std::fs::read_dir(&real_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name() != "Cargo.toml" {
            std::fs::copy(entry.path(), translated.join(entry.file_name()))?;
        }
    }

    let c_src_src = real_dir.join("c_src");
    if c_src_src.is_dir() {
        let c_src_dst = translated.join("c_src");
        if c_src_dst.exists() {
            std::fs::remove_dir_all(&c_src_dst)?;
        }
        copy_dir_all(&c_src_src, &c_src_dst)?;
    }

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

    Ok(())
}

pub fn propagate_config(
    paths: &Paths,
    battery: &str,
    real_case: &str,
    cfg: &battery::Config,
) -> Result<()> {
    propagate_config_phase(paths, battery, real_case, cfg, crate::battery::TRANSLATED)
}

// ── Metrics ────────────────────────────────────────────────────────────

// ── Agent process exit capture ─────────────────────────────────────────
//
// The exit status is captured deep inside translate_case_at but written out by
// run_and_record, so it is stashed in a thread-local instead of being threaded back
// through dispatch_translate's ~12 match arms. Sound only because each case runs on
// its own thread: "last agent exit on this thread" is unambiguously this case's.
//
// `exit_code` is the shell pipeline's status (`set -o pipefail` makes it the agent's
// own code). Absent for agents with no single agent process (kimi/oneshot, c2rust).
#[derive(Clone, Copy, Default)]
pub struct AgentExit {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub recorded: bool,
}

thread_local! {
    static LAST_AGENT_EXIT: std::cell::Cell<AgentExit> =
        const { std::cell::Cell::new(AgentExit { exit_code: None, timed_out: false, recorded: false }) };
}

pub fn record_agent_exit(status: std::process::ExitStatus) {
    let code = status.code();
    LAST_AGENT_EXIT.with(|c| c.set(AgentExit {
        exit_code: code,
        timed_out: code == Some(124), // `timeout` exits 124 when it kills the child
        recorded: true,
    }));
}

/// Call at the start of a case: a non-CLI agent on a re-used thread must not inherit
/// the previous case's exit code.
pub fn clear_agent_exit() {
    LAST_AGENT_EXIT.with(|c| c.set(AgentExit::default()));
}

fn take_agent_exit() -> AgentExit {
    LAST_AGENT_EXIT.with(|c| c.replace(AgentExit::default()))
}

/// Shared by translate and verify so both report agent process health identically.
fn merge_agent_exit(metrics: &mut serde_json::Value) {
    let e = take_agent_exit();
    if e.recorded {
        metrics["exit_code"] = serde_json::json!(e.exit_code);
        metrics["timed_out"] = serde_json::json!(e.timed_out);
    }
}

/// Inside the `translated/` PHASE dir, beside the log it describes. The case ROOT is
/// where this used to land, while `agent_health::collect` and `battery.rs` both read the
/// phase dir — so `exit_code`/`timed_out` were written and then read by nothing, and
/// that is the 124-on-timeout signal for a project killed at the wall clock.
fn translation_metrics_path(case_dir: &Path) -> PathBuf {
    crate::battery::phase_dir(case_dir, crate::battery::TRANSLATED).join("translation.json")
}

fn write_translation_metrics(
    case_dir: &Path,
    agent: &crate::cache::AgentKey,
    duration_secs: u64,
    success: bool,
) {
    let mut metrics = serde_json::json!({
        "agent": agent.as_str(),
        "duration_secs": duration_secs,
        "success": success,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    merge_agent_exit(&mut metrics);
    let path = translation_metrics_path(case_dir);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        path,
        serde_json::to_string_pretty(&metrics).unwrap_or_default() + "\n",
    );
}

/// What one agent invocation cost and how it exited.
///
/// Must be built exactly once per invocation: `merge_agent_exit` consumes the
/// recorded exit. The one value then feeds both `verification.json` and the cache
/// entry, so the two cannot disagree about the same run.
pub fn agent_provenance(agent: &crate::cache::AgentKey, duration_secs: u64) -> serde_json::Value {
    let mut p = serde_json::json!({
        "agent": agent.as_str(),
        "duration_secs": duration_secs,
        "success": true,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        // Which harness code produced this; results are otherwise unattributable
        // after the fact.
        "harness": crate::provenance::harness_id(),
        "sandboxed": crate::sandbox::is_enforceable(),
    });
    merge_agent_exit(&mut p);
    p
}

/// Verify-side sibling of [`write_translation_metrics`].
///
/// `provenance` describes the invocation that produced the artifact, which on a
/// replay is the ORIGINAL one — so `replayed` and `cache_key` are what stop its cost
/// and timestamp being read as this run's spend.
pub fn write_verification_metrics(
    case_dir: &Path,
    provenance: &serde_json::Value,
    replayed: bool,
    cache_key: Option<&str>,
) {
    let mut metrics = provenance.clone();
    metrics["replayed"] = serde_json::json!(replayed);
    if let Some(k) = cache_key {
        metrics["cache_key"] = serde_json::json!(k);
    }
    let _ = std::fs::create_dir_all(case_dir);
    let _ = std::fs::write(
        case_dir.join("verification.json"),
        serde_json::to_string_pretty(&metrics).unwrap_or_default() + "\n",
    );
}

fn count_cases(battery: &battery::Battery) -> usize {
    battery.cases.iter().map(|c| match c {
        Case::Independent(_) => 1,
        Case::SharedSource(g) => 1 + g.configs.len(),
    }).sum()
}

// ── Utilities ──────────────────────────────────────────────────────────

pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("reading dir {}", src.display()))?
    {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::metadata(entry.path());
            match target {
                Ok(m) if m.is_dir() => copy_dir_all(&entry.path(), &dst_path)?,
                Ok(m) if m.is_file() => { std::fs::copy(entry.path(), &dst_path)?; }
                Ok(_) => continue, // non-regular target (pipe, socket, etc.)
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
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("reading dir {}", src.display()))?
    {
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
                Ok(m) if m.is_file() => { std::fs::copy(entry.path(), &dst_path)?; }
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

/// The verify phase's working copy, expressed in terms of [`crate::artifact`].
///
/// Materialises `translated/` into disk-backed scratch, hands the agent a
/// [`crate::artifact::WorkTree`] (the only artifact type that yields a path, hence
/// the only one anything can execute in), and on `finish` requires proof that the
/// agent completed before the result may reach `verified/`.
pub struct IsolatedWorkDir {
    work: crate::artifact::WorkTree<crate::artifact::Verify>,
    /// Digest of the `translated/` artifact this was materialised from.
    input: crate::artifact::TreeDigest,
    /// Digest of the C oracle as handed to the agent, compared on `finish`: the C
    /// side is the reference being graded against, so a run that modified it has not
    /// been verified against the original program. `verify.md` contains no rule
    /// forbidding that, so this check is the only thing catching it.
    c_before: crate::artifact::TreeDigest,
}

impl IsolatedWorkDir {
    pub fn new(case_dir: &Path) -> Result<Self> {
        let translated = crate::artifact::Sealed::<crate::artifact::Translate>::adopt(case_dir)
            .context("adopting translated/ as a sealed artifact")?;
        let scratch = crate::artifact::Scratch::new("harvest-work-")?;
        let input = translated.digest().clone();
        let work = translated.materialise_into::<crate::artifact::Verify>(scratch)?;
        let c_before = work.c().digest()?;
        Ok(Self { work, input, c_before })
    }

    pub fn translated_rust(&self) -> PathBuf {
        self.work.crate_dir()
    }

    /// Scratch root: current_dir, the sandbox policy and the agent's TMPDIR.
    pub fn root(&self) -> &Path {
        self.work.path()
    }

    /// The cache key's input component. Taken here rather than recomputed later
    /// because it must describe what the agent was actually given.
    pub fn input_digest(&self) -> &crate::artifact::TreeDigest {
        &self.input
    }

    /// Seal the agent's output.
    ///
    /// Requires `&Completed` so an infra-failed run cannot be sealed. Returns the
    /// artifact rather than publishing it: publication happens once on the far side
    /// of the cache, so replayed and fresh results travel the same path.
    pub fn finish(
        self,
        proof: &crate::agent_health::Completed,
    ) -> Result<crate::artifact::Sealed<crate::artifact::Verify>> {
        let scrubbed = self.work.scrub()?;
        // Reported rather than silent: a file that embedded the scratch path is a
        // file whose content varied per run (3 files across 345 cases).
        for rel in scrubbed.rewritten() {
            eprintln!("  scrubbed per-run path from {}", rel.as_path().display());
        }
        scrubbed.seal(proof, &self.c_before)
    }
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
        anyhow::bail!("cmake failed: {}", String::from_utf8_lossy(&cmake_out.stderr));
    }

    let cc_json = build_dir.join("compile_commands.json");
    if !cc_json.exists() {
        anyhow::bail!("cmake did not produce compile_commands.json");
    }

    let c2r_out = Command::new("c2rust")
        .args([
            "transpile", "--emit-build-files", "--binary", "main",
            &cc_json.to_string_lossy(), "--output-dir", &work_dir.to_string_lossy(),
        ])
        .output()
        .context("running c2rust transpile")?;
    log.write_all(&c2r_out.stdout)?;
    log.write_all(&c2r_out.stderr)?;
    if !c2r_out.status.success() {
        anyhow::bail!("c2rust transpile failed: {}", String::from_utf8_lossy(&c2r_out.stderr));
    }

    let cargo_path = work_dir.join("Cargo.toml");
    if cargo_path.exists() {
        let mut cargo = std::fs::read_to_string(&cargo_path)?;
        cargo = cargo.replace("name = \"main\"", "name = \"driver\"");
        cargo = cargo.replace("name = \"rust_out\"", "name = \"driver\"");
        cargo = C2RUST_CRATE_NAME_RE.replace_all(&cargo, r#"name = "driver""#).into_owned();
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

    std::fs::write(work_dir.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"nightly\"\n")?;

    let build_out = Command::new("cargo")
        .args(["+nightly", "build", "--release"])
        .current_dir(work_dir)
        .output()
        .context("cargo build")?;
    log.write_all(&build_out.stdout)?;
    log.write_all(&build_out.stderr)?;
    writeln!(log, "\nc2rust translation {}", if build_out.status.success() { "succeeded" } else { "FAILED to compile" })?;

    Ok(())
}

// ── Kimi one-shot LLM translation (harvest methodology) ───────────────

struct LlmResponse {
    content: String,
    input_tokens: u64,
    output_tokens: u64,
}

const BEDROCK_MODEL_ID: &str = "moonshotai.kimi-k2.5";
const BEDROCK_REGION: &str = "us-east-1";
const BEDROCK_MAX_TOKENS: u32 = 16384;


fn kimi_translate_case(paths: &Paths, battery: &str, name: &str, is_lib_hint: bool) -> Result<()> {
    oneshot_llm_translate(paths, battery, name, is_lib_hint, None, bedrock_converse)
}

fn oneshot_translate_case(paths: &Paths, battery: &str, name: &str, is_lib_hint: bool) -> Result<()> {
    // As in `opencode_model`: main rejects a missing --model, but this runs per case.
    let model = paths.model.as_deref().context(
        "--agent oneshot requires --model <provider>/<model-id> (should have been \
         rejected at startup)",
    )?;
    oneshot_llm_translate(paths, battery, name, is_lib_hint, Some(model), |sys, usr, log| {
        openrouter_converse(model, sys, usr, log)
    })
}

fn oneshot_llm_translate(
    paths: &Paths,
    battery: &str,
    name: &str,
    is_lib_hint: bool,
    model: Option<&str>,
    invoke_llm: impl FnOnce(&str, &str, &Path) -> Result<LlmResponse>,
) -> Result<()> {
    let case_dir = paths.case_dir(battery, name);
    if case_dir.exists() { std::fs::remove_dir_all(&case_dir)?; }

    let logs_dir = case_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = logs_dir.join("translation.log");

    let input_test_case = paths.input_dir(battery).join(name).join("test_case");
    let translated = crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED);
    std::fs::create_dir_all(&translated)?;

    // The test harness expects the C sources beside the crate.
    let c_src = translated.join("c_src");
    std::fs::create_dir_all(&c_src)?;
    copy_dir_all(&input_test_case, &c_src)?;

    let files_json = collect_c_files_json(&input_test_case)?;
    let is_lib = detect_is_library(&input_test_case).unwrap_or(is_lib_hint);
    let system_prompt = require_prompt(paths, PromptKind::independent(is_lib))?;

    // As on the CLI-agent path: prompt files change, so store the text that ran.
    let _ = std::fs::write(logs_dir.join("prompt.md"), &system_prompt);

    let user_msg = format!(
        "Please translate the following C project into a Rust project including Cargo manifest:\n\n{files_json}\n\nreturn as JSON"
    );

    let resp = invoke_llm(&system_prompt, &user_msg, &log_path)?;

    let mut usage = serde_json::json!({
        "input_tokens": resp.input_tokens,
        "output_tokens": resp.output_tokens,
    });
    if let Some(m) = model { usage["model"] = serde_json::json!(m); }
    let _ = std::fs::write(logs_dir.join("usage.json"),
        serde_json::to_string_pretty(&usage).unwrap_or_default() + "\n");

    write_llm_files(&resp.content, &translated)?;

    if !crate::battery::has_crate(&translated) {
        anyhow::bail!("no Cargo.toml in LLM response");
    }
    Ok(())
}

/// Collect all files under `dir` as a JSON object: `{"files": [{"path": "...", "contents": "..."}]}`.
fn collect_c_files_json(dir: &Path) -> Result<String> {
    #[derive(serde::Serialize)]
    struct FileEntry { path: String, contents: String }
    #[derive(serde::Serialize)]
    struct FilesPayload { files: Vec<FileEntry> }

    let mut files = Vec::new();
    for path in walkdir(dir)? {
        let rel = path.strip_prefix(dir)?.to_string_lossy().to_string();
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| String::from("<binary file>"));
        files.push(FileEntry { path: rel, contents });
    }
    Ok(serde_json::to_string(&FilesPayload { files })?)
}

fn detect_is_library(dir: &Path) -> Option<bool> {
    let cmake = std::fs::read_to_string(dir.join("CMakeLists.txt")).ok()?;
    if cmake.lines().any(|l| l.trim_start().starts_with("add_library(")) {
        Some(true)
    } else if cmake.lines().any(|l| l.trim_start().starts_with("add_executable(")) {
        Some(false)
    } else {
        None
    }
}

fn bedrock_converse(system_prompt: &str, user_message: &str, log_path: &Path) -> Result<LlmResponse> {
    let request = serde_json::json!({
        "modelId": BEDROCK_MODEL_ID,
        "system": [{"text": system_prompt}],
        "messages": [{"role": "user", "content": [{"text": user_message}]}],
        "inferenceConfig": {"maxTokens": BEDROCK_MAX_TOKENS, "temperature": 0.0}
    });

    let request_file = log_path.with_extension("request.json");
    std::fs::write(&request_file, serde_json::to_string_pretty(&request)?)?;

    let output = Command::new("aws")
        .args(["bedrock-runtime", "converse",
            "--region", BEDROCK_REGION,
            "--cli-read-timeout", "300",
            "--cli-input-json", &format!("file://{}", request_file.display()),
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
         === SYSTEM PROMPT ===\n{system_prompt}\n\n\
         === USER MESSAGE (first 2000 chars) ===\n{}\n\n\
         === STDERR ===\n{stderr}",
        truncated(user_message, 2000)
    );
    let _ = std::fs::write(log_path, &log_content);

    if !output.status.success() {
        anyhow::bail!("bedrock converse failed: {stderr}");
    }

    let resp: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("failed to parse Bedrock response: {}", truncated(&stdout, 500)))?;

    let content = resp["output"]["message"]["content"][0]["text"]
        .as_str()
        .context("no text in Bedrock response")?
        .trim()
        .to_string();

    let input_tokens = resp["usage"]["inputTokens"].as_u64().unwrap_or(0);
    let output_tokens = resp["usage"]["outputTokens"].as_u64().unwrap_or(0);

    Ok(LlmResponse { content, input_tokens, output_tokens })
}

fn openrouter_converse(model: &str, system_prompt: &str, user_message: &str, log_path: &Path) -> Result<LlmResponse> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .context("OPENROUTER_API_KEY env var not set")?;

    let request = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_message}
        ],
        "temperature": 0,
        "response_format": {"type": "json_object"}
    });

    let request_file = log_path.with_extension("request.json");
    std::fs::write(&request_file, serde_json::to_string_pretty(&request)?)?;

    let output = Command::new("curl")
        .args(["-s", "--max-time", "600",
            "-X", "POST", "https://openrouter.ai/api/v1/chat/completions",
            "-H", &format!("Authorization: Bearer {api_key}"),
            "-H", "Content-Type: application/json",
            "-d", &format!("@{}", request_file.display()),
        ])
        .output()
        .context("failed to invoke curl for OpenRouter")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    let response_file = log_path.with_file_name("translation.response.json"); // see bedrock_converse
    let _ = std::fs::write(&response_file, &stdout);

    let log_content = format!(
        "=== OPENROUTER REQUEST ===\nModel: {model}\n\n\
         === SYSTEM PROMPT ===\n{system_prompt}\n\n\
         === USER MESSAGE (first 2000 chars) ===\n{}\n\n\
         === RAW RESPONSE (first 2000 chars) ===\n{}",
        truncated(user_message, 2000),
        truncated(&stdout, 2000)
    );
    let _ = std::fs::write(log_path, &log_content);

    if !output.status.success() {
        anyhow::bail!("curl failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let resp: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("failed to parse OpenRouter response: {}", truncated(&stdout, 500)))?;

    if let Some(err) = resp.get("error") {
        anyhow::bail!("OpenRouter error: {err}");
    }

    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_string())
        .context("no content in OpenRouter response")?;

    let input_tokens = resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
    let output_tokens = resp["usage"]["completion_tokens"].as_u64().unwrap_or(0);

    Ok(LlmResponse { content, input_tokens, output_tokens })
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
    struct FileEntry { path: String, contents: String }
    #[derive(serde::Deserialize)]
    struct FilesPayload { files: Vec<FileEntry> }

    // Models wrap the object in prose or a markdown fence, so cut to the outermost
    // brace pair rather than parsing the response as-is.
    let json_str = if let Some(start) = json_response.find('{') {
        let from_brace = &json_response[start..];
        let mut depth = 0;
        let mut end = from_brace.len();
        for (i, c) in from_brace.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => { depth -= 1; if depth == 0 { end = i + 1; break; } }
                _ => {}
            }
        }
        &from_brace[..end]
    } else {
        json_response
    };

    let payload: FilesPayload = serde_json::from_str(json_str)
        .with_context(|| format!("failed to parse LLM JSON response: {}", truncated(json_str, 500)))?;

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

fn laertes_translate_case(paths: &Paths, battery: &str, name: &str) -> Result<()> {
    use std::io::Write;

    // c2rust never runs a verify phase, so its translated/ IS the crate to consume.
    let c2rust_case = paths.results_dir
        .parent().context("no parent for results_dir")?
        .join("c2rust").join(battery).join(name);
    let c2rust_original = crate::battery::phase_dir(&c2rust_case, crate::battery::TRANSLATED);
    anyhow::ensure!(c2rust_original.is_dir(),
        "c2rust translated/ crate not found: {}", c2rust_original.display());

    let case_dir = paths.case_dir(battery, name);
    if case_dir.exists() { std::fs::remove_dir_all(&case_dir)?; }
    let logs_dir = case_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = logs_dir.join("translation.log");
    let translated = crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED);

    // Staged in scratch like its c2saferrust neighbour: the container gets this mounted
    // read-write, and the compile check writes `target/` plus a `Cargo.lock` that is part
    // of the hashed artifact — so in `translated/` the arm mutated its own result.
    let tmp = crate::workdir::tempdir("harvest-laertes-")
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
        .args(["run", "--rm", "-v", &mount, LAERTES_DOCKER_IMAGE, "bash", "-c", LAERTES_DOCKER_SCRIPT])
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
    writeln!(log, "\nlaertes translation {}", if ok { "succeeded" } else { "FAILED to compile (non-fatal)" })?;

    copy_dir_filtered(&work, &translated, &["target"])?;
    Ok(())
}

// ── C2SaferRust (c2rust output -> LLM unsafe-reduction via Bedrock) ────────

const C2SAFERRUST_DOCKER_IMAGE: &str = "c2saferrust:latest";
const C2SAFERRUST_MODEL: &str = "bedrock-gpt54";
const C2SAFERRUST_DEFAULT_BASE_URL: &str =
    "https://bedrock-mantle.us-west-2.api.aws/openai/v1";

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
        if !t.trim().is_empty() { return Ok(t); }
    }
    if let Ok(t) = std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
        if !t.trim().is_empty() { return Ok(t); }
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
        .context("minting Bedrock token (is aws_bedrock_token_generator installed \
                  and are `default`-profile creds valid? run `aws-creds <account>`)")?;
    anyhow::ensure!(
        out.status.success(),
        "Bedrock token mint failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tok = String::from_utf8(out.stdout)?.trim().to_string();
    anyhow::ensure!(
        tok.starts_with("bedrock-api-key-"),
        "unexpected token format from provide_token (got {} chars)", tok.len()
    );
    Ok(tok)
}

/// Reduce unsafe code in this repo's c2rust output with the pinned submodule tool in
/// Docker, driven by gpt-5.4 via Bedrock.
///
/// Blind by design (no `--test_dir`): compile-gated only, which is what makes its
/// numbers comparable to ACTOR self-verified. `BEDROCK_BASE_URL` may override the
/// default us-west-2 mantle endpoint.
fn c2saferrust_translate_case(paths: &Paths, battery: &str, name: &str, _is_lib: bool) -> Result<()> {
    use std::io::Write;

    // c2rust never runs a verify phase, so its translated/ IS the crate to consume.
    let c2rust_case = paths.results_dir
        .parent().context("no parent for results_dir")?
        .join("c2rust").join(battery).join(name);
    let c2rust_original = crate::battery::phase_dir(&c2rust_case, crate::battery::TRANSLATED);
    anyhow::ensure!(c2rust_original.is_dir(),
        "c2rust translated/ crate not found (run the c2rust agent first): {}",
        c2rust_original.display());

    let base_url = std::env::var("BEDROCK_BASE_URL")
        .unwrap_or_else(|_| C2SAFERRUST_DEFAULT_BASE_URL.to_string());
    let region = base_url
        .split("bedrock-mantle.").nth(1)
        .and_then(|s| s.split('.').next())
        .unwrap_or("us-west-2")
        .to_string();
    let token = bedrock_token(&region)?;

    let case_dir = paths.case_dir(battery, name);
    if case_dir.exists() { std::fs::remove_dir_all(&case_dir)?; }
    let logs_dir = case_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = logs_dir.join("translation.log");
    let translated = crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED);

    // Bind-mounted into the container: the tool reshapes <work>/rust in place and
    // writes <work>/rust_WIP.
    let tmp = crate::workdir::tempdir("harvest-c2sr-")
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
        .args(["run", "--rm",
               "--user", &format!("{uid}:{gid}"),
               "-e", "C2SR_MODEL",
               "-e", &format!("BEDROCK_API_KEY={token}"),
               "-e", &format!("BEDROCK_BASE_URL={base_url}"),
               "-v", &mount,
               C2SAFERRUST_DOCKER_IMAGE, "bash", "-c", C2SAFERRUST_DOCKER_SCRIPT])
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
        writeln!(log, "\nNo rust_WIP produced (C2Rust input failed to compile under \
                       nightly-2022-08-08, or translation failed). Falling back to the \
                       unmodified C2Rust input so the case is counted as a failure.")?;
        work_rust.clone()
    };
    copy_dir_filtered(&source_dir, &translated, &["target"])?;
    for junk in ["callgraph.dot", "callgraph.pdf", "slices.json", "log.txt", "prompts.txt", "ordering.txt"] {
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

    writeln!(log, "\nc2saferrust translation collected into {}", translated.display())?;
    Ok(())
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
            s = CRATE_TYPE_RE.replace(&s, r#"crate-type = ["staticlib","rlib"]"#).into_owned();
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
unsafe fn libc_getuid() -> u32 { getuid() }
unsafe fn libc_getgid() -> u32 { getgid() }

/// Adapt c2rust output for Laertes' nightly-2020-10-15 toolchain.
fn laertes_preprocess(work_dir: &Path) -> Result<()> {
    for path in walkdir(work_dir)? {
        if path.extension().is_none_or(|e| e != "rs") { continue; }
        let mut src = std::fs::read_to_string(&path)?;
        let changed = src.contains("::core::ffi::") || src.contains("::core::ptr") || src.contains("::core::mem");
        if !changed && !path.ends_with("lib.rs") { continue; }

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
        if path.extension().is_none_or(|e| e != "rs") { continue; }
        let src = std::fs::read_to_string(&path)?;
        let mut out = src.replace("extern crate libc;\n", "");
        out = LIBC_INTERNAL_PATH_RE.replace_all(&out, "libc::$1").into_owned();
        if out != src { std::fs::write(&path, out)?; }
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
struct RetrySession<'a> {
    prompt: &'a str,
    log_path: &'a Path,
    work_dir: &'a Path,
    /// Identifies the case in retry/abort diagnostics.
    context_label: &'a str,
}

/// Retry-aware codex invocation: codex exits 0 after a transient Bedrock error
/// mid-conversation, so without this a Bedrock failure counts as a successful (but
/// empty) translation. Each retry is a fresh invocation, discarding partial state.
fn invoke_codex_with_retry(
    session: RetrySession<'_>,
    model: &str,
    region: &str,
    openssl_dir: &str,
) -> Result<()> {
    let RetrySession { prompt, log_path, work_dir, context_label } = session;
    const MAX_RETRIES: u32 = 3;
    const RETRY_BACKOFF_SECS: u64 = 30;

    for attempt in 1..=MAX_RETRIES {
        let status = Command::new("bash")
            .arg("-lc")
            .arg(r#"set -o pipefail; timeout 10800 codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -C "$3" -c model="$5" -c model_providers.amazon-bedrock.aws.region="$6" --json "$1" < /dev/null 2>&1 | tee "$2""#)
            .arg("--")
            .arg(prompt)
            .arg(log_path)
            .arg(work_dir)
            .arg("__unused__")
            .arg(model)
            .arg(region)
            .env("OPENSSL_DIR", openssl_dir)
            .current_dir(work_dir)
            .status()
            .with_context(|| format!("invoking codex ({context_label})"))?;
        // Overwritten each retry, so the exit that decided the outcome is the one
        // that survives.
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
    session: &crate::session::Session,
    tmp_root: &Path,
    model: &crate::opencode::Model,
) -> Result<()> {
    let RetrySession { prompt, log_path, work_dir, context_label } = retry;
    const MAX_RETRIES: u32 = 3;
    const RETRY_BACKOFF_SECS: u64 = 30;

    for attempt in 1..=MAX_RETRIES {
        crate::opencode::invoke(session, prompt, log_path, work_dir, tmp_root, model)
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
    use super::*;
    use std::process::Command;

    /// ExitStatus cannot be constructed directly, so shell out for a real one.
    /// main's tests predate `AgentKey`; this is the one spelling they share.
    fn test_agent_key() -> crate::cache::AgentKey {
        crate::cache::AgentKey::new(Agent::Claude, None).expect("claude has a fixed name")
    }

    fn exit_status(code: i32) -> std::process::ExitStatus {
        Command::new("sh").arg("-c").arg(format!("exit {code}")).status().unwrap()
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

    #[test]
    fn merge_agent_exit_records_code_when_captured() {
        clear_agent_exit();
        record_agent_exit(exit_status(0));
        let mut m = serde_json::json!({"success": true});
        merge_agent_exit(&mut m);
        assert_eq!(m["exit_code"], serde_json::json!(0));
        assert_eq!(m["timed_out"], serde_json::json!(false));
    }

    #[test]
    fn merge_agent_exit_flags_timeout_124() {
        clear_agent_exit();
        record_agent_exit(exit_status(124)); // `timeout` uses 124
        let mut m = serde_json::json!({});
        merge_agent_exit(&mut m);
        assert_eq!(m["exit_code"], serde_json::json!(124));
        assert_eq!(m["timed_out"], serde_json::json!(true));
    }

    #[test]
    fn merge_agent_exit_absent_for_non_cli_agent() {
        // No record_agent_exit call, as on the kimi/oneshot API path.
        clear_agent_exit();
        let mut m = serde_json::json!({"success": true});
        merge_agent_exit(&mut m);
        assert!(m.get("exit_code").is_none(), "exit_code must be absent when no CLI agent ran");
        assert!(m.get("timed_out").is_none());
    }

    /// The writer must land where `agent_health::collect` and `battery.rs` look, or the
    /// timeout signal (`timeout` exits 124) is written and read by nobody.
    #[test]
    fn translation_metrics_land_where_the_readers_look() {
        let tmp = tempfile::tempdir().unwrap();
        let case = tmp.path().join("mujs");
        clear_agent_exit();
        record_agent_exit(exit_status(124));
        write_translation_metrics(&case, &test_agent_key(), 10_800, false);

        let expected = case.join(crate::battery::TRANSLATED).join("translation.json");
        assert!(expected.is_file(), "the reader path must be the writer path");
        assert!(!case.join("translation.json").exists(), "and nothing may be left at the case root");
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
        let tmp = tempfile::tempdir().unwrap();
        let case = tmp.path().join("libpng");
        clear_agent_exit();
        record_agent_exit(exit_status(124));
        write_translation_metrics(&case, &test_agent_key(), 10_800, false);
        let before = std::fs::read_to_string(translation_metrics_path(&case)).unwrap();

        // The joining thread carries some OTHER case's exit; it must not be borrowed.
        record_agent_exit(exit_status(0));
        let r = CaseResult::panicked("libpng".into(), &case, &test_agent_key(), Box::new("boom"));

        assert!(!r.success);
        assert_eq!(
            std::fs::read_to_string(translation_metrics_path(&case)).unwrap(),
            before,
            "the real 3h/124 record must survive the panic report",
        );
    }

    #[test]
    fn a_panic_with_no_record_yet_writes_one_without_borrowing_an_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let case = tmp.path().join("jansson");
        clear_agent_exit();
        // Belongs to whatever case this thread ran before, NOT to jansson.
        record_agent_exit(exit_status(0));

        CaseResult::panicked("jansson".into(), &case, &test_agent_key(), Box::new("boom"));

        let m: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(translation_metrics_path(&case)).unwrap()).unwrap();
        assert_eq!(m["success"], serde_json::json!(false), "the panic must leave a trace");
        assert!(m.get("exit_code").is_none(), "another case's exit must not be attributed here");
        assert!(m.get("timed_out").is_none());
    }

    #[test]
    fn take_agent_exit_clears_so_next_case_starts_fresh() {
        record_agent_exit(exit_status(1));
        let _ = take_agent_exit();          // consume
        let second = take_agent_exit();     // must be empty now
        assert!(!second.recorded, "exit must not leak into the next case on a reused thread");
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
        let hard = write_log(r#"{"type":"error","error":{"message":"ValidationException: bad request"}}"#);
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
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("tools/ has a parent");
        let model = match agent {
            Agent::Oneshot => Some("openai/gpt-5.4"),
            Agent::OpenCode => Some("amazon-bedrock/us.anthropic.claude-sonnet-5"),
            _ => None,
        };
        Paths::new(root, agent, dataset, model, crate::cache::Mode::Bypass, true).expect("Paths")
    }

    #[test]
    fn every_prompt_the_matrix_names_is_on_disk() {
        // The whole point of naming the file separately from reading it: a rename used
        // to surface as an empty prompt handed to a live agent, mid-sweep.
        use clap::ValueEnum;
        for dataset in [crate::cli::Dataset::TestCorpus, crate::cli::Dataset::HarvestBench] {
            for agent in Agent::value_variants() {
                let paths = paths_for(*agent, dataset);
                for kind in PromptKind::ALL {
                    if prompt_file_for(*agent, *kind).is_none() {
                        continue;
                    }
                    let text = read_prompt(&paths, *kind)
                        .unwrap_or_else(|e| panic!("{agent:?} {kind:?} {dataset:?}: {e:#}"));
                    assert!(text.is_some_and(|t| t.len() > 100), "{agent:?} {kind:?}: too short to be a prompt");
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
                crate::verify::has_verify_phase(*agent),
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
        assert_eq!(prompt_file_for(Agent::ClaudeCrossPrompt, Library), claude(Executable));
        assert_eq!(prompt_file_for(Agent::ClaudeCrossPrompt, Executable), claude(Library));
        assert_eq!(prompt_file_for(Agent::ClaudeCrossPrompt, Shared), claude(Shared));

        // E2 and E6 vary the shared-source prompt only; their independent cases must be
        // byte-identical to claude's or the arm measures more than one change.
        for (agent, shared) in [
            (Agent::ClaudeNoFeatures, "ablations/translate-no-features-shared.md"),
            (Agent::ClaudeNoSubtask, "ablations/translate-no-subtask-shared.md"),
        ] {
            assert_eq!(prompt_file_for(agent, Library), claude(Library), "{agent:?}");
            assert_eq!(prompt_file_for(agent, Executable), claude(Executable), "{agent:?}");
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
        for agent in [Agent::Kiro, Agent::OpenCode, Agent::CodexGpt55, Agent::CodexGpt54] {
            for k in [Library, Executable, Shared] {
                assert_eq!(prompt_file_for(agent, k), claude(k), "{agent:?} {k:?}");
            }
        }
    }
}
