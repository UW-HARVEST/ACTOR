// scrub() consumes the WorkTree, so the agent cannot run again against a tree that
// has been normalised for hashing. Without this the digest would cover one state and
// the artifact would be another.
fn main() {
    let case = std::path::Path::new("/nonexistent");
    let sealed = harvest_tools::artifact::Sealed::<harvest_tools::artifact::Translate>::adopt(case).unwrap();
    let scratch = harvest_tools::artifact::Scratch::new("t-").unwrap();
    let work: harvest_tools::artifact::WorkTree<harvest_tools::artifact::Verify> =
        sealed.materialise_into(scratch).unwrap();
    let _scrubbed = work.scrub().unwrap();
    let _ = work.path();
}
