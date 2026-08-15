//! The pure layer. Nothing here may name `std::fs`, `std::process` or `std::env`:
//! a decision that would read a file, spawn something or look up a variable takes the
//! result as an argument instead, and the read happens in the layer above.
//! `nothing_in_the_pure_layer_names_the_filesystem_a_process_or_the_environment` in
//! `tests/architecture.rs` is what keeps that true rather than intended.

pub mod contents;
pub mod outcome;
pub mod relpath;
