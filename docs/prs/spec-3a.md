# PR 3a — Create `domain/` and move what is already pure

## Goal

Create the bottom layer of the target architecture and move into it only the items that
are *already* free of `std::fs`, `std::process` and `std::env`. Add the layer-purity rule
that keeps it that way. No behaviour change; every move is a pure move.

## What moves

Create `tools/src/domain/` as a directory module (`domain/mod.rs` re-exports; the module
doc states the one rule: nothing in here touches the filesystem, a process, or the
environment).

| from | to | why it is already pure |
|---|---|---|
| all of `src/scoring.rs` | `domain/outcome.rs` | 81 lines, zero fs/process/env |
| `RelPath` (+ `impl`, + its tests) from `artifact.rs` | `domain/relpath.rs` | validates a string, touches nothing |
| `Disposition`, `Carry`, `classify`, `is_ignored`, `BUILD_DIRS`, `ROOT_ONLY_IGNORED`, `is_cmake_build_dir` (+ their tests) from `artifact.rs` | `domain/contents.rs` | pure decisions over a `RelPath` and a bool |

Keep `pub use` re-exports from the old paths where that avoids touching call sites — but
do NOT leave a re-export that hides a layering violation. If a caller should now name
`domain::contents::classify`, change the caller.

## What does NOT move, and why

**The digest newtypes stay where their hashing is.** `TreeDigest`, and later the
`cache.rs` digests, are unforgeable *because* their tuple field is private and only their
own module can construct one. Moving `TreeDigest` to `domain/digest.rs` while `hash_tree`
stays behind would force a `pub(crate)` constructor, turning "only the hasher can make one"
into "any module can make one from a string" — which is exactly what
`digests_cannot_be_fabricated` exists to prevent. Same reasoning as the typestate family
living in one file: the private field is the mechanism, so the type lives with its only
legitimate constructor.

`Agent`, `Dataset` and `LogFormat` also stay for now. They are pure and belong in
`domain/` eventually, but `Agent` carries `clap` derive attributes and moving it is a
larger call about whether the parsing spelling travels with the type. Not this PR.

## The layer-purity rule

Add one architecture rule: no file under `src/domain/` may name `std::fs`, `std::process`
or `std::env`, in any spelling — including `use std::fs::…`, a bare `fs::`, `File::open`,
`Command::new`, `env::var`, `env!`, `std::io` on a real handle. Choose a detection method
you can defend and state its limits in the failure message.

It must be **negative-tested against the real extraction**, the way PR 2's DAG rule now
is: plant a temp tree containing a `domain/` file that names `std::fs` and assert the rule
reports it. A synthetic list is not sufficient — that was the hole PR 2 had to fix.

Also assert the rule cannot pass vacuously: it must fail if `src/domain/` contains no
files at all.

## The three filename-keyed rules — update them in the same commit

These key on the literal string `"artifact.rs"` and will panic with "not found" once
`classify` moves. `the_digest_path_is_lossless`'s failure message reads like a rule bug
rather than the rename that caused it, so it must be fixed here, not discovered later:

- `no_public_path_escapes_the_artifact_modules` — iterates `["artifact.rs", "cache.rs"]`
- `digests_cannot_be_fabricated` — iterates `["artifact.rs", "cache.rs"]`
- `the_digest_path_is_lossless` — names `("artifact.rs", "hash_tree" | "digest_tree" |
  "scrub" | "classify")`

Make them address code by module path rather than by leaf filename, so they keep working
as PRs 4–9 move more files. Whatever scheme you choose must still be able to *fail*: if a
guarded function disappears, the rule must say so loudly rather than find nothing and pass.

`classify` moving to `domain/contents.rs` means `the_digest_path_is_lossless` should guard
it there. Do not drop it from the guarded set.

## Constraints

- Every move is a **pure move**: the moved code is byte-identical apart from `use` lines
  and visibility needed to keep it compiling. Report anything you had to change beyond
  that, and why.
- No visibility may widen to make a move work. If a move would require making a private
  field or a private fn `pub(crate)`, that item is in the wrong layer — leave it where it
  is and say so.
- Do not add `#[allow]`, `#[expect]`, `#[ignore]`; do not weaken any rule; do not
  re-record any `.stderr`.
- Tests move with the code they test.

## Acceptance criteria

On the pinned toolchain with `RUSTUP_TOOLCHAIN` unset:

```
cargo fmt --check                                        clean
cargo test  --locked --lib --bin harvest-tools           180 passed (count unchanged)
cargo test  --locked --test architecture                 all pass, count = 12 + 1
cargo test  --locked --test compile_fail                 10 cases, 1 passed
cargo clippy --locked --all-targets                      0 warnings
cargo clippy --locked --lib --bins -- -D clippy::panic   0
cargo doc   --locked --no-deps                           clean
python3 tools/comment_budget.py --max 14                 exit 0  (after `git add -A`)
python3 tools/check_paths.py                             exit 0
```

Plus, and this is the point of PR 2's fingerprint existing:

```
HARVEST_GOLDEN_RESULTS=/local/home/scheschb/research/ACTOR/results \
  cargo test --locked --test integration artifact_fingerprint
```

must pass **and must not skip** — 40 cases compared, digests identical. A pure move that
changes an artifact digest is the failure this whole plan risks.

`a_module_cycle_may_only_shrink` must still pass. Report the cycle membership it prints if
it changed; moving pure leaves out of `artifact.rs` may or may not affect it, and either
outcome is information.

## Known flake

`a_runner_that_errors_is_not_scored_from_the_file_it_left` fails with `ETXTBSY` roughly 1
run in 97 — pre-existing, unrelated, in code this PR does not touch. If you hit it, re-run
and say so. Do not fix it here.

## Commit message

Say what moved, that the moves are byte-identical apart from `use` lines, why the digest
newtypes did NOT move, and that the golden fingerprint confirms 40 artifact digests
unchanged.
