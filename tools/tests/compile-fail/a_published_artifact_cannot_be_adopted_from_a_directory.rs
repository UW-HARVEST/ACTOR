// `Sealed::adopt` was `pub` and proofless: from any directory it manufactured the type whose whole
// invariant is that an infra-failed run cannot become one, and it was verify's only seed. Both doors
// are shut from outside the crate: `adopt` is gone, and `Published`'s one path-taking mint,
// `unkeyed_from_phase_dir` — the item this PR moved out of `#[cfg(test)]` into production — is
// `pub(crate)`, so E0624 below disappears the moment it is widened.
type Sealed = harvest_tools::artifact::Sealed<harvest_tools::artifact::Translate>;
type Published = harvest_tools::artifact::Published<harvest_tools::artifact::Translate>;

fn main() {
    let case = std::path::Path::new("/nonexistent");
    let _sealed = Sealed::adopt(case);
    let _published = Published::unkeyed_from_phase_dir(case);
}
