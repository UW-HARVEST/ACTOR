//! Materializes the differential-verification environment into a crate's
//! workspace, so the verify agent can write property tests that run the C
//! reference and the translated Rust on the same generated inputs and assert
//! they agree.
//!
//! Approach: a pure-Rust `difftest` crate (proptest + libloading). The C
//! reference `.so` is built coverage-instrumented via the project's OWN
//! `c_src/CMakeLists.txt` (reused verbatim — handles zlib links, nested source
//! dirs, and compile defs), and both `.so`s are dlopen'd as black boxes and
//! compared across many generated + edge-biased inputs. Running the harness
//! pools coverage into `verify_env/cov/*.profraw`, which the completeness gate
//! reads to prove every public function was exercised.
//!
//! This replaced an earlier FuzzTest (C++/CMake/abseil/antlr) harness: that
//! build took minutes and OOM'd, and a rebuild after every agent edit recompiled
//! a 500-object dependency tree. proptest is smart-random rather than
//! coverage-guided, but for our need — breadth of coverage + catching
//! value-dependent divergences — it gets comparable results at a fraction of the
//! build cost (C `.so` ~2s, Rust ~1s, harness ~2s; rebuilds are incremental).

use anyhow::{Context, Result};
use std::path::Path;

/// Directory name (under the crate root) the verify env is materialized into.
pub const VERIFY_ENV_DIR: &str = "verify_env";

// Template files (kept editable under tools/verify_env_template/ for review, then
// embedded). The difftest crate's manifest + starter harness, the build driver,
// and the README.
const VE_DIFFTEST_CARGO: &str = include_str!("../verify_env_template/difftest_Cargo.toml");
const VE_DIFFTEST_MAIN: &str = include_str!("../verify_env_template/difftest_main.rs");
const VE_BUILD_SH: &str = include_str!("../verify_env_template/build.sh");
const VE_README: &str = include_str!("../verify_env_template/README.md");

/// Write the verification environment into `crate_root/verify_env/`.
///
/// `crate_root` is the workspace the agent operates in (it must contain
/// `c_src/CMakeLists.txt` — the project's own reference build). Lays down a
/// `difftest/` proptest crate, a `build.sh` that builds the coverage-instrumented
/// C `.so` + the Rust `.so` + the harness, a `cov/` dir where coverage pools, and
/// a README. `_fuzz` is accepted for call-site compatibility (the env is always
/// the differential harness now); when false the caller simply doesn't run it.
pub fn materialize(crate_root: &Path, _fuzz: bool) -> Result<()> {
    let env_dir = crate_root.join(VERIFY_ENV_DIR);
    std::fs::create_dir_all(&env_dir).context("creating verify_env dir")?;

    // Validate the C build is present and has a linkable library target. build.sh
    // reuses this CMake verbatim; we surface a clear error early if it's missing.
    let c_cmake = crate_root.join("c_src").join("CMakeLists.txt");
    let _c_target = std::fs::read_to_string(&c_cmake)
        .ok()
        .and_then(|s| parse_c_target(&s))
        .with_context(|| format!(
            "no add_library() target in {} — the difftest env reuses the project's \
             own c_src build to produce a coverage-instrumented reference .so",
            c_cmake.display()
        ))?;

    // cov/ is where the C reference's coverage profiles pool (build.sh bakes the
    // absolute cov-%m path into the C .so at link time, so profiles land here
    // regardless of the agent's CWD and accumulate across runs).
    std::fs::create_dir_all(env_dir.join("cov"))?;

    // The difftest crate.
    let difftest = env_dir.join("difftest");
    std::fs::create_dir_all(difftest.join("src"))?;
    std::fs::write(difftest.join("Cargo.toml"), VE_DIFFTEST_CARGO)?;
    std::fs::write(difftest.join("src").join("main.rs"), VE_DIFFTEST_MAIN)?;

    std::fs::write(env_dir.join("README.md"), VE_README)?;
    write_script(&env_dir.join("build.sh"), VE_BUILD_SH)?;
    Ok(())
}

/// Extract the C library target name from the project's `add_library(<name> ...`
/// call. Returns the first target declared with SHARED/STATIC sources (skips
/// alias/interface-only forms, which have no compile step to link against).
fn parse_c_target(cmakelists: &str) -> Option<String> {
    const MARKER: &str = "add_library(";
    let mut rest = cmakelists;
    while let Some(pos) = rest.find(MARKER) {
        rest = &rest[pos + MARKER.len()..];
        let token: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ')')
            .collect();
        let after = rest[token.len()..].split_whitespace().take(2).collect::<Vec<_>>();
        let is_linkable = !after.iter().any(|t| {
            matches!(*t, "ALIAS" | "IMPORTED" | "INTERFACE" | "OBJECT")
        });
        if !token.is_empty() && is_linkable {
            return Some(token);
        }
    }
    None
}

fn write_script(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_target() {
        let cml = "cmake_minimum_required(VERSION 3.10)\nadd_library(lz4 SHARED\n  src/lz4.c\n)\n";
        assert_eq!(parse_c_target(cml).as_deref(), Some("lz4"));
    }

    #[test]
    fn parses_globbed_target() {
        let cml = "file(GLOB_RECURSE SRCS libsodium/*.c)\nadd_library(sodium SHARED ${SRCS})\n";
        assert_eq!(parse_c_target(cml).as_deref(), Some("sodium"));
    }

    #[test]
    fn skips_alias_library() {
        let cml = "add_library(foo::foo ALIAS foo)\nadd_library(foo SHARED src/foo.c)\n";
        assert_eq!(parse_c_target(cml).as_deref(), Some("foo"));
    }

    #[test]
    fn none_when_no_target() {
        assert_eq!(parse_c_target("project(x C)\n"), None);
    }

    #[test]
    fn materialize_lays_down_difftest_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("c_src")).unwrap();
        std::fs::write(root.join("c_src/CMakeLists.txt"), "add_library(png SHARED src/png.c)\n").unwrap();

        materialize(root, true).unwrap();
        let ve = root.join("verify_env");
        // proptest crate + build driver + cov dir, NO FuzzTest/CMake scaffold.
        assert!(ve.join("difftest/Cargo.toml").is_file(), "difftest manifest");
        assert!(ve.join("difftest/src/main.rs").is_file(), "difftest harness");
        assert!(ve.join("build.sh").is_file(), "build driver");
        assert!(ve.join("cov").is_dir(), "cov/ dir for pooled coverage");
        assert!(!ve.join("CMakeLists.txt").exists(), "no FuzzTest CMake");
        assert!(!ve.join("build_fuzz.sh").exists(), "no FuzzTest build script");

        let cargo = std::fs::read_to_string(ve.join("difftest/Cargo.toml")).unwrap();
        assert!(cargo.contains("proptest"), "proptest dep");
        assert!(cargo.contains("libloading"), "libloading dep");
        assert!(cargo.contains("[workspace]"), "detached from parent workspace");

        let build = std::fs::read_to_string(ve.join("build.sh")).unwrap();
        assert!(build.contains("-fprofile-instr-generate="), "C .so coverage-instrumented");
        assert!(build.contains("cov/cov-%m.profraw"), "pooled cov pattern");
    }
}
