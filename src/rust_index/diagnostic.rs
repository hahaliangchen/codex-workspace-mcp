use proc_macro2::LineColumn;
use quote::ToTokens;
use syn::visit::Visit;
use syn::{ImplItemFn, ItemEnum, ItemFn, ItemImpl, ItemStruct};

use crate::sandbox_diagnostic::AstNodeSnippet;

struct NodeVisitor {
    target_line: usize,
    best_match: Option<AstNodeSnippet>,
    min_range: usize,
}

impl NodeVisitor {
    fn new(line: usize) -> Self {
        Self {
            target_line: line,
            best_match: None,
            min_range: usize::MAX,
        }
    }

    fn check_span(
        &mut self,
        start: LineColumn,
        end: LineColumn,
        kind: &str,
        signature: &str,
        body: &str,
    ) {
        if self.target_line >= start.line && self.target_line <= end.line {
            let range = end.line - start.line;
            if range < self.min_range {
                self.min_range = range;
                self.best_match = Some(AstNodeSnippet {
                    kind: kind.to_string(),
                    signature: signature.to_string(),
                    body: body.to_string(),
                });
            }
        }
    }
}

impl<'ast> Visit<'ast> for NodeVisitor {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        use syn::spanned::Spanned;
        let start = node.span().start();
        let end = node.span().end();

        let sig = node.sig.to_token_stream().to_string();
        let body = node.to_token_stream().to_string();
        self.check_span(start, end, "Function", &sig, &body);

        syn::visit::visit_item_fn(self, node);
    }

    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        use syn::spanned::Spanned;
        let start = node.span().start();
        let end = node.span().end();

        let sig = format!("struct {}", node.ident);
        let body = node.to_token_stream().to_string();
        self.check_span(start, end, "Struct", &sig, &body);

        syn::visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast ItemEnum) {
        use syn::spanned::Spanned;
        let start = node.span().start();
        let end = node.span().end();

        let sig = format!("enum {}", node.ident);
        let body = node.to_token_stream().to_string();
        self.check_span(start, end, "Enum", &sig, &body);

        syn::visit::visit_item_enum(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        use syn::spanned::Spanned;
        let start = node.span().start();
        let end = node.span().end();

        let sig = format!("impl {}", node.self_ty.to_token_stream());
        let body = node.to_token_stream().to_string();
        self.check_span(start, end, "Impl", &sig, &body);

        syn::visit::visit_item_impl(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        use syn::spanned::Spanned;
        let start = node.span().start();
        let end = node.span().end();

        let sig = node.sig.to_token_stream().to_string();
        let body = node.to_token_stream().to_string();
        self.check_span(start, end, "Method", &sig, &body);

        syn::visit::visit_impl_item_fn(self, node);
    }
}

pub fn diagnose_ast_error(
    line: usize,
    _column: usize,
    source_code: &str,
) -> Option<AstNodeSnippet> {
    let file = syn::parse_file(source_code).ok()?;

    let mut visitor = NodeVisitor::new(line);
    visitor.visit_file(&file);

    visitor.best_match
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnose_ast_error_function() {
        let code = r#"
fn unrelated() {}

fn target_function() {
    let x = 1; // Error line 5
    let y = 2;
}

fn another() {}
"#;
        let snippet = diagnose_ast_error(5, 5, code).unwrap();
        assert_eq!(snippet.kind, "Function");
        assert!(snippet.signature.contains("target_function"));
        assert!(snippet.body.contains("let x = 1 ;"));
    }

    #[test]
    fn test_diagnose_ast_error_method_inside_impl() {
        let code = r#"
struct MyStruct;

impl MyStruct {
    fn method_a() {
        // Line 6
    }
    fn method_b() { // Line 8
        broken_code(); // Line 9
    }
}
"#;
        // Looking at line 9 should extract `method_b`, not the whole `impl MyStruct`
        // because `method_b` has a smaller line range.
        let snippet = diagnose_ast_error(9, 9, code).unwrap();
        assert_eq!(snippet.kind, "Method");
        assert!(snippet.signature.contains("method_b"));
        assert!(!snippet.body.contains("method_a"));
    }
}
