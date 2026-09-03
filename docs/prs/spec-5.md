# PR 5 — Create `agents/`, and break three dependency cycles

## Goal

Extract the machinery both phases share into `agents/`. This is the highest-value cut in
`docs/architecture-plan.md`: three of the module cycles exist *only* because the shared
agent-invocation machinery lives inside `translate.rs`.

## The three cycles and why they exist

Measured on `main`:

```
verify.rs  -> translate.rs   9 distinct items
translate.rs -> verify.rs    1 item  (has_verify_phase)

session.rs -> translate.rs   AGENT_ENV, CLAUDE_PLAIN_AGENT_JSON
translate.rs -> session.rs

opencode.rs -> translate.rs  record_agent_exit
translate.rs -> opencode.rs
```

`verify.rs` reaches into `translate.rs` for **its own** metrics writer
(`write_verification_metrics`) and **its own** work-tree type (`IsolatedWorkDir`). None of
this is translation-specific; it is what any agent phase needs. `translate.rs` is 2,973
lines because it is three concepts fused, and this is the one that peels off cleanly.

## What moves into `agents/`

Create `tools/src/agents/`. The module doc states what the subsystem is: *run an external
agent, classify what came back, produce a typed result.*

Determine the minimal item set yourself — the goal is defined by the cycles disappearing,
not by this list — but these are the items known to be involved:

**From `verify.rs`** (currently private, which is why translate cannot use them):
`Backend`, `Invocation`, `verify_invocation`, `Backend::policy_shape`. These four *are* the
"resolve model + CLI + command + policy before the agent starts, so the key can name them"
abstraction, and it is the right shape — it is just private to one phase and enumerates only
3 of the 17 backends.

**From `translate.rs`:**
- the agent-exit thread-local family: `AgentExit`, `record_agent_exit`, `clear_agent_exit`,
  `take_agent_exit`, `observed_exit`, `merge_agent_exit`
- `AGENT_ENV`, `CLAUDE_PLAIN_AGENT_JSON`
- `claude_model`, `assert_pins_honoured`, `agent_provenance`
- `IsolatedWorkDir`
- `Semaphore`

**Whole files:** `session.rs` → `agents/session.rs`, `opencode.rs` → `agents/opencode.rs`.

**`has_verify_phase`** currently lives in `verify.rs` and is called by `translate.rs`. It
answers "does this agent have a verify phase at all" — an agent capability, not a verify
detail. Moving it to `agents/` is what removes the last `translate -> verify` edge. Check
that it and `verify_invocation` do not disagree: a test already asserts they must not.

## What must NOT happen

- **No visibility may widen to make a move work.** This has now bitten twice. If an item
  needs `pub(crate)` to move, it stays and you report it. In particular the agent-exit
  thread-local's privacy matters: `merge_agent_exit` CONSUMES it and must be called exactly
  once per invocation, while `observed_exit` peeks. If moving them apart would widen
  `take_agent_exit`, keep the family together.
- **Do not generalise `IsolatedWorkDir` over the phase.** That is PR 6. Move it as-is, still
  `WorkTree<Verify>`-shaped. This PR is a move, not a redesign.
- **Do not unify the two metrics writers.** Also PR 6.
- Do not add `#[allow]`/`#[expect]`/`#[ignore]`, weaken any rule, grow any ALLOWED list,
  change what any test asserts, or re-record any `.stderr`.
- Do not write to `/tmp`. Use `/local/home/scheschb/scratch/<yours>` and delete it after.

## The ratchet is the acceptance test

`a_module_cycle_may_only_shrink` records a 10-module baseline. If this PR works, the rule
FAILS with "X is in no cycle any more" naming the modules that left, and prints the current
membership. That failure is the deliverable.

Shrink `CYCLE_BASELINE` to exactly what the rule prints, and quote in the commit message:
the old membership, the new membership, and which of the three target cycles is gone. If a
cycle you expected to break is still there, say which edge is holding it — that is more
useful than a partial claim.

Note that the baseline names top-level modules, so `session` and `opencode` becoming
`agents/session.rs` and `agents/opencode.rs` makes them part of the `agents` node. Say
whether `agents` itself ends up in a cycle, and if so via which edge.

## Acceptance criteria

Pinned toolchain, `RUSTUP_TOOLCHAIN` unset:

```
cargo fmt --check                                        clean
cargo test  --locked --lib --bin harvest-tools           all pass, count unchanged
cargo test  --locked --test architecture                 all pass
cargo test  --locked --test compile_fail                 10 cases
cargo clippy --locked --all-targets                      0 warnings
cargo clippy --locked --lib --bins -- -D clippy::panic   0
cargo doc   --locked --no-deps                           clean
python3 tools/comment_budget.py --max 14                 exit 0  (after `git add -A`)
python3 tools/check_paths.py                             exit 0
HARVEST_GOLDEN_RESULTS=<repo>/results cargo test --locked --manifest-path tools/Cargo.toml --test integration artifact_fingerprint
```

The fingerprint must pass and must not skip.

`MIN_FILES` must match the measured count minus 2.

## Commit message

Which cycles broke and which edge each one hung on; the old and new `CYCLE_BASELINE`
membership; how many lines left `translate.rs`; anything that stayed because moving it would
have widened visibility; and that 40 golden digests are unchanged.
