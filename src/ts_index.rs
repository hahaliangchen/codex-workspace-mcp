use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use swc_common::{FileName, SourceMap, Span, comments::SingleThreadedComments, sync::Lrc};
use swc_ecma_ast::*;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsSymbol {
    pub id: String,
    pub name: String,
    pub kind: TsSymbolKind,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub docstring: String,
    pub export: bool,
    pub calls: Vec<TsCall>,
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
    pub target_text: String,
    pub line: usize,
    pub snippet: String,
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
    pub suggested_reads: Vec<TsSuggestedRead>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsSymbolSummary {
    pub id: String,
    pub name: String,
    pub kind: TsSymbolKind,
    pub file_path: String,
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
    let (callees, callers, suggested_reads) = if request.include_context {
        build_context(&index, &symbol)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    Ok(ReadTsSymbolResponse {
        symbol,
        content,
        callees,
        callers,
        suggested_reads,
    })
}

fn build_index(root: &Path) -> Result<TsIndex> {
    let mut symbols = Vec::new();
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
            symbols.extend(parse_ts_file(root, &path, &content));
        }
    }
    Ok(TsIndex {
        workspace_root: root.display().to_string(),
        generated_at_unix: now_unix(),
        files_indexed,
        symbols,
    })
}

fn parse_ts_file(root: &Path, path: &Path, content: &str) -> Vec<TsSymbol> {
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
        Err(_) => return Vec::new(),
    };
    let mut collector = TsCollector::new(relative_path, content, cm);
    collector.collect(&module);
    collector.symbols
}

struct TsCollector {
    file_path: String,
    content: String,
    cm: Lrc<SourceMap>,
    symbols: Vec<TsSymbol>,
    imports: Vec<String>,
    index_by_name: BTreeMap<String, Vec<String>>,
}

impl TsCollector {
    fn new(file_path: String, content: &str, cm: Lrc<SourceMap>) -> Self {
        Self {
            file_path,
            content: content.to_string(),
            cm,
            symbols: Vec::new(),
            imports: Vec::new(),
            index_by_name: BTreeMap::new(),
        }
    }

    fn collect(&mut self, module: &Module) {
        for item in &module.body {
            match item {
                ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                    self.imports.push(format!("{:?}", import.src.value));
                }
                ModuleItem::Stmt(Stmt::Decl(decl)) => self.collect_decl(decl, true),
                ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => match &export.decl {
                    Decl::Fn(func) => self.collect_fn(&func, true),
                    Decl::Class(class) => self.collect_class(&class, true),
                    Decl::TsInterface(interface) => self.collect_interface(&interface, true),
                    Decl::TsTypeAlias(alias) => self.collect_type_alias(&alias, true),
                    Decl::TsEnum(enum_decl) => self.collect_enum(&enum_decl, true),
                    Decl::Var(var) => self.collect_var(&var, true),
                    _ => {}
                },
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(export)) => match &export.decl
                {
                    DefaultDecl::Fn(func) => self.collect_default_fn(func, true),
                    DefaultDecl::Class(class) => self.collect_default_class(class, true),
                    _ => {}
                },
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(expr)) => {
                    if let Expr::Arrow(arrow) = &*expr.expr {
                        self.collect_arrow(arrow, true, "defaultExport");
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_decl(&mut self, decl: &Decl, export: bool) {
        match decl {
            Decl::Fn(func) => self.collect_fn(func, export),
            Decl::Class(class) => self.collect_class(class, export),
            Decl::Var(var) => self.collect_var(var, export),
            Decl::TsInterface(interface) => self.collect_interface(interface, export),
            Decl::TsTypeAlias(alias) => self.collect_type_alias(alias, export),
            Decl::TsEnum(enum_decl) => self.collect_enum(enum_decl, export),
            _ => {}
        }
    }

    fn collect_fn(&mut self, func: &FnDecl, export: bool) {
        let name = func.ident.sym.to_string();
        let start_line = line_of(&self.cm, func.function.span.lo);
        let end_line = line_of(&self.cm, func.function.span.hi);
        let signature = self.signature_snippet(func.function.span);
        let docstring = self.docstring_before(func.function.span);
        let calls = collect_calls_from_span(&self.content, &self.cm, func.function.span);
        let id = ts_symbol_id(&self.file_path, &name, start_line);
        self.index_by_name
            .entry(name.clone())
            .or_default()
            .push(id.clone());
        self.symbols.push(TsSymbol {
            id,
            name,
            kind: TsSymbolKind::Function,
            file_path: self.file_path.clone(),
            start_line,
            end_line,
            signature,
            docstring,
            export,
            calls,
            imports: self.imports.clone(),
        });
    }

    fn collect_default_fn(&mut self, func: &FnExpr, export: bool) {
        if let Some(ident) = &func.ident {
            let name = ident.sym.to_string();
            let start_line = line_of(&self.cm, func.function.span.lo);
            let end_line = line_of(&self.cm, func.function.span.hi);
            let signature = self.signature_snippet(func.function.span);
            let docstring = self.docstring_before(func.function.span);
            let calls = collect_calls_from_span(&self.content, &self.cm, func.function.span);
            let id = ts_symbol_id(&self.file_path, &name, start_line);
            self.index_by_name
                .entry(name.clone())
                .or_default()
                .push(id.clone());
            self.symbols.push(TsSymbol {
                id,
                name,
                kind: TsSymbolKind::Function,
                file_path: self.file_path.clone(),
                start_line,
                end_line,
                signature,
                docstring,
                export,
                calls,
                imports: self.imports.clone(),
            });
        }
    }

    fn collect_class(&mut self, class: &ClassDecl, export: bool) {
        let name = class.ident.sym.to_string();
        let start_line = line_of(&self.cm, class.class.span.lo);
        let end_line = line_of(&self.cm, class.class.span.hi);
        let signature = self.signature_snippet(class.class.span);
        let docstring = self.docstring_before(class.class.span);
        let id = ts_symbol_id(&self.file_path, &name, start_line);
        self.index_by_name
            .entry(name.clone())
            .or_default()
            .push(id.clone());
        self.symbols.push(TsSymbol {
            id,
            name: name.clone(),
            kind: TsSymbolKind::Class,
            file_path: self.file_path.clone(),
            start_line,
            end_line,
            signature,
            docstring,
            export,
            calls: Vec::new(),
            imports: self.imports.clone(),
        });
        for member in &class.class.body {
            match member {
                ClassMember::Method(method) => self.collect_method(&name, method, export),
                ClassMember::ClassProp(prop) => self.collect_class_prop(&name, prop, export),
                _ => {}
            }
        }
    }

    fn collect_method(&mut self, class_name: &str, method: &ClassMethod, export: bool) {
        if let PropName::Ident(ident) = &method.key {
            let name = ident.sym.to_string();
            let start_line = line_of(&self.cm, method.span.lo);
            let end_line = line_of(&self.cm, method.span.hi);
            let signature = self.signature_snippet(method.span);
            let docstring = self.docstring_before(method.span);
            let calls = collect_calls_from_span(&self.content, &self.cm, method.span);
            let id = ts_symbol_id(&self.file_path, &format!("{class_name}.{name}"), start_line);
            self.index_by_name
                .entry(name.clone())
                .or_default()
                .push(id.clone());
            self.symbols.push(TsSymbol {
                id,
                name,
                kind: TsSymbolKind::Method,
                file_path: self.file_path.clone(),
                start_line,
                end_line,
                signature,
                docstring,
                export,
                calls,
                imports: self.imports.clone(),
            });
        }
    }

    fn collect_class_prop(&mut self, class_name: &str, prop: &ClassProp, export: bool) {
        let PropName::Ident(ident) = &prop.key else {
            return;
        };
        let Some(value) = &prop.value else {
            return;
        };
        let Expr::Arrow(arrow) = &**value else {
            return;
        };
        let name = ident.sym.to_string();
        let start_line = line_of(&self.cm, prop.span.lo);
        let end_line = line_of(&self.cm, prop.span.hi);
        let signature = self.signature_snippet(prop.span);
        let docstring = self.docstring_before(prop.span);
        let calls = collect_calls_from_span(&self.content, &self.cm, arrow.span);
        let id = ts_symbol_id(&self.file_path, &format!("{class_name}.{name}"), start_line);
        self.index_by_name
            .entry(name.clone())
            .or_default()
            .push(id.clone());
        self.symbols.push(TsSymbol {
            id,
            name,
            kind: TsSymbolKind::Method,
            file_path: self.file_path.clone(),
            start_line,
            end_line,
            signature,
            docstring,
            export,
            calls,
            imports: self.imports.clone(),
        });
    }

    fn collect_interface(&mut self, interface: &TsInterfaceDecl, export: bool) {
        let name = interface.id.sym.to_string();
        let start_line = line_of(&self.cm, interface.span.lo);
        let end_line = line_of(&self.cm, interface.span.hi);
        let signature = self.signature_snippet(interface.span);
        let docstring = self.docstring_before(interface.span);
        let id = ts_symbol_id(&self.file_path, &name, start_line);
        self.index_by_name
            .entry(name.clone())
            .or_default()
            .push(id.clone());
        self.symbols.push(TsSymbol {
            id,
            name,
            kind: TsSymbolKind::Interface,
            file_path: self.file_path.clone(),
            start_line,
            end_line,
            signature,
            docstring,
            export,
            calls: Vec::new(),
            imports: self.imports.clone(),
        });
    }

    fn collect_type_alias(&mut self, alias: &TsTypeAliasDecl, export: bool) {
        let name = alias.id.sym.to_string();
        let start_line = line_of(&self.cm, alias.span.lo);
        let end_line = line_of(&self.cm, alias.span.hi);
        let signature = self.signature_snippet(alias.span);
        let docstring = self.docstring_before(alias.span);
        let id = ts_symbol_id(&self.file_path, &name, start_line);
        self.index_by_name
            .entry(name.clone())
            .or_default()
            .push(id.clone());
        self.symbols.push(TsSymbol {
            id,
            name,
            kind: TsSymbolKind::TypeAlias,
            file_path: self.file_path.clone(),
            start_line,
            end_line,
            signature,
            docstring,
            export,
            calls: Vec::new(),
            imports: self.imports.clone(),
        });
    }

    fn collect_enum(&mut self, enum_decl: &TsEnumDecl, export: bool) {
        let name = enum_decl.id.sym.to_string();
        let start_line = line_of(&self.cm, enum_decl.span.lo);
        let end_line = line_of(&self.cm, enum_decl.span.hi);
        let signature = self.signature_snippet(enum_decl.span);
        let docstring = self.docstring_before(enum_decl.span);
        let id = ts_symbol_id(&self.file_path, &name, start_line);
        self.index_by_name
            .entry(name.clone())
            .or_default()
            .push(id.clone());
        self.symbols.push(TsSymbol {
            id,
            name,
            kind: TsSymbolKind::Enum,
            file_path: self.file_path.clone(),
            start_line,
            end_line,
            signature,
            docstring,
            export,
            calls: Vec::new(),
            imports: self.imports.clone(),
        });
    }

    fn collect_var(&mut self, var: &VarDecl, export: bool) {
        if var.kind != VarDeclKind::Const {
            return;
        }
        for declarator in &var.decls {
            if let Pat::Ident(binding) = &declarator.name {
                let name = binding.id.sym.to_string();
                if let Some(init) = &declarator.init {
                    if let Expr::Arrow(arrow) = &**init {
                        self.collect_arrow(arrow, export, &name);
                    } else {
                        let start_line = line_of(&self.cm, declarator.span.lo);
                        let end_line = line_of(&self.cm, declarator.span.hi);
                        let signature = self.signature_snippet(declarator.span);
                        let docstring = self.docstring_before(declarator.span);
                        let id = ts_symbol_id(&self.file_path, &name, start_line);
                        self.index_by_name
                            .entry(name.clone())
                            .or_default()
                            .push(id.clone());
                        self.symbols.push(TsSymbol {
                            id,
                            name,
                            kind: TsSymbolKind::Const,
                            file_path: self.file_path.clone(),
                            start_line,
                            end_line,
                            signature,
                            docstring,
                            export,
                            calls: Vec::new(),
                            imports: self.imports.clone(),
                        });
                    }
                }
            }
        }
    }

    fn collect_default_class(&mut self, class: &ClassExpr, export: bool) {
        let name = class
            .ident
            .as_ref()
            .map(|ident| ident.sym.to_string())
            .unwrap_or_else(|| "default".to_string());
        let start_line = line_of(&self.cm, class.class.span.lo);
        let end_line = line_of(&self.cm, class.class.span.hi);
        let signature = self.signature_snippet(class.class.span);
        let docstring = self.docstring_before(class.class.span);
        let id = ts_symbol_id(&self.file_path, &name, start_line);
        self.index_by_name
            .entry(name.clone())
            .or_default()
            .push(id.clone());
        self.symbols.push(TsSymbol {
            id,
            name,
            kind: TsSymbolKind::Class,
            file_path: self.file_path.clone(),
            start_line,
            end_line,
            signature,
            docstring,
            export,
            calls: Vec::new(),
            imports: self.imports.clone(),
        });
    }

    fn collect_arrow(&mut self, arrow: &ArrowExpr, export: bool, name: &str) {
        let start_line = line_of(&self.cm, arrow.span.lo);
        let end_line = line_of(&self.cm, arrow.span.hi);
        let signature = self.signature_snippet(arrow.span);
        let docstring = self.docstring_before(arrow.span);
        let calls = collect_calls_from_span(&self.content, &self.cm, arrow.span);
        let id = ts_symbol_id(&self.file_path, name, start_line);
        self.index_by_name
            .entry(name.to_string())
            .or_default()
            .push(id.clone());
        self.symbols.push(TsSymbol {
            id,
            name: name.to_string(),
            kind: if looks_like_component(name) {
                TsSymbolKind::Component
            } else {
                TsSymbolKind::ArrowFunction
            },
            file_path: self.file_path.clone(),
            start_line,
            end_line,
            signature,
            docstring,
            export,
            calls,
            imports: self.imports.clone(),
        });
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

fn build_context(
    index: &TsIndex,
    symbol: &TsSymbol,
) -> (Vec<TsCallee>, Vec<TsCaller>, Vec<TsSuggestedRead>) {
    let mut name_to_ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut id_to_symbol: BTreeMap<String, &TsSymbol> = BTreeMap::new();
    for item in &index.symbols {
        name_to_ids
            .entry(item.name.clone())
            .or_default()
            .push(item.id.clone());
        id_to_symbol.insert(item.id.clone(), item);
    }
    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| TsCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: name_to_ids
                .get(&call.target_text)
                .cloned()
                .unwrap_or_default(),
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
                suggested_reads.push(TsSuggestedRead {
                    reason: "direct_callee".to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
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
            if call.target_text == symbol.name {
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
    (callees, callers, suggested_reads)
}

fn collect_calls_from_span(content: &str, cm: &Lrc<SourceMap>, span: Span) -> Vec<TsCall> {
    let lines: Vec<&str> = content.lines().collect();
    let start_line = line_of(cm, span.lo);
    let end_line = line_of(cm, span.hi).min(lines.len().max(1));
    let call_re = Regex::new(r"(?:\.|\b)([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap();
    let keywords: BTreeSet<&str> = [
        "if",
        "for",
        "switch",
        "return",
        "await",
        "new",
        "typeof",
        "instanceof",
        "catch",
    ]
    .into_iter()
    .collect();
    let mut calls = Vec::new();
    for (idx, line) in lines
        .iter()
        .enumerate()
        .take(end_line)
        .skip(start_line.saturating_sub(1))
    {
        let clean = strip_line_comment(line);
        for captures in call_re.captures_iter(&clean) {
            let target = captures.get(1).map(|value| value.as_str()).unwrap_or("");
            if target.is_empty() || keywords.contains(target) {
                continue;
            }
            calls.push(TsCall {
                target_text: target.to_string(),
                line: idx + 1,
                snippet: line.trim().to_string(),
            });
        }
    }
    calls
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

fn ts_symbol_id(file_path: &str, name: &str, line: usize) -> String {
    format!("ts:{file_path}:{name}:{line}")
}

fn strip_line_comment(line: &str) -> String {
    line.split("//").next().unwrap_or(line).to_string()
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
}
