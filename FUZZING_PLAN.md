# Fuzzing integration plan (harvest-bench verify)

## Goal
Make the verifier catch the value-dependent / adversarial-input bugs that
hand-written differential tests miss (lz4 `HC_destSize` tight-budget, pcre2
`{0,n}` codegen, libpng transform/gamma). The verifier's blind spot is
input-generation, not checking: it writes a few fixed inputs; the benchmark
uses fuzzed/adversarial ones. Fix = coverage-guided differential fuzzing, with
a mechanical gate that EVERY public function was actually fuzzed-and-compared.

## Anti-overfit invariant (non-negotiable)
Everything is derived from the C source; we supply METHOD + GATE, never inputs,
domains, or which functions matter. The gate checks API COVERAGE ("fuzz
everything"), never specific values ("fuzz the gamma path"). Zero
benchmark-specific content — same discipline as CONFIGS.md/ERRORS.md.

## Approach (adopt Haoran's design; instrument the C side)
Google FuzzTest on top of gtest. Differential property:
`EXPECT_EQ(RunC(args), RunRust(args))` fuzzed over typed domains.
- **C reference** compiled STATIC + coverage-instrumented → coverage guidance
  steers by the SPEC (correct: finds paths C handles that Rust doesn't).
- **Rust cdylib** dlopen'd as a black box (`rust_lib.h`, RTLD_LOCAL) → no symbol
  collision; every generated input runs both sides, any mismatch/crash = gtest fail.
- Rationale for NOT cargo-fuzz: instrumenting Rust (the buggy side) under-explores
  exactly the divergent regions; instrumenting the C spec is the right target.
  Also both sides are C-ABI .so's called from C++/gtest — C++ is the neutral caller.

## Division of labor (LOCKED)
- **We pre-build the `verify_env/` framework** (Haoran's way): a working CMake +
  dlopen shim + differential helpers, so the fiddly coverage-instrumentation
  plumbing (C compiled static + instrumented, Rust dlopen'd RTLD_LOCAL, Clang +
  FUZZTEST_FUZZING_MODE) is GUARANTEED correct. The agent only writes
  `FUZZ_TEST` properties + domains into `verification_tests.cc`.
- **Because we own the CMake, WE must handle the C-source layout** across all
  libs: `GLOB_RECURSE` c_src + mirror each project's own
  `test_case/CMakeLists.txt` include dirs/compile-defs (that build is known-good
  — it produced the full-ABI .so). Flat `src/*.c` (Haoran's) misses libsodium's
  145 files in 94 dirs.
- **The completeness gate is ALWAYS ours + mechanical + coverage-based** — the
  check ON the agent, never the agent's job. Reads `nm -D` + llvm-cov; agnostic
  to how the env was built.

## Parts

### F1 — verify_env fuzz template (materialized into the verify workspace)
Adapt Haoran's `verify_env_template/`: CMakeLists (FuzzTest+gtest FetchContent;
C-under-test static lib with coverage in fuzzing mode), `rust_lib.h` (dlopen
shim), `harvest_diff.h`, `build.sh` (unit) + `build_fuzz.sh` (Clang,
`-DFUZZTEST_FUZZING_MODE=ON`), vendored FuzzTest docs. Rust side = our
`translated/` (or `verified/`) crate's `.so`.

### F2 — verify.md fuzz phase + FUZZ.md manifest
- Default: wherever an API has an input dimension, a `FUZZ_TEST` over it (not
  fixed values). Domains derived from the C (`InRange`/`ElementOf`/`VectorOf`;
  never fuzz raw-int-cast enums or pointer+len separately).
- Running discipline: "test binary passed ≠ property was fuzzed"; run real
  campaigns (`--fuzz_for`), confirm Edges/Total runs climb, run to plateau;
  save reproducers as fixed regression TESTs.
- `FUZZ.md`: one row per public symbol → covering FUZZ_TEST property → campaign
  evidence (edges/runs). Auditable, agent-authored.

### F3 — coverage-based completeness gate (THE CRUX, LLM-independent)
1. `S` = `nm -D` public function symbols of the C `.so` (we already compute this).
2. Run the fuzz campaigns in fuzzing mode with `llvm-cov` on the instrumented C.
3. `covered` = C public functions actually EXECUTED during fuzzing (llvm-cov ∩ S).
4. GATE: `S − covered` must be empty. Non-empty ⇒ print the "left behind" list,
   verify is NOT complete (loop/fail, like the compile-gate). A function never
   executed under fuzzing cannot have been differentially compared.
This is the fuzz-completeness twin of the translate symbol-parity gate.

### F4 — wire into verify_case
After the agent's verify run + existing compile-gate: run campaigns + F3 gate.
Preflight clang/llvm-cov; per-property time budget (`--fuzz_for`); overall cap
so a huge-ABI lib (pcre2 112, libsodium 881 symbols) can't run unbounded.

## STATUS: F1–F4 CODE COMPLETE + validated on real data (2026-08-05)

Merged Haoran's fuzz framework, improving it. Key change vs his design: instead
of his flat `file(GLOB c_src/src/*.c)` + `parse_c_compile_defs`, the verify_env
CMake does **`add_subdirectory(../c_src)`** to reuse each project's OWN
known-good CMakeLists (the exact build that made the full-ABI .so). This is why
it works uniformly — his glob finds ZERO files in libsodium (145 files/108
nested dirs) and can't recover libpng's `find_package(ZLIB)` link; ours inherits
all of it via the linked target's PUBLIC deps. We parse only the `add_library()`
target name.

- **F1** `tools/src/verify_env.rs` — `materialize(crate_root, fuzz)` fills the
  CMake placeholders (FuzzTest FetchContent pinned `2026-06-29`, link_fuzztest,
  fuzz flags) + writes the 12 template files. In fuzz mode ALSO injects
  `-fprofile-instr-generate -fcoverage-mapping` on the C build (coexists with
  FuzzTest's sancov; the former is what llvm-cov reads for the gate).
  VALIDATED: libpng (needs zlib) AND libsodium (108 dirs) both configure+build
  in fuzz mode; C .so carries `__llvm_covmap`; 6 unit tests.
- **F2** `prompts/claude/verify.md` — fuzz phase GATED on `verify_env/` existing
  (Test-Corpus has none → inert there). Reuses SYMBOLS/CONFIGS/ERRORS manifests;
  a FUZZ_TEST supersedes the Phase-B random loop. Adds `FUZZ.md` manifest + a
  Phase-D completion checkbox. Zero library-specific leakage (grep-verified).
- **F3** `tools/src/fuzz_gate.rs` — the crux. `S`=nm -D public FUNC symbols;
  `covered`=`llvm-cov export -object <C.so>` funcs with count>0; `left_behind`=
  S−covered (restricted to public). `GateReport::passed()` = measured && empty.
  VALIDATED on libpng: 381 symbols, 1 covered by a 1-property probe, 380 left
  behind → correctly FAILS (proves it detects un-fuzzed functions).
- **F4** wired into `verify.rs::verify_case(.., fuzz_case)`: HB library cases
  materialize verify_env/ into the agent workspace BEFORE the run. The AGENT
  runs its own coverage-guided campaigns (to plateau, its judgment); the C
  reference is built with `-fprofile-instr-generate=<ve>/cov/cov-%m.profraw`
  (pooled, CWD-independent, baked at build time), so those campaigns accumulate
  coverage automatically — no gate-run campaign, NO fixed duration. After the
  agent, `fuzz_gate::measure_existing(verify_env)` merges the pooled profiles,
  runs `llvm-cov`, evaluates S−covered, writes `logs/FUZZ_GATE.md` + `fuzz_gate`
  field in verification.json. Measured BEFORE work.finish() (profiles live in
  the temp dir); heavy build-fuzz/_deps stripped so verified/ stays lean.
  MEASURES + RECORDS; never discards verified/.
  DESIGN CHOICE (user): agent decides ALL campaign durations; gate only MEASURES
  what the agent's campaigns covered — no magic number anywhere. Controlled by
  ONE proper CLI flag (NOT env vars): `--no-fuzz` skips the whole fuzz phase
  (default: ON for HB library cases, mirroring `--no-verify`). Threaded
  CLI → Benchmark::verify → verify_case as `bool`. Env preflight passed:
  clang 15, llvm-cov, llvm-profdata, cmake 3.28, ninja, GitHub reachable.
  VALIDATED: baked cov-%m path pools coverage from an agent-style run with NO
  env var, from any CWD; measure_existing reads it.

43 lib + 43 bin tests pass; release build clean. verify_env_template/ files are
editable-then-embedded via include_str! from src/verify_env.rs.

## Honest caveats
- Un-fork: Haoran's fuzz framework is on `haoran-agent-analysis`, meant to PR
  into canonical ACTOR. We build now, RECONCILE/dedupe with his during un-fork
  (user's explicit call) — do not treat as permanently separate.
- Cost: FuzzTest toolchain (Clang + vendored deps) + campaigns to plateau are
  SLOW; full-ABI fuzz gate on 100s of symbols is potentially many hours. The
  gate is right; budget accordingly (time-box per property, parallelize).
- Coverage gate needs per-function llvm-cov extraction filtered to S; if the C
  build can't be cleanly coverage-instrumented for a given lib, fall back to
  the manifest gate (every S symbol has a FUZZ.md row + a real campaign log)
  and LOG the downgrade (no silent weakening).
