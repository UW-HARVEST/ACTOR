<!-- markdownlint-disable MD041 -->
Translate the C code in c_src/ to Rust that produces **byte-identical output** for the same inputs.
Write Cargo.toml and src/ files in the current directory (NOT in c_src/).

## Step 1: Analyze BEFORE writing any code

Before writing a single line of Rust, you MUST:
1. Read `c_src/CMakeLists.txt` to understand the build system, source file selection,
   and any build-time configurability (cache variables, options, conditional compilation)
2. Read all header files to understand the public API, preprocessor macros, and
   namespace/symbol renaming patterns
3. Determine the project type:
   - Has `main()` → needs `[[bin]]` with `name = "driver"`
   - Exports library functions → needs `[lib]` with `crate-type = ["cdylib"]`
   - Both → include both `[lib]` and `[[bin]]` sections
4. Identify ALL backends/variants if the project has build-time configurability

## Step 2: Plan the translation

If the project has build-time configurability (CMake cache variables selecting different
source files or parameters):
- You MUST preserve this using **Cargo features**. Each CMake cache variable value
  becomes a Cargo feature using the **exact same name in lowercase**.
- Use `#[cfg(feature = "...")]` to conditionally compile modules and set constants.
- ALL combinations of features must compile.
- Plan which source files map to which features before writing code.
- Do NOT hardcode a single configuration — every variant must be implemented.

For large projects, break the work into phases: shared/core code first, then each
backend or variant, then wire up feature gates.

## Step 3: Translate

- All public C functions must use `#[unsafe(no_mangle)]` and `extern "C"` with exact
  C signatures (use `*const c_char`, `c_int`, etc. from `std::ffi`)
- Pay attention to C preprocessor macros that RENAME functions (e.g.,
  `#define foo NAMESPACE(foo)` makes the linker symbol `PREFIX_foo`, not `foo`).
  The Rust `#[no_mangle]` name must match the FINAL linker symbol, not the
  source-level name.
- Do NOT fix bugs in the original C code — reproduce behavior exactly
- Preserve the exact order of error checks and validation
- Match C's stdin reading behavior exactly (scanf reads across newlines, fgets does not)
- Match C's exact printf format output including spacing and newlines
- Do NOT use the `openssl` crate or any OpenSSL bindings — use pure-Rust crates instead
- Use safe Rust internally where possible

## Step 4: Verify

Run `cargo build --release` and fix any errors until it compiles.
If the project has Cargo features, verify ALL feature combinations compile:
run `cargo build --release --features <feature>` for each one.

Do NOT modify anything in c_src/.
