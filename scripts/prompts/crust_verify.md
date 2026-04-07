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
3. Create test files in `src/bin/` that exercise the Rust public API and verify
   it matches the C behavior.

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

1. Read the C code carefully. The C behavior is the ground truth — test for
   what the C code does, including edge cases.
2. Test every public function. For each function, test:
   - Normal/happy path inputs
   - Boundary values (0, 1, empty, max)
   - Edge cases visible in the C code (error returns, special conditions)
3. Use `assert_eq!`, `assert!`, and `assert_ne!` for assertions.
4. Do NOT use FFI or call C code. Test the Rust API directly.
5. Do NOT modify any existing files — only create new files in `src/bin/`.
6. Name test files `src/bin/test_<module>.rs` (one per module is fine, or
   combine into fewer files if the project is small).
7. After writing tests, run `cargo build` to verify the tests compile.
8. Do NOT run `cargo test` — just ensure the tests compile.
