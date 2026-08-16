use crate::agents::exit::{clear_agent_exit, observed_exit, record_agent_exit};
use crate::agents::session::{ClaudeRun, Session};
use crate::agents::work::IsolatedWorkDir;
use crate::agents::Semaphore;
use crate::battery::{self, Case, Paths};
use crate::cache::{self, CliVersion, ModelId};
use crate::cli::Agent;
use crate::io::workdir::Roots;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

/// Wall-clock cap on one verify session. Reaches the command through
/// [`Session`], which is also what the cache key records, so the two cannot diverge.
pub(crate) const VERIFY_TIMEOUT_SECS: u64 = 10800;

const KIRO_VERIFY_TIMEOUT_SECS: u64 = 2700;

pub fn run(
    paths: &Paths,
    battery_name: &str,
    filter: Option<&str>,
    force: bool,
    parallel: usize,
) -> Result<()> {
    let sem = Arc::new(Semaphore::new(parallel));
    run_with_semaphore(paths, battery_name, filter, force, &sem)
}

pub fn run_all(paths: &Paths, batteries: &[String], force: bool, parallel: usize) -> Result<()> {
    let sem = Arc::new(Semaphore::new(parallel));

    let errors: Vec<anyhow::Error> = std::thread::scope(|s| {
        let handles: Vec<_> = batteries
            .iter()
            .map(|bat| {
                let sem = sem.clone();
                s.spawn(move || -> Result<()> { run_with_semaphore(paths, bat, None, force, &sem) })
            })
            .collect();

        handles
            .into_iter()
            .filter_map(|h| match h.join() {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e),
                Err(_) => Some(anyhow::anyhow!("verify thread panicked")),
            })
            .collect()
    });

    if let Some(first) = errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}

fn run_with_semaphore(
    paths: &Paths,
    battery_name: &str,
    filter: Option<&str>,
    force: bool,
    sem: &Arc<Semaphore>,
) -> Result<()> {
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
        let handles: Vec<_> = independent
            .iter()
            .map(|c| {
                let handle = s.spawn(|| {
                    let _permit = sem.acquire();
                    let case_dir = output_dir.join(&c.name);
                    if !crate::battery::has_crate(&crate::battery::phase_dir(
                        &case_dir,
                        crate::battery::TRANSLATED,
                    )) {
                        return (c.name.clone(), None);
                    }
                    if !force
                        && crate::battery::phase_dir(&case_dir, crate::battery::VERIFIED)
                            .join("logs/verify.log")
                            .exists()
                    {
                        return (c.name.clone(), None);
                    }
                    let cmake_flags = get_cmake_flags(paths, battery_name, &c.name);
                    let outcome =
                        verify_case(&case_dir, &prompt_template, &cmake_flags, "", paths, &store);
                    (
                        c.name.clone(),
                        Some(crate::refusal::record(&c.name, outcome)),
                    )
                });
                (c.name.clone(), handle)
            })
            .collect();
        // A panicking worker is that one case's failure, not the sweep's: the name is kept
        // outside the thread so the remaining cases still report. The panic is collected
        // rather than swallowed — see the bail below.
        handles
            .into_iter()
            .map(|(name, h)| match h.join() {
                Ok((n, r)) => (n, r, false),
                Err(_) => {
                    eprintln!("  ⚠️  {name}: verify thread panicked — counting as failed");
                    (name, Some(false), true)
                }
            })
            .collect()
    });
    let panicked: Vec<&str> = ind_results
        .iter()
        .filter(|(_, _, p)| *p)
        .map(|(n, _, _)| n.as_str())
        .collect();

    let mut verified = 0usize;
    let mut failed = 0usize;
    let mut current = 0usize;
    for (name, result, _) in &ind_results {
        current += 1;
        match result {
            None => println!("[{current}/{total}] ⏭️  {name} (skipped)"),
            Some(true) => {
                verified += 1;
                println!("[{current}/{total}] ✅ {name}");
            }
            Some(false) => {
                failed += 1;
                println!("[{current}/{total}] ❌ {name}");
            }
        }
    }

    for group in &shared {
        current += 1;
        let real_dir = output_dir.join(&group.real_case);

        if !crate::battery::has_crate(&crate::battery::phase_dir(
            &real_dir,
            crate::battery::TRANSLATED,
        )) {
            continue;
        }

        if !force
            && crate::battery::phase_dir(&real_dir, crate::battery::VERIFIED)
                .join("logs/verify.log")
                .exists()
        {
            println!(
                "[{current}/{total}] ⏭️  {} (already verified)",
                group.real_case
            );
        } else {
            println!(
                "[{current}/{total}] 🔬 {} (shared-source, {} configs)",
                group.real_case,
                group.configs.len()
            );
            let cmake_flags = get_cmake_flags(paths, battery_name, &group.real_case);
            let configs_text = build_configs_text(paths, battery_name, group);
            let ok = verify_case(
                &real_dir,
                &prompt_template,
                &cmake_flags,
                &configs_text,
                paths,
                &store,
            )?;

            if ok {
                verified += 1;
                println!("[{current}/{total}] ✅ {} — verified", group.real_case);
            } else {
                failed += 1;
                println!(
                    "[{current}/{total}] ❌ {} — verification incomplete",
                    group.real_case
                );
            }
        }

        // Unconditional: without it runtests scores only the real case as verified,
        // never the config followers.
        println!(
            "Re-propagating verified fixes from {} to {} configs...",
            group.real_case,
            group.configs.len()
        );
        for cfg in &group.configs {
            crate::translate::propagate_config_phase(
                paths,
                battery_name,
                &group.real_case,
                cfg,
                crate::battery::VERIFIED,
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
    crate::refusal::bail_if_any()?;
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
pub fn run_harvest_bench(
    paths: &Paths,
    projects: &[battery::HarvestBenchProject],
    parallel: usize,
    force: bool,
) -> Result<()> {
    let sem = Arc::new(Semaphore::new(parallel));
    let prompt_template = std::fs::read_to_string(paths.prompts_dir.join("verify.md"))
        .context("reading verify.md")?;

    let store = cache::Store::open(&paths.repo_root, paths.cache_mode)?;
    let total = projects.len();
    println!("=== Verifying harvest-bench ({total} projects) ===");

    let results: Vec<(String, Option<bool>, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = projects
            .iter()
            .map(|p| {
                let sem = sem.clone();
                let prompt = &prompt_template;
                let store = &store;
                let name = p.name().to_string();
                let handle = s.spawn({
                    let name = name.clone();
                    move || {
                        let _permit = sem.acquire();
                        let case_dir = paths.output_dir(&name);
                        if !crate::battery::has_crate(&crate::battery::phase_dir(
                            &case_dir,
                            crate::battery::TRANSLATED,
                        )) {
                            return (name, None);
                        }
                        if !force
                            && crate::battery::phase_dir(&case_dir, crate::battery::VERIFIED)
                                .join("logs/verify.log")
                                .exists()
                        {
                            return (name, None);
                        }
                        let ok = crate::refusal::record(
                            &name,
                            verify_case(&case_dir, prompt, "", "", paths, store),
                        );
                        (name, Some(ok))
                    }
                });
                (name, handle)
            })
            .collect();
        // A panicking worker is that one project's failure, not the sweep's: the name is
        // kept outside the thread so the remaining projects still report.
        handles
            .into_iter()
            .map(|(name, h)| match h.join() {
                Ok((n, r)) => (n, r, false),
                Err(_) => {
                    eprintln!("  ⚠️  {name}: verify thread panicked — counting as failed");
                    (name, Some(false), true)
                }
            })
            .collect()
    });
    let panicked: Vec<&str> = results
        .iter()
        .filter(|(_, _, p)| *p)
        .map(|(n, _, _)| n.as_str())
        .collect();

    let (mut verified, mut failed) = (0usize, 0usize);
    for (i, (name, result, _)) in results.iter().enumerate() {
        let n = i + 1;
        match result {
            None => {
                println!("[{n}/{total}] ⏭️  {name} (skipped: no translated/ or already verified)")
            }
            Some(true) => {
                verified += 1;
                println!("[{n}/{total}] ✅ {name}");
            }
            Some(false) => {
                failed += 1;
                println!("[{n}/{total}] ❌ {name}");
            }
        }
    }
    println!("\nHB verify: {verified}/{total} verified, {failed} failed");
    // See run_with_semaphore: a panic is an infrastructure failure, not a result.
    crate::refusal::bail_if_any()?;
    anyhow::ensure!(
        panicked.is_empty(),
        "{} verify worker(s) panicked: {}. Re-run them before scoring.",
        panicked.len(),
        panicked.join(", ")
    );
    Ok(())
}

/// Which CLI runs the verify phase. An enum rather than a `bool` keeps the invocation
/// `match` below exhaustive over the backends that exist, with no second list of agent
/// names to keep in step. OpenCode carries its own parsed model, so its arm cannot reach
/// for another backend's — which is how claude's model came to be keyed for all three.
enum Backend {
    Kiro,
    Claude,
    OpenCode(crate::agents::opencode::Model),
}

/// Everything about the run the key must name, resolved per backend and BEFORE the agent
/// starts: the model that will actually be asked for, the CLI build that will ask for it,
/// and the exact command.
struct Invocation {
    backend: Backend,
    model: ModelId,
    cli: CliVersion,
    session: Session,
}

/// kiro-cli takes no `--model` and reports none in its prose transcript, so no honest
/// model id exists to key. Named as unpinned rather than filled in with a plausible
/// one, which is what the claude default used to do here.
const KIRO_UNPINNED_MODEL: &str = "unpinned:kiro-cli-default";

/// Resolves the backend a verify phase will use; `None` means no verify phase. Consulted
/// before the store so a verify-less agent never materialises a work tree to discard.
///
/// `agents::invocation::has_verify_phase` answers the same question without a `Paths`, so
/// the two are one decision split across two files.
/// `a_verify_backend_resolves_exactly_where_a_verify_phase_is_declared` is what holds them
/// together; adjacency used to be all there was, and it was never a guard.
fn verify_invocation(paths: &Paths) -> Result<Option<Invocation>> {
    let inv = match paths.agent {
        Agent::Kiro => Invocation {
            backend: Backend::Kiro,
            model: ModelId::new(KIRO_UNPINNED_MODEL)?,
            cli: CliVersion::probe("kiro-cli")?,
            session: Session::kiro(KIRO_VERIFY_TIMEOUT_SECS),
        },
        Agent::Claude => Invocation {
            backend: Backend::Claude,
            model: crate::agents::invocation::claude_model()?,
            cli: CliVersion::probe("claude")?,
            session: Session::claude(VERIFY_TIMEOUT_SECS),
        },
        Agent::OpenCode => {
            let model =
                crate::agents::opencode::parse_model(paths.model.as_deref().unwrap_or_default())?;
            Invocation {
                model: ModelId::new(model.as_arg())?,
                backend: Backend::OpenCode(model),
                cli: CliVersion::probe("opencode")?,
                session: Session::opencode(
                    crate::agents::opencode::Phase::Verify,
                    VERIFY_TIMEOUT_SECS,
                ),
            }
        }
        // ClaudeCombined verifies inside translate; ClaudeMinimal and the
        // prompt-sensitivity ablations (E2/E3/E4/E6) are translate-only by design;
        // Codex is excluded because it over-fixates on irrelevant linker symbols
        // during C-as-oracle verification.
        Agent::C2rust
        | Agent::Laertes
        | Agent::C2SaferRust
        | Agent::SmartC2Rust
        | Agent::Kimi
        | Agent::Oneshot
        | Agent::ClaudeCombined
        | Agent::ClaudeMinimal
        | Agent::ClaudeNoIter
        | Agent::ClaudeNoFeatures
        | Agent::ClaudeNoSubtask
        | Agent::ClaudeCrossPrompt
        | Agent::CodexGpt55
        | Agent::CodexGpt54 => return Ok(None),
    };
    Ok(Some(inv))
}

impl Backend {
    /// The filesystem policy this backend actually applies, paths tokenised. A
    /// hand-written summary would drift, and the literal directory names are
    /// machine-specific and must not enter the key.
    fn policy_shape(&self, paths: &Paths, roots: &Roots) -> Result<Option<String>> {
        let work_root = roots.work.as_path();
        let tokenise = |s: String| crate::cache::normalise(&s, roots);
        Ok(match self {
            // `--trust-all-tools` and no policy file: there is nothing to record.
            Backend::Kiro => None,
            Backend::Claude => Some(tokenise(
                crate::io::sandbox::settings_json(crate::io::sandbox::Policy {
                    repo_root: &paths.repo_root,
                    work_root,
                    enforcement: paths.enforcement,
                })?
                .to_string(),
            )),
            Backend::OpenCode(_) => Some(tokenise(crate::agents::opencode::permission_shape(
                work_root,
            ))),
        })
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
    let Some(inv) = verify_invocation(paths)? else {
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

    if matches!(inv.backend, Backend::OpenCode(_)) {
        prompt.push_str(&crate::agents::opencode::prompt_suffix(work.root()));
    }

    let _ = std::fs::write(verified_logs.join("prompt.md"), &prompt);

    let input_tree = work.input_digest().clone();
    let toolchain = cache::ToolchainId::detect()?;
    // Resolved once, here: every root that decides the key is then a value, and the two
    // digests below cannot disagree about what machine they were taken on.
    let roots = Roots::resolve(work.root(), &paths.repo_root);
    let prompt_digest = cache::prompt_digest(&prompt, &roots);
    let policy = inv.backend.policy_shape(paths, &roots)?;
    let recipe = cache::Recipe::new(&inv.session, policy)?.digest();
    let inputs = cache::KeyInputs {
        phase: crate::battery::VERIFIED,
        agent: &paths.agent_key,
        model: &inv.model,
        cli: &inv.cli,
        toolchain: &toolchain,
        prompt: &prompt_digest,
        recipe: &recipe,
        input_tree: &input_tree,
    };

    let obtained = store.obtain(&inputs, || {
        run_verify_agent(case_dir, &inv, work, &prompt, &log_path, paths)
    })?;

    let verified_dir = crate::battery::phase_dir(case_dir, crate::battery::VERIFIED);

    let Some(obtained) = obtained else {
        // Nothing published or stored, but `verified/logs/verify.log` is on disk (the
        // invocation tees it live), so the post-mortem survives and the "already
        // verified" skip check still sees this case.
        crate::translate::write_verification_metrics(
            &verified_dir,
            &serde_json::json!({
                "agent": paths.agent_key.as_str(),
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
        println!(
            "  ♻️  replayed a stored verification ({:?})",
            obtained.sealed.digest()
        );
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
    inv: &Invocation,
    work: IsolatedWorkDir,
    prompt: &str,
    log_path: &Path,
    paths: &Paths,
) -> Result<Option<cache::Produced<crate::artifact::Verify>>> {
    let start = std::time::Instant::now();

    // Cleared so a skipped or absent CLI run records no spend, and so a replay
    // (which never reaches this function) reports none either.
    clear_agent_exit();

    match &inv.backend {
        Backend::Kiro => {
            let status = inv
                .session
                .kiro_command(work.root(), prompt, log_path)
                .status()
                .context("invoking kiro-cli for verification")?;
            record_agent_exit(status);
        }
        Backend::Claude => {
            // Denies the repo root (the graded oracle, plus results/) and the shared
            // scratch base holding sibling work dirs, then re-grants this run's own root.
            let settings_path = crate::io::sandbox::write_settings(crate::io::sandbox::Policy {
                repo_root: &paths.repo_root,
                work_root: work.root(),
                enforcement: paths.enforcement,
            })?;
            let status = inv
                .session
                .claude_command(&ClaudeRun {
                    cwd: &work.translated_rust(),
                    prompt,
                    log: log_path,
                    settings: &settings_path,
                    agent_tmp: &crate::io::workdir::agent_tmp(work.root())?,
                    model: &inv.model,
                })
                .status()
                .context("invoking claude for verification")?;
            record_agent_exit(status);
            crate::agents::invocation::assert_pins_honoured(log_path, &inv.model, &inv.cli)?;
        }
        Backend::OpenCode(oc_model) => {
            // The compaction plugin restores SYMBOLS/ERRORS/CONFIGS.md, which
            // verify.md's Phases B/C are gated on.
            crate::agents::opencode::materialize_config(
                work.root(),
                crate::agents::opencode::Phase::Verify,
                oc_model,
            )?;
            crate::agents::opencode::invoke(
                &inv.session,
                prompt,
                log_path,
                &work.translated_rust(),
                work.root(),
                oc_model,
            )?;
        }
    }

    // Same discriminator the scoring gate uses: an api_error run is not a measurement,
    // so its output must become neither `verified/` nor a cache entry.
    //
    // The backend's log format is an argument rather than a guess: kiro writes prose, so
    // classifying its log as stream-json made `completed()` return None every time and
    // this function could never publish a kiro verification at all.
    let health = match crate::agent_health::read_tail(log_path) {
        Ok(tail) => {
            crate::domain::health::classify(&tail, paths.agent.log_format(), observed_exit())
        }
        // The transcript is the evidence, so an unreadable one is no evidence: `Unknown`
        // mints no proof, and a case with nothing to show for itself must not be sealed.
        Err(e) => crate::domain::health::Health::Unknown {
            why: format!("cannot read {}: {e}", log_path.display()),
        },
    };
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
    let gate = crate::io::workdir::tempdir("harvest-verify-gate-")?;
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
        crate::agents::exit::agent_provenance(&paths.agent_key, start.elapsed().as_secs()),
    )))
}

fn build_configs_text(paths: &Paths, battery: &str, group: &battery::SharedSourceGroup) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();

    let real_flags = get_cmake_flags(paths, battery, &group.real_case);
    let real_presets = paths
        .input_dir(battery)
        .join(&group.real_case)
        .join("CMakePresets.json");
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
    let presets = paths
        .input_dir(battery)
        .join(case_name)
        .join("CMakePresets.json");
    if !presets.exists() {
        return String::new();
    }
    let Ok(content) = std::fs::read_to_string(&presets) else {
        return String::new();
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) else {
        return String::new();
    };
    let Some(cv) = data
        .pointer("/configurePresets/1/cacheVariables")
        .and_then(|v| v.as_object())
    else {
        return String::new();
    };
    cv.iter()
        .filter(|(k, _)| *k != "CMAKE_C_STANDARD" && *k != "CMAKE_BUILD_TYPE")
        .map(|(k, v)| format!("-D{}={}", k, v.as_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one decision written twice, and since `has_verify_phase` moved to `agents/`,
    /// written in two files. Nothing else connects them: `translate.rs` pins
    /// `has_verify_phase` against the verify-prompt table, not against this match, and the
    /// only other test to call `verify_invocation` calls it for one agent with no phase.
    #[test]
    fn a_verify_backend_resolves_exactly_where_a_verify_phase_is_declared() {
        use crate::agents::invocation::has_verify_phase;
        use clap::ValueEnum;

        let (mut with_backend, mut without) = (0usize, 0usize);
        // clap's derived list, the same one translate.rs walks; `verify_invocation`'s match
        // is exhaustive over `Agent`, so a new variant is a compile error in this file
        // before it can be a case this loop silently never sees.
        for agent in Agent::value_variants() {
            // A model for every agent because opencode and oneshot make it part of their
            // identity and refuse without one; the rest ignore it. An unbuildable `Paths`
            // fails the test rather than dropping the variant from it.
            let paths = Paths::new(
                Path::new("/nonexistent"),
                *agent,
                crate::cli::Dataset::TestCorpus,
                Some("amazon-bedrock/us.anthropic.claude-sonnet-5"),
                cache::Mode::Bypass,
                crate::io::sandbox::Enforcement::AllowUnsandboxed,
            )
            .unwrap_or_else(|e| panic!("{agent:?}: no Paths, so it went unchecked: {e:#}"));
            // `Ok(None)` is the only way this function says "no verify phase": every
            // verify-less arm returns it before touching the environment, so an `Err` is a
            // backend that resolved and then failed to probe a CLI this machine lacks —
            // a phase, not the absence of one.
            let resolves = match verify_invocation(&paths) {
                Ok(inv) => inv.is_some(),
                Err(_) => true,
            };
            assert_eq!(
                has_verify_phase(*agent),
                resolves,
                "{agent:?}: has_verify_phase and verify_invocation disagree, so either a \
                 verify phase runs and resolves to nothing, or a backend exists that \
                 Benchmark::verifies never reaches"
            );
            if resolves {
                with_backend += 1;
            } else {
                without += 1;
            }
        }
        assert!(
            with_backend > 0 && without > 0,
            "{with_backend} agents with a backend, {without} without: the assertion above \
             discriminates only if both sides actually occur"
        );
    }

    #[test]
    fn a_verify_less_agent_resolves_before_any_cli_is_probed() {
        // The order matters: resolving the backend is what stops a verify-less agent
        // materialising a work tree, and now also what stops it probing a CLI it will
        // never run.
        let paths = Paths::new(
            Path::new("/nonexistent"),
            Agent::Oneshot,
            crate::cli::Dataset::TestCorpus,
            Some("openai/gpt-5.4"),
            cache::Mode::Bypass,
            // AllowUnsandboxed so the test asserts policy content rather than whether
            // this machine happens to have bwrap installed.
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap();
        assert!(verify_invocation(&paths).unwrap().is_none());
    }

    #[test]
    fn each_backend_records_the_policy_it_actually_applies() {
        // Every backend's recipe used to carry claude's sandbox settings, including the
        // two that never read that file.
        let repo = crate::io::workdir::test_tempdir().unwrap();
        let paths = Paths::new(
            repo.path(),
            Agent::Claude,
            crate::cli::Dataset::TestCorpus,
            None,
            cache::Mode::Bypass,
            // AllowUnsandboxed so the test asserts policy content rather than whether
            // this machine happens to have bwrap installed.
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap();
        let work = repo.path().join("work");
        let roots = Roots::resolve(&work, repo.path());

        let claude = Backend::Claude.policy_shape(&paths, &roots).unwrap();
        assert!(
            claude.as_deref().is_some_and(|p| p.contains("denyRead")),
            "{claude:?}"
        );
        assert!(
            claude
                .as_deref()
                .is_some_and(|p| !p.contains(&*repo.path().to_string_lossy())),
            "the literal paths must be tokenised or no key is portable: {claude:?}"
        );

        assert_eq!(Backend::Kiro.policy_shape(&paths, &roots).unwrap(), None);

        let oc = Backend::OpenCode(crate::agents::opencode::parse_model("p/m").unwrap())
            .policy_shape(&paths, &roots)
            .unwrap();
        assert!(
            oc.as_deref()
                .is_some_and(|p| p.contains("external_directory")),
            "{oc:?}"
        );
        assert_ne!(oc, claude);
    }
}
