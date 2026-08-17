# PR 16 — Both cache keys move with the checkout's location

## The defect

`cache::normalise` exists so a key names *what ran*, not *where it ran*. It tokenises four
roots: `$WORK`, `$REPO`, `$WORKBASE`, `$HOME` (`cache.rs:265-270`). The sandbox policy that
feeds `RecipeDigest` contains a fifth path it has no token for.

`io::sandbox::denied_read_roots` (`io/sandbox.rs:68-77`) denies **the repo's parent** as well as
the repo, deliberately and for a good reason recorded there: reads are default-allow outside
`denyRead`, so a stale sibling results tree was readable, and one audited log really did read a
third run's translated output. That parent path reaches `settings_json`, reaches
`Backend::policy_shape`, and is hashed into `Recipe::digest` — which is a key component of
**both** phases.

So the same code, same model, same CLI, same toolchain and same corpus produce **different keys
from a differently-located checkout**. A cache that is meant to be pushed to the `results`
submodule and reused cannot be, and nothing reports it: the symptom is a store that looks
enabled and never hits.

## The part that is accidental rather than designed

`normalise` substitutes by plain string replacement, and `Roots::resolve` does **not**
canonicalise `HOME` (`io/workdir.rs`, `std::env::var_os("HOME").map(PathBuf::from)`) while
`denied_read_roots` **does** canonicalise its roots. On this host `$HOME` is a symlink into
`/local`, so the policy holds `/local/home/scheschb/research` while the substitution looks for
`/home/scheschb` — and it matches, as a *substring*, producing `/local$HOME/research`.

That accident is doing real work: it tokenises the machine-specific middle of the path. What it
leaves behind is the parent directory's **name**, so:

- cloning to `/local/home/scheschb/work/ACTOR` changes every key;
- the canonicalisation mismatch is load-bearing without being intended, and anyone who "fixes"
  `Roots::resolve` to canonicalise `HOME` silently changes every key.

Measure both of these before changing anything, and report the two digests.

## The fix

The policy *shape* is keyed because a different sandbox policy can change what the agent
produces. What matters is the **structure** — which roots are denied and granted — not their
literal paths. So:

1. Give the repo's parent a token of its own (`$REPOPARENT`, or whatever reads best beside the
   existing four) and add it to `Roots`, so the tokenisation is deliberate rather than a
   substring accident.
2. Canonicalise the roots used for substitution the same way `denied_read_roots` canonicalises
   the roots it writes, so the two agree by construction instead of by coincidence. Record that
   the mismatch previously made `$HOME` match as a substring.
3. Assert the result: **the same inputs from two different checkout paths produce the same
   key.** That is the whole point of the PR and it is the acceptance test.

Do not solve it by removing the parent from `denied_read_roots`. That deny root exists because
an agent really did read a sibling results tree, and weakening the sandbox to stabilise a digest
would trade a correctness property for a caching one.

## This changes every key, so it needs a `SCHEMA` bump

`SCHEMA` is the manual lever for "a change genuinely alters what a key means". Bump it, and say
in the commit message why.

**Land it soon, because it is nearly free right now.** `results/.cache` holds four entries,
102 MB, all `phase=verified agent=claude` — and they are **already unreachable**: they record
`cli claude 2.1.232.657` while the installed CLI reports `2.1.233.669`, so their keys no longer
match anything. Bumping `SCHEMA` today strands nothing that is not already stranded. Every week
this waits, it costs more.

## Required tests

1. **`the_same_inputs_from_two_checkout_paths_produce_the_same_key`** — build `KeyInputs` twice
   with repo roots that differ only in their parent directory's name, and assert one key. Show it
   failing before the change; that failure IS the defect.
2. **`the_policy_shape_names_no_literal_host_path`** — assert the tokenised shape contains no
   `/home`, no `/local`, and no absolute path at all. This is the non-vacuity guard: it must be
   possible to fail it by removing any one token.
3. **`a_denied_root_that_disappears_from_the_policy_changes_the_shape`** — the shape must still
   distinguish genuinely different policies, or this PR has turned the recipe digest into a
   constant. Name the input that makes it fail.

Test 3 is what stops the fix being a loosening. If tokenising can be taken too far, it ends with
a shape that describes every policy identically, and a wrong key is worse than no cache.

## Constraints

- No visibility widening; report instead.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion.
- `denied_read_roots` keeps denying the repo's parent. The sandbox does not get weaker.
- The 40 golden digests must be unchanged: this touches key derivation, not artifact content. If
  one moves, stop — something is hashing a policy into a tree digest.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`.
- Answer, for every check your diff touches: **after my change, what input still makes this
  check fail?** Name it.

## Acceptance criteria

The ten gates plus the release build (see `docs/HANDOFF.md`), the golden fingerprint passing and
not skipping with 40 digests unchanged, plus:

- the measured before/after keys from two checkout paths, showing they differed and now agree;
- `SCHEMA` bumped, with the old and new values stated;
- the three tests, with evidence each can fail.

## Commit message

That both keys moved with the checkout location and why (`denied_read_roots` denies the repo's
parent, which had no token); that `$HOME` was previously tokenising it only by substring
accident because `Roots::resolve` did not canonicalise while `denied_read_roots` did; the
measured keys from two paths before and after; the `SCHEMA` bump and that it stranded nothing,
because the four entries on disk were already unreachable after the CLI version moved; and the
three tests with the evidence each can fail.
