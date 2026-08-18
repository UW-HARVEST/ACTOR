// materialise_at takes a ScratchPath and nothing else, so the copy a case is scored in
// cannot be the phase dir being scored: the build writes target/ and Cargo.lock, and
// Cargo.lock is hashed, so building in place changes the artifact's own digest.
fn published() -> harvest_tools::artifact::Published<harvest_tools::artifact::Translate> {
    unreachable!()
}

fn main() {
    let case = std::path::Path::new("/nonexistent");
    let work: harvest_tools::artifact::WorkTree<harvest_tools::artifact::Verify> = published()
        .materialise_at(harvest_tools::battery::phase_dir(case, harvest_tools::battery::VERIFIED))
        .unwrap();
    let _ = work.path();
}
