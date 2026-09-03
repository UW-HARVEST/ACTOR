#!/usr/bin/env bash
# Reproduce the published numbers in ONE shot: resolve every phase from the cache, score, emit
# tables/ -- then check nothing moved. Incapable of spending money: `--replay-only` makes a miss a
# refusal. There is no separate score or report step to point at a tree, so `git diff` IS the check.
#
# `all` means EVERY dataset: Test-Corpus and harvest-bench are earned by separate runs and each writes
# only its own tables, so reproducing "the published numbers" is both legs plus one combined check.
#
# Usage: TOOLS=<csv> tools/reproduce.sh [target]     (defaults: every tool, all = Test-Corpus + HB)
set -uo pipefail

TARGET="${1:-all}"
# THE tool list. `AGENT=claude` meant CI only ever replayed claude, so no other tool's numbers were
# checked against the store at all -- `runtests.rs` says so in as many words: four agents each publish
# `0/128` for P01 and nobody noticed, "because `reproduce.sh` replays claude only".
ALL_TOOLS="claude,codex,kiro"
TOOLS="${TOOLS:-$ALL_TOOLS}"
# `tables/` is written ONCE per run from every tool's attestation MERGED, so only a run covering every
# tool can be diffed at all. A subset run proves its own tool replays and leaves the tables alone.
if [ "$TOOLS" = "$ALL_TOOLS" ]; then TABLES=identical; else TABLES=subset; fi
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

# COVERAGE, not just cost. Every check above asks "was anything paid for"; none asks "was everything
# measured". A battery whose store coverage is incomplete goes OUT OF SCOPE, scores nothing, and emits
# no tally line -- so `tallies` merely drops and "all 0 run" stays trivially true for the batteries that
# did score. Measured: codex silently lost all 128 P01_sphincs_plus cases, 332/338 -> 204/338, and its
# own per-tool arm went green; only the merged arm caught it, and only because it diffs the tables.
if grep -qE 'out of scope|no stored entry for the' "$log"; then
  grep -hE 'out of scope|no stored entry for the' "$log" | sed 's/^/   /' | sort -u >&2
  die "a battery went out of scope, or a step had no stored entry. The store does not cover what the
   committed tables claim, so those numbers rest on cases this replay never scored."
fi
echo "✅ every battery in scope: no case unresolved, no battery skipped"

# PR #116 removes the tree even when the score exits 1; one left standing is one the next run reads.
[ -z "$(find .eval -mindepth 1 2>/dev/null | head -1)" ] || die ".eval/ still holds files"
echo "✅ .eval/ is empty"

echo
echo "--- did anything move? ---"
# Which tables THIS run wrote, from what it SAID: `git diff` cannot tell "same bytes" from "never ran".
# Relative to `tables/`, never $(pwd): the binary prints /local/home/... and `pwd` the same directory as
# /home/..., through a symlinked home -- so a $(pwd) anchor matched nothing and could only ever fail.
written=$(grep -o "Wrote .*/tables/[^ ]*" "$log" | sed "s|.*/tables/|tables/|" | sort -u)
if grep -q 'Tables regenerated' "$log"; then
  [ -n "$written" ] || die "the run reported table regeneration but named no file it wrote"
  for f in $written; do
    git ls-files --error-unmatch "$f" >/dev/null 2>&1 \
      || die "the run wrote $f, which is not committed: commit it or stop producing it"
  done
  if [ "$TABLES" = identical ]; then
    git diff --stat -- $written
    git diff -U0 -- $written | head -80
    git diff --exit-code -- $written >/dev/null \
      || die "tables moved: the replayed numbers differ from the committed ones. The moved lines are
   above; commit them deliberately only if these are the numbers you mean to publish."
    echo "✅ byte-identical to committed: $(echo $written | tr '\n' ' ')"
        for f in $(git ls-files tables/); do
      echo "$written" | grep -qxF "$f" \
        || die "$f is committed but this run did not write it: either table generation was skipped, or
   nothing produces that file any more."
    done
    echo "✅ all $(git ls-files tables/ | wc -l) committed table(s) were regenerated by this run"
  else
    # A subset run's tables are NOT comparable, not even row by row: `numbers.tex` is named macros for
    # every tool, so a codex-only run correctly writes `--` for the others, which the committed file does
    # not contain. This run proves what it can -- the store covers ITS tool -- and the diff belongs to
    # the run covering every tool.
    git checkout -- tables/
    echo "✅ $TOOLS replayed every phase; its tables are not comparable alone, and none were left moved"
  fi
else
  # A partial scope writes no tables -- one battery's numbers cannot claim the whole table's rows.
  git diff --exit-code -- tables/ >/dev/null \
    || die "$TARGET is a partial scope and must write no tables, but tables/ moved"
  echo "✅ $TARGET is a partial scope: it wrote no tables, and none moved"
fi

echo
echo "=== reproduced $TOOLS / $TARGET from the cache, no agent invoked ==="
