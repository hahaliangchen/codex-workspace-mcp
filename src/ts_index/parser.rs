use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use swc_common::{FileName, SourceMap, Span, comments::SingleThreadedComments, sync::Lrc};
use swc_ecma_ast::*;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_visit::{Visit, VisitWith};

use crate::ts_index::*;

pub(crate) fn build_index(root: &Path) -> Result<(usize, usize)> {
    let mut files_indexed = 0;
    let mut symbols_indexed = 0;

    let mut conn = crate::database::init_db(root).unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute(
        "DELETE FROM ts_symbols WHERE workspace_root = ?",
        rusqlite::params![root.to_string_lossy()],
    )
    .unwrap();

    for entry in walk_source_files(root) {
        let path = match entry {
            Ok(p) => p,
            Err(_) => continue,
        };
        if path.extension().and_then(|value| value.to_str()) == Some("js")
            || path.extension().and_then(|value| value.to_str()) == Some("jsx")
            || path.extension().and_then(|value| value.to_str()) == Some("ts")
            || path.extension().and_then(|value| value.to_str()) == Some("tsx")
        {
            let metadata = std::fs::metadata(&path)?;
            if metadata.len() > MAX_TS_FILE_BYTES {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            files_indexed += 1;
            let parsed = parse_ts_file(root, &path, &content);
            let mut symbols_to_insert = parsed.symbols;
            if symbols_to_insert.is_empty() && !parsed.re_exports.is_empty() {
                let file_path_str = relative_display(root, &path);
                let id = format!("ts:{}:<file-facade>:1", file_path_str);
                symbols_to_insert.push(TsSymbol {
                    id,
                    name: "<file-facade>".to_string(),
                    kind: TsSymbolKind::Component,
                    file_path: file_path_str,
                    scope_path: String::new(),
                    parent_id: None,
                    start_line: 1,
                    end_line: 1,
                    signature: String::new(),
                    docstring: String::new(),
                    export: false,
                    export_names: Vec::new(),
                    calls: Vec::new(),
                    import_bindings: Vec::new(),
                    imports: Vec::new(),
                    re_exports: parsed.re_exports.clone(),
                });
            }
            for mut sym in symbols_to_insert {
                sym.re_exports = parsed.re_exports.clone(); // Attach re_exports to symbol
                let export_names_json =
                    serde_json::to_string(&sym.export_names).unwrap_or_default();
                let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();
                let import_bindings_json =
                    serde_json::to_string(&sym.import_bindings).unwrap_or_default();
                let imports_json = serde_json::to_string(&sym.imports).unwrap_or_default();
                let kind = serde_json::to_string(&sym.kind)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string();
                let re_exports_json = serde_json::to_string(&sym.re_exports).unwrap_or_default();

                tx.execute(
                    "INSERT INTO ts_symbols (
                        id, workspace_root, name, kind, file_path, scope_path, parent_id, start_line, end_line,
                        signature, docstring, export, export_names_json, calls_json, import_bindings_json, imports_json, re_exports_json
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    rusqlite::params![
                        sym.id, root.to_string_lossy(), sym.name, kind, sym.file_path, sym.scope_path, sym.parent_id,
                        sym.start_line, sym.end_line, sym.signature, sym.docstring, if sym.export { 1 } else { 0 },
                        export_names_json, calls_json, import_bindings_json, imports_json, re_exports_json
                    ]
                ).unwrap();
                symbols_indexed += 1;
            }
        }
    }
    tx.commit().unwrap();
    // Bug3: 记录本次索引的实际时间戳
    let ts = crate::rust_index::now_unix();
    let meta_conn = crate::database::init_db(root).unwrap();
    crate::database::upsert_index_metadata(&meta_conn, &root.to_string_lossy(), "ts", ts).unwrap();
    Ok((files_indexed, symbols_indexed))
}

pub(crate) struct ParsedTsFile {
    symbols: Vec<TsSymbol>,
    re_exports: Vec<TsReExport>,
}

pub(crate) fn parse_ts_file(root: &Path, path: &Path, content: &str) -> ParsedTsFile {
    let relative_path = relative_display(root, path);
    let comments = SingleThreadedComments::default();
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        FileName::Real(path.to_path_buf()).into(),
        content.to_string(),
    );
    let syntax = Syntax::Typescript(TsSyntax {
        tsx: matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("tsx" | "jsx")
        ),
        decorators: true,
        ..Default::default()
    });
    let lexer = Lexer::new(
        syntax,
        Default::default(),
        StringInput::from(&*fm),
        Some(&comments),
    );
    let mut parser = Parser::new_from(lexer);
    let module = match parser.parse_module() {
        Ok(module) => module,
        Err(_) => {
            return ParsedTsFile {
                symbols: Vec::new(),
                re_exports: Vec::new(),
            };
        }
    };
    let mut collector = TsCollector::new(relative_path, content, cm);
    collector.collect(&module);
    ParsedTsFile {
        symbols: collector.symbols,
        re_exports: collector.re_exports,
    }
}

pub(crate) struct TsCollector {
    file_path: String,
    content: String,
    cm: Lrc<SourceMap>,
    symbols: Vec<TsSymbol>,
    re_exports: Vec<TsReExport>,
    import_bindings: Vec<TsImport>,
    imports: Vec<String>,
    index_by_name: BTreeMap<String, Vec<String>>,
    scope_stack: Vec<String>,
    parent_stack: Vec<Option<String>>,
    /// Bool flag for each scope: whether we want calls inside it to attach to the
    /// scope's symbol. True for functions / methods / arrows / classes that hold a body.
    scope_collects_calls: Vec<bool>,
    /// Maps symbol id to its index in `symbols`, so visit_call_expr can append calls.
    id_to_index: BTreeMap<String, usize>,
    /// Toplevel ModuleItems are visited first to learn which decls are exported.
    /// We collect those Spans here so visit_*_decl knows to mark `export = true`.
    exported_spans: BTreeSet<swc_common::BytePos>,
    default_exported_spans: BTreeSet<swc_common::BytePos>,
}

impl TsCollector {
    fn new(file_path: String, content: &str, cm: Lrc<SourceMap>) -> Self {
        Self {
            file_path,
            content: content.to_string(),
            cm,
            symbols: Vec::new(),
            re_exports: Vec::new(),
            import_bindings: Vec::new(),
            imports: Vec::new(),
            index_by_name: BTreeMap::new(),
            scope_stack: Vec::new(),
            parent_stack: Vec::new(),
            scope_collects_calls: Vec::new(),
            id_to_index: BTreeMap::new(),
            exported_spans: BTreeSet::new(),
            default_exported_spans: BTreeSet::new(),
        }
    }

    fn collect(&mut self, module: &Module) {
        // Pass 1: walk top-level ModuleItems to record imports / re-exports and
        // remember which decls are exported (so visit_*_decl can set `export = true`).
        for item in &module.body {
            match item {
                ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => self.collect_import(import),
                ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => {
                    self.mark_exported(&export.decl);
                }
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(export)) => {
                    match &export.decl {
                        DefaultDecl::Fn(func) => {
                            self.default_exported_spans.insert(func.function.span.lo);
                        }
                        DefaultDecl::Class(class) => {
                            self.default_exported_spans.insert(class.class.span.lo);
                        }
                        _ => {}
                    }
                }
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(expr)) => {
                    if let Expr::Arrow(arrow) = &*expr.expr {
                        self.default_exported_spans.insert(arrow.span.lo);
                    }
                }
                ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(export)) => {
                    self.collect_named_export(export);
                }
                ModuleItem::ModuleDecl(ModuleDecl::ExportAll(export)) => {
                    let source = export.src.value.to_string_lossy().to_string();
                    self.imports.push(source.clone());
                    self.re_exports.push(TsReExport {
                        file_path: self.file_path.clone(),
                        source,
                        local_name: "*".to_string(),
                        exported_name: "*".to_string(),
                        kind: TsImportKind::Namespace,
                        type_only: export.type_only,
                    });
                }
                _ => {}
            }
        }

        // Pass 2: deep walk via swc Visit — handles nested fn/class/method/arrow,
        // including HOC patterns and class expressions.
        module.visit_with(self);
    }

    fn mark_exported(&mut self, decl: &Decl) {
        match decl {
            Decl::Fn(func) => {
                self.exported_spans.insert(func.function.span.lo);
            }
            Decl::Class(class) => {
                self.exported_spans.insert(class.class.span.lo);
            }
            Decl::Var(var) => {
                for declarator in &var.decls {
                    self.exported_spans.insert(declarator.span.lo);
                }
            }
            Decl::TsInterface(interface) => {
                self.exported_spans.insert(interface.span.lo);
            }
            Decl::TsTypeAlias(alias) => {
                self.exported_spans.insert(alias.span.lo);
            }
            Decl::TsEnum(enum_decl) => {
                self.exported_spans.insert(enum_decl.span.lo);
            }
            _ => {}
        }
    }

    fn scope_path(&self) -> String {
        self.scope_stack.join(".")
    }

    fn current_parent_id(&self) -> Option<String> {
        self.parent_stack.last().cloned().flatten()
    }

    /// Push a symbol's scope (so nested visits see this symbol as their parent),
    /// run the callback, then pop. `collects_calls` controls whether calls found
    /// inside this scope should be attached to the symbol (true for functions /
    /// methods / arrows; false for classes / enums / interfaces).
    fn enter_scope<F: FnOnce(&mut Self)>(
        &mut self,
        name: String,
        parent_id: Option<String>,
        collects_calls: bool,
        f: F,
    ) {
        self.scope_stack.push(name);
        self.parent_stack.push(parent_id);
        self.scope_collects_calls.push(collects_calls);
        f(self);
        self.scope_collects_calls.pop();
        self.parent_stack.pop();
        self.scope_stack.pop();
    }

    fn push_symbol(
        &mut self,
        name: &str,
        kind: TsSymbolKind,
        span: Span,
        signature_span: Span,
        explicit_export: bool,
        default_export: bool,
    ) -> String {
        let scope_path = self.scope_path();
        let parent_id = self.current_parent_id();
        let start_line = line_of(&self.cm, span.lo);
        let end_line = line_of(&self.cm, span.hi);
        let signature = self.signature_snippet(signature_span);
        let docstring = self.docstring_before(span);
        let id = ts_symbol_id(&self.file_path, &scope_path, name, start_line);
        let export = explicit_export || default_export;
        let export_names_val = if default_export {
            default_export_names()
        } else {
            export_names(name, export)
        };
        self.index_by_name
            .entry(name.to_string())
            .or_default()
            .push(id.clone());
        self.id_to_index.insert(id.clone(), self.symbols.len());
        self.symbols.push(TsSymbol {
            id: id.clone(),
            name: name.to_string(),
            kind,
            file_path: self.file_path.clone(),
            scope_path,
            parent_id,
            start_line,
            end_line,
            signature,
            docstring,
            export,
            export_names: export_names_val,
            calls: Vec::new(),
            import_bindings: self.import_bindings.clone(),
            imports: self.imports.clone(),
            re_exports: Vec::new(),
        });
        id
    }

    fn collect_import(&mut self, import: &ImportDecl) {
        let source = import.src.value.to_string_lossy().to_string();
        self.imports.push(source.clone());
        if import.specifiers.is_empty() {
            self.import_bindings.push(TsImport {
                source,
                local_name: String::new(),
                imported_name: String::new(),
                kind: TsImportKind::SideEffect,
                type_only: import.type_only,
            });
            return;
        }

        for specifier in &import.specifiers {
            match specifier {
                ImportSpecifier::Named(named) => {
                    let local_name = named.local.sym.to_string();
                    let imported_name = named
                        .imported
                        .as_ref()
                        .map(module_export_name)
                        .unwrap_or_else(|| local_name.clone());
                    self.import_bindings.push(TsImport {
                        source: source.clone(),
                        local_name,
                        imported_name,
                        kind: TsImportKind::Named,
                        type_only: import.type_only || named.is_type_only,
                    });
                }
                ImportSpecifier::Default(default) => {
                    self.import_bindings.push(TsImport {
                        source: source.clone(),
                        local_name: default.local.sym.to_string(),
                        imported_name: "default".to_string(),
                        kind: TsImportKind::Default,
                        type_only: import.type_only,
                    });
                }
                ImportSpecifier::Namespace(namespace) => {
                    self.import_bindings.push(TsImport {
                        source: source.clone(),
                        local_name: namespace.local.sym.to_string(),
                        imported_name: "*".to_string(),
                        kind: TsImportKind::Namespace,
                        type_only: import.type_only,
                    });
                }
            }
        }
    }

    fn collect_named_export(&mut self, export: &NamedExport) {
        let Some(source) = export.src.as_ref() else {
            return;
        };
        let source = source.value.to_string_lossy().to_string();
        self.imports.push(source.clone());
        for specifier in &export.specifiers {
            match specifier {
                ExportSpecifier::Named(named) => {
                    let imported_name = module_export_name(&named.orig);
                    let local_name = named
                        .exported
                        .as_ref()
                        .map(module_export_name)
                        .unwrap_or_else(|| imported_name.clone());
                    self.import_bindings.push(TsImport {
                        source: source.clone(),
                        local_name: local_name.clone(),
                        imported_name: imported_name.clone(),
                        kind: TsImportKind::Named,
                        type_only: export.type_only || named.is_type_only,
                    });
                    self.re_exports.push(TsReExport {
                        file_path: self.file_path.clone(),
                        source: source.clone(),
                        local_name: imported_name,
                        exported_name: local_name,
                        kind: TsImportKind::Named,
                        type_only: export.type_only || named.is_type_only,
                    });
                }
                ExportSpecifier::Namespace(namespace) => {
                    let local_name = module_export_name(&namespace.name);
                    self.import_bindings.push(TsImport {
                        source: source.clone(),
                        local_name: local_name.clone(),
                        imported_name: "*".to_string(),
                        kind: TsImportKind::Namespace,
                        type_only: export.type_only,
                    });
                    self.re_exports.push(TsReExport {
                        file_path: self.file_path.clone(),
                        source: source.clone(),
                        local_name: "*".to_string(),
                        exported_name: local_name,
                        kind: TsImportKind::Namespace,
                        type_only: export.type_only,
                    });
                }
                ExportSpecifier::Default(default) => {
                    let local_name = default.exported.sym.to_string();
                    self.import_bindings.push(TsImport {
                        source: source.clone(),
                        local_name: local_name.clone(),
                        imported_name: "default".to_string(),
                        kind: TsImportKind::Default,
                        type_only: export.type_only,
                    });
                    self.re_exports.push(TsReExport {
                        file_path: self.file_path.clone(),
                        source: source.clone(),
                        local_name: "default".to_string(),
                        exported_name: local_name,
                        kind: TsImportKind::Default,
                        type_only: export.type_only,
                    });
                }
            }
        }
    }

    fn record_call_for_current_scope(&mut self, call: &CallExpr) {
        let Some(parent_id) = self.current_parent_id() else {
            return;
        };
        if !self.scope_collects_calls.last().copied().unwrap_or(false) {
            return;
        }
        let Some(idx) = self.id_to_index.get(&parent_id).copied() else {
            return;
        };
        let Some((namespace, target_text)) = describe_callee(&call.callee) else {
            return;
        };
        let line = line_of(&self.cm, call.span.lo);
        let snippet_line = self
            .content
            .lines()
            .nth(line.saturating_sub(1))
            .map(|line| line.trim().to_string())
            .unwrap_or_default();
        self.symbols[idx].calls.push(TsCall {
            namespace,
            target_text,
            line,
            snippet: snippet_line,
        });
    }

    fn arrow_kind(&self, name: &str) -> TsSymbolKind {
        if looks_like_component(name) {
            TsSymbolKind::Component
        } else {
            TsSymbolKind::ArrowFunction
        }
    }
    fn signature_snippet(&self, span: Span) -> String {
        snippet(&self.content, &self.cm, span, 5)
    }

    fn docstring_before(&self, span: Span) -> String {
        let start = line_of(&self.cm, span.lo);
        let mut docs = Vec::new();
        let lines: Vec<&str> = self.content.lines().collect();
        let mut current = start.saturating_sub(1);
        while current > 0 {
            current -= 1;
            let line = lines.get(current).copied().unwrap_or("").trim();
            if line.is_empty() {
                break;
            }
            if let Some(text) = line.strip_prefix("//") {
                docs.push(text.trim().to_string());
                continue;
            }
            if line.starts_with("/*") {
                docs.push(line.trim_matches(&['/', '*', ' '][..]).to_string());
            }
            break;
        }
        docs.reverse();
        docs.join("\n")
    }
}

impl Visit for TsCollector {
    fn visit_fn_decl(&mut self, node: &FnDecl) {
        let name = node.ident.sym.to_string();
        let export = self.exported_spans.contains(&node.function.span.lo);
        let id = self.push_symbol(
            &name,
            TsSymbolKind::Function,
            node.function.span,
            node.function.span,
            export,
            false,
        );
        self.enter_scope(name, Some(id), true, |this| {
            node.function.visit_children_with(this);
        });
    }

    fn visit_fn_expr(&mut self, node: &FnExpr) {
        let name = node
            .ident
            .as_ref()
            .map(|i| i.sym.to_string())
            .unwrap_or_else(|| "<anonymous-fn>".to_string());
        let default_export = self.default_exported_spans.contains(&node.function.span.lo);
        let id = self.push_symbol(
            &name,
            TsSymbolKind::Function,
            node.function.span,
            node.function.span,
            false,
            default_export,
        );
        self.enter_scope(name, Some(id), true, |this| {
            node.function.visit_children_with(this);
        });
    }

    fn visit_class_decl(&mut self, node: &ClassDecl) {
        let name = node.ident.sym.to_string();
        let export = self.exported_spans.contains(&node.class.span.lo);
        let id = self.push_symbol(
            &name,
            TsSymbolKind::Class,
            node.class.span,
            node.class.span,
            export,
            false,
        );
        // Class scope: calls inside the class declaration itself (decorators, computed
        // keys) are not attached. Method bodies push their own scope with collects_calls=true.
        self.enter_scope(name, Some(id), false, |this| {
            node.class.visit_children_with(this);
        });
    }

    fn visit_class_expr(&mut self, node: &ClassExpr) {
        // Named class expression `const X = class extends ... {}` is handled in
        // visit_var_declarator (so the var name becomes scope). Truly anonymous class
        // expressions or `return class extends ... {}` fall here.
        let name = node
            .ident
            .as_ref()
            .map(|i| i.sym.to_string())
            .unwrap_or_else(|| "<anonymous-class>".to_string());
        let default_export = self.default_exported_spans.contains(&node.class.span.lo);
        let id = self.push_symbol(
            &name,
            TsSymbolKind::Class,
            node.class.span,
            node.class.span,
            false,
            default_export,
        );
        self.enter_scope(name, Some(id), false, |this| {
            node.class.visit_children_with(this);
        });
    }

    fn visit_class_method(&mut self, node: &ClassMethod) {
        let Some(name) = prop_name_text(&node.key) else {
            node.visit_children_with(self);
            return;
        };
        let id = self.push_symbol(
            &name,
            TsSymbolKind::Method,
            node.span,
            node.span,
            false,
            false,
        );
        self.enter_scope(name, Some(id), true, |this| {
            node.function.visit_children_with(this);
        });
    }

    fn visit_class_prop(&mut self, node: &ClassProp) {
        // Arrow-function class properties: `foo = (x) => {...}` — record as Method.
        let Some(name) = prop_name_text(&node.key) else {
            node.visit_children_with(self);
            return;
        };
        let Some(value) = node.value.as_deref() else {
            node.visit_children_with(self);
            return;
        };
        match value {
            Expr::Arrow(arrow) => {
                let id = self.push_symbol(
                    &name,
                    TsSymbolKind::Method,
                    node.span,
                    arrow.span,
                    false,
                    false,
                );
                self.enter_scope(name, Some(id), true, |this| {
                    arrow.visit_children_with(this);
                });
            }
            Expr::Fn(fn_expr) => {
                let id = self.push_symbol(
                    &name,
                    TsSymbolKind::Method,
                    node.span,
                    fn_expr.function.span,
                    false,
                    false,
                );
                self.enter_scope(name, Some(id), true, |this| {
                    fn_expr.function.visit_children_with(this);
                });
            }
            _ => node.visit_children_with(self),
        }
    }

    fn visit_var_declarator(&mut self, node: &VarDeclarator) {
        if let (Pat::Ident(binding), Some(init)) = (&node.name, node.init.as_deref()) {
            let name = binding.id.sym.to_string();
            let export = self.exported_spans.contains(&node.span.lo);
            match init {
                Expr::Arrow(arrow) => {
                    let kind = self.arrow_kind(&name);
                    let id = self.push_symbol(&name, kind, arrow.span, arrow.span, export, false);
                    self.enter_scope(name, Some(id), true, |this| {
                        arrow.visit_children_with(this);
                    });
                    return;
                }
                Expr::Class(class_expr) => {
                    let id = self.push_symbol(
                        &name,
                        TsSymbolKind::Class,
                        class_expr.class.span,
                        class_expr.class.span,
                        export,
                        false,
                    );
                    self.enter_scope(name, Some(id), false, |this| {
                        class_expr.class.visit_children_with(this);
                    });
                    return;
                }
                Expr::Fn(fn_expr) => {
                    let id = self.push_symbol(
                        &name,
                        TsSymbolKind::Function,
                        fn_expr.function.span,
                        fn_expr.function.span,
                        export,
                        false,
                    );
                    self.enter_scope(name, Some(id), true, |this| {
                        fn_expr.function.visit_children_with(this);
                    });
                    return;
                }
                _ => {
                    self.push_symbol(
                        &name,
                        TsSymbolKind::Const,
                        node.span,
                        node.span,
                        export,
                        false,
                    );
                }
            }
        }
        node.visit_children_with(self);
    }

    fn visit_ts_interface_decl(&mut self, node: &TsInterfaceDecl) {
        let name = node.id.sym.to_string();
        let export = self.exported_spans.contains(&node.span.lo);
        self.push_symbol(
            &name,
            TsSymbolKind::Interface,
            node.span,
            node.span,
            export,
            false,
        );
        node.visit_children_with(self);
    }

    fn visit_ts_type_alias_decl(&mut self, node: &TsTypeAliasDecl) {
        let name = node.id.sym.to_string();
        let export = self.exported_spans.contains(&node.span.lo);
        self.push_symbol(
            &name,
            TsSymbolKind::TypeAlias,
            node.span,
            node.span,
            export,
            false,
        );
        node.visit_children_with(self);
    }

    fn visit_ts_enum_decl(&mut self, node: &TsEnumDecl) {
        let name = node.id.sym.to_string();
        let export = self.exported_spans.contains(&node.span.lo);
        self.push_symbol(
            &name,
            TsSymbolKind::Enum,
            node.span,
            node.span,
            export,
            false,
        );
        node.visit_children_with(self);
    }

    fn visit_arrow_expr(&mut self, node: &ArrowExpr) {
        // Only register a symbol here for `export default () => {}`. Otherwise
        // the arrow is wrapped by a var_declarator / class_prop / call argument
        // which already handles naming. Just descend so nested decls inside the
        // arrow body remain visible.
        if self.default_exported_spans.contains(&node.span.lo) {
            let name = "defaultExport";
            let kind = self.arrow_kind(name);
            let id = self.push_symbol(name, kind, node.span, node.span, false, true);
            self.enter_scope(name.to_string(), Some(id), true, |this| {
                node.visit_children_with(this);
            });
            return;
        }
        node.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, node: &CallExpr) {
        self.record_call_for_current_scope(node);
        node.visit_children_with(self);
    }
}

pub(crate) fn prop_name_text(key: &PropName) -> Option<String> {
    match key {
        PropName::Ident(ident) => Some(ident.sym.to_string()),
        PropName::Str(s) => Some(s.value.to_string_lossy().to_string()),
        PropName::Num(n) => Some(n.value.to_string()),
        PropName::BigInt(b) => Some(b.value.to_string()),
        PropName::Computed(_) => None,
    }
}

pub(crate) fn describe_callee(callee: &Callee) -> Option<(Option<String>, String)> {
    let expr = match callee {
        Callee::Expr(expr) => expr,
        _ => return None,
    };
    match expr.as_ref() {
        Expr::Ident(ident) => Some((None, ident.sym.to_string())),
        Expr::Member(member) => {
            let target_text = match &member.prop {
                MemberProp::Ident(ident) => ident.sym.to_string(),
                MemberProp::PrivateName(p) => format!("#{}", p.name),
                MemberProp::Computed(_) => return None,
            };
            let namespace = expr_to_text(&member.obj);
            Some((namespace, target_text))
        }
        _ => None,
    }
}

pub(crate) fn expr_to_text(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Ident(ident) => Some(ident.sym.to_string()),
        Expr::This(_) => Some("this".to_string()),
        Expr::Member(member) => {
            let head = expr_to_text(&member.obj)?;
            let tail = match &member.prop {
                MemberProp::Ident(ident) => ident.sym.to_string(),
                MemberProp::PrivateName(p) => format!("#{}", p.name),
                MemberProp::Computed(_) => return None,
            };
            Some(format!("{head}.{tail}"))
        }
        _ => None,
    }
}
