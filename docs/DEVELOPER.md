# Developer Documentation for ACTOR

This file comprises documentation for ACTOR developers.
Follow the instructions in [Command-line tools](#command-line-tools) to obtain the ACTOR binary.
Separate instructions exist for [Evaluation benchmarks and results](#evaluation-benchmarks-and-results)

## Requirements

ACTOR cannot be run in a sandboxed environment on macOS because there are no macOS versions of the
  dependencies used to sandbox an agent (see below).
ACTOR can be run in unsandboxed mode with the `--allow-unsandboxed` flag.

- [Rust and the `Cargo` package manager](https://rust-lang.org/tools/install/), including the
  `clippy` component (`rust-toolchain.toml` pins it): scoring lints every crate it grades, and
  `test` refuses at preflight if `cargo clippy` is unavailable
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

## Configuring Different Models for Claude Code

To run translation with the non-default model for Claude Code ([configured here](../tools/src/agents/invocation.rs)),
    run the following command:

```sh
% HARVEST_CLAUDE_MODEL=claude-sonnet-5 harvest-tools --agent claude translate <TARGET>
```

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

## FAQs

> I can't delete the `.cache` folder that's generated during translation!

When ACTOR generates a cache,
  it unsets the write bit.
Even if you appear to have the permissions to modify a folder that it generates
  (check with `ls -la`),
  you may be barred from certain operations.

Run the following command to reset write permissions:

```sh
% chmod -R u+w <FOLDER_PATH>
```

## License

Copyright 2026 HARVEST Developers. See [LICENSE](../LICENSE).
