<!-- markdownlint-disable MD041 -->
Translate the C code in c_src/ to Rust that produces **byte-identical output** for the same inputs.
Write Cargo.toml and src/ files in the current directory (NOT in c_src/).

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

After the Rust library compiles, verify it matches the C behavior via FFI testing:
1. Build the C as a shared library:
   ```
   cd c_src && mkdir -p build && cd build && \
   cmake .. -DCMAKE_POSITION_INDEPENDENT_CODE=ON && cmake --build . && cd ../..
   ```
   Find the resulting .so file in c_src/build/.
2. Add `libloading = "0.8"` to [dev-dependencies] in Cargo.toml.
3. Write integration tests in tests/ that use `libloading` to load BOTH the C
   .so AND the Rust .so, and compare their outputs through the FFI boundary.
   Never call Rust functions directly — always load the Rust .so via libloading
   and call its exported symbols, exactly as an external caller would. This
   also tests the `#[no_mangle]` export wrappers.
4. Start with the lowest-level functions and work upward. Look at the C
   headers to identify the public API and function call hierarchy.
5. For each function: create test inputs, call both C and Rust via their .so
   exports, assert outputs match byte-for-byte.
6. Run `cargo test` and fix any mismatches. The C is ground truth — fix the
   Rust translation, never the test expectations.
7. Compare `nm -D` on the C .so and the Rust .so. Every symbol the C .so
   exports, the Rust .so must also export with the exact same name. This
   includes symbols created by preprocessor macros. Add missing exports.
8. Iterate until all public functions match.

IMPORTANT: Use timeouts for all commands. No single build or test command should
run longer than 600 seconds. Use `timeout 600 cargo test ...` or similar.

Do NOT modify anything in c_src/.
