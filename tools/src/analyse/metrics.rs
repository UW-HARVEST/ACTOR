use std::path::Path;

/// Counted via AST (`syn`), not text matching.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UnsafeCounts {
    pub blocks: usize,
    pub fns: usize,
    pub impls: usize,
    /// Total lines inside unsafe blocks/fns/impls.
    pub lines: usize,
}

pub fn count_unsafe(src_dir: &Path) -> UnsafeCounts {
    let mut counts = UnsafeCounts::default();
    let Ok(entries) = std::fs::read_dir(src_dir) else {
        return counts;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            if name == "bin" || name == "tests" {
                continue;
            }
            let sub = count_unsafe(&path);
            counts.blocks += sub.blocks;
            counts.fns += sub.fns;
            counts.impls += sub.impls;
            counts.lines += sub.lines;
        } else if path.extension().is_some_and(|x| x == "rs") {
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(file) = syn::parse_file(&src) else {
                continue;
            };
            let mut v = UnsafeVisitor::default();
            syn::visit::visit_file(&mut v, &file);
            counts.blocks += v.blocks;
            counts.fns += v.fns;
            counts.impls += v.impls;
            counts.lines += v.lines;
        }
    }
    counts
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LocCounts {
    /// Non-blank, non-comment lines: the LOC definition the paper reports.
    pub code: usize,
}

pub fn count_loc(src_dir: &Path) -> LocCounts {
    let mut counts = LocCounts::default();
    let Ok(entries) = std::fs::read_dir(src_dir) else {
        return counts;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            if name == "bin" || name == "tests" {
                continue;
            }
            counts.code += count_loc(&path).code;
        } else if path.extension().is_some_and(|x| x == "rs") {
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            counts.code += src
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with("//")
                })
                .count();
        }
    }
    counts
}

#[derive(Default)]
struct UnsafeVisitor {
    blocks: usize,
    fns: usize,
    impls: usize,
    lines: usize,
}

fn span_lines(open: proc_macro2::Span, close: proc_macro2::Span) -> usize {
    let start = open.start().line;
    let end = close.end().line;
    if end >= start {
        end - start + 1
    } else {
        1
    }
}

impl<'ast> syn::visit::Visit<'ast> for UnsafeVisitor {
    fn visit_expr_unsafe(&mut self, node: &'ast syn::ExprUnsafe) {
        self.blocks += 1;
        let b = node.block.brace_token;
        self.lines += span_lines(b.span.open(), b.span.close());
        syn::visit::visit_expr_unsafe(self, node);
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if node.sig.unsafety.is_some() {
            self.fns += 1;
            let b = node.block.brace_token;
            self.lines += span_lines(b.span.open(), b.span.close());
        }
        syn::visit::visit_item_fn(self, node);
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if node.sig.unsafety.is_some() {
            self.fns += 1;
            let b = node.block.brace_token;
            self.lines += span_lines(b.span.open(), b.span.close());
        }
        syn::visit::visit_impl_item_fn(self, node);
    }
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if node.unsafety.is_some() {
            self.impls += 1;
            let b = node.brace_token;
            self.lines += span_lines(b.span.open(), b.span.close());
        }
        syn::visit::visit_item_impl(self, node);
    }
}
