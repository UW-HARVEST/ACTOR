#!/bin/bash
# run.sh — Translate C to Rust and/or test C-to-Rust translations
#
# Usage:
#   ./scripts/run.sh <battery>[/<case>]                    # translate + test
#   ./scripts/run.sh <battery>[/<case>] --translate-only   # translate only
#   ./scripts/run.sh <battery>[/<case>] --test-only        # test only
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
DO_TEST=true
INCLUDE_REGEX=""  # regex pattern: only process cases whose names match

while [[ $# -gt 0 ]]; do
    case "$1" in
        --translate-only) DO_TEST=false; shift ;;
        --test-only) DO_TRANSLATE=false; shift ;;
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
    if [[ -n "$INCLUDE_REGEX" ]]; then
        "$SCRIPT_DIR/kiro-translate.sh" "$BATTERY" --include-regex "$INCLUDE_REGEX"
    else
        "$SCRIPT_DIR/kiro-translate.sh" "$BATTERY"
    fi
fi

# === TEST ===
if $DO_TEST; then
    echo ""
    echo "========================================"
    echo "  Testing translations"
    echo "========================================"

    # Determine which cases to test
    if [[ -n "$CASE" ]]; then
        CASES=("$CASE")
    elif [[ -n "$INCLUDE_REGEX" ]]; then
        CASES=()
        for d in "$OUTPUT_DIR"/*/; do
            name=$(basename "$d")
            if echo "$name" | grep -qE "$INCLUDE_REGEX"; then
                CASES+=("$name")
            fi
        done
    else
        CASES=()
        for d in "$OUTPUT_DIR"/*/; do
            name=$(basename "$d")
            [[ -d "$d/translated_rust" ]] && CASES+=("$name")
        done
    fi

    PYTHONPATH="$REPO_ROOT/test-corpus/deployment/scripts/github-actions:${PYTHONPATH:-}"
    export PYTHONPATH

    total=0          # test vectors run
    passed=0         # test vectors passed
    failed=0         # test vectors failed
    failed_names=()  # case names with any failing vectors

    for case_name in "${CASES[@]}"; do
        case_dir="$OUTPUT_DIR/$case_name"
        [[ -d "$case_dir/translated_rust" ]] || continue
        [[ -d "$case_dir/test_vectors" ]] || continue

        total=$((total + 1))

        # Run runtests on this single case
        output=$(cd "$REPO_ROOT/test-corpus" && python3 -m runtests.rust \
            --root "$OUTPUT_DIR" \
            --subset "$case_dir" \
            --keep-going 2>&1) || true

        # Parse results
        vp=$(echo "$output" | grep -oP "Test Vectors Passed:\s+\K\d+" || echo "0")
        vf=$(echo "$output" | grep -oP "Test Vectors Failed:\s+\K\d+" || echo "0")
        vs=$(echo "$output" | grep -oP "Test Vectors Skipped:\s+\K\d+" || echo "0")
        cf=$(echo "$output" | grep -oP "Test Cases Failed:\s+\K\d+" || echo "0")

        # Write per-case result.json
        cat > "$case_dir/result.json" << EOF
{
  "case": "$case_name",
  "battery": "$BATTERY",
  "vectors_passed": $vp,
  "vectors_failed": $vf,
  "vectors_skipped": $vs,
  "passed": $([ "$cf" = "0" ] && echo "true" || echo "false")
}
EOF

        if [[ "$cf" == "0" ]]; then
            passed=$((passed + 1))
            echo "  ✅ $case_name ($vp passed, $vs skipped)"
        else
            failed=$((failed + 1))
            failed_names+=("$case_name")
            echo "  ❌ $case_name ($vp passed, $vf failed, $vs skipped)"
        fi
    done

    echo ""
    echo "========================================"
    echo "  Results: $passed/$total passed, $failed failed"
    if [[ ${#failed_names[@]} -gt 0 ]]; then
        echo "  Failed: ${failed_names[*]}"
    fi
    echo "========================================"

    # Update expected_results.json and write summaries
    python3 "$REPO_ROOT/scripts/validate.py" --update "$BATTERY"
fi
