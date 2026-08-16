//! Where the harness puts its per-case scratch trees.
//!
//! These are multi-hundred-MB build trees (`target/` alone runs to ~350 MB), and
//! `tempfile`'s default puts them in `std::env::temp_dir()` — on the dev desktops
//! `/tmp` is a **tmpfs**, so every C build, Rust build and test binary would run
//! in RAM (one agent-generated harness wrote a 12.0 GiB log there and killed the
//! sweep).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const ENV_BASE: &str = "HARVEST_WORK_BASE";
/// Escape hatch for hosts whose only writable filesystem is a tmpfs (some CI).
pub const ENV_ALLOW_TMPFS: &str = "HARVEST_ALLOW_TMPFS_WORK";

static BASE: OnceLock<PathBuf> = OnceLock::new();

/// Whether a RAM-backed scratch base is tolerated. Named rather than `bool` because the
/// permissive side of this switch is what lets a runaway build tree be charged to memory
/// and kill the host, and `resolve_from(.., .., true, ..)` does not say which side that is.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Tmpfs {
    Refuse,
    Allow,
}

/// Base for scratch trees: `$HARVEST_WORK_BASE`, else `$HOME/.harvest/work`.
///
/// Not under `$HOME/.cache` even though these trees are disposable: that dir is
/// treated as free-to-delete (`brazil-package-cache-clean` sweeps part of it on a
/// timer), which would destroy in-flight cases.
///
/// `TMPDIR` is deliberately not consulted: it is normally unset, so it would
/// fall through to the `/tmp` tmpfs this module exists to avoid, and it collides
/// with the `TMPDIR` the harness sets for the agent child (see `agent_tmp`).
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
        std::env::var_os(ENV_BASE)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from),
        if std::env::var_os(ENV_ALLOW_TMPFS).is_some() {
            Tmpfs::Allow
        } else {
            Tmpfs::Refuse
        },
        &mounts,
    )
}

/// Every input is a parameter so the precedence and the tmpfs refusal are
/// testable without mutating process env, which races across test threads.
fn resolve_from(
    base_override: Option<PathBuf>,
    home: Option<PathBuf>,
    tmpfs: Tmpfs,
    mounts: &str,
) -> Result<PathBuf> {
    let base = match base_override {
        Some(b) => b,
        None => home
            .with_context(|| format!("HOME is unset; set {ENV_BASE} to choose a scratch base"))?
            .join(".harvest/work"),
    };
    if tmpfs == Tmpfs::Refuse {
        // Decide before creating, or the refusal has already written to the location it
        // refuses. The base itself need not exist yet for that: a mount point does, so its
        // nearest existing ancestor is on the same filesystem. Canonicalised, because $HOME
        // is a symlink into /local here and prefix-matching an uncanonicalised path against
        // /proc/mounts misfires.
        let probe = base
            .ancestors()
            .find(|a| a.exists())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .with_context(|| format!("resolving scratch base {}", base.display()))?;
        if let Some(fstype) = fstype_for(mounts, &probe) {
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
    std::fs::create_dir_all(&base)
        .with_context(|| format!("creating scratch base {}", base.display()))?;
    base.canonicalize()
        .with_context(|| format!("resolving scratch base {}", base.display()))
}

/// Filesystem type of the mount containing `path`, by longest-prefix match over
/// `/proc/mounts` content: a shorter mount like `/` prefixes almost everything.
fn fstype_for(mounts: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        // <device> <mountpoint> <fstype> <opts> ...  — mountpoints escape spaces as \040.
        // Skipping a malformed line rather than bailing keeps a correct match
        // found earlier, instead of silently reporting "unknown fs".
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

pub fn tempdir(prefix: &str) -> Result<tempfile::TempDir> {
    let base = base()?;
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&base)
        .with_context(|| format!("creating {prefix}* scratch dir in {}", base.display()))
}

/// Every machine-specific root [`crate::cache::normalise`] rewrites to a stable token.
///
/// One named struct rather than four paths in a row: they are all the same type, and a
/// transposed root is a wrong cache key — silently, since the digest is still a digest.
/// The two optional ones are `None` where the root does not exist to be substituted;
/// leaving a literal path in the normalised text can only cost a cache miss.
pub struct Roots {
    pub work: PathBuf,
    pub repo: PathBuf,
    pub work_base: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

impl Roots {
    /// The reads `normalise` used to do inside itself, in the layer that is allowed to do
    /// them — and where `$HOME` and the scratch base are already this module's business.
    /// A key-affecting function that consults the environment cannot be reasoned about
    /// from its arguments, and could not be tested without setting one.
    pub fn resolve(work: &Path, repo: &Path) -> Self {
        Self {
            work: work.to_path_buf(),
            repo: repo.to_path_buf(),
            work_base: base().ok(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

/// Scratch dir handed to the agent as `TMPDIR`: agent-generated test code calls
/// `std::env::temp_dir()`, which is the `/tmp` tmpfs unless `TMPDIR` is set, and
/// inside the work root that scratch is on disk and discarded with the case.
pub fn agent_tmp(work_root: &Path) -> Result<PathBuf> {
    let dir = work_root.join("tmp");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating agent TMPDIR {}", dir.display()))?;
    Ok(dir)
}

/// `ulimit -f` argument capping any single file the agent writes. Backstop for
/// agent code that hardcodes `/tmp` (30 files in the current corpus do), where
/// `TMPDIR` cannot help: the write dies with SIGXFSZ, failing one case instead of
/// the sweep or the host.
///
/// Unit is **1024-byte blocks**: POSIX specifies 512 for `-f` but bash, which is
/// what we invoke, uses 1024. Assuming 512 silently doubled the cap to 8 GiB.
pub const AGENT_FSIZE_BLOCKS: u64 = 4 * 1024 * 1024;

/// `ulimit -d` argument, in KB, capping the heap of *each* process under the
/// agent (RLIMIT_DATA is per-process and covers anonymous mmap since Linux 4.7).
///
/// Uncapped, a runaway test binary (13.44 GB anon RSS observed) OOMs the sweep's
/// whole cgroup: no result is recorded and `Restart=on-failure` then re-enters the
/// same case forever. Capped, the allocation fails inside the test — a *recorded*
/// outcome.
///
/// 6 GiB is above the largest legitimate test binaries seen (`driver` cases at
/// 4.4–4.7 GB), below that runaway, and leaves two concurrent cases plus agent,
/// cargo and rust-analyzer inside a 16 GiB `MemoryMax` (2×6 + ~2 = 14 GiB).
///
/// A case that fails *at* this cap needs review, not silent scoring: the
/// allocation may be a translation-fidelity signal, not a harness bug.
pub const AGENT_DATA_KB: u64 = 6 * 1024 * 1024;

/// The one scratch tree for tests, in `tools/target/tmp`: gitignored, cleared by `cargo
/// clean`, and on disk rather than in the `/tmp` tmpfs where 24,707 leaked trees exhausted
/// the inode table and every process on the box then failed to create a file.
///
/// Derived from `CARGO_MANIFEST_DIR`, not from the `TMPDIR` `.cargo/config.toml` sets to
/// the same directory for child processes: cargo reads that config by walking up from the
/// invocation directory, so any run started elsewhere would leave the variable unset and
/// put the suite back on the tmpfs with nothing going red.
///
/// Creating the directory is part of the job: cargo sets the variable without creating it,
/// and `rm -rf target/tmp` — the documented cleanup for the read-only trees a killed run
/// leaves behind — then failed 90 of 180 tests with `NotFound`.
///
/// `pub` and not `#[cfg(test)]` so the `--test` targets reach this one definition; a copy
/// for them drifts, and half the suite quietly returning to /tmp is the incident itself.
pub fn test_tempdir() -> Result<tempfile::TempDir> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the suite scratch root {}", dir.display()))?;
    tempfile::Builder::new()
        .tempdir_in(&dir)
        .with_context(|| format!("creating a scratch tree in {}", dir.display()))
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
        assert_eq!(
            fstype_for(
                MOUNTS,
                Path::new("/local/home/scheschb/.cache/harvest/work")
            )
            .as_deref(),
            Some("xfs")
        );
        assert_eq!(
            fstype_for(MOUNTS, Path::new("/var/log")).as_deref(),
            Some("xfs")
        );
    }

    #[test]
    fn fstype_detects_tmpfs() {
        assert_eq!(
            fstype_for(MOUNTS, Path::new("/tmp/harvest-work-abc")).as_deref(),
            Some("tmpfs")
        );
        assert_eq!(
            fstype_for(MOUNTS, Path::new("/dev/shm/x")).as_deref(),
            Some("tmpfs")
        );
    }

    #[test]
    fn fstype_handles_escaped_spaces_and_short_lines() {
        let m = "/dev/sda1 /mnt/my\\040disk ext4 rw 0 0\nbroken\n";
        assert_eq!(
            fstype_for(m, Path::new("/mnt/my disk/x")).as_deref(),
            Some("ext4")
        );
        assert_eq!(fstype_for(m, Path::new("/elsewhere")), None);
    }

    #[test]
    fn explicit_base_wins_over_home_and_is_created_recursively() {
        let tmp = test_tempdir().unwrap();
        let want = tmp.path().join("deep/nested/base");
        let home = tmp.path().join("home");
        let got = resolve_from(Some(want.clone()), Some(home.clone()), Tmpfs::Allow, MOUNTS)
            .expect("explicit base resolves");
        assert!(got.is_dir(), "base should be created recursively");
        assert_eq!(got, want.canonicalize().unwrap());
        assert!(
            !home.exists(),
            "HOME must be ignored when an override is given"
        );
    }

    #[test]
    fn default_base_derives_from_home_and_is_not_tmp_or_cache() {
        let tmp = test_tempdir().unwrap();
        let home = tmp.path().join("home");
        let got = resolve_from(None, Some(home.clone()), Tmpfs::Allow, MOUNTS)
            .expect("resolves from HOME");
        assert_eq!(got, home.join(".harvest/work").canonicalize().unwrap());
        assert!(
            !got.starts_with("/tmp/harvest"),
            "must not be tempfile's old default"
        );
        assert!(
            !got.components().any(|c| c.as_os_str() == ".cache"),
            "scratch must not live under a cache dir: {}",
            got.display()
        );
    }

    /// A mount table that declares `dir` RAM-backed. `resolve_from` takes the table, so
    /// the tests below control what counts as tmpfs; they used to rely on
    /// `tempfile::tempdir()` landing under the `/tmp` MOUNTS calls tmpfs, which stopped
    /// being true once the suite's scratch moved onto disk (`test_tempdir`).
    fn mounts_calling_it_tmpfs(dir: &Path) -> String {
        format!(
            "{MOUNTS}tmpfs {} tmpfs rw,nosuid,nodev 0 0\n",
            dir.display()
        )
    }

    #[test]
    fn tmpfs_base_is_refused_with_an_actionable_message() {
        let tmp = test_tempdir().unwrap();
        // Canonical, because `resolve_from` matches the mount table against the
        // canonicalised base.
        let base = tmp.path().canonicalize().unwrap();
        let mounts = mounts_calling_it_tmpfs(&base);
        let err = resolve_from(Some(base), None, Tmpfs::Refuse, &mounts)
            .expect_err("a tmpfs base must be refused");
        let err = format!("{err:#}");
        assert!(
            err.contains("tmpfs"),
            "message should name the fstype: {err}"
        );
        assert!(
            err.contains(ENV_BASE),
            "message should say how to fix it: {err}"
        );
        assert!(
            err.contains(ENV_ALLOW_TMPFS),
            "message should offer the override: {err}"
        );
    }

    #[test]
    fn tmpfs_base_is_accepted_when_explicitly_allowed() {
        let tmp = test_tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let mounts = mounts_calling_it_tmpfs(&base);
        resolve_from(Some(base), None, Tmpfs::Allow, &mounts)
            .expect("override should permit a tmpfs base");
    }

    #[test]
    fn a_refused_base_is_not_created_before_the_refusal() {
        let tmp = test_tempdir().unwrap();
        let parent = tmp.path().canonicalize().unwrap();
        let base = parent.join("work");
        let mounts = mounts_calling_it_tmpfs(&parent);
        resolve_from(Some(base.clone()), None, Tmpfs::Refuse, &mounts)
            .expect_err("a tmpfs base must be refused");
        assert!(
            !base.exists(),
            "the refusal wrote {} anyway, so it had already used what it declined",
            base.display()
        );
    }

    #[test]
    fn missing_home_without_override_errors_and_says_what_to_set() {
        let err = resolve_from(None, None, Tmpfs::Allow, MOUNTS)
            .expect_err("no base and no HOME must fail");
        let err = format!("{err:#}");
        assert!(
            err.contains(ENV_BASE),
            "error should name the env var to set: {err}"
        );
    }

    #[test]
    fn unusable_base_errors_and_names_the_path() {
        let tmp = test_tempdir().unwrap();
        let file = tmp.path().join("i-am-a-file");
        std::fs::write(&file, b"x").unwrap();
        let bad = file.join("cannot/nest/under/a/file");
        let err = resolve_from(Some(bad), None, Tmpfs::Allow, MOUNTS)
            .expect_err("nesting under a regular file must fail");
        let err = format!("{err:#}");
        assert!(
            err.contains("cannot/nest"),
            "error should name the offending path, got: {err}"
        );
    }

    #[test]
    fn agent_tmp_is_inside_the_work_root() {
        let tmp = test_tempdir().unwrap();
        let dir = agent_tmp(tmp.path()).unwrap();
        assert!(dir.starts_with(tmp.path()));
        assert!(dir.is_dir());
    }

    #[test]
    fn fsize_cap_is_four_gib_in_bashs_1024_byte_blocks() {
        // bash's ulimit -f block size, not POSIX's 512: /proc/<pid>/limits showed
        // "Max file size = 8589934592" when this constant assumed 512.
        assert_eq!(AGENT_FSIZE_BLOCKS * 1024, 4 * 1024u64.pow(3));
    }

    #[test]
    fn fsize_cap_still_refuses_the_runaway_log_that_motivated_it() {
        // /tmp/driver-difftest/cfg26.log, 2026-08-13 22:26.
        let runaway_bytes = 12_888_260_608u64;
        assert!(AGENT_FSIZE_BLOCKS * 1024 < runaway_bytes);
    }

    #[test]
    fn data_cap_is_six_gib_and_two_of_them_fit_a_16_gib_cgroup() {
        assert_eq!(AGENT_DATA_KB * 1024, 6 * 1024u64.pow(3));
        // parallel 2 must leave room for the agent, cargo and rust-analyzer.
        let two_cases = 2 * AGENT_DATA_KB * 1024;
        let cgroup_max = 16 * 1024u64.pow(3);
        assert!(
            cgroup_max - two_cases >= 2 * 1024u64.pow(3),
            "two capped cases must leave >=2 GiB headroom in a 16 GiB cgroup"
        );
    }

    #[test]
    fn data_cap_admits_the_largest_legitimate_test_binary_seen() {
        let legit_peak_kb = 4_896_256; // anon-rss from the kernel OOM report
        let runaway_kb = 14_091_264;
        assert!(
            AGENT_DATA_KB > legit_peak_kb,
            "must not break the 4.67 GB cases"
        );
        assert!(
            AGENT_DATA_KB < runaway_kb,
            "must refuse the 13.44 GB runaway"
        );
    }
}
