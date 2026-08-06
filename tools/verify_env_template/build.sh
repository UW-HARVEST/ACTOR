#!/usr/bin/env bash
# Build the differential-test environment:
#   1. the C reference `.so`, coverage-instrumented (so the completeness gate can
#      read which functions the properties exercised), via the project's OWN
#      CMake in ../c_src (reused verbatim — handles zlib links, nested dirs, defs).
#   2. the Rust cdylib under test (../.. is the translated crate).
#   3. the difftest harness (pure Rust: proptest + libloading).
# No FuzzTest/abseil/antlr — everything here builds in seconds and rebuilds
# incrementally, so the edit→retest loop is cheap.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"                      # verify_env/
crate_root="$(cd .. && pwd)"    # translated_rust/  (the Rust crate)

# Memory-aware -j (the C build is small, but stay polite on shared boxes).
mem_gb=$(awk '/MemAvailable/ {printf "%d", $2/1024/1024}' /proc/meminfo 2>/dev/null || echo 4)
cores=$(nproc 2>/dev/null || echo 4)
jobs_by_mem=$(( mem_gb / 2 )); [ "$jobs_by_mem" -lt 1 ] && jobs_by_mem=1
jobs=$(( cores < jobs_by_mem ? cores : jobs_by_mem ))

mkdir -p cov

# ── 1. C reference .so, coverage-instrumented. Profiles pool into verify_env/cov/
# via the %m merge pattern (baked at link time, CWD-independent, accumulates
# across runs) so the properties' coverage is captured no matter how it's run.
cov_pat="$here/cov/cov-%m.profraw"
echo "[difftest-build] C reference (coverage-instrumented) -j$jobs"
( cd "$crate_root/c_src"
  # A stale build/ (e.g. a CMakeCache with a baked absolute path from an earlier
  # workspace) makes cmake refuse to configure. Always start clean.
  rm -rf build
  CC=clang cmake -S . -B build -DCMAKE_BUILD_TYPE=RelWithDebInfo \
    -DCMAKE_C_FLAGS="-fprofile-instr-generate=$cov_pat -fcoverage-mapping" \
    -DCMAKE_SHARED_LINKER_FLAGS="-fprofile-instr-generate=$cov_pat" >/dev/null
  cmake --build build -j"$jobs" >/dev/null )
C_SO="$(find "$crate_root/c_src/build" -maxdepth 1 -name 'lib*.so' | head -1)"
echo "[difftest-build] C .so: $C_SO"

# ── 2. Rust cdylib under test.
echo "[difftest-build] Rust cdylib"
( cd "$crate_root" && cargo build --release >/dev/null 2>&1 || cargo build --release )
RUST_SO="$(find "$crate_root/target/release" -maxdepth 1 -name 'lib*.so' | head -1)"
echo "[difftest-build] Rust .so: $RUST_SO"

# ── 3. the difftest harness.
echo "[difftest-build] difftest harness"
( cd difftest && cargo build --release >/dev/null 2>&1 || cargo build --release )

echo
echo "Built. Run the differential properties (pools coverage into cov/):"
echo "  C_SO=\"$C_SO\" \\"
echo "  RUST_SO=\"$RUST_SO\" \\"
echo "  ./difftest/target/release/difftest"
