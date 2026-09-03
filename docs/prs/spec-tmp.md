# PR — Stop the test suite writing to /tmp, and stop it leaking what it writes

## What happened

`/tmp` (a 16 GB tmpfs, `nr_inodes=1048576`) hit its **inode** cap with ~2 GB of bytes
still free. Every process on the box then failed to create a file, including the tooling
needed to clean it up. Measured cause: **24,707 leaked `tempfile` directories**, each
containing a `results/.cache/2/...` tree.

Two independent defects produced that.

### Defect 1 — the suite writes to /tmp at all

Production code already avoids it: `workdir::tempdir` resolves through `workdir::base()`,
which refuses tmpfs and puts agent work trees under `$HOME/.harvest/work`. But tests call
`tempfile::tempdir()` directly, which honours `TMPDIR` and so lands in `/tmp`. A single
suite run creates dozens; the suite gets run thousands of times.

### Defect 2 — the leaked directories cannot be deleted

The cache store deliberately chmods stored entries read-only, so nothing can mutate a
published artifact. `tempfile::TempDir`'s `Drop` does a plain recursive delete, which fails
on a read-only file inside a read-only directory — and `Drop` cannot report an error, so it
fails **silently**. The directory survives forever. Reproducing by hand:

```
rm: cannot remove '/tmp/.tmpXXXXXX/results/.cache/2/verified/claude/<hash>/meta.json': Permission denied
```

`chmod -R u+w` first, then delete, works. So every cache test that writes an entry leaks
its tempdir permanently, on every machine, including CI.

## Required fixes

### 1. Point the suite's temp directory at disk

Add `tools/.cargo/config.toml`:

```toml
[env]
TMPDIR = { value = "target/tmp", relative = true }
```

`relative = true` resolves against the package root, so this is machine-independent — no
absolute path, nothing for `tools/check_paths.py` to flag — and it lands inside `target/`,
which is already ignored and cleared by `cargo clean`.

Cargo does not create the directory. Create it in `tools/build.rs` (which already exists
for the vergen stamp) so it is present before any test runs. Keep that addition small and
comment *why*, because a build script creating a directory is otherwise surprising.

Verify it works: run the suite and confirm no new `/tmp/.tmp*` directories appear, and that
`target/tmp/` fills instead.

### 2. Make cleanup survive read-only entries

Anything in the tests that creates a tempdir the cache store may write into must clean up
even when entries are read-only. Choose the mechanism, but:

* It must not stop the store making entries read-only. That is a real invariant — the point
  is that a published artifact cannot be mutated — and tests must exercise the real
  behaviour.
* It must not silently ignore a cleanup failure. Silence in `Drop` is what made this
  invisible for so long.
* Prefer one shared helper over per-test cleanup, so a new cache test cannot forget.

Find every test that lets the store write into a tempdir; there are several in `cache.rs`
and at least one in `test.rs`. Report the full list.

### 3. Fix the one test that depends on ambient /tmp being tmpfs

`workdir::tests::tmpfs_base_is_refused_with_an_actionable_message` calls
`tempfile::tempdir()` and relies on the result being under `/tmp` so the `MOUNTS` fixture
classifies it as tmpfs. Its own comment says so. Once `TMPDIR` points at disk this test
stops testing what it claims.

It does not need a real tmpfs directory: `resolve_from` already takes the mount table as a
parameter, so the test controls what counts as tmpfs. Rewrite it to assert the refusal
using a path the `MOUNTS` fixture declares tmpfs, without depending on where the ambient
temp directory happens to be — and without creating anything under `/tmp`.

Check whether `resolve_from` refuses *before* creating the directory. If it creates first
and refuses second, that is a second finding worth reporting: a refusal that has already
written to the location it is refusing.

### 4. The ETXTBSY flake, already fixed but never compiled

Branch `fix-etxtbsy-flake` in `/local/home/scheschb/pr-auto-flake` holds an unbuilt fix for
a separate 1-in-97 flake (measured: 6 failures in 600 runs) in
`a_runner_that_errors_is_not_scored_from_the_file_it_left` and
`cache.rs::a_cli_version_must_be_observed_rather_than_assumed`. It writes the script to a
`.sh` data file, materialises the program with `cp`, then chmods by pathname, so this
process never holds a write fd on the exec'd inode for a sibling's `fork()` to inherit.

Bring that change into this PR and **compile and verify it**, since it has never been
built. Re-measure: at least 200 suite runs under contention with zero failures. Use the
new on-disk `TMPDIR` so the measurement does not repeat the inode exhaustion — and report
how many inodes `/tmp` gained or lost across the run as evidence.

## Constraints

- Do not weaken any gate, add `#[allow]`/`#[expect]`/`#[ignore]`, grow any ALLOWED list, or
  re-record any `.stderr`.
- Do not change what any test asserts.
- No absolute machine-specific path may enter a committed file.

## Acceptance criteria

On the pinned toolchain with `RUSTUP_TOOLCHAIN` unset:

```
cargo fmt --check                                        clean
cargo test  --locked --lib --bin harvest-tools           all pass
cargo test  --locked --test architecture                 all pass
cargo test  --locked --test compile_fail                 10 cases
cargo clippy --locked --all-targets                      0 warnings
cargo clippy --locked --lib --bins -- -D clippy::panic   0
cargo doc   --locked --no-deps                           clean
python3 tools/comment_budget.py --max 14                 exit 0  (after `git add -A`)
python3 tools/check_paths.py                             exit 0
HARVEST_GOLDEN_RESULTS=<repo>/results cargo test --locked --test integration artifact_fingerprint
```

Plus the two measurements that are the point of this PR:

- a full suite run creates **zero** new `/tmp/.tmp*` directories;
- 200+ contended suite runs with zero ETXTBSY failures, with the `/tmp` inode delta
  reported.
