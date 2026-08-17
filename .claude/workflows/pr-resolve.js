export const meta = {
  name: 'pr-resolve',
  description: 'Resolve confirmed findings on an existing PR worktree, then re-critique, verify and audit',
  whenToUse: 'When a pr-pipeline2 run died after its critique stage. args {pr, worktree, branch, spec, base, findings}',
  phases: [
    { title: 'Resolve', detail: 'fix every confirmed finding' },
    { title: 'Critique', detail: 'two lenses re-read the fixed tree' },
    { title: 'Screen', detail: 'one skeptic per new blocking finding' },
    { title: 'Verify', detail: 'a different agent runs every gate' },
    { title: 'Audit', detail: 'what did every earlier stage miss?' },
  ],
}

const PR = args.pr
const WT = args.worktree
const SPEC = args.spec
const BASE = args.base
const FINDINGS = args.findings
const REPO = args.repo || '/local/home/scheschb/research/ACTOR'

const CONTEXT = `
YOUR WORKTREE: ${WT}. Work ONLY here. Never touch ${REPO}.
YOUR DIFF IS AGAINST ${BASE}: 'git -C ${WT} diff ${BASE}'. Not origin/main, which moves.

READ FIRST with the Read tool:
  ${WT}/${SPEC}                    the spec for THIS PR - authoritative
  ${WT}/CLAUDE.md                  comment/design/testing/refusal principles
  ${WT}/docs/HANDOFF.md            gates, machine constraints, traps that already bit

TOOLCHAIN before any cargo command:
  export PATH="$HOME/.cargo/bin:$PATH" && unset RUSTUP_TOOLCHAIN     # rustc must be 1.94.0
RUSTUP_TOOLCHAIN is exported in this shell and silently overrides rust-toolchain.toml. The
trybuild .stderr files are toolchain-sensitive; one recorded under the wrong compiler has
shipped a red main once.

THE GATES (from ${WT}/tools unless noted):
  cargo fmt --check
  cargo test  --locked --lib --bin harvest-tools
  cargo test  --locked --test architecture
  cargo test  --locked --test compile_fail
  cargo clippy --locked --all-targets
  cargo clippy --locked --lib --bins -- -D clippy::panic
  cargo doc   --locked --no-deps
  cargo build --release --locked
  python3 tools/test_comment_budget.py                                  (from ${WT})
  python3 tools/comment_budget.py --max-comments 2560 --max-ratio 20    (from ${WT}, after git add -A)
  python3 tools/check_paths.py                                          (from ${WT})

THE COMMENT BUDGET CHANGED FLAGS. '--max 14' is retired and now exits 2. The invocation above
is the one .github/workflows/type-safety.yaml runs; check it textually if in doubt.

Plus, because this PR touches the artifact pipeline, the golden fingerprint - which needs the
env var, or it prints NO SIGNAL and proves nothing:
  HARVEST_GOLDEN_RESULTS=${REPO}/results cargo test --locked --test integration artifact_fingerprint
It must pass AND the 40 pinned digests must be unchanged. 'cargo test --test integration' as a
whole cannot pass in a worktree (six tests need a submodule a worktree does not inherit); run
the fingerprint by name.

DO NOT WRITE TO /tmp - a tmpfs whose inode table has been exhausted once, taking the machine
down. Use /local/home/scheschb/scratch/<yours>, CARGO_TARGET_DIR included.

DESTRUCTIVE COMMANDS: one absolute path, spelled in full, as the whole command. Never a glob,
never relative, never on a line with anything else - a denied command blocks the whole run
until a human answers. Cache entries are chmod'd read-only, so 'chmod -R u+w <abs path>' first
or the delete silently leaves the tree. If denied, report the path and move on.

NEVER: weaken/disable/skip/delete a gate; add #[allow]/#[expect]/#![allow]/#[ignore]; grow an
ALLOWED list; run a blanket TRYBUILD=overwrite. If a gate fires, the code is wrong.

WHEN A CHECK STARTS FAILING there are three moves: fix the code; give the check the information
it lacks; or escalate a genuinely miscalibrated check. Making it unable to fire is never one of
them - and "classify that input as unknown/not-applicable" IS making it unable to fire. Before
finishing, answer for every check your diff touches: after my change, what input still makes
this check FAIL? Name it.

PRIVACY CARRIES INVARIANTS. An item whose private visibility enforces something cannot be
widened or moved down a layer to make a change compile - the move is what breaks it. If you
need a widening, the item is in the wrong place: leave it and report it.

WRITE YOUR REPORT FROM THE DIFF, AS THE LAST THING YOU DO. Reports in this sequence have
described trees that were never written. Run 'git diff --numstat ${BASE}' and 'git diff
${BASE}' at the END and derive every claim and every number from what you see.
`

phase('Resolve')

const resolved = await agent(`${CONTEXT}

You are resolving CONFIRMED blocking findings on PR ${PR}. The implementation already exists in
${WT}; a previous run's critique stage found five defects and an independent skeptic confirmed
EACH of them against the code. Do not re-litigate them - they are real.

READ THE FINDINGS WITH THE Read TOOL: ${FINDINGS}
That JSON has "confirmed_findings" (five, each with what/where/why/fix) and
"screener_confirmations" (the evidence each was checked against).

Two of the five are severe and interact, so read them together before editing:

* The accepted build products are still Disposition::StoreAndHash, so digest_tree hashes them
  and Carry::FromArtifact stores them - but scrub() reads with read_to_string and silently
  skips non-UTF-8. So a binary holding the random per-run scratch path enters the SEALED digest
  unscrubbed. That breaks the module's third compiler-enforced invariant ("a tree cannot be
  hashed before it is scrubbed") and makes the digest differ every run, so the cache looks
  enabled and never hits. This is the single most important thing to get right.
* The guard cannot see a recorded reference file LEAVING the admitted set, so any agent action
  that reclassifies part of c_src as build output silences the check entirely. A recorded path
  that is still on disk but no longer admitted is tampering, and is neither Edited nor Removed
  - it wants its own named OracleChange.

Fix all five. Keep spec-14's constraints: classify, BUILD_DIRS, Carry and digest_tree/hash_tree
untouched, the 40 golden digests unchanged, both cache keys unchanged, no visibility widening.
Tests 3-5 of the spec (edited oracle refused, deleted oracle refused and named, added header
refused) are what stop this PR being a loosening - if any of them can pass with your new logic
deleted, the logic is wrong.

Report: what you changed per finding, the verbatim result line of every gate, the golden
fingerprint result, and for each of the spec's five required tests the mutation that makes it
fail.`,
  { label: `pr${PR}:resolve`, phase: 'Resolve' })

log(`resolve returned ${resolved ? resolved.length : 0} chars`)

const FIND = {
  type: 'object', additionalProperties: false,
  required: ['lens', 'blocking', 'nonblocking'],
  properties: {
    lens: { type: 'string' },
    blocking: {
      type: 'array', maxItems: 8,
      items: {
        type: 'object', additionalProperties: false,
        required: ['what', 'where', 'why', 'fix'],
        properties: { what: { type: 'string' }, where: { type: 'string' }, why: { type: 'string' }, fix: { type: 'string' } },
      },
    },
    nonblocking: { type: 'array', maxItems: 6, items: { type: 'string' } },
  },
}

const LENSES = [
  { key: 'gates', text: `LENS: **gate integrity, and you are the only lens that builds.** Diff
against ${BASE}. Hunt for a gate weakened to pass: a rule loosened, an ALLOWED list grown, a new
#[allow]/#[expect]/#[ignore], a weakened assertion, a .stderr re-recorded beyond line/column
shifts, a changed pinned error code, a raised threshold, a visibility widened to make a change
compile. Then RUN every gate including the release build, the budget and the golden fingerprint,
and compare with what was reported. A discrepancy between claimed and actual is blocking. Also
confirm the 40 golden digests are unchanged and say how you know.` },
  { key: 'adversarial', text: `LENS: **adversarial correctness on the oracle guard.** Assume the
five fixes are incomplete. The specific things to attack: can any file still reach the sealed
digest without scrub() having normalised it? Can the guard still be silenced by making a
recorded reference file stop being admitted? Does the guard's admitted set still exactly match
what digest_tree hashes and Carry::FromArtifact stores, and if they diverge is the divergence a
refusal rather than a silent tolerance? Is an added .h or .c still refused? Give a concrete
failing scenario - inputs, then wrong output - for each finding; one without a scenario is
nonblocking. Do NOT run the gates.` },
]

phase('Critique')
const verdicts = (await parallel(LENSES.map(l => () =>
  agent(`${CONTEXT}

You are a READ-ONLY reviewer of PR ${PR} after a fix round. Do not edit anything. Read the real
diff and the real code; do not trust the account.

THE RESOLVER REPORTED:
${resolved}

The five findings it was fixing are in ${FINDINGS} - read them, and check each is ACTUALLY
fixed in the tree rather than described as fixed.

${l.text}

Mark a finding blocking only if it must be fixed before merge. An empty blocking list is the
right answer if the fixes are sound.`,
    { label: `pr${PR}:crit:${l.key}`, phase: 'Critique', schema: FIND })
))).filter(Boolean)

const claimed = verdicts.flatMap(v => (v.blocking || []).map(b => ({ ...b, lens: v.lens })))
log(`${claimed.length} new blocking claimed`)

const SCREEN = {
  type: 'object', additionalProperties: false,
  required: ['refuted', 'reason'],
  properties: { refuted: { type: 'boolean' }, reason: { type: 'string' } },
}

let surviving = []
if (claimed.length) {
  phase('Screen')
  const screened = await parallel(claimed.map(f => () =>
    agent(`${CONTEXT}

Screen ONE review finding on PR ${PR} for correctness. Read-only.

THE FINDING:
${JSON.stringify(f, null, 1)}

Try to REFUTE it against the actual code. Is the claim true of the tree as it stands? Is the
consequence real, or does something else already prevent it? Would the proposed fix be a change
for the worse? Quote the lines you checked.

Set refuted=true if it is factually wrong, already handled, or would make the code worse. Set
refuted=false only if you confirmed it. Default to refuted=true when you cannot confirm - an
unconfirmed finding must not drive an edit.`,
      { label: `pr${PR}:screen`, phase: 'Screen', schema: SCREEN })
      .then(v => ({ finding: f, screen: v }))
  ))
  surviving = screened.filter(Boolean).filter(s => s.screen && s.screen.refuted === false)
  log(`${surviving.length} of ${claimed.length} survived screening`)
}

phase('Verify')

const VERDICT = {
  type: 'object', additionalProperties: false,
  required: ['all_gates_green', 'golden_unchanged', 'gate_output', 'net_diff_stat', 'concerns'],
  properties: {
    all_gates_green: { type: 'boolean' },
    golden_unchanged: { type: 'boolean' },
    gate_output: { type: 'string' },
    net_diff_stat: { type: 'string' },
    concerns: { type: 'array', maxItems: 6, items: { type: 'string' } },
  },
}

const verified = await agent(`${CONTEXT}

You are the INDEPENDENT verifier for PR ${PR}. You did not write this and must not edit it.
Run every gate in ${WT} from a clean state and report exactly what happened.

Run 'git -C ${WT} add -A' first, then the budget, since it reads git ls-files. Run the golden
fingerprint WITH its env var and confirm it did not print NO SIGNAL; set golden_unchanged only
if it passed with the pinned digests intact.

Also report: git -C ${WT} diff --stat ${BASE} | tail -1

Set all_gates_green true ONLY if every gate passed. Do not fix anything. Do not round up. Put
anything that passed but looks wrong in concerns.`,
  { label: `pr${PR}:verify`, phase: 'Verify', schema: VERDICT })

phase('Audit')

const AUDIT = {
  type: 'object', additionalProperties: false,
  required: ['omissions', 'report_matches_diff', 'notes'],
  properties: {
    omissions: { type: 'array', maxItems: 8, items: { type: 'string' } },
    report_matches_diff: { type: 'boolean' },
    notes: { type: 'string' },
  },
}

const audit = await agent(`${CONTEXT}

You are the completeness auditor for PR ${PR}. Read-only. Everyone before you hunted defects;
find what they did NOT look at.

Read ${WT}/${SPEC} and the full diff yourself, fresh. Also read ${FINDINGS}.

THE RESOLVER'S ACCOUNT:
${resolved}

Answer three things:
1. What in the spec has no earlier stage checked - an acceptance criterion never measured, a
   required test absent or vacuous, a "must NOT change" item nobody verified? spec-14 requires
   five tests and demands the 40 golden digests be unchanged; confirm each by command.
2. Does the account match the diff? Name every file in the diff the account omits and every
   claim the diff contradicts. This has gone wrong repeatedly here.
3. Anything a reviewer needs in the commit message that is not written down yet.

Set report_matches_diff false if they disagree in any way that matters. Cite files and lines.`,
  { label: `pr${PR}:audit`, phase: 'Audit', schema: AUDIT })

return {
  pr: PR,
  worktree: WT,
  base: BASE,
  unresolved_blocking: surviving.map(s => s.finding),
  verified,
  audit,
  ready_to_merge: Boolean(
    verified && verified.all_gates_green && verified.golden_unchanged && !surviving.length &&
    audit && audit.report_matches_diff && (audit.omissions || []).length === 0
  ),
}
