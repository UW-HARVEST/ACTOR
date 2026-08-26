# CLAUDE.md

## Code comments

Default to no comments.

Only add a comment when it explains WHY something non-obvious is necessary:

- a hidden constraint
- a subtle invariant
- a workaround for a specific bug
- surprising behavior

Never:

- restate what the code does
- narrate implementation steps
- explain obvious control flow
- copy information from the conversation into comments
- add comments that are redundant with names/types

Before finishing, remove any comment whose meaning is obvious from the code
directly below it.

Exception: doc comments on `clap`-derived types are `--help` output, not comments.
Deleting one silently changes what the binary prints.

## Design principles

Each of these prevented, or failed to prevent, a real failure in this repo. Follow them.

### Types

**Parse at the edges; types inside.** Only edge functions touch the filesystem,
processes or the environment. Everything inward takes and returns types. A function like
`classify(log: &Path)` that reads the file is an edge disguised as logic — it becomes
`classify(text, format, exit)` and the read moves out.

**Make illegal states unrepresentable, not checked.** If you are writing a runtime check
for an invariant, ask whether a type can carry it instead. `Completed` has a private
field, so "never cache an infra failure" cannot be written wrong. `SeededBy` declares the
only legal phase transitions, so an illegal one does not compile.

**Newtypes over primitives; named enums over bools.** A bool that gates a safety property
is waiting for a transposition: `write_settings(.., false)` reads as both "not allowed to
be unsandboxed" and "sandbox off". No function takes two bools.

**One definition per concept.** One table mapping agent to log format, one phase
predicate, one traversal predicate. A second copy drifts — two agent-name tables put a
wrong name in 208 result files.

### Structure

**Module dependencies form a DAG.** Ten of nineteen modules were a single cycle because
nothing enforced it. A cyclic split is a nominal one.

**Split by concern, not by type.** A state machine is one concept and belongs in one
file however many types it holds. Conversely, a 148-line classifier that exists only to
serve digest/copy/scrub is not a peer module.

**Do not add a function that duplicates an existing abstraction.** If the behaviour
already exists (`Sealed::publish`), route to it instead of hand-rolling a second copy.

### Storage

The target design for the store. Where the code still disagrees, that is debt, not licence:
`.cache/<SCHEMA>/<phase>/…`, the `failed/` subtree, keying the toolchain and the recipe, and the
oracle tamper check all predate these rules.

**Git versions the layout; the layout does not version itself.** `SCHEMA` does one job three
times — hashed into the key, a path level (`.cache/4/`), and a field in `meta.json` — and only the
field earns it. A version level in every path buys nothing but side-by-side coexistence during a
migration, which git already gives: 279,505 directory renames landed in one commit, re-keyed
nothing, and were revertible throughout. Migrate into a fresh tree, keep the old one under another
name until `reproduce.sh all` is green, then delete it. Keep ONE `format` field and refuse loudly
on mismatch, so a reader never guesses at a layout it does not understand.

The same applies to `KEY_ALGORITHM`, `TREE_ALGORITHM`, `ORACLE_TREE_ALGORITHM` and
`PROMPT_ALGORITHM` — version strings hashed into their own digests. They are redundant on their own
terms: change which components a key covers, or their order, and every key changes anyway, because
the components are length-prefixed and hashed in sequence. A tag would only earn its place if the
formula's meaning could change while the hash stayed equal. Delete them.

**The unit is an AGENT INVOCATION, and a pipeline is a chain of them.** One function,
`run_or_replay(working_dir, prompt) -> working_dir`, and nothing in it knows where in the chain it
sits. There is no such thing as a translate entry or a verify entry — there are invocations, each
with its own cache entry, differing only in the working dir handed in and the prompt. So `phase` is
not key material, `SeedAt` has no reason to exist, there is ONE tree algorithm rather than a corpus
one and an artifact one, and adding a third step to the chain requires no new concept.

```
W0 = assemble(corpus)                       c_src/ + empty translation/
W1 = run_or_replay(W0, prompt_1)   ← entry   then transform(W1)
W2 = run_or_replay(W1, prompt_2)   ← entry   then transform(W2)
Wn = run_or_replay(W(n-1), prompt_n)         … same function every time
```

What we run today happens to be a chain of two, with `prompt_1` translating and `prompt_2`
verifying. That is a fact about the prompts, not about the machinery.

**Every working dir has one shape.** `c_src/` beside the translation, hashed as one tree with its
contents uninspected. The first in a chain differs only in that its translation is empty — not in
kind, not in layout, not in how it is hashed. Every change happens inside the working dir.

**Two kinds of edge, and only one is cached.** An AGENT RUN is nondeterministic and expensive, so
it is keyed on `tool ‖ model ‖ input_tree ‖ prompt` and stored, with N attempts per key. A HARNESS
TRANSFORM is deterministic and cheap, so it is recomputed and never cached — and it must stay
OUTSIDE the cache, or harness logic is baked into the agent's artifact and changing it invalidates
runs that are still good. `post_process_independent` is one of these: it renames `[lib]` to the
corpus runner's name and appends `[workspace]`, so the NEXT invocation's `before` is
`transform(previous after)` and not the previous `after` itself. Measured: 216 of 216 paired entries
differ, and `agents/run.rs` records 0 of 84 matching from the other direction.

**The graded tree contains no C.** `runtests`' own discovery needs exactly `<case>/translated_rust/`
and `<case>/test_vectors/` and reads nothing else, so the eval tree is assembled from the
translation and the corpus vectors alone. An artifact that tries to link the original C then fails
to build at grading time, because there is no C there to link. Agents are misaligned; this is not
a policy to enforce per agent but a shape that makes the shortcut unrepresentable. One published
artifact CMake-built the original library, `objcopy`-renamed all 881 public symbols and jumped to
them from naked asm, and scored full marks at 1,013 lines against another agent's 27,044.

**Restore rather than detect.** `c_src` is the pinned corpus, so a working dir is always assembled
with the corpus's copy and the agent's edits to it are discarded before hashing — which is why
tampering cannot persist and a tamper check is unnecessary. Nothing in the grading path reads the C
anyway: `runtests.rust` scores the Rust against static `test_vectors/`, so the check protected
provenance only, and a restore protects it better and covers the next phase too. Restoring is about
what is ASSEMBLED; it does not license storing less than was hashed.

**Store the preimage of every hash.** An entry keeps the exact bytes that were hashed — the whole
`before` working dir, the whole `after`, the raw prompt text — even where they duplicate the corpus.
This is the single property that keeps the design changeable: alter the ignore rules, the path
prefix, the digest algorithm or the definition of a tree, and every key is recomputable from stored
bytes with no agent re-run. It is the only reason re-keying was possible when the store gained a
model level, and it is the cheapest insurance available — storage costs nothing next to a sweep,
which costs $625 and twelve hours. Derive nothing that a hash was taken over.

**Key the identity; path the rendering.** The key hashes raw identifiers (`claude`,
`global.anthropic.claude-opus-5[1m]`); the path uses filesystem-safe slugs
(`claude/claude-opus-5-1m`). `model_dir_slug` is lossy on purpose, and a lossy rendering used as
key material is how `openai/gpt-5.4` came to name the directory `oneshot/4`. Renaming a directory
must never re-key an entry.

**Key only what changes the answer; pin and record the rest.** The toolchain is fixed by
`rust-toolchain.toml`, so keying it strands every entry on a bump and proves nothing — refuse at
preflight if the active one differs, and record it. #109 removed `cli` for exactly this.

**Every run pins its model, and a sentinel is not a model.** kiro keyed `unpinned:kiro-cli-default`
on a comment claiming kiro-cli takes no `--model`; it takes one, and because nothing passed it, 0
files under `results/Test-Corpus/kiro/` name a model and those rows are unattributable forever.
Assert the flag on the command line, not on the transcript — a missing pin is invisible after the
run.

**One key, one entry. Several attempts must not be representable.** The cache is a function: a key
maps to one value, so a table's numbers follow from the key alone and reproducibility is structural
rather than a selection rule to get right. There is no attempt level, no pin to record, no tie to
break. An entry whose `outcome` is not `completed` does not satisfy a lookup — it is a record, and a
re-run replaces it.

**Both trees raw, and one record beside them.** The path already carries tool, model, before-tree
and prompt, so recording those again is the redundancy this replaces. What is NOT redundant is the
trees: `before` and `after` are stored whole, as hashed.

```
key = sha256(tool ‖ model ‖ before_hash ‖ prompt_hash)

.cache/<tool>/<model>/<before_hash>/
    before/                      the raw working dir that hashes to <before_hash>
    <prompt_hash>/
        prompt.json              {digest, text}
        after/                   the raw working dir the agent left
        agent.json               {outcome, output_tree, wall_secs, cost_usd, cli}
        run.log                  the transcript
```

`before/` is stored once and shared by every prompt beneath it. Nothing else is recorded: not the
ACTOR commit, not the toolchain. Neither can influence the entry, because the cached function is
`(before, prompt) -> after` with both inputs content-hashed and every harness transform outside the
cache. `output_tree` stays, not as bookkeeping but as an integrity check on the `after/` beside it —
the same reason a `before/` that no longer reproduces its own directory name is a corrupt entry.

`<tool>` is the spelling `--agent` accepts (`claude`, `codex`, `kiro`), so the path cannot drift
from the CLI surface. Today's function is called `harness_dir`, which collides with `harness`
meaning the ACTOR commit everywhere else; it is the TOOL level and should say so.

`outcome` is CLASSIFIED from the transcript, never the exit code: every session pipes through
`tee`, so a killed agent reported a clean 0 until `set -o pipefail` was asserted. `output_tree` is
not optional — with several attempts under one key it is the only thing selection can read. Keep
`run.log`, because `agent.json` is derived from it: outcome, cost and the model-pin check all read
it, so dropping it makes the record unverifiable and un-rederivable.

**What used to be `seal` is four steps, not one.** CLASSIFY the transcript, RESTORE `c_src` from
the corpus, SCRUB absolute paths to a token, DIGEST. Only the last is `seal`'s remaining job.
`scrub` must stay ahead of the digest or every key carries a per-run nonce; and with failures
stored like anything else, the `Completed` capability token has nothing left to guard — the
outcome is a field, and selection is what refuses to serve it.

### Testing

**A test names a failure, not a function.** If you cannot say what breaks in the world
when it goes red, it is restating the code — delete it. Test names are sentences about
consequences: `a_runner_that_errors_is_not_scored_from_the_file_it_left`.

**Favour whole-path tests and specialised regression tests. Nothing in between.** Either
exercise a real path end to end (corpus → translate → seal → publish → digest → replay),
or pin one specific documented failure. A test that touches a single function to confirm
it does what it says is friction with no information, and it breaks on every refactor.

**Keep the count down.** Prefer one table-driven test looping over cases to N tests
sharing a fixture. The architecture rules are the model: one test, whole crate.

**Every test must be able to fail.** Assert non-vacuity — that the fixture really
contains the trap, that the old path really refused what the new one accepts, that the
inspection found something to inspect. A green test that inspects nothing is worse than
no test.

**A test that hands in the value under test proves nothing about how it is produced.**
This is the most repeated defect in this repo's history: three PRs in one night shipped a
test that passed a literal where the function being tested should have produced it. One
handed `SkipCheck::Keyed` straight in while nothing anywhere asserted the resolver ever
returns `Keyed`, so the resolver could be reverted to its pre-PR answer — undoing the whole
change on exactly the sweep it was written for — with the suite still green. Assert the
mapping, exhaustively over the input type, not one hand-built output.

**A fixture that pins a parameter makes that parameter untestable.** A `paths_at` helper
hardcoded `Mode::Bypass`, and the code under test collapsed every value under `Bypass`, so
the value being resolved was unobservable through the only door the tests used. When a
fixture fixes a value, ask which assertion that fixing silently disables.

**Mutate before you claim.** Never write "a regression here fails this test" without
breaking the named thing and watching it go red. Each of the tests above carried a comment
asserting exactly the coverage it did not have, and a false comment about coverage is worse
than no comment: it stops the next reader from checking.

### Verification

**A check that can pass while seeing nothing is worse than no check.** `rust_sources()`
is a flat `read_dir`: move one file into a subdirectory and every shape rule reports green
while inspecting nothing. Gates assert what they found.

**Never work around your own gate.** If a gate fires, fix the code. If the gate is
miscalibrated, say so explicitly and change it deliberately — never quietly widen it to
admit the change that tripped it.

**When your change makes a check start failing, there are exactly three moves.** The check
is right and the code is wrong, so fix the code. Or the check lacks the information to judge
correctly, so *give it that information*. Or it is genuinely miscalibrated, which is an
escalation and not your decision. Making it unable to fire is never one of the three.

**"Classify it as no-evidence" is making it unable to fire.** This is the disguise the
previous rule does not catch, because it feels like a correctness fix rather than a
weakening. A gate was made blind to 7 of 17 agents by passing `Exit::Unobserved`, which made
every opaque transcript classify `Unknown` — semantically true for that input, and the gate
filters on `is_infra()`, so it went silent two files away. Nobody wrote "disable the gate".

So ask the mechanical question instead of trusting the reasoning:

> **After my change, what input still makes this check fail?**

If the answer is "none, for this class of input", the check is off for that class. Name the
input, or you have not verified anything. A check whose failing input you cannot produce is
not a check.

**Measure; do not estimate.** "Verify is ~92% of the available saving" was measured on
Test-Corpus; on harvest-bench it is 45%. Numbers in commit messages are measured or
absent.

### Refusal

**Refuse before the money, not after.** Preflight everything a long run depends on:
toolchain versions, binaries on PATH, provenance. A 3h20m sweep completed and then had
all seven verifications refused for a variable that was already set at launch.

**An artifact records what produced it** — commit, compiler, model, CLI version. A path
names a file, not a commit; that cost $625 and twelve hours.

**The environment is an input.** `RUSTUP_TOOLCHAIN` silently overrides
`rust-toolchain.toml`; `/tmp` is cleared on reboot; `HARVEST_CLI_VERSION` is applied to
every program probed. Pin it, or refuse.
