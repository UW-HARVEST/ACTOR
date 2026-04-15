# ACTOR

**A**gentic **C**-**to**-**R**ust translation for [DARPA TRACTOR](https://www.darpa.mil/program/translating-all-c-to-rust).

This repository evaluates multiple translation agents — from mechanical transpilers to agentic LLM pipelines to one-shot LLM baselines — across MIT's TRACTOR Test-Corpus and CRUST-bench. All results are CI-validated with exact-match checks against stored summaries.

## Agents

| Agent | Type | Description |
|-------|------|-------------|
| **kiro** | Agentic | Multi-turn [kiro-cli](https://github.com/aws/kiro-cli) agent: translate → verify (C-as-oracle) → test |
| **kiro-translate** | Agentic (translate-only) | kiro's translation without the verify/repair loop |
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
├── kiro/                   # kiro + kiro-translate prompts
├── claude/                 # Claude Code prompts
└── oneshot/                # One-shot LLM prompts (kimi, gpt-5.4, gemini)
test-corpus/                # MIT TRACTOR Test-Corpus (submodule)
crust-bench/                # CRUST-bench dataset (submodule)
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

# CRUST-bench
harvest-tools --agent kiro run CRUST/all
harvest-tools --agent kiro run CRUST/all --blind  # blind mode (no ground-truth tests)
```

All commands are resume-friendly — they skip already-completed cases.

## Results

Auto-generated from validated `result.json` and `summary.json` files.
Regenerate with `harvest-tools report`.
Auto-generated from validated `result.json` and `summary.json` files.

## B01_organic

| Agent | Cases Passed | Vectors Passed | LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-----|-------------|----------|
| c2rust | 38/38 | 775/775 | 8974 | 2932 | 32.7% |
| gemini-3.1-pro-preview | 35/38 | 761/770 | 2186 | 498 | 22.8% |
| gpt-5.4 | 32/38 | 708/728 | 2575 | 511 | 19.8% |
| kimi | 30/38 | 728/755 | 2163 | 642 | 29.7% |
| kiro | 38/38 | 775/775 | 2554 | 969 | 37.9% |
| kiro-translate | 38/38 | 775/775 | 2505 | 954 | 38.1% |
| laertes | 37/38 | 775/775 | 9636 | 2618 | 27.2% |

## B01_synthetic

| Agent | Cases Passed | Vectors Passed | LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-----|-------------|----------|
| c2rust | 85/85 | 393/393 | 5214 | 2972 | 57.0% |
| claude | 1/1 | 3/3 | 3 | 0 | 0.0% |
| gemini-3.1-pro-preview | 75/85 | 368/379 | 2407 | 110 | 4.6% |
| gpt-5.4 | 71/85 | 368/383 | 2510 | 50 | 2.0% |
| kimi | 56/85 | 241/348 | 2413 | 204 | 8.5% |
| kiro | 85/85 | 393/393 | 4762 | 729 | 15.3% |
| kiro-translate | 83/85 | 391/393 | 2888 | 483 | 16.7% |
| laertes | 83/85 | 389/389 | 5970 | 2972 | 49.8% |

## B02_organic

| Agent | Cases Passed | Vectors Passed | LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-----|-------------|----------|
| c2rust | 42/44 | 254/254 | 44792 | 34150 | 76.2% |
| gemini-3.1-pro-preview | 22/44 | 125/170 | 10298 | 2661 | 25.8% |
| gpt-5.4 | 25/44 | 165/193 | 12823 | 2107 | 16.4% |
| kimi | 16/44 | 121/149 | 10911 | 2559 | 23.5% |
| kiro | 42/44 | 257/263 | 21796 | 14709 | 67.5% |
| kiro-translate | 41/44 | 254/261 | 18329 | 12176 | 66.4% |
| laertes | 41/44 | 245/245 | 44821 | 32559 | 72.6% |

## B02_synthetic

| Agent | Cases Passed | Vectors Passed | LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-----|-------------|----------|
| c2rust | 38/42 | 988/989 | 21620 | 16365 | 75.7% |
| gemini-3.1-pro-preview | 23/42 | 712/843 | 8117 | 407 | 5.0% |
| gpt-5.4 | 25/42 | 770/907 | 9215 | 158 | 1.7% |
| kimi | 16/42 | 241/306 | 9311 | 891 | 9.6% |
| kiro | 31/42 | 942/1025 | 12634 | 3828 | 30.3% |
| kiro-translate | 31/42 | 942/1025 | 8420 | 1922 | 22.8% |
| laertes | 39/42 | 818/819 | 21664 | 15770 | 72.8% |

## P00_perlin_noise

| Agent | Cases Passed | Vectors Passed | LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-----|-------------|----------|
| c2rust | 1/1 | 30/30 | 1643 | 514 | 31.3% |
| claude | 1/1 | 30/30 | 377 | 0 | 0.0% |
| gemini-3.1-pro-preview | 0/1 | 2/30 | 271 | 0 | 0.0% |
| gpt-5.4 | 0/1 | 19/30 | 355 | 0 | 0.0% |
| kimi | 0/1 | 0/30 | 443 | 0 | 0.0% |
| kiro | 1/1 | 30/30 | 556 | 0 | 0.0% |
| kiro-translate | 1/1 | 30/30 | 315 | 0 | 0.0% |
| laertes | 1/1 | 30/30 | 1643 | 514 | 31.3% |

## P01_sphincs_plus

| Agent | Cases Passed | Vectors Passed | LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-----|-------------|----------|
| c2rust | 0/128 | 0/0 | 4887 | 3571 | 73.1% |
| claude | 115/128 | 115/128 | 4994 | 456 | 9.1% |
| gemini-3.1-pro-preview | 0/128 | 0/0 | 1192 | 34 | 2.9% |
| gpt-5.4 | 0/128 | 0/0 | 816 | 4 | 0.5% |
| kimi | 0/1 | 0/0 | 0 | 0 | N/A |
| kiro | 128/128 | 128/128 | 4948 | 3187 | 64.4% |
| kiro-translate | 36/128 | 36/128 | 4921 | 3181 | 64.6% |
| laertes | 0/128 | 0/0 | 4887 | 3571 | 73.1% |

## Summary: Cases Passed

| Battery | c2rust | claude | gemini-3.1-pro-preview | gpt-5.4 | kimi | kiro | kiro-translate | laertes |
|---------|------|------|------|------|------|------|------|------|
| B01_organic | 38/38 | — | 35/38 | 32/38 | 30/38 | 38/38 | 38/38 | 37/38 |
| B01_synthetic | 85/85 | 1/1 | 75/85 | 71/85 | 56/85 | 85/85 | 83/85 | 83/85 |
| B02_organic | 42/44 | — | 22/44 | 25/44 | 16/44 | 42/44 | 41/44 | 41/44 |
| B02_synthetic | 38/42 | — | 23/42 | 25/42 | 16/42 | 31/42 | 31/42 | 39/42 |
| P00_perlin_noise | 1/1 | 1/1 | 0/1 | 0/1 | 0/1 | 1/1 | 1/1 | 1/1 |
| P01_sphincs_plus | 0/128 | 115/128 | 0/128 | 0/128 | 0/1 | 128/128 | 36/128 | 0/128 |

## CRUST

| Agent | Projects Passed | Tests Passed | LOC | Unsafe Lines | Unsafe % |
|-------|----------------|-------------|-----|-------------|----------|
| kiro | 86/95 | 616/632 | 56466 | 536 | 0.9% |

## CRUST-blind

| Agent | Projects Passed | Tests Passed | LOC | Unsafe Lines | Unsafe % |
|-------|----------------|-------------|-----|-------------|----------|
| kiro | 59/94 | 443/478 | 46669 | 466 | 1.0% |
| kiro-translate | 54/95 | 410/454 | 47395 | 466 | 1.0% |

