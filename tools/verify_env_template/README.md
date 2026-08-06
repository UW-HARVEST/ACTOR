# verify_env — differential property-test environment

This directory verifies the translated Rust against the C reference by
**differential property testing**: both are built as C-ABI shared libraries,
loaded as black boxes via `libloading`, and called through their identical
exported symbols on many generated + edge-biased inputs (`proptest`). Any input
where the two sides disagree is a translation bug.

## Layout

- `build.sh` — builds everything: the C reference `.so` (coverage-instrumented,
  via the project's own `../c_src` CMake), the Rust cdylib under test, and the
  `difftest` harness.
- `difftest/` — a small pure-Rust crate (proptest + libloading). Edit
  `difftest/src/main.rs` to add one property per public function.
- `cov/` — coverage profiles pool here (the C `.so` is built with
  `-fprofile-instr-generate=cov/cov-%m.profraw`). The completeness gate reads
  these to confirm every public function was actually exercised.

## Workflow

```
./build.sh                       # build C .so + Rust .so + difftest
# then run the harness (build.sh prints the exact command with paths):
C_SO=<c .so> RUST_SO=<rust .so> ./difftest/target/release/difftest
```

Add a property for every exported symbol (`nm -D` on the C `.so` is your target
list). Derive each input domain from the C source; proptest already biases
toward edges (0, MAX, empty, boundaries), so a plain `any::<T>()` or a
`vec(any::<u8>(), 0..N)` payload usually surfaces value-dependent bugs. Iterate:
add properties → rebuild (incremental, ~1s) → run → fix any divergence in the
Rust (never the C) → re-run. Coverage accumulates in `cov/` across runs.
