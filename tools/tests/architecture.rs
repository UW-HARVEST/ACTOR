//! Shape rules over the source, for invariants enforced by ABSENCE.
//!
//! `Sealed<P>` stops anything executing in a published artifact by not implementing
//! `AsRef<Path>` and not having a `path()`. A trybuild test proves today's code
//! rejects `Command::current_dir(&sealed)`, but it cannot prove nobody *adds* the
//! impl tomorrow — at which point the trybuild case starts failing for a reason a
//! reader may well "fix" by re-recording it. These rules assert the shape instead.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::visit::Visit;

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn src(name: &str) -> PathBuf {
    src_dir().join(name)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// `src/artifact.rs` is `artifact`, `src/domain/contents.rs` is `domain::contents`, and
/// `src/domain/mod.rs` is `domain`. Rules key on this rather than on the leaf filename,
/// which stops naming the same code the moment a module becomes a directory.
fn module_path(file: &Path) -> String {
    let rel = file.strip_prefix(src_dir()).expect("under src/");
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let leaf = parts.pop().expect("a file name");
    let stem = leaf.strip_suffix(".rs").unwrap_or(&leaf).to_owned();
    if stem != "mod" {
        parts.push(stem);
    }
    parts.join("::")
}

/// The file a rule's module path names, or a panic: a rule whose subject has moved must
/// say so, rather than inspect nothing and report green.
fn module_file(module: &str) -> PathBuf {
    let base = module.split("::").fold(src_dir(), |p, s| p.join(s));
    for candidate in [base.with_extension("rs"), base.join("mod.rs")] {
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "no source file for module `{module}`, which a rule below is written about. Repoint \
         it at the module that now holds the code."
    )
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn parse(path: &Path) -> syn::File {
    syn::parse_file(&read(path)).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn rust_sources() -> Vec<PathBuf> {
    rust_sources_under(&src_dir())
}

fn rust_sources_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.expect("src/ entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn count_rust_files(dir: &Path, depth: usize) -> (usize, usize) {
    let (mut total, mut nested) = (0, 0);
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.expect("src/ entry").path();
        if path.is_dir() {
            let (t, n) = count_rust_files(&path, depth + 1);
            total += t;
            nested += n;
        } else if path.extension().is_some_and(|x| x == "rs") {
            total += 1;
            nested += usize::from(depth > 1);
        }
    }
    (total, nested)
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
            if matches!(
                s.ident.to_string().as_str(),
                "Path" | "PathBuf" | "OsStr" | "OsString"
            ) {
                *self.0 = true;
            }
            syn::visit::visit_path_segment(self, s);
        }
    }
    V(&mut hit).visit_type(ty);
    hit
}

fn is_public(vis: &syn::Visibility) -> bool {
    matches!(
        vis,
        syn::Visibility::Public(_) | syn::Visibility::Restricted(_)
    )
}

/// Every rule here iterates `rust_sources()` and none assert they found anything: while it
/// was a flat `read_dir`, the first module to become a directory would have dropped out of
/// all of them, `sealed_implements_only_debug` included, and each would still report green.
#[test]
fn the_shape_rules_cannot_pass_while_inspecting_nothing() {
    // Measured 23 today: 21 module files plus lib.rs and main.rs. The floor is that count
    // minus 2, so a merge landing a file needs no edit here while deleting three fails
    // instead of quietly narrowing what every rule below inspects. Add files, raise it.
    const MIN_FILES: usize = 21;
    const REQUIRED: &[&str] = &["Sealed", "WorkTree", "Scrubbed", "Corpus", "TreeDigest"];

    let found = rust_sources();
    assert!(
        found.len() >= MIN_FILES,
        "rust_sources() found {} files, below the {MIN_FILES} the rules expect to inspect: \
         {found:#?}",
        found.len()
    );

    struct Declared(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Declared {
        fn visit_item_struct(&mut self, s: &'ast syn::ItemStruct) {
            self.0.insert(s.ident.to_string());
            syn::visit::visit_item_struct(self, s);
        }
        fn visit_item_enum(&mut self, e: &'ast syn::ItemEnum) {
            self.0.insert(e.ident.to_string());
            syn::visit::visit_item_enum(self, e);
        }
    }
    let mut declared = Declared(BTreeSet::new());
    for path in &found {
        declared.visit_file(&parse(path));
    }
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|t| !declared.0.contains(*t))
        .collect();
    assert!(
        missing.is_empty(),
        "the rules are written about {missing:?}, and nothing rust_sources() returned \
         declares them: either they were renamed, or the traversal no longer reaches the \
         file that holds them."
    );

    let dir = src_dir();
    let depth = |p: &Path| {
        p.strip_prefix(&dir)
            .expect("under src/")
            .components()
            .count()
    };
    let nested = found.iter().filter(|p| depth(p) > 1).count();
    assert_eq!(
        (found.len(), nested),
        count_rust_files(&dir, 1),
        "rust_sources() and an independent walk of src/ disagree on (total, nested). \
         domain/ made the nested half live: 4 of 23 today, so a traversal that stopped at \
         the top level of src/ would fail here instead of reporting green."
    );
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
                Some((_, p, _)) => p
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default(),
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

/// In the artifact and cache modules, and the pure leaves split out of them, no public
/// item may hand out a path.
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

    for module in ["artifact", "cache", "domain::relpath", "domain::contents"] {
        for item in parse(&module_file(module)).items {
            match item {
                syn::Item::Impl(imp) => {
                    let self_ty = type_name(&imp.self_ty);
                    for it in imp.items {
                        let syn::ImplItem::Fn(f) = it else { continue };
                        if !is_public(&f.vis) {
                            continue;
                        }
                        let syn::ReturnType::Type(_, ret) = &f.sig.output else {
                            continue;
                        };
                        let name = f.sig.ident.to_string();
                        if is_pathish(ret) && !ALLOWED.contains(&(self_ty.as_str(), name.as_str()))
                        {
                            leaks
                                .push(format!("{module}: {self_ty}::{name} -> {}", type_name(ret)));
                        }
                    }
                }
                syn::Item::Struct(s) if is_public(&s.vis) => {
                    for f in s.fields {
                        if is_public(&f.vis) && is_pathish(&f.ty) {
                            let fname =
                                f.ident.map(|i| i.to_string()).unwrap_or_else(|| "0".into());
                            leaks.push(format!(
                                "{module}: {}.{fname} is a public path field",
                                s.ident
                            ));
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

/// Digest and identity newtypes must be unforgeable: private field, no `From<String>`.
///
/// A digest that can be constructed from an arbitrary string is a digest that can
/// be wrong, and the cache compares them to decide whether to reuse an artifact.
/// `AgentKey` and `CliVersion` are the same hazard from the other side: they name WHAT
/// ran, so a caller able to spell one without deriving it from `--agent`, or from the
/// CLI itself, is how a key comes to name something that did not run.
#[test]
fn digests_cannot_be_fabricated() {
    const GUARDED: &[&str] = &[
        "TreeDigest",
        "PromptDigest",
        "RecipeDigest",
        "CacheKey",
        "ToolchainId",
        "AgentKey",
        "CliVersion",
    ];
    let mut bad: Vec<String> = Vec::new();

    for module in ["artifact", "cache"] {
        for item in parse(&module_file(module)).items {
            match &item {
                syn::Item::Struct(s) if GUARDED.contains(&s.ident.to_string().as_str()) => {
                    for f in &s.fields {
                        if is_public(&f.vis) {
                            bad.push(format!("{}: {} has a public field", module, s.ident));
                        }
                    }
                }
                syn::Item::Impl(imp) => {
                    let Some((_, tr, _)) = &imp.trait_ else {
                        continue;
                    };
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
        ("sealed_has_no_path", "E0599"),             // no method named `path`
        ("sealed_is_not_a_command_cwd", "E0277"),    // AsRef<Path> not satisfied
        ("phases_are_not_interchangeable", "E0308"), // mismatched types
        ("completed_cannot_be_forged", "E0603"),     // private constructor
        ("worktree_cannot_be_used_after_scrub", "E0382"), // scrub() consumed it
        ("scrubbed_cannot_be_used_after_seal", "E0382"), // seal() consumed it
        ("materialise_at_refuses_a_results_tree_path", "E0308"), // needs a Cwd, not a Path
        ("a_verification_cannot_seed_a_translation", "E0277"), // no such SeededBy impl
        ("phase_cannot_be_implemented_downstream", "E0277"), // sealed supertrait
        ("sealed_does_not_display", "E0277"),        // no Display impl
        ("materialise_at_refuses_a_results_tree_path", "E0308"), // not a ScratchPath
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
    assert_eq!(
        cases.len(),
        expected.len(),
        "a pinned case disappeared: {cases:?}"
    );
}

// ── A5 ─────────────────────────────────────────────────────────────────────

/// Nothing new may execute inside the results tree.
///
/// A ratchet, not a gate. It matches two sites, and only `build_harvest_bench_lib`
/// is really in `results/`; the other reaches scratch through `translated_rust()`.
/// It is also blind to the builds that matter most, which are spawned with a
/// `--root` or `--target-dir` argument rather than a `current_dir` (MIT `runtests`,
/// the gtest suite). A `Cwd` newtype only scratch can construct is the real fix.
/// The `c/`+`rust/` layout split this comment used to demand is not (see
/// `artifact.rs`): `runtests` pins the build output inside the case either way.
#[test]
fn nothing_new_runs_inside_the_results_tree() {
    const KNOWN: usize = 2;

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

    let mut v = V {
        current_fn: "<top>".into(),
        hits: Vec::new(),
    };
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

/// An agent's identity may never be spelled with `Debug`.
///
/// `format!("{agent:?}").to_lowercase()` was at once the cache key component, the entry
/// directory, the field `load` re-validates, and the `"agent"` of every result file — so
/// renaming a variant silently renamed the identity of a run, and `Debug` is not a
/// serialization contract. It has already happened: 208 files under `codex-gpt55/`
/// record `"agent": "codex"`, which no `--agent` value has spelled since. `AgentKey`,
/// derived from clap's `ValueEnum` name, is the one spelling.
///
/// Matched on tokens rather than raw text, so the word "agent" inside a message and a
/// `{:?}` for something else in the same macro are not a false positive.
#[test]
fn an_agents_identity_is_never_its_debug_output() {
    fn debugs_an_agent(tokens: proc_macro2::TokenStream) -> bool {
        let mut names_agent = false;
        let mut debug_placeholder = false;
        for t in tokens {
            match t {
                proc_macro2::TokenTree::Ident(i) => names_agent |= i == "agent",
                proc_macro2::TokenTree::Literal(l) => {
                    let s = l.to_string();
                    // The inline form needs no separate argument to be the giveaway.
                    if s.contains("{agent:?}") {
                        return true;
                    }
                    debug_placeholder |= s.contains("{:?}");
                }
                proc_macro2::TokenTree::Group(g) => {
                    if debugs_an_agent(g.stream()) {
                        return true;
                    }
                }
                proc_macro2::TokenTree::Punct(_) => {}
            }
        }
        names_agent && debug_placeholder
    }

    struct V(Vec<String>);
    impl<'ast> Visit<'ast> for V {
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            let name = m
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();
            // Diagnostic macros are exempt: their Debug output is a message a human reads
            // when a test or invariant fails, never a persisted identity.
            let diagnostic = name.starts_with("assert")
                || matches!(
                    name.as_str(),
                    "panic" | "unreachable" | "todo" | "ensure" | "bail"
                );
            if !diagnostic && debugs_an_agent(m.tokens.clone()) {
                let name = m
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default();
                self.0.push(format!("{name}!"));
            }
            syn::visit::visit_macro(self, m);
        }
    }

    let mut found: Vec<String> = Vec::new();
    for path in rust_sources() {
        let mut v = V(Vec::new());
        v.visit_file(&parse(&path));
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        found.extend(v.0.into_iter().map(|m| format!("{file}: {m}")));
    }
    assert!(
        found.is_empty(),
        "an agent is being formatted with Debug: {found:#?}\n\
         Use `cache::AgentKey` (and `Paths::agent_key`), which is clap's own name for the\n\
         variant and is what the results tree and the store are already keyed by."
    );
}

/// "Did this phase produce a crate?" has exactly one spelling: `battery::has_crate`.
///
/// It had two — `crate_dir`'s `is_dir()` and its callers' `verified/Cargo.toml` — and
/// pcre2 satisfied one and not the other, so it left the harvest-bench denominator
/// instead of counting as a failure. Nothing else would catch a third spelling.
#[test]
fn only_battery_defines_the_has_crate_predicate() {
    struct V {
        file: String,
        hits: Vec<String>,
    }
    impl<'ast> Visit<'ast> for V {
        fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
            if matches!(c.method.to_string().as_str(), "exists" | "is_file") {
                if let syn::Expr::MethodCall(inner) = &*c.receiver {
                    let joins_manifest = inner.method == "join"
                        && inner.args.iter().any(|a| {
                            matches!(a,
                            syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. })
                                if s.value() == "Cargo.toml")
                        });
                    if joins_manifest {
                        self.hits.push(format!(
                            "{}: .join(\"Cargo.toml\").{}()",
                            self.file, c.method
                        ));
                    }
                }
            }
            syn::visit::visit_expr_method_call(self, c);
        }
    }

    let mut hits: Vec<String> = Vec::new();
    for path in rust_sources() {
        if file_name(&path) == "battery.rs" {
            continue;
        }
        let mut v = V {
            file: file_name(&path),
            hits: Vec::new(),
        };
        v.visit_file(&parse(&path));
        hits.extend(v.hits);
    }
    assert!(
        hits.is_empty(),
        "the phase predicate is spelled out again outside battery.rs: {hits:#?}\n\
         Call `battery::has_crate(dir)`. Two spellings of \"this phase produced a crate\"\n\
         is how a project vanished from a published denominator."
    );
    let battery = std::fs::read_to_string(src("battery.rs")).expect("battery.rs");
    assert!(
        battery.contains(r#"phase_dir.join("Cargo.toml").is_file()"#),
        "battery::has_crate must still BE the predicate this rule redirects callers to"
    );
}

/// The three key-deriving functions must keep their exhaustive patterns.
///
/// `Recipe::digest`, `KeyInputs::key` and `KeyInputs::meta` open with a full destructuring
/// pattern precisely so adding a field fails to compile (E0027) rather than silently
/// leaving the cache key unchanged, which would let two different invocations share an
/// entry. Each escape below restores that silence: `..` and `field: _` skip a field,
/// `let _ = x` and the bare `_ = x` consume a binding without hashing it (the latter is
/// destructuring assignment, and compiles with no `let` at all), and
/// `#[allow(unused_variables)]` disables the other half of the guarantee.
#[test]
fn the_key_deriving_functions_keep_their_exhaustive_patterns() {
    const GUARDED: &[&str] = &["digest", "key", "meta"];
    let text = std::fs::read_to_string(src("cache.rs")).expect("cache.rs");
    let file = syn::parse_file(&text).expect("cache.rs parses");
    let mut bad: Vec<String> = Vec::new();

    for item in file.items {
        let syn::Item::Impl(imp) = item else { continue };
        let owner = type_name(&imp.self_ty);
        if owner != "Recipe" && owner != "KeyInputs" {
            continue;
        }
        for it in imp.items {
            let syn::ImplItem::Fn(f) = it else { continue };
            let name = f.sig.ident.to_string();
            if !GUARDED.contains(&name.as_str()) {
                continue;
            }
            let mut note = |what: &str| bad.push(format!("{owner}::{name}: {what}"));
            if f.attrs.iter().any(|a| {
                let p = a
                    .path()
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::");
                let t = a
                    .meta
                    .require_list()
                    .map(|l| l.tokens.to_string())
                    .unwrap_or_default();
                p.contains("allow") && t.contains("unused_variables")
            }) {
                note("#[allow(unused_variables)]");
            }
            for st in &f.block.stmts {
                match st {
                    syn::Stmt::Local(l) => {
                        if let syn::Pat::Struct(ps) = &l.pat {
                            if ps.rest.is_some() {
                                note("`..` in the destructuring pattern");
                            }
                            if ps
                                .fields
                                .iter()
                                .any(|fp| matches!(&*fp.pat, syn::Pat::Wild(_)))
                            {
                                note("a field bound to `_`");
                            }
                        }
                        if matches!(&l.pat, syn::Pat::Wild(_)) {
                            note("`let _ =` discards a binding");
                        }
                    }
                    // bare `_ = x;` is destructuring assignment: no `let`, compiles clean
                    syn::Stmt::Expr(syn::Expr::Assign(a), _) => {
                        if matches!(&*a.left, syn::Expr::Infer(_)) {
                            note("bare `_ =` discards a binding");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a key-deriving function can now skip a field silently: {bad:#?}\n\
         Keep the pattern exhaustive and feed every binding, so a new field is a compile\n\
         error rather than two invocations quietly sharing a cache entry."
    );
}

struct Func {
    module: String,
    name: String,
}

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
        let module = module_path(&path);
        for sig in v.0 {
            out.push(Func {
                module: module.clone(),
                name: sig.ident.to_string(),
            });
        }
    }
    out
}

fn method_calls() -> BTreeMap<(String, String), Vec<String>> {
    struct V {
        module: String,
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
                self.out
                    .entry((self.module.clone(), func.clone()))
                    .or_default()
                    .push(name);
            }
            syn::visit::visit_expr_method_call(self, c);
        }
    }
    let mut v = V {
        module: String::new(),
        enclosing: Vec::new(),
        out: BTreeMap::new(),
    };
    for path in rust_sources() {
        v.module = module_path(&path);
        v.visit_file(&parse(&path));
    }
    v.out
}

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

fn returned_ty(sig: &syn::Signature) -> Option<&syn::Type> {
    let syn::ReturnType::Type(_, ty) = &sig.output else {
        return None;
    };
    let mut ty: &syn::Type = ty;
    loop {
        let syn::Type::Path(p) = ty else {
            return Some(ty);
        };
        let last = p.path.segments.last()?;
        if !matches!(last.ident.to_string().as_str(), "Result" | "Option") {
            return Some(ty);
        }
        let syn::PathArguments::AngleBracketed(a) = &last.arguments else {
            return Some(ty);
        };
        let Some(syn::GenericArgument::Type(inner)) = a
            .args
            .iter()
            .find(|g| matches!(g, syn::GenericArgument::Type(_)))
        else {
            return Some(ty);
        };
        ty = inner;
    }
}

// ── A8 ─────────────────────────────────────────────────────────────────────

#[test]
fn the_digest_path_is_lossless() {
    const GUARDED: &[(&str, &str)] = &[
        ("artifact", "hash_tree"), // where the path bytes are actually fed
        ("artifact", "digest_tree"),
        ("artifact", "scrub"),
        ("domain::contents", "classify"), // decides WHICH files hash_tree hashes
        ("cache", "normalise"),
    ];
    const BANNED: &[&str] = &["to_string_lossy", "display().to_string"];

    // Existence and call-set are separate questions: a guarded function that delegates
    // (`digest_tree` is one line calling `hash_tree`) makes no method calls at all, so
    // an absent key in `method_calls()` means "calls nothing", not "was renamed".
    let defined: std::collections::BTreeSet<(String, String)> = signatures()
        .into_iter()
        .map(|f| (f.module, f.name))
        .collect();
    let calls = method_calls();
    let mut bad: Vec<String> = Vec::new();
    for (module, func) in GUARDED {
        assert!(
            defined.contains(&(module.to_string(), func.to_string())),
            "{module}: {func} not found — this rule is guarding a function that has been \
             renamed or moved. Repoint it at the module that now handles the path bytes."
        );
        let empty = Vec::new();
        let found = calls
            .get(&(module.to_string(), func.to_string()))
            .unwrap_or(&empty);
        for banned in BANNED {
            if found.iter().any(|c| c == banned) {
                bad.push(format!("{module}: {func} calls {banned}"));
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

    let hashing = &calls[&("artifact".to_string(), "hash_tree".to_string())];
    assert!(
        hashing.contains(&"as_encoded_bytes".to_string()),
        "hash_tree no longer feeds as_encoded_bytes: {hashing:?}\n\
         The path component of the digest must be hashed as its exact bytes."
    );
}

// ── A11 ────────────────────────────────────────────────────────────────────

const TYPESTATE_ORDER: &[&str] = &["WorkTree", "Scrubbed", "Sealed"];

#[test]
fn typestates_have_private_fields_and_consuming_transitions() {
    let mut family: Vec<String> = Vec::new();
    let mut leaks: Vec<String> = Vec::new();

    for path in rust_sources() {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        for item in parse(&path).items {
            let syn::Item::Struct(s) = item else { continue };
            if !s
                .fields
                .iter()
                .any(|f| type_name(&f.ty).ends_with("PhantomData"))
            {
                continue;
            }
            family.push(s.ident.to_string());
            for f in &s.fields {
                if is_public(&f.vis) {
                    let fname = f
                        .ident
                        .as_ref()
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "0".into());
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
            let Some(from) = rank(&type_name(&imp.self_ty)) else {
                continue;
            };
            for it in imp.items {
                let syn::ImplItem::Fn(f) = it else { continue };
                let Some(ret) = returned_ty(&f.sig) else {
                    continue;
                };
                let consumes = matches!(
                    f.sig.inputs.first(),
                    Some(syn::FnArg::Receiver(r)) if r.reference.is_none()
                );
                let forward = TYPESTATE_ORDER
                    .iter()
                    .enumerate()
                    .any(|(to, name)| to > from && mentions_type(ret, name));
                if forward && !consumes {
                    borrowing.push(format!(
                        "{file}: {}::{}",
                        type_name(&imp.self_ty),
                        f.sig.ident
                    ));
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

/// Measured: `src/` is one strongly connected component of these ten modules. Shrink-only —
/// a cut makes the rule fail as stale, and the list is then edited DOWN to what it reports.
const CYCLE_BASELINE: &[&str] = &[
    "agent_health",
    "artifact",
    "battery",
    "cache",
    "cargo_toml",
    "cli",
    "opencode",
    "session",
    "translate",
    "verify",
];

fn top_level_module(root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?;
    let head = rel.components().next()?.as_os_str().to_string_lossy();
    let name = head.strip_suffix(".rs").unwrap_or(&head).to_owned();
    (name != "lib" && name != "main").then_some(name)
}

/// How many `super`s reach the crate root from this file's own module: one from `src/m.rs`
/// and `src/m/mod.rs`, two from `src/m/x.rs`. `None` for the crate roots themselves.
fn supers_to_crate_root(root: &Path, file: &Path) -> Option<usize> {
    top_level_module(root, file)?;
    let mut comps: Vec<String> = file
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let last = comps.pop()?;
    Some(comps.len() + usize::from(last != "mod.rs"))
}

/// `::` lexes as two `Punct`s, so the separator is skipped rather than matched.
fn after_path_sep(tokens: &[TokenTree], i: usize) -> Option<(usize, &TokenTree)> {
    let mut j = i + 1;
    while matches!(tokens.get(j), Some(TokenTree::Punct(p)) if p.as_char() == ':') {
        j += 1;
    }
    if j == i + 1 {
        return None;
    }
    Some((j, tokens.get(j)?))
}

/// The names a path tail introduces: `foo` from `::foo`, `b` and `c` from `::{b, c}`.
fn head_names(tail: &TokenTree, out: &mut BTreeSet<String>) {
    match tail {
        TokenTree::Ident(m) => {
            out.insert(m.to_string());
        }
        TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
            let mut head = true;
            for token in g.stream() {
                match token {
                    TokenTree::Ident(m) if head => {
                        out.insert(m.to_string());
                        head = false;
                    }
                    TokenTree::Punct(p) if p.as_char() == ',' => head = true,
                    _ => head = false,
                }
            }
        }
        _ => {}
    }
}

/// Every `crate::<name>`, the grouped `use crate::{a, b}` included. Lexed, not searched for
/// as text: raw strings hold shell and JSON, and doc comments link modules nothing depends on.
fn crate_refs(text: &str) -> BTreeSet<String> {
    fn walk(stream: TokenStream, out: &mut BTreeSet<String>) {
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        for (i, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(g) = token {
                walk(g.stream(), out);
            }
            let TokenTree::Ident(id) = token else {
                continue;
            };
            if id != "crate" {
                continue;
            }
            if let Some((_, tail)) = after_path_sep(&tokens, i) {
                head_names(tail, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(text.parse().expect("src/ lexes as Rust tokens"), &mut out);
    out
}

/// Every `super::<name>` whose run of `super`s is long enough to land on the crate root,
/// where it names exactly what `crate::<name>` names. `super::*` is not one: it introduces
/// no name to read an edge from. An inline `mod` only ever makes a run reach less far than
/// `supers`, so counting from the file is the direction that refuses rather than misses.
///
/// Every `super` starts a run, including the second of `super::super`: a shorter run inside
/// a longer one ends at the same name, so it can only repeat a refusal, never invent one.
/// Skipping it would need to tell `::super` from the `:` of `let x: super::Foo`.
fn root_super_refs(text: &str, supers_to_root: usize) -> BTreeSet<String> {
    fn walk(stream: TokenStream, supers_to_root: usize, out: &mut BTreeSet<String>) {
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        for (i, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(g) = token {
                walk(g.stream(), supers_to_root, out);
            }
            let TokenTree::Ident(id) = token else {
                continue;
            };
            if id != "super" {
                continue;
            }
            let mut at = i;
            let mut supers = 0;
            let tail = loop {
                match after_path_sep(&tokens, at) {
                    None => break None,
                    Some((j, next)) => {
                        supers += 1;
                        match next {
                            TokenTree::Ident(m) if m == "super" => at = j,
                            _ => break Some(next),
                        }
                    }
                }
            };
            match tail {
                Some(tail) if supers >= supers_to_root => head_names(tail, out),
                _ => {}
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(
        text.parse().expect("src/ lexes as Rust tokens"),
        supers_to_root,
        &mut out,
    );
    out
}

/// The module graph, and the paths that would hide an edge from it.
struct ModuleGraph {
    edges: BTreeMap<String, BTreeSet<String>>,
    invisible: Vec<String>,
}

/// From `src/<m>.rs`, `super::X` IS `crate::X`, so an edge spelled that way is invisible to
/// `crate_refs`: a whole cycle can hide behind it, and respelling one module's refs is enough
/// to make it look like it left the cycle. Collected to be refused, not rewritten — inside
/// `src/scoring.rs`, `mod tests { use super::translate; }` means `crate::scoring::translate`,
/// so rewriting would invent an edge that is not there.
fn module_graph(root: &Path) -> ModuleGraph {
    let sources: Vec<(PathBuf, String, String)> = rust_sources_under(root)
        .into_iter()
        .filter_map(|p| top_level_module(root, &p).map(|m| (p.clone(), m, read(&p))))
        .collect();
    let mut edges: BTreeMap<String, BTreeSet<String>> = sources
        .iter()
        .map(|(_, m, _)| (m.clone(), BTreeSet::new()))
        .collect();
    let mut invisible = Vec::new();
    for (path, module, text) in &sources {
        let refs: Vec<String> = crate_refs(text)
            .into_iter()
            .filter(|r| r != module && edges.contains_key(r))
            .collect();
        edges.get_mut(module).expect("keyed above").extend(refs);
        let rel = path.strip_prefix(root).unwrap_or(path).display();
        let supers = supers_to_crate_root(root, path).expect("a module, so not a crate root");
        invisible.extend(
            root_super_refs(text, supers)
                .into_iter()
                .map(|r| format!("{rel}: super::{r}")),
        );
    }
    ModuleGraph { edges, invisible }
}

/// Components with more than one member. Takes an edge map, so a planted cycle can test it.
fn cycles(edges: &BTreeMap<String, BTreeSet<String>>) -> Vec<Vec<String>> {
    let nodes: Vec<&String> = edges.keys().collect();
    let n = nodes.len();
    let index: BTreeMap<&String, usize> = nodes.iter().enumerate().map(|(i, m)| (*m, i)).collect();
    let mut reaches = vec![vec![false; n]; n];
    for (from, tos) in edges {
        for to in tos {
            if let Some(&j) = index.get(to) {
                reaches[index[from]][j] = true;
            }
        }
    }
    for k in 0..n {
        let via = reaches[k].clone();
        for row in reaches.iter_mut() {
            if row[k] {
                for (r, v) in row.iter_mut().zip(&via) {
                    *r |= *v;
                }
            }
        }
    }
    let mut seen = vec![false; n];
    let mut out = Vec::new();
    for i in 0..n {
        if seen[i] {
            continue;
        }
        let members: Vec<usize> = (i..n).filter(|&j| reaches[i][j] && reaches[j][i]).collect();
        for &j in &members {
            seen[j] = true;
        }
        if members.len() > 1 {
            out.push(members.iter().map(|&j| nodes[j].clone()).collect());
        }
    }
    out
}

/// A shrunken cycle is not a violation; a baseline naming a module that has left one is.
fn cycle_violations(edges: &BTreeMap<String, BTreeSet<String>>, baseline: &[&str]) -> Vec<String> {
    let found = cycles(edges);
    let members: BTreeSet<&str> = found.iter().flatten().map(String::as_str).collect();
    let mut out = Vec::new();
    let largest = found.iter().map(Vec::len).max().unwrap_or(0);
    if largest > baseline.len() {
        out.push(format!(
            "the largest cycle has {largest} modules; the baseline records {}",
            baseline.len()
        ));
    }
    out.extend(
        members
            .iter()
            .filter(|m| !baseline.contains(m))
            .map(|m| format!("{m} is in a cycle the baseline does not record")),
    );
    out.extend(
        baseline
            .iter()
            .filter(|m| !members.contains(*m))
            .map(|m| format!("{m} is in no cycle any more: shrink CYCLE_BASELINE past it")),
    );
    out
}

/// A cyclic split is a nominal one: the file names promise a layering nothing enforced.
#[test]
fn a_module_cycle_may_only_shrink() {
    let graph = module_graph(&src_dir());
    assert!(
        graph.invisible.is_empty(),
        "these paths hide a module edge from the graph: {:#?}\n\
         From a module root `super::X` is `crate::X`, and the graph is built by looking for\n\
         `crate::`. Spell it `crate::<mod>` so the module graph can see the edge.",
        graph.invisible
    );
    let violations = cycle_violations(&graph.edges, CYCLE_BASELINE);
    assert!(
        violations.is_empty(),
        "{violations:#?}\ncycles now: {:#?}\n\
         Cut the edge the new member adds, or shrink CYCLE_BASELINE to exactly what this\n\
         message reports.",
        cycles(&graph.edges)
    );

    // A ring of eleven is the one violation no plantable src/ tree reaches cheaply.
    let ring: Vec<&str> = CYCLE_BASELINE.iter().copied().chain(["scoring"]).collect();
    let planted: BTreeMap<String, BTreeSet<String>> = ring
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_owned(),
                BTreeSet::from([ring[(i + 1) % ring.len()].to_owned()]),
            )
        })
        .collect();
    let caught = cycle_violations(&planted, CYCLE_BASELINE);
    assert!(
        caught
            .iter()
            .any(|v| v == "the largest cycle has 11 modules; the baseline records 10"),
        "a planted eleven-module ring did not read as bigger than the ten the baseline\n\
         records: {caught:#?}"
    );

    // Driven through the real extraction, not a synthetic edge map: the hole this rule
    // shipped with was in `crate_refs`, a layer a synthetic map never reaches.
    let tree = tempfile::tempdir().expect("tempdir");
    let src = tree.path().join("src");
    let write = |name: &str, text: &str| {
        let path = src.join(name);
        std::fs::create_dir_all(path.parent().expect("under src/")).expect("mkdir");
        std::fs::write(&path, text).expect("write");
    };
    write("lib.rs", "pub mod report;\npub mod scoring;\n");
    write("report.rs", "use crate::scoring;\n");
    write("scoring.rs", "use crate::report;\n");
    let extracted = module_graph(&src);
    let caught = cycle_violations(&extracted.edges, CYCLE_BASELINE);
    for module in ["report", "scoring"] {
        assert!(
            caught.contains(&format!(
                "{module} is in a cycle the baseline does not record"
            )),
            "extraction over a planted src/ tree missed the report <-> scoring cycle it\n\
             holds: {caught:#?} from {:#?}",
            extracted.edges
        );
    }

    write(
        "scoring.rs",
        "use super::report;\nfn f(_: super::report::T) {}\nmod tests {\n    use super::*;\n}\n",
    );
    let respelt = module_graph(&src);
    assert!(
        cycles(&respelt.edges).is_empty(),
        "respelling the edge `super::` was supposed to make it invisible to the extraction,\n\
         and the cycle is still visible: {:#?}\n\
         If `crate_refs` now reads `super::` too, this rule can compare graphs instead of\n\
         refusing the spelling.",
        respelt.edges
    );
    assert_eq!(
        respelt.invisible,
        ["scoring.rs: super::report"],
        "the invisible edge was not refused, so the rule would report the cycle shrank past\n\
         a module that is still in it. `use super::*` in the same file must stay allowed: it\n\
         introduces no name an edge can be read from."
    );
}

const PURE_LAYER: &str = "domain";

/// The names that make an edge readable, in every spelling a path can introduce one by:
/// `use std::fs::…`, a bare `fs::copy`, `std::process::Command`, `env::var`, `env!`,
/// `option_env!`, and `std::io` on a real handle. `option_env` needs its own entry because
/// it lexes as one identifier, so the `env` entry does not cover it. `tempfile` is here
/// because a pure decision needs no directory to be tested in.
const EDGE_NAMES: &[&str] = &[
    "Command",
    "File",
    "OpenOptions",
    "Stdio",
    "env",
    "fs",
    "option_env",
    "process",
    "stderr",
    "stdin",
    "stdout",
    "tempfile",
];

/// Which edge names a file introduces. Lexed rather than searched for as text, so prose in
/// a doc comment and a path inside a string literal are not hits: naming an edge in either
/// is not reaching for one.
fn edge_names(text: &str) -> BTreeSet<String> {
    fn walk(stream: TokenStream, out: &mut BTreeSet<String>) {
        for token in stream {
            match token {
                TokenTree::Group(g) => walk(g.stream(), out),
                TokenTree::Ident(id) => {
                    let name = id.to_string();
                    if EDGE_NAMES.contains(&name.as_str()) {
                        out.insert(name);
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(text.parse().expect("src/ lexes as Rust tokens"), &mut out);
    out
}

/// Takes the layer root, so a planted tree can test the extraction itself.
fn impurities(layer: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for file in rust_sources_under(layer) {
        let rel = file.strip_prefix(layer).unwrap_or(&file);
        for name in edge_names(&read(&file)) {
            out.push(format!("{}: {name}", rel.to_string_lossy()));
        }
    }
    out
}

/// "Parse at the edges; types inside" as a rule rather than a habit: a decision that
/// cannot read a file, spawn a process or look up a variable has to be handed what it
/// needs, and is then testable without a tempdir.
#[test]
fn nothing_in_the_pure_layer_names_the_filesystem_a_process_or_the_environment() {
    let layer = src_dir().join(PURE_LAYER);
    assert!(
        !rust_sources_under(&layer).is_empty(),
        "src/{PURE_LAYER}/ holds no Rust files, so this rule would report green having \
         inspected nothing."
    );
    let found = impurities(&layer);
    assert!(
        found.is_empty(),
        "the pure layer reaches for an edge: {found:#?}\n\
         Take the read, the spawn or the lookup out to a caller and pass the result in.\n\
         The scan is over identifier tokens, so it refuses the spellings that make an edge\n\
         readable — `env!` and `option_env!` among them — and cannot see access that names\n\
         none of them: `Path::is_file`, `Path::metadata`, the `include!` family\n\
         (`include_str!`, `include_bytes!`), or anything a dependency wraps. A helper that\n\
         sniffs a directory belongs in the io layer for exactly that reason."
    );

    // Through the real extraction over a planted tree, not a synthetic name list: the hole
    // the DAG rule shipped with was in its extraction, which no synthetic list reaches.
    let tree = tempfile::tempdir().expect("tempdir");
    let planted = tree.path().join(PURE_LAYER);
    std::fs::create_dir_all(&planted).expect("mkdir");
    std::fs::write(
        planted.join("contents.rs"),
        "use std::fs;\npub fn f(p: &std::path::Path) -> bool {\n    \
         fs::read(p).is_ok() && !env!(\"CARGO_PKG_NAME\").is_empty()\n}\n",
    )
    .expect("write");
    std::fs::write(
        planted.join("outcome.rs"),
        "pub fn f() -> bool { option_env!(\"HOME\").is_some() }\n",
    )
    .expect("write");
    std::fs::write(
        planted.join("relpath.rs"),
        "pub fn pure(p: &std::path::Path) -> bool { p.is_relative() }\n",
    )
    .expect("write");
    assert_eq!(
        impurities(&planted),
        [
            "contents.rs: env",
            "contents.rs: fs",
            "outcome.rs: option_env"
        ],
        "a planted domain/ file naming std::fs and env! was not reported, so this rule\n\
         cannot fail — and the file beside it that names neither must not be reported.\n\
         `option_env!` is asserted from a file that names nothing else, because it lexes as\n\
         the identifier `option_env` and so is not covered by the entry `env!` matches."
    );
}
