# PR 10 — The renames: make the state types read as the directories they are

## Why last

Purely mechanical, and it touches ten column-exact `.stderr` files. Everything else lands
first so the renames happen once, against a settled tree.

## The renames

The state types are adjectives where they should be nouns; `Scrubbed` does not tell you it is a
directory.

| now | after |
|---|---|
| `Scratch` | `ScratchDir` |
| `WorkTree<P>` | `WorkTree<P>` — unchanged, "Tree" already says it |
| `Scrubbed<P>` | `ScrubbedTree<P>` |
| `Sealed<P>` | `SealedTree<P>` |
| `CDir` | `OracleDir` — its job is "the reference we grade against", not "a C directory" |
| `Corpus` / `CorpusDir` | `CorpusDir` (check which it is on the tree at the time) |

## What makes this risky, and it is not the rename

**`Sealed` appears in rule bodies as a string literal.** `sealed_implements_only_debug` matches
`type_name(&imp.self_ty) != "Sealed"`. If the type is renamed and that literal is not, the rule
**silently stops guarding anything** — it will inspect zero impls and report green. That is the
worst outcome available here and it will not fail loudly.

So before renaming: grep every occurrence of each old name across `tools/src/**` **and**
`tools/tests/**`, including inside string literals, test names, `TYPESTATE_ORDER`, and any
`ALLOWED` list. Report the count per name per file. Then after renaming, prove the rules still
bind — for `sealed_implements_only_debug` specifically, plant an offending trait impl on the
renamed type in a scratch copy and show the rule FAILS, then remove it. A rename that leaves a
rule inspecting nothing is exactly the failure mode `CLAUDE.md` names.

## The `.stderr` files

Ten cases under `tools/tests/compile-fail/`. Renaming changes the type names in their expected
output, and the files are column-exact.

- Re-record on the **pinned 1.94.0** with `RUSTUP_TOOLCHAIN` unset. A `.stderr` recorded under
  1.97.1 has already shipped a red `main` once.
- Re-record **one at a time**, and after each, `diff` it against the previous version and
  confirm only the type name, line numbers and column widths moved.
- Never run a blanket `TRYBUILD=overwrite`. `compile_fail_cases_still_assert_what_they_were_written_for`
  pins each case's error code; check every pinned code survives, and if one legitimately changed
  say which and why.

## Constraints

- Names only. No behaviour change, no signature change beyond the type name, no visibility
  change. `git diff` should contain nothing but renames, `use` lines, and `.stderr` updates.
- Do not rename `WorkTree` — it already reads correctly, and leaving it reduces the blast radius.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion.
- `CYCLE_BASELINE` unchanged; `MIN_FILES` unchanged (no files added).
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`.

## Acceptance criteria

The nine gates (see `docs/HANDOFF.md`), plus the golden fingerprint passing and not skipping,
plus:

- **every cache key unchanged**, measured. A rename must not touch a digest — if it does,
  something is hashing a type name, which is itself a finding worth reporting;
- proof that `sealed_implements_only_debug` still binds after the rename, by planting a
  violation and showing it red.

## Commit message

The rename table; the pre-rename occurrence counts including string literals; that
`sealed_implements_only_debug` was proven still to bind by a planted violation; which `.stderr`
files changed and that each diff is type name plus line/column only, with every pinned error
code intact; and that no cache key moved.
