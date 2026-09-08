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

Each tool needs its own CLI on `PATH`, already logged in: `claude`, `codex` and
`kiro-cli`. A run refuses at preflight if one is missing rather than partway
through, so a missing login costs nothing.

## Configuring Different Models for Claude Code

To run with a non-default model for Claude Code ([resolved here](../tools/src/runners/mod.rs)),
    run the following command:

```sh
% HARVEST_CLAUDE_MODEL=claude-sonnet-5 harvest-tools --tool claude run <TARGET>
```

The model is part of the cache key, so changing it is a different invocation and
will not be served by entries earned under the previous one.

## Evaluation Benchmarks and Results

From the root of `ACTOR`, run:

```sh
git submodule update --init --recursive
```

## Usage

```bash
# The whole chain for one battery: every step the prompt variant declares, then score, then tables
harvest-tools --tool kiro run B01_synthetic

# A prefix of the chain -- one step is translate, with no verify
harvest-tools --tool kiro run --steps 1 B01_synthetic

# All three tools, three invocations in flight each
harvest-tools --tool claude,codex,kiro --parallel 3 run all

# Single case, and harvest-bench instead of Test-Corpus
harvest-tools --tool kiro run B01_synthetic/001_helloworld
harvest-tools --tool claude run HB

# Reproduce the published numbers from the cache: a miss refuses, so this cannot spend money
harvest-tools --tool claude --replay-only run all

# Both datasets, every tool, and diff the regenerated tables against the committed ones
TOOLS=claude,codex,kiro tools/reproduce.sh all

# Inspect the cache without touching an agent
harvest-tools cache stats
harvest-tools cache verify      # every stored tree must hash to the name it is filed under
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
