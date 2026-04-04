use crate::battery::{self, Case, Paths};
use crate::cargo_toml::{self, CargoToml};
use crate::cli::Agent;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

pub fn run(repo_root: &Path, battery_name: &str, filter: Option<&str>, agent: Agent) -> Result<()> {
    let paths = Paths::with_agent(repo_root, agent);

    // Pre-flight: verify the agent CLI is available and print its version
    preflight_check(agent)?;

    let battery = battery::discover(&paths.corpus_dir, battery_name, filter)?;
    let output_dir = paths.output_dir(battery_name);
    std::fs::create_dir_all(&output_dir)?;

    let total = count_cases(&battery);
    let mut current = 0;
    let mut translated = 0;
    let mut failed = 0;

    for case in &battery.cases {
        match case {
            Case::Independent(c) => {
                current += 1;
                let case_dir = output_dir.join(&c.name);

                if case_dir.join("translated_rust/Cargo.toml").exists() {
                    println!("[{current}/{total}] ⏭️  {} (already done)", c.name);
                    translated += 1;
                    continue;
                }

                let prompt_text = match paths.agent {
                    Agent::C2rust => String::new(),
                    Agent::Claude => std::fs::read_to_string(paths.prompts_dir.join("translate.md"))?,
                    Agent::Kiro => {
                        let f = if c.is_lib { "library.md" } else { "executable.md" };
                        std::fs::read_to_string(paths.prompts_dir.join(f))?
                    }
                };

                let start = Instant::now();
                match translate_case(&paths, battery_name, &c.name, &prompt_text) {
                    Ok(()) => {
                        let elapsed = start.elapsed().as_secs();
                        write_translation_metrics(&output_dir.join(&c.name), paths.agent, elapsed, true);
                        post_process_independent(&paths, battery_name, &c.name, c.is_lib)?;
                        save_original(&output_dir, &c.name)?;
                        translated += 1;
                        println!("  ✅ {} ({elapsed}s) [{translated} translated, {failed} failed of {current}/{total}]", c.name);
                    }
                    Err(e) => {
                        let elapsed = start.elapsed().as_secs();
                        write_translation_metrics(&output_dir.join(&c.name), paths.agent, elapsed, false);
                        failed += 1;
                        println!("  ❌ {} — {e} ({elapsed}s) [{translated} translated, {failed} failed of {current}/{total}]", c.name);
                    }
                }
            }
            Case::SharedSource(group) => {
                current += 1;
                let real_dir = output_dir.join(&group.real_case);

                if real_dir.join("translated_rust/Cargo.toml").exists() {
                    println!("[{current}/{total}] ⏭️  {} (already done)", group.real_case);
                    translated += 1;
                } else {
                    let prompt_text = match paths.agent {
                        Agent::C2rust => String::new(),
                        Agent::Claude => std::fs::read_to_string(paths.prompts_dir.join("translate.md"))?,
                        Agent::Kiro => std::fs::read_to_string(paths.prompts_dir.join("configurable.md"))?,
                    };

                    println!("[{current}/{total}] Translating: {} (shared-source, {} configs)", group.real_case, group.configs.len());
                    let start = Instant::now();
                    match translate_case(&paths, battery_name, &group.real_case, &prompt_text) {
                        Ok(()) => {
                            let elapsed = start.elapsed().as_secs();
                            write_translation_metrics(&real_dir, paths.agent, elapsed, true);
                            // Real case: add workspace, set both lib+bin (configurable prompt handles this)
                            let cargo_path = real_dir.join("translated_rust/Cargo.toml");
                            if cargo_path.exists() {
                                let mut cargo = CargoToml::open(&cargo_path)?;
                                cargo.add_workspace();
                                cargo.save()?;
                            }
                            save_original(&output_dir, &group.real_case)?;
                            translated += 1;
                            println!("  ✅ {} ({elapsed}s) [{translated} translated, {failed} failed of {current}/{total}]", group.real_case);
                        }
                        Err(e) => {
                            let elapsed = start.elapsed().as_secs();
                            write_translation_metrics(&real_dir, paths.agent, elapsed, false);
                            failed += 1;
                            println!("  ❌ {} — {e} ({elapsed}s)", group.real_case);
                            // Skip configs if real case failed
                            current += group.configs.len();
                            continue;
                        }
                    }
                }

                // Propagate to each config
                for cfg in &group.configs {
                    current += 1;
                    let cfg_dir = output_dir.join(&cfg.name);

                    if cfg_dir.join("translated_rust/Cargo.toml").exists() {
                        println!("[{current}/{total}] ⏭️  {} (already done)", cfg.name);
                        translated += 1;
                        continue;
                    }

                    match propagate_config(&paths, battery_name, &group.real_case, cfg) {
                        Ok(()) => {
                            save_original(&output_dir, &cfg.name)?;
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
        }
    }

    println!();
    println!("Done: {translated}/{total} translated, {failed} failed");
    Ok(())
}

/// Verify the agent CLI is on PATH and print its version.
fn preflight_check(agent: Agent) -> Result<()> {
    let (cmd, version_args): (&str, &[&str]) = match agent {
        Agent::Kiro => ("kiro-cli", &["--version"]),
        Agent::Claude => ("claude", &["--version"]),
        Agent::C2rust => ("c2rust", &["--version"]),
    };

    let output = Command::new("bash")
        .arg("-c")
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

    // For Claude, enforce minimum version 2.1
    if agent == Agent::Claude {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Version line looks like: "2.1.89 (Claude Code)"
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
                 The subprocess may be using a different binary than your shell.\n\
                 Subprocess resolved: {}",
                stdout.lines().next().unwrap_or("unknown"),
            );
        }
    }

    Ok(())
}

fn translate_case(paths: &Paths, battery: &str, name: &str, prompt: &str) -> Result<()> {
    let case_dir = paths.case_dir(battery, name);

    // Clean any stale artifacts from previous runs (test_vectors, result.json, etc.)
    if case_dir.exists() {
        std::fs::remove_dir_all(&case_dir)?;
    }

    let logs_dir = case_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;

    // Copy C source into the work directory
    let input_test_case = paths.input_dir(battery).join(name).join("test_case");
    let log_path = logs_dir.join("translation.log");
    let openssl_dir = std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into());

    // For Claude: run in an isolated temp dir so the agent can't access
    // test vectors, existing results, or other repo contents.
    // For Kiro: run in-place (existing behavior).
    let (work_dir, _tmp_guard) = match paths.agent {
        Agent::Kiro => {
            let translated = case_dir.join("translated_rust");
            let c_src = translated.join("c_src");
            std::fs::create_dir_all(&c_src)?;
            copy_dir_all(&input_test_case, &c_src)?;
            (translated, None)
        }
        Agent::Claude | Agent::C2rust => {
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
            let status = Command::new("bash")
                .arg("-c")
                .arg(format!(
                    "set -o pipefail; timeout 1800 kiro-cli chat --no-interactive --trust-all-tools \"$PROMPT\" < /dev/null 2>&1 | tee \"$LOG\"",
                ))
                .env("PROMPT", prompt)
                .env("LOG", &log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(&work_dir)
                .status()
                .context("invoking kiro-cli")?;
            let _ = status; // LLM may produce Cargo.toml even on non-zero exit
        }
        Agent::Claude => {
            let status = Command::new("bash")
                .arg("-c")
                .arg(format!(
                    "set -o pipefail; timeout 10800 claude -p \"$PROMPT\" \
                        --allowedTools 'Bash(*)' 'Write' 'Edit' \
                        --max-turns 50 \
                        --verbose \
                        --output-format stream-json \
                        < /dev/null 2>&1 | tee \"$LOG\"",
                ))
                .env("PROMPT", prompt)
                .env("LOG", &log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(&work_dir)
                .status()
                .context("invoking claude")?;
            let _ = status;
        }
        Agent::C2rust => {
            c2rust_translate(&work_dir, &log_path)?;
        }
    };

    if !work_dir.join("Cargo.toml").exists() {
        anyhow::bail!("no Cargo.toml produced");
    }

    // For Claude/C2rust: move results from temp dir back to the real output location
    if matches!(paths.agent, Agent::Claude | Agent::C2rust) {
        let translated = case_dir.join("translated_rust");
        if translated.exists() {
            std::fs::remove_dir_all(&translated)?;
        }
        // Copy rather than rename (temp dir may be on different filesystem)
        copy_dir_all(&work_dir, &translated)?;
    }

    Ok(())
}

fn post_process_independent(paths: &Paths, battery: &str, name: &str, is_lib: bool) -> Result<()> {
    let cargo_path = paths.case_dir(battery, name).join("translated_rust/Cargo.toml");
    if !cargo_path.exists() {
        return Ok(());
    }
    let mut cargo = CargoToml::open(&cargo_path)?;
    cargo.add_workspace();

    if is_lib {
        cargo.remove_bin();
        // Extract lib name from corpus runner
        let lib_name = battery::extract_lib_name(
            &paths.input_dir(battery),
            name,
        );
        if let Some(ref ln) = lib_name {
            cargo.set_lib(ln);
        } else {
            cargo.set_lib(name);
        }
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

    // Copy src/ from real case
    let src_dst = translated.join("src");
    if src_dst.exists() {
        std::fs::remove_dir_all(&src_dst)?;
    }
    copy_dir_all(&real_dir.join("src"), &src_dst)?;

    // Copy Cargo.toml
    std::fs::copy(real_dir.join("Cargo.toml"), translated.join("Cargo.toml"))?;

    // Copy c_src if present
    let c_src_src = real_dir.join("c_src");
    if c_src_src.is_dir() {
        let c_src_dst = translated.join("c_src");
        if c_src_dst.exists() {
            std::fs::remove_dir_all(&c_src_dst)?;
        }
        copy_dir_all(&c_src_src, &c_src_dst)?;
    }

    // Set per-config features
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

/// Recursively copy a directory (equivalent to cp -a).
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("reading dir {}", src.display()))?
    {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Deterministic C-to-Rust transpilation via c2rust.
fn c2rust_translate(work_dir: &Path, log_path: &Path) -> Result<()> {
    let c_src = work_dir.join("c_src");
    let build_dir = c_src.join("build");
    std::fs::create_dir_all(&build_dir)?;

    let mut log = std::fs::File::create(log_path)?;
    use std::io::Write;

    // 1. cmake with compile_commands.json
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

    // 2. c2rust transpile with --emit-build-files --binary main
    let c2r_out = Command::new("c2rust")
        .args([
            "transpile",
            "--emit-build-files",
            "--binary", "main",
            &cc_json.to_string_lossy(),
            "--output-dir", &work_dir.to_string_lossy(),
        ])
        .output()
        .context("running c2rust transpile")?;
    log.write_all(&c2r_out.stdout)?;
    log.write_all(&c2r_out.stderr)?;
    if !c2r_out.status.success() {
        anyhow::bail!("c2rust transpile failed: {}", String::from_utf8_lossy(&c2r_out.stderr));
    }

    // 3. Patch the generated Cargo.toml: rename binary to "driver", add libc/f128, add [workspace]
    let cargo_path = work_dir.join("Cargo.toml");
    if cargo_path.exists() {
        let mut cargo = std::fs::read_to_string(&cargo_path)?;
        cargo = cargo.replace("name = \"main\"", "name = \"driver\"");
        cargo = cargo.replace("name = \"rust_out\"", "name = \"driver\"");
        // Fix package/lib name from output dir name
        let re = regex::Regex::new(r#"name = "translated_rust[^"]*""#).unwrap();
        cargo = re.replace_all(&cargo, r#"name = "driver""#).into_owned();
        // Also fix Rust source files that import the old crate name
        for entry in walkdir(work_dir)? {
            if entry.extension().map_or(false, |e| e == "rs") {
                let content = std::fs::read_to_string(&entry)?;
                if content.contains("translated_rust") {
                    std::fs::write(&entry, content.replace("translated_rust", "driver"))?;
                }
            }
        }
        // Add deps if missing
        if !cargo.contains("libc") {
            cargo = cargo.replace("[dependencies]", "[dependencies]\nlibc = \"0.2\"");
        }
        if !cargo.contains("[workspace]") {
            cargo.push_str("\n[workspace]\n");
        }
        std::fs::write(&cargo_path, cargo)?;
    }

    // Override rust-toolchain.toml to use latest nightly (c2rust pins an old one)
    std::fs::write(work_dir.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"nightly\"\n")?;

    // 4. Try to build
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

/// Walk a directory recursively, returning all file paths.
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
