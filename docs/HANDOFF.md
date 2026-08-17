# Handoff: state of the refactor, and how to keep running it

Written to survive a context compaction. `CLAUDE.md` holds the *principles*;
`docs/architecture-plan.md` holds the *target and PR sequence*; the per-PR briefs are in
`docs/prs/`. This file holds the **operational state and the traps** — the things that are
not in any of those and that a fresh context would otherwise rediscover expensively.

## Where the work stands

Eighteen PRs merged (#89–#106). The layered architecture is most of the way in:

```
tools/src/
  domain/   PURE — outcome, contents, relpath, health   (layer-purity rule enforces it)
  io/       external — workdir, sandbox
  agents/   exit, invocation, work, session, opencode, run.rs  <- run_cached<P> lives here
  oracle/   runtests, gtest, score
  analyse/  report, cargo_toml, metrics
  artifact.rs  cache.rs  translate.rs  verify.rs  battery.rs  cli.rs  ...
```

**The module cycle went from 10 modules to 5.** `CYCLE_BASELINE` is now
`["agents", "artifact", "battery", "cache", "cli"]`, enforced shrink-only in both
directions by `a_module_cycle_may_only_shrink`.

`run_cached<P>` exists in `agents/run.rs` and is the **only** `store.obtain` call site in the
crate, enforced by `the_store_is_obtained_from_exactly_one_place`. Verify runs through it.

## What remains

| # | PR | spec | state |
|---|---|---|---|
| 12 | **the skip check consults the store**, not the presence of a `Cargo.toml` | `docs/prs/spec-12.md` | in flight; this is what makes 7b pay |
| 11 | **an entry records the inputs its key came from** — store `input/` and the digest preimages | `docs/prs/spec-11.md` | ready; land before the next sweep |
| 7c | shared-source groups: one key, N publishes | `docs/prs/spec-7c.md` | ready |
| 8 | `cache/` + `dataset/` split; `cache_mode` off `Paths` | `docs/prs/spec-8.md` | ready; breaks `battery ↔ cache`, `cache ↔ cli` |
| 15 | five seams the 7b review exposed, led by a store failure losing a paid artifact | `docs/prs/spec-15.md` | ready; item 1 is a money bug |
| 17 | **invariant I1 holds only on the keyed path** — five unkeyed translate paths uncovered | `docs/prs/spec-17.md` | ready; a wrong published number |
| 16 | **both cache keys move with the checkout's location** | `docs/prs/spec-16.md` | ready; needs a `SCHEMA` bump, nearly free right now |
| 10 | renames: `Scrubbed`→`ScrubbedTree`, `Sealed`→`SealedTree`, `CDir`→`OracleDir` | `docs/prs/spec-10.md` | ready; last, touches 10 column-exact `.stderr` files |
| — | **DEBT: lower `--max-comments` back to the measured total** | — | after PR 10; it was raised 2560 → 3100 as a budget for the sequence |

**Landed since this table was written:** 7b as #105 (translate is memoised; shared-source
groups deliberately left at `Mode::Bypass` for 7c) and 13 as #106 (the comment budget).

**The four cached entries are already unreachable.** They record
`cli claude 2.1.232.657 (ASBX Claude Code, channel stable)`; the installed CLI reports
`2.1.233.669`. `cli` is a deliberate key component because the CLIs auto-update through a shim,
so those entries were stranded by their own key before any of tonight's work — the
provenance-blind `has_crate` skip was hiding it by never asking. Consequences: the next default
sweep re-verifies all four, `HARVEST_CLI_VERSION` is the documented lever for replaying them
deliberately, and a `SCHEMA` bump (PR 16) costs nothing while this is true.

**PR 14 landed (#107), so the sweep is unblocked.** It taught the oracle check to tell a modified
reference from an added build artefact: `Edited`/`Removed`/`Added`/`Hidden`/`Symlinked`, each named.
Measured before merging: zero stored trees newly refuse, and of the 329 the old check refused the
new one accepts about 307. Coverage output counts as build output — in 203 stored trees it is the
only build product present.

**PR T (the `/tmp` fix) landed as #98 — and looking for it in the obvious place says
otherwise.** Worth spelling out, because this handoff briefly claimed it was unlanded on
exactly that evidence:

- The `[env] TMPDIR` block is in the **repo-root** `.cargo/config.toml`, *not*
  `tools/.cargo/config.toml`, and deliberately so: cargo discovers config by walking up
  from the **invocation** directory, and CI runs `cargo test --manifest-path
  tools/Cargo.toml` from the root, which reads nothing below it. So `ls tools/.cargo/`
  returns "No such file or directory" for a fix that is present and working.
- `tempdir().unwrap()` greps to ~76 sites, but they are
  `io::workdir::test_tempdir()` — anchored at `CARGO_MANIFEST_DIR/target/tmp`, so it does
  not even depend on `TMPDIR`. Exactly one `tempfile::tempdir()` remains in the tree and it
  is inside a doc comment. Match on the qualified path, not the suffix.
- Evidence it is holding: `tools/target/tmp` is empty and `/tmp` contains **zero** `.tmp*`
  directories.

`docs/prs/spec-7.md` is superseded by 7a/7b/7c — it bundled nine changes and stalled an agent
after 198 tool calls with nothing produced. **Keep PRs to one concern.**

## How to run one PR (the ritual that works)

```bash
# 1. spec it, commit to main
git add docs/prs/spec-N.md && git commit && git push origin main

# 2. worktree off main
git worktree add -b prN-slug /local/home/scheschb/pr-auto-N main

# 3. run the pipeline -- v2, and PASS THE BASE SHA
Workflow({scriptPath: ".claude/workflows/pr-pipeline2.js",
          args: {pr:"N", worktree:"/local/home/scheschb/pr-auto-N",
                 branch:"prN-slug", spec:"docs/prs/spec-N.md",
                 repo:"/local/home/scheschb/research/ACTOR",
                 base:"<the sha the worktree was cut from>", rounds:2}})
# `base` matters: origin/main moves while a run is in flight, and then every agent reads
# unrelated commits as this branch's deletions. One run lost real time to exactly that.
# v2 also screens each blocking finding with a skeptic before a resolver acts on it, and
# ends with an audit stage asking what every earlier stage did not check.

# 4. when it returns: REBASE, then RE-VERIFY EVERY GATE YOURSELF, then merge
cd /local/home/scheschb/pr-auto-N
git stash -u && git fetch origin && git rebase origin/main && git stash pop
# ... run the gates ...
git add -A && git commit -F - <<'EOF' ... EOF
git push -u origin prN-slug --force-with-lease
gh pr create --title ... --body ...
gh pr checks              # wait for `tests`
gh pr merge N --squash --admin --delete-branch
git worktree remove /local/home/scheschb/pr-auto-N --force
```

**Always rebase and re-verify before merging.** Gates measured against a tree that no longer
exists prove nothing, and the comment budget is measured whole-tree, so two PRs can each pass
and jointly fail.

**Write the commit message from `git diff`, never from the agent's report.** Two reports in
this sequence described trees that were never written: one claimed four items had moved and
been widened when they had not moved at all; one stated in prose that a change was
deliberately NOT made while the diff made it, omitting two changed files. The pipeline now
instructs against this, but verify anyway.

## The ten gates

Run from `tools/`, with `export PATH="$HOME/.cargo/bin:$PATH" && unset RUSTUP_TOOLCHAIN`
first — `RUSTUP_TOOLCHAIN=1.97.1` is exported in the login shell and **silently overrides**
`rust-toolchain.toml`'s 1.94.0. The trybuild `.stderr` files are toolchain-sensitive; a
`.stderr` recorded under the wrong compiler has already shipped a red `main` once.

```
cargo fmt --check
cargo test  --locked --lib --bin harvest-tools
cargo test  --locked --test architecture
cargo test  --locked --test compile_fail
cargo clippy --locked --all-targets
cargo clippy --locked --lib --bins -- -D clippy::panic
cargo doc   --locked --no-deps
python3 tools/test_comment_budget.py                  # the same CI step runs this first
python3 tools/comment_budget.py --max-comments 3100 --max-ratio 20   # root, AFTER `git add -A`
python3 tools/check_paths.py                 # from repo root
```

**An eleventh gate exists in CI and is missing from that list: `cargo build --release --locked`.**
It is the `build` job of `validate-translations`, i.e. the *first* thing CI does and the job
the seven agent matrix arms depend on — so a PR can pass all ten gates locally and still fail
CI before a single test runs. Release differs from the debug builds above in real ways
(`debug_assertions` off, no overflow checks, different dead-code reachability). Run it too:

```
cargo build --release --locked --manifest-path tools/Cargo.toml   # from repo root
```

Those ten are the `type safety` workflow, which runs one more failing command they do not
cover: the `Install the pinned toolchain` step, which exits 1 if `rustc --version` disagrees
with `rust-toolchain.toml` or `rust-src` is missing. The `export PATH` / `unset
RUSTUP_TOOLCHAIN` preamble above is how you satisfy it locally, which is why it is a preamble
here and a step there. `validate-translations` is the expensive one. Neither CI workflow runs
`--test integration`, which is why the golden fingerprint has to be run by hand and why it
silently prints `NO SIGNAL` without `HARVEST_GOLDEN_RESULTS`.

The count is per *command*, not per CI step or per flag — the `Lint` step is one step running
two `clippy` commands, and the `Comment budget` step became two commands when PR 13 added
`tools/test_comment_budget.py`, which is what moved the list from nine to ten. `--max-ratio`
is a second *limit* on the same command, so it did not move the count again. The specs in
`docs/prs/` each froze whatever count was in force when they were written ("the nine gates",
"the ten gates"); they all defer to this list, and this list is the count that is in force.

**PR 13 also changed the flags**, so the invocation in force is the one above,
`--max-comments 3100 --max-ratio 20`; `--max` retired with the whole-tree ratio. Nine older
specs still print `--max 14` (3a, 3b, 4, 5, 6, 9, tmp) or `--max 13` (0, 2) — frozen records of
already-merged PRs, deliberately not rewritten. Copying one exits 2 loudly ("the following
arguments are required: --max-comments, --max-ratio") rather than mismeasuring.

Plus, for any PR that touches the artifact pipeline:

```
HARVEST_GOLDEN_RESULTS=/local/home/scheschb/research/ACTOR/results \
  cargo test --locked --manifest-path tools/Cargo.toml --test integration artifact_fingerprint
```

It must pass **and not skip** — a worktree has no submodules, so without the env var it prints
`NO SIGNAL` and proves nothing. 40 pinned digests.

## Machine constraints — these caused an outage

**30.4 GB RAM, `SwapTotal: 0`.**

**`/tmp` is a tmpfs: files written there are resident RAM, accounted as `Shmem`, and cannot be
reclaimed without swap.** ~13.8 GB of agent scratch and 24,707 leaked test tempdirs sat there
un-evictable, leaving ~0.24 GB free; the kernel spent 2h13m in direct reclaim scanning
10.1M pages/s at 0.74% efficiency, load 395, 142 blocked tasks, and OOM-killed 96 processes
across 28 hours. Never write to `/tmp`. Use `/local/home/scheschb/scratch/<name>`.

**Bound fan-out by memory, not CPU.** `Workflow` caps concurrency at `min(16, CPUs-2)`, which
is memory-blind. Ten agents at ~0.7 GB plus their `cargo` children is most of the headroom.
Run the critique lenses in waves rather than all at once.

**Remove worktrees when their PR merges.** 20 stale worktrees held 38 GB of `target/` and were
what 8 `rust-analyzer` instances (5.14 GB, one alive 47 hours) were indexing.

**Watch `Shmem` and resident RSS, not `%commit`.** `overcommit_memory = 0` here, so
`CommitLimit` is advisory and `%commit > 100%` is normal and harmless — it read 106% before
anything went wrong and predicted nothing.

**Destructive commands must name one absolute path.** `rm -f -- *`, `rm -rf foo/*` and
`cd X && rm -rf *` are statically unresolvable, so the permission system flags them against
the worst case it can infer — which has read as the repo root — and the whole workflow blocks
until a human answers. Cache-store entries are chmod'd read-only, so a delete needs
`chmod -R u+w <abs path>` first or it silently leaves the tree behind.

## Deliberately deferred — do not mistake these for oversights

- **The infra-failure gate has no test.** It lives at `main.rs:232` inside `run_test`, is a
  *runtime* gate reached only by running a real sweep, and no test anywhere walks a results
  tree containing an opaque backend's log. It was silently blinded for 7 of 17 agents in an
  early 7a draft and **no CI gate went red** — two reviewers caught it by reading. A
  whole-path test (one log per `LogFormat`, asserting `describe_infra_failures`) is the
  highest-value unwritten test in the repo.
- **Six backends still cannot produce an infra failure**: c2rust, laertes, c2saferrust,
  smartc2rust, kimi, oneshot. None wraps its invocation in `timeout` or calls
  `record_agent_exit`; kimi/oneshot are single API calls with no child process. Kiro gained
  real sight in #104. Giving the other six sight means adding a timeout wrapper and an exit
  record to five invocation paths — its own PR.
- **Laertes and C2SaferRust must stay `Mode::Bypass`** in any caching PR. Their input is
  reached by path surgery into a sibling agent's results tree with no digest, so the key
  cannot name *which* c2rust output was consumed. A wrong key is worse than no cache.
  C2SaferRust's `BEDROCK_API_KEY` must never reach a digest or `meta.json`.
- **The comment budget is an absolute comment-line ceiling, `--max-comments 3100`, with
  `--max-ratio 20` as a loose backstop** — PR 13 replaced the whole-tree ratio as the primary
  metric, so this is no longer deferred; the ratio survives only to catch the class a count
  cannot see (code deleted, comments kept). It also fixed the masker that made the ratio green
  on a false measurement (`r(#*)"` matched any `r` before a quote: 95 of 149 matches were
  ordinary words, hiding 92 comment and 320 counted lines; the corrected detector still
  accepts exactly one, `"/r"` in `cache.rs`, at a measured cost of 0 lines). The tree measures
  2,413 comment lines at 14.42%; the duty on a PR is to stay under both ceilings or lower
  them, never raise them. `tools/test_comment_budget.py` pins six failures, including that
  each limit really does exit non-zero.
- **`provenance.rs`'s git plumbing did not move to `io/`.** Extracting any subset needs ≥3
  widenings. It is one cohesive concept — *which code produced this result, and refuse to
  measure if we cannot say* — and splitting it to satisfy a diagram is the sprawl this plan
  removes.

## The rule that has bitten three times

**An item whose private visibility CARRIES an invariant cannot move to a lower layer, because
the move is what breaks it.**

1. `TreeDigest` stays with `hash_tree` — a `pub(crate)` constructor turns "only the hasher can
   make one" into "any module can make one from a string".
2. The typestate family stays in one file — private fields are what make the transitions
   unforgeable, and `is_public()` in the shape rules counts `pub(super)` as public.
3. `digest_tree`, `visit` and `copy_carrying` stay in `artifact.rs` — widening them let a PR 4
   draft rewrite the module doc from "Three invariants are enforced by the compiler" to "Two",
   conceding that any module could hash an unscrubbed tree. An unscrubbed digest differs every
   run, so the cache would look enabled and never hit.

## Sweep and cache state

Last harvest-bench sweep: 2026-08-15, relaunched 13:40, finished 20:33. **4 of 7 verified**
(jansson, libpng, libsodium, pcre2). lz4, mujs and zstd died with
`terminal_reason=aborted_tools` — the three biggest projects, verify logs 4.1/13/23 MB. The
infra gate then correctly **refused to score** and exited 1, so no partial number reached
`results/`.

`results/.cache` holds **4 entries, 99 MB**, one per verified project, keyed
`phase=verified agent=claude`. It is untracked in the `results` submodule — the cache will be
pushed once a run completes end to end, and growth plus loss of read-only mode bits on clone
are both accepted.

Driver: `bash ./run_hb_all.sh` (mode 100755 now, but it is invoked with `bash` in the docs).
It unsets `RUSTUP_TOOLCHAIN`, preflights cmake ≥3.24 / python ≥3.10 / claude, and puts cmake
at `$HOME/.local/opt/cmake-3.28.6-linux-x86_64`.

## Scratch to clean up when convenient

Deletes the permission system refused; each is one absolute path:

```
/local/home/scheschb/scratch/pr9-verify
/local/home/scheschb/scratch/pr5-vis
/local/home/scheschb/scratch/pr6-arch-probe
/local/home/scheschb/scratch/pr7a-gates2
/local/home/scheschb/scratch/pr7a-cb
```

Also: `git stash@{0}` on the superproject holds `prompt-merge-62-63` WIP stashed 2026-08-15 so
the sweep could run from `main`. `prune-comments` is the one worktree branch with **no** merged
PR — leave it alone.
