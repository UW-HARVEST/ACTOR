#!/bin/bash
# analyze-failures.sh — Analyze failing test cases using kiro-cli
#
# Usage:
#   ./scripts/analyze-failures.sh <battery>                          # all failures
#   ./scripts/analyze-failures.sh <battery> --include-regex <regex>  # filtered
#   ./scripts/analyze-failures.sh                                    # all batteries
#
# Examples:
#   ./scripts/analyze-failures.sh B01_organic
#   ./scripts/analyze-failures.sh B01_organic --include-regex "colourblind"
#   ./scripts/analyze-failures.sh B02_synthetic --force
#
# For each failing case:
#   1. Copies the case to /tmp/analyze/ (scratch dir)
#   2. Invokes kiro-cli to investigate, explain, and attempt a fix
#   3. Saves analysis.log, fix.patch, fix-result.json back to results/<battery>/<case>/logs/
#   4. Cleans up scratch dir
#
# The original translated_rust/ is NEVER modified — all changes happen in scratch.
#
# Outputs per case (in results/<battery>/<case>/logs/):
#   analysis.log    — full kiro-cli conversation
#   fix.patch       — diff of Rust changes (empty if none)
#   fix-result.json — whether the fix worked

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS_DIR="$REPO_ROOT/results"
TEST_CORPUS="$REPO_ROOT/test-corpus"
SCRATCH_BASE="/tmp/analyze"
PROMPT_TEMPLATE="$SCRIPT_DIR/prompts/analyze.md"

# --- Parse arguments ---
BATTERY=""
FILTER=""
FORCE=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --include-regex) FILTER="${2:?--include-regex requires an argument}"; shift 2 ;;
        --force) FORCE=true; shift ;;
        *) BATTERY="$1"; shift ;;
    esac
done

# If no battery specified, iterate all
if [[ -z "$BATTERY" ]]; then
    batteries=()
    for d in "$RESULTS_DIR"/*/; do
        b=$(basename "$d")
        [[ -f "$d/summary.json" ]] && batteries+=("$b")
    done
else
    batteries=("$BATTERY")
fi

# --- Helpers ---

is_lib_case() {
    [[ -d "$1/runner" ]]
}

is_failing_case() {
    local result_json="$1/result.json"
    [[ -f "$result_json" ]] && grep -q '"passed": false' "$result_json"
}

already_analyzed() {
    [[ -f "$1/logs/analysis.log" ]] && [[ "$FORCE" != true ]]
}

setup_scratch_exec() {
    local case_dir="$1" scratch_case="$2"
    mkdir -p "$scratch_case"
    cp -a "$case_dir/translated_rust" "$scratch_case/"
    cp -a "$case_dir/test_vectors" "$scratch_case/"
    [[ -d "$case_dir/logs" ]] && cp -a "$case_dir/logs" "$scratch_case/"
}

setup_scratch_lib() {
    local case_dir="$1" scratch_case="$2" scratch_battery="$3" case_name="$4"

    mkdir -p "$scratch_case"
    cp -a "$case_dir/translated_rust" "$scratch_case/"
    cp -a "$case_dir/test_vectors" "$scratch_case/"
    cp -a "$case_dir/runner" "$scratch_case/"
    [[ -d "$case_dir/logs" ]] && cp -a "$case_dir/logs" "$scratch_case/"

    # Generate single-member workspace Cargo.toml at battery level
    cat > "$scratch_battery/Cargo.toml" << EOF
[workspace]
members = ["$case_name/runner"]
resolver = "2"
EOF
}

collect_results() {
    local case_dir="$1" scratch_case="$2" scratch_battery="$3" case_name="$4" battery="$5"
    local logs_dir="$case_dir/logs"
    mkdir -p "$logs_dir"

    # Generate patch: diff original vs scratch Rust src
    diff -ruN "$case_dir/translated_rust/src" "$scratch_case/translated_rust/src" \
        > "$logs_dir/fix.patch" 2>/dev/null || true

    # Run tests one final time on scratch to record result
    local test_output
    test_output=$(
        cd "$TEST_CORPUS/deployment/scripts/github-actions" && \
        PYTHONPATH=. python3 -m runtests.rust \
            --root "$scratch_battery" \
            --subset "$scratch_case" \
            --keep-going 2>&1
    ) || true

    local vectors_passed vectors_failed fix_passed
    vectors_passed=$(echo "$test_output" | grep "Test Vectors Passed:" | awk '{print $NF}')
    vectors_failed=$(echo "$test_output" | grep "Test Vectors Failed:" | awk '{print $NF}')
    vectors_passed="${vectors_passed:-0}"
    vectors_failed="${vectors_failed:-0}"

    if [[ "$vectors_failed" == "0" && "$vectors_passed" != "0" ]]; then
        fix_passed=true
    else
        fix_passed=false
    fi

    # Check if any changes were made
    local fix_attempted=false
    if [[ -s "$logs_dir/fix.patch" ]]; then
        fix_attempted=true
    fi

    cat > "$logs_dir/fix-result.json" << EOF
{
  "case": "$case_name",
  "battery": "$battery",
  "fix_attempted": $fix_attempted,
  "fix_passed": $fix_passed,
  "vectors_passed": $vectors_passed,
  "vectors_failed": $vectors_failed
}
EOF
}

# --- Main loop ---

total_analyzed=0
total_skipped=0
total_fixed=0
total_failed=0

for battery in "${batteries[@]}"; do
    battery_dir="$RESULTS_DIR/$battery"
    if [[ ! -d "$battery_dir" ]]; then
        echo "Warning: battery not found: $battery_dir"
        continue
    fi

    echo "=== $battery ==="
    progress_file="$battery_dir/progress_analysis.csv"
    if [[ ! -f "$progress_file" ]]; then
        echo "name,status,timestamp,duration_s" > "$progress_file"
    fi

    # Collect failing cases
    cases=()
    for case_dir in "$battery_dir"/*/; do
        [[ -d "$case_dir" ]] || continue
        case_name=$(basename "$case_dir")

        # Apply filter
        if [[ -n "$FILTER" ]] && ! echo "$case_name" | grep -qE "$FILTER"; then
            continue
        fi

        # Only failing cases
        if ! is_failing_case "$case_dir"; then
            continue
        fi

        cases+=("$case_name")
    done

    if [[ ${#cases[@]} -eq 0 ]]; then
        echo "  No failing cases found"
        continue
    fi

    echo "  ${#cases[@]} failing cases"
    case_idx=0

    for case_name in "${cases[@]}"; do
        case_dir="$battery_dir/$case_name"
        case_idx=$((case_idx + 1))

        # Skip already analyzed (unless --force)
        if already_analyzed "$case_dir"; then
            echo "  [$case_idx/${#cases[@]}] ⏭️  $case_name (already analyzed)"
            total_skipped=$((total_skipped + 1))
            continue
        fi

        echo "  [$case_idx/${#cases[@]}] 🔍 $case_name"
        start_time=$(date +%s)

        # Set up scratch
        scratch_battery="$SCRATCH_BASE/$battery"
        scratch_case="$scratch_battery/$case_name"
        rm -rf "$scratch_case"
        rm -f "$scratch_battery/Cargo.toml" "$scratch_battery/Cargo.lock"

        if is_lib_case "$case_dir"; then
            setup_scratch_lib "$case_dir" "$scratch_case" "$scratch_battery" "$case_name"
        else
            setup_scratch_exec "$case_dir" "$scratch_case"
        fi

        # Build prompt with real paths
        prompt=$(cat "$PROMPT_TEMPLATE")
        prompt="${prompt//CASE_DIR_PLACEHOLDER/$scratch_case}"
        prompt="${prompt//TEST_CORPUS_PLACEHOLDER/$TEST_CORPUS}"
        prompt="${prompt//ROOT_PLACEHOLDER/$scratch_battery}"

        # Invoke kiro-cli
        mkdir -p "$case_dir/logs"
        if (
            cd "$scratch_case"
            kiro-cli chat \
                --no-interactive \
                --trust-all-tools \
                "$prompt" \
                < /dev/null \
                2>&1 | tee "$case_dir/logs/analysis.log"
        ); then
            : # kiro-cli succeeded
        else
            : # kiro-cli failed but we still collect results
        fi

        # Collect results
        collect_results "$case_dir" "$scratch_case" "$scratch_battery" "$case_name" "$battery"

        end_time=$(date +%s)
        duration=$((end_time - start_time))

        # Read back result
        fix_passed=$(grep -o '"fix_passed": [a-z]*' "$case_dir/logs/fix-result.json" | awk '{print $2}')
        fix_attempted=$(grep -o '"fix_attempted": [a-z]*' "$case_dir/logs/fix-result.json" | awk '{print $2}')

        if [[ "$fix_passed" == "true" ]]; then
            echo "  [$case_idx/${#cases[@]}] ✅ $case_name — fixed (${duration}s)"
            echo "$case_name,fixed,$(date -Iseconds),${duration}" >> "$progress_file"
            total_fixed=$((total_fixed + 1))
        elif [[ "$fix_attempted" == "true" ]]; then
            echo "  [$case_idx/${#cases[@]}] ❌ $case_name — fix attempted but failed (${duration}s)"
            echo "$case_name,fix_failed,$(date -Iseconds),${duration}" >> "$progress_file"
            total_failed=$((total_failed + 1))
        else
            echo "  [$case_idx/${#cases[@]}] 📝 $case_name — analyzed, no fix attempted (${duration}s)"
            echo "$case_name,analyzed,$(date -Iseconds),${duration}" >> "$progress_file"
            total_failed=$((total_failed + 1))
        fi

        total_analyzed=$((total_analyzed + 1))

        # Clean up scratch for this case
        rm -rf "$scratch_case"
        rm -f "$scratch_battery/Cargo.toml" "$scratch_battery/Cargo.lock"
        rm -rf "$scratch_battery/target"
    done
done

echo ""
echo "========================================"
echo "Analyzed: $total_analyzed, Fixed: $total_fixed, Not fixed: $total_failed, Skipped: $total_skipped"
