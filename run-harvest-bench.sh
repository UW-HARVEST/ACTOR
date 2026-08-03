#!/usr/bin/env bash
# Run an ACTOR agent against a harvest-bench project and score it.
#
# harvest-bench (the `harvest-bench/` submodule) holds real C libraries, each as
# tests/<name>/{test_case,gtest_suite}. This script does the simple flow:
#   1. translate  test_case/  ->  Rust cdylib   (via a translation agent)
#   2. build the cdylib
#   3. score it with harvest-bench's own runner (gtest suite linked by ABI)
#
# It deliberately does NOT touch harvest-tools' Dataset/battery/plan machinery —
# it is a thin wrapper so we can point our agents at harvest-bench cases.
#
# Usage:
#   run-harvest-bench.sh <project> [--agent claude|kiro|...] [--out DIR] [--score-only DIR]
#
#   <project>          a dir under harvest-bench/tests (e.g. libsodium, libpng, lz4)
#   --agent AGENT      translation agent (default: claude). Passed to the translator.
#   --out DIR          where the translated crate goes (default: results/HarvestBench/<agent>/<project>)
#   --score-only DIR   skip translation; score an existing crate dir (its cdylib) instead
#
# Requires: cmake >= 3.24 (harvest-bench gtest FetchContent), cargo, a C compiler.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HB="$REPO_ROOT/harvest-bench"
RUNNER_SRC="$HB/runner"

PROJECT="${1:?usage: run-harvest-bench.sh <project> [--agent A] [--out DIR] [--score-only DIR]}"
shift
AGENT="claude"
OUT=""
SCORE_ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --agent) AGENT="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --score-only) SCORE_ONLY="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

CASE_DIR="$HB/tests/$PROJECT"
SUITE_DIR="$CASE_DIR/gtest_suite"
TEST_CASE="$CASE_DIR/test_case"
[ -d "$SUITE_DIR" ] || { echo "no harvest-bench project '$PROJECT' (looked in $CASE_DIR)"; exit 1; }
[ -z "$OUT" ] && OUT="$REPO_ROOT/results/HarvestBench/$AGENT/$PROJECT"

# ── 1. build the harvest-bench runner (once) ────────────────────────────────
RUNNER_BIN="$RUNNER_SRC/target/release/harvest-bench"
if [ ! -x "$RUNNER_BIN" ]; then
  echo ">> building harvest-bench runner"
  ( cd "$RUNNER_SRC" && cargo build --release ) || { echo "runner build failed"; exit 1; }
fi

# ── 2. translate (unless --score-only) ──────────────────────────────────────
if [ -n "$SCORE_ONLY" ]; then
  CRATE="$SCORE_ONLY"
  echo ">> scoring existing crate: $CRATE"
else
  echo ">> translating $PROJECT ($AGENT) from $TEST_CASE"
  # Use harvest-tools to translate the test_case. harvest-tools writes the crate
  # under its results tree; we point it at this test_case as a one-off library.
  # NOTE: adjust the invocation to your translator entry point if it differs.
  echo "   (translation step: run your agent on '$TEST_CASE' -> a Rust cdylib crate in '$OUT')"
  echo "   e.g.  harvest-tools --agent $AGENT translate <case>   OR   translate --agentic '$TEST_CASE' -o '$OUT'"
  echo "   then re-run with:  $0 $PROJECT --score-only <crate-dir>"
  CRATE="$OUT/translated_rust"
  [ -d "$CRATE" ] || { echo "no translated crate at $CRATE yet; translate first then --score-only"; exit 3; }
fi

# ── 3. build the translated cdylib ──────────────────────────────────────────
echo ">> building translated cdylib"
( cd "$CRATE" && cargo build --release ) || { echo "cdylib build failed"; exit 1; }
SO="$(find "$CRATE/target/release" -maxdepth 1 -name '*.so' | head -1)"
[ -n "$SO" ] || { echo "no .so produced in $CRATE/target/release"; exit 1; }
echo "   cdylib: $SO"

# ── 4. score via harvest-bench runner (gtest suite linked against the cdylib) ─
echo ">> scoring with harvest-bench runner"
"$RUNNER_BIN" run --suite "$SUITE_DIR" --lib "$SO"
