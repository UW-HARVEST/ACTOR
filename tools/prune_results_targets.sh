#!/usr/bin/env bash
# Delete the regenerable cargo build output under results/, and prove the deletion
# changed nothing else.
#
# `target/` is `BuildOutput` in tools/src/artifact.rs: no `Carry` variant admits it and
# the tree digest skips it, so no recorded result depends on it.
#
# Cargo.lock is NOT a candidate and never will be: it is `StoreAndHash` — part of the
# hashed artifact — and results/.gitignore ignores it, so git cannot put one back. Every
# `target/` here was checked to contain none; `prune` re-checks per directory and skips
# any that does.
#
# The manifest deliberately cannot see inside a `target/`, so it proves only that nothing
# OUTSIDE one changed. What protects a source directory that merely happens to be named
# `target` is the per-directory `.rustc_info.json` check below.
#
#   record         (re)write the manifest of every crate dir that owns a target/
#   check          recompute those digests and diff against the manifest
#   prune --yes    check, delete, check again
#
# Not run by CI or by harvest-tools: running it is the operator's call.
set -euo pipefail

here=$(cd -- "$(dirname -- "$0")" && pwd)
results=${HARVEST_RESULTS_DIR:-$here/../results}
manifest=$here/prune_results_targets.baseline

die() { printf '%s\n' "$*" >&2; exit 1; }

[ -d "$results" ] || die "no results tree at $results (set HARVEST_RESULTS_DIR)"
results=$(cd -- "$results" && pwd)

crate_dirs() { find . -type d -name target -prune -printf '%h\n' | LC_ALL=C sort -u; }

# Hashes path+content of every file and every symlink outside ANY target/, so a nested
# crate's build output cannot leak into its parent's digest and look like a change.
digest_of() {
    local dir=$1
    {
        find "$dir" -type d -name target -prune -o -type f -print0 |
            LC_ALL=C sort -z | xargs -0 -r sha256sum
        find "$dir" -type d -name target -prune -o -type l -printf '%p -> %l\n' |
            LC_ALL=C sort
    } | sha256sum | cut -d' ' -f1
}

digests_of_stdin() {
    local dir
    cd -- "$results"
    while read -r dir; do
        if [ -d "$dir" ]; then
            printf '%s  %s\n' "$(digest_of "$dir")" "$dir"
        else
            printf 'MISSING  %s\n' "$dir"
        fi
    done
}

recorded_dirs() {
    [ -f "$manifest" ] || die "no manifest at $manifest — run: $0 record"
    grep -v '^#' -- "$manifest" | cut -d' ' -f3-
}

record() {
    local dirs
    dirs=$( cd -- "$results" && crate_dirs )
    printf '%s\n' "$dirs" | grep -q '[[:space:]]' &&
        die "a crate dir name contains whitespace; the manifest format cannot express it"
    {
        printf '# %s: sha256 over every file outside target/, one line per crate dir\n' "${0##*/}"
        printf '# recorded: %s from %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$results"
        printf '# results HEAD: %s\n' "$(git -C "$results" rev-parse HEAD 2>/dev/null || echo unknown)"
        printf '# crate dirs: %s, target/ total: %s MB\n' \
            "$(printf '%s\n' "$dirs" | wc -l)" \
            "$( cd -- "$results" && find . -type d -name target -prune -print0 |
                 du -sm --files0-from=- 2>/dev/null | awk '{s+=$1} END {print s+0}' )"
        printf '%s\n' "$dirs" | digests_of_stdin
    } > "$manifest"
    printf 'wrote %s\n' "$manifest"
}

check() {
    if diff -u <(grep -v '^#' -- "$manifest") <(recorded_dirs | digests_of_stdin); then
        printf 'manifest matches: no file outside target/ differs\n'
    else
        die "the tree no longer matches $manifest — investigate before pruning"
    fi
}

prune() {
    [ "${1:-}" = "--yes" ] || die "prune deletes GBs of build output; pass --yes"
    check
    local dir deleted=0 skipped=0
    while read -r dir; do
        local t=$results/${dir#./}/target
        [ -d "$t" ] || continue
        if [ ! -f "$t/.rustc_info.json" ]; then
            printf 'skip, not a cargo target dir: %s\n' "$t"; skipped=$((skipped + 1)); continue
        fi
        if [ -n "$(find "$t" -name Cargo.lock -print -quit)" ]; then
            printf 'skip, holds a Cargo.lock: %s\n' "$t"; skipped=$((skipped + 1)); continue
        fi
        rm -rf -- "$t"
        deleted=$((deleted + 1))
    done < <(recorded_dirs)
    printf 'deleted %s target/ dirs, skipped %s\n' "$deleted" "$skipped"
    check
}

case "${1:-}" in
    record) record ;;
    check)  check ;;
    prune)  shift; prune "$@" ;;
    *)      die "usage: ${0##*/} record|check|prune --yes" ;;
esac
