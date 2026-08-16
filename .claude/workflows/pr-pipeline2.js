export const meta = {
  name: 'pr-pipeline2',
  description: 'Implement one specified PR, critique it, refute weak findings, resolve, verify every gate, audit for omissions',
  whenToUse: 'Invoke once per PR with args {pr, worktree, branch, spec, base}. Returns a verdict; the caller merges.',
  phases: [
    { title: 'Implement', detail: 'one agent, in the pre-made worktree' },
    { title: 'Critique', detail: 'four lenses in parallel; only the gate lens builds' },
    { title: 'Screen', detail: 'one skeptic per blocking finding, in parallel' },
    { title: 'Resolve', detail: 'fix the findings that survived screening' },
    { title: 'Verify', detail: 'a different agent runs every gate from scratch' },
    { title: 'Audit', detail: 'what did every earlier stage miss?' },
  ],
}

const PR = args.pr
const WT = args.worktree
const BRANCH = args.branch
const SPEC = args.spec
const REPO = args.repo || '/local/home/scheschb/research/ACTOR'
const MAX_ROUNDS = args.rounds || 2
// The commit this branch was cut from. Passed explicitly because origin/main can move
// while a run is in flight, and then 'git diff origin/main' shows other people's commits
// as this branch's deletions. One earlier run lost time to exactly that.
const BASE = args.base || 'origin/main'

const CONTEXT = `
YOUR WORKTREE: ${WT}   (branch ${BRANCH}). Work ONLY here. Never touch ${REPO}.

YOUR DIFF IS AGAINST ${BASE}, always. Use 'git -C ${WT} diff ${BASE}' and
'git -C ${WT} diff --numstat ${BASE}'. Do NOT diff against origin/main: it moves while you
work, and unrelated commits then read as your deletions.

READ THESE FIRST, with the Read tool:
  ${WT}/${SPEC}                    the spec for THIS PR - it is authoritative
  ${WT}/CLAUDE.md                  comment policy, design/testing/refusal principles
  ${WT}/docs/HANDOFF.md            gates, machine constraints, traps that already bit
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
  cargo build --release --locked            (CI runs this FIRST; debug passing proves little)
  python3 tools/test_comment_budget.py                 (from ${WT}; same CI step)
  python3 tools/comment_budget.py --max-comments 2560 --max-ratio 20
                                            (from ${WT}; must match type-safety.yaml)
  python3 tools/check_paths.py              (from ${WT})

And if your diff touches the artifact pipeline, the golden fingerprint, which needs the env
var because a worktree has no submodules and it otherwise prints NO SIGNAL and proves nothing:
  HARVEST_GOLDEN_RESULTS=${REPO}/results cargo test --locked --test integration artifact_fingerprint

'cargo test --locked --test integration' as a whole CANNOT pass in a worktree: six of its ten
tests need the test-corpus submodule a worktree does not inherit. That is pre-existing and
identical on the base tree. Run the fingerprint by name, not the whole file.

COMMENT BUDGET TRAP: comment_budget.py reads git ls-files, so it does NOT see untracked
files. Run 'git add -A' before measuring, or a new file's comments are invisible locally
and fail in CI. It measures 2468 comment lines against the 2560 ceiling (92 lines of
headroom) and 14.42% against the 20% ratio backstop. Either may be lowered, never raised.

DO NOT WRITE TO /tmp. It is a 16 GB tmpfs whose inode table has been exhausted once, taking
the whole machine down: every process then fails to create a file, including the tooling
needed to clean up. Use /local/home/scheschb/scratch/<something-you-own> for any scratch
tree, proof copy or build target, and put CARGO_TARGET_DIR there too.

CLEANUP RULES - a denied command BLOCKS this whole workflow until a human answers, so a
sloppy cleanup command costs more than the disk it frees:
  * NEVER use a wildcard or glob in a destructive command. 'rm -f -- *', 'rm -rf foo/*'
    and 'rm -rf /tmp/prefix-*' are all statically unresolvable, so the permission system
    flags them against the worst-case target it can imagine - which has read as the repo
    itself.
  * NEVER rely on 'cd' to make a relative destructive command safe.
  * NEVER put a destructive command in the same shell line as anything else.
  * Delete ONE absolute path, spelled in full, as the whole command:
        rm -rf /local/home/scheschb/scratch/pr5-review
  * Cache-store entries are chmod'd read-only on purpose, so a delete fails on them. Run
    'chmod -R u+w /full/absolute/path' first, or your cleanup silently leaves the tree.
  * If a cleanup is denied anyway, do NOT retry variants and do NOT stall: say in your
    report which absolute path you left behind and continue.

NEVER, under any circumstances:
  * widen, disable, skip or delete a gate to make your change pass
  * add #[allow], #[expect], #![allow] or #[ignore]
  * grow an ALLOWED list in tools/tests/architecture.rs
  * run a blanket TRYBUILD=overwrite. If a .stderr genuinely must be re-recorded, do it
    alone, on the pinned toolchain, then diff it and confirm ONLY line numbers moved and
    the pinned error code is intact.
If a gate fires, the code is wrong. Fix the code.

WHEN YOUR CHANGE MAKES A CHECK START FAILING there are exactly three moves: the check is
right and your code is wrong (fix the code); the check lacks the information to judge
correctly (GIVE IT that information); or it is genuinely miscalibrated (escalate - say so and
stop, it is not your call). Making it unable to fire is never one of the three.

Beware the disguise: "classify that input as no-evidence / unknown / not-applicable" IS
making it unable to fire, and it feels like a correctness fix. A gate was recently made blind
to 7 of 17 agents this way - the classification was semantically true for the input, and the
gate went silent two files away. So before you finish, answer this out loud for every check
your diff touches:

    After my change, what input still makes this check FAIL?

Name that input. If the answer is "none, for this class of input", you turned the check off,
however principled the reasoning felt - and you must escalate instead.

PRIVACY CARRIES INVARIANTS. An item whose private visibility enforces something cannot be
moved to a lower layer or widened to make a move work, because the move is what breaks it.
This has bitten three times here: TreeDigest with hash_tree, the typestate family, and
digest_tree/visit/copy_carrying in artifact.rs. If a move needs a widening, the item is in
the wrong layer - leave it and report it. Never widen to pass.

WRITE YOUR REPORT FROM THE DIFF, AS THE LAST THING YOU DO. Two reports in this sequence
described a tree that was never written, because they were composed from the plan and from
intermediate reasoning rather than from the final state. One claimed four items had moved and
been widened when they had not moved at all; one stated in prose that a change was
deliberately NOT made while the diff made it, and omitted two changed files from its table.
Both would have put a false structural record in permanent history.

So: run 'git diff --numstat ${BASE}' and 'git diff ${BASE}' at the END, list EVERY file, and
derive each claim from what you see there. If you reasoned your way to a decision mid-task and
then did something else, the diff is what is true. Every number you state must come from a
command you ran, and measured numbers beat estimates every time.
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
2. The VERBATIM result line of each gate command (not a summary, not "all passed").
3. Anything in the spec you could not do, and precisely why.
4. Any visibility that widened, and whether the move forced it.
5. Anything you noticed that is wrong but is out of scope.

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
      maxItems: 10,
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

const SCREEN = {
  type: 'object',
  additionalProperties: false,
  required: ['refuted', 'reason'],
  properties: {
    refuted: { type: 'boolean' },
    reason: { type: 'string' },
  },
}

// Only the gate lens builds. The other three reason over the diff and the code, and four
// concurrent cargo builds in one target dir serialise on the lock for no extra coverage.
const LENSES = [
  {
    key: 'spec',
    text: `LENS: **spec conformance.** Read the spec, then the actual diff
(git -C ${WT} diff ${BASE}). Did the implementer do exactly what was specified?
Blocking: anything specified but missing, anything done that was not specified, any
"What must NOT change" item that changed. Walk the acceptance criteria one by one against
reality, not against the implementer's claims. Do NOT run the gates; another lens does.`,
  },
  {
    key: 'gates',
    text: `LENS: **gate integrity - the most important lens, and the only one that builds.**
An agent facing a red gate will "fix" it by weakening the gate. Diff against ${BASE} and hunt
for exactly that: a rule deleted or loosened the spec did not authorise, an ALLOWED list that
grew, a new #[allow]/#[expect]/#[ignore], a weakened assertion, a .stderr re-recorded beyond
line numbers, a changed pinned error code, a raised threshold (comment budget, KNOWN counts),
a visibility widened to make a move compile.
Then RUN every gate yourself in ${WT}, including the release build, and compare with what the
implementer reported. A discrepancy between claimed and actual output is blocking.`,
  },
  {
    key: 'claudemd',
    text: `LENS: **CLAUDE.md compliance.** Read ${WT}/CLAUDE.md and judge the diff against
it. Blocking: comments that restate code or narrate steps; a new bool gating a safety
property; two bools on one function; a duplicated abstraction where one exists; a test
named after a function rather than a failure; a test that cannot fail (no non-vacuity
assertion); a second definition of something that already has one; an estimate stated where
a measurement was possible. Do NOT run the gates; another lens does.`,
  },
  {
    key: 'adversarial',
    text: `LENS: **adversarial correctness.** Assume there IS a bug and find it. Look for:
a wrong or fabricated digest reaching a cache key; anything that could produce a false
cache HIT (the worst failure - it silently serves the wrong artifact); a destructive
filesystem operation ordered before the thing that can fail; a consumed-then-reused
value; an error swallowed into a success; an invariant claimed but unenforced; a replay
that differs from a fresh run. Give a concrete failing scenario for each - inputs, then
wrong output. A finding with no scenario is nonblocking. Do NOT run the gates.`,
  },
]

let round = 0
let surviving = []
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
correct answer when the change is sound. A finding you cannot ground in a line of the diff
is nonblocking.`,
      { label: `pr${PR}:crit:${l.key}:r${round}`, phase: 'Critique', schema: FINDINGS })
  ))).filter(Boolean)

  const claimed = verdicts.flatMap(v => (v.blocking || []).map(b => ({ ...b, lens: v.lens })))
  const nb = verdicts.flatMap(v => v.nonblocking || [])
  log(`round ${round}: ${claimed.length} claimed blocking, ${nb.length} nonblocking`)

  if (!claimed.length) { surviving = []; break }

  // Screen every blocking finding before a resolver acts on it. A wrong blocking finding is
  // worse than a missed one: the resolver changes correct code to satisfy it, and a false
  // premise has already reached a spec in this sequence once.
  phase('Screen')
  const screened = await parallel(claimed.map(f => () =>
    agent(`${CONTEXT}

You are screening ONE review finding on PR ${PR} for correctness. Read-only.

THE FINDING:
${JSON.stringify(f, null, 1)}

Try to REFUTE it. Read the actual code and the actual diff at the place it names. Is the
claim true of the tree as it stands? Is the consequence real, or does something else already
prevent it? Would the proposed fix be a change for the worse?

Set refuted=true if the finding is factually wrong, already handled elsewhere, or would make
the code worse. Set refuted=false only if you confirmed it against the code. Quote the lines
you checked in your reason. Default to refuted=true if you cannot confirm it - an unconfirmed
finding must not drive an edit.`,
      { label: `pr${PR}:screen:r${round}`, phase: 'Screen', schema: SCREEN })
      .then(v => ({ finding: f, screen: v }))
  ))

  surviving = screened.filter(Boolean).filter(s => s.screen && s.screen.refuted === false)
  log(`round ${round}: ${surviving.length} of ${claimed.length} survived screening`)

  if (!surviving.length) break
  if (round === MAX_ROUNDS) {
    log(`STILL ${surviving.length} confirmed blocking after ${MAX_ROUNDS} rounds - escalating`)
    break
  }

  phase('Resolve')
  await agent(`${CONTEXT}

You are resolving CONFIRMED blocking review findings on PR ${PR} in ${WT}, round ${round}.
Each has already been independently screened against the code, so treat them as real.

FINDINGS TO FIX (each with the screener's confirmation):
${JSON.stringify(surviving, null, 1)}

Fix every one, in ${WT}. Re-run all gates afterwards. If you still believe a finding is
WRONG, do not silently ignore it - say so with the evidence that refutes it.

Remember the NEVER list. If a finding can only be resolved by weakening a gate, the
finding is telling you the change itself is wrong; fix the change.

Return plain text: what you changed per finding, and the verbatim gate result lines.`,
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

Run each gate command above, including the release build, and capture its real result line.
Run 'git -C ${WT} add -A' first and then the comment budget, since it reads git ls-files and
would otherwise miss new files. If the diff touches the artifact pipeline, run the golden
fingerprint WITH its env var and confirm it did not print NO SIGNAL.

Also report: git -C ${WT} diff --stat ${BASE} | tail -1

Set all_gates_green true ONLY if every gate passed. If anything failed, set it false and
put the failure in gate_output. Do not fix anything. Do not round up. List in concerns
anything that passed but looks wrong to you.`,
  { label: `pr${PR}:verify`, phase: 'Verify', schema: VERDICT }
)

phase('Audit')

const AUDIT = {
  type: 'object',
  additionalProperties: false,
  required: ['omissions', 'report_matches_diff', 'notes'],
  properties: {
    omissions: { type: 'array', maxItems: 8, items: { type: 'string' } },
    report_matches_diff: { type: 'boolean' },
    notes: { type: 'string' },
  },
}

const audit = await agent(
  `${CONTEXT}

You are the completeness auditor for PR ${PR}. Read-only. Everyone before you looked for
defects; your job is to find what they did NOT look at.

Read ${WT}/${SPEC} and the full diff (git -C ${WT} diff ${BASE}) yourself, fresh.

THE IMPLEMENTER'S FINAL ACCOUNT:
${impl_report}

Answer three things:
1. What is in the spec that no earlier stage appears to have checked - an acceptance
   criterion never measured, a required test absent or vacuous, a "must NOT change" item
   nobody verified?
2. Does the implementer's account match the diff? Name every file in the diff that the
   account omits, and every claim in the account the diff contradicts. This has gone wrong
   twice in this sequence and it puts a false record in permanent history.
3. Anything a reviewer will need in the commit message that is not yet written down.

Set report_matches_diff false if the account and the diff disagree in any way that matters.
Be specific and cite files and lines. An empty omissions list is the correct answer if the
work is genuinely complete.`,
  { label: `pr${PR}:audit`, phase: 'Audit', schema: AUDIT }
)

return {
  pr: PR,
  worktree: WT,
  branch: BRANCH,
  base: BASE,
  rounds_used: round,
  unresolved_blocking: surviving.map(s => s.finding),
  verified,
  audit,
  ready_to_merge: Boolean(
    verified && verified.all_gates_green && !surviving.length &&
    audit && audit.report_matches_diff && (audit.omissions || []).length === 0
  ),
}
