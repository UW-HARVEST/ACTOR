#!/usr/bin/env bash
# Full harvest-bench run over ALL 7 projects, --agent claude, 3-way parallel.
# One invocation: `run HB --parallel 3` translates all 7 (3 concurrent), then
# verifies all 7 (3 concurrent), then scores. The harness skips any project
# already translated+verified, so this is resume-friendly.
set -uo pipefail

cd /local/home/scheschb/research/ACTOR

# Toolchain: cmake 3.28 (HB needs >=3.24), python 3.12 (runtests match syntax),
# cargo, claude. Order matters — these must precede system paths.
export PATH="/tmp/cmake-3.28.6-linux-x86_64/bin:/home/scheschb/.local/share/mise/installs/python/3.12.13/bin:$HOME/.cargo/bin:$HOME/.nix-profile/bin:$HOME/.local/bin:$PATH"

# Harden against API instability on long agentic sessions (inherited by claude).
export API_TIMEOUT_MS="1200000"             # 20 min per-request
export API_FORCE_IDLE_TIMEOUT="0"           # disable 5-min streaming idle abort
export CLAUDE_CODE_MAX_RETRIES="20"
export CLAUDE_CODE_RETRY_WATCHDOG="1"

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
