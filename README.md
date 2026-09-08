# ACTOR

ACTOR performs **A**gentic **C**-**to**-**R**ust translation.
Its input is a C program and its output is a Rust program.

## Requirements and Installation

The [developer documentation](docs/DEVELOPER.md) document the prerequisite toolchain and steps
    required to build and install the ACTOR binary (i.e., `harvest-tools`) and set up the evaluation
    benchmarks and datasets.

## Running ACTOR on your own C program

To translate an arbitrary
C program of your own, add it as a one-off case and run any tool on it.

An arbitrary C program (i.e., a set of `.c` and `.h` files) must be supplied to ACTOR in the
    `test-corpus` directory as test case.
The test case directory must follow the structure:

```
ACTOR/test-corpus/Public-Tests/<BATTERY_NAME>/<TEST_CASE_NAME>
```

A battery comprises a collection of test cases.
The source code file(s) must be placed in a directory named `test_case`, i.e.,

```
ACTOR/test-corpus/Public-Tests/<BATTERY_NAME>/<TEST_CASE_NAME>/<test_case>
```

The harness can only detect files in the `test_case` directory when a sibling `test_vectors`
    directory is present, i.e.,


```
ACTOR/test-corpus/Public-Tests/<BATTERY_NAME>/<TEST_CASE_NAME>/<test_vectors>
```

`test_vectors` can be left empty if you only want to translate the code (and not run any evaluations
    on it).

### Steps to Supply a C Program to ACTOR

1. Create a case directory under a battery. A case is a folder named
   `<something>` (suffix it with `_lib` if it is a library rather than an
   executable) containing a `test_case/` subdirectory with your C sources
   and headers. The harness only discovers a case if the following conditions are met:
   - It is placed under `Public-Tests`; and
   - The `test_case` directory also has a sibling `test_vectors` directory - it may be empty if you
     only want to translate.

   ```bash
   mkdir -p test-corpus/Public-Tests/mycases/hello/test_case
   mkdir -p test-corpus/Public-Tests/mycases/hello/test_vectors
   cp path/to/your/*.c path/to/your/*.h \
      test-corpus/Public-Tests/mycases/hello/test_case/
   ```
   
2. Translate it with any tool. The agent works only from the C source in
   `test_case/`; it writes a Cargo project (`Cargo.toml` + `src/`) with the
   translated Rust:

   ```bash
   # One tool, one case. --tool takes claude, codex or kiro.
   harvest-tools --tool claude run mycases/hello

   # Several tools at once, three invocations in flight per tool
   harvest-tools --tool claude,codex,kiro --parallel 3 run mycases/hello
   ```

   `run` is the whole chain: every step the prompt variant declares (today
   translate then verify), then scoring, then `tables/`. There is no separate
   `translate` or `verify` subcommand — they were the same function at two
   prompts, so `--steps N` takes a prefix of the chain instead.

   The translated crate is written under `results/.../mycases/hello/translated/`
   (the pre-verify phase dir), and the verify phase's output into a sibling
   `verified/`; scoring reads `verified/` if present, else `translated/`.

3. (Optional) To also *score* correctness the way the benchmarks do, put
   JSON input/expected-output vectors in the `test_vectors/` directory before
   running. With an empty `test_vectors/`, ACTOR still translates the program
   and verifies it against tests it generates itself; only the built-in
   benchmark scorer is skipped.

At its core, ACTOR is an off-the-shelf coding agent pointed at a directory
whose `c_src/` holds the C code, driven by the prompts in `prompts/`. The
case layout above is just how the benchmark harness feeds that C to the
agent and (optionally) scores the result.

## Evaluation

The remainder of this README file discusses our experimental evaluation of ACTOR.

This repository evaluates agentic C-to-Rust translation across the public parts of Lincoln Labs's TRACTOR Test-Corpus. Every published number is replayed from the invocation cache in CI, one job per tool, with the regenerated tables diffed byte-for-byte against the committed ones.

## Tools

`--tool` takes exactly these three. Each is a coding CLI driving one pinned model;
the prompts, the pipeline and the scoring are identical across them, so the only
difference a table shows is the tool and its model.

| Tool | Description |
|------|-------------|
| **claude** | Claude Code |
| **codex** | OpenAI Codex CLI on Amazon Bedrock. `--model` selects the model (`gpt-5.6-sol` and earlier) |
| **kiro** | [kiro-cli](https://github.com/aws/kiro-cli) |
| **kiro-translate** | Not a `--tool`: a derived column. kiro's `translated/` phase scored without the verify/repair loop — the "no validate" number. Emitted automatically; no separate run. |

Seven further backends (`opencode`, `c2rust`, `laertes`, `c2saferrust`,
`smartc2rust`, `kimi`, `oneshot`) were removed in #138. None had ever produced a
`result.json` in any results tree, so nothing they could have been compared on
was ever published.

## Datasets

| Dataset | Cases | Description |
|---------|-------|-------------|
| **B01_organic** | 38 | Real-world single-function libraries from open-source projects |
| **B01_synthetic** | 85 | Synthetic single/multi-file programs and libraries |
| **B02_organic** | 44 | Multi-file real-world libraries (higher complexity) |
| **B02_synthetic** | 42 | Synthetic multi-file programs with complex patterns |
| **P00_perlin_noise** | 1 | Single large project (Perlin noise generator) |
| **P01_sphincs_plus** | 128 | SPHINCS+ post-quantum crypto — 1 shared translation, 128 KAT vectors |

## Repository Structure

```
tools/src/                  # harvest-tools CLI (Rust)
├── main.rs                 # CLI dispatch
├── cli.rs                  # Tool/command definitions
├── chain.rs                # The pipeline: a chain of agent invocations
├── invocation.rs           # ONE invocation: run_or_replay(work_dir, prompt)
├── store.rs                # The invocation cache; the path IS the key
├── tree.rs                 # Working dirs and sealed trees
├── prompt.rs               # Which prompt a step is handed; chain length
├── battery.rs              # Case discovery, phase dirs, agent metadata
├── agents/                 # Concurrency pool and session recipes
├── runners/                # The keyed backends: claude, codex, kiro
├── oracle/                 # Scoring: MIT runtests, harvest-bench gtest
├── analyse/                # Metrics and table generation
├── domain/                 # Pure logic: health, contents, outcomes
└── io/                     # Filesystem and sandbox edges
prompts/
├── shared/                 # The methodology, one copy for every tool
├── claude/, codex/, kiro/  # Each tool's protocol tail, plus claude's ablations
test-corpus/                # MIT TRACTOR Test-Corpus (submodule)
harvest-bench/              # Whole-library benchmark + its gtest runner (submodule)
results/                    # Invocation cache and published results (submodule)
tables/                     # Generated result tables (never edited by hand)
```

## Results

The tables live in `tables/` and are GENERATED — never edited by hand:

| File | What it holds |
|------|---------------|
| `tables/results.md` | Per-battery cases and vectors passed, C and Rust LOC, unsafe lines, for every tool |
| `tables/tractor.tex` | The Test-Corpus table as the paper prints it |
| `tables/harvest-bench.tex` | Whole-library results: builds, upstream gtest cases passing, LOC, unsafe |
| `tables/datasets.tex` | Corpus shape: cases and C LOC per battery |
| `tables/numbers.tex` | Named constants the prose quotes, so text and tables cannot disagree |
| `tables/prompt-sensitivity.tex` | The prompt ablations |
| `tables/manual.tex` | The few numbers not derived from data, with their source |

They are rewritten by `harvest-tools --tool <t> run all` from what that run
resolved, and `harvest-tools tables` regenerates them from the committed results
alone with no replay and no agent. Both are checked in CI: `tables/` must follow
from the committed `result.json` files byte-for-byte, so a number cannot move
without the results moving with it.

Reproduce any published figure without spending anything:

```bash
TOOLS=claude,codex,kiro tools/reproduce.sh all
```

`--replay-only` makes a cache miss a refusal rather than a paid run, so the
script is incapable of invoking an agent. It fails if any phase paid for a case,
if any battery went out of scope, or if a regenerated table differs from the
committed one.

Numbers are deliberately not duplicated here. This README carried a hand-copied
copy of every battery's table for nine agents; six of those agents no longer
exist and the claude column had drifted from the generated file, which is what
duplicating a derived artifact buys.
