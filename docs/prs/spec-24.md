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

## Acceptance criteria

The eleven gates, plus:

1. **Scoring something the pipeline did not produce does not compile.** `Source::Archive` and
   `archived_artifacts` are gone; show the compile-fail case or the deleted API, and name what an
   external caller can still reach.
2. **One command produces everything**, demonstrated end to end: artifacts resolved, `.eval/` created
   empty and removed, scores printed, `tables/` written — with `0 agent invocation(s)` on every tally
   line under `--replay-only`.
3. **The scope is derived, shown by earning and losing one.** Move one battery's entries aside: it
   must drop out of scope and out of `tables/`, loudly. Restore them: it returns. No file edited
   either way.
4. **A partial run does not write `tables/`**, shown by running one battery and diffing `tables/`
   to empty.
5. **`tables/` contains exactly the derived scope** — measured today that is `claude` × five
   batteries, and the 16 other agents are absent. Quote the row count before and after.
6. **The 40 golden digests unchanged**, fingerprint passing and not skipping; both keys and `SCHEMA`
   unchanged with the probe output.
7. **CI is one job, not a hand-listed matrix**, and its scope comes from the store.

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
