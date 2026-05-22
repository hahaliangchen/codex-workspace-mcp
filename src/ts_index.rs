use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use swc_common::{FileName, SourceMap, Span, comments::SingleThreadedComments, sync::Lrc};
use swc_ecma_ast::*;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_visit::{Visit, VisitWith};

const INDEX_DIR: &str = ".codex-workspace-mcp";
const INDEX_FILE: &str = "ts_index.json";
const MAX_TS_FILE_BYTES: u64 = 2 * 1024 * 1024;
const NOISE_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".venv",
    "venv",
    "__pycache__",
    ".codex-workspace-mcp",
];

#[derive(Debug, thiserror::Error)]
pub enum TsIndexError {
    #[error("ts index not found; call index_ts_workspace first")]
    MissingIndex,
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, TsIndexError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsIndex {
    pub workspace_root: String,
    pub generated_at_unix: u64,
    pub files_indexed: usize,
    pub symbols: Vec<TsSymbol>,
    #[serde(default)]
    pub re_exports: Vec<TsReExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsSymbol {
    pub id: String,
    pub name: String,
    pub kind: TsSymbolKind,
    pub file_path: String,
    #[serde(default)]
    pub scope_path: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub docstring: String,
    pub export: bool,
    #[serde(default)]
    pub export_names: Vec<String>,
    pub calls: Vec<TsCall>,
    #[serde(default)]
    pub import_bindings: Vec<TsImport>,
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TsSymbolKind {
    Function,
    ArrowFunction,
    Class,
    Method,
    Interface,
    TypeAlias,
    Enum,
    Const,
    Component,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsCall {
    #[serde(default)]
    pub namespace: Option<String>,
    pub target_text: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsReExport {
    pub file_path: String,
    pub source: String,
    pub local_name: String,
    pub exported_name: String,
    pub kind: TsImportKind,
    pub type_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TsImportKind {
    Named,
    Default,
    Namespace,
    SideEffect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsImport {
    pub source: String,
    pub local_name: String,
    pub imported_name: String,
    pub kind: TsImportKind,
    pub type_only: bool,
}

#[derive(Debug, Deserialize)]
pub struct IndexTsWorkspaceRequest {
    pub workspace_root: String,
}

#[derive(Debug, Serialize)]
pub struct IndexTsWorkspaceResponse {
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct TsIndexStatus {
    pub index_path: String,
    pub exists: bool,
    pub workspace_root: String,
    pub generated_at_unix: Option<u64>,
    pub files_indexed: Option<usize>,
    pub symbols_indexed: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ListTsSymbolsRequest {
    pub workspace_root: String,
    pub file_path: Option<String>,
    pub kind: Option<TsSymbolKind>,
}

#[derive(Debug, Serialize)]
pub struct ListTsSymbolsResponse {
    pub symbols: Vec<TsSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct SearchTsSymbolsRequest {
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchTsSymbolsResponse {
    pub query: String,
    pub matches: Vec<TsSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct ReadTsSymbolRequest {
    pub workspace_root: String,
    pub symbol_id: String,
    #[serde(default)]
    pub include_context: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadTsSymbolResponse {
    pub symbol: TsSymbol,
    pub content: String,
    pub callees: Vec<TsCallee>,
    pub callers: Vec<TsCaller>,
    pub resolved_imports: Vec<TsResolvedImport>,
    pub suggested_reads: Vec<TsSuggestedRead>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsSymbolSummary {
    pub id: String,
    pub name: String,
    pub kind: TsSymbolKind,
    pub file_path: String,
    pub scope_path: String,
    pub parent_id: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub docstring: String,
    pub export: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsCallee {
    pub target_text: String,
    pub line: usize,
    pub snippet: String,
    pub matched_symbol_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsCaller {
    pub symbol_id: String,
    pub name: String,
    pub file_path: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsResolvedImport {
    pub source: String,
    pub local_name: String,
    pub imported_name: String,
    pub kind: TsImportKind,
    pub target_file_path: Option<String>,
    pub matched_symbol_ids: Vec<String>,
    pub re_export_chain: Vec<TsExportChainStep>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsExportChainStep {
    pub file_path: String,
    pub source: String,
    pub imported_name: String,
    pub local_name: String,
    pub kind: TsImportKind,
    pub target_file_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsSuggestedRead {
    pub reason: String,
    pub trigger_call: String,
    pub trigger_line: usize,
    pub trigger_snippet: String,
    pub symbol: TsSymbolSummary,
}

pub fn index_workspace(root: &Path) -> Result<IndexTsWorkspaceResponse> {
    let index = build_index(root)?;
    let index_path = index_path(root);
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&index_path, serde_json::to_vec_pretty(&index)?)?;
    Ok(IndexTsWorkspaceResponse {
        index_path: relative_display(root, &index_path),
        files_indexed: index.files_indexed,
        symbols_indexed: index.symbols.len(),
        generated_at_unix: index.generated_at_unix,
    })
}

pub fn status(root: &Path) -> TsIndexStatus {
    let index_path = index_path(root);
    if let Ok(content) = fs::read_to_string(&index_path)
        && let Ok(index) = serde_json::from_str::<TsIndex>(&content)
    {
        return TsIndexStatus {
            index_path: relative_display(root, &index_path),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: Some(index.generated_at_unix),
            files_indexed: Some(index.files_indexed),
            symbols_indexed: Some(index.symbols.len()),
        };
    }
    TsIndexStatus {
        index_path: relative_display(root, &index_path),
        exists: false,
        workspace_root: root.display().to_string(),
        generated_at_unix: None,
        files_indexed: None,
        symbols_indexed: None,
    }
}

pub fn maybe_reindex_after_write(
    root: &Path,
    changed_path: &Path,
) -> Result<Option<IndexTsWorkspaceResponse>> {
    if !matches!(
        changed_path.extension().and_then(|value| value.to_str()),
        Some("ts" | "tsx" | "js" | "jsx")
    ) {
        return Ok(None);
    }
    if !index_path(root).exists() {
        return Ok(None);
    }
    index_workspace(root).map(Some)
}

pub fn list_symbols(root: &Path, request: ListTsSymbolsRequest) -> Result<ListTsSymbolsResponse> {
    let index = load_or_build_or_create(root)?;
    let symbols = index
        .symbols
        .iter()
        .filter(|symbol| {
            request
                .file_path
                .as_deref()
                .map(|file| symbol.file_path == normalize_slashes(file))
                .unwrap_or(true)
        })
        .filter(|symbol| {
            request
                .kind
                .as_ref()
                .map(|kind| &symbol.kind == kind)
                .unwrap_or(true)
        })
        .map(TsSymbolSummary::from)
        .collect();
    Ok(ListTsSymbolsResponse { symbols })
}

pub fn search_symbols(
    root: &Path,
    request: SearchTsSymbolsRequest,
) -> Result<SearchTsSymbolsResponse> {
    let index = load_or_build_or_create(root)?;
    let needle = request.query.to_lowercase();
    let mut matches: Vec<_> = index
        .symbols
        .iter()
        .filter(|symbol| {
            [
                symbol.name.as_str(),
                symbol.signature.as_str(),
                symbol.docstring.as_str(),
                symbol.file_path.as_str(),
                symbol.scope_path.as_str(),
            ]
            .join("\n")
            .to_lowercase()
            .contains(&needle)
        })
        .take(request.limit.max(1))
        .map(TsSymbolSummary::from)
        .collect();
    matches.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
    });
    Ok(SearchTsSymbolsResponse {
        query: request.query,
        matches,
    })
}

pub fn read_symbol(root: &Path, request: ReadTsSymbolRequest) -> Result<ReadTsSymbolResponse> {
    let index = load_or_build(root)?;
    let symbol = index
        .symbols
        .iter()
        .find(|symbol| symbol.id == request.symbol_id)
        .cloned()
        .ok_or_else(|| TsIndexError::SymbolNotFound(request.symbol_id.clone()))?;
    let path = root.join(&symbol.file_path);
    let content = fs::read_to_string(path)?;
    let lines: Vec<_> = content.lines().collect();
    let content = lines[(symbol.start_line - 1)..symbol.end_line.min(lines.len())].join("\n");
    let (callees, callers, resolved_imports, suggested_reads) = if request.include_context {
        build_context(&index, &symbol)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), Vec::new())
    };
    Ok(ReadTsSymbolResponse {
        symbol,
        content,
        callees,
        callers,
        resolved_imports,
        suggested_reads,
    })
}

fn build_index(root: &Path) -> Result<TsIndex> {
    let mut symbols = Vec::new();
    let mut re_exports = Vec::new();
    let mut files_indexed = 0;
    for entry in walk_source_files(root) {
        let path = entry?;
        if path.extension().and_then(|value| value.to_str()) == Some("js")
            || path.extension().and_then(|value| value.to_str()) == Some("jsx")
            || path.extension().and_then(|value| value.to_str()) == Some("ts")
            || path.extension().and_then(|value| value.to_str()) == Some("tsx")
        {
            let metadata = fs::metadata(&path)?;
            if metadata.len() > MAX_TS_FILE_BYTES {
                continue;
            }
            let content = match fs::read_to_string(&path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            files_indexed += 1;
            let parsed = parse_ts_file(root, &path, &content);
            symbols.extend(parsed.symbols);
            re_exports.extend(parsed.re_exports);
        }
    }
    Ok(TsIndex {
        workspace_root: root.display().to_string(),
        generated_at_unix: now_unix(),
        files_indexed,
        symbols,
        re_exports,
    })
}

struct ParsedTsFile {
    symbols: Vec<TsSymbol>,
    re_exports: Vec<TsReExport>,
}

fn parse_ts_file(root: &Path, path: &Path, content: &str) -> ParsedTsFile {
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

struct TsCollector {
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

fn prop_name_text(key: &PropName) -> Option<String> {
    match key {
        PropName::Ident(ident) => Some(ident.sym.to_string()),
        PropName::Str(s) => Some(s.value.to_string_lossy().to_string()),
        PropName::Num(n) => Some(n.value.to_string()),
        PropName::BigInt(b) => Some(b.value.to_string()),
        PropName::Computed(_) => None,
    }
}

fn describe_callee(callee: &Callee) -> Option<(Option<String>, String)> {
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

fn expr_to_text(expr: &Expr) -> Option<String> {
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

fn build_context(
    index: &TsIndex,
    symbol: &TsSymbol,
) -> (
    Vec<TsCallee>,
    Vec<TsCaller>,
    Vec<TsResolvedImport>,
    Vec<TsSuggestedRead>,
) {
    let mut name_to_ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut id_to_symbol: BTreeMap<String, &TsSymbol> = BTreeMap::new();
    let file_export_to_ids = build_file_export_map(index);
    for item in &index.symbols {
        name_to_ids
            .entry(item.name.clone())
            .or_default()
            .push(item.id.clone());
        id_to_symbol.insert(item.id.clone(), item);
    }

    let resolved_imports: Vec<_> = symbol
        .import_bindings
        .iter()
        .map(|import| {
            let target_file_path = resolve_import_source(&symbol.file_path, &import.source, index);
            let re_export_chain = target_file_path
                .as_ref()
                .map(|target_file| {
                    build_re_export_chain(
                        index,
                        target_file,
                        &import.imported_name,
                        &file_export_to_ids,
                    )
                })
                .unwrap_or_default();
            let matched_symbol_ids = target_file_path
                .as_ref()
                .map(|target_file| match import.kind {
                    TsImportKind::Namespace | TsImportKind::SideEffect => {
                        exported_ids_for_file(target_file, &file_export_to_ids)
                    }
                    TsImportKind::Default | TsImportKind::Named => file_export_to_ids
                        .get(&(target_file.clone(), import.imported_name.clone()))
                        .cloned()
                        .unwrap_or_default(),
                })
                .unwrap_or_default();
            TsResolvedImport {
                source: import.source.clone(),
                local_name: import.local_name.clone(),
                imported_name: import.imported_name.clone(),
                kind: import.kind.clone(),
                target_file_path,
                matched_symbol_ids,
                re_export_chain,
            }
        })
        .collect();

    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| TsCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: resolve_call_targets(
                call,
                &name_to_ids,
                &resolved_imports,
                &file_export_to_ids,
            ),
        })
        .collect();
    let mut suggested_reads = Vec::new();
    let mut seen = BTreeSet::new();
    for callee in &callees {
        for matched_id in &callee.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched_symbol) = id_to_symbol.get(matched_id) {
                let reason = if resolved_imports.iter().any(|import| {
                    import.local_name == callee.target_text
                        || callee.snippet.contains(&format!("{}.", import.local_name))
                }) {
                    "resolved_import"
                } else {
                    "direct_callee"
                };
                suggested_reads.push(TsSuggestedRead {
                    reason: reason.to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: TsSymbolSummary::from(*matched_symbol),
                });
            }
        }
    }
    for import in &resolved_imports {
        for matched_id in &import.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched_symbol) = id_to_symbol.get(matched_id) {
                suggested_reads.push(TsSuggestedRead {
                    reason: "resolved_import".to_string(),
                    trigger_call: import.local_name.clone(),
                    trigger_line: symbol.start_line,
                    trigger_snippet: format!("import {} from {}", import.local_name, import.source),
                    symbol: TsSymbolSummary::from(*matched_symbol),
                });
            }
        }
    }
    let mut callers = Vec::new();
    for item in &index.symbols {
        if item.id == symbol.id {
            continue;
        }
        for call in &item.calls {
            if call.target_text == symbol.name || imported_call_matches(index, item, call, symbol) {
                callers.push(TsCaller {
                    symbol_id: item.id.clone(),
                    name: item.name.clone(),
                    file_path: item.file_path.clone(),
                    line: call.line,
                    snippet: call.snippet.clone(),
                });
            }
        }
    }
    (callees, callers, resolved_imports, suggested_reads)
}

fn resolve_call_targets(
    call: &TsCall,
    name_to_ids: &BTreeMap<String, Vec<String>>,
    resolved_imports: &[TsResolvedImport],
    file_export_to_ids: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<String> {
    if let Some(namespace) = call.namespace.as_deref()
        && let Some(import) = resolved_imports
            .iter()
            .find(|import| import.local_name == namespace && import.kind == TsImportKind::Namespace)
        && let Some(target_file) = import.target_file_path.as_ref()
        && let Some(ids) = file_export_to_ids.get(&(target_file.clone(), call.target_text.clone()))
    {
        return ids.clone();
    }

    if let Some(import) = resolved_imports
        .iter()
        .find(|import| import.local_name == call.target_text)
    {
        if !import.matched_symbol_ids.is_empty() {
            return import.matched_symbol_ids.clone();
        }
    }
    name_to_ids
        .get(&call.target_text)
        .cloned()
        .unwrap_or_default()
}

fn imported_call_matches(
    index: &TsIndex,
    caller: &TsSymbol,
    call: &TsCall,
    target: &TsSymbol,
) -> bool {
    let file_export_to_ids = build_file_export_map(index);
    caller.import_bindings.iter().any(|import| {
        if call.namespace.as_deref() == Some(import.local_name.as_str())
            && import.kind == TsImportKind::Namespace
            && let Some(target_file) =
                resolve_import_source(&caller.file_path, &import.source, index)
            && let Some(ids) = file_export_to_ids.get(&(target_file, call.target_text.clone()))
        {
            return ids.iter().any(|id| id == &target.id);
        }

        import.local_name == call.target_text
            && resolve_import_source(&caller.file_path, &import.source, index).as_deref()
                == Some(target.file_path.as_str())
            && exported_names(target)
                .iter()
                .any(|export_name| export_name == &import.imported_name)
    })
}

fn build_file_export_map(index: &TsIndex) -> BTreeMap<(String, String), Vec<String>> {
    let mut file_export_to_ids: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for item in &index.symbols {
        if item.export {
            for export_name in exported_names(item) {
                file_export_to_ids
                    .entry((item.file_path.clone(), export_name))
                    .or_default()
                    .push(item.id.clone());
            }
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for re_export in &index.re_exports {
            let Some(source_file) =
                resolve_import_source(&re_export.file_path, &re_export.source, index)
            else {
                continue;
            };
            let source_exports =
                if re_export.kind == TsImportKind::Namespace && re_export.exported_name == "*" {
                    exported_names_for_file(&source_file, &file_export_to_ids)
                } else {
                    vec![re_export.local_name.clone()]
                };
            for source_export in source_exports {
                let exported_name = if re_export.kind == TsImportKind::Namespace {
                    source_export.clone()
                } else {
                    re_export.exported_name.clone()
                };
                let Some(ids) = file_export_to_ids
                    .get(&(source_file.clone(), source_export))
                    .cloned()
                else {
                    continue;
                };
                let entry = file_export_to_ids
                    .entry((re_export.file_path.clone(), exported_name))
                    .or_default();
                for id in ids {
                    if !entry.contains(&id) {
                        entry.push(id);
                        changed = true;
                    }
                }
            }
        }
    }
    file_export_to_ids
}

fn build_re_export_chain(
    index: &TsIndex,
    file_path: &str,
    imported_name: &str,
    file_export_to_ids: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<TsExportChainStep> {
    let mut chain = Vec::new();
    let mut current_file = file_path.to_string();
    let mut current_name = imported_name.to_string();
    let mut seen = BTreeSet::new();

    loop {
        if !seen.insert((current_file.clone(), current_name.clone())) {
            break;
        }
        let Some(re_export) = find_re_export_for_name(index, &current_file, &current_name) else {
            break;
        };
        let Some(target_file) =
            resolve_import_source(&re_export.file_path, &re_export.source, index)
        else {
            break;
        };
        let next_name = if re_export.kind == TsImportKind::Namespace {
            current_name.clone()
        } else {
            re_export.local_name.clone()
        };
        if !file_export_to_ids.contains_key(&(target_file.clone(), next_name.clone())) {
            break;
        }
        chain.push(TsExportChainStep {
            file_path: re_export.file_path.clone(),
            source: re_export.source.clone(),
            imported_name: next_name.clone(),
            local_name: re_export.exported_name.clone(),
            kind: re_export.kind.clone(),
            target_file_path: Some(target_file.clone()),
        });
        current_file = target_file;
        current_name = next_name;
    }

    chain
}

fn find_re_export_for_name<'a>(
    index: &'a TsIndex,
    file_path: &str,
    export_name: &str,
) -> Option<&'a TsReExport> {
    index.re_exports.iter().find(|re_export| {
        re_export.file_path == file_path
            && (re_export.exported_name == export_name
                || (re_export.kind == TsImportKind::Namespace && re_export.exported_name == "*"))
    })
}

fn exported_names_for_file(
    file_path: &str,
    file_export_to_ids: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<String> {
    let mut names: Vec<_> = file_export_to_ids
        .keys()
        .filter_map(|(file, name)| (file == file_path).then_some(name.clone()))
        .collect();
    names.sort();
    names.dedup();
    names
}

fn exported_ids_for_file(
    file_path: &str,
    file_export_to_ids: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<String> {
    let mut ids = Vec::new();
    for (file, _name) in file_export_to_ids.keys() {
        if file != file_path {
            continue;
        }
        if let Some(values) = file_export_to_ids.get(&(file.clone(), _name.clone())) {
            for value in values {
                if !ids.contains(value) {
                    ids.push(value.clone());
                }
            }
        }
    }
    ids
}

fn resolve_import_source(from_file: &str, source: &str, index: &TsIndex) -> Option<String> {
    if !source.starts_with('.') {
        return None;
    }
    let from_dir = Path::new(from_file)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let candidate = normalize_workspace_relative_path(&from_dir.join(source).to_string_lossy());
    let candidates = import_path_candidates(&candidate);
    candidates.into_iter().find(|candidate| {
        index
            .symbols
            .iter()
            .any(|symbol| symbol.file_path == *candidate)
            || index
                .re_exports
                .iter()
                .any(|re_export| re_export.file_path == *candidate)
    })
}

fn import_path_candidates(base: &str) -> Vec<String> {
    let base = normalize_workspace_relative_path(base);
    let path = Path::new(&base);
    if path.extension().is_some() {
        return vec![base];
    }
    ["ts", "tsx", "js", "jsx"]
        .into_iter()
        .map(|ext| format!("{base}.{ext}"))
        .chain(
            ["ts", "tsx", "js", "jsx"]
                .into_iter()
                .map(|ext| format!("{base}/index.{ext}")),
        )
        .collect()
}

fn exported_names(symbol: &TsSymbol) -> Vec<String> {
    if !symbol.export_names.is_empty() {
        symbol.export_names.clone()
    } else if symbol.export {
        export_names(&symbol.name, true)
    } else {
        Vec::new()
    }
}

fn export_names(name: &str, export: bool) -> Vec<String> {
    if !export {
        return Vec::new();
    }
    if name == "default" || name == "defaultExport" {
        vec!["default".to_string()]
    } else {
        vec![name.to_string()]
    }
}

fn default_export_names() -> Vec<String> {
    vec!["default".to_string()]
}

fn module_export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::Ident(ident) => ident.sym.to_string(),
        ModuleExportName::Str(value) => value.value.to_string_lossy().to_string(),
    }
}

fn normalize_workspace_relative_path(value: &str) -> String {
    let mut normalized = normalize_slashes(value);
    while normalized.starts_with("./") {
        normalized = normalized.trim_start_matches("./").to_string();
    }
    while normalized.starts_with(".\\") {
        normalized = normalized.trim_start_matches(".\\").to_string();
    }
    normalized
}

fn walk_source_files(root: &Path) -> Vec<std::io::Result<PathBuf>> {
    let mut paths = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| !NOISE_DIRS.contains(&name))
                .unwrap_or(true)
        });
    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(err) => {
                paths.push(Err(std::io::Error::other(err)));
                continue;
            }
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        paths.push(Ok(entry.path().to_path_buf()));
    }
    paths
}

fn load_or_build(root: &Path) -> Result<TsIndex> {
    let path = index_path(root);
    if !path.exists() {
        return Err(TsIndexError::MissingIndex);
    }
    let content = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

fn load_or_build_or_create(root: &Path) -> Result<TsIndex> {
    match load_or_build(root) {
        Ok(index) => Ok(index),
        Err(TsIndexError::MissingIndex) => {
            index_workspace(root)?;
            load_or_build(root)
        }
        Err(error) => Err(error),
    }
}

fn index_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(INDEX_FILE)
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_slashes(value: &str) -> String {
    value.replace('\\', "/")
}

fn ts_symbol_id(file_path: &str, scope_path: &str, name: &str, line: usize) -> String {
    if scope_path.is_empty() {
        format!("ts:{file_path}:{name}:{line}")
    } else {
        format!("ts:{file_path}:{scope_path}.{name}:{line}")
    }
}

fn snippet(content: &str, cm: &Lrc<SourceMap>, span: Span, max_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = line_of(cm, span.lo).saturating_sub(1);
    let end = line_of(cm, span.hi).min(lines.len());
    lines[start..end.min(start + max_lines)].join("\n")
}

fn line_of(cm: &Lrc<SourceMap>, pos: swc_common::BytePos) -> usize {
    cm.lookup_char_pos(pos).line
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

fn default_limit() -> usize {
    20
}

fn looks_like_component(name: &str) -> bool {
    name.chars()
        .next()
        .map(|ch| ch.is_uppercase())
        .unwrap_or(false)
}

impl From<&TsSymbol> for TsSymbolSummary {
    fn from(symbol: &TsSymbol) -> Self {
        Self {
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            file_path: symbol.file_path.clone(),
            scope_path: symbol.scope_path.clone(),
            parent_id: symbol.parent_id.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: symbol.signature.clone(),
            docstring: symbol.docstring.clone(),
            export: symbol.export,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("codex_ts_index_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn indexes_ts_symbols_and_suggested_reads() {
        let root = temp_workspace("basic");
        fs::write(
            root.join("api.ts"),
            r#"import { request } from './request';

// Create session for PPT
export async function createPptSession(input: string) {
  return request.post('/ppt/session', input);
}

// format id
export const formatId = (id: string) => {
  return normalize(id);
}

export class PptEditor {
  save() {
    return createPptSession('demo');
  }
}

function normalize(id: string) {
  return id.trim();
}
"#,
        )
        .unwrap();

        let response = index_workspace(&root).unwrap();
        assert_eq!(response.files_indexed, 1);
        assert!(response.symbols_indexed >= 4);

        let search = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "createPptSession".to_string(),
                limit: 5,
            },
        )
        .unwrap();
        let symbol = search
            .matches
            .iter()
            .find(|item| item.name == "createPptSession")
            .unwrap();
        assert!(symbol.docstring.contains("Create session"));

        let read = read_symbol(
            &root,
            ReadTsSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: symbol.id.clone(),
                include_context: true,
            },
        )
        .unwrap();
        assert!(
            read.callees
                .iter()
                .any(|callee| callee.target_text == "post")
        );

        let save_search = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "save".to_string(),
                limit: 5,
            },
        )
        .unwrap();
        let save = save_search
            .matches
            .iter()
            .find(|item| item.name == "save")
            .unwrap();
        let save_read = read_symbol(
            &root,
            ReadTsSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: save.id.clone(),
                include_context: true,
            },
        )
        .unwrap();
        assert!(save_read.suggested_reads.iter().any(|suggestion| {
            suggestion.symbol.name == "createPptSession" && suggestion.reason == "direct_callee"
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_builds_index_when_missing() {
        let root = temp_workspace("auto_build");
        fs::write(
            root.join("api.ts"),
            "export function AutoBuild() { return 1; }\n",
        )
        .unwrap();

        let search = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "AutoBuild".to_string(),
                limit: 5,
            },
        )
        .unwrap();

        assert_eq!(search.matches.len(), 1);
        assert!(index_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_imported_symbols_in_context() {
        let root = temp_workspace("imports");
        fs::write(
            root.join("provider.ts"),
            r#"export function createThing(name: string) {
  return name.trim();
}

export default function defaultThing() {
  return createThing('default');
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("consumer.ts"),
            r#"import defaultThing, { createThing as makeThing } from './provider';

export function run() {
  makeThing('demo');
  return defaultThing();
}
"#,
        )
        .unwrap();

        index_workspace(&root).unwrap();
        let run = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "run".to_string(),
                limit: 5,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "run")
        .unwrap();
        let read = read_symbol(
            &root,
            ReadTsSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: run.id,
                include_context: true,
            },
        )
        .unwrap();

        assert!(read.resolved_imports.iter().any(|import| {
            import.local_name == "makeThing"
                && import.target_file_path.as_deref() == Some("provider.ts")
                && !import.matched_symbol_ids.is_empty()
        }));
        assert!(read.suggested_reads.iter().any(|suggestion| {
            suggestion.reason == "resolved_import" && suggestion.symbol.name == "createThing"
        }));
        assert!(read.callees.iter().any(|callee| {
            callee.target_text == "makeThing" && !callee.matched_symbol_ids.is_empty()
        }));

        let create = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "createThing".to_string(),
                limit: 5,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "createThing")
        .unwrap();
        let create_read = read_symbol(
            &root,
            ReadTsSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: create.id,
                include_context: true,
            },
        )
        .unwrap();
        assert!(
            create_read
                .callers
                .iter()
                .any(|caller| caller.name == "run")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_namespace_import_calls() {
        let root = temp_workspace("namespace_imports");
        fs::write(
            root.join("provider.ts"),
            "export function createThing(name: string) { return name.trim(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("consumer.ts"),
            r#"import * as provider from './provider';

export function run() {
  return provider.createThing('demo');
}
"#,
        )
        .unwrap();

        index_workspace(&root).unwrap();
        let run = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "run".to_string(),
                limit: 5,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "run")
        .unwrap();
        let read = read_symbol(
            &root,
            ReadTsSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: run.id,
                include_context: true,
            },
        )
        .unwrap();

        assert!(read.callees.iter().any(|callee| {
            callee.target_text == "createThing" && !callee.matched_symbol_ids.is_empty()
        }));
        assert!(read.suggested_reads.iter().any(|suggestion| {
            suggestion.reason == "resolved_import" && suggestion.symbol.name == "createThing"
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_export_star_barrel_imports() {
        let root = temp_workspace("barrel_imports");
        fs::write(
            root.join("provider.ts"),
            "export function createThing(name: string) { return name.trim(); }\n",
        )
        .unwrap();
        fs::write(root.join("barrel.ts"), "export * from './provider';\n").unwrap();
        fs::write(
            root.join("consumer.ts"),
            r#"import { createThing } from './barrel';

export function run() {
  return createThing('demo');
}
"#,
        )
        .unwrap();

        index_workspace(&root).unwrap();
        let run = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "run".to_string(),
                limit: 5,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "run")
        .unwrap();
        let read = read_symbol(
            &root,
            ReadTsSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: run.id,
                include_context: true,
            },
        )
        .unwrap();

        assert!(read.resolved_imports.iter().any(|import| {
            import.local_name == "createThing"
                && import.target_file_path.as_deref() == Some("barrel.ts")
                && !import.matched_symbol_ids.is_empty()
                && import.re_export_chain.iter().any(|step| {
                    step.file_path == "barrel.ts"
                        && step.target_file_path.as_deref() == Some("provider.ts")
                })
        }));
        assert!(read.suggested_reads.iter().any(|suggestion| {
            suggestion.reason == "resolved_import" && suggestion.symbol.name == "createThing"
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn indexes_hoc_inner_class_with_visit() {
        let root = temp_workspace("hoc_inner_class");
        fs::write(
            root.join("container.tsx"),
            r#"import React from 'react';

export default function container(Comp: any) {
  class Wrapper extends React.Component {
    updateDisk() {
      this.foo();
    }
    getMax() {
      return 1;
    }
    foo() {}
  }
  return Wrapper;
}
"#,
        )
        .unwrap();

        index_workspace(&root).unwrap();

        let update_disk = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "updateDisk".to_string(),
                limit: 10,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "updateDisk")
        .expect("updateDisk should be indexed");
        assert_eq!(update_disk.kind, TsSymbolKind::Method);
        assert_eq!(update_disk.scope_path, "container.Wrapper");

        let wrapper = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "Wrapper".to_string(),
                limit: 10,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "Wrapper")
        .expect("Wrapper should be indexed");
        assert_eq!(wrapper.kind, TsSymbolKind::Class);
        assert_eq!(wrapper.scope_path, "container");
        assert_eq!(update_disk.parent_id.as_deref(), Some(wrapper.id.as_str()));

        let read = read_symbol(
            &root,
            ReadTsSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: update_disk.id.clone(),
                include_context: true,
            },
        )
        .unwrap();
        assert!(
            read.callees.iter().any(|c| c.target_text == "foo"),
            "updateDisk should record `this.foo()` call"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn indexes_anonymous_class_expression() {
        let root = temp_workspace("anon_class_expr");
        fs::write(
            root.join("wrap.ts"),
            r#"class Base {}
const Wrapped = class extends Base {
  foo() { return 1; }
};
"#,
        )
        .unwrap();

        index_workspace(&root).unwrap();

        let foo = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "foo".to_string(),
                limit: 10,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "foo")
        .expect("foo should be indexed");
        assert_eq!(foo.kind, TsSymbolKind::Method);
        assert_eq!(foo.scope_path, "Wrapped");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn indexes_nested_function() {
        let root = temp_workspace("nested_fn");
        fs::write(
            root.join("util.ts"),
            r#"export function outer() {
  function inner() {
    return 42;
  }
  return inner();
}
"#,
        )
        .unwrap();

        index_workspace(&root).unwrap();

        let inner = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "inner".to_string(),
                limit: 10,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "inner")
        .expect("inner should be indexed");
        assert_eq!(inner.kind, TsSymbolKind::Function);
        assert_eq!(inner.scope_path, "outer");

        let outer = search_symbols(
            &root,
            SearchTsSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "outer".to_string(),
                limit: 10,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "outer")
        .expect("outer should be indexed");
        let read = read_symbol(
            &root,
            ReadTsSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: outer.id.clone(),
                include_context: true,
            },
        )
        .unwrap();
        assert!(
            read.callees.iter().any(|c| c.target_text == "inner"),
            "outer should record inner() call as callee"
        );

        let _ = fs::remove_dir_all(root);
    }
}
