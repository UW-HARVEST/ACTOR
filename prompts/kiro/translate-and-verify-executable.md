<!-- markdownlint-disable MD041 -->
You are in the working directory. It holds two subtrees:
- `c_src/` — the original C. READ IT; never modify it.
- `translation/` — the Rust crate. Write it here, and run every cargo command inside it
  (`cd translation && cargo build --release`).

Translate the C code in c_src/ to Rust that produces **byte-identical output** for the same inputs.
Write Cargo.toml and src/ files in `translation/` (NOT in c_src/).

This is an EXECUTABLE. Requirements:
- Do NOT fix bugs in the original C code — if the C has incorrect behavior, reproduce it exactly
- Preserve the exact order of error checks and validation
- Match C's stdin reading behavior exactly (scanf reads across newlines, fgets does not)
- Match C's exact printf format output including spacing and newlines
- Use safe Rust internally where possible

After writing the Rust code, verify it matches the C behavior:
1. Compile the C: `cd c_src && mkdir -p build && cd build && cmake .. && cmake --build . && cd ../..`
2. Run `cargo build --release` and fix any errors until it compiles
3. Run both binaries against representative inputs (including edge cases) and
   compare stdout byte-for-byte. Examples:
   ```
   echo "<input>" | ./c_src/build/<binary> > /tmp/c_out
   echo "<input>" | ./target/release/driver > /tmp/rust_out
   diff /tmp/c_out /tmp/rust_out
   ```
4. If outputs differ, fix the Rust translation in src/ to match C exactly.
   The C is ground truth. Iterate until outputs match for all inputs you test.

Do NOT modify anything in c_src/.
