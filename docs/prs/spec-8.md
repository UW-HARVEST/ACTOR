# PR 8 — Split `cache/` and `dataset/`, and break the last two cycles

## Goal

`CYCLE_BASELINE` is `["agents", "artifact", "battery", "cache", "cli"]`. This PR targets the
two remaining mutual pairs that involve `battery` and `cache`, and splits the god-module.

## The measured edges

```
battery.rs -> crate::cache::{AgentKey, Mode}
cache.rs   -> crate::battery::{TRANSLATED, VERIFIED, phase_dir}
```

That is the whole `battery ↔ cache` cycle: two items each way. `battery.rs` reaches into
`cache` only because `Paths` carries cache configuration, and `cache.rs` reaches into `battery`
only for phase-directory naming.

## The two cuts

**1. `cache_mode` comes off `Paths`.** `battery.rs:640` has
`pub cache_mode: crate::cache::Mode`, and `AgentKey` is the other reference. `Paths` is a
layout type — where things live — and cache policy is not layout. Thread the mode from the CLI
to the one place that opens the store instead. That removes `battery -> cache`.

**2. Phase-directory naming belongs where the phases are defined.** `cache.rs` needs
`TRANSLATED`, `VERIFIED` and `phase_dir`. `artifact::Phase` already owns `DIR` — PR 6 made
`KeyInputs.phase` derive from `P::DIR` for exactly this reason. Decide whether `cache` can take
its phase naming from the `Phase` trait it already depends on, which removes
`cache -> battery`. If some spelling genuinely cannot, say which and why.

Shrink `CYCLE_BASELINE` to whatever the rule prints. Quote the old and new membership.

## The split

`battery.rs` is ~1,180 lines and is referenced 138 times from 8 modules. It holds four
unrelated concepts:

| concept | goes to |
|---|---|
| case/battery/config discovery (`discover`, `Battery::discover`) | `dataset/discover.rs` |
| path layout (`Paths`, `phase_dir`, `case_dir`, `input_dir`) | `dataset/layout.rs` |
| harvest-bench project handling | `dataset/harvest_bench.rs` |
| `Credits` and `Usd` (`battery.rs:390`, `:396`) | `domain/money.rs` — they are pure newtypes |

`cache.rs` is ~1,700 lines and splits along an existing seam:

| concept | goes to |
|---|---|
| `KeyInputs`, `Recipe`, `normalise`, `prompt_digest`, the digest newtypes | `cache/key.rs` |
| `Store`, `obtain`, `load`, quarantine, `restore_log` | `cache/store.rs` |

**The digest newtypes must travel with the code that constructs them.** This has bitten three
times (see `docs/HANDOFF.md`). If `CacheKey` is built in `key.rs` and read in `store.rs`, they
can be split — but if splitting forces a `pub(crate)` constructor on any digest, stop and put
them together, because `digests_cannot_be_fabricated` exists to prevent exactly that.

## Constraints

- Pure moves: byte-identical apart from `use` lines and the module. Report anything beyond that.
- **No visibility may widen to make a move work.** If it must, the item is in the wrong layer —
  leave it and say so. Prove it by diffing every declaration's visibility against `origin/main`.
- `Credits`/`Usd` moving to `domain/` means the layer-purity rule applies to them; confirm they
  name no `std::fs`/`process`/`env`.
- `MIN_FILES` must equal the measured count minus 2, comment updated with the measurement.
- Rules that key on module paths (`no_public_path_escapes_the_artifact_modules`,
  `digests_cannot_be_fabricated`, `only_battery_defines_the_has_crate_predicate`,
  `the_digest_path_is_lossless`) will need repointing — `only_battery_defines...` names
  `battery` in its own title, so decide what it means after the split and say so.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`.

## Acceptance criteria

The nine gates (see `docs/HANDOFF.md`), plus:

- the golden fingerprint passing and not skipping — 40 digests;
- **the verify AND translate cache keys for fixed inputs unchanged**, measured. This PR moves
  the key-derivation code, so a silent key change would invalidate every entry on disk;
- `SCHEMA` unmoved, with evidence.

## Commit message

The two cuts and which edge each removed; old and new `CYCLE_BASELINE`; how `battery.rs` and
`cache.rs` were split and anything that stayed because moving it would have widened visibility;
whether the digest newtypes could be separated from their constructors; both cache keys
unchanged; 40 golden digests unchanged.
