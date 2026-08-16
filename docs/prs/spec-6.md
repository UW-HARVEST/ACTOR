# PR 6 — `run_cached<P>`: one cached execution path, verify ported onto it

## Goal

Extract the single generic driver both phases will call, and port verify onto it with its
behaviour bit-identical. This PR adds no caching to translate — it builds the thing PR 7
reuses — so its whole job is to change the *shape* of verify without changing what verify
does.

## Depends on PR 5

`Invocation`, `verify_invocation`, `IsolatedWorkDir` and the agent-exit family are in
`agents/` by the time this runs. If PR 5 has not landed, stop.

## The driver

The duplication PR 7 will remove is orchestration, not types: materialise, render prompt,
digest, `store.obtain`, restore log on replay, publish, write metrics. Extract exactly that
sequence, generic over `P: Phase`, into `agents/run.rs`.

```rust
/// Everything a cached agent phase needs, resolved BEFORE the agent runs so the key can
/// name it. A struct, not positional parameters: `case_dir` and `log_path` are both
/// `&Path` and `input_tree` and `c_before` are both `&TreeDigest`, so positional args
/// transpose silently and a transposed `input_tree` is a wrong key.
pub struct PhaseRun<'a, P: Phase> { ... }

/// The ONE place store.obtain is called. `compute` runs the agent and returns the sealed
/// artifact, or Ok(None) for "nothing worth keeping" (infra failure, non-compiling crate)
/// — the store keeps no entry for it, so a transient failure is never memoised permanent.
pub fn run_cached<P, F>(run: PhaseRun<'_, P>, store: &Store, compute: F) -> Result<Outcome<P>>
where P: Phase, F: FnOnce(WorkTree<P>) -> Result<Option<Produced<P>>>;
```

Design the exact fields and `Outcome` shape from what `verify_case` actually threads
through today — do not invent fields it does not need. `verify_case` then reduces to:
resolve the invocation, build `PhaseRun`, call `run_cached`, done. It must no longer contain
`store.obtain`, a publish call, a metrics write, or a replay branch — those move into the
driver.

## The two things that make this a real unification, not indirection

1. **Exactly one `store.obtain` call site in the whole crate after this PR.** Add an
   architecture rule asserting it: grep the token stream, and fail if `obtain` is called
   anywhere but `agents/run.rs`. Negative-test it against a planted second call. Without
   this, PR 7 could quietly add a second path and the "one driver" claim would be false.

2. **`KeyInputs.phase` derives from `P::DIR`, not a passed `&str`.** Today it is
   `phase: crate::battery::VERIFIED` at the call site, which can disagree with the `P` the
   store writes under. Make the driver set it from `<P as Phase>::DIR`. This is task #37 and
   it is key-preserving: `<Verify as Phase>::DIR == battery::VERIFIED == "verified"`, exactly
   the literal used today. Prove the key is unchanged; `SCHEMA` must not bump.

## Behaviour must not change

This is the highest-bar PR for the golden fingerprint, and the fingerprint alone is not
enough — it only covers the artifact tree, not the cache key or the replay path. So also:

- **The verify cache key for a fixed input is identical before and after.** Measure it the
  way PR 3b did: compute a key from fixed inputs on `origin/main` and on this branch, show
  the 256-bit values match. If they differ, this PR silently invalidates the 4 real cache
  entries already on disk, and translate's future entries — stop and report.
- **A replay still restores the verify log** and writes `replayed`/`cache_key` into the
  metrics. The `restore_log` path must survive the refactor; a test should cover a hit.
- Every existing verify test passes with its assertions unchanged.

## `Store::restore_log` signature

The map noted `restore_log(&self, inputs, key, dest)` cannot infer `P` once `entry_dir`
needs `P::DIR`, forcing a turbofish at every call. If you touch it, take `&Obtained<P>` so
`P` is inferred and the key provably belongs to that entry. Only if you touch it — do not
change it gratuitously.

## Constraints

- No visibility widening to make the move work; report rather than widen.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion, no
  `.stderr` re-record beyond a path/column shift (and if one shifts, diff it and confirm
  the pinned error code is intact).
- Do not write to `/tmp`. Use `/local/home/scheschb/scratch/<yours>`, delete it after.
- Report the DAG cycle membership; `translate ↔ verify` may shrink further here if the
  driver removes an edge. Shrink the baseline to what the rule prints.

## Acceptance criteria

Pinned toolchain, `RUSTUP_TOOLCHAIN` unset:

```
cargo fmt --check                                        clean
cargo test  --locked --lib --bin harvest-tools           all pass, count unchanged
cargo test  --locked --test architecture                 all pass (count = previous + 1, the obtain rule)
cargo test  --locked --test compile_fail                 10 cases
cargo clippy --locked --all-targets                      0 warnings
cargo clippy --locked --lib --bins -- -D clippy::panic   0
cargo doc   --locked --no-deps                           clean
python3 tools/comment_budget.py --max 14                 exit 0  (after `git add -A`)
python3 tools/check_paths.py                             exit 0
HARVEST_GOLDEN_RESULTS=<repo>/results cargo test --locked --manifest-path tools/Cargo.toml --test integration artifact_fingerprint
```

Plus the key-equivalence measurement above.

## Commit message

The driver's shape and why each `PhaseRun` field is needed; that exactly one `store.obtain`
call site now exists and is rule-enforced; that `phase` now derives from `P::DIR` and the
key is provably unchanged (`SCHEMA` steady); that a replay still restores the log; the DAG
membership; and 40 golden digests unchanged.
