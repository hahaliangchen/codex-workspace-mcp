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

#[derive(Clone, Debug, Deserialize)]
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
    #[serde(default)]
    pub source_task_id: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub applies_to: String,
    #[serde(default)]
    pub source_refs: Vec<String>,
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
    #[serde(default)]
    pub source_task_id: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub applies_to: String,
    #[serde(default)]
    pub source_refs: Vec<String>,
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
    #[serde(default)]
    pub qualified_name: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub code_hash: String,
    #[serde(default)]
    pub definition_hash: String,
    #[serde(default = "default_stale")]
    pub stale: bool,
}

#[derive(Debug, Deserialize)]
pub struct RecordSymbolBusinessContextRequest {
    #[serde(flatten)]
    pub description: crate::symbol_description::RecordOptions,
    pub workspace_root: String,
    #[serde(default)]
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
    let source_refs_json = serde_json::to_string(&request.source_refs)?;
    let kind = if request.kind.trim().is_empty() {
        "work"
    } else {
        request.kind.trim()
    };
    let source_task_id = request
        .source_task_id
        .as_deref()
        .filter(|id| !id.trim().is_empty());

    conn.execute(
        "INSERT INTO memories (time_unix, workspace_root, summary, implementation, tests, risks,
          files_changed, source_task_id, kind, applies_to, source_refs)
         SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
         WHERE ? <> 'observer' OR NOT EXISTS (
           SELECT 1 FROM memories WHERE kind='observer' AND workspace_root=?
             AND summary=? AND implementation=? AND applies_to=? AND source_refs=?)
         ON CONFLICT DO UPDATE SET time_unix=excluded.time_unix, summary=excluded.summary,
          implementation=excluded.implementation, tests=excluded.tests, risks=excluded.risks,
          files_changed=excluded.files_changed, kind=excluded.kind,
          applies_to=excluded.applies_to, source_refs=excluded.source_refs",
        params![
            now as i64,
            request.workspace_root,
            request.summary,
            request.implementation,
            request.tests,
            request.risks,
            files_json,
            source_task_id,
            kind,
            request.applies_to,
            source_refs_json,
            kind,request.workspace_root,request.summary,request.implementation,request.applies_to,source_refs_json,
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
        source_task_id: request.source_task_id,
        kind: kind.to_owned(),
        applies_to: request.applies_to,
        source_refs: request.source_refs,
    };

    Ok(RecordWorkMemoryResponse {
        memory_path: display_path,
        recorded: memory,
    })
}

/// Keep conflicting source versions in history. Search projects only evidence
/// whose observed file hashes still match; no model or source investigation.
pub fn bind_observer_sources(root:&Path,summary:&str,implementation:&str,sources:&serde_json::Value)->Result<()> {
    init_db(root)?.execute("INSERT INTO observer_memory_sources(workspace_root,summary,implementation,sources) VALUES (?1,?2,?3,?4)
        ON CONFLICT DO UPDATE SET sources=excluded.sources",params![root.to_string_lossy(),summary,implementation,sources.to_string()])?;
    Ok(())
}

pub fn observer_sources_current(root:&Path,memory:&WorkMemory)->bool {
    use rusqlite::OptionalExtension;
    if memory.kind!="observer" {return true;}
    let sources=init_db(root).ok().and_then(|conn|conn.query_row("SELECT sources FROM observer_memory_sources WHERE workspace_root=?1 AND summary=?2 AND implementation=?3",
        params![memory.workspace_root,memory.summary,memory.implementation],|row|row.get::<_,String>(0)).optional().ok().flatten());
    let Some(sources)=sources else{return memory.source_refs.is_empty();};
    let Ok(sources)=serde_json::from_str::<serde_json::Value>(&sources) else{return false;};
    sources.as_array().is_some_and(|items|items.iter().all(|source| {
        let file=source["file_path"].as_str().unwrap_or("");let hash=source["code_hash"].as_str().unwrap_or("");
        !hash.is_empty() && crate::file_edit::workspace_path(root,file).ok().and_then(|path|std::fs::read(path).ok())
            .is_some_and(|bytes|crate::symbol_description::content_hash(&bytes)==hash)
    }))
}

fn work_memory_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkMemory> {
    let files_json: String = row.get(6)?;
    let source_refs_json: String = row.get(10)?;
    Ok(WorkMemory {
        time_unix: row.get::<_, i64>(0)? as u64,
        workspace_root: row.get(1)?,
        summary: row.get(2)?,
        implementation: row.get(3)?,
        tests: row.get(4)?,
        risks: row.get(5)?,
        files_changed: serde_json::from_str(&files_json).unwrap_or_default(),
        source_task_id: row.get(7)?,
        kind: row.get(8)?,
        applies_to: row.get(9)?,
        source_refs: serde_json::from_str(&source_refs_json).unwrap_or_default(),
    })
}

pub fn list(_server_root: &Path, request: ListWorkMemoryRequest) -> Result<ListWorkMemoryResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let conn = init_db(workspace_root_path)?;

    let mut stmt = conn.prepare(
        "SELECT time_unix, workspace_root, summary, implementation, tests, risks, files_changed,
          source_task_id, kind, applies_to, source_refs
         FROM memories
         WHERE workspace_root = ?
         ORDER BY time_unix DESC
         LIMIT ?",
    )?;

    let rows = stmt.query_map(
        params![request.workspace_root, request.limit as i64],
        work_memory_from_row,
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
        "SELECT time_unix, workspace_root, summary, implementation, tests, risks, files_changed,
          source_task_id, kind, applies_to, source_refs
         FROM memories
         WHERE workspace_root = ?
           AND (
             LOWER(summary) LIKE ? OR
             LOWER(implementation) LIKE ? OR
             LOWER(tests) LIKE ? OR
             LOWER(risks) LIKE ? OR
             LOWER(files_changed) LIKE ? OR
             LOWER(applies_to) LIKE ? OR
             LOWER(source_refs) LIKE ?
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
            needle,
            needle,
            request.limit.saturating_mul(4).min(200) as i64
        ],
        work_memory_from_row,
    )?;

    let mut matches = Vec::new();
    for row in rows {
        if matches.len()>=request.limit {break;}
        let memory=row?;
        if observer_sources_current(workspace_root_path,&memory) {matches.push(memory);}
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

fn default_stale() -> bool {
    true
}
pub(crate) const SYMBOL_CONTEXT_SELECT: &str = "SELECT updated_at_unix, workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence, qualified_name, keywords, scope, source, code_hash, stale, definition_hash FROM symbol_business_contexts";

pub fn record_symbol_business_context(
    _server_root: &Path,
    request: RecordSymbolBusinessContextRequest,
) -> Result<RecordSymbolBusinessContextResponse> {
    let workspace_root_path = Path::new(&request.workspace_root);
    let prepared = crate::symbol_description::prepare_record(workspace_root_path, &request)?;
    let mut conn = init_db(workspace_root_path)?;
    let tx = conn.transaction()?;
    let now = unix_now();
    let keywords = request
        .description
        .keywords
        .iter()
        .take(24)
        .map(|word| crate::symbol_query::preview(word, 80))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    // Replace an annotation with an obsolete line-based ID when its qualified identity is unique.
    if prepared.unambiguous {
        tx.execute("DELETE FROM symbol_business_contexts WHERE workspace_root=?1 AND language=?2 AND file_path=?3 AND qualified_name=?4 AND scope=?5 AND symbol_id<>?6",params![request.workspace_root,prepared.symbol.language,prepared.symbol.file_path,prepared.symbol.qualified_name,prepared.scope,prepared.symbol.id])?;
    }
    tx.execute("INSERT INTO symbol_business_contexts(workspace_root,symbol_id,symbol_name,language,file_path,belongs_to_area,business_role,common_tasks,read_when,avoid_when,risks,confidence,updated_at_unix,qualified_name,keywords,scope,source,code_hash,stale)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
        ON CONFLICT(workspace_root,symbol_id) DO UPDATE SET symbol_name=excluded.symbol_name,language=excluded.language,file_path=excluded.file_path,belongs_to_area=excluded.belongs_to_area,business_role=excluded.business_role,common_tasks=excluded.common_tasks,read_when=excluded.read_when,avoid_when=excluded.avoid_when,risks=excluded.risks,confidence=excluded.confidence,updated_at_unix=excluded.updated_at_unix,qualified_name=excluded.qualified_name,keywords=excluded.keywords,scope=excluded.scope,source=excluded.source,code_hash=excluded.code_hash,stale=excluded.stale",
        params![request.workspace_root,prepared.symbol.id,prepared.symbol.name,prepared.symbol.language,prepared.symbol.file_path,request.belongs_to_area,crate::symbol_query::preview(&request.business_role,1200),serde_json::to_string(&request.common_tasks)?,request.read_when,request.avoid_when,request.risks,request.confidence.clamp(0.0,1.0),now as i64,prepared.symbol.qualified_name,serde_json::to_string(&keywords)?,prepared.scope,prepared.source,prepared.hash,prepared.stale])?;
    tx.execute("UPDATE symbol_business_contexts SET definition_hash=?3 WHERE workspace_root=?1 AND symbol_id=?2",params![request.workspace_root,prepared.symbol.id,prepared.definition_hash])?;
    tx.commit()?;
    let recorded =
        fetch_symbol_business_context(&conn, &request.workspace_root, &prepared.symbol.id)?;
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
    crate::symbol_description::refresh_recorded_sources(workspace_root_path)?;
    let conn = init_db(workspace_root_path)?;
    let mut contexts = Vec::new();

    if request.belongs_to_area.trim().is_empty() {
        let mut stmt = conn.prepare(
            "SELECT updated_at_unix, workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence, qualified_name, keywords, scope, source, code_hash, stale, definition_hash
             FROM symbol_business_contexts
             WHERE workspace_root = ?
             ORDER BY updated_at_unix DESC
             LIMIT ?",
        )?;
        let rows = stmt.query_map(
            params![request.workspace_root, request.limit.clamp(1, 100) as i64],
            |row| symbol_business_context_from_row(row),
        )?;
        for row in rows {
            contexts.push(row?);
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT updated_at_unix, workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence, qualified_name, keywords, scope, source, code_hash, stale, definition_hash
             FROM symbol_business_contexts
             WHERE workspace_root = ? AND belongs_to_area = ?
             ORDER BY updated_at_unix DESC
             LIMIT ?",
        )?;
        let rows = stmt.query_map(
            params![
                request.workspace_root,
                request.belongs_to_area,
                request.limit.clamp(1, 100) as i64
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
    crate::symbol_description::refresh_recorded_sources(workspace_root_path)?;
    let conn = init_db(workspace_root_path)?;
    let needle = format!("%{}%", request.query.to_lowercase());
    let mut stmt = conn.prepare(
        "SELECT updated_at_unix, workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence, qualified_name, keywords, scope, source, code_hash, stale, definition_hash
         FROM symbol_business_contexts
         WHERE workspace_root = ?
           AND (
             LOWER(qualified_name) LIKE ? OR
             LOWER(keywords) LIKE ? OR
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
         ORDER BY stale ASC, updated_at_unix DESC
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
            needle,
            needle,
            request.limit.clamp(1, 100) as i64,
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
        "SELECT updated_at_unix, workspace_root, symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence, qualified_name, keywords, scope, source, code_hash, stale, definition_hash
         FROM symbol_business_contexts
         WHERE workspace_root = ? AND symbol_id = ?",
        params![workspace_root, symbol_id],
        |row| symbol_business_context_from_row(row),
    )?)
}

pub(crate) fn symbol_business_context_from_row(
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
        qualified_name: row.get(13)?,
        keywords: serde_json::from_str(&row.get::<_, String>(14)?).unwrap_or_default(),
        scope: row.get(15)?,
        source: row.get(16)?,
        code_hash: row.get(17)?,
        stale: row.get(18)?,
        definition_hash: row.get(19)?,
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
    fn observer_lessons_deduplicate_and_search_excludes_replaced_source_versions() {
        let ws=temp_workspace("observer_versions");let file=ws.join("source.rs");fs::write(&file,"old source").unwrap();
        let make=|summary:&str,implementation:&str,refs:Vec<String>|RecordWorkMemoryRequest {
            workspace_root:ws.to_string_lossy().into_owned(),summary:summary.into(),implementation:implementation.into(),
            files_changed:vec![],tests:String::new(),risks:String::new(),source_task_id:Some("one-task-many-nodes".into()),
            kind:"observer".into(),applies_to:"mapping tasks".into(),source_refs:refs,
        };
        let first=record(&ws,make("Mapping fact","old mapping",vec!["source.rs".into()])).unwrap();
        record(&ws,make("Mapping fact","old mapping",vec!["source.rs".into()])).unwrap();
        bind_observer_sources(&ws,&first.recorded.summary,&first.recorded.implementation,&serde_json::json!([{"file_path":"source.rs","code_hash":crate::symbol_description::content_hash(b"old source")}])).unwrap();
        let route=record(&ws,make("Mapping route","start with the known source mapping",vec![])).unwrap();
        bind_observer_sources(&ws,&route.recorded.summary,&route.recorded.implementation,&serde_json::json!([])).unwrap();
        assert_eq!(list(&ws,ListWorkMemoryRequest{workspace_root:ws.to_string_lossy().into_owned(),limit:10}).unwrap().memories.len(),2);
        fs::write(&file,"new source").unwrap();
        let new=record(&ws,make("Mapping fact","new mapping",vec!["source.rs".into()])).unwrap();
        bind_observer_sources(&ws,&new.recorded.summary,&new.recorded.implementation,&serde_json::json!([{"file_path":"source.rs","code_hash":crate::symbol_description::content_hash(b"new source")}])).unwrap();
        let current=search(&ws,SearchWorkMemoryRequest{workspace_root:ws.to_string_lossy().into_owned(),query:"mapping".into(),limit:10}).unwrap();
        assert_eq!(current.matches.len(),2);assert!(!current.matches.iter().any(|m|m.implementation=="old mapping"));
        assert_eq!(list(&ws,ListWorkMemoryRequest{workspace_root:ws.to_string_lossy().into_owned(),limit:10}).unwrap().memories.len(),3);
        fs::remove_dir_all(ws).unwrap();
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
                source_task_id: None,
                kind: String::new(),
                applies_to: String::new(),
                source_refs: Vec::new(),
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
                description: Default::default(),
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
