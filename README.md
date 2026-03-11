# HARVEST code

A place to put HARVEST code that has not yet been migrated into its own
repository.

## Building the Rust code

If you have [rustup](https://rustup.rs) installed, you can build the code by
running:

```bash
cargo build --release
```

If you do not use rustup, you will need a sufficiently-new stable Rust compiler
(see rust-toolchain.toml for a toolchain version that is known to work).

## LLM server

You will also need an LLM server. This can be local, or remote. A couple options
are given below:

### Local Ollama instance

You can follow Ollama's [download instructions](https://ollama.com/download), or
download its [Docker image](https://hub.docker.com/r/ollama/ollama).

Once you have it installed, you need to download a model. By default,
harvest_translate uses `codellama:7b`:

```bash
ollama pull codellama:7b                       # If installed in your system
docker container run ollama pull codellama:7b  # If using Docker
```

You will need to have Ollama running to run harvest_translate.

### Remote OpenAI instance

First, you'll need to provision an [OpenAI API key](https://platform.openai.com/api-keys).

Then, you'll need to set up a custom Harvest config file:
```toml 
[tools.raw_source_to_cargo_llm]
backend = "openai"
model = "gpt-4o"
api_key = "your_key_here" # Will be read from environment if empty
address = ""  # Not needed for OpenAI
max_tokens = 16384
```
You should place this config at the OS-dependent harvest config location, which you can find by running:
```bash
cargo run -- --print-config-path
``` 


## Running

Some of the examples below assume you have a local copy of the TRACTOR
Test-Corpus repository in `export TEST_CORPUS_PATH=/path/to/test-corpus`.

### Translate C code to Rust
```bash
cargo run --bin=translate --release -- /path/to/c/code -o /path/to/output
```

#### Test-Corpus Example:

```bash
cargo run --bin=translate --release -- $TEST_CORPUS_PATH/Public-Tests/B01_synthetic/001_helloworld/test_case/ -o example_output/
```

### Running a set of TRACTOR benchmarks

```bash
cargo run --bin=benchmark --release -- /path/to/input/dir /path/to/output/dir
```

#### Example: run all benchmarks

```bash
cargo run --bin=benchmark --release -- $TEST_CORPUS_PATH/Public-Tests/B01_synthetic example_output/
```

_Optional: add --filter=<regex> to keep only matching benchmarks (by directory name)_

#### Example: run only library benchmarks (directories ending with \_lib)

```bash
cargo run --bin=benchmark --release -- $TEST_CORPUS_PATH/Public-Tests/B01_synthetic example_output/ --filter=".*_lib$"
```

#### Example: run only benchmarks starting with B01

```bash
cargo run --bin=benchmark --release -- $TEST_CORPUS_PATH/Public-Tests example_output/ --filter="^B01"
```

_Optional: add --exclude=<regex> to exclude matching benchmarks (by directory name)_
_Note: --filter and --exclude are mutually exclusive_

### Example: exclude library benchmarks (directories ending with \_lib)

```bash
cargo run --bin=benchmark --release -- $TEST_CORPUS_PATH/Public-Tests/B01_synthetic example_output/ --exclude=".*_lib$"
```

#### Example: exclude benchmarks starting with test\_

```
cargo run --bin=benchmark --release -- $TEST_CORPUS_PATH/Public-Tests example_output/ --exclude="^test_"
```

### Testing pre-translated results with `harvest-test`

`harvest-test` is a standalone test runner that evaluates translated Rust code
against the TRACTOR test corpus. It is decoupled from translation — you can use
it to test output from any translator (HARVEST, kiro-cli, etc.).

#### Build

```bash
cargo build --release --bin=harvest-test
```

#### Usage

```bash
./target/release/harvest-test <CORPUS_DIR> <RESULTS_DIR> [OPTIONS]
```

- `CORPUS_DIR` — a battery directory in Test-Corpus (e.g. `Public-Tests/B01_organic`)
- `RESULTS_DIR` — directory of translated Rust projects, one subdirectory per case with `Cargo.toml` + `src/` at the root

**Options:**

| Flag | Description |
|---|---|
| `--filter <regex>` | Only run cases matching the pattern |
| `--exclude <regex>` | Skip cases matching the pattern |
| `--timeout <secs>` | Per-test-vector timeout (default: 10) |

#### Examples

```bash
export TEST_CORPUS_PATH=/path/to/Test-Corpus

# Test HARVEST e2e results on B01_organic
./target/release/harvest-test \
  $TEST_CORPUS_PATH/Public-Tests/B01_organic \
  /path/to/harvest-translate-results/3_2_results/B01_organic/B01_organic_e2e_codex

# Executables only (skip library cases)
./target/release/harvest-test \
  $TEST_CORPUS_PATH/Public-Tests/B01_synthetic \
  /path/to/results/B01_synthetic \
  --exclude ".*_lib$"

# Single case
./target/release/harvest-test \
  $TEST_CORPUS_PATH/Public-Tests/B01_synthetic \
  /path/to/results/B01_synthetic \
  --filter "001_helloworld$"
```

#### How it works

1. **Discover** — walks the results directory, finds cases with `Cargo.toml`, matches them with corpus test vectors
2. **Build** — runs `cargo build --release` for each case in parallel
3. **Prepare libs** — for library cases: copies runners from the corpus, sets up a shared cando2 dependency, ensures `crate-type = ["cdylib"]`, and builds the runner workspace
4. **Test** — executes tests in parallel and writes `results.csv`

#### Expected results directory layout

```
results_dir/
├── 001_helloworld/
│   ├── Cargo.toml
│   └── src/main.rs
├── some_lib/
│   ├── Cargo.toml
│   └── src/lib.rs
```

Each case directory must contain `Cargo.toml` at its root (flat layout, not nested in a subdirectory).

### Configuration

Print config file location:
```bash
cargo run --bin=translate -- --print-config-path
```

You can find more information on configuration in [doc/Configuration.md].
