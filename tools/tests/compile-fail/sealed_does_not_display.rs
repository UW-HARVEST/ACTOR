// Debug is hand-written to print the digest, not the location. Display would put the
// path back in reach of anything that formats it into a command.
fn sealed() -> harvest_tools::artifact::Sealed<harvest_tools::artifact::Translate> {
    unreachable!()
}

fn main() {
    let sealed = sealed();
    println!("{sealed}");
}
