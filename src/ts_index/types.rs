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

pub(crate) const INDEX_DIR: &str = ".codex-workspace-mcp";
pub(crate) const INDEX_FILE: &str = "ts_index.json";
pub(crate) const MAX_TS_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const NOISE_DIRS: &[&str] = &[
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
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
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

pub(crate) fn default_limit() -> usize {
    20
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
