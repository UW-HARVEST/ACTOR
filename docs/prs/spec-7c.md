# PR 7c — Shared-source groups: SHELVED, and this spec's premise was wrong

**Status: attempted, not merged.** The work is preserved on branch `pr7c-shared-groups`
(`0e0ce41`, +731/−84 in `translate.rs`, all eleven gates green). Do not resume it from that branch
without reading this first — the design needs rethinking, not another review round.

## Why it was shelved

**1. The value is small and in the wrong place.** Measured: all **129** symlinked `test_case`
directories are in Test-Corpus (two groups — `B02_synthetic/macrodepth_*` at 3 configs and
`P01_sphincs_plus` at 128). Harvest-bench is **7 independent projects with no shared groups at
all.** So this saves nothing on the workload that costs a measured $795.59 per sweep, only on
Test-Corpus, where translate is cheap.

**2. It introduces a wrong published number.** Keying the group's real case means an ordinary
replay deletes the real case's `verified/` (via `Translate::INVALIDATES`) while every follower
keeps its own. The group then straddles two phases and the battery's headline score silently loses
a case. On `main` this is unreachable, because 7b deliberately left shared groups at
`Mode::Bypass`.

**3. This spec's central premise was false**, which is why the attempt ballooned from "wire up
what exists" to +731 lines.

## The false premise, stated plainly so it is not repeated

This spec said:

> Publish it from `run_cached`, then run the existing propagate loop over the followers **exactly
> as the code does today**. … The propagate loop already runs on the skip path, so it is
> replay-safe as written.

That is true only while the phase is **unkeyed**. Once the real case is keyed, PR 12's
`SkipCheck::Keyed` makes `already_done` return `false` unconditionally, so the real case is never
`skipped` — and the old follower skip (`has_crate(follower)` → `continue`) therefore leaves every
follower holding the **previous** translation while the real case has a new one. The loop is not
replay-safe once keyed; it was only ever replay-safe because nothing replayed.

The implementer was right to change it, and right that the spec forbade doing so. That contradiction
is the spec's fault, not the implementation's.

## What a future attempt has to solve

Four things, all found by review of the shelved branch and all confirmed against the code:

1. **Followers must be re-derived whenever the real case was not itself skipped**, and that decision
   must be **gated by a test**. On the branch it lives in an inline `if/else` inside
   `run_test_corpus`, which no test can reach (it needs `preflight_check` and a real `claude` on
   PATH), and both tests hand the value in as a literal — so collapsing the mapping to either
   constant leaves the whole suite green. Lift it into a pure function over the result shape and
   assert it exhaustively, the way PR 12 made `translate_skip_check` pure for exactly this reason.
2. **`propagate_config_phase` never clears its destination.** It replaces `src/` and `c_src/` but
   leaves everything else, so re-deriving over a follower that already holds a complete crate can
   leave a stale file from the previous translation. `clear_phase`/`Sealed::publish` exist for this.
3. **The group must not straddle two phases.** Whatever invalidates the real case's `verified/`
   must invalidate the followers' too, or the score loses cases. This is the wrong-number defect
   above and it is the hard one.
4. **`Translating::independent` duplicates `PromptKind::independent` verbatim** on one code path —
   one definition per concept.

## If you resume it

Rewrite this spec first, from what the code does now rather than from what it did before 7b and 12
landed. Then decide whether the value in item 1 above justifies solving item 3 — and note that
"leave shared groups bypassed" remains a correct, cheap answer that costs only Test-Corpus re-run
time. `main` is in that state today and is consistent.
