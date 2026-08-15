// seal() consumes the Scrubbed, so the tree that was hashed cannot be handed on a second
// time — a second seal would re-digest a tree the first seal's caller may already have
// published, and the two digests would silently describe different states.
//
// `scrub` is pinned separately; without this case the seal edge of the same typestate
// chain is asserted by nothing.
fn proof() -> &'static harvest_tools::agent_health::Completed {
    // Never runs: this file must fail to compile. The token's field is private, so a
    // trybuild case cannot construct one, and only its *type* is needed here.
    unreachable!()
}

fn main() {
    let case = std::path::Path::new("/nonexistent");
    let sealed = harvest_tools::artifact::Sealed::<harvest_tools::artifact::Translate>::adopt(case).unwrap();
    let scratch = harvest_tools::artifact::Scratch::new("t-").unwrap();
    let work: harvest_tools::artifact::WorkTree<harvest_tools::artifact::Verify> =
        sealed.materialise_into(scratch).unwrap();
    let scrubbed = work.scrub().unwrap();
    let c_before = sealed.digest();
    let _sealed = scrubbed.seal(proof(), c_before).unwrap();
    let _ = scrubbed.rewritten();
}
