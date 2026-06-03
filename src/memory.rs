use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::database::init_db;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
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
    _server_root: &Path,
    request: RecordWorkMemoryRequest,
) -> Result<RecordWorkMemoryResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();

    let files_json = serde_json::to_string(&request.files_changed)?;

    conn.execute(
        "INSERT INTO memories (time_unix, workspace_root, summary, implementation, tests, risks, files_changed)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            now as i64,
            request.workspace_root,
            request.summary,
            request.implementation,
            request.tests,
            request.risks,
            files_json
        ],
    )?;

    let db_path = workspace_root_path.join(".codex-workspace-mcp").join("codex_state.db");
    let display_path = db_path.to_string_lossy().replace('\\', "/");

    let memory = WorkMemory {
        time_unix: now,
        workspace_root: request.workspace_root,
        summary: request.summary,
        files_changed: request.files_changed,
        implementation: request.implementation,
        tests: request.tests,
        risks: request.risks,
    };

    Ok(RecordWorkMemoryResponse {
        memory_path: display_path,
        recorded: memory,
    })
}

pub fn list(_server_root: &Path, request: ListWorkMemoryRequest) -> Result<ListWorkMemoryResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;

    let mut stmt = conn.prepare(
        "SELECT time_unix, workspace_root, summary, implementation, tests, risks, files_changed
         FROM memories
         WHERE workspace_root = ?
         ORDER BY time_unix DESC
         LIMIT ?"
    )?;

    let rows = stmt.query_map(params![request.workspace_root, request.limit as i64], |row| {
        let time_unix: i64 = row.get(0)?;
        let workspace_root: String = row.get(1)?;
        let summary: String = row.get(2)?;
        let implementation: String = row.get(3)?;
        let tests: String = row.get(4)?;
        let risks: String = row.get(5)?;
        let files_json: String = row.get(6)?;

        let files_changed: Vec<String> = serde_json::from_str(&files_json).unwrap_or_default();

        Ok(WorkMemory {
            time_unix: time_unix as u64,
            workspace_root,
            summary,
            files_changed,
            implementation,
            tests,
            risks,
        })
    })?;

    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }

    let db_path = workspace_root_path.join(".codex-workspace-mcp").join("codex_state.db");
    let display_path = db_path.to_string_lossy().replace('\\', "/");

    Ok(ListWorkMemoryResponse {
        memory_path: display_path,
        memories,
    })
}

pub fn search(
    _server_root: &Path,
    request: SearchWorkMemoryRequest,
) -> Result<SearchWorkMemoryResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;

    let needle = format!("%{}%", request.query.to_lowercase());

    let mut stmt = conn.prepare(
        "SELECT time_unix, workspace_root, summary, implementation, tests, risks, files_changed
         FROM memories
         WHERE workspace_root = ?
           AND (
             LOWER(summary) LIKE ? OR
             LOWER(implementation) LIKE ? OR
             LOWER(tests) LIKE ? OR
             LOWER(risks) LIKE ? OR
             LOWER(files_changed) LIKE ?
           )
         ORDER BY time_unix DESC
         LIMIT ?"
    )?;

    let rows = stmt.query_map(
        params![
            request.workspace_root,
            needle,
            needle,
            needle,
            needle,
            needle,
            request.limit as i64
        ],
        |row| {
            let time_unix: i64 = row.get(0)?;
            let workspace_root: String = row.get(1)?;
            let summary: String = row.get(2)?;
            let implementation: String = row.get(3)?;
            let tests: String = row.get(4)?;
            let risks: String = row.get(5)?;
            let files_json: String = row.get(6)?;

            let files_changed: Vec<String> = serde_json::from_str(&files_json).unwrap_or_default();

            Ok(WorkMemory {
                time_unix: time_unix as u64,
                workspace_root,
                summary,
                files_changed,
                implementation,
                tests,
                risks,
            })
        }
    )?;

    let mut matches = Vec::new();
    for row in rows {
        matches.push(row?);
    }

    let db_path = workspace_root_path.join(".codex-workspace-mcp").join("codex_state.db");
    let display_path = db_path.to_string_lossy().replace('\\', "/");

    Ok(SearchWorkMemoryResponse {
        memory_path: display_path,
        query: request.query,
        matches,
    })
}

fn default_limit() -> usize {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("codex_workspace_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn records_lists_and_searches_memory() {
        let ws = temp_workspace("basic");
        let ws_str = ws.to_string_lossy().into_owned();

        record(
            &ws,
            RecordWorkMemoryRequest {
                workspace_root: ws_str.clone(),
                summary: "Added Go symbol index".to_string(),
                files_changed: vec!["src/go_index.rs".to_string()],
                implementation: "Indexed methods and docstrings".to_string(),
                tests: "cargo test passed".to_string(),
                risks: String::new(),
            },
        )
        .unwrap();

        let listed = list(
            &ws,
            ListWorkMemoryRequest {
                workspace_root: ws_str.clone(),
                limit: 5,
            },
        )
        .unwrap();
        assert_eq!(listed.memories.len(), 1);

        let searched = search(
            &ws,
            SearchWorkMemoryRequest {
                workspace_root: ws_str,
                query: "docstrings".to_string(),
                limit: 5,
            },
        )
        .unwrap();
        assert_eq!(searched.matches.len(), 1);

        let _ = fs::remove_dir_all(ws);
    }
}
