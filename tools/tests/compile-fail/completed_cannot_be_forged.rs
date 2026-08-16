// The proof token has a private field, so an infra-failed run cannot be sealed:
// only `Health::completed()` can mint one.
fn main() {
    let _forged = harvest_tools::domain::health::Completed(());
}
