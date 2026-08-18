// A sealed artifact must not yield a filesystem path: a path IS the capability to
// execute there, and nothing may run in a published artifact.
fn sealed() -> harvest_tools::artifact::Sealed<harvest_tools::artifact::Translate> {
    // Never runs: only the value's *type* is needed. `Sealed::adopt` used to stand here.
    unreachable!()
}

fn main() {
    let _ = sealed().path();
}
