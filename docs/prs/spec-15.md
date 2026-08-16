# PR 15 — Five seams the 7b review exposed

Five findings from 7b's review that were correctly ruled out of its scope. Each is small; they
are grouped because they all live on the newly unified execution path and would otherwise be
five one-file PRs against the same three files.

Take them in this order. **(1) is a money bug and is worth landing even if nothing else here
is.**

## 1. A store failure must not destroy a paid artifact

`Store::obtain` (`cache.rs:562-568`):

```rust
let Some(produced) = compute()? else { return Ok(None) };
if self.mode != Mode::Bypass {
    self.store(inputs, &key, &dir, &produced)
        .with_context(|| format!("storing cache entry {}", key.as_str()))?;   // <-- here
}
```

By this line the agent has run, the money is spent and `produced.sealed` exists. If `store`
fails, `?` returns `Err`, `run_cached` never reaches `obtained.sealed.publish(case_dir)`, and
**the translation is lost**. ENOSPC is the realistic trigger on a multi-GB store; a leftover
read-only staging directory is another (`set_read_only` makes entries `0o555`, and a delete
inside one fails EACCES).

Storing is an optimisation. Publishing is the deliverable. So a failed store must be **loud and
non-fatal**: report it, and return `Obtained { replayed: false, .. }` so the artifact is
published anyway. The next run recomputes rather than replays, which is exactly the cost of a
cache miss.

Pre-existing — verify has carried it since the cache landed — but 7b makes it expensive:
translate is a measured **$795.59 per harvest-bench sweep**.

**Test: `a_store_that_cannot_write_still_publishes_the_artifact_it_was_given`.** Make the store
root unwritable, run a phase, assert the artifact is published, the failure is reported, and no
entry exists. Non-vacuity: assert the run *would* have stored one had the root been writable.

## 2. The two publish paths must carry the same thing

The keyed path publishes through `Sealed::publish` → `Carry::FromArtifact`, which drops
`target/`, `.claude/` and root-level `*.log`/`*.bak`/`*.sha256`. The bypassed path still uses
the hand-rolled recursive copy, which carries them.

So after 7b, **what `translated/` contains depends on which backend produced it** — claude and
kiro get one tree shape, opencode/codex/c2rust/laertes/c2saferrust another. Two spellings of
"publish this phase" is the duplication this whole sequence removes, and here it makes the
results tree inconsistent per agent, which is a measurement hazard rather than a tidiness one.

Route the bypassed path through the same publish. If it genuinely cannot be (no `Sealed`
without a `Completed`), say precisely why and record the difference in one place instead of
leaving it implicit in two copy helpers.

**Test: `every_backend_publishes_the_same_tree_shape`.** One table-driven test over both paths
asserting the same admitted/excluded set. Named after the failure, per `CLAUDE.md`.

## 3. Refuse before the money, not per case

`cache::ToolchainId::detect()` now runs **once per keyed case**, inside the loop, and refuses
when `RUSTUP_TOOLCHAIN` disagrees with the pin. `preflight_check` does not cover it.

That is precisely the failure `CLAUDE.md` records: *"A 3h20m sweep completed and then had all
seven verifications refused for a variable that was already set at launch."* A sweep can now
translate case 1, spend real money, and refuse case 2 for a condition that was already true
before it started.

Hoist it: probe the toolchain in `preflight_check`, once, and refuse there. Per-case detection
may stay as a cheap consistency assertion, but the *refusal* belongs before the money.

**Test: `a_toolchain_that_will_refuse_every_case_refuses_before_the_first_one`.**

## 4. Delete what cannot happen

Three items 7b left that are unreachable or duplicated, each confirmed by a reviewer reading
the code:

- **`Outcome::Nothing` is unreachable for translate.** Its `compute` closure returns
  `Ok(Some(..))` or `Err`, never `Ok(None)`, so the
  `anyhow::ensure!(matches!(outcome, Outcome::Published(_)), ..)` guard describes a state that
  cannot occur. Either make it representable-and-handled or delete the arm; do not leave a
  guard whose message is fiction.
- **`Backend::OpenCode(_) => anyhow::bail!(..)` inside `Launch::Keyed`** cannot be reached:
  `resolve_launch` is the only constructor of `Launch::Keyed` and never builds it for
  opencode. Dead defensive code that reads as a live policy.
- **`Launch` is a fourth spelling of the backend set** — `Agent` → `InTool` → `Launch` →
  `Backend`. `CLAUDE.md`: *"One definition per concept. A second copy drifts — two agent-name
  tables put a wrong name in 208 result files."* Four is worse than two. Collapse at least one
  level, or write down what distinct question each of the four answers.

## 5. `Corpus::adopt` should refuse a corpus that hashes to the sentinel

`CDir::digest` returns `TreeDigest("sha256:absent")` for a missing directory — the one
`TreeDigest` not derived from bytes. `Corpus::adopt`'s `is_dir()` check is what makes it
unreachable, and that is the stated reason the check exists. But a corpus that *is* a directory
and hashes to a constant for any other reason is not covered, and for translate `input_tree` is
the **only** per-case key component, so a constant digest collides every case in a battery onto
one key.

Assert it where it cannot be bypassed: `Corpus::adopt` refuses a digest equal to the absent
sentinel, and refuses an empty corpus. One line each, and it closes the last route to a false
hit that is not a hash collision.

**Test: `a_corpus_whose_digest_is_the_absent_sentinel_cannot_be_adopted`.**

## Constraints

- No visibility widening; report instead.
- No `#[allow]`/`#[expect]`/`#[ignore]`, no ALLOWED growth, no weakened assertion.
- **No key change and no `SCHEMA` bump.** Measure both keys for fixed inputs, both sides.
- The 40 golden digests unchanged — item 2 changes what the *bypassed* path publishes, so say
  explicitly whether any stored `translated/` shape moves and which agents' results are
  affected.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`.
- Answer, for every check your diff touches: **after my change, what input still makes this
  check fail?** Name it.

## Acceptance criteria

The ten gates (see `docs/HANDOFF.md`), the golden fingerprint passing and not skipping, plus
the five tests above with evidence each can fail, plus both cache keys unchanged.

If any item turns out to need more than a small change, land the others and say which you
dropped and why. Item 1 is not droppable.

## Commit message

Per item: what was wrong, what it would have cost, and the test that now fails without the fix.
For item 2, whether any published tree shape changed and for which agents. For item 4, what
each surviving spelling of the backend set answers that the others do not.
