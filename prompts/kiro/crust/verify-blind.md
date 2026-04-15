# CRUST Verify — Blind Test Generation

You are writing Rust tests for a C-to-Rust translation. You have access to the
original C source code and the Rust translation, but you do NOT have access to
any existing test suite. Your job is to write comprehensive tests that verify
the Rust code behaves identically to the C code.

## Project Layout

- `c_src/` — the original C source files: the ground truth for behavior (read-only, do NOT modify)
- `src/*.rs` — the Rust translation you are testing
- `src/lib.rs` — re-exports the modules (do NOT modify)
- `Cargo.toml` — project manifest; package name defines the `use` import
  path  (add dependencies if needed, do NOT remove existing ones)

## Your Task

1. Read ALL C source files in `c_src/` to understand the expected behavior.
2. Read ALL Rust source files in `src/` to understand the public API.
3. Read `src/lib.rs` to get the full list of public modules. You MUST write
   tests for EVERY module — do not skip any.
4. Build and run the C code to get ground truth outputs. Compile with:
   ```sh
   cd c_src && gcc -o test_prog *.c -lm 2>/dev/null || gcc -o test_prog src/*.c -I include -lm 2>/dev/null
   ```
   Use the C executable to compute expected values for your tests. Do NOT
   compute expected values by reading C source — always run the C code.
5. Create test files in `src/bin/` that exercise the Rust public API and verify
   it matches the C behavior.
6. Run `cargo test` to verify your tests pass. If tests fail, read the failure
   output carefully and fix your tests OR identify bugs in the Rust translation
   and fix those too (in `src/*.rs`, not `src/lib.rs`).
7. Iterate until all your tests pass.

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

CRITICAL RULES about test format:
- Every test function MUST have the `#[test]` attribute. Without it, `cargo test`
  will not discover or run the test.
- `fn main() {}` MUST be empty. Do NOT put assertions inside main().
- Do NOT write C-style test programs with assertions in main(). That pattern
  does not work with `cargo test`.

## Testing Rules

1. **Test functions directly.** Call every public function by name. Do NOT test
   through wrapper functions or higher-level APIs. If the module exposes
   `expand_definitions()`, call `expand_definitions()` directly — do not call
   `reduce()` which happens to use it internally.

2. **Assert all output fields.** When a function returns a struct, assert EVERY
   field. When a function returns a numeric value, assert the exact value. Do
   not just check booleans or truthiness — check the actual computed result.
   Bad: `assert!(!result.valid);`
   Good: `assert!(!result.valid); assert_eq!(result.valid_upto, 0); assert_eq!(result.code_point, 72);`

3. **Test edge cases you find — never skip them.** If the C code handles
   boundary values, overflow, out-of-range inputs, or error conditions, you
   MUST test those. If the Rust code handles an edge case differently from C,
   that is a translation bug — fix the Rust code. Do NOT work around edge
   cases by choosing different test inputs.

4. **Get expected values from running C, not from reading C.** For every test
   case, write a small C program or use the existing C test to compute the
   expected output. Use that exact output as your assertion value.

5. **Cover every module.** List all `pub mod` entries in `src/lib.rs`. Write at
   least one test file covering each module. Name files `src/bin/test_<module>.rs`.

6. Do NOT use FFI or call C code from your tests. Test the Rust API directly.
7. Do NOT modify `src/lib.rs`.
8. Do NOT change function signatures.
9. Do NOT leave any `unimplemented!()`, `todo!()`, or `panic!()` placeholders.
10. When `cargo test` fails:
    - If the test expectation is wrong (you misread the C behavior), fix the test.
    - If the Rust implementation is wrong (doesn't match C), fix `src/*.rs`.
