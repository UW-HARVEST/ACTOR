#!/bin/bash
# aggregate-analysis.sh — Use kiro-cli to generate a failure analysis report
#
# Collects all fix-result.json and fix.patch data, feeds it to kiro-cli,
# and writes a natural language report.
#
# Usage:
#   ./scripts/aggregate-analysis.sh                    # all batteries
#   ./scripts/aggregate-analysis.sh B01_organic        # one battery
#   ./scripts/aggregate-analysis.sh --output report.md # custom output path

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS_DIR="$REPO_ROOT/results"
OUTPUT="$REPO_ROOT/results/analysis-report.md"

# Parse args
batteries=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output) OUTPUT="$2"; shift 2 ;;
        *) batteries+=("$1"); shift ;;
    esac
done

# Default: all batteries with summary.json
if [[ ${#batteries[@]} -eq 0 ]]; then
    for d in "$RESULTS_DIR"/*/; do
        [[ -f "$d/summary.json" ]] && batteries+=("$(basename "$d")")
    done
fi

# Collect all data into a single context file
context=$(mktemp)
{
    echo "# Failure Analysis Data"
    echo ""

    for battery in "${batteries[@]}"; do
        battery_dir="$RESULTS_DIR/$battery"
        [[ -d "$battery_dir" ]] || continue

        echo "## $battery"
        echo ""

        for case_dir in "$battery_dir"/*/; do
            [[ -d "$case_dir" ]] || continue
            fr="$case_dir/logs/fix-result.json"
            [[ -f "$fr" ]] || continue

            case_name=$(basename "$case_dir")
            echo "### $case_name"
            echo ""
            echo "fix-result.json:"
            echo '```json'
            cat "$fr"
            echo '```'

            patch="$case_dir/logs/fix.patch"
            if [[ -s "$patch" ]]; then
                echo ""
                echo "fix.patch:"
                echo '```diff'
                cat "$patch"
                echo '```'
            fi
            echo ""
        done
    done
} > "$context"

echo "Collected data from ${#batteries[@]} batteries ($(wc -l < "$context") lines)"
echo "Invoking kiro-cli to generate report..."

prompt=$(cat "$REPO_ROOT/scripts/prompts/aggregate.md")
prompt="${prompt//DATA_FILE_PLACEHOLDER/$context}"
prompt="${prompt//OUTPUT_PLACEHOLDER/$OUTPUT}"

kiro-cli chat \
    --no-interactive \
    --trust-all-tools \
    "$prompt" \
    < /dev/null \
    2>&1 | tee /tmp/aggregate-analysis.log

rm -f "$context"

if [[ -f "$OUTPUT" ]]; then
    echo ""
    echo "Report written to: $OUTPUT"
else
    echo ""
    echo "Warning: report file not created at $OUTPUT"
fi
