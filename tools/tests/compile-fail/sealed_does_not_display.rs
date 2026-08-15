// Debug is hand-written to print the digest, not the location. Display would put the
// path back in reach of anything that formats it into a command.
fn main() {
    let case = std::path::Path::new("/nonexistent");
    let sealed = harvest_tools::artifact::Sealed::<harvest_tools::artifact::Translate>::adopt(case).unwrap();
    println!("{sealed}");
}
