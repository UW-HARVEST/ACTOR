// scrub() consumes the WorkTree, so the agent cannot run again against a tree that
// has been normalised for hashing. Without this the digest would cover one state and
// the artifact would be another.
fn published() -> harvest_tools::artifact::Published<harvest_tools::artifact::Translate> {
    unreachable!()
}

fn main() {
    let scratch = harvest_tools::artifact::Scratch::new("t-").unwrap();
    let work: harvest_tools::artifact::WorkTree<harvest_tools::artifact::Verify> =
        published().materialise_into(scratch).unwrap();
    let _scrubbed = work.scrub().unwrap();
    let _ = work.path();
}
