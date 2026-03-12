# harvest-agentic

Agentic C-to-Rust translation using [kiro-cli](https://github.com/aws/kiro-cli) for DARPA TRACTOR.

## Repository Structure

```text
scripts/
├── kiro-translate.sh       # Translation harness
└── prompts/
    ├── executable.md       # Prompt for executable cases
    └── library.md          # Prompt for library cases
test-corpus/                # MIT Test-Corpus (submodule)
results/                    # kiro-cli translation results (submodule)
```

## Setup

```bash
git submodule update --init
```

## Translate

```bash
# Translate a full battery
./scripts/kiro-translate.sh B01_synthetic

# Translate a single case
./scripts/kiro-translate.sh B01_synthetic/001_helloworld

# Filter by regex
./scripts/kiro-translate.sh B01_organic --filter "hex2bin_lib$"
```

The script is resume-friendly — it skips already-completed cases.

## Test

Testing uses MIT's `runtests` from the Test-Corpus:

```bash
cd test-corpus/deployment/scripts/github-actions
PYTHONPATH=. python3 -m runtests.rust \
  --root ../../../../results/B01_synthetic \
  --subset ../../../../results/B01_synthetic \
  --keep-going
```

## Results

| Battery | Builds | Tests (no UB) |
|---|---|---|
| B01_synthetic | 83/83 (100%) | 390/412 (94.7%) |
| B01_organic | 38/38 (100%) | 803/808 (99.4%) |
| B02_synthetic | 42/42 (100%) | 969/1053 (92.0%) |
| B02_organic | 42/43 (97.7%) | 260/267 (97.4%) |
