use crate::battery::{self, Case, Paths};
use crate::cargo_toml::{self, CargoToml};
use crate::cli::Agent;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

// ── claude_plain agent (mirrors kiro_plain) ────────────────────────────
// Used with `--agent claude_plain` to give Claude Code a neutral profile
// matching kiro_plain.json: built-in tools only (Bash/Edit/Read/Write/Task),
// no skills/plugins/MCP, no extra system prompt.
pub const CLAUDE_PLAIN_AGENT_JSON: &str = r#"{"claude_plain":{"description":"Bare-bones agent matching kiro_plain","prompt":"You are a coding assistant. Use the available tools to complete the user's task.","tools":["Bash","Edit","Read","Write","Task"]}}"#;

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

pub fn run_test_corpus(paths: &Paths, battery_name: &str, filter: Option<&str>, parallel: usize) -> Result<()> {
    preflight_check(paths.agent)?;

    let battery = battery::discover(&paths.corpus_dir, battery_name, filter)?;
    let output_dir = paths.output_dir(battery_name);
    std::fs::create_dir_all(&output_dir)?;

    let total = count_cases(&battery);

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
            if output_dir.join(&cfg.name).join("translated_rust/Cargo.toml").exists() {
                translated += 1;
                println!("[{current}/{total}] ⏭️  {} (already done)", cfg.name);
                continue;
            }
            match propagate_config(&paths, battery_name, &group.real_case, cfg) {
                Ok(()) => {
                    let _ = save_original(&output_dir, &cfg.name);
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
    if output_dir.join(&case.name).join("translated_rust/Cargo.toml").exists() {
        return CaseResult { name: case.name.clone(), elapsed_secs: 0, success: true, error: None, skipped: true };
    }

    run_and_record(&case.name, &output_dir.join(&case.name), paths.agent, output_dir,
        || dispatch_translate(paths, battery_name, &case.name, case.is_lib),
        || post_process_independent(paths, battery_name, &case.name, case.is_lib),
    )
}

fn translate_one_shared(
    paths: &Paths,
    output_dir: &Path,
    battery_name: &str,
    group: &battery::SharedSourceGroup,
) -> CaseResult {
    let real_dir = output_dir.join(&group.real_case);
    if real_dir.join("translated_rust/Cargo.toml").exists() {
        return CaseResult { name: group.real_case.clone(), elapsed_secs: 0, success: true, error: None, skipped: true };
    }

    println!("Translating: {} (shared-source, {} configs)", group.real_case, group.configs.len());
    run_and_record(&group.real_case, &real_dir, paths.agent, output_dir,
        || dispatch_translate_shared(paths, battery_name, &group.real_case),
        || {
            if let Ok(mut cargo) = CargoToml::open(&real_dir.join("translated_rust/Cargo.toml")) {
                cargo.add_workspace();
                // Patch default features from CMakePresets.json (same as config copies)
                let features = battery::extract_features_from_path(
                    &paths.input_dir(battery_name).join(&group.real_case).join("CMakePresets.json"),
                ).unwrap_or_default();
                let resolved = battery::resolve_features(
                    &real_dir.join("translated_rust/Cargo.toml"), &features,
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
    output_dir: &Path,
    translate_fn: impl FnOnce() -> Result<()>,
    post_process_fn: impl FnOnce() -> Result<()>,
) -> CaseResult {
    let start = Instant::now();
    match translate_fn() {
        Ok(()) => {
            let elapsed = start.elapsed().as_secs();
            write_translation_metrics(case_dir, agent, elapsed, true);
            let _ = post_process_fn();
            let _ = save_original(output_dir, name);
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
        Agent::Kimi => kimi_translate_case(paths, battery, name, is_lib),
        Agent::Oneshot => oneshot_translate_case(paths, battery, name, is_lib),
        Agent::Kiro | Agent::KiroTranslate | Agent::Claude => {
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
        Agent::Kimi => kimi_translate_case(paths, battery, name, true),
        Agent::Oneshot => oneshot_translate_case(paths, battery, name, true),
        Agent::Kiro | Agent::KiroTranslate | Agent::Claude => {
            let prompt = std::fs::read_to_string(paths.prompts_dir.join("translate-shared.md")).unwrap_or_default();
            translate_case(paths, battery, name, &prompt)
        }
        Agent::C2rust => translate_case(paths, battery, name, ""),
    }
}

// ── Preflight ──────────────────────────────────────────────────────────

fn preflight_check(agent: Agent) -> Result<()> {
    let (cmd, version_args): (&str, &[&str]) = match agent {
        Agent::Kiro | Agent::KiroTranslate => ("kiro-cli", &["--version"]),
        Agent::Claude => ("claude", &["--version"]),
        Agent::C2rust => ("c2rust", &["--version"]),
        Agent::Laertes => ("docker", &["--version"]),
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

    if agent == Agent::Claude {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let version_str = stdout.lines()
            .find(|l| l.chars().next().map_or(false, |c| c.is_ascii_digit()))
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
    let case_dir = paths.case_dir(battery, name);

    if case_dir.exists() {
        std::fs::remove_dir_all(&case_dir)?;
    }

    let logs_dir = case_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;

    let input_test_case = paths.input_dir(battery).join(name).join("test_case");
    let log_path = logs_dir.join("translation.log");
    let openssl_dir = std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into());

    let (work_dir, _tmp_guard) = match paths.agent {
        Agent::Kiro | Agent::KiroTranslate | Agent::Claude | Agent::C2rust | Agent::Laertes | Agent::Kimi | Agent::Oneshot => {
            let tmp = tempfile::Builder::new()
                .prefix("harvest-translate-")
                .tempdir()
                .context("creating isolated temp dir")?;
            let work = tmp.path().join("translated_rust");
            let c_src = work.join("c_src");
            std::fs::create_dir_all(&c_src)?;
            copy_dir_all(&input_test_case, &c_src)?;

            if paths.agent == Agent::Claude {
                let claude_dir = tmp.path().join(".claude");
                std::fs::create_dir_all(&claude_dir)?;
                let repo_root = paths.results_dir.parent().unwrap_or(Path::new("/"));
                std::fs::write(
                    claude_dir.join("settings.json"),
                    serde_json::json!({
                        "sandbox": {
                            "enabled": true,
                            "allowUnsandboxedCommands": false,
                            "filesystem": {
                                "denyRead": [repo_root.to_string_lossy()],
                                "allowRead": [tmp.path().to_string_lossy()],
                                "allowWrite": [tmp.path().to_string_lossy()]
                            }
                        }
                    }).to_string(),
                )?;
            }

            (work, Some(tmp))
        }
    };

    match paths.agent {
        Agent::Kiro | Agent::KiroTranslate => {
            let _status = Command::new("bash")
                .arg("-lc")
                .arg(r#"set -o pipefail; timeout 5400 kiro-cli chat --no-interactive --trust-all-tools --agent kiro_plain "$1" < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(prompt)
                .arg(&log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(&work_dir)
                .status()
                .context("invoking kiro-cli")?;
        }
        Agent::Claude => {
            let settings_path = work_dir.parent().unwrap().join(".claude/settings.json");
            let _status = Command::new("bash")
                .arg("-lc")
                .arg(r#"set -o pipefail; timeout 10800 claude -p "$1" --strict-mcp-config --disable-slash-commands --settings "$3" --agents "$4" --agent claude_plain --max-turns 1000 --permission-mode bypassPermissions --verbose --output-format stream-json < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(prompt)
                .arg(&log_path)
                .arg(&settings_path)
                .arg(CLAUDE_PLAIN_AGENT_JSON)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(&work_dir)
                .status()
                .context("invoking claude")?;
        }
        Agent::C2rust => {
            c2rust_translate(&work_dir, &log_path)?;
        }
        Agent::Laertes => unreachable!("laertes uses laertes_translate_case"),
        Agent::Kimi => unreachable!("kimi uses kimi_translate_case"),
        Agent::Oneshot => unreachable!("oneshot uses oneshot_translate_case"),
    };

    if !work_dir.join("Cargo.toml").exists() {
        anyhow::bail!("no Cargo.toml produced");
    }

    // Copy from temp dir back to results
    let translated = case_dir.join("translated_rust");
    if translated.exists() {
        std::fs::remove_dir_all(&translated)?;
    }
    copy_dir_all(&work_dir, &translated)?;

    Ok(())
}

// ── Post-processing ────────────────────────────────────────────────────

fn post_process_independent(paths: &Paths, battery: &str, name: &str, is_lib: bool) -> Result<()> {
    let cargo_path = paths.case_dir(battery, name).join("translated_rust/Cargo.toml");
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
        cargo_toml::strip_for_lib(&paths.case_dir(battery, name).join("translated_rust"))?;
    } else {
        cargo.set_bin_driver();
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
    let real_dir = paths.case_dir(battery, real_case).join("translated_rust");
    let cfg_dir = paths.case_dir(battery, &cfg.name);
    let translated = cfg_dir.join("translated_rust");

    std::fs::create_dir_all(&translated)?;
    std::fs::create_dir_all(cfg_dir.join("logs"))?;

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

// ── Metrics ────────────────────────────────────────────────────────────

fn write_translation_metrics(case_dir: &Path, agent: Agent, duration_secs: u64, success: bool) {
    let metrics = serde_json::json!({
        "agent": format!("{agent:?}").to_lowercase(),
        "duration_secs": duration_secs,
        "success": success,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let _ = std::fs::create_dir_all(case_dir);
    let _ = std::fs::write(
        case_dir.join("translation.json"),
        serde_json::to_string_pretty(&metrics).unwrap_or_default() + "\n",
    );
}

fn save_original(output_dir: &Path, name: &str) -> Result<()> {
    let translated = output_dir.join(name).join("translated_rust");
    let original = output_dir.join(name).join("translated_rust_original");
    if original.exists() {
        std::fs::remove_dir_all(&original)?;
    }
    copy_dir_all(&translated, &original)?;
    Ok(())
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

    let tmp = tempfile::Builder::new()
        .prefix("harvest-crust-")
        .tempdir()
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
fn invoke_agent(agent: Agent, prompt: &str, log_path: &Path, work: &Path) -> Result<()> {
    match agent {
        Agent::Kiro | Agent::KiroTranslate => {
            Command::new("bash")
                .arg("-lc")
                .arg(r#"set -o pipefail; timeout 1800 kiro-cli chat --no-interactive --trust-all-tools --agent kiro_plain "$1" < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(prompt)
                .arg(log_path)
                .current_dir(work)
                .status()
                .context("invoking kiro-cli for CRUST")?;
        }
        Agent::Claude => {
            // Write a minimal settings.json so --settings can find one
            let claude_dir = work.parent().unwrap_or(work).join(".claude");
            std::fs::create_dir_all(&claude_dir)?;
            let settings_path = claude_dir.join("settings.json");
            std::fs::write(&settings_path, "{}")?;
            Command::new("bash")
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
        }
        Agent::C2rust | Agent::Laertes | Agent::Kimi | Agent::Oneshot => anyhow::bail!("c2rust/laertes/kimi/oneshot not supported for CRUST-bench"),
    }
    Ok(())
}

pub fn run_crust(paths: &Paths, projects: &[battery::CrustProject], parallel: usize) -> Result<()> {
    run_crust_with_mode(paths, projects, parallel, ScaffoldMode::Standard, "translate.md")
}

pub fn run_crust_blind(paths: &Paths, projects: &[battery::CrustProject], parallel: usize) -> Result<()> {
    run_crust_with_mode(paths, projects, parallel, ScaffoldMode::Blind, "translate-blind.md")
}

fn run_crust_with_mode(
    paths: &Paths,
    projects: &[battery::CrustProject],
    parallel: usize,
    mode: ScaffoldMode,
    prompt_file: &str,
) -> Result<()> {
    preflight_check(paths.agent)?;

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

    let start = Instant::now();
    invoke_agent(paths.agent, prompt, &log_path, &work)?;
    let elapsed = start.elapsed().as_secs();

    // Copy back code from temp, preserving logs dir
    copy_dir_filtered(&work, &out, &["target", "c_src"])?;
    copy_dir_all(&work.join("c_src"), &out.join("c_src"))?;

    let success = out.join("Cargo.toml").exists();
    write_translation_metrics(&out, paths.agent, elapsed, success);
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
    let tmp = tempfile::Builder::new()
        .prefix("harvest-crust-verify-")
        .tempdir()
        .context("creating temp dir for CRUST verify")?;
    let work = tmp.path().join("project");
    copy_dir_filtered(translate.as_ref(), &work, &["target", "logs"])?;

    let logs_dir = verify.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = logs_dir.join("verify.log");

    let start = Instant::now();
    invoke_agent(paths.agent, prompt, &log_path, &work)?;
    let elapsed = start.elapsed().as_secs();

    // Copy agent output to verify/ (not back to translate/)
    copy_dir_filtered(&work, verify.as_ref(), &["target", "c_src", "logs"])?;
    // Ensure c_src is available in verify/ for test compilation
    if translate.join("c_src").is_dir() {
        copy_dir_all(&translate.join("c_src"), &verify.join("c_src"))?;
    }

    let bin_dir = verify.join("src/bin");
    let success = bin_dir.is_dir() && std::fs::read_dir(&bin_dir)?.next().is_some();
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
                Ok(_) => { std::fs::copy(entry.path(), &dst_path)?; }
                Err(_) => continue, // dangling symlink
            }
        } else if ft.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
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
                Ok(_) => { std::fs::copy(entry.path(), &dst_path)?; }
                Err(_) => continue, // dangling symlink, skip
            }
        } else if ft.is_dir() {
            if skip.iter().any(|s| *s == &*name_str) {
                continue;
            }
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
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
        let tmp = tempfile::Builder::new()
            .prefix("harvest-work-")
            .tempdir()
            .context("creating isolated work dir")?;
        let src = case_dir.join("translated_rust");
        if src.is_dir() {
            copy_dir_filtered(&src, &tmp.path().join("translated_rust"), &["target"])?;
        }
        Ok(Self { tmp, dest: case_dir.to_owned(), finished: false })
    }

    /// Path the agent should work in.
    pub fn translated_rust(&self) -> PathBuf {
        self.tmp.path().join("translated_rust")
    }

    /// Path to the temp root (for setting current_dir).
    pub fn root(&self) -> &Path {
        self.tmp.path()
    }

    /// Copy results back to the case dir. Consumes self.
    /// Skips target/ and c_src/ (kept from original).
    pub fn finish(mut self) -> Result<()> {
        let dst = self.dest.join("translated_rust");
        // Copy back everything except target/ and c_src/ (those stay from original)
        copy_dir_filtered(
            &self.tmp.path().join("translated_rust"),
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
    let translated = case_dir.join("translated_rust");
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

    // Locate c2rust source (sibling directory under results/Test-Corpus/)
    let c2rust_original = paths.results_dir
        .parent().context("no parent for results_dir")?
        .join("c2rust").join(battery).join(name).join("translated_rust_original");
    anyhow::ensure!(c2rust_original.is_dir(),
        "c2rust translated_rust_original not found: {}", c2rust_original.display());

    let case_dir = paths.case_dir(battery, name);
    if case_dir.exists() { std::fs::remove_dir_all(&case_dir)?; }
    let logs_dir = case_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = logs_dir.join("translation.log");
    let translated = case_dir.join("translated_rust");

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
