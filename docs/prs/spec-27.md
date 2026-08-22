# PR 27 — A case flows through the whole pipeline as one unit; `--parallel` is cases in flight

## The operator's design

> Rather than all translation, then all verification, then all scoring: for one test case do
> translation, verification and scoring together, and `--parallel` says how many cases are in flight.
> That feels like a normal pipeline.

## What it is today

Phase-major. `Command::Run` does:

1. `bench.translate(target, parallel)` — every case of every battery in scope, N cases in flight
2. derive the publishable scope from what the store served
3. per battery: `bench.verify(battery, parallel)`, then `run_test` → materialise the whole battery into
   `.eval/<agent>/<battery>/<phase>/` and score it in one `runtests` invocation

So `--parallel` means something different in each phase, verify cannot start until the slowest
translate finishes, and a case's lifecycle is spread across three sweeps.

## The shape

One worker pool over **work items**, each carrying a case from corpus to score:

```
item = Independent(case) | SharedSource(group)
for each item, in flight up to `parallel`:
    translate  -> resolve through the store (hit replays, miss pays)
    verify     -> seeded from what translate resolved
    score      -> materialise into this item's own eval scope, run the oracle, record
```

A **group is one item**, not 128: its followers are `propagate_config`'s deterministic derivation of the
real case, so they cannot start before it finishes and they cost no agent call. Inside the item the
order is translate(real) → derive → verify(real) → derive → score all its cases. This is the split
`battery::discover` already returns as `Case::{Independent, SharedSource}`.

## The two things that make this non-trivial, measured

**1. Scoring is batched today for a reason.** `Scope::finish` writes ONE cargo workspace listing every
case's `runner/`, so a battery's runners build in a single invocation — P01's 128 in ~30 s. Each runner
depends on `cando2`, hence `arbitrary` (with derive), `libloading` and `serde_json` with
`arbitrary_precision`/`preserve_order`. Per-case builds against a cold target dir rebuild that tree per
case: ~30 s becomes ~45 min for P01.

**So per-case scoring MUST share one cargo target dir** across the run, built once and reused. Cargo
takes an exclusive lock on it, so concurrent per-case runner builds serialise — acceptable, because each
is one small crate once the deps are warm, and the agent phases are minutes to hours. Do not implement
per-case scoring without this; it is the difference between a reshuffle and a 90x regression.

**2. The battery summary is parsed from aggregate console output.** `run_runtests` regexes
`Test Cases Discovered: N`, `Test Vectors Passed: N`, … out of one battery-wide invocation. Case-major
has no such invocation, so the battery `summary.json` must be SUMMED from per-case results.

Treat that as an improvement, not a port: the JUnit report already carries per-case, per-vector
verdicts, and #124 already moved the case set off console text for exactly this reason. After this PR
nothing that reaches a table is parsed from console output. But it is also the one place a published
number can move, so:

**`tables/` must come out byte-identical.** CI enforces it. That is the acceptance criterion that makes
this refactor safe to attempt at all — the numbers are committed, so a summing bug cannot merge.

## What must not change

- **The published numbers.** 338/338 built, 335/338 passing, and every one of the six tables
  byte-identical.
- **Two cache hits per case.** A replay must still report every phase served from the store with
  `0 agent invocation(s)`; `reproduce.sh` asserts this and its tally shape will change, so the script
  changes with it.
- **The scope gate.** A battery is publishable only if every case came from a keyed replay
  (`Benchmark::attests`). Case-major learns this per item and must still decide per battery, after all
  of that battery's items finish.
- **`.eval/` created empty and removed.** Per-case scopes live under it; nothing may read another case's.
- **The infra-health gate.** `agent_health::Gate::grade` currently grades the whole covered set before
  scoring. Per item it must grade that item, and a run may not publish while any item's transcript shows
  an infrastructure failure.

## Acceptance criteria

The twelve gates, plus:

1. **`tables/` byte-identical**, and `tools/reproduce.sh all` green: every phase replayed, all
   `0 agent invocation(s)`, `.eval/` empty, all six tables regenerated.
2. **A case's three stages are adjacent in the log**, shown by the ordering: a case's verify appears
   before the next case's translate. That is the whole point, so it is asserted rather than eyeballed.
3. **`--parallel N` bounds cases in flight**, not per-phase workers — asserted over the work-item
   scheduler as a pure function of (items, N), not by watching a sweep.
4. **A group is one item**, and its followers are derived after its real case and never concurrently
   with it. Mutate: let the followers start early and show the derivation read a crate that was not
   there yet.
5. **The battery summary equals the sum of its cases**, asserted against the stored records for a real
   battery — and mutate the sum (drop one case) to show the tables move.
6. **One shared cargo target dir**, with the per-case runner build reusing it: measured, P01's scoring
   stays within a small factor of today's ~30 s rather than rebuilding the dependency tree per case.
   Quote both numbers.
7. **A failing case does not sink the run's other cases**, and a refusal still refuses the run: one
   item's infra failure must not silently narrow the published scope.

## What this does not change

Not a behaviour change to any agent, prompt, model, recipe or cache key. `SCHEMA` stays 4 and no stored
entry may be invalidated — if a key moves, the design is wrong, because nothing about WHAT is invoked
changes here, only WHEN.

## Commit message

That a case now flows through translate, verify and score as one work item with `--parallel` bounding
items in flight, replacing three phase-major sweeps in which `--parallel` meant something different each
time and verify could not start until the slowest translate finished. That a shared-source group is ONE
item because its followers are a deterministic derivation of its real case, which is the split
`battery::discover` already returned. That per-case scoring shares one cargo target dir, measured,
because a battery's runners build as a single workspace today and rebuilding `cando2`/`arbitrary`/
`serde_json` per case turns P01's ~30 s into ~45 min. That the battery summary is now SUMMED from
per-case results rather than regexed out of one aggregate console run, so after this nothing reaching a
table is parsed from console output. And that `tables/` is byte-identical, which is what makes the
change safe to attempt: the numbers are committed and CI compares them.
