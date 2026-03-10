#!/bin/bash
# kiro-translate.sh — Translate C test cases to Rust using kiro-cli
#
# Usage:
#   ./scripts/kiro-translate.sh <battery> [--filter regex]
#
# Examples:
#   ./scripts/kiro-translate.sh B01_synthetic
#   ./scripts/kiro-translate.sh B01_organic --filter "hex2bin_lib$"
#
# The script uses submodule paths automatically:
#   Input:  test-corpus/Public-Tests/<battery>/
#   Output: results/<battery>/
#
# Features:
#   - Skips already-completed cases (resume-friendly)
#   - Writes per-case status to progress.csv in real-time
#   - Safe to interrupt — completed cases are preserved
#
# Validate results:
#   ./scripts/validate.sh <battery>

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BATTERY="${1:?Usage: $0 <battery> [--filter regex]}"
FILTER="${2:-}"

if [[ "$FILTER" == "--filter" ]]; then
    FILTER="${3:?--filter requires a regex argument}"
fi

INPUT_DIR="$REPO_ROOT/test-corpus/Public-Tests/$BATTERY"
OUTPUT_DIR="$REPO_ROOT/results/$BATTERY"

if [[ ! -d "$INPUT_DIR" ]]; then
    echo "Error: battery not found: $INPUT_DIR"
    echo "Available batteries:"
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
    if [[ -f "$OUTPUT_DIR/$name/translated_rust/Cargo.toml" ]]; then
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
    mkdir -p "$out/translated_rust"

    # Copy test_vectors and runner (for _lib cases)
    cp -r "$test_case/test_vectors" "$out/"
    if [[ -d "$test_case/runner" ]]; then
        cp -r "$test_case/runner" "$out/"
        # Fix cando2 relative path to absolute (use submodule path)
        if [[ -f "$out/runner/Cargo.toml" ]]; then
            CANDO2_ABS="$REPO_ROOT/test-corpus/tools/cando2"
            if [[ -d "$CANDO2_ABS" ]]; then
                sed -i '' "s|path = \"../../../../tools/cando2\"|path = \"$CANDO2_ABS\"|" "$out/runner/Cargo.toml" 2>/dev/null || \
                sed -i "s|path = \"../../../../tools/cando2\"|path = \"$CANDO2_ABS\"|" "$out/runner/Cargo.toml" 2>/dev/null || true
            fi
        fi
    fi

    # Load prompt based on project type
    if [[ "$name" == *_lib ]]; then
        prompt=$(cat "$SCRIPT_DIR/prompts/library.md" | sed "s/LIBRARY_NAME_PLACEHOLDER/$name/")
    else
        prompt=$(cat "$SCRIPT_DIR/prompts/executable.md")
    fi

    # Invoke kiro-cli, capturing failures without killing the script
    if (
        cd "$out/translated_rust"
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
        if [[ -f "$out/translated_rust/Cargo.toml" ]]; then
            # Add workspace isolation so this package isn't pulled into a parent workspace
            if ! grep -q '\[workspace\]' "$out/translated_rust/Cargo.toml"; then
                echo -e '\n[workspace]' >> "$out/translated_rust/Cargo.toml"
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
    echo "Generated root workspace with $(echo "$runners" | wc -l | tr -d ' ') lib runners"
fi

echo "Done: $translated/$total translated, $failed failed, $skipped skipped (already done)"
echo "Progress: $PROGRESS"
echo "Logs: $LOG_DIR"
echo ""
echo "To validate, run from the Test-Corpus repo:"
echo "  python3 -m runtests.rust --root $OUTPUT_DIR --subset $OUTPUT_DIR --keep-going"
