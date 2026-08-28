# Developer Documentation for ACTOR

This file comprises documentation for ACTOR developers.
Follow the instructions in [Command-line tools](#command-line-tools) to obtain the ACTOR binary.
Separate instructions exist for [Evaluation benchmarks and results](#evaluation-benchmarks-and-results)

## Requirements

ACTOR cannot be run in a sandboxed environment on macOS because there are no macOS versions of the
  dependencies used to sandbox an agent (see below).
ACTOR can be run in unsandboxed mode with the `--allow-unsandboxed` flag.

- [Rust and the `Cargo` package manager](https://rust-lang.org/tools/install/)
- For sandboxing:
  - [`socat`](https://linux.die.net/man/1/socat)
  - [bubblewrap (`bwrap`)](https://github.com/containers/bubblewrap)

## Command-line Tools

To build (but not install) the `harvest-tools` binary (which enables you to run ACTOR),
Run the following comand from the root of ACTOR:

```sh
cd tools && cargo build --release
```

which builds the binary: `ACTOR/tools/target/release/harvest-tools`.

To build **and** install the `harvest-tools` binary,
Run the following comand from the root of ACTOR:

```sh
cd tools && cargo install --path .
```

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

# Reproduce the published numbers from the cache: a miss refuses, so this cannot spend money
harvest-tools --agent claude --replay-only run all

# Single case
harvest-tools --agent kiro run B01_synthetic/001_helloworld
```

## License

Copyright 2026 HARVEST Developers. See [LICENSE](../LICENSE).
