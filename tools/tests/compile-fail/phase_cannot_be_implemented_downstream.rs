// Phase is a sealed trait: no phase can be defined outside artifact.rs, so every
// phase-dependent constant lives in one place and cannot drift.
struct Sideways;
impl harvest_tools::artifact::Phase for Sideways {
    const DIR: &'static str = "sideways";
}
fn main() {}
