use crate::battery::Credits;
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Summary {
    cases_passed: u32,
    cases_tested: u32,
    vectors_passed: u32,
    vectors_failed: u32,
}

/// Read for LOC / unsafe / build status only: the pass/fail verdict comes from
/// the battery `summary.json`, not from here.
#[derive(Deserialize)]
struct CaseResult {
    /// `Some("build failed")` iff the crate did not compile; any other value,
    /// or absent, means it compiled.
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
    cases_built: u32,
    vectors_passed: u32,
    vectors_total: u32,
    c_loc: u32,
    total_loc: u32,
    unsafe_lines: u32,
}

pub fn generate(repo_root: &Path) -> Result<()> {
    let results_dir = repo_root.join("results/Test-Corpus");
    let test_corpus_dir = repo_root.join("test-corpus/Public-Tests");
    let tables_dir = repo_root.join("tables");
    std::fs::create_dir_all(&tables_dir)?;

    // results.md's "C LOC" column comes from the test-corpus submodule; without
    // it every C LOC would silently become 0 and clobber the committed file, so
    // results.md is skipped. tractor.tex uses Rust LOC and never needs the corpus.
    let has_c_corpus = test_corpus_dir.is_dir();

    // C LOC is a property of the battery, identical for every agent.
    let mut c_loc_cache: BTreeMap<String, u32> = BTreeMap::new();

    let mut rows: Vec<BatteryRow> = Vec::new();

    for agent_entry in sorted_read_dir(&results_dir)? {
        let agent = agent_entry.file_name().to_string_lossy().to_string();
        let agent_dir = agent_entry.path();
        if !agent_dir.is_dir() {
            continue;
        }

        for bat_entry in sorted_read_dir(&agent_dir)? {
            let battery = bat_entry.file_name().to_string_lossy().to_string();
            let bat_dir = bat_entry.path();
            if !bat_dir.is_dir() {
                continue;
            }

            // Headline row: the verified phase.
            let summary_path = bat_dir.join("summary.json");
            let summary: Summary = match read_json(&summary_path) {
                Some(s) => s,
                None => continue,
            };

            let totals = aggregate_cases(&bat_dir, &test_corpus_dir.join(&battery));

            let c_loc = *c_loc_cache
                .entry(battery.clone())
                .or_insert_with(|| count_c_loc_battery(&test_corpus_dir, &battery));

            rows.push(BatteryRow {
                agent: agent.clone(),
                battery: battery.clone(),
                cases_passed: summary.cases_passed,
                cases_tested: summary.cases_tested,
                cases_built: totals.cases_built,
                vectors_passed: summary.vectors_passed,
                vectors_total: summary.vectors_passed + summary.vectors_failed,
                c_loc,
                total_loc: totals.total_loc,
                unsafe_lines: totals.unsafe_lines,
            });

            // Kiro's PRE-verify numbers, emitted as a synthetic "kiro-translate"
            // agent row so \ActorKiroNoValidate* and TRACTOR_TABLE_ROWS can key
            // on it without a separate results tree.
            if agent == "kiro" {
                if let Some(nv) = read_json::<Summary>(&bat_dir.join("summary_translated.json")) {
                    let nv_totals = aggregate_cases_phase(
                        &bat_dir,
                        &test_corpus_dir.join(&battery),
                        Some(crate::battery::TRANSLATED),
                    );
                    rows.push(BatteryRow {
                        agent: "kiro-translate".to_string(),
                        battery: battery.clone(),
                        cases_passed: nv.cases_passed,
                        cases_tested: nv.cases_tested,
                        cases_built: nv_totals.cases_built,
                        vectors_passed: nv.vectors_passed,
                        vectors_total: nv.vectors_passed + nv.vectors_failed,
                        c_loc,
                        total_loc: nv_totals.total_loc,
                        unsafe_lines: nv_totals.unsafe_lines,
                    });
                }
            }
        }
    }

    let mut by_battery: BTreeMap<String, Vec<&BatteryRow>> = BTreeMap::new();
    for row in &rows {
        by_battery.entry(row.battery.clone()).or_default().push(row);
    }

    let mut all = String::new();
    writeln!(all, "# Translation Results\n")?;
    writeln!(
        all,
        "Auto-generated from validated `result.json` and `summary.json` files.\n"
    )?;

    for (battery, brows) in &by_battery {
        writeln!(all, "## {battery}\n")?;
        writeln!(all, "| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |")?;
        writeln!(
            all,
            "|-------|-------------|----------------|-------|----------|-------------|----------|"
        )?;
        for r in brows {
            let unsafe_pct = if r.total_loc > 0 {
                format!("{:.1}%", r.unsafe_lines as f64 / r.total_loc as f64 * 100.0)
            } else {
                "N/A".into()
            };
            writeln!(
                all,
                "| {} | {}/{} | {}/{} | {} | {} | {} | {} |",
                r.agent,
                r.cases_passed,
                r.cases_tested,
                r.vectors_passed,
                r.vectors_total,
                r.c_loc,
                r.total_loc,
                r.unsafe_lines,
                unsafe_pct,
            )?;
        }
        writeln!(all)?;
    }

    writeln!(all, "## Summary: Cases Passed\n")?;
    let agents: Vec<String> = rows
        .iter()
        .map(|r| r.agent.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    write!(all, "| Battery |")?;
    for a in &agents {
        write!(all, " {} |", a)?;
    }
    writeln!(all)?;
    write!(all, "|---------|")?;
    for _ in &agents {
        write!(all, "------|")?;
    }
    writeln!(all)?;
    for (battery, brows) in &by_battery {
        let lookup: BTreeMap<&str, &BatteryRow> =
            brows.iter().map(|r| (r.agent.as_str(), *r)).collect();
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

    // See has_c_corpus: rewriting without the corpus would zero the C LOC column.
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

    let tex = generate_tractor_tex(&rows);
    let tex_path = tables_dir.join("tractor.tex");
    std::fs::write(&tex_path, &tex)?;
    println!("✅ Wrote {}", tex_path.display());

    let numbers = generate_numbers_tex(&rows, repo_root);
    let numbers_path = tables_dir.join("numbers.tex");
    std::fs::write(&numbers_path, &numbers)?;
    println!("✅ Wrote {}", numbers_path.display());

    // tab:datasets body.
    let datasets = generate_datasets_tex(repo_root);
    let datasets_path = tables_dir.join("datasets.tex");
    std::fs::write(&datasets_path, &datasets)?;
    println!("✅ Wrote {}", datasets_path.display());

    // tab:prompt-sensitivity body.
    let promptsens = generate_prompt_sensitivity_tex(repo_root, &rows);
    let promptsens_path = tables_dir.join("prompt-sensitivity.tex");
    std::fs::write(&promptsens_path, &promptsens)?;
    println!("✅ Wrote {}", promptsens_path.display());

    let manual = generate_manual_tex(repo_root);
    let manual_path = tables_dir.join("manual.tex");
    std::fs::write(&manual_path, &manual)?;
    println!("✅ Wrote {}", manual_path.display());

    // ── Invariant checks. Deliberately over the DATA the tables were built
    //    from, not over compile-time constants: a renamed/missing input dir or a
    //    lost symlink must fail generation, not silently change a number. ──

    // (1) 338 cases. Max-over-agents works because the ACTOR harnesses run
    //     every case.
    let mut per_agent_total: BTreeMap<&str, u32> = BTreeMap::new();
    for r in &rows {
        *per_agent_total.entry(r.agent.as_str()).or_insert(0) += r.cases_tested;
    }
    let tractor_total = per_agent_total.values().copied().max().unwrap_or(0);
    anyhow::ensure!(
        tractor_total == 338,
        "TRACTOR invariant failed: max agent case total is {tractor_total}, expected 338"
    );

    // (1b) Also pin each battery, so compensating errors cannot inflate one
    //      denominator while the total still sums to 338.
    const EXPECTED_BATTERY_SIZES: &[(&str, u32)] = &[
        ("B01_synthetic", 85),
        ("B01_organic", 38),
        ("B02_synthetic", 42),
        ("B02_organic", 44),
        ("P00_perlin_noise", 1),
        ("P01_sphincs_plus", 128),
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

    // (1c) A renamed/missing agent dir emits a plausible all-zeros row ("0/338")
    //      that trips no other invariant and is indistinguishable from a genuine
    //      baseline zero. kiro-translate is absent here on purpose: it is the
    //      virtual no-validate agent, covered by the invariant just below.
    for agent in ["kiro", "claude", "codex-gpt54"] {
        anyhow::ensure!(
            results_dir.join(agent).is_dir(),
            "TRACTOR agent-dir invariant failed: results/Test-Corpus/{agent} is missing \
             (a rename would silently emit a 0/338 row). Fix the mapping or restore the dir."
        );
    }
    // Without at least one summary_translated.json the pre-verify scoring pass
    // did not run, and the no-validate row would silently read 0/338.
    anyhow::ensure!(
        sorted_read_dir(&results_dir.join("kiro")).map(|bats| bats.iter().any(|b|
            b.path().join("summary_translated.json").is_file())).unwrap_or(false),
        "no-validate invariant failed: no results/Test-Corpus/kiro/<battery>/summary_translated.json \
         found (the pre-verify scoring pass did not run; \\ActorKiroNoValidate* would be 0/338)."
    );
    // (2) P01/P00 must collapse to one distinct source. If symlinks did not
    //     survive checkout (core.symlinks=false materializes them as real dirs),
    //     every config counts and LOC inflates ~120x silently. Skipped when the corpus
    //     is absent: dedup cannot run and datasets.tex omits these rows anyway.
    if has_c_corpus {
        for (bat, expected_cases) in [("P01_sphincs_plus", 1u32), ("P00_perlin_noise", 1u32)] {
            let bat_dir = test_corpus_dir.join(bat);
            if !bat_dir.is_dir() {
                continue;
            }
            let mut distinct: std::collections::HashSet<std::path::PathBuf> =
                std::collections::HashSet::new();
            let mut case_dirs = 0u32;
            if let Ok(cases) = sorted_read_dir(&bat_dir) {
                for ce in cases {
                    let tc = ce.path().join("test_case");
                    if !tc.is_dir() {
                        continue;
                    }
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

    // (3) Catch a renamed/removed macro here rather than as a LaTeX "Undefined
    //     control sequence".
    let macros_used = repo_root.join("macros_used.txt");
    if macros_used.is_file() {
        let emitted = format!("{numbers}\n{manual}");
        let mut missing = Vec::new();
        for line in std::fs::read_to_string(&macros_used)?.lines() {
            let name = line.trim();
            if name.is_empty() || name.starts_with('#') {
                continue;
            }
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

/// Comma thousands separators, matching the paper's dataset table.
fn fmt_commas(n: u32) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Rounds half away from zero (`f64::round`); 0 for an empty slice.
fn median_round(values: &[u32]) -> u32 {
    if values.is_empty() {
        return 0;
    }
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
    "B01_synthetic",
    "B01_organic",
    "B02_synthetic",
    "B02_organic",
    "P00_perlin_noise",
    "P01_sphincs_plus",
];

/// Cases whose corpus `test_case/` symlinks to one real source are counted once
/// (P01's 128 configs → one source). The single definition used by BOTH
/// tab:datasets and the TRACTOR total/mean prose macros, so they cannot disagree.
fn tractor_battery_c_locs(corpus: &Path, dir_name: &str) -> Vec<u32> {
    let bat_dir = corpus.join(dir_name);
    let mut locs: Vec<u32> = Vec::new();
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    if let Ok(cases) = sorted_read_dir(&bat_dir) {
        for ce in cases {
            if !ce.path().is_dir() {
                continue;
            }
            let test_case = ce.path().join("test_case");
            if !test_case.is_dir() {
                continue;
            }
            let real = std::fs::canonicalize(&test_case).unwrap_or_else(|_| test_case.clone());
            if !seen.insert(real) {
                continue;
            }
            locs.push(count_c_loc_dir(&test_case));
        }
    }
    locs
}

/// tab:datasets body: `Dataset & Cases & Total & Mean & Median & Max` per
/// battery, counting non-comment, non-blank C LOC.
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
        let mean = if cases > 0 {
            (total as f64 / cases as f64).round() as u32
        } else {
            0
        };
        let median = median_round(vec);
        let max = vec.iter().copied().max().unwrap_or(0);
        out.push_str(&format!(
            "{} & {} & {} & {} & {} & {} \\\\\n",
            name,
            cases,
            fmt_commas(total),
            fmt_commas(mean),
            fmt_commas(median),
            fmt_commas(max),
        ));
    };

    for (dir_name, display) in batteries {
        let locs = tractor_battery_c_locs(&corpus, dir_name);
        if locs.is_empty() {
            continue;
        }
        emit_row(&mut out, display, locs.len() as u32, &locs);
    }

    out
}

/// tab:prompt-sensitivity body: base ACTOR run plus four prompt ablations, then
/// the cross-prompt swap block. The swap figures come from
/// `manual_constants.toml [prompt_sensitivity]` because those runs are a one-off
/// TRACTOR-subset experiment, not re-derived here.
fn generate_prompt_sensitivity_tex(repo_root: &Path, rows: &[BatteryRow]) -> String {
    use std::collections::HashMap;
    let mut tractor_pass: HashMap<&str, u32> = HashMap::new();
    let mut battery_size: HashMap<&str, u32> = HashMap::new();
    for r in rows {
        *tractor_pass.entry(r.agent.as_str()).or_insert(0) += r.cases_passed;
        let e = battery_size.entry(r.battery.as_str()).or_insert(0);
        *e = (*e).max(r.cases_tested);
    }
    let tractor_total: u32 = battery_size.values().sum();

    // (paper label, results-dir agent key); base row first.
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

fn prompt_sensitivity_manual(repo_root: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let path = repo_root.join("manual_constants.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return out;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return out;
    };
    if let Some(table) = doc.get("prompt_sensitivity").and_then(|t| t.as_table()) {
        for (k, v) in table.iter() {
            if let Some(s) = v.as_str() {
                out.insert(k.to_string(), s.to_string());
            }
        }
    }
    out
}

/// Macro names are `\<CamelSection><CamelKey>`: section `unsafe_categories` with key
/// `c_abi_preservation` becomes `\UnsafeCategoriesCAbiPreservation`. A missing TOML
/// file emits just the header comment; never fails.
fn generate_manual_tex(repo_root: &Path) -> String {
    let mut out = String::new();
    out.push_str("% MANUALLY ENTERED — not derived from data. Source: manual_constants.toml\n");

    let path = repo_root.join("manual_constants.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return out;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return out;
    };

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
    // As it appears in the file: scalars unquoted.
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
        let Some(table) = item.as_table() else {
            continue;
        };
        let sec_camel = camel(section);
        for (key, val) in table.iter() {
            let Some(v) = val.as_value() else { continue };
            let macro_name = format!("{}{}", sec_camel, camel(key));
            out.push_str(&format!(
                "\\newcommand{{\\{}}}{{{}}}\n",
                macro_name,
                render(v)
            ));
        }
    }

    out
}

/// THE archival resolver, and the only one left: `tables/` is regenerated from the shipped submodule
/// with no run in flight, so the `result.json` the run left is the only record of what was scored.
fn archived_score(case_dir: &Path) -> PathBuf {
    let verified = crate::battery::phase_dir(case_dir, crate::battery::VERIFIED);
    if verified.join("result.json").is_file() {
        verified
    } else {
        crate::battery::phase_dir(case_dir, crate::battery::TRANSLATED)
    }
}

/// Keyed `"battery/case"`; the value is the runner's own build result.
fn case_builds(repo_root: &Path, agent: &str) -> std::collections::BTreeMap<String, bool> {
    let mut out = std::collections::BTreeMap::new();
    let agent_dir = repo_root.join("results/Test-Corpus").join(agent);
    let Ok(bats) = sorted_read_dir(&agent_dir) else {
        return out;
    };
    for be in bats {
        if !be.path().is_dir() {
            continue;
        }
        let battery = be.file_name().to_string_lossy().to_string();
        let Ok(cases) = sorted_read_dir(&be.path()) else {
            continue;
        };
        for ce in cases {
            if !ce.path().is_dir() {
                continue;
            }
            let case = ce.file_name().to_string_lossy().to_string();
            let rp = archived_score(&ce.path()).join("result.json");
            let Some(r) = read_json::<CaseResult>(&rp) else {
                continue;
            };
            out.insert(
                format!("{battery}/{case}"),
                r.error.as_deref() != Some("build failed"),
            );
        }
    }
    out
}

/// The paper's Laertes claim is the *direction* of these two counts, so they are named:
/// as a `(u32, u32)` the two are interchangeable at every call site and swapping them
/// reverses the claim without changing a type.
struct LaertesDelta {
    /// Compilations Laertes breaks that C2Rust had working.
    broke: u32,
    /// Ones it fixes that C2Rust could not build.
    fixed: u32,
}

fn laertes_vs_c2rust(repo_root: &Path) -> LaertesDelta {
    let c2 = case_builds(repo_root, "c2rust");
    let la = case_builds(repo_root, "laertes");
    let mut d = LaertesDelta { broke: 0, fixed: 0 };
    for (case, c2_ok) in &c2 {
        if let Some(&la_ok) = la.get(case) {
            if *c2_ok && !la_ok {
                d.broke += 1;
            }
            if !*c2_ok && la_ok {
                d.fixed += 1;
            }
        }
    }
    d
}

struct KiroCost {
    credits: Credits,
    /// The verify phase's share OF `credits`, not an addition to it — the published
    /// validate-percentage is the ratio of the two.
    verify_credits: Credits,
    wall_secs: u64,
}

/// Reads exactly ONE result.json per case, the canonical phase dir. Each case
/// may have BOTH translated/result.json and verified/result.json, and the
/// verified one already carries the full translate+verify credit breakdown, so
/// reading both would double-count the translate phase. Shared-source
/// duplicates carry no credits, so they do not inflate the total.
fn kiro_cost(base: &Path) -> KiroCost {
    let zero = || KiroCost {
        credits: Credits::new(0.0),
        verify_credits: Credits::new(0.0),
        wall_secs: 0,
    };
    let (mut total, mut verify, mut secs) = (0.0f64, 0.0f64, 0u64);
    let Ok(rd) = std::fs::read_dir(base) else {
        return zero();
    };
    let mut stack: Vec<std::path::PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    while let Some(p) = stack.pop() {
        if !p.is_dir() {
            continue;
        }
        // A case dir is one with a translated/ phase. Do NOT also descend into
        // its phase dirs — each carries a result.json and would double-count.
        if crate::battery::phase_dir(&p, crate::battery::TRANSLATED).is_dir() {
            let rp = archived_score(&p).join("result.json");
            if let Some(r) = read_json::<serde_json::Value>(&rp) {
                for ph in ["translate", "verify"] {
                    let c = r
                        .pointer(&format!("/{ph}/credits"))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let s = r
                        .pointer(&format!("/{ph}/wall_secs"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    total += c;
                    secs += s;
                    if ph == "verify" {
                        verify += c;
                    }
                }
            }
        } else if let Ok(rd) = std::fs::read_dir(&p) {
            stack.extend(rd.filter_map(|e| e.ok().map(|e| e.path())));
        }
    }
    KiroCost {
        credits: Credits::new(total),
        verify_credits: Credits::new(verify),
        wall_secs: secs,
    }
}

/// Named constants for result numbers quoted in the prose, so text and tables
/// cannot disagree. Only numbers derived directly from the committed results
/// appear here; anything the data does not reproduce is left to the paper text.
fn generate_numbers_tex(rows: &[BatteryRow], repo_root: &Path) -> String {
    use std::collections::HashMap;
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
    o.push_str(&format!(
        "\\newcommand{{\\TractorTotalCases}}{{{}}}\n",
        total_cases
    ));
    // Same deduped counting as tab:datasets, so prose and table agree.
    let corpus = repo_root.join("test-corpus/Public-Tests");
    let tractor_total_loc: u32 = TRACTOR_BATTERY_DIRS
        .iter()
        .flat_map(|d| tractor_battery_c_locs(&corpus, d))
        .sum();
    if tractor_total_loc > 0 && total_cases > 0 {
        let mean = (tractor_total_loc as f64 / total_cases as f64).round() as u32;
        o.push_str(&format!(
            "\\newcommand{{\\TractorTotalLoc}}{{{}}}\n",
            fmt_commas(tractor_total_loc)
        ));
        o.push_str(&format!("\\newcommand{{\\TractorMeanLoc}}{{{}}}\n", mean));
    }
    // Claude and Codex report no credits, so their costs stay manual in the prose.
    // Deduped per-battery Rust LOC, matching what the tables report.
    let tractor_rust_loc: u32 = rows
        .iter()
        .filter(|r| r.agent == "kiro")
        .map(|r| r.total_loc)
        .sum();
    // (label, results dir, translated-Rust-kLOC denominator)
    let cost_rows: &[(&str, &str, u32)] =
        &[("Tractor", "results/Test-Corpus/kiro", tractor_rust_loc)];
    for (name, base, rust_loc) in cost_rows {
        let cost = kiro_cost(&repo_root.join(base));
        let credits = cost.credits.as_f64();
        if credits <= 0.0 {
            continue;
        }
        let dollars = cost.credits.to_usd().as_f64();
        let minutes = cost.wall_secs as f64 / 60.0;
        let kloc = *rust_loc as f64 / 1000.0;
        o.push_str(&format!("\\newcommand{{\\Cost{name}}}{{{:.0}}}\n", dollars));
        o.push_str(&format!(
            "\\newcommand{{\\Cost{name}Minutes}}{{{:.0}}}\n",
            minutes
        ));
        if kloc > 0.0 {
            o.push_str(&format!(
                "\\newcommand{{\\Cost{name}PerKLoc}}{{{:.2}}}\n",
                dollars / kloc
            ));
            o.push_str(&format!(
                "\\newcommand{{\\Cost{name}MinPerKLoc}}{{{:.0}}}\n",
                minutes / kloc
            ));
        }
        o.push_str(&format!(
            "\\newcommand{{\\Cost{name}ValidatePct}}{{{:.0}}}\n",
            cost.verify_credits.as_f64() / credits * 100.0
        ));
    }
    let p01 = kiro_cost(&repo_root.join("results/Test-Corpus/kiro/P01_sphincs_plus"));
    if p01.credits.as_f64() > 0.0 {
        o.push_str(&format!(
            "\\newcommand{{\\CostPOne}}{{{:.2}}}\n",
            p01.credits.to_usd().as_f64()
        ));
        o.push_str(&format!(
            "\\newcommand{{\\CostPOneMinutes}}{{{:.0}}}\n",
            p01.wall_secs as f64 / 60.0
        ));
    }

    o.push_str(&format!(
        "\\newcommand{{\\ActorKiroTests}}{{{}/{}}}\n",
        g(&total_tests, "kiro"),
        total_cases
    ));
    o.push_str(&format!(
        "\\newcommand{{\\ActorClaudeTests}}{{{}/{}}}\n",
        g(&total_tests, "claude"),
        total_cases
    ));
    o.push_str(&format!(
        "\\newcommand{{\\ActorCodexTests}}{{{}/{}}}\n",
        g(&total_tests, "codex-gpt54"),
        total_cases
    ));
    // The paper says "no validate" where the agent dir says `kiro-translate`.
    // The macro name MUST match paper.tex, else it is an undefined control
    // sequence.
    o.push_str(&format!(
        "\\newcommand{{\\ActorKiroNoValidateTests}}{{{}/{}}}\n",
        g(&total_tests, "kiro-translate"),
        total_cases
    ));
    o.push_str(&format!(
        "\\newcommand{{\\ActorKiroPOneTests}}{{{}/128}}\n",
        g(&p01_tests, "kiro")
    ));
    o.push_str(&format!(
        "\\newcommand{{\\ActorKiroNoValidatePOneTests}}{{{}/128}}\n",
        g(&p01_tests, "kiro-translate")
    ));
    let laertes = laertes_vs_c2rust(repo_root);
    o.push_str(&format!(
        "\\newcommand{{\\LaertesBreaks}}{{{}}}\n",
        laertes.broke
    ));
    o.push_str(&format!(
        "\\newcommand{{\\LaertesFixes}}{{{}}}\n",
        laertes.fixed
    ));
    o
}

/// In paper order: ACTOR harness rows first, then transpiler/LLM baselines.
const TRACTOR_TABLE_ROWS: &[(&str, &str, bool)] = &[
    // (display label, results dir, is_actor_harness)
    ("ACTOR (Kiro)", "kiro", true),
    ("ACTOR (Claude Code)", "claude", true),
    ("ACTOR (Codex)", "codex-gpt54", true),
    (
        "\\makebox[\\knvLength][l]{ACTOR (Kiro, no validate)}",
        "kiro-translate",
        true,
    ),
    ("C2Rust", "c2rust", false),
    ("Laertes", "laertes", false),
    ("C2SaferRust", "c2saferrust", false),
    ("SmartC2Rust", "smartc2rust", false),
    ("Kimi K2.5 (query)", "kimi", false),
    ("GPT-5.4 (query)", "gpt-5.4", false),
    ("Gemini 3.1 Pro (query)", "gemini-3.1-pro-preview", false),
];

/// In paper order. Each entry is (label on the first agent row, label on the
/// second, dir): P00/P01 split their name across two rows to match the paper's
/// layout, the others put the whole label on line 1.
const TRACTOR_BATTERIES: &[(&str, &str, &str)] = &[
    ("B01-syn", "", "B01_synthetic"),
    ("B01-org", "", "B01_organic"),
    ("B02-syn", "", "B02_synthetic"),
    ("B02-org", "", "B02_organic"),
    ("P00", "(Perlin)", "P00_perlin_noise"),
    (
        "P01",
        "\\makebox[1cm][l]{\\smaller (SPHINCS+)}",
        "P01_sphincs_plus",
    ),
    ("Total", "", ""), // empty battery dir => aggregate across all
];

/// Matches the paper table's LOC formatting.
fn fmt_k(loc: u32) -> String {
    if loc < 1000 {
        loc.to_string()
    } else if loc < 2000 {
        format!("{:.1}k", loc as f64 / 1000.0)
    } else {
        format!("{}k", (loc as f64 / 1000.0).round() as u32)
    }
}

/// tab:tractor body rows only; the table/tabular/caption stays in paper.tex.
fn generate_tractor_tex(rows: &[BatteryRow]) -> String {
    use std::collections::HashMap;
    // (agent dir, battery dir) -> row
    let mut idx: HashMap<(&str, &str), &BatteryRow> = HashMap::new();
    for r in rows {
        idx.insert((r.agent.as_str(), r.battery.as_str()), r);
    }

    // Largest cases_tested any agent recorded = the full battery size, since the
    // ACTOR harnesses ran every case. A baseline's partial run still divides by
    // it, so a case it never attempted counts as a failure (paper methodology).
    let mut battery_size: HashMap<&str, u32> = HashMap::new();
    for r in rows {
        let e = battery_size.entry(r.battery.as_str()).or_insert(0);
        *e = (*e).max(r.cases_tested);
    }
    // Derived, not hardcoded, so it stays correct if the corpus changes.
    let total_cases: u32 = TRACTOR_BATTERIES
        .iter()
        .filter(|&(_, _, d)| !d.is_empty())
        .map(|(_, _, d)| battery_size.get(d).copied().unwrap_or(0))
        .sum();

    struct Cell {
        built: u32,
        denom: u32,
        tests_pass: u32,
        loc: u32,
        unsafe_lines: u32,
        present: bool,
    }
    let cell = |agent: &str, bat_dir: &str| -> Cell {
        if bat_dir.is_empty() {
            let (mut b, mut tp, mut loc, mut un) = (0u32, 0u32, 0u32, 0u32);
            let mut present = false;
            for (_, _, bd) in TRACTOR_BATTERIES.iter().filter(|(_, _, d)| !d.is_empty()) {
                if let Some(r) = idx.get(&(agent, *bd)) {
                    present = true;
                    b += r.cases_built;
                    tp += r.cases_passed;
                    loc += r.total_loc;
                    un += r.unsafe_lines;
                }
            }
            Cell {
                built: b,
                denom: total_cases,
                tests_pass: tp,
                loc,
                unsafe_lines: un,
                present,
            }
        } else {
            let denom = battery_size.get(bat_dir).copied().unwrap_or(0);
            if let Some(r) = idx.get(&(agent, bat_dir)) {
                Cell {
                    built: r.cases_built,
                    denom,
                    tests_pass: r.cases_passed,
                    loc: r.total_loc,
                    unsafe_lines: r.unsafe_lines,
                    present: true,
                }
            } else {
                Cell {
                    built: 0,
                    denom,
                    tests_pass: 0,
                    loc: 0,
                    unsafe_lines: 0,
                    present: false,
                }
            }
        }
    };

    let mut out = String::new();
    out.push_str(
        "% GENERATED by `harvest-tools report` from results/Test-Corpus/. Do not edit by hand.\n",
    );
    out.push_str("% Builds = crate compiles (result.json error != \"build failed\").\n");
    out.push_str("% Tests = cases passing all vectors. Every system is scored over all cases.\n");

    for (bi, (bat_line1, bat_line2, bat_dir)) in TRACTOR_BATTERIES.iter().enumerate() {
        // Bold goes to the best Tests among ACTOR harness rows.
        let best = TRACTOR_TABLE_ROWS
            .iter()
            .filter(|(_, _, actor)| *actor)
            .map(|(_, a, _)| cell(a, bat_dir).tests_pass)
            .max()
            .unwrap_or(0);
        for (ri, (label, agent, _)) in TRACTOR_TABLE_ROWS.iter().enumerate() {
            let c = cell(agent, bat_dir);
            let denom = c.denom;
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
            // "--" only when the system produced no output at all for this
            // battery (the paper's dashes for e.g. SmartC2Rust P01).
            let (loc, un) = if c.present && c.loc > 0 {
                (
                    fmt_k(c.loc),
                    format!(
                        "{}\\%",
                        (c.unsafe_lines as f64 / c.loc as f64 * 100.0).round() as u32
                    ),
                )
            } else {
                ("--".into(), "--".into())
            };
            out.push_str(&format!(
                "{} & {} & {}/{} & {} & {} & {} \\\\\n",
                first_col, label, c.built, denom, tests, loc, un
            ));
        }
        // hline between batteries; double before Total.
        if bi + 2 == TRACTOR_BATTERIES.len() {
            out.push_str("\\hline\\hline\n");
        } else if bi + 1 < TRACTOR_BATTERIES.len() {
            out.push_str("\\hline\n");
        }
    }
    out
}

/// Three same-width counts feeding three different published columns — one of them the
/// unsafe percentage, which is `unsafe_lines / total_loc`. As a `(u32, u32, u32)` any
/// permutation type-checks at every call site and lands in the table relabelled.
struct CaseTotals {
    total_loc: u32,
    unsafe_lines: u32,
    cases_built: u32,
}

/// `corpus_bat_dir` is the matching `test-corpus/Public-Tests/<battery>`, needed to dedup
/// shared-source LOC.
///
/// Two quantities, two rules. `built` counts EVERY config (P01 = 128/128 — all
/// are compiled and run despite sharing one translation), whereas `loc`/`unsafe`
/// describe the translated code, which for a shared-source group exists ONCE:
/// every config's result.json repeats the same LOC, so summing multiplies it.
/// Followers are identified by a symlinked corpus `test_case/` — authoritative
/// grouping, not a LOC-ratio guess. Without the corpus that grouping is
/// unresolvable, so LOC/unsafe sum every case (correct for the all-independent
/// batteries; only P00/P01 need dedup and they require the corpus anyway).
fn aggregate_cases(bat_dir: &Path, corpus_bat_dir: &Path) -> CaseTotals {
    aggregate_cases_phase(bat_dir, corpus_bat_dir, None)
}

/// `phase`: `Some("translated")` for the pre-verify (no-validate) numbers, `None` for [`archived_score`].
fn aggregate_cases_phase(bat_dir: &Path, corpus_bat_dir: &Path, phase: Option<&str>) -> CaseTotals {
    let corpus_present = corpus_bat_dir.is_dir();
    let (mut total_loc, mut total_unsafe, mut built) = (0u32, 0u32, 0u32);
    let Ok(entries) = std::fs::read_dir(bat_dir) else {
        return CaseTotals {
            total_loc: 0,
            unsafe_lines: 0,
            cases_built: 0,
        };
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let phase_dir = match phase {
            Some(p) => crate::battery::phase_dir(&entry.path(), p),
            None => archived_score(&entry.path()),
        };
        let result_path = phase_dir.join("result.json");
        let cr: CaseResult = match read_json(&result_path) {
            Some(r) => r,
            None => continue,
        };
        if cr.error.as_deref() != Some("build failed") {
            built += 1;
        }
        // Skip shared-source followers so one translation is counted once.
        if corpus_present {
            let tc = corpus_bat_dir.join(entry.file_name()).join("test_case");
            if tc.is_symlink() {
                continue;
            }
        }
        total_loc += cr.loc.map_or(0, |l| l.code);
        total_unsafe += cr.unsafe_.map_or(0, |u| u.lines);
    }
    CaseTotals {
        total_loc,
        unsafe_lines: total_unsafe,
        cases_built: built,
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

/// Non-blank, non-comment `*.c`/`*.h` lines. Shared-source cases symlink their
/// `test_case/` to one real source; each DISTINCT resolved source is counted
/// once, so P01's 128 configs contribute their one source a single time.
fn count_c_loc_battery(test_corpus_dir: &Path, battery: &str) -> u32 {
    let bat_dir = test_corpus_dir.join(battery);
    let Ok(entries) = std::fs::read_dir(&bat_dir) else {
        return 0;
    };
    let mut total = 0u32;
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let test_case = entry.path().join("test_case");
        if !test_case.is_dir() {
            continue;
        }
        let real = std::fs::canonicalize(&test_case).unwrap_or_else(|_| test_case.clone());
        if !seen.insert(real) {
            continue;
        }
        total += count_c_loc_dir(&test_case);
    }
    total
}

fn count_c_loc_dir(dir: &Path) -> u32 {
    let mut total = 0u32;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += count_c_loc_dir(&path);
        } else if path.extension().is_some_and(|x| x == "c" || x == "h") {
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            total += src
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with("//") && !t.starts_with("/*")
                })
                .count() as u32;
        }
    }
    total
}
