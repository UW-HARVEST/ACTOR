#!/bin/bash
# kiro-translate.sh — Translate C test cases to Rust using kiro-cli
#
# Usage:
#   ./scripts/kiro-translate.sh <test-suite>              # all cases
#   ./scripts/kiro-translate.sh <test-suite>/<case>       # single case
#   ./scripts/kiro-translate.sh <test-suite> --filter <regex>
#
# Examples:
#   ./scripts/kiro-translate.sh B01_synthetic
#   ./scripts/kiro-translate.sh B01_synthetic/001_helloworld
#   ./scripts/kiro-translate.sh B01_organic --filter "hex2bin_lib$"
#
# The script uses submodule paths automatically:
#   Input:  test-corpus/Public-Tests/<test-suite>/
#   Output: results/<test-suite>/
#
# Features:
#   - Skips already-completed cases (resume-friendly)
#   - Writes per-case status to progress.csv in real-time
#   - Safe to interrupt — completed cases are preserved

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ARG="${1:?Usage: $0 <test-suite>[/<case>] [--filter regex]}"
FILTER=""

# Handle test-suite/case syntax
if [[ "$ARG" == */* ]]; then
    BATTERY="${ARG%%/*}"
    FILTER="${ARG#*/}$"
else
    BATTERY="$ARG"
    if [[ "${2:-}" == "--filter" ]]; then
        FILTER="${3:?--filter requires a regex argument}"
    fi
fi

INPUT_DIR="$REPO_ROOT/test-corpus/Public-Tests/$BATTERY"
OUTPUT_DIR="$REPO_ROOT/results/$BATTERY"

if [[ ! -d "$INPUT_DIR" ]]; then
    echo "Error: test suite not found: $INPUT_DIR"
    echo "Available:"
    ls "$REPO_ROOT/test-corpus/Public-Tests/"
    exit 1
fi

TIMESTAMP=$(date +%Y%m%d_%H%M%S)
LOG_DIR="$OUTPUT_DIR/logs_$TIMESTAMP"
mkdir -p "$LOG_DIR"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROGRESS="$OUTPUT_DIR/progress.csv"

# Initialize progress file if it doesn't exist
if [[ ! -f "$PROGRESS" ]]; then
    echo "name,status,timestamp,duration_s" > "$PROGRESS"
fi

total=0
translated=0
failed=0
skipped=0

# Count total eligible cases first
for test_case in "$INPUT_DIR"/*/; do
    name=$(basename "$test_case")
    [[ -d "$test_case/test_case" && -d "$test_case/test_vectors" ]] || continue
    if [[ -n "$FILTER" ]] && ! echo "$name" | grep -qE "$FILTER"; then
        continue
    fi
    total=$((total + 1))
done

current=0
for test_case in "$INPUT_DIR"/*/; do
    name=$(basename "$test_case")

    # Must have test_case/ and test_vectors/
    [[ -d "$test_case/test_case" && -d "$test_case/test_vectors" ]] || continue

    # Apply filter if provided
    if [[ -n "$FILTER" ]] && ! echo "$name" | grep -qE "$FILTER"; then
        continue
    fi

    current=$((current + 1))

    # Skip already-completed cases (resume support)
    if [[ -f "$OUTPUT_DIR/$name/Cargo.toml" ]]; then
        skipped=$((skipped + 1))
        translated=$((translated + 1))
        echo "[$current/$total] ⏭️  $name (already done)"
        continue
    fi

    echo "[$current/$total] Translating: $name"
    start_time=$(date +%s)

    # Set up output directory
    out="$OUTPUT_DIR/$name"
    rm -rf "$out"
    mkdir -p "$out"

    # Load prompt based on project type
    if [[ "$name" == *_lib ]]; then
        prompt=$(cat "$SCRIPT_DIR/prompts/library.md" | sed "s/LIBRARY_NAME_PLACEHOLDER/$name/")
    else
        prompt=$(cat "$SCRIPT_DIR/prompts/executable.md")
    fi

    # Invoke kiro-cli, capturing failures without killing the script
    if (
        cd "$out"
        mkdir -p c_src
        cp -a "$test_case/test_case/." c_src/

        kiro-cli chat \
            --no-interactive \
            --trust-all-tools \
            "$prompt" \
            2>&1 | tee "$LOG_DIR/$name.log" | tail -5
    ); then
        end_time=$(date +%s)
        duration=$((end_time - start_time))
        if [[ -f "$out/Cargo.toml" ]]; then
            # Add workspace isolation so this package isn't pulled into a parent workspace
            if ! grep -q '\[workspace\]' "$out/Cargo.toml"; then
                echo -e '\n[workspace]' >> "$out/Cargo.toml"
            fi
            translated=$((translated + 1))
            echo "$name,success,$TIMESTAMP,${duration}" >> "$PROGRESS"
            echo "  ✅ $name (${duration}s) [$translated translated, $failed failed of $current/$total]"
        else
            failed=$((failed + 1))
            echo "$name,no_cargo_toml,$TIMESTAMP,${duration}" >> "$PROGRESS"
            echo "  ❌ $name — no Cargo.toml (${duration}s) [$translated translated, $failed failed of $current/$total]"
        fi
    else
        end_time=$(date +%s)
        duration=$((end_time - start_time))
        failed=$((failed + 1))
        echo "$name,error,$TIMESTAMP,${duration}" >> "$PROGRESS"
        echo "  ❌ $name — kiro-cli error (${duration}s) [$translated translated, $failed failed of $current/$total]"
    fi
done

echo ""
echo "========================================"

# Generate root workspace Cargo.toml for lib runners
echo "Done: $translated/$total translated, $failed failed, $skipped skipped (already done)"
echo "Progress: $PROGRESS"
echo "Logs: $LOG_DIR"
echo ""
echo "To test results:"
echo "  cargo run --release --bin=harvest-test -- $INPUT_DIR $OUTPUT_DIR"
