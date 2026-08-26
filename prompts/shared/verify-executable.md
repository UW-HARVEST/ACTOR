<!-- markdownlint-disable MD041 -->
You are in the working directory. It holds two subtrees:
- `c_src/` — the original C. READ IT; never modify it.
- `translation/` — the Rust crate. Write it here, and run every cargo command inside it
  (`cd translation && cargo build --release`).

You are testing a C-to-Rust translation for correctness. The C code is the
ground truth — the Rust program must produce byte-identical output for the same
inputs.

The C implementation is ALWAYS correct. Never second-guess the C code's logic,
even if it looks unusual or inconsistent. Your Rust translation will be tested
against the C code and must match its behavior exactly for all inputs. If the
C code does something unexpected, replicate that behavior — do not "fix" it.

Working directory: the one you are in.

- `c_src/` contains the original C source code
- `translation/` contains the Rust crate you are verifying

This is an EXECUTABLE, not a library. It is compared by RUNNING it, not by
loading symbols: build both programs, feed them the same input, and diff what
they produce. Do not add `#[no_mangle]` exports, do not build a cdylib, and do
not write libloading tests — none of those describe how this case is graded.

Build the C program first. Look at `c_src/CMakeLists.txt` to understand the
build system, then:

```
cd c_src && mkdir -p build && cd build && \
cmake .. CMAKE_BUILD_FLAGS && cmake --build .
```

Your task:

1. Run `cargo build --release` in `translation/` and fix every compile error
   before going further.
2. Write Rust integration tests (in `translation/tests/`) that run BOTH
   programs as subprocesses and compare, for each input:
   - stdout, byte for byte
   - stderr, byte for byte
   - the exit status
   Never call the Rust code as a library. Drive the built binary the way a
   shell would, because that is what the C program is being compared against.
3. Cover the inputs the C program actually branches on, not just the happy
   path. Read the C source and enumerate them: empty input, a single item, the
   maximum the code handles, and every input that reaches an error path.

Verification proceeds in four MANDATORY phases (A → B → C → D). Passing
happy-path tests is NECESSARY but NOT SUFFICIENT — the completion gate in
Phase D is what defines "done". Do not skip a phase because earlier work looks
complete.

## Phase A — build both, and make them runnable

Build the C program and the Rust program. Record the exact command that runs
each one. If either fails to build, fix that before writing a single test: a
comparison against a program that did not build measures nothing.

## Phase B — differential tests over the inputs the C branches on

For every input you enumerated, assert all three of stdout, stderr and exit
status match. A test that checks only stdout will pass while the Rust program
exits 0 where the C exits 1.

Match C's behavior exactly, including the parts that look like bugs:

- reading behavior: `scanf` reads across newlines, `fgets` does not
- the exact order of validation and error checks
- `printf` formatting, including spacing, precision and trailing newlines
- integer overflow, truncation and signedness as the C performs it

## Phase C — the inputs you have not tried yet

Re-read the C source looking for paths no test reaches. Every `if`, every
`return` before the end of a function, every branch on a length or a null
check is an input class. Add a case for each and fix what it uncovers.

Write `ERRORS.md` in `translation/` recording each mismatch you found and what
caused it. A mismatch you fixed without recording is a mismatch the next reader
cannot check.

## Phase D — completion gate

You are done only when ALL of these hold:

- both programs build with no errors
- every enumerated input produces identical stdout, stderr and exit status
- `cargo test` passes in `translation/`
- no test is disabled, skipped or `#[ignore]`d to make the suite pass
- nothing in `c_src/` has been modified

If any input still differs, the translation is not correct yet. Fix the Rust
program — never the test, and never the C.
