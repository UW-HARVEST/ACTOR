# PR 9 — Create `oracle/` and `analyse/`

## Goal

Separate "grade a translation against its reference" from "measure and report on a
translation". Two subsystems of `docs/architecture-plan.md`, moved in one PR because they
are mutually disjoint and both disjoint from the rest of the tree.

## Why this can run in parallel with PR 4

Measured: `test.rs`, `report.rs` and `cargo_toml.rs` reference **none** of
`crate::artifact`, `crate::workdir`, `crate::sandbox` or `crate::provenance` — the modules
PR 4 moves. The single edge between the two sets is `benchmark.rs → crate::sandbox`.

That means the two PRs will not fight over source files, but they WILL both touch
`tools/tests/architecture.rs` (`MIN_FILES`, module-path keys) and both add comment lines to
a whole-tree ratio. Whichever merges second gets rebased and re-verified. Write your commit
message so it does not assume you are first.

## What moves

Create `tools/src/oracle/` and `tools/src/analyse/`, each a directory module whose `mod.rs`
re-exports and states in one line what the subsystem is for.

**`oracle/` — running a reference and scoring against it:**

| from | to |
|---|---|
| the MIT `runtests` driving in `test.rs` | `oracle/runtests.rs` |
| the harvest-bench gtest driving in `test.rs` (`score_harvest_bench_suite` and its helpers, `HarvestBenchResult`) | `oracle/gtest.rs` |
| the scoring/aggregation half of `test.rs` | `oracle/score.rs` |

`test.rs` is 1,291 production lines and does all three. Split it along those lines. If a
piece genuinely serves more than one, put it in `oracle/mod.rs` and say why.

**`analyse/` — measuring an artifact and reporting on it:**

| from | to |
|---|---|
| all of `report.rs` | `analyse/report.rs` |
| all of `cargo_toml.rs` | `analyse/cargo_toml.rs` |
| the unsafe-count / LOC metric code, wherever it currently lives | `analyse/metrics.rs` |

Find the metric code rather than assuming: it is referenced from reporting and from
scoring, and may already be in one of the files above. Report where you found it.

`benchmark.rs` stays where it is. It is the CLI-facing dataset driver, not an oracle, and
it is the one file with an edge into PR 4's territory.

## Constraints

- Every move is a **pure move**: byte-identical apart from `use` lines and the module it
  lives in. `test.rs` is being *split*, which means deciding where each item goes — but no
  item's body may change. Report anything you altered beyond `use` lines and why.
- No visibility may widen to make a move work. If splitting `test.rs` would require making a
  private helper `pub(crate)` across the new boundary, that tells you the split line is
  wrong — put those items in the same module and say so.
- Do not add `#[allow]`/`#[expect]`/`#[ignore]`, weaken any rule, grow any ALLOWED list,
  change what any test asserts, or re-record any `.stderr`.
- Tests move with the code they test.
- Do not write to `/tmp` (see the pipeline's standing instruction).

## Rules you will have to update in the same commit

- **`only_battery_defines_the_has_crate_predicate`** — `test.rs` is one of the modules this
  polices. Repoint it at the new module paths; it must still fail if a second definition of
  the predicate appears.
- **`nothing_new_runs_inside_the_results_tree`** has `KNOWN = 2` and its one current hit is
  in `test.rs`. After the split that hit lives somewhere new. Keep the count honest: do not
  raise `KNOWN`, and report where the hit now is.
- **`the_shape_rules_cannot_pass_while_inspecting_nothing`** — you are adding files. Raise
  `MIN_FILES`, keep the 2-file margin, update its comment with the measured count.
- **`a_module_cycle_may_only_shrink`** — report the cycle membership if it changes. Do not
  edit the baseline to silence it unless the cycle genuinely shrank; say so if it did.

## Acceptance criteria

Pinned toolchain, `RUSTUP_TOOLCHAIN` unset:

```
cargo fmt --check                                        clean
cargo test  --locked --lib --bin harvest-tools           all pass, count unchanged
cargo test  --locked --test architecture                 all pass
cargo test  --locked --test compile_fail                 10 cases
cargo clippy --locked --all-targets                      0 warnings
cargo clippy --locked --lib --bins -- -D clippy::panic   0
cargo doc   --locked --no-deps                           clean
python3 tools/comment_budget.py --max 14                 exit 0  (after `git add -A`)
python3 tools/check_paths.py                             exit 0
HARVEST_GOLDEN_RESULTS=<repo>/results cargo test --locked --test integration artifact_fingerprint
```

The fingerprint must pass and must not skip.

## Commit message

What moved and how `test.rs` was split; that the moves are byte-identical apart from `use`
lines; where the `nothing_new_runs_inside_the_results_tree` hit now lives; and any place you
declined to split because it would have required widening visibility.
