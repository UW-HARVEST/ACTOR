// A score could name a phase dir carrying no key instead of taking the run's own output: ~95% of them.
fn main() {
    let dir = std::path::Path::new("/nonexistent");
    let _source = harvest_tools::eval::Source::Archive(dir);
    let _found = harvest_tools::artifact::archived_artifacts(dir);
}
