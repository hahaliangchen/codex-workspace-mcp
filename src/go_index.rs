use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

const INDEX_DIR: &str = ".codex-workspace-mcp";
const INDEX_FILE: &str = "go_index.json";
const MAX_GO_FILE_BYTES: u64 = 2 * 1024 * 1024;
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
pub enum GoIndexError {
    #[error("go index not found; call index_go_workspace first")]
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

pub type Result<T> = std::result::Result<T, GoIndexError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoIndex {
    pub workspace_root: String,
    pub generated_at_unix: u64,
    pub files_indexed: usize,
    #[serde(default)]
    pub files: Vec<GoFileInfo>,
    pub symbols: Vec<GoSymbol>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoFileInfo {
    pub file_path: String,
    pub package: String,
    #[serde(default)]
    pub imports: Vec<GoImport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoImport {
    pub alias: Option<String>,
    pub path: String,
    pub package_hint: String,
    pub dot: bool,
    pub blank: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoSymbol {
    pub id: String,
    #[serde(default)]
    pub file_imports: Vec<GoImport>,
    pub name: String,
    pub kind: GoSymbolKind,
    pub package: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub docstring: String,
    #[serde(default)]
    pub receiver: Option<String>,
    #[serde(default)]
    pub receiver_name: Option<String>,
    #[serde(default)]
    pub receiver_type: Option<String>,
    #[serde(default)]
    pub calls: Vec<GoCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoSymbolKind {
    Function,
    Method,
    Struct,
    Interface,
    Type,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoCall {
    #[serde(default)]
    pub qualifier: Option<String>,
    pub target_text: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Deserialize)]
pub struct IndexGoWorkspaceRequest {
    pub workspace_root: String,
}

#[derive(Debug, Serialize)]
pub struct IndexGoWorkspaceResponse {
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct GoIndexStatus {
    pub index_path: String,
    pub exists: bool,
    pub workspace_root: String,
    pub generated_at_unix: Option<u64>,
    pub files_indexed: Option<usize>,
    pub symbols_indexed: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ListGoSymbolsRequest {
    pub workspace_root: String,
    pub file_path: Option<String>,
    pub kind: Option<GoSymbolKind>,
}

#[derive(Debug, Serialize)]
pub struct ListGoSymbolsResponse {
    pub symbols: Vec<GoSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct SearchGoSymbolsRequest {
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_symbol_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchGoSymbolsResponse {
    pub query: String,
    pub matches: Vec<GoSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct ReadGoSymbolRequest {
    pub workspace_root: String,
    pub symbol_id: String,
    #[serde(default)]
    pub include_context: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadGoSymbolResponse {
    pub symbol: GoSymbol,
    pub content: String,
    pub callers: Vec<GoCaller>,
    pub callees: Vec<GoCallee>,
    pub suggested_reads: Vec<GoSuggestedRead>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoSymbolSummary {
    pub id: String,
    pub name: String,
    pub kind: GoSymbolKind,
    pub package: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub docstring: String,
    pub receiver: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoCaller {
    pub symbol_id: String,
    pub name: String,
    pub file_path: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoCallee {
    pub target_text: String,
    pub line: usize,
    pub snippet: String,
    pub matched_symbol_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoSuggestedRead {
    pub reason: String,
    pub trigger_call: String,
    pub trigger_line: usize,
    pub trigger_snippet: String,
    pub symbol: GoSymbolSummary,
}

pub fn index_workspace(root: &Path) -> Result<IndexGoWorkspaceResponse> {
    let (files_indexed, symbols_indexed) = build_index(root)?;
    Ok(IndexGoWorkspaceResponse {
        index_path: "SQLite".to_string(),
        files_indexed,
        symbols_indexed,
        generated_at_unix: now_unix(),
    })
}

pub fn status(root: &Path) -> GoIndexStatus {
    let conn = crate::database::init_db(root).unwrap();
    // Bug3: 读取元数据中记录的真实索引创建时间
    let generated_at = crate::database::get_index_generated_at(
        &conn,
        &root.to_string_lossy(),
        "go",
    );
    if generated_at.is_some() {
        let symbols_indexed: i64 = conn.query_row(
            "SELECT count(*) FROM go_symbols WHERE workspace_root = ?",
            rusqlite::params![root.to_string_lossy()],
            |row| row.get(0)
        ).unwrap_or(0);
        let files_indexed: i64 = conn.query_row(
            "SELECT count(DISTINCT file_path) FROM go_symbols WHERE workspace_root = ?",
            rusqlite::params![root.to_string_lossy()],
            |row| row.get(0)
        ).unwrap_or(0);
        return GoIndexStatus {
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: generated_at,
            files_indexed: Some(files_indexed as usize),
            symbols_indexed: Some(symbols_indexed as usize),
        };
    }
    GoIndexStatus {
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
) -> Result<Option<IndexGoWorkspaceResponse>> {
    if changed_path.extension().and_then(|value| value.to_str()) != Some("go") {
        return Ok(None);
    }
    let conn = crate::database::init_db(root).unwrap();
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM go_symbols WHERE workspace_root = ?",
        rusqlite::params![root.to_string_lossy()],
        |row| row.get(0)
    ).unwrap_or(0);
    if count == 0 {
        return Ok(None);
    }
    index_workspace(root).map(Some)
}

pub fn list_symbols(root: &Path, request: ListGoSymbolsRequest) -> Result<ListGoSymbolsResponse> {
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
        .map(GoSymbolSummary::from)
        .collect();
    Ok(ListGoSymbolsResponse { symbols })
}

pub fn search_symbols(
    root: &Path,
    request: SearchGoSymbolsRequest,
) -> Result<SearchGoSymbolsResponse> {
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
                symbol.package.as_str(),
            ]
            .join("\n")
            .to_lowercase()
            .contains(&needle)
        })
        .take(request.limit.max(1))
        .map(GoSymbolSummary::from)
        .collect();
    matches.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
    });
    Ok(SearchGoSymbolsResponse {
        query: request.query,
        matches,
    })
}

pub fn read_symbol(root: &Path, request: ReadGoSymbolRequest) -> Result<ReadGoSymbolResponse> {
    let index_symbols = load_or_build_or_create(root)?;
    let symbol = index_symbols
        .iter()
        .find(|symbol| symbol.id == request.symbol_id)
        .cloned()
        .ok_or_else(|| GoIndexError::SymbolNotFound(request.symbol_id.clone()))?;
    let path = root.join(&symbol.file_path);
    let content = fs::read_to_string(path)?;
    let lines: Vec<_> = content.lines().collect();
    let content = lines[(symbol.start_line - 1)..symbol.end_line.min(lines.len())].join("\n");

    let (callers, callees, suggested_reads) = if request.include_context {
        build_context(&index_symbols, &symbol)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    Ok(ReadGoSymbolResponse {
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
    tx.execute("DELETE FROM go_symbols WHERE workspace_root = ?", rusqlite::params![root.to_string_lossy()]).unwrap();

    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("go") {
            continue;
        }
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > MAX_GO_FILE_BYTES {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let parsed = parse_go_file(root, path, &content);
        files_indexed += 1;
        for sym in parsed.symbols {
            let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();
            let file_imports_json = serde_json::to_string(&parsed.file.imports).unwrap_or_default();
            let kind = serde_json::to_string(&sym.kind).unwrap_or_default().trim_matches('"').to_string();
            tx.execute(
                "INSERT INTO go_symbols (
                    id, workspace_root, name, kind, package_name, file_path, start_line, end_line,
                    signature, docstring, receiver, receiver_name, receiver_type, calls_json, file_imports_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    sym.id, root.to_string_lossy(), sym.name, kind, sym.package, sym.file_path,
                    sym.start_line, sym.end_line, sym.signature, sym.docstring,
                    sym.receiver, sym.receiver_name, sym.receiver_type, calls_json, file_imports_json
                ]
            ).unwrap();
            symbols_indexed += 1;
        }
    }
    tx.commit().unwrap();
    // Bug3: 记录本次索引的实际时间戳
    let ts = now_unix();
    let meta_conn = crate::database::init_db(root).unwrap();
    crate::database::upsert_index_metadata(
        &meta_conn,
        &root.to_string_lossy(),
        "go",
        ts,
    ).unwrap();
    Ok((files_indexed, symbols_indexed))
}

struct ParsedGoFile {
    file: GoFileInfo,
    symbols: Vec<GoSymbol>,
}

fn parse_go_file(root: &Path, path: &Path, content: &str) -> ParsedGoFile {
    let relative_path = relative_display(root, path);
    let lines: Vec<&str> = content.lines().collect();
    let mut symbols = Vec::new();
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .is_err()
    {
        return ParsedGoFile {
            file: GoFileInfo {
                file_path: relative_path,
                package: String::new(),
                imports: Vec::new(),
            },
            symbols,
        };
    }
    let Some(tree) = parser.parse(content, None) else {
        return ParsedGoFile {
            file: GoFileInfo {
                file_path: relative_path,
                package: String::new(),
                imports: Vec::new(),
            },
            symbols,
        };
    };
    let root_node = tree.root_node();
    let package = parse_package_ast(root_node, content);
    let imports = parse_imports_ast(root_node, content);

    collect_symbols_ast(
        root_node,
        content,
        &lines,
        &relative_path,
        &package,
        &mut symbols,
    );

    ParsedGoFile {
        file: GoFileInfo {
            file_path: relative_path,
            package,
            imports,
        },
        symbols,
    }
}

fn parse_package_ast(root: Node<'_>, content: &str) -> String {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "package_clause" {
            continue;
        }
        let mut package_cursor = child.walk();
        for item in child.children(&mut package_cursor) {
            if item.kind() == "package_identifier" || item.kind() == "identifier" {
                return node_text(item, content).to_string();
            }
        }
    }
    String::new()
}

fn parse_imports_ast(root: Node<'_>, content: &str) -> Vec<GoImport> {
    let mut imports = Vec::new();
    visit_nodes(root, &mut |node| {
        if node.kind() == "import_spec"
            && let Some(import) = parse_import_spec(node, content)
        {
            imports.push(import);
        }
    });
    imports
}

fn parse_import_spec(node: Node<'_>, content: &str) -> Option<GoImport> {
    let mut alias = None;
    let mut path = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "package_identifier" | "identifier" => {
                alias = Some(node_text(child, content).to_string())
            }
            "." | "_" => alias = Some(child.kind().to_string()),
            "interpreted_string_literal" | "raw_string_literal" => {
                path = Some(unquote_import_path(node_text(child, content)));
            }
            _ => {}
        }
    }
    let path = path?;
    let dot = alias.as_deref() == Some(".");
    let blank = alias.as_deref() == Some("_");
    let alias = alias.filter(|value| value != "." && value != "_");
    let package_hint = path
        .rsplit('/')
        .next()
        .unwrap_or(path.as_str())
        .replace('-', "_");
    Some(GoImport {
        alias,
        path,
        package_hint,
        dot,
        blank,
    })
}

fn collect_symbols_ast(
    root: Node<'_>,
    content: &str,
    lines: &[&str],
    relative_path: &str,
    package: &str,
    symbols: &mut Vec<GoSymbol>,
) {
    visit_nodes(root, &mut |node| match node.kind() {
        "function_declaration" => {
            if let Some(symbol) =
                parse_function_symbol(node, content, lines, relative_path, package)
            {
                symbols.push(symbol);
            }
        }
        "method_declaration" => {
            if let Some(symbol) = parse_method_symbol(node, content, lines, relative_path, package)
            {
                symbols.push(symbol);
            }
        }
        "type_spec" => {
            if let Some(symbol) = parse_type_symbol(node, content, lines, relative_path, package) {
                symbols.push(symbol);
            }
        }
        _ => {}
    });
}

fn parse_function_symbol(
    node: Node<'_>,
    content: &str,
    lines: &[&str],
    relative_path: &str,
    package: &str,
) -> Option<GoSymbol> {
    let name_node = child_by_kind(node, &["identifier"])?;
    let name = node_text(name_node, content).to_string();
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;
    Some(GoSymbol {
        id: symbol_id(relative_path, &name, start_line, None),
        name,
        kind: GoSymbolKind::Function,
        package: package.to_string(),
        file_path: relative_path.to_string(),
        start_line,
        end_line,
        signature: signature_from_node(node, content),
        docstring: collect_docstring(lines, start_line - 1),
        receiver: None,
        receiver_name: None,
        receiver_type: None,
        calls: collect_calls_ast(node, content, lines),
        file_imports: Vec::new(),
    })
}

fn parse_method_symbol(
    node: Node<'_>,
    content: &str,
    lines: &[&str],
    relative_path: &str,
    package: &str,
) -> Option<GoSymbol> {
    let name_node = child_by_kind(node, &["field_identifier", "identifier"])?;
    let name = node_text(name_node, content).to_string();
    let receiver_node = child_by_kind(node, &["parameter_list"])?;
    let receiver = normalize_whitespace(node_text(receiver_node, content));
    let (receiver_name, receiver_type) = parse_receiver_parts(receiver_node, content);
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;
    Some(GoSymbol {
        id: symbol_id(relative_path, &name, start_line, Some(receiver.as_str())),
        name,
        kind: GoSymbolKind::Method,
        package: package.to_string(),
        file_path: relative_path.to_string(),
        start_line,
        end_line,
        signature: signature_from_node(node, content),
        docstring: collect_docstring(lines, start_line - 1),
        receiver: Some(receiver),
        receiver_name,
        receiver_type,
        calls: collect_calls_ast(node, content, lines),
        file_imports: Vec::new(),
    })
}

fn parse_type_symbol(
    node: Node<'_>,
    content: &str,
    lines: &[&str],
    relative_path: &str,
    package: &str,
) -> Option<GoSymbol> {
    let name_node = child_by_kind(node, &["type_identifier", "identifier"])?;
    let name = node_text(name_node, content).to_string();
    let kind = if child_by_kind(node, &["struct_type"]).is_some() {
        GoSymbolKind::Struct
    } else if child_by_kind(node, &["interface_type"]).is_some() {
        GoSymbolKind::Interface
    } else {
        GoSymbolKind::Type
    };
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;
    Some(GoSymbol {
        id: symbol_id(relative_path, &name, start_line, None),
        name,
        kind,
        package: package.to_string(),
        file_path: relative_path.to_string(),
        start_line,
        end_line,
        signature: signature_from_node(node, content),
        docstring: collect_docstring(lines, start_line - 1),
        receiver: None,
        receiver_name: None,
        receiver_type: None,
        calls: Vec::new(),
        file_imports: Vec::new(),
    })
}

fn collect_calls_ast(node: Node<'_>, content: &str, lines: &[&str]) -> Vec<GoCall> {
    let mut calls = Vec::new();
    visit_nodes(node, &mut |item| {
        if item.kind() != "call_expression" {
            return;
        }
        let Some(function_node) = item
            .child_by_field_name("function")
            .or_else(|| item.child(0))
        else {
            return;
        };
        let Some((qualifier, target_text)) = parse_call_target(function_node, content) else {
            return;
        };
        let line = function_node.start_position().row + 1;
        calls.push(GoCall {
            qualifier,
            target_text,
            line,
            snippet: lines
                .get(line.saturating_sub(1))
                .map(|line| line.trim().to_string())
                .unwrap_or_default(),
        });
    });
    calls
}

fn parse_call_target(node: Node<'_>, content: &str) -> Option<(Option<String>, String)> {
    match node.kind() {
        "identifier" | "field_identifier" => Some((None, node_text(node, content).to_string())),
        "selector_expression" => {
            let operand = node
                .child_by_field_name("operand")
                .or_else(|| node.child(0))?;
            let field = node.child_by_field_name("field").or_else(|| {
                let mut cursor = node.walk();
                node.children(&mut cursor)
                    .find(|child| child.kind() == "field_identifier")
            })?;
            Some((
                Some(selector_qualifier_text(operand, content)),
                node_text(field, content).to_string(),
            ))
        }
        _ => None,
    }
}

fn selector_qualifier_text(node: Node<'_>, content: &str) -> String {
    normalize_whitespace(node_text(node, content))
}

fn parse_receiver_parts(
    receiver_node: Node<'_>,
    content: &str,
) -> (Option<String>, Option<String>) {
    let mut receiver_name = None;
    let mut receiver_type = None;
    visit_nodes(receiver_node, &mut |node| match node.kind() {
        "identifier" if receiver_name.is_none() => {
            receiver_name = Some(node_text(node, content).to_string());
        }
        "type_identifier" if receiver_type.is_none() => {
            receiver_type = Some(node_text(node, content).to_string());
        }
        _ => {}
    });
    if receiver_type.is_none() {
        let text = node_text(receiver_node, content);
        let cleaned = text
            .trim_matches(|ch| ch == '(' || ch == ')')
            .replace('*', " ");
        receiver_type = cleaned
            .split_whitespace()
            .last()
            .map(|value| value.to_string());
    }
    (receiver_name, receiver_type)
}

fn signature_from_node(node: Node<'_>, content: &str) -> String {
    let end_byte = child_by_kind(node, &["block"])
        .map(|body| body.start_byte())
        .unwrap_or_else(|| node.end_byte());
    normalize_whitespace(&content[node.start_byte()..end_byte])
        .trim_end_matches('{')
        .trim()
        .to_string()
}

fn child_by_kind<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| kinds.contains(&child.kind()))
}

fn visit_nodes(node: Node<'_>, visitor: &mut impl FnMut(Node<'_>)) {
    visitor(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_nodes(child, visitor);
    }
}

fn node_text<'a>(node: Node<'_>, content: &'a str) -> &'a str {
    node.utf8_text(content.as_bytes()).unwrap_or("")
}

fn unquote_import_path(value: &str) -> String {
    value.trim().trim_matches('"').trim_matches('`').to_string()
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_docstring(lines: &[&str], decl_idx: usize) -> String {
    let mut docs = Vec::new();
    let mut idx = decl_idx;
    while idx > 0 {
        idx -= 1;
        let line = lines[idx].trim();
        if line.is_empty() {
            break;
        }
        if let Some(comment) = line.strip_prefix("//") {
            docs.push(comment.trim().to_string());
        } else {
            break;
        }
    }
    docs.reverse();
    docs.join("\n")
}

fn build_context(
    index_symbols: &[GoSymbol],
    symbol: &GoSymbol,
) -> (Vec<GoCaller>, Vec<GoCallee>, Vec<GoSuggestedRead>) {
    let mut id_to_symbol = std::collections::BTreeMap::new();
    for item in index_symbols {
        id_to_symbol.insert(item.id.clone(), item);
    }
    
    let mut file_infos = std::collections::BTreeMap::new();
    for sym in index_symbols {
        file_infos.entry(sym.file_path.clone()).or_insert_with(|| GoFileInfo {
            file_path: sym.file_path.clone(),
            package: sym.package.clone(),
            imports: sym.file_imports.clone(),
        });
    }
    
    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| GoCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: resolve_call(index_symbols, &file_infos, symbol, call)
                .into_iter()
                .map(|s| s.id.clone())
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
            if let Some(matched_symbol) = id_to_symbol.get(matched_id) {
                suggested_reads.push(GoSuggestedRead {
                    reason: suggestion_reason(symbol, matched_symbol).to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: GoSymbolSummary::from(*matched_symbol),
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
            let matched = resolve_call(index_symbols, &file_infos, item, call)
                .into_iter()
                .any(|m| m.id == symbol.id);
            if matched {
                callers.push(GoCaller {
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
    index_symbols: &'a [GoSymbol],
    file_infos: &std::collections::BTreeMap<String, GoFileInfo>,
    caller: &GoSymbol,
    call: &GoCall,
) -> Vec<&'a GoSymbol> {
    let mut matches = Vec::new();
    if let Some(qualifier) = call.qualifier.as_deref() {
        if caller.receiver_name.as_deref() == Some(qualifier)
            && let Some(receiver_type) = caller.receiver_type.as_deref()
        {
            matches.extend(index_symbols.iter().filter(|symbol| {
                symbol.name == call.target_text
                    && symbol.receiver_type.as_deref() == Some(receiver_type)
                    && symbol.package == caller.package
            }));
        }

        if let Some(file) = file_infos.get(&caller.file_path)
            && let Some(import) = file.imports.iter().find(|import| {
                import.alias.as_deref() == Some(qualifier)
                    || (import.alias.is_none() && import.package_hint == qualifier)
            })
        {
            matches.extend(index_symbols.iter().filter(|symbol| {
                symbol.name == call.target_text
                    && (symbol.package == import.package_hint
                        || package_path_matches(&symbol.file_path, &import.path))
            }));
        }

        dedupe_symbols(matches)
    } else {
        matches.extend(
            index_symbols.iter().filter(|symbol| {
                symbol.name == call.target_text && symbol.package == caller.package
            }),
        );
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
        dedupe_symbols(matches)
    }
}

fn package_path_matches(file_path: &str, import_path: &str) -> bool {
    let package_dir = import_path.rsplit('/').next().unwrap_or(import_path);
    file_path
        .rsplit_once('/')
        .map(|(dir, _)| dir.ends_with(package_dir))
        .unwrap_or(false)
}

fn dedupe_symbols(symbols: Vec<&GoSymbol>) -> Vec<&GoSymbol> {
    let mut seen = BTreeSet::new();
    symbols
        .into_iter()
        .filter(|symbol| seen.insert(symbol.id.clone()))
        .collect()
}

fn suggestion_reason(caller: &GoSymbol, matched: &GoSymbol) -> &'static str {
    if caller.package == matched.package {
        if caller.receiver_type.is_some() && caller.receiver_type == matched.receiver_type {
            "receiver_method_call"
        } else {
            "same_package_call"
        }
    } else {
        "imported_package_call"
    }
}

fn load_all_symbols(root: &std::path::Path) -> Result<Vec<GoSymbol>> {
    let conn = crate::database::init_db(root).map_err(|e| GoIndexError::SymbolNotFound(e.to_string()))?;
    let mut stmt = conn.prepare("SELECT id, name, kind, package_name, file_path, start_line, end_line, signature, docstring, receiver, receiver_name, receiver_type, calls_json, file_imports_json FROM go_symbols WHERE workspace_root = ?").map_err(|e| GoIndexError::SymbolNotFound(e.to_string()))?;
    let symbol_iter = stmt.query_map(rusqlite::params![root.to_string_lossy()], |row| {
        Ok(GoSymbol {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: serde_json::from_str(&format!("\"{}\"", row.get::<_, String>(2)?)).unwrap_or(GoSymbolKind::Function),
            package: row.get(3)?,
            file_path: row.get(4)?,
            start_line: row.get(5)?,
            end_line: row.get(6)?,
            signature: row.get(7)?,
            docstring: row.get(8)?,
            receiver: row.get(9)?,
            receiver_name: row.get(10)?,
            receiver_type: row.get(11)?,
            calls: serde_json::from_str(&row.get::<_, String>(12)?).unwrap_or_default(),
            file_imports: serde_json::from_str(&row.get::<_, String>(13)?).unwrap_or_default(),
        })
    }).map_err(|e| GoIndexError::SymbolNotFound(e.to_string()))?;

    let mut symbols = Vec::new();
    for sym in symbol_iter {
        if let Ok(s) = sym {
            symbols.push(s);
        }
    }
    Ok(symbols)
}

fn load_or_build_or_create(root: &std::path::Path) -> Result<Vec<GoSymbol>> {
    // Bug4: 用元数据判断是否已索引，避免把「空项目」误判为「从未索引」
    let conn = crate::database::init_db(root).unwrap();
    let already_indexed = crate::database::get_index_generated_at(
        &conn,
        &root.to_string_lossy(),
        "go",
    ).is_some();
    let symbols = load_all_symbols(root)?;
    if !already_indexed {
        index_workspace(root)?;
        return load_all_symbols(root);
    }
    Ok(symbols)
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

fn symbol_id(file_path: &str, name: &str, line: usize, receiver: Option<&str>) -> String {
    let receiver = receiver.unwrap_or("").replace([' ', '*', '(', ')'], "");
    if receiver.is_empty() {
        format!("go:{file_path}:{name}:{line}")
    } else {
        format!("go:{file_path}:{receiver}.{name}:{line}")
    }
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

impl From<&GoSymbol> for GoSymbolSummary {
    fn from(symbol: &GoSymbol) -> Self {
        Self {
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            package: symbol.package.clone(),
            file_path: symbol.file_path.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: symbol.signature.clone(),
            docstring: symbol.docstring.clone(),
            receiver: symbol.receiver.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("codex_go_index_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("service")).unwrap();
        path
    }

    #[test]
    fn indexes_go_symbols_docstrings_and_calls() {
        let root = temp_workspace("basic");
        fs::write(
            root.join("service").join("ppt.go"),
            r#"package service

// PptService handles PPT workflow.
type PptService struct{}

// CreatePPT creates a PPT workflow.
func (s *PptService) CreatePPT(topic string) error {
    validateTopic(topic)
    return SaveWorkflow(topic)
}

// validateTopic checks topic text.
func validateTopic(topic string) {}

// SaveWorkflow stores workflow state.
func SaveWorkflow(topic string) error {
    return nil
}
"#,
        )
        .unwrap();

        let response = index_workspace(&root).unwrap();
        assert_eq!(response.files_indexed, 1);
        assert_eq!(response.symbols_indexed, 4);

        let search = search_symbols(
            &root,
            SearchGoSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "workflow".to_string(),
                limit: 10,
            },
        )
        .unwrap();
        assert!(
            search
                .matches
                .iter()
                .any(|symbol| symbol.name == "CreatePPT")
        );

        let create = search
            .matches
            .iter()
            .find(|symbol| symbol.name == "CreatePPT")
            .unwrap();
        assert_eq!(create.kind, GoSymbolKind::Method);
        assert!(create.docstring.contains("creates a PPT workflow"));

        let read = read_symbol(
            &root,
            ReadGoSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: create.id.clone(),
                include_context: true,
            },
        )
        .unwrap();
        assert!(read.content.contains("func (s *PptService) CreatePPT"));
        assert!(
            read.callees
                .iter()
                .any(|callee| callee.target_text == "SaveWorkflow")
        );
        assert!(read.suggested_reads.iter().any(|suggestion| {
            suggestion.reason == "same_package_call" && suggestion.symbol.name == "SaveWorkflow"
        }));
        let save = search
            .matches
            .iter()
            .find(|symbol| symbol.name == "SaveWorkflow")
            .unwrap();
        let save_read = read_symbol(
            &root,
            ReadGoSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: save.id.clone(),
                include_context: true,
            },
        )
        .unwrap();
        assert!(
            save_read
                .callers
                .iter()
                .any(|caller| caller.name == "CreatePPT")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_import_alias_calls() {
        let root = temp_workspace("import_alias");
        fs::create_dir_all(root.join("handler")).unwrap();
        fs::write(
            root.join("service").join("workflow.go"),
            r#"package service

func SaveWorkflow(topic string) error {
    return nil
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("handler").join("handler.go"),
            r#"package handler

import svc "demo/service"

func Run() {
    _ = svc.SaveWorkflow("demo")
}
"#,
        )
        .unwrap();

        index_workspace(&root).unwrap();
        let run = search_symbols(
            &root,
            SearchGoSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "Run".to_string(),
                limit: 5,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "Run")
        .unwrap();
        let read = read_symbol(
            &root,
            ReadGoSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: run.id,
                include_context: true,
            },
        )
        .unwrap();

        assert!(read.callees.iter().any(|callee| {
            callee.target_text == "SaveWorkflow" && !callee.matched_symbol_ids.is_empty()
        }));
        assert!(read.suggested_reads.iter().any(|suggestion| {
            suggestion.reason == "imported_package_call" && suggestion.symbol.name == "SaveWorkflow"
        }));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_receiver_method_calls_and_multiline_signature() {
        let root = temp_workspace("receiver");
        fs::write(
            root.join("service").join("ppt.go"),
            r#"package service

type PptService struct{}

func (s *PptService) Create(
    topic string,
) error {
    return s.Save(topic)
}

func (s *PptService) Save(topic string) error {
    return nil
}
"#,
        )
        .unwrap();

        index_workspace(&root).unwrap();
        let create = search_symbols(
            &root,
            SearchGoSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "Create".to_string(),
                limit: 5,
            },
        )
        .unwrap()
        .matches
        .into_iter()
        .find(|symbol| symbol.name == "Create")
        .unwrap();
        assert!(create.signature.contains("topic string"));

        let read = read_symbol(
            &root,
            ReadGoSymbolRequest {
                workspace_root: root.display().to_string(),
                symbol_id: create.id,
                include_context: true,
            },
        )
        .unwrap();
        assert!(read.callees.iter().any(|callee| {
            callee.target_text == "Save" && !callee.matched_symbol_ids.is_empty()
        }));
        assert!(read.suggested_reads.iter().any(|suggestion| {
            suggestion.reason == "receiver_method_call" && suggestion.symbol.name == "Save"
        }));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_builds_index_when_missing() {
        let root = temp_workspace("auto_build");
        fs::write(
            root.join("service").join("ppt.go"),
            "package service\n\n// AutoBuild proves search can index.\nfunc AutoBuild() {}\n",
        )
        .unwrap();

        let search = search_symbols(
            &root,
            SearchGoSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "AutoBuild".to_string(),
                limit: 5,
            },
        )
        .unwrap();

        assert_eq!(search.matches.len(), 1);
        // Bug2: 已迁移到 SQLite，不再生成 JSON 文件，改为验证元数据表中确实有索引记录
        {
            let conn = crate::database::init_db(&root).unwrap();
            let ts = crate::database::get_index_generated_at(
                &conn,
                &root.to_string_lossy(),
                "go",
            );
            assert!(ts.is_some(), "index metadata should be recorded after auto-build");
        }
        let _ = fs::remove_dir_all(root);
    }
}
