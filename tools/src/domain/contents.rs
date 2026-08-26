use crate::domain::relpath::RelPath;
use std::path::Path;

/// What a file contributes to. The agent's build output is legitimately its work, but it
/// is regenerable, 9x the bytes (4,536 MB vs 500 MB over `results/`), and where per-run
/// paths get baked in — so it is neither carried nor hashed.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Disposition {
    StoreAndHash,
    BuildOutput,
    Ignore,
}

const BUILD_DIRS: &[&str] = &[
    "target",
    "build",
    "c_build",
    "build_c",
    "artifacts",
    "gtest_build",
    "CMakeFiles",
    "e2e_out",
    "build_ffi",
    "fuzz_scripts",
];

/// Harness bookkeeping. Matched only at the artifact ROOT: deeper down the same name
/// is the translated program's own file, and must be inside its digest.
const ROOT_ONLY_IGNORED: &[&str] = &[
    "result.json",
    "verification.json",
    "translation.json",
    "harvest_bench_report.json",
    "harvest_batch_report.json",
];

/// Likewise root-anchored: `src/logs/` is source, and matching `logs` at any depth hid
/// it from the digest entirely.
const ROOT_ONLY_IGNORED_DIRS: &[&str] = &["logs", ".claude"];

/// The C oracle. Nothing under it is ever [`Disposition::Ignore`]:
/// [`crate::artifact::Scrubbed::seal`] grades a run by comparing this subtree file by file
/// before and after, so a rule firing inside it hides a change to the reference. 26 real
/// `c_src/doc/footer.html.bak` files sat in that blind spot.
pub(crate) const C_ORACLE_DIR: &str = "c_src";

/// `in_build_dir` must be true if any ancestor within the tree was itself classified
/// `BuildOutput`, including by the content sniff in `visit`: the name check below misses
/// `c_src/build`, which is *nested* (so a top-level check walks past it) and which is
/// precisely the directory whose `CMakeCache.txt` records the random scratch path.
pub fn classify(rel: &RelPath, in_build_dir: bool) -> Disposition {
    let p = rel.as_path();

    if is_ignored(p) {
        return Disposition::Ignore;
    }

    if in_build_dir
        || p.components().any(|c| {
            // Bytes, not `to_string_lossy`: a lossy name maps every invalid byte to U+FFFD,
            // so two different directories can compare equal here and be classified alike.
            let s = c.as_os_str().as_encoded_bytes();
            BUILD_DIRS.iter().any(|d| d.as_bytes() == s) || s.starts_with(b"cbuild")
        })
    {
        return Disposition::BuildOutput;
    }

    Disposition::StoreAndHash
}

fn is_ignored(p: &Path) -> bool {
    let mut components = p.components().map(|c| c.as_os_str());
    let Some(first) = components.next() else {
        return false;
    };
    if first == C_ORACLE_DIR {
        return false;
    }
    if ROOT_ONLY_IGNORED_DIRS.iter().any(|d| first == *d) {
        return true;
    }
    let at_root = components.next().is_none();
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    (at_root && ROOT_ONLY_IGNORED.contains(&name))
        || name.ends_with(".log")
        || name.ends_with(".bak")
        || name.ends_with(".sha256")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn classify_ignores_harness_output_and_logs() {
        assert_eq!(classify(&rel("result.json"), false), Disposition::Ignore);
        assert_eq!(
            classify(&rel("verification.json"), false),
            Disposition::Ignore
        );
        assert_eq!(
            classify(&rel("logs/verify.log"), false),
            Disposition::Ignore
        );
        assert_eq!(classify(&rel("src/x.rs.bak"), false), Disposition::Ignore);
    }

    /// The harness writes its bookkeeping at the phase-dir root. The same NAME deeper in
    /// the tree is the translated program's own file and must be inside its digest.
    #[test]
    fn harness_bookkeeping_is_ignored_only_at_the_artifact_root() {
        for name in ROOT_ONLY_IGNORED {
            assert_eq!(
                classify(&rel(name), false),
                Disposition::Ignore,
                "{name} at the root"
            );
            let nested = format!("src/data/{name}");
            assert_eq!(
                classify(&rel(&nested), false),
                Disposition::StoreAndHash,
                "{nested} belongs to the translation, not the harness"
            );
        }
        assert_eq!(
            classify(&rel("src/logs/mod.rs"), false),
            Disposition::StoreAndHash,
            "a source module named logs/ is source"
        );
    }

    /// `Scrubbed::seal` grades a run on the c_src file set before vs after. Anything
    /// excluded there is a change to the reference that nothing detects — and
    /// `c_src/doc/footer.html.bak` is a real upstream file in 26 stored cases.
    #[test]
    fn nothing_under_the_c_oracle_is_ignored() {
        for p in [
            "c_src/doc/footer.html.bak",
            "c_src/tests/expected.log",
            "c_src/lib.c.sha256",
            "c_src/logs/note.txt",
            "c_src/result.json",
        ] {
            assert_eq!(classify(&rel(p), false), Disposition::StoreAndHash, "{p}");
        }
        // Build output under the oracle stays build output: its CMakeCache.txt names a
        // dead scratch dir, which is why it is neither carried nor hashed.
        assert_eq!(
            classify(&rel("c_src/build/CMakeCache.txt"), false),
            Disposition::BuildOutput
        );
    }

    #[test]
    fn classify_treats_named_build_dirs_as_build_output() {
        for p in [
            "target/debug/x",
            "cbuild/a",
            "gtest_build/b",
            "artifacts/cbuild_sub_7/c",
        ] {
            assert_eq!(classify(&rel(p), false), Disposition::BuildOutput, "{p}");
        }
    }

    #[test]
    fn classify_catches_nested_build_dirs_a_toplevel_check_would_miss() {
        assert_eq!(
            classify(&rel("c_src/build/CMakeCache.txt"), false),
            Disposition::BuildOutput
        );
        assert_eq!(
            classify(&rel("weird_name/CMakeCache.txt"), true),
            Disposition::BuildOutput
        );
    }

    #[test]
    fn classify_keeps_source_and_dotfiles() {
        assert_eq!(
            classify(&rel("src/lib.rs"), false),
            Disposition::StoreAndHash
        );
        assert_eq!(
            classify(&rel("Cargo.lock"), false),
            Disposition::StoreAndHash
        );
        // .cargo/config.toml is a real build input in 16 corpus cases.
        assert_eq!(
            classify(&rel(".cargo/config.toml"), false),
            Disposition::StoreAndHash
        );
        assert_eq!(
            classify(&rel("c_src/src/lib.c"), false),
            Disposition::StoreAndHash
        );
    }
}
