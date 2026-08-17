# PR 20 — WITHDRAWN: there is no c2rust regression. The harness is correct.

**Status: withdrawn, and the earlier version of this spec was wrong.** It asserted a harness
regression in c2rust's scored results, bisected to #48. That claim does not survive measurement.

## What I actually measured

`harvest-tools --agent c2rust test <battery> --check` scores the ALREADY-STORED translations under
`results/Test-Corpus/c2rust`, so it needs no transpiler and reproduces locally. Run on `main` at
`b82e988`:

| battery | CI claims | measured locally |
|---|---|---|
| B01_synthetic | 393 → 160 vectors, 85 → 32 cases | **85/85 (393v) — stored == actual ✅** |
| B02_synthetic | 988 → 28 vectors, 38 → 7 cases | **38/42 (988v) — stored == actual ✅** |
| P00_perlin_noise | 30 → 0 vectors, 1 → 0 cases | **1/1 (30v) — stored == actual ✅** |

Every battery CI reports as catastrophically mismatched reproduces its stored value exactly. So:

- **the harness is not regressed;**
- **the stored expectations are not stale;**
- **the published numbers are unaffected;**
- the CI failure is environmental to the GitHub runner and nothing else.

The green→red transition at `80b9475` (#48) is real as a *CI* fact, and #48 did add a submodule
while the checkout step is `git submodule update --init --depth=1 results test-corpus` — a plausible
mechanism, since `--depth=1` cannot fetch a pinned commit that is not the tip, which would give the
runner a different `results` tree and therefore different "actual" scores. That remains a
hypothesis; it is not worth pursuing here.

## Why it is withdrawn rather than fixed

Operator decision: the reproducibility CI is known-buggy and will be addressed separately, at a
different time. It is explicitly out of scope. `docs/prs/spec-19.md` is deferred for the same
reason.

## The lesson worth keeping

I wrote the previous version of this spec, and the commit recording it, from CI logs alone —
asserting a six-week regression in published numbers on evidence I had not reproduced. One command
refuted it. That is the same defect this sequence has caught in agent reports repeatedly (a report
describing a tree that was never written), and `CLAUDE.md` already names the rule I broke:
**measure; do not estimate.** A bisect over CI job conclusions establishes when a *check* changed
state, which is not the same as establishing that the *code* changed behaviour.
