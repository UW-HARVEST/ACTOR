# PR 21 — One pipeline run: resolve every phase through the cache, then evaluate in a folder that did not exist a second ago

**This spec has been rewritten twice, and both earlier versions were worse.** The first added a
provenance stamp, a resolver and a `--allow-stale` flag — machinery to *detect* stale artifacts. The
second threaded a manifest so that readers iterated the run's own output instead of the filesystem.
The operator's design is better than both and this version encodes it:

> One run of the pipeline resolves every phase through the cache — a hit replays, a miss runs the
> agent and stores — and then **materialises the result into a fresh directory and evaluates there.**
> A fully cached battery is **two cache hits per case**, translate and verify, and then an evaluation
> over bytes that were written seconds ago from cache entries.

Staleness is then not detected, not avoided, and not checked. **It is absent**, because the directory
that gets scored is created empty at the start of the run and everything in it is materialised from a
cache entry whose key matches this run's inputs. There is no old file to read because there is no old
file.

## Why this is airtight, in one paragraph

The key covers `phase, agent, model, toolchain, prompt, recipe, input_tree`. If any of those moved,
the key moves and the lookup misses. **So a hit is by construction a run whose inputs are today's
inputs, and its wall-clock age is irrelevant.** Verify's `input_tree` *is* the digest of the tree it
was seeded from, so a verify hit already proves the verification was performed on exactly that
translation — **the phase chain is already inside the key and only needs using.** Feed the evaluation
from the cache and from nothing else, into a directory that starts empty, and the property "what we
scored was produced by this run" holds by construction rather than by inspection.

What may be old is a *value in the cache*, and that is fine: its key says it matches. What must never
happen is a file from a superseded run reaching this run's output. Those are different things, and
only the second is a defect.

---

# Part A — What is wrong today: nineteen production sites, swept

`has_crate` (`battery.rs:30`) is `phase_dir.join("Cargo.toml").is_file()`. **One `stat` is the entire
gate between an arbitrary directory and a published score.** Every site below is production code; the
`#[cfg(test)]` boundary of each file was checked, and `agents/run.rs`'s hits are all inside its test
module and excluded.

## A.1 Which phase gets scored — three independent `stat`-based choosers

| # | site | scope | consequence |
|---|---|---|---|
| 1 | `battery::crate_dir` (`battery.rs:37`) | per case | `verified/` if `has_crate`, else `translated/` |
| 2 | **`has_verified` (`runtests.rs:137`)** | **whole battery** | if **any** case has a `verified/Cargo.toml`, `score_phase(VERIFIED)` runs — and `runtests.rs:147` says *"the LAST phase scored becomes the headline summary"* |
| 3 | `load_summary` (`runtests.rs:651`) | whole battery | prefers `summary.json` over `summary_translated.json` whenever the file exists |

**Site 2 is the mechanism behind "how can there be infra failures and a perfect score."** It is
battery-wide: one surviving `verified/` directory anywhere in the battery promotes the entire
battery's headline number to the verified phase, and every case then contributes whatever crate sits
in its own phase dir. It is not a per-case accident; it is a battery-scope switch thrown by a single
stale directory, and no per-case reasoning finds it.

## A.2 A stale `result.json` survives a run and is then read

| # | site | consequence |
|---|---|---|
| 4 | `write_results` (`runtests.rs:623`) | writes `result.json` only for cases in **this run's** `per_case`; a case absent from the run keeps its previous one |
| 5 | `report.rs:628` | reads `<crate_dir>/result.json` into the published pass/fail table |
| 6 | `enrich_test_corpus` (`oracle/mod.rs:198`) | enriches **every** `result.json` under the battery, both phases, regardless of who produced it — stamping this run's model and credits onto an older score |
| 7 | `check_enrichment` (`runtests.rs:177`) | `--check` compares against `crate_dir(case)/result.json` |

Site 6 is the hazard an existing test comment already names — *"scored as this run's result, with this
run's model and cost stamped on by the enrichers"* — except at battery scope rather than per case.

**An mtime comparison cannot refute this and none is offered as evidence:** 0 of 248
`verified/result.json` are older than the `verification.json` beside them, but `enrich_test_corpus`
rewrites `result.json` in place, so its mtime records the last enrichment, not the run that produced
the score. The mechanism is in the control flow.

## A.3 Stale logs are read as evidence

| # | site | consequence |
|---|---|---|
| 8 | `write_results` (`runtests.rs:626`), `enrich_test_corpus` (`oracle/mod.rs:200`) | `logs/translation.log` and `logs/verify.log` feed `Enrichment::compute` (credits, cost) — and **both** are read from the *same* phase dir |
| 9 | `agent_health::audit`, the infra gate | classify the phase log; a run that wrote none leaves the previous one to be classified |

`clear_phase` preserves `logs/` deliberately — the transcript is teed there while the agent runs — so
a phase that published nothing keeps its old transcript exactly where every reader looks.

## A.4 Skip and gate decisions taken on the presence of old files

| # | site | consequence |
|---|---|---|
| 10 | `verify.rs:101`, `:276` | `already_done(\|\| phase_log::<Verify>(case_dir).exists())` — a stale `verified/logs/verify.log` skips the case |
| 11 | `verify.rs:93` | verify refuses a case unless `has_crate(translated/)` — it gates on an old translation, then (site 16) seeds from it |
| 12 | `translate.rs:700` | `already_done(\|\| has_crate(translated/))` |

**Sites 10 and 12 are already closed on the keyed backends**, and that is the proof this design runs
with the grain of the code rather than against it: `SkipCheck::Keyed` returns `false` unconditionally,
so for Claude and Kiro the phase dir is *already* not consulted. PR 12 did that deliberately. This PR
finishes the same thought everywhere else.

## A.5 Battery and case discovery from the output tree

| # | site | consequence |
|---|---|---|
| 13 | `discover_batteries` (`runtests.rs:101`) | a battery "has cases" if any case has a `translated/` **directory** — not a crate. `014_dead_code_lib`'s logs-only `translated/` counts, so **the experiment's denominator is defined by leftovers** |
| 14 | `stage_phase` (`runtests.rs:306`) | skips a case unless `phase_dir(TRANSLATED).is_dir()` |
| 15 | `report.rs:698`, `:111`, `:309` | the report's case list and tables come from `translated/` existing and from `summary_translated.json` on disk |

## A.6 The seed, the post-seal edit, and two kinds of reused build state

| # | site | consequence |
|---|---|---|
| 16 | `IsolatedWorkDir::<Verify>::new` → `Sealed::adopt` (`work.rs:57`) | **the seed leak** — verify's input is a translation this run did not produce |
| 17 | `post_process_independent` (`translate.rs:1186`), harvest-bench arm (`:726`) | edits the published tree **after** the seal, so what is scored is not what was sealed |
| 18 | **666 `target/` directories inside published phase dirs** | measured under `results/Test-Corpus/claude`; cargo reuses an old build. Two tests already assert `target/` is absent after publish (`run.rs:826`, `:955`) and they pass, so these arrive *afterwards* from the scoring build, and the failure path never clears them |
| 19 | `copy_test_artifacts` (`runtests.rs:311`, `:319`) | `if tv_src.is_dir() && !tv_dst.exists()` — **`test_vectors/` and `runner/` are not re-copied when present**, so a leftover set from an older corpus revision is reused as the oracle's inputs |

## The leak, in one function

`agents/work.rs:57`:

```rust
impl IsolatedWorkDir<crate::artifact::Verify> {
    pub fn new(case_dir: &Path) -> Result<Self> {
        let translated = crate::artifact::Sealed::<crate::artifact::Translate>::adopt(case_dir)
```

and `artifact.rs:971`:

```rust
impl<P: Phase> Sealed<P> {
    pub fn adopt(case_dir: &Path) -> Result<Self> {      // <-- pub, no proof, any directory
        let root = crate::battery::phase_dir(case_dir, P::DIR);
        anyhow::ensure!(root.is_dir(), ..);
        let digest = digest_tree(&root)?;
```

Nineteen lines below it, `Sealed::from_cache` is deliberately held at `pub(crate)`:

> *Kept `pub(crate)`: widening it would be a way to manufacture a `Sealed` without a `Completed`
> proof.*

**`adopt` is that widening, and it is `pub`.** It manufactures a `Sealed<P>` — the type whose entire
purpose is that an infra-failed run cannot become one — from whatever directory happens to exist, with
no `Completed`, no key and no provenance. The door is bolted on one side and open on the other, and
the open side is the phase boundary.

### What site 16 costs today, measured

`results/Test-Corpus/claude/B01_synthetic/014_dead_code_lib`:

```
translated/            logs/ translation.json     success:false  2026-08-17T20:23
translated.displaced/  the crate this run could not replace
verified/              Cargo.toml Cargo.lock c_src/ src/ result.json   success:true  2026-08-13T14:21
```

Translate published nothing. `crate_dir` → `verified/` → the case scored a pass, from a verification
of a translation no longer on disk. That 2026-08-13 verify log holds **1 assistant message and 0 tool
calls** in 16 KB, so its crate can only be the copy `IsolatedWorkDir::<Verify>::new` seeds from
`translated/`. A five-day-old translation, verified by nothing, scored as a pass.

### What site 17 costs today, measured

`post_process_independent` opens `<case>/translated/Cargo.toml` **after** `Sealed::publish` wrote it
and calls `add_workspace`/`set_lib`/`remove_bin`; `strip_for_lib` then deletes `src/main.rs` and
`tests/`. Comparing the store's own two copies for `013_poor_quality_addition` —
`translated/<tk>/code/` (what translate sealed) against `verified/<vk>/input/` (what verify was
seeded from, exported verbatim by PR 11's `Preimage`):

```
Cargo.toml differs:   + "\n[workspace]\n"
Only in verify input: logs/, translation.json      (Ignore-class, unhashed, benign)
```

So `digest(<case>/translated/)` never equals the `output_tree` recorded in that case's own cache
entry — for every case, always. Checking `verified.input_tree == translated.output_tree` across every
stamped case gives **0 intact, 84 broken**: not because the corpus is stale, but because the
published tree is edited after it is sealed, so no recorded digest describes it.

---

# Part B — The design

## B.1 The run

`Command::Run` already exists and already sequences translate → verify → test (`main.rs:49`). It
threads nothing: each step is an independent battery sweep that re-discovers cases on disk. Make it
resolve and thread.

For each case in the battery, **in one pass**:

1. **Translate.** Derive the key from the corpus tree, prompt, model, toolchain and recipe.
   - hit → `Published<Translate>` materialised from the entry's `code/`
   - miss → run the agent; on success store the entry and take its output; on failure record it
     (§B.4) and **stop this case's chain**
2. **Verify.** Derive the key from translate's output plus verify's prompt and the rest.
   - hit → `Published<Verify>`
   - miss → run the agent; same two outcomes
3. **Materialise** the case's final artifact into the evaluation tree (§B.2).

A fully cached battery is therefore **two hits per case and no agent invocations**, which is the
headline acceptance criterion: the run must *print* the hit and miss counts per phase, so "two hits
per case" is observable rather than inferred.

`Published<Translate>` is a stable path plus a digest with no scratch lifetime, so the existing
all-translate-then-all-verify sweep shape and `--parallel N` are unchanged if that is simpler to land;
only the hand-off medium changes. **A case whose chain stopped is absent from the evaluation tree**,
and therefore absent from the score — not defaulted, not carried over, not scored.

## B.2 The evaluation tree — the whole point

MIT's `runtests` is invoked as `python3 -m runtests.rust --root <dir> --subset <dir>` with
`current_dir = corpus_dir` (`runtests.rs:404`). **`--root` may be any directory.** So:

```
<repo>/.eval/<agent>/<battery>/<case>/translated_rust/   <- materialised from the cache entry
<repo>/.eval/<agent>/<battery>/<case>/c_src/
<repo>/.eval/<agent>/<battery>/<case>/test_vectors/      <- copied fresh from the corpus
<repo>/.eval/<agent>/<battery>/<case>/runner/            <- copied fresh from the corpus
```

- **Created empty at the start of every run** and removed at the end (or left for post-mortem behind
  a flag). `.eval/` is gitignored and lives on `/local` beside the repo, never `/tmp` — `/tmp` is
  tmpfs here and this tree holds real bytes.
- **`translated_rust/` is a real directory, not a symlink into `results/`.** The existing
  `stage_phase_for_runtests` symlink trick exists only because the crate lived in a phase dir; here
  the crate is materialised at the name `runtests` hardcodes, so the symlink staging, its guard and
  `unstage_phase` all go.
- **Every byte is materialised this run**, from the cache entry for translate or verify plus the
  corpus for the oracle inputs. Nothing is copied out of `results/`.

This is what makes sites 1–15, 18 and 19 stop existing rather than get fixed:

| sites | why they cannot fire |
|---|---|
| 1, 2, 3 | there is one crate per case in the eval tree — the one the pipeline resolved. No phase to choose between, and no `summary.json` to prefer |
| 4, 5, 6, 7 | a fresh tree holds no `result.json`, so none can be read or re-enriched |
| 8, 9 | logs are materialised from the entry's `agent/run.log`, so a phase's log cannot be read out of another phase's dir |
| 13, 14, 15 | discovery comes from the corpus, which is the experiment's *input*; `results/` describes the output and may not define the denominator |
| 18 | a fresh tree holds no `target/`, and the build products land in a tree that is deleted |
| 19 | `test_vectors/` and `runner/` are copied into an empty directory, so `!tv_dst.exists()` is always true and the "don't re-copy" branch is unreachable |

## B.3 `results/` becomes write-only

`results/` is a shipped git submodule the paper reads, so it keeps its layout and keeps receiving the
artifacts, the logs, the metrics and the scores. But during a run it is **written and never read**:
the pipeline publishes to it *from* the resolved artifacts, and the evaluation reads the eval tree.

That is the single sentence to enforce, and it wants a rule rather than a convention: **no module
outside the pipeline may name a phase directory.** The DAG rules already lex module references, so
this is the same machinery pointed at `phase_dir`/`crate_dir`/`has_crate` call sites. Without it this
PR is a snapshot of nineteen sites rather than a guarantee about the twentieth.

Consequences to carry out:

- **`Sealed::adopt` is deleted.** `IsolatedWorkDir::<Verify>` takes a `Published<Translate>` *value*,
  not a `case_dir`, so site 16 does not compile.
- **`crate_dir` is deleted.** Its doc comment already says *"THE READER RULE: every reader wanting a
  case's current state must come through here"* — the intent was right, the mechanism was a
  convention. Keep the sentence; let the type carry it.
- **Post-processing moves onto `Publishing<P>`** (site 17): `Sealed::publish` returns `Publishing<P>`,
  the `add_workspace`/`set_lib`/`remove_bin`/`strip_for_lib` bodies relocate **verbatim**, and
  `Publishing::finish()` digests the tree as it then stands, yielding `Published<P>`. The artifact
  handed to verify and to the eval tree is therefore the post-processed one, which is what makes
  §B.2's materialisation correct.
- `publish_unsealed` also yields a `Published<P>` — it can digest what it just wrote; what it cannot
  mint is a `Sealed`, having no `Completed`. So the **staleness** guarantee covers all seven arms even
  though the **caching** guarantee covers only the keyed two. `keyed: false` records which.

### No key may move

Verify's seed bytes do not change: post-processing produces the same result, only earlier in the call
graph. Translate's key never included its own output. So **no key moves, no stored entry is
invalidated, and `SCHEMA` stays 4** — and that is to be *proven* by a probe test on both the base tree
and the branch, not asserted. If a key moves, stop: the design claim is wrong and the spec needs
revising before the code does.

## B.4 Always record the run, successful or not

Two changes, and they are deliberately separated so that neither can weaken the other.

**Store the log always.** Today `store` copies `produced.log` and is only reached on success, so a
failed run's transcript exists only in `results/<case>/<phase>/logs/`, where the next attempt
overwrites it. That is how three of tonight's `api_error` attempts became unexaminable: attempt 2's
log replaced attempt 1's at the same path, so the question "had attempt 1 done real work?" is now
unanswerable for those cases. The transcript is the entire post-mortem; it must be stored.

**Record failed runs, in a tree the loader cannot see.**

```
.cache/<SCHEMA>/failed/<phase>/<agent>/<key>/<attempt>/
    meta.json     every key component, plus outcome, the health classification,
                  observed exit, timed_out, duration, timestamp, cli, model
    agent/run.log the transcript
```

- **A failure is recorded, never served.** `Store::load` reads only `.cache/<SCHEMA>/<phase>/...`, so
  it cannot see this tree — which means "a failure can never be replayed as a result" is a property of
  the layout rather than of a condition somebody has to keep writing correctly. The original reasoning
  behind storing nothing stays intact: *"a failure is a property of the moment, not of the inputs, so
  memoising it would make a transient failure permanent"* — so the entry preserves the evidence and
  the next run still recomputes.
- `<attempt>` is a counter over existing entries for that key, so retries accumulate instead of
  overwriting each other. This is what makes "did attempt 1 do real work?" answerable next time.
- **The same read-only, staged-then-renamed discipline as a success entry**, so a killed run leaves an
  orphan under `tmp/` and never a half-written record.
- **`BEDROCK_API_KEY` must not reach any digest or any `meta.json`.** C2SaferRust's environment is in
  scope for this path; the recipe already tokenises policy, and the failure record must be held to the
  same standard.
- `harvest-tools cache` gains a way to list failures per key, because an unexaminable record is not a
  record.

---

# Part C — Constraints

- **No behaviour change in the site-17 move.** The post-processing bodies relocate verbatim; prove it
  by diffing their token streams before and after, which is `spec-18.md`'s gate and the reason that
  batch was reviewable.
- **No key moves and `SCHEMA` stays 4**, proven by probe on base and branch, with the output quoted.
- **No `Sealed` constructible from a path.** After deleting `adopt`, enumerate every remaining
  constructor and name the proof each requires. `from_cache` stays `pub(crate)`.
- **Do not modify `test-corpus`** — MIT's `runtests` is a read-only graded oracle. This PR points
  `--root` at a different directory; it does not touch the scorer.
- **Do not delete anything under `results/`** beyond a case's own phase dirs being republished.
  `results/CRUST` (580 MB) and `results/CRUST-blind` (873 MB) are not to be touched, and the 990
  `Cargo.lock` files under `results/` are not to be deleted.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion, no visibility
  widened to make a move work.
- Never write to `/tmp` (tmpfs here); scratch under `/local/home/scheschb/scratch/<yours>`;
  destructive commands name one absolute path as the whole command.
- Answer, for every check the diff touches: **after my change, what input still makes this check
  fail?** Name it.

# Part D — Acceptance criteria

The eleven gates (`docs/HANDOFF.md`), plus:

1. **Two cache hits per case, printed.** A fully cached `Command::Run` over B01_synthetic reports
   translate 85/85 hits and verify 85/85 hits, zero agent invocations, with a wall-clock number. This
   is the criterion the whole design exists to satisfy.
2. **The evaluation tree did not exist before the run.** Assert it is created empty: plant a file in
   it, run, and show the file gone and the score unchanged.
3. **Site 2 shown red first, because it is the operator's actual complaint.** Build a battery where
   nothing verified but one case has a stale `verified/Cargo.toml`; show it reporting a *verified*
   headline on the base commit, and the translate headline plus one absent case after.
4. **014 shown unreachable, not merely detected.** A case whose `translated/` holds no crate and whose
   `verified/` holds a complete one is reported **absent** from the score. Show the test failing on
   the base commit, where it scores a pass.
5. **A hand-edited `results/` changes no score** — the direct demonstration that the guarantee is
   structural. Edit a byte in a published crate, re-run, show the score identical.
6. **`Sealed::adopt` gone**, plus a twelfth compile-fail case
   `a_published_artifact_cannot_be_adopted_from_a_directory`, red at its pinned error code, with all
   eleven existing codes intact and the count constant moved to twelve.
7. **The rule that stops a twentieth site** shown red by adding a `phase_dir` call in a module that
   may not name one.
8. **A failed run is recorded and not served.** Force a failure, show the record under
   `.cache/<SCHEMA>/failed/...` with its log, then re-run and show the case recomputes rather than
   replaying the failure. Then show two attempts accumulating as `1/` and `2/` rather than
   overwriting.
9. **Both keys unchanged and `SCHEMA` still 4**, with the probe output quoted.
10. **The 40 golden digests unchanged**, fingerprint passing and not skipping.
11. **All nineteen sites accounted for individually in the PR description** — closed by the design,
    closed separately, or deliberately left with the reason. Do not let the remainder go unmentioned
    because the headline change is elsewhere.

# Part E — Commit message

That staleness was never a cache defect and never an age comparison: a hit's key covers every input so
its age is irrelevant, and verify's `input_tree` already *is* translate's output digest, so the phase
chain was already in the key and only needed using. That the fix is to resolve both phases through the
cache and evaluate in a directory created empty that run, so no old file is read because no old file
is present. That a fully cached battery is two hits per case, with the measured counts and wall clock.
The nineteen leak sites with their measurements — and that the one behind the complaint is
`has_verified`, which is **battery-scope**: a single stale `verified/` directory promotes a whole
battery's headline to the verified phase, which is how a sweep full of infra failures reports a perfect
score. That `Sealed::adopt` was `pub` and proofless, manufacturing from any directory the very type
whose invariant is that an infra-failed run cannot become one, nineteen lines below the comment
explaining why `from_cache` is not `pub` for exactly that reason. That failed runs and their
transcripts are now recorded under a path `Store::load` cannot see, so preserving the evidence and
refusing to memoise a transient failure are both properties of the layout. That no key moved, `SCHEMA`
is still 4 and no stored entry was invalidated — with the probe output.
