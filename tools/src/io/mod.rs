//! The layer that touches the outside world, and the complement of `domain/`: naming
//! `std::fs`, `std::process` or `std::env`, or shelling out to git, belongs here, and
//! everything inward is handed the result instead.
//!
//! No rule enforces that yet, and one written today would be false: `provenance.rs` still
//! shells out to git, because moving that plumbing here needs a private item widened to
//! `pub(crate)`, and the widening is what this layering exists to avoid.

pub mod sandbox;
pub mod workdir;
