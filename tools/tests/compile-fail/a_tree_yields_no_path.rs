//! Nothing runs in a sealed tree. `Command::current_dir` and `--target-dir` both take
//! `impl AsRef<Path>`, so "can obtain a path" IS "can execute here".
fn main() {
    let tree: harvest_tools::tree::Tree = unimplemented!();
    let _where = tree.path();
}
