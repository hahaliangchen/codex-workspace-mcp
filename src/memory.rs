use std::{
    collections::hash_map::DefaultHasher,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const MEMORY_DIR: &str = "memory";
const MEMORY_FILE: &str = "memories.jsonl";

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, MemoryError>;

#[derive(Debug, Deserialize)]
pub struct RecordWorkMemoryRequest {
    pub workspace_root: String,
    pub summary: String,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub implementation: String,
    #[serde(default)]
    pub tests: String,
    #[serde(default)]
    pub risks: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkMemory {
    pub time_unix: u64,
    pub workspace_root: String,
    pub summary: String,
    pub files_changed: Vec<String>,
    pub implementation: String,
    pub tests: String,
    pub risks: String,
}

#[derive(Debug, Serialize)]
pub struct RecordWorkMemoryResponse {
    pub memory_path: String,
    pub recorded: WorkMemory,
}

#[derive(Debug, Deserialize)]
pub struct ListWorkMemoryRequest {
    pub workspace_root: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct ListWorkMemoryResponse {
    pub memory_path: String,
    pub memories: Vec<WorkMemory>,
}

#[derive(Debug, Deserialize)]
pub struct SearchWorkMemoryRequest {
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchWorkMemoryResponse {
    pub memory_path: String,
    pub query: String,
    pub matches: Vec<WorkMemory>,
}

pub fn record(
    server_root: &Path,
    request: RecordWorkMemoryRequest,
) -> Result<RecordWorkMemoryResponse> {
    let memory = WorkMemory {
        time_unix: now_unix(),
        workspace_root: request.workspace_root,
        summary: request.summary,
        files_changed: request.files_changed,
        implementation: request.implementation,
        tests: request.tests,
        risks: request.risks,
    };
    let path = memory_path(server_root, &memory.workspace_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{}", serde_json::to_string(&memory)?)?;
    Ok(RecordWorkMemoryResponse {
        memory_path: relative_display(server_root, &path),
        recorded: memory,
    })
}

pub fn list(server_root: &Path, request: ListWorkMemoryRequest) -> Result<ListWorkMemoryResponse> {
    let path = memory_path(server_root, &request.workspace_root);
    let mut memories = read_memories(&path)?;
    memories.reverse();
    memories.truncate(request.limit.max(1));
    Ok(ListWorkMemoryResponse {
        memory_path: relative_display(server_root, &path),
        memories,
    })
}

pub fn search(
    server_root: &Path,
    request: SearchWorkMemoryRequest,
) -> Result<SearchWorkMemoryResponse> {
    let path = memory_path(server_root, &request.workspace_root);
    let needle = request.query.to_lowercase();
    let mut matches: Vec<_> = read_memories(&path)?
        .into_iter()
        .rev()
        .filter(|memory| memory_text(memory).to_lowercase().contains(&needle))
        .take(request.limit.max(1))
        .collect();
    matches.shrink_to_fit();
    Ok(SearchWorkMemoryResponse {
        memory_path: relative_display(server_root, &path),
        query: request.query,
        matches,
    })
}

fn read_memories(path: &Path) -> Result<Vec<WorkMemory>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)?;
    let mut memories = Vec::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        memories.push(serde_json::from_str(line)?);
    }
    Ok(memories)
}

fn memory_path(server_root: &Path, workspace_root: &str) -> PathBuf {
    server_root
        .join(MEMORY_DIR)
        .join(workspace_key(workspace_root))
        .join(MEMORY_FILE)
}

fn workspace_key(workspace_root: &str) -> String {
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in workspace_root.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            parts.push(to_pascal_case(&current));
            current.clear();
        }
    }
    if !current.is_empty() {
        parts.push(to_pascal_case(&current));
    }
    let readable = if parts.is_empty() {
        "Workspace".to_string()
    } else {
        parts.join("")
    };
    format!("{readable}_{:08x}", short_hash(workspace_root))
}

fn to_pascal_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn short_hash(value: &str) -> u32 {
    let mut hasher = DefaultHasher::new();
    value.to_lowercase().hash(&mut hasher);
    hasher.finish() as u32
}

fn memory_text(memory: &WorkMemory) -> String {
    [
        memory.summary.as_str(),
        memory.implementation.as_str(),
        memory.tests.as_str(),
        memory.risks.as_str(),
        &memory.files_changed.join("\n"),
    ]
    .join("\n")
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

fn default_limit() -> usize {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("codex_memory_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn records_lists_and_searches_memory() {
        let root = temp_root("basic");
        let workspace = r"D:\enterpriseProject\ai-ppt-server".to_string();
        record(
            &root,
            RecordWorkMemoryRequest {
                workspace_root: workspace.clone(),
                summary: "Added Go symbol index".to_string(),
                files_changed: vec!["src/go_index.rs".to_string()],
                implementation: "Indexed methods and docstrings".to_string(),
                tests: "cargo test passed".to_string(),
                risks: String::new(),
            },
        )
        .unwrap();

        let listed = list(
            &root,
            ListWorkMemoryRequest {
                workspace_root: workspace.clone(),
                limit: 5,
            },
        )
        .unwrap();
        assert_eq!(listed.memories.len(), 1);
        assert!(listed.memory_path.contains("DEnterpriseProjectAiPptServer"));

        let searched = search(
            &root,
            SearchWorkMemoryRequest {
                workspace_root: workspace,
                query: "docstrings".to_string(),
                limit: 5,
            },
        )
        .unwrap();
        assert_eq!(searched.matches.len(), 1);

        let _ = fs::remove_dir_all(root);
    }
}
