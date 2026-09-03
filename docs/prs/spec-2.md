# PR 2 — Guards: make the shape rules unable to go silent, and start the DAG ratchet

## Goal

Three additions, no production behaviour change. This PR is the hard prerequisite for
PRs 3–9: no file may move into a subdirectory until item 1 exists.

## Why

`rust_sources()` in `tools/tests/architecture.rs` does a flat `read_dir` of `src/` and
filters by `.rs` extension. The moment `artifact.rs` becomes `artifact/tree.rs`, every
rule that iterates it stops seeing that code and **reports green while inspecting
nothing** — including `sealed_implements_only_debug`, which guards the invariant that
nothing executes in a published artifact.

## 1. `rust_sources()` recurses, and cannot go quiet

Make `rust_sources()` walk `src/` recursively, returning every `.rs` file at any depth,
sorted deterministically.

Then add ONE new test that fails if the rule set ever stops seeing the code it inspects.
It must assert all three of:

- a minimum file count (use today's actual count minus a small margin, and say in a
  comment what today's count is);
- that a set of named types the rules depend on are actually found somewhere in the
  returned files — at minimum `Sealed`, `WorkTree`, `Scrubbed`, `Corpus`, `TreeDigest`;
- that at least one file is found at depth > 1 **once such a file exists**. Today none
  does, so this third assertion must be written so it is *inert now and live later*
  without being a silent no-op: assert that the count of nested files equals the count
  of nested files on disk, computed independently in the test.

Name it after the failure, not the function.

## 2. The module-graph DAG rule

Add one rule asserting the crate's module dependency graph is acyclic, with today's
cycle recorded as a **shrink-only baseline**.

Mechanics:

- For each file under `src/`, derive its top-level module: `src/foo.rs` → `foo`,
  `src/foo/mod.rs` → `foo`, `src/foo/bar.rs` → `foo`. Ignore `lib.rs` and `main.rs`.
- An edge `a → b` exists if module `a`'s source references `crate::b` (including the
  grouped form `use crate::{b, c}`).
- Compute strongly connected components. Any SCC with more than one member is a cycle.

Today's measured baseline is a **single 10-module cycle**:

```
agent_health, artifact, battery, cache, cargo_toml,
cli, opencode, session, translate, verify
```

Record that as a `const` baseline. The rule must fail if:

- the largest cycle has **more** members than the baseline, or
- any module appears in a cycle that is **not** in the baseline list.

It must NOT fail merely because the cycle shrank — shrinking is the goal. But it must
fail if the baseline goes stale, i.e. if the recorded list names a module that is no
longer in any cycle, so the baseline cannot silently over-permit. Emit the current cycle
membership in the failure message either way, so the next PR can update the baseline by
reading the output.

Negative-test it: prove the rule detects a cycle that is not in the baseline. Do this
without editing production code — e.g. by factoring the SCC computation so a test can
feed it a synthetic edge map.

## 3. Golden digest fingerprint

Add a test to `tools/tests/integration.rs` (which CI does not run, because it needs the
`results/` and `test-corpus` submodules) that pins the artifact pipeline's output.

- Walk cases under `results/` that have a `translated/Cargo.toml`.
- For each, compute `Sealed::<Translate>::adopt(case)?.digest()`.
- Compare against a committed `tools/tests/golden-digests.json` mapping case path →
  digest string.
- Generate that file in this PR from the current tree. Cap it at the first 40 cases in
  sorted order so it stays reviewable, and record in the JSON how many cases were
  considered.

Follow the existing convention in `integration.rs` for an absent submodule (it prints
`Skipping: …` and returns). But it must not be able to pass vacuously: when `results/`
IS present, assert the number of cases compared is at least 40, and fail if the golden
file is empty.

## What must NOT change

- No file under `tools/src/` may change. This PR is tests and one JSON fixture only.
- The other rules keep their current semantics. You may only change `rust_sources()`'s
  traversal, not what any rule asserts.
- Do not add `#[allow]`, `#[expect]`, `#[ignore]`, or grow any `ALLOWED` list.
- Do not re-record any `.stderr`.

## Acceptance criteria

On the pinned toolchain with `RUSTUP_TOOLCHAIN` unset:

```
cargo test  --locked --test architecture         all pass (count = previous + 2)
cargo test  --locked --lib --bin harvest-tools   all pass, count unchanged
cargo test  --locked --test compile_fail         9 cases, 1 passed
cargo clippy --locked --all-targets              0 warnings
cargo clippy --locked --lib --bins -- -D clippy::panic   0
cargo doc   --locked --no-deps                   clean
cargo fmt --check                                clean
python3 tools/comment_budget.py --max 13         exit 0   (after `git add -A`)
python3 tools/check_paths.py                     exit 0
```

Plus, run and report:

```
cargo test --locked --test integration           golden fingerprint test passes
git diff --stat origin/main -- tools/src          MUST be empty
```

## Commit message

State that `rust_sources()` was flat and would have made every shape rule pass while
inspecting nothing once files moved, give the measured cycle membership the DAG baseline
records, and name how many cases the golden fingerprint covers.
