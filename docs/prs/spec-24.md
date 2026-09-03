# PR 24 — One invocation produces everything. A number that no cached agent call backs cannot exist.

**The operator's design, stated first because everything below is a consequence of it:**

> The pipeline runs start to finish in one shot and produces every output — artifacts, scores,
> `tables/` — from that one run. The agent calls are cached, so nothing is stale. Everything else is
> recreated on the fly, every time. There is no manual step and nothing to check afterwards, because
> there is nothing that could have drifted.

Today the harness is three commands that can each be pointed at anything: `translate`/`verify` resolve
through the store, `test` scores **whatever is in `results/`**, and `report` aggregates **whatever
`test` last left there**. The last two are the hole. `Source::Archive` reads a phase dir with no key,
mints `Keying::Unkeyable`, and scores it; `report` then publishes that number into `tables/`.

## What that costs today, measured

| | cache entries | rows in `tables/results.md` |
|---|---|---|
| `claude` | **415** | yes |
| the other 16 agents — c2rust, laertes, kiro, codex-gpt54/55, kimi, gpt-5.4, gemini-3.1-pro-preview, c2saferrust, smartc2rust, claude-combined/-minimal/-no-iter/-no-features/-no-subtask/-cross-prompt | **0** | yes |

**One of seventeen published agents has a single cached agent call.** And of claude's six batteries,
`P01_sphincs_plus` has none either — 128 of its 338 phase records carry no `cache_key`.

So ~95% of the published numbers rest on artifacts nothing can attest. They are not *stale* in the
`014_dead_code_lib` sense — most are internally consistent — they are **unattributable**: no key
proves they came from the inputs the repo currently holds. `test --check` already says as much out
loud, and `tables/` does not.

**The operator's decision, reaffirmed twice: the archival path goes.** Not a label on the number — the
capability. A number no cached agent call backs must be unrepresentable.

## The shape

One entry point. For each case in scope, in one process:

1. **translate** — resolve through the store; a hit replays, a miss runs the agent and stores.
2. **verify** — the same, seeded from what step 1 resolved.
3. **materialise** into `.eval/`, created empty this run.
4. **score** there.
5. **emit `tables/`** from what steps 1–4 produced, and nothing else.

Every artifact below the agent calls is derived on the fly. The only durable state is the store.

## Part 1 — Delete the archival path

- **`Source` loses `Archive`.** It becomes a struct, not an enum: there is one source, this run's
  resolution. `Provenance` and its `announce()` go with it — a banner distinguishing two sources is
  dead weight when there is one.
- **`artifact::archived_artifacts` is deleted.** It is the reader that turns a phase dir into a
  `Published` with no key.
- **`Published::unkeyed_from_phase_dir` survives, narrowed.** It is still needed where no key CAN be
  asked — an unkeyed backend, or a shared-source group at `SHARED_SOURCE_CACHE = Mode::Bypass` — and
  PR 21 already records that as `Keying::Unkeyable`. What must go is calling it for a case the store
  could have answered for. State which call sites remain and why each is unavoidable.
- **`Command::Test` and `Command::Report` stop being entry points.** Scoring and reporting become
  steps of the run. Deleting the subcommands is what makes "score something the pipeline did not
  produce" fail to compile rather than fail a check.

If `Command::Test` must survive for an operator workflow, say which and make it resolve through the
store like everything else — never from `results/`.

## Part 2 — `tables/` is an output of a whole-scope run, never of a partial one

The trap this walks into: if tables are regenerated from one run, then running one battery **erases
every other agent's rows**. So:

- A run whose scope is the full published set writes `tables/`.
- A run over one battery **reports its numbers and does not write `tables/`**, and says so. This is
  exactly PR 22's `Covers::Subset` rule — *"a subset's count is not the battery's"* — generalised one
  level up, and it should reuse that type rather than grow a second spelling of it.

## Part 3 — The published set is DERIVED from the store, never listed

**This is the operator's "no manual check" requirement, and it is the part most likely to be got
wrong.** PR #120 hand-listed four batteries in `validate.yaml`; that is a second definition of "what
is cacheable" and it drifts the moment a battery is earned or lost.

Instead: **discover the scope from the store.** An agent/battery pair is in scope iff the store holds
an entry for every case's derived key. Then:

- earning a battery puts it in scope with no edit anywhere;
- losing one takes it out, loudly, rather than silently comparing against a number nobody can
  reproduce;
- `tables/` contains exactly the pairs the store can back, by construction.

The mechanical rule, and it needs no special cases — measured: only `claude` has phase records at all,
and exactly its five earned batteries carry a `cache_key` that resolves. The other 16 agents have no
records to stamp, so they fall out of scope without being named anywhere.

## Part 4 — What this costs, stated plainly so nobody is surprised

**`tables/` loses 16 of 17 agents.** The paper's comparison — c2rust, laertes, kiro, the Codex and
query baselines, the six prompt-sensitivity ablations — disappears from the generated tables until
each is re-run through the cache.

Two of those cannot be re-run into the cache *at all* today, and this PR does not change that:

- **Unkeyed backends mint no `Completed`** (c2rust, laertes, c2saferrust, smartc2rust, opencode,
  oneshot), so `run_cached` cannot key them. That is backlog **#38**.
- **Shared-source groups open at `Mode::Bypass`** and mint no key, which is why `P01_sphincs_plus`
  has zero entries. That is **`spec-7c`**, shelved.

So this PR makes the guarantee real and simultaneously makes those two items **the gate on
republishing the comparison**. That is the trade the operator has accepted; write it down here so a
later reader does not think the numbers were lost by accident.

**Artifacts are NOT deleted.** "Structurally impossible" is about the capability to publish an
unattested number, not about erasing evidence. `results/CRUST` (580 MB) and `results/CRUST-blind`
(873 MB) and the 990 `Cargo.lock` files remain untouched — the operator set that constraint and this
PR does not revisit it. Historic artifacts stay on disk and out of the tables.

## Constraints

- **No key may move and `SCHEMA` stays 4.** This PR deletes readers; it must not touch key
  derivation. Prove with a probe on base and branch and quote it.
- **`--replay-only` must still make a paid run unreachable**, and the run must still assert `0 run`
  and `0 agent invocation(s)` rather than trusting exit 0.
- **Do not weaken the enrichment check** to bring B02_synthetic into scope. Its three `macrodepth_*`
  followers drift because each replay re-propagates the group crate — `spec-7c`'s path. If the derived
  scope includes it and it fails, that is information, not a reason to loosen a check.
- Do not modify `test-corpus`. Never write to `/tmp`. No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED
  growth, no threshold raised. The comment budget is at **3150 of 3150** — prune, do not raise.
- Answer, for every check the diff touches: **after my change, what input still makes this check
  fail?** Name it.

## Acceptance criteria, measured

The eleven gates, all green (`fmt`, lib+bin 251 tests, architecture, `compile_fail`, both clippys,
`doc`, release build, `test_comment_budget.py`, `comment_budget.py --max-comments 3150 --max-ratio 20`
at **3150/3150** — pruned to the ceiling, not raised — and `check_paths.py`). Plus:

1. **Scoring something the pipeline did not produce does not compile.**
   `tests/compile-fail/a_score_cannot_come_from_a_phase_dir.rs` pins both doors: `Source::Archive` is
   E0599 and `artifact::archived_artifacts` is E0425. Proven non-vacuous — re-adding a
   `pub fn archived_artifacts` stub turns the case red (`mismatch`, 1 failed), and removing it green
   again. What an external caller can still reach: nothing. `Published`'s only path-taking mint,
   `unkeyed_from_phase_dir`, is `pub(crate)` (already pinned as E0624 by the PR-21 case), and
   `publish_unsealed` and `from_published_tree` are `pub(crate)`/private too.
2. **One command produces everything.** `tools/reproduce.sh all` exits 0: 5 translate + 4 verify
   tally lines, every one `N hit / 0 run (0 agent invocation(s))` — the operator's two cache hits per
   case — `.eval/` created empty and empty again afterwards, scores printed, all six `tables/` files
   written, and `git diff --exit-code -- tables/` clean.
3. **The scope is derived, shown by earning and losing one.** P00's two entries renamed aside in the
   store (no file edited): the run refuses, exit 1 —
   *"--replay-only: 1 of 1 case(s) had no stored entry for this run's key, so nothing was reproduced
   and no number here is measured"* — and `tables/` does not move. Renamed back: P00 returns, exit 0,
   `tables/` byte-identical, `git -C results status -- .cache` clean.

   **This is not what this spec predicted, and the difference is worth stating.** It said the battery
   would drop out of scope. It does not, and should not: a case that *can* have a key but whose entry
   is absent is earnable by a paid run, so refusing is right — CI must not publish a narrower table
   because the store lost an entry. Scope narrowing is for cases that can never be keyed at all:
   `B02_synthetic` (3 of 42) and `P01_sphincs_plus` (128 of 128) are shared-source followers, opened at
   `SHARED_SOURCE_CACHE = Mode::Bypass`, so they mint no key and no sweep at any price changes that
   until `spec-7c`. Both are announced by name with their counts and excluded from `tables/`.
4. **A partial run does not write `tables/`.** `--replay-only run B01_organic`: 38 translate hits, 38
   verify hits, exit 0, `git diff --exit-code -- tables/` clean.
5. **`tables/` contains exactly the derived scope.** `results.md` **119 → 13** data rows;
   `tractor.tex` **77 → 5**. Agents present: **17 → 1** (`claude`; the other 16 are absent, each
   announced as "not resolved by this run, so it is not reported" — 97 pairs). Batteries present:
   **5 → 4**. So the measured scope is `claude` × four batteries, not five as drafted above:
   `B02_synthetic` drops for the reason in (3).
6. **The 40 golden digests unchanged**, `tests/integration.rs` 10 passed / **0 ignored** — nothing
   skipped, and the fixture's own `pins nothing` guard held. `SCHEMA` still **4**, `KeyInputs` still
   the same seven components (`phase, agent, model, toolchain, prompt, recipe, input_tree`).
7. **CI is one job.** `validate.yaml` is `reproduce` alone, running `./tools/reproduce.sh all`; its
   scope comes from the store, so no battery is named anywhere in it. The `archive` matrix is deleted
   rather than left on `workflow_dispatch` — its command *was* `test all --check`, which no longer
   exists.

## Measured side effects, stated rather than left to be found

- **An out-of-scope battery loses its `result.json` files in the `results/` working tree.** Translate
  republishes on a hit, `clear_phase` empties the phase dir immediately before, and `result.json` is
  written by the score — which an out-of-scope battery never reaches. Measured: 78 files under
  `Test-Corpus/claude/B02_synthetic` (39 cases × 2 phases). Nothing commits them, `report` does not
  read them (the battery is out of scope), and `tables/` is byte-identical either way — but they were
  restored with `git checkout` rather than left staged for someone to commit. This PR opens the window
  by creating out-of-scope batteries at all; the durable fix belongs with `spec-7c`, which removes the
  category.
- **A replay leaves exactly one `Test-Corpus` record modified**, and it is noise, not drift:
  `B02_organic/underhanded-c-nuke_lib/verified/result.json` embeds the runner's PID in a panic
  message (`thread 'main' (2761648)` → `(3734255)`). The counts are identical, so no table input moves.
  This is why "`results/` is clean after a replay" is not added as a gate: it would fire on a PID.
- **`TestMode::Check` now has no production caller** — `main.rs` uses `Update`, and `Check` is reached
  only from tests. It is left in place rather than deleted in the same PR as the archival path, and is
  the obvious next cleanup: the comparison it performs is what `git diff -- tables/` does one level out.

## Commit message

That ~95% of the published numbers rested on artifacts nothing could attest — one of seventeen agents
had a single cached agent call — and that this deletes the capability rather than labelling the
number. What the pipeline becomes: one invocation that resolves both phases through the store,
materialises into a tree created empty, scores it and emits `tables/`, so every output below the agent
calls is derived every time and nothing can drift. That the published set is DERIVED from the store
rather than listed, so earning a battery brings it into scope with no edit and losing one removes it
loudly. That a partial run may not write `tables/`, reusing `Covers::Subset` rather than a second
spelling of it. That the tables consequently lose 16 of 17 agents, that #38 and `spec-7c` are now the
gate on republishing them, and that artifacts were not deleted — `CRUST`, `CRUST-blind` and the 990
`Cargo.lock` files are untouched. And that no key moved and `SCHEMA` is still 4.
