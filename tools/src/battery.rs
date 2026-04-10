use crate::cli::{Agent, Dataset};
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A test case that is independently translated and verified.
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

/// A discovered case — either independent or part of a shared-source group.
#[derive(Debug, Clone)]
pub enum Case {
    Independent(IndependentCase),
    SharedSource(SharedSourceGroup),
}

/// A battery with all its discovered cases.
#[derive(Debug)]
pub struct Battery {
    pub name: String,
    pub cases: Vec<Case>,
}

/// Discover all cases in a battery, resolving symlinks to group shared-source cases.
/// List all battery names available in the corpus.
pub fn all_batteries(corpus_dir: &Path) -> Result<Vec<String>> {
    let public_tests = corpus_dir.join("Public-Tests");
    anyhow::ensure!(public_tests.is_dir(), "Public-Tests not found: {}", public_tests.display());

    let mut batteries: Vec<String> = std::fs::read_dir(&public_tests)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map_or(false, |t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    batteries.sort();
    Ok(batteries)
}

/// Quick check: does this battery contain shared-source groups (symlinked test_case)?
pub fn has_shared_source_groups(corpus_dir: &Path, battery_name: &str) -> bool {
    let dir = corpus_dir.join("Public-Tests").join(battery_name);
    std::fs::read_dir(&dir).ok().map_or(false, |entries| {
        entries.filter_map(|e| e.ok()).any(|e| e.path().join("test_case").is_symlink())
    })
}

// ── CRUST-bench project (validated newtype) ────────────────────────────

const CRUST_SKIP: &[&str] = &[
    "Genetic_neural_network_for_simple_control", // C test >120s with -O2, https://github.com/anirudhkhatry/CRUST-bench/issues/40
    "Holdem_Odds", // contradictory tests, https://github.com/anirudhkhatry/CRUST-bench/issues/37
    "VaultSync", // test hardcodes /home/elhalili/... absolute path, only passes with leftover state
    "bitset", // test uses bs.test() but C checks raw bits, https://github.com/anirudhkhatry/CRUST-bench/issues/41
    "clog", // THIS_FILE hardcodes C filename, https://github.com/anirudhkhatry/CRUST-bench/issues/39
];

/// A validated CRUST project. Can only be constructed through `discover()` or
/// `validated()`, which enforce the skip list and resolve paths.
#[derive(Debug, Clone)]
pub struct CrustProject {
    name: String,
    scaffold: PathBuf,
    c_source: PathBuf,
}

impl CrustProject {
    pub fn name(&self) -> &str { &self.name }
    pub fn scaffold(&self) -> &Path { &self.scaffold }
    pub fn c_source(&self) -> &Path { &self.c_source }

    /// Discover all valid CRUST projects, applying skip list and optional limit.
    pub fn discover(datasets_dir: &Path, limit: Option<usize>) -> Result<Vec<Self>> {
        let rbench = datasets_dir.join("RBench");
        anyhow::ensure!(rbench.is_dir(), "RBench not found: {}", rbench.display());

        let mut names: Vec<String> = std::fs::read_dir(&rbench)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map_or(false, |t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !CRUST_SKIP.contains(&n.as_str()))
            .collect();
        names.sort();
        if let Some(n) = limit { names.truncate(n); }

        names.into_iter()
            .map(|name| Self::resolve(datasets_dir, name))
            .collect()
    }

    /// Validate a single project name against the skip list and resolve paths.
    pub fn validated(datasets_dir: &Path, name: &str) -> Result<Self> {
        anyhow::ensure!(
            !CRUST_SKIP.contains(&name),
            "{name} is in the CRUST skip list"
        );
        Self::resolve(datasets_dir, name.to_string())
    }

    fn resolve(datasets_dir: &Path, name: String) -> Result<Self> {
        let scaffold = datasets_dir.join("RBench").join(&name);
        anyhow::ensure!(scaffold.is_dir(), "RBench scaffold not found: {}", scaffold.display());

        let c_source = Self::find_cbench(datasets_dir, &name)
            .with_context(|| format!("CBench source not found for {name}"))?;

        Ok(Self { name, scaffold, c_source })
    }

    fn find_cbench(datasets_dir: &Path, project: &str) -> Option<PathBuf> {
        let cbench = datasets_dir.join("CBench");
        for candidate in [
            project.to_string(),
            project.replace('_', "-"),
            project.strip_prefix("proj_").unwrap_or(project).replace('_', "-"),
        ] {
            let p = cbench.join(&candidate);
            if p.is_dir() { return Some(p); }
        }
        None
    }
}

pub fn discover(corpus_dir: &Path, battery_name: &str, filter: Option<&str>) -> Result<Battery> {
    let input_dir = corpus_dir.join("Public-Tests").join(battery_name);
    anyhow::ensure!(input_dir.is_dir(), "Battery not found: {}", input_dir.display());

    let filter_re = filter.map(|f| Regex::new(f)).transpose()?;

    // Phase 1: scan all cases, resolve symlinks
    let mut symlink_map: HashMap<String, String> = HashMap::new(); // symlinked_name -> real_name
    let mut all_names: Vec<String> = Vec::new();

    let mut entries: Vec<_> = std::fs::read_dir(&input_dir)?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let test_case_path = entry.path().join("test_case");

        // Must have test_case/ (dir or symlink) and test_vectors/
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

    // Phase 2: group into Case variants
    // Collect real_case -> Vec<symlinked configs>
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
            // This is the real case of a shared-source group
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
            // Independent case
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

/// Extract features from a CMakePresets.json path directly.
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

/// Extract features from CMakePresets.json cache variables.
fn extract_features(input_dir: &Path, case_name: &str) -> Result<Vec<String>> {
    extract_features_from_path(&input_dir.join(case_name).join("CMakePresets.json"))
}

/// Extract [lib] name from the test corpus runner/src/main.rs.
pub fn extract_lib_name(input_dir: &Path, case_name: &str) -> Option<String> {
    let runner_main = input_dir.join(case_name).join("runner/src/main.rs");
    let content = std::fs::read_to_string(&runner_main).ok()?;
    let re = Regex::new(r#"library:\s*"([^"]+)""#).ok()?;
    re.captures(&content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Resolve features against actual Cargo.toml feature definitions.
/// Tries raw names first, then composite names like "sphincs-blake-128f".
pub fn resolve_features(
    cargo_toml_path: &Path,
    raw_features: &[String],
) -> Result<Vec<String>> {
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

/// Get all case names in a battery (flat list, for iteration).
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

/// Paths helper.
/// Immutable translation output directory. Created once by translate, never modified after.
#[derive(Debug, Clone)]
pub struct TranslateDir(PathBuf);

/// Mutable verify workspace. Can be wiped and recreated from a [`TranslateDir`].
#[derive(Debug, Clone)]
pub struct VerifyDir(PathBuf);

macro_rules! impl_dir_newtype {
    ($T:ty) => {
        impl $T {
            pub fn join(&self, path: impl AsRef<Path>) -> PathBuf { self.0.join(path) }
            pub fn exists(&self) -> bool { self.0.exists() }
            pub fn is_dir(&self) -> bool { self.0.is_dir() }
        }
        impl AsRef<Path> for $T {
            fn as_ref(&self) -> &Path { &self.0 }
        }
    };
}

impl_dir_newtype!(TranslateDir);
impl_dir_newtype!(VerifyDir);

/// LLM API credits consumed by a single agent invocation.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Credits(pub f64);

/// Metadata extracted from an agent run log (kiro-cli / claude).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentRunMeta {
    pub credits: Credits,
    pub wall_secs: u64,
}

/// Parse the last `▸ Credits: X.XX • Time: Xm Xs` line from an agent log.
pub fn extract_agent_meta(log_path: &Path) -> Option<AgentRunMeta> {
    let data = std::fs::read_to_string(log_path).ok()?;
    let re = Regex::new(r"Credits:\s*([0-9.]+).*?Time:\s*(.+)").ok()?;
    let caps = re.captures_iter(&data).last()?;
    let credits = Credits(caps[1].parse().ok()?);
    let wall_secs = parse_duration(&caps[2]);
    Some(AgentRunMeta { credits, wall_secs })
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

/// Unsafe usage counts extracted via AST (`syn`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UnsafeCounts {
    /// `unsafe { ... }` blocks
    pub blocks: usize,
    /// `unsafe fn` declarations
    pub fns: usize,
    /// `unsafe impl` blocks
    pub impls: usize,
    /// Total lines inside unsafe blocks/fns/impls
    pub lines: usize,
}

impl UnsafeCounts {
    pub fn total(&self) -> usize { self.blocks + self.fns + self.impls }
}

/// Count unsafe constructs in `*.rs` files under `src_dir`, excluding `bin/` and `tests/`.
pub fn count_unsafe(src_dir: &Path) -> UnsafeCounts {
    let mut counts = UnsafeCounts::default();
    let Ok(entries) = std::fs::read_dir(src_dir) else { return counts };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            if name == "bin" || name == "tests" { continue; }
            let sub = count_unsafe(&path);
            counts.blocks += sub.blocks;
            counts.fns += sub.fns;
            counts.impls += sub.impls;
            counts.lines += sub.lines;
        } else if path.extension().is_some_and(|x| x == "rs") {
            let Ok(src) = std::fs::read_to_string(&path) else { continue };
            let Ok(file) = syn::parse_file(&src) else { continue };
            let mut v = UnsafeVisitor::default();
            syn::visit::visit_file(&mut v, &file);
            counts.blocks += v.blocks;
            counts.fns += v.fns;
            counts.impls += v.impls;
            counts.lines += v.lines;
        }
    }
    counts
}

#[derive(Default)]
struct UnsafeVisitor {
    blocks: usize,
    fns: usize,
    impls: usize,
    lines: usize,
}

fn span_lines(open: proc_macro2::Span, close: proc_macro2::Span) -> usize {
    let start = open.start().line;
    let end = close.end().line;
    if end >= start { end - start + 1 } else { 1 }
}

impl<'ast> syn::visit::Visit<'ast> for UnsafeVisitor {
    fn visit_expr_unsafe(&mut self, node: &'ast syn::ExprUnsafe) {
        self.blocks += 1;
        let b = node.block.brace_token;
        self.lines += span_lines(b.span.open(), b.span.close());
        syn::visit::visit_expr_unsafe(self, node);
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if node.sig.unsafety.is_some() {
            self.fns += 1;
            let b = node.block.brace_token;
            self.lines += span_lines(b.span.open(), b.span.close());
        }
        syn::visit::visit_item_fn(self, node);
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if node.sig.unsafety.is_some() {
            self.fns += 1;
            let b = node.block.brace_token;
            self.lines += span_lines(b.span.open(), b.span.close());
        }
        syn::visit::visit_impl_item_fn(self, node);
    }
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if node.unsafety.is_some() {
            self.impls += 1;
            let b = node.brace_token;
            self.lines += span_lines(b.span.open(), b.span.close());
        }
        syn::visit::visit_item_impl(self, node);
    }
}

pub struct Paths {
    pub corpus_dir: PathBuf,
    pub results_dir: PathBuf,
    pub prompts_dir: PathBuf,
    pub agent: Agent,
    pub dataset: Dataset,
}

impl Paths {
    pub fn new(repo_root: &Path, agent: Agent, dataset: Dataset) -> Self {
        let agent_name = match agent {
            Agent::Kiro => "kiro",
            Agent::KiroTranslate => "kiro-translate",
            Agent::Claude => "claude",
            Agent::C2rust => "c2rust",
        };
        let (corpus_dir, results_dir) = match dataset {
            Dataset::TestCorpus => (
                repo_root.join("test-corpus"),
                repo_root.join("results/Test-Corpus").join(agent_name),
            ),
            Dataset::Crust => (
                repo_root.join("crust-bench/datasets"),
                repo_root.join("results/CRUST").join(agent_name),
            ),
            Dataset::BlindCrust => (
                repo_root.join("crust-bench/datasets"),
                repo_root.join("results/CRUST-blind").join(agent_name),
            ),
        };
        let prompts_dir = match agent {
            Agent::Claude => repo_root.join("scripts/prompts/claude"),
            _ => repo_root.join("scripts/prompts"),
        };
        Self { corpus_dir, results_dir, prompts_dir, agent, dataset }
    }

    pub fn input_dir(&self, battery: &str) -> PathBuf {
        self.corpus_dir.join("Public-Tests").join(battery)
    }

    pub fn output_dir(&self, name: &str) -> PathBuf {
        self.results_dir.join(name)
    }

    /// Blind CRUST: immutable translation output.
    pub fn translate_dir(&self, name: &str) -> TranslateDir {
        TranslateDir(self.results_dir.join(name).join("translate"))
    }

    /// Blind CRUST: mutable verify workspace (tests + possible src fixes).
    pub fn verify_dir(&self, name: &str) -> VerifyDir {
        VerifyDir(self.results_dir.join(name).join("verify"))
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

    /// THE BUG: B02_synthetic has 40 independent cases + 2 symlinked (macrodepth).
    /// Old bash set REAL_CASE globally and skipped ALL non-real cases from verify.
    /// This test ensures independent cases are NOT grouped with shared-source.
    #[test]
    fn mixed_battery_separates_independent_and_shared() {
        let tmp = tempfile::tempdir().unwrap();
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
    fn no_symlinks_all_independent() {
        let tmp = tempfile::tempdir().unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let battery = tmp.path().join("Public-Tests/libtest");

        for name in ["foo_lib", "bar", "baz_lib"] {
            let case = battery.join(name);
            fs::create_dir_all(case.join("test_case")).unwrap();
            fs::create_dir_all(case.join("test_vectors")).unwrap();
        }

        let result = discover(tmp.path(), "libtest", None).unwrap();
        for case in &result.cases {
            if let Case::Independent(c) = case {
                assert_eq!(c.is_lib, c.name.ends_with("_lib"), "is_lib wrong for {}", c.name);
            }
        }
    }

    #[test]
    fn extract_lib_name_from_runner() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = tmp.path().join("mycase/runner/src");
        fs::create_dir_all(&runner).unwrap();
        fs::write(
            runner.join("main.rs"),
            r#"fn main() { let lib = cando2::Library::new(library: "blake", path: "..."); }"#,
        ).unwrap();

        let result = extract_lib_name(tmp.path(), "mycase");
        assert_eq!(result, Some("blake".to_string()));
    }

    #[test]
    fn extract_lib_name_missing_runner() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(extract_lib_name(tmp.path(), "nonexistent"), None);
    }

    #[test]
    fn filter_regex_selects_matching_cases() {
        let tmp = tempfile::tempdir().unwrap();
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

    #[test]
    fn translate_and_verify_dirs_are_distinct_newtypes() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("crust-bench/datasets")).unwrap();
        fs::create_dir_all(tmp.path().join("results/CRUST-blind/kiro")).unwrap();
        fs::create_dir_all(tmp.path().join("scripts/prompts")).unwrap();

        let paths = Paths::new(tmp.path(), crate::cli::Agent::Kiro, crate::cli::Dataset::BlindCrust);

        let t = paths.translate_dir("vec");
        let v = paths.verify_dir("vec");

        assert_ne!(t.as_ref(), v.as_ref());
        assert!(t.as_ref().ends_with("vec/translate"));
        assert!(v.as_ref().ends_with("vec/verify"));
        assert_eq!(t.as_ref().parent(), v.as_ref().parent());
    }

    #[test]
    fn verify_wipe_preserves_translate() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("myproj");
        let translate = project.join("translate");
        let verify = project.join("verify");

        fs::create_dir_all(translate.join("src")).unwrap();
        fs::write(translate.join("Cargo.toml"), "[package]\nname = \"x\"").unwrap();
        fs::write(translate.join("src/lib.rs"), "pub fn f() {}").unwrap();

        fs::create_dir_all(verify.join("src/bin")).unwrap();
        fs::write(verify.join("src/bin/test_f.rs"), "#[test] fn t() {}").unwrap();

        // Wipe verify (simulates --force)
        fs::remove_dir_all(&verify).unwrap();

        // Translate is untouched
        assert!(translate.join("Cargo.toml").exists());
        assert_eq!(fs::read_to_string(translate.join("src/lib.rs")).unwrap(), "pub fn f() {}");
    }
}
