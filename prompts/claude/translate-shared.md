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
