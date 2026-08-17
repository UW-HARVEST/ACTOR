// `Oracle::Ungraded` was a public, field-less variant: a caller could seal a tree whose C
// reference it had never read, where a wrong-but-real digest refused loudly. Only the walk mints.
fn main() {
    let _forged = harvest_tools::artifact::Oracle;
}
