<!-- markdownlint-disable MD041 -->
Translate the C code in c_src/ to Rust that produces **byte-identical output** for the same inputs.
Write Cargo.toml and src/ files in the current directory (NOT in c_src/).

This project has **build-time configurability** via CMake cache variables.
Look at c_src/CMakeLists.txt — it uses variables like HASH_BACKEND, THASH, SECPAR
to select which source files to compile and which parameter headers to include.

You MUST preserve this configurability using **Cargo features**. Specifically:
- Each CMake cache variable value becomes a Cargo feature, using the **exact same name in lowercase**
  (e.g., CMake value "blake" → Cargo feature "blake", "128f" → "128f", "robust" → "robust")
- Use `#[cfg(feature = "...")]` to conditionally compile modules and set constants
- The Cargo.toml should define all features with a sensible default
- All combinations of features must compile

For example, if CMake has `HASH_BACKEND` with values blake/sha2/shake/haraka:
```toml
[features]
blake = []
sha2 = []
shake = []
haraka = []
```
And in Rust:
```rust
#[cfg(feature = "blake")]
mod hash_blake;
#[cfg(feature = "sha2")]
mod hash_sha2;
```

Look at ALL the source files in c_src/ including all subdirectories — every hash backend,
every thash variant, every params header. Translate ALL of them, gated by features.

This project produces BOTH a shared library AND a binary executable:
- The C code builds a shared library (sphincs_core) AND an executable (driver) from the same source
- Look at c_src/app/CMakeLists.txt to see both targets
- Your Cargo.toml must have BOTH:
  - `[lib]` with `crate-type = ["cdylib"]` — exports the public C API functions
  - `[[bin]]` with `name = "driver"` and `path = "src/main.rs"` — the KAT test binary
- `src/lib.rs` exports all public C functions with `#[unsafe(no_mangle)] extern "C"`
- `src/main.rs` contains the KAT entry point (PQCgenKAT_sign main function)

Requirements:
- All public C functions must use #[unsafe(no_mangle)] and extern "C"
- Preserve the exact C function signatures (use *const c_char, c_int, etc. from std::ffi)
- Do NOT fix bugs in the original C code — if the C has incorrect behavior, reproduce it exactly
- Use safe Rust internally where possible

Run 'cargo build --release' with the default features and fix any errors until it compiles.
Do NOT modify anything in c_src/.
