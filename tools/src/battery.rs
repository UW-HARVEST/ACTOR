use crate::cli::{Agent, Dataset};
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

// ── Per-case phase directories: ONE source of truth ────────────────────
//
// A case's results live in two PHASE DIRECTORIES, uniform across every dataset:
// `<case>/translated/` and `<case>/verified/`. Each IS a self-contained crate
// root (src/, Cargo.toml, c_src/) and carries that phase's own `result.json`
// and `logs/`, so nothing lives at the case root.

/// Always present: exactly what translation produced.
pub const TRANSLATED: &str = "translated";

/// Present iff the verify phase ran.
pub const VERIFIED: &str = "verified";

pub fn phase_dir(case_dir: &Path, phase: &str) -> PathBuf {
    case_dir.join(phase)
}

/// THE PHASE PREDICATE: did this phase produce a crate? ONE definition, enforced by
/// `tests/architecture.rs` (A6), because a case falls between two spellings of it.
/// `verified/` exists as soon as verify writes a log, so the old `is_dir()` said yes
/// for pcre2 — logs, no crate — while every reader asked for `Cargo.toml` and
/// `continue`d, taking pcre2 out of the harvest-bench denominator.
pub fn has_crate(phase_dir: &Path) -> bool {
    phase_dir.join("Cargo.toml").is_file()
}

/// The crate-dir name MIT `runtests` hardcodes (test-corpus/.../discovery/rust.py), also used for the
/// agent's temp workspace and for the crate [`crate::eval`] materialises. NOT a storage phase dir, and
/// no longer a symlink to one — see [`crate::eval`] for what `.resolve()` did with the last symlink.
pub const TRANSLATED_RUST: &str = "translated_rust";

#[derive(Debug, Clone)]
pub struct IndependentCase {
    pub name: String,
    pub is_lib: bool,
}

/// A configuration within a shared-source group (symlinked test_case/).
#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub features: Vec<String>,
    pub is_lib: bool,
    pub lib_name: Option<String>,
}

/// A group of cases sharing the same C source, differentiated by CMake features.
#[derive(Debug, Clone)]
pub struct SharedSourceGroup {
    pub real_case: String,
    pub configs: Vec<Config>,
}

#[derive(Debug, Clone)]
pub enum Case {
    Independent(IndependentCase),
    SharedSource(SharedSourceGroup),
}

#[derive(Debug)]
pub struct Battery {
    pub name: String,
    pub cases: Vec<Case>,
}

pub fn all_batteries(corpus_dir: &Path) -> Result<Vec<String>> {
    let public_tests = corpus_dir.join("Public-Tests");
    anyhow::ensure!(
        public_tests.is_dir(),
        "Public-Tests not found: {}",
        public_tests.display()
    );

    let mut batteries: Vec<String> = std::fs::read_dir(&public_tests)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    batteries.sort();
    Ok(batteries)
}

pub fn has_shared_source_groups(corpus_dir: &Path, battery_name: &str) -> bool {
    let dir = corpus_dir.join("Public-Tests").join(battery_name);
    std::fs::read_dir(&dir).ok().is_some_and(|entries| {
        entries
            .filter_map(|e| e.ok())
            .any(|e| e.path().join("test_case").is_symlink())
    })
}

// ── harvest-bench project ──────────────────────────────────────────────

/// `harvest-bench/tests/<name>/` holding a `test_case/` (the C library to
/// translate) and a `gtest_suite/` (the upstream suite the runner links against
/// the translated cdylib by ABI).
#[derive(Debug, Clone)]
pub struct HarvestBenchProject {
    name: String,
    test_case: PathBuf,
    gtest_suite: PathBuf,
}

impl HarvestBenchProject {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn test_case(&self) -> &Path {
        &self.test_case
    }
    pub fn gtest_suite(&self) -> &Path {
        &self.gtest_suite
    }

    /// Resolve a single named project under `tests_dir` (= harvest-bench/tests).
    pub fn resolve(tests_dir: &Path, name: &str) -> Result<Self> {
        let root = tests_dir.join(name);
        let test_case = root.join("test_case");
        let gtest_suite = root.join("gtest_suite");
        anyhow::ensure!(
            test_case.is_dir(),
            "harvest-bench test_case not found: {}",
            test_case.display()
        );
        anyhow::ensure!(
            gtest_suite.is_dir(),
            "harvest-bench gtest_suite not found: {}",
            gtest_suite.display()
        );
        Ok(Self {
            name: name.to_string(),
            test_case,
            gtest_suite,
        })
    }

    pub fn discover(tests_dir: &Path) -> Result<Vec<Self>> {
        anyhow::ensure!(
            tests_dir.is_dir(),
            "harvest-bench tests dir not found: {} (did you `git submodule update --init`?)",
            tests_dir.display()
        );
        let mut names: Vec<String> = std::fs::read_dir(tests_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| {
                e.path().join("test_case").is_dir() && e.path().join("gtest_suite").is_dir()
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|n| Self::resolve(tests_dir, &n))
            .collect()
    }
}

pub fn discover(corpus_dir: &Path, battery_name: &str, filter: Option<&str>) -> Result<Battery> {
    let input_dir = corpus_dir.join("Public-Tests").join(battery_name);
    anyhow::ensure!(
        input_dir.is_dir(),
        "Battery not found: {}",
        input_dir.display()
    );

    let filter_re = filter.map(Regex::new).transpose()?;

    let mut symlink_map: HashMap<String, String> = HashMap::new(); // symlinked_name -> real_name
    let mut all_names: Vec<String> = Vec::new();

    let mut entries: Vec<_> = std::fs::read_dir(&input_dir)?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let test_case_path = entry.path().join("test_case");

        // `exists()`, not `is_dir()`: test_case may be a symlink.
        if !test_case_path.exists() || !entry.path().join("test_vectors").is_dir() {
            continue;
        }

        if let Some(ref re) = filter_re {
            if !re.is_match(&name) {
                continue;
            }
        }

        if test_case_path.is_symlink() {
            let real = std::fs::canonicalize(&test_case_path)
                .with_context(|| format!("resolving symlink for {name}"))?;
            // real is .../real_case/test_case — parent is the real case dir
            let real_name = real
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .with_context(|| format!("extracting real case name from {}", real.display()))?;
            symlink_map.insert(name.clone(), real_name);
        }

        all_names.push(name);
    }

    let mut shared_groups: HashMap<String, Vec<String>> = HashMap::new();
    for (symlinked, real) in &symlink_map {
        shared_groups
            .entry(real.clone())
            .or_default()
            .push(symlinked.clone());
    }

    let mut cases: Vec<Case> = Vec::new();
    let mut handled: std::collections::HashSet<String> = std::collections::HashSet::new();

    for name in &all_names {
        if handled.contains(name) {
            continue;
        }

        if symlink_map.contains_key(name) {
            // This is a symlinked config — will be handled when we process its real case
            continue;
        }

        if let Some(config_names) = shared_groups.get(name) {
            let mut configs = Vec::new();
            for cn in config_names {
                let cfg = build_config(&input_dir, cn)?;
                configs.push(cfg);
                handled.insert(cn.clone());
            }
            configs.sort_by(|a, b| a.name.cmp(&b.name));
            cases.push(Case::SharedSource(SharedSourceGroup {
                real_case: name.clone(),
                configs,
            }));
            handled.insert(name.clone());
        } else {
            cases.push(Case::Independent(IndependentCase {
                name: name.clone(),
                is_lib: name.ends_with("_lib"),
            }));
            handled.insert(name.clone());
        }
    }

    Ok(Battery {
        name: battery_name.to_string(),
        cases,
    })
}

fn build_config(input_dir: &Path, name: &str) -> Result<Config> {
    let is_lib = name.ends_with("_lib");
    let lib_name = extract_lib_name(input_dir, name);
    let features = extract_features(input_dir, name).unwrap_or_default();
    Ok(Config {
        name: name.to_string(),
        features,
        is_lib,
        lib_name,
    })
}

pub fn extract_features_from_path(presets_path: &Path) -> Result<Vec<String>> {
    if !presets_path.exists() {
        return Ok(vec![]);
    }
    let data: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(presets_path)
            .with_context(|| format!("reading {}", presets_path.display()))?,
    )?;

    let cv = data
        .pointer("/configurePresets/1/cacheVariables")
        .and_then(|v| v.as_object());

    let Some(cv) = cv else {
        return Ok(vec![]);
    };

    let mut features = Vec::new();
    for key in ["HASH_BACKEND", "THASH", "SECPAR"] {
        if let Some(val) = cv.get(key).and_then(|v| v.as_str()) {
            let lower = val.to_lowercase();
            if !lower.is_empty() {
                features.push(lower);
            }
        }
    }
    Ok(features)
}

fn extract_features(input_dir: &Path, case_name: &str) -> Result<Vec<String>> {
    extract_features_from_path(&input_dir.join(case_name).join("CMakePresets.json"))
}

static LIB_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"library:\s*"([^"]+)""#).expect("literal pattern"));

pub fn extract_lib_name(input_dir: &Path, case_name: &str) -> Option<String> {
    let runner_main = input_dir.join(case_name).join("runner/src/main.rs");
    let content = std::fs::read_to_string(&runner_main).ok()?;
    LIB_NAME_RE
        .captures(&content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Raw names first, then composite names like "sphincs-blake-128f".
pub fn resolve_features(cargo_toml_path: &Path, raw_features: &[String]) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(cargo_toml_path)?;
    let doc: toml_edit::DocumentMut = content.parse()?;

    let defined: std::collections::HashSet<String> = doc
        .get("features")
        .and_then(|f| f.as_table())
        .map(|t| {
            t.iter()
                .map(|(k, _)| k.to_string())
                .filter(|k| k != "default")
                .collect()
        })
        .unwrap_or_default();

    let mut resolved = Vec::new();
    for feat in raw_features {
        if defined.contains(feat) {
            resolved.push(feat.clone());
        } else {
            // Try composite: sphincs-{backend}-{secpar}
            let composite = format!(
                "sphincs-{}-{}",
                raw_features.first().unwrap_or(&String::new()),
                feat
            );
            if defined.contains(&composite) {
                resolved.push(composite);
            }
        }
    }
    Ok(resolved)
}

pub fn all_case_names(battery: &Battery) -> Vec<String> {
    let mut names = Vec::new();
    for case in &battery.cases {
        match case {
            Case::Independent(c) => names.push(c.name.clone()),
            Case::SharedSource(g) => {
                names.push(g.real_case.clone());
                for cfg in &g.configs {
                    names.push(cfg.name.clone());
                }
            }
        }
    }
    names
}

/// The Kiro Power add-on rate, and the only bridge between the two money types below.
const USD_PER_CREDIT: f64 = 0.04;

/// LLM API credits consumed by a single agent invocation. The field is private so the
/// only way to read the number is [`Credits::as_f64`], which cannot be reached by
/// accident where a dollar amount was meant.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Credits(f64);

/// US dollars — deliberately NOT the same type as [`Credits`]. Both end up in the paper's
/// cost table and they differ by 25x, so a bare `f64` for each makes a 25x error a
/// type-correct expression.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Usd(f64);

impl Credits {
    pub fn new(credits: f64) -> Self {
        Self(credits)
    }

    pub fn as_f64(self) -> f64 {
        self.0
    }

    /// The single place the rate is applied, so no other expression in the crate can
    /// produce a dollar figure and none can spell a dollar figure as a credit count.
    pub fn to_usd(self) -> Usd {
        Usd(self.0 * USD_PER_CREDIT)
    }
}

impl Usd {
    pub fn as_f64(self) -> f64 {
        self.0
    }
}

/// Every field is as the provider reported it; none is derived.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

/// Provenance for ONE agent CLI invocation. `--agent claude` passes no
/// `--model`, so the model is whatever the CLI defaulted to at invocation time,
/// and the CLI auto-updates mid-sweep — it must be recorded per run.
///
/// EVERY OPTIONAL FIELD MUST SERIALIZE AS ABSENT WHEN UNKNOWN, never as zero.
/// kiro-cli reports credits and no dollars; claude the reverse. A
/// `total_cost_usd: 0.0` on a kiro run is a measurement nobody made, and it
/// would silently average into a cost table.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentRunMeta {
    // Not Option: existing result.json files carry these, and check_enrichment
    // requires credits for Agent::Kiro.
    pub credits: Credits,
    pub wall_secs: u64,

    // ── identity ────────────────────────────────────────────────────────────
    /// Requested model, from the `system`/`init` record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Agent CLI version, from the same record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    /// From `modelUsage`. May be a SUPERSET of `model`: `Task` subagents can
    /// bill a different one, and a verify session spawns many.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models_billed: Vec<String>,

    // ── how it ended ────────────────────────────────────────────────────────
    /// `completed` | `api_error`. See [`crate::domain::health`]: this is the
    /// discriminator, and `subtype` reads "success" even on a 403.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error_status: Option<i64>,
    /// Process exit status, from the sibling `verification.json` / `translation.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,

    // ── effort and cost ─────────────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

/// kiro-cli writes prose; claude writes stream-json. A log in neither format
/// yields `None` rather than a zero-filled record.
pub fn extract_agent_meta(log_path: &Path) -> Option<AgentRunMeta> {
    extract_kiro_meta(log_path).or_else(|| extract_stream_json_meta(log_path))
}

static KIRO_CREDITS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Credits:\s*([0-9.]+).*?Time:\s*(.+)").expect("literal pattern"));

/// Tail-only: the `Credits:` line is last, and this runs once per case over
/// logs that reach 10+ MB.
fn extract_kiro_meta(log_path: &Path) -> Option<AgentRunMeta> {
    let data = crate::agent_health::read_tail(log_path).ok()?;
    let caps = KIRO_CREDITS_RE.captures_iter(&data).last()?;
    let credits = Credits::new(caps[1].parse().ok()?);
    let wall_secs = parse_duration(&caps[2]);
    Some(AgentRunMeta {
        credits,
        wall_secs,
        ..Default::default()
    })
}

/// Identity comes from the head `system`/`init` record, cost and effort from the
/// tail `result` record. Non-JSON lines must be skipped: the harness pipes the
/// agent through `2>&1 | tee`, so stderr is interleaved and a whole-file parse
/// dies on the first such line.
fn extract_stream_json_meta(log_path: &Path) -> Option<AgentRunMeta> {
    let mut m = AgentRunMeta::default();
    let mut found = false;

    let head = read_head(log_path).unwrap_or_default();
    if let Some(init) = find_record(&head, false, |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("system")
            && v.get("subtype").and_then(|t| t.as_str()) == Some("init")
    }) {
        m.model = init
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        m.cli_version = init
            .get("claude_code_version")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        found = true;
    }

    let tail = crate::agent_health::read_tail(log_path).unwrap_or_default();
    if let Some(t) = find_record(&tail, true, |v| {
        v.get("type").and_then(|x| x.as_str()) == Some("result")
    }) {
        found = true;
        m.terminal_reason = t
            .get("terminal_reason")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        // Present-but-null on success, so as_i64 yields None rather than 0.
        m.api_error_status = t.get("api_error_status").and_then(|v| v.as_i64());
        m.num_turns = t.get("num_turns").and_then(|v| v.as_u64());
        m.duration_ms = t.get("duration_ms").and_then(|v| v.as_u64());
        m.total_cost_usd = t.get("total_cost_usd").and_then(|v| v.as_f64());
        if let Some(mu) = t.get("modelUsage").and_then(|v| v.as_object()) {
            m.models_billed = mu.keys().cloned().collect();
            m.models_billed.sort();
        }
        if let Some(u) = t.get("usage").and_then(|v| v.as_object()) {
            let g = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            let tk = Tokens {
                input: g("input_tokens"),
                output: g("output_tokens"),
                cache_creation: g("cache_creation_input_tokens"),
                cache_read: g("cache_read_input_tokens"),
            };
            // All-zero means the provider reported nothing usable.
            if tk != Tokens::default() {
                m.tokens = Some(tk);
            }
        }
        if let Some(ms) = m.duration_ms {
            m.wall_secs = ms / 1000;
        }
    }

    // Written beside the log by agents::run::write_phase_metrics.
    m.exit_code = log_path
        .parent()
        .and_then(|logs| logs.parent())
        .and_then(|phase| {
            ["verification.json", "translation.json"]
                .iter()
                .map(|f| phase.join(f))
                .find(|p| p.is_file())
        })
        .and_then(|p| crate::agent_health::exit_code(&p));

    if found {
        Some(m)
    } else {
        None
    }
}

/// First 256 KB. The `init` record is near the top, but a SessionStart hook can
/// emit several sizeable records ahead of it.
fn read_head(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// First (or, with `last`, final) JSON line satisfying `pred`.
fn find_record(
    hay: &str,
    last: bool,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> Option<serde_json::Value> {
    let parse = |l: &str| serde_json::from_str::<serde_json::Value>(l).ok();
    if last {
        hay.lines()
            .rev()
            .filter(|l| l.starts_with('{'))
            .filter_map(parse)
            .find(|v| pred(v))
    } else {
        hay.lines()
            .filter(|l| l.starts_with('{'))
            .filter_map(parse)
            .find(|v| pred(v))
    }
}

fn parse_duration(s: &str) -> u64 {
    let mut secs = 0u64;
    for part in s.split_whitespace() {
        if let Some(m) = part.strip_suffix('m') {
            secs += m.parse::<u64>().unwrap_or(0) * 60;
        } else if let Some(s_val) = part.strip_suffix('s') {
            secs += s_val.parse::<u64>().unwrap_or(0);
        }
    }
    secs
}

pub struct Paths {
    /// The ACTUAL repository root, not a dataset/agent dir: `crate::io::sandbox`'s
    /// deny list must cover the corpus (the graded oracle), not just a results
    /// subdirectory.
    pub repo_root: PathBuf,
    pub corpus_dir: PathBuf,
    pub results_dir: PathBuf,
    pub prompts_dir: PathBuf,
    pub agent: Agent,
    /// KEPT, not merely used to derive the dirs above: a phase's wall-clock ceiling depends on it, and
    /// recovering it from `results_dir`'s spelling would be a string where a type belongs.
    pub dataset: Dataset,
    /// How this run is spelled in the results tree, the cache key and the recorded
    /// provenance — one value, so the three cannot disagree.
    pub agent_key: crate::cache::AgentKey,
    pub model: Option<String>,
    /// A required parameter of `new` rather than a default, so the compiler names
    /// every construction site that would otherwise silently get read-write
    /// caching.
    pub cache_mode: crate::cache::Mode,
    /// Whether the operator accepted running without an enforceable sandbox.
    pub enforcement: crate::io::sandbox::Enforcement,
}

impl Paths {
    pub fn new(
        repo_root: &Path,
        agent: Agent,
        dataset: Dataset,
        model: Option<&str>,
        cache_mode: crate::cache::Mode,
        enforcement: crate::io::sandbox::Enforcement,
    ) -> Result<Self> {
        // The same value the cache key and every `"agent"` field use: a second table
        // here is what let 208 result files record an agent name no `--agent` value
        // spells, under a `codex-gpt55/` directory.
        let agent_key = crate::cache::AgentKey::new(
            agent,
            model,
            crate::agents::invocation::resolved_model(agent, model)?,
        )?;
        let (corpus_dir, results_dir) = match dataset {
            Dataset::TestCorpus => (
                repo_root.join("test-corpus"),
                repo_root
                    .join("results/Test-Corpus")
                    .join(agent_key.as_str()),
            ),
            Dataset::HarvestBench => (
                repo_root.join("harvest-bench/tests"),
                repo_root
                    .join("results/HarvestBench")
                    .join(agent_key.as_str()),
            ),
        };
        let prompts_dir = match agent {
            // Codex's own set: `prompts/claude/*.md` carry a sub-agent protocol built on Claude
            // Code's Task tool, so every codex run so far was told to use a tool it cannot call.
            Agent::CodexGpt56Sol => repo_root.join("prompts/codex"),
            Agent::Claude
            | Agent::ClaudeCombined
            | Agent::ClaudeMinimal
            | Agent::ClaudeNoIter
            | Agent::ClaudeNoFeatures
            | Agent::ClaudeNoSubtask
            | Agent::ClaudeCrossPrompt
            | Agent::CodexGpt55
            | Agent::CodexGpt54
            | Agent::OpenCode => match dataset {
                // harvest-bench cases are libraries, which the test-corpus
                // prompts already dispatch on; no separate prompt set needed.
                Dataset::TestCorpus | Dataset::HarvestBench => repo_root.join("prompts/claude"),
            },
            Agent::Kimi | Agent::Oneshot => repo_root.join("prompts/oneshot"),
            _ => match dataset {
                Dataset::TestCorpus | Dataset::HarvestBench => {
                    repo_root.join("prompts/kiro/test-corpus")
                }
            },
        };
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            corpus_dir,
            results_dir,
            cache_mode,
            enforcement,
            prompts_dir,
            agent,
            dataset,
            agent_key,
            model: model.map(String::from),
        })
    }

    pub fn input_dir(&self, battery: &str) -> PathBuf {
        self.corpus_dir.join("Public-Tests").join(battery)
    }

    pub fn output_dir(&self, name: &str) -> PathBuf {
        self.results_dir.join(name)
    }

    pub fn case_dir(&self, battery: &str, case: &str) -> PathBuf {
        self.results_dir.join(battery).join(case)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs as unix_fs;

    /// Regression: the old bash set REAL_CASE globally and so skipped every
    /// non-real case in a mixed battery (B02_synthetic) from verify.
    #[test]
    fn mixed_battery_separates_independent_and_shared() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let battery = tmp.path().join("Public-Tests/mixed");

        for name in ["arity_lib", "strcmp", "cleanup_lib"] {
            let case = battery.join(name);
            fs::create_dir_all(case.join("test_case")).unwrap();
            fs::create_dir_all(case.join("test_vectors")).unwrap();
        }

        let real = battery.join("macrodepth_add_5");
        fs::create_dir_all(real.join("test_case")).unwrap();
        fs::create_dir_all(real.join("test_vectors")).unwrap();

        for name in ["macrodepth_mul_4", "macrodepth_sub_6"] {
            let case = battery.join(name);
            fs::create_dir_all(&case).unwrap();
            fs::create_dir_all(case.join("test_vectors")).unwrap();
            unix_fs::symlink(real.join("test_case"), case.join("test_case")).unwrap();
        }

        let result = discover(tmp.path(), "mixed", None).unwrap();

        let mut independent_count = 0;
        let mut shared_count = 0;
        for case in &result.cases {
            match case {
                Case::Independent(_) => independent_count += 1,
                Case::SharedSource(g) => {
                    shared_count += 1;
                    assert_eq!(g.real_case, "macrodepth_add_5");
                    assert_eq!(g.configs.len(), 2);
                    let names: Vec<_> = g.configs.iter().map(|c| c.name.as_str()).collect();
                    assert!(names.contains(&"macrodepth_mul_4"));
                    assert!(names.contains(&"macrodepth_sub_6"));
                }
            }
        }
        assert_eq!(independent_count, 3, "should have 3 independent cases");
        assert_eq!(shared_count, 1, "should have 1 shared-source group");
    }

    #[test]
    fn a_verified_dir_holding_only_logs_is_not_a_crate() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let case = tmp.path().join("pcre2");
        fs::create_dir_all(case.join("verified/logs")).unwrap();
        fs::write(case.join("verified/logs/verify.log"), "transcript").unwrap();
        fs::write(case.join("verified/verification.json"), "{}").unwrap();
        fs::create_dir_all(case.join("translated")).unwrap();
        fs::write(case.join("translated/Cargo.toml"), "[package]").unwrap();

        assert!(
            case.join("verified").is_dir(),
            "fixture must retain the trap"
        );
        assert!(!has_crate(&case.join("verified")));
        assert!(has_crate(&case.join("translated")));
    }

    #[test]
    fn no_symlinks_all_independent() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let battery = tmp.path().join("Public-Tests/simple");

        for name in ["case_a", "case_b_lib", "case_c"] {
            let case = battery.join(name);
            fs::create_dir_all(case.join("test_case")).unwrap();
            fs::create_dir_all(case.join("test_vectors")).unwrap();
        }

        let result = discover(tmp.path(), "simple", None).unwrap();
        assert_eq!(result.cases.len(), 3);
        for case in &result.cases {
            assert!(matches!(case, Case::Independent(_)));
        }
    }

    #[test]
    fn lib_detection_from_name() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let battery = tmp.path().join("Public-Tests/libtest");

        for name in ["foo_lib", "bar", "baz_lib"] {
            let case = battery.join(name);
            fs::create_dir_all(case.join("test_case")).unwrap();
            fs::create_dir_all(case.join("test_vectors")).unwrap();
        }

        let result = discover(tmp.path(), "libtest", None).unwrap();
        for case in &result.cases {
            if let Case::Independent(c) = case {
                assert_eq!(
                    c.is_lib,
                    c.name.ends_with("_lib"),
                    "is_lib wrong for {}",
                    c.name
                );
            }
        }
    }

    #[test]
    fn extract_lib_name_from_runner() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let runner = tmp.path().join("mycase/runner/src");
        fs::create_dir_all(&runner).unwrap();
        fs::write(
            runner.join("main.rs"),
            r#"fn main() { let lib = cando2::Library::new(library: "blake", path: "..."); }"#,
        )
        .unwrap();

        let result = extract_lib_name(tmp.path(), "mycase");
        assert_eq!(result, Some("blake".to_string()));
    }

    #[test]
    fn extract_lib_name_missing_runner() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        assert_eq!(extract_lib_name(tmp.path(), "nonexistent"), None);
    }

    #[test]
    fn filter_regex_selects_matching_cases() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let battery = tmp.path().join("Public-Tests/filtered");

        for name in ["alpha", "beta_lib", "gamma"] {
            let case = battery.join(name);
            fs::create_dir_all(case.join("test_case")).unwrap();
            fs::create_dir_all(case.join("test_vectors")).unwrap();
        }

        let result = discover(tmp.path(), "filtered", Some("_lib$")).unwrap();
        assert_eq!(result.cases.len(), 1);
        if let Case::Independent(c) = &result.cases[0] {
            assert_eq!(c.name, "beta_lib");
        } else {
            panic!("expected Independent");
        }
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    const INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/w","session_id":"s1","tools":["Bash"],"model":"global.anthropic.claude-opus-5[1m]","permissionMode":"bypassPermissions","claude_code_version":"2.1.231.653"}"#;
    const DONE: &str = r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","api_error_status":null,"duration_ms":1029979,"num_turns":56,"total_cost_usd":4.12094025,"usage":{"input_tokens":669,"output_tokens":40232,"cache_creation_input_tokens":111263,"cache_read_input_tokens":9004512},"modelUsage":{"global.anthropic.claude-opus-5[1m]":{"inputTokens":669}},"session_id":"s1"}"#;
    const DEAD: &str = r#"{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error","api_error_status":403,"duration_ms":4569000,"num_turns":193,"total_cost_usd":90.00792925,"result":"Failed to authenticate. API Error: 403 ... expired"}"#;

    fn log(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let logs = dir.join("verified/logs");
        std::fs::create_dir_all(&logs).unwrap();
        let p = logs.join("verify.log");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn records_the_model_and_cli_version_from_the_init_record() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let p = log(tmp.path(), &format!("{INIT}\n{DONE}\n"));
        let m = extract_agent_meta(&p).expect("stream-json is recognised");
        assert_eq!(
            m.model.as_deref(),
            Some("global.anthropic.claude-opus-5[1m]")
        );
        assert_eq!(m.cli_version.as_deref(), Some("2.1.231.653"));
    }

    #[test]
    fn records_cost_turns_and_tokens_from_the_terminal_record() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let p = log(tmp.path(), &format!("{INIT}\n{DONE}\n"));
        let m = extract_agent_meta(&p).unwrap();
        assert_eq!(m.num_turns, Some(56));
        assert_eq!(m.duration_ms, Some(1029979));
        assert_eq!(m.wall_secs, 1029, "derived from duration_ms");
        assert!((m.total_cost_usd.unwrap() - 4.12094025).abs() < 1e-9);
        let t = m.tokens.expect("token counts present");
        assert_eq!((t.input, t.output), (669, 40232));
        assert_eq!((t.cache_creation, t.cache_read), (111263, 9004512));
    }

    #[test]
    fn models_billed_comes_from_model_usage_not_the_requested_model() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let two = DONE.replace(
            r#""modelUsage":{"global.anthropic.claude-opus-5[1m]":{"inputTokens":669}}"#,
            r#""modelUsage":{"global.anthropic.claude-opus-5[1m]":{"inputTokens":1},"claude-haiku-4-5":{"inputTokens":2}}"#,
        );
        let p = log(tmp.path(), &format!("{INIT}\n{two}\n"));
        let m = extract_agent_meta(&p).unwrap();
        assert_eq!(
            m.models_billed,
            vec!["claude-haiku-4-5", "global.anthropic.claude-opus-5[1m]"]
        );
        assert_eq!(
            m.model.as_deref(),
            Some("global.anthropic.claude-opus-5[1m]"),
            "requested stays distinct"
        );
    }

    #[test]
    fn absent_is_absent_never_zero() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        // Credits and time MUST stay on one line: the regex `.` does not cross a
        // newline, so a two-line fixture silently fails to match.
        let p = log(tmp.path(), "▸ Credits: 1.25 • Time: 3m 4s\n");
        let m = extract_agent_meta(&p).expect("kiro log is recognised");
        assert_eq!(m.credits.as_f64(), 1.25);
        assert_eq!(m.wall_secs, 184);
        assert!(m.total_cost_usd.is_none(), "no dollar cost for kiro");
        assert!(m.model.is_none());
        assert!(m.tokens.is_none());
        let json = serde_json::to_string(&m).unwrap();
        for k in [
            "total_cost_usd",
            "model",
            "tokens",
            "num_turns",
            "exit_code",
        ] {
            assert!(
                !json.contains(k),
                "{k} must be omitted, not zero-valued: {json}"
            );
        }
    }

    #[test]
    fn api_error_status_null_on_success_is_none_not_zero() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let p = log(tmp.path(), &format!("{INIT}\n{DONE}\n"));
        let m = extract_agent_meta(&p).unwrap();
        assert!(
            m.api_error_status.is_none(),
            "present-but-null must not become 0"
        );
        assert_eq!(m.terminal_reason.as_deref(), Some("completed"));
    }

    #[test]
    fn a_dead_run_records_its_cost_and_its_reason() {
        // jansson really burned $90 over 193 turns and then died on a 403: the
        // cost was real, the result was not.
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let p = log(tmp.path(), &format!("{INIT}\n{DEAD}\n"));
        let m = extract_agent_meta(&p).unwrap();
        assert_eq!(m.terminal_reason.as_deref(), Some("api_error"));
        assert_eq!(m.api_error_status, Some(403));
        assert!((m.total_cost_usd.unwrap() - 90.00792925).abs() < 1e-9);
        assert_eq!(m.num_turns, Some(193));
    }

    #[test]
    fn exit_code_is_read_from_the_sibling_metrics_file() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let p = log(tmp.path(), &format!("{INIT}\n{DEAD}\n"));
        std::fs::write(
            tmp.path().join("verified/verification.json"),
            r#"{"exit_code":1,"success":true,"duration_secs":4569}"#,
        )
        .unwrap();
        let m = extract_agent_meta(&p).unwrap();
        assert_eq!(
            m.exit_code,
            Some(1),
            "already on disk, previously read by nothing"
        );
    }

    #[test]
    fn interleaved_stderr_does_not_defeat_extraction() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let body = format!("warning: bwrap not installed\n{INIT}\nnote: noise\n{DONE}\n");
        let p = log(tmp.path(), &body);
        let m = extract_agent_meta(&p).unwrap();
        assert_eq!(
            m.model.as_deref(),
            Some("global.anthropic.claude-opus-5[1m]")
        );
        assert_eq!(m.num_turns, Some(56));
    }

    #[test]
    fn a_log_in_neither_format_yields_none() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let p = log(tmp.path(), "just some prose, no credits, no json\n");
        assert!(
            extract_agent_meta(&p).is_none(),
            "must not fabricate a zero record"
        );
    }
}
