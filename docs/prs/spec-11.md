# PR 11 — An entry records the inputs its key was computed from

## The one-line version

`code/` is already re-derived and validated on every load: `load` rebuilds the digest from
the bytes on disk and refuses the entry if it disagrees with `meta.output_tree`
(`cache.rs:636-645`). **Give the input the same treatment.** Then a change to any digest
algorithm is a re-key of existing entries rather than a cache wipe.

## Why, concretely

The key is `sha256("key-v1" ‖ SCHEMA ‖ phase ‖ agent ‖ model ‖ cli ‖ toolchain ‖ prompt ‖
recipe ‖ input_tree)`. Three of those are themselves digests, and **the bytes they were
computed from are stored nowhere**:

| component | recorded today | preimage recorded |
|---|---|---|
| phase, agent, model, cli, toolchain | yes, cleartext in `meta.json` | n/a, they *are* the value |
| `prompt` | `sha256:…` only | **no** — the normalised prompt text is lost |
| `recipe` | `sha256:…` only | **no** — `session.shape()` and `policy_shape` are lost |
| `input_tree` | `sha256:…` only | **no** — the tree walked to produce it is lost |

So today a new key algorithm that is any function of those eight *strings* is already
re-derivable from `meta.json`. What is not re-derivable is a change to how a component is
computed — and each of the three has a change already on the horizon:

- **`normalise`** — `docs/architecture-plan.md` lists unifying `scrub`'s `$HARVEST_WORKDIR`
  token with `normalise`'s `$WORK`/`$REPO`/`$HOME` as known future work. That moves every
  prompt digest.
- **`Recipe::digest`** is already tagged `recipe-v2`, so its framing has changed once.
- **`hash_tree`** is `harvest-tree-v1` and does **not** hash file mode. Trap 17 in
  `docs/translate-cache-design.md`: `copy_carrying` chmods everything to `0o644` and loses
  `+x`, zero executables exist in today's corpus, and the C-dataset projects ship them. Mode
  entering the tree digest is a question of when, not if, and it moves every tree digest.

Any one of those empties the store today. After this PR each is a re-key.

## What to add

Two things, inside the entry, i.e. under
`results/.cache/<SCHEMA>/<phase>/<agent>/<key>/`:

**1. `input/` — the input tree, verbatim.** Exported the same way `code/` is
(`produced.sealed.export_into(&staging.join("code"))`, `cache.rs:706`), from the *seed*
rather than from the work tree. Storing the tree rather than a manifest of per-file hashes
is both more complete and simpler: it re-derives *any* future digest, and it raises no
question about how to encode a non-UTF-8 filename — which a manifest would have to answer
losslessly, since `hash_tree` hashes `as_encoded_bytes()` precisely because a lossy name
collapses `a\xFF` and `a\xFE` to the same bytes (`artifact.rs:351-354`).

**2. `key-preimage.json`** — the two small preimages plus the algorithm identity:

```json
{
  "schema": 2,
  "key": "sha256:…",
  "algorithms": { "key": "key-v1", "prompt": "prompt-v1",
                  "recipe": "recipe-v2", "input_tree": "harvest-tree-v1" },
  "prompt": "<the NORMALISED prompt text, i.e. exactly the bytes prompt_digest hashed>",
  "recipe": { "session_shape": "…", "policy_shape": "…" }
}
```

`algorithms` is not decoration — it is what tells a future re-keyer what it is converting
*from*. Those four tags are inline literals today (`feed(&mut h, b"key-v1")`,
`b"prompt-v1"`, `b"recipe-v2"`, `b"harvest-tree-v1"`). **Make each a named constant and have
both the hasher and the record read it**, or the record can claim a version the hash did not
use — a second definition of one concept, which is what `CLAUDE.md` forbids.

## Where the input tree comes from

Not from `PhaseRun::work`: by the time `store` runs, the agent has mutated the work tree, so
it is the output. The input is the **seed** — `Sealed<Translate>` for verify,
`CorpusDir` for translate once 7b lands — and `SeededBy<S>` already names that relation in
the type system. Carry the seed to the store call and export it there.

Export on a **miss only**. On a hit nothing is written, and the wasted-materialise cost that
trap 12 describes is unchanged by this PR.

**The exported `input/` must re-hash to exactly `meta.input_tree`.** That is the same
property `code/`/`output_tree` already has, and it is what makes the record trustworthy
rather than decorative. It also means the export predicate and the digest predicate must be
the same one — `CorpusDir` digests through `CDir`'s `|d| d != BuildOutput`, never
`digest_tree`, for the false-hit reason in `spec-7b.md`. If they disagree, this fails loudly
on the first entry written, which is the correct outcome.

Validate it on load, symmetrically with `code/`. That is a second tree re-hash per load —
a few MB against a multi-hour agent run, and it is what catches a corrupted or hand-edited
input tree.

## THE test — the one that makes this real

**`an_entry_can_recompute_the_key_it_is_filed_under`.** From the entry directory *alone*,
with no access to the original run:

1. hash `input/` → must equal `meta.input_tree`;
2. hash the recorded normalised prompt → must equal `meta.prompt`;
3. hash the recorded recipe shape → must equal `meta.recipe`;
4. feed all eight components → must equal the **directory name**.

If that passes, the entry is provably self-sufficient for re-keying. Non-vacuity, both
directions, because a test that inspects nothing is worse than no test: flip one byte inside
`input/` and assert the recomputed key **differs**; likewise for one byte of the recorded
prompt.

Second test: **`an_entry_written_before_this_change_is_still_served`** — an entry with no
`input/` and no `key-preimage.json` must still hit. See below.

## What must NOT change

- **No `SCHEMA` bump, and not one key byte moves.** This PR only adds recorded files.
  Measure the verify key for a fixed input against `origin/main` and report both.
- **The four real entries on disk must still hit.** They have no `input/` and no
  `key-preimage.json`, and neither may be treated as a miss. Contrast `agent/run.json`,
  which *is* a miss on purpose (`cache.rs:646-651`): a missing provenance publishes a replay
  whose metrics show no cost, which reads as a free run. A missing preimage costs nothing at
  replay time — only future re-keying. Refusing those entries would throw away 99 MB of real
  artifacts for an audit file. **But if `input/` is present it must validate**, so a
  corrupted one is loud rather than silently served.
- **No backfill.** The four existing entries' prompt preimages were never written and cannot
  be recovered; say so in the commit message rather than fabricating them. They will be
  unrecoverable if an algorithm changes, and that is acceptable for four entries.
- Do not rename `hash_tree`, `digest_tree`, `scrub`, `classify` or `visit`.
  `the_digest_path_is_lossless` asserts those exact `(file, fn)` names and that `hash_tree`
  still calls `as_encoded_bytes`; its failure message reads like a rule bug rather than your
  rename.
- `Mode::Bypass` stores no entry, so it records nothing. One line in the report confirming
  that, not a special case in the code.

## Secrets

**Nothing may enter the record that a digest would refuse.** That keeps the exposure delta at
exactly zero: `Session::shape()` is already hashed, and `cache.rs` already refuses to hash
raw argv, which is where C2SaferRust's `BEDROCK_API_KEY` lives (`-e BEDROCK_API_KEY=`).

Two required assertions, both concrete:

- a c2saferrust-shaped recipe's `key-preimage.json` contains neither `BEDROCK_API_KEY` nor
  its value;
- the recorded prompt is the **normalised** text, never the raw prompt. The raw one embeds
  `/local/home/...`, which would make the record machine-specific and re-import the problem
  `normalise` exists to solve. Assert no absolute host path appears in `key-preimage.json`.

## Depends on 7b

It touches `cache.rs`, `artifact.rs` and `agents/run.rs`; 7b rewrites the latter two. Land
after it — and **before the next harvest-bench sweep**, so every entry the sweep writes is
recoverable. Entries written before this lands are not.

## Constraints

- No visibility widening; report instead.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion, no
  `.stderr` re-record beyond a path/column shift.
- No new dependency. Nothing here needs one.
- `CYCLE_BASELINE` unchanged unless the rule says otherwise; report what it printed if so.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`, deleted with
  one absolute path, and if the delete is denied report the path and move on.
- Answer, for every check your diff touches: **after my change, what input still makes this
  check fail?** Name it.

## Acceptance criteria

The ten gates (see `docs/HANDOFF.md` — nine plus CI's release build), the golden fingerprint
passing and not skipping, plus:

- the verify cache key for a fixed input **unchanged** from `main`, measured both sides;
- `SCHEMA` unmoved, with evidence;
- all four existing entries on disk still served, demonstrated;
- the recompute-the-key test passing, with the flipped-byte run shown failing.

## Commit message

Which three preimages were missing and what upcoming change would have moved each; that the
input tree is stored verbatim rather than as a manifest and why that is both more complete
and simpler; where the seed is exported from and that `input/` re-hashes to `meta.input_tree`;
that the four algorithm tags became named constants read by both the hasher and the record;
that a missing preimage is not a miss while a present-but-wrong `input/` is; that no key
moved and `SCHEMA` did not; the measured size delta per entry; and the two secret assertions.
