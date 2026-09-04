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

/// Binaries Claude Code needs before it will sandbox anything on Linux.
///
/// Without them it prints a fail-open banner and runs UNSANDBOXED. Measured before
/// they were installed: the banner appears in 1899 of 1902 Claude-arm agent logs, so
/// every one of those runs had the policy written and none had it applied.
const SANDBOX_BINARIES: &[&str] = &["bwrap", "socat"];

/// Refuse to hand an agent a policy that cannot be applied.
///
/// The deny list was correct and inert for 164 runs. `enabled: true` is a request;
/// enforcement needs `bwrap`, whose mechanism is a tmpfs overmount inside a mount
/// namespace. Harness and agent share a uid, so no `chmod` scheme substitutes.
pub fn require_enforceable() -> Result<()> {
    let missing: Vec<&str> = SANDBOX_BINARIES
        .iter()
        .copied()
        .filter(|b| which(b).is_none())
        .collect();
    anyhow::ensure!(
        missing.is_empty(),
        "the agent sandbox cannot be enforced: {} not on PATH.\n           Claude Code would print a fail-open banner and run unsandboxed, so the graded \
         oracle and every sibling work dir would be readable.\n           Install them (Amazon Linux 2023: `sudo dnf install -y bubblewrap socat`), or pass \
         --allow-unsandboxed to proceed with artifacts stamped as unsandboxed.",
        missing.join(" and ")
    );
    Ok(())
}

/// Whether the agent that ran was actually confined, RECORDED in its entry.
///
/// The gap this closes: `write_settings` ran for every backend and only claude received the path, so
/// codex (`--dangerously-bypass-approvals-and-sandbox`) and kiro (`--trust-all-tools`) ran with
/// `<work>/.claude/settings.json` unread beside them -- the repo readable, including the graded oracle's
/// `test_vectors/` and every sibling agent's translation -- while `Enforcement::Required` refused to
/// launch on the grounds that it was not. `require_enforceable` only probes PATH, so it passed.
///
/// RECORDED rather than refused, deliberately and by the operator's decision: refusing would make every
/// codex and kiro run require `--allow-unsandboxed`, and the honest problem with the published rows is
/// that nobody can tell from the artifact which way it was. Now they can. `Ran` requires one of these,
/// so a backend cannot return a result without stating it.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Sandboxed {
    /// The CLI took the policy and `failIfUnavailable` was set, so it could not fail open.
    Enforced,
    /// This CLI has no mechanism to accept a filesystem policy. The default, because every entry
    /// written before this was recorded has no answer and `Enforced` would be a claim nobody checked.
    #[default]
    NotSupportedByBackend,
}

/// Whether the sandbox is enforceable, for the provenance record.
pub fn is_enforceable() -> bool {
    SANDBOX_BINARIES.iter().all(|b| which(b).is_some())
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

/// The scratch base is the non-obvious entry: sibling per-case work dirs live
/// under it and reads are default-allow *outside* `denyRead`, so it is denied
/// wholesale and this run's own root re-granted by `allowRead`, which wins.
/// Roots are canonicalised because `$HOME` is a symlink into `/local` while work dirs
/// are addressed as `/local/...`; an uncanonicalised deny root simply would not match
/// the path the agent uses.
///
/// The repo's PARENT is denied too. Reads are default-allow *outside* `denyRead`, so a
/// stale sibling results tree was readable — and one audited log did exactly that,
/// reading a third run's translated output.
pub fn denied_read_roots(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = vec![repo_root.to_path_buf(), crate::io::workdir::base()?];
    if let Some(parent) = repo_root.parent() {
        roots.push(parent.to_path_buf());
    }
    Ok(roots
        .into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect())
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
    /// Whether the operator accepted running without an enforceable sandbox. In the
    /// struct, not a parameter, so a new call site cannot omit the decision.
    pub enforcement: Enforcement,
}

/// Whether an unenforceable sandbox is fatal. A named enum because the polarity *is*
/// the safety property: `write_settings(.., false)` reads as "not allowed to be
/// unsandboxed" to one reader and "sandbox off" to another, and backwards it hands the
/// agent the graded oracle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Enforcement {
    /// Refuse to launch unless the sandbox can actually be enforced.
    Required,
    /// `--allow-unsandboxed`: warn, stamp the artifacts, continue.
    AllowUnsandboxed,
}

impl Enforcement {
    /// The one bool→enum boundary, named for the flag that is its only source so the
    /// polarity is checkable against `--help`.
    pub fn from_allow_unsandboxed_flag(flag: bool) -> Self {
        if flag {
            Enforcement::AllowUnsandboxed
        } else {
            Enforcement::Required
        }
    }
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
            // Without this the CLI degrades to unsandboxed with a banner, which is how
            // the policy stayed inert for 164 runs.
            "failIfUnavailable": true,
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
    // Scoped here rather than at startup: only the agents that receive a policy reach
    // this function, so c2rust/laertes/c2saferrust are not refused for lacking a shell.
    match policy.enforcement {
        Enforcement::AllowUnsandboxed if !is_enforceable() => eprintln!(
            "⚠️  --allow-unsandboxed: {SANDBOX_BINARIES:?} missing, so the agent runs with \
             the graded oracle readable. Artifacts are stamped unsandboxed."
        ),
        Enforcement::AllowUnsandboxed => {}
        Enforcement::Required => require_enforceable()?,
    }
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
        assert!(
            deny.iter().any(|d| d == "/repo"),
            "repo root must be denied: {deny:?}"
        );
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
        assert!(
            deny.iter().any(|d| d == "/scratch/base"),
            "scratch base must be denied: {deny:?}"
        );
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
            tc["sandbox"]["filesystem"]["denyRead"], hb["sandbox"]["filesystem"]["denyRead"],
            "deny list must not depend on how deep the case dir happens to be"
        );
    }

    #[test]
    fn sandbox_stanza_shape_is_what_claude_code_expects() {
        let p = policy("/repo", "/w");
        assert_eq!(p["sandbox"]["enabled"], true);
        assert_eq!(p["sandbox"]["allowUnsandboxedCommands"], false);
        for k in ["denyRead", "allowRead", "allowWrite"] {
            assert!(
                p["sandbox"]["filesystem"][k].is_array(),
                "{k} must be an array"
            );
        }
    }

    #[test]
    fn write_settings_creates_the_file_where_settings_flag_looks_for_it() {
        let tmp = crate::io::workdir::test_tempdir().unwrap();
        let parent = tmp.path().join("root");
        std::fs::create_dir_all(&parent).unwrap();
        // The real function, not `policy`: base() needs HOME, which tests have.
        let p = write_settings(Policy {
            repo_root: Path::new("/repo"),
            work_root: &parent,
            enforcement: Enforcement::AllowUnsandboxed,
        })
        .expect("writes policy");
        assert_eq!(p, parent.join(".claude/settings.json"));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["sandbox"]["enabled"], true);
        let deny = v["sandbox"]["filesystem"]["denyRead"].as_array().unwrap();
        assert!(deny.iter().any(|d| d == "/repo"));
        // repo root + its parent + the scratch base. The parent is here because reads
        // are default-allow outside denyRead, so a stale sibling results tree was
        // readable; one audited log read a third run's translated output.
        assert_eq!(deny.len(), 3, "repo root + repo parent + scratch base");
        assert!(deny.iter().any(|d| d == "/"), "the parent of /repo is /");
    }

    #[test]
    fn a_policy_that_cannot_be_enforced_is_refused() {
        // The defect this PR closes: for 164 runs `enabled: true` was written while
        // nothing applied it, because bwrap was absent and the CLI fails open. Whether
        // this machine can enforce is environmental, so assert the two directions
        // against the same predicate rather than hardcoding an expectation.
        let parent = crate::io::workdir::test_tempdir().unwrap();
        let refused = write_settings(Policy {
            repo_root: Path::new("/repo"),
            work_root: parent.path(),
            enforcement: Enforcement::Required,
        });
        assert_eq!(
            refused.is_ok(),
            is_enforceable(),
            "without --allow-unsandboxed, write_settings must succeed exactly when the \
             sandbox is enforceable"
        );
        if let Err(e) = refused {
            let msg = format!("{e:#}");
            assert!(msg.contains("cannot be enforced"), "{msg}");
            assert!(
                msg.contains("--allow-unsandboxed"),
                "must name the escape hatch: {msg}"
            );
        }
        // The escape hatch always proceeds, so an operator is never blocked outright.
        assert!(write_settings(Policy {
            repo_root: Path::new("/repo"),
            work_root: parent.path(),
            enforcement: Enforcement::AllowUnsandboxed
        })
        .is_ok());
    }

    #[test]
    fn the_deny_list_covers_the_repos_parent() {
        // Reads are default-allow OUTSIDE denyRead, so a stale sibling results tree was
        // readable — and one audited log read a third run's translated output.
        let roots = denied_read_roots(Path::new("/local/home/x/research/ACTOR")).unwrap();
        let strs: Vec<String> = roots.iter().map(|p| p.display().to_string()).collect();
        assert!(
            strs.iter().any(|s| s.ends_with("/research")),
            "the repo's parent must be denied: {strs:?}"
        );
    }
}
