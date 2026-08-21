# PR 25 — An addition is the build's own where the reference never stood. No name list.

## The failure, measured

`P01_sphincs_plus`'s verify was run on 2026-08-20 and refused:

```
Error: the agent modified the C oracle source: build-blake-robust-128f/build.log was added,
and is not a compiled build product. The C side is the reference the translation is graded
against; a run that changes it has not been verified against the original program.
```

PR 14's oracle guard is right. What refused is one level down, and **not** where the first draft of
this spec said it was.

## Where it actually refuses, probed rather than reasoned

The refusal comes from `OracleFiles::judge`, whose addition loop asks a magic-byte-and-extension
question:

```rust
if !is_build_product(&rel, &head(&abs)?) {
    return tampered(OracleChange::Added, &rel);
}
```

A build log is not ELF, and `log` is not in `BUILD_PRODUCT_EXTS`. That is the whole bug.

**The first draft proposed adding a `build-` arm to `classify`'s build-directory check. That would not
have fixed it.** `classify` is never asked. Probed directly:

```
classify("build-blake-robust-128f/build.log")  =>  Ignore      ← the oracle walk's actual input
classify("build/x.log")                        =>  Ignore
classify("build/CMakeCache.txt")               =>  BuildOutput
```

`is_ignored` runs first and returns `Ignore` for anything ending `.log`, and `oracle_admits` is
`d != Disposition::BuildOutput` — it *admits* `Ignore` deliberately, because the oracle walk roots at
`c_src` itself where root-anchored rules cannot see the prefix, and narrowing it loses 26 stored cases'
`doc/footer.html.bak`. So the log is admitted, reaches `judge`, and is refused there.

Two further claims in the first draft were wrong, and are retracted here rather than quietly dropped:

- **"`c_src/build/x.log` refuses too."** It does not. `OracleDir::walk` classifies each *directory*
  before descending, so a directory whose name is in `BUILD_DIRS` is rejected and never opened. The bug
  needs a build directory whose name is unlisted **and** which holds no CMake evidence — SPHINCS+ uses
  plain Makefiles, so `is_cmake_build_dir` (`CMakeCache.txt` or a `CMakeFiles/` child) misses it too.
- **"108 stored files are hashed into `OracleDir::digest`."** They are not. Probing a real stored tree —
  `Test-Corpus/kiro/B02_synthetic/macrodepth_add_5` — `OracleDir::contents()` admits **4 files, zero
  `.log`, zero under `build_*/`**: each `build_*` directory holds a `CMakeFiles/` child, so the cmake
  sniff excludes it. Measuring filesystem paths against a predicate is not measuring what the walker
  admits.

## The rule

`BUILD_DIRS` is a name list grown once per project — `target`, `build`, `c_build`, `build_c`,
`artifacts`, `gtest_build`, `CMakeFiles`, `e2e_out`, `build_ffi`, `fuzz_scripts`, plus a `cbuild`
prefix. `build-<variant>/` would be the twelfth entry. The guard has something better available to it:
**the reference snapshot itself.**

> An addition is the build's own iff it is a build product by sniff, **or the reference recorded nothing
> anywhere in the directory that holds it.**

A directory the reference never occupied is a directory the build created. Nothing about SPHINCS+, no
names, no prefixes, and it covers every future project's convention at once.

```rust
fn occupied_dir_of(&self, rel: &RelPath) -> bool {
    let dir = rel.as_path().parent().unwrap_or(Path::new(""));
    self.0.is_empty() || self.0.keys().any(|k| k.as_path().starts_with(dir))
}
```

`parent()` is the safety property: it drops the final component, so the rule is about the *directory*
and a reference file named `build-config.c` can never be waved through. `is_empty()` is
`Reference::Ungraded` — a run handed no reference may not invent one, so nothing there is a build's
either. That branch is not decoration: without it the existing test
`a_tree_that_never_had_an_oracle_seals_but_may_not_invent_one` goes red, which is how it was found.

## After this change, what input still makes the guard refuse?

Named, because a guard whose failing input you cannot produce is not a guard:

| input | verdict | why |
|---|---|---|
| `evil.c` added at the `c_src` root | **refuses** | the reference occupies `.` |
| `src/extra.c`, where a glob would sweep it up | **refuses** | occupies `src/` |
| `include/shim.h`, pre-empting an include | **refuses** | occupies `include/` |
| `doc/note.bak`, beside the 26 real `footer.html.bak` | **refuses** | occupies `doc/` |
| any recorded file edited, removed or hidden | **refuses** | the first loop, untouched |
| an invented `c_src` where none was handed over | **refuses** | `Reference::Ungraded` |
| `build-blake-robust-128f/build.log` | the build's; dropped | unoccupied directory |
| `obj/notes.txt` — an unlisted name, no cmake evidence | the build's; dropped | same rule, no name needed |
| `src/foo.o`, `test_runner` (bare ELF), `src/lib.gcda` | the build's; dropped | `is_build_product`, unchanged |

**The residual widening, stated:** an agent could add a *source* file inside a brand-new directory and
have the C build pick it up, where today that refuses. It is narrow — the file is deleted by
`drop_build_products` before anything is hashed or published, and *moving* a reference file there still
trips `Removed` — but it is real. The knob that closes it is to require the new directory to hold at
least one sniff-detected build product; not taken, because a C build that emitted only a log would then
refuse for the wrong reason.

## Blast radius: none, by construction

`is_build_product` and `occupied_dir_of` are reachable only from `OracleFiles::judge`. Neither is in
`classify`, `hash_tree`, `digest_tree` or any cache key. So:

- **No digest can move.** The 40 golden digests are unchanged and the fixture's own "pins nothing"
  guard held (`integration`: 10 passed, 0 ignored).
- **`SCHEMA` is still 4** and `KeyInputs` still names the same seven components.
- **All 415 stored entries still validate**, shown by `tools/reproduce.sh all` replaying every phase
  with `0 run` and `0 agent invocation(s)` and `tables/` byte-identical.
- **`classify` is untouched.** The first draft's `classify` edit is dropped: measured, there are **0**
  `build-`-prefixed components anywhere in the tree, so it protected against a hypothetical while
  putting every tree digest at risk.

## Acceptance criteria

The eleven gates, plus:

1. **The nine rows above, pinned exhaustively** in one table-driven test. The four refusals are what
   make the widening safe, so they are not optional.
2. **Two mutations, both red.** Make `occupied_dir_of` return `true` (the old behaviour): the accepted
   rows fail with the *exact* production message, `build-blake-robust-128f/build.log was added, and is
   not a compiled build product`. Drop the `parent()` restriction so the rule reads the file's own path:
   red again.
3. **Golden digests, `SCHEMA` and both keys unchanged**, with the probe output quoted.
4. **A replay of the earned scope reports all hits**, proving no entry was quarantined.
5. **Comment budget pruned, not raised** — the ceiling is 3150 and was already at it, so the five
   verbatim copies of a note in `cache.rs` that restated a `KeyInputs<'_>` borrow the compiler enforces
   are deduped instead.

## What this does and does not unblock

**Does:** any project that builds into a directory the corpus did not ship can be verified at all,
whatever it names it.

**Does not:** make P01 publishable. P01 is one shared-source group, groups open at
`SHARED_SOURCE_CACHE = Mode::Bypass`, so it mints no key and stores no entry — exactly what `spec-24`
now prints out loud (`P01_sphincs_plus: out of scope — the store serves 0 of its 128 case(s), 128 with
no key`). After this PR a P01 re-verify costs a fresh ~3-hour paid run and still yields a number nothing
can attest. **Do not sequence a P01 re-run off this PR as though it fixed publishing.** `#38`/`spec-7c`
decides that.

## Commit message

That the oracle guard was right and the refusal came from `is_build_product`'s magic-and-extension
sniff, not from `classify` — which is never asked, because `is_ignored` returns `Ignore` for a `.log`
first and `oracle_admits` admits `Ignore` deliberately. That the fix is to ask the reference instead of
a name list: an addition is the build's own where the reference recorded nothing in its directory, which
retires the reason `BUILD_DIRS` grew once per project. That `parent()` is the safety property and
`is_empty()` is `Reference::Ungraded`, found by an existing test going red. The nine pinned rows and
both mutations, including the one that reproduces the production message verbatim. That nothing can move
a digest because neither function is in `classify` or any key — 40 goldens unchanged, `SCHEMA` still 4,
415 entries still validating. The three claims retracted from the first draft, measured rather than
reasoned. And that this unblocks execution only: P01 stays out of `tables/` until `#38`/`spec-7c`.
