# ACTOR

ACTOR performs **A**gentic **C**-**to**-**R**ust translation.
Its input is a C program and its output is a Rust program.


## Running ACTOR on your own C program

To translate an arbitrary
C program of your own, add it as a one-off case and run any agent on it.

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
   
2. Translate it with any agent. The agent works only from the C source in
   `test_case/`; it writes a Cargo project (`Cargo.toml` + `src/`) with the
   translated Rust:

   ```bash
   # Agentic translation with Claude Code (or --agent kiro)
   harvest-tools --agent claude translate mycases/hello

   # Mechanical transpilation with c2rust
   harvest-tools --agent c2rust translate mycases/hello
   ```

   The translated crate is written under `results/.../mycases/hello/translated/`
   (the pre-verify phase dir). If a verify phase runs, its output lands in a
   sibling `verified/` dir; scoring reads `verified/` if present, else `translated/`.

3. (Optional) To also *score* correctness the way the benchmarks do, put
   JSON input/expected-output vectors in the `test_vectors/` directory, then
   run the full pipeline instead of just `translate`:

   ```bash
   harvest-tools --agent claude run mycases/hello
   ```

   With an empty `test_vectors/`, ACTOR still translates the program (and,
   for the agentic agents, verifies it against tests it generates itself);
   only the built-in benchmark scorer is skipped.

At its core, ACTOR is an off-the-shelf coding agent pointed at a directory
whose `c_src/` holds the C code, driven by the prompts in `prompts/`. The
case layout above is just how the benchmark harness feeds that C to the
agent and (optionally) scores the result.

## Evaluation

The remainder of this README file discusses our experimental evaluation of ACTOR.

This repository evaluates multiple translation agents — from mechanical transpilers to agentic LLM pipelines to one-shot LLM baselines — across CRUST-bench and the public parts of Lincoln Labs's TRACTOR Test-Corpus. All results are CI-validated with exact-match checks against stored summaries.

## Agents

| Agent | Type | Description |
|-------|------|-------------|
| **kiro** | Agentic | Multi-turn [kiro-cli](https://github.com/aws/kiro-cli) agent: translate → verify (C-as-oracle) → test |
| **kiro-translate** | Derived column (not an `--agent`) | kiro's pre-verify result — its `translated/` phase scored without the verify/repair loop (the "no validate" column). Reported for every agent automatically; no separate run. |
| **claude** | Agentic | Claude Code with project-level context |
| **c2rust** | Mechanical | [c2rust](https://github.com/immunant/c2rust) transpiler (unsafe, line-for-line) |
| **laertes** | Mechanical + rules | c2rust + [Laertes](https://doi.org/10.1145/3453483.3454107) (PLDI 2021) rule-based unsafe reduction |
| **kimi** | One-shot LLM | Kimi K2.5 via AWS Bedrock |
| **gpt-5.4** | One-shot LLM | GPT-5.4 via OpenRouter |
| **gemini-3.1-pro-preview** | One-shot LLM | Gemini 3.1 Pro Preview via OpenRouter |

## Datasets

| Dataset | Cases | Description |
|---------|-------|-------------|
| **B01_organic** | 38 | Real-world single-function libraries from open-source projects |
| **B01_synthetic** | 85 | Synthetic single/multi-file programs and libraries |
| **B02_organic** | 44 | Multi-file real-world libraries (higher complexity) |
| **B02_synthetic** | 42 | Synthetic multi-file programs with complex patterns |
| **P00_perlin_noise** | 1 | Single large project (Perlin noise generator) |
| **P01_sphincs_plus** | 128 | SPHINCS+ post-quantum crypto — 1 shared translation, 128 KAT vectors |
| **CRUST-bench** | 100 | Independent Rust crate translations with `cargo test` validation |

## Repository Structure

```
tools/                      # harvest-tools CLI (Rust)
├── src/
│   ├── main.rs             # CLI dispatch
│   ├── cli.rs              # Agent/command definitions
│   ├── translate.rs        # Translation: kiro, c2rust, laertes, kimi, oneshot
│   ├── verify.rs           # C-as-oracle verification
│   ├── test.rs             # MIT runtests + CI --check mode
│   ├── battery.rs          # Path resolution, unsafe counting, LOC
│   └── report.rs           # Markdown table generation
prompts/                    # System prompts organized by agent
├── kiro/                   # kiro prompts
├── claude/                 # Claude Code prompts
└── oneshot/                # One-shot LLM prompts (kimi, gpt-5.4, gemini)
test-corpus/                # MIT TRACTOR Test-Corpus (submodule)
crust-bench/                # CRUST-bench dataset (submodule, see https://github.com/benedikt-schesch/CRUST-bench)
results/                    # All translation results (submodule)
tables/                     # Auto-generated result tables
```

## Setup

```bash
git submodule update --init --recursive
cd tools && cargo build --release
```

One-shot LLM agents require API keys:
- `--agent kimi`: AWS Bedrock access (account `121913092579` via `ada-auth`)
- `--agent oneshot`: `OPENROUTER_API_KEY` environment variable

## Usage

```bash
# Full pipeline: translate → verify → test
harvest-tools --agent kiro run B01_synthetic

# Translate only
harvest-tools --agent c2rust translate B02_organic

# One-shot LLM translation
harvest-tools --agent oneshot --model openai/gpt-5.4 translate B01_organic

# Test and update stored results
harvest-tools --agent kiro test all --update

# CI validation (exact-match against stored summary.json)
harvest-tools --agent kiro test all --check

# Single case
harvest-tools --agent kiro run B01_synthetic/001_helloworld

# Generate result tables
harvest-tools report
```

## CRUST-bench

One-shot LLM results and per-project metrics (LOC, unsafe) live in our fork: https://github.com/benedikt-schesch/CRUST-bench

CRUST-bench can be run in two modalities, matching the settings reported
in the paper:

- **default (test repair)** — the agent is given the benchmark's
  ground-truth test binaries and repairs its translation against them.
- **`--blind` (self-generated tests)** — the ground-truth tests are
  withheld; the agent generates its own tests to verify the translation.
  This matches how TRACTOR is scored and is the more realistic setting,
  since a deployed translation tool has no ground-truth test suite. The
  paper calls this the *self-generated tests* setting.

```bash
harvest-tools --agent kiro run CRUST/all           # test repair (agent sees ground-truth tests)
harvest-tools --agent kiro run CRUST/all --blind   # self-generated tests (ground-truth tests withheld)
```

All commands are resume-friendly — they skip already-completed cases.

### Results

Auto-generated from validated `result.json` and `summary.json` files.
C LOC counted from `test-corpus/Public-Tests/`. Regenerate with `harvest-tools report`.
Auto-generated from validated `result.json` and `summary.json` files.

### B01_organic

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| c2rust | 38/38 | 775/775 | 2455 | 8974 | 2932 | 32.7% |
| claude | 38/38 | 775/775 | 2455 | 2872 | 1253 | 43.6% |
| claude-combined | 38/38 | 775/775 | 2455 | 2952 | 1244 | 42.1% |
| gemini-3.1-pro-preview | 35/38 | 761/770 | 2455 | 2186 | 498 | 22.8% |
| gpt-5.4 | 32/38 | 708/728 | 2455 | 2575 | 511 | 19.8% |
| kimi | 30/38 | 728/755 | 2455 | 2163 | 642 | 29.7% |
| kiro | 38/38 | 775/775 | 2455 | 2554 | 969 | 37.9% |
| kiro-translate | 38/38 | 775/775 | 2455 | 2505 | 954 | 38.1% |
| laertes | 37/38 | 775/775 | 2455 | 9636 | 2618 | 27.2% |

### B01_synthetic

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| c2rust | 85/85 | 393/393 | 2597 | 5214 | 2972 | 57.0% |
| claude | 85/85 | 393/393 | 2597 | 6145 | 1096 | 17.8% |
| claude-combined | 83/85 | 391/392 | 2597 | 4712 | 1008 | 21.4% |
| gemini-3.1-pro-preview | 75/85 | 368/379 | 2597 | 2407 | 110 | 4.6% |
| gpt-5.4 | 71/85 | 368/383 | 2597 | 2510 | 50 | 2.0% |
| kimi | 56/85 | 241/348 | 2597 | 2413 | 204 | 8.5% |
| kiro | 85/85 | 393/393 | 2597 | 4762 | 729 | 15.3% |
| kiro-translate | 83/85 | 391/393 | 2597 | 2888 | 483 | 16.7% |
| laertes | 83/85 | 389/389 | 2597 | 5970 | 2972 | 49.8% |

### B02_organic

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| c2rust | 42/44 | 254/254 | 23072 | 44792 | 34150 | 76.2% |
| claude | 43/44 | 261/263 | 23072 | 24391 | 18973 | 77.8% |
| claude-combined | 42/44 | 254/261 | 23072 | 24550 | 21806 | 88.8% |
| gemini-3.1-pro-preview | 22/44 | 125/170 | 23072 | 10298 | 2661 | 25.8% |
| gpt-5.4 | 25/44 | 165/193 | 23072 | 12823 | 2107 | 16.4% |
| kimi | 16/44 | 121/149 | 23072 | 10911 | 2559 | 23.5% |
| kiro | 42/44 | 257/263 | 23072 | 21796 | 14709 | 67.5% |
| kiro-translate | 41/44 | 254/261 | 23072 | 18329 | 12176 | 66.4% |
| laertes | 41/44 | 245/245 | 23072 | 44821 | 32559 | 72.6% |

### B02_synthetic

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| c2rust | 38/42 | 988/989 | 9877 | 21620 | 16365 | 75.7% |
| claude | 35/42 | 995/1025 | 9877 | 14752 | 5583 | 37.8% |
| claude-combined | 36/42 | 996/1025 | 9877 | 11977 | 4529 | 37.8% |
| gemini-3.1-pro-preview | 23/42 | 712/843 | 9877 | 8117 | 407 | 5.0% |
| gpt-5.4 | 25/42 | 770/907 | 9877 | 9215 | 158 | 1.7% |
| kimi | 16/42 | 241/306 | 9877 | 9311 | 891 | 9.6% |
| kiro | 31/42 | 942/1025 | 9877 | 12634 | 3828 | 30.3% |
| kiro-translate | 31/42 | 942/1025 | 9877 | 8420 | 1922 | 22.8% |
| laertes | 39/42 | 818/819 | 9877 | 21664 | 15770 | 72.8% |

### P00_perlin_noise

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| c2rust | 1/1 | 30/30 | 380 | 1643 | 514 | 31.3% |
| claude | 1/1 | 30/30 | 380 | 450 | 3 | 0.7% |
| claude-combined | 1/1 | 30/30 | 380 | 528 | 0 | 0.0% |
| gemini-3.1-pro-preview | 0/1 | 2/30 | 380 | 271 | 0 | 0.0% |
| gpt-5.4 | 0/1 | 19/30 | 380 | 355 | 0 | 0.0% |
| kimi | 0/1 | 0/30 | 380 | 443 | 0 | 0.0% |
| kiro | 1/1 | 30/30 | 380 | 556 | 0 | 0.0% |
| kiro-translate | 1/1 | 30/30 | 380 | 315 | 0 | 0.0% |
| laertes | 1/1 | 30/30 | 380 | 1643 | 514 | 31.3% |

### P01_sphincs_plus

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| c2rust | 0/128 | 0/0 | 6611 | 4887 | 3571 | 73.1% |
| claude | 116/128 | 116/128 | 6611 | 4354 | 929 | 21.3% |
| claude-combined | 116/128 | 116/128 | 6611 | 5083 | 1709 | 33.6% |
| gemini-3.1-pro-preview | 0/128 | 0/0 | 6611 | 1192 | 34 | 2.9% |
| gpt-5.4 | 0/128 | 0/0 | 6611 | 816 | 4 | 0.5% |
| kimi | 0/1 | 0/0 | 6611 | 0 | 0 | N/A |
| kiro | 128/128 | 128/128 | 6611 | 4948 | 3187 | 64.4% |
| kiro-translate | 36/128 | 36/128 | 6611 | 4921 | 3181 | 64.6% |
| laertes | 0/128 | 0/0 | 6611 | 4887 | 3571 | 73.1% |

### Summary: Cases Passed

| Battery | c2rust | claude | claude-combined | gemini-3.1-pro-preview | gpt-5.4 | kimi | kiro | kiro-translate | laertes |
|---------|------|------|------|------|------|------|------|------|------|
| B01_organic | 38/38 | 38/38 | 38/38 | 35/38 | 32/38 | 30/38 | 38/38 | 38/38 | 37/38 |
| B01_synthetic | 85/85 | 85/85 | 83/85 | 75/85 | 71/85 | 56/85 | 85/85 | 83/85 | 83/85 |
| B02_organic | 42/44 | 43/44 | 42/44 | 22/44 | 25/44 | 16/44 | 42/44 | 41/44 | 41/44 |
| B02_synthetic | 38/42 | 35/42 | 36/42 | 23/42 | 25/42 | 16/42 | 31/42 | 31/42 | 39/42 |
| P00_perlin_noise | 1/1 | 1/1 | 1/1 | 0/1 | 0/1 | 0/1 | 1/1 | 1/1 | 1/1 |
| P01_sphincs_plus | 0/128 | 116/128 | 116/128 | 0/128 | 0/128 | 0/1 | 128/128 | 36/128 | 0/128 |

### CRUST (test repair)

Results for the default modality: the agent is given the benchmark's
ground-truth tests (the paper's *test repair* setting).

| Agent | Projects Passed | Adjusted* | Tests Passed | LOC | Unsafe Lines | Unsafe % |
|-------|----------------|-----------|-------------|-----|-------------|----------|
| claude | 84/94 | 81/89 | 584/622 | 49757 | 209 | 0.4% |
| claude-combined | 88/95 | 85/90 | 646/654 | 55300 | 910 | 1.6% |
| kiro | 86/95 | 86/90 | 616/632 | 56466 | 536 | 0.9% |

### CRUST-blind (self-generated tests)

Results for the `--blind` modality: the benchmark's ground-truth tests
are withheld and the agent verifies against tests it generates itself
(the paper's *self-generated tests* setting). The `## CRUST` table above
is the *test repair* setting, where the agent sees the ground-truth tests.

| Agent | Projects Passed | Adjusted* | Tests Passed | LOC | Unsafe Lines | Unsafe % |
|-------|----------------|-----------|-------------|-----|-------------|----------|
| claude | 60/95 | 59/90 | 462/500 | 59989 | 412 | 0.7% |
| claude-combined | 61/95 | 60/90 | 454/514 | 55369 | 584 | 1.1% |
| kiro | 59/94 | 58/89 | 443/478 | 46669 | 466 | 1.0% |
| kiro-translate | 54/95 | 53/90 | 410/454 | 47395 | 466 | 1.0% |
