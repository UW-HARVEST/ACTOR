# PR 14 — The oracle check must judge the oracle, not the directory it sits in

## Blocks the next sweep

7b routes translate through `Scrubbed::seal`, which refuses when the C oracle changed. That
refusal is right in principle and wrong in one specific way, and until this lands **no
harvest-bench or Test-Corpus sweep should be run**, because the refusal lowers measured
translation success for a reason that is not the agent's fault.

## The defect

`Scrubbed::seal` (`artifact.rs:528`) compares `CDir(root/c_src).digest()` against the
`c_before` recorded at seed time, and refuses on **any** difference.
`CDir::digest` hashes everything that is not `Disposition::BuildOutput`, and
`classify` decides `BuildOutput` **purely from the directory**:

```rust
fn is_cmake_build_dir(dir: &Path) -> bool {
    dir.join("CMakeCache.txt").is_file() || dir.join("CMakeFiles").is_dir()
}
// plus: any path component in BUILD_DIRS, or starting with "cbuild"
```

So an object file beside its source — `c_src/src/foo.o` — is `StoreAndHash`, is hashed, and
refuses the translation.

**The agent is instructed to produce exactly that.** The translate prompt tells it to build
the C library and run `nm -D` on the resulting `.so`. An in-tree `make` is the natural way to
comply, and it is then punished.

Measured on the stored tree by the 7b reviewer: **71 of 6,312 `translated/*/c_src` trees under
`results/` already hold a `.o`, `.a` or `.so` at a non-build path** — e.g.
`results/CRUST-blind/claude-combined/c_string/translated/c_src/src/string_t.o` — and bare
compiled binaries at the oracle root (`c_src/test_runner`, `c_src/main`, `c_src/tsp`,
`c_src/test_1`) push the true figure higher, since those have no extension to match. Re-measure
with an ELF-magic sniff and report the real number.

## What the check is actually for

The invariant is **"the agent did not tamper with the C reference we grade against."** Against
that:

- a reference file **modified** or **deleted** is tampering, and must refuse;
- a **compiled artefact added** by building the reference is not tampering, and must not;
- a **source or header added** *might* be tampering — a new `.h` can shadow an include and a
  new `.c` can be swept up by a glob in the C build — so it must still refuse.

The current check cannot tell these apart because it only has a whole-directory digest. This
is the second of `CLAUDE.md`'s three moves: the check lacks the information to judge correctly,
so **give it that information**. It is not a licence to loosen it.

## The change

Compare the oracle as *the file set that existed before*, plus a rule for additions:

1. At seed time record the oracle's **file list**, not only its digest. `IsolatedWorkDir`
   already computes `c_before` by walking the corpus, so the list is in hand.
2. At seal time refuse if any recorded file is **missing** or its **content changed**. A
   deletion is currently only implied by a digest difference; make it explicit and named.
3. For files **not** in the recorded list, refuse unless the file is a compiled artefact.
   Decide that by extension (`.o .a .so .lo .la .obj .d .gch .pch .dylib` and numbered
   `.so.N`) **or** by content sniff (ELF `\x7fELF`, `ar` archive `!<arch>\n`, Mach-O). Sniff
   as well as match, because the measured cases include extensionless binaries.

Do **not** change `domain::contents::classify` or `BUILD_DIRS`. `classify` is shared with
`Carry` and `digest_tree`, so touching it moves the 40 pinned golden digests and changes what
`translated/` contains. The discrimination this PR needs belongs to the oracle comparison
alone.

Do **not** change what `input_tree` hashes. The corpus digest is the cache key's only per-case
component for translate (`spec-7b.md`), and a change there is a key change.

## Required tests

1. **`an_added_object_file_beside_its_source_is_not_an_oracle_modification`** — seed a corpus,
   have the run leave `c_src/src/foo.o`, and assert the seal **succeeds**. Show it failing
   before the change, since that is the whole point.
2. **`an_extensionless_compiled_binary_at_the_oracle_root_is_not_an_oracle_modification`** —
   the measured case (`c_src/test_runner`) with ELF magic and no extension.
3. **`an_edited_oracle_source_is_still_refused`** — the invariant must survive. Assert
   `Refusal::OracleModified`.
4. **`a_deleted_oracle_source_is_refused_and_says_so`** — currently only implied by a digest
   difference.
5. **`an_added_header_is_still_refused`** — the shadowing hazard; an addition that is not a
   compiled artefact must not pass.

Tests 3–5 are what stop this PR from being a loosening. If any of them can be made to pass by
deleting the new logic, the logic is wrong.

## Constraints

- No visibility widening; report instead.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion.
- `classify`, `BUILD_DIRS`, `Carry`, `digest_tree` and `hash_tree` untouched. The 40 golden
  digests must be unchanged — that is the evidence you did not touch the artifact predicate.
- Both cache keys unchanged for fixed inputs, measured. `c_before` is not a key component;
  confirm that on the tree rather than assuming it.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`.
- Answer: **after my change, what input still makes this check fail?** Name it — and tests 3–5
  are that answer, so they must exist and must be shown red against a stubbed-out check.

## Acceptance criteria

The ten gates (see `docs/HANDOFF.md`), the golden fingerprint passing and not skipping with 40
digests unchanged, plus:

- the re-measured count of stored `translated/*/c_src` trees that the *old* check would refuse
  and the *new* check accepts, with the ELF sniff applied;
- the five tests, with evidence each can fail.

## Commit message

What the invariant is and the three cases it has to tell apart; that `classify` was
deliberately not touched and why; the measured before/after refusal counts; that a deletion is
now named rather than implied; and the five tests with the evidence each can fail.
