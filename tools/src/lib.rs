//! Every module lives here, and `main.rs` consumes this crate rather than
//! re-declaring them.
//!
//! That distinction is load-bearing. Until 2026-08-14 `main.rs` re-declared 14 of
//! these modules with `mod`, so ~9,500 lines were compiled TWICE — once into the lib
//! and once into the bin. Three consequences, none of them cosmetic:
//!
//! * **Phantom dead code.** Anything used only by `tests/integration.rs` (a separate
//!   crate that links the lib) looked unused from the bin's private copy. That is why
//!   `all_case_names` and `defined_features` were reported dead while having three and
//!   one real callers respectively — and why a `-D warnings` gate looked unreachable.
//! * **Multiplied warnings.** One source line emitted a warning per compilation, so
//!   `--all-targets` reported ~357 where only 19 distinct issues existed. The gap was
//!   multiplicity, not debt.
//! * **Two copies of every `static`.** `workdir::BASE` and the agent-exit thread-locals
//!   existed once per compilation, so the integration tests exercised a different
//!   instantiation of the code than the binary ran.
//!
//! Adding `mod foo;` to `main.rs` reintroduces all three. Add it here instead.

pub mod agent_health;
pub mod artifact;
pub mod cache;
pub mod provenance;
pub mod report;
pub mod battery;
pub mod benchmark;
pub mod cargo_toml;
pub mod cli;
pub mod opencode;
pub mod sandbox;
pub mod scoring;
pub mod test;
pub mod translate;
pub mod verify;
pub mod workdir;
