# PR 16 — Two things make the cache unable to accumulate

Both are key-composition defects, both need a `SCHEMA` bump, and doing them in one PR means one
bump instead of two. **Part A is the more important one.**

# Part A — `cli` must not be a key component

## The defect

`CliVersion` is fed to the key (`cache.rs:356`, and `"cli"` is in `KeyInputs::VALIDATED`), with
the recorded reason that "the CLIs auto-update through a shim". That reason is an argument for
**recording** it, not for keying it — and the file already contains the correct precedent, sitting
beside `harness` in `KeyInputs::meta`:

> Recorded for audit, deliberately NOT keyed and not among the fields `load` re-compares: every
> harness commit would otherwise empty the cache, including commits that cannot affect an
> artifact. When a change genuinely alters what an artifact IS, bump `SCHEMA` by hand.

That applies to `cli` verbatim, and more forcefully. The harness commit changes when *we* change
it; the agent CLI auto-updates on a vendor's release schedule, several times a month. A key
component that turns over weekly means the store can never accumulate.

**Measured, and this is the whole argument:** `results/.cache` holds four entries, 102 MB, all
`phase=verified agent=claude`. Every one is unreachable, because they record
`cli claude 2.1.232.657 (ASBX Claude Code, channel stable)` and the installed CLI reports
`2.1.233.669`. The cache has never served a single hit in production for this reason. Translate is
a measured $795.59 per harvest-bench sweep and verify about $970; keying `cli` throws that saving
away on every vendor release.

What actually determines the artifact is the **model**, the **prompt**, the **corpus** and the
**toolchain** — all four already keyed. A CLI patch bump is client-side.

## The change

Move `cli` from the key to the record: keep it in `meta.json` for audit, remove it from the
hashed components and from `VALIDATED`. `SCHEMA` is the lever for the case where a CLI release
genuinely changes what the agent produces, exactly as it is for the harness.

Do **not** delete `CliVersion` or stop probing it. Provenance still needs it, `preflight_check`
still refuses on it, and an artifact must still record what produced it.

## Required tests

1. **`two_runs_under_different_cli_versions_share_an_entry`** — same model, prompt, corpus and
   toolchain, different `CliVersion`, one key. Show it red before the change; that failure is the
   defect.
2. **`the_cli_version_is_still_recorded_in_the_entry`** — it must not vanish from `meta.json`, and
   `load` must not start refusing entries over it.
3. **`the_model_still_matters`** — the anti-loosening guard. Removing a component from a key is a
   loosening by construction, so assert every remaining component still changes the key. There is
   an existing test of this shape (`model must matter`, `agent must matter`, …); confirm it covers
   all seven survivors and fails if one stops being fed.

Test 3 is what stops Part A from being a hole. If a component can be dropped and no test notices,
the next one will be dropped by accident.

# Part B — both keys move with the checkout's location

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

## Both parts change every key, so ONE `SCHEMA` bump covers them

`SCHEMA` is the manual lever for "a change genuinely alters what a key means". Bump it, and say
in the commit message why.

**Land it soon, because it is nearly free right now.** The four entries on disk are already
unreachable (Part A), so bumping `SCHEMA` today strands nothing that is not already stranded.
After this PR the store can finally accumulate: a vendor CLI release stops invalidating it, and a
differently-located checkout stops producing different keys. Every week this waits, it costs
another sweep.

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

- **Part A:** two `CliVersion` values producing one key, measured; `cli` still present in
  `meta.json`; every remaining component still shown to matter;
- **Part B:** the measured before/after keys from two checkout paths, showing they differed and
  now agree;
- `SCHEMA` bumped once, with the old and new values stated;
- all six tests, with evidence each can fail.

## Commit message

For Part A: that `cli` was keyed while `harness` -- the same class of value, changing less often --
was deliberately not, quoting that precedent; the measured evidence that the four stored entries
were already unreachable because the CLI moved 2.1.232.657 -> 2.1.233.669; what still determines
the artifact (model, prompt, corpus, toolchain, all keyed); and that SCHEMA remains the lever for a
CLI release that genuinely changes output.

For Part B: that both keys moved with the checkout location and why (`denied_read_roots` denies the repo's
parent, which had no token); that `$HOME` was previously tokenising it only by substring
accident because `Roots::resolve` did not canonicalise while `denied_read_roots` did; the
measured keys from two paths before and after; the `SCHEMA` bump and that it stranded nothing,
because the four entries on disk were already unreachable after the CLI version moved; and the
three tests with the evidence each can fail.
