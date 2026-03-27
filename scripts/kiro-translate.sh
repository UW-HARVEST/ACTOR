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

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Ensure OpenSSL is discoverable for C/Rust builds that need it
export OPENSSL_DIR="${OPENSSL_DIR:-/usr}"

ARG="${1:?Usage: $0 <test-suite>[/<case>] [--filter regex] [--verify]}"
FILTER=""
VERIFY=false

shift || true
while [[ $# -gt 0 ]]; do
    case "$1" in
        --filter) FILTER="${2:?--filter requires a regex argument}"; shift 2 ;;
        --verify) VERIFY=true; shift ;;
        *) shift ;;
    esac
done

# Handle test-suite/case syntax
if [[ "$ARG" == */* ]]; then
    BATTERY="${ARG%%/*}"
    FILTER="${ARG#*/}$"
else
    BATTERY="$ARG"
fi

INPUT_DIR="$REPO_ROOT/test-corpus/Public-Tests/$BATTERY"
OUTPUT_DIR="$REPO_ROOT/results/$BATTERY"

if [[ ! -d "$INPUT_DIR" ]]; then
    echo "Error: test suite not found: $INPUT_DIR"
    echo "Available:"
    ls "$REPO_ROOT/test-corpus/Public-Tests/"
    exit 1
fi

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
    [[ -d "$test_case/test_case" || -L "$test_case/test_case" ]] || continue
    [[ -d "$test_case/test_vectors" ]] || continue
    if [[ -n "$FILTER" ]] && ! echo "$name" | grep -qE "$FILTER"; then
        continue
    fi
    total=$((total + 1))
done

# --- Detect shared test_case (symlinks → same source, build-time configurability) ---
# Find the "real" case (non-symlink test_case) and all cases that symlink to it
REAL_CASE=""
declare -A SYMLINKED_CASES  # case_name → real_case_name
for test_case in "$INPUT_DIR"/*/; do
    name=$(basename "$test_case")
    [[ -d "$test_case/test_case" || -L "$test_case/test_case" ]] || continue
    if [[ -L "$test_case/test_case" ]]; then
        # Resolve symlink to find the real case
        real_dir=$(basename "$(dirname "$(realpath "$test_case/test_case")")")
        SYMLINKED_CASES["$name"]="$real_dir"
        if [[ -z "$REAL_CASE" ]]; then
            REAL_CASE="$real_dir"
        fi
    fi
done

if [[ -n "$REAL_CASE" ]]; then
    echo "Detected shared source: $REAL_CASE (${#SYMLINKED_CASES[@]} configurations)"
    echo "Will translate once, then symlink translated_rust/ for all configurations"
    echo ""
fi

current=0
for test_case in "$INPUT_DIR"/*/; do
    name=$(basename "$test_case")

    # Must have test_case/ and test_vectors/
    [[ -d "$test_case/test_case" || -L "$test_case/test_case" ]] || continue
    [[ -d "$test_case/test_vectors" ]] || continue

    # Apply filter if provided
    if [[ -n "$FILTER" ]] && ! echo "$name" | grep -qE "$FILTER"; then
        continue
    fi

    current=$((current + 1))

    # --- Symlinked case: symlink translated_rust/ to the real case's translation ---
    if [[ -n "${SYMLINKED_CASES[$name]:-}" ]]; then
        real_name="${SYMLINKED_CASES[$name]}"
        out="$OUTPUT_DIR/$name"

        # Skip if already done
        if [[ -f "$out/translated_rust/Cargo.toml" || -L "$out/translated_rust" ]]; then
            skipped=$((skipped + 1))
            translated=$((translated + 1))
            echo "[$current/$total] ⏭️  $name (already done)"
            continue
        fi

        # Wait for real case to be translated first
        if [[ ! -f "$OUTPUT_DIR/$real_name/translated_rust/Cargo.toml" ]]; then
            echo "[$current/$total] ⏭️  $name (waiting for $real_name to be translated)"
            continue
        fi

        mkdir -p "$out"

        # Set up translated_rust with copied src and own Cargo.toml/target
        mkdir -p "$out/translated_rust"
        # Copy all source files from real case
        cp -a "$OUTPUT_DIR/$real_name/translated_rust/src" "$out/translated_rust/"
        # Copy Cargo.toml (will set per-case default features below)
        cp "$OUTPUT_DIR/$real_name/translated_rust/Cargo.toml" "$out/translated_rust/Cargo.toml"
        # Copy c_src for reference
        [[ -d "$OUTPUT_DIR/$real_name/translated_rust/c_src" ]] && \
            cp -a "$OUTPUT_DIR/$real_name/translated_rust/c_src" "$out/translated_rust/"
        mkdir -p "$out/logs"

        # Extract features from CMakePresets.json and set as default in Cargo.toml
        if [[ -f "$test_case/CMakePresets.json" ]]; then
            features=$(python3 -c "
import json, re
d = json.load(open('$test_case/CMakePresets.json'))
cv = d['configurePresets'][1]['cacheVariables']
backend = cv.get('HASH_BACKEND','').lower()
thash = cv.get('THASH','').lower()
secpar = cv.get('SECPAR','').lower()

# Read Cargo.toml and find all defined feature names
cargo = open('$out/translated_rust/Cargo.toml').read()
defined = set(re.findall(r'^([a-zA-Z0-9][a-zA-Z0-9_-]*)\s*=\s*\[', cargo, re.MULTILINE)) - {'default'}

result = []
if backend in defined: result.append(backend)
if thash in defined: result.append(thash)
if secpar in defined: result.append(secpar)
else:
    composite = f'sphincs-{backend}-{secpar}'
    if composite in defined: result.append(composite)
print(','.join(result))
" 2>/dev/null)
            if [[ -n "$features" ]]; then
                # Set default features in Cargo.toml
                feat_array=$(echo "$features" | tr ',' '\n' | sed 's/.*/"&"/' | paste -sd, -)
                sed -i "/^default = /d" "$out/translated_rust/Cargo.toml"
                sed -i "/^\[features\]/a default = [$feat_array]" "$out/translated_rust/Cargo.toml"
            fi
        fi

        # Lib cases: remove [[bin]] section and set [lib] name from runner
        # Exec cases: keep [[bin]] (need driver binary)
        if [[ "$name" == *_lib ]]; then
            sed -i '/^\[\[bin\]\]/,/^$/d' "$out/translated_rust/Cargo.toml"
            rm -f "$out/translated_rust/src/main.rs"
            rm -rf "$out/translated_rust/tests"
            # Set [lib] name from test corpus runner
            corpus_runner="$INPUT_DIR/$name/runner/src/main.rs"
            if [[ -f "$corpus_runner" ]]; then
                lib_name=$(grep 'library:' "$corpus_runner" 2>/dev/null | sed 's/.*library: "\(.*\)".*/\1/' | head -1)
                if [[ -n "$lib_name" ]]; then
                    sed -i "/^\[lib\]/,/^\[/{/^name/d;}" "$out/translated_rust/Cargo.toml"
                    sed -i "/^\[lib\]/a name = \"$lib_name\"" "$out/translated_rust/Cargo.toml"
                fi
            fi
        fi

        # Save original for this config
        rm -rf "$out/translated_rust_original"
        cp -a "$out/translated_rust" "$out/translated_rust_original"

        translated=$((translated + 1))
        echo "[$current/$total] 🔗 $name → $real_name"
        continue
    fi

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

    # Load prompt based on project type
    if [[ -n "$REAL_CASE" && "$name" == "$REAL_CASE" ]]; then
        prompt=$(cat "$SCRIPT_DIR/prompts/configurable.md")
    elif [[ "$name" == *_lib ]]; then
        prompt=$(cat "$SCRIPT_DIR/prompts/library.md")
    else
        prompt=$(cat "$SCRIPT_DIR/prompts/executable.md")
    fi

    # Invoke kiro-cli, capturing failures without killing the script
    if (
        cd "$out/translated_rust"
        mkdir -p c_src "$out/logs"
        cp -a "$test_case/test_case/." c_src/

        kiro-cli chat \
            --no-interactive \
            --trust-all-tools \
            "$prompt" \
            < /dev/null \
            2>&1 | tee "$out/logs/translation.log"
    ); then
        end_time=$(date +%s)
        duration=$((end_time - start_time))
        if [[ -f "$out/translated_rust/Cargo.toml" ]]; then
            # === Post-processing: fix Cargo.toml for MIT compatibility ===
            cargo_toml="$out/translated_rust/Cargo.toml"

            # Add [workspace] isolation
            if ! grep -q '\[workspace\]' "$cargo_toml"; then
                echo -e '\n[workspace]' >> "$cargo_toml"
            fi

            # Skip lib/bin post-processing for shared-source real case
            # (the configurable.md prompt already produces both [lib] and [[bin]])
            if [[ -n "$REAL_CASE" && "$name" == "$REAL_CASE" ]]; then
                : # no-op — LLM handles Cargo.toml structure
            elif [[ "$name" == *_lib ]]; then
                # Set [lib] name to match runner's expected library name
                lib_name=$(grep 'library:' "$test_case/runner/src/main.rs" 2>/dev/null | sed 's/.*library: "\(.*\)".*/\1/' | head -1)
                if [[ -n "$lib_name" ]]; then
                    # Remove any existing [lib] section and rewrite it
                    sed -i '/^\[lib\]/,/^\[/{/^\[lib\]/d;/^name\|^crate-type/d;}' "$cargo_toml"
                    echo -e "\n[lib]\nname = \"$lib_name\"\ncrate-type = [\"cdylib\"]" >> "$cargo_toml"
                elif ! grep -q 'cdylib' "$cargo_toml"; then
                    echo -e "\n[lib]\ncrate-type = [\"cdylib\"]" >> "$cargo_toml"
                fi
            else
                # Set [[bin]] name = "driver"
                if grep -q '^\[\[bin\]\]' "$cargo_toml"; then
                    sed -i '/^\[\[bin\]\]/,/^\[/ s/^name = .*/name = "driver"/' "$cargo_toml"
                else
                    echo -e '\n[[bin]]\nname = "driver"\npath = "src/main.rs"' >> "$cargo_toml"
                fi
            fi
            translated=$((translated + 1))
            # Save original translation before any verification modifies it
            rm -rf "$out/translated_rust_original"
            cp -a "$out/translated_rust" "$out/translated_rust_original"
            echo "$name,success,$(date -Iseconds),${duration}" >> "$PROGRESS"
            echo "  ✅ $name (${duration}s) [$translated translated, $failed failed of $current/$total]"
        else
            failed=$((failed + 1))
            echo "$name,no_cargo_toml,$(date -Iseconds),${duration}" >> "$PROGRESS"
            echo "  ❌ $name — no Cargo.toml (${duration}s) [$translated translated, $failed failed of $current/$total]"
        fi
    else
        end_time=$(date +%s)
        duration=$((end_time - start_time))
        failed=$((failed + 1))
        echo "$name,error,$(date -Iseconds),${duration}" >> "$PROGRESS"
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
echo ""
echo "To test results (from test-corpus/deployment/scripts/github-actions/):"
echo "  PYTHONPATH=. python3 -m runtests.rust --root $OUTPUT_DIR --subset $OUTPUT_DIR --keep-going"

# --- Optional: C-as-oracle verification ---
if [[ "$VERIFY" == true ]]; then
    echo ""
    echo "========================================"
    echo "Running C-as-oracle verification..."
    verify_args=("$BATTERY")
    [[ -n "$FILTER" ]] && verify_args+=(--include-regex "$FILTER")
    "$SCRIPT_DIR/verify-translation.sh" "${verify_args[@]}"
fi
