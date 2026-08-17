# PR 19 — `validate-translations` has been red on `main` for six weeks, so CI proves nothing

## The measurement

```
gh run list --workflow validate-translations --branch main --limit 100
  → 0 successes, oldest checked 2026-07-05
```

**Zero passing runs in a hundred, back six weeks.** Every commit on `main` in that window is red,
including every commit of the current refactor sequence and everything before it.

So every PR in this repo has been merged on the `type safety` workflow alone. The handoff's own
ritual says "wait for `tests`", which encodes the situation rather than questioning it. The seven
`Test-Corpus` matrix arms — the only CI that exercises a real translation end to end — contribute
no signal at all, and a genuine regression in translation behaviour would not be caught by CI.

## Why it is red, which is not a bug

From the `Test-Corpus claude` job on run 31995081297: **every case ends
`terminal_reason=max_turns`**, and the harness then does exactly what it was built to do:

```
Refusing to score. An infrastructure failure is not a result.
Details written to .../results/Test-Corpus/claude/INFRA_FAILURES.json
❌ Failed after 3 attempts
```

That is the infra gate working. The workflow is red for an *honest* reason: agent invocations do
not function on the runner, so there is no result to score, so nothing is scored.

The defect is therefore not in the gate. It is that **a workflow whose preconditions cannot be met
in CI is still wired to every push and pull request**, where its permanent failure is
indistinguishable from a real one. That is the same disease this sequence has been fixing in the
small — a check that cannot discriminate — one level up.

It is also expensive in a way nobody chose: seven paid agent matrices are being launched on every
commit, running for up to 15 minutes each before failing.

## The decision this PR exists to make

**Stop triggering paid agent translations on push and pull_request.** Move
`validate-translations` to `workflow_dispatch` plus a schedule, so that:

- a red check on a PR means something again, because only checks that *can* pass are attached to
  PRs;
- the expensive matrix runs deliberately, when someone wants the answer and the credentials work;
- `type safety` remains the PR gate it has effectively been for six weeks, now honestly.

Do **not** "fix" this by passing `--allow-infra-failures` in CI. That would make the workflow green
by scoring infrastructure failures as results, which is precisely the refusal the harness exists to
perform, and it would publish numbers from runs where every case hit `max_turns`.

Do **not** delete the workflow. The matrix is the only end-to-end validation that exists; it needs a
working trigger, not removal.

## What to establish, and report as measurements

1. **Whether the arms can pass at all on a runner.** Is `max_turns` a credential failure, a
   too-low turn limit for CI's slower environment, or a model that is unreachable from the runner?
   Answer it from the logs before changing the trigger, because the answer decides whether a
   scheduled run is worth having or whether the matrix needs a smaller corpus.
2. **The cost that has been silently spent.** Seven arms × the run frequency over six weeks, with
   the per-arm duration from the run list. If those invocations were billed, say so; if they failed
   before spending, say that instead — it changes whether this is a cleanup or a refund.
3. **Which arms fail differently.** `claude` fails in ~1m, `c2rust` and `laertes` in ~15m. c2rust is
   a deterministic transpiler with no model, so if *it* fails, the cause is not credentials and the
   diagnosis differs per arm.

## Required changes

- `validate-translations` triggers on `workflow_dispatch` and a schedule, not on push/PR.
- A line in `docs/HANDOFF.md`'s gate section recording what CI does and does not prove, so the next
  agent does not read a green `tests` as "CI validated this translation".
- If item 1 finds a cheap fix that makes the arms pass, take it and keep the PR trigger. State
  which, with the evidence.

## Acceptance criteria

- the measurement above, reproduced;
- a PR that shows only checks capable of passing;
- `type safety` unchanged — this PR must not touch the eleven gates;
- no use of `--allow-infra-failures` anywhere in CI.

## Commit message

The measured six-week red streak and that the cause is the infra gate correctly refusing to score
`max_turns` runs; that the defect is the trigger rather than the gate; the per-arm failure modes;
the cost that was being spent; and what CI does and does not prove after the change.
