// A translate artifact must not be usable where a verify artifact is required:
// that is what would let translate output be published into verified/.
fn takes_verify(_: &harvest_tools::artifact::Sealed<harvest_tools::artifact::Verify>) {}
fn main() {
    let case = std::path::Path::new("/tmp/x");
    let sealed = harvest_tools::artifact::Sealed::<harvest_tools::artifact::Translate>::adopt(case).unwrap();
    takes_verify(&sealed);
}
