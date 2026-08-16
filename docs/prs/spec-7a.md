# PR 7a — Make translate's four paths publish safely and report identically

## Why this is its own PR

PR 7 was specified as one change and an implementing agent stalled on it after 198 tool
calls having produced nothing. It asked for nine substantial edits to a 2,595-line file at
once. Split: **7a is everything that is valuable without a cache**, 7b ports translate onto
`run_cached`, 7c handles shared-source groups.

Nothing here depends on the cache. Every item is a defect on `main` today.

## 1. The destructive wipe, at four sites — a real data-loss ordering bug

`translate.rs:758, 1314, 1692, 1897` each do:

```rust
if case_dir.exists() { std::fs::remove_dir_all(case_dir)?; }
```

Two problems, and the ordering one is the serious half:

* **It runs before the agent does.** A crash, an API outage or a timeout therefore leaves the
  case holding *nothing* where it previously held a complete result.
* **It deletes what translate does not own** — `verified/`, and for Test-Corpus cases
  `test_vectors/` and `runner/`.

`artifact::Sealed::publish` already does the safe version: clear the phase dir, keep `logs/`.
Route these through the same semantics, and do it *immediately before the output is written*
rather than before the agent starts.

One judgement to make explicitly and record: **a new translation legitimately invalidates the
old `verified/`**, because `battery::crate_dir`'s reader rule prefers `verified/` when present
and would otherwise return a stale verification for a fresh translation. So `verified/` must
go — but at publish time, not before the agent, and as a deliberate act with a stated reason
rather than a side effect of wiping the case.

Do NOT hand-roll a fourth copy of "clear a phase dir". If `Sealed::publish` cannot be reached
from these paths yet (they do not produce a `Sealed` until 7b), put the logic in exactly ONE
place and say where. A previous attempt at this added a `clear_translated_for_republish`
helper duplicating `publish`, and that was correctly rejected — one definition per concept.

## 2. `translation.log` has four homes; every reader looks at one

`translate.rs:767, 1319, 1696, 1901` each build a `translation.log` path, and they do not all
land in the same place: `translate_case_at` writes `<case>/translated/logs/`, while the
oneshot/kimi, laertes and c2saferrust paths write `<case>/logs/`.

Every reader — `agent_health::audit`, and the `oracle/` readers — looks only at
`translated/logs/`. So **three of the four paths are invisible to the infra-failure gate**: a
run that died for infrastructure reasons on those backends is scored as a result.

Make the log path a function of the phase, in one place. Expect the infra gate to start
firing on backends it previously ignored — that is the fix working, not a regression. Say
which backends become visible.

## 3. Two metrics writers that disagree

`write_translation_metrics` (translate.rs:1052) and `write_verification_metrics` are separate,
and only the verify one carries `replayed` / `cache_key`. They also disagree about what
`case_dir` means — one derives the phase dir itself, the other is handed it.

Merge them into one writer parameterised by phase. **It must not gain a `replayed: bool`** —
that would put two bools on one function, where transposition is silent. Use a named
two-variant enum. A replay must record that it was one, so the original run's cost is not
read as this run's spend.

## 4. `unreachable!()` in the dispatch match

`translate.rs:919-923` — `Agent::{Laertes, C2SaferRust, SmartC2Rust, Kimi, Oneshot}` all
`unreachable!()`. They are reachable: `--agent laertes translate HB/<project>` panics there,
the panic is caught by `CaseResult::panicked`, and the case is reported as an ordinary ❌
indistinguishable from a translation that genuinely failed.

Return a typed "this agent has no in-tool translate phase for this dataset", the way
`verify_invocation` returns `Ok(None)`. Line 886's `_ => unreachable!()` is a different case —
judge it separately and say what you concluded.

## Constraints

- No visibility widening to make something move; report instead.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion, no
  `.stderr` re-record beyond a path/column shift.
- Do not port anything onto `run_cached` — that is 7b. Do not touch shared-source group
  handling — that is 7c.
- `CYCLE_BASELINE` must stay `["agents","artifact","battery","cache","cli"]` unless the rule
  says otherwise; if it does, report what it printed.
- Never write to `/tmp`. Use `/local/home/scheschb/scratch/<yours>`, delete it with one
  absolute path, and if the delete is denied report the path and move on.

## Acceptance criteria

Pinned toolchain, `RUSTUP_TOOLCHAIN` unset — the nine gates, plus:

```
HARVEST_GOLDEN_RESULTS=<repo>/results cargo test --locked --manifest-path tools/Cargo.toml --test integration artifact_fingerprint
```

must pass and must not skip.

Two tests this PR must add, each named after the failure:

1. **A failed translation leaves the previous result standing.** Simulate a translate that
   errors after the point the old code would have wiped, and assert the prior `translated/`
   and `verified/` are still there. This is the ordering bug; without a test it comes back.
2. **A replay is recorded as a replay.** Assert the merged metrics writer distinguishes a
   replayed run from a fresh one, so the original's cost is not attributed to this run.

## Commit message

The ordering bug and what each of the four sites destroyed; where the single "clear a phase
dir" definition now lives; which backends the infra gate can now see; that the merged metrics
writer takes a named enum and not a second bool; what you concluded about line 886; and 40
golden digests unchanged.
