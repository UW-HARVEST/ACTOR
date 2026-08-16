# Handoff: state of the refactor, and how to keep running it

Written to survive a context compaction. `CLAUDE.md` holds the *principles*;
`docs/architecture-plan.md` holds the *target and PR sequence*; the per-PR briefs are in
`docs/prs/`. This file holds the **operational state and the traps** — the things that are
not in any of those and that a fresh context would otherwise rediscover expensively.

## Where the work stands

Sixteen PRs merged (#89–#104). The layered architecture is most of the way in:

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
| 7b | translate on `run_cached` — **the translate cache** | `docs/prs/spec-7b.md` | in flight |
| T | stop the suite writing to `/tmp`, and stop it leaking | `docs/prs/spec-tmp.md` | **written, never landed — see below** |
| 7c | shared-source groups: one key, N publishes | `docs/prs/spec-7c.md` | ready |
| 8 | `cache/` + `dataset/` split; `cache_mode` off `Paths` | `docs/prs/spec-8.md` | ready; breaks `battery ↔ cache`, `cache ↔ cli` |
| 10 | renames: `Scrubbed`→`ScrubbedTree`, `Sealed`→`SealedTree`, `CDir`→`OracleDir` | `docs/prs/spec-10.md` | ready; last, touches 10 column-exact `.stderr` files |

**PR T is the unlanded root cause of the outage below, and it should go early rather than
last.** Verified on `main` at 27581dc: `tools/.cargo/config.toml` does not exist, and
**76 `tempdir().unwrap()` call sites across 16 files** still resolve through
`tempfile`'s default, i.e. `env::temp_dir()`, i.e. the `/tmp` tmpfs — the heaviest being
`artifact.rs` (19), `battery.rs` (17) and `analyse/cargo_toml.rs` (7). Production code
refuses a tmpfs base *deliberately* (`io/workdir.rs` opens by explaining why, and
`Tmpfs::Refuse` is the default), so the suite is the one part of the crate that ignores the
rule the crate wrote down. Combined with cache entries being chmod'd read-only —
`TempDir::drop` does a plain recursive delete, cannot report an error, and so fails
**silently** — every cache test leaks its tempdir permanently.

`/tmp` currently holds 99 MB and zero `.tmp*` dirs, but only because the reboot cleared it:
the machine had accumulated 24,707 leaked dirs and hit the **inode** cap with ~2 GB of bytes
still free. Nothing has changed that will stop it happening again. Every PR in this sequence
runs the suite many times.

`docs/prs/spec-7.md` is superseded by 7a/7b/7c — it bundled nine changes and stalled an agent
after 198 tool calls with nothing produced. **Keep PRs to one concern.**

## How to run one PR (the ritual that works)

```bash
# 1. spec it, commit to main
git add docs/prs/spec-N.md && git commit && git push origin main

# 2. worktree off main
git worktree add -b prN-slug /local/home/scheschb/pr-auto-N main

# 3. run the pipeline
Workflow({scriptPath: ".claude/workflows/pr-pipeline.js",
          args: {pr:"N", worktree:"/local/home/scheschb/pr-auto-N",
                 branch:"prN-slug", spec:"docs/prs/spec-N.md",
                 repo:"/local/home/scheschb/research/ACTOR", rounds:2}})

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
exists prove nothing, and the comment budget is a whole-tree ratio so two PRs can each pass
and jointly fail.

**Write the commit message from `git diff`, never from the agent's report.** Two reports in
this sequence described trees that were never written: one claimed four items had moved and
been widened when they had not moved at all; one stated in prose that a change was
deliberately NOT made while the diff made it, omitting two changed files. The pipeline now
instructs against this, but verify anyway.

## The nine gates

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
python3 tools/comment_budget.py --max 14     # from repo root, AFTER `git add -A`
python3 tools/check_paths.py                 # from repo root
```

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
- **The comment budget is a whole-tree ratio at `--max 14`**, currently ~13.99%. A ratio can
  be tripped by a *deletion* (removing comment-sparse code raises it), which is wrong for a
  refactor made of deletions. Replacing it with an absolute ceiling is unlanded work; a PR 0
  reviewer recommended it and it was deferred.
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
