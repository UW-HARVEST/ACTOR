# PR 7c — Shared-source groups: one key, N publishes

## Depends on 7b

7b caches the agentic **single-case** path. This extends it to shared-source groups. If 7b has
not landed, stop.

## What a shared-source group is

One agent invocation serves N configurations. `translate_one_shared` (`translate.rs:269`)
translates `group.real_case`, then `propagate_config` (called at `:201`) derives each follower
config from it. The followers are **not copies**: they differ in default features, in the
`[lib]` name, and `propagate_config_phase` strips `main.rs` and tests from them.

## The rule: one key, not N

The N followers are *derived* trees, so they can never be byte-identical to the stored
artifact. Therefore:

- **Key and store the real case only.** Publish it from `run_cached`, then run the existing
  propagate loop over the followers exactly as the code does today.
- **Do not give followers their own cache keys.** That would key N invocations that never
  happened, and a hit on a follower key would serve a derived tree as if an agent had produced
  it.

The propagate loop already runs on the skip path (the "already done" branch at `:179`), so it
is replay-safe as written — a replay of the real case followed by propagation produces the same
followers a fresh run would. **Verify that claim rather than assuming it**; it is the crux of
this PR.

## The digest subtlety, already established

All N configs' `test_case` directories are **symlinks** to the real case's, and
`artifact::digest_tree`/`CorpusDir` follow symlinks to hash content. So the input digest is
shared by construction across the group — which is exactly what makes one key correct. Do not
add a per-config component to the group key; that resurrects N keys.

Confirm the symlink claim on the current tree before relying on it, and say what you found.

## Required tests

1. **A replayed group produces the same followers as a fresh one.** Translate a group, capture
   every follower tree, clear the published output, translate again from the cache, and assert
   the followers are identical. This is the property the whole design rests on.
2. **A follower is never served from the store as if it were an invocation.** Assert the store
   holds exactly one entry for a group of N, and that the follower configs have no entry of
   their own.

Named after the failure, per `CLAUDE.md`.

## Constraints

- No visibility widening; report instead.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion, no
  `.stderr` re-record beyond a path/column shift.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`, deleted with one
  absolute path; if the delete is denied, report the path and move on.
- Answer for every check your diff touches: **after my change, what input still makes this
  check fail?**

## Acceptance criteria

The nine gates (see `docs/HANDOFF.md`), plus the golden fingerprint passing and not skipping,
plus: the verify cache key for a fixed input unchanged, and `SCHEMA` unmoved with evidence.

## Commit message

Whether the propagate loop was already replay-safe and how you verified it; the symlink finding;
that the group takes one key and followers take none; and the two tests with evidence each can
fail.
