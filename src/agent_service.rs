//! Standalone task API for the local Rust agent. Events are append-only so a
//! client can recover a task's trajectory after reconnecting.

use std::{
    collections::HashMap,
    sync::{Arc, atomic::{AtomicU64,Ordering}},
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
const OBSERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
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
    /// Model-specific template options for context work only, from the catalog.
    pub context_request_options: HashMap<String, Value>,
}

#[derive(Clone)]
pub struct AgentServiceState {
    pub workspace: Arc<crate::tools::Workspace>,
    pub client: Client,
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

    pub observer_enabled: bool,
    pub observer_provider: String,
    pub observer_provider_url: String,
    pub observer_api_key: String,
    pub observer_model: String,
    pub observer_inherits_model: bool,
    pub visual: crate::visual_artifacts::VisualSettings,
    pub visual_tool_settings: crate::model_settings::ModelSettings,
    pub context_agent_settings: crate::model_settings::ModelSettings,
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
        if self.observer_inherits_model {
            self.observer_provider = name.to_owned();
            self.observer_provider_url = route.url.clone();
            self.observer_api_key = route.api_key.clone();
        }
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
pub struct DraftSessionInput { pub workspace: Option<String>, pub kind: Option<String> }
pub async fn create_draft_task(State(state): State<AgentServiceState>, input: Option<Json<DraftSessionInput>>) -> impl IntoResponse {
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
    let kind = if input.kind.as_deref() == Some("chat") { "chat" } else { "code" };
    let selected_workspace = match input.workspace {
        Some(path) => match std::fs::canonicalize(path) {
            Ok(path) if path.is_dir() => path,
            _ => return (StatusCode::BAD_REQUEST, Json(json!({"error":"项目目录不存在"}))).into_response(),
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
         CREATE TABLE IF NOT EXISTS ppt_session_bindings (
            task_id TEXT PRIMARY KEY REFERENCES agent_tasks(id) ON DELETE CASCADE,
            subject TEXT NOT NULL, project_id INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS ppt_session_jobs (
            host_task_id TEXT NOT NULL REFERENCES agent_tasks(id) ON DELETE CASCADE,
            remote_task_id TEXT NOT NULL, tool TEXT NOT NULL,
            PRIMARY KEY(host_task_id,remote_task_id)
         );
         CREATE TABLE IF NOT EXISTS ppt_template_pins (
            task_id TEXT PRIMARY KEY REFERENCES agent_tasks(id) ON DELETE CASCADE,
            selection TEXT NOT NULL
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
    let has_node: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_task_events') WHERE name='node_id')", [], |r|r.get(0))?;
    if !has_node {
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        if !tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_task_events') WHERE name='node_id')", [], |r|r.get::<_,bool>(0))? {
            tx.execute("ALTER TABLE agent_task_events ADD COLUMN node_id TEXT", [])?;
        }
        tx.commit()?;
    }
    crate::task_tree::schema(&conn)?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS task_events_node ON agent_task_events(task_id,node_id,seq);")?;
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
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        let migrated: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_tasks') WHERE name='event_high_water')", [], |row| row.get(0))?;
        if !migrated {
            tx.execute("ALTER TABLE agent_tasks ADD COLUMN event_high_water INTEGER NOT NULL DEFAULT -1", [])?;
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
            if let Some(event)=crate::session_history::load_events(&conn,&task_id,&crate::session_history::EventQuery{kinds:&["step/start","step/end"],newest:Some(1),..Default::default()})?.pop()
                && event.kind=="step/start" {
                let step_data=event.data()?;
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                append_event_tx(&tx, &task_id, "step/end", &step_data, None)?;
                tx.commit()?;
            }
            let turn=crate::session_history::load_events(&conn,&task_id,&crate::session_history::EventQuery{kinds:&["turn/start"],newest:Some(1),..Default::default()})?.pop().map(|event|event.data()).transpose()?.and_then(|data|data["turn"].as_u64()).unwrap_or(1) as usize;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            append_event_tx(&tx, &task_id, "turn/end", &json!({"turn":turn,"reason":{"kind":"interrupted"}}), None)?;
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
    let (task_id, kind, surface_op) = (task_id.to_owned(), kind.to_owned(), surface_op.map(str::to_owned));
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
        crate::task_tree::active(tx,task_id)?.map(|n|n.node_id)
    };
    tx.execute(
        "INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data,surface_op,node_id) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![task_id, seq, now(), kind, serde_json::to_string(&data)?, surface_op,node],
    )?;
    if matches!(kind,"user/message"|"user/interjection") && let Some(node) = &node {
        crate::task_tree::register_user(tx,task_id,seq,node,data.get("message").map(|m|&m["content"]).unwrap_or(&data["text"]))?;
    }
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

fn model_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn observer_excerpt(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_owned();
    }
    let head = limit / 2;
    let tail = limit - head;
    format!(
        "{}\n[...]\n{}",
        chars[..head].iter().collect::<String>(),
        chars[chars.len() - tail..].iter().collect::<String>()
    )
}

#[cfg(test)]
pub(crate) async fn observer_json_response(
    state: &AgentServiceState,
    model: &str,
    system: &str,
    context: Value,
    max_tokens: usize,
    task_id: &str,
    trace_metadata: Value,
) -> anyhow::Result<String> {
    observer_json_response_cancelled(
        state,
        model,
        system,
        context,
        max_tokens,
        task_id,
        trace_metadata,
        &state.plugin_cancel,
    )
    .await
}
pub(crate) async fn observer_json_response_cancelled(
    state: &AgentServiceState,
    model: &str,
    system: &str,
    mut context: Value,
    max_tokens: usize,
    task_id: &str,
    mut trace_metadata: Value,
    cancel: &CancellationToken,
) -> anyhow::Result<String> {
    let system = format!(
        "{}\n\n{system}",
        include_str!("../prompts/shared_reasoning.md").trim()
    );
    let mut observer_view = crate::context_window::HistoryView::default();
    observer_view.prepend(json!({"role":"system","content":system}));
    if context["stage"] == "retrospective" {
        let mut header = context.clone();
        header["task_history"]
            .as_object_mut()
            .unwrap()
            .remove("records");
        observer_view
            .messages
            .push(json!({"role":"user","content":crate::session_history::plain_context(&header)}));
        observer_view.sources.push(None);
        let process = crate::session_history::process_view(&context["task_history"])?;
        observer_view.messages.extend(process.messages);
        observer_view.sources.extend(process.sources);
    } else {
        let header = context.clone();
        observer_view
            .messages
            .push(json!({"role":"user","content":crate::session_history::plain_context(&header)}));
        observer_view.sources.push(None);
        let process =
            crate::session_history::process_view(&json!({"records":context["recent_activity"]}))?;
        observer_view.messages.extend(process.messages);
        observer_view.sources.extend(process.sources);
    }
    let mut request_body = json!({"model":model,"stream":false,"max_tokens":max_tokens,
        "messages":observer_view.messages,
        "tools":crate::observer_service::tools(state),"tool_choice":"auto"});
    if context["stage"] == "retrospective"
        && crate::context_window::estimate(&request_body)
            > crate::context_window::MAX_TOKENS.saturating_sub(max_tokens + 8192)
    {
        context["task_history"] =
            crate::observer_service::acceptance_history(&context["task_history"]);
        let mut header = context.clone();
        header["task_history"]
            .as_object_mut()
            .unwrap()
            .remove("records");
        observer_view.messages.truncate(1);
        observer_view.sources.truncate(1);
        observer_view
            .messages
            .push(json!({"role":"user","content":crate::session_history::plain_context(&header)}));
        observer_view.sources.push(None);
        let process = crate::session_history::process_view(&context["task_history"])?;
        observer_view.messages.extend(process.messages);
        observer_view.sources.extend(process.sources);
        request_body["messages"] = json!(observer_view.messages);
    }
    let mut visual_context = crate::visual_artifacts::VisualContext {
        turn: trace_metadata["turn"].as_u64().map(|turn| turn as usize),
        identity: context["identity"].clone(),
        execution_epoch: context["execution_epoch"].as_u64().unwrap_or(0) as usize,
        related_source_versions: context["related_source_versions"].clone(),
        current_page: context["task_page"].clone(),
        host_current_artifact_ids: context["host_current_artifact_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect(),
        allowed_artifact_ids: context["allowed_visual_artifact_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect(),
        ..Default::default()
    };
    visual_context
        .allowed_artifact_ids
        .extend(crate::visual_artifacts::request_artifact_ids(
            state.workspace.root(),
            task_id,
            context["identity"]["request_id"].as_u64().unwrap_or(1),
        )?);
    // An observation never selects or captures images on the model's behalf.
    let ids: Vec<String> = Vec::new();
    let scope = format!(
        "request:{}:node:{}",
        context["identity"]["request_id"].as_u64().unwrap_or(1),
        context["identity"]["node_id"].as_str().unwrap_or("request")
    );
    trace_metadata["context_window"] = crate::context_rebuild::prepare(
        state,
        model,
        "observer",
        task_id,
        &scope,
        &context,
        &mut request_body,
        observer_view.clone(),
        trace_metadata.clone(),
        cancel,
        OBSERVER_REQUEST_TIMEOUT,
    )
    .await?;
    let mut lookup_messages = Vec::<Value>::new();
    let mut wire_body = request_body.clone();
    let visual_dispatch = crate::visual_artifacts::prepare_request(
        state,
        "observer",
        model,
        &visual_context,
        &ids,
        context["request"]["goal"]
            .as_str()
            .unwrap_or("review current visual delivery"),
        &mut wire_body,
    )
    .await?;
    crate::context_window::prepare(&mut wire_body, 2)?;
    trace_metadata["visual_dispatch"] = visual_dispatch.clone();
    trace_metadata["actor"] = json!("observer");
    let mut trace = crate::request_context::record(
        state.workspace.root(),
        task_id,
        trace_metadata.clone(),
        &wire_body,
    )
    .await;
    let result = tokio::time::timeout(OBSERVER_REQUEST_TIMEOUT, async {
        // A small ordinary tool loop lets Observer retrieve missing history.
        // It does not dispatch Worker tasks or change the scheduler.
        let mut lookup_round=0;
        loop {
            let followup_trace=if lookup_round>0 {
                let mut view=observer_view.clone();view.sources.extend(std::iter::repeat_n(None,lookup_messages.len()));view.messages.extend(lookup_messages.clone());
                trace_metadata["context_window"]=crate::context_rebuild::prepare(state,model,"observer",task_id,&scope,&context,&mut request_body,view,trace_metadata.clone(),cancel,OBSERVER_REQUEST_TIMEOUT).await?;
                wire_body=request_body.clone();
                let dispatch=crate::visual_artifacts::prepare_request(state,"observer",model,&visual_context,&ids,context["request"]["goal"].as_str().unwrap_or("review current visual delivery"),&mut wire_body).await?;
                crate::context_window::prepare(&mut wire_body,2)?;
                let mut metadata=trace_metadata.clone();metadata["history_lookup_round"]=json!(lookup_round);
                metadata["visual_dispatch"]=dispatch;
                crate::request_context::record(state.workspace.root(),task_id,metadata,&wire_body).await
            } else {trace.take()};
            let response=tokio::select! { biased;
                _=cancel.cancelled()=>Err(anyhow::anyhow!("Observer cancelled by user")),
                response=async {
                let response = state.client.post(format!("{}/chat/completions", state.observer_provider_url.trim_end_matches('/')))
                    .bearer_auth(&state.observer_api_key).header("x-codex-visual-dispatch","task-owned").json(&wire_body).send().await?;
                crate::visual_probe::successful_response(response).await?.json::<Value>().await.map_err(anyhow::Error::from)
            }=>response };
            crate::request_context::finish(followup_trace,if cancel.is_cancelled(){"cancelled"}else if response.is_ok(){"completed"}else{"failed"},
                json!({"response":response.as_ref().ok(),"error":response.as_ref().err().map(|error|format!("{error:#}"))})).await;
            let body=response?;let message=&body["choices"][0]["message"];
            let calls=message["tool_calls"].as_array().filter(|calls|!calls.is_empty());
            if let Some(calls)=calls {
                let exchange=json!({"role":"assistant","content":message["content"],"tool_calls":calls});
                lookup_messages.push(exchange.clone());request_body["messages"].as_array_mut().unwrap().push(exchange);
                for call in calls {
                    let args=call["function"]["arguments"].as_str().unwrap_or("{}");
                    if cancel.is_cancelled() {anyhow::bail!("Observer cancelled by user");}
                    let name=call["function"]["name"].as_str().unwrap_or("");
                    let output=match serde_json::from_str::<Value>(args) {
                        Ok(args) if name=="read_session_history"=>crate::session_history::read(state.workspace.root(),task_id,&args).await,
                        Ok(args) if crate::observer_service::read_tool(name)&&request_body["tools"].as_array().into_iter().flatten().any(|tool|tool["function"]["name"]==name)=> {
                            if matches!(name,"view_image"|"browser_read"|"browser_diagnostics") {crate::browser_control::execute_scoped(&state.workspace,&visual_context,name,&args).await}
                            else {execute_tool(state.workspace.clone(),state.tool_catalog.as_deref(),name,args).await}
                        },
                        Ok(_)=>Err(anyhow::anyhow!("Observer tool is not available: {name}")),
                        Err(error)=>Err(error.into()),
                    };
                    let is_error=output.is_err();
                    let result=output.unwrap_or_else(|error|json!({"error":format!("{error:#}")}));
                    emit(state.workspace.root(),task_id,"observer/history_read",json!({"actor":"observer","turn":trace_metadata["turn"],"stage":"retrospective",
                        "identity":context["identity"],"callId":call["id"],"name":call["function"]["name"],"is_error":is_error,
                        "review_id":context["review_id"],"arguments":args,"result":result})).await?;
                    let message=json!({"role":"tool","tool_call_id":call["id"],"content":result.to_string()});
                    lookup_messages.push(message.clone());request_body["messages"].as_array_mut().unwrap().push(message);
                    let images=crate::session_history::history_image_messages(&result);
                    lookup_messages.extend(images.clone());request_body["messages"].as_array_mut().unwrap().extend(images);
                    if !is_error&&name=="view_image" {
                        let image=json!({"role":"user","source_identity":result["visual_artifact"]["identity"],
                            "content":[{"type":"image_ref","artifact_id":result["artifact_id"],"view_original":serde_json::from_str::<Value>(args)?["original"]==true}]});
                        lookup_messages.push(image.clone());request_body["messages"].as_array_mut().unwrap().push(image);
                    }
                }
                lookup_round+=1;
                continue;
            }
            let raw=message.get("content").map(model_text).unwrap_or_default();
            anyhow::ensure!(!raw.trim().is_empty(), "observer returned an empty response");
            request_body["messages"].as_array_mut().unwrap().push(json!({"role":"assistant","content":raw}));
            return Ok((raw,body.get("usage").cloned().unwrap_or(Value::Null)));
        }
    }).await.context("observer request timed out").and_then(|value| value);
    match result {
        Ok((raw, usage)) => {
            if !ids.is_empty() {
                emit(state.workspace.root(),task_id,"visual/model_received",json!({"manifest":visual_dispatch,"actor":"observer","review_id":context["review_id"]})).await?;
            }
            let mut validation = Value::Null;
            if let Some(result) = serde_json::from_str::<Value>(
                raw.trim()
                    .trim_start_matches("```json")
                    .trim_end_matches("```")
                    .trim(),
            )
            .ok()
            .filter(Value::is_object)
            {
                if let Some(check) = result
                    .get("visual_check_result")
                    .filter(|_| !ids.is_empty())
                {
                    let requests = vec![visual_dispatch.clone()];
                    validation = match crate::visual_artifacts::validate_check(
                        state.workspace.root(),
                        &visual_context,
                        check,
                        &requests,
                        "observer",
                    ) {
                        Ok(check) => {
                            emit(
                                state.workspace.root(),
                                task_id,
                                "visual/check_result",
                                json!({"result":check,"review_id":context["review_id"]}),
                            )
                            .await?;
                            check
                        }
                        Err(error) => {
                            let failure = json!({"error":error.to_string(),"review_id":context["review_id"],"actor":"observer","visual_request":visual_dispatch});
                            emit(
                                state.workspace.root(),
                                task_id,
                                "visual/check_error",
                                failure.clone(),
                            )
                            .await?;
                            failure
                        }
                    };
                }
            }
            crate::request_context::finish(trace,"completed",json!({"response_chars":raw.chars().count(),"response_text":raw,"usage":usage,"visual_validation":validation})).await;
            Ok(raw)
        }
        Err(error) => {
            if !ids.is_empty() {
                emit(
                    state.workspace.root(),
                    task_id,
                    "visual/model_failed",
                    json!({"manifest":visual_dispatch,"actor":"observer",
                    "review_id":context["review_id"],"error":format!("{error:#}")}),
                )
                .await?;
            }
            crate::request_context::finish(
                trace,
                if error.is::<tokio::time::error::Elapsed>() {
                    "timeout"
                } else {
                    "failed"
                },
                json!({"error":format!("{error:#}")}),
            )
            .await;
            if ids.is_empty() {
                Err(error)
            } else {
                Err(crate::visual_probe::VisualModelFailure {
                    manifest: visual_dispatch,
                    error,
                }
                .into())
            }
        }
    }
}

fn observer_string_list(value: Option<&Value>, limit: usize, char_limit: usize) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|item| truncate(item.trim(), char_limit))
        .filter(|item| !item.is_empty())
        .take(limit)
        .collect()
}

fn supported_source_refs(value: Option<&Value>, trace_text: &str) -> Vec<String> {
    let normalized_trace = trace_text.replace('\\', "/").replace("//", "/");
    observer_string_list(value, 6, 240)
        .into_iter()
        .filter(|reference| {
            let path = reference
                .rsplit_once(':')
                .filter(|(_, suffix)| {
                    suffix.chars().any(|ch| ch.is_ascii_digit())
                        && suffix.chars().all(|ch| ch.is_ascii_digit() || ch == '-')
                })
                .map(|(path, _)| path)
                .unwrap_or(reference);
            let normalized = path.replace('\\', "/");
            let file = normalized
                .rsplit('/')
                .next()
                .unwrap_or("")
                .split("::")
                .next()
                .unwrap_or("");
            file.len() >= 4 && normalized_trace.contains(file)
        })
        .collect()
}

pub(crate) fn load_observer_work_trace(
    root: &std::path::Path,
    task_id: &str,
) -> anyhow::Result<Vec<Value>> {
    load_observer_trace_since(root, task_id, -1)
}

pub(crate) fn load_observer_trace_since(
    root: &std::path::Path,
    task_id: &str,
    after_seq: i64,
) -> anyhow::Result<Vec<Value>> {
    let conn = open_db(root)?;
    let raw_events = crate::session_history::load_events(
        &conn,
        task_id,
        &crate::session_history::EventQuery {
            after: Some(after_seq),
            ..Default::default()
        },
    )?
    .into_iter()
    .map(|event| (event.seq, event.kind, event.raw))
    .collect::<Vec<_>>();
    let mut tool_names = HashMap::<String, String>::new();
    for (_, kind, raw) in &raw_events {
        if kind != "tool/call" {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        if let (Some(id), Some(name)) = (
            data.get("callId").and_then(Value::as_str),
            data.get("name").and_then(Value::as_str),
        ) {
            tool_names.insert(id.to_owned(), name.to_owned());
        }
    }
    let mut trace = Vec::new();
    for (seq, kind, raw) in raw_events {
        if !matches!(
            kind.as_str(),
            "flow/plan"
                | "worker/progress"
                | "tool/call"
                | "tool/result"
                | "assistant/message"
                | "observer/consult"
                | "observer/consult_reply"
                | "observer/plan_review"
                | "observer/progress_review"
                | "subagent/start"
                | "subagent/end"
                | "turn/end"
                | "user/message"
                | "organizer/decision"
                | "worker/yield"
                | "visual/model_received"
                | "visual/model_failed"
                | "visual/service_result"
                | "visual/check_result"
        ) {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let mut entry = json!({"seq":seq,"type":kind});
        for field in ["turn", "step", "nodeId", "reason"] {
            if let Some(value) = data.get(field) {
                entry[field] = value.clone();
            }
        }
        match kind.as_str() {
            "flow/plan" => {
                let nodes = data.get("nodes").and_then(Value::as_array).into_iter().flatten().take(128)
                    .map(|node| json!({
                        "id":node.get("id"),"title":node.get("title"),"kind":node.get("kind"),
                        "status":node.get("status"),"description":node.get("description"),
                        "parent_id":node.get("parent_id"),"objective":node.get("objective"),"done_when":node.get("done_when"),
                    })).collect::<Vec<_>>();
                let edges = data
                    .get("edges")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(128)
                    .map(|edge| json!({"source":edge.get("source"),"target":edge.get("target")}))
                    .collect::<Vec<_>>();
                entry["nodes"] = json!(nodes);
                entry["edges"] = json!(edges);
            }
            "worker/progress" => {
                for field in ["purpose", "knownConditions", "nextAction", "nodeResult"] {
                    if let Some(value) = data.get(field) {
                        entry[field] = value.clone();
                    }
                }
            }
            "tool/call" => {
                entry["tool"] = data.get("name").cloned().unwrap_or(Value::Null);
                entry["input"] = json!(observer_excerpt(
                    data.get("arguments").and_then(Value::as_str).unwrap_or(""),
                    700
                ));
            }
            "tool/result" => {
                let message = data.get("message").unwrap_or(&Value::Null);
                let call_id = message
                    .pointer("/source/callId")
                    .and_then(Value::as_str)
                    .or_else(|| message.get("toolCallId").and_then(Value::as_str))
                    .unwrap_or("");
                let tool_name = tool_names
                    .get(call_id)
                    .cloned()
                    .or_else(|| {
                        crate::session_history::load_events(
                            &conn,
                            task_id,
                            &crate::session_history::EventQuery {
                                kinds: &["tool/call"],
                                call_id: Some(call_id),
                                newest: Some(1),
                                ..Default::default()
                            },
                        )
                        .ok()?
                        .pop()?
                        .data()
                        .ok()?["name"]
                            .as_str()
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                entry["tool"] = json!(tool_name);
                let output = data
                    .pointer("/meta/result")
                    .map(Value::to_string)
                    .unwrap_or_else(|| model_text(message.get("content").unwrap_or(&Value::Null)));
                entry["outcome"] = json!(observer_excerpt(&output, 700));
                // Preserve a compact, versioned target even when the source excerpt is shortened.
                if let Some(result) = data.pointer("/meta/result") {
                    let tool = entry.get("tool").and_then(Value::as_str).unwrap_or("");
                    if tool.starts_with("read_") && tool.ends_with("_symbol") {
                        if let Some(symbol) = result.get("symbol") {
                            entry["codeTarget"] = json!({"scope":"symbol","symbol_id":symbol.get("id"),"symbol_name":symbol.get("name"),"file_path":symbol.get("file_path"),"language":tool.trim_start_matches("read_").trim_end_matches("_symbol"),"qualified_name":result.pointer("/description/qualified_name"),"code_hash":result.pointer("/description/code_hash")});
                        }
                    } else if tool == "read_file" {
                        let file = result.get("path").and_then(Value::as_str).unwrap_or("");
                        let language = std::path::Path::new(file)
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .and_then(crate::symbol_description::language);
                        if let Some(language) = language {
                            entry["codeTarget"] = json!({"scope":"file","file_path":file,"language":language,"code_hash":result.get("code_hash")});
                        }
                    }
                }
                if let Some(duration) = data.pointer("/meta/durationMs") {
                    entry["durationMs"] = duration.clone();
                }
                entry["failed"] =
                    json!(message.get("isError").and_then(Value::as_bool) == Some(true));
                if entry["failed"] == true {
                    entry["error_result"] = data
                        .pointer("/meta/result")
                        .cloned()
                        .unwrap_or_else(|| json!(output));
                }
            }
            "user/message" | "assistant/message" => {
                let message = data.get("message").unwrap_or(&Value::Null);
                let text = model_text(message.get("content").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    entry["text"] = json!(if kind == "user/message" {
                        text
                    } else {
                        observer_excerpt(&text, 700)
                    });
                }
            }
            "observer/consult" => {
                entry["question"] = data.get("question").cloned().unwrap_or(Value::Null)
            }
            "observer/consult_reply" => {
                entry["answer"] = data.get("answer").cloned().unwrap_or(Value::Null)
            }
            "observer/plan_review" | "observer/progress_review" => {
                entry["summary"] = data.get("summary").cloned().unwrap_or(Value::Null);
                entry["suggestions"] = data
                    .get("suggestions")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
            }
            "subagent/start" => {
                entry["prompt"] = data.get("prompt").cloned().unwrap_or(Value::Null)
            }
            "subagent/end" => entry["status"] = data.get("status").cloned().unwrap_or(Value::Null),
            "organizer/decision" => entry["decision"] = data["decision"].clone(),
            "worker/yield" => {
                entry["work_id"] = data["output"]["work_id"].clone();
                entry["worker_return"] = data["output"]["worker_return"].clone();
                entry["summary"] = data["output"]["summary"].clone();
                entry["visual_check_result"] = data["output"]["visual_check_result"].clone();
            }
            "visual/model_received" => entry["dispatch"] = data["manifest"].clone(),
            "visual/model_failed" => {
                entry["dispatch"] = data["manifest"].clone();
                entry["error"] = data["error"].clone();
            }
            "visual/service_result" => {
                entry["dispatch"] = data["manifest"].clone();
                entry["result"] = data["result"].clone();
            }
            "visual/check_result" => entry["result"] = data["result"].clone(),
            _ => {}
        }
        trace.push(entry);
    }
    Ok(trace)
}

pub(crate) fn observer_known_read_targets(
    trace: &[Value],
    task_id: &str,
    limit: usize,
) -> Vec<Value> {
    let mut targets = trace
        .iter()
        .filter(|entry| entry["failed"] != true)
        .filter_map(|entry| {
            let mut target = entry.get("codeTarget")?.clone();
            if target["code_hash"].as_str().is_none_or(str::is_empty) {
                return None;
            }
            if let Some(seq) = entry["seq"].as_i64() {
                target["source_event_seq"] = json!(seq);
                target["source_event_id"] = json!(format!("{task_id}:event:{seq}"));
            }
            target["source_task_id"] = json!(task_id);
            Some(target)
        })
        .collect::<Vec<_>>();
    targets
        .sort_by_key(|target| std::cmp::Reverse(target["source_event_seq"].as_i64().unwrap_or(0)));
    targets.truncate(limit);
    targets
}

fn observer_source_path(root: &std::path::Path, reference: &str) -> Option<String> {
    let reference = reference.split("::").next().unwrap_or(reference);
    let file = reference
        .rsplit_once(':')
        .filter(|(_, suffix)| {
            !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit() || c == '-')
        })
        .map(|(file, _)| file)
        .unwrap_or(reference);
    let path = crate::file_edit::workspace_path(root, file)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    Some(if cfg!(windows) {
        path.to_ascii_lowercase()
    } else {
        path
    })
}

/// A plain file reference means its latest observed read. Explicit source_reads
/// select the exact read/hash instead. Conflicting or unordered versions remain
/// uncertain; checking today's file bytes cannot establish which read a claim used.
fn observer_fact_sources(
    root: &std::path::Path,
    item: &Value,
    refs: &[String],
    targets: &[Value],
) -> Option<Vec<Value>> {
    if refs.is_empty() {
        return None;
    }
    let selectors = match item.get("source_reads") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            if items.is_empty() {
                None
            } else {
                Some(items)
            }
        }
        _ => return None,
    };
    let mut sources = std::collections::BTreeMap::new();
    for reference in refs {
        let path = observer_source_path(root, reference)?;
        let mut candidates = targets
            .iter()
            .filter(|target| {
                target["file_path"]
                    .as_str()
                    .and_then(|file| observer_source_path(root, file))
                    .as_ref()
                    == Some(&path)
            })
            .collect::<Vec<_>>();
        if let Some(selectors) = selectors {
            let selected = selectors
                .iter()
                .filter(|selector| {
                    selector["file_path"]
                        .as_str()
                        .and_then(|file| observer_source_path(root, file))
                        .as_ref()
                        == Some(&path)
                })
                .collect::<Vec<_>>();
            if selected.is_empty()
                || selected.iter().any(|selector| {
                    selector["code_hash"].as_str().is_none_or(str::is_empty)
                        && selector["source_event_seq"].as_i64().is_none()
                        && selector["source_event_id"]
                            .as_str()
                            .is_none_or(str::is_empty)
                })
            {
                return None;
            }
            let matches = |selector: &&Value, target: &&Value| {
                [
                    "scope",
                    "symbol_id",
                    "qualified_name",
                    "code_hash",
                    "source_event_id",
                    "source_event_seq",
                    "source_task_id",
                ]
                .iter()
                .all(|field| {
                    selector
                        .get(*field)
                        .is_none_or(|value| value.is_null() || value == &target[*field])
                })
            };
            if selected
                .iter()
                .any(|selector| !candidates.iter().any(|target| matches(selector, target)))
            {
                return None;
            }
            candidates.retain(|target| selected.iter().any(|selector| matches(selector, target)));
            if candidates
                .iter()
                .map(|target| target["code_hash"].as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                > 1
            {
                return None;
            }
        } else {
            let versions = candidates
                .iter()
                .map(|target| target["code_hash"].as_str())
                .collect::<std::collections::BTreeSet<_>>();
            if versions.len() > 1
                && candidates
                    .iter()
                    .any(|target| target["source_event_seq"].as_i64().is_none())
            {
                return None;
            }
        }
        let latest = candidates
            .iter()
            .map(|target| target["source_event_seq"].as_i64().unwrap_or(0))
            .max()?;
        candidates.retain(|target| target["source_event_seq"].as_i64().unwrap_or(0) == latest);
        if candidates
            .iter()
            .map(|target| target["code_hash"].as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != 1
        {
            return None;
        }
        sources.insert(path, (*candidates.first()?).clone());
    }
    Some(sources.into_values().collect())
}

pub(crate) async fn save_observer_lessons(
    state: &AgentServiceState,
    task_id: &str,
    prompt: &str,
    report: &Value,
    trace: &[Value],
) -> anyhow::Result<Value> {
    let known_read_targets = observer_known_read_targets(trace, task_id, 24);
    // Lessons belong to the project the task worked in, not the server's own root.
    let project = crate::session_browser::task_workspace(state.workspace.root(), task_id)?;
    let project_root = project.display().to_string();
    let source_trace = serde_json::to_string(
        &trace
            .iter()
            .filter(|entry| {
                matches!(
                    entry.get("type").and_then(Value::as_str),
                    Some("tool/call" | "tool/result")
                )
            })
            .collect::<Vec<_>>(),
    )?;
    let action_count = trace
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("tool/call"))
        .count();
    let changed_inputs = trace
        .iter()
        .filter(|entry| {
            entry.get("type").and_then(Value::as_str) == Some("tool/call")
                && matches!(
                    entry.get("tool").and_then(Value::as_str),
                    Some("write_file" | "replace_range" | "edit_file")
                )
        })
        .filter_map(|entry| entry.get("input").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let verification_command_seen = trace.iter().any(|entry| {
        entry.get("type").and_then(Value::as_str) == Some("tool/call")
            && matches!(
                entry.get("tool").and_then(Value::as_str),
                Some("run_program" | "run_project_script")
            )
            && entry
                .get("input")
                .and_then(Value::as_str)
                .is_some_and(|input| {
                    let lower = input.to_ascii_lowercase();
                    lower.contains("test") || lower.contains("check") || lower.contains("build")
                })
    });
    let summary = truncate(
        report.get("summary").and_then(Value::as_str).unwrap_or(""),
        1000,
    );
    let path_review = truncate(
        report
            .get("path_review")
            .and_then(Value::as_str)
            .unwrap_or(""),
        2400,
    );
    let shortening = observer_string_list(report.get("shortening_opportunities"), 5, 700);
    let findings = report.get("work_findings").and_then(Value::as_array).into_iter().flatten()
            .take(5).filter_map(|item| {
                let finding = item.get("finding").and_then(Value::as_str)?.trim();
                if finding.is_empty() { return None; }
                let source_refs = supported_source_refs(item.get("source_refs"), &source_trace);
                let requested_refs=observer_string_list(item.get("source_refs"),6,240);
                let all_sources_observed=requested_refs.len()==source_refs.len() && item["source_refs"].as_array().is_none_or(|refs|refs.len()<=6);
                let source_reads=if all_sources_observed {observer_fact_sources(state.workspace.root(),item,&source_refs,&known_read_targets)}else{None};
                let confirmed = item.get("certainty").and_then(Value::as_str) == Some("confirmed")
                    && source_reads.is_some() && !matches!(item["evidence_kind"].as_str(),Some("visual_observation"|"worker_interaction"|"inferred_cause"));
                Some(json!({
                    "finding":truncate(finding, 900),
                    "relevance":truncate(item.get("relevance").and_then(Value::as_str).unwrap_or(""), 700),
                    "watch_for":truncate(item.get("watch_for").and_then(Value::as_str).unwrap_or(""), 700),
                    "sourceRefs":source_refs,
                    "sourceReads":source_reads,
                    "evidence_kind":item["evidence_kind"],"visual_artifact_ids":item["visual_artifact_ids"],
                    "certainty":if confirmed { "confirmed" } else { "uncertain" },
                }))
            }).collect::<Vec<_>>();
    let route_shortcuts = report.get("route_shortcuts").and_then(Value::as_array)
            .into_iter().flatten().take(5).filter_map(|item| {
                let situation = item.get("situation").and_then(Value::as_str)?.trim();
                let look_first = item.get("look_first").and_then(Value::as_str)?.trim();
                if situation.is_empty() || look_first.is_empty() { return None; }
                Some(json!({
                    "situation":truncate(situation, 350),
                    "lookFirst":truncate(look_first, 500),
                    "avoid":truncate(item.get("avoid").and_then(Value::as_str).unwrap_or(""), 500),
                    "exceptions":truncate(item.get("exceptions").and_then(Value::as_str).unwrap_or(""), 500),
                }))
            }).collect::<Vec<_>>();
    let memory = report.get("memory").unwrap_or(&Value::Null);
    let mut memory_recorded = false;
    let mut memory_path = String::new();
    let mut memory_error = String::new();
    let memory_summary = truncate(
        memory.get("summary").and_then(Value::as_str).unwrap_or(""),
        500,
    );
    let confirmed = findings
        .iter()
        .filter(|item| item.get("certainty").and_then(Value::as_str) == Some("confirmed"))
        .collect::<Vec<_>>();
    if memory.get("record").and_then(Value::as_bool) == Some(true)
        && !memory_summary.trim().is_empty()
        && (!confirmed.is_empty() || (!route_shortcuts.is_empty() && action_count >= 2))
    {
        let fact_entries = confirmed.iter().map(|item| {
                let finding = item.get("finding").and_then(Value::as_str).unwrap_or("");
                let relation = item.get("relevance").and_then(Value::as_str).unwrap_or("");
                let watch = item.get("watch_for").and_then(Value::as_str).unwrap_or("");
                let sources = observer_string_list(item.get("sourceRefs"), 6, 240).join(", ");
                let versions=item["sourceReads"].as_array().unwrap();
                let version_text=versions.iter().map(|source|format!("{}@{}",source["file_path"].as_str().unwrap_or(""),source["code_hash"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ");
                (format!("- {finding}\n  适用：{relation}\n  来源：{sources}\n  来源版本：{version_text}\n  边界：{watch}"),item["sourceReads"].clone(),observer_string_list(item.get("sourceRefs"),6,240))
            }).collect::<Vec<_>>();
        let routes = route_shortcuts
            .iter()
            .map(|item| {
                format!(
                    "- 场景：{}；先看：{}；避免：{}；例外：{}",
                    item.get("situation").and_then(Value::as_str).unwrap_or(""),
                    item.get("lookFirst").and_then(Value::as_str).unwrap_or(""),
                    item.get("avoid").and_then(Value::as_str).unwrap_or(""),
                    item.get("exceptions").and_then(Value::as_str).unwrap_or(""),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let files_changed = if !changed_inputs.is_empty() {
            observer_string_list(memory.get("files_changed"), 20, 300)
                .into_iter()
                .filter(|path| changed_inputs.contains(path))
                .collect()
        } else {
            Vec::new()
        };
        let source_refs = confirmed
            .iter()
            .flat_map(|item| observer_string_list(item.get("sourceRefs"), 6, 240))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let request = crate::memory::RecordWorkMemoryRequest {
            workspace_root: project_root.clone(),
            summary: memory_summary.clone(),
            files_changed,
            implementation: String::new(),
            tests: if verification_command_seen {
                truncate(
                    memory.get("tests").and_then(Value::as_str).unwrap_or(""),
                    900,
                )
            } else {
                String::new()
            },
            risks: truncate(
                memory.get("risks").and_then(Value::as_str).unwrap_or(""),
                1200,
            ),
            source_task_id: Some(task_id.to_owned()),
            kind: "observer".to_owned(),
            applies_to: truncate(
                memory
                    .get("applies_to")
                    .and_then(Value::as_str)
                    .unwrap_or(prompt),
                600,
            ),
            source_refs,
        };
        let workspace = state.workspace.clone();
        match tokio::task::spawn_blocking(move || {
            let mut last = None;
            let mut entries = fact_entries
                .into_iter()
                .map(|(content, versions, refs)| ("项目事实", content, versions, refs))
                .collect::<Vec<_>>();
            entries.push(("路径经验", routes, json!([]), vec![]));
            for (suffix, content, versions, refs) in entries {
                if content.is_empty() {
                    continue;
                }
                let mut entry = request.clone();
                entry.summary = format!("{} · {suffix}", entry.summary);
                entry.implementation = truncate(&content, 5000);
                entry.source_refs = refs;
                let response = workspace.record_work_memory(entry)?;
                crate::memory::bind_observer_sources(
                    &project,
                    &response.recorded.summary,
                    &response.recorded.implementation,
                    &versions,
                )?;
                last = Some(response);
            }
            Ok::<_, anyhow::Error>(last)
        })
        .await
        {
            Ok(Ok(Some(response))) => {
                memory_recorded = true;
                memory_path = response.memory_path;
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => memory_error = truncate(&error.to_string(), 700),
            Err(error) => memory_error = truncate(&error.to_string(), 700),
        }
    }
    let mut description_requests = Vec::new();
    for context in report
        .get("symbol_contexts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(4)
    {
        let file = context
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        let scope = context
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("symbol");
        let id = context
            .get("symbol_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let qualified = context
            .get("qualified_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let role = context
            .get("business_role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let target = known_read_targets.iter().find(|target| {
            target.get("file_path").and_then(Value::as_str) == Some(file)
                && target.get("scope").and_then(Value::as_str) == Some(scope)
                && (scope == "file"
                    || (!id.is_empty()
                        && target.get("symbol_id").and_then(Value::as_str) == Some(id))
                    || (!qualified.is_empty()
                        && target.get("qualified_name").and_then(Value::as_str) == Some(qualified)))
        });
        if role.is_empty() {
            continue;
        }
        if let Some(target) = target {
            description_requests.push(crate::memory::RecordSymbolBusinessContextRequest {
                workspace_root: project_root.clone(),
                symbol_id: target
                    .get("symbol_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                symbol_name: target
                    .get("symbol_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                language: target
                    .get("language")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                file_path: file.to_owned(),
                belongs_to_area: truncate(
                    context
                        .get("belongs_to_area")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    160,
                ),
                business_role: truncate(role, 500),
                common_tasks: Vec::new(),
                read_when: truncate(
                    context
                        .get("read_when")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    350,
                ),
                avoid_when: String::new(),
                risks: String::new(),
                confidence: 0.8,
                description: crate::symbol_description::RecordOptions {
                    qualified_name: target
                        .get("qualified_name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    keywords: observer_string_list(context.get("keywords"), 12, 60),
                    scope: scope.to_owned(),
                    source: "observer".to_owned(),
                    expected_code_hash: target
                        .get("code_hash")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            });
        }
    }
    let workspace = state.workspace.clone();
    let descriptions=tokio::task::spawn_blocking(move || {
            description_requests.into_iter().map(|request|match workspace.record_symbol_business_context(request) {Ok(response)=>json!({"recorded":true,"file":response.recorded.file_path,"name":response.recorded.qualified_name,"stale":response.recorded.stale}),Err(error)=>json!({"recorded":false,"error":truncate(&error.to_string(),400)})}).collect::<Vec<_>>()
        }).await.unwrap_or_default();
    Ok::<Value, anyhow::Error>(json!({
        "status":"completed","summary":summary,"pathReview":path_review,
        "shorteningOpportunities":shortening,"workFindings":findings,"routeShortcuts":route_shortcuts,
        "memoryRecorded":memory_recorded,"memorySummary":if memory_recorded { memory_summary } else { String::new() },
        "memoryPath":memory_path,"memoryError":memory_error,"codeDescriptions":descriptions,
    }))
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
    let board_task_id=task_id.clone();
    let root = state.workspace.root().to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = open_db(&root)?;
        Ok(conn.query_row("SELECT id,prompt,model,status,created_at,updated_at,parent_task_id,provider_name,reasoning_effort,fast_mode FROM agent_tasks WHERE id=?1", [&task_id], |r| Ok(json!({"task_id":r.get::<_,String>(0)?,"prompt":r.get::<_,String>(1)?,"model":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"created_at":r.get::<_,i64>(4)?,"updated_at":r.get::<_,i64>(5)?,"parent_task_id":r.get::<_,Option<String>>(6)?,"provider_name":r.get::<_,Option<String>>(7)?,"reasoning_effort":r.get::<_,Option<String>>(8)?,"fast_mode":r.get::<_,i64>(9)? != 0}))).optional()?)
    }).await;
    match result {
        Ok(Ok(Some(task))) => match open_db(state.workspace.root()).and_then(|conn|crate::session_browser::decorate(&conn,state.workspace.root(),task)) {
            Ok(mut task) => {
                match crate::main_task_anchor::board(state.workspace.root(),&board_task_id,None) {
                    Ok(board)=>task["task_board"]=json!(board),
                    Err(error)=>task["task_board_error"]=json!(error.to_string()),
                }
                Json(task).into_response()
            },
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":"could not read sidebar state"}))).into_response(),
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
    State(state): State<AgentServiceState>, Path(task): Path<String>, Json(args): Json<Value>,
) -> impl IntoResponse {
    let result=(|| -> anyhow::Result<Value> {
        let conn=open_db(state.workspace.root())?;
        let plugin:Option<String>=conn.query_row("SELECT plugin_id FROM plugin_sessions WHERE task_id=?1",[&task],|r|r.get(0)).optional()?;
        anyhow::ensure!(plugin.is_none(),"plugin task decisions require the authenticated plugin session");
        crate::main_task_anchor::decide_definition(state.workspace.root(),&task,
            args["version"].as_i64().context("proposal version required")?,
            args["approve"].as_bool().context("approve must be boolean")?,"agent-ui")
    })();
    match result {Ok(value)=>Json(value).into_response(),Err(error)=>(StatusCode::CONFLICT,Json(json!({"error":error.to_string()}))).into_response()}
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
        Ok(Ok(Some(task))) => match open_db(state.workspace.root()).and_then(|conn|crate::session_browser::decorate(&conn,state.workspace.root(),task)) {
            Ok(task) => Json(task).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":"could not read sidebar state"}))).into_response(),
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
    let mut bytes=[0u8;16];
    if getrandom::fill(&mut bytes).is_ok() {
        return bytes.iter().map(|byte|format!("{byte:02x}")).collect();
    }
    static FALLBACK: AtomicU64=AtomicU64::new(0);
    format!("{:08x}{:016x}{:08x}",std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64,
        FALLBACK.fetch_add(1,Ordering::Relaxed) as u32)
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

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
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
        let name=tool["name"].as_str().unwrap_or_default(); let description=tool["description"].as_str().unwrap_or_default();
        let mut schema=tool.get("inputSchema").cloned().unwrap_or_else(||json!({"type":"object","properties":{}}));
        if let Some(props)=schema.get_mut("properties").and_then(Value::as_object_mut) {props.remove("workspace_root");}
        if let Some(required)=schema.get_mut("required").and_then(Value::as_array_mut) {required.retain(|name| name.as_str()!=Some("workspace_root"));}
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
    use axum::routing::post;
    #[tokio::test]
    async fn temporary_observer_keeps_current_failure_intact_without_previous_context() {
        let root = std::env::temp_dir().join(format!("observer-original-window-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        open_db(&root).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let received = captured.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener,axum::Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>| {
                let received=received.clone();async move {
                    received.lock().unwrap().push(body);
                    Json(json!({"choices":[{"message":{"role":"assistant","content":"{\"assessment\":\"uncertain\",\"summary\":\"ORIGINAL_OBSERVER_REPLY\"}"}}]}))
                }
            }))).await.unwrap();
        });
        let mut state = test_state(&root, address);
        state.observer_provider_url = state.provider_url.clone();
        let failure = format!("{} ORIGINAL_FAILURE_TAIL", "诊断信息".repeat(5000));
        let input = json!({"stage":"handoff","identity":{"task_id":"task"},"request":{"goal":"load"},"recent_activity":[
            {"seq":0,"kind":"assistant/message","payload":{"role":"assistant","content":null,"tool_calls":[{"id":"upload","type":"function","function":{"name":"browser_upload","arguments":"{\"path\":\"sample.pptx\"}"}}]}},
            {"seq":1,"kind":"tool/call","call_id":"upload","payload":{"name":"browser_upload","arguments":"{\"path\":\"sample.pptx\"}"}},
            {"seq":2,"kind":"tool/result","call_id":"upload","is_error":true,"payload":{"reason":failure}}]});
        observer_json_response(
            &state,
            "fake-model",
            "review",
            input,
            1000,
            "task",
            json!({}),
        )
        .await
        .unwrap();
        observer_json_response(&state,"fake-model","review",json!({"stage":"progress","identity":{"task_id":"task"},"recent_activity":[
            {"seq":3,"kind":"assistant/message","payload":{"role":"assistant","content":null,"tool_calls":[{"id":"recovery","type":"function","function":{"name":"read_file","arguments":"{}"}}]}},
            {"seq":4,"kind":"tool/result","call_id":"recovery","payload":{"recovery":"NEW_RECOVERY"}}]}),1000,"task",json!({})).await.unwrap();
        let requests = captured.lock().unwrap();
        let text = requests[1]["messages"].to_string();
        assert!(!text.contains("Called method browser_upload"));
        assert!(!text.contains("ORIGINAL_FAILURE_TAIL"));
        assert!(!text.contains("ORIGINAL_OBSERVER_REPLY"));
        assert!(text.contains("NEW_RECOVERY"));
        assert!(
            requests[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["content"]
                    .as_str()
                    .and_then(|text| serde_json::from_str::<Value>(
                        crate::session_history::original_text(text)
                    )
                    .ok())
                    .is_some_and(|value| value["reason"] == failure))
        );
        drop(requests);
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    pub(crate) fn test_state(
        root: &std::path::Path,
        address: std::net::SocketAddr,
    ) -> AgentServiceState {
        AgentServiceState {
            workspace: Arc::new(crate::tools::Workspace::new(root.to_path_buf()).unwrap()),
            client: Client::new(),
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

            observer_enabled: false,
            observer_provider: String::new(),
            observer_provider_url: String::new(),
            observer_api_key: String::new(),
            observer_model: String::new(),
            observer_inherits_model: true,
            visual: Default::default(),
            visual_tool_settings: Default::default(),
            context_agent_settings: Default::default(),
            delegate_agent_settings: Default::default(),
            config: None,
            config_path: None,
            tool_catalog: None,
            plugin_cancel: CancellationToken::new(),
            cancellations: Arc::default(),
        }
    }

    async fn record_observer_source_read(state: &AgentServiceState, task: &str, file: &str) {
        let call_id = format!("read_{}", uuid_like());
        let result = state
            .workspace
            .read_file(crate::tools::ReadFileRequest {
                workspace_root: None,
                path: file.into(),
                max_bytes: 1024,
            })
            .unwrap();
        emit(state.workspace.root(),task,"tool/call",json!({"turn":1,"step":1,"callId":call_id,"name":"read_file","arguments":json!({"path":file}).to_string()})).await.unwrap();
        emit(state.workspace.root(),task,"tool/result",json!({"turn":1,"step":1,"message":{"toolCallId":call_id,"source":{"callId":call_id},"content":"source read","isError":false},"meta":{"result":result}})).await.unwrap();
    }

    #[tokio::test]
    async fn observer_latest_fact_after_source_edit_uses_one_version_and_remains_searchable() {
        let root =
            std::env::temp_dir().join(format!("agent-observer-fact-versions-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let task = "task";
        let state = test_state(&root, "127.0.0.1:9".parse().unwrap());
        {
            let conn = open_db(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','mapping','fake','running',0,0)",[]).unwrap();
        }
        let report = |finding: &str| {
            json!({"work_findings":[{"finding":finding,"source_refs":["source.rs"],"certainty":"confirmed"}],
            "memory":{"record":true,"summary":"Mapping fact","applies_to":"mapping"}})
        };
        std::fs::write(root.join("source.rs"), "old source").unwrap();
        record_observer_source_read(&state, task, "source.rs").await;
        let old_trace = load_observer_work_trace(&root, task).unwrap();
        let old_report = report("old mapping is present");
        assert_eq!(
            save_observer_lessons(&state, task, "mapping", &old_report, &old_trace)
                .await
                .unwrap()["memoryRecorded"],
            true
        );
        std::fs::write(root.join("source.rs"), "new source").unwrap();
        record_observer_source_read(&state, task, "source.rs").await;
        let trace = load_observer_work_trace(&root, task).unwrap();
        let new_report = report("latest mapping is present");
        let saved = save_observer_lessons(&state, task, "mapping", &new_report, &trace)
            .await
            .unwrap();
        assert_eq!(saved["memoryRecorded"], true);
        assert_eq!(
            saved["workFindings"][0]["sourceReads"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let read = &saved["workFindings"][0]["sourceReads"][0];
        assert_eq!(
            read["code_hash"],
            crate::symbol_description::content_hash(b"new source")
        );
        assert_eq!(
            read["source_event_seq"].as_i64(),
            trace
                .iter()
                .filter(|e| e.get("codeTarget").is_some())
                .filter_map(|e| e["seq"].as_i64())
                .max()
        );
        assert!(
            read["source_event_id"]
                .as_str()
                .unwrap()
                .starts_with("task:event:")
        );
        let list = || {
            state
                .workspace
                .list_work_memory(crate::memory::ListWorkMemoryRequest {
                    workspace_root: root.to_string_lossy().into_owned(),
                    limit: 10,
                })
                .unwrap()
                .memories
        };
        let search = |query: &str| {
            state
                .workspace
                .search_work_memory(crate::memory::SearchWorkMemoryRequest {
                    workspace_root: root.to_string_lossy().into_owned(),
                    query: query.into(),
                    limit: 10,
                })
                .unwrap()
                .matches
        };
        assert_eq!(list().len(), 2);
        let current = search("mapping");
        assert_eq!(current.len(), 1);
        assert!(current[0].implementation.contains("latest mapping"));
        let manifest:String=crate::database::init_db(&root).unwrap().query_row("SELECT sources FROM observer_memory_sources WHERE summary=?1 AND implementation=?2",params![current[0].summary,current[0].implementation],|row|row.get(0)).unwrap();
        let manifest: Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(manifest.as_array().unwrap().len(), 1);
        assert_eq!(manifest[0], *read);
        save_observer_lessons(&state, task, "mapping", &new_report, &trace)
            .await
            .unwrap();
        assert_eq!(list().len(), 2, "same fact and version stay deduplicated");
        // Two facts may deliberately cite different versions; do not combine
        // their manifests into one memory that can never match a current file.
        let mut both = new_report.clone();
        let mut old_finding = old_report["work_findings"][0].clone();
        old_finding["source_reads"] = json!(observer_known_read_targets(&old_trace, task, 8));
        both["work_findings"] = json!([old_finding, new_report["work_findings"][0]]);
        save_observer_lessons(&state, task, "mapping", &both, &trace)
            .await
            .unwrap();
        assert_eq!(list().len(), 2);
        assert_eq!(search("mapping").len(), 1);
        // Multi-file facts retain one matching observed version for each file.
        std::fs::write(root.join("other.rs"), "other source").unwrap();
        record_observer_source_read(&state, task, "other.rs").await;
        let trace = load_observer_work_trace(&root, task).unwrap();
        let mut multiple = report("cross mapping is consistent");
        multiple["work_findings"][0]["source_refs"] = json!(["source.rs", "other.rs"]);
        let saved = save_observer_lessons(&state, task, "mapping", &multiple, &trace)
            .await
            .unwrap();
        assert_eq!(
            saved["workFindings"][0]["sourceReads"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(search("cross mapping").len(), 1);
        std::fs::write(root.join("other.rs"), "changed other source").unwrap();
        assert!(search("cross mapping").is_empty());
        assert_eq!(list().len(), 3);
        // Conflicting reads with no event order cannot prove which version a
        // fact refers to; keep it uncertain instead of choosing today's bytes.
        let mut unordered = trace.clone();
        for entry in &mut unordered {
            entry.as_object_mut().unwrap().remove("seq");
        }
        let unknown = save_observer_lessons(
            &state,
            task,
            "mapping",
            &report("unversioned mapping"),
            &unordered,
        )
        .await
        .unwrap();
        assert_eq!(unknown["workFindings"][0]["certainty"], "uncertain");
        assert_eq!(unknown["memoryRecorded"], false);
        let mut wrong = report("unsupported mapping");
        wrong["work_findings"][0]["source_reads"] =
            json!([{"file_path":"source.rs","code_hash":"unobserved"}]);
        let unknown = save_observer_lessons(&state, task, "mapping", &wrong, &trace)
            .await
            .unwrap();
        assert_eq!(unknown["workFindings"][0]["certainty"], "uncertain");
        assert_eq!(unknown["memoryRecorded"], false);
        assert_eq!(list().len(), 3);
        wrong["work_findings"][0]["source_reads"] =
            json!([read,{"file_path":"source.rs","code_hash":"unobserved"}]);
        let unknown = save_observer_lessons(&state, task, "mapping", &wrong, &trace)
            .await
            .unwrap();
        assert_eq!(unknown["workFindings"][0]["certainty"], "uncertain");
        assert_eq!(unknown["memoryRecorded"], false);
        wrong["work_findings"][0]["source_reads"] = json!(
            observer_known_read_targets(&trace, task, 24)
                .into_iter()
                .filter(|target| target["file_path"].as_str().unwrap().ends_with("source.rs"))
                .collect::<Vec<_>>()
        );
        let unknown = save_observer_lessons(&state, task, "mapping", &wrong, &trace)
            .await
            .unwrap();
        assert_eq!(unknown["workFindings"][0]["certainty"], "uncertain");
        assert_eq!(unknown["memoryRecorded"], false);
        assert_eq!(list().len(), 3);
        let mut incomplete = report("partially evidenced mapping");
        incomplete["work_findings"][0]["source_refs"] = json!(["source.rs", "unread.rs"]);
        let unknown = save_observer_lessons(&state, task, "mapping", &incomplete, &trace)
            .await
            .unwrap();
        assert_eq!(unknown["workFindings"][0]["certainty"], "uncertain");
        assert_eq!(unknown["memoryRecorded"], false);
        assert_eq!(list().len(), 3);
        drop(state);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn retrospective_projection_keeps_old_necessary_original_for_role_selection() {
        let older_call = json!({"seq":2,"kind":"assistant/message","payload":{"role":"assistant","content":null,"tool_calls":[{"id":"old","type":"function","function":{"name":"read_file","arguments":"{}"}}]}});
        let older = json!({"seq":3,"kind":"tool/result","call_id":"old","payload":{"needed":"original dependency"}});
        let large_call = json!({"seq":8,"kind":"assistant/message","payload":{"role":"assistant","content":null,"tool_calls":[{"id":"large","type":"function","function":{"name":"read_file","arguments":"{}"}}]}});
        let large = json!({"seq":9,"kind":"tool/result","call_id":"large","payload":"旧".repeat(crate::context_window::MAX_TOKENS)});
        let latest = json!({"seq":12,"kind":"worker/yield","payload":"latest result"});
        let view = crate::session_history::process_view(
            &json!({"records":[older_call,older,large_call,large,latest]}),
        )
        .unwrap();
        assert_eq!(
            view.sources,
            vec![Some(2), Some(2), Some(8), Some(8), Some(12)]
        );
        assert!(
            view.messages[1]["content"]
                .as_str()
                .unwrap()
                .contains("original dependency")
        );
        assert!(
            crate::context_window::estimate(&json!(view.messages))
                > crate::context_window::MAX_TOKENS
        );
    }

    #[test]
    fn observer_trace_covers_all_actions_and_incremental_reads_start_after_the_cursor() {
        let root = std::env::temp_dir().join(format!("observer-full-trace-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let task = "task";
        {
            let conn = open_db(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        }
        for index in 0..340 {
            append_event(
                &root,
                task,
                "tool/call",
                json!({"callId":format!("call_{index}"),"name":"workspace_info","arguments":"{}"}),
                None,
            )
            .unwrap();
        }
        let full = load_observer_work_trace(&root, task).unwrap();
        assert_eq!(
            full.len(),
            340,
            "final review retains middle actions as well as the ends"
        );
        assert_eq!(full[0]["seq"], 0);
        assert_eq!(full[339]["seq"], 339);
        let recent = load_observer_trace_since(&root, task, 330).unwrap();
        assert_eq!(recent.len(), 9);
        assert_eq!(recent[0]["seq"], 331);
        std::fs::remove_dir_all(root).unwrap();
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
