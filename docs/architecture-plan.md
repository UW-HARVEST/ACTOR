# Getting to a functional core with conversion at the edges

The principles are in `CLAUDE.md`. This is the plan for making the code actually obey
them, in reviewable steps.

## Where we are

Measured, not estimated. `tools/src` is 14,671 lines across 19 modules. **Ten of them
form a single dependency cycle:**

```
agent_health · artifact · battery · cache · cargo_toml
cli · opencode · session · translate · verify
```

Ten mutual pairs, including `translate ↔ verify`, `session ↔ translate`,
`opencode ↔ translate`, `cache ↔ translate`, `battery ↔ cache`, `cache ↔ cli`,
`agent_health ↔ artifact`. Only eight modules are cycle-free: `workdir`, `sandbox`,
`provenance`, `refusal`, `scoring`, `test`, `report`, `benchmark`.

So the file names suggest a design, and nothing enforces it. Two symptoms:

* **`translate.rs` is 2,973 lines and depends on 13 of the 19 modules.** It is not a
  module, it is the application — dataset iteration, agent invocation and artifact
  production fused. Most cycles run through it.
* **`battery.rs` is referenced 138 times from 8 modules.** It has become the god-module:
  path layout, case discovery, the `Paths` struct, and the `Credits`/`Usd` money types.

`verify.rs` reaching into `translate.rs` for its *own* work-tree type
(`IsolatedWorkDir`) and its *own* metrics writer (`write_verification_metrics`) is the
clearest single example: 12 references one way, 1 back.

## Where we are going

Dependencies flow one way only:

```
domain ← io ← artifact ← {agents, cache} ← {dataset, oracle, analyse} ← cli/run
```

```
domain/       PURE. May not name std::fs, std::process, std::env.
  agent.rs      Agent, LogFormat
  dataset.rs    Dataset, battery/case identity — names, never paths
  phase.rs      Phase, Translate, Verify, SeedAt, SeededBy
  digest.rs     TreeDigest, CacheKey, PromptDigest — newtypes only
  money.rs      Credits, Usd
  contents.rs   Disposition, Carry, classify — pure decisions about paths
  health.rs     Health, Completed, classify(text, format, exit)
  outcome.rs    pass rules
  provenance.rs assess()

io/           The ONLY layer allowed to touch fs / process / env.
  tree.rs       visit, hash_tree, copy_carrying
  workdir.rs    scratch base, ulimits
  sandbox.rs    settings.json
  git.rs        rev-parse, status
  proc.rs       spawn + tee + timeout

artifact/     The state machine.
  tree.rs       ScratchDir, CorpusDir, WorkTree, ScrubbedTree, SealedTree, OracleDir

agents/       "Run something external, classify what came back, produce a typed result."
  invocation.rs all 17 backends: model + cli + command + policy
  session.rs    the shell recipes
  opencode.rs
  run.rs        run_cached<P> — THE one driver

cache/          key.rs · store.rs
dataset/        discover.rs · layout.rs · harvest_bench.rs
oracle/         runtests.rs · gtest.rs · score.rs
analyse/        metrics.rs · cargo_toml.rs · report.rs

cli.rs        argv → domain types
run.rs        orchestration
```

Two mechanical rules keep it true rather than aspirational, and both are cheap:

* **Layer purity** — `domain/**` may not mention `std::fs`, `std::process` or
  `std::env`. This is "conversion at the edges" as a test rather than a habit. It forces
  one real fix: `agent_health::classify` takes a `&Path` and reads it today; it becomes
  `classify(text, format, exit)` and the read moves to `io/`. It then needs no tempdir to
  test.
* **DAG rule** — the module graph must be acyclic, with today's ten-module cycle recorded
  as a shrink-only baseline. It fails today and stays failing until each cut lands.

`Agents` and `Oracle` deliberately share one mechanism. Both are "run something external,
classify the result, produce a typed value" — that is `run_cached<P>`, one driver with two
configurations. Building them as separate subsystems would recreate the duplication this
plan exists to remove.

## How the operation is kept safe

Only the cycle-breaking commits change behaviour. Everything else is a **pure move**,
and pure moves are mechanically verifiable: extract the token stream of each moved item
before and after, and a real move diffs to empty. That is what makes reorganising ~10k
lines reviewable at all.

Three non-negotiables:

1. **The guards land before anything moves.** `rust_sources()` is a flat `read_dir`;
   the instant a file enters a subdirectory every shape rule reports green while
   inspecting nothing.
2. **A golden fingerprint is recorded first** — every test name and outcome, the nine
   `.stderr` files, and `SealedTree::adopt(case).digest()` over N existing `results/`
   cases. Identical digests afterwards are the proof the artifact pipeline is unchanged.
3. **A differential run at the end** — old binary vs new on one case with `--cache off`,
   comparing produced trees byte for byte.

## The PRs

| # | PR | kind | breaks | verified by |
|---|---|---|---|---|
| 0 | **Delete four shape rules** and their allowlists — **landed** | net removal | the lockstep tax on 3–9 | 10 rules still green |
| 1 | `CorpusDir` + `SeedAt` + `SeededBy` | types | — | 3 new tests + 1 compile-fail case |
| 2 | **Guards**: recursive `rust_sources()` + anti-vacuity, DAG rule w/ baseline, golden fingerprint | additions only | — | rule fails on a planted cycle |
| 3a | `domain/` + layer-purity rule; move only what is already pure | move | — | purity rule; token-diff |
| 3b | the boundary cuts: `classify(text, …)`, `normalise(text, roots)` | 2 cuts | `agent_health ↔ artifact` | purity rule |
| 4 | `io/` moves | move | — | token-diff |
| 5 | `agents/`: session + opencode + `Invocation` out of verify | move + 1 cut | `translate ↔ verify`, `session ↔ translate`, `opencode ↔ translate` | DAG baseline drops 3 |
| 6 | `run_cached<P>`; verify ported onto it | behaviour | — | golden digests identical |
| 7 | **translate ported onto it — the translate cache** | behaviour | — | differential run |
| 8 | `cache/` + `dataset/` split; `cache_mode` off `Paths` | move + 1 cut | `battery ↔ cache`, `cache ↔ cli` | DAG baseline drops 2 |
| 9 | `oracle/` + `analyse/` | move | — | token-diff |
| 10 | renames: `Scrubbed`→`ScrubbedTree`, `Sealed`→`SealedTree`, `CDir`→`OracleDir` | mechanical | — | 9 `.stderr` re-records, one at a time |

### What each PR actually does

**0 — Delete four shape rules (landed).** 33% of the crate was checking code (5,450 lines
against 10,844 doing), and `tests/architecture.rs` was 1,273 lines for 14 rules. Four of
them earned the least and cost the most:

| lines | rule |
|---|---|
| 88 | `safety_gating_bools_are_named_enums` |
| 72 | `money_amounts_cannot_be_substituted_for_one_another` |
| 54 | `no_function_takes_three_interchangeable_primitives` |
| 33 | `a_tuple_return_may_not_repeat_an_element_type` |
| **408** | measured total, with `is_bool_to_enum_boundary`, `collect_enums_and_impls`, `PRIMITIVES`, `ty_key`, `params` and `Param` becoming dead |

Three of them carry **closed, shrink-only `ALLOWED` lists checked in both directions**,
naming `translate_case`, `verify_case`, `dispatch_translate`, `post_process_independent`,
`write_translation_metrics` and others. Every PR from 3 to 9 moves or resignatures one of
those, so every PR from 3 to 9 must edit `architecture.rs` in lockstep or the staleness
half fails. That tax is most of the friction in this plan, and it is paid to enforce
signature patterns rather than structure.

The syn helpers mostly stay — `signatures`, `method_calls`, `returned_ty`,
`mentions_type`, `quote_min` and `is_pathish` are used by the ten rules we keep. `ty_key`,
`params` and `Param` went too: the four rules were their only callers, so `warnings =
"deny"` made keeping them a build error. Rule bodies, allowlists and the helpers that went
with them: 1,273 lines down to 865.

What replaces them is cheaper and catches more. The DAG rule and the layer-purity rule
are ~20 lines each with no allowlist and no type analysis: a cycle, or an `std::fs` inside
`domain/`, is a structural fact. `sealed_implements_only_debug` and
`compile_fail_cases_still_assert_what_they_were_written_for` stay — ~30 lines each,
guarding invariants nothing else can.

**1 — `CorpusDir`.** Translate's input is a plain `&Path` to C sources today, so no
`WorkTree<Translate>` can exist and nothing can be keyed. Adds `CorpusDir` as the missing
edge conversion, `SeedAt` for where a seed lands, and `SeededBy` so only the two real
phase transitions compile. Digests through `OracleDir`, never `digest_tree` — the
root-anchored ignore rules drop `*.bak`/`*.log`/`*.sha256` at the corpus root while the
same files *are* hashed under `c_src/`, so `digest_tree` would let two corpora share an
input digest and replay each other's translation.

**2 — Guards.** The prerequisite. Nothing may move until the shape rules can still see
the code and the DAG rule is ratcheting.

**3a and 3b — `domain/`.** Split in two, because measuring purity showed the layer cannot
be populated by moving files. Only `scoring.rs` (81 lines) and `refusal.rs` (115) are
already free of `std::fs`/`std::process`/`std::env`; `artifact.rs` has 36 `fs` references,
`cache.rs` 38 plus 7 `env`, `battery.rs` 39, `agent_health.rs` 8. Everything else has to be
*split*, not moved. (`cli.rs`'s seven `Command::` hits are clap's subcommand enum, not
`std::process` — it is process-pure.)

So **3a** creates `domain/` with the layer-purity rule and moves only what is already pure:
`scoring.rs` → `domain/outcome.rs`, `Disposition`/`Carry`/`classify` → `domain/contents.rs`,
`RelPath`, the `TreeDigest` newtype, `Agent`/`Dataset`/`LogFormat`. **3b** then makes the
two boundary cuts — `agent_health::classify` taking text instead of a `&Path`, and
`cache::normalise` taking its roots instead of reading `HOME` — each of which is the
canonical "move the read to the edge" change and testable with no tempdir afterwards.

**5 — `agents/`.** The highest-value cut. Those three cycles exist *only* because the
shared invocation machinery lives inside `translate.rs`; extracting it breaks all three
and removes roughly a third of that file.

**6 and 7 — the driver.** `verify_case` and `translate_case_at` become thin callers of
one generic function. Everything below deletes itself as a consequence rather than by
patching:

| today | after |
|---|---|
| 4× `remove_dir_all(case_dir)` | `SealedTree::publish` |
| 4× "make workdir, copy corpus, run, copy back" | one `materialise` + one `publish` |
| 2× metrics writers | one |
| 2× homes for `translation.log` | one, a function of `P` |
| 2× 17-arm dispatch matches | one, parameterised by `PromptKind` |
| `IsolatedWorkDir` (Verify hardcoded) | generic over `P` |

**10 — renames.** Last, because the nine `.stderr` files are column- and
toolchain-exact.

## Why PR 7 is worth reaching

Measured on the 2026-08-15 harvest-bench sweep:

| phase | invocations | cost | cached |
|---|---|---|---|
| translate | 7 | **$795.59** | no |
| verify | 7 | ~$970 | yes |

PR #74 scoped translate caching out because "verify is ~92% of the available saving".
That was Test-Corpus (345 small cases). On harvest-bench the split is ~45/55, so every
sweep currently re-pays about $800 that a cache would return.

## Risks

* **The shrink-only `ALLOWED` lists.** Removed by PR 0, which is why PR 0 goes first. If
  it is skipped, every PR from 3 to 9 must edit `tests/architecture.rs` in the same commit
  or the staleness half of those rules fails.
* **The comment budget is a whole-tree ratio with no headroom, so every deletion PR in
  this plan fails it.** Measured with `tools/comment_budget.py`: `main` is 1,942 comment /
  14,940 counted lines = 12.9987% against `--max 13`, i.e. it passes by 0.0013 percentage
  points. PR 0 removes 26 comment and 351 code lines — 6.9% comment density against the
  tree's 13.0% — which *raises* the ratio to 1,916 / 14,563 = 13.157% and makes the
  required "Comment budget" CI step exit 1. Getting back under 13% needs 27 comment-only
  lines deleted or 175 comment-free code lines added; the whole non-invariant surplus in
  `tests/architecture.rs` (the `//!` header, the eight `// ── An ──` labels, two helper
  doc lines) is 17 lines and still leaves 13.055%, and everything else countable is either
  a rule's WHY documentation or outside PR 0's surface. Scoping tests out is worse, not
  better: excluding `tools/tests/**` measures 1,775 / 13,416 = 13.231%. So the ratio was
  being held under budget by the low-comment checking code this plan exists to delete, and
  PRs 6 and 7 — "everything below deletes itself" — hit the same wall. What the budget
  measures (an absolute comment-line ceiling cannot be tripped by a deletion; a ratio
  can) is a deliberate decision to land as its own change, with the rationale recorded in
  `comment_budget.py`. A deletion PR must not widen it on the way past.

  **What actually happened:** the reviewer above recommended landing the metric change
  separately; PR 0 instead raised the flag 13 → 14 in the same commit, with the reasoning
  recorded beside it in `.github/workflows/type-safety.yaml`. That was a deliberate call
  by the operator, not the implementing agent — the agent correctly refused to touch the
  gate and escalated. The reviewer's preference remains the better shape: replacing the
  ratio with an absolute ceiling is still unlanded work.

* **Three surviving rules key on the literal filename `"artifact.rs"`** —
  `no_public_path_escapes_the_artifact_modules` and `digests_cannot_be_fabricated` iterate
  `["artifact.rs", "cache.rs"]`, and `the_digest_path_is_lossless` names
  `("artifact.rs", "hash_tree" | "digest_tree" | "scrub" | "classify")`. PR 2's recursive
  `rust_sources()` does NOT fix these: they use `src("<name>.rs")` and literal tuples, not
  the walk. Any PR that moves those functions out of `artifact.rs` must make these rules
  module-path aware in the same commit, or they panic with "not found" — which reads like
  a rule bug rather than the rename that caused it.

* **`a_runner_that_errors_is_not_scored_from_the_file_it_left` is flaky, ~1 in 97 suite
  runs**, failing with `ETXTBSY` from `src/test.rs`. The test writes `fake-runner`, chmods
  it 0755 and execs it; with 180 tests in parallel another thread's fork can still hold a
  write fd during the exec. Pre-existing and unrelated to this plan, but it makes the
  autonomous pipeline report a false "not green", so it is worth fixing before relying on
  unattended verdicts.
* **`is_public()` counts `Visibility::Restricted`.** `pub(super)` reads as public to the
  typestate rule, so the state machine's fields stay fully private. (Child modules *can*
  see an ancestor's private fields, so this constrains visibility, not file count.)
* **`.stderr` files are toolchain-sensitive.** Re-record on the pinned 1.94.0 with
  `RUSTUP_TOOLCHAIN` unset, one at a time, never a blanket overwrite. A note line was
  lost this way once already.
* **`results/` and the live sweep.** All work happens in a worktree; the primary checkout
  and `results/` stay untouched.

## Not in scope

* Caching the test phase — ~350 s of a 19.5 h sweep.
* Laertes/C2SaferRust input provenance: their input is reached by path surgery into a
  sibling agent's results tree with no digest, so the key cannot name which c2rust output
  was consumed. Until adopted as a `CorpusDir` they must be keyed as `Mode::Bypass` — a
  wrong key is worse than no cache.
* Unifying `scrub`'s `$HARVEST_WORKDIR` token with `cache::normalise`'s
  `$WORK`/`$REPO`/`$HOME`. Two vocabularies for one concern; worth doing, separable.
