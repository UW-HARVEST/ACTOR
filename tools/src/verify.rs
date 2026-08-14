use crate::battery::{self, Case, Paths};
use crate::cli::Agent;
use crate::translate::{IsolatedWorkDir, Semaphore};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

/// Wall-clock cap on one verify session, matching the `timeout 10800` the
/// claude verify invocation uses.
const VERIFY_TIMEOUT_SECS: u64 = 10800;

pub fn run(paths: &Paths, battery_name: &str, filter: Option<&str>, force: bool, parallel: usize) -> Result<()> {
    let sem = Arc::new(Semaphore::new(parallel));
    run_with_semaphore(paths, battery_name, filter, force, &sem)
}

pub fn run_all(paths: &Paths, batteries: &[String], force: bool, parallel: usize) -> Result<()> {
    let sem = Arc::new(Semaphore::new(parallel));

    let errors: Vec<anyhow::Error> = std::thread::scope(|s| {
        let handles: Vec<_> = batteries.iter().map(|bat| {
            let sem = sem.clone();
            s.spawn(move || -> Result<()> {
                run_with_semaphore(paths, bat, None, force, &sem)
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

fn run_with_semaphore(paths: &Paths, battery_name: &str, filter: Option<&str>, force: bool, sem: &Arc<Semaphore>) -> Result<()> {
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
                let ok = verify_case(&case_dir, &prompt_template, &cmake_flags, paths)
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
            let ok = verify_case(&real_dir, &prompt_template, &cmake_flags, paths)?;

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
                let ok = verify_case(&case_dir, prompt, "", paths).unwrap_or(false);
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

fn verify_case(case_dir: &Path, prompt_template: &str, cmake_flags: &str, paths: &Paths) -> Result<bool> {
    let agent = paths.agent;
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

    // `substitute_required`, not `str::replace`: a missing placeholder must be an
    // error, not a silent no-op. `ALL_CONFIGURATIONS` was substituted here for
    // months against a prompt that no longer contained it (removed by f7a4c5d,
    // "no config names leak to agent"), so a 40-line function read 128
    // CMakePresets.json files per shared-source group and had its output
    // discarded. Nothing failed, nothing warned.
    let mut prompt = prompt_template.to_string();
    substitute_required(&mut prompt, "CASE_DIR_PLACEHOLDER", &work.root().to_string_lossy())?;
    substitute_required(&mut prompt, "CMAKE_BUILD_FLAGS", cmake_flags)?;

    // OpenCode needs its filesystem-boundary contract and output-cap warning
    // appended (empty for every other agent, so no other prompt changes).
    if matches!(agent, Agent::OpenCode) {
        prompt.push_str(&crate::opencode::prompt_suffix(work.root()));
    }

    // Record the EXACT verify prompt the agent was given (post-substitution,
    // post-suffix), verbatim, next to the result — same rationale as
    // translate/logs/prompt.md: makes every verified/ result self-documenting
    // about what prompt ran.
    let _ = std::fs::write(verified_logs.join("prompt.md"), &prompt);

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
            // Deny the repo root (corpus = the graded oracle, plus results/) and
            // the shared scratch base (sibling work dirs), then re-grant this
            // run's own root. See crate::sandbox for why the previous
            // `ancestors().nth(2)` was both inconsistent and too narrow.
            let settings_path =
                crate::sandbox::write_settings(&paths.repo_root, work.root(), work.root())?;
            let agent_tmp = crate::workdir::agent_tmp(work.root())?;
            let status = Command::new("bash")
                .arg("-c")
                .arg(format!(
                    "ulimit -f {} -d {}; set -o pipefail; timeout 10800 claude -p \"$PROMPT\" \
                    --strict-mcp-config --disable-slash-commands --settings \"$SETTINGS\" \
                    --agents \"$AGENTS\" --agent claude_plain \
                    --max-turns 1000 --permission-mode bypassPermissions \
                    --verbose \
                    --output-format stream-json \
                    < /dev/null 2>&1 | tee \"$LOG\"",
                    crate::workdir::AGENT_FSIZE_BLOCKS,
                    crate::workdir::AGENT_DATA_KB
                ))
                .env("PROMPT", &prompt)
                .env("LOG", &log_path)
                .env("SETTINGS", &settings_path)
                .env("AGENTS", crate::translate::CLAUDE_PLAIN_AGENT_JSON)
                .env("OPENSSL_DIR", &openssl_dir)
                // Agent scratch on disk inside the work root, not the /tmp tmpfs,
                // plus a hard per-file cap. See crate::workdir.
                .env("TMPDIR", &agent_tmp)
                .env("CLAUDE_CODE_TMPDIR", &agent_tmp)
                .current_dir(work.translated_rust())
                .status()
                .context("invoking claude for verification")?;
            crate::translate::record_agent_exit(status);
        }
        Agent::OpenCode => {
            // Same C-as-oracle verify prompt as Claude Code, different backend.
            // The compaction plugin restores SYMBOLS/ERRORS/CONFIGS.md, which
            // verify.md's Phases B/C are gated on.
            let model = crate::opencode::parse_model(paths.model.as_deref().unwrap_or_default())?;
            crate::opencode::materialize_config(
                work.root(), crate::opencode::Phase::Verify, &model,
            )?;
            crate::opencode::invoke(
                crate::opencode::Phase::Verify, &prompt, &log_path,
                &work.translated_rust(), work.root(), &model, VERIFY_TIMEOUT_SECS,
            )?;
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

/// Substitute a REQUIRED prompt placeholder, erroring if it is absent.
///
/// `str::replace` returns the haystack unchanged when the needle is missing, so a
/// prompt that stops containing a placeholder silently discards whatever the
/// harness computed for it. That is not hypothetical: `ALL_CONFIGURATIONS` was
/// removed from `verify.md` by f7a4c5d ("no config names leak to agent") while the
/// substitution stayed, so for months a 40-line function read one
/// `CMakePresets.json` per configuration — 128 of them for P01 — and its output
/// went nowhere. Nothing failed and nothing warned.
///
/// Fail loudly instead. A prompt/harness mismatch is a bug in the experiment
/// setup, and the only safe time to learn about it is before the agent runs.
fn substitute_required(prompt: &mut String, placeholder: &str, value: &str) -> Result<()> {
    anyhow::ensure!(
        prompt.contains(placeholder),
        "prompt is missing the required placeholder {placeholder}: the harness computed a \
         value for it that would be silently discarded. Either restore {placeholder} to the \
         prompt or stop substituting it."
    );
    *prompt = prompt.replace(placeholder, value);
    Ok(())
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

#[cfg(test)]
mod prompt_substitution_tests {
    use super::*;

    #[test]
    fn substitutes_when_the_placeholder_is_present() {
        let mut p = String::from("build with FLAGS_HERE and go");
        substitute_required(&mut p, "FLAGS_HERE", "-DOP=add").expect("present placeholder");
        assert_eq!(p, "build with -DOP=add and go");
    }

    #[test]
    fn substitutes_every_occurrence() {
        let mut p = String::from("X then X");
        substitute_required(&mut p, "X", "y").unwrap();
        assert_eq!(p, "y then y");
    }

    #[test]
    fn errors_when_the_placeholder_is_absent() {
        // THE regression this guards: a silent no-op becomes a loud failure.
        let mut p = String::from("a prompt that no longer mentions the token");
        let err = substitute_required(&mut p, "ALL_CONFIGURATIONS", "some computed value")
            .expect_err("absent placeholder must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("ALL_CONFIGURATIONS"), "names the placeholder: {msg}");
        assert!(msg.contains("silently discarded"), "explains the consequence: {msg}");
    }

    #[test]
    fn an_empty_value_is_still_a_valid_substitution() {
        // 207 of 338 cases have no CMakePresets.json, so cmake_flags is "" — that
        // must succeed (the placeholder exists), not be mistaken for absence.
        let mut p = String::from("cmake .. CMAKE_BUILD_FLAGS && build");
        substitute_required(&mut p, "CMAKE_BUILD_FLAGS", "").unwrap();
        assert_eq!(p, "cmake ..  && build");
    }

    /// Every placeholder the harness substitutes must exist in the verify prompt
    /// of every agent that uses it. This is the check that would have caught the
    /// ALL_CONFIGURATIONS drift the day it happened.
    ///
    /// `prompts/` lives OUTSIDE this crate, so a tool that copies only `tools/`
    /// into a sandbox (cargo-mutants without `--in-place`) will not have it. In
    /// that case the check skips rather than failing on its own fixtures — it is a
    /// prompt/harness parity guarantee, not a code-coverage one. CI runs
    /// cargo-mutants with `--in-place` precisely so this stays meaningful there.
    #[test]
    fn every_required_placeholder_exists_in_every_verify_prompt() {
        const REQUIRED: &[&str] = &["CASE_DIR_PLACEHOLDER", "CMAKE_BUILD_FLAGS"];
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tools/ has a parent");
        for rel in ["prompts/claude/verify.md", "prompts/kiro/test-corpus/verify.md"] {
            let path = repo.join(rel);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue; // prompts not in this tree; see doc comment
            };
            for ph in REQUIRED {
                assert!(
                    text.contains(ph),
                    "{rel} is missing required placeholder {ph} — the harness substitutes it, \
                     so its value would be silently discarded"
                );
            }
        }
    }
}
