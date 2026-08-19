//! Declare every module here, never with `mod` in `main.rs`: that compiles it twice,
//! once into the lib and once into the bin, which duplicates every warning, reports
//! code reached only from `tests/` as dead, and gives statics such as `io::workdir::BASE`
//! one instance per compilation.

pub mod agent_health;
pub mod agents;
pub mod analyse;
pub mod artifact;
pub mod battery;
pub mod benchmark;
pub mod cache;
pub mod cli;
pub mod domain;
pub mod eval;
pub mod io;
pub mod oracle;
pub mod provenance;
pub mod refusal;
pub mod translate;
pub mod verify;
