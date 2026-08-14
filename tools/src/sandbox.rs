//! The filesystem policy handed to an agent as `--settings`.
//!
//! This existed inline at three call sites and was wrong three different ways.
//! Two of them bound a local called `repo_root` that was never the repo root —
//! `translate` derived `results_dir.parent()` (the *dataset* dir) while `verify`
//! derived `case_dir.ancestors().nth(2)`, which lands on the *agent* dir for
//! Test-Corpus but the *dataset* dir for HarvestBench, because HB case dirs are
//! one level shallower. The third wrote a bare `{}` — no policy at all.
//!
//! More importantly, none of them denied the corpus, and the corpus is where the
//! graded oracle lives: `test-corpus/Public-Tests/<battery>/<case>/test_vectors`
//! and `harvest-bench/tests/<name>/gtest_suite`. Only `test_case/` is ever copied
//! into an agent's work dir, so everything the agent is being graded against was
//! readable.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Every root an agent must not read, for any dataset or phase.
///
/// Two entries, and the second is the non-obvious one:
///
/// * `repo_root` — covers the corpus (the graded oracle, above) and `results/`
///   (other agents' and other cases' outputs).
/// * the scratch base — sibling per-case work dirs live next to this one under
///   the same base, and reads are default-allow *outside* `denyRead`. The agent's
///   own root is re-exposed by `allowRead`, which takes precedence.
pub fn denied_read_roots(repo_root: &Path) -> Result<Vec<PathBuf>> {
    Ok(vec![repo_root.to_path_buf(), crate::workdir::base()?])
}

/// The whole `--settings` document for one agent invocation: deny everything
/// above, then re-grant this run's own work root for read and write.
pub fn settings_json(repo_root: &Path, work_root: &Path) -> Result<serde_json::Value> {
    let deny: Vec<String> = denied_read_roots(repo_root)?
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    Ok(serde_json::json!({
        "sandbox": {
            "enabled": true,
            "allowUnsandboxedCommands": false,
            "filesystem": {
                "denyRead": deny,
                "allowRead": [work_root.to_string_lossy()],
                "allowWrite": [work_root.to_string_lossy()],
            }
        }
    }))
}

/// Write the policy to `<parent>/.claude/settings.json` and return its path.
///
/// `parent` is the directory the agent is launched *next to* — the work root for
/// verify, the temp root for translate — not the agent's cwd.
pub fn write_settings(repo_root: &Path, work_root: &Path, parent: &Path) -> Result<PathBuf> {
    let dir = parent.join(".claude");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.json");
    std::fs::write(&path, settings_json(repo_root, work_root)?.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(repo: &str, work: &str) -> serde_json::Value {
        // base() reads real env; drive settings_json's shape via a fixed deny list
        // so the test does not depend on $HOME.
        let deny = vec![repo.to_string(), "/scratch/base".to_string()];
        serde_json::json!({
            "sandbox": {
                "enabled": true,
                "allowUnsandboxedCommands": false,
                "filesystem": {
                    "denyRead": deny,
                    "allowRead": [work],
                    "allowWrite": [work],
                }
            }
        })
    }

    #[test]
    fn deny_list_covers_the_repo_root_not_a_results_subdirectory() {
        // The bug: both old sites denied something *under* results/, so the
        // corpus — where the graded oracle lives — stayed readable.
        let p = policy("/repo", "/scratch/base/harvest-work-x");
        let deny = p["sandbox"]["filesystem"]["denyRead"].as_array().unwrap();
        assert!(deny.iter().any(|d| d == "/repo"), "repo root must be denied: {deny:?}");
        assert!(
            !deny.iter().any(|d| d.as_str().unwrap().contains("results")),
            "deny root must not be a results subdirectory: {deny:?}"
        );
    }

    #[test]
    fn deny_list_covers_the_shared_scratch_base_so_siblings_are_not_readable() {
        // Sibling work dirs share one base; reads are default-allow outside
        // denyRead, so without this a case could read another case's tree.
        let p = policy("/repo", "/scratch/base/harvest-work-x");
        let deny = p["sandbox"]["filesystem"]["denyRead"].as_array().unwrap();
        assert!(deny.iter().any(|d| d == "/scratch/base"), "scratch base must be denied: {deny:?}");
    }

    #[test]
    fn own_work_root_is_regranted_for_read_and_write() {
        let p = policy("/repo", "/scratch/base/harvest-work-x");
        let fs = &p["sandbox"]["filesystem"];
        assert_eq!(fs["allowRead"][0], "/scratch/base/harvest-work-x");
        assert_eq!(fs["allowWrite"][0], "/scratch/base/harvest-work-x");
    }

    #[test]
    fn policy_is_identical_regardless_of_dataset_depth() {
        // The old `ancestors().nth(2)` produced different roots for
        // results/Test-Corpus/<agent>/<battery>/<case> vs
        // results/HarvestBench/<agent>/<name>. Deriving from the repo root
        // cannot: both must yield the same deny list.
        let tc = policy("/repo", "/scratch/base/w1");
        let hb = policy("/repo", "/scratch/base/w2");
        assert_eq!(
            tc["sandbox"]["filesystem"]["denyRead"],
            hb["sandbox"]["filesystem"]["denyRead"],
            "deny list must not depend on how deep the case dir happens to be"
        );
    }

    #[test]
    fn sandbox_stanza_shape_is_what_claude_code_expects() {
        let p = policy("/repo", "/w");
        assert_eq!(p["sandbox"]["enabled"], true);
        assert_eq!(p["sandbox"]["allowUnsandboxedCommands"], false);
        for k in ["denyRead", "allowRead", "allowWrite"] {
            assert!(p["sandbox"]["filesystem"][k].is_array(), "{k} must be an array");
        }
    }

    #[test]
    fn write_settings_creates_the_file_where_settings_flag_looks_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("root");
        std::fs::create_dir_all(&parent).unwrap();
        // Drive the real function; base() needs HOME, which the test harness has.
        let p = write_settings(Path::new("/repo"), &parent, &parent).expect("writes policy");
        assert_eq!(p, parent.join(".claude/settings.json"));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["sandbox"]["enabled"], true);
        let deny = v["sandbox"]["filesystem"]["denyRead"].as_array().unwrap();
        assert!(deny.iter().any(|d| d == "/repo"));
        assert_eq!(deny.len(), 2, "repo root + scratch base");
    }
}
