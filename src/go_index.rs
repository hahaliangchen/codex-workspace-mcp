use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};

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
}

pub type Result<T> = std::result::Result<T, GoIndexError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoIndex {
    pub workspace_root: String,
    pub generated_at_unix: u64,
    pub files_indexed: usize,
    pub symbols: Vec<GoSymbol>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoSymbol {
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
    let index = build_index(root)?;
    let index_path = index_path(root);
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&index_path, serde_json::to_vec_pretty(&index)?)?;
    Ok(IndexGoWorkspaceResponse {
        index_path: relative_display(root, &index_path),
        files_indexed: index.files_indexed,
        symbols_indexed: index.symbols.len(),
        generated_at_unix: index.generated_at_unix,
    })
}

pub fn status(root: &Path) -> GoIndexStatus {
    let index_path = index_path(root);
    if let Ok(content) = fs::read_to_string(&index_path)
        && let Ok(index) = serde_json::from_str::<GoIndex>(&content)
    {
        return GoIndexStatus {
            index_path: relative_display(root, &index_path),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: Some(index.generated_at_unix),
            files_indexed: Some(index.files_indexed),
            symbols_indexed: Some(index.symbols.len()),
        };
    }
    GoIndexStatus {
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
) -> Result<Option<IndexGoWorkspaceResponse>> {
    if changed_path.extension().and_then(|value| value.to_str()) != Some("go") {
        return Ok(None);
    }
    if !index_path(root).exists() {
        return Ok(None);
    }
    index_workspace(root).map(Some)
}

pub fn list_symbols(root: &Path, request: ListGoSymbolsRequest) -> Result<ListGoSymbolsResponse> {
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
        .map(GoSymbolSummary::from)
        .collect();
    Ok(ListGoSymbolsResponse { symbols })
}

pub fn search_symbols(
    root: &Path,
    request: SearchGoSymbolsRequest,
) -> Result<SearchGoSymbolsResponse> {
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
    let index = load_or_build(root)?;
    let symbol = index
        .symbols
        .iter()
        .find(|symbol| symbol.id == request.symbol_id)
        .cloned()
        .ok_or_else(|| GoIndexError::SymbolNotFound(request.symbol_id.clone()))?;
    let path = root.join(&symbol.file_path);
    let content = fs::read_to_string(path)?;
    let lines: Vec<_> = content.lines().collect();
    let content = lines[(symbol.start_line - 1)..symbol.end_line.min(lines.len())].join("\n");

    let (callers, callees, suggested_reads) = if request.include_context {
        build_context(&index, &symbol)
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

fn build_index(root: &Path) -> Result<GoIndex> {
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
        if path.extension().and_then(|value| value.to_str()) != Some("go") {
            continue;
        }
        if path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .ends_with("_test.go")
        {
            continue;
        }
        let metadata = fs::metadata(path)?;
        if metadata.len() > MAX_GO_FILE_BYTES {
            continue;
        }
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        files_indexed += 1;
        symbols.extend(parse_go_file(root, path, &content));
    }

    Ok(GoIndex {
        workspace_root: root.display().to_string(),
        generated_at_unix: now_unix(),
        files_indexed,
        symbols,
    })
}

fn parse_go_file(root: &Path, path: &Path, content: &str) -> Vec<GoSymbol> {
    let relative_path = relative_display(root, path);
    let package = parse_package(content);
    let lines: Vec<&str> = content.lines().collect();
    let func_re = Regex::new(r"^\s*func\s+(\([^)]+\)\s*)?([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap();
    let type_re =
        Regex::new(r"^\s*type\s+([A-Za-z_][A-Za-z0-9_]*)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    let mut symbols = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if let Some(captures) = func_re.captures(line) {
            let start_line = idx + 1;
            let receiver = captures
                .get(1)
                .map(|value| value.as_str().trim().to_string());
            let name = captures
                .get(2)
                .map(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let end_line = find_block_end(&lines, idx);
            let signature = collect_signature(&lines, idx);
            let docstring = collect_docstring(&lines, idx);
            let kind = if receiver.is_some() {
                GoSymbolKind::Method
            } else {
                GoSymbolKind::Function
            };
            let calls = collect_calls(&lines, idx, end_line);
            let id = symbol_id(&relative_path, &name, start_line, receiver.as_deref());
            symbols.push(GoSymbol {
                id,
                name,
                kind,
                package: package.clone(),
                file_path: relative_path.clone(),
                start_line,
                end_line,
                signature,
                docstring,
                receiver,
                calls,
            });
        } else if let Some(captures) = type_re.captures(line) {
            let start_line = idx + 1;
            let name = captures
                .get(1)
                .map(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let type_kind = captures.get(2).map(|value| value.as_str()).unwrap_or("");
            let kind = match type_kind {
                "struct" => GoSymbolKind::Struct,
                "interface" => GoSymbolKind::Interface,
                _ => GoSymbolKind::Type,
            };
            let end_line = find_block_end(&lines, idx);
            let signature = collect_signature(&lines, idx);
            let docstring = collect_docstring(&lines, idx);
            let id = symbol_id(&relative_path, &name, start_line, None);
            symbols.push(GoSymbol {
                id,
                name,
                kind,
                package: package.clone(),
                file_path: relative_path.clone(),
                start_line,
                end_line,
                signature,
                docstring,
                receiver: None,
                calls: Vec::new(),
            });
        }
    }

    symbols
}

fn parse_package(content: &str) -> String {
    content
        .lines()
        .find_map(|line| line.trim().strip_prefix("package ").map(str::trim))
        .unwrap_or("")
        .to_string()
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

fn collect_signature(lines: &[&str], start_idx: usize) -> String {
    let mut parts = Vec::new();
    for line in &lines[start_idx..] {
        let before_body = line.split('{').next().unwrap_or(line).trim();
        if !before_body.is_empty() {
            parts.push(before_body.to_string());
        }
        if line.contains('{') || parens_balanced(&parts.join(" ")) {
            break;
        }
    }
    parts.join(" ")
}

fn find_block_end(lines: &[&str], start_idx: usize) -> usize {
    let mut depth = 0isize;
    let mut seen_body = false;
    for (idx, line) in lines.iter().enumerate().skip(start_idx) {
        for ch in strip_line_comment(line).chars() {
            match ch {
                '{' => {
                    depth += 1;
                    seen_body = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if seen_body && depth <= 0 {
            return idx + 1;
        }
        if !seen_body && !line.trim_end().ends_with(',') && parens_balanced(line) {
            return idx + 1;
        }
    }
    start_idx + 1
}

fn collect_calls(lines: &[&str], start_idx: usize, end_line: usize) -> Vec<GoCall> {
    let call_re = Regex::new(r"(?:\.|\b)([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap();
    let keywords: BTreeSet<&str> = [
        "if", "for", "switch", "select", "func", "return", "go", "defer", "range",
    ]
    .into_iter()
    .collect();
    let mut calls = Vec::new();
    for (idx, line) in lines.iter().enumerate().take(end_line).skip(start_idx + 1) {
        let clean = strip_line_comment(line);
        for captures in call_re.captures_iter(&clean) {
            let target = captures.get(1).map(|value| value.as_str()).unwrap_or("");
            if target.is_empty() || keywords.contains(target) {
                continue;
            }
            calls.push(GoCall {
                target_text: target.to_string(),
                line: idx + 1,
                snippet: line.trim().to_string(),
            });
        }
    }
    calls
}

fn build_context(
    index: &GoIndex,
    symbol: &GoSymbol,
) -> (Vec<GoCaller>, Vec<GoCallee>, Vec<GoSuggestedRead>) {
    let mut name_to_ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut id_to_symbol: BTreeMap<String, &GoSymbol> = BTreeMap::new();
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
        .map(|call| GoCallee {
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
                suggested_reads.push(GoSuggestedRead {
                    reason: "direct_callee".to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: GoSymbolSummary::from(*matched_symbol),
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

fn load_or_build(root: &Path) -> Result<GoIndex> {
    let path = index_path(root);
    if !path.exists() {
        return Err(GoIndexError::MissingIndex);
    }
    let content = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

fn load_or_build_or_create(root: &Path) -> Result<GoIndex> {
    match load_or_build(root) {
        Ok(index) => Ok(index),
        Err(GoIndexError::MissingIndex) => {
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

fn symbol_id(file_path: &str, name: &str, line: usize, receiver: Option<&str>) -> String {
    let receiver = receiver.unwrap_or("").replace([' ', '*', '(', ')'], "");
    if receiver.is_empty() {
        format!("go:{file_path}:{name}:{line}")
    } else {
        format!("go:{file_path}:{receiver}.{name}:{line}")
    }
}

fn strip_line_comment(line: &str) -> String {
    line.split("//").next().unwrap_or(line).to_string()
}

fn parens_balanced(value: &str) -> bool {
    value.chars().filter(|ch| *ch == '(').count() == value.chars().filter(|ch| *ch == ')').count()
}

fn now_unix() -> u64 {
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
            suggestion.reason == "direct_callee" && suggestion.symbol.name == "SaveWorkflow"
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
        assert!(index_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }
}
