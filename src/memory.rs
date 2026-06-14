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

#[derive(Debug, Deserialize)]
pub struct RecordArchitectureMemoryRequest {
    pub workspace_root: String,
    pub area: String,
    pub summary: String,
    #[serde(default)]
    pub key_symbols: Vec<String>,
    #[serde(default)]
    pub key_files: Vec<String>,
    #[serde(default)]
    pub boundaries: String,
    #[serde(default)]
    pub common_tasks: Vec<String>,
    #[serde(default)]
    pub risks: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArchitectureMemory {
    pub created_at_unix: u64,
    pub updated_at_unix: u64,
    pub workspace_root: String,
    pub area: String,
    pub summary: String,
    pub key_symbols: Vec<String>,
    pub key_files: Vec<String>,
    pub boundaries: String,
    pub common_tasks: Vec<String>,
    pub risks: String,
}

#[derive(Debug, Serialize)]
pub struct RecordArchitectureMemoryResponse {
    pub memory_path: String,
    pub recorded: ArchitectureMemory,
}

#[derive(Debug, Deserialize)]
pub struct ListArchitectureMemoryRequest {
    pub workspace_root: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct ListArchitectureMemoryResponse {
    pub memory_path: String,
    pub memories: Vec<ArchitectureMemory>,
}

#[derive(Debug, Deserialize)]
pub struct SearchArchitectureMemoryRequest {
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchArchitectureMemoryResponse {
    pub memory_path: String,
    pub query: String,
    pub matches: Vec<ArchitectureMemory>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SymbolBusinessContext {
    #[serde(default)]
    pub updated_at_unix: u64,
    #[serde(default)]
    pub workspace_root: String,
    pub symbol_id: String,
    pub symbol_name: String,
    pub language: String,
    pub file_path: String,
    pub belongs_to_area: String,
    pub business_role: String,
    #[serde(default)]
    pub common_tasks: Vec<String>,
    #[serde(default)]
    pub read_when: String,
    #[serde(default)]
    pub avoid_when: String,
    #[serde(default)]
    pub risks: String,
    #[serde(default)]
    pub confidence: f64,
}

#[derive(Debug, Deserialize)]
pub struct RecordSymbolBusinessContextRequest {
    pub workspace_root: String,
    pub symbol_id: String,
    #[serde(default)]
    pub symbol_name: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub file_path: String,
    #[serde(default)]
    pub belongs_to_area: String,
    pub business_role: String,
    #[serde(default)]
    pub common_tasks: Vec<String>,
    #[serde(default)]
    pub read_when: String,
    #[serde(default)]
    pub avoid_when: String,
    #[serde(default)]
    pub risks: String,
    #[serde(default)]
    pub confidence: f64,
}

#[derive(Debug, Serialize)]
pub struct RecordSymbolBusinessContextResponse {
    pub memory_path: String,
    pub recorded: SymbolBusinessContext,
}

#[derive(Debug, Deserialize)]
pub struct ListSymbolBusinessContextRequest {
    pub workspace_root: String,
    #[serde(default)]
    pub belongs_to_area: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct ListSymbolBusinessContextResponse {
    pub memory_path: String,
    pub contexts: Vec<SymbolBusinessContext>,
}

#[derive(Debug, Deserialize)]
pub struct SearchSymbolBusinessContextRequest {
    pub workspace_root: String,
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct SearchSymbolBusinessContextResponse {
    pub memory_path: String,
    pub query: String,
    pub matches: Vec<SymbolBusinessContext>,
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

    let db_path = workspace_root_path
        .join(".codex-workspace-mcp")
        .join("codex_state.db");
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
         LIMIT ?",
    )?;

    let rows = stmt.query_map(
        params![request.workspace_root, request.limit as i64],
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
        },
    )?;

    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }

    let db_path = workspace_root_path
        .join(".codex-workspace-mcp")
        .join("codex_state.db");
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
         LIMIT ?",
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
        },
    )?;

    let mut matches = Vec::new();
    for row in rows {
        matches.push(row?);
    }

    let db_path = workspace_root_path
        .join(".codex-workspace-mcp")
        .join("codex_state.db");
    let display_path = db_path.to_string_lossy().replace('\\', "/");

    Ok(SearchWorkMemoryResponse {
        memory_path: display_path,
        query: request.query,
        matches,
    })
}

pub fn record_architecture(
    _server_root: &Path,
    request: RecordArchitectureMemoryRequest,
) -> Result<RecordArchitectureMemoryResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;

    let now = unix_now();
    let key_symbols_json = serde_json::to_string(&request.key_symbols)?;
    let key_files_json = serde_json::to_string(&request.key_files)?;
    let common_tasks_json = serde_json::to_string(&request.common_tasks)?;

    conn.execute(
        "INSERT INTO architecture_memories
         (workspace_root, area, summary, key_symbols, key_files, boundaries, common_tasks, risks, created_at_unix, updated_at_unix)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(workspace_root, area) DO UPDATE SET
           summary = excluded.summary,
           key_symbols = excluded.key_symbols,
           key_files = excluded.key_files,
           boundaries = excluded.boundaries,
           common_tasks = excluded.common_tasks,
           risks = excluded.risks,
           updated_at_unix = excluded.updated_at_unix",
        params![
            request.workspace_root,
            request.area,
            request.summary,
            key_symbols_json,
            key_files_json,
            request.boundaries,
            common_tasks_json,
            request.risks,
            now as i64,
            now as i64,
        ],
    )?;

    let memory = fetch_architecture_by_area(&conn, &request.workspace_root, &request.area)?;
    Ok(RecordArchitectureMemoryResponse {
        memory_path: db_display_path(workspace_root_path),
        recorded: memory,
    })
}

pub fn list_architecture(
    _server_root: &Path,
    request: ListArchitectureMemoryRequest,
) -> Result<ListArchitectureMemoryResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;
    let mut stmt = conn.prepare(
        "SELECT created_at_unix, updated_at_unix, workspace_root, area, summary, key_symbols, key_files, boundaries, common_tasks, risks
         FROM architecture_memories
         WHERE workspace_root = ?
         ORDER BY updated_at_unix DESC
         LIMIT ?",
    )?;

    let rows = stmt.query_map(
        params![request.workspace_root, request.limit as i64],
        |row| architecture_memory_from_row(row),
    )?;

    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }

    Ok(ListArchitectureMemoryResponse {
        memory_path: db_display_path(workspace_root_path),
        memories,
    })
}

pub fn search_architecture(
    _server_root: &Path,
    request: SearchArchitectureMemoryRequest,
) -> Result<SearchArchitectureMemoryResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;
    let needle = format!("%{}%", request.query.to_lowercase());

    let mut stmt = conn.prepare(
        "SELECT created_at_unix, updated_at_unix, workspace_root, area, summary, key_symbols, key_files, boundaries, common_tasks, risks
         FROM architecture_memories
         WHERE workspace_root = ?
           AND (
             LOWER(area) LIKE ? OR
             LOWER(summary) LIKE ? OR
             LOWER(key_symbols) LIKE ? OR
             LOWER(key_files) LIKE ? OR
             LOWER(boundaries) LIKE ? OR
             LOWER(common_tasks) LIKE ? OR
             LOWER(risks) LIKE ?
           )
         ORDER BY updated_at_unix DESC
         LIMIT ?",
    )?;

    let rows = stmt.query_map(
        params![
            request.workspace_root,
            needle,
            needle,
            needle,
            needle,
            needle,
            needle,
            needle,
            request.limit as i64,
        ],
        |row| architecture_memory_from_row(row),
    )?;

    let mut matches = Vec::new();
    for row in rows {
        matches.push(row?);
    }

    Ok(SearchArchitectureMemoryResponse {
        memory_path: db_display_path(workspace_root_path),
        query: request.query,
        matches,
    })
}

pub fn record_symbol_business_context(
    _server_root: &Path,
    request: RecordSymbolBusinessContextRequest,
) -> Result<RecordSymbolBusinessContextResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;
    let now = unix_now();
    let common_tasks_json = serde_json::to_string(&request.common_tasks)?;

    conn.execute(
        "INSERT INTO symbol_business_contexts
         (workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence, updated_at_unix)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(workspace_root, symbol_id) DO UPDATE SET
           symbol_name = excluded.symbol_name,
           language = excluded.language,
           file_path = excluded.file_path,
           belongs_to_area = excluded.belongs_to_area,
           business_role = excluded.business_role,
           common_tasks = excluded.common_tasks,
           read_when = excluded.read_when,
           avoid_when = excluded.avoid_when,
           risks = excluded.risks,
           confidence = excluded.confidence,
           updated_at_unix = excluded.updated_at_unix",
        params![
            request.workspace_root,
            request.symbol_id,
            request.symbol_name,
            request.language,
            request.file_path,
            request.belongs_to_area,
            request.business_role,
            common_tasks_json,
            request.read_when,
            request.avoid_when,
            request.risks,
            request.confidence,
            now as i64,
        ],
    )?;

    let recorded =
        fetch_symbol_business_context(&conn, &request.workspace_root, &request.symbol_id)?;
    Ok(RecordSymbolBusinessContextResponse {
        memory_path: db_display_path(workspace_root_path),
        recorded,
    })
}

pub fn list_symbol_business_context(
    _server_root: &Path,
    request: ListSymbolBusinessContextRequest,
) -> Result<ListSymbolBusinessContextResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;
    let mut contexts = Vec::new();

    if request.belongs_to_area.trim().is_empty() {
        let mut stmt = conn.prepare(
            "SELECT updated_at_unix, workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence
             FROM symbol_business_contexts
             WHERE workspace_root = ?
             ORDER BY updated_at_unix DESC
             LIMIT ?",
        )?;
        let rows = stmt.query_map(
            params![request.workspace_root, request.limit as i64],
            |row| symbol_business_context_from_row(row),
        )?;
        for row in rows {
            contexts.push(row?);
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT updated_at_unix, workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence
             FROM symbol_business_contexts
             WHERE workspace_root = ? AND belongs_to_area = ?
             ORDER BY updated_at_unix DESC
             LIMIT ?",
        )?;
        let rows = stmt.query_map(
            params![
                request.workspace_root,
                request.belongs_to_area,
                request.limit as i64
            ],
            |row| symbol_business_context_from_row(row),
        )?;
        for row in rows {
            contexts.push(row?);
        }
    }

    Ok(ListSymbolBusinessContextResponse {
        memory_path: db_display_path(workspace_root_path),
        contexts,
    })
}

pub fn search_symbol_business_context(
    _server_root: &Path,
    request: SearchSymbolBusinessContextRequest,
) -> Result<SearchSymbolBusinessContextResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;
    let needle = format!("%{}%", request.query.to_lowercase());
    let mut stmt = conn.prepare(
        "SELECT updated_at_unix, workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence
         FROM symbol_business_contexts
         WHERE workspace_root = ?
           AND (
             LOWER(symbol_id) LIKE ? OR
             LOWER(symbol_name) LIKE ? OR
             LOWER(language) LIKE ? OR
             LOWER(file_path) LIKE ? OR
             LOWER(belongs_to_area) LIKE ? OR
             LOWER(business_role) LIKE ? OR
             LOWER(common_tasks) LIKE ? OR
             LOWER(read_when) LIKE ? OR
             LOWER(avoid_when) LIKE ? OR
             LOWER(risks) LIKE ?
           )
         ORDER BY updated_at_unix DESC
         LIMIT ?",
    )?;
    let rows = stmt.query_map(
        params![
            request.workspace_root,
            needle,
            needle,
            needle,
            needle,
            needle,
            needle,
            needle,
            needle,
            needle,
            needle,
            request.limit as i64,
        ],
        |row| symbol_business_context_from_row(row),
    )?;

    let mut matches = Vec::new();
    for row in rows {
        matches.push(row?);
    }

    Ok(SearchSymbolBusinessContextResponse {
        memory_path: db_display_path(workspace_root_path),
        query: request.query,
        matches,
    })
}

fn fetch_symbol_business_context(
    conn: &rusqlite::Connection,
    workspace_root: &str,
    symbol_id: &str,
) -> Result<SymbolBusinessContext> {
    Ok(conn.query_row(
        "SELECT updated_at_unix, workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence
         FROM symbol_business_contexts
         WHERE workspace_root = ? AND symbol_id = ?",
        params![workspace_root, symbol_id],
        |row| symbol_business_context_from_row(row),
    )?)
}

fn symbol_business_context_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SymbolBusinessContext> {
    let updated_at_unix: i64 = row.get(0)?;
    let common_tasks_json: String = row.get(8)?;
    Ok(SymbolBusinessContext {
        updated_at_unix: updated_at_unix as u64,
        workspace_root: row.get(1)?,
        symbol_id: row.get(2)?,
        symbol_name: row.get(3)?,
        language: row.get(4)?,
        file_path: row.get(5)?,
        belongs_to_area: row.get(6)?,
        business_role: row.get(7)?,
        common_tasks: serde_json::from_str(&common_tasks_json).unwrap_or_default(),
        read_when: row.get(9)?,
        avoid_when: row.get(10)?,
        risks: row.get(11)?,
        confidence: row.get(12)?,
    })
}

fn fetch_architecture_by_area(
    conn: &rusqlite::Connection,
    workspace_root: &str,
    area: &str,
) -> Result<ArchitectureMemory> {
    Ok(conn.query_row(
        "SELECT created_at_unix, updated_at_unix, workspace_root, area, summary, key_symbols, key_files, boundaries, common_tasks, risks
         FROM architecture_memories
         WHERE workspace_root = ? AND area = ?",
        params![workspace_root, area],
        |row| architecture_memory_from_row(row),
    )?)
}

fn architecture_memory_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArchitectureMemory> {
    let created_at_unix: i64 = row.get(0)?;
    let updated_at_unix: i64 = row.get(1)?;
    let workspace_root: String = row.get(2)?;
    let area: String = row.get(3)?;
    let summary: String = row.get(4)?;
    let key_symbols_json: String = row.get(5)?;
    let key_files_json: String = row.get(6)?;
    let boundaries: String = row.get(7)?;
    let common_tasks_json: String = row.get(8)?;
    let risks: String = row.get(9)?;

    Ok(ArchitectureMemory {
        created_at_unix: created_at_unix as u64,
        updated_at_unix: updated_at_unix as u64,
        workspace_root,
        area,
        summary,
        key_symbols: serde_json::from_str(&key_symbols_json).unwrap_or_default(),
        key_files: serde_json::from_str(&key_files_json).unwrap_or_default(),
        boundaries,
        common_tasks: serde_json::from_str(&common_tasks_json).unwrap_or_default(),
        risks,
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

fn db_display_path(workspace_root_path: &Path) -> String {
    workspace_root_path
        .join(".codex-workspace-mcp")
        .join("codex_state.db")
        .to_string_lossy()
        .replace('\\', "/")
}

fn default_limit() -> usize {
    10
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    fn temp_workspace(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("codex_workspace_{name}_{}", std::process::id()));
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

    #[test]
    fn records_updates_lists_and_searches_architecture_memory() {
        let ws = temp_workspace("architecture");
        let ws_str = ws.to_string_lossy().into_owned();

        record_architecture(
            &ws,
            RecordArchitectureMemoryRequest {
                workspace_root: ws_str.clone(),
                area: "Responses format translation".to_string(),
                summary: "Maps Codex Responses input into upstream chat messages.".to_string(),
                key_symbols: vec!["responses_body_to_openai_chat_messages".to_string()],
                key_files: vec!["src/format_translate/responses_chat.rs".to_string()],
                boundaries: "Do not change agent tool execution for pure escaping fixes."
                    .to_string(),
                common_tasks: vec!["转义功能".to_string()],
                risks: "Bad role mapping can break upstream compatibility.".to_string(),
            },
        )
        .unwrap();

        record_architecture(
            &ws,
            RecordArchitectureMemoryRequest {
                workspace_root: ws_str.clone(),
                area: "Responses format translation".to_string(),
                summary: "Updated summary".to_string(),
                key_symbols: vec!["build_openai_chat_request".to_string()],
                key_files: vec!["src/format_translate/responses_chat.rs".to_string()],
                boundaries: String::new(),
                common_tasks: vec!["escaping".to_string()],
                risks: String::new(),
            },
        )
        .unwrap();

        let listed = list_architecture(
            &ws,
            ListArchitectureMemoryRequest {
                workspace_root: ws_str.clone(),
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(listed.memories.len(), 1);
        assert_eq!(listed.memories[0].summary, "Updated summary");
        assert_eq!(
            listed.memories[0].key_symbols,
            vec!["build_openai_chat_request"]
        );

        let searched = search_architecture(
            &ws,
            SearchArchitectureMemoryRequest {
                workspace_root: ws_str,
                query: "escaping".to_string(),
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(searched.matches.len(), 1);

        let _ = fs::remove_dir_all(ws);
    }

    #[test]
    fn records_lists_and_searches_symbol_business_context() {
        let ws = temp_workspace("symbol_business_context");
        let ws_str = ws.to_string_lossy().into_owned();

        record_symbol_business_context(
            &ws,
            RecordSymbolBusinessContextRequest {
                workspace_root: ws_str.clone(),
                symbol_id: "rust:src/agent_runtime.rs:run_agent_loop".to_string(),
                symbol_name: "run_agent_loop".to_string(),
                language: "rust".to_string(),
                file_path: "src/agent_runtime.rs".to_string(),
                belongs_to_area: "Agent Runtime".to_string(),
                business_role: "Runs the local ReAct tool loop for /v1/responses.".to_string(),
                common_tasks: vec!["工具循环".to_string(), "并发工具调用".to_string()],
                read_when: "User asks about agent tool execution behavior.".to_string(),
                avoid_when: "User asks only about Responses-to-Chat escaping.".to_string(),
                risks: "Loop changes can affect all tool calls.".to_string(),
                confidence: 0.9,
            },
        )
        .unwrap();

        let listed = list_symbol_business_context(
            &ws,
            ListSymbolBusinessContextRequest {
                workspace_root: ws_str.clone(),
                belongs_to_area: "Agent Runtime".to_string(),
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(listed.contexts.len(), 1);
        assert_eq!(listed.contexts[0].symbol_name, "run_agent_loop");

        let searched = search_symbol_business_context(
            &ws,
            SearchSymbolBusinessContextRequest {
                workspace_root: ws_str,
                query: "并发工具调用".to_string(),
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(searched.matches.len(), 1);

        let _ = fs::remove_dir_all(ws);
    }
}
