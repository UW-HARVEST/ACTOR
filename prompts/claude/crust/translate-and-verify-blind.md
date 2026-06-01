# C-to-Rust Translation + Blind Test Generation (CRUST-bench)

You are translating a C project to idiomatic, safe Rust AND writing tests
that verify the translation is correct. You have access to the original C
source code, but you do NOT have access to any existing test suite. Your
job is to produce a working Rust translation along with comprehensive
tests that prove it matches the C behavior.

## Project Layout

- `c_src/` — the original C source files (read-only, do NOT modify)
- `src/*.rs` — Rust interface files with struct definitions, function
  signatures, and constants. The function bodies contain `unimplemented!()`.
  **Your job is to replace every `unimplemented!()` with a correct implementation.**
- `src/lib.rs` — re-exports the modules (do NOT modify)
- `Cargo.toml` — project manifest (add dependencies if needed, do NOT remove existing ones)

## Phase 1: Translate

1. Read ALL C source files in `c_src/` and ALL Rust source files in `src/`.
2. Implement every `unimplemented!()` body to match the C behavior exactly.
   Pay close attention to edge cases: empty inputs, boundary values, overflow,
   null/None handling, and error return codes.
3. Produce safe, idiomatic Rust. Do NOT use `unsafe` unless absolutely necessary.
4. Do NOT use FFI calls to C (no `libc`, no `extern "C"`). Rewrite the logic in pure Rust.
5. Do NOT modify `src/lib.rs`.
6. Do NOT modify function signatures, struct definitions, or constants in the
   source files — only fill in the bodies.
7. You may add helper functions or private modules if needed.
8. You may add crate dependencies to `Cargo.toml` if the C code uses functionality
   that has a well-known Rust crate equivalent (e.g., `rand`, `chrono`).
9. Run `cargo build` to verify it compiles. Fix any errors.
10. Do NOT leave any `unimplemented!()`, `todo!()`, or `panic!()` placeholders.
11. If a file is too large to write in one go, build it up piece by piece
    using multiple smaller writes.

## Phase 2: Write tests and verify behavior matches C

After translation compiles, write tests that prove the Rust matches C:

1. Read `src/lib.rs` to get the full list of public modules. You MUST write
   tests for EVERY module — do not skip any.
2. Build and run the C code to get ground truth outputs. Compile with:
   ```
   cd c_src && gcc -o test_prog *.c -lm 2>/dev/null || gcc -o test_prog src/*.c -I include -lm 2>/dev/null
   ```
   Use the C executable to compute expected values for your tests. Do NOT
   compute expected values by reading C source — always run the C code.
3. Create test files in `src/bin/` that exercise the Rust public API and verify
   it matches the C behavior.
4. Run `cargo test` to verify your tests pass. If tests fail, either fix the
   tests (if you misread C behavior) OR fix the Rust translation in `src/*.rs`
   (if the Rust is wrong).
5. Iterate until all tests pass.

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
