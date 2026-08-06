<!-- markdownlint-disable MD041 -->
You are testing a C-to-Rust translation for correctness. The C code is the
ground truth — the Rust code must produce byte-identical results.

The C implementation is ALWAYS correct. Never second-guess the C code's logic,
even if it looks unusual or inconsistent. Your Rust translation will be tested
against the C code and must match its behavior exactly for all inputs. If the
C code does something unexpected, replicate that behavior — do not "fix" it.

Working directory: CASE_DIR_PLACEHOLDER

- `translated_rust/c_src/` contains the original C source code
- `translated_rust/src/` contains the Rust translation
- The C code can be compiled as a shared library. Look at c_src/CMakeLists.txt
  to understand the build system. Build it with:
  ```
  cd translated_rust/c_src && mkdir -p build && cd build && \
  cmake .. -DCMAKE_POSITION_INDEPENDENT_CODE=ON CMAKE_BUILD_FLAGS && \
  cmake --build .
  ```
- Find the resulting .so files in the build output

Your task:
1. Read Cargo.toml [features] and c_src/CMakeLists.txt to understand all
   build-time configurations. Enumerate every valid feature combination.
2. Run `cargo check --no-default-features --features <combo>` for EVERY
   combination. Fix all compile errors before proceeding. Modules or code
   that only apply to certain backends must use `#[cfg(feature = "...")]`.
3. Build the C code as a shared library for the default configuration.
4. Write Rust integration tests (in translated_rust/tests/) that use
   `libloading` to load BOTH the C .so AND the Rust .so, and compare their
   outputs through the FFI boundary. Never call Rust functions directly —
   always load the Rust .so via libloading and call its exported symbols,
   exactly as an external caller would. This tests the `#[no_mangle]`
   export wrappers too.

Verification proceeds in four MANDATORY phases (A → B → C → D). Matching
symbols and passing happy-path tests are NECESSARY but NOT SUFFICIENT — the
completion gate in Phase D is what defines "done". Do not skip a phase because
earlier work looks complete.

### Phase A — Map the surface (produce artifacts BEFORE writing tests)

Create THREE files in the crate root; all derived mechanically from the C
source, not from assumptions or from what looks "important":

1. `SYMBOLS.md` — every public symbol from `nm -D` on the C `.so`. Every one
   MUST also be exported by the Rust `.so` (exact name, incl. macro-generated
   ones). For each symbol MISSING from the Rust `.so`, first determine WHY:
   - If the implementation exists in Rust but isn't exported, add the
     `#[no_mangle]`/`extern "C"` wrapper.
   - If the implementation is ABSENT because that C source was never translated
     (a whole module/file is missing), TRANSLATE the missing C source now. This
     is a real completeness failure — the prior translate step covered only a
     subset of the library. Do NOT stub, fake, or `unimplemented!()` a symbol
     just to make it appear in `nm -D`; a stub that lies about behavior is worse
     than a missing symbol. Translate the actual C.
2. `ERRORS.md` — the ERROR-SURFACE TABLE. This is the anti-blind-spot step.
   Mechanically grep the C source for EVERY distinct way it rejects or errors
   on input — every error-return macro/statement (`RETURN_ERROR`, `return -1`,
   `return NULL`, error enums), every `assert`, every explicit range check,
   null check, and min/max constant. Write ONE ROW per distinct rejection:

   | # | function | trigger (the exact invalid input/condition) | expected C result |
   |---|----------|----------------------------------------------|-------------------|

   Derive rows from what the C ACTUALLY checks — do not invent, guess, or rely
   on happy-path docs. Three distinct `RETURN_ERROR` branches = three rows.
3. `CONFIGS.md` — the CONFIGURATION-SURFACE TABLE. This is the anti-blind-spot
   step for VALID inputs (the mirror of `ERRORS.md`). A library's bugs hide in
   the INTERACTION of options and data shapes, not in calling one function once
   with one input. Mechanically enumerate the axes the C code actually branches
   on — derive them from the source, the same way `ERRORS.md` is derived:
   - every runtime option/mode/flag the public API can set, and the state each
     toggles (grep the public headers + the `if` / `switch` / `#ifdef` branches
     the C takes on those flags);
   - every distinct input SHAPE the code special-cases (sizes, widths, element
     types, counts, formats, byte order, empty / one / many, boundary values);
   - the FULL set of public entry points, INCLUDING the lowest-level ones — not
     just the convenience / one-shot / simplified wrappers.
   Write ONE ROW per meaningful COMBINATION of these axes that the C treats
   differently (their cross-product, pruned to the combinations the code
   actually distinguishes). Derive rows from what the C branches on — do not
   guess which configurations "matter," and do not restrict to the simplest API.

   | # | entry point(s) | configuration (options set + input shape) | [ ] |
   |---|----------------|--------------------------------------------|-----|

### Phase B — Valid-path differential tests (GATED on `CONFIGS.md`)

Start with the lowest-level functions and work upward, using the C headers to
identify the public API and call hierarchy. Drive the library the way a real
consumer does — set up state, apply the options, run the full operation end to
end — exercising the LOW-LEVEL entry points directly, not only the convenience
wrappers (bugs in the composed pipeline are invisible to per-wrapper tests).

For EVERY ROW in `CONFIGS.md`: call BOTH C and Rust via their `.so` exports in
that configuration and assert outputs match byte-for-byte. Use MANY randomized
inputs per row (property-style, with a fixed seed for reproducibility), not a
single hand-picked value — one scalar input hits only one code path and misses
value-dependent and out-of-range-index bugs. Fix the Rust (never the C) on any
divergence; check a row off only once it passes across the randomized inputs.
This is necessary but only half the job.

### Phase C — Error-path differential tests (GATED on `ERRORS.md`)

For EVERY ROW in `ERRORS.md`, write a differential test that constructs that
exact invalid input/condition, calls BOTH C and Rust, and asserts they return
the SAME error/rejection (the same error code or sentinel — not merely "both
failed somehow"). Check the row off only when its test passes against both.
Also cover the generic boundaries every C API has even if not in the table:
null pointers, zero and oversized lengths, and values one step past a
documented valid range — INCLUDING out-of-range enum values passed across the
FFI boundary (C enums accept any int, so a value with no valid variant is a
real input the C handles and the Rust must handle identically; this is exactly
the class of bug that happy-path tests miss). You MAY NOT proceed to Phase D
while any `ERRORS.md` row is unchecked.

### Coverage-driven differential property testing — ONLY when `verify_env/` is present

Check for a `verify_env/` directory in the crate root. If it does NOT exist,
skip this section entirely (the libloading tests above are the whole job). If it
DOES exist, this is a library case pre-wired for **differential property
testing** — the PRIMARY differential harness, and where you catch the
value-dependent bugs a fixed-seed random loop (Phase B) systematically misses.
Read `verify_env/README.md`.

The harness is a small pure-Rust crate at `verify_env/difftest/` (`proptest` +
`libloading`). `verify_env/build.sh` builds three things: the C reference `.so`
(coverage-instrumented, via the project's own `c_src` build), your translated
Rust `.so`, and the `difftest` binary. Both `.so`s are loaded as black boxes and
called through their identical C-ABI symbols; each property generates many
varied + edge-biased inputs and asserts the two sides agree. A property lives in
`verify_env/difftest/src/main.rs`:

```rust
// C is ground truth; both fns resolved by the SAME symbol name from each .so.
type GetU32 = unsafe extern "C" fn(*const u8) -> u32;
differential!("png_get_uint_32", b"png_get_uint_32", GetU32,
    proptest::collection::vec(any::<u8>(), 4..=4),
    |cf: GetU32, rf: GetU32, buf: &Vec<u8>| unsafe { (cf(buf.as_ptr()), rf(buf.as_ptr())) });
```

Cover EVERY public symbol (the same `nm -D` set as `SYMBOLS.md`). Write one
property per function (or family), driving it the way a real consumer does —
set up state, apply options, run the whole operation — and exercise the
LOW-LEVEL entry points directly, not just the convenience wrappers (bugs in the
composed pipeline are invisible to per-wrapper tests). Wherever an API has an
input dimension — the primary payload, and any scalar/enum/flag that steers
behavior (the axes you enumerated in `CONFIGS.md`) — make it a proptest input,
not a few literals. Derive each domain from what the C treats as legal
(`any::<T>()`, `0..=N`, `prop::collection::vec(any::<u8>(), 0..N)`,
`prop::sample::select(...)` for enums); never generate a raw int then cast it to
an enum, and never generate a pointer and a length independently (generate a
`Vec`, pass `.as_ptr()`/`.len()`). Fold the `ERRORS.md` rows in too — include the
invalid values in the domain, or add a fixed `#[test]` for the exact condition.
proptest is edge-biased (it already tries 0, MAX, empty, boundaries), so a plain
`any::<T>()` or a byte-vector payload usually surfaces value-dependent bugs.

Running discipline — a property is NOT exercised until you actually run it.
Iterate: add properties → `./verify_env/build.sh` (rebuilds are incremental,
seconds) → run the harness (build.sh prints the exact `C_SO=… RUST_SO=…
./difftest/target/release/difftest` command) → fix any divergence → re-run. Use
enough cases per property to explore the space (the harness default is 2000; set
`DIFFTEST_CASES` higher for the properties covering the most behavior). When a
property finds a mismatch, proptest prints the minimized failing input — pin it
as a fixed regression `#[test]`, fix the Rust (never the C), then re-run.

Your runs are what get MEASURED. The C reference is built so that every run of
the harness records which C functions it executed; the completeness gate reads
exactly that coverage (not your `FUZZ.md` claims) and fails any public symbol
your properties never actually exercised. A property that compiles but is never
run counts as un-covered — run the harness after adding properties, and keep
adding until every public symbol is covered.

C is the ground-truth side: if the C reference itself crashes on an input, that
is a reference-side issue (record it; do not conclude the Rust is wrong). If C
completes (success or a normal error) and Rust diverges or crashes, that is a
translation bug — fix the Rust.

`FUZZ.md` — the COVERAGE manifest. One row per public symbol (the `SYMBOLS.md` /
`nm -D` set), mapping it to the property that exercises it:

| # | public symbol | covering property (label) | exercised (Y/N) |
|---|---------------|---------------------------|-----------------|

Every symbol MUST have a row backed by a property the harness actually ran. This
manifest is checked MECHANICALLY after your run against the coverage the harness
produced, and any symbol left behind fails verification. Do not pad the table: a
row whose property never covered that symbol's code counts as left behind.

### Phase D — Symbol parity, feature combos, and completion gate

- Compare `nm -D` on the C .so and the Rust .so. Every symbol the C .so
  exports, the Rust .so must also export with the exact same name (incl. macro-
  generated). For each missing symbol, apply the Phase A rule: add the export
  if the impl exists, or TRANSLATE the missing C source if a whole module was
  skipped. The symbol diff MUST reach empty — a Rust `.so` that exports only a
  fraction of the C `.so`'s symbols means the library is only partially
  translated and verification is NOT complete, no matter how well the present
  subset passes its tests.
- Repeat Phases B–C for EVERY feature combination from Phase A. Switch features
  with `cargo test --no-default-features --features <combo>`. Each combination
  may exercise completely different code paths.

Verification is complete ONLY when ALL of these hold (re-read before declaring
done; do not stop early just because symbols match or happy-path tests pass):

- [ ] `SYMBOLS.md`: `nm -D` shows 0 missing/undefined non-libc symbols in Rust.
- [ ] Phase B: EVERY row in `CONFIGS.md` passes across randomized inputs.
- [ ] Phase C: EVERY row in `ERRORS.md` has a passing error-path differential test.
- [ ] If `verify_env/` exists: `FUZZ.md` has a row for EVERY public symbol, each
      backed by a differential property the harness actually ran and that covered
      that symbol. No symbol left behind — this is checked mechanically against
      the coverage the harness produced.
- [ ] All of the above hold under EVERY feature combination — this code is
      shared across ALL configurations, not just the default.

**Tip:** Write shell loops or scripts to automate repetitive work. For example,
to check all feature combinations: extract them from Cargo.toml, loop over them,
and run `cargo check` for each. Same for running tests across combinations.
Do not manually repeat commands for each configuration — automate it.

**This may be a large verification task.** If the project has more than one
configuration or code path to verify, break the work into focused subtasks —
do NOT try to verify everything in a single session. Create a plan, then
work through each subtask with a focused scope covering a specific subset of
the code or functionality to verify and fix. After each subtask completes,
check that its fixes didn't break anything else.

Add `libloading = "0.8"` to [dev-dependencies] in translated_rust/Cargo.toml.
Do NOT modify anything in c_src/.

IMPORTANT: If a file is too large to write in one tool call, build it up
piece by piece using multiple smaller writes (create then append).

IMPORTANT: Use timeouts for all commands. No single build or test command should
run longer than 600 seconds. If a test takes too long, skip it and move on to
the next function. Use `timeout 600 cargo test ...` or similar. Do not get stuck
on any single step.

## Sub-agent protocol (follow exactly)
1. The Task tool is SYNCHRONOUS. A Task call returns ONLY when the sub-agent has
   FINISHED, and the sub-agent's final report IS the call's return value. There
   are NO asynchronous "completion notifications." NEVER say you are "waiting
   for", "pausing for", or "will be notified by" a sub-agent — the instant the
   Task call returns, its work is already done and its result is in your hands.
2. After EVERY sub-agent returns, INDEPENDENTLY verify its actual output with
   your own Bash/Read commands (ls, wc -l, grep -c). NEVER report success from a
   sub-agent's self-report alone — sub-agents sometimes claim work they did not
   finish.
3. If verification shows missing/incomplete output, either re-dispatch a sub-agent
   for JUST that gap (split large files into smaller function-range chunks so each
   sub-agent's job fits comfortably in one turn) or complete it yourself.
4. Your turn is NOT complete until every required artifact exists and has passed
   your own verification. Do not end your turn with unverified or pending work.
5. Prefer synchronous, one-at-a-time delegation you can verify over "fire many
   and wait." If you spawn several Task calls, remember each already returned its
   result by the time you read this — go verify each on disk now.
