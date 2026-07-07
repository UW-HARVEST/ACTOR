use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

#[derive(Deserialize)]
struct Summary {
    cases_passed: u32,
    cases_tested: u32,
    vectors_passed: u32,
    vectors_failed: u32,
}

#[derive(Deserialize)]
struct CaseResult {
    passed: bool,
    /// Present and equal to "build failed" when the crate did not compile.
    /// Any other value (or absent) means the crate compiled.
    error: Option<String>,
    loc: Option<Loc>,
    #[serde(rename = "unsafe")]
    unsafe_: Option<Unsafe>,
}

#[derive(Deserialize)]
struct Loc {
    code: u32,
}

#[derive(Deserialize)]
struct Unsafe {
    lines: u32,
}

struct BatteryRow {
    agent: String,
    battery: String,
    cases_passed: u32,
    cases_tested: u32,
    /// Cases whose translated crate compiled (error != "build failed").
    cases_built: u32,
    vectors_passed: u32,
    vectors_total: u32,
    c_loc: u32,
    total_loc: u32,
    unsafe_lines: u32,
}

/// Generate markdown tables from results/Test-Corpus/ into tables/.
pub fn generate(repo_root: &Path) -> Result<()> {
    let results_dir = repo_root.join("results/Test-Corpus");
    let test_corpus_dir = repo_root.join("test-corpus/Public-Tests");
    let tables_dir = repo_root.join("tables");
    std::fs::create_dir_all(&tables_dir)?;

    // The Markdown table's "C LOC" column is read from the test-corpus submodule.
    // If it is not checked out, every C LOC would silently become 0 and clobber
    // the committed results.md. We therefore skip rewriting results.md when the
    // submodule is absent. The LaTeX table (tractor.tex) uses Rust LOC from the
    // results data and never needs the corpus, so it is always regenerated.
    let has_c_corpus = test_corpus_dir.is_dir();

    // Cache C LOC per battery (same for all agents)
    let mut c_loc_cache: BTreeMap<String, u32> = BTreeMap::new();

    let mut rows: Vec<BatteryRow> = Vec::new();

    for agent_entry in sorted_read_dir(&results_dir)? {
        let agent = agent_entry.file_name().to_string_lossy().to_string();
        let agent_dir = agent_entry.path();
        if !agent_dir.is_dir() { continue; }

        for bat_entry in sorted_read_dir(&agent_dir)? {
            let battery = bat_entry.file_name().to_string_lossy().to_string();
            let bat_dir = bat_entry.path();
            if !bat_dir.is_dir() { continue; }

            let summary_path = bat_dir.join("summary.json");
            let summary: Summary = match read_json(&summary_path) {
                Some(s) => s,
                None => continue,
            };

            let (total_loc, unsafe_lines, cases_built) = aggregate_cases(&bat_dir);

            let c_loc = *c_loc_cache.entry(battery.clone()).or_insert_with(|| {
                count_c_loc_battery(&test_corpus_dir, &battery)
            });

            rows.push(BatteryRow {
                agent: agent.clone(),
                battery,
                cases_passed: summary.cases_passed,
                cases_tested: summary.cases_tested,
                cases_built,
                vectors_passed: summary.vectors_passed,
                vectors_total: summary.vectors_passed + summary.vectors_failed,
                c_loc,
                total_loc,
                unsafe_lines,
            });
        }
    }

    // Group by battery for per-battery tables
    let mut by_battery: BTreeMap<String, Vec<&BatteryRow>> = BTreeMap::new();
    for row in &rows {
        by_battery.entry(row.battery.clone()).or_default().push(row);
    }

    // 1. Per-battery comparison tables
    let mut all = String::new();
    writeln!(all, "# Translation Results\n")?;
    writeln!(all, "Auto-generated from validated `result.json` and `summary.json` files.\n")?;

    for (battery, brows) in &by_battery {
        writeln!(all, "## {battery}\n")?;
        writeln!(all, "| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |")?;
        writeln!(all, "|-------|-------------|----------------|-------|----------|-------------|----------|")?;
        for r in brows {
            let unsafe_pct = if r.total_loc > 0 {
                format!("{:.1}%", r.unsafe_lines as f64 / r.total_loc as f64 * 100.0)
            } else {
                "N/A".into()
            };
            writeln!(
                all,
                "| {} | {}/{} | {}/{} | {} | {} | {} | {} |",
                r.agent, r.cases_passed, r.cases_tested,
                r.vectors_passed, r.vectors_total,
                r.c_loc, r.total_loc, r.unsafe_lines, unsafe_pct,
            )?;
        }
        writeln!(all)?;
    }

    // 2. Summary cross-table (agents as columns, batteries as rows)
    writeln!(all, "## Summary: Cases Passed\n")?;
    let agents: Vec<String> = rows.iter().map(|r| r.agent.clone())
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    write!(all, "| Battery |")?;
    for a in &agents { write!(all, " {} |", a)?; }
    writeln!(all)?;
    write!(all, "|---------|")?;
    for _ in &agents { write!(all, "------|")?; }
    writeln!(all)?;
    for (battery, brows) in &by_battery {
        let lookup: BTreeMap<&str, &BatteryRow> = brows.iter().map(|r| (r.agent.as_str(), *r)).collect();
        write!(all, "| {} |", battery)?;
        for a in &agents {
            if let Some(r) = lookup.get(a.as_str()) {
                write!(all, " {}/{} |", r.cases_passed, r.cases_tested)?;
            } else {
                write!(all, " — |")?;
            }
        }
        writeln!(all)?;
    }
    writeln!(all)?;

    // 3. CRUST-bench tables
    // Projects with known benchmark bugs (test translation errors or C→Rust type gaps)
    let benchmark_bugs: std::collections::HashSet<&str> = [
        "cissy", "libpgn", "libwecan", "razz_simulation", "fs_c",
    ].into_iter().collect();

    for (label, dir_name) in [
        ("CRUST (test repair)", "CRUST"),
        ("CRUST-blind (self-generated tests)", "CRUST-blind"),
    ] {
        let crust_dir = repo_root.join("results").join(dir_name);
        if !crust_dir.is_dir() { continue; }
        writeln!(all, "## {label}\n")?;
        writeln!(all, "| Agent | Projects Passed | Adjusted* | Tests Passed | LOC | Unsafe Lines | Unsafe % |")?;
        writeln!(all, "|-------|----------------|-----------|-------------|-----|-------------|----------|")?;
        for agent_entry in sorted_read_dir(&crust_dir)? {
            let agent = agent_entry.file_name().to_string_lossy().to_string();
            if !agent_entry.path().is_dir() { continue; }
            let (mut total, mut passed, mut tests_ok, mut tests_failed) = (0u32, 0u32, 0u32, 0u32);
            let (mut total_loc, mut unsafe_lines) = (0u32, 0u32);
            let (mut adj_total, mut adj_passed, mut adj_tok, mut adj_tfail) = (0u32, 0u32, 0u32, 0u32);
            for proj_entry in sorted_read_dir(&agent_entry.path())? {
                if !proj_entry.path().is_dir() { continue; }
                let proj_name = proj_entry.file_name().to_string_lossy().to_string();
                // CRUST: result.json at top; CRUST-blind: verify/result.json
                let rp = proj_entry.path().join("result.json");
                let rp = if rp.exists() { rp } else { proj_entry.path().join("verify/result.json") };
                let r: serde_json::Value = match read_json(&rp) { Some(v) => v, None => continue };
                total += 1;
                let tok = r.get("tests_ok").or(r.get("real_tests_ok")).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let tfail = r.get("tests_failed").or(r.get("real_tests_failed")).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                tests_ok += tok;
                tests_failed += tfail;
                let build_ok = r.get("build_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                if build_ok && tfail == 0 {
                    passed += 1;
                }
                total_loc += r.pointer("/loc/code").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                unsafe_lines += r.pointer("/unsafe/lines").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                // Adjusted: exclude benchmark bugs
                if !benchmark_bugs.contains(proj_name.as_str()) {
                    adj_total += 1;
                    adj_tok += tok;
                    adj_tfail += tfail;
                    if build_ok && tfail == 0 { adj_passed += 1; }
                }
            }
            let unsafe_pct = if total_loc > 0 {
                format!("{:.1}%", unsafe_lines as f64 / total_loc as f64 * 100.0)
            } else { "N/A".into() };
            writeln!(all, "| {} | {}/{} | {}/{} | {}/{} | {} | {} | {} |",
                agent, passed, total, adj_passed, adj_total,
                tests_ok, tests_ok + tests_failed,
                total_loc, unsafe_lines, unsafe_pct)?;
        }
        writeln!(all)?;
    }

    // Only rewrite results.md when the C corpus is present; otherwise its C LOC
    // column would be zeroed. (See has_c_corpus above.)
    let out_path = tables_dir.join("results.md");
    if has_c_corpus {
        std::fs::write(&out_path, &all)?;
        println!("✅ Wrote {}", out_path.display());
    } else {
        println!(
            "⚠️  Skipped {} (test-corpus submodule not checked out; \
             C LOC would be zeroed). Run `git submodule update --init test-corpus` \
             to regenerate it.",
            out_path.display(),
        );
    }

    // LaTeX table for the paper. Numbers are derived from the results data
    // (Rust LOC, not C LOC), so this is always regenerated.
    let tex = generate_tractor_tex(&rows);
    let tex_path = tables_dir.join("tractor.tex");
    std::fs::write(&tex_path, &tex)?;
    println!("✅ Wrote {}", tex_path.display());

    // Named constants for the prose, so a number quoted in the text cannot
    // drift from the same number in a table.
    let numbers = generate_numbers_tex(&rows, repo_root);
    let numbers_path = tables_dir.join("numbers.tex");
    std::fs::write(&numbers_path, &numbers)?;
    println!("✅ Wrote {}", numbers_path.display());

    Ok(())
}

/// Count CRUST projects that pass in a given mode, excluding the known-buggy
/// benchmarks (the paper's "adjusted" /90 count). `mode_dir` is "CRUST"
/// (test-repair) or "CRUST-blind" (self-generated tests).
fn crust_pass_adjusted(repo_root: &Path, mode_dir: &str) -> std::collections::BTreeMap<String, u32> {
    let bugs: std::collections::HashSet<&str> =
        ["cissy", "libpgn", "libwecan", "razz_simulation", "fs_c"].into_iter().collect();
    let mut out = std::collections::BTreeMap::new();
    let dir = repo_root.join("results").join(mode_dir);
    let Ok(agents) = sorted_read_dir(&dir) else { return out };
    for agent_entry in agents {
        if !agent_entry.path().is_dir() { continue; }
        let agent = agent_entry.file_name().to_string_lossy().to_string();
        let mut passed = 0u32;
        if let Ok(projs) = sorted_read_dir(&agent_entry.path()) {
            for pe in projs {
                if !pe.path().is_dir() { continue; }
                let name = pe.file_name().to_string_lossy().to_string();
                if bugs.contains(name.as_str()) { continue; }
                let rp = pe.path().join("result.json");
                let rp = if rp.exists() { rp } else { pe.path().join("verify/result.json") };
                let Some(r) = read_json::<serde_json::Value>(&rp) else { continue };
                let build_ok = r.get("build_ok").and_then(|v| v.as_bool()).unwrap_or(false);
                // test-repair uses tests_*; blind uses real_tests_* (fall back to tests_*).
                let tfail = r.get("tests_failed").or(r.get("real_tests_failed"))
                    .and_then(|v| v.as_u64()).unwrap_or(0);
                if build_ok && tfail == 0 { passed += 1; }
            }
        }
        out.insert(agent, passed);
    }
    out
}

/// Emit \newcommand constants for result numbers that are quoted in the prose.
/// Only numbers that are derived directly from the committed results appear
/// here; figures the results data does not reproduce (e.g. the CRUST
/// self-generated pass count) are intentionally left to the paper text.
fn generate_numbers_tex(rows: &[BatteryRow], repo_root: &Path) -> String {
    use std::collections::HashMap;
    // TRACTOR totals (tests passed over the full corpus) and P01 sub-totals.
    let mut total_tests: HashMap<&str, u32> = HashMap::new();
    let mut p01_tests: HashMap<&str, u32> = HashMap::new();
    let mut battery_size: HashMap<&str, u32> = HashMap::new();
    for r in rows {
        let e = battery_size.entry(r.battery.as_str()).or_insert(0);
        *e = (*e).max(r.cases_tested);
    }
    let total_cases: u32 = battery_size.values().sum();
    for r in rows {
        *total_tests.entry(r.agent.as_str()).or_insert(0) += r.cases_passed;
        if r.battery == "P01_sphincs_plus" {
            p01_tests.insert(r.agent.as_str(), r.cases_passed);
        }
    }
    let crust_tr = crust_pass_adjusted(repo_root, "CRUST");

    let g = |m: &HashMap<&str, u32>, k: &str| m.get(k).copied().unwrap_or(0);
    let mut o = String::new();
    o.push_str("% GENERATED by `harvest-tools report`. Do not edit by hand.\n");
    o.push_str("% Named constants for result numbers quoted in the prose, so text and\n");
    o.push_str("% tables cannot disagree. Values are derived from results/.\n");
    o.push_str(&format!("\\newcommand{{\\TractorTotalCases}}{{{}}}\n", total_cases));
    o.push_str(&format!("\\newcommand{{\\ActorKiroTests}}{{{}/{}}}\n", g(&total_tests, "kiro"), total_cases));
    o.push_str(&format!("\\newcommand{{\\ActorClaudeTests}}{{{}/{}}}\n", g(&total_tests, "claude"), total_cases));
    o.push_str(&format!("\\newcommand{{\\ActorCodexTests}}{{{}/{}}}\n", g(&total_tests, "codex-gpt54"), total_cases));
    o.push_str(&format!("\\newcommand{{\\ActorKiroNoVerifyTests}}{{{}/{}}}\n", g(&total_tests, "kiro-translate"), total_cases));
    o.push_str(&format!("\\newcommand{{\\ActorKiroPOneTests}}{{{}/128}}\n", g(&p01_tests, "kiro")));
    o.push_str(&format!("\\newcommand{{\\ActorKiroNoVerifyPOneTests}}{{{}/128}}\n", g(&p01_tests, "kiro-translate")));
    o.push_str(&format!("\\newcommand{{\\CrustKiroTestRepair}}{{{}/90}}\n", crust_tr.get("kiro").copied().unwrap_or(0)));
    o
}

/// Rows of the TRACTOR table, in paper order. Each entry maps a display label
/// to the results/Test-Corpus/ agent directory. ACTOR harness rows are grouped
/// first, then the transpiler/LLM baselines.
const TRACTOR_TABLE_ROWS: &[(&str, &str, bool)] = &[
    // (display label, results dir, is_actor_harness)
    ("ACTOR (Kiro)", "kiro", true),
    ("ACTOR (Claude Code)", "claude", true),
    ("ACTOR (Codex)", "codex-gpt54", true),
    ("ACTOR (Kiro, no verify)", "kiro-translate", true),
    ("C2Rust", "c2rust", false),
    ("Laertes", "laertes", false),
    ("C2SaferRust", "c2saferrust", false),
    ("SmartC2Rust", "smartc2rust", false),
    ("Kimi K2.5 (query)", "kimi", false),
    ("GPT-5.4 (query)", "gpt-5.4", false),
    ("Gemini 3.1 Pro (query)", "gemini-3.1-pro-preview", false),
];

/// Battery display names and their results-directory names, in paper order.
const TRACTOR_BATTERIES: &[(&str, &str)] = &[
    ("B01-syn", "B01_synthetic"),
    ("B01-org", "B01_organic"),
    ("B02-syn", "B02_synthetic"),
    ("B02-org", "B02_organic"),
    ("P00 (Perlin)", "P00_perlin_noise"),
    ("P01 (SPHINCS+)", "P01_sphincs_plus"),
    ("Total", ""), // empty battery dir => aggregate across all
];

/// Format a LOC count the way the paper's table does: raw below 1000, one
/// decimal place for 1000-1999 (e.g. 1.6k), rounded integer-k at/above 2000.
fn fmt_k(loc: u32) -> String {
    if loc < 1000 {
        loc.to_string()
    } else if loc < 2000 {
        format!("{:.1}k", loc as f64 / 1000.0)
    } else {
        format!("{}k", (loc as f64 / 1000.0).round() as u32)
    }
}

/// Build the LaTeX body (table rows only) for tab:tractor. The surrounding
/// table/tabular/caption stays in paper.tex, which \input{}s this file.
fn generate_tractor_tex(rows: &[BatteryRow]) -> String {
    use std::collections::HashMap;
    // (agent dir, battery dir) -> row
    let mut idx: HashMap<(&str, &str), &BatteryRow> = HashMap::new();
    for r in rows {
        idx.insert((r.agent.as_str(), r.battery.as_str()), r);
    }

    // Canonical battery size = the largest cases_tested any agent recorded for it.
    // (The ACTOR harnesses ran every case, so this is the full battery size.)
    // A partial run by some baseline still divides by this full size, so a case
    // it never attempted counts as a failure — matching the paper's methodology.
    let mut battery_size: HashMap<&str, u32> = HashMap::new();
    for r in rows {
        let e = battery_size.entry(r.battery.as_str()).or_insert(0);
        *e = (*e).max(r.cases_tested);
    }
    // Total case count = sum of the batteries shown in the table (derived from
    // the data, not hardcoded, so it stays correct if the corpus changes).
    let total_cases: u32 = TRACTOR_BATTERIES.iter()
        .filter_map(|(_, d)| (!d.is_empty()).then(|| battery_size.get(d).copied().unwrap_or(0)))
        .sum();

    // For each (agent, battery-or-Total) produce built/tested/tests_pass/loc/unsafe,
    // always over the full battery size so untranslated cases count as failures.
    struct Cell { built: u32, denom: u32, tests_pass: u32, loc: u32, unsafe_lines: u32, present: bool }
    let cell = |agent: &str, bat_dir: &str| -> Cell {
        if bat_dir.is_empty() {
            // Total: sum across all batteries, denom = full corpus size.
            let (mut b, mut tp, mut loc, mut un) = (0u32, 0u32, 0u32, 0u32);
            let mut present = false;
            for (_, bd) in TRACTOR_BATTERIES.iter().filter(|(_, d)| !d.is_empty()) {
                if let Some(r) = idx.get(&(agent, *bd)) {
                    present = true;
                    b += r.cases_built; tp += r.cases_passed; loc += r.total_loc; un += r.unsafe_lines;
                }
            }
            Cell { built: b, denom: total_cases, tests_pass: tp, loc, unsafe_lines: un, present }
        } else {
            // Denominator is the canonical battery size, so partial runs still
            // divide by the full size (unattempted cases count as failures).
            let denom = battery_size.get(bat_dir).copied().unwrap_or(0);
            if let Some(r) = idx.get(&(agent, bat_dir)) {
                Cell { built: r.cases_built, denom, tests_pass: r.cases_passed,
                       loc: r.total_loc, unsafe_lines: r.unsafe_lines, present: true }
            } else {
                Cell { built: 0, denom, tests_pass: 0, loc: 0, unsafe_lines: 0, present: false }
            }
        }
    };

    let mut out = String::new();
    out.push_str("% GENERATED by `harvest-tools report` from results/Test-Corpus/. Do not edit by hand.\n");
    out.push_str("% Builds = crate compiles (result.json error != \"build failed\").\n");
    out.push_str("% Tests = cases passing all vectors. Every system is scored over all cases.\n");

    for (bi, (bat_label, bat_dir)) in TRACTOR_BATTERIES.iter().enumerate() {
        // Best Tests among ACTOR harness rows in this battery, for bold.
        let best = TRACTOR_TABLE_ROWS.iter().filter(|(_, _, actor)| *actor)
            .map(|(_, a, _)| cell(a, bat_dir).tests_pass)
            .max().unwrap_or(0);
        for (label, agent, _) in TRACTOR_TABLE_ROWS {
            let c = cell(agent, bat_dir);
            let denom = c.denom;
            let first_col = if *label == TRACTOR_TABLE_ROWS[0].0 { *bat_label } else { "" };
            let tests = if c.tests_pass == best && best > 0 {
                format!("\\textbf{{{}/{}}}", c.tests_pass, denom)
            } else {
                format!("{}/{}", c.tests_pass, denom)
            };
            // LOC/unsafe are "--" only when the system produced no output for this
            // battery at all (matches the paper's dashes for e.g. SmartC2Rust P01).
            let (loc, un) = if c.present && c.loc > 0 {
                (fmt_k(c.loc), format!("{}\\%",
                    (c.unsafe_lines as f64 / c.loc as f64 * 100.0).round() as u32))
            } else {
                ("--".into(), "--".into())
            };
            out.push_str(&format!("{} & {} & {}/{} & {} & {} & {} \\\\\n",
                first_col, label, c.built, denom, tests, loc, un));
        }
        // hline between batteries; double before Total.
        if bi + 2 == TRACTOR_BATTERIES.len() { out.push_str("\\hline\\hline\n"); }
        else if bi + 1 < TRACTOR_BATTERIES.len() { out.push_str("\\hline\n"); }
    }
    out
}

/// Returns (total_loc, unsafe_lines, cases_built) for a battery directory.
/// `cases_built` counts cases whose crate compiled (result.json error != "build failed"),
/// which is the runner's own build/no-build signal (see test.rs).
fn aggregate_cases(bat_dir: &Path) -> (u32, u32, u32) {
    let mut locs = Vec::new();
    let mut unsafes = Vec::new();
    let mut built = 0u32;
    let Ok(entries) = std::fs::read_dir(bat_dir) else { return (0, 0, 0) };
    for entry in entries.flatten() {
        if !entry.path().is_dir() { continue; }
        let result_path = entry.path().join("result.json");
        let cr: CaseResult = match read_json(&result_path) {
            Some(r) => r,
            None => continue,
        };
        if cr.error.as_deref() != Some("build failed") {
            built += 1;
        }
        locs.push(cr.loc.map_or(0, |l| l.code));
        unsafes.push(cr.unsafe_.map_or(0, |u| u.lines));
    }
    if locs.is_empty() { return (0, 0, built); }
    // Shared-translation detection: if cases share a translated_rust directory
    // (P00/P01 style), LOC values cluster tightly. Use max instead of sum.
    // Heuristic: if max/min ratio < 2, it's a shared translation.
    let min_loc = *locs.iter().min().unwrap();
    let max_loc = *locs.iter().max().unwrap();
    if locs.len() > 1 && min_loc > 0 && max_loc <= min_loc * 2 {
        let max_unsafe = *unsafes.iter().max().unwrap();
        (max_loc, max_unsafe, built)
    } else {
        (locs.iter().sum(), unsafes.iter().sum(), built)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn sorted_read_dir(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

/// Count C LOC (non-blank, non-comment *.c/*.h lines) across all cases in a battery.
/// Reads from test-corpus/Public-Tests/<battery>/<case>/test_case/.
/// For shared-source batteries (P01), deduplicates like aggregate_cases.
fn count_c_loc_battery(test_corpus_dir: &Path, battery: &str) -> u32 {
    let bat_dir = test_corpus_dir.join(battery);
    let Ok(entries) = std::fs::read_dir(&bat_dir) else { return 0 };
    let mut locs = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() { continue; }
        let test_case = entry.path().join("test_case");
        if !test_case.is_dir() { continue; }
        locs.push(count_c_loc_dir(&test_case));
    }
    if locs.is_empty() { return 0; }
    let min = *locs.iter().min().unwrap();
    let max = *locs.iter().max().unwrap();
    if locs.len() > 1 && min > 0 && max <= min * 2 {
        max // shared translation — count once
    } else {
        locs.iter().sum()
    }
}

fn count_c_loc_dir(dir: &Path) -> u32 {
    let mut total = 0u32;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += count_c_loc_dir(&path);
        } else if path.extension().is_some_and(|x| x == "c" || x == "h") {
            let Ok(src) = std::fs::read_to_string(&path) else { continue };
            total += src.lines()
                .filter(|l| { let t = l.trim(); !t.is_empty() && !t.starts_with("//") && !t.starts_with("/*") })
                .count() as u32;
        }
    }
    total
}
