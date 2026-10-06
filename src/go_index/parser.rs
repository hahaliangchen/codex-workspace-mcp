use std::{
    collections::BTreeSet,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

const MAX_GO_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum GoIndexError {
    #[error("go index not found; call index_go_workspace first")]
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

pub type Result<T> = std::result::Result<T, GoIndexError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
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
    pub index: crate::symbol_index_state::IndexHealth,
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct GoIndexStatus {
    pub index: Option<crate::symbol_index_state::IndexHealth>,
    pub index_path: String,
    pub exists: bool,
    pub workspace_root: String,
    pub generated_at_unix: Option<u64>,
    pub files_indexed: Option<usize>,
    pub symbols_indexed: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ListGoSymbolsRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::ListOptions,
    pub workspace_root: String,
    pub file_path: Option<String>,
    pub kind: Option<GoSymbolKind>,
}

#[derive(Debug, Serialize)]
pub struct ListGoSymbolsResponse {
    pub page: crate::symbol_query::PageInfo,
    pub index: crate::symbol_index_state::IndexHealth,
    pub symbols: Vec<GoSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct SearchGoSymbolsRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::SearchOptions,
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_symbol_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchGoSymbolsResponse {
    pub terms: Vec<String>,
    pub match_mode: crate::symbol_query::MatchMode,
    pub page: crate::symbol_query::PageInfo,
    pub index: crate::symbol_index_state::IndexHealth,
    pub query: String,
    pub matches: Vec<GoSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct ReadGoSymbolRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::ReadOptions,
    pub workspace_root: String,
    #[serde(default)]
    pub symbol_id: String,
    #[serde(default)]
    pub include_context: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadGoSymbolResponse {
    pub description: crate::symbol_description::Description,
    pub index: crate::symbol_index_state::IndexHealth,
    pub relationship_accuracy: &'static str,
    pub symbol: GoSymbol,
    pub content: String,
    pub callers: Vec<GoCaller>,
    pub callees: Vec<GoCallee>,
    pub suggested_reads: Vec<GoSuggestedRead>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoSymbolSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<crate::symbol_description::Description>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_terms: Vec<String>,
    pub id: String,
    pub name: String,
    pub kind: GoSymbolKind,
    pub package: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub signature: String,
    #[serde(skip_serializing_if = "String::is_empty")]
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
        index: crate::symbol_index_state::health(root, "go", "go_symbols")?,
        index_path: "SQLite".to_string(),
        files_indexed,
        symbols_indexed,
        generated_at_unix: status(root).generated_at_unix.unwrap_or_else(now_unix),
    })
}

pub fn status(root: &Path) -> GoIndexStatus {
    let Some(conn) = crate::database::init_db(root).ok() else {
        return GoIndexStatus {
            index: None,
            index_path: "SQLite".to_string(),
            exists: false,
            workspace_root: root.display().to_string(),
            generated_at_unix: None,
            files_indexed: None,
            symbols_indexed: None,
        };
    };
    // Bug3: 读取元数据中记录的真实索引创建时间
    let generated_at =
        crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "go");
    if generated_at.is_some() {
        let symbols_indexed: i64 = conn
            .query_row(
                "SELECT count(*) FROM go_symbols WHERE workspace_root = ?",
                rusqlite::params![root.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let files_indexed: i64 = conn
            .query_row(
                "SELECT count(*) FROM symbol_index_files WHERE workspace_root = ? AND language = 'go' AND status = 'indexed'",
                rusqlite::params![root.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        return GoIndexStatus {
            index: crate::symbol_index_state::health(root, "go", "go_symbols").ok(),
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: generated_at,
            files_indexed: Some(files_indexed as usize),
            symbols_indexed: Some(symbols_indexed as usize),
        };
    }
    GoIndexStatus {
        index: crate::symbol_index_state::health(root, "go", "go_symbols").ok(),
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
    let conn = crate::database::init_db(root)?;
    if crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "go").is_none() {
        return Ok(None);
    }
    let (files_indexed,symbols_indexed)=build_index_scope(root,Some(changed_path))?;
    Ok(Some(IndexGoWorkspaceResponse { index:crate::symbol_index_state::health(root,"go","go_symbols")?, index_path:"SQLite".into(), files_indexed,symbols_indexed, generated_at_unix:crate::rust_index::now_unix() }))
}

pub fn list_symbols(root: &Path, request: ListGoSymbolsRequest) -> Result<ListGoSymbolsResponse> {
    ensure_query_index(root,request.file_path.as_deref(),request.options.directory.as_deref())?;
    let kind = request
        .kind
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?
        .and_then(|value| value.as_str().map(str::to_owned));
    let mut selection = crate::symbol_query::Selection {
        file_path: request.file_path.as_deref(),
        directory: request.options.directory.as_deref(),
        kind: kind.as_deref(),
        include_locals: request.options.include_locals,
        include_details: request.options.detailed,
        ..Default::default()
    };
    let total = selection.count(root, "go_symbols")?;
    let page = crate::symbol_query::page(total, request.options.offset, request.options.limit);
    selection.pagination = Some((page.offset, page.limit));
    let symbols = load_selected_symbols(root, &selection)?;
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "go", &symbols.iter().map(|symbol|symbol.file_path.as_str()).collect::<Vec<_>>())?;
    let symbols = symbols
        .iter()
        .map(|symbol| {
            let mut summary = compact_summary(symbol, request.options.detailed);
            summary.description = Some(describe_symbol(&catalog, symbol));
            summary
        })
        .collect();
    Ok(ListGoSymbolsResponse {
        symbols,
        page,
        index: crate::symbol_index_state::health(root, "go", "go_symbols")?,
    })
}

fn describe_symbol(
    catalog: &crate::symbol_description::Catalog,
    symbol: &GoSymbol,
) -> crate::symbol_description::Description {
    let scope = format!(
        "{}.{}",
        symbol.package,
        symbol.receiver_type.as_deref().unwrap_or("")
    )
    .trim_matches('.')
    .to_owned();
    let qualified = crate::symbol_description::qualified_name("go", &scope, &symbol.name);
    catalog.describe(&symbol.id, &symbol.file_path, &qualified)
}

fn compact_summary(symbol: &GoSymbol, detailed: bool) -> GoSymbolSummary {
    let mut summary = GoSymbolSummary::from(symbol);
    if detailed {
        summary.signature = crate::symbol_query::preview(&symbol.signature, 240);
        summary.docstring = crate::symbol_query::preview(&symbol.docstring, 480);
    } else {
        summary.signature.clear();
        summary.docstring.clear();
    }
    summary
}

pub fn search_symbols(
    root: &Path,
    request: SearchGoSymbolsRequest,
) -> Result<SearchGoSymbolsResponse> {
    let terms = crate::symbol_query::terms(&request.query, request.options.match_mode);
    if terms.is_empty() {
        return Err(std::io::Error::other("query must contain at least one search term").into());
    }
    ensure_query_index(root,request.options.file_path.as_deref(),request.options.directory.as_deref())?;
    let selection = crate::symbol_query::Selection {
        file_path: request.options.file_path.as_deref(),
        directory: request.options.directory.as_deref(),
        kind: request.options.kind.as_deref(),
        include_details: true,
        terms: &terms,
        mode: request.options.match_mode,
        include_locals: request.options.include_locals,
        ..Default::default()
    };
    let symbols = load_selected_symbols(root, &selection)?;
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "go", &symbols.iter().map(|symbol|symbol.file_path.as_str()).collect::<Vec<_>>())?;
    let mut ranked: Vec<_> = symbols
        .iter()
        .filter_map(|symbol| {
            let scope = format!(
                "{}.{}",
                symbol.package,
                symbol.receiver_type.as_deref().unwrap_or("")
            )
            .trim_end_matches('.')
            .to_owned();
            let description = describe_symbol(&catalog, symbol);
            crate::symbol_query::score(
                &symbol.name,
                &scope,
                &symbol.signature,
                &symbol.docstring,
                &symbol.file_path,
                &terms,
                request.options.match_mode,
                &description,
            )
            .map(|(score, matched)| (score, symbol, matched, description))
        })
        .collect();
    let page = crate::symbol_query::rank_page(&mut ranked, request.options.offset, request.limit, |(a_score, a, _, _), (b_score, b, _, _)| {
        b_score
            .cmp(a_score)
            .then(a.file_path.cmp(&b.file_path))
            .then(a.start_line.cmp(&b.start_line))
            .then(a.id.cmp(&b.id))
    });
    let matches = ranked
        .iter()
        .skip(page.offset)
        .take(page.limit)
        .map(|(score, symbol, matched, description)| {
            let mut summary = compact_summary(symbol, request.options.detailed);
            summary.description = Some(description.clone());
            summary.score = Some(*score);
            summary.matched_terms = matched.clone();
            summary
        })
        .collect();
    Ok(SearchGoSymbolsResponse {
        terms,
        match_mode: request.options.match_mode,
        query: request.query,
        matches,
        page,
        index: crate::symbol_index_state::health(root, "go", "go_symbols")?,
    })
}

pub fn read_symbol(root: &Path, request: ReadGoSymbolRequest) -> Result<ReadGoSymbolResponse> {
    read_symbol_snapshot(root,&request,true)
}

fn read_symbol_snapshot(root: &Path, request: &ReadGoSymbolRequest, retry:bool) -> Result<ReadGoSymbolResponse> {
    if request.symbol_id.is_empty()
        && (request
            .options
            .file_path
            .as_deref()
            .is_none_or(str::is_empty)
            || request.options.name.as_deref().is_none_or(str::is_empty))
    {
        return Err(std::io::Error::other(
            "provide symbol_id or both file_path and name (qualified name allowed)",
        )
        .into());
    }
    let initialized=crate::database::init_db(root).ok().is_some_and(|conn|crate::database::get_index_generated_at(&conn,&root.to_string_lossy(),"go").is_some());
    if !initialized {build_index(root)?;}
    else if request.symbol_id.is_empty() {
        if let Some(file)=request.options.file_path.as_deref() {
            if crate::source_read::needs_index_refresh(root,"go",file) {maybe_reindex_after_write(root,&root.join(file))?;}
        }
    }
    let by_id = !request.symbol_id.is_empty();
    let selection = crate::symbol_query::Selection {
        symbol_id: by_id.then_some(request.symbol_id.as_str()),
        file_path: if by_id {
            None
        } else {
            request.options.file_path.as_deref()
        },
        name: if by_id {
            None
        } else {
            request.options.name.as_deref()
        },
        include_locals: true,
        include_relationships: true,
        include_details: true,
        ..Default::default()
    };
    let mut candidates = load_selected_symbols(root, &selection)?;
    if !by_id {
        candidates.retain(|symbol| {
            let scope = format!(
                "{}.{}",
                symbol.package,
                symbol.receiver_type.as_deref().unwrap_or("")
            )
            .trim_end_matches('.')
            .to_owned();
            crate::symbol_query::name_matches(
                &symbol.name,
                &scope,
                request.options.name.as_deref().unwrap_or(""),
            )
        });
    }
    if candidates.len() > 1 {
        let choices = candidates
            .iter()
            .take(10)
            .map(|symbol| {
                format!(
                    "{} ({}:{}-{})",
                    symbol.id, symbol.file_path, symbol.start_line, symbol.end_line
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(std::io::Error::other(format!(
            "ambiguous symbol name; use a qualified name or symbol_id: {choices}"
        ))
        .into());
    }
    let symbol = candidates.pop().ok_or_else(|| {
        GoIndexError::SymbolNotFound(if by_id {
            request.symbol_id.clone()
        } else {
            format!(
                "{}:{}",
                request.options.file_path.as_deref().unwrap_or(""),
                request.options.name.as_deref().unwrap_or("")
            )
        })
    })?;
    let snapshot=crate::source_read::read(&root.join(&symbol.file_path),crate::source_read::FILE_LIMIT)?;
    if crate::source_read::indexed_hash(root,"go",&symbol.file_path).as_deref()!=Some(snapshot.hash.as_str()) {
        if !retry {return Err(std::io::Error::other("source_version_conflict: indexed symbol and source versions differ; no source returned").into());}
        maybe_reindex_after_write(root,&root.join(&symbol.file_path))?;
        return read_symbol_snapshot(root,request,false);
    }
    let source_code_hash=snapshot.hash.clone();
    let content = crate::symbol_query::source_range(&snapshot.content, symbol.start_line, symbol.end_line)?;
    let (callers, callees, suggested_reads) = if request.include_context {
        let index_symbols = load_all_symbols(root)?;
        build_context(&index_symbols, &symbol)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "go", &[symbol.file_path.as_str()])?;
    let mut description = describe_symbol(&catalog, &symbol);
    if description.code_hash != source_code_hash && description.status == "current" {
        description.status = "stale";
    }
    description.code_hash = source_code_hash;
    Ok(ReadGoSymbolResponse {
        description,
        symbol,
        content,
        callers,
        callees,
        suggested_reads,
        index: crate::symbol_index_state::health(root, "go", "go_symbols")?,
        relationship_accuracy: "heuristic_not_type_checked",
    })
}

fn build_index(root: &Path) -> Result<(usize, usize)> {
    build_index_scope(root,None)
}

fn build_index_scope(root: &Path, changed_path: Option<&Path>) -> Result<(usize, usize)> {
    build_index_scoped(root,changed_path,false)
}

fn build_index_scoped(root: &Path, changed_path: Option<&Path>, directory: bool) -> Result<(usize, usize)> {
    let refresh_lock = crate::symbol_index_state::lock(root, "go");
    let _guard = refresh_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut conn = crate::database::init_db(root)?;
    let plan = match changed_path { Some(path) if directory=>crate::symbol_index_state::RefreshPlan::for_directory(root,"go",&conn,path)?, Some(path)=>crate::symbol_index_state::RefreshPlan::for_file(root,"go",&conn,path)?, None=>crate::symbol_index_state::RefreshPlan::new(root,"go",&conn)? };
    if !plan.needs_update() {
        plan.record_check(&conn, root, "go")?;
        return Ok(crate::symbol_index_state::counts(
            &conn,
            root,
            "go",
            "go_symbols",
        )?);
    }
    let tx = conn.transaction()?;
    plan.prepare(&tx, root, "go", "go_symbols")?;
    for relative in &plan.changed {
        let path_buf = root.join(relative);
        let path = path_buf.as_path();
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > MAX_GO_FILE_BYTES {
            plan.record(
                &tx,
                root,
                "go",
                path,
                "skipped_size",
                "source exceeds the 2 MiB indexing limit",
            )?;
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                plan.record(
                    &tx,
                    root,
                    "go",
                    path,
                    "parse_or_read_error",
                    &error.to_string(),
                )?;
                continue;
            }
        };
        plan.record_content(&tx, root, "go", path, &content)?;
        let parsed = parse_go_file(root, path, &content);
        plan.record(
            &tx,
            root,
            "go",
            path,
            if parsed.parse_error.is_some() {
                "parse_error_or_partial"
            } else {
                "indexed"
            },
            parsed.parse_error.as_deref().unwrap_or(""),
        )?;

        for sym in parsed.symbols {
            let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();
            let file_imports_json = serde_json::to_string(&parsed.file.imports).unwrap_or_default();
            let kind = serde_json::to_string(&sym.kind)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
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
            )?;
        }
    }
    plan.finish(&tx, root, "go")?;
    let ts = now_unix();
    crate::database::upsert_index_metadata(&tx, &root.to_string_lossy(), "go", ts)?;
    tx.commit()?;
    Ok(crate::symbol_index_state::counts(
        &conn,
        root,
        "go",
        "go_symbols",
    )?)
}

struct ParsedGoFile {
    parse_error: Option<String>,
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
            parse_error: Some("Go parser could not produce a syntax tree".to_owned()),
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
            parse_error: Some("Go parser could not produce a syntax tree".to_owned()),
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
        parse_error: root_node
            .has_error()
            .then(|| "Go syntax tree contains errors; symbols may be partial".to_owned()),
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
        file_infos
            .entry(sym.file_path.clone())
            .or_insert_with(|| GoFileInfo {
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

pub(crate) fn load_all_symbols(root: &std::path::Path) -> Result<Vec<GoSymbol>> {
    load_selected_symbols(
        root,
        &crate::symbol_query::Selection {
            include_locals: true,
            include_relationships: true,
            include_details: true,
            ..Default::default()
        },
    )
}

pub(crate) fn load_selected_symbols(
    root: &std::path::Path,
    selection: &crate::symbol_query::Selection<'_>,
) -> Result<Vec<GoSymbol>> {
    let conn =
        crate::database::init_db(root).map_err(|e| GoIndexError::SymbolNotFound(e.to_string()))?;
    let (mut where_sql, mut params) = selection.sql(
        root,
        "go_symbols",
        &[
            "name",
            "package_name",
            "receiver_type",
            "signature",
            "docstring",
            "file_path",
        ],
    );
    let mut select = "SELECT id, name, kind, package_name, file_path, start_line, end_line, signature, docstring, receiver, receiver_name, receiver_type, calls_json, file_imports_json FROM go_symbols".to_owned();
    if !selection.include_relationships {
        for column in &["calls_json", "file_imports_json"] {
            select = select.replace(column, &format!("'[]' AS {column}"));
        }
    }
    if !selection.include_details {
        select = select
            .replace(", signature,", ", '' AS signature,")
            .replace(", docstring,", ", '' AS docstring,");
    }
    if let Some((offset, limit)) = selection.pagination {
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(
            i64::try_from(offset).unwrap_or(i64::MAX),
        ));
        where_sql.push_str(&format!(
            " LIMIT ?{} OFFSET ?{}",
            params.len() - 1,
            params.len()
        ));
    }
    let mut stmt = conn
        .prepare(&(select + where_sql.as_str()))
        .map_err(|e| GoIndexError::SymbolNotFound(e.to_string()))?;
    let symbol_iter = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(GoSymbol {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: serde_json::from_str(&format!("\"{}\"", row.get::<_, String>(2)?))
                    .unwrap_or(GoSymbolKind::Function),
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
        })
        .map_err(|e| GoIndexError::SymbolNotFound(e.to_string()))?;

    Ok(symbol_iter.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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
            description: None,
            score: None,
            matched_terms: Vec::new(),
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            package: symbol.package.clone(),
            file_path: symbol.file_path.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: crate::symbol_query::preview(&symbol.signature, 160),
            docstring: crate::symbol_query::preview(&symbol.docstring, 240),
            receiver: symbol.receiver.clone(),
        }
    }
}

pub(crate) fn ensure_query_index(root: &Path, file: Option<&str>, directory: Option<&str>) -> Result<()> {
    crate::symbol_index_state::ensure_query_index(root,"go",file,directory,|path,is_directory|build_index_scoped(root,path,is_directory).map(|_|()))
}
