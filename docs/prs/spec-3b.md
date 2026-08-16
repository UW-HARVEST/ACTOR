# PR 3b — The two boundary cuts: move the reads to the edge

## Goal

Two functions currently reach out to the world from the middle of the logic. Make each take
what it needs as a value, so the reading happens at the edge and the deciding is pure. Then
the pure parts can live in `domain/`.

This is the change `CLAUDE.md`'s first principle names by example.

## Cut 1 — `agent_health::classify` takes text, not a path

`classify(log: &Path, format: LogFormat, exit: Exit)` reads the file itself. It is an edge
disguised as logic: to test it you must create a tempdir and write a fixture, which is why
`agent_health.rs` has ~17 `test_tempdir()` calls for what is fundamentally string analysis.

Change it to take the already-read text. The read moves to the caller. Keep exactly one
place that does the reading — `read_tail` already exists and is the natural home; the
audit-time entry point `classify_log` becomes the thin path-taking wrapper over it.

Then move the pure vocabulary to `domain/health.rs`: `Health`, `Completed`, `LogFormat`,
`Exit`, and `classify` itself.

**`Completed` must keep its invariant.** Its private unit field is what makes
`Health::completed()` the only constructor, which is what makes `Scrubbed::seal` unable to
seal an infra-failed run. `Completed`, `Health` and `classify` must therefore move
**together** — the same rule PR 4 established: an item whose privacy carries an invariant
travels with its only legitimate constructor. If any part of that family would have to stay
behind or widen, stop and report instead.

`artifact.rs` will then reference `domain::health::Completed` rather than
`agent_health::Completed`. That is the point: `domain/` is below `artifact`, so the
dependency runs one way.

What stays in `agent_health.rs` (or moves to `io/`, your call — say which and why): the
filesystem walking (`audit`, `collect`), `read_tail`, `exit_code`, and the
`describe_infra_failures` / `record_infra_failures` reporting.

## Cut 2 — `cache::normalise` takes its roots

`normalise(text, work_root, repo_root)` also reads `io::workdir::base()` and
`std::env::var_os("HOME")` internally. So a key-affecting function silently depends on the
environment, and `cache.rs` cannot be reasoned about without knowing what `HOME` was.

Change it to take every root it substitutes. Four paths as positional parameters would be
transposable and a transposed root is a wrong cache key, so pass them as one named struct
with named fields — the same reasoning `PhaseRun` and `sandbox::Policy` use.

The resolution of those roots moves to the caller, which is already the place that knows
them. `normalise` becomes pure and its tests need no environment.

Do NOT change what the function computes. The substitution set, the longest-first ordering
and the `$WORK`/`$REPO`/`$WORKBASE`/`$HOME` token spellings must be identical, and the
existing tests — `normalise_removes_every_machine_specific_path` and
`prompt_digest_is_machine_independent` — must still pass unchanged in what they assert.
**A change to the normalisation changes every cache key**, so prove equivalence: state
whether `cache::SCHEMA` needs a bump, and if you believe it does not, say what you checked.

## Report the true cycle membership

`a_module_cycle_may_only_shrink`'s baseline records 10 modules. If these cuts remove edges,
the rule will fail with "X is in no cycle any more" and print the current membership. Shrink
the baseline to exactly what it prints — that is the ratchet working — and quote both the old
and new membership in the commit message.

**Do not trust `docs/architecture-plan.md`'s "ten mutual pairs" list.** It was produced by a
regex over raw text, which matches `crate::foo` inside doc comments; the rule token-lexes via
`proc_macro2`, where a doc comment is a string literal and creates no edge. The rule is
right and the document is wrong. If you can determine which pairs are real, correct that
section of the plan in this PR.

## Constraints

- No visibility may widen. If a cut requires it, report rather than widen.
- Do not add `#[allow]`/`#[expect]`/`#[ignore]`, weaken any rule, grow any ALLOWED list, or
  re-record any `.stderr`.
- Do not change what any test asserts. Tests may lose their tempdir setup — that is the
  point of cut 1 — but the assertions stay.
- Keep `domain/` pure: the layer-purity rule will reject a `std::fs` reference in
  `domain/health.rs`, which is the check that this cut actually happened.
- `MIN_FILES` must match the measured count minus 2.
- Do not write to `/tmp`. Use `/local/home/scheschb/scratch/<yours>` and delete it after.

## Acceptance criteria

Pinned toolchain, `RUSTUP_TOOLCHAIN` unset:

```
cargo fmt --check                                        clean
cargo test  --locked --lib --bin harvest-tools           all pass
cargo test  --locked --test architecture                 all pass
cargo test  --locked --test compile_fail                 10 cases
cargo clippy --locked --all-targets                      0 warnings
cargo clippy --locked --lib --bins -- -D clippy::panic   0
cargo doc   --locked --no-deps                           clean
python3 tools/comment_budget.py --max 14                 exit 0  (after `git add -A`)
python3 tools/check_paths.py                             exit 0
HARVEST_GOLDEN_RESULTS=<repo>/results cargo test --locked --manifest-path tools/Cargo.toml --test integration artifact_fingerprint
```

The fingerprint must pass and must not skip.

Plus, because cut 2 touches key derivation: show that a cache key computed for a fixed set of
inputs is unchanged from before the cut, or explain precisely why it legitimately changed and
bump `SCHEMA`.

## Commit message

The two cuts and why each is an edge-disguised-as-logic; that `Completed` travelled with its
only constructor; the old and new cycle membership; whether `SCHEMA` moved and the evidence;
and any place you declined to cut because it needed a widening.
