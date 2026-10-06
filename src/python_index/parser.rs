use std::{collections::BTreeSet, path::Path};

use rustpython_parser::{
    Parse,
    ast::{self, Constant, Expr, ExprCall, Stmt},
    text_size::TextSize,
};
use serde::{Deserialize, Serialize};

const MAX_PYTHON_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PythonIndexError {
    #[error("python index not found; call index_python_workspace first")]
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

pub type Result<T> = std::result::Result<T, PythonIndexError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
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
    #[serde(default)]
    pub file_imports: Vec<PythonImport>,
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
    pub index: crate::symbol_index_state::IndexHealth,
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct PythonIndexStatus {
    pub index: Option<crate::symbol_index_state::IndexHealth>,
    pub index_path: String,
    pub exists: bool,
    pub workspace_root: String,
    pub generated_at_unix: Option<u64>,
    pub files_indexed: Option<usize>,
    pub symbols_indexed: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ListPythonSymbolsRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::ListOptions,
    pub workspace_root: String,
    pub file_path: Option<String>,
    pub kind: Option<PythonSymbolKind>,
}

#[derive(Debug, Serialize)]
pub struct ListPythonSymbolsResponse {
    pub page: crate::symbol_query::PageInfo,
    pub index: crate::symbol_index_state::IndexHealth,
    pub symbols: Vec<PythonSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct SearchPythonSymbolsRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::SearchOptions,
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_symbol_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchPythonSymbolsResponse {
    pub terms: Vec<String>,
    pub match_mode: crate::symbol_query::MatchMode,
    pub page: crate::symbol_query::PageInfo,
    pub index: crate::symbol_index_state::IndexHealth,
    pub query: String,
    pub matches: Vec<PythonSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct ReadPythonSymbolRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::ReadOptions,
    pub workspace_root: String,
    #[serde(default)]
    pub symbol_id: String,
    #[serde(default)]
    pub include_context: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadPythonSymbolResponse {
    pub description: crate::symbol_description::Description,
    pub index: crate::symbol_index_state::IndexHealth,
    pub relationship_accuracy: &'static str,
    pub symbol: PythonSymbol,
    pub content: String,
    pub callers: Vec<PythonCaller>,
    pub callees: Vec<PythonCallee>,
    pub suggested_reads: Vec<PythonSuggestedRead>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PythonSymbolSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<crate::symbol_description::Description>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_terms: Vec<String>,
    pub id: String,
    pub name: String,
    pub kind: PythonSymbolKind,
    pub file_path: String,
    pub class_name: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub signature: String,
    #[serde(skip_serializing_if = "String::is_empty")]
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
    let (files_indexed, symbols_indexed) = build_index(root)?;
    Ok(IndexPythonWorkspaceResponse {
        index: crate::symbol_index_state::health(root, "python", "python_symbols")?,
        index_path: "SQLite".to_string(),
        files_indexed,
        symbols_indexed,
        generated_at_unix: status(root)
            .generated_at_unix
            .unwrap_or_else(crate::rust_index::now_unix),
    })
}

pub fn status(root: &Path) -> PythonIndexStatus {
    let Some(conn) = crate::database::init_db(root).ok() else {
        return PythonIndexStatus {
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
        crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "python");
    if generated_at.is_some() {
        let symbols_indexed: i64 = conn
            .query_row(
                "SELECT count(*) FROM python_symbols WHERE workspace_root = ?",
                rusqlite::params![root.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let files_indexed: i64 = conn
            .query_row(
                "SELECT count(*) FROM symbol_index_files WHERE workspace_root = ? AND language = 'python' AND status = 'indexed'",
                rusqlite::params![root.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        return PythonIndexStatus {
            index: crate::symbol_index_state::health(root, "python", "python_symbols").ok(),
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: generated_at,
            files_indexed: Some(files_indexed as usize),
            symbols_indexed: Some(symbols_indexed as usize),
        };
    }
    PythonIndexStatus {
        index: crate::symbol_index_state::health(root, "python", "python_symbols").ok(),
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
) -> Result<Option<IndexPythonWorkspaceResponse>> {
    if changed_path.extension().and_then(|value| value.to_str()) != Some("py") {
        return Ok(None);
    }
    let conn = crate::database::init_db(root)?;
    if crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "python").is_none() {
        return Ok(None);
    }
    let (files_indexed,symbols_indexed)=build_index_scope(root,Some(changed_path))?;
    Ok(Some(IndexPythonWorkspaceResponse { index:crate::symbol_index_state::health(root,"python","python_symbols")?, index_path:"SQLite".into(), files_indexed,symbols_indexed, generated_at_unix:crate::rust_index::now_unix() }))
}

pub fn list_symbols(
    root: &Path,
    request: ListPythonSymbolsRequest,
) -> Result<ListPythonSymbolsResponse> {
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
    let total = selection.count(root, "python_symbols")?;
    let page = crate::symbol_query::page(total, request.options.offset, request.options.limit);
    selection.pagination = Some((page.offset, page.limit));
    let symbols = load_selected_symbols(root, &selection)?;
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "python", &symbols.iter().map(|symbol|symbol.file_path.as_str()).collect::<Vec<_>>())?;
    let symbols = symbols
        .iter()
        .map(|symbol| {
            let mut summary = compact_summary(symbol, request.options.detailed);
            summary.description = Some(describe_symbol(&catalog, symbol));
            summary
        })
        .collect();
    Ok(ListPythonSymbolsResponse {
        symbols,
        page,
        index: crate::symbol_index_state::health(root, "python", "python_symbols")?,
    })
}

fn describe_symbol(
    catalog: &crate::symbol_description::Catalog,
    symbol: &PythonSymbol,
) -> crate::symbol_description::Description {
    let scope = symbol.class_name.clone().unwrap_or_default();
    let qualified = crate::symbol_description::qualified_name("python", &scope, &symbol.name);
    catalog.describe(&symbol.id, &symbol.file_path, &qualified)
}

fn compact_summary(symbol: &PythonSymbol, detailed: bool) -> PythonSymbolSummary {
    let mut summary = PythonSymbolSummary::from(symbol);
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
    request: SearchPythonSymbolsRequest,
) -> Result<SearchPythonSymbolsResponse> {
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
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "python", &symbols.iter().map(|symbol|symbol.file_path.as_str()).collect::<Vec<_>>())?;
    let mut ranked: Vec<_> = symbols
        .iter()
        .filter_map(|symbol| {
            let scope = symbol.class_name.clone().unwrap_or_default();
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
    Ok(SearchPythonSymbolsResponse {
        terms,
        match_mode: request.options.match_mode,
        query: request.query,
        matches,
        page,
        index: crate::symbol_index_state::health(root, "python", "python_symbols")?,
    })
}

pub fn read_symbol(root: &Path, request: ReadPythonSymbolRequest) -> Result<ReadPythonSymbolResponse> {
    read_symbol_snapshot(root,&request,true)
}

fn read_symbol_snapshot(root: &Path, request: &ReadPythonSymbolRequest, retry:bool) -> Result<ReadPythonSymbolResponse> {
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
    let initialized=crate::database::init_db(root).ok().is_some_and(|conn|crate::database::get_index_generated_at(&conn,&root.to_string_lossy(),"python").is_some());
    if !initialized {build_index(root)?;}
    else if request.symbol_id.is_empty() {
        if let Some(file)=request.options.file_path.as_deref() {
            if crate::source_read::needs_index_refresh(root,"python",file) {maybe_reindex_after_write(root,&root.join(file))?;}
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
            let scope = symbol.class_name.clone().unwrap_or_default();
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
        PythonIndexError::SymbolNotFound(if by_id {
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
    if crate::source_read::indexed_hash(root,"python",&symbol.file_path).as_deref()!=Some(snapshot.hash.as_str()) {
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
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "python", &[symbol.file_path.as_str()])?;
    let mut description = describe_symbol(&catalog, &symbol);
    if description.code_hash != source_code_hash && description.status == "current" {
        description.status = "stale";
    }
    description.code_hash = source_code_hash;
    Ok(ReadPythonSymbolResponse {
        description,
        symbol,
        content,
        callers,
        callees,
        suggested_reads,
        index: crate::symbol_index_state::health(root, "python", "python_symbols")?,
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
    let refresh_lock = crate::symbol_index_state::lock(root, "python");
    let _guard = refresh_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut conn = crate::database::init_db(root)?;
    let plan = match changed_path { Some(path) if directory=>crate::symbol_index_state::RefreshPlan::for_directory(root,"python",&conn,path)?, Some(path)=>crate::symbol_index_state::RefreshPlan::for_file(root,"python",&conn,path)?, None=>crate::symbol_index_state::RefreshPlan::new(root,"python",&conn)? };
    if !plan.needs_update() {
        plan.record_check(&conn, root, "python")?;
        return Ok(crate::symbol_index_state::counts(
            &conn,
            root,
            "python",
            "python_symbols",
        )?);
    }
    let tx = conn.transaction()?;
    plan.prepare(&tx, root, "python", "python_symbols")?;
    for relative in &plan.changed {
        let path_buf = root.join(relative);
        let path = path_buf.as_path();
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > MAX_PYTHON_FILE_BYTES {
            plan.record(
                &tx,
                root,
                "python",
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
                    "python",
                    path,
                    "parse_or_read_error",
                    &error.to_string(),
                )?;
                continue;
            }
        };

        plan.record_content(&tx, root, "python", path, &content)?;
        let ast = match ast::Suite::parse(&content, "<embedded>") {
            Ok(ast) => ast,
            Err(error) => {
                plan.record(
                    &tx,
                    root,
                    "python",
                    path,
                    "parse_or_read_error",
                    &error.to_string(),
                )?;
                continue;
            }
        };
        let parsed = parse_python_file(root, path, &content, &ast);
        plan.record(&tx, root, "python", path, "indexed", "")?;

        for sym in parsed.symbols {
            let kind = serde_json::to_string(&sym.kind)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            let decorators_json = serde_json::to_string(&sym.decorators).unwrap_or_default();
            let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();
            let file_imports_json = serde_json::to_string(&parsed.file.imports).unwrap_or_default();

            tx.execute(
                "INSERT INTO python_symbols (
                    id, workspace_root, name, kind, file_path, class_name, start_line, end_line,
                    signature, docstring, decorators_json, calls_json, file_imports_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    sym.id,
                    root.to_string_lossy(),
                    sym.name,
                    kind,
                    sym.file_path,
                    sym.class_name,
                    sym.start_line,
                    sym.end_line,
                    sym.signature,
                    sym.docstring,
                    decorators_json,
                    calls_json,
                    file_imports_json
                ],
            )?;
        }
    }
    plan.finish(&tx, root, "python")?;
    let ts = crate::rust_index::now_unix();
    crate::database::upsert_index_metadata(&tx, &root.to_string_lossy(), "python", ts)?;
    tx.commit()?;
    Ok(crate::symbol_index_state::counts(
        &conn,
        root,
        "python",
        "python_symbols",
    )?)
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

    collect_stmts(
        stmts,
        &file_path,
        &lines,
        &line_map,
        None,
        &mut imports,
        &mut symbols,
    );

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
                let module = node
                    .module
                    .as_ref()
                    .map(|m| m.to_string())
                    .unwrap_or_default();
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
                let decorators: Vec<String> =
                    node.decorator_list.iter().map(|d| expr_text(d)).collect();
                let kind = if class_name.is_some() {
                    PythonSymbolKind::Method
                } else {
                    PythonSymbolKind::Function
                };
                let signature = build_function_signature(&name, &node.args, lines, start_line);
                let docstring = extract_docstring(&node.body);
                let calls = collect_calls(&node.body, lines, line_map);
                symbols.push(PythonSymbol {
                    id: format!("{file_path}:{name}"),
                    name,
                    kind,
                    file_path: file_path.to_string(),
                    class_name: class_name.map(String::from),
                    start_line,
                    end_line,
                    signature,
                    docstring,
                    decorators,
                    calls,
                    file_imports: Vec::new(),
                });
            }
            Stmt::ClassDef(cls) => {
                let name = cls.name.to_string();
                let decorators: Vec<String> =
                    cls.decorator_list.iter().map(|d| expr_text(d)).collect();
                let bases: Vec<String> = cls.bases.iter().map(|b| expr_text(b)).collect();
                let signature = if bases.is_empty() {
                    format!("class {name}")
                } else {
                    format!("class {name}({})", bases.join(", "))
                };
                let docstring = extract_docstring(&cls.body);
                symbols.push(PythonSymbol {
                    id: format!("{file_path}:{name}"),
                    name: name.clone(),
                    kind: PythonSymbolKind::Class,
                    file_path: file_path.to_string(),
                    class_name: None,
                    start_line: line_map.line(cls.range.start()),
                    end_line: line_map.line(cls.range.end()),
                    signature,
                    docstring,
                    decorators,
                    calls: Vec::new(),
                    file_imports: Vec::new(),
                });
                collect_stmts(
                    &cls.body,
                    file_path,
                    lines,
                    line_map,
                    Some(&name),
                    imports,
                    symbols,
                );
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

fn collect_calls_from_stmts(
    stmts: &[Stmt],
    lines: &[&str],
    line_map: &LineMap,
    calls: &mut Vec<PythonCall>,
) {
    for stmt in stmts {
        collect_calls_from_stmt(stmt, lines, line_map, calls);
    }
}

fn collect_calls_from_stmt(
    stmt: &Stmt,
    lines: &[&str],
    line_map: &LineMap,
    calls: &mut Vec<PythonCall>,
) {
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

fn collect_calls_from_expr(
    expr: &Expr,
    lines: &[&str],
    line_map: &LineMap,
    calls: &mut Vec<PythonCall>,
) {
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
    index_symbols: &[PythonSymbol],
    symbol: &PythonSymbol,
) -> (
    Vec<PythonCaller>,
    Vec<PythonCallee>,
    Vec<PythonSuggestedRead>,
) {
    let mut id_to_symbol = std::collections::BTreeMap::new();
    for item in index_symbols {
        id_to_symbol.insert(item.id.clone(), item);
    }

    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| PythonCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: resolve_call(index_symbols, symbol, call)
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
                suggested_reads.push(PythonSuggestedRead {
                    reason: suggestion_reason(symbol, matched_symbol).to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: PythonSymbolSummary::from(*matched_symbol),
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
                .any(|m| m.id == symbol.id);
            if matched {
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
    index_symbols: &'a [PythonSymbol],
    caller: &PythonSymbol,
    call: &PythonCall,
) -> Vec<&'a PythonSymbol> {
    let mut matches = Vec::new();

    if let Some(qualifier) = call.qualifier.as_deref() {
        // self.method() — look for methods on same class
        if qualifier == "self" || qualifier == "cls" {
            if let Some(class_name) = caller.class_name.as_deref() {
                matches.extend(index_symbols.iter().filter(|s| {
                    s.name == call.target_text
                        && s.class_name.as_deref() == Some(class_name)
                        && s.file_path == caller.file_path
                }));
            }
        }
        // Qualified.name() — qualifier tail matches class name
        let qualifier_tail = qualifier.rsplit('.').next().unwrap_or(qualifier);
        matches.extend(index_symbols.iter().filter(|s| {
            s.name == call.target_text && s.class_name.as_deref() == Some(qualifier_tail)
        }));
    } else {
        // Bare call — same file first, then workspace-wide
        matches.extend(index_symbols.iter().filter(|s| {
            s.name == call.target_text
                && s.file_path == caller.file_path
                && matches!(s.kind, PythonSymbolKind::Function | PythonSymbolKind::Class)
        }));
        if matches.is_empty() {
            matches.extend(index_symbols.iter().filter(|s| {
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

pub(crate) fn load_all_symbols(root: &std::path::Path) -> Result<Vec<PythonSymbol>> {
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
) -> Result<Vec<PythonSymbol>> {
    let conn = crate::database::init_db(root)
        .map_err(|e| PythonIndexError::SymbolNotFound(e.to_string()))?;
    let (mut where_sql, mut params) = selection.sql(
        root,
        "python_symbols",
        &["name", "class_name", "signature", "docstring", "file_path"],
    );
    let mut select = "SELECT id, name, kind, file_path, class_name, start_line, end_line, signature, docstring, decorators_json, calls_json, file_imports_json FROM python_symbols".to_owned();
    if !selection.include_relationships {
        for column in &["decorators_json", "calls_json", "file_imports_json"] {
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
        .map_err(|e| PythonIndexError::SymbolNotFound(e.to_string()))?;
    let symbol_iter = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(PythonSymbol {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: serde_json::from_str(&format!("\"{}\"", row.get::<_, String>(2)?))
                    .unwrap_or(PythonSymbolKind::Function),
                file_path: row.get(3)?,
                class_name: row.get(4)?,
                start_line: row.get(5)?,
                end_line: row.get(6)?,
                signature: row.get(7)?,
                docstring: row.get(8)?,
                decorators: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default(),
                calls: serde_json::from_str(&row.get::<_, String>(10)?).unwrap_or_default(),
                file_imports: serde_json::from_str(&row.get::<_, String>(11)?).unwrap_or_default(),
            })
        })
        .map_err(|e| PythonIndexError::SymbolNotFound(e.to_string()))?;

    Ok(symbol_iter.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn line_snippet(lines: &[&str], line: usize) -> String {
    lines
        .get(line.saturating_sub(1))
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

fn default_symbol_limit() -> usize {
    20
}

impl From<&PythonSymbol> for PythonSymbolSummary {
    fn from(s: &PythonSymbol) -> Self {
        Self {
            description: None,
            score: None,
            matched_terms: Vec::new(),
            id: s.id.clone(),
            name: s.name.clone(),
            kind: s.kind.clone(),
            file_path: s.file_path.clone(),
            class_name: s.class_name.clone(),
            start_line: s.start_line,
            end_line: s.end_line,
            signature: crate::symbol_query::preview(&s.signature, 160),
            docstring: crate::symbol_query::preview(&s.docstring, 240),
            decorators: s.decorators.clone(),
        }
    }
}

pub(crate) fn ensure_query_index(root: &Path, file: Option<&str>, directory: Option<&str>) -> Result<()> {
    crate::symbol_index_state::ensure_query_index(root,"python",file,directory,|path,is_directory|build_index_scoped(root,path,is_directory).map(|_|()))
}
