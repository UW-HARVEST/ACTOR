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

/// A TRACTOR per-case `result.json`, read for LOC / unsafe / build status. The
/// pass/fail verdict is taken from the battery `summary.json`, not from here.
#[derive(Deserialize)]
struct CaseResult {
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

            let (total_loc, unsafe_lines, cases_built) =
                aggregate_cases(&bat_dir, &test_corpus_dir.join(&battery));

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

    // 3. CRUST-bench tables. Every column is computed by the SAME functions the
    // paper's crust.tex uses (crust_pass_adjusted / crust_build_counts /
    // crust_loc_unsafe), over the canonical 87-project subset — so this markdown
    // debug view can never drift from the paper table. (Earlier this block had its
    // own inline loop that dropped missing projects from the denominator, yielding
    // /86 where the paper says /87; consolidating onto the shared path removes that
    // divergence.)
    for (label, dir_name) in [
        ("CRUST (test repair)", "CRUST"),
        ("CRUST-blind (self-generated tests)", "CRUST-blind"),
    ] {
        let crust_dir = repo_root.join("results").join(dir_name);
        if !crust_dir.is_dir() { continue; }
        let pass = crust_pass_adjusted(repo_root, dir_name);
        let builds = crust_build_counts(repo_root, dir_name);
        writeln!(all, "## {label}\n")?;
        writeln!(all, "| Agent | Builds (/87) | Tests (/87) | LOC | Unsafe Lines | Unsafe % |")?;
        writeln!(all, "|-------|-------------|-------------|-----|-------------|----------|")?;
        for agent_entry in sorted_read_dir(&crust_dir)? {
            let agent = agent_entry.file_name().to_string_lossy().to_string();
            if !agent_entry.path().is_dir() { continue; }
            let (passed, total) = pass.get(&agent).copied().unwrap_or((0, 0));
            let built = builds.get(&agent).copied().unwrap_or(0);
            let (loc, unsafe_lines, _) = crust_loc_unsafe(repo_root, dir_name, &agent);
            let unsafe_pct = if loc > 0 {
                format!("{:.1}%", unsafe_lines as f64 / loc as f64 * 100.0)
            } else { "N/A".into() };
            writeln!(all, "| {} | {}/{} | {}/{} | {} | {} | {} |",
                agent, built, total, passed, total, loc, unsafe_lines, unsafe_pct)?;
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

    // Dataset-size table body (tab:datasets), derived from the C corpus and the
    // CRUST-Bench C sources copied into results/.
    let datasets = generate_datasets_tex(repo_root);
    let datasets_path = tables_dir.join("datasets.tex");
    std::fs::write(&datasets_path, &datasets)?;
    println!("✅ Wrote {}", datasets_path.display());

    // CRUST-Bench ACTOR + baseline table body (tab:crust). The leaderboard and
    // self-generated baselines are manual (see manual.tex).
    let crust = generate_crust_tex(repo_root, &rows);
    let crust_path = tables_dir.join("crust.tex");
    std::fs::write(&crust_path, &crust)?;
    println!("✅ Wrote {}", crust_path.display());

    // Prompt-sensitivity ablations (tab:prompt-sensitivity): base + 4 variants,
    // each over TRACTOR / CRUST self-gen / CRUST test-repair.
    let promptsens = generate_prompt_sensitivity_tex(repo_root, &rows);
    let promptsens_path = tables_dir.join("prompt-sensitivity.tex");
    std::fs::write(&promptsens_path, &promptsens)?;
    println!("✅ Wrote {}", promptsens_path.display());

    // Manually-entered constants transcribed from manual_constants.toml.
    let manual = generate_manual_tex(repo_root);
    let manual_path = tables_dir.join("manual.tex");
    std::fs::write(&manual_path, &manual)?;
    println!("✅ Wrote {}", manual_path.display());

    // ── Invariant checks: fail generation (nonzero exit) rather than emit a
    //    silently-wrong paper. These catch drift at the source, before LaTeX.
    //    They check the DATA the tables were built from, not compile-time
    //    constants, so a renamed/missing/extra input dir or a lost symlink fails
    //    generation instead of silently changing a number. ──

    // (1) TRACTOR is scored over exactly 338 cases. Take the max cases_tested
    //     summed across batteries for any single agent (the ACTOR harnesses run
    //     every case), which must equal 338.
    let mut per_agent_total: BTreeMap<&str, u32> = BTreeMap::new();
    for r in &rows {
        *per_agent_total.entry(r.agent.as_str()).or_insert(0) += r.cases_tested;
    }
    let tractor_total = per_agent_total.values().copied().max().unwrap_or(0);
    anyhow::ensure!(
        tractor_total == 338,
        "TRACTOR invariant failed: max agent case total is {tractor_total}, expected 338"
    );

    // (1b) Per-battery sizes must match the known corpus, so a malformed
    //      summary.json can't inflate a per-battery denominator while still
    //      summing to 338 (compensating errors). These are the fixed battery sizes.
    const EXPECTED_BATTERY_SIZES: &[(&str, u32)] = &[
        ("B01_synthetic", 85), ("B01_organic", 38),
        ("B02_synthetic", 42), ("B02_organic", 44),
        ("P00_perlin_noise", 1), ("P01_sphincs_plus", 128),
    ];
    let mut battery_max: BTreeMap<&str, u32> = BTreeMap::new();
    for r in &rows {
        let e = battery_max.entry(r.battery.as_str()).or_insert(0);
        *e = (*e).max(r.cases_tested);
    }
    for (bat, expected) in EXPECTED_BATTERY_SIZES {
        if let Some(&got) = battery_max.get(bat) {
            anyhow::ensure!(
                got == *expected,
                "TRACTOR battery-size invariant failed: {bat} max cases_tested is {got}, expected {expected}"
            );
        }
    }
    let battery_sum: u32 = EXPECTED_BATTERY_SIZES.iter().map(|(_, n)| n).sum();
    anyhow::ensure!(
        battery_sum == 338,
        "TRACTOR battery sizes sum to {battery_sum}, expected 338"
    );

    // (1c) The ACTOR agent directories that feed prose macros and TRACTOR/CRUST
    //      rows must exist. A renamed/missing dir otherwise emits a plausible
    //      all-zeros row (e.g. "0/338") that trips no other invariant and is
    //      indistinguishable from a legitimate baseline zero.
    for agent in ["kiro", "claude", "codex-gpt54", "kiro-translate"] {
        anyhow::ensure!(
            results_dir.join(agent).is_dir(),
            "TRACTOR agent-dir invariant failed: results/Test-Corpus/{agent} is missing \
             (a rename would silently emit a 0/338 row). Fix the mapping or restore the dir."
        );
    }
    for agent in ["kiro", "claude", "codex-gpt54"] {
        anyhow::ensure!(
            repo_root.join("results/CRUST").join(agent).is_dir(),
            "CRUST agent-dir invariant failed: results/CRUST/{agent} is missing \
             (a rename would silently emit a 0/87 row)."
        );
    }

    // (2) CRUST reported denominator must be 87 (100 − 13 canonical exclusions).
    anyhow::ensure!(
        crate::exclusions::CRUST_TOTAL - crate::exclusions::excluded_count()
            == crate::exclusions::CRUST_REPORTED,
        "CRUST invariant failed: 100 − {} ≠ 87",
        crate::exclusions::excluded_count()
    );

    // (2b) Every CRUST agent's EMITTED denominator must actually equal 87, checked
    //      against the data (not the constant). Catches an extra/missing/renamed
    //      project dir that would silently shift the denominator to 86/88.
    //      Also assert builds >= passes per agent (a passing crate must have built).
    for mode in ["CRUST", "CRUST-blind"] {
        let pass = crust_pass_adjusted(repo_root, mode);
        let builds = crust_build_counts(repo_root, mode);
        for (agent, (passed, total)) in &pass {
            anyhow::ensure!(
                *total == crate::exclusions::CRUST_REPORTED as u32,
                "CRUST denominator invariant failed: {mode}/{agent} has {total} included projects, expected 87 \
                 (an extra/missing/renamed project dir shifted the denominator)"
            );
            let built = builds.get(agent).copied().unwrap_or(0);
            anyhow::ensure!(
                built >= *passed,
                "CRUST invariant failed: {mode}/{agent} reports {passed} passes but only {built} builds \
                 (a passing crate must have compiled)"
            );
        }
    }

    // (2b-gap) The manual gap classification (defects + bugs) must equal the
    //      derived gap size (kiro test-repair passes − self-generated passes).
    //      This is the drift guard for the "tests encode a specification absent
    //      from the C" prose: if a re-score moves either endpoint, the sum in
    //      manual_constants.toml [crust_gap] must be re-reconciled or generation
    //      fails here, before the paper can print a split that doesn't add up.
    {
        let tr = crust_pass_adjusted(repo_root, "CRUST");
        let bl = crust_pass_adjusted(repo_root, "CRUST-blind");
        let (kp, _) = tr.get("kiro").copied().unwrap_or((0, 0));
        let (kb, _) = bl.get("kiro").copied().unwrap_or((0, 0));
        let gap = kp.saturating_sub(kb);
        let (defects, bugs) = crust_gap_split(repo_root);
        anyhow::ensure!(
            defects + bugs == gap,
            "CRUST gap invariant failed: manual_constants.toml [crust_gap] defects ({defects}) \
             + bugs ({bugs}) = {} but the data-derived gap (kiro test-repair {kp} − self-gen {kb}) \
             is {gap}. Re-reconcile the per-project classification in \
             CRUST_SELFGEN_GAP_AUDIT.md and update [crust_gap].",
            defects + bugs
        );
    }

    // (2c) Every re-scored baseline report must also cover exactly 87 included
    //      projects, so a malformed report can't shift a baseline denominator.
    for name in ["gpt54", "kimi_k25", "gemini31pro",
                 "gpt54_test_repair", "kimi_k25_test_repair", "gemini31pro_test_repair"] {
        let report = if name.ends_with("_test_repair") {
            crust_baseline_87(repo_root, name)
        } else {
            crust_baseline_selfgen_87(repo_root, name)
        };
        if let Some((builds, passed, total)) = report {
            anyhow::ensure!(
                total == crate::exclusions::CRUST_REPORTED as u32,
                "CRUST baseline invariant failed: {name} covers {total} included projects, expected 87"
            );
            anyhow::ensure!(
                builds >= passed,
                "CRUST baseline invariant failed: {name} reports {passed} passes but only {builds} builds"
            );
        }
    }

    // (2d) Shared-source dedup must have happened: P01/P00 collapse to one distinct
    //      source. If the corpus is present but symlinks did not survive checkout
    //      (materialized as real dirs), every config would be counted and LOC would
    //      inflate ~120x silently. Fail loudly instead. (Skip when corpus absent —
    //      then dedup can't run and datasets.tex omits these rows anyway.)
    if has_c_corpus {
        for (bat, expected_cases) in [("P01_sphincs_plus", 1u32), ("P00_perlin_noise", 1u32)] {
            let bat_dir = test_corpus_dir.join(bat);
            if !bat_dir.is_dir() { continue; }
            let mut distinct: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
            let mut case_dirs = 0u32;
            if let Ok(cases) = sorted_read_dir(&bat_dir) {
                for ce in cases {
                    let tc = ce.path().join("test_case");
                    if !tc.is_dir() { continue; }
                    case_dirs += 1;
                    distinct.insert(std::fs::canonicalize(&tc).unwrap_or(tc));
                }
            }
            anyhow::ensure!(
                distinct.len() as u32 == expected_cases,
                "shared-source dedup invariant failed: {bat} has {case_dirs} case dirs collapsing to \
                 {} distinct sources, expected {expected_cases} — symlinks likely did not survive checkout \
                 (core.symlinks=false?), which would inflate LOC. Re-clone with symlink support.",
                distinct.len()
            );
        }
    }

    // (3) Every macro the paper references (listed in macros_used.txt, if present)
    //     must be emitted by the generator, so a rename/removal is caught here
    //     instead of surfacing as an LaTeX "Undefined control sequence".
    let macros_used = repo_root.join("macros_used.txt");
    if macros_used.is_file() {
        let emitted = format!("{numbers}\n{manual}");
        let mut missing = Vec::new();
        for line in std::fs::read_to_string(&macros_used)?.lines() {
            let name = line.trim();
            if name.is_empty() || name.starts_with('#') { continue; }
            // match `\newcommand{\Name}` in the emitted files
            if !emitted.contains(&format!("{{\\{name}}}")) {
                missing.push(name.to_string());
            }
        }
        anyhow::ensure!(
            missing.is_empty(),
            "macros_used.txt lists macros the generator does not emit: {missing:?}"
        );
    }

    Ok(())
}

/// Count CRUST projects that pass in a given mode, over the paper's canonical
/// 87-project subset (see `crate::exclusions`). `mode_dir` is "CRUST"
/// (test-repair) or "CRUST-blind" (self-generated tests). Returns, per agent,
/// `(passed, total)` where `total` is the number of NON-excluded projects that
/// have a result.json present (the reproducible /87 basis).
fn crust_pass_adjusted(
    repo_root: &Path,
    mode_dir: &str,
) -> std::collections::BTreeMap<String, (u32, u32)> {
    let mut out = std::collections::BTreeMap::new();
    let dir = repo_root.join("results").join(mode_dir);
    let Ok(agents) = sorted_read_dir(&dir) else { return out };
    for agent_entry in agents {
        if !agent_entry.path().is_dir() { continue; }
        let agent = agent_entry.file_name().to_string_lossy().to_string();
        let (mut passed, mut total) = (0u32, 0u32);
        if let Ok(projs) = sorted_read_dir(&agent_entry.path()) {
            for pe in projs {
                if !pe.path().is_dir() { continue; }
                let name = pe.file_name().to_string_lossy().to_string();
                if crate::exclusions::is_excluded(name.as_str()) { continue; }
                // Denominator counts every INCLUDED project dir. A project whose
                // result.json is missing/unscored counts as a non-pass (the same
                // "unattempted = failure" rule TRACTOR uses), so the denominator
                // stays the canonical 87 rather than shrinking to the number of
                // successfully-scored projects.
                total += 1;
                let rp = pe.path().join("result.json");
                let rp = if rp.exists() { rp } else { pe.path().join("verify/result.json") };
                let Some(r) = read_json::<serde_json::Value>(&rp) else { continue };
                // Single canonical rule, identical to the baselines (scoring.rs):
                // a pass needs >=1 passing test and 0 failing. A crate that builds
                // but runs zero ground-truth tests is NOT a pass.
                if crate::scoring::CrustOutcome::from_actor(&r).passed() { passed += 1; }
            }
        }
        out.insert(agent, (passed, total));
    }
    out
}

/// Format an integer with comma thousands separators (e.g. 23072 -> "23,072"),
/// matching the paper's dataset table.
fn fmt_commas(n: u32) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Median of a slice of counts, rounded to the nearest integer (half away from
/// zero, i.e. Rust's f64::round). Returns 0 for an empty slice.
fn median_round(values: &[u32]) -> u32 {
    if values.is_empty() { return 0; }
    let mut v = values.to_vec();
    v.sort_unstable();
    let n = v.len();
    let m = if n % 2 == 1 {
        v[n / 2] as f64
    } else {
        (v[n / 2 - 1] as f64 + v[n / 2] as f64) / 2.0
    };
    m.round() as u32
}

/// Count C LOC in a directory the way CRUST-Bench's `get_c_stats.py` does:
/// recurse over *.c / *.h, skip files whose path contains "test" or whose parent
/// or grandparent directory name contains "bin"; strip C block (`/* */`) and line
/// (`//`) comments, then count non-blank lines. `block_re`/`line_re` are shared,
/// pre-compiled regexes so we do not recompile per file.
fn count_crust_c_loc_dir(dir: &Path, block_re: &regex::Regex, line_re: &regex::Regex) -> u32 {
    let mut total = 0u32;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += count_crust_c_loc_dir(&path, block_re, line_re);
        } else if path.extension().is_some_and(|x| x == "c" || x == "h") {
            let path_lc = path.to_string_lossy().to_lowercase();
            let parent_lc = path.parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let grandparent_lc = path.parent().and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if path_lc.contains("test") || parent_lc.contains("bin") || grandparent_lc.contains("bin") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else { continue };
            let src = block_re.replace_all(&src, "");
            let src = line_re.replace_all(&src, "");
            total += src.lines().filter(|l| !l.trim().is_empty()).count() as u32;
        }
    }
    total
}

/// TRACTOR battery directory names, in paper order (results-dir keys).
const TRACTOR_BATTERY_DIRS: &[&str] = &[
    "B01_synthetic", "B01_organic", "B02_synthetic",
    "B02_organic", "P00_perlin_noise", "P01_sphincs_plus",
];

/// Per-case, shared-source-deduplicated C LOC for one TRACTOR battery. Cases whose
/// corpus `test_case/` symlinks to one real source are counted once (e.g. P01's 128
/// configs → one source). This is the single definition used by BOTH tab:datasets
/// and the TRACTOR total/mean prose macros, so they cannot disagree.
fn tractor_battery_c_locs(corpus: &Path, dir_name: &str) -> Vec<u32> {
    let bat_dir = corpus.join(dir_name);
    let mut locs: Vec<u32> = Vec::new();
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    if let Ok(cases) = sorted_read_dir(&bat_dir) {
        for ce in cases {
            if !ce.path().is_dir() { continue; }
            let test_case = ce.path().join("test_case");
            if !test_case.is_dir() { continue; }
            let real = std::fs::canonicalize(&test_case).unwrap_or_else(|_| test_case.clone());
            if !seen.insert(real) { continue; }
            locs.push(count_c_loc_dir(&test_case));
        }
    }
    locs
}

/// Emit the LaTeX body rows for tab:datasets: one row per TRACTOR battery plus a
/// CRUST-Bench row, each `Dataset & Cases & Total & Mean & Median & Max`.
/// Counts are non-comment, non-blank C LOC. Numbers carry comma thousands
/// separators (except Cases). Every value is derived from committed data.
fn generate_datasets_tex(repo_root: &Path) -> String {
    let mut out = String::new();
    out.push_str("% GENERATED by harvest-tools report — do not edit\n");

    let corpus = repo_root.join("test-corpus/Public-Tests");
    // (results dir name, display name), in paper order.
    let batteries: &[(&str, &str)] = &[
        ("B01_synthetic", "B01-synthetic"),
        ("B01_organic", "B01-organic"),
        ("B02_synthetic", "B02-synthetic"),
        ("B02_organic", "B02-organic"),
        ("P00_perlin_noise", "P00 (Perlin)"),
        ("P01_sphincs_plus", "P01 {\\smaller (SPHINCS+)}"),
    ];

    let emit_row = |out: &mut String, name: &str, cases: u32, vec: &[u32]| {
        let total: u32 = vec.iter().sum();
        let mean = if cases > 0 { (total as f64 / cases as f64).round() as u32 } else { 0 };
        let median = median_round(vec);
        let max = vec.iter().copied().max().unwrap_or(0);
        out.push_str(&format!(
            "{} & {} & {} & {} & {} & {} \\\\\n",
            name, cases,
            fmt_commas(total), fmt_commas(mean), fmt_commas(median), fmt_commas(max),
        ));
    };

    for (dir_name, display) in batteries {
        // Per-case shared-source-deduplicated C LOC (see tractor_battery_c_locs).
        let locs = tractor_battery_c_locs(&corpus, dir_name);
        if locs.is_empty() { continue; }
        emit_row(&mut out, display, locs.len() as u32, &locs);
    }

    // CRUST-Bench row: C LOC over the 87 included projects (agent-independent,
    // read from the benchmark corpus itself). See crust_bench_c_locs.
    let locs = crust_bench_c_locs(repo_root, false);
    if !locs.is_empty() {
        let cases = locs.len() as u32;
        // Rule off the TRACTOR batteries from the CRUST-Bench summary row.
        out.push_str("\\hline\n");
        emit_row(&mut out, "CRUST-Bench", cases, &locs);
    }

    out
}

/// Per-project non-comment/non-blank C LOC for the CRUST-Bench corpus
/// (crust-bench/datasets/CBench/<project>/), excluding test/bin paths. When
/// `all` is false, the 13 excluded projects are dropped (the reported /87 subset);
/// when true, all 100 projects are counted (for describing the full benchmark).
/// Single definition shared by tab:datasets and the CRUST-Bench prose macros.
fn crust_bench_c_locs(repo_root: &Path, all: bool) -> Vec<u32> {
    let cbench = repo_root.join("crust-bench/datasets/CBench");
    let mut locs: Vec<u32> = Vec::new();
    if !cbench.is_dir() { return locs; }
    let block_re = regex::Regex::new(r"(?s)/\*.*?\*/").unwrap();
    let line_re = regex::Regex::new(r"(?m)//.*$").unwrap();
    if let Ok(projs) = sorted_read_dir(&cbench) {
        for pe in projs {
            if !pe.path().is_dir() { continue; }
            let name = pe.file_name().to_string_lossy().to_string();
            if !all && crate::exclusions::is_excluded(&name) { continue; }
            locs.push(count_crust_c_loc_dir(&pe.path(), &block_re, &line_re));
        }
    }
    locs
}

/// Sum (loc.code, unsafe.lines) over the 87 included projects of one CRUST agent
/// directory, returning `(total_loc, unsafe_lines, projects_counted)`. Reads the
/// runner's own result.json (top-level for CRUST, verify/ for CRUST-blind).
fn crust_loc_unsafe(repo_root: &Path, mode_dir: &str, agent: &str) -> (u32, u32, u32) {
    let dir = repo_root.join("results").join(mode_dir).join(agent);
    let (mut loc, mut unsafe_lines, mut n) = (0u32, 0u32, 0u32);
    let Ok(projs) = sorted_read_dir(&dir) else { return (0, 0, 0) };
    for pe in projs {
        if !pe.path().is_dir() { continue; }
        let name = pe.file_name().to_string_lossy().to_string();
        if crate::exclusions::is_excluded(name.as_str()) { continue; }
        let rp = pe.path().join("result.json");
        let rp = if rp.exists() { rp } else { pe.path().join("verify/result.json") };
        let Some(r) = read_json::<serde_json::Value>(&rp) else { continue };
        n += 1;
        loc += r.pointer("/loc/code").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        unsafe_lines += r.pointer("/unsafe/lines").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    }
    (loc, unsafe_lines, n)
}

/// LOC + unsafe lines for a CRUST-Bench baseline model, summed over its crates in
/// the canonical 87 subset. The baselines have no result.json, so we count the
/// committed translated crates directly with the SAME `battery::count_loc` /
/// `count_unsafe` definitions used for ACTOR (recursive over `src/`, excluding
/// `bin`/`tests`). `model` is a `crust-bench/src/outputs/<model>` dir. Returns
/// `(loc, unsafe_lines)`, or (0,0) if the model dir is absent.
fn crust_baseline_loc_unsafe(repo_root: &Path, model: &str) -> (u32, u32) {
    let dir = repo_root.join("crust-bench/src/outputs").join(model);
    let (mut loc, mut unsafe_lines) = (0u32, 0u32);
    let Ok(projs) = sorted_read_dir(&dir) else { return (0, 0) };
    for pe in projs {
        if !pe.path().is_dir() { continue; }
        let name = pe.file_name().to_string_lossy().to_string();
        if crate::exclusions::is_excluded(name.as_str()) { continue; }
        let src = pe.path().join("src");
        if !src.is_dir() { continue; }
        loc += crate::battery::count_loc(&src).code as u32;
        unsafe_lines += crate::battery::count_unsafe(&src).lines as u32;
    }
    (loc, unsafe_lines)
}

/// Re-score a CRUST-Bench baseline test-repair run over the canonical 87 subset.
/// Prefers our own `test_report_scored.json` (produced by
/// `harvest-tools score-selfgen-baselines`, carrying a real `built` compile flag)
/// so the Builds column is a true compile check identical to ACTOR's. Falls back
/// to the CRUST-Bench authors' highest-numbered `test_report_<N>.json` (whose
/// `built` is inferred from "ran a test") if we have not re-scored it. Counts
/// builds and passes via the shared `crate::scoring::CrustOutcome` rule. Returns
/// `(builds, tests_passed, total)` or None if no report exists.
fn crust_baseline_87(repo_root: &Path, name: &str) -> Option<(u32, u32, u32)> {
    let dir = repo_root.join("crust-bench/src/outputs").join(name);
    if !dir.is_dir() { return None; }
    // Prefer our re-scored report (has a real build flag).
    let scored = dir.join("test_report_scored.json");
    if scored.is_file() {
        return score_baseline_report(&scored);
    }
    let mut best: Option<(u32, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()? {
        let entry = entry.ok()?;
        let fname = entry.file_name().to_string_lossy().to_string();
        if let Some(n) = fname.strip_prefix("test_report_").and_then(|s| s.strip_suffix(".json")) {
            if let Ok(n) = n.parse::<u32>() {
                if best.as_ref().map_or(true, |(b, _)| n > *b) {
                    best = Some((n, entry.path()));
                }
            }
        }
    }
    let (_, path) = best?;
    score_baseline_report(&path)
}

/// Score a CRUST-Bench self-generated (no-test-access) baseline over the canonical
/// 87 subset. Reads `test_report_selfgen.json` (produced by
/// `harvest-tools score-selfgen-baselines`), the SAME `{project, ok, fail}` format
/// as the test-repair reports, so it goes through the identical scoring path.
fn crust_baseline_selfgen_87(repo_root: &Path, name: &str) -> Option<(u32, u32, u32)> {
    let path = repo_root
        .join("crust-bench/src/outputs")
        .join(name)
        .join("test_report_selfgen.json");
    if !path.is_file() { return None; }
    score_baseline_report(&path)
}

/// Score a baseline `test_report` JSON array (`[{project, ok, fail}, ...]`) over the
/// canonical 87 subset via the shared `crate::scoring::CrustOutcome` rule. Returns
/// `(builds, tests_passed, total)` or None if the file is unreadable.
fn score_baseline_report(path: &Path) -> Option<(u32, u32, u32)> {
    let arr: serde_json::Value = read_json(path)?;
    let items = arr.as_array()?;
    let (mut builds, mut passed, mut total) = (0u32, 0u32, 0u32);
    for it in items {
        let proj = it.get("project").and_then(|v| v.as_str()).unwrap_or("");
        if crate::exclusions::is_excluded(proj) { continue; }
        total += 1;
        let outcome = crate::scoring::CrustOutcome::from_baseline(it);
        if outcome.built() { builds += 1; }
        if outcome.passed() { passed += 1; }
    }
    Some((builds, passed, total))
}

/// Build the LaTeX body (rows only) for tab:prompt-sensitivity: the base
/// ACTOR (Claude Code) run plus four prompt-ablation variants, each scored on
/// TRACTOR (tests passed / total cases), CRUST self-generated, and CRUST
/// test-repair — the SAME `crust_pass_adjusted` path as tab:crust, so the shared
/// rows agree. Followed by the cross-prompt swap block (TRACTOR B0X, split by
/// exec/lib via the `is_lib` naming); those two figures come from
/// `manual_constants.toml [prompt_sensitivity]` because the swap runs are a
/// one-off TRACTOR-subset experiment not re-derived here.
fn generate_prompt_sensitivity_tex(repo_root: &Path, rows: &[BatteryRow]) -> String {
    use std::collections::HashMap;
    // TRACTOR tests passed per agent (sum of cases_passed over the batteries) and
    // the full corpus size (max cases_tested summed per battery).
    let mut tractor_pass: HashMap<&str, u32> = HashMap::new();
    let mut battery_size: HashMap<&str, u32> = HashMap::new();
    for r in rows {
        *tractor_pass.entry(r.agent.as_str()).or_insert(0) += r.cases_passed;
        let e = battery_size.entry(r.battery.as_str()).or_insert(0);
        *e = (*e).max(r.cases_tested);
    }
    let tractor_total: u32 = battery_size.values().sum();

    let crust_tr = crust_pass_adjusted(repo_root, "CRUST");
    let crust_bl = crust_pass_adjusted(repo_root, "CRUST-blind");
    let reported = crate::exclusions::CRUST_REPORTED;

    // (paper label, results-dir agent key). Base row first, then the four variants.
    let variants: &[(&str, &str)] = &[
        ("ACTOR (Claude Code)", "claude"),
        ("\\textit{no-subtask}", "claude-no-subtask"),
        ("\\textit{no-iterate}", "claude-no-iter"),
        ("\\textit{no-features}", "claude-no-features"),
        ("\\textit{minimal}", "claude-minimal"),
    ];

    let mut out = String::new();
    out.push_str("% GENERATED by harvest-tools report — do not edit\n");
    for (label, agent) in variants {
        let tp = tractor_pass.get(agent).copied().unwrap_or(0);
        let (sg, _) = crust_bl.get(*agent).copied().unwrap_or((0, reported as u32));
        let (tr, _) = crust_tr.get(*agent).copied().unwrap_or((0, reported as u32));
        out.push_str(&format!(
            "{} & {}/{} & {}/{} & {}/{} \\\\\n",
            label, tp, tractor_total, sg, reported, tr, reported,
        ));
    }

    // Cross-prompt swap block (TRACTOR B0X only). Manual constants — a one-off
    // experiment; unaffected by the CRUST protocol.
    let ps = prompt_sensitivity_manual(repo_root);
    let g = |k: &str| ps.get(k).cloned().unwrap_or_default();
    out.push_str("\\hline \\hline\n");
    out.push_str("\\multicolumn{4}{l}{\\emph{Cross-prompt swap on TRACTOR B0X (n = 210)}} \\\\\n");
    out.push_str("\\hline\n");
    out.push_str(&format!(
        "\\textit{{lib prompt on execs}} & \\multicolumn{{3}}{{l}}{{{}}} \\\\\n",
        g("lib_prompt_on_execs")
    ));
    out.push_str(&format!(
        "\\textit{{exec prompt on libs}} & \\multicolumn{{3}}{{l}}{{{}}} \\\\\n",
        g("exec_prompt_on_libs")
    ));
    out
}

/// Read the `[prompt_sensitivity]` block from `manual_constants.toml` (string
/// values like "59/61"). Empty map if absent.
fn prompt_sensitivity_manual(repo_root: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let path = repo_root.join("manual_constants.toml");
    let Ok(text) = std::fs::read_to_string(&path) else { return out };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else { return out };
    if let Some(table) = doc.get("prompt_sensitivity").and_then(|t| t.as_table()) {
        for (k, v) in table.iter() {
            if let Some(s) = v.as_str() {
                out.insert(k.to_string(), s.to_string());
            }
        }
    }
    out
}

/// Build the LaTeX body (table rows only) for tab:crust's ACTOR and baseline
/// sections. The prior-work leaderboard block and the self-generated baseline
/// cells (GPT-5.4 58, etc.) are manual (manual.tex) and NOT emitted here.
/// Columns: `System & Setting & Builds & Tests & LOC & Unsafe`.
fn generate_crust_tex(repo_root: &Path, _rows: &[BatteryRow]) -> String {
    let mut out = String::new();
    out.push_str("% GENERATED by harvest-tools report — do not edit\n");

    // Passed/total over 87 for each mode/agent.
    let blind = crust_pass_adjusted(repo_root, "CRUST-blind");
    let tr = crust_pass_adjusted(repo_root, "CRUST");
    // Builds counts (compiled among the 87) per mode/agent.
    let blind_builds = crust_build_counts(repo_root, "CRUST-blind");
    let tr_builds = crust_build_counts(repo_root, "CRUST");

    // Helper to format an ACTOR row with computed LOC/Unsafe.
    let actor_row = |out: &mut String, label: &str, setting: &str, mode_dir: &str, agent: &str,
                     builds_map: &std::collections::BTreeMap<String, u32>,
                     pass_map: &std::collections::BTreeMap<String, (u32, u32)>| {
        let (passed, total) = pass_map.get(agent).copied().unwrap_or((0, 0));
        let builds = builds_map.get(agent).copied().unwrap_or(0);
        let (loc, unsafe_lines, _) = crust_loc_unsafe(repo_root, mode_dir, agent);
        let (loc_s, un_s) = if loc > 0 {
            (fmt_k(loc), format!("{}\\%", (unsafe_lines as f64 / loc as f64 * 100.0).round() as u32))
        } else {
            ("--".into(), "--".into())
        };
        out.push_str(&format!(
            "{} & {} & {}/{} & {}/{} & {} & {} \\\\\n",
            label, setting, builds, total, passed, total, loc_s, un_s,
        ));
    };

    // Reported CRUST denominator (87), single-sourced from the exclusions module.
    let reported = crate::exclusions::CRUST_REPORTED;

    // Self-generated (blank setting): ACTOR (Kiro) from CRUST-blind.
    actor_row(&mut out, "ACTOR (Kiro)", "", "CRUST-blind", "kiro", &blind_builds, &blind);

    // Transpiler baselines cannot do CRUST-Bench (no test-repair loop, and the
    // scaffold-fill task is outside their design): all score 0 over the 87 subset.
    // This is a structural fact, not a scored run, so the values are constants.
    for label in ["C2Rust", "Laertes", "C2SaferRust", "SmartC2Rust"] {
        out.push_str(&format!("{} & & 0/{reported} & 0/{reported} & -- & -- \\\\\n", label));
    }

    // LOC/unsafe formatter shared by the baseline rows (counted from the committed
    // crates via crust_baseline_loc_unsafe). Dashes only when no crates were found.
    let baseline_loc_un = |model: &str| -> (String, String) {
        let (loc, un) = crust_baseline_loc_unsafe(repo_root, model);
        if loc > 0 {
            (fmt_k(loc), format!("{}\\%", (un as f64 / loc as f64 * 100.0).round() as u32))
        } else {
            ("--".into(), "--".into())
        }
    };

    // Self-generated baseline rows (no test access), scored on ground-truth tests
    // via the SAME rule as ACTOR (test_report_selfgen.json). LOC/Unsafe counted
    // from the committed baseline crates.
    for (label, out_name) in [
        ("GPT-5.4", "gpt54"),
        ("Kimi K2.5", "kimi_k25"),
        ("Gemini 3.1 Pro", "gemini31pro"),
    ] {
        if let Some((builds, passed, total)) = crust_baseline_selfgen_87(repo_root, out_name) {
            let (loc_s, un_s) = baseline_loc_un(out_name);
            out.push_str(&format!(
                "{} & & {}/{} & {}/{} & {} & {} \\\\\n",
                label, builds, total, passed, total, loc_s, un_s,
            ));
        }
    }

    // ── Ablations section (test-repair) ──
    out.push_str("\\hline\n");
    out.push_str("\\multicolumn{6}{l}{\\emph{Ablations}} \\\\\n");
    out.push_str("\\hline\n");

    // Test-repair ACTOR rows from CRUST.
    for (label, agent) in [
        ("ACTOR (Kiro)", "kiro"),
        ("ACTOR (Claude)", "claude"),
        ("ACTOR (Codex)", "codex-gpt54"),
    ] {
        actor_row(&mut out, label, "test repair", "CRUST", agent, &tr_builds, &tr);
    }

    // Baseline test-repair rows, re-scored over the 87 subset. LOC/Unsafe counted
    // from the committed test-repair baseline crates.
    for (label, out_name) in [
        ("GPT-5.4", "gpt54_test_repair"),
        ("Kimi K2.5", "kimi_k25_test_repair"),
        ("Gemini 3.1 Pro", "gemini31pro_test_repair"),
    ] {
        if let Some((builds, passed, total)) = crust_baseline_87(repo_root, out_name) {
            let (loc_s, un_s) = baseline_loc_un(out_name);
            out.push_str(&format!(
                "{} & test repair & {}/{} & {}/{} & {} & {} \\\\\n",
                label, builds, total, passed, total, loc_s, un_s,
            ));
        }
    }

    // ── Prior-work leaderboard section (over /100, cited external data). Values
    //    come from manual_constants.toml [crust_leaderboard] — NOT our runs. ──
    let lb = crust_leaderboard(repo_root);
    let g = |k: &str| lb.get(k).copied().unwrap_or(0);
    out.push_str("\\hline\n");
    out.push_str("\\multicolumn{6}{l}{\\emph{Prior results from CRUST-Bench leaderboard}~\\cite{KhatryZPWCDD2025}} \\\\\n");
    out.push_str("\\hline\n");
    for (label, setting, b, t) in [
        ("GPT-5", "", "gpt5_compiler_builds", "gpt5_compiler_tests"),
        ("Gemini 3", "", "gemini3_compiler_builds", "gemini3_compiler_tests"),
        ("o3", "", "o3_compiler_builds", "o3_compiler_tests"),
        ("SWE-agent", "test repair", "sweagent_builds", "sweagent_tests"),
        ("GPT-5", "test repair", "gpt5_testrepair_builds", "gpt5_testrepair_tests"),
        ("Gemini 3", "test repair", "gemini3_testrepair_builds", "gemini3_testrepair_tests"),
        ("o3", "test repair", "o3_testrepair_builds", "o3_testrepair_tests"),
    ] {
        out.push_str(&format!(
            "{} & {} & {}/100 & {}/100 & -- & -- \\\\\n",
            label, setting, g(b), g(t),
        ));
    }

    out
}

/// Read the `[crust_leaderboard]` block from `manual_constants.toml` into a map of
/// key -> value. These are external cited results from the CRUST-Bench paper (over
/// /100), NOT our runs, so they live in the manual constants file. Returns an empty
/// map if the file or section is absent (the leaderboard rows then read as 0).
fn crust_leaderboard(repo_root: &Path) -> std::collections::BTreeMap<String, u32> {
    let mut out = std::collections::BTreeMap::new();
    let path = repo_root.join("manual_constants.toml");
    let Ok(text) = std::fs::read_to_string(&path) else { return out };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else { return out };
    if let Some(table) = doc.get("crust_leaderboard").and_then(|t| t.as_table()) {
        for (k, v) in table.iter() {
            if let Some(i) = v.as_integer() {
                out.insert(k.to_string(), i as u32);
            }
        }
    }
    out
}

/// Read the `[crust_gap]` block from `manual_constants.toml`: the human
/// classification of the self-generated-vs-test-repair gap into `defects`
/// (ground-truth test expects behavior the C never produces; ACTOR faithful)
/// and `bugs` (ACTOR genuinely diverges from C). Returns `(defects, bugs)`, or
/// `(0, 0)` if absent. The gap SIZE is derived, not read; a generator invariant
/// asserts `defects + bugs == gap` so this manual split cannot drift.
fn crust_gap_split(repo_root: &Path) -> (u32, u32) {
    let path = repo_root.join("manual_constants.toml");
    let Ok(text) = std::fs::read_to_string(&path) else { return (0, 0) };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else { return (0, 0) };
    let get = |k: &str| doc.get("crust_gap")
        .and_then(|t| t.as_table())
        .and_then(|t| t.get(k))
        .and_then(|v| v.as_integer())
        .unwrap_or(0) as u32;
    (get("defects"), get("bugs"))
}

/// Per-mode/agent count of CRUST projects whose translated crate compiled
/// (build_ok == true) over the canonical 87 subset. Mirrors `crust_pass_adjusted`
/// but reports builds rather than test passes.
fn crust_build_counts(repo_root: &Path, mode_dir: &str) -> std::collections::BTreeMap<String, u32> {
    let mut out = std::collections::BTreeMap::new();
    let dir = repo_root.join("results").join(mode_dir);
    let Ok(agents) = sorted_read_dir(&dir) else { return out };
    for agent_entry in agents {
        if !agent_entry.path().is_dir() { continue; }
        let agent = agent_entry.file_name().to_string_lossy().to_string();
        let mut builds = 0u32;
        if let Ok(projs) = sorted_read_dir(&agent_entry.path()) {
            for pe in projs {
                if !pe.path().is_dir() { continue; }
                let name = pe.file_name().to_string_lossy().to_string();
                if crate::exclusions::is_excluded(name.as_str()) { continue; }
                let rp = pe.path().join("result.json");
                let rp = if rp.exists() { rp } else { pe.path().join("verify/result.json") };
                let Some(r) = read_json::<serde_json::Value>(&rp) else { continue };
                if crate::scoring::CrustOutcome::from_actor(&r).built() { builds += 1; }
            }
        }
        out.insert(agent, builds);
    }
    out
}

/// Emit `\newcommand`s for the manually-entered constants in
/// `manual_constants.toml`. Macro names are `\<CamelSection><CamelKey>`: each of
/// the section and key is split on `_`, every word capitalized, and concatenated
/// (e.g. section `unsafe_categories`, key `c_abi_preservation` ->
/// `\UnsafeCategoriesCAbiPreservation`). If the TOML file is missing, only the
/// header comment is emitted (never fails).
fn generate_manual_tex(repo_root: &Path) -> String {
    let mut out = String::new();
    out.push_str("% MANUALLY ENTERED — not derived from data. Source: manual_constants.toml\n");

    let path = repo_root.join("manual_constants.toml");
    let Ok(text) = std::fs::read_to_string(&path) else { return out };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else { return out };

    // CamelCase a snake_case identifier: split on '_', capitalize each word.
    let camel = |s: &str| -> String {
        let mut r = String::new();
        for word in s.split('_') {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                r.extend(first.to_uppercase());
                r.push_str(chars.as_str());
            }
        }
        r
    };
    // Render a toml value the way it appears in the file (unquoted scalars).
    let render = |v: &toml_edit::Value| -> String {
        match v {
            toml_edit::Value::String(s) => s.value().to_string(),
            toml_edit::Value::Integer(i) => i.value().to_string(),
            toml_edit::Value::Float(f) => f.value().to_string(),
            toml_edit::Value::Boolean(b) => b.value().to_string(),
            other => other.to_string().trim().to_string(),
        }
    };

    for (section, item) in doc.iter() {
        let Some(table) = item.as_table() else { continue };
        let sec_camel = camel(section);
        for (key, val) in table.iter() {
            let Some(v) = val.as_value() else { continue };
            let macro_name = format!("{}{}", sec_camel, camel(key));
            out.push_str(&format!("\\newcommand{{\\{}}}{{{}}}\n", macro_name, render(v)));
        }
    }

    out
}

/// Per-case build outcome (crate compiles?) for one agent, keyed by
/// "battery/case". Reads the runner's own build result from result.json.
fn case_builds(repo_root: &Path, agent: &str) -> std::collections::BTreeMap<String, bool> {
    let mut out = std::collections::BTreeMap::new();
    let agent_dir = repo_root.join("results/Test-Corpus").join(agent);
    let Ok(bats) = sorted_read_dir(&agent_dir) else { return out };
    for be in bats {
        if !be.path().is_dir() { continue; }
        let battery = be.file_name().to_string_lossy().to_string();
        let Ok(cases) = sorted_read_dir(&be.path()) else { continue };
        for ce in cases {
            if !ce.path().is_dir() { continue; }
            let case = ce.file_name().to_string_lossy().to_string();
            let Some(r) = read_json::<CaseResult>(&ce.path().join("result.json")) else { continue };
            out.insert(format!("{battery}/{case}"), r.error.as_deref() != Some("build failed"));
        }
    }
    out
}

/// Count cases where Laertes breaks a compilation that C2Rust had working
/// (and, separately, where it fixes one C2Rust could not build).
fn laertes_vs_c2rust(repo_root: &Path) -> (u32, u32) {
    let c2 = case_builds(repo_root, "c2rust");
    let la = case_builds(repo_root, "laertes");
    let (mut broke, mut fixed) = (0u32, 0u32);
    for (case, c2_ok) in &c2 {
        if let Some(&la_ok) = la.get(case) {
            if *c2_ok && !la_ok { broke += 1; }
            if !*c2_ok && la_ok { fixed += 1; }
        }
    }
    (broke, fixed)
}

/// Sum ACTOR credits and wall-seconds over a results tree. Returns
/// `(total_credits, verify_credits, total_wall_secs)` across the translate and
/// verify phases of every result.json under `base`. Shared-source duplicates carry
/// no credits (only the real translation does), so they don't double-count.
fn kiro_cost(base: &Path) -> (f64, f64, u64) {
    let (mut total, mut verify, mut secs) = (0.0f64, 0.0f64, 0u64);
    let Ok(rd) = std::fs::read_dir(base) else { return (0.0, 0.0, 0) };
    let mut stack: Vec<std::path::PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.filter_map(|e| e.ok().map(|e| e.path())));
            }
        } else if p.file_name().is_some_and(|n| n == "result.json") {
            if let Some(r) = read_json::<serde_json::Value>(&p) {
                for ph in ["translate", "verify"] {
                    let c = r.pointer(&format!("/{ph}/credits")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let s = r.pointer(&format!("/{ph}/wall_secs")).and_then(|v| v.as_u64()).unwrap_or(0);
                    total += c; secs += s;
                    if ph == "verify" { verify += c; }
                }
            }
        }
    }
    (total, verify, secs)
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
    let crust_bl = crust_pass_adjusted(repo_root, "CRUST-blind");

    let g = |m: &HashMap<&str, u32>, k: &str| m.get(k).copied().unwrap_or(0);
    let mut o = String::new();
    o.push_str("% GENERATED by `harvest-tools report`. Do not edit by hand.\n");
    o.push_str("% Named constants for result numbers quoted in the prose, so text and\n");
    o.push_str("% tables cannot disagree. Values are derived from results/.\n");
    o.push_str(&format!("\\newcommand{{\\TractorTotalCases}}{{{}}}\n", total_cases));
    // TRACTOR C-LOC total and mean-per-case, from the SAME deduped counting as
    // tab:datasets (so prose and the datasets table agree). Total = sum of the
    // per-battery deduped C LOC; mean = total / TractorTotalCases.
    let corpus = repo_root.join("test-corpus/Public-Tests");
    let tractor_total_loc: u32 = TRACTOR_BATTERY_DIRS.iter()
        .flat_map(|d| tractor_battery_c_locs(&corpus, d))
        .sum();
    if tractor_total_loc > 0 && total_cases > 0 {
        let mean = (tractor_total_loc as f64 / total_cases as f64).round() as u32;
        o.push_str(&format!("\\newcommand{{\\TractorTotalLoc}}{{{}}}\n", fmt_commas(tractor_total_loc)));
        o.push_str(&format!("\\newcommand{{\\TractorMeanLoc}}{{{}}}\n", mean));
    }
    // CRUST-Bench full-benchmark mean C LOC over all 100 projects (prose describes
    // the whole benchmark before we restrict to the 87-project subset).
    let crust_all = crust_bench_c_locs(repo_root, true);
    if !crust_all.is_empty() {
        let mean = (crust_all.iter().map(|&x| x as u64).sum::<u64>() as f64
            / crust_all.len() as f64).round() as u32;
        o.push_str(&format!("\\newcommand{{\\CrustBenchMeanLoc}}{{{}}}\n", fmt_commas(mean)));
    }

    // ── ACTOR (Kiro) cost/time, derived from result.json credits at the Kiro
    //    Power add-on rate of $0.04/credit (translate + verify phases). Claude and
    //    Codex do not record credits, so their costs stay manual in the prose.
    const USD_PER_CREDIT: f64 = 0.04;
    // Translated Rust LOC per benchmark (kiro), used as the per-kLOC denominator.
    // TRACTOR: sum of the deduped per-battery Rust LOC in `rows`. CRUST: the
    // self-generated (blind) kiro LOC. Both match the LOC the tables report.
    let tractor_rust_loc: u32 = rows.iter().filter(|r| r.agent == "kiro").map(|r| r.total_loc).sum();
    let (crust_rust_loc, _, _) = crust_loc_unsafe(repo_root, "CRUST-blind", "kiro");
    // (label, results dir, translated-Rust-kLOC denominator)
    let cost_rows: &[(&str, &str, u32)] = &[
        ("Tractor", "results/Test-Corpus/kiro", tractor_rust_loc),
        ("Crust", "results/CRUST-blind/kiro", crust_rust_loc),
    ];
    for (name, base, rust_loc) in cost_rows {
        let (credits, verify_credits, secs) = kiro_cost(&repo_root.join(base));
        if credits <= 0.0 { continue; }
        let dollars = credits * USD_PER_CREDIT;
        let minutes = secs as f64 / 60.0;
        let kloc = *rust_loc as f64 / 1000.0;
        o.push_str(&format!("\\newcommand{{\\Cost{name}}}{{{:.0}}}\n", dollars));
        o.push_str(&format!("\\newcommand{{\\Cost{name}Minutes}}{{{:.0}}}\n", minutes));
        if kloc > 0.0 {
            o.push_str(&format!("\\newcommand{{\\Cost{name}PerKLoc}}{{{:.2}}}\n", dollars / kloc));
            o.push_str(&format!("\\newcommand{{\\Cost{name}MinPerKLoc}}{{{:.0}}}\n", minutes / kloc));
        }
        // Validation (verify phase) share of total cost.
        if credits > 0.0 {
            o.push_str(&format!("\\newcommand{{\\Cost{name}ValidatePct}}{{{:.0}}}\n",
                verify_credits / credits * 100.0));
        }
    }
    // P01 (SPHINCS+) single-project cost/time (kiro), the paper's exemplar.
    let (p01_cr, _, p01_secs) = kiro_cost(&repo_root.join("results/Test-Corpus/kiro/P01_sphincs_plus"));
    if p01_cr > 0.0 {
        o.push_str(&format!("\\newcommand{{\\CostPOne}}{{{:.2}}}\n", p01_cr * USD_PER_CREDIT));
        o.push_str(&format!("\\newcommand{{\\CostPOneMinutes}}{{{:.0}}}\n", p01_secs as f64 / 60.0));
    }

    o.push_str(&format!("\\newcommand{{\\ActorKiroTests}}{{{}/{}}}\n", g(&total_tests, "kiro"), total_cases));
    o.push_str(&format!("\\newcommand{{\\ActorClaudeTests}}{{{}/{}}}\n", g(&total_tests, "claude"), total_cases));
    o.push_str(&format!("\\newcommand{{\\ActorCodexTests}}{{{}/{}}}\n", g(&total_tests, "codex-gpt54"), total_cases));
    // Paper terminology is "no validate" (the verify/validate phase, skipped); the
    // agent dir is `kiro-translate` (translation snapshot before that phase). Macro
    // name MUST match what paper.tex references (\ActorKiroNoValidate...), else the
    // wired paper hits an undefined control sequence.
    o.push_str(&format!("\\newcommand{{\\ActorKiroNoValidateTests}}{{{}/{}}}\n", g(&total_tests, "kiro-translate"), total_cases));
    o.push_str(&format!("\\newcommand{{\\ActorKiroPOneTests}}{{{}/128}}\n", g(&p01_tests, "kiro")));
    o.push_str(&format!("\\newcommand{{\\ActorKiroNoValidatePOneTests}}{{{}/128}}\n", g(&p01_tests, "kiro-translate")));
    let (kiro_p, kiro_t) = crust_tr.get("kiro").copied().unwrap_or((0, 0));
    o.push_str(&format!("\\newcommand{{\\CrustKiroTestRepair}}{{{}/{}}}\n", kiro_p, kiro_t));
    // ACTOR (Kiro) self-generated (no test access) CRUST pass rate, for prose that
    // contrasts it with test-repair. Same source as the tab:crust self-gen row.
    let (kbp, kbt) = crust_bl.get("kiro").copied().unwrap_or((0, 0));
    o.push_str(&format!("\\newcommand{{\\CrustKiroSelfGen}}{{{}/{}}}\n", kbp, kbt));

    // ── Self-generated vs test-repair gap, for the "tests encode a specification
    //    absent from the C" prose. The reported denominator and the count of
    //    excluded suites come straight from the exclusions module (single source).
    //    The gap SIZE is derived here (test-repair passes − self-gen passes for
    //    kiro) so it can never disagree with the tab:crust cells; the split of
    //    that gap into benchmark defects vs genuine bugs is a manual per-project
    //    judgment read from manual_constants.toml, and an invariant below asserts
    //    defects + bugs == gap so the manual split cannot drift from the data.
    o.push_str(&format!("\\newcommand{{\\CrustReported}}{{{}}}\n",
        crate::exclusions::CRUST_REPORTED));
    o.push_str(&format!("\\newcommand{{\\CrustExcluded}}{{{}}}\n",
        crate::exclusions::excluded_count()));
    let gap = kiro_p.saturating_sub(kbp);
    o.push_str(&format!("\\newcommand{{\\CrustGapProjects}}{{{}}}\n", gap));
    let (gap_defects, gap_bugs) = crust_gap_split(repo_root);
    o.push_str(&format!("\\newcommand{{\\CrustGapDefects}}{{{}}}\n", gap_defects));
    o.push_str(&format!("\\newcommand{{\\CrustGapBugs}}{{{}}}\n", gap_bugs));
    // Next-best test-blind system, to substantiate the ceiling claim. Derived from
    // the SAME re-scored self-gen baseline path as the tab:crust GPT-5.4 row.
    if let Some((_, gpt_pass, gpt_total)) = crust_baseline_selfgen_87(repo_root, "gpt54") {
        o.push_str(&format!("\\newcommand{{\\CrustGptFourSelfGen}}{{{}/{}}}\n", gpt_pass, gpt_total));
    }

    let (laertes_breaks, laertes_fixes) = laertes_vs_c2rust(repo_root);
    o.push_str(&format!("\\newcommand{{\\LaertesBreaks}}{{{}}}\n", laertes_breaks));
    o.push_str(&format!("\\newcommand{{\\LaertesFixes}}{{{}}}\n", laertes_fixes));

    // NOTE: CRUST baseline numbers (GPT-5.4/Kimi/Gemini) are emitted ONLY as
    // table rows in crust.tex, via the single `crust_baseline_87` path. We do not
    // also emit them as prose macros here: two code paths for the same quantity is
    // exactly the drift risk this generator exists to remove. If the prose ever
    // needs a baseline number, add a macro that calls `crust_baseline_87` so it is
    // by construction the same value as the table cell.
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
    ("\\makebox[\\knvLength][l]{ACTOR (Kiro, no validate)}", "kiro-translate", true),
    ("C2Rust", "c2rust", false),
    ("Laertes", "laertes", false),
    ("C2SaferRust", "c2saferrust", false),
    ("SmartC2Rust", "smartc2rust", false),
    ("Kimi K2.5 (query)", "kimi", false),
    ("GPT-5.4 (query)", "gpt-5.4", false),
    ("Gemini 3.1 Pro (query)", "gemini-3.1-pro-preview", false),
];

/// Battery labels and their results-directory names, in paper order. Each entry is
/// (line-1 label on the first agent row, line-2 label on the second agent row, dir).
/// P00/P01 split the battery name across two rows to match the paper's layout; the
/// others put the whole label on line 1.
const TRACTOR_BATTERIES: &[(&str, &str, &str)] = &[
    ("B01-syn", "", "B01_synthetic"),
    ("B01-org", "", "B01_organic"),
    ("B02-syn", "", "B02_synthetic"),
    ("B02-org", "", "B02_organic"),
    ("P00", "(Perlin)", "P00_perlin_noise"),
    ("P01", "\\makebox[1cm][l]{\\smaller (SPHINCS+)}", "P01_sphincs_plus"),
    ("Total", "", ""), // empty battery dir => aggregate across all
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
        .filter_map(|(_, _, d)| (!d.is_empty()).then(|| battery_size.get(d).copied().unwrap_or(0)))
        .sum();

    // For each (agent, battery-or-Total) produce built/tested/tests_pass/loc/unsafe,
    // always over the full battery size so untranslated cases count as failures.
    struct Cell { built: u32, denom: u32, tests_pass: u32, loc: u32, unsafe_lines: u32, present: bool }
    let cell = |agent: &str, bat_dir: &str| -> Cell {
        if bat_dir.is_empty() {
            // Total: sum across all batteries, denom = full corpus size.
            let (mut b, mut tp, mut loc, mut un) = (0u32, 0u32, 0u32, 0u32);
            let mut present = false;
            for (_, _, bd) in TRACTOR_BATTERIES.iter().filter(|(_, _, d)| !d.is_empty()) {
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

    for (bi, (bat_line1, bat_line2, bat_dir)) in TRACTOR_BATTERIES.iter().enumerate() {
        // Best Tests among ACTOR harness rows in this battery, for bold.
        let best = TRACTOR_TABLE_ROWS.iter().filter(|(_, _, actor)| *actor)
            .map(|(_, a, _)| cell(a, bat_dir).tests_pass)
            .max().unwrap_or(0);
        for (ri, (label, agent, _)) in TRACTOR_TABLE_ROWS.iter().enumerate() {
            let c = cell(agent, bat_dir);
            let denom = c.denom;
            // Battery label spans up to two rows: line 1 on the first agent row,
            // line 2 (if any) on the second — matching the paper's P00/P01 layout.
            let first_col = match ri {
                0 => *bat_line1,
                1 => *bat_line2,
                _ => "",
            };
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
/// Aggregate per-case results for one battery into (Rust LOC, unsafe lines, cases
/// built). `bat_dir` is the agent's results battery dir; `corpus_bat_dir` is the
/// matching corpus battery dir (test-corpus/Public-Tests/<battery>), used to
/// deduplicate the SHARED-SOURCE LOC.
///
/// Two different quantities with two different rules:
///   * `built` (the "Compiles" column) counts EVERY test config, matching the
///     paper (e.g. P01 = 128/128): all 128 configs are compiled and run, even
///     though they share one translation.
///   * `loc`/`unsafe` describe the TRANSLATED CODE, which for a shared-source
///     group (P00/P01) exists once. Every config's result.json repeats the same
///     LOC, so summing would multiply-count it. We add LOC/unsafe only for the
///     real case of each group, skipping symlinked followers (corpus `test_case/`
///     is a symlink). This is the authoritative grouping, not a LOC-ratio guess.
///
/// If the corpus is absent we cannot resolve grouping, so LOC/unsafe sum every
/// case (correct for the all-independent batteries; only P00/P01 need dedup and
/// those require the corpus anyway).
fn aggregate_cases(bat_dir: &Path, corpus_bat_dir: &Path) -> (u32, u32, u32) {
    let corpus_present = corpus_bat_dir.is_dir();
    let (mut total_loc, mut total_unsafe, mut built) = (0u32, 0u32, 0u32);
    let Ok(entries) = std::fs::read_dir(bat_dir) else { return (0, 0, 0) };
    for entry in entries.flatten() {
        if !entry.path().is_dir() { continue; }
        let result_path = entry.path().join("result.json");
        let cr: CaseResult = match read_json(&result_path) {
            Some(r) => r,
            None => continue,
        };
        // Compiles: count every config (all are built and run).
        if cr.error.as_deref() != Some("build failed") {
            built += 1;
        }
        // LOC/unsafe describe the translated code: skip shared-source followers
        // (corpus test_case is a symlink) so a single translation is counted once.
        if corpus_present {
            let tc = corpus_bat_dir.join(entry.file_name()).join("test_case");
            if tc.is_symlink() { continue; }
        }
        total_loc += cr.loc.map_or(0, |l| l.code);
        total_unsafe += cr.unsafe_.map_or(0, |u| u.lines);
    }
    (total_loc, total_unsafe, built)
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
/// Reads from test-corpus/Public-Tests/<battery>/<case>/test_case/. Shared-source
/// cases symlink their `test_case/` to one real source; we count each DISTINCT
/// real source once (resolving the symlink), so P01's 128 configs contribute their
/// one source a single time. This is the authoritative grouping, not a LOC ratio.
fn count_c_loc_battery(test_corpus_dir: &Path, battery: &str) -> u32 {
    let bat_dir = test_corpus_dir.join(battery);
    let Ok(entries) = std::fs::read_dir(&bat_dir) else { return 0 };
    let mut total = 0u32;
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() { continue; }
        let test_case = entry.path().join("test_case");
        if !test_case.is_dir() { continue; }
        let real = std::fs::canonicalize(&test_case).unwrap_or_else(|_| test_case.clone());
        if !seen.insert(real) { continue; }
        total += count_c_loc_dir(&test_case);
    }
    total
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
