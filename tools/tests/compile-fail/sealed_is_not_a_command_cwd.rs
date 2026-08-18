// The concrete danger: handing a sealed artifact to a subprocess as its cwd.
// `current_dir` takes `impl AsRef<Path>`, which `Sealed` deliberately does not impl.
fn sealed() -> harvest_tools::artifact::Sealed<harvest_tools::artifact::Translate> {
    unreachable!()
}

fn main() {
    let _ = std::process::Command::new("cargo").current_dir(&sealed());
}
