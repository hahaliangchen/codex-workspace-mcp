use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use quote::ToTokens;
use serde::{Deserialize, Serialize};
use syn::{
    Expr, ExprCall, ExprMethodCall, File, ImplItem, Item, ItemImpl, ItemUse, UseTree,
    spanned::Spanned, visit::Visit,
};

const MAX_RUST_FILE_BYTES: u64 = 2 * 1024 * 1024;
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
pub enum RustIndexError {
    #[error("rust index not found; call index_rust_workspace first")]
    #[allow(dead_code)]
    MissingIndex,
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, RustIndexError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct RustFileInfo {
    pub file_path: String,
    #[serde(default)]
    pub uses: Vec<RustUse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustUse {
    pub path: String,
    pub local_name: String,
    pub alias: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustSymbol {
    pub id: String,
    pub name: String,
    pub kind: RustSymbolKind,
    pub file_path: String,
    #[serde(default)]
    pub module_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub docstring: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub impl_type: Option<String>,
    #[serde(default)]
    pub trait_name: Option<String>,
    #[serde(default)]
    pub calls: Vec<RustCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RustSymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Const,
    Static,
    Module,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustCall {
    #[serde(default)]
    pub qualifier: Option<String>,
    pub target_text: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Deserialize)]
pub struct IndexRustWorkspaceRequest {
    pub workspace_root: String,
}

#[derive(Debug, Serialize)]
pub struct IndexRustWorkspaceResponse {
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct RustIndexStatus {
    pub index_path: String,
    pub exists: bool,
    pub workspace_root: String,
    pub generated_at_unix: Option<u64>,
    pub files_indexed: Option<usize>,
    pub symbols_indexed: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ListRustSymbolsRequest {
    pub workspace_root: String,
    pub file_path: Option<String>,
    pub kind: Option<RustSymbolKind>,
}

#[derive(Debug, Serialize)]
pub struct ListRustSymbolsResponse {
    pub symbols: Vec<RustSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct SearchRustSymbolsRequest {
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_symbol_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchRustSymbolsResponse {
    pub query: String,
    pub matches: Vec<RustSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct ReadRustSymbolRequest {
    pub workspace_root: String,
    pub symbol_id: String,
    #[serde(default)]
    pub include_context: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadRustSymbolResponse {
    pub symbol: RustSymbol,
    pub content: String,
    pub callers: Vec<RustCaller>,
    pub callees: Vec<RustCallee>,
    pub suggested_reads: Vec<RustSuggestedRead>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RustSymbolSummary {
    pub id: String,
    pub name: String,
    pub kind: RustSymbolKind,
    pub file_path: String,
    pub module_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub docstring: String,
    pub impl_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RustCaller {
    pub symbol_id: String,
    pub name: String,
    pub file_path: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RustCallee {
    pub target_text: String,
    pub line: usize,
    pub snippet: String,
    pub matched_symbol_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RustSuggestedRead {
    pub reason: String,
    pub trigger_call: String,
    pub trigger_line: usize,
    pub trigger_snippet: String,
    pub symbol: RustSymbolSummary,
}

pub fn index_workspace(root: &Path) -> Result<IndexRustWorkspaceResponse> {
    let (files_indexed, symbols_indexed) = build_index(root)?;
    Ok(IndexRustWorkspaceResponse {
        index_path: "SQLite".to_string(),
        files_indexed,
        symbols_indexed,
        generated_at_unix: crate::rust_index::now_unix(),
    })
}

pub fn status(root: &Path) -> RustIndexStatus {
    let conn = crate::database::init_db(root).unwrap();
    // Bug3: 读取元数据中记录的真实索引创建时间
    let generated_at =
        crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "rust");
    if generated_at.is_some() {
        let symbols_indexed: i64 = conn
            .query_row(
                "SELECT count(*) FROM rust_symbols WHERE workspace_root = ?",
                rusqlite::params![root.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let files_indexed: i64 = conn
            .query_row(
                "SELECT count(DISTINCT file_path) FROM rust_symbols WHERE workspace_root = ?",
                rusqlite::params![root.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        return RustIndexStatus {
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: generated_at,
            files_indexed: Some(files_indexed as usize),
            symbols_indexed: Some(symbols_indexed as usize),
        };
    }
    RustIndexStatus {
        index_path: "SQLite".to_string(),
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
) -> Result<Option<IndexRustWorkspaceResponse>> {
    if changed_path.extension().and_then(|value| value.to_str()) != Some("rs") {
        return Ok(None);
    }
    let conn = crate::database::init_db(root).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM rust_symbols WHERE workspace_root = ?",
            rusqlite::params![root.to_string_lossy()],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if count == 0 {
        return Ok(None);
    }
    index_workspace(root).map(Some)
}

pub fn list_symbols(
    root: &Path,
    request: ListRustSymbolsRequest,
) -> Result<ListRustSymbolsResponse> {
    let index_symbols = load_or_build_or_create(root)?;
    let symbols = index_symbols
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
        .map(RustSymbolSummary::from)
        .collect();
    Ok(ListRustSymbolsResponse { symbols })
}

pub fn search_symbols(
    root: &Path,
    request: SearchRustSymbolsRequest,
) -> Result<SearchRustSymbolsResponse> {
    let needle = request.query.to_lowercase();
    let index_symbols = load_or_build_or_create(root)?;
    let mut matches: Vec<_> = index_symbols
        .iter()
        .filter(|symbol| {
            [
                symbol.name.as_str(),
                symbol.signature.as_str(),
                symbol.docstring.as_str(),
                symbol.file_path.as_str(),
                symbol.module_path.as_str(),
                symbol.impl_type.as_deref().unwrap_or(""),
            ]
            .join("\n")
            .to_lowercase()
            .contains(&needle)
        })
        .take(request.limit.max(1))
        .map(RustSymbolSummary::from)
        .collect();
    matches.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
    });
    Ok(SearchRustSymbolsResponse {
        query: request.query,
        matches,
    })
}

pub fn read_symbol(root: &Path, request: ReadRustSymbolRequest) -> Result<ReadRustSymbolResponse> {
    let index_symbols = load_or_build_or_create(root)?;
    let symbol = index_symbols
        .iter()
        .find(|symbol| symbol.id == request.symbol_id)
        .cloned()
        .ok_or_else(|| RustIndexError::SymbolNotFound(request.symbol_id.clone()))?;
    let path = root.join(&symbol.file_path);
    let content = fs::read_to_string(path)?;
    let lines: Vec<_> = content.lines().collect();
    let content = lines[(symbol.start_line - 1)..symbol.end_line.min(lines.len())].join("\n");

    let (callers, callees, suggested_reads) = if request.include_context {
        build_context(&index_symbols, &symbol)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    Ok(ReadRustSymbolResponse {
        symbol,
        content,
        callers,
        callees,
        suggested_reads,
    })
}

fn build_index(root: &Path) -> Result<(usize, usize)> {
    let mut files_indexed = 0;
    let mut symbols_indexed = 0;
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

    let mut conn = crate::database::init_db(root).unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute(
        "DELETE FROM rust_symbols WHERE workspace_root = ?",
        rusqlite::params![root.to_string_lossy()],
    )
    .unwrap();

    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("rs") {
            continue;
        }
        let metadata = fs::metadata(path)?;
        if metadata.len() > MAX_RUST_FILE_BYTES {
            continue;
        }
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let parsed = match syn::parse_file(&content) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        files_indexed += 1;
        let parsed_file = parse_rust_file(root, path, &content, &parsed);
        for sym in parsed_file.symbols {
            let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();
            let kind = serde_json::to_string(&sym.kind)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            tx.execute(
                "INSERT INTO rust_symbols (
                    id, workspace_root, name, kind, file_path, module_path, start_line, end_line,
                    signature, docstring, visibility, impl_type, trait_name, calls_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    sym.id,
                    root.to_string_lossy(),
                    sym.name,
                    kind,
                    sym.file_path,
                    sym.module_path,
                    sym.start_line,
                    sym.end_line,
                    sym.signature,
                    sym.docstring,
                    sym.visibility,
                    sym.impl_type,
                    sym.trait_name,
                    calls_json
                ],
            )
            .unwrap();
            symbols_indexed += 1;
        }
    }
    tx.commit().unwrap();
    // Bug3: 记录本次索引的实际时间戳
    let ts = now_unix();
    let meta_conn = crate::database::init_db(root).unwrap();
    crate::database::upsert_index_metadata(&meta_conn, &root.to_string_lossy(), "rust", ts)
        .unwrap();
    Ok((files_indexed, symbols_indexed))
}

struct ParsedRustFile {
    symbols: Vec<RustSymbol>,
}

fn parse_rust_file(root: &Path, path: &Path, content: &str, file: &File) -> ParsedRustFile {
    let file_path = relative_display(root, path);
    let lines: Vec<&str> = content.lines().collect();
    let mut collector = RustCollector {
        file_path: file_path.clone(),
        lines: &lines,
        symbols: Vec::new(),
        uses: Vec::new(),
        module_stack: Vec::new(),
        impl_type: None,
        trait_name: None,
    };
    for item in &file.items {
        collector.collect_item(item);
    }

    ParsedRustFile {
        symbols: collector.symbols,
    }
}

struct RustCollector<'a> {
    file_path: String,
    lines: &'a [&'a str],
    symbols: Vec<RustSymbol>,
    uses: Vec<RustUse>,
    module_stack: Vec<String>,
    impl_type: Option<String>,
    trait_name: Option<String>,
}

impl RustCollector<'_> {
    fn collect_item(&mut self, item: &Item) {
        match item {
            Item::Use(item) => self.collect_use(item),
            Item::Fn(item) => {
                let start_line = start_line(item);
                let end_line = end_line(item);
                let name = item.sig.ident.to_string();
                self.symbols.push(RustSymbol {
                    id: symbol_id(
                        &self.file_path,
                        &self.module_path(),
                        &name,
                        start_line,
                        None,
                    ),
                    name,
                    kind: RustSymbolKind::Function,
                    file_path: self.file_path.clone(),
                    module_path: self.module_path(),
                    start_line,
                    end_line,
                    signature: item.sig.to_token_stream().to_string(),
                    docstring: collect_docstring(self.lines, start_line - 1),
                    visibility: item.vis.to_token_stream().to_string(),
                    impl_type: None,
                    trait_name: None,
                    calls: collect_calls_from_block(&item.block, self.lines),
                });
            }
            Item::Impl(item) => self.collect_impl(item),
            Item::Mod(item) => {
                let start_line = start_line(item);
                let end_line = end_line(item);
                let name = item.ident.to_string();
                self.symbols.push(RustSymbol {
                    id: symbol_id(
                        &self.file_path,
                        &self.module_path(),
                        &name,
                        start_line,
                        None,
                    ),
                    name: name.clone(),
                    kind: RustSymbolKind::Module,
                    file_path: self.file_path.clone(),
                    module_path: self.module_path(),
                    start_line,
                    end_line,
                    signature: item_signature(self.lines, start_line, end_line),
                    docstring: collect_docstring(self.lines, start_line - 1),
                    visibility: item.vis.to_token_stream().to_string(),
                    impl_type: None,
                    trait_name: None,
                    calls: Vec::new(),
                });
                if let Some((_, items)) = &item.content {
                    self.module_stack.push(name);
                    for item in items {
                        self.collect_item(item);
                    }
                    self.module_stack.pop();
                }
            }
            Item::Struct(item) => self.collect_named_item(
                &item.ident.to_string(),
                RustSymbolKind::Struct,
                item,
                &item.vis.to_token_stream().to_string(),
            ),
            Item::Enum(item) => self.collect_named_item(
                &item.ident.to_string(),
                RustSymbolKind::Enum,
                item,
                &item.vis.to_token_stream().to_string(),
            ),
            Item::Trait(item) => self.collect_named_item(
                &item.ident.to_string(),
                RustSymbolKind::Trait,
                item,
                &item.vis.to_token_stream().to_string(),
            ),
            Item::Type(item) => self.collect_named_item(
                &item.ident.to_string(),
                RustSymbolKind::TypeAlias,
                item,
                &item.vis.to_token_stream().to_string(),
            ),
            Item::Const(item) => self.collect_named_item(
                &item.ident.to_string(),
                RustSymbolKind::Const,
                item,
                &item.vis.to_token_stream().to_string(),
            ),
            Item::Static(item) => self.collect_named_item(
                &item.ident.to_string(),
                RustSymbolKind::Static,
                item,
                &item.vis.to_token_stream().to_string(),
            ),
            _ => {}
        }
    }

    fn collect_named_item(
        &mut self,
        name: &str,
        kind: RustSymbolKind,
        item: &impl Spanned,
        visibility: &str,
    ) {
        let start_line = start_line(item);
        let end_line = end_line(item);
        self.symbols.push(RustSymbol {
            id: symbol_id(&self.file_path, &self.module_path(), name, start_line, None),
            name: name.to_string(),
            kind,
            file_path: self.file_path.clone(),
            module_path: self.module_path(),
            start_line,
            end_line,
            signature: item_signature(self.lines, start_line, end_line),
            docstring: collect_docstring(self.lines, start_line - 1),
            visibility: visibility.to_string(),
            impl_type: None,
            trait_name: None,
            calls: Vec::new(),
        });
    }

    fn collect_impl(&mut self, item: &ItemImpl) {
        let previous_impl = self.impl_type.clone();
        let previous_trait = self.trait_name.clone();
        self.impl_type = Some(type_text(&item.self_ty));
        self.trait_name = item.trait_.as_ref().and_then(|(_, path, _)| {
            path.segments
                .last()
                .map(|segment| segment.ident.to_string())
        });
        for impl_item in &item.items {
            if let ImplItem::Fn(method) = impl_item {
                let start_line = start_line(method);
                let end_line = end_line(method);
                let name = method.sig.ident.to_string();
                self.symbols.push(RustSymbol {
                    id: symbol_id(
                        &self.file_path,
                        &self.module_path(),
                        &name,
                        start_line,
                        self.impl_type.as_deref(),
                    ),
                    name,
                    kind: RustSymbolKind::Method,
                    file_path: self.file_path.clone(),
                    module_path: self.module_path(),
                    start_line,
                    end_line,
                    signature: method.sig.to_token_stream().to_string(),
                    docstring: collect_docstring(self.lines, start_line - 1),
                    visibility: method.vis.to_token_stream().to_string(),
                    impl_type: self.impl_type.clone(),
                    trait_name: self.trait_name.clone(),
                    calls: collect_calls_from_block(&method.block, self.lines),
                });
            }
        }
        self.impl_type = previous_impl;
        self.trait_name = previous_trait;
    }

    fn collect_use(&mut self, item: &ItemUse) {
        collect_use_tree(&item.tree, Vec::new(), start_line(item), &mut self.uses);
    }

    fn module_path(&self) -> String {
        self.module_stack.join("::")
    }
}

fn collect_calls_from_block(block: &syn::Block, lines: &[&str]) -> Vec<RustCall> {
    let mut collector = CallCollector {
        lines,
        calls: Vec::new(),
    };
    collector.visit_block(block);
    collector.calls
}

struct CallCollector<'a> {
    lines: &'a [&'a str],
    calls: Vec<RustCall>,
}

impl<'ast> Visit<'ast> for CallCollector<'_> {
    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let Some((qualifier, target_text)) = call_target_from_expr(&node.func) {
            let line = start_line(&node.func);
            self.calls.push(RustCall {
                qualifier,
                target_text,
                line,
                snippet: line_snippet(self.lines, line),
            });
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let line = start_line(&node.method);
        self.calls.push(RustCall {
            qualifier: Some(expr_text(&node.receiver)),
            target_text: node.method.to_string(),
            line,
            snippet: line_snippet(self.lines, line),
        });
        syn::visit::visit_expr_method_call(self, node);
    }
}

fn call_target_from_expr(expr: &Expr) -> Option<(Option<String>, String)> {
    match expr {
        Expr::Path(path) => {
            let mut segments: Vec<_> = path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            let target = segments.pop()?;
            let qualifier = if segments.is_empty() {
                None
            } else {
                Some(segments.join("::"))
            };
            Some((qualifier, target))
        }
        _ => None,
    }
}

fn collect_use_tree(tree: &UseTree, mut prefix: Vec<String>, line: usize, uses: &mut Vec<RustUse>) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_tree(&path.tree, prefix, line, uses);
        }
        UseTree::Name(name) => {
            let local_name = name.ident.to_string();
            let mut full = prefix;
            full.push(local_name.clone());
            uses.push(RustUse {
                path: full.join("::"),
                local_name,
                alias: None,
                line,
            });
        }
        UseTree::Rename(rename) => {
            let alias = rename.rename.to_string();
            let mut full = prefix;
            full.push(rename.ident.to_string());
            uses.push(RustUse {
                path: full.join("::"),
                local_name: alias.clone(),
                alias: Some(alias),
                line,
            });
        }
        UseTree::Glob(_) => {
            let path = if prefix.is_empty() {
                "*".to_string()
            } else {
                format!("{}::*", prefix.join("::"))
            };
            uses.push(RustUse {
                path,
                local_name: "*".to_string(),
                alias: None,
                line,
            });
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_use_tree(item, prefix.clone(), line, uses);
            }
        }
    }
}

fn build_context(
    index_symbols: &[RustSymbol],
    symbol: &RustSymbol,
) -> (Vec<RustCaller>, Vec<RustCallee>, Vec<RustSuggestedRead>) {
    let mut id_to_symbol: BTreeMap<String, &RustSymbol> = BTreeMap::new();
    for item in index_symbols {
        id_to_symbol.insert(item.id.clone(), item);
    }

    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| RustCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: resolve_call(index_symbols, symbol, call)
                .into_iter()
                .map(|symbol| symbol.id.clone())
                .collect(),
        })
        .collect();

    let mut suggested_reads = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for callee in &callees {
        for matched_id in &callee.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched_symbol) = index_symbols.iter().find(|s| &s.id == matched_id) {
                suggested_reads.push(RustSuggestedRead {
                    reason: suggestion_reason(symbol, matched_symbol).to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: RustSymbolSummary::from(matched_symbol),
                });
            }
        }
    }

    let mut callers = Vec::new();
    for item in index_symbols {
        if item.id == symbol.id {
            continue;
        }
        for call in &item.calls {
            let matched = resolve_call(index_symbols, item, call)
                .into_iter()
                .any(|matched| matched.id == symbol.id);
            if matched {
                callers.push(RustCaller {
                    symbol_id: item.id.clone(),
                    name: item.name.clone(),
                    file_path: item.file_path.clone(),
                    line: call.line,
                    snippet: call.snippet.clone(),
                });
            }
        }
    }

    (callers, callees, suggested_reads)
}

fn resolve_call<'a>(
    index_symbols: &'a [RustSymbol],
    caller: &RustSymbol,
    call: &RustCall,
) -> Vec<&'a RustSymbol> {
    let mut matches = Vec::new();
    if let Some(qualifier) = call.qualifier.as_deref() {
        if matches_self_receiver(qualifier)
            && let Some(impl_type) = caller.impl_type.as_deref()
        {
            matches.extend(index_symbols.iter().filter(|symbol| {
                symbol.name == call.target_text
                    && symbol.impl_type.as_deref() == Some(impl_type)
                    && symbol.module_path == caller.module_path
            }));
        }

        let qualifier_tail = qualifier.rsplit("::").next().unwrap_or(qualifier);
        matches.extend(index_symbols.iter().filter(|symbol| {
            symbol.name == call.target_text
                && (symbol.impl_type.as_deref() == Some(qualifier_tail)
                    || symbol.module_path.ends_with(qualifier)
                    || symbol.name == qualifier_tail)
        }));
    } else {
        matches.extend(index_symbols.iter().filter(|symbol| {
            symbol.name == call.target_text
                && symbol.module_path == caller.module_path
                && matches!(symbol.kind, RustSymbolKind::Function)
        }));
        if matches.is_empty() {
            matches.extend(index_symbols.iter().filter(|symbol| {
                symbol.name == call.target_text && symbol.file_path == caller.file_path
            }));
        }
        if matches.is_empty() {
            matches.extend(
                index_symbols
                    .iter()
                    .filter(|symbol| symbol.name == call.target_text),
            );
        }
    }
    dedupe_symbols(matches)
}

fn matches_self_receiver(qualifier: &str) -> bool {
    matches!(qualifier, "self" | "& self" | "&self" | "Self")
}

fn dedupe_symbols(symbols: Vec<&RustSymbol>) -> Vec<&RustSymbol> {
    let mut seen = BTreeSet::new();
    symbols
        .into_iter()
        .filter(|symbol| seen.insert(symbol.id.clone()))
        .collect()
}

fn suggestion_reason(caller: &RustSymbol, matched: &RustSymbol) -> &'static str {
    if caller.impl_type.is_some() && caller.impl_type == matched.impl_type {
        "receiver_method_call"
    } else if caller.module_path == matched.module_path {
        "same_module_call"
    } else {
        "resolved_call"
    }
}

fn load_all_symbols(root: &std::path::Path) -> Result<Vec<RustSymbol>> {
    let conn = crate::database::init_db(root)
        .map_err(|e| RustIndexError::SymbolNotFound(e.to_string()))?;
    let mut stmt = conn.prepare("SELECT id, name, kind, file_path, module_path, start_line, end_line, signature, docstring, visibility, impl_type, trait_name, calls_json FROM rust_symbols WHERE workspace_root = ?").map_err(|e| RustIndexError::SymbolNotFound(e.to_string()))?;
    let symbol_iter = stmt
        .query_map(rusqlite::params![root.to_string_lossy()], |row| {
            Ok(RustSymbol {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: serde_json::from_str(&format!("\"{}\"", row.get::<_, String>(2)?))
                    .unwrap_or(RustSymbolKind::Function),
                file_path: row.get(3)?,
                module_path: row.get(4)?,
                start_line: row.get(5)?,
                end_line: row.get(6)?,
                signature: row.get(7)?,
                docstring: row.get(8)?,
                visibility: row.get(9)?,
                impl_type: row.get(10)?,
                trait_name: row.get(11)?,
                calls: serde_json::from_str(&row.get::<_, String>(12)?).unwrap_or_default(),
            })
        })
        .map_err(|e| RustIndexError::SymbolNotFound(e.to_string()))?;

    let mut symbols = Vec::new();
    for sym in symbol_iter {
        if let Ok(s) = sym {
            symbols.push(s);
        }
    }
    Ok(symbols)
}

fn load_or_build_or_create(root: &std::path::Path) -> Result<Vec<RustSymbol>> {
    // Bug4: 用元数据判断是否已索引，避免把「空项目」误判为「从未索引」
    let conn = crate::database::init_db(root).unwrap();
    let already_indexed =
        crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "rust").is_some();
    let symbols = load_all_symbols(root)?;
    if !already_indexed {
        index_workspace(root)?;
        return load_all_symbols(root);
    }
    Ok(symbols)
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

fn symbol_id(
    file_path: &str,
    module_path: &str,
    name: &str,
    line: usize,
    impl_type: Option<&str>,
) -> String {
    let prefix = if module_path.is_empty() {
        String::new()
    } else {
        format!("{module_path}::")
    };
    if let Some(impl_type) = impl_type {
        format!("rust:{file_path}:{prefix}{impl_type}::{name}:{line}")
    } else {
        format!("rust:{file_path}:{prefix}{name}:{line}")
    }
}

fn start_line(item: &impl Spanned) -> usize {
    item.span().start().line
}

fn end_line(item: &impl Spanned) -> usize {
    item.span().end().line
}

fn item_signature(lines: &[&str], start_line: usize, end_line: usize) -> String {
    let mut parts = Vec::new();
    for line in lines
        .iter()
        .skip(start_line.saturating_sub(1))
        .take(end_line.saturating_sub(start_line) + 1)
    {
        let before_body = line.split('{').next().unwrap_or(line).trim();
        if !before_body.is_empty() {
            parts.push(before_body.to_string());
        }
        if line.contains('{') || line.trim_end().ends_with(';') {
            break;
        }
    }
    parts.join(" ")
}

fn collect_docstring(lines: &[&str], decl_idx: usize) -> String {
    let mut docs = Vec::new();
    let mut idx = decl_idx;
    while idx > 0 {
        idx -= 1;
        let line = lines[idx].trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#[") || line.starts_with("# !") {
            // 智能跳过 Rust 宏与属性标注，确保不被阻断
            continue;
        }
        if let Some(comment) = line.strip_prefix("///") {
            docs.push(comment.trim().to_string());
        } else if let Some(comment) = line.strip_prefix("//!") {
            docs.push(comment.trim().to_string());
        } else if let Some(comment) = line.strip_prefix("//") {
            // 兼容普通的双斜杠注释
            docs.push(comment.trim().to_string());
        } else {
            break;
        }
    }
    docs.reverse();
    docs.join("\n")
}

fn line_snippet(lines: &[&str], line: usize) -> String {
    lines
        .get(line.saturating_sub(1))
        .map(|line| line.trim().to_string())
        .unwrap_or_default()
}

fn type_text(ty: &syn::Type) -> String {
    ty.to_token_stream()
        .to_string()
        .replace(" :: ", "::")
        .replace("& ", "&")
}

fn expr_text(expr: &Expr) -> String {
    expr.to_token_stream()
        .to_string()
        .replace(" :: ", "::")
        .replace("& ", "&")
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

fn default_symbol_limit() -> usize {
    20
}

impl From<&RustSymbol> for RustSymbolSummary {
    fn from(symbol: &RustSymbol) -> Self {
        Self {
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            file_path: symbol.file_path.clone(),
            module_path: symbol.module_path.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: symbol.signature.clone(),
            docstring: symbol.docstring.clone(),
            impl_type: symbol.impl_type.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_workspace(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("codex_rust_index_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("src")).unwrap();
        path
    }

    #[test]
    fn indexes_rust_symbols_docstrings_and_calls() {
        let root = temp_workspace("basic");
        fs::write(
            root.join("src").join("lib.rs"),
            r#"pub mod service {
    /// Handles PPT workflow.
    pub struct PptService;

    impl PptService {
        /// Creates a PPT workflow.
        pub fn create(&self, topic: String) -> Result<(), String> {
            validate_topic(&topic);
            self.save(topic)
        }

        pub fn save(&self, topic: String) -> Result<(), String> {
            Ok(())
        }
    }

    fn validate_topic(topic: &str) {}
}
"#,
        )
        .unwrap();

        let response = index_workspace(&root).unwrap();
        assert_eq!(response.files_indexed, 1);
        assert!(response.symbols_indexed >= 4);

        let search = search_symbols(
            &root,
            SearchRustSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "create".to_string(),
                limit: 10,
            },
        )
        .unwrap();
        let create = search
            .matches
            .iter()
            .find(|symbol| symbol.name == "create")
            .unwrap();
        assert_eq!(create.kind, RustSymbolKind::Method);
        assert!(create.signature.contains("create"));

        let read = read_symbol(
            &root,
            ReadRustSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: create.id.clone(),
                include_context: true,
            },
        )
        .unwrap();
        assert!(read.content.contains("pub fn create"));
        assert!(
            read.callees
                .iter()
                .any(|callee| callee.target_text == "save")
        );
        assert!(read.suggested_reads.iter().any(|suggestion| {
            suggestion.reason == "receiver_method_call" && suggestion.symbol.name == "save"
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_builds_index_when_missing() {
        let root = temp_workspace("auto_build");
        fs::write(
            root.join("src").join("lib.rs"),
            "/// AutoBuild proves search can index.\npub fn auto_build() {}\n",
        )
        .unwrap();

        let search = search_symbols(
            &root,
            SearchRustSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "auto_build".to_string(),
                limit: 5,
            },
        )
        .unwrap();

        assert_eq!(search.matches.len(), 1);
        // Bug2: 已迁移到 SQLite，不再生成 JSON 文件，改为验证元数据表中确实有索引记录
        {
            let conn = crate::database::init_db(&root).unwrap();
            let ts =
                crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "rust");
            assert!(
                ts.is_some(),
                "index metadata should be recorded after auto-build"
            );
        }
        let _ = fs::remove_dir_all(root);
    }
}
