#!/usr/bin/env bash
# One-shot, idempotent migration of the results/ submodule to the uniform per-case
# phase-dir layout: translated/ = pre-verify crate, verified/ = post-verify crate and
# present only where a verify ran. In the OLD Test-Corpus layout translated_rust/ is the
# post-verify crate and translated_rust_original/ the pre-verify one -- but only for
# cases that ran verify; translate-only agents have the two identical.
set -euo pipefail

DRY=0
[[ "${1:-}" == "--dry-run" ]] && DRY=1

say() { printf '%s\n' "$*"; }
run() { if [[ $DRY == 1 ]]; then say "  [dry-run] $*"; else eval "$*"; fi; }

move_dir() {
  local src="$1" dst="$2"
  [[ -d "$src" ]] || return 0
  [[ -e "$dst" ]] && { say "  skip (exists): $dst"; return 0; }
  run "mkdir -p \"$(dirname "$dst")\""
  run "mv \"$src\" \"$dst\""
}

move_log() {
  local src="$1" phase_dir="$2" name="$3"
  [[ -f "$src" ]] || return 0
  run "mkdir -p \"$phase_dir/logs\""
  run "cp \"$src\" \"$phase_dir/logs/$name\""
}

migrate_test_corpus_case() {
  local case_dir="$1"
  local tr="$case_dir/translated_rust"
  local orig="$case_dir/translated_rust_original"
  local translated="$case_dir/translated"
  local verified="$case_dir/verified"

  [[ -d "$translated" || -d "$verified" ]] && { say "  skip (migrated): $case_dir"; return 0; }
  [[ -d "$tr" || -d "$orig" ]] || return 0   # nothing to migrate

  # Two signals that verify produced a crate: logs/verify.log (only verify writes it),
  # or translated_rust differing from _original — which catches shared-source config
  # FOLLOWERS, carrying a propagated post-verify crate but no verify.log of their own.
  local verify_ran=0
  if [[ -f "$case_dir/logs/verify.log" ]]; then
    verify_ran=1
  elif [[ -d "$orig" && -d "$tr" ]] && ! diff -rq "$orig" "$tr" >/dev/null 2>&1; then
    verify_ran=1
  fi

  if [[ $verify_ran == 1 && -d "$orig" ]]; then
    move_dir "$orig" "$translated"
    move_dir "$tr"   "$verified"
    move_log "$case_dir/logs/translation.log" "$translated" "translation.log"
    move_log "$case_dir/logs/verify.log"      "$verified"   "verify.log"
    # The case-root result.json scores the crate that ran last, i.e. the post-verify one.
    [[ -f "$case_dir/result.json" ]] && run "cp \"$case_dir/result.json\" \"$verified/result.json\""
  else
    # $orig and $tr are identical here, so discarding $tr loses nothing.
    if [[ -d "$orig" ]]; then move_dir "$orig" "$translated"; run "rm -rf \"$tr\"";
    else move_dir "$tr" "$translated"; fi
    move_log "$case_dir/logs/translation.log" "$translated" "translation.log"
    [[ -f "$case_dir/result.json" ]] && run "cp \"$case_dir/result.json\" \"$translated/result.json\""
  fi
  # logs/ and result.json were copied into the phase dirs above; _original was moved.
  run "rm -rf \"$case_dir/translated_rust_original\" \"$case_dir/logs\" \"$case_dir/result.json\""
}

migrate_harvest_bench_case() {
  local case_dir="$1"
  move_dir "$case_dir/translated_rust" "$case_dir/translated"
  [[ -f "$case_dir/result.json" ]] && { run "mkdir -p \"$case_dir/translated\""; run "mv \"$case_dir/result.json\" \"$case_dir/translated/result.json\""; }
  [[ -d "$case_dir/logs" ]] && move_dir "$case_dir/logs" "$case_dir/translated/logs"
}

fold_kiro_translate() {
  # The kiro-translate pseudo-agent holds pre-verify snapshots copied from kiro, so
  # folding is a no-op wherever kiro already has a translated/.
  local kt="results/Test-Corpus/kiro-translate"
  local kiro="results/Test-Corpus/kiro"
  [[ -d "$kt" ]] || return 0
  say "Folding kiro-translate pseudo-agent into kiro/…/translated/"
  while IFS= read -r -d '' tr; do
    local rel="${tr#"$kt"/}"                 # <battery>/<case>/translated_rust
    local case_rel="$(dirname "$rel")"       # <battery>/<case>
    local dst="$kiro/$case_rel/translated"
    if [[ -d "$dst" ]]; then
      say "  skip (kiro already has translated/): $case_rel"
    else
      move_dir "$tr" "$dst"
      move_log "$kt/$case_rel/logs/translation.log" "$dst" "translation.log"
    fi
    local kt_rj="$kt/$case_rel/result.json"
    [[ -f "$kt_rj" && ! -f "$dst/result.json" ]] && { run "mkdir -p \"$dst\""; run "cp \"$kt_rj\" \"$dst/result.json\""; }
  done < <(find "$kt" -type d -name translated_rust -print0 2>/dev/null)
  # kiro-translate/<battery>/summary.json IS the pre-verify battery summary; landing it
  # as summary_translated.json lets report.rs read a committed number with no re-run.
  for bat in "$kt"/*/; do
    [[ -d "$bat" ]] || continue
    local bname; bname="$(basename "$bat")"
    local src="$bat/summary.json"
    local dst="$kiro/$bname/summary_translated.json"
    [[ -f "$src" && ! -f "$dst" ]] && { run "mkdir -p \"$kiro/$bname\""; run "cp \"$src\" \"$dst\""; }
  done
  run "rm -rf \"$kt\""
}

[[ -d results ]] || { echo "error: run from the ACTOR root (results/ submodule must be checked out)"; exit 1; }

say "== Test-Corpus (per-agent, per-battery, per-case) =="
for agent in results/Test-Corpus/*/; do
  [[ "$agent" == *"/kiro-translate/" ]] && continue   # handled by fold step
  for case_dir in "$agent"*/*/; do
    [[ -d "$case_dir" ]] && migrate_test_corpus_case "${case_dir%/}"
  done
done
fold_kiro_translate

say "== HarvestBench =="
for case_dir in results/HarvestBench/*/*/; do
  [[ -d "$case_dir" ]] && migrate_harvest_bench_case "${case_dir%/}"
done

say ""
say "Migration complete. Next:"
say "  1. Verify:  tools/reproduce.sh all   (replays from the cache and diffs tables/)"
say "     (the no-validate summary was migrated to kiro/<bat>/summary_translated.json"
say "      from the old kiro-translate pseudo-agent; no re-run needed)."
say "  2. Commit inside results/ submodule, then bump the pointer in ACTOR."
