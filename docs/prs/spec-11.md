# PR 11 — An entry records the inputs its key was computed from

**Rewritten after a first attempt stalled**: 112 tool calls, ~1M tokens, and an empty worktree. The
old spec named six concerns and left the hardest one — reaching the seed at store time — as an
exercise. That is spelled out below. Do not go looking for a design; there is one, and it is small.

## The one-line version

`code/` is already re-derived and validated on every load: `load` rebuilds the digest from the bytes
on disk and refuses the entry if it disagrees with `meta.output_tree`. **Give the input the same
treatment**, so a change to any digest algorithm becomes a re-key of existing entries rather than a
cache wipe.

## Why, concretely

The key is `sha256(SCHEMA ‖ phase ‖ agent ‖ model ‖ cli ‖ toolchain ‖ prompt ‖ recipe ‖ input_tree)`.
Five of those are recorded in cleartext in `meta.json` and *are* their own value. Three are digests
whose preimage is stored nowhere:

| component | preimage recorded today |
|---|---|
| `prompt` | **no** — the normalised prompt text is discarded after hashing |
| `recipe` | **no** — `session.shape()` and `policy_shape` are discarded |
| `input_tree` | **no** — the tree that was walked is discarded |

So a new key algorithm that is any function of those eight *strings* is already re-derivable from
`meta.json`. What is not re-derivable is a change to how a component is **computed**, and all three
have one on the horizon: `normalise` (the `$HARVEST_WORKDIR`/`$WORK` token unification, and PR 16
adds a token), `Recipe::digest` (already `recipe-v2`), and `hash_tree` (`harvest-tree-v1`, which does
not hash file mode — and `copy_carrying` flattens mode, which the C-dataset projects will care about).

Any one of those empties the store today. After this, each is a re-key.

## The mechanism — this is the part that stalled, so here it is

`IsolatedWorkDir` is constructed from the seed's path in both cases and **throws the path away**,
keeping only the digest:

- `IsolatedWorkDir::<Translate>::from_corpus(corpus_dir)` — seed is the corpus dir
- `IsolatedWorkDir::<Verify>::new(case_dir)` — seed is `<case>/translated`

Both seed paths are still valid at store time (the corpus lives in the dataset; `translated/` is not
cleared by verify, whose `INVALIDATES` is empty). **So: keep the seed path in `IsolatedWorkDir`, add
an accessor, and copy that tree into `input/` in `Store::store` beside where `code/` is written.**
One field, one accessor, one copy. Do not thread a new parameter through `PhaseRun`.

### The subtlety that matters more than the plumbing

`input/` must re-hash to exactly `meta.input_tree`, and **the two phases digest their seed through
different predicates**:

- translate: `Corpus::digest` → `CDir`/oracle predicate, everything but `BuildOutput`;
- verify: `Sealed::digest` → `digest_tree`, `StoreAndHash` only.

So the export must use the **same predicate that produced the digest**, per phase. Give each seed
type one method that exports with its own predicate — `Corpus` already has `materialise_into`,
`Sealed` already has `export_into` — rather than inventing a shared walker with a phase flag. If you
find yourself adding a `bool` or a phase discriminant to a copy function, stop: that is the
transposition hazard this codebase keeps naming.

Assert the round-trip in the test below. If `input/` does not re-hash to `meta.input_tree`, the
predicates disagree and that is a finding, not something to paper over.

## What to add

**1. `input/`** — the seed tree, verbatim, exported on a **store (miss) only**. Storing the tree
rather than a manifest of per-file hashes is both more complete and simpler: it re-derives *any*
future digest, and it raises no question about encoding a non-UTF-8 filename losslessly, which
`hash_tree` already has to care about (it hashes `as_encoded_bytes()` precisely because a lossy name
collapses distinct bytes).

**2. `key-preimage.json`**, beside `meta.json`:

```json
{
  "schema": 2,
  "key": "sha256:…",
  "algorithms": { "key": "key-v1", "prompt": "prompt-v1",
                  "recipe": "recipe-v2", "input_tree": "harvest-tree-v1" },
  "prompt": "<the NORMALISED prompt text, exactly the bytes prompt_digest hashed>",
  "recipe": { "session_shape": "…", "policy_shape": "…" }
}
```

`algorithms` is not decoration: it is what tells a future re-keyer what it is converting *from*.
Those four tags are inline literals today (`feed(&mut h, b"key-v1")` and friends). **Make each a
named constant read by both the hasher and the record**, or the record can claim a version the hash
did not use — a second definition of one concept.

## Explicitly OUT of scope

- **Do not validate `input/` on load.** That is a second concern and it adds a full tree re-hash to
  every load. A missing or wrong `input/` costs future re-keying, not correctness now.
- **Do not backfill existing entries.** Their prompt preimages were never written and cannot be
  recovered.
- **Do not touch key composition.** No `SCHEMA` bump. If a key moves, you have a bug.

## The test that makes this real

**`an_entry_can_recompute_the_key_it_is_filed_under`** — from the entry directory alone:

1. hash `input/` with the phase's predicate → must equal `meta.input_tree`;
2. hash the recorded normalised prompt → must equal `meta.prompt`;
3. hash the recorded recipe shape → must equal `meta.recipe`;
4. feed all components → must equal the **directory name**.

Non-vacuity, both directions: flip one byte inside `input/` and assert the recomputed key **differs**;
same for one byte of the recorded prompt.

Second test: **`an_entry_written_before_this_change_is_still_served`** — an entry with no `input/` and
no `key-preimage.json` must still hit. A missing preimage is not a miss, unlike `agent/run.json`,
which is one on purpose.

## Secrets

**Nothing may enter the record that a digest would refuse.** That keeps the exposure delta at exactly
zero: `Session::shape()` is already hashed, and `cache.rs` already refuses to hash raw argv, which is
where C2SaferRust's `BEDROCK_API_KEY` lives. Two required assertions:

- a c2saferrust-shaped recipe's `key-preimage.json` contains neither `BEDROCK_API_KEY` nor its value;
- the recorded prompt is the **normalised** text, never the raw prompt, which embeds host paths.
  Assert no absolute host path appears in `key-preimage.json`.

## Constraints

- Write `input/` and `key-preimage.json` into the **staging** dir before the read-only lock, where
  `meta.json` is written. Entries are chmod'd `0o555` after staging; writing later fails EACCES.
- `Mode::Bypass` stores no entry, so it records nothing. One line in the report, not a special case.
- No visibility widening; report instead. No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`; destructive commands
  name one absolute path as the whole command.
- Answer: **after my change, what input still makes this check fail?** Name it.

## Acceptance criteria

The eleven gates (see `docs/HANDOFF.md`), the golden fingerprint passing and not skipping with 40
digests unchanged, plus: both cache keys unchanged for fixed inputs (measured), `SCHEMA` unmoved,
the four existing entries still served, and the recompute test passing with the flipped-byte run
shown failing.

## Commit message

Which three preimages were missing and what upcoming change would have moved each; that the seed
path is retained rather than threaded through `PhaseRun`; that each phase exports `input/` with the
same predicate that produced its digest, and that the round-trip is asserted; that the four algorithm
tags became named constants read by both hasher and record; that a missing preimage is not a miss;
the measured size delta per entry; and the two secret assertions.
