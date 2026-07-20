# CRUST-Bench self-generated gap audit

**Question:** ACTOR (Kiro) passes 84/87 CRUST projects with ground-truth tests
(test-repair) but only 52/87 when translating without test access
(self-generated). This audit classifies the 32-project gap to answer: **are the
failing ground-truth tests meaningful — i.e. do they encode what the original C
actually does?**

Method: for each project, compare the failing ground-truth Rust test's asserted
value against the original C test / C implementation. The C behavior is the only
arbiter — test-repair passing a test does NOT prove the test is correct, because
the test-repair agent iterates until the test passes and can conform to a wrong
test.

Data snapshot: 2026-07-18, after the exit-code scoring fix + full re-score +
openssl-contention fix (c_blind_rsa corrected to a PASS). Gap set = 31 projects
(kiro test-repair 84/87 pass, self-generated 53/87 pass → 31 gap).

## RESULT (all 31 gap projects classified)

NOTE: an earlier version of this file (below) tallied 6 build failures as "genuine
ACTOR bugs". That was WRONG — see the "CORRECTION" section: the build failures are
mostly ground-truth-TEST compile defects, and are being re-audited per-project with
the same rigor as the behavioral cases. c_blind_rsa left the gap (now a pass).

Current gap = 31: 25 built-but-test-failed + 5 build-failures + 1 unattempted.

## FINAL TALLY (all 31 classified, each with file:line proof)

| Root cause | Count | Projects |
|--------|-------|----------|
| **BENCHMARK / test-or-interface DEFECT** | **23** | behavioral (19): quadtree, skp, fleur, aes128_SIMD, ljmm, Simple_Config, rhbloom, libvcd, kd3, merkle_tree_c, SimpleXML, mvptree, bostree, emlang, lib2bit, recordManager, dict, Math_Library_in_C, hamta · build-fail (4): clhash, morton, roaring_bitmap, libutf |
| **GENUINE ACTOR bug** | **7** | behavioral (6): lambda_calculus_eval, libbeaufort, kairoCompiler, Megalania, fslib, libpsbt · build-fail (1): cset |
| **Unattempted** | **1** | Remimu |

**Bottom line: 23 of 31 gap projects (74%) are CRUST-Bench defects — the ground-truth
test or interface is broken (asserts non-C behavior, uses an undeclared crate,
references a private field / unimported type, or imposes bounds the interface
forbids) — and ACTOR's translation is faithful to C / the interface. Only 7 are
genuine ACTOR bugs** (translation truly diverges from C, or over-constrains a
generic). Every build "failure" except cset is a ground-truth-TEST compile defect,
NOT an ACTOR compile bug: in each, the agent's own library compiled cleanly (the
rlib built; the agent verified its own build), and only the swapped-in test fails.

Implication: these defects affect ALL systems equally (test-repair agents just
conform to whatever the — possibly wrong — test says). This is a benchmark
data-quality issue in a subset of the counted 87, not an ACTOR-specific problem,
and not a broken CRUST-Bench framework (the harness itself is fine; ACTOR passes
84/87 in test-repair).

**Answer to "are the behavioral test failures meaningful?": mostly NO.** Of the 25
compiled-but-failed cases, **19 (76%) are benchmark test defects** — the RBench
ground-truth test asserts something the original C does not do (inverted
assertions, degenerate harness setup, f64-retyped precision bars, `.abs()`
rewrites, dangling-reference designs, broken fixtures, path typos, etc.), and
ACTOR's translation faithfully reproduces the real C behavior. Only **6 are
genuine ACTOR bugs** where the blind translation diverges from C (verified against
the C source, and in ~9 cases by compiling and running the original C).

The 6 genuine bugs: lambda_calculus_eval (no-op function body), libbeaufort
(missing NULL fallback), kairoCompiler (lexer/parser divergence), Megalania
(stack-allocated 1MB struct → overflow), fslib (`split('\t')` vs C scanf
whitespace), libpsbt (ignores src_size param). These are exactly the class of bug
that test-repair catches by iterating against test feedback — which is *why* the
test-repair setting scores higher, and it is an HONEST advantage (the extra passes
are real fixes), not a benchmark artifact.

Caveat: the 19-vs-6 split is a per-project judgment; a handful of projects had
multiple failing tests with mixed causes (e.g. libpsbt = 2 bugs + 1 defect,
classified "bug" since ≥1 real divergence). See the `bostree` timeout note.

## Categories (by reliable signals: build_ok + ground-truth real_tests)

- **6 build failures** — self-generated translation did not compile. These are
  ACTOR limitations by definition (test-repair would have iterated through the
  build errors); no test-fidelity question:
  c_blind_rsa_signatures, clhash, cset, libutf, morton, roaring_bitmap.
- **1 unattempted** — Remimu (no result.json).
- **25 compiled-but-failed** — the test-fidelity question applies. Audited below.

## Per-project verdicts (25 compiled-but-failed)

<!-- filled from parallel per-project C-vs-Rust-test audits -->

| Project | Failing test | Verdict | Confidence | Reason |
|---------|-------------|---------|-----------|--------|
| quadtree | test_node: `assert!(node.quadtree_node_isleaf())` | BENCHMARK_TEST_DEFECT | high | RBench test INVERTED C's assertions (C: fresh node `!isleaf` test.c:20; Rust asserts isleaf true test.rs:9) and asserts 3 mutually-exclusive predicates at once; ACTOR matches C. |
| lambda_calculus_eval | test_expand_definitions: `assert_eq!(variable.type_, VAR)` (got DEFINITION) | GENUINE_ACTOR_BUG | high | C expand_definitions mutates node in place to VAR (reducer.c:52); Rust test faithful; ACTOR blind wrote empty no-op body (reducer.rs:69-71), node never expanded. |
| libbeaufort | test_encrypt/test_decrypt: panic `index out of bounds` on empty mat | GENUINE_ACTOR_BUG | high | C handles NULL mat via default-tableau fallback (encrypt.c:35-38); Rust test faithful (`&[]`=NULL, same cipher strings); ACTOR omitted fallback, panics on mat[0]. |
| skp | ut_test2: `skptest!(alt==0 && len==0)` (got alt=3) | BENCHMARK_TEST_DEFECT | high | Compiled+ran original C: C ALSO produces alt=3 (its own assert "fails" but C's skptest macro is non-fatal, exits 0); RBench turned soft C assert into hard panic!. ACTOR matches C's alt=3. |
| fleur | test_checking: `assert_eq!(check(not_in),0)` (got 1); +3 fixture I/O errors | BENCHMARK_TEST_DEFECT | high | RBench harness builds degenerate k=0/m=0 filter, never calls initialize (C calls fleur_initialize→k=10); at k=0 both ACTOR & C check() return 1 vacuously. Other 3 fails = fixture-not-in-cwd I/O. |
| kairoCompiler | test_877: `assert!(res==0)` (got non-zero) | GENUINE_ACTOR_BUG | high | Compiled+ran original C: returns 0 (COMPILED_OK) on `5467 abcd $` (compiler.c:58); Rust test faithful; ACTOR blind lexer/parser port returns non-zero — real divergence. |
| aes128_SIMD | test_cipher/test_inv_cipher: expected FIPS-197 vectors | BENCHMARK_TEST_DEFECT | high | Compiled+ran C: outputs [103,165,...] BYTE-IDENTICAL to ACTOR (C key schedule non-standard, keys.c:17-55); C test only checks round-trip, never ciphertext. RBench hard-codes textbook AES the buggy C never produces. ACTOR matches C. |
| ljmm | test_002: harness error `Unrecognized option: 'child'` | BENCHMARK_TEST_DEFECT | high | RBench test re-execs with `--child` to emulate C fork(); libtest rejects the flag → fails regardless of ACTOR. mytest() also fully stubbed (never runs ACTOR code). C uses real fork()/malloc. Broken harness. |
| Simple_Config | run_print_test: `to_string()==expected` (trailing `\n` diff) | BENCHMARK_TEST_DEFECT | high | C cfg_fprint prints trailing `\n` after every entry (config.c:739); C test uses lenient strncmp prefix ignoring it. RBench uses strict `==` vs no-newline constant. ACTOR faithfully emits C's trailing newline. |
| rhbloom | test: panic "bad probability" (ratio 0.202 vs p 0.31) | BENCHMARK_TEST_DEFECT | high | Compiled+ran C: one-sided check `hits/n-p>0.1` PASSES (exit 0, hits=202 identical to ACTOR); RBench rewrote as two-sided `.abs()>0.1`, panics when filter is BETTER than target. ACTOR matches C. |
| Megalania | substring_enumerator_aa_bb_cc_test: stack overflow / SIGABRT | GENUINE_ACTOR_BUG | high | Test faithful (0 substrings for "aa bb cc", matches C). C malloc's the ~1MB struct (2×256×256 arrays) on heap (substring_enumerator.c:56); ACTOR built it on the STACK in new(), overflows 2MB test-thread stack. |
| libvcd | test_ram_vcd: `assert_eq!(date_str, "Fri Jul 15...")` (trailing NULs) | BENCHMARK_TEST_DEFECT | high | C calloc+fscanf leaves 64-byte date NUL-padded (vcd.c:83,135); ACTOR reproduces exact buffer. RBench compares raw [u8;64] via from_utf8_lossy without stripping NULs. C only reads as NUL-terminated. |
| kd3 | test_kd: panic `get_next().unwrap()` on None (search_space case) | BENCHMARK_TEST_DEFECT | high | RBench interface DROPPED the iterator out-param from search_space (`&self`, no iter arg) that C has (kdtree_iterator** ); test validates a stale iter no faithful translation can populate. Structurally impossible. |
| merkle_tree_c | test_rebuild_proof: `assert_eq!(ret,0)` got -3 (indices/lemmas asserts PASSED) | BENCHMARK_TEST_DEFECT | high | Failing assert is downstream cbmt_proof_verify. C test builds 2-elem needed_leaves=[2,7] (test.c:195-211); RBench passes full 5-elem leaves. C's proof_root needs leaves.len==indices.len(2)→both C AND ACTOR return -3 for 5. (Corrects earlier eyeball "genuine".) |
| fslib | test_compile: process exit(1), parse fails on `"0 1 1 1 1.0"` | GENUINE_ACTOR_BUG | high | C sscanf `%zd\t%zd...` matches spaces (\t matches any whitespace run) → parses 5 fields → n_states=2 w/ arc; Rust test faithful. ACTOR used `split('\t')` (never splits spaces) → parse fail → process::exit(1). |
| SimpleXML | test_xml: `parse_xml_from_text(s).unwrap()` panics (Err) | BENCHMARK_TEST_DEFECT | high | Compiled+ran C 3 ways: C trims only ' ' not '\n' → newline input aborts STATE_ERROR (simple_xml.c:313). C test used backslash line-continuation (no newlines). RBench feeds raw-string WITH newlines, asserts success — C rejects it. ACTOR's Err mirrors C. |
| mvptree | test: panic index usize::MAX in tree.add | BENCHMARK_TEST_DEFECT | high | RBench generate_point RE-SEEDS fresh RNG w/ fixed seed every call (testmvp.rs:48)→100 identical points, all dist 0→select_vp leaves s2=-1→points[-1]. C uses monotonic uid+once-seeded rand→distinct points→success. ACTOR's unguarded index mirrors C (mvptree.c:417). |
| bostree | timing: 60s TIMEOUT on 10M-element perf benchmark (debug build) | BENCHMARK_TEST_DEFECT | high | `timing` is a wall-clock perf micro-benchmark (10M inserts, unopt debug), NOT a correctness test; killed by 60s timeout → my exit-code fix recorded fail=1. Real correctness tests (remove_bug×2) PASSED. A correct translation times out too. ⚠️ TIMEOUT-vs-fix interaction — see note below. |
| emlang | test: `assert!(runtime.em.is_err())` (got Ok) | BENCHMARK_TEST_DEFECT | high | RBench harness path typo `resources/test/` vs actual `tests/` (tests.rs:7) + run_program discards load_file error → files load empty → Ok. ACTOR EM_DIV correctly errors (env.rs:130-133=env.c:98-99). Symlink test→tests makes all pass. |
| lib2bit | test: `assert_eq!(expected_lines, output_lines)` (fractions vs raw bytes) | BENCHMARK_TEST_DEFECT | high | `expected` fixture is C's %f doubles (0.080000); RBench interface types twobit_bases→Vec<u8> + prints `{} as f64` per byte — structurally can't emit 6-decimal fractions. ACTOR's bytes LE-decode to exactly 0.08/0.086667 (=C 2bit.c:379). Broken harness. |
| recordManager | testOperators/test_expressions: RC return-code asserts | BENCHMARK_TEST_DEFECT | high | RBench asserts value_equals RETURN CODE = comparison answer; C puts answer in result->v.boolV, returns RC_OK regardless (expr.c:19-21). AND case: C boolAnd never sets result->dt (stays DT_INT)→mismatch err; C test only 'passes' via type-pun UB. ACTOR mirrors C. |
| dict | test5: panic range start 24588 oob (corrupt header) | BENCHMARK_TEST_DEFECT | high | Compiled+ran C: test5.bin is a broken copy of test3.bin (carries test3 ASCII '6012\n' prefix test5.c never strips)→corrupt header. C also errors 'data corrupted'→deserialize NULL→segfault (exit 139). ACTOR panics on same garbage. |
| Math_Library_in_C | castom_test_exp/pow: approx_eq |diff|~1.9e-6 > 1e-6 | BENCHMARK_TEST_DEFECT | high | C castom_exp/pow accumulate in long double (80-bit); meet 1e-6 ABS tol only via that precision. RBench narrowed interface to f64 + finer grid samples large points (exp(21.5)≈2.2e9) below f64 ULP. C suite passes 0 fails. ACTOR f64 = faithful f64 port. |
| hamta | test_big2: SIGSEGV (use-after-free, not assertion) | BENCHMARK_TEST_DEFECT | high | RBench declares key/value as loop-local Box (drop each iter) but map stores &mut refs (interface KeyValue{key:&'a mut T})→dangling. C malloc's persistent keys freed by hamt_destroy (test.c:55-81). Interface forces ACTOR into unsafe raw-ptr. C never exercises UAF. |
| libpsbt | empty_input/encode_decode (bug) + read_test_vector (defect) | GENUINE_ACTOR_BUG | high | 2 fails: ACTOR psbt_read discards _src_size param, uses src.len()=2048 vs logical psbt_len=889→spurious OobWrite; C honors size arg→Ok (psbt.rs:493). 1 fail (read_test_vector) is a defect (persistent *step mistranslated to per-call local). ≥1 genuine → classified bug. |

## ⚠️ Follow-up: timeout-vs-scoring interaction (from bostree)

The exit-code-trust scoring fix (test.rs: nonzero cargo exit → fail=1) correctly
catches crashed/aborted test binaries, BUT it also records a 60s TIMEOUT as a
failure. For bostree the failing "test" is `timing`, a 10M-element wall-clock
performance micro-benchmark (unoptimized debug build) that a CORRECT translation
would also time out on. This is a benchmark-test-defect (a perf benchmark should
not be a pass/fail correctness gate) that the fix currently scores as a fail.
TODO: decide whether to (a) exclude perf-benchmark tests, (b) distinguish
timeout (exit 124) from real test failure in scoring, or (c) leave as-is
(conservative). Check whether any OTHER cell is a pure-timeout on a perf benchmark.

## Build-failure bucket: breakdown (2026-07-18)

The 6 self-generated "build failures" (build_ok=false) are NOT uniform. All 6 agent
runs PROPERLY FINISHED (clean session end, real code produced, no unimplemented!()
stubs) — this is not incompleteness. Breakdown:

- 5 GENUINE ACTOR compile bugs — agent claimed success but wrote uncompilable Rust
  (blind mode has no test to catch it; test-repair would hit the error and fix it):
  - clhash — uses `bytemuck` crate without declaring the dependency (E0433)
  - libutf — references undeclared type `Utf8String` (E0433)
  - morton — undefined variable `z` (E0425); near-empty (loc=29)
  - roaring_bitmap — accesses private field `buffer` (E0616)
  - cset — `Cset<Node>::new` trait bounds unsatisfied (E0599)

- 1 SPURIOUS infra failure — c_blind_rsa_signatures. Its openssl-sys dependency's
  build FAILS UNDER MACHINE LOAD (exit 127 subprocess-spawn failures / perl errors
  when many parallel cargo builds contend). On a QUIET machine, one-at-a-time, it
  reproducibly builds and PASSES 3/0 ground-truth tests (clean rebuild verified
  multiple times). So its build_ok=false is an artifact of scoring under
  contention, not an ACTOR bug. => kiro self-gen should be 53, not 52 (pending a
  clean single-crate re-score).

LESSON: do NOT run multiple full CRUST re-scores in parallel — resource contention
causes spurious build failures (exit 127) on heavy deps like openssl-sys. Re-score
serially, or re-score suspect crates alone on a quiet machine.
| morton (build-fail) | E0425 `cannot find value z` in src/bin/test.rs:32-33 | BENCHMARK_TEST_DEFECT | high | ACTOR's morton.rs compiled clean (rlib built, interface exported); ground-truth TEST references undeclared var `z` (loop only binds n/x/y). No translation can compile it. |
| clhash (build-fail) | E0433 unresolved crate `bytemuck` in src/bin/unit.rs | BENCHMARK_TEST_DEFECT | high | GT test uses external `bytemuck` crate; scaffold Cargo.toml has EMPTY [dependencies], interface doesn't re-export it. ACTOR's lib rlib built clean. No translation compiles it. |
| roaring_bitmap (build-fail) | E0616 field `buffer` private in src/bin/tests.rs:26-29 | BENCHMARK_TEST_DEFECT | high | GT test reads set.buffer directly, but the INTERFACE (interfaces/rset.rs:7) declares buffer private. ACTOR faithfully matched the interface. Test contradicts its own interface. |
| cset (build-fail) | E0599 `Cset<Node>::new` trait bounds unsatisfied | GENUINE_ACTOR_BUG | high | Interface is `impl<T> Cset<T>` (NO bounds, cset.rs:90); test's Node derives only PartialEq/Eq/Hash (interface-conformant). ACTOR OVER-CONSTRAINED to `impl<T: Copy+Default>` (src/cset.rs:272). Faithful translation WOULD compile it. |
| libutf (build-fail) | E0433 undeclared type `Utf8String` in src/bin/test.rs | BENCHMARK_TEST_DEFECT | high | GT test uses `Utf8String::new()` but only imports the MODULE (`use libutf::libutf_string;`), never the type. ACTOR correctly exports `pub struct Utf8String` per interface. Test missing a `use`. |

## CORRECTION: the "build failures" are mostly TEST-compile failures, not ACTOR bugs (2026-07-18)

I initially labeled the 5-6 self-gen build failures "genuine ACTOR compile bugs."
That was WRONG. Checking the agent translation logs: EVERY agent finished cleanly
and verified its OWN build succeeded ("Final clean build verification: Finished",
"compile cleanly", ran its own tests) in its /tmp/harvest-crust-*/project workspace.
The build_ok=false arises when our harness swaps in the GROUND-TRUTH test
(src/bin/*.rs) and rebuilds — and it is the TEST that fails to compile:

- clhash (all 3 agents): ground-truth test src/bin/unit.rs uses `bytemuck::bytes_of`
  but NO Cargo.toml (agent's OR the RBench scaffold's, which ships empty
  [dependencies]) declares bytemuck → E0433. Test can't compile for ANYONE.
- morton (all 3): E0425 in src/bin (test references a value not in scope).
- roaring_bitmap (all 3): E0616 — test accesses a PRIVATE field `buffer` (test
  violates the interface's encapsulation).
- cset (kiro/claude): E0599 — test's `Node` struct lacks Copy/Default that
  `Cset<Node>::new` requires (interface/test bound mismatch).
- libutf (all 3): E0433 in src/bin — test uses `Utf8String::new()` not exported
  under that name (interface/test naming mismatch).

Signature that these are BENCHMARK defects, not ACTOR bugs: the SAME projects fail
IDENTICALLY across kiro AND claude AND codex — independent translations don't all
break the same way unless the shared ground-truth test/interface is the problem.

Agent-side undeclared-crate cases (codex only): coroutine (`corosensei`), expr
(`libm`) — the agent's own src uses a crate it didn't add to Cargo.toml. Gray area
(code likely fine, Cargo.toml incomplete) — a merge_cargo_deps gap, arguably a
harness limitation rather than a translation-correctness bug.

REVISED TAKE: the build-failure bucket is NOT "6 genuine ACTOR bugs." It is
dominated by ground-truth-test compile defects (undeclared deps, private-field
access, missing exports, trait-bound mismatches), same benchmark-defect theme as
the behavioral failures. TODO: fold these into the per-project verdict table with
the same rigor; re-examine whether the 6 "genuine ACTOR bugs" from the behavioral
audit still hold (they were separately verified against C, so likely yes, but the
BUILD-failure reclassification means the overall "genuine ACTOR bug" count is LOWER
than any earlier tally).

## Remimu (the "unattempted") — RESOLVED to a PASS (2026-07-18)

"Unattempted" was kiro-only and NOT a real ACTOR failure. kiro's Remimu was
translated fine (agent finished, 9m56s; translate/src/{my_regex.rs,lib.rs} exist),
but its verify/ workspace was never populated (only logs/) → no result.json → scored
non-pass. Harness copy-gap, not an ACTOR miss. Every OTHER ACTOR variant passes
Remimu (claude/codex/kiro-translate self-gen: 1/0; all 3 test-repair: 1/0).
Reconstructed kiro's verify (translate src + ground-truth test) and it builds+passes
1/0 — verified clean run. Corrected verify/result.json accordingly.

=> kiro self-gen 53 -> 54/87. Gap now 30 (not 31): 25 built-but-test-failed +
   4 build-fail-defects + 1 build-fail-bug(cset). Remimu removed from gap.
=> BOTH earlier "non-test" gap members (c_blind_rsa, Remimu) turned out to be
   harness/environment artifacts, not ACTOR failures.
