use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use ignore::WalkBuilder;
use rustpython_parser::{Parse, ast::{self, Constant, Stmt, Expr, ExprCall}, text_size::TextSize};
use serde::{Deserialize, Serialize};

const INDEX_DIR: &str = ".codex-workspace-mcp";
const INDEX_FILE: &str = "python_index.json";
const MAX_PYTHON_FILE_BYTES: u64 = 2 * 1024 * 1024;
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
pub enum PythonIndexError {
    #[error("python index not found; call index_python_workspace first")]
    MissingIndex,
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, PythonIndexError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonIndex {
    pub workspace_root: String,
    pub generated_at_unix: u64,
    pub files_indexed: usize,
    #[serde(default)]
    pub files: Vec<PythonFileInfo>,
    pub symbols: Vec<PythonSymbol>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonFileInfo {
    pub file_path: String,
    #[serde(default)]
    pub imports: Vec<PythonImport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonImport {
    pub module: String,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonSymbol {
    pub id: String,
    pub name: String,
    pub kind: PythonSymbolKind,
    pub file_path: String,
    #[serde(default)]
    pub class_name: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub docstring: String,
    #[serde(default)]
    pub decorators: Vec<String>,
    #[serde(default)]
    pub calls: Vec<PythonCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PythonSymbolKind {
    Function,
    Method,
    Class,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonCall {
    #[serde(default)]
    pub qualifier: Option<String>,
    pub target_text: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Deserialize)]
pub struct IndexPythonWorkspaceRequest {
    pub workspace_root: String,
}

#[derive(Debug, Serialize)]
pub struct IndexPythonWorkspaceResponse {
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct PythonIndexStatus {
    pub index_path: String,
    pub exists: bool,
    pub workspace_root: String,
    pub generated_at_unix: Option<u64>,
    pub files_indexed: Option<usize>,
    pub symbols_indexed: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ListPythonSymbolsRequest {
    pub workspace_root: String,
    pub file_path: Option<String>,
    pub kind: Option<PythonSymbolKind>,
}

#[derive(Debug, Serialize)]
pub struct ListPythonSymbolsResponse {
    pub symbols: Vec<PythonSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct SearchPythonSymbolsRequest {
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_symbol_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchPythonSymbolsResponse {
    pub query: String,
    pub matches: Vec<PythonSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct ReadPythonSymbolRequest {
    pub workspace_root: String,
    pub symbol_id: String,
    #[serde(default)]
    pub include_context: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadPythonSymbolResponse {
    pub symbol: PythonSymbol,
    pub content: String,
    pub callers: Vec<PythonCaller>,
    pub callees: Vec<PythonCallee>,
    pub suggested_reads: Vec<PythonSuggestedRead>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PythonSymbolSummary {
    pub id: String,
    pub name: String,
    pub kind: PythonSymbolKind,
    pub file_path: String,
    pub class_name: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub docstring: String,
    pub decorators: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PythonCaller {
    pub symbol_id: String,
    pub name: String,
    pub file_path: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PythonCallee {
    pub target_text: String,
    pub line: usize,
    pub snippet: String,
    pub matched_symbol_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PythonSuggestedRead {
    pub reason: String,
    pub trigger_call: String,
    pub trigger_line: usize,
    pub trigger_snippet: String,
    pub symbol: PythonSymbolSummary,
}

pub fn index_workspace(root: &Path) -> Result<IndexPythonWorkspaceResponse> {
    let index = build_index(root)?;
    let index_path = index_path(root);
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&index_path, serde_json::to_vec_pretty(&index)?)?;
    Ok(IndexPythonWorkspaceResponse {
        index_path: relative_display(root, &index_path),
        files_indexed: index.files_indexed,
        symbols_indexed: index.symbols.len(),
        generated_at_unix: index.generated_at_unix,
    })
}

pub fn status(root: &Path) -> PythonIndexStatus {
    let index_path = index_path(root);
    if let Ok(content) = fs::read_to_string(&index_path)
        && let Ok(index) = serde_json::from_str::<PythonIndex>(&content)
    {
        return PythonIndexStatus {
            index_path: relative_display(root, &index_path),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: Some(index.generated_at_unix),
            files_indexed: Some(index.files_indexed),
            symbols_indexed: Some(index.symbols.len()),
        };
    }
    PythonIndexStatus {
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
) -> Result<Option<IndexPythonWorkspaceResponse>> {
    if changed_path.extension().and_then(|v| v.to_str()) != Some("py") {
        return Ok(None);
    }
    if !index_path(root).exists() {
        return Ok(None);
    }
    index_workspace(root).map(Some)
}

pub fn list_symbols(
    root: &Path,
    request: ListPythonSymbolsRequest,
) -> Result<ListPythonSymbolsResponse> {
    let index = load_or_build_or_create(root)?;
    let symbols = index
        .symbols
        .iter()
        .filter(|s| {
            request
                .file_path
                .as_deref()
                .map(|f| s.file_path == normalize_slashes(f))
                .unwrap_or(true)
        })
        .filter(|s| {
            request
                .kind
                .as_ref()
                .map(|k| &s.kind == k)
                .unwrap_or(true)
        })
        .map(PythonSymbolSummary::from)
        .collect();
    Ok(ListPythonSymbolsResponse { symbols })
}

pub fn search_symbols(
    root: &Path,
    request: SearchPythonSymbolsRequest,
) -> Result<SearchPythonSymbolsResponse> {
    let index = load_or_build_or_create(root)?;
    let needle = request.query.to_lowercase();
    let mut matches: Vec<_> = index
        .symbols
        .iter()
        .filter(|s| {
            [
                s.name.as_str(),
                s.signature.as_str(),
                s.docstring.as_str(),
                s.file_path.as_str(),
                s.class_name.as_deref().unwrap_or(""),
                &s.decorators.join(" "),
            ]
            .join("\n")
            .to_lowercase()
            .contains(&needle)
        })
        .take(request.limit.max(1))
        .map(PythonSymbolSummary::from)
        .collect();
    matches.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
    });
    Ok(SearchPythonSymbolsResponse {
        query: request.query,
        matches,
    })
}

pub fn read_symbol(root: &Path, request: ReadPythonSymbolRequest) -> Result<ReadPythonSymbolResponse> {
    let index = load_or_build(root)?;
    let symbol = index
        .symbols
        .iter()
        .find(|s| s.id == request.symbol_id)
        .cloned()
        .ok_or_else(|| PythonIndexError::SymbolNotFound(request.symbol_id.clone()))?;
    let path = root.join(&symbol.file_path);
    let content = fs::read_to_string(path)?;
    let lines: Vec<_> = content.lines().collect();
    let content = lines[(symbol.start_line - 1)..symbol.end_line.min(lines.len())].join("\n");

    let (callers, callees, suggested_reads) = if request.include_context {
        build_context(&index, &symbol)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    Ok(ReadPythonSymbolResponse {
        symbol,
        content,
        callers,
        callees,
        suggested_reads,
    })
}

fn build_index(root: &Path) -> Result<PythonIndex> {
    let mut files = Vec::new();
    let mut symbols = Vec::new();
    let mut files_indexed = 0;

    let mut builder = WalkBuilder::new(root);
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
            Err(_) => continue,
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("py") {
            continue;
        }
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() > MAX_PYTHON_FILE_BYTES {
            continue;
        }
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let parsed = match ast::Suite::parse(&content, "<embedded>") {
            Ok(p) => p,
            Err(_) => continue,
        };
        files_indexed += 1;
        let result = parse_python_file(root, path, &content, &parsed);
        files.push(result.file);
        symbols.extend(result.symbols);
    }

    Ok(PythonIndex {
        workspace_root: root.display().to_string(),
        generated_at_unix: now_unix(),
        files_indexed,
        files,
        symbols,
    })
}

struct ParsedPythonFile {
    file: PythonFileInfo,
    symbols: Vec<PythonSymbol>,
}

struct LineMap {
    line_starts: Vec<u32>,
}

impl LineMap {
    fn new(content: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in content.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        Self { line_starts }
    }

    fn line(&self, offset: TextSize) -> usize {
        let off: u32 = offset.into();
        match self.line_starts.binary_search(&off) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    }
}

fn parse_python_file(root: &Path, path: &Path, content: &str, stmts: &[Stmt]) -> ParsedPythonFile {
    let file_path = relative_display(root, path);
    let lines: Vec<&str> = content.lines().collect();
    let line_map = LineMap::new(content);
    let mut imports = Vec::new();
    let mut symbols = Vec::new();

    collect_stmts(stmts, &file_path, &lines, &line_map, None, &mut imports, &mut symbols);

    ParsedPythonFile {
        file: PythonFileInfo { file_path, imports },
        symbols,
    }
}

fn collect_stmts(
    stmts: &[Stmt],
    file_path: &str,
    lines: &[&str],
    line_map: &LineMap,
    class_name: Option<&str>,
    imports: &mut Vec<PythonImport>,
    symbols: &mut Vec<PythonSymbol>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Import(node) => {
                for alias in &node.names {
                    imports.push(PythonImport {
                        module: alias.name.to_string(),
                        name: None,
                        alias: alias.asname.as_ref().map(|a| a.to_string()),
                        line: line_map.line(node.range.start()),
                    });
                }
            }
            Stmt::ImportFrom(node) => {
                let module = node.module.as_ref().map(|m| m.to_string()).unwrap_or_default();
                for alias in &node.names {
                    imports.push(PythonImport {
                        module: module.clone(),
                        name: Some(alias.name.to_string()),
                        alias: alias.asname.as_ref().map(|a| a.to_string()),
                        line: line_map.line(node.range.start()),
                    });
                }
            }
            Stmt::FunctionDef(node) => {
                let start_line = line_map.line(node.range.start());
                let end_line = line_map.line(node.range.end());
                let name = node.name.to_string();
                let decorators: Vec<String> = node
                    .decorator_list
                    .iter()
                    .map(|d| expr_text(d))
                    .collect();
                let kind = if class_name.is_some() {
                    PythonSymbolKind::Method
                } else {
                    PythonSymbolKind::Function
                };
                let signature = build_function_signature(&name, &node.args, lines, start_line);
                let docstring = extract_docstring(&node.body);
                let calls = collect_calls(&node.body, lines, line_map);
                symbols.push(PythonSymbol {
                    id: symbol_id(file_path, class_name, &name, start_line),
                    name,
                    kind,
                    file_path: file_path.to_string(),
                    class_name: class_name.map(str::to_string),
                    start_line,
                    end_line,
                    signature,
                    docstring,
                    decorators,
                    calls,
                });
            }
            Stmt::ClassDef(node) => {
                let start_line = line_map.line(node.range.start());
                let end_line = line_map.line(node.range.end());
                let name = node.name.to_string();
                let decorators: Vec<String> = node
                    .decorator_list
                    .iter()
                    .map(|d| expr_text(d))
                    .collect();
                let bases: Vec<String> = node.bases.iter().map(|b| expr_text(b)).collect();
                let signature = if bases.is_empty() {
                    format!("class {name}")
                } else {
                    format!("class {name}({})", bases.join(", "))
                };
                let docstring = extract_docstring(&node.body);
                symbols.push(PythonSymbol {
                    id: symbol_id(file_path, None, &name, start_line),
                    name: name.clone(),
                    kind: PythonSymbolKind::Class,
                    file_path: file_path.to_string(),
                    class_name: None,
                    start_line,
                    end_line,
                    signature,
                    docstring,
                    decorators,
                    calls: Vec::new(),
                });
                collect_stmts(&node.body, file_path, lines, line_map, Some(&name), imports, symbols);
            }
            _ => {}
        }
    }
}

fn build_function_signature(
    name: &str,
    args: &ast::Arguments,
    lines: &[&str],
    start_line: usize,
) -> String {
    // Try to reconstruct from source line first — most accurate
    if let Some(line) = lines.get(start_line.saturating_sub(1)) {
        let trimmed = line.trim();
        if trimmed.starts_with("def ") || trimmed.starts_with("async def ") {
            let sig = trimmed.trim_end_matches(':').trim_end();
            return sig.to_string();
        }
    }
    // Fallback: build from parsed args
    let mut parts = Vec::new();
    for arg in args.posonlyargs.iter().chain(args.args.iter()) {
        parts.push(arg.def.arg.to_string());
    }
    format!("def {name}({})", parts.join(", "))
}

fn extract_docstring(body: &[Stmt]) -> String {
    if let Some(Stmt::Expr(expr_stmt)) = body.first() {
        if let Expr::Constant(c) = expr_stmt.value.as_ref() {
            if let Constant::Str(s) = &c.value {
                return s.clone();
            }
        }
    }
    String::new()
}

fn collect_calls(body: &[Stmt], lines: &[&str], line_map: &LineMap) -> Vec<PythonCall> {
    let mut calls = Vec::new();
    collect_calls_from_stmts(body, lines, line_map, &mut calls);
    calls
}

fn collect_calls_from_stmts(stmts: &[Stmt], lines: &[&str], line_map: &LineMap, calls: &mut Vec<PythonCall>) {
    for stmt in stmts {
        collect_calls_from_stmt(stmt, lines, line_map, calls);
    }
}

fn collect_calls_from_stmt(stmt: &Stmt, lines: &[&str], line_map: &LineMap, calls: &mut Vec<PythonCall>) {
    match stmt {
        Stmt::Expr(node) => collect_calls_from_expr(&node.value, lines, line_map, calls),
        Stmt::Assign(node) => {
            collect_calls_from_expr(&node.value, lines, line_map, calls);
        }
        Stmt::AnnAssign(node) => {
            if let Some(val) = &node.value {
                collect_calls_from_expr(val, lines, line_map, calls);
            }
        }
        Stmt::Return(node) => {
            if let Some(val) = &node.value {
                collect_calls_from_expr(val, lines, line_map, calls);
            }
        }
        Stmt::If(node) => {
            collect_calls_from_expr(&node.test, lines, line_map, calls);
            collect_calls_from_stmts(&node.body, lines, line_map, calls);
            collect_calls_from_stmts(&node.orelse, lines, line_map, calls);
        }
        Stmt::For(node) => {
            collect_calls_from_expr(&node.iter, lines, line_map, calls);
            collect_calls_from_stmts(&node.body, lines, line_map, calls);
            collect_calls_from_stmts(&node.orelse, lines, line_map, calls);
        }
        Stmt::While(node) => {
            collect_calls_from_expr(&node.test, lines, line_map, calls);
            collect_calls_from_stmts(&node.body, lines, line_map, calls);
            collect_calls_from_stmts(&node.orelse, lines, line_map, calls);
        }
        Stmt::With(node) => {
            for item in &node.items {
                collect_calls_from_expr(&item.context_expr, lines, line_map, calls);
            }
            collect_calls_from_stmts(&node.body, lines, line_map, calls);
        }
        Stmt::Try(node) => {
            collect_calls_from_stmts(&node.body, lines, line_map, calls);
            for handler in &node.handlers {
                let ast::ExceptHandler::ExceptHandler(h) = handler;
                collect_calls_from_stmts(&h.body, lines, line_map, calls);
            }
            collect_calls_from_stmts(&node.finalbody, lines, line_map, calls);
        }
        Stmt::FunctionDef(node) => {
            collect_calls_from_stmts(&node.body, lines, line_map, calls);
        }
        _ => {}
    }
}

fn collect_calls_from_expr(expr: &Expr, lines: &[&str], line_map: &LineMap, calls: &mut Vec<PythonCall>) {
    match expr {
        Expr::Call(call) => {
            let line = line_map.line(call.range.start());
            let snippet = line_snippet(lines, line);
            let (qualifier, target_text) = call_target(call);
            calls.push(PythonCall {
                qualifier,
                target_text,
                line,
                snippet,
            });
            for arg in &call.args {
                collect_calls_from_expr(arg, lines, line_map, calls);
            }
            for kw in &call.keywords {
                collect_calls_from_expr(&kw.value, lines, line_map, calls);
            }
        }
        Expr::BoolOp(node) => {
            for val in &node.values {
                collect_calls_from_expr(val, lines, line_map, calls);
            }
        }
        Expr::BinOp(node) => {
            collect_calls_from_expr(&node.left, lines, line_map, calls);
            collect_calls_from_expr(&node.right, lines, line_map, calls);
        }
        Expr::UnaryOp(node) => {
            collect_calls_from_expr(&node.operand, lines, line_map, calls);
        }
        Expr::IfExp(node) => {
            collect_calls_from_expr(&node.test, lines, line_map, calls);
            collect_calls_from_expr(&node.body, lines, line_map, calls);
            collect_calls_from_expr(&node.orelse, lines, line_map, calls);
        }
        Expr::Await(node) => {
            collect_calls_from_expr(&node.value, lines, line_map, calls);
        }
        Expr::Attribute(node) => {
            collect_calls_from_expr(&node.value, lines, line_map, calls);
        }
        Expr::Subscript(node) => {
            collect_calls_from_expr(&node.value, lines, line_map, calls);
        }
        Expr::List(node) => {
            for elt in &node.elts {
                collect_calls_from_expr(elt, lines, line_map, calls);
            }
        }
        Expr::Tuple(node) => {
            for elt in &node.elts {
                collect_calls_from_expr(elt, lines, line_map, calls);
            }
        }
        _ => {}
    }
}

fn call_target(call: &ExprCall) -> (Option<String>, String) {
    match call.func.as_ref() {
        Expr::Attribute(attr) => {
            let qualifier = expr_text(&attr.value);
            let target = attr.attr.to_string();
            (Some(qualifier), target)
        }
        Expr::Name(name) => (None, name.id.to_string()),
        other => (None, expr_text(other)),
    }
}

fn expr_text(expr: &Expr) -> String {
    match expr {
        Expr::Name(n) => n.id.to_string(),
        Expr::Attribute(a) => format!("{}.{}", expr_text(&a.value), a.attr),
        Expr::Call(c) => {
            let (q, t) = call_target(c);
            if let Some(q) = q {
                format!("{q}.{t}()")
            } else {
                format!("{t}()")
            }
        }
        Expr::Constant(c) => {
            if let Constant::Str(s) = &c.value {
                s.clone()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

fn build_context(
    index: &PythonIndex,
    symbol: &PythonSymbol,
) -> (Vec<PythonCaller>, Vec<PythonCallee>, Vec<PythonSuggestedRead>) {
    let id_to_symbol: BTreeMap<String, &PythonSymbol> =
        index.symbols.iter().map(|s| (s.id.clone(), s)).collect();

    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| PythonCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: resolve_call(index, symbol, call)
                .into_iter()
                .map(|s| s.id.clone())
                .collect(),
        })
        .collect();

    let mut suggested_reads = Vec::new();
    let mut seen = BTreeSet::new();
    for callee in &callees {
        for matched_id in &callee.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched) = id_to_symbol.get(matched_id) {
                suggested_reads.push(PythonSuggestedRead {
                    reason: suggestion_reason(symbol, matched).to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: PythonSymbolSummary::from(*matched),
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
            if resolve_call(index, item, call)
                .into_iter()
                .any(|m| m.id == symbol.id)
            {
                callers.push(PythonCaller {
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
    index: &'a PythonIndex,
    caller: &PythonSymbol,
    call: &PythonCall,
) -> Vec<&'a PythonSymbol> {
    let mut matches = Vec::new();

    if let Some(qualifier) = call.qualifier.as_deref() {
        // self.method() — look for methods on same class
        if qualifier == "self" || qualifier == "cls" {
            if let Some(class_name) = caller.class_name.as_deref() {
                matches.extend(index.symbols.iter().filter(|s| {
                    s.name == call.target_text
                        && s.class_name.as_deref() == Some(class_name)
                        && s.file_path == caller.file_path
                }));
            }
        }
        // Qualified.name() — qualifier tail matches class name
        let qualifier_tail = qualifier.rsplit('.').next().unwrap_or(qualifier);
        matches.extend(index.symbols.iter().filter(|s| {
            s.name == call.target_text
                && s.class_name.as_deref() == Some(qualifier_tail)
        }));
    } else {
        // Bare call — same file first, then workspace-wide
        matches.extend(index.symbols.iter().filter(|s| {
            s.name == call.target_text
                && s.file_path == caller.file_path
                && matches!(s.kind, PythonSymbolKind::Function | PythonSymbolKind::Class)
        }));
        if matches.is_empty() {
            matches.extend(index.symbols.iter().filter(|s| {
                s.name == call.target_text
                    && matches!(s.kind, PythonSymbolKind::Function | PythonSymbolKind::Class)
            }));
        }
    }

    dedupe_symbols(matches)
}

fn dedupe_symbols<'a>(symbols: Vec<&'a PythonSymbol>) -> Vec<&'a PythonSymbol> {
    let mut seen = BTreeSet::new();
    symbols
        .into_iter()
        .filter(|s| seen.insert(s.id.clone()))
        .collect()
}

fn suggestion_reason(caller: &PythonSymbol, matched: &PythonSymbol) -> &'static str {
    if caller.class_name.is_some() && caller.class_name == matched.class_name {
        "receiver_method_call"
    } else if caller.file_path == matched.file_path {
        "same_file_call"
    } else {
        "resolved_call"
    }
}

fn load_or_build(root: &Path) -> Result<PythonIndex> {
    let path = index_path(root);
    if !path.exists() {
        return Err(PythonIndexError::MissingIndex);
    }
    let content = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

fn load_or_build_or_create(root: &Path) -> Result<PythonIndex> {
    match load_or_build(root) {
        Ok(index) => Ok(index),
        Err(PythonIndexError::MissingIndex) => {
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

fn symbol_id(file_path: &str, class_name: Option<&str>, name: &str, line: usize) -> String {
    if let Some(class) = class_name {
        format!("python:{file_path}:{class}::{name}:{line}")
    } else {
        format!("python:{file_path}:{name}:{line}")
    }
}

fn line_snippet(lines: &[&str], line: usize) -> String {
    lines
        .get(line.saturating_sub(1))
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .unwrap_or_default()
}

fn default_symbol_limit() -> usize {
    20
}

impl From<&PythonSymbol> for PythonSymbolSummary {
    fn from(s: &PythonSymbol) -> Self {
        Self {
            id: s.id.clone(),
            name: s.name.clone(),
            kind: s.kind.clone(),
            file_path: s.file_path.clone(),
            class_name: s.class_name.clone(),
            start_line: s.start_line,
            end_line: s.end_line,
            signature: s.signature.clone(),
            docstring: s.docstring.clone(),
            decorators: s.decorators.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("codex_python_index_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn indexes_functions_classes_and_methods() {
        let root = temp_workspace("basic");
        fs::write(
            root.join("service.py"),
            r#"import os
from pathlib import Path

class PptService:
    """Generates PPT files."""

    def __init__(self, topic: str):
        """Init with topic."""
        self.topic = topic

    def create(self) -> str:
        """Create the PPT."""
        return self._render()

    def _render(self) -> str:
        return self.topic

def validate(topic: str) -> bool:
    """Validate a topic string."""
    return bool(topic)
"#,
        )
        .unwrap();

        let resp = index_workspace(&root).unwrap();
        assert!(resp.files_indexed >= 1);
        assert!(resp.symbols_indexed >= 4);

        let search = search_symbols(
            &root,
            SearchPythonSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "create".to_string(),
                limit: 10,
            },
        )
        .unwrap();
        let create = search.matches.iter().find(|s| s.name == "create").unwrap();
        assert_eq!(create.kind, PythonSymbolKind::Method);
        assert_eq!(create.class_name.as_deref(), Some("PptService"));
        assert!(create.signature.contains("def create"));
        assert!(create.docstring.contains("Create"));

        let read = read_symbol(
            &root,
            ReadPythonSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: create.id.clone(),
                include_context: true,
            },
        )
        .unwrap();
        assert!(read.content.contains("def create"));
        assert!(read.callees.iter().any(|c| c.target_text == "_render"));
        assert!(read
            .suggested_reads
            .iter()
            .any(|s| s.symbol.name == "_render"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extracts_imports() {
        let root = temp_workspace("imports");
        fs::write(
            root.join("app.py"),
            "import os\nfrom pathlib import Path, PurePath\n",
        )
        .unwrap();
        let resp = index_workspace(&root).unwrap();
        let file = resp.index_path;
        assert!(!file.is_empty());

        let index = load_or_build(&root).unwrap();
        let f = &index.files[0];
        assert!(f.imports.iter().any(|i| i.module == "os"));
        assert!(f.imports.iter().any(|i| i.name.as_deref() == Some("Path")));

        let _ = fs::remove_dir_all(root);
    }
}
