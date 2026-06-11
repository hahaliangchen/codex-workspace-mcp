use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::go_index::{
    self, IndexGoWorkspaceRequest, ListGoSymbolsRequest, ReadGoSymbolRequest,
    SearchGoSymbolsRequest,
};
use crate::memory::{
    self, ListWorkMemoryRequest, RecordWorkMemoryRequest, SearchWorkMemoryRequest,
};
use crate::python_index::{
    self, IndexPythonWorkspaceRequest, ListPythonSymbolsRequest, ReadPythonSymbolRequest,
    SearchPythonSymbolsRequest,
};
use crate::rust_index::{
    self, IndexRustWorkspaceRequest, ListRustSymbolsRequest, ReadRustSymbolRequest,
    SearchRustSymbolsRequest,
};
use crate::ts_index::{
    self, IndexTsWorkspaceRequest, ListTsSymbolsRequest, ReadTsSymbolRequest,
    SearchTsSymbolsRequest,
};

const DEFAULT_MAX_READ_BYTES: u64 = 1024 * 1024;
const DEFAULT_MAX_MATCHES: usize = 100;
const DEFAULT_MAX_DEPTH: usize = 1;
const DEFAULT_WRITE_LIMIT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_SEARCH_FILE_LIMIT_BYTES: u64 = 2 * 1024 * 1024;
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
];

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("absolute paths are not accepted: {0}")]
    AbsolutePath(String),
    #[error("parent traversal is not accepted: {0}")]
    ParentTraversal(String),
    #[error("path does not exist: {0}")]
    NotFound(String),
    #[error("path is not a file: {0}")]
    NotFile(String),
    #[error("path is not a directory: {0}")]
    NotDirectory(String),
    #[error("workspace_root must be an absolute directory path: {0}")]
    WorkspaceRootMustBeAbsolute(String),
    #[error("workspace_root is required for this tool")]
    WorkspaceRootRequired,
    #[error("file is too large: {actual} bytes exceeds limit {limit} bytes")]
    FileTooLarge { actual: u64, limit: u64 },
    #[error("content is too large: {actual} bytes exceeds limit {limit} bytes")]
    ContentTooLarge { actual: usize, limit: usize },
    #[error("invalid line range: start_line={start}, end_line={end}")]
    InvalidLineRange { start: usize, end: usize },
    #[error("expected_old_text did not match the selected line range")]
    ExpectedTextMismatch,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("utf-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

pub type Result<T> = std::result::Result<T, ToolError>;

#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceInfo {
    pub workspace_root: String,
    pub platform: String,
    pub allowed_scope: String,
    pub default_ignored_dirs: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceInfoRequest {
    #[serde(default)]
    pub workspace_root: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListDirRequest {
    #[serde(default)]
    pub workspace_root: Option<String>,
    #[serde(default = "default_dot")]
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    #[serde(default)]
    pub respect_gitignore: bool,
}

#[derive(Debug, Serialize)]
pub struct ListDirResponse {
    pub root: String,
    pub entries: Vec<DirEntryInfo>,
}

#[derive(Debug, Serialize)]
pub struct DirEntryInfo {
    pub path: String,
    pub kind: EntryKind,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Deserialize)]
pub struct ReadFileRequest {
    #[serde(default)]
    pub workspace_root: Option<String>,
    pub path: String,
    #[serde(default = "default_max_read_bytes")]
    pub max_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct ReadFileResponse {
    pub path: String,
    pub bytes: usize,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct ReadFileLinesRequest {
    #[serde(default)]
    pub workspace_root: Option<String>,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Serialize)]
pub struct ReadFileLinesResponse {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub lines: Vec<LineContent>,
}

#[derive(Debug, Serialize)]
pub struct LineContent {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchTextRequest {
    #[serde(default)]
    pub workspace_root: Option<String>,
    pub query: String,
    #[serde(default = "default_dot")]
    pub path: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub respect_gitignore: bool,
    #[serde(default = "default_max_matches")]
    pub max_matches: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchTextResponse {
    pub query: String,
    pub matches: Vec<TextMatch>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct TextMatch {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct WriteFileRequest {
    #[serde(default)]
    pub workspace_root: Option<String>,
    pub path: String,
    pub content: String,
    #[serde(default = "default_true")]
    pub create_parent_dirs: bool,
}

#[derive(Debug, Serialize)]
pub struct WriteFileResponse {
    pub path: String,
    pub bytes_written: usize,
    pub go_reindexed: bool,
}

#[derive(Debug, Deserialize)]
pub struct ReplaceRangeRequest {
    #[serde(default)]
    pub workspace_root: Option<String>,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub replacement: String,
    pub expected_old_text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReplaceRangeResponse {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub bytes_written: usize,
    pub go_reindexed: bool,
}

#[derive(Debug, Serialize)]
pub struct GoIndexResult {
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct PythonIndexResult {
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct RustIndexResult {
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

#[derive(Debug, Serialize)]
pub struct TsIndexResult {
    pub index_path: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub generated_at_unix: u64,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = if root.exists() {
            root.canonicalize()?
        } else {
            return Err(ToolError::NotFound(root.display().to_string()));
        };
        if !root.is_dir() {
            return Err(ToolError::NotDirectory(root.display().to_string()));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_info(&self, request: WorkspaceInfoRequest) -> Result<WorkspaceInfo> {
        let Some(workspace_root) = request.workspace_root.as_deref() else {
            return Err(ToolError::WorkspaceRootRequired);
        };
        let workspace = self.with_root(Some(workspace_root))?;
        Ok(WorkspaceInfo {
            workspace_root: workspace.root.display().to_string(),
            platform: std::env::consts::OS.to_string(),
            allowed_scope: "file tools can read/write absolute paths accessible to the MCP server; relative paths resolve below workspace_root".to_string(),
            default_ignored_dirs: NOISE_DIRS.to_vec(),
        })
    }

    pub fn list_dir(&self, request: ListDirRequest) -> Result<ListDirResponse> {
        let workspace = self.with_root(request.workspace_root.as_deref())?;
        let root = workspace.resolve_existing(&request.path)?;
        if !root.is_dir() {
            return Err(ToolError::NotDirectory(request.path));
        }

        let mut builder = WalkBuilder::new(&root);
        builder
            .hidden(false)
            .ignore(request.respect_gitignore)
            .git_ignore(request.respect_gitignore)
            .git_exclude(request.respect_gitignore)
            .parents(request.respect_gitignore)
            .filter_entry(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| !NOISE_DIRS.contains(&name))
                    .unwrap_or(true)
            });
        if request.recursive {
            builder.max_depth(Some(request.max_depth.saturating_add(1)));
        } else {
            builder.max_depth(Some(1));
        }

        let mut entries = Vec::new();
        for item in builder.build().skip(1) {
            let entry = match item {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            entries.push(DirEntryInfo {
                path: workspace.relative_display(path)?,
                kind: entry_kind(&metadata),
                size_bytes: metadata.is_file().then_some(metadata.len()),
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(ListDirResponse {
            root: workspace.relative_display(&root)?,
            entries,
        })
    }

    pub fn read_file(&self, request: ReadFileRequest) -> Result<ReadFileResponse> {
        let workspace = self.with_root(request.workspace_root.as_deref())?;
        let path = workspace.resolve_existing_file(&request.path)?;
        let metadata = fs::metadata(&path)?;
        if metadata.len() > request.max_bytes {
            return Err(ToolError::FileTooLarge {
                actual: metadata.len(),
                limit: request.max_bytes,
            });
        }

        let bytes = fs::read(&path)?;
        let content = String::from_utf8(bytes)?;
        Ok(ReadFileResponse {
            path: workspace.relative_display(&path)?,
            bytes: content.len(),
            content,
        })
    }

    pub fn read_file_lines(&self, request: ReadFileLinesRequest) -> Result<ReadFileLinesResponse> {
        validate_line_range(request.start_line, request.end_line)?;
        let workspace = self.with_root(request.workspace_root.as_deref())?;
        let path = workspace.resolve_existing_file(&request.path)?;
        let content = fs::read_to_string(&path)?;
        let lines = content
            .lines()
            .enumerate()
            .filter_map(|(index, text)| {
                let line = index + 1;
                (line >= request.start_line && line <= request.end_line).then(|| LineContent {
                    line,
                    text: text.to_string(),
                })
            })
            .collect();

        Ok(ReadFileLinesResponse {
            path: workspace.relative_display(&path)?,
            start_line: request.start_line,
            end_line: request.end_line,
            lines,
        })
    }

    pub fn search_text(&self, request: SearchTextRequest) -> Result<SearchTextResponse> {
        let workspace = self.with_root(request.workspace_root.as_deref())?;
        let roots = workspace.resolve_search_roots(&request)?;
        let root_filters = workspace.search_root_filters(&roots)?;
        let max_matches = request.max_matches.max(1);
        let mut matches = Vec::new();
        let mut truncated = false;
        let mut seen_symbols = std::collections::HashSet::new();

        // 1. 优先尝试从 SQLite 符号索引库中查找精确匹配的符号定义
        if let Ok(conn) = crate::database::init_db(&workspace.root) {
            let root_str = workspace.root.to_string_lossy().to_string();
            let query_name = request.query.trim();

            for (lang, table) in [
                ("rust", "rust_symbols"),
                ("go", "go_symbols"),
                ("ts", "ts_symbols"),
                ("python", "python_symbols"),
            ] {
                if truncated {
                    break;
                }
                if crate::database::get_index_generated_at(&conn, &root_str, lang).is_some() {
                    truncated = query_indexed_symbol_table(
                        &conn,
                        &root_str,
                        table,
                        lang,
                        query_name,
                        &root_filters,
                        max_matches,
                        &mut matches,
                        &mut seen_symbols,
                    );
                }
            }
        }

        // 2. 如果置顶的符号匹配项未把配额占满，继续进行常规全文 Walk 扫描匹配
        if !truncated {
            let mut handles = Vec::new();
            for root in roots {
                let workspace_root = workspace.root.clone();
                let query = request.query.clone();
                let case_sensitive = request.case_sensitive;
                let respect_gitignore = request.respect_gitignore;
                handles.push(std::thread::spawn(move || {
                    scan_text_root(
                        workspace_root,
                        root,
                        query,
                        case_sensitive,
                        respect_gitignore,
                        max_matches,
                    )
                }));
            }

            'outer: for handle in handles {
                let Ok((root_matches, root_truncated)) = handle.join() else {
                    continue;
                };
                truncated |= root_truncated;
                for item in root_matches {
                    let line_key = (item.path.clone(), item.line);
                    if seen_symbols.contains(&line_key) {
                        continue;
                    }
                    if matches.len() >= max_matches {
                        truncated = true;
                        break 'outer;
                    }
                    matches.push(item);
                }
            }
        }

        Ok(SearchTextResponse {
            query: request.query,
            matches,
            truncated,
        })
    }

    fn resolve_search_roots(&self, request: &SearchTextRequest) -> Result<Vec<PathBuf>> {
        let mut requested_paths = if request.paths.is_empty() {
            vec![request.path.clone()]
        } else {
            request.paths.clone()
        };

        if requested_paths.len() == 1 {
            let single = requested_paths[0].trim();
            if self.resolve_existing(single).is_err() {
                let split_paths: Vec<String> = single
                    .split_whitespace()
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
                if split_paths.len() > 1
                    && split_paths
                        .iter()
                        .all(|path| self.resolve_existing(path).is_ok())
                {
                    requested_paths = split_paths;
                }
            }
        }

        requested_paths
            .iter()
            .map(|path| self.resolve_existing(path))
            .collect()
    }

    fn search_root_filters(&self, roots: &[PathBuf]) -> Result<Vec<String>> {
        roots
            .iter()
            .map(|root| {
                self.relative_display(root)
                    .map(|path| normalize_rel_path(&path))
            })
            .collect()
    }

    pub fn write_file(&self, request: WriteFileRequest) -> Result<WriteFileResponse> {
        if request.content.len() > DEFAULT_WRITE_LIMIT_BYTES {
            return Err(ToolError::ContentTooLarge {
                actual: request.content.len(),
                limit: DEFAULT_WRITE_LIMIT_BYTES,
            });
        }
        let workspace = self.with_root(request.workspace_root.as_deref())?;
        let path = workspace.resolve_for_write(&request.path)?;
        if let Some(parent) = path.parent() {
            if request.create_parent_dirs {
                fs::create_dir_all(parent)?;
            } else if !parent.exists() {
                return Err(ToolError::NotFound(parent.display().to_string()));
            }
        }

        write_atomic(&path, request.content.as_bytes())?;
        let go_reindexed = go_index::maybe_reindex_after_write(&workspace.root, &path)
            .ok()
            .flatten()
            .is_some();
        let ts_reindexed = ts_index::maybe_reindex_after_write(&workspace.root, &path)
            .ok()
            .flatten()
            .is_some();
        let rust_reindexed = rust_index::maybe_reindex_after_write(&workspace.root, &path)
            .ok()
            .flatten()
            .is_some();
        let python_reindexed = python_index::maybe_reindex_after_write(&workspace.root, &path)
            .ok()
            .flatten()
            .is_some();
        Ok(WriteFileResponse {
            path: workspace.relative_display(&path)?,
            bytes_written: request.content.len(),
            go_reindexed: go_reindexed || ts_reindexed || rust_reindexed || python_reindexed,
        })
    }

    pub fn replace_range(&self, request: ReplaceRangeRequest) -> Result<ReplaceRangeResponse> {
        validate_line_range(request.start_line, request.end_line)?;
        if request.replacement.len() > DEFAULT_WRITE_LIMIT_BYTES {
            return Err(ToolError::ContentTooLarge {
                actual: request.replacement.len(),
                limit: DEFAULT_WRITE_LIMIT_BYTES,
            });
        }

        let workspace = self.with_root(request.workspace_root.as_deref())?;
        let path = workspace.resolve_existing_file(&request.path)?;
        let content = fs::read_to_string(&path)?;
        let had_trailing_newline = content.ends_with('\n');
        let mut lines: Vec<String> = content.lines().map(ToString::to_string).collect();

        if request.end_line > lines.len() {
            return Err(ToolError::InvalidLineRange {
                start: request.start_line,
                end: request.end_line,
            });
        }

        let selected = lines[(request.start_line - 1)..request.end_line].join("\n");
        if let Some(expected) = request.expected_old_text.as_deref() {
            if normalize_newlines(expected).trim_end_matches('\n') != selected {
                return Err(ToolError::ExpectedTextMismatch);
            }
        }

        let replacement = normalize_newlines(&request.replacement);
        let replacement_lines: Vec<String> = if replacement.is_empty() {
            Vec::new()
        } else {
            replacement
                .trim_end_matches('\n')
                .lines()
                .map(ToString::to_string)
                .collect()
        };
        lines.splice(
            (request.start_line - 1)..request.end_line,
            replacement_lines,
        );

        let mut new_content = lines.join("\n");
        if had_trailing_newline || request.replacement.ends_with('\n') {
            new_content.push('\n');
        }

        write_atomic(&path, new_content.as_bytes())?;
        let go_reindexed = go_index::maybe_reindex_after_write(&workspace.root, &path)
            .ok()
            .flatten()
            .is_some();
        let ts_reindexed = ts_index::maybe_reindex_after_write(&workspace.root, &path)
            .ok()
            .flatten()
            .is_some();
        let rust_reindexed = rust_index::maybe_reindex_after_write(&workspace.root, &path)
            .ok()
            .flatten()
            .is_some();
        let python_reindexed = python_index::maybe_reindex_after_write(&workspace.root, &path)
            .ok()
            .flatten()
            .is_some();
        Ok(ReplaceRangeResponse {
            path: workspace.relative_display(&path)?,
            start_line: request.start_line,
            end_line: request.end_line,
            bytes_written: new_content.len(),
            go_reindexed: go_reindexed || ts_reindexed || rust_reindexed || python_reindexed,
        })
    }

    pub fn index_go_workspace(&self, request: IndexGoWorkspaceRequest) -> Result<GoIndexResult> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        let result = go_index::index_workspace(&workspace.root).map_err(map_go_index_error)?;
        Ok(GoIndexResult {
            index_path: result.index_path,
            files_indexed: result.files_indexed,
            symbols_indexed: result.symbols_indexed,
            generated_at_unix: result.generated_at_unix,
        })
    }

    pub fn go_index_status(
        &self,
        request: IndexGoWorkspaceRequest,
    ) -> Result<go_index::GoIndexStatus> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        Ok(go_index::status(&workspace.root))
    }

    pub fn list_go_symbols(
        &self,
        request: ListGoSymbolsRequest,
    ) -> Result<go_index::ListGoSymbolsResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        go_index::list_symbols(&workspace.root, request).map_err(map_go_index_error)
    }

    pub fn search_go_symbols(
        &self,
        request: SearchGoSymbolsRequest,
    ) -> Result<go_index::SearchGoSymbolsResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        go_index::search_symbols(&workspace.root, request).map_err(map_go_index_error)
    }

    pub fn read_go_symbol(
        &self,
        request: ReadGoSymbolRequest,
    ) -> Result<go_index::ReadGoSymbolResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        go_index::read_symbol(&workspace.root, request).map_err(map_go_index_error)
    }

    pub fn index_rust_workspace(
        &self,
        request: IndexRustWorkspaceRequest,
    ) -> Result<RustIndexResult> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        let result = rust_index::index_workspace(&workspace.root).map_err(map_rust_index_error)?;
        Ok(RustIndexResult {
            index_path: result.index_path,
            files_indexed: result.files_indexed,
            symbols_indexed: result.symbols_indexed,
            generated_at_unix: result.generated_at_unix,
        })
    }

    pub fn rust_index_status(
        &self,
        request: IndexRustWorkspaceRequest,
    ) -> Result<rust_index::RustIndexStatus> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        Ok(rust_index::status(&workspace.root))
    }

    pub fn list_rust_symbols(
        &self,
        request: ListRustSymbolsRequest,
    ) -> Result<rust_index::ListRustSymbolsResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        rust_index::list_symbols(&workspace.root, request).map_err(map_rust_index_error)
    }

    pub fn search_rust_symbols(
        &self,
        request: SearchRustSymbolsRequest,
    ) -> Result<rust_index::SearchRustSymbolsResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        rust_index::search_symbols(&workspace.root, request).map_err(map_rust_index_error)
    }

    pub fn read_rust_symbol(
        &self,
        request: ReadRustSymbolRequest,
    ) -> Result<rust_index::ReadRustSymbolResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        rust_index::read_symbol(&workspace.root, request).map_err(map_rust_index_error)
    }

    pub fn index_ts_workspace(&self, request: IndexTsWorkspaceRequest) -> Result<TsIndexResult> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        let result = ts_index::index_workspace(&workspace.root).map_err(map_ts_index_error)?;
        Ok(TsIndexResult {
            index_path: result.index_path,
            files_indexed: result.files_indexed,
            symbols_indexed: result.symbols_indexed,
            generated_at_unix: result.generated_at_unix,
        })
    }

    pub fn ts_index_status(
        &self,
        request: IndexTsWorkspaceRequest,
    ) -> Result<ts_index::TsIndexStatus> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        Ok(ts_index::status(&workspace.root))
    }

    pub fn list_ts_symbols(
        &self,
        request: ListTsSymbolsRequest,
    ) -> Result<ts_index::ListTsSymbolsResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        ts_index::list_symbols(&workspace.root, request).map_err(map_ts_index_error)
    }

    pub fn search_ts_symbols(
        &self,
        request: SearchTsSymbolsRequest,
    ) -> Result<ts_index::SearchTsSymbolsResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        ts_index::search_symbols(&workspace.root, request).map_err(map_ts_index_error)
    }

    pub fn read_ts_symbol(
        &self,
        request: ReadTsSymbolRequest,
    ) -> Result<ts_index::ReadTsSymbolResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        ts_index::read_symbol(&workspace.root, request).map_err(map_ts_index_error)
    }

    pub fn index_python_workspace(
        &self,
        request: IndexPythonWorkspaceRequest,
    ) -> Result<PythonIndexResult> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        let result =
            python_index::index_workspace(&workspace.root).map_err(map_python_index_error)?;
        Ok(PythonIndexResult {
            index_path: result.index_path,
            files_indexed: result.files_indexed,
            symbols_indexed: result.symbols_indexed,
            generated_at_unix: result.generated_at_unix,
        })
    }

    pub fn python_index_status(
        &self,
        request: IndexPythonWorkspaceRequest,
    ) -> Result<python_index::PythonIndexStatus> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        Ok(python_index::status(&workspace.root))
    }

    pub fn list_python_symbols(
        &self,
        request: ListPythonSymbolsRequest,
    ) -> Result<python_index::ListPythonSymbolsResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        python_index::list_symbols(&workspace.root, request).map_err(map_python_index_error)
    }

    pub fn search_python_symbols(
        &self,
        request: SearchPythonSymbolsRequest,
    ) -> Result<python_index::SearchPythonSymbolsResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        python_index::search_symbols(&workspace.root, request).map_err(map_python_index_error)
    }

    pub fn read_python_symbol(
        &self,
        request: ReadPythonSymbolRequest,
    ) -> Result<python_index::ReadPythonSymbolResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        python_index::read_symbol(&workspace.root, request).map_err(map_python_index_error)
    }

    pub fn record_work_memory(
        &self,
        request: RecordWorkMemoryRequest,
    ) -> Result<memory::RecordWorkMemoryResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        memory::record(
            &self.root,
            RecordWorkMemoryRequest {
                workspace_root: workspace.root.display().to_string(),
                ..request
            },
        )
        .map_err(map_memory_error)
    }

    pub fn list_work_memory(
        &self,
        request: ListWorkMemoryRequest,
    ) -> Result<memory::ListWorkMemoryResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        memory::list(
            &self.root,
            ListWorkMemoryRequest {
                workspace_root: workspace.root.display().to_string(),
                ..request
            },
        )
        .map_err(map_memory_error)
    }

    pub fn search_work_memory(
        &self,
        request: SearchWorkMemoryRequest,
    ) -> Result<memory::SearchWorkMemoryResponse> {
        let workspace = self.with_root(Some(&request.workspace_root))?;
        memory::search(
            &self.root,
            SearchWorkMemoryRequest {
                workspace_root: workspace.root.display().to_string(),
                ..request
            },
        )
        .map_err(map_memory_error)
    }

    fn with_root(&self, raw_root: Option<&str>) -> Result<Self> {
        match raw_root {
            Some(root) if !root.trim().is_empty() => {
                let root = root.trim();
                if !Path::new(root).is_absolute() {
                    return Err(ToolError::WorkspaceRootMustBeAbsolute(root.to_string()));
                }
                Self::new(root)
            }
            _ => Ok(Self {
                root: self.root.clone(),
            }),
        }
    }

    fn resolve_existing(&self, raw: &str) -> Result<PathBuf> {
        let candidate = self.resolve_for_write(raw)?;
        if !candidate.exists() {
            return Err(ToolError::NotFound(raw.to_string()));
        }
        candidate.canonicalize().map_err(ToolError::from)
    }

    fn resolve_existing_file(&self, raw: &str) -> Result<PathBuf> {
        let path = self.resolve_existing(raw)?;
        if !path.is_file() {
            return Err(ToolError::NotFile(raw.to_string()));
        }
        Ok(path)
    }

    fn resolve_for_write(&self, raw: &str) -> Result<PathBuf> {
        let path = Path::new(raw);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        let relative = sanitize_relative_path(raw)?;
        Ok(self.root.join(relative))
    }

    fn relative_display(&self, path: &Path) -> Result<String> {
        let value = if let Ok(relative) = path.strip_prefix(&self.root) {
            if relative.as_os_str().is_empty() {
                ".".to_string()
            } else {
                relative.to_string_lossy().replace('\\', "/")
            }
        } else {
            path.to_string_lossy().replace('\\', "/")
        };
        Ok(value)
    }
}

fn sanitize_relative_path(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(ToolError::AbsolutePath(raw.to_string()));
    }

    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => clean.push(value),
            Component::ParentDir => return Err(ToolError::ParentTraversal(raw.to_string())),
            Component::Prefix(_) | Component::RootDir => {
                return Err(ToolError::AbsolutePath(raw.to_string()));
            }
        }
    }
    Ok(clean)
}

fn validate_line_range(start: usize, end: usize) -> Result<()> {
    if start == 0 || end == 0 || start > end {
        Err(ToolError::InvalidLineRange { start, end })
    } else {
        Ok(())
    }
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let mut tmp = path.to_path_buf();
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!("{value}.tmp"))
        .unwrap_or_else(|| "tmp".to_string());
    tmp.set_extension(extension);

    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(content)?;
        file.sync_all()?;
    }
    fs::rename(tmp, path)?;
    Ok(())
}

fn entry_kind(metadata: &fs::Metadata) -> EntryKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn default_dot() -> String {
    ".".to_string()
}

fn default_true() -> bool {
    true
}

fn default_max_read_bytes() -> u64 {
    DEFAULT_MAX_READ_BYTES
}

fn default_max_matches() -> usize {
    DEFAULT_MAX_MATCHES
}

fn default_max_depth() -> usize {
    DEFAULT_MAX_DEPTH
}

fn map_python_index_error(error: python_index::PythonIndexError) -> ToolError {
    ToolError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        error.to_string(),
    ))
}

fn map_go_index_error(error: go_index::GoIndexError) -> ToolError {
    ToolError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        error.to_string(),
    ))
}

fn map_memory_error(error: memory::MemoryError) -> ToolError {
    ToolError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        error.to_string(),
    ))
}

fn scan_text_root(
    workspace_root: PathBuf,
    root: PathBuf,
    query: String,
    case_sensitive: bool,
    respect_gitignore: bool,
    max_matches: usize,
) -> (Vec<TextMatch>, bool) {
    let needle = if case_sensitive {
        query
    } else {
        query.to_lowercase()
    };
    let mut matches = Vec::new();
    let mut truncated = false;

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .ignore(respect_gitignore)
        .git_ignore(respect_gitignore)
        .git_exclude(respect_gitignore)
        .parents(respect_gitignore)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| !NOISE_DIRS.contains(&name))
                .unwrap_or(true)
        });

    'outer: for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::metadata(path) else {
            continue;
        };
        if metadata.len() > DEFAULT_SEARCH_FILE_LIMIT_BYTES {
            continue;
        }
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };

        for (line_index, line) in content.lines().enumerate() {
            let haystack = if case_sensitive {
                line.to_string()
            } else {
                line.to_lowercase()
            };
            if let Some(byte_index) = haystack.find(&needle) {
                if matches.len() >= max_matches {
                    truncated = true;
                    break 'outer;
                }
                let rel_path = path
                    .strip_prefix(&workspace_root)
                    .unwrap_or(path)
                    .display()
                    .to_string();
                let column = line[..byte_index.min(line.len())].chars().count() + 1;
                matches.push(TextMatch {
                    path: rel_path,
                    line: line_index + 1,
                    column,
                    text: line.to_string(),
                });
            }
        }
    }

    (matches, truncated)
}

fn query_indexed_symbol_table(
    conn: &rusqlite::Connection,
    workspace_root: &str,
    table: &str,
    lang: &str,
    query_name: &str,
    root_filters: &[String],
    max_matches: usize,
    matches: &mut Vec<TextMatch>,
    seen_symbols: &mut std::collections::HashSet<(String, usize)>,
) -> bool {
    let sql = format!(
        "SELECT kind, file_path, start_line, signature FROM {} WHERE name = ? AND workspace_root = ?",
        table
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return false;
    };
    let Ok(mut rows) = stmt.query(rusqlite::params![query_name, workspace_root]) else {
        return false;
    };

    while let Ok(Some(row)) = rows.next() {
        let kind: String = row.get(0).unwrap_or_default();
        let file_path: String = row.get(1).unwrap_or_default();
        let start_line: usize = row.get(2).unwrap_or(0);
        let signature: String = row.get(3).unwrap_or_default();

        if !path_allowed_by_filters(&file_path, root_filters) {
            continue;
        }
        if matches.len() >= max_matches {
            return true;
        }

        matches.push(TextMatch {
            path: file_path.clone(),
            line: start_line,
            column: 1,
            text: format!("[Symbol Definition ({} {})] {}", lang, kind, signature),
        });
        seen_symbols.insert((file_path, start_line));
    }

    false
}

fn normalize_rel_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.is_empty() {
        ".".to_string()
    } else {
        normalized
    }
}

fn path_allowed_by_filters(path: &str, filters: &[String]) -> bool {
    let path = normalize_rel_path(path);
    filters
        .iter()
        .any(|filter| filter == "." || path == *filter || path.starts_with(&format!("{}/", filter)))
}

fn map_ts_index_error(error: ts_index::TsIndexError) -> ToolError {
    ToolError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        error.to_string(),
    ))
}

fn map_rust_index_error(error: rust_index::RustIndexError) -> ToolError {
    ToolError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        error.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("codex_workspace_mcp_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn rejects_parent_traversal() {
        let root = temp_workspace("traversal");
        let workspace = Workspace::new(&root).unwrap();
        let result = workspace.read_file(ReadFileRequest {
            workspace_root: None,
            path: "../secret.txt".to_string(),
            max_bytes: 1024,
        });
        assert!(matches!(result, Err(ToolError::ParentTraversal(_))));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reads_absolute_paths_outside_workspace() {
        let root = temp_workspace("absolute_root");
        let workspace = Workspace::new(&root).unwrap();
        let outside = temp_workspace("absolute_outside");
        let outside_file = outside.join("log.txt");
        fs::write(&outside_file, "outside log").unwrap();

        let result = workspace
            .read_file(ReadFileRequest {
                workspace_root: None,
                path: outside_file.display().to_string(),
                max_bytes: 1024,
            })
            .unwrap();

        assert_eq!(result.content, "outside log");
        assert!(result.path.contains("log.txt"));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn reads_line_ranges() {
        let root = temp_workspace("lines");
        fs::write(root.join("demo.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let workspace = Workspace::new(&root).unwrap();
        let result = workspace
            .read_file_lines(ReadFileLinesRequest {
                workspace_root: None,
                path: "demo.txt".to_string(),
                start_line: 2,
                end_line: 3,
            })
            .unwrap();
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].text, "beta");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replaces_range_with_expected_text() {
        let root = temp_workspace("replace");
        fs::write(root.join("demo.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let workspace = Workspace::new(&root).unwrap();
        workspace
            .replace_range(ReplaceRangeRequest {
                workspace_root: None,
                path: "demo.txt".to_string(),
                start_line: 2,
                end_line: 2,
                replacement: "BETA".to_string(),
                expected_old_text: Some("beta".to_string()),
            })
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("demo.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_finds_text() {
        let root = temp_workspace("search");
        fs::write(root.join("demo.txt"), "alpha\nBeta\ngamma\n").unwrap();
        let workspace = Workspace::new(&root).unwrap();
        let result = workspace
            .search_text(SearchTextRequest {
                workspace_root: None,
                query: "beta".to_string(),
                path: ".".to_string(),
                paths: Vec::new(),
                case_sensitive: false,
                respect_gitignore: false,
                max_matches: 10,
            })
            .unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].line, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_accepts_multiple_paths() {
        let root = temp_workspace("search_paths");
        fs::create_dir_all(root.join("one")).unwrap();
        fs::create_dir_all(root.join("two")).unwrap();
        fs::write(root.join("one").join("a.txt"), "needle one\n").unwrap();
        fs::write(root.join("two").join("b.txt"), "needle two\n").unwrap();
        let workspace = Workspace::new(&root).unwrap();

        let result = workspace
            .search_text(SearchTextRequest {
                workspace_root: None,
                query: "needle".to_string(),
                path: ".".to_string(),
                paths: vec!["one".to_string(), "two".to_string()],
                case_sensitive: false,
                respect_gitignore: false,
                max_matches: 10,
            })
            .unwrap();

        assert_eq!(result.matches.len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_splits_space_separated_path_when_all_parts_exist() {
        let root = temp_workspace("search_split_paths");
        fs::create_dir_all(root.join("one")).unwrap();
        fs::create_dir_all(root.join("two")).unwrap();
        fs::write(root.join("one").join("a.txt"), "needle one\n").unwrap();
        fs::write(root.join("two").join("b.txt"), "needle two\n").unwrap();
        let workspace = Workspace::new(&root).unwrap();

        let result = workspace
            .search_text(SearchTextRequest {
                workspace_root: None,
                query: "needle".to_string(),
                path: "one two".to_string(),
                paths: Vec::new(),
                case_sensitive: false,
                respect_gitignore: false,
                max_matches: 10,
            })
            .unwrap();

        assert_eq!(result.matches.len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_includes_gitignored_files_by_default() {
        let root = temp_workspace("search_gitignored_default");
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join(".ignore"), "logs/\n").unwrap();
        fs::write(root.join("logs").join("run.log"), "needle in ignored log\n").unwrap();
        let workspace = Workspace::new(&root).unwrap();

        let result = workspace
            .search_text(SearchTextRequest {
                workspace_root: None,
                query: "needle".to_string(),
                path: ".".to_string(),
                paths: Vec::new(),
                case_sensitive: false,
                respect_gitignore: false,
                max_matches: 10,
            })
            .unwrap();

        assert_eq!(result.matches.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_can_respect_gitignore_when_requested() {
        let root = temp_workspace("search_gitignored_requested");
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join(".ignore"), "logs/\n").unwrap();
        fs::write(root.join("logs").join("run.log"), "needle in ignored log\n").unwrap();
        let workspace = Workspace::new(&root).unwrap();

        let result = workspace
            .search_text(SearchTextRequest {
                workspace_root: None,
                query: "needle".to_string(),
                path: ".".to_string(),
                paths: Vec::new(),
                case_sensitive: false,
                respect_gitignore: true,
                max_matches: 10,
            })
            .unwrap();

        assert_eq!(result.matches.len(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn path_filters_match_exact_and_child_paths() {
        let filters = vec!["src/pages".to_string(), "README.md".to_string()];

        assert!(path_allowed_by_filters("src/pages/index.tsx", &filters));
        assert!(path_allowed_by_filters("src\\pages\\index.tsx", &filters));
        assert!(path_allowed_by_filters("README.md", &filters));
        assert!(!path_allowed_by_filters("src/redux/store.ts", &filters));
    }

    #[test]
    fn call_can_select_workspace_root() {
        let default_root = temp_workspace("default_root");
        let selected_root = temp_workspace("selected_root");
        fs::write(default_root.join("demo.txt"), "default\n").unwrap();
        fs::write(selected_root.join("demo.txt"), "selected\n").unwrap();
        let workspace = Workspace::new(&default_root).unwrap();

        let result = workspace
            .read_file(ReadFileRequest {
                workspace_root: Some(selected_root.display().to_string()),
                path: "demo.txt".to_string(),
                max_bytes: 1024,
            })
            .unwrap();

        assert_eq!(result.content, "selected\n");
        let _ = fs::remove_dir_all(default_root);
        let _ = fs::remove_dir_all(selected_root);
    }

    #[test]
    fn selected_workspace_root_must_be_absolute() {
        let root = temp_workspace("absolute_root");
        let workspace = Workspace::new(&root).unwrap();
        let result = workspace.read_file(ReadFileRequest {
            workspace_root: Some("relative-project".to_string()),
            path: "demo.txt".to_string(),
            max_bytes: 1024,
        });

        assert!(matches!(
            result,
            Err(ToolError::WorkspaceRootMustBeAbsolute(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_info_requires_workspace_root() {
        let root = temp_workspace("workspace_info_required");
        let workspace = Workspace::new(&root).unwrap();
        let result = workspace.workspace_info(WorkspaceInfoRequest {
            workspace_root: None,
        });

        assert!(matches!(result, Err(ToolError::WorkspaceRootRequired)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn write_go_file_refreshes_existing_go_index() {
        let root = temp_workspace("go_reindex");
        fs::write(root.join("demo.go"), "package demo\n\nfunc OldName() {}\n").unwrap();
        let workspace = Workspace::new(&root).unwrap();
        workspace
            .index_go_workspace(IndexGoWorkspaceRequest {
                workspace_root: root.display().to_string(),
            })
            .unwrap();

        let result = workspace
            .write_file(WriteFileRequest {
                workspace_root: Some(root.display().to_string()),
                path: "demo.go".to_string(),
                content: "package demo\n\nfunc NewName() {}\n".to_string(),
                create_parent_dirs: true,
            })
            .unwrap();

        assert!(result.go_reindexed);
        let search = workspace
            .search_go_symbols(SearchGoSymbolsRequest {
                workspace_root: root.display().to_string(),
                query: "NewName".to_string(),
                limit: 5,
            })
            .unwrap();
        assert_eq!(search.matches.len(), 1);
        assert_eq!(search.matches[0].name, "NewName");
        let _ = fs::remove_dir_all(root);
    }
}
