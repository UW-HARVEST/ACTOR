use crate::battery::{self, Case, Paths};
use crate::cache;
use crate::cli::Agent;
use crate::translate::{IsolatedWorkDir, Semaphore};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

/// Wall-clock cap on one verify session, matching the `timeout 10800` the
/// claude verify invocation uses.
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
                let ok = verify_case(&case_dir, &prompt_template, &cmake_flags, "", paths, &store)
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
            let ok = verify_case(&real_dir, &prompt_template, &cmake_flags, &configs_text, paths, &store)?;

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

    let store = cache::Store::open(&paths.repo_root, paths.cache_mode)?;
    let total = projects.len();
    println!("=== Verifying harvest-bench ({total} projects) ===");

    let results: Vec<(String, Option<bool>)> = std::thread::scope(|s| {
        let handles: Vec<_> = projects.iter().map(|p| {
            let sem = sem.clone();
            let prompt = &prompt_template;
            let store = &store;
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
                let ok = verify_case(&case_dir, prompt, "", "", paths, store).unwrap_or(false);
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

/// Which CLI performs the verify phase for an agent, if any.
///
/// `None` is the answer for every agent that has no verify phase, and the *only*
/// place that is decided. Returning a three-variant enum rather than a `bool` means
/// the invocation `match` below is exhaustive over exactly the backends that exist,
/// so there is no second list of agent names to keep in step with this one and no
/// unreachable arm to reason about.
///
/// Consulted before the store, because an agent with no verify phase has no
/// invocation to memoise and must not materialise a work tree only to discard it.
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
        // ClaudeCombined: the translate phase already verified.
        // ClaudeMinimal: calibration baseline, no verify phase.
        // ClaudeNoIter / NoFeatures / NoSubtask / CrossPrompt: prompt-sensitivity
        //   ablations (E2/E3/E4/E6), each defined as translate-only.
        // Codex: skipped deliberately — the agent over-fixates on irrelevant linker
        //   symbols during C-as-oracle verification (model-specific behaviour).
        // c2rust / laertes / c2saferrust / smartc2rust / kimi / oneshot: no verify
        //   phase by design.
        Agent::C2rust | Agent::Laertes | Agent::C2SaferRust | Agent::SmartC2Rust
        | Agent::Kimi | Agent::Oneshot | Agent::ClaudeCombined | Agent::ClaudeMinimal
        | Agent::ClaudeNoIter | Agent::ClaudeNoFeatures | Agent::ClaudeNoSubtask
        | Agent::ClaudeCrossPrompt | Agent::CodexGpt55 | Agent::CodexGpt54 => None,
    }
}

/// Verify one case.
///
/// The agent invocation is not called directly. It is handed to
/// [`cache::Store::obtain`] as the work to do *if* no stored result matches, so a
/// replayed verification and a freshly computed one leave this function by the same
/// path — same assembly, same publish, same metrics. There is deliberately no
/// "cached" branch to keep in step with an "uncached" one.
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

    // Verify is PURE: it reads the immutable `translated/` crate (via
    // IsolatedWorkDir), works in a temp dir, and writes the result to
    // `verified/`. It never mutates `translated/`, so no snapshot/restore is
    // needed. The verify log lives in `verified/logs/`.
    let verified_logs = crate::battery::phase_dir(case_dir, crate::battery::VERIFIED).join("logs");
    std::fs::create_dir_all(&verified_logs)?;
    let log_path = verified_logs.join("verify.log");

    // Work in an isolated temp dir seeded from translated/ — the agent sees no
    // config-specific path names, and C-as-oracle verification uses only the
    // crate's own c_src (test_vectors/runner never enter the temp workspace).
    //
    // Materialised before the store is consulted, because the prompt embeds this
    // run's work-dir path and the prompt is part of the key. On a hit the copy is
    // then thrown away unused — a second or two against a twenty-minute agent
    // session, and it keeps the prompt the agent sees and the prompt that was
    // hashed provably the same string.
    let work = IsolatedWorkDir::new(case_dir)?;

    let start = std::time::Instant::now();

    let mut prompt = prompt_template
        .replace("CASE_DIR_PLACEHOLDER", &work.root().to_string_lossy())
        .replace("CMAKE_BUILD_FLAGS", cmake_flags)
        .replace("ALL_CONFIGURATIONS", configs_text);

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
        // The agent did not complete, or produced a crate that does not build.
        // Nothing published, nothing stored. `verified/logs/verify.log` is still on
        // disk (the invocation tees it there live), so the post-mortem survives and
        // the "already verified" skip check behaves as it did before.
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
        // A fresh run tees its transcript to verified/logs/verify.log; a replay must
        // leave the same file behind, or the skip check would not see this case as
        // verified and the next sweep would pay for it again.
        store.restore_log(&inputs, &obtained.key, &log_path)?;
    }

    // ONE publish, for a replay and a fresh run alike.
    obtained.sealed.publish(case_dir)?;

    // Reached only when an artifact exists, which — see `run_verify_agent` — means it
    // compiled. Stored entries therefore hold compiling crates by construction, so a
    // replay does not need to re-prove it.
    crate::translate::write_verification_metrics(
        &verified_dir,
        &obtained.provenance,
        obtained.replayed,
        Some(obtained.key.as_str()),
    );
    Ok(true)
}

/// Invoke the verify agent and, if it completed and produced a building crate,
/// return the sealed artifact.
///
/// `Ok(None)` means "nothing worth keeping": the agent hit an API error, was
/// aborted, or left a crate that does not compile. The store treats that as
/// nothing at all, which is the point — a transient failure must not be memoised
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

    // Capture the verify agent's process exit exactly like translate does — no
    // double standard. Cleared here so a skipped/absent CLI run records nothing,
    // and so a replay (which never reaches this function) reports no spend.
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
                // Pinned, and passed via the environment so the `[1m]` in the id is
                // never seen by bash as a bracket glob. See
                // `translate::CLAUDE_MODEL_DEFAULT` for why an unpinned model makes
                // both the measurement and the cache key unsound.
                .env("MODEL", model.as_str())
                // verify.md delegates to subagents via Task; without this they would
                // each pick their own model and the pin would cover only the
                // top-level session.
                .env("CLAUDE_CODE_SUBAGENT_MODEL", model.as_str())
                // Agent scratch on disk inside the work root, not the /tmp tmpfs,
                // plus a hard per-file cap. See crate::workdir.
                .env("TMPDIR", &agent_tmp)
                .env("CLAUDE_CODE_TMPDIR", &agent_tmp)
                .current_dir(work.translated_rust())
                .status()
                .context("invoking claude for verification")?;
            crate::translate::record_agent_exit(status);
            crate::translate::assert_model_honoured(log_path, model)?;
        }
        Backend::OpenCode => {
            // Same C-as-oracle verify prompt as Claude Code, different backend.
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

    // Only with PROOF the agent completed. `classify_log` is the same discriminator
    // the scoring gate uses: an api_error run is not a measurement, and its output
    // must not become `verified/` — nor, now, a cache entry that would make one
    // bad afternoon permanent.
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

    // ── Compile-gate: verify only counts as success if the crate still builds.
    // A mid-response API error can leave the crate half-written (missing symbols,
    // unresolved imports); recording such a crate as verified would make the scorer
    // build and grade garbage.
    //
    // The gate runs on a THROWAWAY ASSEMBLED COPY, not in `verified/`. Two reasons,
    // and the first is the one that matters:
    //
    //  * `cargo check` writes a `target/` directory. Running it in `verified/` meant
    //    the act of checking an artifact mutated it — measurement changing the thing
    //    measured, and a large part of the 1,702 MB of `target/` sitting in the
    //    results tree. A copy assembled by `Sealed::assemble_into` is byte-identical
    //    to what `verified/` would contain, so the verdict is unchanged.
    //  * It lets the gate run BEFORE publication rather than after, which deletes the
    //    old publish-then-delete-and-restore-the-logs rollback entirely. Nothing is
    //    written to the results tree unless it already passed.
    //
    // Consequently every stored cache entry holds a crate that compiled, so a replay
    // need not re-prove it.
    let gate = crate::workdir::tempdir("harvest-verify-gate-")?;
    sealed.assemble_into(case_dir, gate.path())?;
    let check = Command::new("timeout")
        .args(["120", "cargo", "check"])
        .current_dir(gate.path())
        .output();
    if !check.map_or(false, |o| o.status.success()) {
        eprintln!("  ⚠️  verify produced a non-compiling crate — not publishing; scorer will use translated/");
        return Ok(None);
    }
    println!("  verified artifact {:?}", sealed.digest());

    Ok(Some(cache::Produced {
        sealed,
        log: log_path.to_path_buf(),
        provenance: crate::translate::agent_provenance(agent, start.elapsed().as_secs()),
    }))
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
