You are in the working directory. It holds two subtrees:
- `c_src/` — the original C. READ IT; never modify it.
- `translation/` — the Rust crate. Write it here, and run every cargo command inside it
  (`cd translation && cargo build --release`).

Translate the C code in c_src/ to Rust.
Write Cargo.toml and src/ files in `translation/`.
Run `cargo build --release` and fix any errors until it compiles.
Do NOT modify anything in c_src/.
