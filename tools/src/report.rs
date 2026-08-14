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

            // Headline (validated) row: verified-phase summary + per-case data.
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
                battery: battery.clone(),
                cases_passed: summary.cases_passed,
                cases_tested: summary.cases_tested,
                cases_built,
                vectors_passed: summary.vectors_passed,
                vectors_total: summary.vectors_passed + summary.vectors_failed,
                c_loc,
                total_loc,
                unsafe_lines,
            });

            // No-validate ("kiro-translate") virtual agent: kiro's PRE-verify
            // numbers, read from summary_translated.json + each case's
            // translated/result.json. Emitted as a synthetic agent row so the
            // \ActorKiroNoValidate* macros and TRACTOR_TABLE_ROWS keep reading
            // it by the "kiro-translate" key without a separate results tree.
            if agent == "kiro" {
                if let Some(nv) = read_json::<Summary>(&bat_dir.join("summary_translated.json")) {
                    let (nv_loc, nv_unsafe, nv_built) = aggregate_cases_phase(
                        &bat_dir, &test_corpus_dir.join(&battery), Some(crate::battery::TRANSLATED),
                    );
                    rows.push(BatteryRow {
                        agent: "kiro-translate".to_string(),
                        battery: battery.clone(),
                        cases_passed: nv.cases_passed,
                        cases_tested: nv.cases_tested,
                        cases_built: nv_built,
                        vectors_passed: nv.vectors_passed,
                        vectors_total: nv.vectors_passed + nv.vectors_failed,
                        c_loc,
                        total_loc: nv_loc,
                        unsafe_lines: nv_unsafe,
                    });
                }
            }
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

    // Dataset-size table body (tab:datasets), derived from the C corpus.
    let datasets = generate_datasets_tex(repo_root);
    let datasets_path = tables_dir.join("datasets.tex");
    std::fs::write(&datasets_path, &datasets)?;
    println!("✅ Wrote {}", datasets_path.display());

    // Prompt-sensitivity ablations (tab:prompt-sensitivity): base + 4 variants,
    // each scored on TRACTOR.
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

    // (1c) The ACTOR agent directories that feed prose macros and TRACTOR
    //      rows must exist. A renamed/missing dir otherwise emits a plausible
    //      all-zeros row (e.g. "0/338") that trips no other invariant and is
    //      indistinguishable from a legitimate baseline zero.
    // kiro-translate is NOT a real dir anymore — it's the no-validate virtual
    // agent derived from kiro's translated/ phase. Its presence is guaranteed
    // by the summary_translated.json invariant just below, not a dir check.
    for agent in ["kiro", "claude", "codex-gpt54"] {
        anyhow::ensure!(
            results_dir.join(agent).is_dir(),
            "TRACTOR agent-dir invariant failed: results/Test-Corpus/{agent} is missing \
             (a rename would silently emit a 0/338 row). Fix the mapping or restore the dir."
        );
    }
    // The no-validate row is derived from kiro's pre-verify phase; require at
    // least one battery to carry summary_translated.json so a missing pre-verify
    // scoring pass can't silently emit a 0/338 no-validate row.
    anyhow::ensure!(
        sorted_read_dir(&results_dir.join("kiro")).map(|bats| bats.iter().any(|b|
            b.path().join("summary_translated.json").is_file())).unwrap_or(false),
        "no-validate invariant failed: no results/Test-Corpus/kiro/<battery>/summary_translated.json \
         found (the pre-verify scoring pass did not run; \\ActorKiroNoValidate* would be 0/338)."
    );
    // (2) Shared-source dedup must have happened: P01/P00 collapse to one distinct
    //     source. If the corpus is present but symlinks did not survive checkout
    //     (materialized as real dirs), every config would be counted and LOC would
    //     inflate ~120x silently. Fail loudly instead. (Skip when corpus absent —
    //     then dedup can't run and datasets.tex omits these rows anyway.)
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

/// Emit the LaTeX body rows for tab:datasets: one row per TRACTOR battery, each
/// `Dataset & Cases & Total & Mean & Median & Max`.
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

    out
}

/// Build the LaTeX body (rows only) for tab:prompt-sensitivity: the base
/// ACTOR (Claude Code) run plus four prompt-ablation variants, each scored on
/// TRACTOR (tests passed / total cases). Followed by the cross-prompt swap block
/// (TRACTOR B0X, split by exec/lib via the `is_lib` naming); those two figures
/// come from `manual_constants.toml [prompt_sensitivity]` because the swap runs
/// are a one-off TRACTOR-subset experiment not re-derived here.
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
        out.push_str(&format!("{} & {}/{} \\\\\n", label, tp, tractor_total));
    }

    // Cross-prompt swap block (TRACTOR B0X only). Manual constants — a one-off
    // experiment.
    let ps = prompt_sensitivity_manual(repo_root);
    let g = |k: &str| ps.get(k).cloned().unwrap_or_default();
    out.push_str("\\hline \\hline\n");
    out.push_str("\\multicolumn{2}{l}{\\emph{Cross-prompt swap on TRACTOR B0X (n = 210)}} \\\\\n");
    out.push_str("\\hline\n");
    out.push_str(&format!(
        "\\textit{{lib prompt on execs}} & {} \\\\\n",
        g("lib_prompt_on_execs")
    ));
    out.push_str(&format!(
        "\\textit{{exec prompt on libs}} & {} \\\\\n",
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
            let rp = crate::battery::crate_dir(&ce.path()).join("result.json");
            let Some(r) = read_json::<CaseResult>(&rp) else { continue };
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
/// verify phases.
///
/// Reads exactly ONE result.json per case — the canonical phase dir (verified/
/// if present, else translated/, via [`crate::battery::crate_dir`]). This is
/// essential now that each case has BOTH translated/result.json and (when
/// verified) verified/result.json: the verified/ result.json carries the full
/// translate+verify credit breakdown, so reading only it avoids double-counting
/// the translate phase. A case dir is any directory that contains a translated/
/// phase; shared-source duplicates carry no credits so they don't inflate.
fn kiro_cost(base: &Path) -> (f64, f64, u64) {
    let (mut total, mut verify, mut secs) = (0.0f64, 0.0f64, 0u64);
    let Ok(rd) = std::fs::read_dir(base) else { return (0.0, 0.0, 0) };
    let mut stack: Vec<std::path::PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    while let Some(p) = stack.pop() {
        if !p.is_dir() { continue; }
        // A case dir is one that has a translated/ phase. Read its canonical
        // result.json once; do NOT also descend into its phase dirs (which each
        // carry their own result.json and would double-count).
        if crate::battery::phase_dir(&p, crate::battery::TRANSLATED).is_dir() {
            let rp = crate::battery::crate_dir(&p).join("result.json");
            if let Some(r) = read_json::<serde_json::Value>(&rp) {
                for ph in ["translate", "verify"] {
                    let c = r.pointer(&format!("/{ph}/credits")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let s = r.pointer(&format!("/{ph}/wall_secs")).and_then(|v| v.as_u64()).unwrap_or(0);
                    total += c; secs += s;
                    if ph == "verify" { verify += c; }
                }
            }
        } else if let Ok(rd) = std::fs::read_dir(&p) {
            stack.extend(rd.filter_map(|e| e.ok().map(|e| e.path())));
        }
    }
    (total, verify, secs)
}

/// Emit \newcommand constants for result numbers that are quoted in the prose.
/// Only numbers that are derived directly from the committed results appear
/// here; figures the results data does not reproduce are intentionally left to
/// the paper text.
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
    // ── ACTOR (Kiro) cost/time, derived from result.json credits at the Kiro
    //    Power add-on rate of $0.04/credit (translate + verify phases). Claude and
    //    Codex do not record credits, so their costs stay manual in the prose.
    const USD_PER_CREDIT: f64 = 0.04;
    // Translated Rust LOC (kiro), used as the per-kLOC denominator: the sum of
    // the deduped per-battery Rust LOC in `rows`, matching what the tables report.
    let tractor_rust_loc: u32 = rows.iter().filter(|r| r.agent == "kiro").map(|r| r.total_loc).sum();
    // (label, results dir, translated-Rust-kLOC denominator)
    let cost_rows: &[(&str, &str, u32)] = &[
        ("Tractor", "results/Test-Corpus/kiro", tractor_rust_loc),
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
    let (laertes_breaks, laertes_fixes) = laertes_vs_c2rust(repo_root);
    o.push_str(&format!("\\newcommand{{\\LaertesBreaks}}{{{}}}\n", laertes_breaks));
    o.push_str(&format!("\\newcommand{{\\LaertesFixes}}{{{}}}\n", laertes_fixes));
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
    aggregate_cases_phase(bat_dir, corpus_bat_dir, None)
}

/// Aggregate LOC/unsafe/built across a battery's cases. `phase` selects which
/// phase dir's result.json to read: `Some("translated")` for the pre-verify
/// (no-validate) numbers, `None` for the current/headline phase (verified/ if
/// present, else translated/ — the reader rule).
fn aggregate_cases_phase(bat_dir: &Path, corpus_bat_dir: &Path, phase: Option<&str>) -> (u32, u32, u32) {
    let corpus_present = corpus_bat_dir.is_dir();
    let (mut total_loc, mut total_unsafe, mut built) = (0u32, 0u32, 0u32);
    let Ok(entries) = std::fs::read_dir(bat_dir) else { return (0, 0, 0) };
    for entry in entries.flatten() {
        if !entry.path().is_dir() { continue; }
        let phase_dir = match phase {
            Some(p) => crate::battery::phase_dir(&entry.path(), p),
            None => crate::battery::crate_dir(&entry.path()),
        };
        let result_path = phase_dir.join("result.json");
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
