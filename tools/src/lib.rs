//! Declare every module here, never with `mod` in `main.rs`: that compiles it twice,
//! once into the lib and once into the bin, which duplicates every warning, reports
//! code reached only from `tests/` as dead, and gives statics such as `workdir::BASE`
//! one instance per compilation.

pub mod agent_health;
pub mod artifact;
pub mod cache;
pub mod provenance;
pub mod refusal;
pub mod report;
pub mod battery;
pub mod benchmark;
pub mod cargo_toml;
pub mod cli;
pub mod opencode;
pub mod sandbox;
pub mod scoring;
pub mod session;
pub mod test;
pub mod translate;
pub mod verify;
pub mod workdir;
