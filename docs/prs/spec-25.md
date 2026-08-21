# PR 25 — `build-<variant>/` is build output. The oracle guard is right; the classifier is wrong.

## The failure, measured

`P01_sphincs_plus`'s verify was run on 2026-08-20 and refused:

```
Error: the agent modified the C oracle source: build-blake-robust-128f/build.log was added,
and is not a compiled build product. The C side is the reference the translation is graded
against; a run that changes it has not been verified against the original program.
```

That is PR 14's oracle guard (`OracleChange::Added`) doing exactly its job. The defect is one level
down, in `domain/contents.rs`:

```rust
BUILD_DIRS.iter().any(|d| d.as_bytes() == s) || s.starts_with(b"cbuild")
```

`BUILD_DIRS` is `["target", "build", "c_build", "build_c", "artifacts", "gtest_build", "CMakeFiles",
"e2e_out", "build_ffi", "fuzz_scripts"]`, matched **exactly** per path component, plus a `cbuild`
prefix. `build-blake-robust-128f` is neither, so everything under it is `StoreAndHash` — and since
nothing under `c_src/` is ever `Ignore` (deliberately: 26 real `c_src/doc/footer.html.bak` files once
sat in that blind spot), the build log reads as an added reference file.

SPHINCS+ builds one directory per variant — `build-<hash>-<robust|simple>-<size>` — so this is not one
stray file. Any project that emits `build-<variant>/` hits it, and the run is refused **after** the
agent has been paid: the run that found this cost ~3 hours.

## Why the obvious fix is wrong

Adding `s.starts_with(b"build-")` beside the `cbuild` clause misclassifies a **file**. The predicate
runs over *every* component including the last, so a source file named `build-config.c` would become
`BuildOutput` — neither hashed nor carried — and would vanish from the digest and from every published
artifact **silently**. That is the "check that can pass while seeing nothing" shape, one layer down: a
digest that no longer covers a source file.

`cbuild` already carries that latent trap. Leave it: its only occurrence in the tree is
`HarvestBench/claude/jansson/verified/logs/cbuild.log`, already `Ignore` under `logs/`, so touching it
risks moving a digest for no gain. Fix the new rule properly and note the old one.

## The rule

**A path component that starts with `build-` is build output when it is a DIRECTORY — that is, when it
is not the last component of the path.** `classify` already receives the whole `RelPath`, so this is
decidable without touching the walker: check `components()` excluding the final one.

Prefer expressing it so the distinction is visible in the type or the name rather than as an index
arithmetic detail — `is_build_dir_component(..)` over `if i < n - 1`.

## Measured: this moves no digest

The whole point of checking before touching `classify`, which feeds `hash_tree` and therefore every
tree digest and every cache key's `input_tree`:

| where | components starting with `build-` | currently hashed? |
|---|---|---|
| `test-corpus/Public-Tests` (the corpus) | **0** | — |
| cached trees (`code/`, `input/`) | 3, all `logs/build-*.log` | **no** — `logs` is in `ROOT_ONLY_IGNORED_DIRS` |
| `results/` | 1116, all under `target/` | **no** — already `BuildOutput` |
| any FILE named `build-*` outside `target/`/`logs/` | **0** | — |

So no currently-hashed path is reclassified, and the 415 stored entries keep validating. **Re-measure
all four rows before relying on them** — the tree moves — and if any row is nonzero, stop: a digest
change invalidates the store and is a `SCHEMA` question, not a classification tweak.

## Acceptance criteria

The eleven gates, plus:

1. **The 40 golden digests unchanged**, fingerprint passing and not skipping. This is the load-bearing
   gate for this PR.
2. **All 415 cache entries still validate**, shown by a replay of one earned battery reporting all
   hits and `0 agent invocation(s)`. A reclassification that moved a digest would quarantine entries
   instead — that is the failure mode to demonstrate the absence of.
3. **A directory is reclassified, a file is not**, asserted exhaustively over the pairs that matter:
   `c_src/build-blake-robust-128f/build.log` → `BuildOutput`;
   `c_src/build-config.c` → `StoreAndHash`;
   `build-x/y/z.o` → `BuildOutput`;
   `src/build-helper.rs` → `StoreAndHash`.
   The file cases are the ones that make this rule safe, so they are not optional.
4. **The oracle guard still refuses a real modification.** Plant an edit to a recorded `c_src` file and
   an added `c_src/doc/note.bak`, and show `OracleChange::Edited` and `Added` still fire — this PR must
   narrow nothing but the build-directory case. Name the input that still makes the guard refuse.
5. **Mutate**: drop the directory-only restriction so the rule also matches the final component, and
   show criterion 3's file cases go red.
6. Both keys unchanged and `SCHEMA` still 4, with the probe output quoted.

## What this does and does not unblock

**Does:** any project emitting `build-<variant>/` can be verified at all. P01's verify was refused for
this and nothing else.

**Does not:** make P01 cacheable, publishable, or CI-validated. P01 is one shared-source group, groups
open at `SHARED_SOURCE_CACHE = Mode::Bypass`, so it mints no key and stores no entry — see
`spec-7c.md`, and `spec-24.md` for why an unattributable number should not be published at all. After
this PR, re-verifying P01 costs a fresh ~3-hour paid run and yields a number that still nothing can
attest. **So do not sequence a P01 re-run off this PR as though it fixed the publishing problem.** It
fixes the execution bug; `spec-24` and `#38`/`spec-7c` decide whether the number may be published.

## Commit message

That the oracle guard was right and the classifier wrong: `BUILD_DIRS` matches components exactly plus
a `cbuild` prefix, so SPHINCS+'s per-variant `build-<hash>-<robust|simple>-<size>/` directories were
not recognised and a build log inside one read as an added reference file — refusing P01's verify after
~3 hours of paid work. That the rule applies to DIRECTORY components only, because the predicate also
sees the final component and a source file named `build-config.c` would otherwise be dropped from the
digest silently, which is the same shape one layer down; and that `cbuild` keeps its latent version of
that trap deliberately, since its only occurrence is already `Ignore` under `logs/` and touching it
would risk a digest for no gain. The four measurements showing no currently-hashed path is
reclassified, the 40 golden digests unchanged, and 415 entries still validating. And that this unblocks
execution only — P01 remains uncacheable and unpublishable until `#38`/`spec-7c`.
