# PR 0 — Delete four shape rules and their allowlists

## Goal

Remove the four rules in `tools/tests/architecture.rs` that cost the most and catch the
least, plus the helpers that become dead. Net removal of roughly 341 lines of rule
bodies. Ten rules remain and must all still pass.

## Why

Three of the four carry **closed, shrink-only `ALLOWED` lists checked in both
directions** — they fail if a listed entry goes stale as well as if a new violation
appears. They name `translate_case`, `verify_case`, `dispatch_translate`,
`post_process_independent`, `write_translation_metrics`, `kimi_translate_case`,
`oneshot_translate_case`, `oneshot_llm_translate`, `c2saferrust_translate_case`,
`propagate_config_phase` and `invoke_codex_with_retry`.

Every PR from 3 to 9 of `docs/architecture-plan.md` moves or resignatures at least one of
those, so every one of those PRs would have to edit `architecture.rs` in lockstep. That
tax is paid to enforce signature patterns, while the rules that replace them (a
module-graph DAG rule and a layer-purity rule, PR 2) enforce structure in ~20 lines each
with no allowlist.

## Exactly what to remove

Delete these four `#[test]` functions entirely, including their doc comments, their
`const ALLOWED` / `const PRIMITIVES` tables and their staleness assertions:

1. `money_amounts_cannot_be_substituted_for_one_another`
2. `safety_gating_bools_are_named_enums`
3. `no_function_takes_three_interchangeable_primitives`
4. `a_tuple_return_may_not_repeat_an_element_type`

Then delete these helpers, which become dead once those four are gone:

- `is_bool_to_enum_boundary`
- `collect_enums_and_impls`
- `PRIMITIVES`

## What must NOT change

- **The other ten rules stay exactly as they are.** Do not modify, weaken, rename or
  reorder them. They are:
  `sealed_implements_only_debug`, `no_public_path_escapes_the_artifact_modules`,
  `digests_cannot_be_fabricated`,
  `compile_fail_cases_still_assert_what_they_were_written_for`,
  `nothing_new_runs_inside_the_results_tree`, `only_battery_defines_the_has_crate_predicate`,
  `an_agents_identity_is_never_its_debug_output`,
  `the_key_deriving_functions_keep_their_exhaustive_patterns`,
  `the_digest_path_is_lossless`,
  `typestates_have_private_fields_and_consuming_transitions`.
- **Keep the helpers the remaining ten use**: `ty_key`, `Func`, `signatures`,
  `method_calls`, `params`, `returned_ty`, `Param`, `quote_min`, `is_pathish`,
  `type_name`, `is_public`, `parse`, `src`, `rust_sources`, `file_name`,
  `TYPESTATE_ORDER`. Verify by compiling — an unused-helper warning is an error here
  because `[lints.rust] warnings = "deny"`.
- **No production code under `tools/src/` may change.** This PR touches
  `tools/tests/architecture.rs` and documentation only.
- Do not add any `#[allow]`, `#[expect]` or `#![allow]`.

## Documentation to update

In `docs/architecture-plan.md`, the "What each PR actually does" entry for PR 0 states
the removal. Update its line counts to whatever is actually removed, measured, and mark
PR 0 as landed. Do not restructure the document.

## Acceptance criteria

All of these, on the pinned toolchain with `RUSTUP_TOOLCHAIN` unset:

```
cargo test  --locked --test architecture          10 passed, 0 failed
cargo test  --locked --lib --bin harvest-tools    all pass, count unchanged from main
cargo test  --locked --test compile_fail          9 cases, 1 passed
cargo clippy --locked --all-targets               0 warnings
cargo clippy --locked --lib --bins -- -D clippy::panic   0
cargo doc   --locked --no-deps                    clean
cargo fmt --check                                 clean
python3 tools/comment_budget.py --max 13          exit 0
python3 tools/check_paths.py                      exit 0
```

Additionally: `git diff --stat origin/main` must show a **net line reduction** in
`tools/tests/architecture.rs`, and must show **no changes at all** under `tools/src/`.

## Commit message

Explain that the four rules were removed because their bidirectional shrink-only
allowlists taxed every subsequent refactor PR, name the measured line reduction, and
state which ten rules remain. Do not claim the syn framework was removed — most of it is
still used.
