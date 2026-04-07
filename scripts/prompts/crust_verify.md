# CRUST Verify — Blind Test Generation

You are writing Rust tests for a C-to-Rust translation. You have access to the
original C source code and the Rust translation, but you do NOT have access to
any existing test suite. Your job is to write comprehensive tests that verify
the Rust code behaves identically to the C code.

## Project Layout

- `c_src/` — the original C source files (the ground truth for behavior)
- `src/*.rs` — the Rust translation you are testing
- `src/lib.rs` — re-exports the modules
- `Cargo.toml` — project manifest (package name defines the `use` import path)

## Your Task

1. Read ALL C source files in `c_src/` to understand the expected behavior.
2. Read ALL Rust source files in `src/` to understand the public API.
3. Build and run the C code to get ground truth outputs. Compile with:
   ```
   cd c_src && gcc -o test_prog *.c -lm 2>/dev/null || gcc -o test_prog src/*.c -I include -lm 2>/dev/null
   ```
   Run it with various inputs to observe the exact C behavior. Use this to
   derive expected values for your Rust tests.
4. Create test files in `src/bin/` that exercise the Rust public API and verify
   it matches the C behavior.
5. Run `cargo test` to verify your tests pass. If tests fail, read the failure
   output carefully and fix your tests OR identify bugs in the Rust translation
   and fix those too (in `src/*.rs`, not `src/lib.rs`).
6. Iterate until all your tests pass.

## Test File Format

Each test file must be placed in `src/bin/` and follow this format:

```rust
use <package_name>::<module>;

#[test]
fn test_something() {
    // ... assertions ...
}

fn main() {}
```

The `fn main() {}` is required because test files in `src/bin/` are compiled as
binary crates. The package name comes from `Cargo.toml` `[package] name`.

## Rules

1. The C code is the ground truth. When in doubt about expected behavior,
   compile and run the C code to check.
2. Test every public function. For each function, test:
   - Normal/happy path inputs
   - Boundary values (0, 1, empty, max)
   - Edge cases visible in the C code (error returns, special conditions)
3. Use `assert_eq!`, `assert!`, and `assert_ne!` for assertions.
4. Do NOT use FFI or call C code from your tests. Test the Rust API directly.
5. Do NOT modify `src/lib.rs`.
6. Name test files `src/bin/test_<module>.rs` (one per module is fine, or
   combine into fewer files if the project is small).
7. When `cargo test` fails:
   - If the test expectation is wrong (you misread the C behavior), fix the test.
   - If the Rust implementation is wrong (doesn't match C), fix `src/*.rs`.
   - Do NOT change function signatures.
8. Do NOT leave any `unimplemented!()`, `todo!()`, or `panic!()` placeholders.
