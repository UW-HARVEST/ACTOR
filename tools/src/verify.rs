use crate::battery::{self, Case, Paths};
use crate::cli::Agent;
use crate::translate::{copy_dir_all, IsolatedWorkDir, Semaphore};
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
                if !case_dir.join("translated_rust/Cargo.toml").exists() {
                    return (c.name.clone(), None);
                }
                if !force && case_dir.join("logs/verify.log").exists() {
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

        if !real_dir.join("translated_rust/Cargo.toml").exists() {
            continue;
        }

        if !force && real_dir.join("logs/verify.log").exists() {
            println!("[{current}/{total}] ⏭️  {} (already verified)", group.real_case);
        } else {
            println!("[{current}/{total}] 🔬 {} (shared-source, {} configs)", group.real_case, group.configs.len());
            let cmake_flags = get_cmake_flags(paths, battery_name, &group.real_case);
            let configs_text = build_configs_text(paths, battery_name, group);
            let ok = verify_case(&real_dir, &prompt_template, &cmake_flags, &configs_text, paths.agent)?;

            if ok { verified += 1; println!("[{current}/{total}] ✅ {} — verified", group.real_case); }
            else { failed += 1; println!("[{current}/{total}] ❌ {} — verification incomplete", group.real_case); }
        }

        // Always propagate fixes to configs
        println!("Re-propagating fixes from {} to {} configs...", group.real_case, group.configs.len());
        for cfg in &group.configs {
            crate::translate::propagate_config(paths, battery_name, &group.real_case, cfg)?;
        }
        println!("Propagated to {} cases", group.configs.len());
    }

    println!();
    println!("Verified: {verified}, Failed: {failed} (of {total})");
    Ok(())
}

fn verify_case(case_dir: &Path, prompt_template: &str, cmake_flags: &str, configs_text: &str, agent: Agent) -> Result<bool> {
    let translated = case_dir.join("translated_rust");
    let original = case_dir.join("translated_rust_original");

    // Restore from original (clean slate)
    if original.is_dir() {
        if translated.exists() {
            std::fs::remove_dir_all(&translated)?;
        }
        copy_dir_all(&original, &translated)?;
    }

    // Remove test_vectors and runner — verify must use C-as-oracle only
    let tv = case_dir.join("test_vectors");
    let runner = case_dir.join("runner");
    if tv.exists() { std::fs::remove_dir_all(&tv)?; }
    if runner.exists() { std::fs::remove_dir_all(&runner)?; }

    let logs_dir = case_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let log_path = logs_dir.join("verify.log");
    let openssl_dir = std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into());

    // Work in an isolated temp dir — agent sees no config-specific path names
    let work = IsolatedWorkDir::new(case_dir)?;

    let prompt = prompt_template
        .replace("CASE_DIR_PLACEHOLDER", &work.root().to_string_lossy())
        .replace("CMAKE_BUILD_FLAGS", cmake_flags)
        .replace("ALL_CONFIGURATIONS", configs_text);

    match agent {
        Agent::Kiro | Agent::KiroTranslate => {
            let _status = Command::new("bash")
                .arg("-lc")
                .arg(r#"timeout 2700 kiro-cli chat --no-interactive --trust-all-tools --agent kiro_plain "$1" < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(&prompt)
                .arg(&log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(work.root())
                .status()
                .context("invoking kiro-cli for verification")?;
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
            let _status = Command::new("bash")
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
        }
        Agent::C2rust | Agent::Laertes | Agent::Kimi | Agent::Oneshot | Agent::ClaudeCombined | Agent::ClaudeMinimal | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask => {
            // ClaudeCombined: translate phase already did verify, skip this phase.
            // ClaudeMinimal: no verify phase (calibration baseline).
            // ClaudeNoIter: no verify phase (E3 prompt-sensitivity ablation).
            // ClaudeNoFeatures: no verify phase (E2 prompt-sensitivity ablation).
            // ClaudeNoSubtask: no verify phase (E6 prompt-sensitivity ablation).
            // c2rust/laertes/kimi/oneshot: no verify phase by design.
            return Ok(true);
        }
    }

    // Copy verified results back (skips target/ and c_src/)
    work.finish()?;
    Ok(true)
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
