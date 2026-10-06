use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
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
    pub index: crate::symbol_index_state::IndexHealth,
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct RustIndexStatus {
    pub index: Option<crate::symbol_index_state::IndexHealth>,
    pub index_path: String,
    pub exists: bool,
    pub workspace_root: String,
    pub generated_at_unix: Option<u64>,
    pub files_indexed: Option<usize>,
    pub symbols_indexed: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ListRustSymbolsRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::ListOptions,
    pub workspace_root: String,
    pub file_path: Option<String>,
    pub kind: Option<RustSymbolKind>,
}

#[derive(Debug, Serialize)]
pub struct ListRustSymbolsResponse {
    pub page: crate::symbol_query::PageInfo,
    pub index: crate::symbol_index_state::IndexHealth,
    pub symbols: Vec<RustSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct SearchRustSymbolsRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::SearchOptions,
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_symbol_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchRustSymbolsResponse {
    pub terms: Vec<String>,
    pub match_mode: crate::symbol_query::MatchMode,
    pub page: crate::symbol_query::PageInfo,
    pub index: crate::symbol_index_state::IndexHealth,
    pub query: String,
    pub matches: Vec<RustSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct ReadRustSymbolRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::ReadOptions,
    pub workspace_root: String,
    #[serde(default)]
    pub symbol_id: String,
    #[serde(default)]
    pub include_context: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadRustSymbolResponse {
    pub description: crate::symbol_description::Description,
    pub index: crate::symbol_index_state::IndexHealth,
    pub relationship_accuracy: &'static str,
    pub symbol: RustSymbol,
    pub content: String,
    pub callers: Vec<RustCaller>,
    pub callees: Vec<RustCallee>,
    pub suggested_reads: Vec<RustSuggestedRead>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RustSymbolSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<crate::symbol_description::Description>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_terms: Vec<String>,
    pub id: String,
    pub name: String,
    pub kind: RustSymbolKind,
    pub file_path: String,
    pub module_path: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub signature: String,
    #[serde(skip_serializing_if = "String::is_empty")]
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
        index: crate::symbol_index_state::health(root, "rust", "rust_symbols")?,
        index_path: "SQLite".to_string(),
        files_indexed,
        symbols_indexed,
        generated_at_unix: status(root)
            .generated_at_unix
            .unwrap_or_else(crate::rust_index::now_unix),
    })
}

pub fn status(root: &Path) -> RustIndexStatus {
    let Some(conn) = crate::database::init_db(root).ok() else {
        return RustIndexStatus {
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
                "SELECT count(*) FROM symbol_index_files WHERE workspace_root = ? AND language = 'rust' AND status = 'indexed'",
                rusqlite::params![root.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        return RustIndexStatus {
            index: crate::symbol_index_state::health(root, "rust", "rust_symbols").ok(),
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: generated_at,
            files_indexed: Some(files_indexed as usize),
            symbols_indexed: Some(symbols_indexed as usize),
        };
    }
    RustIndexStatus {
        index: crate::symbol_index_state::health(root, "rust", "rust_symbols").ok(),
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
    let conn = crate::database::init_db(root)?;
    if crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "rust").is_none() {
        return Ok(None);
    }
    let (files_indexed,symbols_indexed)=build_index_scope(root,Some(changed_path))?;
    Ok(Some(IndexRustWorkspaceResponse { index:crate::symbol_index_state::health(root,"rust","rust_symbols")?, index_path:"SQLite".into(), files_indexed,symbols_indexed, generated_at_unix:crate::rust_index::now_unix() }))
}

pub fn list_symbols(
    root: &Path,
    request: ListRustSymbolsRequest,
) -> Result<ListRustSymbolsResponse> {
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
    let total = selection.count(root, "rust_symbols")?;
    let page = crate::symbol_query::page(total, request.options.offset, request.options.limit);
    selection.pagination = Some((page.offset, page.limit));
    let symbols = load_selected_symbols(root, &selection)?;
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "rust", &symbols.iter().map(|symbol|symbol.file_path.as_str()).collect::<Vec<_>>())?;
    let symbols = symbols
        .iter()
        .map(|symbol| {
            let mut summary = compact_summary(symbol, request.options.detailed);
            summary.description = Some(describe_symbol(&catalog, symbol));
            summary
        })
        .collect();
    Ok(ListRustSymbolsResponse {
        symbols,
        page,
        index: crate::symbol_index_state::health(root, "rust", "rust_symbols")?,
    })
}

fn describe_symbol(
    catalog: &crate::symbol_description::Catalog,
    symbol: &RustSymbol,
) -> crate::symbol_description::Description {
    let scope = format!(
        "{}::{}",
        symbol.module_path,
        symbol.impl_type.as_deref().unwrap_or("")
    )
    .trim_matches(':')
    .to_owned();
    let qualified = crate::symbol_description::qualified_name("rust", &scope, &symbol.name);
    catalog.describe(&symbol.id, &symbol.file_path, &qualified)
}

fn compact_summary(symbol: &RustSymbol, detailed: bool) -> RustSymbolSummary {
    let mut summary = RustSymbolSummary::from(symbol);
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
    request: SearchRustSymbolsRequest,
) -> Result<SearchRustSymbolsResponse> {
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
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "rust", &symbols.iter().map(|symbol|symbol.file_path.as_str()).collect::<Vec<_>>())?;
    let mut ranked: Vec<_> = symbols
        .iter()
        .filter_map(|symbol| {
            let scope = format!(
                "{}::{}",
                symbol.module_path,
                symbol.impl_type.as_deref().unwrap_or("")
            )
            .trim_end_matches("::")
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
    Ok(SearchRustSymbolsResponse {
        terms,
        match_mode: request.options.match_mode,
        query: request.query,
        matches,
        page,
        index: crate::symbol_index_state::health(root, "rust", "rust_symbols")?,
    })
}

pub fn read_symbol(root: &Path, request: ReadRustSymbolRequest) -> Result<ReadRustSymbolResponse> {
    read_symbol_snapshot(root,&request,true)
}

fn read_symbol_snapshot(root: &Path, request: &ReadRustSymbolRequest, retry:bool) -> Result<ReadRustSymbolResponse> {
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
    let initialized=crate::database::init_db(root).ok().is_some_and(|conn|crate::database::get_index_generated_at(&conn,&root.to_string_lossy(),"rust").is_some());
    if !initialized {build_index(root)?;}
    else if request.symbol_id.is_empty() {
        if let Some(file)=request.options.file_path.as_deref() {
            if crate::source_read::needs_index_refresh(root,"rust",file) {maybe_reindex_after_write(root,&root.join(file))?;}
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
                "{}::{}",
                symbol.module_path,
                symbol.impl_type.as_deref().unwrap_or("")
            )
            .trim_end_matches("::")
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
        RustIndexError::SymbolNotFound(if by_id {
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
    if crate::source_read::indexed_hash(root,"rust",&symbol.file_path).as_deref()!=Some(snapshot.hash.as_str()) {
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
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "rust", &[symbol.file_path.as_str()])?;
    let mut description = describe_symbol(&catalog, &symbol);
    if description.code_hash != source_code_hash && description.status == "current" {
        description.status = "stale";
    }
    description.code_hash = source_code_hash;
    Ok(ReadRustSymbolResponse {
        description,
        symbol,
        content,
        callers,
        callees,
        suggested_reads,
        index: crate::symbol_index_state::health(root, "rust", "rust_symbols")?,
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
    let refresh_lock = crate::symbol_index_state::lock(root, "rust");
    let _guard = refresh_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut conn = crate::database::init_db(root)?;
    let plan = match changed_path { Some(path) if directory=>crate::symbol_index_state::RefreshPlan::for_directory(root,"rust",&conn,path)?, Some(path)=>crate::symbol_index_state::RefreshPlan::for_file(root,"rust",&conn,path)?, None=>crate::symbol_index_state::RefreshPlan::new(root,"rust",&conn)? };
    if !plan.needs_update() {
        plan.record_check(&conn, root, "rust")?;
        return Ok(crate::symbol_index_state::counts(
            &conn,
            root,
            "rust",
            "rust_symbols",
        )?);
    }
    let tx = conn.transaction()?;
    plan.prepare(&tx, root, "rust", "rust_symbols")?;
    let mut id_counts = HashMap::<String, usize>::new();
    for relative in &plan.changed {
        let path_buf = root.join(relative);
        let path = path_buf.as_path();
        let metadata = fs::metadata(path)?;
        if metadata.len() > MAX_RUST_FILE_BYTES {
            plan.record(
                &tx,
                root,
                "rust",
                path,
                "skipped_size",
                "source exceeds the 2 MiB indexing limit",
            )?;
            continue;
        }
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                plan.record(
                    &tx,
                    root,
                    "rust",
                    path,
                    "parse_or_read_error",
                    &error.to_string(),
                )?;
                continue;
            }
        };
        plan.record_content(&tx, root, "rust", path, &content)?;
        let parsed = match syn::parse_file(&content) {
            Ok(parsed) => parsed,
            Err(error) => {
                plan.record(
                    &tx,
                    root,
                    "rust",
                    path,
                    "parse_or_read_error",
                    &error.to_string(),
                )?;
                continue;
            }
        };

        let parsed_file = parse_rust_file(root, path, &content, &parsed);
        plan.record(&tx, root, "rust", path, "indexed", "")?;
        for mut sym in parsed_file.symbols {
            let count = id_counts.entry(sym.id.clone()).or_default();
            *count += 1;
            if *count > 1 {
                sym.id = format!("{}#{}", sym.id, count);
            }
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
            )?;
        }
    }
    plan.finish(&tx, root, "rust")?;
    let ts = now_unix();
    crate::database::upsert_index_metadata(&tx, &root.to_string_lossy(), "rust", ts)?;
    tx.commit()?;
    Ok(crate::symbol_index_state::counts(
        &conn,
        root,
        "rust",
        "rust_symbols",
    )?)
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

pub(crate) fn load_all_symbols(root: &std::path::Path) -> Result<Vec<RustSymbol>> {
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
) -> Result<Vec<RustSymbol>> {
    let conn = crate::database::init_db(root)
        .map_err(|e| RustIndexError::SymbolNotFound(e.to_string()))?;
    let (mut where_sql, mut params) = selection.sql(
        root,
        "rust_symbols",
        &[
            "name",
            "module_path",
            "impl_type",
            "signature",
            "docstring",
            "file_path",
        ],
    );
    let mut select = "SELECT id, name, kind, file_path, module_path, start_line, end_line, signature, docstring, visibility, impl_type, trait_name, calls_json FROM rust_symbols".to_owned();
    if !selection.include_relationships {
        for column in &["calls_json"] {
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
        .map_err(|e| RustIndexError::SymbolNotFound(e.to_string()))?;
    let symbol_iter = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
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

    Ok(symbol_iter.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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
            description: None,
            score: None,
            matched_terms: Vec::new(),
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            file_path: symbol.file_path.clone(),
            module_path: symbol.module_path.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: crate::symbol_query::preview(&symbol.signature, 160),
            docstring: crate::symbol_query::preview(&symbol.docstring, 240),
            impl_type: symbol.impl_type.clone(),
        }
    }
}

pub(crate) fn ensure_query_index(root: &Path, file: Option<&str>, directory: Option<&str>) -> Result<()> {
    crate::symbol_index_state::ensure_query_index(root,"rust",file,directory,|path,is_directory|build_index_scoped(root,path,is_directory).map(|_|()))
}
