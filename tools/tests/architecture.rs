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

/// Full spelling, generic arguments included, so `Option<u32>` and `Option<String>` are
/// not the same key. [`type_name`] deliberately drops them, because A1 compares the impl
/// target `Sealed<P>` against the bare name `Sealed`.
fn ty_key(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => {
            let mut out = String::new();
            for (i, seg) in p.path.segments.iter().enumerate() {
                if i > 0 {
                    out.push_str("::");
                }
                out.push_str(&seg.ident.to_string());
                if let syn::PathArguments::AngleBracketed(a) = &seg.arguments {
                    let inner: Vec<String> = a
                        .args
                        .iter()
                        .filter_map(|g| match g {
                            syn::GenericArgument::Type(t) => Some(ty_key(t)),
                            _ => None,
                        })
                        .collect();
                    if !inner.is_empty() {
                        out.push('<');
                        out.push_str(&inner.join(","));
                        out.push('>');
                    }
                }
            }
            out
        }
        syn::Type::Reference(r) => {
            format!("&{}{}", if r.mutability.is_some() { "mut " } else { "" }, ty_key(&r.elem))
        }
        syn::Type::Slice(s) => format!("[{}]", ty_key(&s.elem)),
        syn::Type::Tuple(t) => {
            format!("({})", t.elems.iter().map(ty_key).collect::<Vec<_>>().join(","))
        }
        syn::Type::Paren(p) => ty_key(&p.elem),
        syn::Type::ImplTrait(_) => "impl".into(),
        _ => "?".into(),
    }
}

struct Func {
    file: String,
    name: String,
    sig: syn::Signature,
}

/// Every `fn` in the crate, including impl and trait items and `#[cfg(test)]` ones: a rule
/// that skipped tests would let a helper introduce the very shape it forbids.
fn signatures() -> Vec<Func> {
    struct V(Vec<syn::Signature>);
    impl<'ast> Visit<'ast> for V {
        fn visit_signature(&mut self, s: &'ast syn::Signature) {
            self.0.push(s.clone());
            syn::visit::visit_signature(self, s);
        }
    }
    let mut out = Vec::new();
    for path in rust_sources() {
        let mut v = V(Vec::new());
        v.visit_file(&parse(&path));
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        for sig in v.0 {
            out.push(Func { file: file.clone(), name: sig.ident.to_string(), sig });
        }
    }
    out
}

/// Method names called inside each `fn`, keyed by `(file, innermost enclosing fn)`.
fn method_calls() -> BTreeMap<(String, String), Vec<String>> {
    struct V {
        file: String,
        enclosing: Vec<String>,
        out: BTreeMap<(String, String), Vec<String>>,
    }
    impl<'ast> Visit<'ast> for V {
        fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
            self.enclosing.push(f.sig.ident.to_string());
            syn::visit::visit_item_fn(self, f);
            self.enclosing.pop();
        }
        fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
            self.enclosing.push(f.sig.ident.to_string());
            syn::visit::visit_impl_item_fn(self, f);
            self.enclosing.pop();
        }
        fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
            if let Some(func) = self.enclosing.last() {
                let mut name = c.method.to_string();
                // Recorded as one construct: `.display()` alone is fine and appears in
                // error messages on this very path, only the `.to_string()` pair is lossy.
                if name == "to_string" {
                    if let syn::Expr::MethodCall(inner) = &*c.receiver {
                        if inner.method == "display" {
                            name = "display().to_string".into();
                        }
                    }
                }
                self.out.entry((self.file.clone(), func.clone())).or_default().push(name);
            }
            syn::visit::visit_expr_method_call(self, c);
        }
    }
    let mut v = V { file: String::new(), enclosing: Vec::new(), out: BTreeMap::new() };
    for path in rust_sources() {
        v.file = path.file_name().unwrap().to_string_lossy().into_owned();
        v.visit_file(&parse(&path));
    }
    v.out
}

/// Does `ty` name `want` anywhere in its spelling, generic arguments included?
fn mentions_type(ty: &syn::Type, want: &str) -> bool {
    struct V<'a>(&'a str, bool);
    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_path_segment(&mut self, s: &'ast syn::PathSegment) {
            if s.ident == self.0 {
                self.1 = true;
            }
            syn::visit::visit_path_segment(self, s);
        }
    }
    let mut v = V(want, false);
    v.visit_type(ty);
    v.1
}

/// Named, not `(String, String)` — A6 forbids that shape in `src/`, and a rule file that
/// exempts itself is one a reader is entitled to stop believing.
struct Param {
    name: String,
    ty: String,
}

fn params(sig: &syn::Signature) -> Vec<Param> {
    sig.inputs
        .iter()
        .filter_map(|arg| match arg {
            syn::FnArg::Typed(t) => {
                let name = match &*t.pat {
                    syn::Pat::Ident(i) => i.ident.to_string(),
                    _ => "_".to_string(),
                };
                Some(Param { name, ty: ty_key(&t.ty) })
            }
            syn::FnArg::Receiver(_) => None,
        })
        .collect()
}

/// Peels `Result<_>` / `Option<_>` so a fallible function's real return shape is seen.
fn returned_ty(sig: &syn::Signature) -> Option<&syn::Type> {
    let syn::ReturnType::Type(_, ty) = &sig.output else { return None };
    let mut ty: &syn::Type = ty;
    loop {
        let syn::Type::Path(p) = ty else { return Some(ty) };
        let last = p.path.segments.last()?;
        if !matches!(last.ident.to_string().as_str(), "Result" | "Option") {
            return Some(ty);
        }
        let syn::PathArguments::AngleBracketed(a) = &last.arguments else { return Some(ty) };
        let Some(syn::GenericArgument::Type(inner)) =
            a.args.iter().find(|g| matches!(g, syn::GenericArgument::Type(_)))
        else {
            return Some(ty);
        };
        ty = inner;
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
            if type_name(&imp.self_ty) != "Sealed" {
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
                    let self_ty = type_name(&imp.self_ty);
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
#[test]
fn digests_cannot_be_fabricated() {
    const GUARDED: &[&str] = &["TreeDigest", "PromptDigest", "RecipeDigest", "CacheKey", "ToolchainId"];
    let mut bad: Vec<String> = Vec::new();

    for module in ["artifact.rs", "cache.rs"] {
        for item in parse(&src(module)).items {
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
                    let target = type_name(&imp.self_ty);
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
        ("scrubbed_cannot_be_used_after_seal", "E0382"),  // seal() consumed it
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

/// A tuple return may not repeat an element type.
///
/// All five that did were destructured positionally and relabelled at the call site, so a
/// reorder inside the callee type-checked everywhere, and each fed a published number:
/// `aggregate_cases` the unsafe-percentage column, `laertes_vs_c2rust` the direction of
/// the paper's Laertes claim, and `score_harvest_bench_suite` the harvest-bench pass
/// column — swapping its failed and skipped counts makes a FAILING project pass, because
/// `ProjectOutcome::passed()` only requires `tests_failed == 0`.
#[test]
fn a_tuple_return_may_not_repeat_an_element_type() {
    let mut bad: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for f in signatures() {
        let Some(ret) = returned_ty(&f.sig) else { continue };
        let syn::Type::Tuple(t) = ret else { continue };
        if t.elems.is_empty() {
            continue; // `()`
        }
        seen += 1;
        let keys: Vec<String> = t.elems.iter().map(ty_key).collect();
        let mut uniq = keys.clone();
        uniq.sort();
        uniq.dedup();
        if uniq.len() != keys.len() {
            bad.push(format!("{}: {} -> ({})", f.file, f.name, keys.join(", ")));
        }
    }
    assert!(
        bad.is_empty(),
        "these functions return a tuple with a repeated element type: {bad:#?}\n\
         Callers destructure positionally, so a reorder inside the callee is invisible to\n\
         the compiler and lands in a table relabelled. Return a struct with named fields."
    );
    // Guards against the extractor silently matching nothing and the rule passing vacuously.
    assert!(seen > 0, "no tuple returns found at all — the return-type extractor is broken");
}

// ── A7 ─────────────────────────────────────────────────────────────────────

/// Types carrying no meaning of their own, so two in a row are interchangeable.
const PRIMITIVES: &[&str] = &[
    "&str", "String", "&Path", "PathBuf", "bool", "u8", "u16", "u32", "u64", "usize", "i8",
    "i16", "i32", "i64", "f32", "f64", "&OsStr", "OsString", "char",
];

/// No function may take THREE OR MORE parameters of the same primitive type.
///
/// Threshold 3, not 2: at 2 it fires on 40 sites including `copy_dir_all(src, dst)`, where
/// the order is universal convention and inverting it fails the first test run — a rule
/// nobody would keep. Crate-wide, not `pub`-only, because the worst offenders are private
/// (`verify_case(prompt_template, cmake_flags, configs_text)`).
///
/// ALLOWED is shrink-only; see the second assertion. Two were fixed instead of listed:
/// `openrouter_converse`, where transposing the system and user prompts silently asks a
/// different question in the path that produced the committed `oneshot` results, and
/// `write_settings`, whose three `&Path` were the entire filesystem policy.
#[test]
fn no_function_takes_three_interchangeable_primitives() {
    const ALLOWED: &[(&str, &str)] = &[
        ("opencode.rs", "extract_limits"),
        ("opencode.rs", "invoke"),
        ("test.rs", "score_harvest_bench_suite"),
        ("translate.rs", "translate_case"),
        ("translate.rs", "propagate_config_phase"),
        ("translate.rs", "invoke_codex_with_retry"),
        ("verify.rs", "verify_case"),
    ];

    let mut hits: Vec<(String, String, String)> = Vec::new();
    for f in signatures() {
        let mut by_ty: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for param in params(&f.sig) {
            if let Some(p) = PRIMITIVES.iter().find(|p| **p == param.ty) {
                by_ty.entry(p).or_default().push(param.name);
            }
        }
        for (ty, names) in by_ty {
            if names.len() >= 3 {
                hits.push((f.file.clone(), f.name.clone(), format!("{}x {ty} ({})", names.len(), names.join(", "))));
            }
        }
    }

    let unlisted: Vec<String> = hits
        .iter()
        .filter(|(file, name, _)| !ALLOWED.contains(&(file.as_str(), name.as_str())))
        .map(|(file, name, detail)| format!("{file}: {name} takes {detail}"))
        .collect();
    assert!(
        unlisted.is_empty(),
        "{unlisted:#?}\n\
         Group them into a struct with named fields, or a newtype per role. Do NOT add\n\
         them to ALLOWED: that list is closed and may only shrink."
    );

    let stale: Vec<&(&str, &str)> = ALLOWED
        .iter()
        .filter(|(file, name)| !hits.iter().any(|(f, n, _)| f == file && n == name))
        .collect();
    assert!(
        stale.is_empty(),
        "these entries no longer take three of one primitive: {stale:?}\n\
         Delete them. The list must only ever shrink, or a future regression lands in a\n\
         slot something else already vacated."
    );
}

// ── A8 ─────────────────────────────────────────────────────────────────────

/// Nothing on the digest path may reach a path through a lossy conversion.
///
/// `feed` documents itself as injective, and `feed_is_injective_where_nul_separators_are_not`
/// tests it, yet `digest_tree` fed it `to_string_lossy()` — while `RelPath::new` validates
/// relative/no-`..`/non-empty but NOT UTF-8. Every invalid byte became U+FFFD, so two
/// different trees digested alike: a cache hit across different artifacts. `normalise` and
/// `scrub` had the mirror bug, substituting a U+FFFD needle that can match real text while
/// the actual path went un-redacted.
///
/// The positive half matters as much as the ban: without it, dropping the path from the
/// digest entirely would satisfy the rule.
#[test]
fn the_digest_path_is_lossless() {
    const GUARDED: &[(&str, &str)] = &[
        ("artifact.rs", "digest_tree"),
        ("artifact.rs", "scrub"),
        ("artifact.rs", "classify"), // decides WHICH files digest_tree hashes
        ("cache.rs", "normalise"),
    ];
    const BANNED: &[&str] = &["to_string_lossy", "display().to_string"];

    let calls = method_calls();
    let mut bad: Vec<String> = Vec::new();
    for (file, func) in GUARDED {
        let found = calls.get(&(file.to_string(), func.to_string())).unwrap_or_else(|| {
            panic!("{file}: {func} not found — this rule is guarding a function that has been renamed or removed")
        });
        for banned in BANNED {
            if found.iter().any(|c| c == banned) {
                bad.push(format!("{file}: {func} calls {banned}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{bad:#?}\n\
         A lossy conversion maps every invalid byte to U+FFFD, so distinct paths become\n\
         equal. Use `as_os_str().as_encoded_bytes()` where bytes are wanted, or `to_str()`\n\
         and skip where a `&str` is required."
    );

    let digest = &calls[&("artifact.rs".to_string(), "digest_tree".to_string())];
    assert!(
        digest.contains(&"as_encoded_bytes".to_string()),
        "digest_tree no longer feeds as_encoded_bytes: {digest:?}\n\
         The path component of the digest must be hashed as its exact bytes."
    );
}

// ── A9 ─────────────────────────────────────────────────────────────────────

/// Credits and dollars may not be the same type, and the rate is applied exactly once.
///
/// `Credits(pub f64)` with dollars as a bare `f64` made `credits` and
/// `credits * USD_PER_CREDIT` mutually substitutable in the code emitting the paper's cost
/// table — a 25x error that type-checks.
#[test]
fn money_amounts_cannot_be_substituted_for_one_another() {
    const MONEY: &[&str] = &["Credits", "Usd"];
    let mut bad: Vec<String> = Vec::new();
    let mut defined: Vec<String> = Vec::new();

    for path in rust_sources() {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        for item in parse(&path).items {
            match &item {
                syn::Item::Struct(s) if MONEY.contains(&s.ident.to_string().as_str()) => {
                    defined.push(s.ident.to_string());
                    for f in &s.fields {
                        if is_public(&f.vis) {
                            bad.push(format!("{file}: {} has a public field", s.ident));
                        }
                    }
                }
                syn::Item::Impl(imp) => {
                    let Some((_, tr, _)) = &imp.trait_ else { continue };
                    let name = tr.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
                    let target = type_name(&imp.self_ty);
                    // `From`/`Deref`/`Add` between the two, or from a bare float, would
                    // restore exactly the implicit conversion the newtypes remove.
                    if MONEY.contains(&target.as_str())
                        && matches!(name.as_str(), "From" | "Deref" | "DerefMut" | "Add" | "Sub" | "Mul")
                    {
                        bad.push(format!("{file}: impl {name} for {target}"));
                    }
                }
                _ => {}
            }
        }
    }
    for want in MONEY {
        assert!(defined.iter().any(|d| d == want), "the {want} newtype is gone");
    }
    assert!(
        bad.is_empty(),
        "{bad:#?}\n\
         A dollar amount is 25x a credit count and both reach a published table. Keep the\n\
         fields private and convert only through `Credits::to_usd`."
    );

    // One definition and one use: the rate cannot be re-applied, or applied twice, anywhere else.
    let uses: usize = rust_sources()
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap().matches("USD_PER_CREDIT").count())
        .sum();
    assert_eq!(
        uses, 2,
        "USD_PER_CREDIT appears {uses} times, expected 2 (its definition and `to_usd`).\n\
         A second application of the rate is a second chance to get the units wrong."
    );
}

// ── A10 ────────────────────────────────────────────────────────────────────

/// The three `bool` parameters that gate a safety property must be two-variant enums.
///
/// Only those three. The other 20 are deliberately allowlisted: no function here takes two
/// `bool`s, so they have no transposition scenario, and several are correct recursion state
/// a caller never spells (`classify(&rel, in_build_dir)`). One shared `enum Flag { Yes, No }`
/// would satisfy every bool rule while buying nothing — a transposed `Flag::Yes` is as wrong
/// as a transposed `true` — so each gets its own type.
///
/// The two-bool clause fires 0 today and is the live half: a second `bool` is where order
/// becomes silently loseable.
#[test]
fn safety_gating_bools_are_named_enums() {
    /// (file, fn, param) — closed, shrink-only.
    const ALLOWED: &[(&str, &str, &str)] = &[
        // Recursion state, threaded by the traversal itself rather than by a caller.
        ("artifact.rs", "classify", "in_build_dir"),
        ("artifact.rs", "visit", "in_build_dir"),
        // `--force` / `--last` / `--allow-infra-failures`: CLI flags whose polarity is
        // fixed by the flag name at the single site that reads them.
        ("battery.rs", "find_record", "last"),
        ("benchmark.rs", "verify", "_force"),
        ("benchmark.rs", "verify", "force"),
        ("main.rs", "run_test", "allow_infra_failures"),
        ("verify.rs", "run", "force"),
        ("verify.rs", "run_all", "force"),
        ("verify.rs", "run_with_semaphore", "force"),
        ("verify.rs", "run_harvest_bench", "force"),
        // Properties of the case being translated, derived once and passed down.
        ("translate.rs", "dispatch_translate", "is_lib"),
        ("translate.rs", "post_process_independent", "is_lib"),
        ("translate.rs", "kimi_translate_case", "is_lib_hint"),
        ("translate.rs", "oneshot_translate_case", "is_lib_hint"),
        ("translate.rs", "oneshot_llm_translate", "is_lib_hint"),
        ("translate.rs", "c2saferrust_translate_case", "_is_lib"),
        // Outcomes being recorded, not decisions being taken.
        ("translate.rs", "write_translation_metrics", "success"),
        ("translate.rs", "write_verification_metrics", "replayed"),
        ("scoring.rs", "outcome", "built"),
    ];

    let mut hits: Vec<(String, String, String)> = Vec::new();
    let mut two_bools: Vec<String> = Vec::new();
    for f in signatures() {
        let bools: Vec<String> = params(&f.sig)
            .into_iter()
            .filter(|p| p.ty == "bool")
            .map(|p| p.name)
            .collect();
        if bools.len() >= 2 {
            two_bools.push(format!("{}: {}({})", f.file, f.name, bools.join(", ")));
        }
        for pname in bools {
            hits.push((f.file.clone(), f.name.clone(), pname));
        }
    }

    assert!(
        two_bools.is_empty(),
        "these functions take two or more bools, where transposing them is silent: {two_bools:#?}\n\
         Give each its own two-variant enum — distinct types, not one shared Yes/No."
    );

    let unlisted: Vec<String> = hits
        .iter()
        .filter(|(file, name, p)| !ALLOWED.contains(&(file.as_str(), name.as_str(), p.as_str())))
        .map(|(file, name, p)| format!("{file}: {name}({p}: bool)"))
        .collect();
    assert!(
        unlisted.is_empty(),
        "{unlisted:#?}\n\
         If this gates a safety property, give it a named two-variant enum as\n\
         `artifact::Access`, `provenance::OnUnreproducible` and `workdir::Tmpfs` do.\n\
         Otherwise justify it in ALLOWED above — but the list is closed and may only shrink."
    );

    let stale: Vec<&(&str, &str, &str)> = ALLOWED
        .iter()
        .filter(|(file, name, p)| {
            !hits.iter().any(|(f, n, hp)| f == file && n == name && hp == p)
        })
        .collect();
    assert!(stale.is_empty(), "these bool parameters are gone; delete them from ALLOWED: {stale:?}");
}

// ── A11 ────────────────────────────────────────────────────────────────────

/// The typestate chain, in order. Membership alone cannot express this: the family is
/// derived from the `PhantomData` field, but `Sealed::materialise_into(&self) -> WorkTree`
/// is a legitimate BACKWARD edge, so only an explicit ordering distinguishes it from a
/// forward transition that must consume its receiver.
const TYPESTATE_ORDER: &[&str] = &["WorkTree", "Scrubbed", "Sealed"];

/// A `PhantomData`-bearing struct has all-private fields, and every forward transition
/// consumes `self`.
///
/// Fires 0 today — insurance, not a fix. `scrub` and `seal` already take `self`, which is
/// why it is worth pinning: a later `&self` would let one tree be sealed twice, so the
/// digest would describe one state and the published artifact another. The two compile-fail
/// cases assert these same edges by example; the shape rule covers edges nobody has written
/// a case for yet.
#[test]
fn typestates_have_private_fields_and_consuming_transitions() {
    let mut family: Vec<String> = Vec::new();
    let mut leaks: Vec<String> = Vec::new();

    for path in rust_sources() {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        for item in parse(&path).items {
            let syn::Item::Struct(s) = item else { continue };
            if !s.fields.iter().any(|f| type_name(&f.ty).ends_with("PhantomData")) {
                continue;
            }
            family.push(s.ident.to_string());
            for f in &s.fields {
                if is_public(&f.vis) {
                    let fname = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_else(|| "0".into());
                    leaks.push(format!("{file}: {}.{fname} is not private", s.ident));
                }
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "{leaks:#?}\n\
         A phase-tagged struct with a reachable field can be rebuilt with a different tag,\n\
         which is the whole invariant the PhantomData exists to carry."
    );
    for want in TYPESTATE_ORDER {
        assert!(
            family.iter().any(|f| f == want),
            "{want} no longer carries a PhantomData phase tag; TYPESTATE_ORDER is stale"
        );
    }

    let rank = |name: &str| TYPESTATE_ORDER.iter().position(|s| *s == name);
    let mut borrowing: Vec<String> = Vec::new();
    for path in rust_sources() {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        for item in parse(&path).items {
            let syn::Item::Impl(imp) = item else { continue };
            let Some(from) = rank(&type_name(&imp.self_ty)) else { continue };
            for it in imp.items {
                let syn::ImplItem::Fn(f) = it else { continue };
                let Some(ret) = returned_ty(&f.sig) else { continue };
                let consumes = matches!(
                    f.sig.inputs.first(),
                    Some(syn::FnArg::Receiver(r)) if r.reference.is_none()
                );
                let forward = TYPESTATE_ORDER
                    .iter()
                    .enumerate()
                    .any(|(to, name)| to > from && mentions_type(ret, name));
                if forward && !consumes {
                    borrowing.push(format!("{file}: {}::{}", type_name(&imp.self_ty), f.sig.ident));
                }
            }
        }
    }
    assert!(
        borrowing.is_empty(),
        "these forward transitions do not consume self: {borrowing:#?}\n\
         Taking `&self` lets the source state be used again after the transition, so the\n\
         digest and the published tree can describe different states."
    );
}
