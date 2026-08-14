use crate::battery::{self, Case, Paths};
use crate::cargo_toml::{self, CargoToml};
use crate::cli::Agent;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

// ── claude_plain agent (mirrors kiro_plain) ────────────────────────────
// Used with `--agent claude_plain` to give Claude Code a neutral profile
// matching kiro_plain.json: built-in tools only (Bash/Edit/Read/Write/Task),
// no skills/plugins/MCP, no extra system prompt.
pub const CLAUDE_PLAIN_AGENT_JSON: &str = r#"{"claude_plain":{"description":"Bare-bones agent matching kiro_plain","prompt":"You are a coding assistant. Use the available tools to complete the user's task.","tools":["Bash","Edit","Read","Write","Task"]}}"#;

/// Wall-clock cap on one agentic translate/verify session, matching the
/// `timeout 10800` already used by the claude and codex invocations.
const TRANSLATE_TIMEOUT_SECS: u64 = 10800;

/// CRUST cases are small single-crate projects; the kiro CRUST arm caps them at
/// `timeout 1800`, so a new backend on the same dataset uses the same budget.
const CRUST_TIMEOUT_SECS: u64 = 1800;

/// The `--model` for `--agent opencode`, parsed. `Paths` carries the raw string;
/// `main` has already validated it, so a failure here means an internal bug
/// rather than bad user input.
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
    pub fn acquire(&self) -> SemaphoreGuard<'_> {
        let mut count = self.state.lock().unwrap();
        while *count >= self.max {
            count = self.cvar.wait(count).unwrap();
        }
        *count += 1;
        SemaphoreGuard(self)
    }
}

pub struct SemaphoreGuard<'a>(&'a Semaphore);

impl Drop for SemaphoreGuard<'_> {
    fn drop(&mut self) {
        *self.0.state.lock().unwrap() -= 1;
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

// ── Public entry point ─────────────────────────────────────────────────

/// The CLI value for an agent (e.g. `claude`), for copy-pasteable hints in
/// diagnostics. Uses clap's derived `ValueEnum` mapping so it always matches
/// what `--agent` accepts.
fn agent_cli_name(agent: Agent) -> String {
    use clap::ValueEnum;
    agent.to_possible_value()
        .map(|v| v.get_name().to_string())
        .unwrap_or_else(|| format!("{agent:?}").to_lowercase())
}

/// A translate target names a BATTERY; cases live one level deeper as
/// `<battery>/<case>/` with a `test_case/` (C sources) and a `test_vectors/`
/// dir inside each. Discovering zero cases almost always means the target is a
/// case dir mistaken for a battery (the #1 first-run pitfall). Fail loudly with
/// a fix, rather than the old silent "0/0 translated".
fn ensure_cases_found(count: usize, paths: &Paths, battery_name: &str) -> Result<()> {
    if count > 0 { return Ok(()); }
    let input_dir = paths.input_dir(battery_name);
    let agent = agent_cli_name(paths.agent);
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
        let handles: Vec<_> = independent.iter().map(|c| {
            s.spawn(|| {
                let _permit = sem.acquire();
                translate_one_independent(&paths, &output_dir, battery_name, c)
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
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
        let r = translate_one_shared(&paths, &output_dir, battery_name, group);

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
            if crate::battery::phase_dir(&output_dir.join(&cfg.name), crate::battery::TRANSLATED).join("Cargo.toml").exists() {
                translated += 1;
                println!("[{current}/{total}] ⏭️  {} (already done)", cfg.name);
                continue;
            }
            match propagate_config(&paths, battery_name, &group.real_case, cfg) {
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
    if crate::battery::phase_dir(&output_dir.join(&case.name), crate::battery::TRANSLATED).join("Cargo.toml").exists() {
        return CaseResult { name: case.name.clone(), elapsed_secs: 0, success: true, error: None, skipped: true };
    }

    run_and_record(&case.name, &output_dir.join(&case.name), paths.agent,
        || dispatch_translate(paths, battery_name, &case.name, case.is_lib),
        || {
            if paths.agent == Agent::ClaudeCrossPrompt {
                // E4: SWAP prompts. Don't override the agent's lib-vs-bin choice
                // (that IS the experiment), but DO add `[workspace]` so cargo
                // doesn't try to absorb each case into a parent workspace.
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
    if crate::battery::phase_dir(&real_dir, crate::battery::TRANSLATED).join("Cargo.toml").exists() {
        return CaseResult { name: group.real_case.clone(), elapsed_secs: 0, success: true, error: None, skipped: true };
    }

    println!("Translating: {} (shared-source, {} configs)", group.real_case, group.configs.len());
    run_and_record(&group.real_case, &real_dir, paths.agent,
        || dispatch_translate_shared(paths, battery_name, &group.real_case),
        || {
            if let Ok(mut cargo) = CargoToml::open(&crate::battery::phase_dir(&real_dir, crate::battery::TRANSLATED).join("Cargo.toml")) {
                cargo.add_workspace();
                // Patch default features from CMakePresets.json (same as config copies)
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
    agent: Agent,
    translate_fn: impl FnOnce() -> Result<()>,
    post_process_fn: impl FnOnce() -> Result<()>,
) -> CaseResult {
    // Clear any stale agent-exit from a prior case on this (possibly re-used)
    // thread; CLI agents re-stamp it during translate_fn, non-CLI agents leave
    // it absent so no exit_code is falsely attributed.
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

fn dispatch_translate(paths: &Paths, battery: &str, name: &str, is_lib: bool) -> Result<()> {
    match paths.agent {
        Agent::Laertes => laertes_translate_case(paths, battery, name),
        Agent::C2SaferRust => c2saferrust_translate_case(paths, battery, name, is_lib),
        Agent::SmartC2Rust => anyhow::bail!("smartc2rust is translated via the external fixture pipeline (docs), not in-tool; harvest-tools only scores its results"),
        Agent::Kimi => kimi_translate_case(paths, battery, name, is_lib),
        Agent::Oneshot => oneshot_translate_case(paths, battery, name, is_lib),
        // OpenCode runs the SAME engineered prompts as Claude Code / Kiro — the
        // backend is what varies, not the methodology.
        Agent::Kiro | Agent::Claude | Agent::OpenCode => {
            let f = if is_lib { "translate-library.md" } else { "translate-executable.md" };
            let prompt = std::fs::read_to_string(paths.prompts_dir.join(f)).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeCombined => {
            let f = if is_lib { "ablations/translate-and-verify-library.md" } else { "ablations/translate-and-verify-executable.md" };
            let prompt = std::fs::read_to_string(paths.prompts_dir.join(f)).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeMinimal => {
            // Universal minimal prompt — no project-type dispatch.
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("ablations/translate-minimal.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeNoIter => {
            // Same project-type dispatch as Claude, but without "build → fix → iterate" steps.
            let f = if is_lib { "ablations/translate-no-iter-library.md" } else { "ablations/translate-no-iter-executable.md" };
            let prompt = std::fs::read_to_string(paths.prompts_dir.join(f)).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeNoFeatures => {
            // E2: cmake-features ablation only affects shared-source cases.
            // Independent (executable/library) cases reuse the engineered claude prompts.
            let f = if is_lib { "translate-library.md" } else { "translate-executable.md" };
            let prompt = std::fs::read_to_string(paths.prompts_dir.join(f)).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeNoSubtask => {
            // E6: subtask-decomposition ablation only affects shared-source cases.
            // Independent (executable/library) cases reuse the engineered claude prompts.
            let f = if is_lib { "translate-library.md" } else { "translate-executable.md" };
            let prompt = std::fs::read_to_string(paths.prompts_dir.join(f)).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeCrossPrompt => {
            // E4: SWAP project-type prompts. Libraries get the executable prompt;
            // executables get the library prompt. Directly answers Reviewer 2's
            // question about cross-prompt application.
            let f = if is_lib { "translate-executable.md" } else { "translate-library.md" };
            let prompt = std::fs::read_to_string(paths.prompts_dir.join(f)).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::CodexGpt55 | Agent::CodexGpt54 => {
            // Codex on Bedrock — same prompts as Claude Code, different harness.
            let f = if is_lib { "translate-library.md" } else { "translate-executable.md" };
            let prompt = std::fs::read_to_string(paths.prompts_dir.join(f)).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::C2rust => translate_case(paths, battery, name, ""),
    }
}

fn dispatch_translate_shared(paths: &Paths, battery: &str, name: &str) -> Result<()> {
    match paths.agent {
        Agent::Laertes => laertes_translate_case(paths, battery, name),
        Agent::C2SaferRust => c2saferrust_translate_case(paths, battery, name, true),
        Agent::SmartC2Rust => anyhow::bail!("smartc2rust is translated via the external fixture pipeline (docs), not in-tool; harvest-tools only scores its results"),
        Agent::Kimi => kimi_translate_case(paths, battery, name, true),
        Agent::Oneshot => oneshot_translate_case(paths, battery, name, true),
        Agent::Kiro | Agent::Claude | Agent::OpenCode => {
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("translate-shared.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeCombined => {
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("ablations/translate-and-verify-shared.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeMinimal => {
            // Universal minimal prompt — same as for executables/libraries.
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("ablations/translate-minimal.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeNoIter => {
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("ablations/translate-no-iter-shared.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeNoFeatures => {
            // E2: shared-source prompt without cmake-features → cargo-features guidance.
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("ablations/translate-no-features-shared.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeNoSubtask => {
            // E6: shared-source prompt without subtask-decomposition guidance.
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("ablations/translate-no-subtask-shared.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::ClaudeCrossPrompt => {
            // E4: cross-prompt ablation only affects independent libs/execs.
            // For shared-source cases, use the standard claude shared prompt.
            // (Run scope is typically B01_synthetic which has no shared-source anyway.)
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("translate-shared.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::CodexGpt55 | Agent::CodexGpt54 => {
            // Codex on Bedrock — same shared prompt as Claude Code.
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("translate-shared.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::C2rust => translate_case(paths, battery, name, ""),
    }
}

// ── harvest-bench translation ──────────────────────────────────────────

/// Translate every harvest-bench project's `test_case/` into a Rust crate that
/// builds a cdylib with the same C ABI. Each project is independent, so they
/// run in parallel under a semaphore (like the test-corpus independent cases).
/// The produced crate lands at `results/HarvestBench/<agent>/<name>/translated_rust`;
/// the test phase builds it into a `.so` and runs the upstream gtest suite.
pub fn run_harvest_bench(paths: &Paths, projects: &[battery::HarvestBenchProject], parallel: usize) -> Result<()> {
    preflight_check(paths.agent)?;

    // harvest-bench test_case/ is always a library (a C lib the suite links by
    // ABI). Reuse the same project-type-dispatching library prompt the
    // test-corpus/CRUST paths use — it handles the cdylib / FFI-type case.
    let prompt = std::fs::read_to_string(paths.prompts_dir.join("translate-library.md"))
        .context("reading translate-library.md for harvest-bench")?;

    anyhow::ensure!(!projects.is_empty(),
        "No harvest-bench projects to translate. Targets are `HB` (all) or \
         `HB/<project>`; each project is a dir under harvest-bench/tests/ with \
         both a `test_case/` and a `gtest_suite/`. Did you `git submodule update --init`?");
    let total = projects.len();
    let sem = Semaphore::new(parallel);

    let results: Vec<CaseResult> = std::thread::scope(|s| {
        let handles: Vec<_> = projects.iter().map(|p| {
            let prompt = &prompt;
            let sem = &sem;
            s.spawn(move || {
                let _permit = sem.acquire();
                translate_one_harvest_bench(paths, p, prompt)
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
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

    if crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED).join("Cargo.toml").exists() {
        return CaseResult { name: name.into(), elapsed_secs: 0, success: true, error: None, skipped: true };
    }

    run_and_record(name, &case_dir, paths.agent,
        || translate_case_at(paths, project.test_case(), &case_dir, prompt),
        || {
            // Post-process to a library crate: cdylib + strip bin/tests, same as
            // the test-corpus independent-lib path. The lib name is the project
            // name (the suite links `lib<name>.so` by ABI, not by symbol crate name).
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
        // Claude version output may be "2.1.150.280 ..." (older) or
        // "claude 2.1.158.312 ..." (newer). Match any line containing a digit-dot-digit pattern.
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

/// Test-corpus wrapper: derive the input `test_case/` and output case dir from
/// the `battery`/`name` layout, then run the shared core.
fn translate_case(paths: &Paths, battery: &str, name: &str, prompt: &str) -> Result<()> {
    let input_test_case = paths.input_dir(battery).join(name).join("test_case");
    let case_dir = paths.case_dir(battery, name);
    translate_case_at(paths, &input_test_case, &case_dir, prompt)
}

/// The agentic-translation core, parameterized by explicit input/output paths
/// so it serves any dataset layout (test-corpus's `battery/name/test_case`,
/// harvest-bench's `tests/<name>/test_case`, …). Copies the C source into an
/// isolated temp workspace, invokes the agent there, and on success copies the
/// produced crate to `<out_case_dir>/translated_rust`.
pub fn translate_case_at(paths: &Paths, input_test_case: &Path, out_case_dir: &Path, prompt: &str) -> Result<()> {
    let case_dir = out_case_dir;

    if case_dir.exists() {
        std::fs::remove_dir_all(case_dir)?;
    }

    // Translation output is the immutable `translated/` phase dir; its logs live
    // inside it (translated/logs/translation.log). Created before the agent runs
    // so `tee` can write there live; the crate copy-back below merges into this
    // dir without clobbering logs.
    let translated_dir = crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED);
    let logs_dir = translated_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;

    let log_path = logs_dir.join("translation.log");
    let openssl_dir = std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into());

    let (work_dir, _tmp_guard) = match paths.agent {
        Agent::Kiro | Agent::Claude | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt | Agent::CodexGpt55 | Agent::CodexGpt54 | Agent::OpenCode | Agent::C2rust | Agent::Laertes | Agent::C2SaferRust | Agent::SmartC2Rust | Agent::Kimi | Agent::Oneshot => {
            let tmp = crate::workdir::tempdir("harvest-translate-")
                .context("creating isolated temp dir")?;
            let work = tmp.path().join(crate::battery::TRANSLATED_RUST);
            let c_src = work.join("c_src");
            std::fs::create_dir_all(&c_src)?;
            copy_dir_all(input_test_case, &c_src)?;

            if matches!(paths.agent, Agent::Claude | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt) {
                crate::sandbox::write_settings(&paths.repo_root, tmp.path(), tmp.path())?;
            }

            if matches!(paths.agent, Agent::OpenCode) {
                // OpenCode's run-private config: the Bedrock provider, the
                // sub-agent permission policy, the compaction plugin, and the
                // XDG isolation dir. Analogous to the .claude/settings.json
                // sandbox written just above.
                crate::opencode::materialize_config(
                    tmp.path(),
                    crate::opencode::Phase::Translate,
                    &opencode_model(paths)?,
                )?;
            }

            (work, Some(tmp))
        }
    };

    // OpenCode needs its filesystem-boundary contract and output-cap warning
    // appended, and the boundary names the temp dir — so the final prompt is
    // only known once the workspace exists. Every other agent appends nothing.
    let prompt: &str = &match paths.agent {
        Agent::OpenCode => {
            let tmp_root = work_dir.parent().unwrap_or(&work_dir);
            format!("{prompt}{}", crate::opencode::prompt_suffix(tmp_root))
        }
        _ => prompt.to_string(),
    };

    // Record the EXACT prompt the agent was given, verbatim, next to the result.
    // Prompt files evolve over time, so the filename alone isn't enough to know
    // what a past run actually saw — copying the rendered text makes every result
    // self-documenting (no need to look up a git hash at the run's timestamp).
    // Written AFTER any per-agent suffix so it captures what the agent truly saw.
    // Empty for non-prompt agents (c2rust etc.); skip those to avoid a blank file.
    if !prompt.is_empty() {
        let _ = std::fs::write(logs_dir.join("prompt.md"), prompt);
    }

    match paths.agent {
        Agent::Kiro => {
            let status = Command::new("bash")
                .arg("-lc")
                .arg(r#"set -o pipefail; timeout 5400 kiro-cli chat --no-interactive --trust-all-tools --agent kiro_plain "$1" < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(prompt)
                .arg(&log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(&work_dir)
                .status()
                .context("invoking kiro-cli")?;
            record_agent_exit(status);
        }
        Agent::Claude | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt => {
            let work_root = work_dir.parent().unwrap();
            let settings_path = work_root.join(".claude/settings.json");
            // Agent scratch lands inside the work root — on disk, and discarded
            // with the case — rather than in the shared /tmp tmpfs, and no single
            // file the agent writes may exceed the cap. See crate::workdir.
            let agent_tmp = crate::workdir::agent_tmp(work_root)?;
            let script = format!(
                "ulimit -f {} -d {}; set -o pipefail; timeout 10800 claude -p \"$1\" \
                 --strict-mcp-config --disable-slash-commands --settings \"$3\" \
                 --agents \"$4\" --agent claude_plain --max-turns 1000 \
                 --permission-mode bypassPermissions --verbose \
                 --output-format stream-json < /dev/null 2>&1 | tee \"$2\"",
                crate::workdir::AGENT_FSIZE_BLOCKS,
                crate::workdir::AGENT_DATA_KB
            );
            let status = Command::new("bash")
                .arg("-lc")
                .arg(&script)
                .arg("--")
                .arg(prompt)
                .arg(&log_path)
                .arg(&settings_path)
                .arg(CLAUDE_PLAIN_AGENT_JSON)
                .env("OPENSSL_DIR", &openssl_dir)
                .env("TMPDIR", &agent_tmp)
                .env("CLAUDE_CODE_TMPDIR", &agent_tmp)
                .current_dir(&work_dir)
                .status()
                .context("invoking claude")?;
            record_agent_exit(status);
        }
        Agent::CodexGpt55 | Agent::CodexGpt54 => {
            // Codex CLI on Bedrock. Bedrock backend only — OpenAI auth/telemetry
            // disabled. Model + region overridden per-agent via -c flags so
            // we don't depend on a global ~/.codex/config.toml.
            let (model, region) = match paths.agent {
                Agent::CodexGpt55 => ("openai.gpt-5.5", "us-east-2"),
                Agent::CodexGpt54 => ("openai.gpt-5.4", "us-west-2"),
                _ => unreachable!(),
            };
            invoke_codex_with_retry(
                prompt, &log_path, &work_dir, model, region, &openssl_dir, "translate",
            )?;
        }
        Agent::OpenCode => {
            let tmp_root = work_dir.parent().unwrap_or(&work_dir).to_path_buf();
            invoke_opencode_with_retry(
                crate::opencode::Phase::Translate, prompt, &log_path, &work_dir,
                &tmp_root, &opencode_model(paths)?, TRANSLATE_TIMEOUT_SECS, "translate",
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

    if !work_dir.join("Cargo.toml").exists() {
        anyhow::bail!("no Cargo.toml produced");
    }

    // Merge the produced crate from the temp dir into `translated/`, preserving
    // the logs/ already written there (copy_dir_all merges, never wipes the dst).
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

/// Propagate the real case's crate to a shared-source config follower, for a
/// given phase. Translate propagates the `translated/` phase; verify (after
/// fixing the real case) re-propagates the `verified/` phase, so every config
/// follower has the SAME post-verify crate the real case ended with — this is
/// what makes runtests score all N configs as verified, not just the real one.
pub fn propagate_config_phase(
    paths: &Paths,
    battery: &str,
    real_case: &str,
    cfg: &battery::Config,
    phase: &str,
) -> Result<()> {
    let real_dir = crate::battery::phase_dir(&paths.case_dir(battery, real_case), phase);
    // Nothing to propagate if the real case never produced this phase (e.g. an
    // agent with no verify phase → no verified/ to copy).
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

    // Copy root-level files (lib.rs, build.rs, rust-toolchain.toml, etc.)
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

/// Propagate the real case's `translated/` crate to a config follower (the
/// translate-phase default).
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
// The agent CLIs (kiro-cli / claude / codex) are shelled out with `.status()`,
// but the metrics JSON is written in a different, shallower function than the
// one that invokes them (run_and_record vs translate_case_at, and similarly
// for verify). Rather than thread the exit status back through
// dispatch_translate's ~12 match arms, we stash the most recent agent exit in
// a THREAD-LOCAL. This is sound because each case runs on its own thread
// (translate/verify parallelize with one case per spawned thread), so the
// "last agent exit on this thread" is unambiguously this case's agent run.
//
// `exit_code` is the shell pipeline's status (`set -o pipefail` makes it the
// agent's own code, or `timeout`'s 124 on timeout → `timed_out`). It is absent
// for non-CLI agents (API-based kimi/oneshot, in-process c2rust) which have no
// single agent-process exit code.
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

/// Record the exit of an agent CLI invocation for the current case/thread.
pub fn record_agent_exit(status: std::process::ExitStatus) {
    let code = status.code();
    LAST_AGENT_EXIT.with(|c| c.set(AgentExit {
        exit_code: code,
        timed_out: code == Some(124), // `timeout` exits 124 when it kills the child
        recorded: true,
    }));
}

/// Clear the stashed exit at the start of a case, so a non-CLI agent (or a
/// re-used thread) can never inherit a previous case's exit code.
pub fn clear_agent_exit() {
    LAST_AGENT_EXIT.with(|c| c.set(AgentExit::default()));
}

/// Take (and clear) the stashed exit for the current thread.
fn take_agent_exit() -> AgentExit {
    LAST_AGENT_EXIT.with(|c| c.replace(AgentExit::default()))
}

/// Add `exit_code` / `timed_out` to a metrics object if a CLI agent exit was
/// recorded this run. Shared by translate and verify so both report it
/// identically — no double standard.
fn merge_agent_exit(metrics: &mut serde_json::Value) {
    let e = take_agent_exit();
    if e.recorded {
        metrics["exit_code"] = serde_json::json!(e.exit_code);
        metrics["timed_out"] = serde_json::json!(e.timed_out);
    }
}

fn write_translation_metrics(case_dir: &Path, agent: Agent, duration_secs: u64, success: bool) {
    let mut metrics = serde_json::json!({
        "agent": format!("{agent:?}").to_lowercase(),
        "duration_secs": duration_secs,
        "success": success,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    merge_agent_exit(&mut metrics);
    let _ = std::fs::create_dir_all(case_dir);
    let _ = std::fs::write(
        case_dir.join("translation.json"),
        serde_json::to_string_pretty(&metrics).unwrap_or_default() + "\n",
    );
}

/// Verify-side sibling of [`write_translation_metrics`], writing
/// `verification.json` with the same shape (incl. agent exit). No double
/// standard: verify records agent process health exactly like translate.
pub fn write_verification_metrics(case_dir: &Path, agent: Agent, duration_secs: u64, success: bool) {
    let mut metrics = serde_json::json!({
        "agent": format!("{agent:?}").to_lowercase(),
        "duration_secs": duration_secs,
        "success": success,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    merge_agent_exit(&mut metrics);
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

// ── CRUST-bench translation ────────────────────────────────────────────

/// Whether the scaffold includes ground-truth test files.
enum ScaffoldMode {
    /// Standard: copy everything including src/bin/ (agent sees tests).
    Standard,
    /// Blind: strip src/bin/ after copy (agent never sees tests).
    Blind,
}

/// Prepare a CRUST workspace: copy scaffold, move interfaces, copy C source.
/// Returns (tempdir, work_path, log_path).
fn prepare_crust_workspace(
    paths: &Paths,
    project: &battery::CrustProject,
    mode: &ScaffoldMode,
    log_dir: &Path,
    log_name: &str,
) -> Result<(tempfile::TempDir, PathBuf, PathBuf)> {
    std::fs::create_dir_all(log_dir)?;
    let log_path = log_dir.join(log_name);

    let tmp = crate::workdir::tempdir("harvest-crust-")
        .context("creating temp dir for CRUST")?;
    let work = tmp.path().join("project");

    copy_dir_all(project.scaffold(), &work)?;

    // Blind mode: remove test files and test metadata so agent never sees them
    if matches!(mode, ScaffoldMode::Blind) {
        let bin_dir = work.join("src/bin");
        if bin_dir.is_dir() {
            std::fs::remove_dir_all(&bin_dir)?;
        }
        // Strip [[test]] entries from Cargo.toml — they reference the hidden test files
        let cargo_path = work.join("Cargo.toml");
        if cargo_path.exists() {
            let content = std::fs::read_to_string(&cargo_path)?;
            let stripped = strip_test_entries(&content);
            std::fs::write(&cargo_path, stripped)?;
        }
    }

    // Move interfaces/*.rs → src/ (matches CRUST-bench's format_into_compilable_rust)
    // Skip main.rs — it conflicts with Cargo's binary crate detection
    let interfaces = work.join("src/interfaces");
    if interfaces.is_dir() {
        let src = work.join("src");
        for entry in std::fs::read_dir(&interfaces)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "main.rs" { continue; }
            if entry.path().extension().map_or(false, |e| e == "rs") {
                std::fs::rename(entry.path(), src.join(&name))?;
            }
        }
        if std::fs::read_dir(&interfaces)?.next().is_none() {
            std::fs::remove_dir(&interfaces)?;
        }
    }

    let c_dst = work.join("c_src");
    std::fs::create_dir_all(&c_dst)?;
    copy_dir_all(project.c_source(), &c_dst)?;

    Ok((tmp, work, log_path))
}

/// Invoke the agent in a working directory with a prompt.
///
/// `phase` only matters for backends whose per-phase setup differs (OpenCode's
/// compaction-recovery command); the others ignore it.
fn invoke_agent(paths: &Paths, phase: crate::opencode::Phase, prompt: &str, log_path: &Path, work: &Path) -> Result<()> {
    let agent = paths.agent;
    match agent {
        Agent::Kiro => {
            let status = Command::new("bash")
                .arg("-lc")
                .arg(r#"set -o pipefail; timeout 1800 kiro-cli chat --no-interactive --trust-all-tools --agent kiro_plain "$1" < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(prompt)
                .arg(log_path)
                .current_dir(work)
                .status()
                .context("invoking kiro-cli for CRUST")?;
            record_agent_exit(status);
        }
        Agent::Claude | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt => {
            // This wrote a bare `{}` — i.e. no sandbox policy at all — while the
            // Test-Corpus paths did have one, so CRUST translate and CRUST verify
            // ran entirely unrestricted. Same policy as everywhere else now.
            let work_root = work.parent().unwrap_or(work);
            let settings_path =
                crate::sandbox::write_settings(&paths.repo_root, work_root, work_root)?;
            let status = Command::new("bash")
                .arg("-lc")
                .arg(r#"set -o pipefail; timeout 10800 claude -p "$1" --strict-mcp-config --disable-slash-commands --settings "$3" --agents "$4" --agent claude_plain --max-turns 1000 --permission-mode bypassPermissions --verbose --output-format stream-json < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(prompt)
                .arg(log_path)
                .arg(&settings_path)
                .arg(CLAUDE_PLAIN_AGENT_JSON)
                .current_dir(work)
                .status()
                .context("invoking claude for CRUST")?;
            record_agent_exit(status);
        }
        Agent::CodexGpt55 | Agent::CodexGpt54 => {
            let (model, region) = match agent {
                Agent::CodexGpt55 => ("openai.gpt-5.5", "us-east-2"),
                Agent::CodexGpt54 => ("openai.gpt-5.4", "us-west-2"),
                _ => unreachable!(),
            };
            let openssl_dir = std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into());
            invoke_codex_with_retry(
                prompt, log_path, work, model, region, &openssl_dir, "CRUST",
            )?;
        }
        Agent::OpenCode => {
            let tmp_root = work.parent().unwrap_or(work).to_path_buf();
            let model = opencode_model(paths)?;
            crate::opencode::materialize_config(&tmp_root, phase, &model)?;
            let prompt = format!("{prompt}{}", crate::opencode::prompt_suffix(&tmp_root));
            invoke_opencode_with_retry(
                phase, &prompt, log_path, work, &tmp_root, &model,
                CRUST_TIMEOUT_SECS, "CRUST",
            )?;
        }
        Agent::C2rust | Agent::Laertes | Agent::C2SaferRust | Agent::SmartC2Rust | Agent::Kimi | Agent::Oneshot => anyhow::bail!("c2rust/laertes/c2saferrust/smartc2rust/kimi/oneshot not supported for CRUST-bench"),
    }
    Ok(())
}

pub fn run_crust(paths: &Paths, projects: &[battery::CrustProject], parallel: usize) -> Result<()> {
    // CRUST regular: existing translate.md already has "iterate cargo test until passing"
    // built in, so claude-combined uses the same prompt as claude.
    let prompt_file = match paths.agent {
        Agent::ClaudeMinimal => "translate-minimal.md",
        Agent::ClaudeNoIter => "translate-no-iter.md",
        _ => "translate.md",
    };
    run_crust_with_mode(paths, projects, parallel, ScaffoldMode::Standard, prompt_file)
}

pub fn run_crust_blind(paths: &Paths, projects: &[battery::CrustProject], parallel: usize) -> Result<()> {
    let prompt_file = match paths.agent {
        Agent::ClaudeCombined => {
            // Combined prompt: agent translates AND writes its own tests in one session.
            // After translate, the harness mirrors translate/ → verify/ so the test phase
            // (which reads from verify_dir) finds both the Rust translation and the tests.
            "translate-and-verify-blind.md"
        }
        Agent::ClaudeMinimal => "translate-minimal-blind.md",
        Agent::ClaudeNoIter => "translate-no-iter-blind.md",
        _ => "translate-blind.md",
    };
    run_crust_with_mode(paths, projects, parallel, ScaffoldMode::Blind, prompt_file)
}

fn run_crust_with_mode(
    paths: &Paths,
    projects: &[battery::CrustProject],
    parallel: usize,
    mode: ScaffoldMode,
    prompt_file: &str,
) -> Result<()> {
    preflight_check(paths.agent)?;

    anyhow::ensure!(!projects.is_empty(),
        "No CRUST projects to translate. Targets are `CRUST` (all) or \
         `CRUST/<project>`; projects come from crust-bench/datasets/RBench/. \
         Did you unzip crust-bench/datasets and `git submodule update --init`?");
    let total = projects.len();
    let sem = Semaphore::new(parallel);
    // Read prompt once, share across threads
    let prompt = std::fs::read_to_string(paths.prompts_dir.join(prompt_file))
        .with_context(|| format!("reading {prompt_file}"))?;

    let results: Vec<CaseResult> = std::thread::scope(|s| {
        let handles: Vec<_> = projects.iter().map(|p| {
            let prompt = &prompt;
            let mode = &mode;
            let sem = &sem;
            s.spawn(move || {
                let _permit = sem.acquire();
                translate_one_crust(paths, p, mode, prompt)
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
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

fn translate_one_crust(paths: &Paths, project: &battery::CrustProject, mode: &ScaffoldMode, prompt: &str) -> CaseResult {
    match translate_one_crust_inner(paths, project, mode, prompt) {
        Ok(r) => r,
        Err(e) => CaseResult {
            name: project.name().to_string(),
            elapsed_secs: 0,
            success: false,
            error: Some(e.to_string()),
            skipped: false,
        },
    }
}

fn translate_one_crust_inner(paths: &Paths, project: &battery::CrustProject, mode: &ScaffoldMode, prompt: &str) -> Result<CaseResult> {
    let is_blind = matches!(mode, ScaffoldMode::Blind);
    let out: PathBuf = if is_blind {
        paths.translate_dir(project.name()).as_ref().to_owned()
    } else {
        paths.output_dir(project.name())
    };

    if out.join("Cargo.toml").exists() {
        return Ok(CaseResult { name: project.name().into(), elapsed_secs: 0, success: true, error: None, skipped: true });
    }

    let (_tmp, work, log_path) = prepare_crust_workspace(paths, project, mode, &out.join("logs"), "translation.log")?;

    clear_agent_exit();
    let start = Instant::now();
    invoke_agent(paths, crate::opencode::Phase::Translate, prompt, &log_path, &work)?;
    let elapsed = start.elapsed().as_secs();

    // Copy back code from temp, preserving logs dir
    copy_dir_filtered(&work, &out, &["target", "c_src"])?;
    copy_dir_all(&work.join("c_src"), &out.join("c_src"))?;

    let success = out.join("Cargo.toml").exists();
    write_translation_metrics(&out, paths.agent, elapsed, success);

    // Blind CRUST + Claude{Combined,Minimal,NoIter}: when the translate prompt does not
    // run a separate verify phase, mirror translate/ → verify/ so the test phase
    // (which reads from verify_dir) finds the translation. ClaudeMinimal/NoIter won't
    // have written tests; the test phase scores against held-out real tests.
    if is_blind && matches!(paths.agent, Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::CodexGpt55 | Agent::CodexGpt54) && success {
        let verify = paths.verify_dir(project.name());
        if verify.is_dir() {
            std::fs::remove_dir_all(verify.as_ref())?;
        }
        copy_dir_filtered(&out, verify.as_ref(), &["target", "logs"])?;
    }

    Ok(CaseResult { name: project.name().into(), elapsed_secs: elapsed, success, error: None, skipped: false })
}

// ── Blind CRUST verify: agent generates tests ──────────────────────────

pub fn verify_crust_blind(paths: &Paths, projects: &[battery::CrustProject], parallel: usize, force: bool) -> Result<()> {
    preflight_check(paths.agent)?;

    let prompt = std::fs::read_to_string(paths.prompts_dir.join("verify-blind.md"))
        .context("reading verify-blind.md")?;

    let total = projects.len();
    let sem = Semaphore::new(parallel);

    let results: Vec<CaseResult> = std::thread::scope(|s| {
        let handles: Vec<_> = projects.iter().map(|p| {
            let prompt = &prompt;
            let sem = &sem;
            s.spawn(move || {
                let _permit = sem.acquire();
                verify_one_crust_blind(paths, p, prompt, force)
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut verified = 0usize;
    let mut failed = 0usize;
    for (i, r) in results.iter().enumerate() {
        let n = i + 1;
        if r.skipped {
            verified += 1;
            println!("[{n}/{total}] ⏭️  {} (already has tests)", r.name);
        } else if r.success {
            verified += 1;
            println!("[{n}/{total}] ✅ {} ({}s)", r.name, r.elapsed_secs);
        } else {
            failed += 1;
            let err = r.error.as_deref().unwrap_or("unknown");
            println!("[{n}/{total}] ❌ {} — {err} ({}s)", r.name, r.elapsed_secs);
        }
    }

    println!("\nDone: {verified}/{total} verified, {failed} failed");
    Ok(())
}

fn verify_one_crust_blind(paths: &Paths, project: &battery::CrustProject, prompt: &str, force: bool) -> CaseResult {
    match verify_one_crust_blind_inner(paths, project, prompt, force) {
        Ok(r) => r,
        Err(e) => CaseResult {
            name: project.name().to_string(),
            elapsed_secs: 0,
            success: false,
            error: Some(e.to_string()),
            skipped: false,
        },
    }
}

fn verify_one_crust_blind_inner(paths: &Paths, project: &battery::CrustProject, prompt: &str, force: bool) -> Result<CaseResult> {
    let translate = paths.translate_dir(project.name());
    let verify = paths.verify_dir(project.name());

    anyhow::ensure!(translate.join("Cargo.toml").exists(), "translation not found for {}", project.name());

    // Skip if LLM-generated tests already exist (unless --force)
    let bin_dir = verify.join("src/bin");
    if !force && bin_dir.is_dir() && std::fs::read_dir(&bin_dir)?.next().is_some() {
        return Ok(CaseResult { name: project.name().into(), elapsed_secs: 0, success: true, error: None, skipped: true });
    }

    // Wipe old verify dir — always start fresh from translation
    if verify.is_dir() {
        std::fs::remove_dir_all(&verify)?;
    }

    // Set up temp workspace from the immutable translation
    let tmp = crate::workdir::tempdir("harvest-crust-verify-")
        .context("creating temp dir for CRUST verify")?;
    let work = tmp.path().join("project");
    copy_dir_filtered(translate.as_ref(), &work, &["target", "logs"])?;

    let logs_dir = verify.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = logs_dir.join("verify.log");

    clear_agent_exit();
    let start = Instant::now();
    invoke_agent(paths, crate::opencode::Phase::Verify, prompt, &log_path, &work)?;
    let elapsed = start.elapsed().as_secs();

    // Copy agent output to verify/ (not back to translate/)
    copy_dir_filtered(&work, verify.as_ref(), &["target", "c_src", "logs"])?;
    // Ensure c_src is available in verify/ for test compilation
    if translate.join("c_src").is_dir() {
        copy_dir_all(&translate.join("c_src"), &verify.join("c_src"))?;
    }

    let bin_dir = verify.join("src/bin");
    let success = bin_dir.is_dir() && std::fs::read_dir(&bin_dir)?.next().is_some();
    // Record verify agent exit + metrics, mirroring translate — no double standard.
    write_verification_metrics(verify.as_ref(), paths.agent, elapsed, success);
    Ok(CaseResult { name: project.name().into(), elapsed_secs: elapsed, success, error: None, skipped: false })
}

// ── Utilities ──────────────────────────────────────────────────────────

/// Strip `[[test]]` sections from a Cargo.toml string.
/// Each section starts with `[[test]]` and ends at the next `[[` or `[` header or EOF.
fn strip_test_entries(content: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[[test]]" {
            skip = true;
            continue;
        }
        if skip && (trimmed.starts_with("[[") || trimmed.starts_with('[') && !trimmed.starts_with("[[")) {
            skip = false;
        }
        if !skip {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

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

/// Copy a directory tree, skipping top-level directories in `skip`.
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
            // Resolve symlink: if target is dir, recurse; if file, copy target
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
        // Skip non-regular files: FIFOs, sockets, char/block devices.
        // These can appear in agent workspaces (e.g. impcheck creates .pipe FIFOs)
        // and would cause std::fs::copy to block forever.
    }
    Ok(())
}

/// RAII isolated working directory. Copies translated_rust/ into a temp dir,
/// agent works there, `finish()` copies results back. Drop without finish
/// discards the temp dir (safe on failure).
pub struct IsolatedWorkDir {
    tmp: tempfile::TempDir,
    dest: PathBuf,
    finished: bool,
}

impl IsolatedWorkDir {
    pub fn new(case_dir: &Path) -> Result<Self> {
        let tmp = crate::workdir::tempdir("harvest-work-")
            .context("creating isolated work dir")?;
        let src = crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED);
        if src.is_dir() {
            copy_dir_filtered(&src, &tmp.path().join(crate::battery::TRANSLATED_RUST), &["target"])?;
        }
        Ok(Self { tmp, dest: case_dir.to_owned(), finished: false })
    }

    /// Path the agent should work in.
    pub fn translated_rust(&self) -> PathBuf {
        self.tmp.path().join(crate::battery::TRANSLATED_RUST)
    }

    /// Path to the temp root (for setting current_dir).
    pub fn root(&self) -> &Path {
        self.tmp.path()
    }

    /// Write the verified crate to the case's `verified/` phase dir. Consumes
    /// self. Pure: `translated/` is never touched.
    ///
    /// `verified/` is seeded from `translated/` (so `c_src/` and any files the
    /// agent didn't rewrite are present), then the agent's temp output is
    /// overlaid on top. `target/` is skipped from both (build artifact).
    pub fn finish(mut self) -> Result<()> {
        let translated = crate::battery::phase_dir(&self.dest, crate::battery::TRANSLATED);
        let dst = crate::battery::phase_dir(&self.dest, crate::battery::VERIFIED);
        // Wipe verified/ EXCEPT logs/ — the caller writes verify.log into
        // verified/logs/ live during the run, so preserve it across the reseed.
        if dst.exists() {
            for entry in std::fs::read_dir(&dst)? {
                let entry = entry?;
                if entry.file_name() == "logs" { continue; }
                let p = entry.path();
                if entry.file_type()?.is_dir() { std::fs::remove_dir_all(&p)?; }
                else { std::fs::remove_file(&p)?; }
            }
        }
        // 1. Seed verified/ from the immutable translated/ crate (incl. c_src/),
        //    skipping translated/'s own logs/ so the verify log isn't shadowed.
        if translated.is_dir() {
            copy_dir_filtered(&translated, &dst, &["target", "logs"])?;
        }
        // 2. Overlay the agent's temp output (its src/Cargo.toml edits), keeping
        //    the seeded c_src/ and dropping build artifacts.
        copy_dir_filtered(
            &self.tmp.path().join(crate::battery::TRANSLATED_RUST),
            &dst,
            &["target", "c_src"],
        )?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for IsolatedWorkDir {
    fn drop(&mut self) {
        if !self.finished {
            // Agent failed — temp dir discarded, original untouched
        }
    }
}

// ── c2rust ─────────────────────────────────────────────────────────────

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

    // Patch Cargo.toml and source files
    let cargo_path = work_dir.join("Cargo.toml");
    if cargo_path.exists() {
        let mut cargo = std::fs::read_to_string(&cargo_path)?;
        cargo = cargo.replace("name = \"main\"", "name = \"driver\"");
        cargo = cargo.replace("name = \"rust_out\"", "name = \"driver\"");
        let re = regex::Regex::new(r#"name = "translated_rust[^"]*""#).unwrap();
        cargo = re.replace_all(&cargo, r#"name = "driver""#).into_owned();
        for entry in walkdir(work_dir)? {
            if entry.extension().map_or(false, |e| e == "rs") {
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
    let model = paths.model.as_deref().expect("--model required for oneshot");
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

    // Copy c_src for the test harness
    let c_src = translated.join("c_src");
    std::fs::create_dir_all(&c_src)?;
    copy_dir_all(&input_test_case, &c_src)?;

    // Collect C files and detect project kind
    let files_json = collect_c_files_json(&input_test_case)?;
    let is_lib = detect_is_library(&input_test_case).unwrap_or(is_lib_hint);
    let prompt_file = if is_lib { "translate-library.md" } else { "translate-executable.md" };
    let system_prompt = std::fs::read_to_string(paths.prompts_dir.join(prompt_file))
        .with_context(|| format!("reading {prompt_file}"))?;

    // Record the exact system prompt used, verbatim, next to the result (same
    // rationale as the CLI-agent path): the result is self-documenting about
    // which prompt ran, robust to prompt files changing later.
    let _ = std::fs::write(logs_dir.join("prompt.md"), &system_prompt);

    let user_msg = format!(
        "Please translate the following C project into a Rust project including Cargo manifest:\n\n{files_json}\n\nreturn as JSON"
    );

    // Call LLM backend and write output files
    let resp = invoke_llm(&system_prompt, &user_msg, &log_path)?;

    // Write usage metadata
    let mut usage = serde_json::json!({
        "input_tokens": resp.input_tokens,
        "output_tokens": resp.output_tokens,
    });
    if let Some(m) = model { usage["model"] = serde_json::json!(m); }
    let _ = std::fs::write(logs_dir.join("usage.json"),
        serde_json::to_string_pretty(&usage).unwrap_or_default() + "\n");

    write_llm_files(&resp.content, &translated)?;

    if !translated.join("Cargo.toml").exists() {
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

/// Detect whether a project is a library by reading CMakeLists.txt.
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

/// Call AWS Bedrock Converse API and return the assistant's text response.
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

    // Save full raw response
    let response_file = log_path.parent().unwrap().join("translation.response.json");
    let _ = std::fs::write(&response_file, &stdout);

    // Log human-readable summary
    let log_content = format!(
        "=== BEDROCK REQUEST ===\nModel: {BEDROCK_MODEL_ID}\nRegion: {BEDROCK_REGION}\n\n\
         === SYSTEM PROMPT ===\n{system_prompt}\n\n\
         === USER MESSAGE (first 2000 chars) ===\n{}\n\n\
         === STDERR ===\n{stderr}",
        &user_message[..user_message.len().min(2000)]
    );
    let _ = std::fs::write(log_path, &log_content);

    if !output.status.success() {
        anyhow::bail!("bedrock converse failed: {stderr}");
    }

    // Parse full response
    let resp: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("failed to parse Bedrock response: {}", &stdout[..stdout.len().min(500)]))?;

    let content = resp["output"]["message"]["content"][0]["text"]
        .as_str()
        .context("no text in Bedrock response")?
        .trim()
        .to_string();

    let input_tokens = resp["usage"]["inputTokens"].as_u64().unwrap_or(0);
    let output_tokens = resp["usage"]["outputTokens"].as_u64().unwrap_or(0);

    Ok(LlmResponse { content, input_tokens, output_tokens })
}

/// Call OpenRouter chat completions API and return the assistant's text response.
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

    // Save full raw response
    let response_file = log_path.parent().unwrap().join("translation.response.json");
    let _ = std::fs::write(&response_file, &stdout);

    // Log human-readable summary
    let log_content = format!(
        "=== OPENROUTER REQUEST ===\nModel: {model}\n\n\
         === SYSTEM PROMPT ===\n{system_prompt}\n\n\
         === USER MESSAGE (first 2000 chars) ===\n{}\n\n\
         === RAW RESPONSE (first 2000 chars) ===\n{}",
        &user_message[..user_message.len().min(2000)],
        &stdout[..stdout.len().min(2000)]
    );
    let _ = std::fs::write(log_path, &log_content);

    if !output.status.success() {
        anyhow::bail!("curl failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Parse OpenAI-compatible response
    let resp: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("failed to parse OpenRouter response: {}", &stdout[..stdout.len().min(500)]))?;

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

/// Parse a JSON response `{"files": [{"path": "...", "contents": "..."}]}` and write files.
fn write_llm_files(json_response: &str, output_dir: &Path) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct FileEntry { path: String, contents: String }
    #[derive(serde::Deserialize)]
    struct FilesPayload { files: Vec<FileEntry> }

    // Try to extract JSON from markdown code blocks if present
    let json_str = if let Some(start) = json_response.find('{') {
        let from_brace = &json_response[start..];
        // Find the matching closing brace
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
        .with_context(|| format!("failed to parse LLM JSON response: {}", &json_str[..json_str.len().min(500)]))?;

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

/// Shell script executed inside the Laertes Docker container.
/// Expects the project mounted at /mnt/project with read-write access.
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

    // Locate c2rust source: its translated/ crate (sibling under results/).
    // c2rust never runs a verify phase, so translated/ IS the crate to consume.
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

    // Copy c2rust output (skip target/ and Cargo.lock)
    copy_dir_filtered(&c2rust_original, &translated, &["target"])?;

    let mut log = std::fs::File::create(&log_path)?;
    writeln!(log, "source: {}", c2rust_original.display())?;

    // Pre-process for nightly-2020-10-15
    writeln!(log, "\n=== Laertes pre-process ===")?;
    laertes_preprocess(&translated)?;
    writeln!(log, "done")?;

    // Run Laertes in Docker
    writeln!(log, "\n=== Laertes Docker ===")?;
    let mount = format!("{}:/mnt/project", translated.display());
    let docker_out = Command::new("docker")
        .args(["run", "--rm", "-v", &mount, LAERTES_DOCKER_IMAGE, "bash", "-c", LAERTES_DOCKER_SCRIPT])
        .output()
        .context("running laertes docker container")?;
    log.write_all(&docker_out.stdout)?;
    log.write_all(&docker_out.stderr)?;

    // Post-process for modern toolchain
    writeln!(log, "\n=== Laertes post-process ===")?;
    laertes_postprocess(&translated)?;

    // Verify it compiles
    let build = Command::new("cargo")
        .args(["+nightly", "build", "--release"])
        .env("RUSTFLAGS", "-Awarnings")
        .current_dir(&translated)
        .output()
        .context("cargo build after laertes")?;
    log.write_all(&build.stdout)?;
    log.write_all(&build.stderr)?;
    let ok = build.status.success();
    writeln!(log, "\nlaertes translation {}", if ok { "succeeded" } else { "FAILED to compile (non-fatal)" })?;

    Ok(())
}

// ── C2SaferRust (c2rust output -> LLM unsafe-reduction via Bedrock) ────────

const C2SAFERRUST_DOCKER_IMAGE: &str = "c2saferrust:latest";
const C2SAFERRUST_MODEL: &str = "bedrock-gpt54";
const C2SAFERRUST_DEFAULT_BASE_URL: &str =
    "https://bedrock-mantle.us-west-2.api.aws/openai/v1";

/// Shell script run inside the C2SaferRust container.
/// The mounted workspace is at /work; the reshaped crate is /work/rust.
/// We give the (non-root) container user a writable HOME + CARGO_HOME and seed
/// the cargo registry from the image so the pinned-nightly build has no network
/// dependency. translate.py writes its result to /work/rust_WIP.
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

// Process-lived Bedrock bearer-token cache. Follows the internal pattern
// (e.g. ElasticGumbyAgenticMCP, SageMaker hosting benchmark, CodeBlocksLibrary
// midway token-refresh): mint a 12h token but refresh well before expiry so a
// long batch run can never outlive its token. We refresh at 50% of the 12h
// lifetime (6h), matching ElasticGumby's "hours of retry headroom" guidance.
static BEDROCK_TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
const BEDROCK_TOKEN_REFRESH_AFTER: Duration = Duration::from_secs(6 * 3600);

/// Return a valid Bedrock bearer token. Precedence:
///   1. BEDROCK_API_KEY / AWS_BEARER_TOKEN_BEDROCK env (CI / manual injection)
///   2. process cache, if the cached token is younger than the refresh window
///   3. a freshly minted token from the host (aws_bedrock_token_generator)
/// Minting strips AWS_PROFILE/AWS_DEFAULT_PROFILE so the token is issued for the
/// operator's `default` (ada) profile, not any session profile that Claude Code
/// or other tooling may have exported (a real bug we hit: wrong-account 401s).
fn bedrock_token(region: &str) -> Result<String> {
    if let Ok(t) = std::env::var("BEDROCK_API_KEY") {
        if !t.trim().is_empty() { return Ok(t); }
    }
    if let Ok(t) = std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
        if !t.trim().is_empty() { return Ok(t); }
    }

    let mut guard = BEDROCK_TOKEN.lock().unwrap();
    if let Some((tok, born)) = guard.as_ref() {
        if born.elapsed() < BEDROCK_TOKEN_REFRESH_AFTER {
            return Ok(tok.clone());
        }
    }

    let tok = mint_bedrock_token(region)?;
    *guard = Some((tok.clone(), Instant::now()));
    Ok(tok)
}

/// Mint a short-term (12h) Bedrock bearer token on the host via the standard
/// `aws_bedrock_token_generator` python package, using the `default` AWS profile.
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

/// C2SaferRust: post-process this repo's c2rust output with an LLM to reduce
/// unsafe code. Input is the sibling `c2rust/.../translated_rust_original`
/// (same source as Laertes). Runs the pinned submodule tool in Docker, driven
/// by gpt-5.4 via Amazon Bedrock. Blind by design (no `--test_dir`): the tool
/// is compile-gated only, making its numbers comparable to ACTOR self-verified.
///
/// Requires `BEDROCK_API_KEY` in the environment (a Bedrock bearer token).
/// `BEDROCK_BASE_URL` may override the default us-west-2 mantle endpoint.
fn c2saferrust_translate_case(paths: &Paths, battery: &str, name: &str, _is_lib: bool) -> Result<()> {
    use std::io::Write;

    // Locate c2rust source: its translated/ crate (sibling under results/).
    // c2rust never runs a verify phase, so translated/ IS the crate to consume.
    let c2rust_case = paths.results_dir
        .parent().context("no parent for results_dir")?
        .join("c2rust").join(battery).join(name);
    let c2rust_original = crate::battery::phase_dir(&c2rust_case, crate::battery::TRANSLATED);
    anyhow::ensure!(c2rust_original.is_dir(),
        "c2rust translated/ crate not found (run the c2rust agent first): {}",
        c2rust_original.display());

    // Bedrock bearer token: env override wins (CI/manual), else a fresh token
    // is minted on the host and cached with early refresh (see bedrock_token).
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

    // Isolated workspace we bind-mount into the container. The tool reshapes
    // <work>/rust and writes <work>/rust_WIP.
    let tmp = crate::workdir::tempdir("harvest-c2sr-")
        .context("creating c2saferrust temp workspace")?;
    let work_rust = tmp.path().join("rust");
    // Copy c2rust output as the tool's input (skip build artifacts + bundled C).
    copy_dir_filtered(&c2rust_original, &work_rust, &["target", "c_src"])?;
    let _ = std::fs::remove_file(work_rust.join("Cargo.lock"));

    let mut log = std::fs::File::create(&log_path)?;
    writeln!(log, "source: {}", c2rust_original.display())?;
    writeln!(log, "model: {} via {}", C2SAFERRUST_MODEL, base_url)?;

    // Pre-process: reshape the c2rust crate into what the slicer expects.
    writeln!(log, "\n=== C2SaferRust pre-process ===")?;
    c2saferrust_preprocess(&work_rust)?;
    writeln!(log, "done")?;

    // Run the tool in Docker, as the host user so outputs are not root-owned.
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

    // Collect the tool's output (<work>/rust_WIP) into translated_rust/.
    // If the tool produced no rust_WIP, the C2Rust input did not compile under
    // the pinned nightly (e.g. SPHINCS+, whose duplicate `randombytes` symbol is
    // a hard error on nightly-2022-08-08) or translation otherwise failed. In
    // that case fall back to emitting the unmodified C2Rust input as the result,
    // so the case is still counted and fails at test time — the faithful
    // representation of "C2SaferRust could not improve this input", matching how
    // c2rust/laertes are reported (0/128 on P01) rather than silently vanishing.
    let wip = tmp.path().join("rust_WIP");
    let source_dir = if wip.join("Cargo.toml").exists() {
        writeln!(log, "\nrust_WIP produced; collecting C2SaferRust output")?;
        wip.clone()
    } else {
        writeln!(log, "\nNo rust_WIP produced (C2Rust input failed to compile under \
                       nightly-2022-08-08, or translation failed). Falling back to the \
                       unmodified C2Rust input so the case is counted as a failure.")?;
        work_rust.clone()
    };
    // Keep only source + manifest; drop the tool's bookkeeping + build artifacts.
    copy_dir_filtered(&source_dir, &translated, &["target"])?;
    for junk in ["callgraph.dot", "callgraph.pdf", "slices.json", "log.txt", "prompts.txt", "ordering.txt"] {
        let _ = std::fs::remove_file(translated.join(junk));
    }
    // Remove any leftover .old rollback files.
    if let Ok(entries) = std::fs::read_dir(&translated) {
        for e in entries.flatten() {
            if e.path().extension().map_or(false, |x| x == "old") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    // Post-process: restore a standard toolchain so downstream testing matches
    // the other agents (the tool pins nightly-2022-08-08 only for its slicer).
    c2saferrust_postprocess(&translated)?;

    // Copy the tool's per-function log alongside for provenance.
    if wip.join("log.txt").exists() {
        let _ = std::fs::copy(wip.join("log.txt"), logs_dir.join("c2saferrust_log.txt"));
    }

    writeln!(log, "\nc2saferrust translation collected into {}", translated.display())?;
    Ok(())
}

/// Reshape a c2rust crate so the C2SaferRust slicer can build it as a library:
/// ensure an rlib crate-type and pin the nightly the tool requires.
fn c2saferrust_preprocess(work_dir: &Path) -> Result<()> {
    // crate-type must include rlib (c2rust emits cdylib for _lib cases).
    let cargo = work_dir.join("Cargo.toml");
    if cargo.exists() {
        let mut s = std::fs::read_to_string(&cargo)?;
        let re = regex::Regex::new(r#"crate-type\s*=\s*\[[^\]]*\]"#).unwrap();
        if re.is_match(&s) {
            s = re.replace(&s, r#"crate-type = ["staticlib","rlib"]"#).into_owned();
        }
        std::fs::write(&cargo, s)?;
    }
    // Pin the toolchain the slicer/metrics were built against.
    std::fs::write(
        work_dir.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2022-08-08\"\ncomponents = [\"rustfmt\", \"rustc-dev\", \"rust-src\", \"llvm-tools-preview\"]\n",
    )?;
    Ok(())
}

/// Restore a standard toolchain after translation so downstream build/test uses
/// the same toolchain as the other agents rather than the slicer's old pin.
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
        if path.extension().map_or(true, |e| e != "rs") { continue; }
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

    // Fix entry point features
    let lib_rs = work_dir.join("lib.rs");
    if lib_rs.exists() {
        let mut src = std::fs::read_to_string(&lib_rs)?;
        if !src.contains("rustc_private") {
            src.insert_str(0, "#![feature(rustc_private)]\n");
        }
        std::fs::write(&lib_rs, src)?;
    }

    // Cargo.toml: edition 2018, pin libc for old resolver
    let cargo = work_dir.join("Cargo.toml");
    if cargo.exists() {
        let mut s = std::fs::read_to_string(&cargo)?;
        s = s.replace("edition = \"2021\"", "edition = \"2018\"");
        s = s.replace("libc = \"0.2\"", "libc = \"=0.2.126\"");
        std::fs::write(&cargo, s)?;
    }
    Ok(())
}

/// Restore modern-toolchain compatibility after Laertes rewrites.
fn laertes_postprocess(work_dir: &Path) -> Result<()> {
    let libc_internal = regex::Regex::new(r"libc::(?:[a-z_0-9]+::)+([a-z_0-9]+)").unwrap();
    for path in walkdir(work_dir)? {
        if path.extension().map_or(true, |e| e != "rs") { continue; }
        let src = std::fs::read_to_string(&path)?;
        let mut out = src.replace("extern crate libc;\n", "");
        out = libc_internal.replace_all(&out, "libc::$1").into_owned();
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

/// Retry-aware codex invocation. Bedrock occasionally returns transient
/// errors mid-conversation (404 "Engine not found", "stream disconnected",
/// "server had an error") that surface in the JSON log as `"type":"error"`
/// followed by `"type":"turn.failed"`. The codex process exits 0 in those
/// cases, so the harness has historically treated a Bedrock failure as a
/// successful (but empty) translation.
///
/// This helper runs codex, scans the log for those patterns, and re-runs
/// up to MAX_RETRIES times if a transient error is detected. Each retry
/// clears the log (codex's `tee` overwrites anyway) and gets a fresh
/// invocation so any partial state is discarded.
fn invoke_codex_with_retry(
    prompt: &str,
    log_path: &Path,
    work_dir: &Path,
    model: &str,
    region: &str,
    openssl_dir: &str,
    context_label: &str,
) -> Result<()> {
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
        // Record the final attempt's exit (overwritten each retry, so the last
        // one wins — the exit that actually determined the outcome).
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
/// Detected patterns: see [`TRANSIENT_BEDROCK_PATTERNS`].
fn scan_codex_log_for_transient_error(log_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(log_path).ok()?;

    // The transient errors all appear as `"type":"error"` events. We require
    // the run to ALSO end in `"turn.failed"` (so it really aborted) and
    // NOT contain a `"turn.completed"` (would mean it recovered).
    if !content.contains(r#""type":"turn.failed""#) {
        return None;
    }
    if content.contains(r#""type":"turn.completed""#) {
        return None;
    }

    first_transient_pattern(&content)
}

/// Returns Some(reason) if an OpenCode log indicates a transient Bedrock
/// failure. Same motivation as the codex scanner: the CLI can exit 0 after
/// Bedrock drops the conversation, so the harness would otherwise record the
/// throttle as a legitimately-empty translation.
///
/// OpenCode's `--format json` emits one event object per line. A transient
/// failure surfaces as an error event; a session that produced any assistant
/// output is treated as recovered, since retrying would discard real work.
fn scan_opencode_log_for_transient_error(log_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(log_path).ok()?;

    // No error surfaced at all → nothing to retry.
    let has_error = content.contains(r#""type":"error""#)
        || content.contains(r#""error":"#)
        || content.contains("APICallError");
    if !has_error {
        return None;
    }
    // The session produced assistant/tool activity → it recovered on its own.
    if content.contains(r#""type":"tool"#) || content.contains(r#""role":"assistant""#) {
        return None;
    }

    first_transient_pattern(&content)
}

/// Retry-aware OpenCode invocation, mirroring [`invoke_codex_with_retry`].
/// Bedrock throttles and 5xx responses are transient and can leave the CLI
/// exiting 0 with nothing written; without this a throttle is indistinguishable
/// from a genuine translation failure.
#[allow(clippy::too_many_arguments)]
fn invoke_opencode_with_retry(
    phase: crate::opencode::Phase,
    prompt: &str,
    log_path: &Path,
    work_dir: &Path,
    tmp_root: &Path,
    model: &crate::opencode::Model,
    timeout_secs: u64,
    context_label: &str,
) -> Result<()> {
    const MAX_RETRIES: u32 = 3;
    const RETRY_BACKOFF_SECS: u64 = 30;

    for attempt in 1..=MAX_RETRIES {
        crate::opencode::invoke(phase, prompt, log_path, work_dir, tmp_root, model, timeout_secs)
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

    /// A real ExitStatus from `sh -c "exit N"`, for testing exit capture.
    fn exit_status(code: i32) -> std::process::ExitStatus {
        Command::new("sh").arg("-c").arg(format!("exit {code}")).status().unwrap()
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
        // No record_agent_exit call (e.g. kimi/oneshot API path) → no fields,
        // so a stale exit is never falsely attributed.
        clear_agent_exit();
        let mut m = serde_json::json!({"success": true});
        merge_agent_exit(&mut m);
        assert!(m.get("exit_code").is_none(), "exit_code must be absent when no CLI agent ran");
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
        // Bedrock throttled before any assistant output: retrying is right,
        // because otherwise the empty result is scored as a real failure.
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
        // An error followed by assistant/tool activity means the session
        // recovered. Retrying would DISCARD a completed translation.
        let log = write_log(
            "{\"type\":\"error\",\"error\":{\"message\":\"ThrottlingException\"}}\n\
             {\"type\":\"tool\",\"name\":\"write\"}\n",
        );
        assert_eq!(scan_opencode_log_for_transient_error(log.path()), None);
    }

    #[test]
    fn opencode_scanner_ignores_clean_and_nontransient_logs() {
        // No error at all → nothing to retry.
        let clean = write_log(r#"{"type":"step","name":"done"}"#);
        assert_eq!(scan_opencode_log_for_transient_error(clean.path()), None);
        // An error that is NOT transient (a genuine model/tool failure) must
        // not be retried either — retrying can't fix it and burns hours.
        let hard = write_log(r#"{"type":"error","error":{"message":"ValidationException: bad request"}}"#);
        assert_eq!(scan_opencode_log_for_transient_error(hard.path()), None);
    }

    #[test]
    fn both_bedrock_backends_share_one_transient_pattern_table() {
        // The failures come from Bedrock, not from the CLI wrapping it, so the
        // codex and opencode scanners must agree on what is retryable.
        for (needle, label) in TRANSIENT_BEDROCK_PATTERNS {
            assert_eq!(first_transient_pattern(needle).as_deref(), Some(*label));
        }
    }
}
