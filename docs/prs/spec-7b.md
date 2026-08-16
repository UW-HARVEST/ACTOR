# PR 7b — Translate on `run_cached`: the cache itself

## Goal

Port the **agentic, single-case** translate path onto `run_cached` so translate is memoised.
This is what the plan was for: on harvest-bench, translate is a measured **$795.59 per sweep**
that is currently re-paid every time.

Supersedes `spec-7.md`, which bundled nine changes and stalled an agent. 7a landed the safe
publish, the single log home, the merged metrics writer and the typed refusals. 7c will handle
shared-source groups.

## What is already in place — read it before designing anything

- `run_cached<P, F>(run: PhaseRun<'_, P>, store, compute)` in `agents/run.rs:58`, the ONE
  `store.obtain` call site in the crate, rule-enforced.
- `PhaseRun<'a, P: Cached>` already holds `IsolatedWorkDir<P>` — PR 6 generalised it over the
  phase, so it is not verify-shaped any more.
- `artifact::CorpusDir` (`adopt`, `digest`, `materialise_into`) from PR 1, and
  `SeededBy<CorpusDir> for Translate` with `AT = SeedAt::COracle`.
- `artifact::clear_phase` and the phase-derived log/metrics paths from 7a.

So the pieces exist. This PR wires them together; it should be mostly deletion in
`translate.rs`, not new machinery. If you find yourself writing a second version of something
`agents/run.rs` already does, stop — that is the mistake this whole sequence has been
correcting.

## Scope: the agentic path only

Port `translate_case_at` for the LLM backends that already resolve through `Invocation`.
Everything else must be `Mode::Bypass` — **not keyed wrongly**, because a wrong key is worse
than no cache:

- **Laertes and C2SaferRust.** Their input is reached by path surgery into a sibling agent's
  results tree (`results_dir.parent()/c2rust/<battery>/<name>`) with no digest, so the key
  cannot name *which* c2rust output was consumed, and re-running c2rust silently changes it.
- **C2SaferRust's `BEDROCK_API_KEY`** must never reach a digest or `meta.json`. It is passed
  as `-e BEDROCK_API_KEY=` in the docker argv; `cache.rs` already refuses to hash raw argv —
  keep it that way.
- **c2rust and smartc2rust.** c2rust is deterministic and would need a sentinel `ModelId` like
  the existing `KIRO_UNPINNED_MODEL`. Decide, and if bypassing, say why.
- **kimi and oneshot** are single Bedrock API calls with no `Invocation`. Bypass unless you can
  key them honestly.

State the final bypass list and the reason for each.

## THE risk: a false hit

Verify's key has a per-case prompt. **Translate's does not** — there is no placeholder
substitution anywhere in `translate.rs`, so `input_tree` is the ONLY per-case component. If
the corpus digest is wrong, every case in a battery collides on one key and the store serves
one case's translation as another's. Nothing downstream would catch it.

So:

- **Digest the corpus through `CorpusDir::digest`, never `digest_tree`.** With the corpus as
  hash root, the root-anchored ignore rules drop every `*.bak`, `*.log` and `*.sha256` — while
  the identical files ARE hashed once seeded under `c_src/`. `doc/footer.html.bak` is real in
  26 cases under `results/`. An existing test asserts both that two such corpora differ AND
  that `digest_tree` would have collided; do not weaken it.
- The key must include everything that changes what the agent produces. Verify's key names
  agent, model, cli, toolchain, prompt, recipe, input_tree. Say explicitly, field by field,
  what translate's names and why each is right — and name anything that reaches the agent and
  is NOT in the key, with the argument for why it cannot change the output.

## Required tests

1. **A false-hit test.** Two corpora differing only in a file the root-anchored rules would
   ignore must produce different keys. Assert both that the keys differ AND that the naive
   spelling would have collided, so it cannot pass vacuously.
2. **A round-trip test.** Translate a case into the store, then again with the same inputs, and
   assert: the agent closure is NOT invoked (make it panic if it is), the published tree is
   identical, the log is restored, and the metrics record a replay carrying the original's
   cache key. `agents::run::tests::a_replayed_phase_publishes_and_restores_the_transcript...`
   is the model.
3. **A no-cross-phase-hit test.** A translate entry must never serve a verify request or vice
   versa, even with otherwise-identical inputs. `phase` derives from `P::DIR`, so assert the
   two keys differ.

Each named after the failure, per `CLAUDE.md`.

## Hard requirements carried from the design doc

- `agent_provenance` exactly once per invocation, inside `compute` — `merge_agent_exit`
  CONSUMES the thread-local, so calling it in the caller makes a replay steal the previous
  case's exit code.
- `Store::load` treats a missing or unparsable `agent/run.json` as a **MISS**. Translate must
  write real provenance or every entry it stores is unservable, and the symptom is a cache that
  looks enabled and never hits.
- Do not touch shared-source group handling. That is 7c.

## Constraints

- No visibility widening; report instead.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion, no
  `.stderr` re-record beyond a path/column shift.
- `CYCLE_BASELINE` stays `["agents","artifact","battery","cache","cli"]` unless the rule says
  otherwise; report what it printed if so.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`, deleted with one
  absolute path, and if the delete is denied report the path and move on.
- Answer, for every check your diff touches: **after my change, what input still makes this
  check fail?** Name it.

## Acceptance criteria

The nine gates, plus the golden fingerprint passing and not skipping, plus:

- **The verify cache key for a fixed input is unchanged from `main`.** Measure it. This PR must
  not disturb the four real cache entries on disk.
- Report whether `SCHEMA` moved, with evidence either way.

## Commit message

The bypass list with a reason each; field by field what translate's key names and what reaches
the agent but is not keyed; how the corpus is digested and why not `digest_tree`; the three
tests and the evidence each can fail; the verify key unchanged; and 40 golden digests unchanged.
