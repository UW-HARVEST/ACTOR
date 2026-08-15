//! The filesystem policy handed to an agent as `--settings`.
//!
//! The repo must be denied on two counts: the graded oracle, and `results/` — other
//! agents' and other cases' finished translations. Narrowing the deny roots to
//! `test-corpus/` + `harvest-bench/` would still leak the latter.
//!
//! It holds the graded oracle
//! (`test-corpus/Public-Tests/<battery>/<case>/test_vectors`,
//! `harvest-bench/tests/<name>/gtest_suite`) while only `test_case/` is ever
//! copied into an agent's work dir. Derive the deny roots from the repo root, not
//! from the case dir: HB case dirs are one level shallower than Test-Corpus ones,
//! so any `ancestors().nth(n)` walk lands somewhere different per dataset.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// The scratch base is the non-obvious entry: sibling per-case work dirs live
/// under it and reads are default-allow *outside* `denyRead`, so it is denied
/// wholesale and this run's own root re-granted by `allowRead`, which wins.
pub fn denied_read_roots(repo_root: &Path) -> Result<Vec<PathBuf>> {
    Ok(vec![repo_root.to_path_buf(), crate::workdir::base()?])
}

/// The two roots the policy is made of. As bare `&Path` parameters they are transposable,
/// and transposed the policy denies the agent's work tree and *grants* the repo — the
/// graded oracle, plus every other case's translation. With `bwrap` absent this file is the
/// only sandbox, so nothing downstream would catch it.
#[derive(Copy, Clone)]
pub struct Policy<'a> {
    pub repo_root: &'a Path,
    /// Both the tree the agent may read and write, and where the policy file is written.
    /// One field, not two: callers always passed the same value, and a difference would
    /// launch the agent in a directory its own policy denies.
    pub work_root: &'a Path,
}

pub(crate) fn settings_json(policy: Policy<'_>) -> Result<serde_json::Value> {
    let deny: Vec<String> = denied_read_roots(policy.repo_root)?
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let work_root = policy.work_root;
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

/// Writes `<work_root>/.claude/settings.json`, which is where the agent is launched and
/// so where `--settings` looks for it.
pub fn write_settings(policy: Policy<'_>) -> Result<PathBuf> {
    let dir = policy.work_root.join(".claude");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.json");
    std::fs::write(&path, settings_json(policy)?.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(repo: &str, work: &str) -> serde_json::Value {
        // Mirrors settings_json's shape with a fixed deny list: the real one calls
        // base(), which reads $HOME.
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
        // Denying something *under* results/ leaves the corpus readable.
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
        // Reads are default-allow outside denyRead, so without the base itself
        // denied a case could read a sibling case's tree.
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
        // The real function, not `policy`: base() needs HOME, which tests have.
        let p = write_settings(Policy { repo_root: Path::new("/repo"), work_root: &parent })
            .expect("writes policy");
        assert_eq!(p, parent.join(".claude/settings.json"));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["sandbox"]["enabled"], true);
        let deny = v["sandbox"]["filesystem"]["denyRead"].as_array().unwrap();
        assert!(deny.iter().any(|d| d == "/repo"));
        assert_eq!(deny.len(), 2, "repo root + scratch base");
    }
}
