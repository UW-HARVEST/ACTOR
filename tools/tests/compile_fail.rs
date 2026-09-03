//! The invariants in `crate::artifact` are enforced by ABSENCE — `Sealed` has no
//! `path()`, no `AsRef<Path>`, and `seal` demands a proof token. A runtime test
//! cannot express "this must not compile", so these do.
//!
//! If any of these files starts compiling, an invariant has been lost.

#[test]
fn invariants_do_not_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile-fail/*.rs");
}
