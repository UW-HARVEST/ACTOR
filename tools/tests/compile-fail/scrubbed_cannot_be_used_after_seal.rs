// seal() consumes the Scrubbed, so a second seal cannot re-digest a tree the first
// seal's caller may already have published. `scrub`'s edge is pinned separately; without
// this case the seal edge of the same chain is asserted by nothing.
fn proof() -> &'static harvest_tools::domain::health::Completed {
    // Never runs: the file must fail to compile, and only the token's *type* is needed.
    unreachable!()
}

fn published() -> harvest_tools::artifact::Published<harvest_tools::artifact::Translate> {
    unreachable!()
}

fn main() {
    let scratch = harvest_tools::artifact::Scratch::new("t-").unwrap();
    let work: harvest_tools::artifact::WorkTree<harvest_tools::artifact::Verify> =
        published().materialise_into(scratch).unwrap();
    let c_before = work.c().snapshot().unwrap();
    let scrubbed = work.scrub().unwrap();
    let _sealed = scrubbed.seal(proof(), &c_before).unwrap();
    let _ = scrubbed.rewritten();
}
