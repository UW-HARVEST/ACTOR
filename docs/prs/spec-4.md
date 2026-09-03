# PR 4 — Create `io/`, the only layer allowed to touch the outside world

## Goal

Second layer of `docs/architecture-plan.md`. Everything that touches the filesystem, a
process, the environment or git moves into `io/`. Pure moves only; no behaviour change.

## Why

`domain/` (PR 3a) is enforced pure by a rule. The complement is not enforced at all:
filesystem and process access is scattered across `artifact.rs`, `workdir.rs`,
`sandbox.rs` and `provenance.rs`, so "conversion at the edges" is a description of
intent rather than a property. Naming the layer is what lets a later rule say *only* `io/`
may do these things.

## What moves

Create `tools/src/io/` as a directory module. `io/mod.rs` re-exports and states the rule in
its module doc: this is the only layer that may name `std::fs`, `std::process`,
`std::env`, or shell out to git.

| from | to | what it is |
|---|---|---|
| all of `workdir.rs` | `io/workdir.rs` | scratch base, ulimits, tmpfs refusal |
| all of `sandbox.rs` | `io/sandbox.rs` | writes `settings.json`, probes PATH |
| ~~the git plumbing in `provenance.rs`~~ | — | **not moved**: see below |

### Corrected after the first attempt: the tree walking does NOT move

The first attempt also moved `visit`, `hash_tree`, `digest_tree`, `feed`, `copy_carrying`,
`Access` and `set_read_only` to `io/tree.rs`. Doing so required widening `digest_tree`,
`visit` and `copy_carrying` from private to `pub(crate)`, and the implementer then rewrote
`artifact.rs`'s module doc from "Three invariants are enforced by the compiler" to "Two",
conceding that "any module can now hash a raw path".

That third invariant is **a tree cannot be hashed before it is scrubbed**. Agent output
embeds the random scratch directory name, so a digest of unscrubbed output differs every
run — a cache keyed on one looks enabled and never hits, silently re-paying the agent cost.
`digest_tree` being private in `artifact.rs` is what enforced it: the only routes to a
`TreeDigest` of a work tree ran through `Scrubbed::seal` / `Sealed::adopt` /
`Sealed::from_cache`, all in that one file.

So these stay in `artifact.rs`, for the same reason `TreeDigest` does and for the same
reason the typestate family lives in one file: **the privacy IS the enforcement
mechanism**. An item whose private visibility carries an invariant cannot be moved to a
lower layer, because the move is what breaks it. `io/` gets the genuinely external things —
scratch bases, sandbox policy files, git subprocesses — not the implementation of the
artifact pipeline's own transitions.

`provenance::assess` is already pure and stays put for now — PR 3b decides whether it
moves to `domain/`. Do not move it here.

## What does NOT move, and why

**`TreeDigest` stays with the hashing, and the hashing stays in `artifact.rs`** — see the
correction above. PR 3a established the principle: the newtype is unforgeable because its
tuple field is private and only its own module can construct one.

**The typestate family stays in `artifact.rs`.** `Scratch`, `WorkTree`, `Scrubbed`, `Sealed`,
`Corpus`, `CDir` and their transitions are one state machine and one concept. They will
*call* `io/tree.rs`; they do not move into it. If a transition's private field would have to
widen for that to compile, report it — do not widen.

## Constraints

- Every move is a **pure move**: byte-identical apart from `use` lines and the module it
  lives in. Report anything you changed beyond that and why.
- No visibility may widen to make a move work. If a move requires it, that item is in the
  wrong layer — leave it and say so.
- Do not add `#[allow]`/`#[expect]`/`#[ignore]`, weaken any rule, grow any ALLOWED list, or
  re-record any `.stderr`.
- Tests move with the code they test.
- Do not write to `/tmp` (see the pipeline's standing instruction).

## Rules you will have to update in the same commit

- **`the_digest_path_is_lossless`** needs NO repointing: with the hashing staying in
  `artifact.rs`, none of the modules it guards moved. Confirm that rather than assuming it.
- **`the_shape_rules_cannot_pass_while_inspecting_nothing`** has `MIN_FILES` set to the
  measured file count minus 2. You are adding files. Raise it and keep the 2-file margin,
  and update its comment with the new measured count.
- **`no_public_path_escapes_the_artifact_modules`** and **`digests_cannot_be_fabricated`**
  address the artifact and cache modules by module path. `io/tree.rs` now holds
  `TreeDigest` and path-returning helpers — decide deliberately whether each rule should
  cover `io/` too, and say why in the commit message. Getting this wrong silently drops
  coverage.
- **`a_module_cycle_may_only_shrink`** — report the cycle membership it prints if it
  changes. Either outcome is information; do not edit the baseline to make it quiet unless
  the cycle genuinely shrank, and say so if it did.

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
```

And the one that matters most for a move PR:

```
HARVEST_GOLDEN_RESULTS=/local/home/scheschb/research/ACTOR/results \
  cargo test --locked --test integration artifact_fingerprint
```

must pass **and must not skip** — 40 artifact digests identical. A pure move that changes a
digest is the failure this plan risks; the hashing code itself is what you are moving, so
this is the direct check on it.

## Commit message

What moved; that the moves are byte-identical apart from `use` lines; why `TreeDigest`
travelled with `hash_tree`; what you decided about extending the two artifact-module rules
to `io/` and why; and that 40 golden digests are unchanged.

## Outcome: `io/` is smaller than this spec first said

`io/` contains `workdir.rs` and `sandbox.rs`. The git plumbing stayed in `provenance.rs`,
because extracting any nonempty subset of `{git, repo_root, head_sha, count_dirty_in,
dirty_file_count}` requires at least three widenings — `head_sha` and `dirty_file_count`
have callers that stay, and `BEHAVIOUR_PATHS` is read by `impl Display for Unreproducible`
as well as by `count_dirty_in`.

That was allowed to stand rather than widened. `provenance.rs` is one cohesive concept —
which code produced this result, and refuse to measure if we cannot say — and splitting it
three ways to satisfy a layer diagram is the sprawl this plan exists to remove. `io/` is for
items whose only job is touching the outside world, not for every call that happens to.
