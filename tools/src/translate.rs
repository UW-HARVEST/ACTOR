use crate::battery::{self, Case, Paths};
use crate::cargo_toml::{self, CargoToml};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub fn run(repo_root: &Path, battery_name: &str, filter: Option<&str>) -> Result<()> {
    let paths = Paths::new(repo_root);
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

                let prompt = if c.is_lib { "library.md" } else { "executable.md" };
                let prompt_text = std::fs::read_to_string(paths.prompts_dir.join(prompt))?;

                match translate_case(&paths, battery_name, &c.name, &prompt_text) {
                    Ok(()) => {
                        post_process_independent(&paths, battery_name, &c.name, c.is_lib)?;
                        save_original(&output_dir, &c.name)?;
                        translated += 1;
                        println!("  ✅ {} [{translated} translated, {failed} failed of {current}/{total}]", c.name);
                    }
                    Err(e) => {
                        failed += 1;
                        println!("  ❌ {} — {e} [{translated} translated, {failed} failed of {current}/{total}]", c.name);
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
                    let prompt_text =
                        std::fs::read_to_string(paths.prompts_dir.join("configurable.md"))?;

                    println!("[{current}/{total}] Translating: {} (shared-source, {} configs)", group.real_case, group.configs.len());
                    match translate_case(&paths, battery_name, &group.real_case, &prompt_text) {
                        Ok(()) => {
                            // Real case: add workspace, set both lib+bin (configurable prompt handles this)
                            let cargo_path = real_dir.join("translated_rust/Cargo.toml");
                            if cargo_path.exists() {
                                let mut cargo = CargoToml::open(&cargo_path)?;
                                cargo.add_workspace();
                                cargo.save()?;
                            }
                            save_original(&output_dir, &group.real_case)?;
                            translated += 1;
                            println!("  ✅ {} [{translated} translated, {failed} failed of {current}/{total}]", group.real_case);
                        }
                        Err(e) => {
                            failed += 1;
                            println!("  ❌ {} — {e}", group.real_case);
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

fn translate_case(paths: &Paths, battery: &str, name: &str, prompt: &str) -> Result<()> {
    let case_dir = paths.case_dir(battery, name);
    let translated = case_dir.join("translated_rust");
    let c_src = translated.join("c_src");
    let logs_dir = case_dir.join("logs");

    std::fs::create_dir_all(&c_src)?;
    std::fs::create_dir_all(&logs_dir)?;

    // Copy C source
    let input_test_case = paths.input_dir(battery).join(name).join("test_case");
    copy_dir_all(&input_test_case, &c_src)?;

    // Invoke kiro-cli
    let log_path = logs_dir.join("translation.log");
    let status = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "set -o pipefail; timeout 1800 kiro-cli chat --no-interactive --trust-all-tools \"$PROMPT\" < /dev/null 2>&1 | tee \"$LOG\"",
        ))
        .env("PROMPT", prompt)
        .env("LOG", &log_path)
        .env("OPENSSL_DIR", std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into()))
        .current_dir(&translated)
        .status()
        .context("invoking kiro-cli")?;

    if !translated.join("Cargo.toml").exists() {
        anyhow::bail!("no Cargo.toml produced (exit={})", status);
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
