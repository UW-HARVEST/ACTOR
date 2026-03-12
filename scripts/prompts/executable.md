<!-- markdownlint-disable MD041 -->
Translate the C code in c_src/ to Rust that produces **byte-identical output** for the same inputs.
Write Cargo.toml and src/ files in the current directory (NOT in c_src/).

This is an EXECUTABLE. Requirements:
- The binary name in Cargo.toml must be "driver" (use [[bin]] name = "driver")
- Do NOT fix bugs in the original C code — if the C has incorrect behavior, reproduce it exactly
- Preserve the exact order of error checks and validation
- Match C's stdin reading behavior exactly (scanf reads across newlines, fgets does not)
- Match C's exact printf format output including spacing and newlines
- Use safe Rust internally where possible

Run 'cargo build --release' and fix any errors until it compiles.
Do NOT modify anything in c_src/.
