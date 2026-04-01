<!-- markdownlint-disable MD041 -->
Translate the C code in c_src/ to Rust that produces **byte-identical output** for the same inputs.
Write Cargo.toml and src/ files in the current directory (NOT in c_src/).

Read the C source to determine the project type and set up Cargo.toml accordingly:
- If the code has a `main()` function → create a `[[bin]]` with `name = "driver"`
- If the code exports library functions (no main, or used as a shared library) →
  add `[lib]` with `crate-type = ["cdylib"]`, and all public C functions must use
  `#[unsafe(no_mangle)]` and `extern "C"` with exact C signatures
  (use `*const c_char`, `c_int`, etc. from `std::ffi`)
- If it's both → include both `[lib]` and `[[bin]]` sections
- If `c_src/CMakeLists.txt` uses cache variables for build-time configurability →
  preserve this using Cargo features. Each CMake variable value becomes a lowercase
  Cargo feature. Use `#[cfg(feature = "...")]` for conditional compilation.
  All feature combinations must compile.

Pay attention to C preprocessor macros that RENAME functions (e.g.,
`#define foo NAMESPACE(foo)` makes the linker symbol `PREFIX_foo`, not `foo`).
The Rust `#[no_mangle]` name must match the FINAL linker symbol, not the
source-level name. Check header files for namespace macros.

Requirements:
- Do NOT fix bugs in the original C code — reproduce behavior exactly
- Preserve the exact order of error checks and validation
- Match C's stdin reading behavior exactly (scanf reads across newlines, fgets does not)
- Match C's exact printf format output including spacing and newlines
- Do NOT use the `openssl` crate or any OpenSSL bindings — use pure-Rust crates instead
- Use safe Rust internally where possible

Run `cargo build --release` and fix any errors until it compiles.
Do NOT modify anything in c_src/.
