# Caching translate through the same path as verify

## Why this is smaller than it looks

`translate_case_at` already builds `<tmp>/translated_rust/c_src/`. `WorkTree::crate_dir()`
is `root.join(TRANSLATED_RUST)` and `WorkTree::c()` is `crate_dir().join(C_ORACLE_DIR)`
— i.e. `<root>/translated_rust/c_src`. **The layout translate produces is already,
byte for byte, the layout the artifact typestate expects.** What is missing is not a
shape but three things: a way to *seed* a `WorkTree` from a corpus instead of a
`Sealed`, a completion proof for backends that emit no stream-json, and a driver both
phases call.

Also smaller than it looks: the 17-variant work-dir `match` at `translate.rs:895-948`
is a **single arm** — one setup for every agent. The real per-backend branching is the
invocation match at `:964`, which has 6 live arms and 5 `unreachable!()`.

## Why it is worth doing

Measured on the 2026-08-15 harvest-bench sweep (self-reported `total_cost_usd` from the
agent CLI):

| phase | invocations | cost | cached |
|---|---|---|---|
| translate | 7 | **$795.59** | no |
| verify | 7 | ~$970 (2 done at $276.89) | yes |

PR #74 scoped translate out on the grounds that "verify is ~92% of the available
saving". On harvest-bench the split is closer to **45/55**. The 92% figure is
consistent with Test-Corpus (345 small cases), not with HB (7 large projects at ~$114
per translate). Every future HB sweep currently re-pays that $796.

---

## The design

### 1. Seeding: one body, two entry points, no new impl on `Sealed`

`Sealed::materialise_into` and `materialise_at` already both route through one private
body (`artifact.rs:605`), which hardcodes the destination:

```rust
fn materialise<Q: Phase>(&self, root: PathBuf, keep: Scratch) -> Result<WorkTree<Q>> {
    copy_carrying(&self.root, &root.join(TRANSLATED_RUST), Carry::IntoWorkTree)?;
    ...
}
```

Verify's source belongs at the crate root; a corpus belongs at `<crate root>/c_src`.
Add one parameter — a named two-variant enum, never a `bool`:

```rust
/// Where a seed's contents land inside the work tree. Named, because the two
/// destinations are not interchangeable: a corpus at the crate root would present C
/// sources as the Rust crate, and a crate under `c_src/` would be graded as the oracle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SeedAt {
    /// The whole crate, as verify materialises `translated/`.
    CrateRoot,
    /// C source only, as translate seeds its oracle.
    COracle,
}

fn materialise<Q: Phase>(&self, root: PathBuf, keep: Scratch, at: SeedAt) -> Result<WorkTree<Q>>
```

The corpus gets its own type and its own public entry point. It is **not** a `Phase`
— see trap 3 below.

```rust
/// The C corpus an agent translates: an input, never an output.
///
/// Not a `Phase`, deliberately. A `Sealed<Corpus>` would inherit `publish(case_dir)`,
/// which clears everything but `logs/` under `case_dir/<DIR>` — with `DIR = "test_case"`
/// that deletes the experiment's input.
pub struct Corpus {
    c: CDir,
}

impl Corpus {
    /// The only constructor. `ensure!(dir.is_dir())` is what makes `CDir::digest`'s
    /// `sha256:absent` sentinel unreachable from a cache key.
    pub fn adopt(dir: &Path) -> Result<Self>;

    /// Digest of the corpus AS THE AGENT WILL SEE IT — computed through `CDir`, i.e.
    /// `|d| d != BuildOutput`, never through `digest_tree`. See trap 1.
    pub fn digest(&self) -> Result<TreeDigest>;

    pub fn materialise_into<Q: Phase>(&self, scratch: Scratch) -> Result<WorkTree<Q>>;
    pub fn materialise_at<Q: Phase>(&self, root: PathBuf, keep: Scratch) -> Result<WorkTree<Q>>;
}
```

Both `Corpus` entry points call the same `materialise` body with `SeedAt::COracle`;
both `Sealed` entry points call it with `SeedAt::CrateRoot`. One copy path, four doors.

`impl <AnyTrait> for Sealed<P>` is **not** used anywhere here — that would fail
`sealed_implements_only_debug`. A trait is why an enum/struct pair is used instead.

### 2. Pair the phases so an illegal transition will not compile

`Q` in `materialise_into<Q>` is currently unconstrained by `P`, so
`Sealed::<Verify>::adopt(c).materialise_into::<Translate>()` → `seal` → `publish`
writes verify output into `translated/`. Adding a third source multiplies the illegal
pairs. Fix with a sealed witness on the **marker types**, not on `Sealed` (so A1 is
untouched):

```rust
mod sealed_seed { pub trait Witness {} }

/// `S` may seed a work tree that will be sealed as `Self`.
pub trait SeededBy<S>: Phase + sealed_seed::Witness {
    const AT: SeedAt;
}

impl SeededBy<Corpus>          for Translate { const AT: SeedAt = SeedAt::COracle; }
impl SeededBy<Sealed<Translate>> for Verify   { const AT: SeedAt = SeedAt::CrateRoot; }
```

`SeedAt` then comes from the type rather than the call site, so it cannot be passed
wrongly. There is no `impl SeededBy<Sealed<Verify>> for Translate`, so that transition
is a compile error rather than a convention.

### 3. The one driver

The genuine duplication is orchestration, not types. One params struct — of its seven
values two are `&Path` and two are `&TreeDigest`, so passed positionally
`case_dir`/`log_path` and `input_tree`/`c_before` transpose with no type error — one
generic function, one `store.obtain` call in the whole crate:

```rust
/// Everything a cached agent phase needs, resolved BEFORE the agent starts so the key
/// can name it. A struct because `case_dir` and `log_path` are both `&Path` and
/// `input_tree` and `c_before` are both `&TreeDigest`: as positional parameters either
/// pair transposes with no type error, and a transposed `input_tree` is a wrong key.
pub struct PhaseRun<'a, P: Phase> {
    pub case_dir: &'a Path,
    pub work: WorkTree<P>,
    pub prompt: &'a str,
    pub log_path: &'a Path,
    pub inv: &'a Invocation,      // model + cli + session/command + policy_shape
    pub input_tree: &'a TreeDigest,
    pub c_before: &'a TreeDigest,
}

/// The ONE cached execution path. `compute` invokes the backend and returns the sealed
/// artifact, or `Ok(None)` for "nothing worth keeping" (API error, abort, non-compiling
/// crate) — the store keeps no entry for it, so a transient failure is never memoised
/// into a permanent one.
pub fn run_cached<P, F>(run: PhaseRun<'_, P>, store: &Store, compute: F) -> Result<Outcome<P>>
where
    P: Phase,
    F: FnOnce(WorkTree<P>) -> Result<Option<Produced<P>>>;
```

`verify_case` and `translate_case_at` each reduce to: resolve the invocation, seed the
work tree, render the prompt, call `run_cached`, done. Neither calls `store.obtain`,
neither publishes, neither writes metrics — the driver does all three, once.

`KeyInputs.phase` becomes derived (`P::DIR`) rather than a `&'static str` that can
disagree with `P` — this is task #37, and it is **key-preserving**: `<Verify as
Phase>::DIR == battery::VERIFIED == "verified"`, which is exactly what `verify.rs:485`
passes today. No `SCHEMA` bump.

`Store::restore_log` takes `&Obtained<P>` instead of `&CacheKey`, so `P` is inferred
and the key provably belongs to that entry — otherwise every call site needs a
turbofish once `entry_dir` needs `P::DIR`.

### 4. `Completed` for backends that emit no stream-json — the blocking dependency

`Scrubbed::seal` demands `&agent_health::Completed`, and `Health::completed()` is its
only constructor. `classify_log` returns `Infra{truncated}` for a c2rust cmake/cargo
log, a kimi/oneshot prose log and a laertes/c2saferrust docker log, and `Unknown` for a
kiro prose log. **So 9 of 16 translate backends cannot seal at all**, and the same
defect already means kiro's *verify* phase can never publish (`verify.rs:612-620`
always returns `Ok(None)`).

This is task **#38**, and it blocks everything here. It needs a second,
evidence-backed minting route keyed on what the backend actually produces — a tool's
exit status for the deterministic translators — not a relaxation of the proof.

**#38 lands first.** Without it this design cannot seal a single translate artifact.

---

## The six hard cases

**Shared-source groups — one key, not N.** The N followers are *derived* trees
(different default features, different `[lib]` name, `main.rs`/tests stripped by
`propagate_config_phase`), so they can never be copies of the stored artifact. Publish
the real case from `obtain`, then run the existing propagate loop — which already runs
on the skip path (`run_test_corpus:311-331`), so it is replay-safe as written. All N
configs' `test_case` are symlinks to the real case's, and the digest follows symlinks
to content, so the input digest is shared by construction. **Do not give followers
their own keys**: that would key N invocations that never happened.

**The four deterministic translators.** Deterministic does not mean "don't cache" — it
changes what the key must name. c2rust has an honest `CliVersion` (`c2rust --version`,
already probed at `preflight_check:795`) and no model, so it needs a sentinel
`ModelId` like the existing `KIRO_UNPINNED_MODEL`. Laertes and C2SaferRust are
identified today by a **mutable docker tag** (`laertes-ready`, `c2saferrust:latest`) —
a tag is not an identity; key the image ID. C2SaferRust is the one "tool" that is
actually LLM-driven (gpt-5.4 via Bedrock) and therefore where caching pays most; its
model, `BEDROCK_BASE_URL` and derived region belong in the key. Its `BEDROCK_API_KEY`
must **never** reach a digest or `meta.json` — it is passed as `-e BEDROCK_API_KEY=`
in the docker argv, and `cache.rs:296` already refuses to hash raw argv.

**`remove_dir_all(case_dir)`.** Delete it. Four copies exist
(`translate.rs:879, 1592, 1970, 2176`). `Sealed::publish` already does the safe
version — clear the phase dir, keep `logs/`. Wiping the case dir also destroys
`verified/`, `test_vectors/` and `runner/`. A hit that destroys the previous artifact
before republishing it is strictly worse than no cache.

**Log restore.** `translation.log` has two homes: `<case>/translated/logs/`
(`translate_case_at`) and `<case>/logs/` (oneshot/kimi/laertes/c2saferrust). Every
reader — `agent_health.rs:158`, `test.rs:220/666/1278` — looks only at the first, so
those four arms are **invisible to the infra gate today**. `restore_log` needs exactly
one answer, so unification forces this fix, and it will make the infra gate start
firing on arms it previously ignored.

**The provenance-blind skip check.** `has_crate(dir) == "Cargo.toml exists"` is what
made the relaunch skip all 7 projects. Once a phase is keyed, the store *is* the
correct skip check: a hit replays, a miss runs. Keep `has_crate` only as the
"something is published here" predicate it honestly is.

**`OPENSSL_DIR`.** #74 excluded it because it "can only influence `build_ok`, decided
in the test phase". That rationale is **false for both phases**: `session.rs:199-211`
sets it for claude, kiro *and* opencode in verify too, and the prompts tell the agent
to run `cargo build` and iterate, so it can change what the agent produces. Either key
it — normalised to a default/custom discriminant, never the raw path, which would make
every key machine-specific — or write a true reason. Keying it changes key composition
and therefore **requires a `SCHEMA` bump**, so land it while `results/.cache` is
still near-empty.

---

## PR sequence

Each step leaves `main` green and is independently reviewable.

1. **#38 — `Completed` for non-stream-json backends.** Blocking dependency; nothing
   else can seal. Test: every backend's log fixture yields a decidable `Health`.
2. **Delete the four `remove_dir_all(case_dir)` sites**, relying on `Sealed::publish`.
   Pure safety fix, no cache involvement. Test: publishing preserves `verified/`,
   `test_vectors/`, `runner/` and sibling `logs/`.
3. **One home for `translation.log`.** Expect the infra gate to start firing on the
   four previously-invisible arms — that is the point. Test: the log path is a function
   of phase, and every reader agrees.
4. **`SeedAt` + `Corpus` + `SeededBy`.** Types only, no behaviour change; verify keeps
   working through the same body. Tests: a compile-fail case proving
   `Sealed::<Verify>` cannot seed `Translate` (add it to A4's `expected` map with its
   pinned code in the same commit — `cases.len() == expected.len()` is asserted); an
   architecture rule that `Corpus` returns a `TreeDigest` and never a `Path`; a test
   that `Corpus::digest` goes through `CDir`, not `digest_tree`.
5. **Task #37 — derive `KeyInputs.phase` from `P::DIR`.** Verified key-preserving, no
   `SCHEMA` bump. Update `cache.rs:1051`'s `v.phase = "translate"` literal to
   `key::<Translate>() != key::<Verify>()`.
6. **`run_cached` + `PhaseRun`; port verify onto it.** Verify's behaviour must be
   bit-identical — same keys, same replays. Architecture rule: exactly one
   `store.obtain` call site in the crate.
7. **Port translate onto `run_cached`.** Collapse `dispatch_translate` and
   `dispatch_translate_shared` into one call parameterised by `PromptKind`. No
   architecture rule names either function any longer; PR 0 deleted the lists that did.
8. **`OPENSSL_DIR`** — key it or justify it, with the `SCHEMA` bump if keyed.

Steps 2 and 3 are worth doing even if the rest is never built.

---

## Traps that will bite

Ranked by how quietly they fail.

1. **The false-hit hazard.** Do **not** digest a corpus with `digest_tree` /
   `Sealed::adopt`. With the corpus as hash root, `is_ignored` (`artifact.rs:272-289`)
   drops every `*.bak`, `*.log`, `*.sha256` at any depth plus root `logs/`, `.claude/`
   and the 5 `ROOT_ONLY_IGNORED` json names — the identical files **are** hashed once
   seeded under `c_src/`. `doc/footer.html.bak` is real in 26 cases under `results/`,
   so two different corpora can share an input digest and **replay each other's
   translation**. Use `work.c().digest()` / `CDir`'s predicate.
2. **The translate prompt carries no case identity.** There is no placeholder
   substitution anywhere in `translate.rs` (verify substitutes three). So `input_tree`
   is the **only** per-case key component for translate. If the corpus digest is wrong
   or absent, every case in a battery collides on one key. This is why trap 1 is
   ranked first.
3. **`CDir::digest` fabricates `TreeDigest("sha256:absent")`** for a missing dir
   (`artifact.rs:480`) — the one `TreeDigest` not derived from bytes, invisible to A3.
   `Corpus::adopt`'s `is_dir()` check is what makes it unreachable.
4. **`agent_provenance` must be called exactly once per invocation** —
   `merge_agent_exit` *consumes* the thread-local exit. Build provenance inside
   `compute`, where verify does, never in the caller, or a replay steals the previous
   case's exit code.
5. **`Store::load` treats missing/unparsable `agent/run.json` as a MISS**, not as null
   provenance. Translate must always supply real provenance or every entry it writes is
   unservable — and the symptom is a cache that looks enabled and never hits.
6. **A5 `nothing_new_runs_inside_the_results_tree` has `KNOWN = 2` and one hit today.**
   Spelling the spawn as `.current_dir(work.crate_dir())` consumes the last slot; two
   such sites fail the build. Route through `session::ClaudeRun { cwd, .. }` as verify
   does and the rule never sees it.
7. **The `ALLOWED` lockstep tax is gone; do not go looking for it.** A7
   `no_function_takes_three_interchangeable_primitives` and A10
   `safety_gating_bools_are_named_enums` held shrink-only lists naming `translate_case`,
   `verify_case`, `dispatch_translate`, `post_process_independent`,
   `write_translation_metrics` and others, so every re-signaturing in the sequence above
   used to require deleting the matching line in the same commit. PR 0 deleted both
   rules. Nothing has to be kept in step — and nothing rejects those shapes either,
   which is trap 8.
8. **`write_translation_metrics` must not gain a `replayed: bool`.** It already takes
   `success: bool` (`translate.rs:1247`), so the two would be adjacent and
   interchangeable, and one transposed call site publishes a replay as a failure and a
   failure as a replay. Since PR 0 this is a convention and not a gate: the build no
   longer fails, so use a named enum because the transposition is real. Merge the two
   metrics writers (only verify's carries `replayed`/`cache_key`, which a replay must
   record so the original run's cost is not read as this run's spend).
9. **Post-processing currently runs on the *published* tree**, after the artifact
   boundary. `Cargo.toml` is hashed, so sealing before post-processing means the
   recorded digest does not describe `translated/` on disk and a replay restores the
   pre-processed crate. Move it inside the `WorkTree` before `scrub()`.
10. **Adding `c_before` at translate seeding time is a correctness fix that will start
    refusing historical cases** where the translate agent touched `c_src`. Translate has
    no oracle check today, and verify inherits translate's possibly-corrupted `c_src`
    as its own baseline. Expect `Refusal::OracleModified` on re-runs that previously
    passed.
11. **Routing translate through `Sealed` changes what `translated/` contains.**
    `Carry::FromArtifact` drops `target/` and `c_src/build/`, which today's
    `copy_dir_all` carries. Almost certainly desirable (9× the bytes, and
    `CMakeCache.txt` bakes the scratch path) but it is a visible change to an existing
    results tree.
12. **`obtain`'s `compute` is `FnOnce` and the work tree moves into it**, so on a hit
    the materialised tree is built and thrown away. Verify accepts this deliberately —
    the prompt embeds the work path, so the string hashed and the string shown must be
    identical. For translate the wasted setup is larger (corpus copy, sandbox settings,
    opencode config), and for the deterministic tools it is most of the phase.
13. **`CliVersion::probe` reads `HARVEST_CLI_VERSION` regardless of which program it
    probes** — one env var silently supplies the "version" of claude, c2rust, laertes
    and codex at once.
14. **`--agent laertes|c2saferrust|smartc2rust|kimi|oneshot translate HB/<p>` hits
    `unreachable!()`** today (`translate.rs:1038`), panics, is caught by
    `CaseResult::panicked` and reported as an ordinary ❌. The driver must return a
    typed "no translate phase for this agent/dataset", the way `verify_invocation`
    returns `Ok(None)`.
15. **Do not rename `classify`, `hash_tree`, `digest_tree`, `scrub` or `visit`'s caller
    set** — `the_digest_path_is_lossless` asserts those exact `(file, fn)` names and
    that `hash_tree` still calls `as_encoded_bytes`. Its failure message reads like a
    rule bug rather than your rename.
16. **A new `Phase` marker changes
    `tests/compile-fail/phase_cannot_be_implemented_downstream.stderr`**, which
    enumerates the sealed-trait impls. Re-record it **alone**, never with a blanket
    `TRYBUILD=overwrite`.
17. **`copy_carrying` chmods every file to `0o644`**, losing `+x`. Zero executables in
    today's corpus; the C-dataset projects ship them.
18. **`visit` follows symlinks via `is_dir()`**; a dangling symlink or non-regular file
    in a corpus is a hard error in `hash_tree`/`copy_carrying` where today's
    `copy_dir_all` silently skips it, and a symlink **loop** recurses forever.

## Not in scope

* **Caching the test phase.** It is ~350 s of a 19.5 h sweep. `build_ok` is decided
  there, which is why `ToolchainId` is in the key at all.
* **Laertes/C2SaferRust input provenance.** Their input is reached by path surgery into
  a sibling agent's results tree (`results_dir.parent()/c2rust/<battery>/<name>`) with
  no digest, so the key cannot name *which* c2rust output was consumed and re-running
  c2rust silently changes their input. Adopting that as a `Corpus` fixes it and should
  follow, but it is separable. Until then those two arms must be keyed as
  `Mode::Bypass`, not keyed wrongly — a wrong key is worse than no cache.
* **The dead second oracle check in `seal`** (`artifact.rs:499` duplicates `:507`, one
  hardcoding `"c_src"` and the other using `C_ORACLE_DIR`). Unreachable, and a second
  full hash of `c_src` on every seal — 341 files for libsodium, doubling again once
  translate seals. Worth deleting; not required here.
