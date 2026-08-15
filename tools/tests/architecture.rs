//! Shape rules over the source, for invariants enforced by ABSENCE.
//!
//! `Sealed<P>` stops anything executing in a published artifact by not implementing
//! `AsRef<Path>` and not having a `path()`. A trybuild test proves today's code
//! rejects `Command::current_dir(&sealed)`, but it cannot prove nobody *adds* the
//! impl tomorrow — at which point the trybuild case starts failing for a reason a
//! reader may well "fix" by re-recording it. These rules assert the shape instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use syn::visit::Visit;

fn src(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name)
}

fn parse(path: &Path) -> syn::File {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    syn::parse_file(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn rust_sources() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("src/")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    out.sort();
    out
}

fn type_name(ty: &syn::Type) -> String {
    use quote_min::ToText;
    ty.to_text()
}

/// Minimal type-to-string, so this test needs no `quote`/`prettyplease` dependency.
mod quote_min {
    pub trait ToText {
        fn to_text(&self) -> String;
    }
    impl ToText for syn::Type {
        fn to_text(&self) -> String {
            match self {
                syn::Type::Path(p) => p
                    .path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::"),
                syn::Type::Reference(r) => format!("&{}", r.elem.to_text()),
                syn::Type::Slice(s) => format!("[{}]", s.elem.to_text()),
                _ => String::new(), // only Type::Path is load-bearing below

            }
        }
    }
}

/// Does this type mention a filesystem path type anywhere in its spelling?
fn is_pathish(ty: &syn::Type) -> bool {
    let mut hit = false;
    struct V<'a>(&'a mut bool);
    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_path_segment(&mut self, s: &'ast syn::PathSegment) {
            if matches!(s.ident.to_string().as_str(), "Path" | "PathBuf" | "OsStr" | "OsString") {
                *self.0 = true;
            }
            syn::visit::visit_path_segment(self, s);
        }
    }
    V(&mut hit).visit_type(ty);
    hit
}

fn is_public(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_) | syn::Visibility::Restricted(_))
}

/// The type an `impl` block is for, by its last path segment. The rules below scan every
/// module because the orphan rule permits the impl to live in any of them — and outside
/// the defining module it must be spelled `crate::cache::CacheKey`, which no comparison
/// against `"CacheKey"` would ever match.
fn impl_target(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => {
            p.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default()
        }
        syn::Type::Reference(r) => impl_target(&r.elem),
        _ => String::new(),
    }
}

// ── A1 ─────────────────────────────────────────────────────────────────────

/// `Sealed<P>` may implement nothing that yields a path, directly or by deref.
///
/// Scans every module, not just `artifact.rs`: the orphan rule permits
/// `impl AsRef<Path> for Sealed<P>` anywhere in the crate.
#[test]
fn sealed_implements_only_debug() {
    const ALLOWED: &[&str] = &["Debug"];
    let mut found: Vec<(String, String)> = Vec::new();

    for path in rust_sources() {
        for item in parse(&path).items {
            let syn::Item::Impl(imp) = item else { continue };
            if impl_target(&imp.self_ty) != "Sealed" {
                continue;
            }
            let name = match &imp.trait_ {
                None => continue, // the inherent impl is where the API lives
                Some((_, p, _)) => p.segments.last().map(|s| s.ident.to_string()).unwrap_or_default(),
            };
            if !ALLOWED.contains(&name.as_str()) {
                let file = path.file_name().unwrap().to_string_lossy().into_owned();
                found.push((file, name));
            }
        }
    }
    assert!(
        found.is_empty(),
        "Sealed<P> gained trait impls beyond {ALLOWED:?}: {found:?}.\n\
         Nothing may run in a published artifact, and that holds only while Sealed\n\
         yields no path. If a new impl is genuinely needed, prove it cannot leak one\n\
         and add it to ALLOWED here."
    );
}

// ── A2 ─────────────────────────────────────────────────────────────────────

/// In the artifact and cache modules, no public item may hand out a path.
///
/// `WorkTree` is the deliberate exception: cargo and `Command` need a real
/// directory, so scratch is the one place a path is available. A trybuild test can
/// assert `Sealed` has no `path()` today; only a shape rule catches a *new*
/// accessor being added.
#[test]
fn no_public_path_escapes_the_artifact_modules() {
    const ALLOWED: &[(&str, &str)] = &[
        // Scratch and WorkTree are the deliberate exceptions: cargo and Command need a
        // real directory, and scratch is the one place that is allowed to be.
        ("WorkTree", "path"),
        ("WorkTree", "crate_dir"),
        ("Scratch", "path"),
        // RelPath is guaranteed relative with no `..`, so it names no location and
        // cannot be a working directory or a build target.
        ("RelPath", "as_path"),
    ];
    let mut leaks: Vec<String> = Vec::new();

    for module in ["artifact.rs", "cache.rs"] {
        let path = src(module);
        for item in parse(&path).items {
            match item {
                syn::Item::Impl(imp) => {
                    let self_ty = impl_target(&imp.self_ty);
                    for it in imp.items {
                        let syn::ImplItem::Fn(f) = it else { continue };
                        if !is_public(&f.vis) {
                            continue;
                        }
                        let syn::ReturnType::Type(_, ret) = &f.sig.output else { continue };
                        let name = f.sig.ident.to_string();
                        if is_pathish(ret)
                            && !ALLOWED.contains(&(self_ty.as_str(), name.as_str()))
                        {
                            leaks.push(format!("{module}: {self_ty}::{name} -> {}", type_name(ret)));
                        }
                    }
                }
                syn::Item::Struct(s) if is_public(&s.vis) => {
                    for f in s.fields {
                        if is_public(&f.vis) && is_pathish(&f.ty) {
                            let fname = f.ident.map(|i| i.to_string()).unwrap_or_else(|| "0".into());
                            leaks.push(format!("{module}: {}.{fname} is a public path field", s.ident));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "a path escapes the artifact modules: {leaks:#?}\n\
         A caller holding a path can run a command there. Return the data, or take a\n\
         destination, rather than handing out the location."
    );
}

// ── A3 ─────────────────────────────────────────────────────────────────────

/// Digest newtypes must be unforgeable: private field, no `From<String>`.
///
/// A digest that can be constructed from an arbitrary string is a digest that can
/// be wrong, and the cache compares them to decide whether to reuse an artifact.
///
/// Scans every module for the same reason A1 does: the orphan rule permits
/// `impl From<String> for CacheKey` in any file of the crate.
#[test]
fn digests_cannot_be_fabricated() {
    const GUARDED: &[&str] = &["TreeDigest", "PromptDigest", "RecipeDigest", "CacheKey", "ToolchainId"];
    let mut bad: Vec<String> = Vec::new();

    for path in rust_sources() {
        let module = path.file_name().unwrap().to_string_lossy().into_owned();
        for item in parse(&path).items {
            match &item {
                syn::Item::Struct(s) if GUARDED.contains(&s.ident.to_string().as_str()) => {
                    for f in &s.fields {
                        if is_public(&f.vis) {
                            bad.push(format!("{}: {} has a public field", module, s.ident));
                        }
                    }
                }
                syn::Item::Impl(imp) => {
                    let Some((_, tr, _)) = &imp.trait_ else { continue };
                    let is_from = tr.segments.last().is_some_and(|s| s.ident == "From");
                    let target = impl_target(&imp.self_ty);
                    if is_from && GUARDED.contains(&target.as_str()) {
                        bad.push(format!("{module}: impl From<..> for {target}"));
                    }
                }
                _ => {}
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a digest became forgeable: {bad:#?}\n\
         The only way to obtain one must be to hash something real."
    );
}

// ── A4: the anti-laundering rule ────────────────────────────────────────────

/// Every compile-fail case must still pin the error code it was written to assert.
///
/// `TRYBUILD=overwrite` re-records every `.stderr` from whatever the compiler now
/// says. Rename `Sealed::adopt` and run it, and all four cases go green while
/// asserting only "no function named adopt" — the AsRef, Deref and privacy
/// assertions silently cease to exist. Pinning the code is what makes that visible.
#[test]
fn compile_fail_cases_still_assert_what_they_were_written_for() {
    let expected: BTreeMap<&str, &str> = [
        ("sealed_has_no_path", "E0599"),                // no method named `path`
        ("sealed_is_not_a_command_cwd", "E0277"),        // AsRef<Path> not satisfied
        ("phases_are_not_interchangeable", "E0308"),     // mismatched types
        ("completed_cannot_be_forged", "E0603"),         // private constructor
        ("worktree_cannot_be_used_after_scrub", "E0382"), // scrub() consumed it
        ("phase_cannot_be_implemented_downstream", "E0277"), // sealed supertrait
        ("sealed_does_not_display", "E0277"),            // no Display impl
    ]
    .into_iter()
    .collect();

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compile-fail");
    let mut cases: Vec<String> = std::fs::read_dir(&dir)
        .expect("tests/compile-fail")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    cases.sort();

    for case in &cases {
        let want = expected.get(case.as_str()).unwrap_or_else(|| {
            panic!(
                "compile-fail case `{case}` has no pinned error code.\n\
                 Add one to `expected` here, or TRYBUILD=overwrite can silently\n\
                 repoint it at an unrelated error and it will still pass."
            )
        });
        let stderr = dir.join(format!("{case}.stderr"));
        let text = std::fs::read_to_string(&stderr)
            .unwrap_or_else(|e| panic!("{}: {e}", stderr.display()));
        assert!(
            text.contains(want),
            "{case}.stderr no longer contains {want}.\n\
             It was written to assert that specific rejection. If the compiler's code\n\
             legitimately changed, update `expected` deliberately — do not just\n\
             re-record, which would leave the case passing while asserting nothing."
        );
    }
    assert_eq!(cases.len(), expected.len(), "a pinned case disappeared: {cases:?}");
}

// ── A5 ─────────────────────────────────────────────────────────────────────

/// Nothing new may execute inside the results tree.
///
/// Four sites do today, all in the test phase: scoring builds in the canonical
/// phase dir and writes `target/` into it, so measuring an artifact mutates it.
/// Fixing that needs the `c/`+`rust/` layout split, which is deliberately not in
/// this change — so this is a ratchet on the count, not a clean gate. The
/// allowlist is the to-do list.
#[test]
fn nothing_new_runs_inside_the_results_tree() {
    const KNOWN: usize = 4;

    struct V {
        current_fn: String,
        hits: Vec<String>,
    }
    impl<'ast> Visit<'ast> for V {
        fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
            let prev = std::mem::replace(&mut self.current_fn, f.sig.ident.to_string());
            syn::visit::visit_item_fn(self, f);
            self.current_fn = prev;
        }
        fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
            if c.method == "current_dir" {
                // A phase dir is only ever reached through these helpers, so the
                // identifiers in the argument are the signal. Collected with a visitor
                // rather than matched against `Debug` output, which needs syn's
                // extra-traits feature and would match text inside string literals.
                let mut idents: Vec<String> = Vec::new();
                struct Ids<'a>(&'a mut Vec<String>);
                impl<'ast> Visit<'ast> for Ids<'_> {
                    fn visit_ident(&mut self, i: &'ast proc_macro2::Ident) {
                        self.0.push(i.to_string());
                    }
                }
                for a in &c.args {
                    Ids(&mut idents).visit_expr(a);
                }
                for marker in ["phase_dir", "crate_dir", "verified_dir", "translated_rust"] {
                    if idents.iter().any(|i| i == marker) {
                        self.hits.push(format!("{}() <- {marker}", self.current_fn));
                        break;
                    }
                }
            }
            syn::visit::visit_expr_method_call(self, c);
        }
    }

    let mut v = V { current_fn: "<top>".into(), hits: Vec::new() };
    for path in rust_sources() {
        v.visit_file(&parse(&path));
    }
    assert!(
        v.hits.len() <= KNOWN,
        "{} sites now run a command inside the results tree, up from {KNOWN}: {:#?}\n\
         Build in a scratch copy instead — measuring an artifact must not mutate it.",
        v.hits.len(),
        v.hits
    );
}

// ── A6 ─────────────────────────────────────────────────────────────────────

/// Every function that composes a cache key must open by destructuring `Self`
/// exhaustively, and must contain nothing that can ignore a binding.
///
/// That shape is what delegates the work to rustc: a field added to the struct is E0027
/// at the pattern, and a field bound but never hashed is `unused_variables`, denied at
/// package level. Each form rejected below quietly reopens the hole — `..` and
/// `field: _` skip the field, an `_`-prefixed name suppresses the lint, `let _ = x` and
/// `_ = x;` consume the binding without hashing it, and `#[allow]`/`#[expect]` switch
/// the lint off. A key that ignores an input silently serves one invocation's artifact
/// for another's — the failure the agent runtime env had while it lived in a shell
/// driver the key could not see.
#[test]
fn the_key_functions_cannot_forget_a_field() {
    const REQUIRED: &[(&str, &str)] =
        &[("Recipe", "digest"), ("KeyInputs", "key"), ("KeyInputs", "meta")];

    /// Rejects everything that makes a binding ignorable.
    struct Escapes(Vec<String>);
    impl<'ast> Visit<'ast> for Escapes {
        fn visit_local(&mut self, l: &'ast syn::Local) {
            if matches!(l.pat, syn::Pat::Wild(_)) {
                self.0.push("`let _ = ...`".into());
            }
            syn::visit::visit_local(self, l);
        }
        fn visit_expr_assign(&mut self, a: &'ast syn::ExprAssign) {
            if matches!(*a.left, syn::Expr::Infer(_)) {
                self.0.push("`_ = ...`".into());
            }
            syn::visit::visit_expr_assign(self, a);
        }
        fn visit_attribute(&mut self, a: &'ast syn::Attribute) {
            for lint in ["allow", "expect"] {
                if a.path().is_ident(lint) {
                    self.0.push(format!("#[{lint}(..)]"));
                }
            }
            syn::visit::visit_attribute(self, a);
        }
    }

    /// The opening statement must bind every field of `Self` to a usable name.
    fn opens_exhaustively(stmt: Option<&syn::Stmt>) -> Result<(), String> {
        let Some(syn::Stmt::Local(local)) = stmt else {
            return Err("does not open with a `let` destructuring".into());
        };
        let syn::Pat::Struct(pat) = &local.pat else {
            return Err("opens with a `let` that does not destructure a struct".into());
        };
        if !pat.path.is_ident("Self") {
            let spelled: Vec<String> =
                pat.path.segments.iter().map(|s| s.ident.to_string()).collect();
            return Err(format!("destructures `{}` rather than `Self`", spelled.join("::")));
        }
        if pat.rest.is_some() {
            return Err("uses `..`, so a field added later is silently skipped".into());
        }
        for field in &pat.fields {
            let name = match &field.member {
                syn::Member::Named(i) => i.to_string(),
                syn::Member::Unnamed(i) => i.index.to_string(),
            };
            match &*field.pat {
                syn::Pat::Ident(id) if !id.ident.to_string().starts_with('_') => {}
                _ => return Err(format!("field `{name}` is not bound to a name that must be used")),
            }
        }
        Ok(())
    }

    let path = src("cache.rs");
    let mut seen: Vec<(&str, &str)> = Vec::new();
    let mut bad: Vec<String> = Vec::new();

    for item in parse(&path).items {
        let syn::Item::Impl(imp) = item else { continue };
        let self_ty = impl_target(&imp.self_ty);
        for it in imp.items {
            let syn::ImplItem::Fn(f) = it else { continue };
            let name = f.sig.ident.to_string();
            let Some(&which) = REQUIRED.iter().find(|(t, n)| *t == self_ty && *n == name) else {
                continue;
            };
            seen.push(which);
            if let Err(why) = opens_exhaustively(f.block.stmts.first()) {
                bad.push(format!("{self_ty}::{name} {why}"));
            }
            let mut escapes = Escapes(Vec::new());
            escapes.visit_impl_item_fn(&f);
            for e in escapes.0 {
                bad.push(format!("{self_ty}::{name} contains {e}"));
            }
        }
    }

    for want in REQUIRED {
        assert!(
            seen.contains(want),
            "{}::{} is gone, so nothing checks its shape any more. If it was renamed,\n\
             update REQUIRED here deliberately; if it was deleted, so was this rule.",
            want.0,
            want.1
        );
    }
    assert!(
        bad.is_empty(),
        "a cache-key function can now ignore one of its inputs: {bad:#?}\n\
         Feed every binding instead. The whole point of the exhaustive pattern is that\n\
         rustc, not a reviewer, notices the field nobody hashed."
    );
}
