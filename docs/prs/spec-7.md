# PR 7 — Translate on `run_cached`: the translate cache

## Goal

Port `translate_case_at` onto the driver PR 6 built, so translate is memoised. This is the
change the whole plan was for: on harvest-bench, translate is a measured **$795.59 per
sweep** that is currently re-paid every time.

## Depends on PR 6

`run_cached<P>`, `PhaseRun<P>`, and the rule asserting exactly one `store.obtain` call site
must be on `main`. If they are not, stop.

## What makes this different from PR 6

PR 6 was a shape change with behaviour held identical. This one adds a cache where there was
none, so the risk is not "did the shape change" but **"can it serve the wrong artifact"**. A
false hit is the worst failure available here: it silently publishes one case's translation
as another's, and nothing downstream would catch it.

Two facts from `docs/translate-cache-design.md` make that risk concrete:

1. **Translate's prompt carries no case identity.** There is no placeholder substitution
   anywhere in `translate.rs` — verify substitutes three, translate substitutes none. So
   `input_tree` is the ONLY per-case component of the key. If the corpus digest is wrong or
   absent, every case in a battery collides on one key.
2. **The corpus must be digested through `CorpusDir`, never `digest_tree`.** With the corpus
   as hash root, the root-anchored ignore rules drop every `*.bak`, `*.log` and `*.sha256`
   — while the identical files ARE hashed once seeded under `c_src/`. `doc/footer.html.bak`
   is real in 26 cases under `results/`, so `digest_tree` would let two different corpora
   share an input digest. `CorpusDir::adopt` + `CorpusDir::digest` (PR 1) exist precisely for
   this. A test already asserts both that the digests differ and that `digest_tree` would
   have collided; do not weaken it.

## Scope

Port the **agentic** translate path — `translate_case_at` for the LLM backends — onto
`run_cached`. That is where the money is.

Explicitly NOT in scope, and each must be keyed as `Mode::Bypass` rather than keyed wrongly,
because a wrong key is worse than no cache:

- **Laertes and C2SaferRust.** Their input is reached by path surgery into a sibling agent's
  results tree (`results_dir.parent()/c2rust/<battery>/<name>`) with no digest, so the key
  cannot name *which* c2rust output was consumed, and re-running c2rust silently changes
  their input. Adopting that as a `CorpusDir` is separable follow-up work.
- **C2SaferRust's `BEDROCK_API_KEY`** must never reach a digest or `meta.json`. It is passed
  as `-e BEDROCK_API_KEY=` in the docker argv; `cache.rs` already refuses to hash raw argv.
- The **deterministic translators** (c2rust, smartc2rust): decide and state whether they are
  cached. c2rust has an honest `CliVersion` and no model, so it would need a sentinel
  `ModelId` like the existing `KIRO_UNPINNED_MODEL`. If keying them is not clean, bypass them
  and say why.

## What deletes itself, and what must not

These should disappear **as a consequence** of the driver existing, not by being patched:

| today | after |
|---|---|
| `remove_dir_all(case_dir)` at 4 sites | `Sealed::publish` |
| 4× "make workdir, copy corpus, run, copy back" | one materialise + one publish |
| 2 metrics writers | one |
| 2 homes for `translation.log` | one, a function of `P` |
| 2× 17-arm dispatch match | one, parameterised by `PromptKind` |

Report which of these actually collapsed. If one did not, say which edge held it — a partial
result honestly described is worth more than a claim.

**Ordering matters and is a correctness issue, not tidiness.** The destructive
`remove_dir_all(case_dir)` currently runs *before* the agent, so a crash leaves the case with
nothing where it had a complete result. It also destroys `verified/`, `test_vectors/` and
`runner/`, none of which translate owns. `Sealed::publish` already does the safe version:
clear the phase dir, keep `logs/`. But note a new translation legitimately invalidates the
old `verified/` — so decide deliberately what happens to it and say so.

## Hard requirements

- **`write_translation_metrics` may not gain a `replayed: bool`.** Two bools on one function
  is a transposition hazard; use a named enum, and merge the two metrics writers as the table
  above says.
- **`agent_provenance` must be called exactly once per invocation**, inside `compute`, where
  verify does it — `merge_agent_exit` CONSUMES the thread-local, so calling it in the caller
  would make a replay steal the previous case's exit code.
- **`Store::load` treats a missing or unparsable `agent/run.json` as a MISS**, not as null
  provenance. Translate must always write real provenance or every entry it stores is
  unservable — and the symptom is a cache that looks enabled and never hits.
- **Shared-source groups get ONE key, not N.** The N followers are *derived* trees (different
  default features, different `[lib]` name, tests stripped by `propagate_config_phase`), so
  they can never be copies of the stored artifact. Publish the real case from `obtain`, then
  run the existing propagate loop — which already runs on the skip path, so it is replay-safe
  as written. Do not give followers their own keys: that would key N invocations that never
  happened.
- **`--agent laertes|c2saferrust|smartc2rust|kimi|oneshot translate HB/<p>` currently hits
  `unreachable!()`**, panics, is caught, and is reported as an ordinary ❌. The driver must
  return a typed "no translate phase for this agent/dataset", the way `verify_invocation`
  returns `Ok(None)`.

## Acceptance criteria

Pinned toolchain, `RUSTUP_TOOLCHAIN` unset — the nine gates, plus:

```
HARVEST_GOLDEN_RESULTS=<repo>/results cargo test --locked --manifest-path tools/Cargo.toml --test integration artifact_fingerprint
```

must pass and must not skip.

And the two that are specific to this PR:

1. **A false-hit test.** Two corpora differing only in a file the root-anchored rules would
   ignore must produce different keys. Assert both that the keys differ AND that the naive
   spelling would have collided, so it cannot pass vacuously.
2. **A round-trip test.** Translate a case into the store, then translate again with the same
   inputs and assert: the agent closure is NOT invoked, the published tree is identical, the
   log is restored, and the metrics record `replayed` with the original's cache key. Verify's
   `a_replayed_phase_publishes_and_restores_the_transcript...` is the model.

Also report the verify cache key for a fixed input, unchanged from `main` — this PR must not
disturb the 4 real entries already on disk.

## Commit message

Which of the five duplications collapsed and which edge held any that did not; how the
corpus is digested and why not `digest_tree`; what happens to `verified/` when a translation
is replaced; which backends are bypassed and why; that shared groups take one key; and the
measured evidence for both new tests.
