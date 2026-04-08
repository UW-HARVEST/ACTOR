use crate::battery::{self, Case, Paths};
use crate::cargo_toml::{self, CargoToml};
use crate::cli::Agent;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

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

    let prompt_text = match paths.agent {
        Agent::C2rust => String::new(),
        Agent::Claude => std::fs::read_to_string(paths.prompts_dir.join("translate.md")).unwrap_or_default(),
        Agent::Kiro => {
            let f = if case.is_lib { "library.md" } else { "executable.md" };
            std::fs::read_to_string(paths.prompts_dir.join(f)).unwrap_or_default()
        }
    };

    let start = Instant::now();
    match translate_case(paths, battery_name, &case.name, &prompt_text) {
        Ok(()) => {
            let elapsed = start.elapsed().as_secs();
            write_translation_metrics(&output_dir.join(&case.name), paths.agent, elapsed, true);
            let _ = post_process_independent(paths, battery_name, &case.name, case.is_lib);
            let _ = save_original(output_dir, &case.name);
            CaseResult { name: case.name.clone(), elapsed_secs: elapsed, success: true, error: None, skipped: false }
        }
        Err(e) => {
            let elapsed = start.elapsed().as_secs();
            write_translation_metrics(&output_dir.join(&case.name), paths.agent, elapsed, false);
            CaseResult { name: case.name.clone(), elapsed_secs: elapsed, success: false, error: Some(e.to_string()), skipped: false }
        }
    }
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

    let prompt_text = match paths.agent {
        Agent::C2rust => String::new(),
        Agent::Claude => std::fs::read_to_string(paths.prompts_dir.join("translate.md")).unwrap_or_default(),
        Agent::Kiro => std::fs::read_to_string(paths.prompts_dir.join("configurable.md")).unwrap_or_default(),
    };

    println!("Translating: {} (shared-source, {} configs)", group.real_case, group.configs.len());
    let start = Instant::now();
    match translate_case(paths, battery_name, &group.real_case, &prompt_text) {
        Ok(()) => {
            let elapsed = start.elapsed().as_secs();
            write_translation_metrics(&real_dir, paths.agent, elapsed, true);
            if let Ok(mut cargo) = CargoToml::open(&real_dir.join("translated_rust/Cargo.toml")) {
                cargo.add_workspace();
                let _ = cargo.save();
            }
            let _ = save_original(output_dir, &group.real_case);
            CaseResult { name: group.real_case.clone(), elapsed_secs: elapsed, success: true, error: None, skipped: false }
        }
        Err(e) => {
            let elapsed = start.elapsed().as_secs();
            write_translation_metrics(&real_dir, paths.agent, elapsed, false);
            CaseResult { name: group.real_case.clone(), elapsed_secs: elapsed, success: false, error: Some(e.to_string()), skipped: false }
        }
    }
}

// ── Preflight ──────────────────────────────────────────────────────────

fn preflight_check(agent: Agent) -> Result<()> {
    let (cmd, version_args): (&str, &[&str]) = match agent {
        Agent::Kiro => ("kiro-cli", &["--version"]),
        Agent::Claude => ("claude", &["--version"]),
        Agent::C2rust => ("c2rust", &["--version"]),
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
        Agent::Kiro | Agent::Claude | Agent::C2rust => {
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
        Agent::Kiro => {
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
            let _status = Command::new("bash")
                .arg("-lc")
                .arg(r#"set -o pipefail; timeout 10800 claude -p "$1" --allowedTools 'Bash(*)' 'Write' 'Edit' --max-turns 50 --verbose --output-format stream-json < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(prompt)
                .arg(&log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(&work_dir)
                .status()
                .context("invoking claude")?;
        }
        Agent::C2rust => {
            c2rust_translate(&work_dir, &log_path)?;
        }
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

fn propagate_config(
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

    // Blind mode: remove test files so agent never sees them
    if matches!(mode, ScaffoldMode::Blind) {
        let bin_dir = work.join("src/bin");
        if bin_dir.is_dir() {
            std::fs::remove_dir_all(&bin_dir)?;
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
        Agent::Kiro => {
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
            Command::new("bash")
                .arg("-lc")
                .arg(r#"set -o pipefail; timeout 10800 claude -p "$1" --allowedTools 'Bash(*)' 'Write' 'Edit' --max-turns 50 --verbose --output-format stream-json < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(prompt)
                .arg(log_path)
                .current_dir(work)
                .status()
                .context("invoking claude for CRUST")?;
        }
        Agent::C2rust => anyhow::bail!("c2rust not supported for CRUST-bench"),
    }
    Ok(())
}

pub fn run_crust(paths: &Paths, projects: &[battery::CrustProject], parallel: usize) -> Result<()> {
    run_crust_with_mode(paths, projects, parallel, ScaffoldMode::Standard, "crust.md")
}

pub fn run_crust_blind(paths: &Paths, projects: &[battery::CrustProject], parallel: usize) -> Result<()> {
    run_crust_with_mode(paths, projects, parallel, ScaffoldMode::Blind, "crust_blind.md")
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

    let prompt = std::fs::read_to_string(paths.prompts_dir.join("crust_verify.md"))
        .context("reading crust_verify.md")?;

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
