use crate::battery::{self, Case, Paths};
use crate::cli::Agent;
use crate::translate::{IsolatedWorkDir, Semaphore};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

pub fn run(repo_root: &Path, paths: &Paths, battery_name: &str, filter: Option<&str>, force: bool, parallel: usize) -> Result<()> {
    let sem = Arc::new(Semaphore::new(parallel));
    run_with_semaphore(repo_root, paths, battery_name, filter, force, &sem)
}

pub fn run_all(repo_root: &Path, paths: &Paths, batteries: &[String], force: bool, parallel: usize) -> Result<()> {
    let sem = Arc::new(Semaphore::new(parallel));

    let errors: Vec<anyhow::Error> = std::thread::scope(|s| {
        let handles: Vec<_> = batteries.iter().map(|bat| {
            let sem = sem.clone();
            s.spawn(move || -> Result<()> {
                run_with_semaphore(repo_root, paths, bat, None, force, &sem)
            })
        }).collect();

        handles.into_iter().filter_map(|h| match h.join() {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some(anyhow::anyhow!("verify thread panicked")),
        }).collect()
    });

    if let Some(first) = errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}

fn run_with_semaphore(repo_root: &Path, paths: &Paths, battery_name: &str, filter: Option<&str>, force: bool, sem: &Arc<Semaphore>) -> Result<()> {
    let battery = battery::discover(&paths.corpus_dir, battery_name, filter)?;
    let output_dir = paths.output_dir(battery_name);
    let prompt_template = std::fs::read_to_string(paths.prompts_dir.join("verify.md"))?;

    // Split into independent (parallelizable) and shared-source (sequential)
    let mut independent: Vec<&battery::IndependentCase> = Vec::new();
    let mut shared: Vec<&battery::SharedSourceGroup> = Vec::new();
    for case in &battery.cases {
        match case {
            Case::Independent(c) => independent.push(c),
            Case::SharedSource(g) => shared.push(g),
        }
    }
    let total = independent.len() + shared.len();
    println!("=== Verifying {battery_name} ({total} cases) ===");

    // ── Parallel: independent cases ────────────────────────────────────
    let ind_results: Vec<(String, Option<bool>)> = std::thread::scope(|s| {
        let handles: Vec<_> = independent.iter().map(|c| {
            s.spawn(|| {
                let _permit = sem.acquire();
                let case_dir = output_dir.join(&c.name);
                if !crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED).join("Cargo.toml").exists() {
                    return (c.name.clone(), None);
                }
                if !force && crate::battery::phase_dir(&case_dir, crate::battery::VERIFIED).join("logs/verify.log").exists() {
                    return (c.name.clone(), None); // skipped
                }
                let cmake_flags = get_cmake_flags(paths, battery_name, &c.name);
                let ok = verify_case(&case_dir, &prompt_template, &cmake_flags, "", paths.agent)
                    .unwrap_or(false);
                (c.name.clone(), Some(ok))
            })
        }).collect();
        handles.into_iter().map(|h| h.join().expect("verify thread panicked")).collect()
    });

    let mut verified = 0usize;
    let mut failed = 0usize;
    let mut current = 0usize;
    for (name, result) in &ind_results {
        current += 1;
        match result {
            None => println!("[{current}/{total}] ⏭️  {name} (skipped)"),
            Some(true) => { verified += 1; println!("[{current}/{total}] ✅ {name}"); }
            Some(false) => { failed += 1; println!("[{current}/{total}] ❌ {name}"); }
        }
    }

    // ── Sequential: shared-source groups ───────────────────────────────
    for group in &shared {
        current += 1;
        let real_dir = output_dir.join(&group.real_case);

        if !crate::battery::phase_dir(&real_dir, crate::battery::TRANSLATED).join("Cargo.toml").exists() {
            continue;
        }

        if !force && crate::battery::phase_dir(&real_dir, crate::battery::VERIFIED).join("logs/verify.log").exists() {
            println!("[{current}/{total}] ⏭️  {} (already verified)", group.real_case);
        } else {
            println!("[{current}/{total}] 🔬 {} (shared-source, {} configs)", group.real_case, group.configs.len());
            let cmake_flags = get_cmake_flags(paths, battery_name, &group.real_case);
            let configs_text = build_configs_text(paths, battery_name, group);
            let ok = verify_case(&real_dir, &prompt_template, &cmake_flags, &configs_text, paths.agent)?;

            if ok { verified += 1; println!("[{current}/{total}] ✅ {} — verified", group.real_case); }
            else { failed += 1; println!("[{current}/{total}] ❌ {} — verification incomplete", group.real_case); }
        }

        // Always re-propagate the real case's VERIFIED crate to each config
        // follower, so every config carries the post-verify fixes (this is what
        // lets runtests score all N configs as verified, not just the real one).
        println!("Re-propagating verified fixes from {} to {} configs...", group.real_case, group.configs.len());
        for cfg in &group.configs {
            crate::translate::propagate_config_phase(
                paths, battery_name, &group.real_case, cfg, crate::battery::VERIFIED,
            )?;
        }
        println!("Propagated to {} cases", group.configs.len());
    }

    println!();
    println!("Verified: {verified}, Failed: {failed} (of {total})");
    Ok(())
}

/// Run the C-as-oracle verify phase over harvest-bench projects. Reuses the
/// EXACT same shared prompts/claude/verify.md and verify_case mechanics as
/// Test-Corpus — same libloading differential + Phase A/B/C/D + subagent
/// protocol — so both benchmarks receive the same verification rigor. HB has
/// no per-project cmake flags or configs, so those are empty.
pub fn run_harvest_bench(paths: &Paths, projects: &[battery::HarvestBenchProject], parallel: usize, force: bool) -> Result<()> {
    let sem = Arc::new(Semaphore::new(parallel));
    let prompt_template = std::fs::read_to_string(paths.prompts_dir.join("verify.md"))
        .context("reading verify.md")?;

    let total = projects.len();
    println!("=== Verifying harvest-bench ({total} projects) ===");

    let results: Vec<(String, Option<bool>)> = std::thread::scope(|s| {
        let handles: Vec<_> = projects.iter().map(|p| {
            let sem = sem.clone();
            let prompt = &prompt_template;
            s.spawn(move || {
                let _permit = sem.acquire();
                let name = p.name().to_string();
                let case_dir = paths.output_dir(&name);
                if !crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED).join("Cargo.toml").exists() {
                    return (name, None); // no translated crate yet
                }
                if !force && crate::battery::phase_dir(&case_dir, crate::battery::VERIFIED).join("logs/verify.log").exists() {
                    return (name, None); // skip: already verified
                }
                let ok = verify_case(&case_dir, prompt, "", "", paths.agent).unwrap_or(false);
                (name, Some(ok))
            })
        }).collect();
        handles.into_iter().map(|h| h.join().expect("verify thread panicked")).collect()
    });

    let (mut verified, mut failed) = (0usize, 0usize);
    for (i, (name, result)) in results.iter().enumerate() {
        let n = i + 1;
        match result {
            None => println!("[{n}/{total}] ⏭️  {name} (skipped: no translated/ or already verified)"),
            Some(true) => { verified += 1; println!("[{n}/{total}] ✅ {name}"); }
            Some(false) => { failed += 1; println!("[{n}/{total}] ❌ {name}"); }
        }
    }
    println!("\nHB verify: {verified}/{total} verified, {failed} failed");
    Ok(())
}

fn verify_case(case_dir: &Path, prompt_template: &str, cmake_flags: &str, configs_text: &str, agent: Agent) -> Result<bool> {
    // Verify is PURE: it reads the immutable `translated/` crate (via
    // IsolatedWorkDir), works in a temp dir, and writes the result to
    // `verified/`. It never mutates `translated/`, so no snapshot/restore is
    // needed. The verify log lives in `verified/logs/`.
    let verified_logs = crate::battery::phase_dir(case_dir, crate::battery::VERIFIED).join("logs");
    std::fs::create_dir_all(&verified_logs)?;
    let log_path = verified_logs.join("verify.log");
    let openssl_dir = std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into());

    // Work in an isolated temp dir seeded from translated/ — the agent sees no
    // config-specific path names, and C-as-oracle verification uses only the
    // crate's own c_src (test_vectors/runner never enter the temp workspace).
    let work = IsolatedWorkDir::new(case_dir)?;

    // Capture the verify agent's process exit exactly like translate does — no
    // double standard. Cleared here so a skipped/absent CLI run records nothing.
    crate::translate::clear_agent_exit();
    let start = std::time::Instant::now();

    let prompt = prompt_template
        .replace("CASE_DIR_PLACEHOLDER", &work.root().to_string_lossy())
        .replace("CMAKE_BUILD_FLAGS", cmake_flags)
        .replace("ALL_CONFIGURATIONS", configs_text);

    match agent {
        Agent::Kiro => {
            let status = Command::new("bash")
                .arg("-lc")
                .arg(r#"timeout 2700 kiro-cli chat --no-interactive --trust-all-tools --agent kiro_plain "$1" < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(&prompt)
                .arg(&log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(work.root())
                .status()
                .context("invoking kiro-cli for verification")?;
            crate::translate::record_agent_exit(status);
        }
        Agent::Claude => {
            // Write sandbox settings in temp dir
            let claude_dir = work.root().join(".claude");
            std::fs::create_dir_all(&claude_dir)?;
            let repo_root = case_dir.ancestors().nth(2).unwrap_or(Path::new("/"));
            std::fs::write(
                claude_dir.join("settings.json"),
                serde_json::json!({
                    "sandbox": {
                        "enabled": true,
                        "allowUnsandboxedCommands": false,
                        "filesystem": {
                            "denyRead": [repo_root.to_string_lossy()],
                            "allowRead": [work.root().to_string_lossy()],
                            "allowWrite": [work.root().to_string_lossy()]
                        }
                    }
                }).to_string(),
            )?;

            let settings_path = claude_dir.join("settings.json");
            let status = Command::new("bash")
                .arg("-c")
                .arg("set -o pipefail; timeout 10800 claude -p \"$PROMPT\" \
                    --strict-mcp-config --disable-slash-commands --settings \"$SETTINGS\" \
                    --agents \"$AGENTS\" --agent claude_plain \
                    --max-turns 1000 --permission-mode bypassPermissions \
                    --verbose \
                    --output-format stream-json \
                    < /dev/null 2>&1 | tee \"$LOG\"")
                .env("PROMPT", &prompt)
                .env("LOG", &log_path)
                .env("SETTINGS", &settings_path)
                .env("AGENTS", crate::translate::CLAUDE_PLAIN_AGENT_JSON)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(work.translated_rust())
                .status()
                .context("invoking claude for verification")?;
            crate::translate::record_agent_exit(status);
        }
        Agent::C2rust | Agent::Laertes | Agent::C2SaferRust | Agent::SmartC2Rust | Agent::Kimi | Agent::Oneshot | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask | Agent::ClaudeCrossPrompt | Agent::CodexGpt55 | Agent::CodexGpt54 => {
            // ClaudeCombined: translate phase already did verify, skip this phase.
            // ClaudeMinimal: no verify phase (calibration baseline).
            // ClaudeNoIter: no verify phase (E3 prompt-sensitivity ablation).
            // ClaudeNoFeatures: no verify phase (E2 prompt-sensitivity ablation).
            // ClaudeNoSubtask: no verify phase (E6 prompt-sensitivity ablation).
            // ClaudeCrossPrompt: no verify phase (E4 prompt-sensitivity ablation).
            // Codex: skip verify; the agent over-fixates on irrelevant linker
            // symbols during C-as-oracle verification (model-specific behavior).
            // c2rust/laertes/kimi/oneshot: no verify phase by design.
            return Ok(true);
        }
    }

    // Copy verified results back (skips target/ and c_src/)
    work.finish()?;

    // ── Compile-gate: verify only counts as success if the crate still builds.
    // A mid-response API error can leave the crate half-written (missing symbols,
    // unresolved imports). Recording such a broken crate as "verified" would then
    // make the scorer build+score garbage. Better: detect the break, discard
    // verified/, and let the scorer fall back to the (less complete but compilable)
    // translated/ crate. The verify log is preserved for debugging.
    let verified_dir = crate::battery::phase_dir(case_dir, crate::battery::VERIFIED);
    let check = Command::new("timeout")
        .args(["120", "cargo", "check"])
        .current_dir(&verified_dir)
        .output();
    let compiles = check.map_or(false, |o| o.status.success());
    if !compiles {
        eprintln!("  ⚠️  verify produced a non-compiling crate — discarding verified/, scorer will use translated/");
        // Keep the log for post-mortem; remove the broken crate so crate_dir() falls back.
        let logs_backup = verified_dir.join("logs");
        let logs_tmp = case_dir.join("_verify_logs_backup");
        if logs_backup.is_dir() { let _ = std::fs::rename(&logs_backup, &logs_tmp); }
        let _ = std::fs::remove_dir_all(&verified_dir);
        // Restore just the logs dir under verified/ (so the log is still findable).
        if logs_tmp.is_dir() {
            let _ = std::fs::create_dir_all(&verified_dir);
            let _ = std::fs::rename(&logs_tmp, &verified_dir.join("logs"));
        }
    }

    // Record verify metrics (incl. agent process exit) alongside verify.log,
    // mirroring translate's translation.json — no double standard.
    crate::translate::write_verification_metrics(&verified_dir, agent, start.elapsed().as_secs(), compiles);
    Ok(compiles)
}

/// Build a text block listing all distinct configurations for the verify prompt.
fn build_configs_text(paths: &Paths, battery: &str, group: &battery::SharedSourceGroup) -> String {
    // Collect unique feature sets (deduplicate configs that share the same features)
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();

    // Include the real case first
    let real_flags = get_cmake_flags(paths, battery, &group.real_case);
    let real_presets = paths.input_dir(battery).join(&group.real_case).join("CMakePresets.json");
    let real_features = battery::extract_features_from_path(&real_presets).unwrap_or_default();
    let real_key: Vec<String> = real_features.iter().cloned().collect();
    if seen.insert(real_key) && !real_flags.is_empty() {
        lines.push(format!(
            "  cmake: {}  →  cargo features: {}",
            real_flags,
            real_features.join(","),
        ));
    }

    for cfg in &group.configs {
        let key: Vec<String> = cfg.features.clone();
        if !seen.insert(key) {
            continue; // skip duplicate feature sets
        }
        let cmake_flags = get_cmake_flags(paths, battery, &cfg.name);
        if cmake_flags.is_empty() {
            continue;
        }
        lines.push(format!(
            "  cmake: {}  →  cargo features: {}",
            cmake_flags,
            cfg.features.join(","),
        ));
    }

    if lines.is_empty() {
        String::new()
    } else {
        format!("Configurations to test:\n{}", lines.join("\n"))
    }
}

fn get_cmake_flags(paths: &Paths, battery: &str, case_name: &str) -> String {
    let presets = paths.input_dir(battery).join(case_name).join("CMakePresets.json");
    if !presets.exists() {
        return String::new();
    }
    let Ok(content) = std::fs::read_to_string(&presets) else {
        return String::new();
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) else {
        return String::new();
    };
    let Some(cv) = data.pointer("/configurePresets/1/cacheVariables").and_then(|v| v.as_object()) else {
        return String::new();
    };
    cv.iter()
        .filter(|(k, _)| *k != "CMAKE_C_STANDARD" && *k != "CMAKE_BUILD_TYPE")
        .map(|(k, v)| format!("-D{}={}", k, v.as_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join(" ")
}
