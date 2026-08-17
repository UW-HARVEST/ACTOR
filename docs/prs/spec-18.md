# PR 18 — The structural pass: split, rename, delete. No behaviour change.

**Supersedes `spec-8.md` and `spec-10.md`, and absorbs the dead-code items of `spec-15.md`.**

## Why these batch safely when PR 7 did not

`spec-7.md` bundled nine changes and stalled an agent after 198 tool calls with nothing produced,
which is why this sequence has kept PRs to one concern. The lesson was not "never batch" — it was
**batch by verification method, not by file**. PR 7's nine changes each needed their own argument
about correctness. Everything here is verified by the *same* mechanical check:

> Nothing changed but names and locations. The 40 golden digests are unchanged, both cache keys are
> unchanged for fixed inputs, and every moved item's token stream is byte-identical apart from its
> `use` lines.

That is one question, it is answerable by command rather than by reading, and it is a *stronger*
gate than the per-item review that three separate PRs would have received. So this is one PR.

**The corollary is a hard rule for this PR: if any part of it needs a behaviour argument, that part
does not belong here.** Take it out, say so, and leave it. A single behaviour change hiding inside a
pure-move PR is exactly what makes a large diff unreviewable.

## Part 1 — Break the last two dependency cycles

`CYCLE_BASELINE` is `["agents", "artifact", "battery", "cache", "cli"]`. Two mutual pairs remain,
and each is two items wide. **Re-measure both before starting; the tree has moved since these were
recorded.**

As last measured:

```
battery.rs -> crate::cache::{AgentKey, Mode}
cache.rs   -> crate::battery::{TRANSLATED, VERIFIED, phase_dir}
```

**Cut 1: cache policy comes off `Paths`.** `battery.rs` had `pub cache_mode: crate::cache::Mode`,
and `AgentKey` is the other reference. `Paths` is a layout type — where things live — and cache
policy is not layout. Thread the mode from the CLI to the one place that opens the store. Note that
PR 12 added `cli::CacheMode`, `cli::Reuse` and `CacheMode::honouring`, so the CLI side of this
already exists; check what remains.

**Cut 2: phase-directory naming belongs where the phases are defined.** `cache.rs` reaches into
`battery` only for `TRANSLATED`, `VERIFIED` and `phase_dir`. `artifact::Phase` already owns `DIR`,
and `KeyInputs.phase` already derives from `P::DIR`. Take the naming from the trait `cache` already
depends on. If some spelling genuinely cannot, say which and why.

Shrink `CYCLE_BASELINE` to whatever the rule prints, and quote old and new membership.

## Part 2 — Split the two god-modules

`battery.rs` holds four unrelated concepts:

| concept | goes to |
|---|---|
| case/battery/config discovery (`discover`, `Battery::discover`) | `dataset/discover.rs` |
| path layout (`Paths`, `phase_dir`, `case_dir`, `input_dir`) | `dataset/layout.rs` |
| harvest-bench project handling | `dataset/harvest_bench.rs` |
| `Credits` and `Usd` | `domain/money.rs` — pure newtypes |

`cache.rs` splits along the seam that already exists:

| concept | goes to |
|---|---|
| `KeyInputs`, `Recipe`, `normalise`, `prompt_digest`, the digest newtypes | `cache/key.rs` |
| `Store`, `obtain`, `load`, quarantine, `restore_log` | `cache/store.rs` |

**The digest newtypes travel with the code that constructs them.** This has bitten three times. If
splitting forces a `pub(crate)` constructor on any digest, stop and keep them together —
`digests_cannot_be_fabricated` exists to prevent exactly that, and PR 14 added a compile-fail case
(`oracle_cannot_be_forged`) for the same reason.

`Credits`/`Usd` moving to `domain/` means the layer-purity rule applies: confirm they name no
`std::fs`, `std::process` or `std::env`.

## Part 3 — The renames

The state types are adjectives where they should be nouns, and `Scrubbed` does not tell you it is a
directory. **Re-derive this table against current `main` before starting** — PR 14 added `Oracle`,
`OracleFiles` and `OracleChange`, and PR 12 added `SkipCheck` and `displace_phase`, so the set has
grown since it was written.

| now | after |
|---|---|
| `Scratch` | `ScratchDir` |
| `Scrubbed<P>` | `ScrubbedTree<P>` |
| `Sealed<P>` | `SealedTree<P>` |
| `CDir` | `OracleDir` — its job is "the reference we grade against" |
| `WorkTree<P>` | unchanged; "Tree" already says it, and leaving it shrinks the blast radius |

### The rename hazard, which is not the rename

**`Sealed` appears in a rule body as a string literal.** `sealed_implements_only_debug` matches
`type_name(&imp.self_ty) != "Sealed"`. Rename the type without the literal and the rule **silently
stops guarding anything** — it inspects zero impls and reports green. That is the worst outcome
available here and it will not fail loudly.

So: before renaming, grep every occurrence of each old name across `tools/src/**` **and**
`tools/tests/**`, *including inside string literals*, `TYPESTATE_ORDER`, and any `ALLOWED` list, and
report the count per name per file. After renaming, **prove each affected rule still binds by
planting a violation and showing it red**, then removing it. A rename that leaves a rule inspecting
nothing is the failure `CLAUDE.md` names.

### The `.stderr` files

Eleven cases under `tools/tests/compile-fail/` as of PR 14 (`oracle_cannot_be_forged` is the
newest). Renaming changes the type names in their expected output, and the files are column-exact.

- Re-record on the **pinned 1.94.0** with `RUSTUP_TOOLCHAIN` unset. A `.stderr` recorded under
  1.97.1 has shipped a red `main` once.
- Re-record **one at a time**, and after each, diff against the previous version and confirm only
  the type name, line numbers and column widths moved.
- Never run a blanket `TRYBUILD=overwrite`.
  `compile_fail_cases_still_assert_what_they_were_written_for` pins each case's error code; confirm
  every pinned code survives, and if one legitimately changed, say which and why.

## Part 4 — Delete what cannot happen

From `spec-15.md`, the items that are pure deletion:

- **`Outcome::Nothing` is unreachable for translate.** Its `compute` returns `Ok(Some(..))` or
  `Err`, so the `ensure!(matches!(outcome, Outcome::Published(_)), ..)` guard describes a state that
  cannot occur. Make it representable-and-handled or delete the arm; do not leave a guard whose
  message is fiction.
- **`Backend::OpenCode(_) => bail!(..)` inside `Launch::Keyed`** is unreachable: `resolve_launch` is
  the only constructor of `Launch::Keyed` and never builds it for opencode.
- **`Launch` is a fourth spelling of the backend set** — `Agent` → `InTool` → `Launch` → `Backend`.
  `CLAUDE.md`: one definition per concept. Collapse at least one level, or write down what distinct
  question each of the four answers.
- **The dead second oracle check in `seal`** if PR 14 has not already removed it — check first;
  PR 14's diff was reported to delete it.

If any of these turns out to need a behaviour argument, leave it and say so. That is the rule above.

## Constraints

- **Pure moves and renames only.** `git diff` should contain nothing but relocations, `use` lines,
  names and `.stderr` updates. Report anything beyond that; if it is a behaviour change, remove it.
- **No visibility may widen to make a move work.** If it must, the item is in the wrong layer —
  leave it and say so. Prove it by diffing every declaration's visibility against the base.
- `MIN_FILES` must equal the measured count minus the files removed, with the comment updated to the
  measurement.
- Rules that key on module paths or filenames will need repointing:
  `no_public_path_escapes_the_artifact_modules`, `digests_cannot_be_fabricated`,
  `only_battery_defines_the_has_crate_predicate`, `the_digest_path_is_lossless`. The third names
  `battery` in its own title — decide what it means after the split and say so.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`; destructive commands
  name one absolute path as the whole command.
- Answer, for every check your diff touches: **after my change, what input still makes this check
  fail?** Name it.

## Acceptance criteria — all mechanical, all by command

The eleven gates (see `docs/HANDOFF.md`), plus:

1. **The 40 golden digests unchanged**, fingerprint passing and not skipping;
2. **Both cache keys unchanged for fixed inputs**, measured on the base tree and on the branch with
   a probe test — this PR moves the key-derivation code, so a silent key change would invalidate
   every entry on disk;
3. **`SCHEMA` unmoved**, with evidence;
4. **Every moved item's token stream byte-identical** apart from `use` lines. Extract before and
   after and diff; a real move diffs to empty. This is the check that makes the whole batch
   reviewable, so do it for every moved item, not a sample;
5. **Every renamed-through rule proven still to bind** by a planted violation shown red;
6. `CYCLE_BASELINE` shrunk to what the rule prints.

## Commit message

The two cuts and which edge each removed, with old and new `CYCLE_BASELINE`; how each module was
split and anything that stayed because moving it would have widened visibility; whether the digest
newtypes could be separated from their constructors; the rename table with pre-rename occurrence
counts including string literals; that each affected rule was proven still to bind by a planted
violation; which `.stderr` files changed and that each diff is names plus line/column only with
every pinned error code intact; what was deleted as unreachable; and that both keys, `SCHEMA` and
the 40 golden digests are unchanged.
