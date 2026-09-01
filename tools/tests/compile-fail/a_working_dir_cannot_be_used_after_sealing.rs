//! `seal` consumes the working dir: restoring the C and scrubbing the per-run paths make the tree
//! ready to hash, and running in it again afterwards would produce bytes the digest does not
//! describe. Reachable code on purpose -- after a diverging `unimplemented!()` rustc skips the move
//! analysis this case exists to assert.
fn main() {
    let work =
        harvest_tools::tree::WorkDir::assemble(std::path::Path::new("c_src")).expect("assemble");
    let _sealed = work.seal();
    let _again = work.translation();
}
