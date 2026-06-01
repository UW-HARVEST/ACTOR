<!-- markdownlint-disable MD041 -->
Translate the C code in c_src/ to Rust that produces **byte-identical output** for the same inputs.
Write Cargo.toml and src/ files in the current directory (NOT in c_src/).

You MUST translate ALL C source files — no stubs, no placeholders, no empty
functions. Every .c file MUST have a complete Rust equivalent. The binary MUST
produce the same stdout as the C binary for the same inputs.

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
2. The binary driver (main.rs) MUST be one of the subtasks — do not leave it for last.
   Translate it fully, not as a stub.
3. Work through the subtasks one at a time, with a clear, focused scope for each:
   - Which specific C source files to translate
   - Which Rust file(s) to write
   - Build and verify each subtask compiles with the relevant features
   - Do NOT modify files outside the current subtask's scope
4. After each subtask completes, verify the work compiles before moving on
5. Once all subtasks are done, wire up the feature gates and verify the full build

After all subtasks complete, wire up the feature gates and do a final build check.
If a combination fails, only fix the glue code (lib.rs, mod declarations) — do NOT
modify the backend implementation files.

Translation requirements:
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

Once translation compiles for all feature combinations, verify it matches the
C behavior via FFI testing:
1. Read Cargo.toml [features] and c_src/CMakeLists.txt to enumerate every
   valid feature combination.
2. Run `cargo check --no-default-features --features <combo>` for EVERY
   combination. Fix all compile errors before proceeding.
3. Build the C as a shared library for the default configuration:
   ```
   cd c_src && mkdir -p build && cd build && \
   cmake .. -DCMAKE_POSITION_INDEPENDENT_CODE=ON CMAKE_BUILD_FLAGS && \
   cmake --build . && cd ../..
   ```
4. Add `libloading = "0.8"` to [dev-dependencies].
5. Write integration tests in tests/ that use `libloading` to load BOTH the C
   .so AND the Rust .so, and compare outputs through the FFI boundary.
   Never call Rust functions directly — always load the Rust .so via libloading.
6. For each public function: create test inputs, call both C and Rust via
   their .so exports, assert outputs match byte-for-byte.
7. Run `cargo test` and fix any mismatches. The C is ground truth.
8. Compare `nm -D` on the C .so and the Rust .so. Every symbol the C .so
   exports, the Rust .so must also export with the exact same name (including
   symbols created by preprocessor macros). Add missing exports.
9. Repeat steps 5-8 for EVERY feature combination from step 1. Switch features
   with `cargo test --no-default-features --features <combo>`. Each combination
   may exercise completely different code paths.
10. Do not declare success until every function matches under every feature
    combination.

**Tip:** Write shell loops or scripts to automate repetitive work. For example,
to check all feature combinations: extract them from Cargo.toml, loop over them,
and run `cargo check` for each. Same for running tests across combinations.
Do not manually repeat commands for each configuration — automate it.

IMPORTANT: Use timeouts for all commands. No single build or test command should
run longer than 600 seconds. Use `timeout 600 cargo test ...` or similar.

Do NOT modify anything in c_src/.
