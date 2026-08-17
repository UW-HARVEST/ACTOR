// seal() consumes the Scrubbed, so a second seal cannot re-digest a tree the first
// seal's caller may already have published. `scrub`'s edge is pinned separately; without
// this case the seal edge of the same chain is asserted by nothing.
fn proof() -> &'static harvest_tools::domain::health::Completed {
    // Never runs: the file must fail to compile, and only the token's *type* is needed.
    unreachable!()
}

fn main() {
    let case = std::path::Path::new("/nonexistent");
    let sealed = harvest_tools::artifact::Sealed::<harvest_tools::artifact::Translate>::adopt(case).unwrap();
    let scratch = harvest_tools::artifact::Scratch::new("t-").unwrap();
    let work: harvest_tools::artifact::WorkTree<harvest_tools::artifact::Verify> =
        sealed.materialise_into(scratch).unwrap();
    let c_before = work.c().snapshot().unwrap();
    let scrubbed = work.scrub().unwrap();
    let _sealed = scrubbed.seal(proof(), &c_before).unwrap();
    let _ = scrubbed.rewritten();
}
