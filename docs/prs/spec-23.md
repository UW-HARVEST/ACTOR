# PR 23 — A reproducibility CI that replays the cache, regenerates the tables, and cannot spend a cent

**Supersedes `docs/prs/spec-19.md`, and corrects it.** spec-19 said the workflow was "expensive in a
way nobody chose: seven paid agent matrices are being launched on every commit". **That is false and I
wrote it.** Every arm runs `test <target> --check`, which invokes no agent at all — it scores stored
translations. The `terminal_reason=max_turns` spec-19 saw came from `agent_health::audit` classifying
**stored** transcripts, not from fresh runs. The workflow is red because the infra gate correctly
refuses to score dead runs; it was never billing anyone. Lesson, the same one as `spec-20.md`: I read
CI logs and inferred a mechanism instead of reading the command.

## What this replaces

`.github/workflows/validate.yaml` today is a 7-arm matrix (`kiro`, `c2rust`, `claude`, `laertes`,
`kimi`, `oneshot/gpt-5.4`, `oneshot/gemini-3.1-pro-preview`), each running `test all --check` on
push and PR. It has not passed once in a hundred runs, back past 2026-07-05, so **every PR in this
repo has merged on `type safety` alone** and a red check on a PR means nothing.

## What it becomes

One job that does what the operator asked: **run the claude agent over the battery we have actually
earned, replay every phase from the cache, prove it called no agent, score it, regenerate the tables,
and fail if the tables move.**

```
harvest-tools --agent claude --replay-only translate B01_synthetic
harvest-tools --agent claude --replay-only verify    B01_synthetic
harvest-tools --agent claude test B01_synthetic --check
harvest-tools report
git diff --exit-code tables/
```

Measured locally on `2e6acb8`, this is what those first three print:

```
🗃️  cache: translated 85 hit / 0 run (0 agent invocation(s))
🗃️  cache: translated 85 hit / 0 run (0 agent invocation(s))   <- verify's seeding leg
🗃️  cache: verified   85 hit / 0 run (0 agent invocation(s))
  B01_synthetic [verified]: 85/85 cases, 393/393 vectors (100.0%)   ✅ matches summary.json
```

170 replays, two per case, zero agent invocations.

## Part 1 — `--replay-only`, a mode that cannot spend money

`cache::Mode::ReplayOnly` already exists ("Read an entry, and on a miss refuse rather than invoke")
and `cli::seeding` already forces it for verify's translate leg. It has **no user-facing flag**. Add
one.

- `--replay-only` sets the store to `Mode::ReplayOnly` for **every** phase, not just the seeding leg.
- A miss is a **hard error** naming the phase, the derived key and the entry path that was absent. Not
  a warning, not a skip: a reproducibility run that silently ran fewer cases is the vacuous-pass
  defect `spec-22.md` spent two rounds killing.
- **`conflicts_with` `--no-cache` and `--refresh-cache`.** "Replay only" and "do not read the cache"
  are opposite instructions; the CLI is the edge where that gets parsed away, per `CLAUDE.md`.
- **Skip `preflight_check`** (`translate.rs:141`, `:752`). It probes the agent CLI's `--version`, so
  today a machine without `claude` installed cannot even reach the store. Under `--replay-only` no
  agent can be invoked, so its version cannot matter — and `KeyInputs::VALIDATED` (`cache.rs:468`)
  excludes `cli` precisely because #109 took it out of the key. Do **not** paper over this with
  `HARVEST_CLI_VERSION`: that env var makes the run *claim* a version, which is the opposite of not
  needing one.

**The test that matters is not "a hit replays" — it is "a miss cannot pay".** Required:

1. A miss under `--replay-only` errors, and **no agent process is spawned**. Assert on the spawn, not
   on the exit code: a test that only checks the error would pass if the agent ran and then failed.
2. `preflight_check` is not reached — provable by putting no agent binary on `PATH` in the test and
   showing the run still resolves a hit.
3. **Mutate**: make `ReplayOnly` fall through to `compute()` and show test 1 red.

## Part 2 — The cache has to ship, and that is the real decision

`git ls-files .cache` in the `results` submodule returns **0**. The store is untracked, so a runner
has no cache, so `--replay-only` would fail every case — the workflow cannot work until this is
settled. **Measured breakdown of the 133 MB, 175 entries:**

| component | size | needed to replay? |
|---|---|---|
| `agent/run.log` | **87 MB** | **no** — `restore_log` no-ops when absent |
| `code/` | 32 MB | **yes** — this is the artifact |
| `input/` | 12 MB | no — it exists to re-key a future algorithm change (#111) |
| `meta.json`, `key-preimage.json`, `agent/run.json` | ~1 MB | `meta.json` and `run.json` yes |

So a replay needs **~33 MB of the 133 MB**. Three options; **take (a) now and file (c)**:

- **(a) Commit `results/.cache/` as it stands.** 133 MB beside a submodule already holding
  `CRUST` (580 MB) and `CRUST-blind` (873 MB). Simplest, and anyone who clones can reproduce. Cost:
  it grows with every sweep, and 65% of it is transcripts nobody needs to replay.
- **(b) Restore it in CI from an Actions cache.** Cheapest in git, and worthless for the paper: an
  outside reader cloning the repo cannot reproduce anything.
- **(c) `harvest-tools cache export --replay-pack`** writing `code/` + `meta.json` + `agent/run.json`
  only, committed separately. ~33 MB and the right long-term shape, but it is a new command, a second
  store layout and a second thing that can drift from `load`. Not in this PR.

**DECIDED: (a).** The operator chose to commit all 133 MB. Done as `results` commit `59fce01c0` on
branch `harvest-bench-claude-results`, staging `.cache/4` and a `.gitignore` that keeps `.cache/tmp/`
and `.cache/quarantine/` out — staging dirs and disputed entries are transient by construction. Only
`.cache/` and `.gitignore` were staged, verified against the submodule's 13,409 other dirty paths.
This PR bumps the submodule pointer to it. `HANDOFF.md` must stop recording "the cache is untracked"
as a fact, because two specs say so.

## Part 3 — The script is the deliverable, not the YAML

**"The script we run should always produce the tables."** So the five commands above live in **one
committed script** — `tools/reproduce.sh` — that CI and a human run identically. The workflow's job is
to call it and nothing else. A reproducibility procedure that exists only inside a GitHub YAML is not
reproducible.

The script must:

1. `unset RUSTUP_TOOLCHAIN` before anything. It is exported as `1.97.1` on the operator's box and
   **silently overrides** `rust-toolchain.toml`'s 1.94.0 — and the toolchain **is a key component**, so
   getting this wrong makes all 175 entries miss and the script fail for a reason that has nothing to
   do with the code. This has already cost this project a sweep.
2. **Assert the counts, not the exit code.** Grep the run's own summary for `0 run` and
   `0 agent invocation(s)` and fail if either is absent. An exit-0 that quietly paid for a case is
   precisely what this workflow exists to make impossible, and `--replay-only` should make it
   unreachable — so this assertion is the belt to that braces, and it is what makes a green run mean
   something.
3. Regenerate `tables/` **every** run and `git diff --exit-code tables/`. The 6 tracked files in
   `tables/` (`results.md`, `datasets.tex`, `manual.tex`, `numbers.tex`, `prompt-sensitivity.tex`,
   `tractor.tex`) are what the paper reads; a number that moves and a table that does not is the
   defect that matters most here.
4. Print the eval tree's path and confirm it is gone at the end (PR #116 removes it even on the exit-1
   path — keep that observable).
5. Take the battery as an argument, defaulting to `B01_synthetic`, so extending coverage later is a
   parameter and not an edit.

## Part 4 — Two workflow hazards to fix while here

**`git submodule update --init --depth=1 results test-corpus` cannot be trusted.** `--depth=1` fetches
only the tip, so a submodule pinned to any commit that is not the tip cannot be checked out — the
runner then holds a *different* `results` tree than the pin names, which is `spec-20.md`'s unproven
hypothesis for why CI and local disagreed. Drop `--depth=1`, or use `--filter=blob:none` plus an
explicit fetch of the pinned SHA. Then **assert the checked-out submodule SHAs equal the pins** and
print them, so a divergence is a named failure instead of a mystery.

**A prompt change must fail this workflow, loudly and on purpose.** The prompt digest is a key
component, so editing `prompts/claude/*.md` moves every key and `--replay-only` misses. That is
correct and it is the point: a prompt edit invalidates the stored numbers. Say so in the script's
failure message — "the prompt changed, so the stored translations no longer answer this question" —
rather than leaving a reader to decode a missing-entry error.

## Part 5 — What happens to the other six arms

The operator asked to scrap what is there. The archival `test all --check` is nonetheless the thing
that **refuted `spec-20.md`'s false regression claim**, so deleting it outright would remove a
capability that has already paid for itself once.

**DECIDED:** the 7-arm matrix comes off `push`/`pull_request` and survives as `workflow_dispatch` —
same commands, run deliberately. Only checks that *can* pass stay attached to PRs.

Do **not** make either job green by passing `--allow-infra-failures`. That scores infrastructure
failures as results, which is the refusal the harness exists to perform.

## Constraints

- **No key may move and `SCHEMA` stays 4.** This PR adds a mode and a script; it must not touch key
  derivation. Prove with a probe on base and branch and quote it.
- **Do not modify `test-corpus`.** MIT's `runtests` is a read-only graded oracle.
- **Do not write to `results/` beyond what a normal `--check` writes**, and do not delete anything
  under it. `results/CRUST` and `results/CRUST-blind` are untouchable and the 990 `Cargo.lock` files
  stay.
- **Do not commit the six untracked `Test-Corpus/claude/*/summary_translated.json`** files. They are
  local `--update` leftovers; the archive has never filed a translate record for `claude`, and
  committing one changes what the shipped archive claims (see `runtests.rs`'s `Record` comment).
- Never write to `/tmp` (tmpfs here); scratch under `/local/home/scheschb/scratch/<yours>`.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion. The comment budget
  is at **3100 of 3100** — prune your own, do not raise the ceiling.
- Answer, for every check the diff touches: **after my change, what input still makes this check
  fail?** Name it.

## Acceptance criteria

The twelve gates, plus:

1. **`tools/reproduce.sh B01_synthetic` green on this box**, with its output quoted: 85 translate hits,
   85 seeding hits, 85 verify hits, `0 agent invocation(s)` three times, `[verified] 85/85 (393v)`
   matching `summary.json`, and `git diff --exit-code tables/` clean.
2. **A miss cannot pay.** Delete one cache entry, re-run, and show the script fail naming the key —
   with the agent never spawned. Then restore it.
3. **`PATH` without `claude`** still replays, proving `preflight_check` is not on the replay path.
4. **The count assertion fires.** Force one case to be a miss-then-run (e.g. by running without
   `--replay-only`) and show the script rejecting the run for reporting a nonzero `run` count even
   though the command exited 0.
5. **A prompt edit fails it, with the intended message.** Touch one byte of `prompts/claude/verify.md`,
   show the named failure, revert.
6. **Submodule pins asserted**, with the SHAs printed and a deliberate mismatch shown failing.
7. **Both keys unchanged and `SCHEMA` still 4**, probe output quoted; the 40 golden digests unchanged
   and the fingerprint proven non-vacuous.
8. **The workflow attached to PRs contains only the one job**, and the archival matrix is
   `workflow_dispatch`-only. Show the rendered trigger list.

## Commit message

That spec-19's "seven paid agent matrices" was wrong — `test all --check` invokes no agent, and the
`max_turns` came from the infra gate reading stored transcripts — so the six-week red streak was the
gate working, not a cost leak. What the workflow does now: one job replaying `claude/B01_synthetic`
from the cache with a new `--replay-only` that makes a miss a refusal rather than an invocation,
asserting `0 agent invocation(s)` rather than trusting exit 0, then regenerating `tables/` and failing
if they move. That the cache had to ship for any of this to work, with the measured 133 MB breakdown
and which 33 MB a replay actually needs. That `--depth=1` cannot check out a pinned submodule commit
and is gone, with the pins now asserted. That a prompt edit fails the workflow by design, because the
prompt is in the key. That the archival matrix moved to `workflow_dispatch` rather than being deleted,
since it is what refuted spec-20. And that no key moved and `SCHEMA` is still 4.
