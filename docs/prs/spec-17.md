# PR 17 — Invariant I1 holds only on the keyed path

## What PR 12 established, and where it stops

PR 12 established two invariants for a run that publishes nothing:

> **I1.** It must never leave a phase dir where one run's crate sits beside another run's metrics
> or transcript.
>
> **I2.** It must never make a previously published artifact unrecoverable.

Both hold on the `run_cached` path, enforced by `displace_phase`, which has exactly one caller
(`agents/run.rs`, the `Outcome::Nothing` branch). Verify is fully covered, because `verify_case`
routes solely through `run_cached`.

**Translate's unkeyed paths are not covered**, and there are five of them:

- the non-`Launch::Keyed` branch of `translate_case_at` — opencode, codex, c2rust, which publish
  by copy rather than by seal;
- `oneshot_translate_case`;
- `laertes_translate_case`;
- `c2saferrust_translate_case`;
- the shared-source group path.

## The failure, precisely

All of them create `<case>/translated/logs/` and tee the transcript there **before the agent
runs** — `tee "$2"` and `File::create` both truncate, so the previous run's transcript is gone at
that moment — and all of them call `clear_phase::<Translate>` only **after** a new artifact is in
hand.

So when the run fails — docker exits non-zero, `bail!("no Cargo.toml in LLM response")`, the CLI is
absent — `run_and_record`'s `Err` arm writes `<case>/translated/translation.json` with
`"success": false` into the very directory where the **previous** run's crate is still standing,
and nothing displaces it.

Two consequences, and the second is worse:

1. **A wrong published number.** `battery::crate_dir` hands that crate to the scorer, and the
   enrichers stamp run B's agent, model, cost and timestamp onto its `result.json` —
   `Phase::METRICS` is read from the phase dir by `agent_health::audit`, the `oracle/` enrichers
   and `battery::extract_agent_meta`. Run A's crate is scored as run B's result.
2. **The next sweep reads the case as done.** These are exactly the paths where
   `SkipCheck::WhateverIsPublished` applies, so the failure mode PR 12 closed for keyed backends
   — `a_published_translation_from_a_different_model_is_not_accepted_as_done` — survives intact on
   the unkeyed ones.

## Why this was not a rider on PR 12

Reported and deliberately left, for three reasons that are also the design constraints here:

- `run_and_record` returns `CaseResult`, not `Result`, so displacing there needs an
  error-propagation decision. Swallowing it with `if let Ok(..)` would be a new silent failure —
  the exact class PR 12's own `read_dir` fix removed.
- It needs the operator warning `run_cached` prints, or the artifact moves aside silently.
- It must not displace a crate that a `RecordedBy::Driver` caller has just published. That is its
  own test, and getting it wrong deletes a good result.

## The change

Give the unkeyed paths the same displacement the keyed path has, once, in one place. Do not add a
second spelling of "make room for this phase": `displace_phase` already exists and
`clear_phase` already exists, and a third would be the duplication this sequence removes.

The cleanest shape is likely to fix the root cause rather than add a call: **the transcript is
teed into the phase dir before the outcome is known, and that is what forces the choice.** Tee to
the work tree or a staging path and move it into the phase dir when the artifact is published, as
`Sealed::publish` already does for the artifact. Then a failed unkeyed run touches the phase dir
not at all, and I1 holds for free on all five paths instead of being enforced five times.

If you take the displacement route instead, say why staging was rejected.

**IN SCOPE, absorbed from `spec-15.md` item 2:** the keyed and unkeyed paths publish *different
trees* — the keyed one through `Carry::FromArtifact`, the unkeyed one by recursive copy — so what
`translated/` contains depends on which backend produced it, which is a measurement hazard rather
than a tidiness one.

That is the same root cause as the invariant gap above: the unkeyed path does not go through the
artifact machinery. So fix both together — route the unkeyed publish through the same
`Sealed::publish`/`Carry::FromArtifact` the keyed path uses. If it genuinely cannot be (no `Sealed`
without a `Completed`), say precisely why and record the difference in ONE place rather than leaving
it implicit in two copy helpers.

**Test: `every_backend_publishes_the_same_tree_shape`** — one table-driven test over both paths
asserting the same admitted and excluded set.

## Required tests

1. **`a_failed_unkeyed_translation_does_not_leave_its_metrics_beside_an_earlier_crate`** — publish
   a translation, then fail an unkeyed run in the same case, and assert the phase dir does not
   hold run A's crate beside run B's `translation.json`. Show it red before the fix; this is the
   defect.
2. **`a_failed_unkeyed_translation_does_not_make_the_earlier_crate_unrecoverable`** — I2 for the
   unkeyed path, which has no store entry to fall back on. The previous artifact must still be
   reachable, wherever it now lives.
3. **`a_case_the_previous_sweep_failed_is_not_read_as_done`** — the consequence that reaches a
   published number. Assert the next sweep runs the case rather than skipping it.
4. **`a_driver_published_artifact_is_never_displaced`** — the hazard named above. Must be red if
   the displacement is applied unconditionally.

Named after the failure, per `CLAUDE.md`. Each must be shown able to fail.

## Constraints

- No visibility widening; report instead.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion.
- **No key change and no `SCHEMA` bump.** This is about when a phase dir is written, not what a
  key names. Measure both keys for fixed inputs, both sides.
- The 40 golden digests unchanged. If one moves, something is hashing a transcript.
- One definition of "make room for this phase". If you add a third call site, explain why the
  existing two could not serve.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`; destructive
  commands name one absolute path as the whole command.
- Answer, for every check your diff touches: **after my change, what input still makes this check
  fail?** Name it.

## Acceptance criteria

The eleven gates (see `docs/HANDOFF.md`), the golden fingerprint passing and not skipping with 40
digests unchanged, both cache keys unchanged, plus the four tests with evidence each can fail.

## Commit message

Which five paths were uncovered and why the keyed path was not; that the root cause is the
transcript being teed before the outcome is known, and which route you took; the two consequences
(run A's crate scored as run B's result, and the next sweep reading the case as done); and the four
tests with the evidence each can fail.
