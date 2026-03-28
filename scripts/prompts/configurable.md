<!-- markdownlint-disable MD041 -->
Translate the C code in c_src/ to Rust that produces **byte-identical output** for the same inputs.
Write Cargo.toml and src/ files in the current directory (NOT in c_src/).

This project has **build-time configurability** via CMake cache variables.
Look at c_src/CMakeLists.txt — it uses variables to select which source files
to compile and which parameter headers to include at build time.

You MUST preserve this configurability using **Cargo features**. Each CMake cache
variable value becomes a Cargo feature, using the **exact same name in lowercase**.
Use `#[cfg(feature = "...")]` to conditionally compile modules and set constants.
All combinations of features must compile.

This project produces BOTH a shared library AND a binary executable.
Your Cargo.toml must have both `[lib]` with `crate-type = ["cdylib"]` and
`[[bin]]` with `name = "driver"` and `path = "src/main.rs"`.

**This is a large project.** Do NOT try to translate everything yourself in one go.
Instead:
1. Analyze the C project structure and create a plan (TODO list) breaking the
   translation into subtasks (e.g., core/shared code, each backend, entry points)
2. For each subtask, invoke a subagent to do the translation by running:
   ```
   kiro-cli chat --no-interactive --trust-all-tools \
     '<detailed prompt for this subtask>' \
     < /dev/null
   ```
3. After each subagent completes, verify the work compiles before moving on
4. Once all subtasks are done, wire up the feature gates and verify the full build

Each subagent should work in the same directory and add to the existing code.
Give each subagent a clear, focused prompt with the specific C files to translate
and where to put the Rust output. Each subagent prompt MUST include:
- Which specific C source files to translate
- Which Rust file(s) to write
- Instructions to build and verify its own work compiles with the relevant features
- Instructions to NOT modify any files outside its scope

After all subagents complete, wire up the feature gates and do a final build check.
If a combination fails, only fix the glue code (lib.rs, mod declarations) — do NOT
modify the backend implementation files.

Requirements:
- Do NOT use the `openssl` crate or any OpenSSL bindings. Use pure-Rust crates
  instead (e.g., `aes` for AES-256-ECB, `sha2` for SHA-256)
- All public C functions must use #[unsafe(no_mangle)] and extern "C"
- Pay attention to C preprocessor macros that RENAME functions (e.g.,
  `#define foo NAMESPACE(foo)` makes the linker symbol `PREFIX_foo`, not `foo`).
  The Rust #[no_mangle] name must match the FINAL linker symbol, not the
  source-level name. Check header files for namespace macros.
- Preserve the exact C function signatures (use *const c_char, c_int, etc. from std::ffi)
- Do NOT fix bugs in the original C code — reproduce behavior exactly
- Use safe Rust internally where possible

Do NOT modify anything in c_src/.
