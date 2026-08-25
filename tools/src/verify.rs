use crate::agents::exit::{clear_agent_exit, observed_exit, record_agent_exit};
use crate::agents::invocation::{Backend, Invocation, KIRO_UNPINNED_MODEL};
use crate::agents::run::{run_cached, Outcome, PhaseRun, SkipCheck};
use crate::agents::session::{ClaudeRun, Session};
use crate::agents::work::IsolatedWorkDir;
use crate::agents::Pool;
use crate::artifact::{Published, Verify};
use crate::battery::{self, Case, Paths};
use crate::cache::{self, CliVersion, ModelId};
use crate::cli::Agent;
use crate::io::workdir::Roots;
use crate::translate::Translations;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a verify sweep RESOLVED, by case dir. A case absent from it is absent from the verified score.
pub type Verifications = HashMap<PathBuf, Published<Verify>>;

/// Wall-clock cap on one verify session. Reaches the command through
/// [`Session`], which is also what the cache key records, so the two cannot diverge.
pub(crate) const VERIFY_TIMEOUT_SECS: u64 = 10800;

/// For a unit far larger than one Test-Corpus case: a group's session covers every config it has, and a
/// harvest-bench project is a whole upstream C library. Scoped, not global, because `timeout=` is in the
/// verify key and Test-Corpus's 208 entries measure 126.6 h with a 2.50 h max (`spec-7c.md`).
const LARGE_UNIT_VERIFY_TIMEOUT_SECS: u64 = 43200;

/// Harvest-bench's own, at 24 h: measured, mujs needed 6.5 h and zstd 4.4 h. SEPARATE from the group
/// ceiling, which `B02_synthetic` and `P01` hold entries under -- one constant would re-key those.
const HB_VERIFY_TIMEOUT_SECS: u64 = 86400;

/// How much wall clock one verify session gets. `WholeProject` was measured: lz4, mujs and zstd all died
/// at EXACTLY 3.00 h, and an infra failure cannot be sealed, so they earned no entry at all.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Budget {
    OneCase,
    WholeGroup,
    WholeProject,
}

impl Budget {
    fn secs(self, one_case: u64) -> u64 {
        match self {
            Self::OneCase => one_case,
            Self::WholeGroup => LARGE_UNIT_VERIFY_TIMEOUT_SECS,
            Self::WholeProject => HB_VERIFY_TIMEOUT_SECS,
        }
    }
}

const KIRO_VERIFY_TIMEOUT_SECS: u64 = 2700;

pub fn run(
    paths: &Paths,
    battery_name: &str,
    filter: Option<&str>,
    force: bool,
    pool: &crate::agents::Pool,
    translations: &Translations,
) -> Result<Verifications> {
    run_with_semaphore(paths, battery_name, filter, force, pool, translations)
}

/// Every battery under ONE semaphore, so `parallel` bounds work in flight across the whole sweep rather
/// than within each battery. `filter` is threaded rather than dropped: this path passed `None` down, so
/// an `--include-regex` reaching it silently ran everything -- the same defect
/// `translate_regex_is_honoured_or_refused` exists for one phase over.
pub fn run_all(
    paths: &Paths,
    batteries: &[String],
    filter: Option<&str>,
    force: bool,
    pool: &crate::agents::Pool,
    translations: &Translations,
) -> Result<Verifications> {
    let mut resolved = Verifications::new();

    let errors: Vec<anyhow::Error> = std::thread::scope(|s| {
        let handles: Vec<_> = batteries
            .iter()
            .map(|bat| {
                s.spawn(move || -> Result<Verifications> {
                    run_with_semaphore(paths, bat, filter, force, pool, translations)
                })
            })
            .collect();

        handles
            .into_iter()
            .filter_map(|h| match h.join() {
                Ok(Ok(v)) => {
                    resolved.extend(v);
                    None
                }
                Ok(Err(e)) => Some(e),
                Err(_) => Some(anyhow::anyhow!("verify thread panicked")),
            })
            .collect()
    });

    if let Some(first) = errors.into_iter().next() {
        return Err(first);
    }
    Ok(resolved)
}

/// `Absent` and `AlreadyVerified` were one "skipped" line, which is how a sweep that verified NOTHING
/// read like one that verified everything. Only the first is refused.
enum Verdict {
    Absent,
    AlreadyVerified(Option<Published<Verify>>),
    Verified(Published<Verify>),
    Failed,
}

/// Names, capped: one shared-source group is 127 followers, and the list would bury the sentence.
fn first_few(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(8).map(String::as_str).collect();
    if names.len() > shown.len() {
        return format!(
            "{} … and {} more",
            shown.join(", "),
            names.len() - shown.len()
        );
    }
    shown.join(", ")
}

/// Beside the verify count, where the number is produced. NOT a refusal: opencode must verify.
fn report_unkeyed_seeds(unkeyed: usize) {
    if unkeyed > 0 {
        println!(
            "{unkeyed} of these were seeded from a translation no key names, so their inputs \
             cannot be attributed to this run (see the ⚠️ lines above)"
        );
    }
}

/// A sweep that verified nothing must not exit 0: the eval tree makes those cases absent, this loud.
fn refuse_absent(absent: &[String], how_to_translate: &str) -> Result<()> {
    anyhow::ensure!(
        absent.is_empty(),
        "{} case(s) went unverified: this run resolved no translation for {}. A verification is \
         seeded from a translation THIS RUN resolved, so there is no verification for these cases \
         and no number derived from them is measured. Translate them deliberately ({}) and verify \
         after.",
        absent.len(),
        first_few(absent),
        how_to_translate,
    );
    Ok(())
}

fn run_with_semaphore(
    paths: &Paths,
    battery_name: &str,
    filter: Option<&str>,
    force: bool,
    pool: &Pool,
    translations: &Translations,
) -> Result<Verifications> {
    let mut resolved = Verifications::new();
    let battery = battery::discover(&paths.corpus_dir, battery_name, filter)?;
    let output_dir = paths.output_dir(battery_name);
    let skip = verify_skip_check(paths);
    let store = cache::Store::open(&paths.repo_root, paths.cache_mode)?;
    // Through `read_prompt`, not a second `verify.md` path of its own: this WAS the second
    // definition, so splitting the prompts broke verify while translate replayed all 209 entries.
    //
    // `Option`, not `require_prompt`: an agent with no verify BACKEND does no work here, and
    // demanding a prompt it will never read would refuse a sweep that is correctly a no-op.
    // `a_verify_prompt_exists_exactly_where_a_verify_phase_does` pins the other direction -- where a
    // backend does resolve, the prompt is there.
    let prompt_template = verify_template(paths)?;

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

    let ind_results: Vec<(String, Verdict, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = independent
            .iter()
            .map(|c| {
                let handle = s.spawn(|| {
                    let _permit = pool.acquire();
                    let case_dir = output_dir.join(&c.name);
                    // Verify used to gate on one `stat` of the directory it then seeded from.
                    let Some(translated) = translations.get(&case_dir) else {
                        return (c.name.clone(), Verdict::Absent);
                    };
                    if !force
                        && skip.already_done(|| {
                            crate::artifact::phase_log::<Verify>(&case_dir).exists()
                        })
                    {
                        return (
                            c.name.clone(),
                            Verdict::AlreadyVerified(already(&c.name, &case_dir)),
                        );
                    }
                    let cmake_flags = get_cmake_flags(paths, battery_name, &c.name);
                    let verdict = match verify_case(
                        &case_dir,
                        translated,
                        Rendering {
                            template: &prompt_template,
                            cmake_flags: &cmake_flags,
                            configs: "",
                        },
                        paths,
                        &store,
                        Budget::OneCase,
                    ) {
                        Ok(Some(published)) => Verdict::Verified(published),
                        Ok(None) => Verdict::Failed,
                        Err(e) => {
                            crate::refusal::record(&c.name, Err(e));
                            Verdict::Failed
                        }
                    };
                    (c.name.clone(), verdict)
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
                    (name, Verdict::Failed, true)
                }
            })
            .collect()
    });
    let panicked: Vec<String> = ind_results
        .iter()
        .filter(|(_, _, p)| *p)
        .map(|(n, _, _)| n.clone())
        .collect();

    let mut verified = 0usize;
    let mut failed = 0usize;
    let mut current = 0usize;
    let mut absent: Vec<String> = Vec::new();
    for (name, result, _) in ind_results {
        current += 1;
        let case_dir = output_dir.join(&name);
        match result {
            Verdict::Absent => {
                println!("[{current}/{total}] ⏭️  {name} (no translation resolved this run)");
                absent.push(name);
            }
            Verdict::AlreadyVerified(published) => {
                if let Some(p) = published {
                    resolved.insert(case_dir, p);
                }
                println!("[{current}/{total}] ⏭️  {name} (already verified)")
            }
            Verdict::Verified(published) => {
                verified += 1;
                resolved.insert(case_dir, published);
                println!("[{current}/{total}] ✅ {name}");
            }
            Verdict::Failed => {
                failed += 1;
                println!("[{current}/{total}] ❌ {name}");
            }
        }
    }

    for group in &shared {
        current += 1;
        let real_dir = output_dir.join(&group.real_case);

        // The propagation below derives every follower's `verified/` from this one, so an unresolved
        // group is refused rather than propagated from what it left last time.
        let Some(translated) = translations.get(&real_dir) else {
            absent.push(group.real_case.clone());
            println!(
                "[{current}/{total}] ⏭️  {} (no translation resolved this run)",
                group.real_case
            );
            continue;
        };

        let real = if !force
            && skip.already_done(|| crate::artifact::phase_log::<Verify>(&real_dir).exists())
        {
            println!(
                "[{current}/{total}] ⏭️  {} (already verified)",
                group.real_case
            );
            already(&group.real_case, &real_dir)
        } else {
            println!(
                "[{current}/{total}] 🔬 {} (shared-source, {} configs)",
                group.real_case,
                group.configs.len()
            );
            let cmake_flags = get_cmake_flags(paths, battery_name, &group.real_case);
            let configs_text = build_configs_text(paths, battery_name, group);
            let published = verify_case(
                &real_dir,
                translated,
                Rendering {
                    template: &prompt_template,
                    cmake_flags: &cmake_flags,
                    configs: &configs_text,
                },
                paths,
                &store,
                Budget::WholeGroup,
            )?;

            if published.is_some() {
                verified += 1;
                println!("[{current}/{total}] ✅ {} — verified", group.real_case);
            } else {
                failed += 1;
                println!(
                    "[{current}/{total}] ❌ {} — verification incomplete",
                    group.real_case
                );
            }
            published
        };

        // A follower's crate is DERIVED, so it is resolved only where the real case was.
        let Some(real) = real else { continue };
        resolved.insert(real_dir.clone(), real);
        println!(
            "Re-propagating verified fixes from {} to {} configs...",
            group.real_case,
            group.configs.len()
        );
        for cfg in &group.configs {
            // The borrow ends before the insert: a follower's provenance is the group's key.
            let derived = match resolved.get(&real_dir) {
                Some(source) => crate::translate::propagate_config_phase::<Verify>(
                    paths,
                    battery_name,
                    &group.real_case,
                    cfg,
                    source,
                )?,
                None => None,
            };
            if let Some(published) = derived {
                resolved.insert(paths.case_dir(battery_name, &cfg.name), published);
            }
        }
        println!("Propagated to {} cases", group.configs.len());
    }

    println!();
    println!("Verified: {verified}, Failed: {failed} (of {total})");
    report_unkeyed_seeds(crate::translate::unkeyed_seeds(translations, &output_dir));
    if let Some(line) = store.tally_line() {
        println!("{line}");
    }
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
    refuse_absent(&absent, &format!("harvest-tools translate {battery_name}"))?;
    paths
        .cache_mode
        .require_every_case_resolved(failed, total)?;
    Ok(resolved)
}

/// A case the skip check passed over: no key names what is in `verified/`, and it says so. That check
/// is the LOG, so `verified/` can hold a transcript and no crate — what the run below leaves when its
/// verification does not compile. `None` then: absent from the score, not a build failure.
fn already(name: &str, case_dir: &Path) -> Option<Published<Verify>> {
    match Published::<Verify>::unkeyed_from_phase_dir(case_dir) {
        Ok(published) => Some(published),
        Err(e) => {
            eprintln!(
                "  ⚠️  {name}: nothing published in verified/ can be taken as this run's \
                 verification, so the case is absent from the verified score: {e:#}"
            );
            None
        }
    }
}

/// Deliberately shares `verify.md` and `verify_case` with Test-Corpus so both
/// benchmarks are graded with the same rigor. HB has no per-project cmake flags or
/// configs, hence the empty strings passed through.
pub fn run_harvest_bench(
    paths: &Paths,
    projects: &[battery::HarvestBenchProject],
    pool: &crate::agents::Pool,
    force: bool,
    translations: &Translations,
) -> Result<Verifications> {
    let mut resolved = Verifications::new();
    let prompt_template = verify_template(paths)?;

    let skip = verify_skip_check(paths);
    let store = cache::Store::open(&paths.repo_root, paths.cache_mode)?;
    let total = projects.len();
    println!("=== Verifying harvest-bench ({total} projects) ===");

    let results: Vec<(String, Verdict, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = projects
            .iter()
            .map(|p| {
                let prompt = &prompt_template;
                let store = &store;
                let name = p.name().to_string();
                let handle = s.spawn({
                    let name = name.clone();
                    move || {
                        let _permit = pool.acquire();
                        let case_dir = paths.output_dir(&name);
                        let Some(translated) = translations.get(&case_dir) else {
                            return (name, Verdict::Absent);
                        };
                        if !force
                            && skip.already_done(|| {
                                crate::artifact::phase_log::<Verify>(&case_dir).exists()
                            })
                        {
                            let published = already(&name, &case_dir);
                            return (name, Verdict::AlreadyVerified(published));
                        }
                        let verdict = match verify_case(
                            &case_dir,
                            translated,
                            Rendering {
                                template: prompt,
                                cmake_flags: "",
                                configs: "",
                            },
                            paths,
                            store,
                            Budget::WholeProject,
                        ) {
                            Ok(Some(published)) => Verdict::Verified(published),
                            Ok(None) => Verdict::Failed,
                            Err(e) => {
                                crate::refusal::record(&name, Err(e));
                                Verdict::Failed
                            }
                        };
                        (name, verdict)
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
                    (name, Verdict::Failed, true)
                }
            })
            .collect()
    });
    let panicked: Vec<String> = results
        .iter()
        .filter(|(_, _, p)| *p)
        .map(|(n, _, _)| n.clone())
        .collect();

    let (mut verified, mut failed) = (0usize, 0usize);
    let mut absent: Vec<String> = Vec::new();
    for (i, (name, result, _)) in results.into_iter().enumerate() {
        let n = i + 1;
        let case_dir = paths.output_dir(&name);
        match result {
            Verdict::Absent => {
                println!("[{n}/{total}] ⏭️  {name} (no translation resolved this run)");
                absent.push(name);
            }
            Verdict::AlreadyVerified(published) => {
                if let Some(p) = published {
                    resolved.insert(case_dir, p);
                }
                println!("[{n}/{total}] ⏭️  {name} (already verified)")
            }
            Verdict::Verified(published) => {
                verified += 1;
                resolved.insert(case_dir, published);
                println!("[{n}/{total}] ✅ {name}");
            }
            Verdict::Failed => {
                failed += 1;
                println!("[{n}/{total}] ❌ {name}");
            }
        }
    }
    println!("\nHB verify: {verified}/{total} verified, {failed} failed");
    report_unkeyed_seeds(crate::translate::unkeyed_seeds(
        translations,
        &paths.results_dir,
    ));
    if let Some(line) = store.tally_line() {
        println!("{line}");
    }
    // See run_with_semaphore: a panic is an infrastructure failure, not a result.
    crate::refusal::bail_if_any()?;
    anyhow::ensure!(
        panicked.is_empty(),
        "{} verify worker(s) panicked: {}. Re-run them before scoring.",
        panicked.len(),
        panicked.join(", ")
    );
    refuse_absent(&absent, "harvest-tools translate HB")?;
    Ok(resolved)
}

/// Resolves the backend a verify phase will use; `None` means no verify phase. Consulted
/// before the store so a verify-less agent never materialises a work tree to discard.
///
/// `agents::invocation::has_verify_phase` answers the same question without a `Paths`, so
/// the two are one decision split across two files.
/// `a_verify_backend_resolves_exactly_where_a_verify_phase_is_declared` is what holds them
/// together; adjacency used to be all there was, and it was never a guard.
fn verify_invocation(paths: &Paths, budget: Budget) -> Result<Option<Invocation>> {
    let inv = match paths.agent {
        Agent::Kiro => Invocation {
            backend: Backend::Kiro,
            model: ModelId::new(KIRO_UNPINNED_MODEL)?,
            cli: CliVersion::probe("kiro-cli")?,
            session: Session::kiro(budget.secs(KIRO_VERIFY_TIMEOUT_SECS)),
        },
        Agent::Claude => Invocation {
            backend: Backend::Claude,
            model: crate::agents::invocation::claude_model()?,
            cli: CliVersion::probe("claude")?,
            session: Session::claude(budget.secs(VERIFY_TIMEOUT_SECS)),
        },
        Agent::CodexGpt56Sol => {
            let (model, _region) = crate::agents::invocation::codex_model(paths.agent)
                .context("codex_model has no entry for a codex variant")?;
            Invocation {
                backend: Backend::Codex,
                model: ModelId::new(model)?,
                cli: CliVersion::probe("codex")?,
                session: Session::codex(budget.secs(VERIFY_TIMEOUT_SECS)),
            }
        }
        Agent::OpenCode => {
            let model =
                crate::agents::opencode::parse_model(paths.model.as_deref().unwrap_or_default())?;
            Invocation {
                model: ModelId::new(model.as_arg())?,
                backend: Backend::OpenCode(model),
                cli: CliVersion::probe("opencode")?,
                session: Session::opencode(
                    crate::agents::opencode::Phase::Verify,
                    budget.secs(VERIFY_TIMEOUT_SECS),
                ),
            }
        }
        // ClaudeCombined verifies inside translate; ClaudeMinimal and the E2/E3/E4/E6 ablations are
        // translate-only by design. `codex-gpt55`/`codex-gpt54` are HISTORICAL -- they predate
        // `prompts/codex/`, so their records stay as earned; the fair run is `codex-gpt56-sol`.
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

/// "Already verified?", the same question `translate::translate_skip_check` answers, narrowed by the
/// same store rule — two skip policies for one concept is how this drifted before. Keyed is NOT
/// `has_verify_phase`: opencode HAS one, but `opencode run --format json` carries none of the
/// terminal records [`crate::domain::health::classify`] reads, so nothing mints the `Completed` a
/// seal needs and no entry can ever exist to hit — keyed, it misses and re-bills every case, every
/// sweep, which the `verify.log` check used to make free. Exhaustive: a new backend decides here.
fn verify_skip_check(paths: &Paths) -> SkipCheck {
    match paths.agent {
        Agent::Kiro | Agent::Claude | Agent::CodexGpt56Sol => SkipCheck::Keyed,
        Agent::OpenCode
        | Agent::C2rust
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
        | Agent::CodexGpt54 => SkipCheck::WhateverIsPublished,
    }
    .through(paths.cache_mode)
}

/// The verify prompt, empty ONLY for an agent that has no verify backend to read it.
///
/// `read_prompt` rather than `require_prompt`, because a no-backend sweep does no work here and
/// demanding a prompt it will never read would refuse a run that is correctly a no-op. But an empty
/// template reaching a real backend would invoke the agent with nothing to do and record the result
/// as a measurement, so the pairing is asserted here rather than trusted to
/// `a_verify_prompt_exists_exactly_where_a_verify_phase_does`.
fn verify_template(paths: &Paths) -> Result<String> {
    let text = crate::translate::read_prompt(paths, crate::translate::PromptKind::Verify)?;
    anyhow::ensure!(
        text.is_some() || !crate::agents::invocation::has_verify_phase(paths.agent),
        "--agent {} has a verify backend but no Verify prompt, so it would be invoked with an \
         empty one",
        paths.agent_key.as_str()
    );
    Ok(text.unwrap_or_default())
}

/// The template plus the two substitutions a group needs and an independent case leaves empty.
struct Rendering<'a> {
    template: &'a str,
    cmake_flags: &'a str,
    configs: &'a str,
}

/// Resolve what will run, then hand it to [`run_cached`] — the one execution path for an
/// agent phase, where the store, the publish and the metrics live for both phases.
fn verify_case(
    case_dir: &Path,
    translated: &Published<crate::artifact::Translate>,
    rendering: Rendering<'_>,
    paths: &Paths,
    store: &cache::Store,
    budget: Budget,
) -> Result<Option<Published<Verify>>> {
    // Unreachable: `has_verify_phase` gates every caller. `None`, not a claimed success.
    let Some(inv) = verify_invocation(paths, budget)? else {
        return Ok(None);
    };

    // Verify never mutates `translated/`, so no snapshot/restore is needed.
    let verified_logs = crate::artifact::phase_logs::<Verify>(case_dir);
    std::fs::create_dir_all(&verified_logs)?;
    let log_path = crate::artifact::phase_log::<Verify>(case_dir);

    // Isolated temp dir: the agent sees no config-specific path names and no
    // test_vectors/runner, only the crate's own c_src. Materialised before the store is
    // consulted because the prompt embeds this path and the prompt is part of the key —
    // on a hit the copy is wasted, but the prompt hashed and the prompt shown to the
    // agent are provably the same string.
    let work = IsolatedWorkDir::new(translated)?;

    let mut prompt = rendering
        .template
        .replace("CASE_DIR_PLACEHOLDER", &work.root().to_string_lossy())
        .replace("CMAKE_BUILD_FLAGS", rendering.cmake_flags)
        .replace("ALL_CONFIGURATIONS", rendering.configs);

    if matches!(inv.backend, Backend::OpenCode(_)) {
        prompt.push_str(&crate::agents::opencode::prompt_suffix(work.root()));
    }

    let _ = std::fs::write(verified_logs.join("prompt.md"), &prompt);

    let toolchain = cache::ToolchainId::detect()?;
    // Resolved once, here: every root that decides the key is then a value, and the two
    // digests below cannot disagree about what machine they were taken on.
    let roots = Roots::resolve(work.root(), &paths.repo_root);
    let rendered = cache::prompt(&prompt, &roots);
    let prompt_digest = rendered.digest.clone();
    let policy = inv.backend.policy_shape(paths.enforcement, &roots)?;
    let recipe_shape = cache::Recipe::new(&inv.session, policy)?;
    let recipe = recipe_shape.digest();

    let outcome = run_cached(
        PhaseRun {
            work,
            case_dir,
            log_path: &log_path,
            agent: &paths.agent_key,
            model: &inv.model,
            cli: &inv.cli,
            toolchain: &toolchain,
            prompt: &prompt_digest,
            recipe: &recipe,
            prompt_text: &rendered.normalised,
            recipe_record: recipe_shape.shape_record(),
        },
        store,
        |work| run_verify_agent(case_dir, &inv, work, &prompt, &log_path, paths),
    )?;
    // An artifact exists only if it compiled (`run_verify_agent`), and `finish` digests what it hands on.
    match outcome {
        Outcome::Published(publishing) => Ok(Some(publishing.finish()?)),
        Outcome::Nothing | Outcome::Unavailable => Ok(None),
    }
}

/// [`cache::Attempt::Nothing`] is "nothing worth keeping" — API error, abort, or a crate that does not
/// compile. Recorded where the loader cannot see it: see [`cache::Store::record_failure`].
fn run_verify_agent(
    case_dir: &Path,
    inv: &Invocation,
    work: IsolatedWorkDir<crate::artifact::Verify>,
    prompt: &str,
    log_path: &Path,
    paths: &Paths,
) -> Result<cache::Attempt<crate::artifact::Verify>> {
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
        Backend::Codex => {
            let (model, region) = crate::agents::invocation::codex_model(paths.agent)
                .context("a Codex backend resolved for a non-codex agent")?;
            // The SAME wrapper translate uses, and verify needs it more: this is the artifact the
            // score is built on.
            crate::translate::invoke_codex_with_retry(
                crate::translate::RetrySession {
                    prompt,
                    log_path,
                    work_dir: &work.translated_rust(),
                    context_label: "verify",
                },
                &inv.session,
                model,
                region,
            )?;
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
        return Ok(cache::Attempt::Nothing(
            cache::NotProduced::DidNotComplete {
                health: format!("{health:?}"),
            },
        ));
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
        eprintln!(
            "  ⚠️  verify produced a non-compiling crate — not publishing, so this case has no \
             verification this run and is absent from the verified score"
        );
        return Ok(cache::Attempt::Nothing(cache::NotProduced::DoesNotCompile));
    }
    println!("  verified artifact {:?}", sealed.digest());

    Ok(cache::Attempt::Produced(cache::Produced::new(
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
    use crate::artifact::{Keying, Translate};

    /// The one decision written twice, and since `has_verify_phase` moved to `agents/`,
    /// written in two files. Nothing else connects them: `translate.rs` pins
    /// `has_verify_phase` against the verify-prompt table, not against this match, and the
    /// only other test to call `verify_invocation` calls it for one agent with no phase.
    /// The group's longer session must reach its key, and an independent case's must not move: the
    /// 208 stored verify entries depend on the second half.
    #[test]
    fn a_group_gets_the_longer_verify_session_and_a_single_case_keeps_its_own() {
        // Exhaustive: a variant left on one case's ceiling keeps a limit measured for something smaller.
        for (budget, want) in [
            (Budget::OneCase, 7),
            (Budget::WholeGroup, LARGE_UNIT_VERIFY_TIMEOUT_SECS),
            (Budget::WholeProject, HB_VERIFY_TIMEOUT_SECS),
        ] {
            assert_eq!(budget.secs(7), want, "{budget:?}");
        }
        assert_eq!(
            (VERIFY_TIMEOUT_SECS, LARGE_UNIT_VERIFY_TIMEOUT_SECS),
            (10_800, 43_200),
            "raising the FIRST of these moves all 208 stored Test-Corpus verify keys"
        );
        let one = Session::claude(Budget::OneCase.secs(VERIFY_TIMEOUT_SECS)).shape();
        let group = Session::claude(Budget::WholeGroup.secs(VERIFY_TIMEOUT_SECS)).shape();
        assert!(
            one.contains("timeout=10800") && group.contains("timeout=43200"),
            "{one} / {group}"
        );
        assert_ne!(
            one, group,
            "or the longer session never reaches the group's key"
        );
    }

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
            let resolves = match verify_invocation(&paths, Budget::OneCase) {
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

    /// Verify gets translate's treatment on both halves: a backend that cannot produce an entry and
    /// a store that cannot read one each leave `verify.log` as the only check there is.
    #[test]
    fn a_verify_sweep_whose_store_cannot_read_keeps_the_only_check_it_has() {
        let paths = |agent, model, mode| {
            Paths::new(
                Path::new("/nonexistent"),
                agent,
                crate::cli::Dataset::TestCorpus,
                model,
                mode,
                crate::io::sandbox::Enforcement::AllowUnsandboxed,
            )
            .unwrap()
        };
        for (mode, expected) in [
            (cache::Mode::ReadWrite, SkipCheck::Keyed),
            (cache::Mode::Refresh, SkipCheck::Keyed),
            (cache::Mode::Bypass, SkipCheck::WhateverIsPublished),
        ] {
            assert_eq!(
                verify_skip_check(&paths(Agent::Claude, None, mode)),
                expected,
                "--agent claude verifies and can seal, so the store decides: {mode:?}"
            );
            assert_eq!(
                verify_skip_check(&paths(Agent::Kiro, None, mode)),
                expected,
                "and so does kiro, whose prose log mints a `Completed` from its exit: {mode:?}"
            );
            assert_eq!(
                verify_skip_check(&paths(
                    Agent::OpenCode,
                    Some("amazon-bedrock/us.anthropic.claude-sonnet-5"),
                    mode
                )),
                SkipCheck::WhateverIsPublished,
                "opencode HAS a verify phase and still cannot seal one, so a key it can never \
                 hit must not delete its only check: {mode:?}"
            );
            assert_eq!(
                verify_skip_check(&paths(Agent::Oneshot, Some("openai/gpt-5.4"), mode)),
                SkipCheck::WhateverIsPublished,
                "and an agent with no verify phase has no key to ask about either way: {mode:?}"
            );
        }
    }

    /// A case absent from the hand-off is not verified — and a sweep that verified NOTHING must not
    /// exit 0. It did: `Verified: 0, Failed: 0 (of 85)`, leaving the scorer the old `verified/`
    /// dirs. The absent case is `014_dead_code_lib` in its MEASURED shape, and the two cases no key
    /// can name must complete THE SAME sweep or refusing them is what makes them unverifiable.
    #[test]
    fn a_case_with_no_resolved_translation_is_refused_rather_than_counted_as_a_pass() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        // Oneshot has no verify backend, so `verify_case` is "nothing to do, ok" and no agent runs.
        let paths = Paths::new(
            tmp.path(),
            Agent::Oneshot,
            crate::cli::Dataset::TestCorpus,
            Some("openai/gpt-5.4"),
            cache::Mode::Bypass,
            crate::io::sandbox::Enforcement::AllowUnsandboxed,
        )
        .unwrap();
        let bat = "B01_synthetic";
        let (absent, seeded, group, follower) = (
            "014_dead_code_lib",
            "013_poor_quality_addition",
            "macrodepth_add_5",
            "macrodepth_add_5_cfg",
        );
        for name in [absent, seeded, group] {
            for dir in ["test_case", "test_vectors"] {
                std::fs::create_dir_all(paths.input_dir(bat).join(name).join(dir)).unwrap();
            }
        }
        // The symlinked `test_case` IS what makes a group.
        std::fs::create_dir_all(paths.input_dir(bat).join(follower).join("test_vectors")).unwrap();
        std::os::unix::fs::symlink(
            format!("../{group}/test_case"),
            paths.input_dir(bat).join(follower).join("test_case"),
        )
        .unwrap();
        // Both parts, because a composed prompt is body + protocol now. Writing only the old
        // single file made this test fail on a prompt-read error instead of the refusal it asserts.
        let shared = paths.repo_root.join("prompts/shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("verify.md"), "verify CASE_DIR_PLACEHOLDER\n").unwrap();
        std::fs::create_dir_all(&paths.prompts_dir).unwrap();
        std::fs::write(
            paths.prompts_dir.join(crate::translate::PROTOCOL_PART),
            "## protocol\n",
        )
        .unwrap();
        assert_eq!(
            battery::discover(&paths.corpus_dir, bat, None)
                .unwrap()
                .cases
                .iter()
                .filter(|c| matches!(c, Case::SharedSource(_)))
                .count(),
            1,
            "fixture: without a group the bypassed-store half of this test is never exercised"
        );

        let crate_at = |dir: &Path| {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
            std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}").unwrap();
        };
        let phase =
            |name: &str, which| crate::battery::phase_dir(&paths.output_dir(bat).join(name), which);
        let absent_dir = paths.output_dir(bat).join(absent);
        std::fs::create_dir_all(crate::artifact::phase_logs::<Verify>(&absent_dir)).unwrap();
        std::fs::create_dir_all(
            crate::artifact::phase_log::<Translate>(&absent_dir)
                .parent()
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            crate::artifact::phase_log::<Translate>(&absent_dir),
            "the transcript of the run that published nothing",
        )
        .unwrap();
        crate_at(&phase(absent, crate::battery::VERIFIED));
        crate_at(&phase(seeded, crate::battery::TRANSLATED));
        crate_at(&phase(group, crate::battery::TRANSLATED));

        // What the translate sweep hands over for those two: the published tree, digested this run.
        let pool = Pool::for_run(1);
        let mut resolved = Translations::new();
        for name in [seeded, group] {
            let dir = paths.output_dir(bat).join(name);
            let published = Published::<Translate>::unkeyed_from_phase_dir(&dir).unwrap();
            assert_eq!(published.keying(), Keying::Unkeyable);
            resolved.insert(dir, published);
        }
        assert_eq!(
            crate::translate::unkeyed_seeds(&resolved, &paths.output_dir(bat)),
            2,
            "the count printed beside the verify number must be the seeds under THIS battery"
        );
        let err = run_with_semaphore(&paths, bat, None, false, &pool, &resolved)
            .expect_err("a sweep that verified nothing is not a measurement");
        let text = format!("{err:#}");
        assert!(
            text.contains(absent) && text.contains("unverified"),
            "the refusal names the case, or the operator cannot tell what went unmeasured: {text}"
        );
        assert!(
            !text.contains(seeded) && !text.contains(group),
            "and it names ONLY the case whose chain stopped — a battery-wide refusal would make \
             the two seedable configurations unverifiable again: {text}"
        );

        crate_at(&phase(absent, crate::battery::TRANSLATED));
        resolved.insert(
            absent_dir.clone(),
            Published::<Translate>::unkeyed_from_phase_dir(&absent_dir).unwrap(),
        );
        run_with_semaphore(&paths, bat, None, false, &pool, &resolved)
            .expect("a case whose translation this run resolved is verified, not refused");
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
        assert!(verify_invocation(&paths, Budget::OneCase)
            .unwrap()
            .is_none());
    }
}
