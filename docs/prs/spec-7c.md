# PR 7c (rewritten) — A shared-source group earns one key, and its followers are derived from it

**Supersedes the shelved version of this spec** (branch `pr7c-shared-groups`, `0e0ce41`, +731/−84 —
do not resume it). That spec was written before 7b, 12, 21 and 24 landed. Three of its four blockers
are now solved by code already on `main`, which is why this is a ~130-line change rather than +731.

## Why this is now the only thing between us and a full Test-Corpus table

After PR 24, a battery is published only if **every** case came from a keyed replay. Two batteries
fail that, and they fail it for the same reason:

```
⏭️  B02_synthetic:    the store serves 39 of its 42 case(s)  (0 unresolved,   3 with no key)
⏭️  P01_sphincs_plus: the store serves  0 of its 128 case(s) (0 unresolved, 128 with no key)
```

`B02_synthetic/macrodepth_*` (3 configs) and `P01_sphincs_plus` (128 configs) are the corpus's only
shared-source groups, and both mint no key because of one line:

```rust
// translate.rs:719
const SHARED_SOURCE_CACHE: cache::Mode = cache::Mode::Bypass;
```

Before PR 24 an unkeyed group was still *published* (from the archival path), so this spec was
optional. It is not any more: unkeyed now means absent from `tables/`. Fixing it takes `claude` from
**168 to 338 cases** — the whole corpus.

## What the shelved spec's four blockers cost today

| shelved blocker | status on `main` |
|---|---|
| **1.** Followers must be re-derived whenever the real case was not itself skipped, and the decision must be test-gated — on the branch it was an untestable inline `if/else`. | **Already solved, and there is no decision.** `translate.rs:237` re-derives every follower on *every* run, skipped or not: *"Propagated every run: a crate already there is a PREVIOUS run's derivation of this one."* Nothing to gate. |
| **2.** `propagate_config_phase` never clears its destination, so a re-derivation can leave a stale file from the previous translation. | **Real, and `publish_unsealed` already does it**: it captures the old digest, calls `clear_phase::<P>`, then `assemble`s. Routing the derivation through it is the fix — no new machinery. |
| **3.** The group must not straddle two phases: keying the real case makes a replay delete its `verified/` via `Translate::INVALIDATES` while followers keep theirs, silently losing a case. *"The hard one."* | **Defused by PR 21.** `Publishing::finish` (`artifact.rs:1104`) calls `invalidate_dependents` only when the republish is **not** proven byte-identical. A keyed replay republishes identical bytes, so `verified/` survives; a genuinely changed translate invalidates the real case's *and*, once followers publish through the same path, each follower's own. The group moves together because both ends use one primitive. |
| **4.** `Translating::independent` duplicates `PromptKind::independent`. | Trivial; fold it. |

## The change

**1. The group's real case is keyed like any other case.** `dispatch_translate_shared` opens the
store at `paths.cache_mode` instead of `SHARED_SOURCE_CACHE`, and `translate_one_shared` takes
`translate_skip_check(paths)` instead of `SkipCheck::Keyed.through(SHARED_SOURCE_CACHE)`. The constant
and its bypass go. The key's `input_tree` is the shared source, its prompt is `PromptKind::Shared` —
nothing special.

**2. A follower is published, not written.** `propagate_config_phase` derives into a `Scratch` and
then publishes through `publish_unsealed`/`Publishing::finish`, which gives clearing (blocker 2) and
byte-identical-aware invalidation (blocker 3) for free, and makes a follower's phase dir assemble
by exactly the primitives a keyed case uses.

**3. `Keying` gains a third state, mintable only with the keyed artifact in hand:**

```rust
pub(crate) enum Keying {
    Keyed,
    /// Derived by a deterministic function from a keyed artifact of the same phase — a
    /// shared-source follower, whose crate is `propagate_config`'s output over the group's real
    /// case. Attributable, because the group's key names the inputs, without an entry of its own.
    Derived,
    Unkeyable,
}
```

`publish_derived` takes the source `&Published<P>` **by reference as the proof**: `Derived` cannot be
minted without the artifact it came from, so no caller can claim attribution it does not have. The
source's keying maps `Keyed → Derived`, `Derived → Derived`, `Unkeyable → Unkeyable` — asserted
exhaustively over the input type, because a mapping collapsed to a constant is this repo's most
repeated defect.

`unkeyed_seeds` counts only `Unkeyable`, so followers pass `attests` and both batteries come into
scope. Nothing else needs to distinguish `Derived`; the compiler finds any exhaustive match.

**4. A group's verify gets a 12-hour ceiling; an independent case keeps 3 hours.**

```rust
const VERIFY_TIMEOUT_SECS: u64 = 10_800;        // independent: one case's work
const GROUP_VERIFY_TIMEOUT_SECS: u64 = 43_200;  // a group: N configs' work in one session
```

Measured, and the reason this is scoped rather than global: the 208 stored verify entries hold
**126.6 h** of agent wall-clock, median **29.4 min**, p90 **65 min**, max **2.50 h**, and **zero**
over 3 h. `timeout=` is inside `Session::shape()` → `Recipe::digest` → the verify key, so a global
raise would move all 208 keys and force **126.6 h of paid re-verification to lift a ceiling nothing
came within 30 minutes of**. Scoped, it moves exactly two keys, neither of which has a stored entry.
The distinction is not arbitrary: a group's single verify session covers 3 or 128 configs, so its
budget is a different unit of work. `VERIFY_TIMEOUT_SECS` itself does not change, so all 208 entries
stay valid and CI stays green throughout.

## Acceptance criteria

The eleven gates, plus:

1. **The group's translate is keyed**, shown by an entry appearing under `results/.cache/4/translated/`
   for the real case and a second run replaying it at `0 agent invocation(s)`.
2. **Derivation is deterministic**, shown without an agent and without money: publish a real crate,
   derive a follower, re-derive from the same published crate, and assert the follower's tree digest is
   identical. This is the claim the shelved spec said it existed to prove and never did.
3. **A follower is `Derived`, not `Unkeyable`**, and the `Keyed → Derived → Unkeyable` mapping is
   asserted exhaustively over `Keying`. Mutate: collapse it to `Unkeyable` and both batteries must drop
   out of scope; collapse it to `Keyed` and the distinction that justifies the number is gone.
4. **A re-derivation leaves no stale file**, shown by planting a file in a follower's phase dir that the
   real case does not have and asserting it is gone after the next derivation. This is blocker 2, and it
   is currently reachable — `propagate_config_phase` replaces `src/` and `c_src/` but nothing else.
5. **The group does not straddle two phases**: a byte-identical re-derivation must leave the follower's
   `verified/` intact (PR 21's rule), and a changed derivation must invalidate it. Both directions, or
   this is blocker 3 unfixed.
6. **The group's recipe carries the 12-hour timeout and an independent case's still carries 3 hours** —
   two different `Session::shape()` values, asserted directly. And **all 208 stored verify entries still
   validate**, shown by a replay reporting all hits, which is what proves the scoping worked.
7. **`SHARED_SOURCE_CACHE` is gone**, not left unused.

## Then, and only then, the paid work

| | cases brought in | paid agent calls |
|---|---|---|
| `B02_synthetic/macrodepth_*` | 42 (39 already replay free) | 1 translate + 1 verify |
| `P01_sphincs_plus` | 128 | 1 translate + 1 verify |

**~4 calls, not 258**, because a group is one agent invocation and N derivations. That completes
`claude` × 6 batteries = **338 cases** and every Test-Corpus row in `tables/`.

Two things to watch on that run, neither of which this PR can settle:

- **P01's verify duration is unknown.** The 2026-08-20 transcript did not survive; the real case's
  `verify.log` holds a 16-minute session dated 2026-06-10. The "~3 hours" figure in `spec-25.md` came
  from a report, not a measurement. Record the real number when it runs — if it exceeds 12 h, the
  ceiling question reopens.
- **PR 25 is unproven against the real tree.** Nothing P01 is stored, so the `build-<variant>/` fix is
  verified only by unit rows and a reproduced production message. P01's verify is its first real test.

## Commit message

That after PR 24 an unkeyed case is absent from `tables/` rather than silently published, which makes
shared-source keying the only thing standing between `claude` and all 338 Test-Corpus cases — both
`B02_synthetic` (3 configs) and `P01_sphincs_plus` (128) fail `attests` for the one reason,
`SHARED_SOURCE_CACHE = Mode::Bypass`. That three of the shelved spec's four blockers are already solved
by code on `main`: followers are re-derived unconditionally at `translate.rs:237` so there is no
decision to gate, `publish_unsealed` already clears the destination, and PR 21's byte-identical check in
`Publishing::finish` is what stops a keyed replay from deleting the group's `verified/`. That the
follower's provenance is expressed as `Keying::Derived`, mintable only by handing over the keyed
artifact it came from, with the mapping asserted exhaustively rather than handed in. And that the
12-hour verify ceiling is scoped to groups because the 208 stored entries measure 126.6 h of agent
wall-clock with a 2.50 h maximum and none over 3 h, so a global raise would re-run all of it to lift a
ceiling nothing approached.
