use std::path::PathBuf;

/// Find the repo root by walking up from the test binary's location.
fn repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().unwrap();
    loop {
        if dir.join("test-corpus").is_dir() && dir.join("results").is_dir() {
            return dir;
        }
        if !dir.pop() {
            panic!("Could not find repo root");
        }
    }
}

mod battery_discovery {
    use super::*;
    use harvest_tools::battery::{self, Case};

    fn corpus_dir() -> PathBuf {
        repo_root().join("test-corpus")
    }

    /// B02_synthetic has 40 independent + 3 macrodepth (1 real + 2 symlinked).
    /// This is the exact scenario that triggered the verify bug.
    #[test]
    fn b02_synthetic_mixed_battery() {
        let battery = battery::discover(&corpus_dir(), "B02_synthetic", None).unwrap();

        let mut independent = Vec::new();
        let mut shared = Vec::new();

        for case in &battery.cases {
            match case {
                Case::Independent(c) => independent.push(c.name.clone()),
                Case::SharedSource(g) => {
                    shared.push(g.real_case.clone());
                    for cfg in &g.configs {
                        shared.push(cfg.name.clone());
                    }
                }
            }
        }

        // macrodepth_add_5 is real, mul_4 and sub_6 are symlinked
        assert!(
            shared.contains(&"macrodepth_add_5".to_string()),
            "real case missing"
        );
        assert!(
            shared.contains(&"macrodepth_mul_4".to_string()),
            "symlinked config missing"
        );
        assert!(
            shared.contains(&"macrodepth_sub_6".to_string()),
            "symlinked config missing"
        );

        // All other cases must be independent — NOT in shared
        assert!(
            independent.contains(&"strcmp".to_string()),
            "strcmp should be independent"
        );
        assert!(
            independent.contains(&"arity_lib".to_string()),
            "arity_lib should be independent"
        );
        assert!(
            !independent.contains(&"macrodepth_add_5".to_string()),
            "real case should not be independent"
        );

        // Total should match
        assert_eq!(
            independent.len() + shared.len(),
            battery::all_case_names(&battery).len()
        );
    }

    /// P01_sphincs_plus: ALL cases share source (1 real + 127 symlinked).
    #[test]
    fn p01_all_shared_source() {
        let battery = battery::discover(&corpus_dir(), "P01_sphincs_plus", None).unwrap();

        assert_eq!(
            battery.cases.len(),
            1,
            "should be exactly 1 SharedSource group"
        );
        if let Case::SharedSource(g) = &battery.cases[0] {
            assert_eq!(g.configs.len(), 127, "should have 127 symlinked configs");
            // Total: 1 real + 127 configs = 128
            assert_eq!(battery::all_case_names(&battery).len(), 128);
        } else {
            panic!("P01 should be SharedSource");
        }
    }

    /// B01_organic: all independent, no symlinks.
    #[test]
    fn b01_organic_all_independent() {
        let battery = battery::discover(&corpus_dir(), "B01_organic", None).unwrap();

        for case in &battery.cases {
            assert!(
                matches!(case, Case::Independent(_)),
                "B01_organic should have no shared source"
            );
        }
        assert_eq!(battery::all_case_names(&battery).len(), 38);
    }

    /// Filter works on real battery.
    #[test]
    fn filter_on_real_battery() {
        let battery = battery::discover(&corpus_dir(), "B02_synthetic", Some("_lib$")).unwrap();
        for case in &battery.cases {
            match case {
                Case::Independent(c) => {
                    assert!(c.name.ends_with("_lib"), "{} doesn't match filter", c.name)
                }
                Case::SharedSource(_) => {} // shared source configs may or may not match
            }
        }
    }
}

mod cargo_toml_manipulation {
    use super::*;
    use harvest_tools::cargo_toml::{strip_for_lib, CargoToml};

    /// Test post-processing on a real P01 translation's Cargo.toml.
    #[test]
    fn real_p01_cargo_toml_roundtrip() {
        let root = repo_root();
        let cargo_path = root.join("results/P01_sphincs_plus/005_sphincs_PQCgenKAT_sign_blake_128f_simple/translated/Cargo.toml");
        if !cargo_path.exists() {
            eprintln!("Skipping: no P01 translation available");
            return;
        }

        // Read, modify, write to temp, verify
        let tmp = harvest_tools::workdir::test_tempdir().unwrap();
        let tmp_cargo = tmp.path().join("Cargo.toml");
        std::fs::copy(&cargo_path, &tmp_cargo).unwrap();

        let mut cargo = CargoToml::open(&tmp_cargo).unwrap();
        cargo.add_workspace();
        cargo.set_default_features(&["blake".into(), "simple".into(), "128f".into()]);
        cargo.save().unwrap();

        // Re-read and verify
        let cargo2 = CargoToml::open(&tmp_cargo).unwrap();
        let features = cargo2.defined_features();
        assert!(!features.is_empty(), "should have features defined");

        let content = std::fs::read_to_string(&tmp_cargo).unwrap();
        assert!(content.contains("[workspace]"));
        assert!(content.contains("default"));
    }

    /// Verify lib case stripping on a real lib translation.
    #[test]
    fn real_lib_case_strip() {
        let root = repo_root();
        // Find any _lib case with a translation
        let results = root.join("results/B01_organic");
        if !results.exists() {
            eprintln!("Skipping: no B01_organic results");
            return;
        }

        for entry in std::fs::read_dir(&results).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with("_lib") {
                continue;
            }
            // The translated/ phase dir is the immutable pre-verify crate.
            let original = entry.path().join("translated");
            if !original.exists() {
                continue;
            }

            // Copy to temp and strip
            let tmp = harvest_tools::workdir::test_tempdir().unwrap();
            let dst = tmp.path().join("translated");
            harvest_tools::translate::copy_dir_all(&original, &dst).unwrap();

            // Create a fake main.rs and tests/ to verify they get removed
            std::fs::write(dst.join("src/main.rs"), "fn main() {}").unwrap();
            std::fs::create_dir_all(dst.join("tests")).unwrap();
            std::fs::write(dst.join("tests/t.rs"), "").unwrap();

            strip_for_lib(&dst).unwrap();

            assert!(
                !dst.join("src/main.rs").exists(),
                "main.rs should be stripped for {name}"
            );
            assert!(
                !dst.join("tests").exists(),
                "tests/ should be stripped for {name}"
            );
            assert!(
                dst.join("src").exists(),
                "src/ should still exist for {name}"
            );
            return; // one case is enough
        }
        eprintln!("Skipping: no lib case with _original found");
    }
}

mod test_artifacts {
    use super::*;

    /// Verify test_vectors and runner exist in corpus for all cases.
    #[test]
    fn corpus_has_test_artifacts() {
        let root = repo_root();
        let corpus = root.join("test-corpus/Public-Tests/B01_organic");

        for entry in std::fs::read_dir(&corpus).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();

            assert!(
                path.join("test_vectors").is_dir(),
                "{name} missing test_vectors/"
            );
            if name.ends_with("_lib") {
                assert!(path.join("runner").is_dir(), "{name} (lib) missing runner/");
            }
        }
    }

    /// Verify lib name extraction works on real corpus runners.
    #[test]
    fn real_lib_name_extraction() {
        let root = repo_root();
        let corpus = root.join("test-corpus/Public-Tests/P01_sphincs_plus");

        let mut found = 0;
        for entry in std::fs::read_dir(&corpus).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with("_lib") {
                continue;
            }

            let runner_main = entry.path().join("runner/src/main.rs");
            if !runner_main.exists() {
                continue;
            }

            let lib_name = harvest_tools::battery::extract_lib_name(&corpus, &name);
            if lib_name.is_some() {
                found += 1;
            }
        }
        assert!(
            found > 0,
            "should have found at least one lib case with library: pattern"
        );
    }

    /// Verify CMakePresets.json feature extraction on real P01 cases.
    #[test]
    fn real_p01_feature_extraction() {
        let root = repo_root();
        let corpus = root.join("test-corpus/Public-Tests/P01_sphincs_plus");

        // Check a known case
        let presets = corpus.join("006_sphincs_PQCgenKAT_sign_blake_128f_robust/CMakePresets.json");
        if !presets.exists() {
            eprintln!("Skipping: no CMakePresets.json");
            return;
        }

        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&presets).unwrap()).unwrap();

        let cv = data.pointer("/configurePresets/1/cacheVariables").unwrap();
        assert!(cv.get("HASH_BACKEND").is_some(), "should have HASH_BACKEND");
        assert!(cv.get("THASH").is_some(), "should have THASH");
        assert!(cv.get("SECPAR").is_some(), "should have SECPAR");
    }
}

mod artifact_fingerprint {
    use super::*;
    use harvest_tools::artifact::{Sealed, Translate};
    use harvest_tools::battery::{has_crate, phase_dir, TRANSLATED};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    const PINNED: usize = 40;
    const RESULTS_ENV: &str = "HARVEST_GOLDEN_RESULTS";

    #[derive(serde::Deserialize)]
    struct Golden {
        considered: usize,
        digests: BTreeMap<String, String>,
    }

    /// Case directories holding a translated crate, relative to the tree. `is_dir()` only
    /// bounds the descent — off the crate trees — while `has_crate` alone decides membership.
    fn translated_cases(results: &Path) -> std::io::Result<Vec<String>> {
        let mut out = Vec::new();
        let mut dirs = vec![results.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                let hidden = path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'));
                if hidden || !path.is_dir() {
                    continue;
                }
                let translated = phase_dir(&path, TRANSLATED);
                if !translated.is_dir() {
                    dirs.push(path);
                } else if has_crate(&translated) {
                    let rel = path.strip_prefix(results).expect("under results/");
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Decided without the walk: an unrecognised layout must not read as an absent tree.
    fn holds_cases(results: &Path) -> bool {
        results.is_dir()
            && std::fs::read_dir(results)
                .unwrap_or_else(|e| panic!("{}: {e}", results.display()))
                .next()
                .is_some()
    }

    /// A git worktree does not inherit submodules, and the reorganisation PRs run in
    /// worktrees, so `results/` alone would have made this gate skip in every place it is
    /// meant to fire. Naming a tree explicitly is a claim there is one: an empty path there
    /// is refused rather than skipped.
    fn results_tree() -> Option<PathBuf> {
        match std::env::var_os(RESULTS_ENV) {
            Some(named) => {
                let named = PathBuf::from(named);
                assert!(
                    holds_cases(&named),
                    "{RESULTS_ENV} names {}, which holds no cases to compare",
                    named.display()
                );
                Some(named)
            }
            None => {
                let results = repo_root().join("results");
                holds_cases(&results).then_some(results)
            }
        }
    }

    /// The plan calls every reorganisation ahead a pure move; this is what "pure" is measured
    /// against. `Sealed::adopt` re-hashes exactly the files `publish` wrote, by the same rules.
    #[test]
    fn a_published_translation_still_digests_to_what_it_did_before() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden-digests.json");
        let text = std::fs::read_to_string(&fixture)
            .unwrap_or_else(|e| panic!("{}: {e}", fixture.display()));
        let golden: Golden = serde_json::from_str(&text).expect("golden-digests.json parses");
        assert!(
            !golden.digests.is_empty(),
            "golden-digests.json pins nothing, so this test would compare nothing"
        );

        let Some(results) = results_tree() else {
            eprintln!(
                "NO SIGNAL: nothing was compared and this gate proved nothing. {RESULTS_ENV} is \
                 unset and the results/ submodule is not checked out — a git worktree never \
                 inherits one. Point {RESULTS_ENV} at a results tree to run it."
            );
            return;
        };
        let cases = translated_cases(&results)
            .unwrap_or_else(|e| panic!("walking {}: {e}", results.display()));

        let mut compared = 0;
        let mut wrong: Vec<String> = Vec::new();
        for case in cases.iter().take(PINNED) {
            let Some(want) = golden.digests.get(case) else {
                wrong.push(format!(
                    "{case}: found in the results tree, pinned by nothing"
                ));
                continue;
            };
            let sealed = Sealed::<Translate>::adopt(&results.join(case))
                .unwrap_or_else(|e| panic!("adopting {case}: {e}"));
            compared += 1;
            let got = sealed.digest().as_str();
            if got != want {
                wrong.push(format!("{case}: {got} != {want}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "published translations no longer digest to what they did: {wrong:#?}\n\
             Nothing in the reorganisation is allowed to move these. If a digest changed on\n\
             purpose, name the rule that changed and re-record deliberately."
        );
        assert!(
            compared >= PINNED,
            "compared {compared} of the {PINNED} pinned cases against the {} found in {}\n\
             ({} were considered when the fixture was written), so this passed without\n\
             inspecting what it exists to inspect.",
            cases.len(),
            results.display(),
            golden.considered
        );
    }
}

// The prompt-layout guard that lived here listed the claude prompts by hand and only
// covered that one directory. `translate::tests::every_prompt_the_matrix_names_is_on_disk`
// derives the same check from `prompt_file_for` for every agent, and runs in the
// type-safety job — which this file, needing submodules, is excluded from.
