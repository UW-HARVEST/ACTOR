# PR 12 — The skip check consults the store, not the presence of a `Cargo.toml`

## Depends on 7b, and is what makes 7b pay

7b memoises translate. This PR is why that matters: **today a re-run never asks the store.**

`translate_one_independent` returns early on `has_crate(phase_dir(case, TRANSLATED))`
(`translate.rs:227`), and so do the shared-group follower loop (`:193`), `:276` and `:629`.
`battery::has_crate` is "a `Cargo.toml` exists here" (`battery.rs:30`). So on any results tree
that already holds a `translated/`, the store is never consulted — and the case is reported
**`success: true`, "already done"**.

Verify has the same shape at `verify.rs:91`, `:153` and `:260`.

## The defect is correctness, not only throughput

`has_crate` cannot say *which* invocation produced that tree. A `translated/` written by a
different model, a different prompt, a different CLI or a different toolchain satisfies it
exactly as well as the right one. That is what made the 2026-08-15 relaunch skip all seven
harvest-bench projects and report them done, and it is recorded as a known defect in
`docs/translate-cache-design.md` ("the provenance-blind skip check") — with the fix that only
becomes available once a phase is keyed:

> Once a phase is keyed, the store *is* the correct skip check: a hit replays, a miss runs.
> Keep `has_crate` only as the "something is published here" predicate it honestly is.

So there are two failures in one place:

1. **A wrong artifact is accepted as done.** Silent, and it publishes numbers.
2. **A right artifact in the store is never used.** The cache is inert on the case that
   matters most — a fresh results tree is not the common one; a populated tree being re-swept
   after a prompt or model change is.

## The change

For a keyed phase (`Launch::Keyed`), the decision becomes: **ask the store.** A hit replays; a
miss runs. `has_crate` stops being a skip and goes back to being the predicate it honestly is.

The cheap and honest ordering, since the key is resolvable before the agent starts:

- resolve the key (it already is, `run_cached` needs it);
- a **hit** → replay and publish, exactly as `run_cached` does today;
- a **miss** → run, whatever `translated/` happens to contain.

For a **bypassed** phase there is no key, so `has_crate` remains the only available check.
Keep it there and say so — that is not a regression, it is the honest limit of a bypassed
backend, and it is one more reason the bypass list should shrink.

**The published tree is not the cache.** Do not "promote" an existing unkeyed `translated/`
into the store: nothing records what produced it, which is the whole defect. An unkeyed tree
present on disk must be treated as absent by the keyed path, and it will be overwritten by
`Sealed::publish`.

Corrected while implementing, because the sentence that stood here was wrong about the code and
a commit message built on it would have been wrong too: `publish` clears `translated/` but keeps
its own `logs/`, and it removes the sibling `verified/` **whole, `logs/` included**, because a
verification of the previous translation is not a verification of this one
(`Translate::INVALIDATES`, and `artifact.rs` says why keeping just its logs would be worse — the
"already verified" skip keys on `verified/logs/verify.log`). `test_vectors/` and `runner/` belong
to the case rather than the phase, and do survive. Measured on the harvest-bench tree
(`du -sck results/HarvestBench/claude/*/verified` = 143,392 KB), a keyed re-translate therefore
destroys **140 MB** of `verified/` across the seven projects — four verified crates plus
lz4/mujs/zstd's 4.1/12.4/22.1 MB abort transcripts. Every statement of this figure is that one
measurement: adding up seven `du -sm` values instead reads 144, because each rounds up
separately. That is correct behaviour and an operator decision, not a free retry.

## The cost this changes, stated plainly

A re-run of a fully-populated tree currently costs ~0 and produces possibly-wrong "done"
lines. Afterwards it costs one key resolution per case — a corpus digest, a CLI probe and a
toolchain probe — and then either a replay (a tree copy) or a real run. **On a tree whose
artifacts came from the current key, every case still replays rather than re-runs, so no
agent money is spent.** Say what a re-run of the four cached projects costs after this
change, measured.

## Required tests

1. **`a_published_translation_from_a_different_model_is_not_accepted_as_done`.** Publish a
   `translated/` under one key, change the model, and assert the case **runs** rather than
   reporting done. This is the correctness failure; it must be able to fail — show it failing
   with the old `has_crate` check restored.
2. **`a_populated_results_tree_still_replays_from_the_store_instead_of_re_running`.** With a
   matching entry in the store and a `translated/` already on disk, assert the agent closure
   is not invoked (panic if it is) and the published tree matches.
3. **`a_bypassed_backend_still_skips_on_a_published_crate`.** The unkeyed path keeps its only
   available check.

Named after the failure, per `CLAUDE.md`.

## Constraints

- Verify and translate must get the **same** treatment. Two skip policies for one concept is
  how this drifted in the first place.
- No visibility widening; report instead.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion.
- No `SCHEMA` bump and no key change: this PR changes *when* the store is asked, never what
  the key is. Measure a fixed key both sides.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`, deleted with one
  absolute path; if the delete is denied, report the path and move on.
- Answer, for every check your diff touches: **after my change, what input still makes this
  check fail?** Name it.

## Acceptance criteria

The ten gates (see `docs/HANDOFF.md`), the golden fingerprint passing and not skipping, plus:

- a fixed verify key and a fixed translate key unchanged from `main`, measured;
- the four real entries on disk still served;
- test 1 shown failing with the old check restored.

## Commit message

That `has_crate` was a provenance-blind skip and what it cost on 2026-08-15; that the keyed
path now asks the store and the bypassed path cannot; that an unkeyed `translated/` is
deliberately never promoted into the store; the measured cost of re-running a populated tree
after the change; and the three tests with the evidence each can fail.
