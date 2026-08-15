// Unconstrained, this compiled and `seal` + `publish` wrote verify output into
// `translated/`. `SeededBy` declares only the two real transitions.
fn main() {
    let case = std::path::Path::new("/nonexistent");
    let sealed = harvest_tools::artifact::Sealed::<harvest_tools::artifact::Verify>::adopt(case)
        .unwrap();
    let scratch = harvest_tools::artifact::Scratch::new("t-").unwrap();
    let _wrong: harvest_tools::artifact::WorkTree<harvest_tools::artifact::Translate> =
        sealed.materialise_into(scratch).unwrap();
}
