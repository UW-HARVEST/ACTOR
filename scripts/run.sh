#!/bin/bash
# run.sh — Translate C to Rust and/or test C-to-Rust translations
#
# Usage:
#   ./scripts/run.sh <battery>[/<case>]                    # translate + verify + test
#   ./scripts/run.sh <battery>[/<case>] --translate-only   # translate only
#   ./scripts/run.sh <battery>[/<case>] --test-only        # test only
#   ./scripts/run.sh <battery>[/<case>] --no-verify        # translate + test (skip verify)
#   ./scripts/run.sh <battery> --include-regex <regex>     # only cases matching regex
#
# A "battery" is a test suite name from test-corpus/Public-Tests/
# (e.g. B01_synthetic, B02_organic, P01_sphincs_plus).
# List available batteries with: ls test-corpus/Public-Tests/
#
# Examples:
#   ./scripts/run.sh B01_synthetic                         # all: translate + test
#   ./scripts/run.sh B01_synthetic/001_helloworld          # single case
#   ./scripts/run.sh B01_organic --test-only               # re-test existing translations
#   ./scripts/run.sh B01_synthetic --include-regex "_lib$" # only library cases

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ARG="${1:?Usage: $0 <battery>[/<case>] [--translate-only|--test-only|--include-regex <regex>]}"
shift

# Parse flags
DO_TRANSLATE=true
DO_VERIFY=true
DO_TEST=true
INCLUDE_REGEX=""  # regex pattern: only process cases whose names match

while [[ $# -gt 0 ]]; do
    case "$1" in
        --translate-only) DO_VERIFY=false; DO_TEST=false; shift ;;
        --test-only) DO_TRANSLATE=false; DO_VERIFY=false; shift ;;
        --no-verify) DO_VERIFY=false; shift ;;
        --include-regex) INCLUDE_REGEX="$2"; shift 2 ;;
        *) echo "Unknown flag: $1"; exit 1 ;;
    esac
done

# Handle test-suite/case syntax
if [[ "$ARG" == */* ]]; then
    BATTERY="${ARG%%/*}"
    CASE="${ARG#*/}"
    INCLUDE_REGEX="${CASE}$"
else
    BATTERY="$ARG"
    CASE=""
fi

INPUT_DIR="$REPO_ROOT/test-corpus/Public-Tests/$BATTERY"
OUTPUT_DIR="$REPO_ROOT/results/$BATTERY"

if [[ ! -d "$INPUT_DIR" ]]; then
    echo "Error: test suite not found: $INPUT_DIR"
    echo "Available:"
    ls "$REPO_ROOT/test-corpus/Public-Tests/"
    exit 1
fi

# === TRANSLATE ===
if $DO_TRANSLATE; then
    SCRIPT_DIR="$REPO_ROOT/scripts"
    # Delegate to kiro-translate.sh with the right args
    translate_args=("$BATTERY")
    [[ -n "$INCLUDE_REGEX" ]] && translate_args+=(--include-regex "$INCLUDE_REGEX")
    $DO_VERIFY && translate_args+=(--verify)
    "$SCRIPT_DIR/kiro-translate.sh" "${translate_args[@]}"
fi

# === TEST ===
if $DO_TEST; then
    echo ""
    echo "========================================"
    echo "  Testing translations"
    echo "========================================"

    PYTHONPATH="$REPO_ROOT/test-corpus/deployment/scripts/github-actions:${PYTHONPATH:-}"
    export PYTHONPATH

    # Copy test_vectors and runner from corpus (required by runtests)
    INPUT_DIR_CORPUS="$REPO_ROOT/test-corpus/Public-Tests/$BATTERY"
    for case_dir_setup in "$OUTPUT_DIR"/*/; do
        name_setup=$(basename "$case_dir_setup")
        test_case_setup="$INPUT_DIR_CORPUS/$name_setup"
        [[ -d "$test_case_setup" ]] || continue
        [[ -d "$case_dir_setup/translated_rust" ]] || continue
        if [[ -d "$test_case_setup/test_vectors" && ! -d "$case_dir_setup/test_vectors" ]]; then
            cp -r "$test_case_setup/test_vectors" "$case_dir_setup/"
        fi
        if [[ -d "$test_case_setup/runner" && ! -d "$case_dir_setup/runner" ]]; then
            cp -r "$test_case_setup/runner" "$case_dir_setup/"
            if [[ -f "$case_dir_setup/runner/Cargo.toml" ]]; then
                CANDO2_ABS="$REPO_ROOT/test-corpus/tools/cando2"
                [[ -d "$CANDO2_ABS" ]] && sed -i "s|path = \"../../../../tools/cando2\"|path = \"$CANDO2_ABS\"|" "$case_dir_setup/runner/Cargo.toml" 2>/dev/null || true
            fi
        fi
    done

    # Generate workspace Cargo.toml for lib runners
    runners=""
    for runner_toml in "$OUTPUT_DIR"/*/runner/Cargo.toml; do
        [[ -f "$runner_toml" ]] || continue
        dir=$(dirname "$runner_toml")
        rel=${dir#"$OUTPUT_DIR/"}
        runners="$runners    \"$rel\","$'\n'
    done
    if [[ -n "$runners" ]]; then
        cat > "$OUTPUT_DIR/Cargo.toml" << EOF
[workspace]
members = [
$runners]
resolver = "2"
EOF
    fi

    # Run MIT runtests and update expected results
    cd "$REPO_ROOT"
    python3 "$REPO_ROOT/scripts/validate.py" --update "$BATTERY"
fi
