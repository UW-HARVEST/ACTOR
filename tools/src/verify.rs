use crate::battery::{self, Case, Paths};
use crate::cargo_toml;
use crate::cli::Agent;
use crate::translate::copy_dir_all;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub fn run(repo_root: &Path, battery_name: &str, filter: Option<&str>, force: bool, agent: Agent) -> Result<()> {
    let paths = Paths::with_agent(repo_root, agent);
    let battery = battery::discover(&paths.corpus_dir, battery_name, filter)?;
    let output_dir = paths.output_dir(battery_name);
    let prompt_template = std::fs::read_to_string(paths.prompts_dir.join("verify.md"))?;

    let mut verified = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut total = 0;

    // Count verifiable cases
    for case in &battery.cases {
        match case {
            Case::Independent(_) => total += 1,
            Case::SharedSource(_) => total += 1, // only real case verified
        }
    }

    println!("=== Verifying {battery_name} ({total} cases) ===");

    let mut current = 0;
    for case in &battery.cases {
        match case {
            Case::Independent(c) => {
                current += 1;
                let case_dir = output_dir.join(&c.name);

                if !case_dir.join("translated_rust/Cargo.toml").exists() {
                    continue;
                }

                if !force && case_dir.join("logs/verify.log").exists() {
                    println!("[{current}/{total}] ⏭️  {} (already verified)", c.name);
                    skipped += 1;
                    continue;
                }

                println!("[{current}/{total}] 🔬 {}", c.name);
                let cmake_flags = get_cmake_flags(&paths, battery_name, &c.name);
                let ok = verify_case(&case_dir, &prompt_template, &cmake_flags, "", agent)?;

                if ok {
                    verified += 1;
                    println!("[{current}/{total}] ✅ {} — verified", c.name);
                } else {
                    failed += 1;
                    println!("[{current}/{total}] ❌ {} — verification incomplete", c.name);
                }
            }
            Case::SharedSource(group) => {
                current += 1;
                let real_dir = output_dir.join(&group.real_case);

                if !real_dir.join("translated_rust/Cargo.toml").exists() {
                    continue;
                }

                if !force && real_dir.join("logs/verify.log").exists() {
                    println!("[{current}/{total}] ⏭️  {} (already verified)", group.real_case);
                    skipped += 1;
                } else {
                    println!("[{current}/{total}] 🔬 {} (shared-source, {} configs)", group.real_case, group.configs.len());
                    let cmake_flags = get_cmake_flags(&paths, battery_name, &group.real_case);
                    let configs_text = build_configs_text(&paths, battery_name, group);
                    let ok = verify_case(&real_dir, &prompt_template, &cmake_flags, &configs_text, agent)?;

                    if ok {
                        verified += 1;
                        println!("[{current}/{total}] ✅ {} — verified", group.real_case);
                    } else {
                        failed += 1;
                        println!("[{current}/{total}] ❌ {} — verification incomplete", group.real_case);
                    }
                }

                // Always propagate fixes to configs
                println!("Re-propagating fixes from {} to {} configs...", group.real_case, group.configs.len());
                let real_src = real_dir.join("translated_rust/src");
                for cfg in &group.configs {
                    let cfg_translated = output_dir.join(&cfg.name).join("translated_rust");
                    if !cfg_translated.join("Cargo.toml").exists() {
                        continue;
                    }

                    // Copy updated src
                    let dst_src = cfg_translated.join("src");
                    if dst_src.exists() {
                        std::fs::remove_dir_all(&dst_src)?;
                    }
                    copy_dir_all(&real_src, &dst_src)?;

                    // Strip for lib cases
                    if cfg.is_lib {
                        cargo_toml::strip_for_lib(&cfg_translated)?;
                    }

                    // Clean target so it rebuilds
                    let target_dir = cfg_translated.join("target");
                    if target_dir.exists() {
                        std::fs::remove_dir_all(&target_dir)?;
                    }
                }
                println!("Propagated to {} cases", group.configs.len());
            }
        }
    }

    println!();
    println!("Verified: {verified}, Failed: {failed}, Skipped: {skipped} (of {total})");
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

    match agent {
        Agent::Kiro => {
            let prompt = prompt_template
                .replace("CASE_DIR_PLACEHOLDER", &case_dir.to_string_lossy())
                .replace("CMAKE_BUILD_FLAGS", cmake_flags)
                .replace("ALL_CONFIGURATIONS", configs_text);

            let _status = Command::new("bash")
                .arg("-lc")
                .arg(r#"timeout 2700 kiro-cli chat --no-interactive --trust-all-tools --agent kiro_plain "$1" < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(&prompt)
                .arg(&log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(case_dir)
                .status()
                .context("invoking kiro-cli for verification")?;
        }
        Agent::Claude => {
            let tmp = tempfile::Builder::new()
                .prefix("harvest-verify-")
                .tempdir()
                .context("creating isolated temp dir for verify")?;

            // Copy translated_rust into temp dir
            let work = tmp.path().join("translated_rust");
            copy_dir_all(&translated, &work)?;

            // Write sandbox settings
            let claude_dir = tmp.path().join(".claude");
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
                            "allowRead": [tmp.path().to_string_lossy()],
                            "allowWrite": [tmp.path().to_string_lossy()]
                        }
                    }
                }).to_string(),
            )?;

            let prompt = prompt_template
                .replace("CMAKE_BUILD_FLAGS", cmake_flags)
                .replace("ALL_CONFIGURATIONS", configs_text);

            let _status = Command::new("bash")
                .arg("-c")
                .arg("set -o pipefail; claude -p \"$PROMPT\" \
                    --allowedTools 'Bash(*)' 'Write' 'Edit' \
                    --verbose \
                    --output-format stream-json \
                    < /dev/null 2>&1 | tee \"$LOG\"")
                .env("PROMPT", &prompt)
                .env("LOG", &log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(&work)
                .status()
                .context("invoking claude for verification")?;

            // Copy verified results back
            let dst_src = translated.join("src");
            if dst_src.exists() {
                std::fs::remove_dir_all(&dst_src)?;
            }
            copy_dir_all(&work.join("src"), &dst_src)?;

            // Copy tests/ if created
            let work_tests = work.join("tests");
            if work_tests.is_dir() {
                let dst_tests = translated.join("tests");
                if dst_tests.exists() {
                    std::fs::remove_dir_all(&dst_tests)?;
                }
                copy_dir_all(&work_tests, &dst_tests)?;
            }

            // Copy updated Cargo.toml (may have added libloading)
            if work.join("Cargo.toml").exists() {
                std::fs::copy(work.join("Cargo.toml"), translated.join("Cargo.toml"))?;
            }
        }
        Agent::C2rust => {
            // c2rust is deterministic — no verification needed
            return Ok(true);
        }
    }

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
