use serde::{Deserialize, Serialize};

pub(crate) const MAX_TS_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TsIndexError {
    #[error("ts index not found; call index_ts_workspace first")]
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

pub type Result<T> = std::result::Result<T, TsIndexError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
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
    #[serde(default)]
    pub re_exports: Vec<TsReExport>,
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
    pub index: crate::symbol_index_state::IndexHealth,
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct TsIndexStatus {
    pub index: Option<crate::symbol_index_state::IndexHealth>,
    pub index_path: String,
    pub exists: bool,
    pub workspace_root: String,
    pub generated_at_unix: Option<u64>,
    pub files_indexed: Option<usize>,
    pub symbols_indexed: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ListTsSymbolsRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::ListOptions,
    pub workspace_root: String,
    pub file_path: Option<String>,
    pub kind: Option<TsSymbolKind>,
}

#[derive(Debug, Serialize)]
pub struct ListTsSymbolsResponse {
    pub page: crate::symbol_query::PageInfo,
    pub index: crate::symbol_index_state::IndexHealth,
    pub symbols: Vec<TsSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct SearchTsSymbolsRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::SearchOptions,
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchTsSymbolsResponse {
    pub terms: Vec<String>,
    pub match_mode: crate::symbol_query::MatchMode,
    pub page: crate::symbol_query::PageInfo,
    pub index: crate::symbol_index_state::IndexHealth,
    pub query: String,
    pub matches: Vec<TsSymbolSummary>,
}

#[derive(Debug, Deserialize)]
pub struct ReadTsSymbolRequest {
    #[serde(flatten)]
    pub options: crate::symbol_query::ReadOptions,
    pub workspace_root: String,
    #[serde(default)]
    pub symbol_id: String,
    #[serde(default)]
    pub include_context: bool,
    #[serde(default)]
    pub include_related_types: bool,
    #[serde(default)]
    pub include_outline: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadTsSymbolResponse {
    pub description: crate::symbol_description::Description,
    pub index: crate::symbol_index_state::IndexHealth,
    pub relationship_accuracy: &'static str,
    pub symbol: TsSymbol,
    pub content: String,
    pub callees: Vec<TsCallee>,
    pub callers: Vec<TsCaller>,
    pub resolved_imports: Vec<TsResolvedImport>,
    pub suggested_reads: Vec<TsSuggestedRead>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related_types: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related_type_issues: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_context: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsSymbolSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<crate::symbol_description::Description>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_terms: Vec<String>,
    pub id: String,
    pub name: String,
    pub kind: TsSymbolKind,
    pub file_path: String,
    pub scope_path: String,
    pub parent_id: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub signature: String,
    #[serde(skip_serializing_if = "String::is_empty")]
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

pub(crate) fn default_limit() -> usize {
    20
}

impl From<&TsSymbol> for TsSymbolSummary {
    fn from(symbol: &TsSymbol) -> Self {
        Self {
            description: None,
            score: None,
            matched_terms: Vec::new(),
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            file_path: symbol.file_path.clone(),
            scope_path: symbol.scope_path.clone(),
            parent_id: symbol.parent_id.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            signature: crate::symbol_query::preview(&symbol.signature, 160),
            docstring: crate::symbol_query::preview(&symbol.docstring, 240),
            export: symbol.export,
        }
    }
}
