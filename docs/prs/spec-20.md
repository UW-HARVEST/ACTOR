# PR 20 — c2rust's scored results collapsed on 2026-08-03 and nobody saw it

## The finding

`c2rust` is a **deterministic** transpiler with no model and no credentials, so its CI arm is the one
end-to-end check that should always be reproducible. Its stored-vs-actual scores on `main` today:

| battery | vectors_passed | cases_passed |
|---|---|---|
| B01_synthetic | 393 → **160** | 85 → **32** |
| B02_synthetic | 988 → **28** | 38 → **7** |
| B02_organic | 254 → **78** | 42 → **8** |
| B01_organic | 775 → **686** | 38 → **33** |
| P00_perlin_noise | 30 → **0** | 1 → **0** |

Between 60% and 97% of scored vectors have disappeared.

## Bisected, by CI job conclusion

```
2026-07-06  c88d6e2f  Test-Corpus c2rust = success
2026-07-07  7f845c13  Test-Corpus c2rust = success
2026-07-20  337e7a77  Test-Corpus c2rust = success
2026-08-03  fb9879ac  Test-Corpus c2rust = success   <- last green
2026-08-03  80b9475e  Test-Corpus c2rust = FAILURE   <- first red
2026-08-04 … 2026-08-17            all FAILURE
```

`git log fb9879ac..80b9475e` is exactly one commit:

> **`80b9475` Add harvest-bench submodule + thin runner script (#48)**

And #48 is not only a submodule addition. Its second half is:

> *refactor(harvest-tools): consolidate the pipeline behind one lifecycle — Collapses the
> per-dataset duplication that had accumulated in harvest-tools into a single parameterized
> lifecycle (translate → [verify?] → enrich → score) … **no behavior change to the existing
> datasets.***

That claim is false. The deterministic arm went from green to red at that commit and has been red
for the six weeks since.

## Why six weeks passed

`validate-translations` was **already red** from the five LLM arms, which fail because agent
invocations do not work on the runner (`terminal_reason=max_turns`, and the infra gate correctly
refuses to score). So "the workflow is red" carried no information, and a genuine regression in the
one reproducible arm was indistinguishable from the standing noise. `docs/prs/spec-19.md` fixes the
signal; this PR fixes the regression.

**This is the strongest argument in the repo for the discipline the rest of these specs enforce.**
A refactor asserted "no behavior change", the only check that could contradict it was drowned out,
and the assertion stood unchallenged for six weeks.

## The job

1. **Reproduce it locally**, which is possible precisely because c2rust is deterministic: run the
   c2rust arm's command on `main` and confirm the mismatch. Do not proceed on the CI log alone.
2. **Find the mechanism.** The magnitude (vectors, not just cases) points at builds or vector
   execution failing wholesale rather than at scoring arithmetic. Candidates, to be confirmed or
   eliminated by measurement, not by reading:
   - the enrichment consolidation (`Enrichment::compute`/`merge_into` became the single writer)
     changing what `result.json` holds, so `test --check` compares different fields;
   - the parameterised lifecycle changing what `translate → enrich → score` does for a dataset
     whose translation comes from a tool rather than an agent;
   - submodule checkout: the step is
     `git submodule update --init --depth=1 results test-corpus`, and `--depth=1` on a submodule
     whose pinned commit is not the tip fails to fetch it. #48 added a third submodule and may have
     moved a pin.
3. **Decide which side is wrong.** Either the harness regressed (fix it) or the stored expectations
   are stale and the *new* numbers are correct (then the stored values must be updated with the
   reason, and that is a paper-relevant change requiring the operator's sign-off — do not update
   stored expectations silently to make a gate green).
4. **Bisect within #48 if needed.** It is a squashed PR containing at least two logical changes, so
   its component commits may be recoverable from the PR's own history.

## Constraints

- **Do not update stored expectations to make the arm pass** without establishing that the new
  numbers are the correct ones and saying so explicitly. That converts a regression into a
  published number.
- Do not touch the eleven gates.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`; destructive commands
  name one absolute path as the whole command.
- Answer: **after my change, what input still makes this check fail?** Name it.

## Acceptance criteria

- the mismatch reproduced locally, with the command;
- the mechanism identified and evidenced by measurement;
- either the harness fixed with the five batteries back to their stored values, or a written case
  that the stored values are wrong, with the operator's decision recorded;
- a regression test that would have caught this: something cheap and deterministic that fails when
  c2rust's scored vectors drop. That test is the deliverable even if the fix itself is one line,
  because the absence of it is why this lasted six weeks.

## Commit message

The bisect (last green, first red, the one commit between them, and that its own message claimed no
behaviour change); the five batteries with before and after; the mechanism, measured; which side was
wrong; and the regression test that now makes this class of failure visible without depending on a
workflow that cannot pass in CI.
