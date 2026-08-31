//! One harvest-bench project, from the corpus to the store lookup.
//!
//! Its own test target: the chain's backend reads `HARVEST_CLI_VERSION`, and setting process env is
//! only race-free where nothing else can observe it, so this file holds exactly ONE test.

use harvest_tools::battery::Paths;
use harvest_tools::cli::{Dataset, Tool, Variant};
use harvest_tools::store::Mode;
use harvest_tools::{agents, benchmark, chain, io, prompt, store};

/// A project reaches its agent, and the only thing it is missing is a stored entry.
///
/// `run HB` for claude, codex and kiro each died at second zero with `Battery not found:
/// harvest-bench/tests/Public-Tests/jansson`. `--replay-only` on an empty store is what makes the
/// whole path -- discovery, paths, prompt, backend -- testable for free: all of it must work before a
/// run can get as far as asking for an entry that is not there.
#[test]
fn a_harvest_bench_project_reaches_its_agent_and_wants_only_a_stored_entry() {
    let tmp = io::workdir::test_tempdir().unwrap();
    let root = tmp.path();
    let real = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tools/ has a parent");

    // The real prompts: the composition exercised is the one a paid run would send.
    for rel in [
        "prompts/shared/translate-library.md",
        "prompts/claude/protocol.md",
    ] {
        let dst = root.join(rel);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(real.join(rel), &dst)
            .unwrap_or_else(|e| panic!("copying {rel}: {e}  (the fixture needs the real prompt)"));
    }
    let project = "tinylib";
    let case = root.join("harvest-bench/tests").join(project);
    std::fs::create_dir_all(case.join("test_case/src")).unwrap();
    std::fs::write(case.join("test_case/src/lib.c"), "int f(void){return 1;}\n").unwrap();
    std::fs::create_dir_all(case.join("gtest_suite")).unwrap();
    std::fs::write(case.join("gtest_suite/CMakeLists.txt"), "# suite\n").unwrap();
    std::fs::create_dir_all(root.join("results/.cache")).unwrap();

    // Recorded, never keyed, and a replay must not probe for it (#109).
    std::env::set_var(
        "HARVEST_CLI_VERSION",
        "replay-only: no agent CLI was invoked",
    );

    let paths = Paths::new(
        root,
        Tool::Claude,
        Variant::Default,
        Dataset::HarvestBench,
        None,
        Mode::ReplayOnly,
        io::sandbox::Enforcement::AllowUnsandboxed,
    )
    .unwrap();

    // Fixture assumption: this corpus really is in harvest-bench's layout and not the other one.
    assert!(paths.input_dir(project).join("test_case").is_dir());
    assert!(!paths.corpus_dir.join("Public-Tests").exists());

    let bench = benchmark::for_dataset(Dataset::HarvestBench);
    let jobs = bench
        .jobs(&paths, project, None)
        .expect("the dataset must be able to describe its own corpus");
    assert_eq!(jobs.len(), 1, "a project is one case");

    let two_steps = prompt::chain(Tool::Claude, Variant::Default);
    assert_eq!(two_steps.len(), 2, "claude runs translate then verify");

    let s = store::Store::open(root, Mode::ReplayOnly).unwrap();
    let ran = chain::run_unit(&paths, &s, project, &jobs, None, &agents::Pool::for_run(1))
        .expect("a case the store cannot serve is that CASE's failure, not the run's");

    assert_eq!(
        ran.failures,
        vec![project.to_string()],
        "an empty store under --replay-only must fail this one case by name"
    );
    assert!(
        ran.refused.is_empty(),
        "nothing refused: no provider was ever asked"
    );

    // How far it got: the chain creates the phase dir and log BEFORE asking the store, so this
    // existing means everything upstream of the lookup resolved. The old driver never got here.
    let published = paths.output_dir(project).join("translated");
    assert!(
        published.join("logs").is_dir(),
        "the chain must have reached the invocation: {}",
        published.display()
    );
    assert!(
        !paths.output_dir(project).join("verified").exists(),
        "and must not have started the second step after the first could not be served"
    );
}
