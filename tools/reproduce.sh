#!/usr/bin/env bash
# Reproduce the published numbers in ONE shot: resolve every phase from the cache, score, and emit
# tables/ -- then check nothing moved. Must be incapable of spending money: `--replay-only` makes a
# cache miss a refusal, never an invocation.
#
# There is no separate score or report step to point at a tree. `run` produces every output from what
# it resolved, so `git diff` IS the check: if what it regenerated matches the committed files, the run
# reproduced them. No row parsing, and nothing a stale file can satisfy.
#
# `all` means EVERY dataset: Test-Corpus and harvest-bench are earned by separate runs and each writes
# only its own tables, so reproducing "the published numbers" is both legs plus one combined check.
#
# Usage: tools/reproduce.sh [target]        (default: all = Test-Corpus + harvest-bench)
set -uo pipefail

TARGET="${1:-all}"
# ALL THREE TOOLS, in ONE invocation, not a loop. Two reasons, and the second is the important one:
#   - `AGENT=claude` meant CI only ever replayed claude, so no other tool's numbers were checked
#     against the store at all. `runtests.rs` says so in as many words -- four agents each publish
#     `0/128` for P01 and nobody noticed, "because `reproduce.sh` replays claude only".
#   - `tables/` is written ONCE per run from every tool's attestation MERGED. Replaying the tools in
#     separate runs would have each rewrite `tables/` from its own rows and blank the others', so the
#     byte-for-byte diff below would compare against whichever tool finished last.
TOOLS="${TOOLS:-claude,codex,kiro}"
if [ "$TARGET" = all ]; then LEGS=(all HB); else LEGS=("$TARGET"); fi
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Exported as 1.97.1 in the operator's login shell, silently overriding rust-toolchain.toml's
# 1.94.0 -- and the toolchain is a cache key component, so this alone makes every entry miss.
unset RUSTUP_TOOLCHAIN
export PATH="$HOME/.cargo/bin:$PATH"

# `CliVersion::probe` runs `claude --version` PER CASE, which failed 85/85 in CI. Sound to state: `cli`
# left the key in #109, a replay stores nothing, and `assert_pins_honoured` runs inside `compute`.
export HARVEST_CLI_VERSION="${HARVEST_CLI_VERSION:-replay-only: no agent CLI was invoked}"

BIN=tools/target/release/harvest-tools
die() { echo "❌ $*" >&2; exit 1; }

echo "=== reproduce: $TOOLS / $TARGET ==="
rustc --version
[ -x "$BIN" ] || die "$BIN not built: cargo build --release --locked --manifest-path tools/Cargo.toml"

[ -d results/.cache ] || die "results/.cache absent — run: git submodule update --init results test-corpus"

# No per-leg input list here. `Benchmark::preflight` owns it, so `run`/`translate`/`verify` refuse on a
# missing scorer or interpreter too -- not just this script, which is how two of them reached CI.

log=$(mktemp -t reproduce.XXXXXX) || die "mktemp failed"
trap 'rm -f "$log"' EXIT

for leg in "${LEGS[@]}"; do
  echo
  echo "--- run $leg (--replay-only) ---"
  "$BIN" --tool "$TOOLS" --replay-only run "$leg" 2>&1 | tee -a "$log"
  [ "${PIPESTATUS[0]}" -eq 0 ] || die "the $leg run failed — the error above says why. Usually either
   'built from X but HEAD is Y' (rebuild), or no stored entry for a key (a prompt, model or toolchain
   moved, so the stored results no longer answer this question)."
done

# Belt to `--replay-only`'s braces: trusting exit 0 alone would let a paid run pass as a replay.
tallies=$(grep -c 'agent invocation(s)' "$log" || true)
[ "$tallies" -ge 2 ] || die "no cache tally from a phase ($tallies found), so nothing is verified"

# Every tool must have SPOKEN. A bare count cannot tell "all three replayed" from "claude replayed
# twice": `-ge 2` passed for years while only claude was ever replayed. Each tool prints its own
# `<tool> cache: N hit / M run` line per leg, so the absence of one is the absence of that tool.
for tool in ${TOOLS//,/ }; do
  grep -q "^$tool .*agent invocation(s)" "$log" \
    || die "$tool produced no cache tally, so its numbers were never checked against the store.
   Either it is out of scope for every battery (the run says which and why), or the store does not
   cover it -- and in both cases the published table for $tool rests on nothing this replay verified."
done
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

# PR #116 removes the tree even when the score exits 1; one left standing is one the next run reads.
[ -z "$(find .eval -mindepth 1 2>/dev/null | head -1)" ] || die ".eval/ still holds files"
echo "✅ .eval/ is empty"

echo
echo "--- did anything move? ---"
if [ "$TARGET" = all ]; then
  # `git diff` alone cannot tell "regenerated the same bytes" from "never regenerated": had the run
  # skipped table generation, the committed files would be trivially unchanged and the diff would pass
  # on stale numbers. So require the run to SAY it wrote each file the diff then covers.
  for f in $(git ls-files tables/); do
    grep -qF "Wrote $(pwd)/$f" "$log" \
      || die "the run never wrote $f, so 'unchanged' says nothing about it: either table generation
   was skipped, or that file is committed but no longer produced by anything."
  done
  grep -q 'Tables regenerated' "$log" || die "the run reported no table regeneration at all"
  echo "✅ all $(git ls-files tables/ | wc -l) committed table(s) were regenerated by this run"

  git diff --stat -- tables/
  git diff --exit-code -- tables/ >/dev/null \
    || die "tables/ moved: the replayed numbers differ from the committed ones. Inspect the diff above,
   then commit it deliberately if the new numbers are the ones you mean to publish."
  echo "✅ tables/ byte-identical to committed"
else
  # A partial scope may not write tables at all -- one battery's numbers cannot claim the whole
  # table's rows. So the check inverts: nothing under tables/ may have moved.
  git diff --exit-code -- tables/ >/dev/null \
    || die "$TARGET is a partial scope and must write no tables, but tables/ moved"
  echo "✅ $TARGET is a partial scope: it wrote no tables, and none moved"
fi

echo
echo "=== reproduced $TOOLS / $TARGET from the cache, no agent invoked ==="
