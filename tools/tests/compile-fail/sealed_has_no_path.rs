// A sealed artifact must not yield a filesystem path: a path IS the capability to
// execute there, and nothing may run in a published artifact.
fn main() {
    let case = std::path::Path::new("/tmp/x");
    let sealed = harvest_tools::artifact::Sealed::<harvest_tools::artifact::Translate>::adopt(case).unwrap();
    let _ = sealed.path();
}
