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

            let (total_loc, unsafe_lines) = aggregate_cases(&bat_dir);

            let c_loc = *c_loc_cache.entry(battery.clone()).or_insert_with(|| {
                count_c_loc_battery(&test_corpus_dir, &battery)
            });

            rows.push(BatteryRow {
                agent: agent.clone(),
                battery,
                cases_passed: summary.cases_passed,
                cases_tested: summary.cases_tested,
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
    for (label, dir_name) in [("CRUST", "CRUST"), ("CRUST-blind", "CRUST-blind")] {
        let crust_dir = repo_root.join("results").join(dir_name);
        if !crust_dir.is_dir() { continue; }
        writeln!(all, "## {label}\n")?;
        writeln!(all, "| Agent | Projects Passed | Tests Passed | LOC | Unsafe Lines | Unsafe % |")?;
        writeln!(all, "|-------|----------------|-------------|-----|-------------|----------|")?;
        for agent_entry in sorted_read_dir(&crust_dir)? {
            let agent = agent_entry.file_name().to_string_lossy().to_string();
            if !agent_entry.path().is_dir() { continue; }
            let (mut total, mut passed, mut tests_ok, mut tests_failed) = (0u32, 0u32, 0u32, 0u32);
            let (mut total_loc, mut unsafe_lines) = (0u32, 0u32);
            for proj_entry in sorted_read_dir(&agent_entry.path())? {
                if !proj_entry.path().is_dir() { continue; }
                // CRUST: result.json at top; CRUST-blind: verify/result.json
                let rp = proj_entry.path().join("result.json");
                let rp = if rp.exists() { rp } else { proj_entry.path().join("verify/result.json") };
                let r: serde_json::Value = match read_json(&rp) { Some(v) => v, None => continue };
                total += 1;
                let tok = r.get("tests_ok").or(r.get("real_tests_ok")).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let tfail = r.get("tests_failed").or(r.get("real_tests_failed")).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                tests_ok += tok;
                tests_failed += tfail;
                if r.get("build_ok").and_then(|v| v.as_bool()).unwrap_or(false) && tfail == 0 {
                    passed += 1;
                }
                total_loc += r.pointer("/loc/code").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                unsafe_lines += r.pointer("/unsafe/lines").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            }
            let unsafe_pct = if total_loc > 0 {
                format!("{:.1}%", unsafe_lines as f64 / total_loc as f64 * 100.0)
            } else { "N/A".into() };
            writeln!(all, "| {} | {}/{} | {}/{} | {} | {} | {} |",
                agent, passed, total, tests_ok, tests_ok + tests_failed,
                total_loc, unsafe_lines, unsafe_pct)?;
        }
        writeln!(all)?;
    }

    let out_path = tables_dir.join("results.md");
    std::fs::write(&out_path, &all)?;
    println!("✅ Wrote {}", out_path.display());
    Ok(())
}

fn aggregate_cases(bat_dir: &Path) -> (u32, u32) {
    let mut locs = Vec::new();
    let mut unsafes = Vec::new();
    let Ok(entries) = std::fs::read_dir(bat_dir) else { return (0, 0) };
    for entry in entries.flatten() {
        if !entry.path().is_dir() { continue; }
        let result_path = entry.path().join("result.json");
        let cr: CaseResult = match read_json(&result_path) {
            Some(r) => r,
            None => continue,
        };
        locs.push(cr.loc.map_or(0, |l| l.code));
        unsafes.push(cr.unsafe_.map_or(0, |u| u.lines));
    }
    if locs.is_empty() { return (0, 0); }
    // Shared-translation detection: if cases share a translated_rust directory
    // (P00/P01 style), LOC values cluster tightly. Use max instead of sum.
    // Heuristic: if max/min ratio < 2, it's a shared translation.
    let min_loc = *locs.iter().min().unwrap();
    let max_loc = *locs.iter().max().unwrap();
    if locs.len() > 1 && min_loc > 0 && max_loc <= min_loc * 2 {
        let max_unsafe = *unsafes.iter().max().unwrap();
        (max_loc, max_unsafe)
    } else {
        (locs.iter().sum(), unsafes.iter().sum())
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
