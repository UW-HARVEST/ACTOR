#!/usr/bin/env bash
# Re-run ONLY the 4 harvest-bench projects unfinished after the host froze
# (lz4, mujs, pcre2, zstd). The 3 that completed (libpng, jansson, libsodium)
# keep their results — verify skips any project that already has a verify.log,
# so we delete ONLY the 4 stale partial logs and run without --force.
#
# TMPDIR -> /local so agent workdirs survive a reboot (tmpfs /tmp is wiped);
# the host froze mid-run last time, so durability matters.
set -uo pipefail
cd /local/home/scheschb/research/ACTOR

export PATH="/tmp/cmake-3.28.6-linux-x86_64/bin:/home/scheschb/.local/share/mise/installs/python/3.12.13/bin:$HOME/.cargo/bin:$HOME/.nix-profile/bin:$HOME/.local/bin:$PATH"
export API_TIMEOUT_MS="1200000"
export API_FORCE_IDLE_TIMEOUT="0"
export CLAUDE_CODE_MAX_RETRIES="20"
export CLAUDE_CODE_RETRY_WATCHDOG="1"

# Durable workdirs (survive reboot); proptest builds are small so /local is fine.
export TMPDIR="/local/home/scheschb/research/hb_work"
mkdir -p "$TMPDIR"

BIN=./tools/target/release/harvest-tools
LOGDIR=/local/home/scheschb/research/hb_run   # on /local too, survives reboot
mkdir -p "$LOGDIR"
SUMMARY="$LOGDIR/SUMMARY.txt"; : > "$SUMMARY"

echo "[$(date +%H:%M:%S)] START HB re-run of 4 unfinished (lz4 mujs pcre2 zstd), proptest, parallel 3" | tee -a "$SUMMARY"

# Delete ONLY the 4 stale partial verify.logs so verify (no --force) runs exactly them.
for p in lz4 mujs pcre2 zstd; do
  rm -f "results/HarvestBench/claude/$p/verified/logs/verify.log"
done

# verify (no --force): skips the 3 with intact verify.log, runs the 4 we cleared.
nice -n 19 ionice -c3 "$BIN" --agent claude verify HB --parallel 3 > "$LOGDIR/verify.log" 2>&1
echo "[$(date +%H:%M:%S)] verify finished rc=$?" | tee -a "$SUMMARY"

# Score ALL 7 (reader rule: verified/ else translated/) + regenerate tables.
nice -n 19 "$BIN" --agent claude test HB --update > "$LOGDIR/test.log" 2>&1
echo "[$(date +%H:%M:%S)] test --update finished rc=$?" | tee -a "$SUMMARY"

echo "=== FUZZ GATE (all 7) ===" | tee -a "$SUMMARY"
for p in lz4 libsodium libpng jansson mujs pcre2 zstd; do
  g="results/HarvestBench/claude/$p/verified/logs/FUZZ_GATE.md"
  [ -f "$g" ] && echo "  $p: $(grep -m1 -oE 'PASS|FAIL|INCONCLUSIVE|[0-9]+/[0-9]+ public functions fuzzed' "$g" | tr '\n' ' ')" | tee -a "$SUMMARY" || echo "  $p: (no gate)" | tee -a "$SUMMARY"
done
echo "[$(date +%H:%M:%S)] ALL DONE" | tee -a "$SUMMARY"
touch "$LOGDIR/ALLDONE.marker"
