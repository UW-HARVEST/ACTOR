export const meta = {
  name: 'pr-pipeline',
  description: 'Implement one specified PR, critique it adversarially, resolve findings, verify every gate',
  whenToUse: 'Invoke once per PR with args {pr, worktree, branch, spec}. Returns a verdict; the caller merges.',
  phases: [
    { title: 'Implement', detail: 'one agent, in the pre-made worktree' },
    { title: 'Critique', detail: 'four read-only lenses in parallel' },
    { title: 'Resolve', detail: 'fix blocking findings, then re-critique' },
    { title: 'Verify', detail: 'a different agent runs every gate from scratch' },
  ],
}

const PR = args.pr
const WT = args.worktree
const BRANCH = args.branch
const SPEC = args.spec
const REPO = args.repo || '/local/home/scheschb/research/ACTOR'
const MAX_ROUNDS = args.rounds || 3

// Every prompt passes PATHS, never file contents: inlining large context is what stalled
// two earlier workflow runs.
const CONTEXT = `
YOUR WORKTREE: ${WT}   (branch ${BRANCH}). Work ONLY here. Never touch ${REPO}.

READ THESE FIRST, with the Read tool:
  ${WT}/${SPEC}                    the spec for THIS PR - it is authoritative
  ${WT}/CLAUDE.md                  comment policy, design/testing/refusal principles
  ${WT}/docs/architecture-plan.md  where this PR sits in the sequence

TOOLCHAIN - mandatory before any cargo command:
  export PATH="$HOME/.cargo/bin:$PATH" && unset RUSTUP_TOOLCHAIN
  rustc --version   # must print 1.94.0
RUSTUP_TOOLCHAIN is exported in this shell and silently overrides rust-toolchain.toml.
The trybuild .stderr expectations are toolchain-sensitive; recording one under the wrong
compiler has already shipped a red main once.

THE GATES (all must pass; run from ${WT}/tools unless noted):
  cargo fmt --check
  cargo test  --locked --lib --bin harvest-tools
  cargo test  --locked --test architecture
  cargo test  --locked --test compile_fail
  cargo clippy --locked --all-targets
  cargo clippy --locked --lib --bins -- -D clippy::panic
  cargo doc   --locked --no-deps
  python3 tools/comment_budget.py --max 14     (from ${WT}; must match type-safety.yaml)
  python3 tools/check_paths.py                 (from ${WT})

COMMENT BUDGET TRAP: comment_budget.py reads git ls-files, so it does NOT see untracked
files. Run 'git add -A' before measuring, or a new file's comments are invisible locally
and fail in CI.

DO NOT WRITE TO /tmp. It is a 16 GB tmpfs with a 1,048,576 inode cap, and it has already
been exhausted once — by ~24,700 leaked test tempdirs plus ~10 GB of agent scratch (tree
copies and cargo target dirs). When it fills, EVERY process on the box fails to create a
file, including the tooling needed to clean up, and all work stops.

Use /local/home/scheschb/scratch/<something-you-own> for any scratch tree, proof copy or
build target you need outside your worktree, and delete it when you are done. If you copy
the repo to prove something, put the copy there and its CARGO_TARGET_DIR there too.
Cache-store entries are chmod'd read-only on purpose, so 'rm -rf' fails on them: run
'chmod -R u+w <dir>' first or your cleanup silently leaves the tree behind.

NEVER, under any circumstances:
  * widen, disable, skip or delete a gate to make your change pass
  * add #[allow], #[expect], #![allow] or #[ignore]
  * grow an ALLOWED list in tools/tests/architecture.rs
  * run a blanket TRYBUILD=overwrite. If a .stderr genuinely must be re-recorded, do it
    alone, on the pinned toolchain, then diff it and confirm ONLY line numbers moved and
    the pinned error code is intact.
If a gate fires, the code is wrong. Fix the code.
`

phase('Implement')

const impl_report = await agent(
  `${CONTEXT}

You are implementing PR ${PR}. Do exactly what ${WT}/${SPEC} specifies - no more, no less.
Scope creep is a review failure even when the extra change is an improvement; note it
instead and move on.

Work in ${WT}. Do NOT commit, do NOT push, do NOT open a PR - a later stage handles that.

When done, return plain text:
1. Every file you changed, with a one-line reason.
2. The VERBATIM output of each gate command (not a summary, not "all passed").
3. Anything in the spec you could not do, and precisely why.
4. Anything you noticed that is wrong but is out of scope.

If you cannot make a gate pass without violating the NEVER list, stop and say so. That is
a valid outcome and far better than a weakened gate.`,
  { label: `pr${PR}:implement`, phase: 'Implement' }
)

log(`implement returned ${impl_report ? impl_report.length : 0} chars`)

const FINDINGS = {
  type: 'object',
  additionalProperties: false,
  required: ['lens', 'blocking', 'nonblocking'],
  properties: {
    lens: { type: 'string' },
    blocking: {
      type: 'array',
      maxItems: 12,
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['what', 'where', 'why', 'fix'],
        properties: {
          what: { type: 'string' },
          where: { type: 'string' },
          why: { type: 'string' },
          fix: { type: 'string' },
        },
      },
    },
    nonblocking: { type: 'array', maxItems: 8, items: { type: 'string' } },
  },
}

const LENSES = [
  {
    key: 'spec',
    text: `LENS: **spec conformance.** Read the spec, then the actual diff
(git -C ${WT} diff origin/main). Did the implementer do exactly what was specified?
Blocking: anything specified but missing, anything done that was not specified, any
"What must NOT change" item that changed. Check the acceptance criteria one by one
against reality, not against the implementer's claims.`,
  },
  {
    key: 'gates',
    text: `LENS: **gate integrity - the most important lens.** An agent facing a red gate
will "fix" it by weakening the gate. Diff against origin/main and hunt for exactly that:
a rule deleted or loosened the spec did not authorise, an ALLOWED list that grew, a new
#[allow]/#[expect]/#[ignore], a weakened assertion, a .stderr re-recorded beyond line
numbers, a changed pinned error code, a raised threshold (comment budget, KNOWN counts).
Then RUN every gate yourself in ${WT} and compare with what the implementer reported. A
discrepancy between claimed and actual output is blocking.`,
  },
  {
    key: 'claudemd',
    text: `LENS: **CLAUDE.md compliance.** Read ${WT}/CLAUDE.md and judge the diff against
it. Blocking: comments that restate code or narrate steps; a new bool gating a safety
property; two bools on one function; a duplicated abstraction where one exists; a test
named after a function rather than a failure; a test that cannot fail (no non-vacuity
assertion); a second definition of something that already has one.`,
  },
  {
    key: 'adversarial',
    text: `LENS: **adversarial correctness.** Assume there IS a bug and find it. Look for:
a wrong or fabricated digest reaching a cache key; anything that could produce a false
cache HIT (the worst failure - it silently serves the wrong artifact); a destructive
filesystem operation ordered before the thing that can fail; a consumed-then-reused
value; an error swallowed into a success; an invariant claimed but unenforced. Give a
concrete failing scenario for each - inputs, then wrong output. A finding with no
scenario is nonblocking.`,
  },
]

let round = 0
let blocking = []
while (round < MAX_ROUNDS) {
  round += 1
  phase('Critique')
  const verdicts = (await parallel(LENSES.map(l => () =>
    agent(`${CONTEXT}

You are a READ-ONLY adversarial reviewer of PR ${PR}, round ${round}. Do not edit any
file. Read the real diff and the real code; do not trust the implementer's account.

THE IMPLEMENTER REPORTED:
${impl_report}

${l.text}

Mark a finding "blocking" only if it must be fixed before merge. Strict but not
inventive: no speculative findings, no style preferences. An empty blocking list is the
correct answer when the change is sound.`,
      { label: `pr${PR}:crit:${l.key}:r${round}`, phase: 'Critique', schema: FINDINGS })
  ))).filter(Boolean)

  blocking = verdicts.flatMap(v => (v.blocking || []).map(b => ({ ...b, lens: v.lens })))
  const nb = verdicts.flatMap(v => v.nonblocking || [])
  log(`round ${round}: ${blocking.length} blocking, ${nb.length} nonblocking`)

  if (!blocking.length) break
  if (round === MAX_ROUNDS) {
    log(`STILL ${blocking.length} blocking after ${MAX_ROUNDS} rounds - escalating`)
    break
  }

  phase('Resolve')
  await agent(`${CONTEXT}

You are resolving blocking review findings on PR ${PR} in ${WT}, round ${round}.

FINDINGS TO FIX:
${JSON.stringify(blocking, null, 1)}

Fix every one, in ${WT}. Re-run all gates afterwards. If you believe a finding is WRONG,
do not silently ignore it - say so with the evidence that refutes it.

Remember the NEVER list. If a finding can only be resolved by weakening a gate, the
finding is telling you the change itself is wrong; fix the change.

Return plain text: what you changed per finding, and the verbatim gate output.`,
    { label: `pr${PR}:resolve:r${round}`, phase: 'Resolve' })
}

phase('Verify')

const VERDICT = {
  type: 'object',
  additionalProperties: false,
  required: ['all_gates_green', 'gate_output', 'net_diff_stat', 'concerns'],
  properties: {
    all_gates_green: { type: 'boolean' },
    gate_output: { type: 'string' },
    net_diff_stat: { type: 'string' },
    concerns: { type: 'array', maxItems: 6, items: { type: 'string' } },
  },
}

const verified = await agent(
  `${CONTEXT}

You are the INDEPENDENT verifier for PR ${PR}. You did not write this code and must not
edit it. Your only job is to run every gate in ${WT} from a clean state and report exactly
what happened.

Run each gate command above and capture its real result line. Run 'git -C ${WT} add -A'
first and then the comment budget, since it reads git ls-files and would otherwise miss
new files.

Also report: git -C ${WT} diff --stat origin/main | tail -1

Set all_gates_green true ONLY if every gate passed. If anything failed, set it false and
put the failure in gate_output. Do not fix anything. Do not round up. List in concerns
anything that passed but looks wrong to you.`,
  { label: `pr${PR}:verify`, phase: 'Verify', schema: VERDICT }
)

return {
  pr: PR,
  worktree: WT,
  branch: BRANCH,
  rounds_used: round,
  unresolved_blocking: blocking,
  verified,
  ready_to_merge: Boolean(verified && verified.all_gates_green && !blocking.length),
}
