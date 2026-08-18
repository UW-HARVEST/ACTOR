// Unconstrained, this compiled and `seal` + `publish` wrote verify output into
// `translated/`. `SeededBy` declares only the two real transitions.
fn published<P: harvest_tools::artifact::Phase>() -> harvest_tools::artifact::Published<P> {
    unreachable!()
}

fn main() {
    let verified = published::<harvest_tools::artifact::Verify>();
    let scratch = harvest_tools::artifact::Scratch::new("t-").unwrap();
    let _wrong: harvest_tools::artifact::WorkTree<harvest_tools::artifact::Translate> =
        verified.materialise_into(scratch).unwrap();
}
