# Developer Documentation for ACTOR

This file comprises documentation for ACTOR developers.
Follow the instructions in [Command-line tools](#command-line-tools) to obtain the ACTOR binary.
Separate instructions exist for [Evaluation benchmarks and results](#evaluation-benchmarks-and-results)

## Requirements

- [Rust and the `Cargo` package manager](https://rust-lang.org/tools/install/)

## Command-line Tools

From the root of `ACTOR`, run:

```sh
cd tools && cargo build --release
```

This builds the binary: `ACTOR/tools/target/release/harvest-tools`.
Run:

```sh
cd tools && cargo install --path .
```

from the root of `ACTOR` to make the `harvest-tools` binary executable from any directory.

One-shot LLM agents require API keys:
- `--agent kimi`: AWS Bedrock access (account `121913092579` via `ada-auth`)
- `--agent oneshot`: `OPENROUTER_API_KEY` environment variable

## Evaluation Benchmarks and Results

From the root of `ACTOR`, run:

```sh
git submodule update --init --recursive
```

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

## License

Copyright 2026 HARVEST Developers. See [LICENSE](../LICENSE).
