use crate::battery::{self, Case, Paths};
use crate::cache;
use crate::cli::Agent;
use crate::translate::{IsolatedWorkDir, Semaphore};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

/// Must stay equal to the `timeout 10800` hard-coded in the claude invocation below.
pub(crate) const VERIFY_TIMEOUT_SECS: u64 = 10800;

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
    let store = cache::Store::open(&paths.repo_root, paths.cache_mode)?;
    let prompt_template = std::fs::read_to_string(paths.prompts_dir.join("verify.md"))?;

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

    let ind_results: Vec<(String, Option<bool>, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = independent.iter().map(|c| {
            let handle = s.spawn(|| {
                let _permit = sem.acquire();
                let case_dir = output_dir.join(&c.name);
                if !crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED).join("Cargo.toml").exists() {
                    return (c.name.clone(), None);
                }
                if !force && crate::battery::phase_dir(&case_dir, crate::battery::VERIFIED).join("logs/verify.log").exists() {
                    return (c.name.clone(), None);
                }
                let cmake_flags = get_cmake_flags(paths, battery_name, &c.name);
                let ok = verify_case(&case_dir, &prompt_template, &cmake_flags, "", paths, &store)
                    .unwrap_or(false);
                (c.name.clone(), Some(ok))
            });
            (c.name.clone(), handle)
        }).collect();
        // A panicking worker is that one case's failure, not the sweep's: the name is kept
        // outside the thread so the remaining cases still report. The panic is collected
        // rather than swallowed — see the bail below.
        handles.into_iter().map(|(name, h)| match h.join() {
            Ok((n, r)) => (n, r, false),
            Err(_) => {
                eprintln!("  ⚠️  {name}: verify thread panicked — counting as failed");
                (name, Some(false), true)
            }
        }).collect()
    });
    let panicked: Vec<&str> =
        ind_results.iter().filter(|(_, _, p)| *p).map(|(n, _, _)| n.as_str()).collect();

    let mut verified = 0usize;
    let mut failed = 0usize;
    let mut current = 0usize;
    for (name, result, _) in &ind_results {
        current += 1;
        match result {
            None => println!("[{current}/{total}] ⏭️  {name} (skipped)"),
            Some(true) => { verified += 1; println!("[{current}/{total}] ✅ {name}"); }
            Some(false) => { failed += 1; println!("[{current}/{total}] ❌ {name}"); }
        }
    }

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
            let ok = verify_case(&real_dir, &prompt_template, &cmake_flags, &configs_text, paths, &store)?;

            if ok { verified += 1; println!("[{current}/{total}] ✅ {} — verified", group.real_case); }
            else { failed += 1; println!("[{current}/{total}] ❌ {} — verification incomplete", group.real_case); }
        }

        // Unconditional: without it runtests scores only the real case as verified,
        // never the config followers.
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
    // A worker panic is an infrastructure failure, not a measurement, and #67's rule is
    // that scoring must refuse one. Reporting it only on stdout while exiting 0 would let
    // a panic that hit every worker produce a plausible-looking verify rate that was never
    // measured. The sweep still finishes first, so the surviving cases are not wasted.
    anyhow::ensure!(
        panicked.is_empty(),
        "{} verify worker(s) panicked: {}. Their cases are recorded as failed, but a panic \
         is not a measurement — re-run them before scoring.",
        panicked.len(),
        panicked.join(", ")
    );
    Ok(())
}

/// Deliberately shares `verify.md` and `verify_case` with Test-Corpus so both
/// benchmarks are graded with the same rigor. HB has no per-project cmake flags or
/// configs, hence the empty strings passed through.
pub fn run_harvest_bench(paths: &Paths, projects: &[battery::HarvestBenchProject], parallel: usize, force: bool) -> Result<()> {
    let sem = Arc::new(Semaphore::new(parallel));
    let prompt_template = std::fs::read_to_string(paths.prompts_dir.join("verify.md"))
        .context("reading verify.md")?;

    let store = cache::Store::open(&paths.repo_root, paths.cache_mode)?;
    let total = projects.len();
    println!("=== Verifying harvest-bench ({total} projects) ===");

    let results: Vec<(String, Option<bool>, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = projects.iter().map(|p| {
            let sem = sem.clone();
            let prompt = &prompt_template;
            let store = &store;
            let name = p.name().to_string();
            let handle = s.spawn({
                let name = name.clone();
                move || {
                    let _permit = sem.acquire();
                    let case_dir = paths.output_dir(&name);
                    if !crate::battery::phase_dir(&case_dir, crate::battery::TRANSLATED).join("Cargo.toml").exists() {
                        return (name, None);
                    }
                    if !force && crate::battery::phase_dir(&case_dir, crate::battery::VERIFIED).join("logs/verify.log").exists() {
                        return (name, None);
                    }
                    let ok = verify_case(&case_dir, prompt, "", "", paths, store).unwrap_or(false);
                    (name, Some(ok))
                }
            });
            (name, handle)
        }).collect();
        // A panicking worker is that one project's failure, not the sweep's: the name is
        // kept outside the thread so the remaining projects still report.
        handles.into_iter().map(|(name, h)| match h.join() {
            Ok((n, r)) => (n, r, false),
            Err(_) => {
                eprintln!("  ⚠️  {name}: verify thread panicked — counting as failed");
                (name, Some(false), true)
            }
        }).collect()
    });
    let panicked: Vec<&str> =
        results.iter().filter(|(_, _, p)| *p).map(|(n, _, _)| n.as_str()).collect();

    let (mut verified, mut failed) = (0usize, 0usize);
    for (i, (name, result, _)) in results.iter().enumerate() {
        let n = i + 1;
        match result {
            None => println!("[{n}/{total}] ⏭️  {name} (skipped: no translated/ or already verified)"),
            Some(true) => { verified += 1; println!("[{n}/{total}] ✅ {name}"); }
            Some(false) => { failed += 1; println!("[{n}/{total}] ❌ {name}"); }
        }
    }
    println!("\nHB verify: {verified}/{total} verified, {failed} failed");
    // See run_with_semaphore: a panic is an infrastructure failure, not a result.
    anyhow::ensure!(
        panicked.is_empty(),
        "{} verify worker(s) panicked: {}. Re-run them before scoring.",
        panicked.len(),
        panicked.join(", ")
    );
    Ok(())
}

/// The only place a per-agent verify phase is decided; `None` means no verify phase.
/// An enum rather than a `bool` keeps the invocation `match` below exhaustive over the
/// backends that exist, with no second list of agent names to keep in step. Consulted
/// before the store so a verify-less agent never materialises a work tree to discard.
#[derive(Copy, Clone, Debug)]
enum Backend {
    Kiro,
    Claude,
    OpenCode,
}

fn verify_backend(agent: Agent) -> Option<Backend> {
    match agent {
        Agent::Kiro => Some(Backend::Kiro),
        Agent::Claude => Some(Backend::Claude),
        Agent::OpenCode => Some(Backend::OpenCode),
        // ClaudeCombined verifies inside translate; ClaudeMinimal and the
        // prompt-sensitivity ablations (E2/E3/E4/E6) are translate-only by design;
        // Codex is excluded because it over-fixates on irrelevant linker symbols
        // during C-as-oracle verification.
        Agent::C2rust | Agent::Laertes | Agent::C2SaferRust | Agent::SmartC2Rust
        | Agent::Kimi | Agent::Oneshot | Agent::ClaudeCombined | Agent::ClaudeMinimal
        | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask
        | Agent::ClaudeCrossPrompt | Agent::CodexGpt55 | Agent::CodexGpt54 => None,
    }
}

/// The agent invocation goes through [`cache::Store::obtain`] rather than being called
/// directly, so a replay and a fresh run leave by the same path — one assembly, one
/// publish, one metrics write, and no "cached" branch to keep in step with an uncached
/// one.
fn verify_case(
    case_dir: &Path,
    prompt_template: &str,
    cmake_flags: &str,
    configs_text: &str,
    paths: &Paths,
    store: &cache::Store,
) -> Result<bool> {
    let agent = paths.agent;
    let Some(backend) = verify_backend(agent) else {
        return Ok(true);
    };

    // Verify never mutates `translated/`, so no snapshot/restore is needed.
    let verified_logs = crate::battery::phase_dir(case_dir, crate::battery::VERIFIED).join("logs");
    std::fs::create_dir_all(&verified_logs)?;
    let log_path = verified_logs.join("verify.log");

    // Isolated temp dir: the agent sees no config-specific path names and no
    // test_vectors/runner, only the crate's own c_src. Materialised before the store is
    // consulted because the prompt embeds this path and the prompt is part of the key —
    // on a hit the copy is wasted, but the prompt hashed and the prompt shown to the
    // agent are provably the same string.
    let work = IsolatedWorkDir::new(case_dir)?;

    let start = std::time::Instant::now();

    let mut prompt = prompt_template
        .replace("CASE_DIR_PLACEHOLDER", &work.root().to_string_lossy())
        .replace("CMAKE_BUILD_FLAGS", cmake_flags)
        .replace("ALL_CONFIGURATIONS", configs_text);

    if matches!(agent, Agent::OpenCode) {
        prompt.push_str(&crate::opencode::prompt_suffix(work.root()));
    }

    let _ = std::fs::write(verified_logs.join("prompt.md"), &prompt);

    let agent_key = format!("{agent:?}").to_lowercase();
    let input_tree = work.input_digest().clone();
    let model = crate::translate::claude_model()?;
    let toolchain = cache::ToolchainId::detect()?;
    let prompt_digest = cache::prompt_digest(&prompt, work.root(), &paths.repo_root);
    let recipe = cache::Recipe::for_verify(paths, work.root()).digest();
    let inputs = cache::KeyInputs {
        phase: crate::battery::VERIFIED,
        agent: &agent_key,
        model: &model,
        toolchain: &toolchain,
        prompt: &prompt_digest,
        recipe: &recipe,
        input_tree: &input_tree,
    };

    let obtained = store.obtain(&inputs, || {
        run_verify_agent(case_dir, backend, work, &prompt, &log_path, paths, &model)
    })?;

    let verified_dir = crate::battery::phase_dir(case_dir, crate::battery::VERIFIED);

    let Some(obtained) = obtained else {
        // Nothing published or stored, but `verified/logs/verify.log` is on disk (the
        // invocation tees it live), so the post-mortem survives and the "already
        // verified" skip check still sees this case.
        crate::translate::write_verification_metrics(
            &verified_dir,
            &serde_json::json!({
                "agent": format!("{agent:?}").to_lowercase(),
                "duration_secs": start.elapsed().as_secs(),
                "success": false,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            false,
            None,
        );
        return Ok(false);
    };

    if obtained.replayed {
        println!("  ♻️  replayed a stored verification ({:?})", obtained.sealed.digest());
        // A replay must leave behind the same verify.log a fresh run tees, or the skip
        // check misses this case and the next sweep pays for it again.
        store.restore_log(&inputs, &obtained.key, &log_path)?;
    }

    obtained.sealed.publish(case_dir)?;

    // An artifact exists only if it compiled (see `run_verify_agent`), so a replay does
    // not re-prove it.
    crate::translate::write_verification_metrics(
        &verified_dir,
        &obtained.provenance,
        obtained.replayed,
        Some(obtained.key.as_str()),
    );
    Ok(true)
}

/// `Ok(None)` means "nothing worth keeping" — API error, abort, or a crate that does
/// not compile. The store keeps no entry for it, so a transient failure is not memoised
/// into a permanent one.
fn run_verify_agent(
    case_dir: &Path,
    backend: Backend,
    work: IsolatedWorkDir,
    prompt: &str,
    log_path: &Path,
    paths: &Paths,
    model: &cache::ModelId,
) -> Result<Option<cache::Produced<crate::artifact::Verify>>> {
    let agent = paths.agent;
    let openssl_dir = std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into());
    let start = std::time::Instant::now();

    // Cleared so a skipped or absent CLI run records no spend, and so a replay
    // (which never reaches this function) reports none either.
    crate::translate::clear_agent_exit();

    match backend {
        Backend::Kiro => {
            let status = Command::new("bash")
                .arg("-lc")
                .arg(r#"timeout 2700 kiro-cli chat --no-interactive --trust-all-tools --agent kiro_plain "$1" < /dev/null 2>&1 | tee "$2""#)
                .arg("--")
                .arg(prompt)
                .arg(log_path)
                .env("OPENSSL_DIR", &openssl_dir)
                .current_dir(work.root())
                .status()
                .context("invoking kiro-cli for verification")?;
            crate::translate::record_agent_exit(status);
        }
        Backend::Claude => {
            // Denies the repo root (the graded oracle, plus results/) and the shared
            // scratch base holding sibling work dirs, then re-grants this run's own root.
            let settings_path =
                crate::sandbox::write_settings(&paths.repo_root, work.root(), work.root(), paths.allow_unsandboxed)?;
            let agent_tmp = crate::workdir::agent_tmp(work.root())?;
            let status = Command::new("bash")
                .arg("-c")
                .arg(format!(
                    "ulimit -f {} -d {}; set -o pipefail; timeout 10800 claude -p \"$PROMPT\" \
                    --strict-mcp-config --disable-slash-commands --settings \"$SETTINGS\" \
                    --agents \"$AGENTS\" --agent claude_plain \
                    --max-turns 1000 --permission-mode bypassPermissions \
                    --model \"$MODEL\" \
                    --verbose \
                    --output-format stream-json \
                    < /dev/null 2>&1 | tee \"$LOG\"",
                    crate::workdir::AGENT_FSIZE_BLOCKS,
                    crate::workdir::AGENT_DATA_KB
                ))
                .env("PROMPT", prompt)
                .env("LOG", log_path)
                .env("SETTINGS", &settings_path)
                .env("AGENTS", crate::translate::CLAUDE_PLAIN_AGENT_JSON)
                .env("OPENSSL_DIR", &openssl_dir)
                // Passed via the environment so bash never sees the `[1m]` in the id as
                // a bracket glob; pinned because an unpinned model makes both the
                // measurement and the cache key unsound (`translate::CLAUDE_MODEL_DEFAULT`).
                .env("MODEL", model.as_str())
                // verify.md delegates to subagents via Task; without this each picks its
                // own model and the pin covers only the top-level session.
                .env("CLAUDE_CODE_SUBAGENT_MODEL", model.as_str())
                .envs(crate::translate::AGENT_ENV.iter().copied())
                // Scratch on disk inside the work root, not the /tmp tmpfs (crate::workdir).
                .env("TMPDIR", &agent_tmp)
                .env("CLAUDE_CODE_TMPDIR", &agent_tmp)
                .current_dir(work.translated_rust())
                .status()
                .context("invoking claude for verification")?;
            crate::translate::record_agent_exit(status);
            crate::translate::assert_model_honoured(log_path, model)?;
        }
        Backend::OpenCode => {
            // The compaction plugin restores SYMBOLS/ERRORS/CONFIGS.md, which
            // verify.md's Phases B/C are gated on.
            let oc_model = crate::opencode::parse_model(paths.model.as_deref().unwrap_or_default())?;
            crate::opencode::materialize_config(
                work.root(), crate::opencode::Phase::Verify, &oc_model,
            )?;
            crate::opencode::invoke(
                crate::opencode::Phase::Verify, prompt, log_path,
                &work.translated_rust(), work.root(), &oc_model, VERIFY_TIMEOUT_SECS,
            )?;
        }
    }

    // Same discriminator the scoring gate uses: an api_error run is not a measurement,
    // so its output must become neither `verified/` nor a cache entry.
    let health = crate::agent_health::classify_log(log_path);
    let Some(proof) = health.completed() else {
        eprintln!(
            "  {} — not publishing verified/: the agent did not complete ({:?})",
            case_dir.display(),
            health
        );
        return Ok(None);
    };
    let sealed = work.finish(&proof)?;

    // A mid-response API error can leave the crate half-written, so verify counts only
    // if it still builds. Gated on a throwaway assembled copy, byte-identical to what
    // `verified/` would hold: `cargo check` writes `target/`, so checking in place would
    // mutate the artifact being measured, and gating before publication removes the
    // publish-then-roll-back path.
    let gate = crate::workdir::tempdir("harvest-verify-gate-")?;
    sealed.assemble_into(case_dir, gate.path())?;
    let check = Command::new("timeout")
        .args(["120", "cargo", "check"])
        .current_dir(gate.path())
        .output();
    if !check.is_ok_and(|o| o.status.success()) {
        eprintln!("  ⚠️  verify produced a non-compiling crate — not publishing; scorer will use translated/");
        return Ok(None);
    }
    println!("  verified artifact {:?}", sealed.digest());

    Ok(Some(cache::Produced::new(
        sealed,
        log_path.to_path_buf(),
        crate::translate::agent_provenance(agent, start.elapsed().as_secs()),
    )))
}

fn build_configs_text(paths: &Paths, battery: &str, group: &battery::SharedSourceGroup) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();

    let real_flags = get_cmake_flags(paths, battery, &group.real_case);
    let real_presets = paths.input_dir(battery).join(&group.real_case).join("CMakePresets.json");
    let real_features = battery::extract_features_from_path(&real_presets).unwrap_or_default();
    let real_key: Vec<String> = real_features.to_vec();
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
            continue;
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
