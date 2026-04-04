<!-- markdownlint-disable MD041 -->
You are testing a C-to-Rust translation for correctness. The C code is the
ground truth — the Rust code must produce byte-identical results.

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
1. First, check CMakeLists.txt for build-time configurability (cache variables)
   and Cargo.toml for features. Identify ALL distinct configurations the project
   supports. Each configuration may exercise completely different code paths.
2. Build the C code as a shared library for the default configuration
3. Write Rust integration tests (in translated_rust/tests/) that use `libloading`
   to load the C .so and compare C vs Rust function outputs
4. Start with the lowest-level functions and work upward to higher-level ones.
   Look at the C headers to identify the public API and function call hierarchy.
5. For each function: create fixed test inputs, call both C and Rust versions,
   assert outputs match byte-for-byte
6. Run `cargo test` and investigate any mismatches
7. When you find a Rust function that produces different output than C,
   fix the Rust code in translated_rust/src/ and re-run until the test passes
8. Keep going until all public functions match
9. Compare `nm -D` on the C .so and the Rust .so. Every symbol the C .so
   exports, the Rust .so must also export with the exact same name. This
   includes symbols created by preprocessor macros. If the C .so exports it,
   the Rust .so must export it — no exceptions. Add missing exports.
10. If the project has a main binary, build and run BOTH the C and Rust binaries
    for EVERY configuration identified in step 1. Compare stdout byte-for-byte.
    Fix any differences in any configuration or in any code path. You will be
    evaluated on every configuration possible. Do not stop after testing only
    the default configuration.

**This may be a large verification task.** If the project has more than one
configuration or code path to verify, you MUST invoke subagents — do NOT try
to verify everything in a single session. Create a plan, then for each subtask
use the use_subagent tool with agent_name "kiro_plain" and a focused query
covering a specific subset of the code or functionality to verify and fix.
After each subagent completes, check that its fixes didn't break anything else.

Add `libloading = "0.8"` to [dev-dependencies] in translated_rust/Cargo.toml.
Do NOT modify anything in c_src/.

IMPORTANT: Use timeouts for all commands. No single build or test command should
run longer than 600 seconds. If a test takes too long, skip it and move on to
the next function. Use `timeout 600 cargo test ...` or similar. Do not get stuck
on any single step.
