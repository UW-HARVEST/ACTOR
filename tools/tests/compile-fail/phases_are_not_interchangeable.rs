// A translate artifact must not be usable where a verify artifact is required:
// that is what would let translate output be published into verified/.
fn takes_verify(_: &harvest_tools::artifact::Sealed<harvest_tools::artifact::Verify>) {}

fn sealed() -> harvest_tools::artifact::Sealed<harvest_tools::artifact::Translate> {
    unreachable!()
}

fn main() {
    takes_verify(&sealed());
}
