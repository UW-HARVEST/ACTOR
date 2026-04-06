# C-to-Rust Translation (CRUST-bench)

You are translating a C project to idiomatic, safe Rust.

## Project Layout

- `c_src/` — the original C source files (read-only, do NOT modify)
- `src/interfaces/*.rs` — Rust interface files with struct definitions, function
  signatures, and constants. The function bodies contain `unimplemented!()`.
  **Your job is to replace every `unimplemented!()` with a correct implementation.**
- `src/lib.rs` — re-exports the interface modules (do NOT modify)
- `src/bin/` — test binaries (do NOT modify)
- `Cargo.toml` — project manifest (add dependencies if needed, do NOT remove existing ones)

## Rules

1. Read ALL C source files in `c_src/` and ALL interface files in `src/interfaces/`.
2. Implement every `unimplemented!()` body to match the C behavior exactly.
3. Produce safe, idiomatic Rust. Do NOT use `unsafe` unless absolutely necessary.
4. Do NOT use FFI calls to C (no `libc`, no `extern "C"`). Rewrite the logic in pure Rust.
5. Do NOT modify `src/lib.rs` or any file in `src/bin/`.
6. Do NOT modify function signatures, struct definitions, or constants in the
   interface files — only fill in the bodies.
7. You may add helper functions or private modules if needed.
8. You may add crate dependencies to `Cargo.toml` if the C code uses functionality
   that has a well-known Rust crate equivalent (e.g., `rand`, `chrono`).
9. After implementing, run `cargo build` to verify it compiles.
10. Run `timeout 60 cargo test` to verify correctness. Fix any failures.
    The 60-second timeout is strict — if tests are too slow, optimize your
    implementation (avoid unnecessary allocations, use iterators, etc.).
    When tests fail, re-read the corresponding C function to understand the
    exact behavior — do not guess fixes from the Rust code alone. The C code
    is always the ground truth.
11. Do NOT leave any `unimplemented!()`, `todo!()`, or `panic!()` placeholders.
