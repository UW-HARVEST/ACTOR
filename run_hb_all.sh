#!/usr/bin/env bash
# Full harvest-bench run over ALL 7 projects, --agent claude, 3-way parallel.
# One invocation: `run HB --parallel 3` translates all 7 (3 concurrent), then
# verifies all 7 (3 concurrent), then scores. The harness skips any project
# already translated+verified, so this is resume-friendly.
set -uo pipefail

# Derived, never hardcoded: this script must stay at the repo root.
cd -- "$(dirname -- "$(readlink -f -- "${BASH_SOURCE[0]}")")" || exit 1

# Toolchain: cmake 3.28 (HB needs >=3.24), python 3.12 (runtests match syntax),
# cargo, claude. Order matters — these must precede system paths.
#
# cmake lives under $HOME, not /tmp: the 2026-08-15 restart found
# /tmp/cmake-3.28.6-linux-x86_64 gone (tmpfs is cleared on reboot) while the system
# cmake is 3.22.2. Nothing checked, so a twelve-hour sweep would have built no gtest
# suite and scored every project zero.
export PATH="$HOME/.local/opt/cmake-3.28.6-linux-x86_64/bin:$HOME/.local/share/mise/installs/python/3.12.13/bin:$HOME/.cargo/bin:$HOME/.nix-profile/bin:$HOME/.local/bin:$PATH"

# RUSTUP_TOOLCHAIN silently overrides rust-toolchain.toml, and harvest-tools refuses
# rather than measure under a compiler the cache key does not describe. It is exported
# in some login shells, so on 2026-08-15 all 7 translations completed (3h20m) and all 7
# verifies were refused for this alone. Unset here so the driver cannot inherit it.
unset RUSTUP_TOOLCHAIN

# Refuse before the money, not after: a too-old cmake or a missing runner is an infra
# failure that scores as a legitimate zero.
require_version() {  # name  minimum  actual
  printf '%s\n%s\n' "$2" "$3" | sort -V -C || {
    echo "::error::$1 $3 is older than the required $2" >&2
    exit 1
  }
}
command -v cmake >/dev/null || { echo "cmake not on PATH" >&2; exit 1; }
require_version cmake 3.24 "$(cmake --version | head -1 | grep -oE '[0-9]+(\.[0-9]+)+')"
require_version python 3.10 "$(python3 -c 'import sys; print("%d.%d" % sys.version_info[:2])')"
command -v claude >/dev/null || { echo "claude not on PATH" >&2; exit 1; }

# The agent-runtime settings are deliberately NOT exported here: agents::session::AGENT_ENV
# applies them, and the cache key can only hash them from there. See its doc comment.

BIN=./tools/target/release/harvest-tools
LOGDIR=/tmp/hb_run
mkdir -p "$LOGDIR"
SUMMARY="$LOGDIR/SUMMARY.txt"
: > "$SUMMARY"

echo "[$(date +%H:%M:%S)] START full HB sweep, 3-way parallel (translate -> verify -> score)" | tee -a "$SUMMARY"

# Single invocation: translate all (parallel 3) -> verify all (parallel 3) -> score.
"$BIN" --agent claude run HB --parallel 3 > "$LOGDIR/run.log" 2>&1
rc=$?
echo "[$(date +%H:%M:%S)] run HB finished rc=$rc" | tee -a "$SUMMARY"

# Final authoritative scoreboard from the result.json files (reader rule:
# verified/ if present else translated/). This is the source of truth — the
# harness's own stdout can lag; these files are what was actually scored.
echo "=== FINAL RESULTS (verified/ else translated/) ===" | tee -a "$SUMMARY"
for p in lz4 libsodium libpng jansson mujs pcre2 zstd; do
  rj=""
  for phase in verified translated; do
    cand="results/HarvestBench/claude/$p/$phase/result.json"
    [ -f "$cand" ] && { rj="$cand"; break; }
  done
  if [ -n "$rj" ]; then
    phase=$(echo "$rj" | grep -oE 'verified|translated')
    line=$(tr -d '\n' < "$rj" | grep -oE '"build_ok": (true|false)|"tests_ok": [0-9]+|"tests_failed": [0-9]+|"tests_skipped": [0-9]+' | tr '\n' ' ')
    echo "  $p [$phase]: $line" | tee -a "$SUMMARY"
  else
    echo "  $p: MISSING (no result.json)" | tee -a "$SUMMARY"
  fi
done
echo "[$(date +%H:%M:%S)] ALL DONE" | tee -a "$SUMMARY"
touch "$LOGDIR/ALLDONE.marker"
