#!/bin/bash
# verify-translation.sh — C-as-oracle differential testing using kiro-cli
#
# Builds the C code as a shared library, then invokes kiro-cli to generate
# integration tests comparing C vs Rust function outputs. Fixes mismatches.
#
# Usage:
#   ./scripts/verify-translation.sh <battery>/<case>                  # single case
#   ./scripts/verify-translation.sh <battery>                         # all cases
#   ./scripts/verify-translation.sh <battery> --include-regex <regex> # filtered
#   ./scripts/verify-translation.sh <battery> --force                 # re-verify
#
# For shared-source batteries (symlinked test_case), only verifies the real case
# and re-propagates fixes to all copies.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Ensure OpenSSL is discoverable for C builds
export OPENSSL_DIR="${OPENSSL_DIR:-/usr}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS_DIR="$REPO_ROOT/results"
PROMPT_TEMPLATE="$SCRIPT_DIR/prompts/verify.md"

# --- Parse arguments ---
ARG="${1:?Usage: $0 <battery>[/<case>] [--include-regex regex] [--force]}"
FILTER=""
FORCE=false

shift
while [[ $# -gt 0 ]]; do
    case "$1" in
        --include-regex) FILTER="${2:?--include-regex requires an argument}"; shift 2 ;;
        --force) FORCE=true; shift ;;
        *) shift ;;
    esac
done

# Handle battery/case syntax
if [[ "$ARG" == */* ]]; then
    BATTERY="${ARG%%/*}"
    FILTER="${ARG#*/}$"
else
    BATTERY="$ARG"
fi

BATTERY_DIR="$RESULTS_DIR/$BATTERY"
INPUT_DIR="$REPO_ROOT/test-corpus/Public-Tests/$BATTERY"

if [[ ! -d "$BATTERY_DIR" ]]; then
    echo "Error: battery not found: $BATTERY_DIR"
    exit 1
fi

# --- Detect shared-source (symlinked test_case) ---
REAL_CASE=""
for test_case in "$INPUT_DIR"/*/; do
    [[ -L "$test_case/test_case" ]] || continue
    real_dir=$(basename "$(dirname "$(realpath "$test_case/test_case")")")
    if [[ -z "$REAL_CASE" ]]; then
        REAL_CASE="$real_dir"
    fi
    break
done

# --- Build cmake flags from CMakePresets.json ---
get_cmake_flags() {
    local case_dir="$1"
    local presets="$INPUT_DIR/$(basename "$case_dir")/CMakePresets.json"
    if [[ -f "$presets" ]]; then
        python3 -c "
import json
d = json.load(open('$presets'))
cv = d['configurePresets'][1]['cacheVariables']
flags = ' '.join(f'-D{k}={v}' for k, v in cv.items() if k not in ('CMAKE_C_STANDARD', 'CMAKE_BUILD_TYPE'))
print(flags)
" 2>/dev/null
    fi
}

# --- Main ---
total=0
verified=0
skipped=0
failed=0

# Collect cases to verify
cases=()
for case_dir in "$BATTERY_DIR"/*/; do
    [[ -d "$case_dir" ]] || continue
    name=$(basename "$case_dir")

    # For shared-source: only verify the real case
    if [[ -n "$REAL_CASE" && "$name" != "$REAL_CASE" ]]; then
        continue
    fi

    # Must have translated_rust/
    [[ -d "$case_dir/translated_rust/Cargo.toml" ]] || [[ -f "$case_dir/translated_rust/Cargo.toml" ]] || continue

    # Apply filter
    if [[ -n "$FILTER" ]] && ! echo "$name" | grep -qE "$FILTER"; then
        continue
    fi

    cases+=("$name")
done

total=${#cases[@]}
echo "=== Verifying $BATTERY ($total cases) ==="
[[ -n "$REAL_CASE" ]] && echo "Shared-source battery: verifying $REAL_CASE only, will re-propagate fixes"

current=0
for name in "${cases[@]}"; do
    case_dir="$BATTERY_DIR/$name"
    current=$((current + 1))

    # Skip already verified (unless --force)
    if [[ -f "$case_dir/logs/verify.log" && "$FORCE" != true ]]; then
        echo "[$current/$total] ⏭️  $name (already verified)"
        skipped=$((skipped + 1))
        continue
    fi

    echo "[$current/$total] 🔬 $name"
    start_time=$(date +%s)

    # Restore from original translation (clean slate for verification)
    if [[ -d "$case_dir/translated_rust_original" ]]; then
        rm -rf "$case_dir/translated_rust"
        cp -a "$case_dir/translated_rust_original" "$case_dir/translated_rust"
    fi

    # Remove test_vectors and runner — verify must use C-as-oracle only
    rm -rf "$case_dir/test_vectors" "$case_dir/runner"

    # Build cmake flags
    cmake_flags=$(get_cmake_flags "$case_dir")

    # Build prompt
    prompt=$(cat "$PROMPT_TEMPLATE")
    prompt="${prompt//CASE_DIR_PLACEHOLDER/$case_dir}"
    prompt="${prompt//CMAKE_BUILD_FLAGS/$cmake_flags}"

    # Invoke kiro-cli (with 45 minute hard timeout)
    mkdir -p "$case_dir/logs"
    if (
        cd "$case_dir"
        timeout 2700 kiro-cli chat \
            --no-interactive \
            --trust-all-tools \
            "$prompt" \
            < /dev/null \
            2>&1 | tee "$case_dir/logs/verify.log"
    ); then
        : # success
    else
        : # kiro-cli failed but we continue
    fi

    end_time=$(date +%s)
    duration=$((end_time - start_time))

    # Check if cargo test passes
    if (cd "$case_dir/translated_rust" && OPENSSL_DIR=/usr cargo test 2>&1 | grep -q "test result: ok"); then
        echo "[$current/$total] ✅ $name — verified (${duration}s)"
        verified=$((verified + 1))
    else
        echo "[$current/$total] ❌ $name — verification incomplete (${duration}s)"
        failed=$((failed + 1))
    fi
done

# Re-propagate fixes for shared-source batteries
if [[ -n "$REAL_CASE" ]]; then
    echo ""
    echo "Re-propagating fixes from $REAL_CASE to all copies..."
    real_src="$BATTERY_DIR/$REAL_CASE/translated_rust/src"
    real_cargo="$BATTERY_DIR/$REAL_CASE/translated_rust/Cargo.toml"
    propagated=0
    for case_dir in "$BATTERY_DIR"/*/; do
        name=$(basename "$case_dir")
        [[ "$name" == "$REAL_CASE" ]] && continue
        [[ -f "$case_dir/translated_rust/Cargo.toml" ]] || continue
        # Copy updated src (including subdirectories)
        rm -rf "$case_dir/translated_rust/src"
        cp -a "$real_src" "$case_dir/translated_rust/"
        # Remove main.rs and tests/ for lib cases
        if [[ "$name" == *_lib ]]; then
            rm -f "$case_dir/translated_rust/src/main.rs"
            rm -rf "$case_dir/translated_rust/tests"
        fi
        # Clean target so it rebuilds with new code
        rm -rf "$case_dir/translated_rust/target"
        propagated=$((propagated + 1))
    done
    echo "Propagated to $propagated cases"
fi

echo ""
echo "========================================"
echo "Verified: $verified, Failed: $failed, Skipped: $skipped (of $total)"
