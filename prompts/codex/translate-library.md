<!-- markdownlint-disable MD041 -->
Translate the **entire** C library in c_src/ to Rust. The Rust cdylib must
export the **complete public ABI** of the C library and produce **byte-identical
output** for the same inputs. Write Cargo.toml and src/ files in the current
directory (NOT in c_src/).

SCOPE — read before you start:
- The WHOLE library in c_src/ is under test, not a subset. The C build globs
  ALL of c_src/ into one shared library; you must reproduce every public symbol
  it exports. Do NOT pick "the interesting" or "the most likely-tested" module
  and translate only that — translating a self-contained corner and verifying it
  perfectly is NOT a success; it is a failed translation of the library.
- First, enumerate the full surface: build the C library, run `nm -D` on the
  resulting C `.so`, and list EVERY exported public symbol (also scan the public
  headers). That list is your definition of done. A large library (dozens of
  files, hundreds of symbols) is expected — cover all of it.

This is a LIBRARY. Requirements:
- Cargo.toml must have crate-type = ["cdylib"] under [lib]
- All public C functions must use #[unsafe(no_mangle)] and extern "C"
- Pay attention to C preprocessor macros that RENAME functions (e.g.,
  `#define foo NAMESPACE(foo)` makes the linker symbol `PREFIX_foo`, not `foo`).
  The Rust #[no_mangle] name must match the FINAL linker symbol, not the
  source-level name. Check header files for namespace macros.
- Preserve the exact C function signatures (use *const c_char, c_int, etc. from std::ffi)
- Do NOT fix bugs in the original C code — if the C has incorrect behavior, reproduce it exactly
- Preserve the exact order of error checks and validation
- Use safe Rust internally where possible

Run 'cargo build --release' and fix any errors until it compiles.
Do NOT modify anything in c_src/.

COMPLETION GATE — you are NOT done when the crate merely compiles. You are done
only when `nm -D` on your Rust `.so` exports EVERY public symbol that `nm -D`
on the C `.so` exports (same names, including macro-generated ones). Diff the
two symbol lists; for every C export still missing from Rust, translate the
source that defines it and repeat. A crate that compiles but exports only a
fraction of the C symbols is incomplete — keep going until the diff is empty.

## Self-verification protocol (follow exactly)
1. You work in ONE session. There is no Task tool and there are no sub-agents to
   delegate to, so do the work yourself in this turn rather than describing what a
   helper should do.
2. After EVERY step that is supposed to produce a file, INDEPENDENTLY verify the
   actual output with your own shell commands (ls, wc -l, grep -c). NEVER report
   success from your own narration alone.
3. If verification shows missing or incomplete output, finish it now. If a file is
   too large to handle in one pass, split it into smaller function-range chunks and
   work through them one at a time, verifying each on disk as you go.
4. Your turn is NOT complete until every required artifact exists and has passed
   your own verification. Do not end your turn with unverified or pending work.
