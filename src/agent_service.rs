//! Standalone task API for the local Rust agent. Events are append-only so a
//! client can recover a task's trajectory after reconnecting.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures::{StreamExt, stream};
use reqwest::Client;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_STEPS: usize = 100;
const MAX_STEP_LIMIT: usize = 100;
// Acceptance may include history, code and image reads before its return.
// Use the same production window as an Organizer decision, not the former
// short summary-only window. A deadline never implies a business defect.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    ReadOnly,
    WorkspaceWrite,
    /// Edits proceed; execution waits for the user's approval (tool_approvals).
    Ask,
    #[default]
    FullAccess,
}

impl PermissionMode {
    pub(crate) fn allows(self, name: &str) -> bool {
        match name {
            "write_file" | "replace_range" | "edit_file" => !matches!(self, Self::ReadOnly),
            // Process execution uses the host account without a filesystem
            // sandbox. Only full access may invoke it; structured writes retain path checks.
            "run_program"
            | "install_dependencies"
            | "run_project_script"
            | "stop_project_process"
            | "browser_open"
            | "browser_read"
            | "browser_wait"
            | "browser_diagnostics"
            | "browser_click"
            | "browser_press_key"
            | "browser_upload"
            | "browser_screenshot" => matches!(self, Self::FullAccess | Self::Ask),
            "browser_close" => matches!(self, Self::FullAccess | Self::Ask),
            _ => true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentProviderRoute {
    pub prefix_system_messages: bool,
    pub url: String,
    pub api_key: String,
    pub models: HashMap<String, String>,
    /// Output limits declared per upstream model id (`maxTokens`).
    pub max_output_tokens: HashMap<String, usize>,
    /// Total input/output limits declared by the model catalog (`contextWindow`).
    pub context_windows: HashMap<String, usize>,
}

#[derive(Clone)]
pub struct AgentServiceState {
    pub workspace: Arc<crate::tools::Workspace>,
    pub model_client: Client,
    pub provider_url: String,
    pub api_key: String,
    pub provider_name: String,
    pub provider_routes: HashMap<String, AgentProviderRoute>,
    pub default_model: String,
    pub model_map: HashMap<String, String>,
    pub reasoning_effort: Option<String>,
    pub fast_mode: bool,
    pub permission_mode: PermissionMode,

    pub visual: crate::visual_artifacts::VisualSettings,
    pub visual_tool_settings: crate::model_settings::ModelSettings,
    /// Model for task delegates; all-empty means "same as the conversation".
    pub delegate_agent_settings: crate::model_settings::ModelSettings,
    pub config: Option<Arc<tokio::sync::RwLock<crate::ai_proxy::AiProxyConfig>>>,
    pub config_path: Option<std::path::PathBuf>,
    pub tool_catalog: Option<Arc<crate::builtin_plugins::ToolCatalog>>,
    pub plugin_cancel: CancellationToken,
    pub cancellations: Arc<Mutex<HashMap<String, ActiveTask>>>,
}

#[derive(Clone)]
pub struct ActiveTask {
    run_id: String,
    token: CancellationToken,
    flow_node: Arc<Mutex<String>>,
    node_interrupt: Arc<Mutex<Option<String>>>,
}

async fn clear_active_task(state: &AgentServiceState, task_id: &str, run_id: &str) {
    let mut active = state.cancellations.lock().await;
    if active
        .get(task_id)
        .is_some_and(|entry| entry.run_id == run_id)
    {
        active.remove(task_id);
    }
}

pub(crate) async fn set_active_flow_node(
    state: &AgentServiceState,
    task_id: &str,
    turn: usize,
    node: &str,
) {
    let active = state.cancellations.lock().await.get(task_id).cloned();
    if let Some(active) = active {
        *active.flow_node.lock().await = format!("turn_{turn}:{node}");
    }
}

impl AgentServiceState {
    pub async fn with_latest_config(mut self) -> Self {
        if let Some(shared) = self.config.clone() {
            if let Some(path) = self.config_path.clone() {
                if let Ok(Ok(latest)) =
                    tokio::task::spawn_blocking(move || crate::ai_proxy::load_config(&path)).await
                {
                    *shared.write().await = latest;
                }
            }
            let config = shared.read().await;
            crate::ai_proxy::apply_agent_config(&mut self, &config);
        }
        self
    }

    fn use_provider(&mut self, name: &str) -> bool {
        if self.provider_routes.is_empty() {
            return name == self.provider_name;
        }
        let Some(route) = self.provider_routes.get(name).cloned() else {
            return false;
        };
        self.provider_name = name.to_owned();
        self.provider_url = route.url.clone();
        self.api_key = route.api_key.clone();
        self.model_map = route.models;

        true
    }
}

fn task_provider_name(
    routes: &HashMap<String, AgentProviderRoute>,
    default_name: &str,
    saved_name: Option<&str>,
    model: &str,
) -> Option<String> {
    if routes.is_empty() {
        return Some(default_name.to_owned());
    }
    if let Some(name) = saved_name.filter(|name| !name.is_empty()) {
        return routes
            .get(name)
            .filter(|route| route.models.contains_key(model))
            .map(|_| name.to_owned());
    }
    if routes
        .get(default_name)
        .is_some_and(|route| route.models.contains_key(model))
    {
        return Some(default_name.to_owned());
    }
    let mut matches = routes
        .iter()
        .filter(|(_, route)| route.models.contains_key(model));
    let name = matches.next()?.0.clone();
    matches.next().is_none().then_some(name)
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_steps: Option<usize>,
    #[serde(default)]
    pub permission_mode: PermissionMode,
}

/// A browser session may exist before its first prompt. Its ID is also the
/// durable Rust task ID, so reconnects do not need a client-side ID map.
#[derive(Default, Deserialize)]
pub struct DraftSessionInput {
    pub workspace: Option<String>,
    pub kind: Option<String>,
}
pub async fn create_draft_task(
    State(state): State<AgentServiceState>,
    input: Option<Json<DraftSessionInput>>,
) -> impl IntoResponse {
    let state = state.with_latest_config().await;
    if state.plugin_cancel.is_cancelled() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"agent task plugin is stopped"})),
        )
            .into_response();
    }
    let task_id = format!("task_{}", uuid_like());
    let root = state.workspace.root().to_path_buf();
    let input = input.map(|Json(input)| input).unwrap_or_default();
    let kind = if input.kind.as_deref() == Some("chat") {
        "chat"
    } else {
        "code"
    };
    let selected_workspace = match input.workspace {
        Some(path) => match std::fs::canonicalize(path) {
            Ok(path) if path.is_dir() => path,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"项目目录不存在"})),
                )
                    .into_response();
            }
        },
        None => root.clone(),
    };
    let model = state.default_model.clone();
    let provider_name = state.provider_name.clone();
    let reasoning_effort = state.reasoning_effort.clone();
    let fast_mode = state.fast_mode;
    let id = task_id.clone();
    let inserted = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut conn = open_db(&root)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at,provider_name,reasoning_effort,fast_mode) VALUES (?1,'',?2,'draft',?3,?3,?4,?5,?6)",
            params![id, model, now(), provider_name, reasoning_effort, fast_mode],
        )?;
        tx.execute("INSERT INTO agent_session_meta(task_id,workspace,kind) VALUES (?1,?2,?3)",params![id,selected_workspace.to_string_lossy(),kind])?;
        tx.commit()?;
        Ok(())
    }).await;
    match inserted {
        Ok(Ok(())) => (StatusCode::CREATED, Json(json!({"task_id":task_id}))).into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not create session"})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ContinueTaskRequest {
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default, rename = "requestId")]
    pub request_id: Option<String>,
    #[serde(default)]
    pub max_steps: Option<usize>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub fast_mode: Option<bool>,
    #[serde(default)]
    pub permission_mode: Option<PermissionMode>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTaskRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<Option<String>>,
    #[serde(default)]
    pub fast_mode: Option<bool>,
}

enum ResumeLoad {
    InvalidAttachment(String),
    AlreadyAccepted,
    Ready {
        model: String,
        provider_name: String,
        reasoning_effort: Option<String>,
        fast_mode: bool,
        permission_mode: PermissionMode,
        turn: usize,
    },
    NotFound,
    ChildTask,
    Busy,
    ModelUnavailable,
}

#[derive(Debug, Deserialize)]
pub struct ListTasksQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct StreamEventsQuery {
    #[serde(default)]
    pub since: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentEvent {
    pub seq: i64,
    pub time: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
    #[serde(rename = "surfaceOp", skip_serializing_if = "Option::is_none")]
    pub surface_op: Option<String>,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(crate) fn open_db(root: &std::path::Path) -> anyhow::Result<rusqlite::Connection> {
    let conn = crate::database::init_db(root)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_tasks (
            id TEXT PRIMARY KEY, prompt TEXT NOT NULL, model TEXT NOT NULL,
            status TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
            parent_task_id TEXT, provider_name TEXT, reasoning_effort TEXT,
            fast_mode INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS agent_task_events (
            task_id TEXT NOT NULL, seq INTEGER NOT NULL, timestamp INTEGER NOT NULL,
            kind TEXT NOT NULL, data TEXT NOT NULL, surface_op TEXT, PRIMARY KEY(task_id, seq),
            FOREIGN KEY(task_id) REFERENCES agent_tasks(id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS plugin_sessions (
            task_id TEXT PRIMARY KEY REFERENCES agent_tasks(id) ON DELETE CASCADE,
            plugin_id TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS agent_observations (
            task_id TEXT NOT NULL, review_id TEXT NOT NULL, request_id INTEGER NOT NULL,
            input TEXT NOT NULL, status TEXT NOT NULL, result TEXT,
            PRIMARY KEY(task_id,review_id), FOREIGN KEY(task_id) REFERENCES agent_tasks(id) ON DELETE CASCADE
         );",
    )?;
    let _ = conn.execute(
        "ALTER TABLE agent_task_events ADD COLUMN surface_op TEXT",
        [],
    );
    let has_node: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_task_events') WHERE name='node_id')",
        [],
        |r| r.get(0),
    )?;
    if !has_node {
        let tx =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        if !tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_task_events') WHERE name='node_id')", [], |r|r.get::<_,bool>(0))? {
            tx.execute("ALTER TABLE agent_task_events ADD COLUMN node_id TEXT", [])?;
        }
        tx.commit()?;
    }
    crate::task_tree::schema(&conn)?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS task_events_node ON agent_task_events(task_id,node_id,seq);",
    )?;
    let _ = conn.execute("ALTER TABLE agent_tasks ADD COLUMN parent_task_id TEXT", []);
    let _ = conn.execute("ALTER TABLE agent_tasks ADD COLUMN provider_name TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE agent_tasks ADD COLUMN reasoning_effort TEXT",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE agent_tasks ADD COLUMN fast_mode INTEGER NOT NULL DEFAULT 0",
        [],
    );
    let has_high_water: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_tasks') WHERE name='event_high_water')",
        [], |row| row.get(0),
    )?;
    if !has_high_water {
        let tx =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        let migrated: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_tasks') WHERE name='event_high_water')", [], |row| row.get(0))?;
        if !migrated {
            tx.execute(
                "ALTER TABLE agent_tasks ADD COLUMN event_high_water INTEGER NOT NULL DEFAULT -1",
                [],
            )?;
            tx.execute("UPDATE agent_tasks SET event_high_water=COALESCE((SELECT MAX(seq) FROM agent_task_events WHERE task_id=agent_tasks.id),-1)", [])?;
        }
        tx.commit()?;
    }
    crate::session_browser::schema(&conn, root)?;
    Ok(conn)
}

pub async fn recover_orphaned_tasks(workspace_root: &std::path::Path) -> anyhow::Result<()> {
    let root = workspace_root.to_path_buf();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut conn = open_db(&root)?;
        let ids = {
            let mut stmt = conn.prepare("SELECT id FROM agent_tasks WHERE status='running'")?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for task_id in ids {
            crate::request_context::recover_interrupted(&mut conn, &task_id)?;
            if let Some(event) = crate::session_history::load_events(
                &conn,
                &task_id,
                &crate::session_history::EventQuery {
                    kinds: &["step/start", "step/end"],
                    newest: Some(1),
                    ..Default::default()
                },
            )?
            .pop()
                && event.kind == "step/start"
            {
                let step_data = event.data()?;
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                append_event_tx(&tx, &task_id, "step/end", &step_data, None)?;
                tx.commit()?;
            }
            let turn = crate::session_history::load_events(
                &conn,
                &task_id,
                &crate::session_history::EventQuery {
                    kinds: &["turn/start"],
                    newest: Some(1),
                    ..Default::default()
                },
            )?
            .pop()
            .map(|event| event.data())
            .transpose()?
            .and_then(|data| data["turn"].as_u64())
            .unwrap_or(1) as usize;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            append_event_tx(
                &tx,
                &task_id,
                "turn/end",
                &json!({"turn":turn,"reason":{"kind":"interrupted"}}),
                None,
            )?;
            tx.execute(
                "UPDATE agent_tasks SET status='interrupted',updated_at=?2 WHERE id=?1",
                params![task_id, now()],
            )?;
            tx.commit()?;
        }
        cleanup_terminal_task_data(&mut conn, None, true)?;
        Ok(())
    })
    .await??;
    Ok(())
}

/// Replace repeated execution snapshots with one resumable Flow record after a run ends.
/// The execution history (tools, progress, Flow and messages), task metadata,
/// Observer conclusions, memories and code indexes are retained.
fn cleanup_terminal_task_data(
    conn: &mut rusqlite::Connection,
    task_id: Option<&str>,
    compact: bool,
) -> anyhow::Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let task_filter = "status IN ('completed','failed','cancelled','max_steps','interrupted') AND (?1 IS NULL OR id=?1)";
    if let Some(task_id) = task_id {
        let status: Option<String> = tx
            .query_row(
                "SELECT status FROM agent_tasks WHERE id=?1",
                [task_id],
                |r| r.get(0),
            )
            .optional()?;
        if !status.as_deref().is_some_and(|s| {
            matches!(
                s,
                "completed" | "failed" | "cancelled" | "max_steps" | "interrupted"
            )
        }) {
            return Ok(());
        }
    }
    let task_selector = format!("SELECT id FROM agent_tasks WHERE {task_filter}");
    // One continuation record replaces the repeated execution snapshots. Keep
    // the exact current frame so a paused node can resume after cleanup.
    let sessions = {
        let mut stmt=tx.prepare(&format!("SELECT task_id,data FROM agent_task_events e WHERE task_id IN ({task_selector}) AND kind='execution/commit' AND seq=(SELECT MAX(seq) FROM agent_task_events WHERE task_id=e.task_id AND kind='execution/commit')"))?;
        let rows = stmt.query_map([task_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, raw) in sessions {
        let mut data: Value = serde_json::from_str(&raw)?;
        let worker=tx.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='worker/work_state' ORDER BY seq DESC LIMIT 1",
            [&id],|row|row.get::<_,String>(0)).optional()?;
        if let Some(state) = worker
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|data| data.get("state").cloned())
        {
            data["worker_state"] = state;
        }
        tx.execute(
            "DELETE FROM agent_task_events WHERE task_id=?1 AND kind='flow/session'",
            [&id],
        )?;
        append_event_tx(&tx, &id, "flow/session", &data, None)?;
    }
    let sql = format!(
        "DELETE FROM agent_task_events WHERE task_id IN ({task_selector}) AND (
            kind IN ('scheduler/state','worker/work_state',
                     'worker/source_working_set','assistant/delta','execution/tick',
                     'organizer/request_metrics')
            OR kind LIKE 'execution/%'
        )"
    );
    tx.execute(&sql, [task_id])?;

    // Actual role inputs and their conversation references are durable audit
    // records. Completion and startup recovery must not erase them.
    tx.commit()?;

    if compact {
        let active_tasks: i64 = conn.query_row(
            "SELECT COUNT(*) FROM agent_tasks WHERE status='running'",
            [],
            |r| r.get(0),
        )?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let free_pages: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        // Avoid blocking concurrent work. A one-time compaction after startup or
        // a large purge returns the reclaimed pages to the filesystem.
        if active_tasks == 0 && free_pages.saturating_mul(page_size) >= 16 * 1024 * 1024 {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }
    }
    Ok(())
}

async fn cleanup_finished_task_data(root: &std::path::Path, task_id: &str) -> anyhow::Result<()> {
    let root = root.to_path_buf();
    let task_id = task_id.to_owned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut conn = open_db(&root)?;
        cleanup_terminal_task_data(&mut conn, Some(&task_id), true)
    })
    .await??;
    Ok(())
}

fn append_event(
    root: &std::path::Path,
    task_id: &str,
    kind: &str,
    data: Value,
    surface_op: Option<&str>,
) -> anyhow::Result<()> {
    let (task_id, kind, surface_op) = (
        task_id.to_owned(),
        kind.to_owned(),
        surface_op.map(str::to_owned),
    );
    crate::db_writer::write(root, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        append_event_tx(&tx, &task_id, &kind, &data, surface_op.as_deref())?;
        tx.commit()?;
        Ok(())
    })
}

/// Preview the next identity inside the same write transaction that appends it.
pub(crate) fn next_event_seq(tx: &rusqlite::Transaction, task_id: &str) -> anyhow::Result<i64> {
    Ok(tx.query_row(
        "SELECT MAX(event_high_water,COALESCE((SELECT MAX(seq) FROM agent_task_events WHERE task_id=?1),-1))+1 FROM agent_tasks WHERE id=?1",
        [task_id], |row| row.get(0),
    )?)
}

pub(crate) fn append_event_tx(
    tx: &rusqlite::Transaction,
    task_id: &str,
    kind: &str,
    data: &Value,
    surface_op: Option<&str>,
) -> anyhow::Result<i64> {
    let seq = next_event_seq(tx, task_id)?;
    let node = if let Some(id) = data["task_node_id"].as_str() {
        Some(id.to_owned())
    } else if let Some(agent) = data["agent_id"].as_str() {
        tx.query_row("SELECT node_id FROM agent_task_nodes WHERE task_id=?1 AND json_extract(data,'$.agent_id')=?2", params![task_id,agent], |r|r.get::<_,String>(0)).optional()?
    } else {
        crate::task_tree::active(tx, task_id)?.map(|n| n.node_id)
    };
    // Work done inside a task belongs to the step it is executing. Task graph
    // events name their own node.
    let node = match node {
        Some(id) if !kind.starts_with("task/") => tx
            .query_row(
                "SELECT node_id FROM agent_task_nodes WHERE task_id=?1 AND parent_id=?2 AND status='current' AND json_extract(data,'$.kind')='step' LIMIT 1",
                params![task_id, id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .or(Some(id)),
        other => other,
    };
    tx.execute(
        "INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data,surface_op,node_id) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![task_id, seq, now(), kind, serde_json::to_string(&data)?, surface_op,node],
    )?;
    tx.execute(
        "UPDATE agent_tasks SET updated_at=?2,event_high_water=?3 WHERE id=?1",
        params![task_id, now(), seq],
    )?;
    Ok(seq)
}

pub async fn create_task(
    State(state): State<AgentServiceState>,
    Json(request): Json<CreateTaskRequest>,
) -> impl IntoResponse {
    let mut state = state.with_latest_config().await;
    state.permission_mode = request.permission_mode;
    if state.plugin_cancel.is_cancelled() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"agent task plugin is stopped"})),
        )
            .into_response();
    }
    let prompt = request.prompt.trim().to_owned();
    if prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"prompt must not be empty"})),
        )
            .into_response();
    }
    if state.provider_url.is_empty() || state.default_model.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"agent model provider is not configured"})),
        )
            .into_response();
    }
    let model = match request.model {
        Some(requested) => match state.model_map.get(&requested).cloned().or_else(|| {
            state
                .model_map
                .values()
                .any(|value| value == &requested)
                .then_some(requested)
        }) {
            Some(model) => model,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"model is not configured"})),
                )
                    .into_response();
            }
        },
        None => state.default_model.clone(),
    };
    let task_id = format!("task_{}", uuid_like());
    let root = state.workspace.root().to_path_buf();
    let provider_name = state.provider_name.clone();
    let reasoning_effort = state.reasoning_effort.clone();
    let fast_mode = state.fast_mode;
    let insert = tokio::task::spawn_blocking({
        let task_id = task_id.clone(); let prompt = prompt.clone(); let model = model.clone();
        let root = root.clone(); move || -> anyhow::Result<()> {
            let conn = open_db(&root)?;
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at,provider_name,reasoning_effort,fast_mode) VALUES (?1,?2,?3,'running',?4,?4,?5,?6,?7)", params![task_id,prompt,model,now(),provider_name,reasoning_effort,fast_mode])?;
            Ok(())
        }
    }).await;
    if !matches!(insert, Ok(Ok(()))) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not create task"})),
        )
            .into_response();
    }
    let cancel = state.plugin_cancel.child_token();
    let run_id = uuid_like();
    state.cancellations.lock().await.insert(
        task_id.clone(),
        ActiveTask {
            run_id: run_id.clone(),
            token: cancel.clone(),
            flow_node: Arc::new(Mutex::new("turn_1:1".into())),
            node_interrupt: Arc::new(Mutex::new(None)),
        },
    );
    let runner_state = state.clone();
    let runner_id = task_id.clone();
    let max_steps = request
        .max_steps
        .unwrap_or(DEFAULT_MAX_STEPS)
        .clamp(1, MAX_STEP_LIMIT);
    spawn_supervised_task(
        runner_state,
        runner_id,
        run_id,
        model,
        prompt,
        max_steps,
        cancel,
        1,
        false,
    );
    (StatusCode::ACCEPTED, Json(json!({"task_id":task_id,"status":"running","events_url":format!("/agent/tasks/{task_id}/events")}))).into_response()
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    q: Option<String>,
}

pub async fn workspace_files(
    State(state): State<AgentServiceState>,
    Query(query): Query<FileQuery>,
) -> impl IntoResponse {
    let root = state.workspace.root().to_path_buf();
    let q = query.q.unwrap_or_default();
    let files =
        tokio::task::spawn_blocking(move || crate::composer_catalog::search_files(&root, &q))
            .await
            .unwrap_or_default();
    Json(json!({"files": files})).into_response()
}

pub async fn slash_commands(State(state): State<AgentServiceState>) -> impl IntoResponse {
    let root = state.workspace.root().to_path_buf();
    let mut commands = serde_json::to_value(
        tokio::task::spawn_blocking(move || crate::composer_catalog::slash_commands(&root))
            .await
            .unwrap_or_default(),
    )
    .unwrap_or(json!([]));
    if let Some(catalog) = state.tool_catalog {
        for skill in catalog.packages.skills().as_array().into_iter().flatten() {
            if !commands
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command["name"] == skill["name"])
            {
                commands.as_array_mut().unwrap().push(skill.clone());
            }
        }
    }
    Json(json!({"commands": commands})).into_response()
}

pub async fn continue_task(
    State(state): State<AgentServiceState>,
    Path(task_id): Path<String>,
    Json(mut request): Json<ContinueTaskRequest>,
) -> impl IntoResponse {
    let mut state = state.with_latest_config().await;
    if let Err(error) = crate::model_catalog::prepare_turn(&mut state, &task_id, &mut request).await
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response();
    }
    if state.plugin_cancel.is_cancelled() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"agent task plugin is stopped"})),
        )
            .into_response();
    }
    let prompt = if request.prompt.trim().is_empty() {
        request
            .content
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        request.prompt.trim().to_owned()
    };
    if prompt.is_empty() && request.content.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"prompt must not be empty"})),
        )
            .into_response();
    }
    if state.provider_url.is_empty() || state.default_model.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"agent model provider is not configured"})),
        )
            .into_response();
    }
    let root = state.workspace.root().to_path_buf();
    let read_id = task_id.clone();
    let default_model = state.default_model.clone();
    let default_provider = state.provider_name.clone();
    let default_effort = state.reasoning_effort.clone();
    let default_fast_mode = state.fast_mode;
    let routes = state.provider_routes.clone();
    let title = if prompt.is_empty() {
        "附件消息".to_owned()
    } else {
        prompt.clone()
    };
    let attachment_content = request.content.clone();
    let attachment_prompt = prompt.clone();
    let attachment_request_id = request.request_id.clone();
    let attachment_policy = match crate::attachments::policy(&state, &task_id) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":e.to_string()})),
            )
                .into_response();
        }
    };
    let req_model = request.model.clone();
    let req_provider = request.provider.clone();
    let req_effort = request.reasoning_effort.clone();
    let req_fast = request.fast_mode;
    let req_permission = request.permission_mode;
    let loaded = tokio::task::spawn_blocking(move || -> anyhow::Result<ResumeLoad> {
        let mut conn = open_db(&root)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(request_id)=&attachment_request_id {
            match crate::attachments::already(&tx,&read_id,request_id,&attachment_prompt,&attachment_content) {
                Ok(true)=>return Ok(ResumeLoad::AlreadyAccepted), Ok(false)=>{},
                Err(e)=>return Ok(ResumeLoad::InvalidAttachment(e.to_string())),
            }
        }
        let task: Option<(String, String, Option<String>, Option<String>, Option<String>, i64)> = tx.query_row(
            "SELECT model,status,parent_task_id,provider_name,reasoning_effort,fast_mode FROM agent_tasks WHERE id=?1", [&read_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).optional()?;
        let Some((model, status, parent_id, saved_provider, saved_effort, saved_fast_mode)) = task else { return Ok(ResumeLoad::NotFound); };
        if parent_id.is_some() { return Ok(ResumeLoad::ChildTask); }
        if !matches!(status.as_str(), "draft" | "completed" | "failed" | "cancelled" | "max_steps" | "interrupted") {
            return Ok(ResumeLoad::Busy);
        }
        let events = crate::session_history::load_events(&tx, &read_id, &Default::default())?
            .into_iter().map(|event| {
                let mut data = event.data()?;
                data["_replay_source"] = crate::session_history::event_source(&read_id,event.seq,event.time,&event.kind,&data);
                Ok((event.kind,data))
            }).collect::<anyhow::Result<Vec<_>>>()?;
        // The single-agent runtime restores its durable window itself, including
        // importing legacy records without requiring legacy native tool pairs.
        let turn = events.iter().filter_map(|(_,data)|data["turn"].as_u64()).max().unwrap_or(0) as usize + 1;
        let permission_mode = req_permission.unwrap_or_else(|| events.iter().rev()
            .find(|(kind, _)| kind == "turn/start")
            .and_then(|(_, data)| data.get("permission_mode"))
            .and_then(|mode| serde_json::from_value(mode.clone()).ok())
            .unwrap_or_default());
        let (model, provider_name, reasoning_effort, fast_mode) = if status == "draft" {
            let m = req_model.unwrap_or(default_model);
            let p = req_provider.or(saved_provider).unwrap_or(default_provider);
            let e = req_effort.or(saved_effort).or(default_effort);
            let f = req_fast.unwrap_or_else(|| saved_fast_mode != 0 || default_fast_mode);
            (m, p, e, f)
        } else {
            let target_model = req_model.unwrap_or(model);
            let target_provider = req_provider.or(saved_provider);
            let Some(provider_name) = task_provider_name(&routes, &default_provider, target_provider.as_deref(), &target_model)
                else { return Ok(ResumeLoad::ModelUnavailable); };
            let reasoning_effort = req_effort.or(saved_effort);
            let fast_mode = req_fast.unwrap_or(saved_fast_mode != 0);
            (target_model, provider_name, reasoning_effort, fast_mode)
        };
        let reasoning_effort=reasoning_effort.filter(|e|!e.is_empty());
        if let Err(e)=crate::attachments::admit(&tx,&root,&read_id,turn,&attachment_prompt,&attachment_content,attachment_request_id.as_deref(),attachment_policy.as_deref()) {
            return Ok(ResumeLoad::InvalidAttachment(e.to_string()));
        }
        let changed = tx.execute(
            "UPDATE agent_tasks SET model=?2,status='running',updated_at=?3,prompt=CASE WHEN prompt='' THEN ?5 ELSE prompt END,provider_name=?6,reasoning_effort=?7,fast_mode=?8 WHERE id=?1 AND status=?4",
            params![read_id, model, now(), status, title, provider_name, reasoning_effort, fast_mode],
        )?;
        if changed == 0 { return Ok(ResumeLoad::Busy); }
        tx.commit()?;
        Ok(ResumeLoad::Ready { model, provider_name, reasoning_effort, fast_mode, permission_mode, turn })
    }).await;
    let (model, provider_name, reasoning_effort, fast_mode, permission_mode, turn) = match loaded {
        Ok(Ok(ResumeLoad::AlreadyAccepted)) => {
            return (
                StatusCode::ACCEPTED,
                Json(json!({"task_id":task_id,"already_accepted":true})),
            )
                .into_response();
        }
        Ok(Ok(ResumeLoad::InvalidAttachment(error))) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response();
        }
        Ok(Ok(ResumeLoad::Ready {
            model,
            provider_name,
            reasoning_effort,
            fast_mode,
            permission_mode,
            turn,
        })) => (
            model,
            provider_name,
            reasoning_effort,
            fast_mode,
            permission_mode,
            turn,
        ),
        Ok(Ok(ResumeLoad::NotFound)) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"task not found"})),
            )
                .into_response();
        }
        Ok(Ok(ResumeLoad::ChildTask)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"subagent tasks cannot receive follow-up messages"})),
            )
                .into_response();
        }
        Ok(Ok(ResumeLoad::Busy)) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error":"task is still running"})),
            )
                .into_response();
        }
        Ok(Ok(ResumeLoad::ModelUnavailable)) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error":"task model is no longer configured"})),
            )
                .into_response();
        }
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"could not load task history"})),
            )
                .into_response();
        }
    };
    if !state.use_provider(&provider_name) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"task provider is no longer configured"})),
        )
            .into_response();
    }
    state.reasoning_effort = reasoning_effort;
    state.fast_mode = fast_mode;
    state.permission_mode = permission_mode;
    let cancel = state.plugin_cancel.child_token();
    let run_id = uuid_like();
    state.cancellations.lock().await.insert(
        task_id.clone(),
        ActiveTask {
            run_id: run_id.clone(),
            token: cancel.clone(),
            flow_node: Arc::new(Mutex::new(format!("turn_{turn}:1"))),
            node_interrupt: Arc::new(Mutex::new(None)),
        },
    );
    let runner_state = state.clone();
    let runner_id = task_id.clone();
    let max_steps = request
        .max_steps
        .unwrap_or(DEFAULT_MAX_STEPS)
        .clamp(1, MAX_STEP_LIMIT);
    spawn_supervised_task(
        runner_state,
        runner_id,
        run_id,
        model,
        prompt,
        max_steps,
        cancel,
        turn,
        false,
    );
    (
        StatusCode::ACCEPTED,
        Json(json!({"task_id":task_id,"status":"running","turn":turn})),
    )
        .into_response()
}

pub async fn get_events(
    State(state): State<AgentServiceState>,
    Path(task_id): Path<String>,
) -> impl IntoResponse {
    let root = state.workspace.root().to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<AgentEvent>> {
        let conn = open_db(&root)?;
        crate::session_history::load_events(&conn, &task_id, &Default::default())?
            .into_iter()
            .map(|event| {
                Ok(AgentEvent {
                    seq: event.seq,
                    time: event.time,
                    data: event.data()?,
                    kind: event.kind,
                    surface_op: event.surface_op,
                })
            })
            .collect()
    })
    .await;
    match result {
        Ok(Ok(events)) => Json(json!({"events":events})).into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not read task events"})),
        )
            .into_response(),
    }
}

pub async fn stream_events(
    State(state): State<AgentServiceState>,
    Path(task_id): Path<String>,
    Query(query): Query<StreamEventsQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|seq| *seq >= 0)
        .unwrap_or(-1)
        .max(query.since.unwrap_or(-1));
    let events = stream::unfold(
        (state, task_id, last_event_id),
        |(state, task_id, mut last_seq)| async move {
            loop {
                let root = state.workspace.root().to_path_buf();
                let read_id = task_id.clone();
                let read = tokio::task::spawn_blocking(
                    move || -> anyhow::Result<(Vec<AgentEvent>, Option<String>)> {
                        let conn = open_db(&root)?;
                        let events = crate::session_history::load_events(
                            &conn,
                            &read_id,
                            &crate::session_history::EventQuery {
                                after: Some(last_seq),
                                ..Default::default()
                            },
                        )?
                        .into_iter()
                        .map(|event| {
                            Ok(AgentEvent {
                                seq: event.seq,
                                time: event.time,
                                data: event.data()?,
                                kind: event.kind,
                                surface_op: event.surface_op,
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;
                        let status = conn
                            .query_row(
                                "SELECT status FROM agent_tasks WHERE id=?1",
                                [read_id],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()?;
                        Ok((events, status))
                    },
                )
                .await;
                let Ok(Ok((new_events, status))) = read else {
                    return None;
                };
                if let Some(event) = new_events.into_iter().next() {
                    last_seq = event.seq;
                    let id = event.seq.to_string();
                    let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                    let sse = Event::default().event("session/event").id(id).data(payload);
                    return Some((
                        Ok::<Event, std::convert::Infallible>(sse),
                        (state, task_id, last_seq),
                    ));
                }
                if status.as_deref().is_none_or(|s| s != "running") {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        },
    );
    Sse::new(events).keep_alive(KeepAlive::default())
}

pub async fn get_task(
    State(state): State<AgentServiceState>,
    Path(task_id): Path<String>,
) -> impl IntoResponse {
    let board_task_id = task_id.clone();
    let root = state.workspace.root().to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = open_db(&root)?;
        Ok(conn.query_row("SELECT id,prompt,model,status,created_at,updated_at,parent_task_id,provider_name,reasoning_effort,fast_mode FROM agent_tasks WHERE id=?1", [&task_id], |r| Ok(json!({"task_id":r.get::<_,String>(0)?,"prompt":r.get::<_,String>(1)?,"model":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"created_at":r.get::<_,i64>(4)?,"updated_at":r.get::<_,i64>(5)?,"parent_task_id":r.get::<_,Option<String>>(6)?,"provider_name":r.get::<_,Option<String>>(7)?,"reasoning_effort":r.get::<_,Option<String>>(8)?,"fast_mode":r.get::<_,i64>(9)? != 0}))).optional()?)
    }).await;
    match result {
        Ok(Ok(Some(task))) => match open_db(state.workspace.root())
            .and_then(|conn| crate::session_browser::decorate(&conn, state.workspace.root(), task))
        {
            Ok(mut task) => {
                match crate::main_task_anchor::board(state.workspace.root(), &board_task_id, None) {
                    Ok(board) => task["task_board"] = json!(board),
                    Err(error) => task["task_board_error"] = json!(error.to_string()),
                }
                Json(task).into_response()
            }
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"could not read sidebar state"})),
            )
                .into_response(),
        },
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"task not found"})),
        )
            .into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not read task"})),
        )
            .into_response(),
    }
}

/// Explicit task-board decision; no chat text or model-provided replacement fields.
pub async fn decide_task_definition(
    State(state): State<AgentServiceState>,
    Path(task): Path<String>,
    Json(args): Json<Value>,
) -> impl IntoResponse {
    let result = (|| -> anyhow::Result<Value> {
        let conn = open_db(state.workspace.root())?;
        let plugin: Option<String> = conn
            .query_row(
                "SELECT plugin_id FROM plugin_sessions WHERE task_id=?1",
                [&task],
                |r| r.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            plugin.is_none(),
            "plugin task decisions require the authenticated plugin session"
        );
        crate::main_task_anchor::decide_definition(
            state.workspace.root(),
            &task,
            args["version"]
                .as_i64()
                .context("proposal version required")?,
            args["approve"]
                .as_bool()
                .context("approve must be boolean")?,
            "agent-ui",
        )
    })();
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

pub async fn update_task(
    State(state): State<AgentServiceState>,
    Path(task_id): Path<String>,
    Json(request): Json<UpdateTaskRequest>,
) -> impl IntoResponse {
    let root = state.workspace.root().to_path_buf();
    let updated = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = open_db(&root)?;
        let exists: Option<(String, String)> = conn.query_row(
            "SELECT status, model FROM agent_tasks WHERE id=?1",
            [&task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let Some((status, _)) = exists else { return Ok(None); };
        if status == "running" || status == "cancelling" {
            anyhow::bail!("task is currently running");
        }
        if let Some(ref model) = request.model {
            conn.execute("UPDATE agent_tasks SET model=?2, updated_at=?3 WHERE id=?1", params![&task_id, model, now()])?;
        }
        if let Some(ref provider) = request.provider {
            conn.execute("UPDATE agent_tasks SET provider_name=?2, updated_at=?3 WHERE id=?1", params![&task_id, provider, now()])?;
        }
        if let Some(ref effort) = request.reasoning_effort {
            conn.execute("UPDATE agent_tasks SET reasoning_effort=?2, updated_at=?3 WHERE id=?1", params![&task_id, effort, now()])?;
        }
        if let Some(fast) = request.fast_mode {
            conn.execute("UPDATE agent_tasks SET fast_mode=?2, updated_at=?3 WHERE id=?1", params![&task_id, if fast { 1 } else { 0 }, now()])?;
        }
        Ok(conn.query_row(
            "SELECT id,prompt,model,status,created_at,updated_at,parent_task_id,provider_name,reasoning_effort,fast_mode FROM agent_tasks WHERE id=?1",
            [&task_id],
            |r| Ok(json!({
                "task_id": r.get::<_,String>(0)?,
                "prompt": r.get::<_,String>(1)?,
                "model": r.get::<_,String>(2)?,
                "status": r.get::<_,String>(3)?,
                "created_at": r.get::<_,i64>(4)?,
                "updated_at": r.get::<_,i64>(5)?,
                "parent_task_id": r.get::<_,Option<String>>(6)?,
                "provider_name": r.get::<_,Option<String>>(7)?,
                "reasoning_effort": r.get::<_,Option<String>>(8)?,
                "fast_mode": r.get::<_,i64>(9)? != 0
            })),
        ).optional()?)
    }).await;
    match updated {
        Ok(Ok(Some(task))) => match open_db(state.workspace.root())
            .and_then(|conn| crate::session_browser::decorate(&conn, state.workspace.root(), task))
        {
            Ok(task) => Json(task).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"could not read sidebar state"})),
            )
                .into_response(),
        },
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"task not found"})),
        )
            .into_response(),
        Ok(Err(err)) => (
            StatusCode::CONFLICT,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not update task"})),
        )
            .into_response(),
    }
}

pub async fn list_tasks(
    State(state): State<AgentServiceState>,
    Query(query): Query<ListTasksQuery>,
) -> impl IntoResponse {
    let root = state.workspace.root().to_path_buf();
    let limit = query.limit.unwrap_or(50).clamp(1, 10000) as i64;
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = open_db(&root)?;
        let mut stmt = conn.prepare("SELECT t.id,t.prompt,t.model,t.status,t.created_at,t.updated_at,t.parent_task_id,t.provider_name,t.reasoning_effort,t.fast_mode,p.plugin_id FROM agent_tasks t LEFT JOIN plugin_sessions p ON p.task_id=t.id WHERE trim(t.prompt)<>'' OR t.status IN ('running','cancelling') OR EXISTS(SELECT 1 FROM agent_task_events e WHERE e.task_id=t.id AND e.kind='user/message') ORDER BY t.updated_at DESC,t.id LIMIT ?1")?;
        let rows = stmt.query_map([limit], |r| Ok(json!({"task_id":r.get::<_,String>(0)?,"prompt":r.get::<_,String>(1)?,"model":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"created_at":r.get::<_,i64>(4)?,"updated_at":r.get::<_,i64>(5)?,"parent_task_id":r.get::<_,Option<String>>(6)?,"provider_name":r.get::<_,Option<String>>(7)?,"reasoning_effort":r.get::<_,Option<String>>(8)?,"fast_mode":r.get::<_,i64>(9)? != 0,"plugin_id":r.get::<_,Option<String>>(10)?})))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(|task|crate::session_browser::decorate(&conn,&root,task)).collect()
    }).await;
    match result {
        Ok(Ok(tasks)) => Json(json!({"tasks":tasks})).into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not list tasks"})),
        )
            .into_response(),
    }
}

pub async fn cancel_task(
    State(state): State<AgentServiceState>,
    Path(task_id): Path<String>,
) -> impl IntoResponse {
    if let Some(active) = state.cancellations.lock().await.get(&task_id).cloned() {
        active.token.cancel();
        return Json(json!({"task_id":task_id,"status":"cancelling"})).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error":"task is not running"})),
    )
        .into_response()
}

pub async fn interrupt_flow_node(
    State(state): State<AgentServiceState>,
    Path((task_id, node_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let active = state.cancellations.lock().await.get(&task_id).cloned();
    let Some(active) = active else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"task is not running"})),
        )
            .into_response();
    };
    let current = active.flow_node.lock().await.clone();
    if current != node_id {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"node is no longer active","active_node":current})),
        )
            .into_response();
    }
    *active.node_interrupt.lock().await = Some(node_id.clone());
    active.token.cancel();
    (
        StatusCode::ACCEPTED,
        Json(json!({"task_id":task_id,"node_id":node_id,"status":"interrupting"})),
    )
        .into_response()
}

pub(crate) fn uuid_like() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_ok() {
        return bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    static FALLBACK: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:08x}{:016x}{:08x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
        FALLBACK.fetch_add(1, Ordering::Relaxed) as u32
    )
}

async fn write_db_retry<F>(action: &str, write: F) -> anyhow::Result<()>
where
    F: Fn() -> anyhow::Result<()> + Send + Sync + 'static,
{
    let write = Arc::new(write);
    let mut last_error = None;
    for attempt in 0..3 {
        let write = write.clone();
        match tokio::task::spawn_blocking(move || write()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => last_error = Some(error),
            Err(error) => last_error = Some(error.into()),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(50 * (attempt + 1))).await;
        }
    }
    let error = last_error.unwrap_or_else(|| anyhow::anyhow!("unknown database error"));
    anyhow::bail!("{action} failed after retries: {error}")
}

pub(crate) async fn emit(
    root: &std::path::Path,
    task_id: &str,
    kind: &str,
    data: Value,
) -> anyhow::Result<()> {
    emit_surface(root, task_id, kind, data, None).await
}

async fn emit_surface(
    root: &std::path::Path,
    task_id: &str,
    kind: &str,
    data: Value,
    surface_op: Option<&'static str>,
) -> anyhow::Result<()> {
    let root = root.to_path_buf();
    let task_id = task_id.to_owned();
    let kind = kind.to_owned();
    write_db_retry("append task event", move || {
        append_event(&root, &task_id, &kind, data.clone(), surface_op)
    })
    .await
}

pub(crate) async fn finish(
    state: &AgentServiceState,
    task_id: &str,
    status: &str,
) -> anyhow::Result<()> {
    // A single-agent conversation owns durable resources across user turns.
    // Closing its browser is an explicit tool action, not failure cleanup.
    let lookup_root = state.workspace.root().to_path_buf();
    let lookup_task = task_id.to_owned();
    let single_agent = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let conn = open_db(&lookup_root)?;
        Ok(crate::session_history::load_events(
            &conn,
            &lookup_task,
            &crate::session_history::EventQuery {
                kinds: &["turn/start"],
                newest: Some(1),
                ..Default::default()
            },
        )?
        .pop()
        .map(|event| event.data())
        .transpose()?
        .is_some_and(|data| data["runtime"] == "single_agent"))
    })
    .await??;
    if !single_agent {
        crate::browser_control::cleanup_after_finish(state.workspace.root(), task_id, status).await;
    }
    let root = state.workspace.root().to_path_buf();
    let id = task_id.to_owned();
    let status = status.to_owned();
    write_db_retry("finish task", move || {
        let conn = open_db(&root)?;
        let changed = conn.execute(
            "UPDATE agent_tasks SET status=?2,updated_at=?3 WHERE id=?1",
            params![id, status, now()],
        )?;
        anyhow::ensure!(changed == 1, "task no longer exists");
        Ok(())
    })
    .await
}

fn spawn_supervised_task(
    state: AgentServiceState,
    task_id: String,
    run_id: String,
    model: String,
    prompt: String,
    max_steps: usize,
    cancel: CancellationToken,
    turn: usize,
    ban_run_command: bool,
) {
    tokio::spawn(async move {
        let worker = tokio::spawn(crate::single_agent::run_task(
            state.clone(),
            task_id.clone(),
            model,
            prompt,
            max_steps,
            cancel,
            turn,
            ban_run_command,
        ));
        let failure = match worker.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(format!("task worker stopped unexpectedly: {error}")),
        };
        if let Some(message) = failure {
            crate::collaboration_jobs::cancel_all(state.workspace.root(), &task_id);
            tracing::error!(task_id = %task_id, error = %message, "agent task failed");
            let root = state.workspace.root();
            if let Err(error) = emit(root, &task_id, "turn/end",
                json!({"turn":turn,"reason":{"kind":"error","error":{"message":message,"code":"WORKER_FAILED"}}})).await {
                tracing::error!(task_id = %task_id, error = %error, "could not record task failure");
            }
            if let Err(error) = finish(&state, &task_id, "failed").await {
                tracing::error!(task_id = %task_id, error = %error, "could not finish failed task");
            }
        }
        if let Err(error) = cleanup_finished_task_data(state.workspace.root(), &task_id).await {
            tracing::warn!(task_id = %task_id, error = %error, "could not clean terminal task execution data");
        }
        clear_active_task(&state, &task_id, &run_id).await;
    });
}

/// Read an OpenAI chat completion stream, emit coalesced `assistant/delta` events, and return the assembled message.
pub(crate) async fn consume_model_stream_observed(
    response: reqwest::Response,
    root: &std::path::Path,
    task_id: &str,
    turn: usize,
    step: usize,
    cancel: &CancellationToken,
    diagnostics: Option<&crate::model_transport::ModelRequestLog>,
) -> anyhow::Result<(Value, Value)> {
    let stream_started = Instant::now();
    let mut accumulator =
        crate::model_stream::ChatStream::new(crate::model_stream::Limits::WORKER, stream_started);
    let mut bytes = response.bytes_stream();
    let mut delta_text = String::new();
    let mut delta_reasoning = String::new();
    let mut last_flush = Instant::now();
    let mut flushed = false;

    while !accumulator.received_done() {
        if let Some(log) = diagnostics {
            log.phase("stream_read");
        }
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(anyhow::anyhow!("cancelled")),
            next = bytes.next() => match next {
                Some(Ok(chunk)) => chunk,
                Some(Err(error)) => return Err(error.into()),
                None => { if let Some(log) = diagnostics { log.eof(); } break; },
            },
        };
        if let Some(log) = diagnostics {
            log.chunk(chunk.len());
            log.phase("stream_decode");
        }
        let delta = accumulator.push(&chunk)?;
        if let Some(log) = diagnostics {
            log.delta(&delta);
            // Keep decoded counts even when the outer deadline/cancellation
            // drops this future before a complete tool call is delivered.
            log.decoded(&accumulator.stats());
        }
        delta_text.push_str(&delta.text);
        delta_reasoning.push_str(&delta.reasoning);
        let ready = !delta_text.is_empty() || !delta_reasoning.is_empty();
        let due = !flushed
            || last_flush.elapsed() >= Duration::from_millis(80)
            || delta_text.len() + delta_reasoning.len() >= 32;
        if ready && due {
            if let Some(log) = diagnostics {
                log.phase("event_persistence");
            }
            emit(
                root,
                task_id,
                "assistant/delta",
                json!({"turn":turn,"step":step,"text":delta_text,"reasoning":delta_reasoning}),
            )
            .await?;
            delta_text.clear();
            delta_reasoning.clear();
            last_flush = Instant::now();
            flushed = true;
        }
    }
    if !delta_text.is_empty() || !delta_reasoning.is_empty() {
        if let Some(log) = diagnostics {
            log.phase("event_persistence");
        }
        emit(
            root,
            task_id,
            "assistant/delta",
            json!({"turn":turn,"step":step,"text":delta_text,"reasoning":delta_reasoning}),
        )
        .await?;
    }
    if let Some(log) = diagnostics {
        log.phase("stream_decode");
        log.decoded(&accumulator.stats());
    }
    let completed = accumulator.finish()?;
    let mut stats = completed.stats;
    stats["format"] = stats["response_format"].clone();
    stats["first_delta_after_headers_ms"] = stats["first_delta_ms"].clone();
    stats["stream_ms"] = stats["response_complete_ms"].clone();
    stats["terminated"] = json!(completed.terminated);
    if let Some(log) = diagnostics {
        log.decoded(&stats);
    }
    Ok((completed.message, stats))
}

pub(crate) fn local_tools(
    _workspace: &crate::tools::Workspace,
    catalog: Option<&crate::builtin_plugins::ToolCatalog>,
    ban_run_command: bool,
) -> Vec<Value> {
    let definitions = crate::mcp::tool_definitions();
    let allowed = [
        "workspace_info",
        "install_dependencies",
        "run_project_script",
        "get_project_process",
        "http_probe",
        "run_program",
        "stop_project_process",
        "browser_open",
        "browser_read",
        "browser_wait",
        "browser_diagnostics",
        "browser_click",
        "browser_press_key",
        "browser_upload",
        "browser_screenshot",
        "view_image",
        "browser_close",
        "list_dir",
        "read_file",
        "read_file_lines",
        "search_text",
        "write_file",
        "replace_range",
        "edit_file",
        "search_code_map",
        "index_go_workspace",
        "go_index_status",
        "list_go_symbols",
        "search_go_symbols",
        "read_go_symbol",
        "index_rust_workspace",
        "rust_index_status",
        "list_rust_symbols",
        "search_rust_symbols",
        "read_rust_symbol",
        "index_ts_workspace",
        "ts_index_status",
        "list_ts_symbols",
        "search_ts_symbols",
        "read_ts_symbol",
        "index_python_workspace",
        "python_index_status",
        "list_python_symbols",
        "search_python_symbols",
        "read_python_symbol",
        "record_work_memory",
        "list_work_memory",
        "search_work_memory",
        "record_architecture_memory",
        "list_architecture_memory",
        "search_architecture_memory",
        "record_symbol_business_context",
        "list_symbol_business_context",
        "search_symbol_business_context",
    ];
    definitions.as_array().into_iter().flatten().filter(|tool| tool.get("name").and_then(Value::as_str).is_some_and(|n| (!ban_run_command || (n != "run_program" && !crate::project_process::is_execution(n))) && catalog.map_or(allowed.contains(&n), |catalog| catalog.contains(n) && !catalog.packages.declares_tool(n)))).map(|tool| {
        let name=tool["name"].as_str().unwrap_or_default(); let mut description=tool["description"].as_str().unwrap_or_default().to_owned();
        let mut schema=tool.get("inputSchema").cloned().unwrap_or_else(||json!({"type":"object","properties":{}}));
        if let Some(props)=schema.get_mut("properties").and_then(Value::as_object_mut) {props.remove("workspace_root");}
        if let Some(required)=schema.get_mut("required").and_then(Value::as_array_mut) {required.retain(|name| name.as_str()!=Some("workspace_root"));}
        if matches!(name, "write_file" | "replace_range" | "edit_file") {
            if let Some(props)=schema.get_mut("properties").and_then(Value::as_object_mut) { props.remove("expected_code_hash"); }
            if let Some(required)=schema.get_mut("required").and_then(Value::as_array_mut) { required.retain(|item| item.as_str()!=Some("expected_code_hash")); }
            description = match name {
                "write_file" => "Create or replace a UTF-8 file. The host manages the file baseline and stops for user action if the file changed externally.",
                "edit_file" => "Apply unique text replacements to a UTF-8 file. The host manages the file baseline and stops for user action if the file changed externally.",
                _ => "Replace an inclusive 1-indexed line range. The host manages the file baseline and stops for user action if the file changed externally.",
            }.to_owned();
            if let Some(prop)=schema.pointer_mut("/properties/expected_old_text") { prop["description"]=json!("Optional exact selected text, used to validate the requested edit."); }
        }
        json!({"type":"function","function":{"name":name,"description":description,"parameters":schema}})
    }).collect()
}

pub(crate) async fn execute_tool(
    workspace: Arc<crate::tools::Workspace>,
    catalog: Option<&crate::builtin_plugins::ToolCatalog>,
    name: &str,
    mut args: Value,
) -> anyhow::Result<Value> {
    if let Some(catalog) = catalog {
        return catalog.execute(workspace, name, args).await;
    }
    if crate::browser_control::is_tool(name) {
        return crate::browser_control::execute(&workspace, name, &args).await;
    }
    if crate::http_probe::is_tool(name) {
        return crate::http_probe::execute(&args).await;
    }
    if crate::project_process::is_tool(name) {
        return crate::project_process::execute(&workspace, name, args).await;
    }
    if name == "run_command" {
        anyhow::bail!(
            "run_command has been removed and was not executed; use run_program or a dedicated tool"
        );
    }
    if crate::program_execution::is_tool(name) {
        return crate::program_execution::execute(&workspace, &args).await;
    }
    if let Some(object) = args.as_object_mut() {
        object.insert(
            "workspace_root".into(),
            json!(workspace.root().display().to_string()),
        );
    }
    let response = crate::mcp::call_tool(&workspace, json!({"name":name,"arguments":args})).await?;
    Ok(response
        .get("structuredContent")
        .cloned()
        .unwrap_or(response))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn test_state(
        root: &std::path::Path,
        address: std::net::SocketAddr,
    ) -> AgentServiceState {
        AgentServiceState {
            workspace: Arc::new(crate::tools::Workspace::new(root.to_path_buf()).unwrap()),
            model_client: crate::model_transport::client().unwrap(),
            provider_url: format!("http://{address}/v1"),
            api_key: "fake".into(),
            provider_name: String::new(),
            provider_routes: HashMap::new(),
            default_model: "fake-model".into(),
            model_map: HashMap::new(),
            reasoning_effort: None,
            fast_mode: false,
            permission_mode: PermissionMode::default(),

            visual: Default::default(),
            visual_tool_settings: Default::default(),
            delegate_agent_settings: Default::default(),
            config: None,
            config_path: None,
            tool_catalog: None,
            plugin_cancel: CancellationToken::new(),
            cancellations: Arc::default(),
        }
    }

    #[test]
    fn terminal_cleanup_preserves_tools_flow_and_messages_after_completion_and_restart() {
        let root = std::env::temp_dir().join(format!("agent-history-retention-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let mut conn = open_db(&root).unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_request_contexts(task_id TEXT,request_json TEXT);
            CREATE TABLE agent_context_debug(task_id TEXT,enabled INTEGER);",
        )
        .unwrap();
        let visible = [
            ("flow/plan", json!({"nodes":[{"id":"probe"}]})),
            (
                "tool/call",
                json!({"callId":"probe_call","name":"http_probe","arguments":"{}"}),
            ),
            (
                "tool/result",
                json!({"toolCallId":"probe_call","message":{"isError":true,"content":[{"type":"text","text":"original failure"}]}}),
            ),
            (
                "assistant/message",
                json!({"message":{"content":[{"type":"tool-call","name":"http_probe"}]}}),
            ),
            (
                "assistant/message",
                json!({"message":{"content":[{"type":"text","text":"final answer with limitation"}]}}),
            ),
            ("worker/progress", json!({"purpose":"Check the service"})),
            (
                "observer/inbox_state",
                json!({"messages":["original advice"]}),
            ),
            (
                "debug/context_request",
                json!({"id":1,"actor":"worker","record_kind":"model_input"}),
            ),
            ("debug/context_end", json!({"id":1})),
        ];
        for status in [
            "completed",
            "failed",
            "cancelled",
            "max_steps",
            "interrupted",
            "running",
        ] {
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'test','model',?1,0,0)",[status]).unwrap();
            for (kind, data) in &visible {
                append_event(&root, status, kind, data.clone(), None).unwrap();
            }
            append_event(
                &root,
                status,
                "scheduler/state",
                json!({"state":"temporary"}),
                None,
            )
            .unwrap();
            conn.execute(
                "INSERT INTO agent_request_contexts VALUES (?1,'raw request')",
                [status],
            )
            .unwrap();
            conn.execute("INSERT INTO agent_context_debug VALUES (?1,1)", [status])
                .unwrap();
        }
        cleanup_terminal_task_data(&mut conn, Some("completed"), false).unwrap();
        // Startup cleanup runs over all terminal tasks, including already cleaned tasks.
        cleanup_terminal_task_data(&mut conn, None, false).unwrap();
        for status in [
            "completed",
            "failed",
            "cancelled",
            "max_steps",
            "interrupted",
            "running",
        ] {
            let rows = conn
                .prepare(
                    "SELECT seq,kind,data FROM agent_task_events WHERE task_id=?1 ORDER BY seq",
                )
                .unwrap()
                .query_map([status], |row| {
                    Ok((
                        row.get::<_, usize>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            for (index, (kind, data)) in visible.iter().enumerate() {
                assert_eq!(rows[index].0, index);
                assert_eq!(&rows[index].1, kind);
                assert_eq!(
                    serde_json::from_str::<Value>(&rows[index].2).unwrap(),
                    *data
                );
            }
            assert_eq!(rows.len(), visible.len() + usize::from(status == "running"));
            let contexts: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_request_contexts WHERE task_id=?1",
                    [status],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                contexts, 1,
                "actual inputs survive completion and startup cleanup"
            );
            let settings: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_context_debug WHERE task_id=?1",
                    [status],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(settings, 1);
        }
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recovery_closes_open_step_and_marks_task_interrupted() {
        let root = std::env::temp_dir().join(format!("agent-recovery-test-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let task_id = format!("task_{}", uuid_like());
        {
            let conn = open_db(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'test','fake-model','running',?2,?2)", params![task_id,now()]).unwrap();
        }
        append_event(&root, &task_id, "turn/start", json!({"turn":1}), None).unwrap();
        append_event(
            &root,
            &task_id,
            "step/start",
            json!({"turn":1,"step":1}),
            None,
        )
        .unwrap();

        recover_orphaned_tasks(&root).await.unwrap();

        let conn = open_db(&root).unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM agent_tasks WHERE id=?1",
                [&task_id],
                |r| r.get(0),
            )
            .unwrap();
        let kinds: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT kind FROM agent_task_events WHERE task_id=?1 ORDER BY seq")
                .unwrap();
            stmt.query_map([&task_id], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(status, "interrupted");
        assert_eq!(&kinds[2..], ["step/end", "turn/end"]);
        let _ = std::fs::remove_dir_all(root);
    }
}
