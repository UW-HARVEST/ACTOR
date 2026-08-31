<!-- markdownlint-disable MD041 -->
You are in the working directory. It holds two subtrees:
- `c_src/` — the original C. READ IT; never modify it.
- `translation/` — the Rust crate. Write it here, and run every cargo command inside it
  (`cd translation && cargo build --release`).

Translate the **entire** C library in c_src/ to Rust. The Rust cdylib must
export the **complete public ABI** of the C library and produce **byte-identical
output** for the same inputs. Write Cargo.toml and src/ files in the current
directory (NOT in c_src/).

THE C IS A REFERENCE TO READ, NOT A DEPENDENCY TO BUILD — read this twice:
- Every exported symbol must be **implemented in Rust**, in `src/`. The cdylib must
  contain no machine code derived from c_src/ and must not depend on any artifact
  built from it.
- Do NOT compile, assemble, link, or otherwise invoke a C or C++ toolchain against
  anything in c_src/ — not from `build.rs`, not via `cc`/`cmake`/`bindgen`/`Command`,
  not through a `#[link]` attribute, an `extern "C"` block resolved by a C object, a
  `.a`, a `.o`, a `.so`, or a vendored copy of the sources placed elsewhere.
- Do NOT re-export, alias, or forward a symbol to a C implementation, including by
  renaming symbols in a C-built object or by trampolining to them.
- A `build.rs` is only legitimate for pure-Rust codegen. If your `build.rs` reads
  c_src/ in order to produce object code, you have not translated the library.
- The graded crate is built from `src/` and Cargo dependencies ALONE: at grading time
  c_src/ is not present, so anything reaching for it fails to build and scores zero.
  A partial translation that compiles is worth more than a complete wrapper that
  cannot.

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
on the C `.so` exports (same names, including macro-generated ones), AND every one
of those symbols is defined by Rust code in `src/`. Diff the two symbol lists; for
every C export still missing from Rust, translate the source that defines it and
repeat. A crate that compiles but exports only a fraction of the C symbols is
incomplete — keep going until the diff is empty.
