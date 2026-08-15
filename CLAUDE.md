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

### Verification

**A check that can pass while seeing nothing is worse than no check.** `rust_sources()`
is a flat `read_dir`: move one file into a subdirectory and every shape rule reports green
while inspecting nothing. Gates assert what they found.

**Never work around your own gate.** If a gate fires, fix the code. If the gate is
miscalibrated, say so explicitly and change it deliberately — never quietly widen it to
admit the change that tripped it.

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
