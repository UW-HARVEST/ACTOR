//! Where the harness puts its per-case scratch trees.
//!
//! These are multi-hundred-MB build trees (a `target/` dir alone runs to
//! ~350 MB), not temp files. `tempfile`'s default lands them in
//! `std::env::temp_dir()`, i.e. `/tmp` — and on the dev desktops `/tmp` is a
//! **tmpfs**, so every C build, Rust build and test binary would run in RAM.
//! On 2026-08-13 one agent-generated differential harness wrote a 12.0 GiB log
//! into that tmpfs, which consumed the sweep's whole memory budget and killed
//! it; earlier the same day the same class of pressure wedged the host for
//! 2h13m with no swap to absorb it.
//!
//! So the base is resolved explicitly, defaults to disk, and **hard-errors if
//! it resolves onto a tmpfs** unless the operator opts in. A wrong answer here
//! is expensive and silent, so it is better to refuse than to guess.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Override the base directory for all harness scratch trees.
pub const ENV_BASE: &str = "HARVEST_WORK_BASE";
/// Escape hatch for hosts whose only writable filesystem is a tmpfs (some CI).
pub const ENV_ALLOW_TMPFS: &str = "HARVEST_ALLOW_TMPFS_WORK";

static BASE: OnceLock<PathBuf> = OnceLock::new();

/// Resolved base for scratch trees: `$HARVEST_WORK_BASE`, else
/// `$HOME/.harvest/work`.
///
/// **Not** under `$HOME/.cache`, despite these trees being disposable. XDG
/// defines the cache dir as regenerable data safe to delete at any time, and on
/// the dev desktops it is treated that way in practice: it already holds ~75 GB
/// (huggingface, uv, pip, torch) on a filesystem with ~83 GB free, so it is the
/// first place anyone reclaims space from, and `brazil-package-cache-clean`
/// already sweeps a subdirectory of it on a timer. Deleting it mid-sweep would
/// destroy in-flight cases. A live 20-hour build tree is working state, not
/// cache, so it gets its own directory.
///
/// `TMPDIR` is deliberately **not** consulted either. It is normally unset, so
/// including it would fall through to `/tmp` and quietly reinstate the tmpfs
/// default this module exists to avoid. It also collides with the `TMPDIR` the
/// harness *sets for the agent child* (see `agent_tmp`), which would be
/// confusing to reason about.
pub fn base() -> Result<PathBuf> {
    if let Some(p) = BASE.get() {
        return Ok(p.clone());
    }
    let resolved = resolve()?;
    let _ = BASE.set(resolved.clone());
    Ok(resolved)
}

fn resolve() -> Result<PathBuf> {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    resolve_from(
        std::env::var_os(ENV_BASE).filter(|v| !v.is_empty()).map(PathBuf::from),
        std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from),
        std::env::var_os(ENV_ALLOW_TMPFS).is_some(),
        &mounts,
    )
}

/// Pure-ish core of [`resolve`]: every input is a parameter, so the precedence
/// and the tmpfs refusal are testable without mutating process env (which races
/// across the test harness's threads) or reading the real mount table.
fn resolve_from(
    base_override: Option<PathBuf>,
    home: Option<PathBuf>,
    allow_tmpfs: bool,
    mounts: &str,
) -> Result<PathBuf> {
    let base = match base_override {
        Some(b) => b,
        None => home
            .with_context(|| format!("HOME is unset; set {ENV_BASE} to choose a scratch base"))?
            .join(".harvest/work"),
    };
    std::fs::create_dir_all(&base)
        .with_context(|| format!("creating scratch base {}", base.display()))?;
    // Canonicalise before the fstype check: $HOME is a symlink into /local on
    // the dev desktops, and prefix-matching an uncanonicalised path against
    // /proc/mounts mis-resolves.
    let base = base
        .canonicalize()
        .with_context(|| format!("resolving scratch base {}", base.display()))?;

    if !allow_tmpfs {
        if let Some(fstype) = fstype_for(mounts, &base) {
            if fstype == "tmpfs" || fstype == "ramfs" {
                bail!(
                    "scratch base {} is on {fstype} (RAM). Per-case build trees there are charged \
                     to memory and a runaway test log will kill the run or the host. Point \
                     {ENV_BASE} at a disk-backed path, or set {ENV_ALLOW_TMPFS}=1 to override.",
                    base.display()
                );
            }
        }
    }
    Ok(base)
}

/// Filesystem type of the mount containing `path`, by longest-prefix match over
/// `/proc/mounts` content. Split out from `resolve` so it is unit-testable
/// without touching the real mount table.
fn fstype_for(mounts: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        // <device> <mountpoint> <fstype> <opts> ...  — mountpoints escape spaces as \040.
        // Skip malformed lines rather than `?`-ing out: aborting the scan would
        // discard a correct match found earlier and silently report "unknown fs".
        let mut f = line.split_whitespace();
        let (Some(_dev), Some(mnt), Some(fstype)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let mnt = mnt.replace("\\040", " ");
        if path.starts_with(&mnt) && best.as_ref().is_none_or(|(len, _)| mnt.len() > *len) {
            best = Some((mnt.len(), fstype.to_string()));
        }
    }
    best.map(|(_, t)| t)
}

/// Create a scratch tree named `<prefix><random>` under [`base`].
pub fn tempdir(prefix: &str) -> Result<tempfile::TempDir> {
    let base = base()?;
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&base)
        .with_context(|| format!("creating {prefix}* scratch dir in {}", base.display()))
}

/// Scratch directory to hand the agent as `TMPDIR`, inside its own work root.
///
/// This is the fix for the failure that motivated this module: the 12.0 GiB log
/// was written by *agent-generated* test code calling
/// `std::env::temp_dir().join("driver-difftest")`, which resolved to `/tmp`
/// because nothing in the harness set `TMPDIR`. Pointing it inside the work
/// root puts that scratch on disk *and* inside the tree that is discarded when
/// the case finishes, instead of leaking into a shared RAM disk.
pub fn agent_tmp(work_root: &Path) -> Result<PathBuf> {
    let dir = work_root.join("tmp");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating agent TMPDIR {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "\
/dev/nvme0n1p1 / xfs rw,relatime 0 0
tmpfs /tmp tmpfs rw,nosuid,nodev 0 0
tmpfs /dev/shm tmpfs rw,nosuid,nodev 0 0
/dev/nvme0n1p1 /local/home xfs rw,relatime 0 0
";

    #[test]
    fn fstype_picks_the_longest_matching_mount_not_the_first() {
        // `/` also prefixes this path; the deeper mount must win.
        assert_eq!(
            fstype_for(MOUNTS, Path::new("/local/home/scheschb/.cache/harvest/work")).as_deref(),
            Some("xfs")
        );
        assert_eq!(fstype_for(MOUNTS, Path::new("/var/log")).as_deref(), Some("xfs"));
    }

    #[test]
    fn fstype_detects_tmpfs() {
        assert_eq!(fstype_for(MOUNTS, Path::new("/tmp/harvest-work-abc")).as_deref(), Some("tmpfs"));
        assert_eq!(fstype_for(MOUNTS, Path::new("/dev/shm/x")).as_deref(), Some("tmpfs"));
    }

    #[test]
    fn fstype_handles_escaped_spaces_and_short_lines() {
        let m = "/dev/sda1 /mnt/my\\040disk ext4 rw 0 0\nbroken\n";
        assert_eq!(fstype_for(m, Path::new("/mnt/my disk/x")).as_deref(), Some("ext4"));
        assert_eq!(fstype_for(m, Path::new("/elsewhere")), None);
    }

    #[test]
    fn explicit_base_wins_over_home_and_is_created_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        let want = tmp.path().join("deep/nested/base");
        let home = tmp.path().join("home");
        let got = resolve_from(Some(want.clone()), Some(home.clone()), true, MOUNTS)
            .expect("explicit base resolves");
        assert!(got.is_dir(), "base should be created recursively");
        assert_eq!(got, want.canonicalize().unwrap());
        assert!(!home.exists(), "HOME must be ignored when an override is given");
    }

    #[test]
    fn default_base_derives_from_home_and_is_not_tmp_or_cache() {
        // The regression this module exists to prevent: with no override the base
        // must not be /tmp the way tempfile's default was. And not under .cache,
        // which is routinely purged to reclaim disk and would take in-flight
        // cases with it.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let got = resolve_from(None, Some(home.clone()), true, MOUNTS).expect("resolves from HOME");
        assert_eq!(got, home.join(".harvest/work").canonicalize().unwrap());
        assert!(!got.starts_with("/tmp/harvest"), "must not be tempfile's old default");
        assert!(
            !got.components().any(|c| c.as_os_str() == ".cache"),
            "scratch must not live under a cache dir: {}",
            got.display()
        );
    }

    #[test]
    fn tmpfs_base_is_refused_with_an_actionable_message() {
        // tempfile::tempdir() lands in /tmp, which MOUNTS marks as tmpfs — exactly
        // the situation that killed the 2026-08-13 sweep.
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_from(Some(tmp.path().to_path_buf()), None, false, MOUNTS)
            .expect_err("a tmpfs base must be refused");
        let err = format!("{err:#}");
        assert!(err.contains("tmpfs"), "message should name the fstype: {err}");
        assert!(err.contains(ENV_BASE), "message should say how to fix it: {err}");
        assert!(err.contains(ENV_ALLOW_TMPFS), "message should offer the override: {err}");
    }

    #[test]
    fn tmpfs_base_is_accepted_when_explicitly_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        resolve_from(Some(tmp.path().to_path_buf()), None, true, MOUNTS)
            .expect("override should permit a tmpfs base");
    }

    #[test]
    fn missing_home_without_override_errors_and_says_what_to_set() {
        let err = resolve_from(None, None, true, MOUNTS).expect_err("no base and no HOME must fail");
        let err = format!("{err:#}");
        assert!(err.contains(ENV_BASE), "error should name the env var to set: {err}");
    }

    #[test]
    fn unusable_base_errors_and_names_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("i-am-a-file");
        std::fs::write(&file, b"x").unwrap();
        let bad = file.join("cannot/nest/under/a/file");
        let err = resolve_from(Some(bad), None, true, MOUNTS)
            .expect_err("nesting under a regular file must fail");
        let err = format!("{err:#}");
        assert!(err.contains("cannot/nest"), "error should name the offending path, got: {err}");
    }

    #[test]
    fn agent_tmp_is_inside_the_work_root() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = agent_tmp(tmp.path()).unwrap();
        assert!(dir.starts_with(tmp.path()));
        assert!(dir.is_dir());
    }

}
