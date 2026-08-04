# Developer Documentation for ACTOR

This file comprises documentation for ACTOR developers.

## Requirements

- [Rust and the `Cargo` package manager](https://rust-lang.org/tools/install/)

## Setup

From the root of `ACTOR`, run:

```sh
cd tools && cargo build --release
```

This builds the binary: `ACTOR/tools/target/release/harvest-tools`.
To make the binary executable from any directory, run:

```sh
cd tools && cargo install --path .
```

The `harvest-tools` binary should now be executable from any directory.

One-shot LLM agents require API keys:
- `--agent kimi`: AWS Bedrock access (account `121913092579` via `ada-auth`)
- `--agent oneshot`: `OPENROUTER_API_KEY` environment variable

## License

Copyright 2026 HARVEST Developers. See [LICENSE](../LICENSE).
