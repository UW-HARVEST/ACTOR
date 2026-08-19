#!/usr/bin/env bash
# Re-derive a battery's published numbers from the cache. Must be incapable of spending money:
# every phase runs `--replay-only`, so a miss refuses instead of invoking. Lives here, not in a
# workflow, so a reader with the repo runs exactly what CI runs.
#
# Usage: tools/reproduce.sh [battery]        (default: B01_synthetic)
set -uo pipefail

BATTERY="${1:-B01_synthetic}"
AGENT=claude
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Exported as 1.97.1 in the operator's login shell, silently overriding rust-toolchain.toml's
# 1.94.0 -- and the toolchain is a cache key component, so this alone makes all 175 entries miss.
unset RUSTUP_TOOLCHAIN
export PATH="$HOME/.cargo/bin:$PATH"

# `CliVersion::probe` runs `claude --version` PER CASE, which failed 85/85 in CI. Sound to state: `cli`
# left the key in #109, a replay stores nothing, and `assert_pins_honoured` runs inside `compute`.
export HARVEST_CLI_VERSION="${HARVEST_CLI_VERSION:-replay-only: no agent CLI was invoked}"

BIN=tools/target/release/harvest-tools
die() { echo "❌ $*" >&2; exit 1; }

echo "=== reproduce: $AGENT / $BATTERY ==="
rustc --version
[ -x "$BIN" ] || die "$BIN not built: cargo build --release --locked --manifest-path tools/Cargo.toml"

python3 - <<'VER' || die "python3 is $(python3 -V 2>&1), but MIT runtests needs >= 3.10"
import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)
VER
[ -d results/.cache ] || die "results/.cache absent — run: git submodule update --init results test-corpus"

log=$(mktemp -t reproduce.XXXXXX) || die "mktemp failed"
scored=$(mktemp -t scored.XXXXXX) || die "mktemp failed"
published=$(mktemp -t published.XXXXXX) || die "mktemp failed"
trap 'rm -f "$log" "$scored" "$published"' EXIT

for phase in translate verify; do
  echo
  echo "--- $phase $BATTERY (--replay-only) ---"
  if ! "$BIN" --agent "$AGENT" --replay-only "$phase" "$BATTERY" 2>&1 | tee -a "$log"; then
    die "$phase failed — the error above says why. Usually either 'built from X but HEAD is Y'
   (rebuild), or no stored entry for the key (a prompt, model or toolchain moved, so the stored
   results no longer answer this question)."
  fi
done

# Belt to `--replay-only`'s braces: trusting exit 0 alone would let a paid run pass as a replay.
tallies=$(grep -c 'agent invocation(s)' "$log" || true)
[ "$tallies" -ge 2 ] || die "no cache tally from a phase ($tallies found), so nothing is verified"
if grep 'agent invocation(s)' "$log" | grep -qv '0 agent invocation(s)'; then
  grep 'agent invocation(s)' "$log" >&2
  die "an agent was invoked; --replay-only must never reach one"
fi
if grep 'cache:' "$log" | grep -qvE '/ 0 run'; then
  grep 'cache:' "$log" >&2
  die "a phase reported a nonzero run count, so it paid for a case instead of replaying it"
fi
echo
echo "✅ every phase replayed: $tallies tally line(s), all '0 run', all '0 agent invocation(s)'"

echo
echo "--- test $BATTERY --check ---"
"$BIN" --agent "$AGENT" test "$BATTERY" --check 2>&1 | tee "$scored"
[ "${PIPESTATUS[0]}" -eq 0 ] || die "the scored numbers disagree with the stored record"

# PR #116 removes the tree even when the score exits 1; one left standing is one the next run reads.
[ -z "$(find .eval -mindepth 1 2>/dev/null | head -1)" ] || die ".eval/ still holds files"
echo "✅ .eval/ is empty"

echo
echo "--- report (regenerating tables/) ---"
"$BIN" report || die "report failed"

# Compare against the COMMITTED table, not the copy `report` just wrote from the same data --
# measured: planting a wrong row and re-running still exited 0, because report overwrote the plant.
# One battery's row, not `git diff tables/`: tables aggregate every agent, three claude batteries
# cannot be scored until their dead runs are re-run, and every claude summary.json was written
# 2026-07-20 against 2026-08-04 case data -- so a whole-file diff fails for staleness this run
# neither caused nor can fix, and says nothing about whether the replay reproduced its own number.
git show HEAD:tables/tractor.tex > "$published" 2>/dev/null \
  || die "tables/tractor.tex is not committed at HEAD, so there is no published number to check"

python3 - "$BATTERY" "$scored" "$published" <<'PY' || die "the replayed numbers do not match the published table"
import re, sys
battery, scored, table = sys.argv[1:4]

LABEL = {"B01_synthetic": "B01-syn", "B01_organic": "B01-org",
         "B02_synthetic": "B02-syn", "B02_organic": "B02-org",
         "P00_perlin_noise": "P00", "P01_sphincs_plus": "P01"}
label = LABEL.get(battery) or sys.exit(f"no tractor.tex label known for {battery}")

text = open(scored).read()
measured = {}
for phase in ("translated", "verified"):
    m = re.search(rf"{re.escape(battery)} \[{phase}\]: (\d+)/(\d+) cases, (\d+)/(\d+) vectors", text)
    if not m:
        sys.exit(f"the run reported no [{phase}] score for {battery}, so a phase went unscored")
    measured[phase] = f"{m[1]}/{m[2]}"

block, row = False, None
for line in open(table):
    if line.startswith(f"{label} &"):
        block = True
    elif block and line.startswith("\\hline"):
        break
    if block and "ACTOR (Claude Code)" in line:
        cells = [c.strip() for c in line.split("&")]
        row = [re.sub(r"\\textbf\{(.*?)\}", r"\1", c) for c in cells]
        break
if row is None:
    sys.exit(f"the committed tractor.tex has no `ACTOR (Claude Code)` row under `{label}` "
             f"({battery}) — the label mapping has drifted from report.rs's TRACTOR_BATTERIES")
published = {"translated": row[2], "verified": row[3]}

ok = True
for phase in ("translated", "verified"):
    mark = "✅" if measured[phase] == published[phase] else "❌"
    print(f"   {mark} {phase:10} replayed {measured[phase]:>7}   published {published[phase]:>7}")
    ok &= measured[phase] == published[phase]
if not ok:
    sys.exit(f"MISMATCH against committed tables/tractor.tex ({label})")
print(f"✅ {battery} [claude] reproduces BOTH published numbers exactly")
PY
git diff --quiet -- tables/ \
  && echo "   tables/ regenerated byte-identical to committed" \
  || echo "   ⚠️  tables/ moved (other agents/batteries are stale; this run reproduces $BATTERY only)"

echo
echo "=== reproduced $AGENT / $BATTERY from the cache, no agent invoked ==="
