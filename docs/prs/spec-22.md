# PR 22 — §B.2: materialise into a tree that did not exist, and score there

**This is the half of `spec-21.md` that PR 21 did not land, and it is the half the operator asked
for.** PR 21's own scope banner says so plainly: it implemented §B.1 (resolve each phase through the
cache and thread it), §B.3's type work (`Sealed::adopt` deleted, post-processing onto `Publishing<P>`)
and §B.4 (failed runs and their transcripts recorded where `Store::load` cannot see them). It did
**not** implement §B.2, and therefore did not meet `spec-21.md` acceptance criteria 1–5.

**Read `docs/prs/spec-21.md` first.** Its Part A is the measured diagnosis — nineteen production sites
that read a previous run's files — and it is not repeated here. This spec is Part A sites 1–15, 18 and
19, and `spec-21.md` criteria 1–5.

## What PR 21 left behind, stated as the gap

After PR 21, a phase is resolved through the cache and threaded, so **verify can no longer be seeded
from a translation this run did not produce** — that is real and it kills the 014 class at its source.
But the *scoring* path is untouched. `oracle/` and `analyse/` still resolve a case's crate by asking
the filesystem what is there:

- `battery::crate_dir` (`battery.rs:37`) still picks `verified/` if it has a `Cargo.toml`;
- **`has_verified` (`runtests.rs:137`) still promotes a whole battery's headline to the verified phase
  if any one case has a stale `verified/Cargo.toml`** — the mechanism behind "infra failures, yet a
  perfect score";
- `load_summary` (`runtests.rs:651`) still prefers a stale `summary.json`;
- `stage_phase_for_runtests` still symlinks `translated_rust` into the phase dir;
- `discover_batteries` (`runtests.rs:101`) still defines the denominator from leftover directories.

So today a hand-edited `results/` still changes the score. **That is what this PR removes, and it
removes it by making the scored bytes younger than the run rather than by checking them.**

## The design

```
<repo>/.eval/<agent>/<battery>/<case>/translated_rust/   <- materialised from the resolved artifact
<repo>/.eval/<agent>/<battery>/<case>/c_src/
<repo>/.eval/<agent>/<battery>/<case>/test_vectors/      <- copied fresh from the corpus
<repo>/.eval/<agent>/<battery>/<case>/runner/            <- copied fresh from the corpus
```

`runtests` is invoked as `python3 -m runtests.rust --root <dir> --subset <dir>` with
`current_dir = corpus_dir` (`runtests.rs:404`), and **`--root` may be any directory**. Point it here.

- **Created empty at the start of every run**, removed at the end unless a flag keeps it for a
  post-mortem. `.eval/` is gitignored and lives on `/local` beside the repo — never `/tmp`, which is
  tmpfs on this box.
- **Every byte is materialised this run**: the crate from the artifact the pipeline resolved (a cache
  hit or a fresh run), the oracle inputs from the corpus. **Nothing is copied out of `results/`.**
- **`translated_rust/` is a real directory, never a symlink.** `runtests/discovery/rust.py:14` is
  `build_project_dir = (case_root / "translated_rust").resolve()` with
  `target_dir = build_project_dir / "target"`. `.resolve()` follows the symlink, which is exactly how
  **666 `target/` directories** got inside `results/` while the two tests asserting `target/` is
  absent after publish both still pass. A real directory in the eval tree puts the build output in the
  tree that gets deleted, with no exclusion list to maintain.
- **`_is_case_dir_rust` requires BOTH `translated_rust/` and `test_vectors/`.** A case missing either
  is silently not discovered. **Count what was materialised against what was scored and refuse on a
  mismatch** — a silently smaller denominator is the defect class of `spec-16.md` and #114, and it is
  how pcre2 left the harvest-bench denominator.

## What this deletes

| sites (`spec-21.md` Part A) | delete |
|---|---|
| 1, 2, 3 | `battery::crate_dir`; `has_verified` and the conditional second `score_phase`; `load_summary`'s `summary.json`-if-it-exists preference |
| 4, 5, 6, 7 | `result.json` read from wherever it sits — scoring, `--check`, the report and `enrich_test_corpus` all consume the run's resolved set |
| 8, 9 | logs read out of a phase dir — the run names its own log per phase |
| 13, 14, 15 | disk-based discovery: the denominator comes from the **corpus**, the experiment's input. `results/` describes the output and may not define what was tested |
| 14 (cont.) | `stage_phase_for_runtests`, `TestArtifactGuard`'s staging half, `unstage_phase` |
| 18 | nothing to exclude: build output lands in the deleted tree |
| 19 | `copy_test_artifacts`'s `!tv_dst.exists()` guard — copying into an empty tree makes the "don't re-copy" branch unreachable, so an older corpus revision's oracle inputs cannot be reused |

## Site 20, found while PR 21 was in flight: the infra gate audits the wrong tree

`agent_health::audit(results_dir, format)` recurses from `results_dir` and collects **every case dir
beneath it**, and it is called with `paths.results_dir` — the whole agent tree — not the battery being
scored. It then picks one log per case off disk, preferring `verified/logs/verify.log` over
`translated/logs/translation.log`.

**Measured just now.** With all 85 B01_synthetic cases freshly translated and verified, scoring
B01_synthetic refused:

```
Error: 27 of 209 agent runs did not complete for infrastructure reasons.
Refusing to score. An infrastructure failure is not a result.
```

All 27 are `api_error`, and **none of them is in B01_synthetic** — 16 are in B02_synthetic, 9 in
B01_organic, 2 in B02_organic. So a battery whose every case is fresh could not be scored because of
dead-run transcripts in batteries that were not being scored. `--allow-infra-failures` was needed to
get the number, on a battery where the flag launders nothing.

This is worse than site 9 and belongs here rather than in a follow-up, because the eval tree is the fix:
**audit the runs in the manifest, not the directories under a root.** The gate then grades exactly the
runs whose results are being scored — which is what it was written to do — and one battery's failures
stop blocking another's. Keep the refusal itself: it is correct and it caught a real thing on 2026-08-14.

Pin it with a test: two batteries under one root, one clean and one holding a dead-run log, and scoring
the clean one must succeed.

**`results/` becomes write-only.** It is a shipped submodule the paper reads, so it keeps its layout
and keeps receiving artifacts, logs, metrics and scores — written from the resolved artifacts, never
read back as input during a run.

That sentence wants a rule, not a convention: **no module outside the pipeline may name a phase
directory.** The DAG rules already lex module references, so this is the same machinery pointed at
`phase_dir`/`crate_dir`/`has_crate` call sites. Without it this PR is a snapshot of nineteen sites
rather than a guarantee about the twentieth, and a twentieth is what the last three months produced.

## Archival scoring stays, and is named honestly

`harvest-tools test <battery> --check` over the shipped `results/CRUST` (580 MB) and
`results/CRUST-blind` (873 MB) has **no cache entries behind it** — `git ls-files .cache` in the
`results` submodule returns 0; the store is untracked and does not ship. That mode must keep working:
it reproduced every stored battery and it is what refuted `spec-20.md`.

So it stays, and it says what it is: it scores an archive and **carries no freshness guarantee**,
printed on every invocation. It is not the same operation as scoring a pipeline run and must not share
a name that implies it is. Materialise the archive into an eval tree too, so even that path scores a
tree assembled this run — it just cannot claim the bytes are this run's *work*.

## Constraints

- **Do not modify `test-corpus`.** MIT's `runtests` is a read-only graded oracle. This PR points
  `--root` at a different directory; it does not touch the scorer, and it may not "fix" a discovery
  rule by editing `rust.py`.
- **Do not delete anything under `results/`.** `results/CRUST` and `results/CRUST-blind` are not to be
  touched and the 990 `Cargo.lock` files under `results/` are not to be deleted. The eval tree is
  where deletion happens, and destructive commands name one absolute path as the whole command.
- **No key moves and `SCHEMA` stays 4** — this PR touches no key component. Prove it by probe on base
  and branch and quote the output.
- **`.eval/` must be gitignored** in the ACTOR repo before the first run writes to it, or the next
  `git status` buries every real change under thousands of untracked files.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion, no visibility
  widened to make a move work.
- The comment budget has **5 lines of headroom** (measured after PR 21: 3095 of 3100). Prune your own
  sprawl; **do not raise the ceiling** — `type-safety.yaml` says never to raise it again without the
  same explicit reasoning, and that is not this PR's call.
- Answer, for every check the diff touches: **after my change, what input still makes this check
  fail?** Name it.

## Acceptance criteria

`spec-21.md`'s criteria 1–5, which PR 21 declared unmet, plus the ten gates:

1. **Two cache hits per case, printed.** A fully cached run over B01_synthetic reports translate 85/85
   and verify 85/85 hits, zero agent invocations, with a wall-clock number.
2. **The evaluation tree did not exist before the run.** Plant a file in it, run, show the file gone
   and the score unchanged.
3. **Site 2 shown red first, because it is the operator's actual complaint.** A battery where nothing
   verified but one case holds a stale `verified/Cargo.toml` must not report a verified headline. Show
   it reporting one on the base commit and the translate headline plus one absent case after.
4. **014 shown unreachable.** A case whose `translated/` holds no crate and whose `verified/` holds a
   complete one is reported **absent** from the score, not passing. Show the test red on base.
5. **A hand-edited `results/` changes no score.** Edit a byte in a published crate, re-run, show the
   score identical — the direct demonstration that the guarantee is structural rather than checked.
6. **Materialised count equals scored count**, with a refusal on mismatch, shown red by removing one
   case's `test_vectors/`.
7. **The rule that stops a twentieth site** shown red by adding a `phase_dir` call in a module that
   may not name one.
8. **The 40 golden digests unchanged**, fingerprint passing and not skipping, and both keys plus
   `SCHEMA` unchanged with the probe output quoted.
9. **Every site in `spec-21.md` Part A accounted for individually in the PR description** — closed
   here, closed by PR 21, or deliberately left with the reason.

## Commit message

That PR 21 resolved the phases through the cache and this PR makes the scored bytes younger than the
run: materialised into `.eval/` created empty that run, with `runtests --root` pointed there, so no old
file is read because no old file is present. That `crate_dir`, `has_verified`, `load_summary`'s
preference, the `translated_rust` symlink staging and the disk-based denominator are deleted rather
than corrected, with the site list from `spec-21.md` Part A. That `.resolve()` in
`runtests/discovery/rust.py:14` is why the scoring build wrote 666 `target/` dirs into `results/` and
why a real directory fixes it with no exclusion list. That a case missing `test_vectors/` was silently
undiscovered and is now a refusal, with the materialised-vs-scored counts. That `results/` is
write-only during a run, enforced by a rule rather than a convention. The measured two-hits-per-case
run over B01_synthetic with its wall clock. And that no key moved and `SCHEMA` is still 4, with the
probe output.
