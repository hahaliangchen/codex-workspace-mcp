//! Standalone task API for the local Rust agent. Events are append-only so a
//! client can recover a task's trajectory after reconnecting.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
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
const ORGANIZER_REQUEST_RETRY_LIMIT: usize = 1;
#[cfg(test)]
const ORGANIZER_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(not(test))]
const ORGANIZER_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const SUBAGENT_MAX_STEPS: usize = 12;
const MAX_STEP_LIMIT: usize = 100;
const OBSERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    ReadOnly,
    WorkspaceWrite,
    #[default]
    FullAccess,
}

impl PermissionMode {
    fn allows(self, name: &str) -> bool {
        match name {
            "write_file" | "replace_range" | "edit_file" => !matches!(self, Self::ReadOnly),
            // Process execution uses the host account without a filesystem
            // sandbox. Only full access may invoke it; structured writes retain path checks.
            "run_program" | "install_dependencies" | "run_project_script" | "stop_project_process"
                | "browser_open" | "browser_read" | "browser_wait" | "browser_diagnostics" | "browser_click" | "browser_press_key" | "browser_upload" | "browser_screenshot" => matches!(self, Self::FullAccess),
            "browser_close" => matches!(self, Self::FullAccess),
            _ => true,
        }
    }

    fn guidance(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only: project files may be inspected, but file writes and project/native-program execution are disabled. Internal index and memory storage remains available.",
            Self::WorkspaceWrite => "workspace_write: structured file writes inside the workspace are allowed; npm project scripts and native-program execution are disabled because the host does not sandbox processes.",
            Self::FullAccess => "full_access: registered file-write, project execution, and allowlisted native-program tools are allowed. Structured file tools still enforce workspace path boundaries.",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentProviderRoute {
    pub url: String,
    pub api_key: String,
    pub models: HashMap<String, String>,
}

#[derive(Clone)]
pub struct AgentServiceState {
    pub workspace: Arc<crate::tools::Workspace>,
    pub client: Client,
    pub provider_url: String,
    pub api_key: String,
    pub provider_name: String,
    pub provider_routes: HashMap<String, AgentProviderRoute>,
    pub default_model: String,
    pub model_map: HashMap<String, String>,
    pub reasoning_effort: Option<String>,
    pub fast_mode: bool,
    pub permission_mode: PermissionMode,
    pub enable_subagent: bool,
    pub expert_provider_name: String,
    pub expert_provider_url: String,
    pub expert_api_key: String,
    pub expert_model: String,
    pub subagent_inherits_model: bool,
    pub observer_enabled: bool,
    pub observer_provider: String,
    pub observer_provider_url: String,
    pub observer_api_key: String,
    pub observer_model: String,
    pub observer_inherits_model: bool,
    pub visual:crate::visual_artifacts::VisualSettings,
    pub config: Option<Arc<tokio::sync::RwLock<crate::ai_proxy::AiProxyConfig>>>,
    pub config_path: Option<std::path::PathBuf>,
    pub tool_catalog: Option<Arc<crate::plugin_builtin::ToolCatalog>>,
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
    if active.get(task_id).is_some_and(|entry| entry.run_id == run_id) {
        active.remove(task_id);
    }
}

async fn set_active_flow_node(state: &AgentServiceState, task_id: &str, turn: usize, node: &str) {
    let active = state.cancellations.lock().await.get(task_id).cloned();
    if let Some(active) = active {
        *active.flow_node.lock().await = format!("turn_{turn}:{node}");
    }
}

async fn finish_cancelled_run(
    state: &AgentServiceState, root: &std::path::Path, task_id: &str,
    turn: usize, active_node: &str,
) -> anyhow::Result<()> {
    let active = state.cancellations.lock().await.get(task_id).cloned();
    let interrupted = if let Some(active) = active {
        active.node_interrupt.lock().await.clone()
    } else { None };
    if interrupted.is_some() && !active_node.is_empty() {
        emit(root, task_id, "flow/node_state", json!({
            "turn": turn, "id": active_node, "status": "interrupted"
        })).await?;
    }
    emit(root, task_id, "turn/end", json!({
        "turn": turn,
        "reason": {"kind": "aborted", "reason": if interrupted.is_some() { "node_interrupted" } else { "cancelled" }}
    })).await?;
    finish(state, task_id, if interrupted.is_some() { "interrupted" } else { "cancelled" }).await
}

impl AgentServiceState {
    pub async fn with_latest_config(mut self) -> Self {
        if let Some(shared) = self.config.clone() {
            if let Some(path) = self.config_path.clone() {
                if let Ok(Ok(latest)) = tokio::task::spawn_blocking(move || crate::ai_proxy::load_config(&path)).await {
                    *shared.write().await = latest;
                }
            }
            let config = shared.read().await;
            crate::ai_proxy::apply_agent_config(&mut self, &config);
        }
        self
    }

    fn use_provider(&mut self, name: &str) -> bool {
        if self.provider_routes.is_empty() { return name == self.provider_name; }
        let Some(route) = self.provider_routes.get(name).cloned() else { return false };
        self.provider_name = name.to_owned();
        self.provider_url = route.url.clone();
        self.api_key = route.api_key.clone();
        self.model_map = route.models;
        if self.observer_inherits_model {
            self.observer_provider = name.to_owned();
            self.observer_provider_url = route.url.clone();
            self.observer_api_key = route.api_key.clone();
        }
        if self.subagent_inherits_model {
            self.expert_provider_url = route.url;
            self.expert_api_key = route.api_key;
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
    if routes.is_empty() { return Some(default_name.to_owned()); }
    if let Some(name) = saved_name.filter(|name| !name.is_empty()) {
        return routes.get(name).filter(|route| route.models.contains_key(model)).map(|_| name.to_owned());
    }
    if routes.get(default_name).is_some_and(|route| route.models.contains_key(model)) {
        return Some(default_name.to_owned());
    }
    let mut matches = routes.iter().filter(|(_, route)| route.models.contains_key(model));
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
pub async fn create_draft_task(State(state): State<AgentServiceState>) -> impl IntoResponse {
    let state = state.with_latest_config().await;
    if state.plugin_cancel.is_cancelled() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"agent task plugin is stopped"}))).into_response();
    }
    let task_id = format!("task_{}", uuid_like());
    let root = state.workspace.root().to_path_buf();
    let model = state.default_model.clone();
    let provider_name = state.provider_name.clone();
    let reasoning_effort = state.reasoning_effort.clone();
    let fast_mode = state.fast_mode;
    let id = task_id.clone();
    let inserted = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let conn = open_db(&root)?;
        conn.execute(
            "INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at,provider_name,reasoning_effort,fast_mode) VALUES (?1,'',?2,'draft',?3,?3,?4,?5,?6)",
            params![id, model, now(), provider_name, reasoning_effort, fast_mode],
        )?;
        Ok(())
    }).await;
    match inserted {
        Ok(Ok(())) => (StatusCode::CREATED, Json(json!({"task_id":task_id}))).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"could not create session"}))).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ContinueTaskRequest {
    pub prompt: String,
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
    #[serde(default)]
    pub resume_target: Option<ResumeTarget>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ResumeTarget { pub work_id:String, pub revision:usize, pub request_id:usize }

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
    Ready {
        model: String,
        provider_name: String,
        reasoning_effort: Option<String>,
        fast_mode: bool,
        permission_mode: PermissionMode,
        turn: usize,
        history: Vec<Value>,
        subagent_used: bool,
    },
    NotFound,
    ChildTask,
    Busy,
    ModelUnavailable,
    InvalidNode(String),
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

fn now() -> i64 {
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
    let _ = conn.execute("ALTER TABLE agent_tasks ADD COLUMN parent_task_id TEXT", []);
    let _ = conn.execute("ALTER TABLE agent_tasks ADD COLUMN provider_name TEXT", []);
    let _ = conn.execute("ALTER TABLE agent_tasks ADD COLUMN reasoning_effort TEXT", []);
    let _ = conn.execute("ALTER TABLE agent_tasks ADD COLUMN fast_mode INTEGER NOT NULL DEFAULT 0", []);
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
            if let Ok((kind, data)) = conn.query_row(
                "SELECT kind,data FROM agent_task_events WHERE task_id=?1 AND kind IN ('step/start','step/end') ORDER BY seq DESC LIMIT 1",
                [&task_id],
                |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)),
            ) && kind == "step/start" {
                let step_data: Value = serde_json::from_str(&data)?;
                let step_seq: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(seq), -1) + 1 FROM agent_task_events WHERE task_id=?1",
                    [&task_id],
                    |row| row.get(0),
                )?;
                conn.execute(
                    "INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES (?1,?2,?3,'step/end',?4)",
                    params![task_id,step_seq,now(),step_data.to_string()],
                )?;
            }
            let turn: usize = conn.query_row(
                "SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='turn/start' ORDER BY seq DESC LIMIT 1",
                [&task_id],
                |row| row.get::<_, String>(0),
            ).optional()?.and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|data| data.get("turn").and_then(Value::as_u64))
                .map(|turn| turn as usize).unwrap_or(1);
            let seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM agent_task_events WHERE task_id=?1",
                [&task_id],
                |row| row.get(0),
            )?;
            conn.execute(
                "INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES (?1,?2,?3,'turn/end',?4)",
                params![task_id,seq,now(),json!({"turn":turn,"reason":{"kind":"interrupted"}}).to_string()],
            )?;
            conn.execute(
                "UPDATE agent_tasks SET status='interrupted',updated_at=?2 WHERE id=?1",
                params![task_id, now()],
            )?;
        }
        cleanup_terminal_task_data(&mut conn, None, true)?;
        Ok(())
    })
    .await??;
    Ok(())
}

/// Replace repeated execution snapshots with one resumable Flow record after a run ends.
/// The execution history (tools, progress, Flow and messages), task metadata,
/// Observer conclusions, notebook data, memories and code indexes are retained.
fn cleanup_terminal_task_data(
    conn: &mut rusqlite::Connection,
    task_id: Option<&str>,
    compact: bool,
) -> anyhow::Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let task_filter = "status IN ('completed','failed','cancelled','max_steps','interrupted') AND (?1 IS NULL OR id=?1)";
    if let Some(task_id) = task_id {
        let status: Option<String> = tx.query_row("SELECT status FROM agent_tasks WHERE id=?1", [task_id], |r| r.get(0)).optional()?;
        if !status.as_deref().is_some_and(|s| matches!(s, "completed" | "failed" | "cancelled" | "max_steps" | "interrupted")) {
            return Ok(());
        }
    }
    let task_selector = format!("SELECT id FROM agent_tasks WHERE {task_filter}");
    // One continuation record replaces the repeated execution snapshots. Keep
    // the exact current frame so a paused node can resume after cleanup.
    let sessions={
        let mut stmt=tx.prepare(&format!("SELECT task_id,data FROM agent_task_events e WHERE task_id IN ({task_selector}) AND kind='execution/commit' AND seq=(SELECT MAX(seq) FROM agent_task_events WHERE task_id=e.task_id AND kind='execution/commit')"))?;
        let rows=stmt.query_map([task_id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id,raw) in sessions {
        let mut data:Value=serde_json::from_str(&raw)?;
        let worker=tx.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='worker/work_state' ORDER BY seq DESC LIMIT 1",
            [&id],|row|row.get::<_,String>(0)).optional()?;
        if let Some(state)=worker.and_then(|raw|serde_json::from_str::<Value>(&raw).ok()).and_then(|data|data.get("state").cloned()) {
            data["worker_state"]=state;
        }
        tx.execute("DELETE FROM agent_task_events WHERE task_id=?1 AND kind='flow/session'",[&id])?;
        append_event_tx(&tx,&id,"flow/session",&data,None)?;
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
        let active_tasks: i64 = conn.query_row("SELECT COUNT(*) FROM agent_tasks WHERE status='running'", [], |r| r.get(0))?;
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
    }).await??;
    Ok(())
}

fn append_event(
    root: &std::path::Path,
    task_id: &str,
    kind: &str,
    data: Value,
    surface_op: Option<&str>,
) -> anyhow::Result<()> {
    let mut conn = open_db(root)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    append_event_tx(&tx,task_id,kind,&data,surface_op)?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn append_event_tx(tx:&rusqlite::Transaction,task_id:&str,kind:&str,data:&Value,surface_op:Option<&str>)->anyhow::Result<i64> {
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM agent_task_events WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data,surface_op) VALUES (?1,?2,?3,?4,?5,?6)",
        params![task_id, seq, now(), kind, serde_json::to_string(&data)?, surface_op],
    )?;
    tx.execute(
        "UPDATE agent_tasks SET updated_at=?2 WHERE id=?1",
        params![task_id, now()],
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
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"agent task plugin is stopped"}))).into_response();
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
            state.model_map.values().any(|value| value == &requested).then_some(requested)
        }) {
            Some(model) => model,
            None => return (StatusCode::BAD_REQUEST, Json(json!({"error":"model is not configured"}))).into_response(),
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
    state
        .cancellations
        .lock()
        .await
        .insert(task_id.clone(), ActiveTask { run_id: run_id.clone(), token: cancel.clone(),
            flow_node: Arc::new(Mutex::new("turn_1:1".into())), node_interrupt: Arc::new(Mutex::new(None)) });
    let runner_state = state.clone();
    let runner_id = task_id.clone();
    let max_steps = request
        .max_steps
        .unwrap_or(DEFAULT_MAX_STEPS)
        .clamp(1, MAX_STEP_LIMIT);
    spawn_supervised_task(runner_state, runner_id, run_id, model, prompt,
        max_steps, cancel, 1, Vec::new(), false, false);
    (StatusCode::ACCEPTED, Json(json!({"task_id":task_id,"status":"running","events_url":format!("/agent/tasks/{task_id}/events")}))).into_response()
}

fn model_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content.as_array().map(|blocks| {
        blocks.iter().filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>().join("\n")
    }).unwrap_or_default()
}

fn observer_output_failed(output: &str) -> bool {
    serde_json::from_str::<Value>(output).ok().is_some_and(|value| {
        value["ok"] == false
            || matches!(value["status"].as_str(),Some("failed"|"timeout"|"invalid_condition"|"invalid_selector"))
            || value.get("status").and_then(Value::as_i64).is_some_and(|status| status != 0)
            || value.get("error").is_some()
            || value.get("observer_interrupted").and_then(Value::as_bool) == Some(true)
    })
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

fn worker_visual_context(task:&str,scheduler:&crate::work_scheduler::WorkScheduler)->crate::visual_artifacts::VisualContext {
    let mut allowed=Vec::new();
    if let Some(order)=scheduler.order() {
        if let Ok(deliveries)=scheduler.resolve_dependency_deliveries(order) {
            for delivery in deliveries {
                for id in delivery["visual_artifact_ids"].as_array().into_iter().flatten().chain(delivery["exported_data"]["visual_artifact_ids"].as_array().into_iter().flatten()) {
                    if let Some(id)=id.as_str() {allowed.push(id.to_owned());}
                }
            }
        }
    }
    crate::visual_artifacts::VisualContext {identity:crate::observer_service::identity(task,scheduler,scheduler.id()),allowed_artifact_ids:allowed,
        execution_epoch:scheduler.frame().map(|f|f.epoch).unwrap_or(0),related_source_versions:json!(scheduler.frame().map(|f|&f.versions)),
        current_page:scheduler.frame().map(|f|f.browser_page.clone()).unwrap_or(Value::Null),
        host_current_artifact_ids:scheduler.frame().map(|f|f.current_visual_artifact_ids.clone()).unwrap_or_default(),
        original_artifact_ids:scheduler.frame().map(|f|f.visual_original_artifact_ids.clone()).unwrap_or_default(),..Default::default()}
}


pub(crate) async fn observer_json_response(
    state: &AgentServiceState,
    model: &str,
    system: &str,
    context: Value,
    max_tokens: usize,
    task_id: &str,
    mut trace_metadata: Value,
) -> anyhow::Result<String> {
    let history_guidance=if context["stage"]=="retrospective" {
        "Review the current human goal's complete chronological task_history, including failures, recovery and earlier Observer messages. If coverage is partial, use read_session_history with the provided history_reference to read omitted process records before claiming a complete review. Report any unread scope explicitly."
    } else {"Use read_session_history only when this input lacks information needed for your answer. Saved history is available on demand; do not request it merely to reconstruct everything."};
    let system = format!("{}\n\n{system}\n{history_guidance}", include_str!("../prompts/shared_reasoning.md").trim());
    let mut observer_view=crate::context_window::HistoryView::default();
    observer_view.prepend(json!({"role":"system","content":system}));
    if context["stage"]=="retrospective" {
        let mut header=context.clone();header["task_history"].as_object_mut().unwrap().remove("records");
        observer_view.messages.push(json!({"role":"user","content":crate::session_history::plain_context(&header)}));observer_view.sources.push(None);
        let process=crate::session_history::process_view(&context["task_history"]);
        observer_view.messages.extend(process.messages);observer_view.sources.extend(process.sources);
    } else {
        let mut header=context.clone();
        if context["recent_activity"].is_array() {
            if let Some(fields)=header.as_object_mut() {
                for field in ["recent_activity","organizer_decision","delivery","resolved_inputs","http_observations","node_facts","current_facts","activity_summary","visual_requests","visual_check_result","materials"] { fields.remove(field); }
            }
        }
        observer_view.messages.push(json!({"role":"user","content":crate::session_history::plain_context(&header)}));observer_view.sources.push(None);
        let process=crate::session_history::process_view(&json!({"records":context["recent_activity"]}));
        observer_view.messages.extend(process.messages);observer_view.sources.extend(process.sources);

    }
    let mut request_body = json!({"model":model,"stream":false,"max_tokens":max_tokens,
        "messages":observer_view.messages,
        "tools":[crate::session_history::tool()],"tool_choice":"auto"});
    let visual_context=crate::visual_artifacts::VisualContext {turn:trace_metadata["turn"].as_u64().map(|turn|turn as usize),identity:context["identity"].clone(),execution_epoch:context["execution_epoch"].as_u64().unwrap_or(0) as usize,related_source_versions:context["related_source_versions"].clone(),current_page:context["task_page"].clone(),host_current_artifact_ids:context["host_current_artifact_ids"].as_array().into_iter().flatten().filter_map(|id|id.as_str().map(str::to_owned)).collect(),allowed_artifact_ids:context["allowed_visual_artifact_ids"].as_array().into_iter().flatten().filter_map(|id|id.as_str().map(str::to_owned)).collect(),..Default::default()};
    // An observation never selects or captures images on the model's behalf.
    let ids:Vec<String>=Vec::new();
    let scope=format!("request:{}:node:{}",context["identity"]["request_id"].as_u64().unwrap_or(1),context["identity"]["node_id"].as_str().unwrap_or("request"));
    trace_metadata["context_window"]=crate::context_rebuild::prepare(state,model,"observer",task_id,&scope,&context,&mut request_body,observer_view.clone(),trace_metadata.clone(),&state.plugin_cancel,OBSERVER_REQUEST_TIMEOUT).await?;
    let mut lookup_messages=Vec::<Value>::new();
    let mut wire_body=request_body.clone();
    let visual_dispatch=crate::visual_artifacts::prepare_request(state,"observer",model,&visual_context,&ids,context["request"]["goal"].as_str().unwrap_or("review current visual delivery"),&mut wire_body).await?;
    crate::context_window::prepare(&mut wire_body,2)?;
    trace_metadata["visual_dispatch"]=visual_dispatch.clone();
    trace_metadata["actor"] = json!("observer");
    let mut trace = crate::request_context::record(state.workspace.root(),task_id,trace_metadata.clone(),&wire_body).await;
    let result = tokio::time::timeout(OBSERVER_REQUEST_TIMEOUT, async {
        // A small ordinary tool loop lets Observer retrieve missing history.
        // It does not dispatch Worker tasks or change the scheduler.
        let mut lookup_round=0;
        loop {
            let followup_trace=if lookup_round>0 {
                let mut view=observer_view.clone();view.sources.extend(std::iter::repeat_n(None,lookup_messages.len()));view.messages.extend(lookup_messages.clone());
                trace_metadata["context_window"]=crate::context_rebuild::prepare(state,model,"observer",task_id,&scope,&context,&mut request_body,view,trace_metadata.clone(),&state.plugin_cancel,OBSERVER_REQUEST_TIMEOUT).await?;
                wire_body=request_body.clone();
                let dispatch=crate::visual_artifacts::prepare_request(state,"observer",model,&visual_context,&ids,context["request"]["goal"].as_str().unwrap_or("review current visual delivery"),&mut wire_body).await?;
                crate::context_window::prepare(&mut wire_body,2)?;
                let mut metadata=trace_metadata.clone();metadata["history_lookup_round"]=json!(lookup_round);
                metadata["visual_dispatch"]=dispatch;
                crate::request_context::record(state.workspace.root(),task_id,metadata,&wire_body).await
            } else {trace.take()};
            let response=async {
                let response = state.client.post(format!("{}/chat/completions", state.observer_provider_url.trim_end_matches('/')))
                    .bearer_auth(&state.observer_api_key).header("x-codex-visual-dispatch","task-owned").json(&wire_body).send().await?;
                crate::visual_probe::successful_response(response).await?.json::<Value>().await.map_err(anyhow::Error::from)
            }.await;
            crate::request_context::finish(followup_trace,if response.is_ok(){"completed"}else{"failed"},
                json!({"response":response.as_ref().ok(),"error":response.as_ref().err().map(|error|format!("{error:#}"))})).await;
            let body=response?;let message=&body["choices"][0]["message"];
            let calls=message["tool_calls"].as_array().filter(|calls|!calls.is_empty());
            if let Some(calls)=calls {
                let exchange=json!({"role":"assistant","content":message["content"],"tool_calls":calls});
                lookup_messages.push(exchange.clone());request_body["messages"].as_array_mut().unwrap().push(exchange);
                for call in calls {
                    let args=call["function"]["arguments"].as_str().unwrap_or("{}");
                    let output=if call["function"]["name"]=="read_session_history" {
                        match serde_json::from_str::<Value>(args) {
                            Ok(args)=>crate::session_history::read(state.workspace.root(),task_id,&args).await,
                            Err(error)=>Err(error.into()),
                        }
                    } else {Err(anyhow::anyhow!("Observer tool is not available: {}",call["function"]["name"]))};
                    let is_error=output.is_err();
                    let result=output.unwrap_or_else(|error|json!({"error":format!("{error:#}")}));
                    emit(state.workspace.root(),task_id,"observer/history_read",json!({"actor":"observer","turn":trace_metadata["turn"],"stage":"retrospective",
                        "identity":context["identity"],"callId":call["id"],"name":call["function"]["name"],"is_error":is_error,
                        "review_id":context["review_id"],"arguments":args,"result":result})).await?;
                    let message=json!({"role":"tool","tool_call_id":call["id"],"content":result.to_string()});
                    lookup_messages.push(message.clone());request_body["messages"].as_array_mut().unwrap().push(message);
                    let images=crate::session_history::history_image_messages(&result);
                    lookup_messages.extend(images.clone());request_body["messages"].as_array_mut().unwrap().extend(images);
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
        Ok((raw,usage)) => {
            if !ids.is_empty() {emit(state.workspace.root(),task_id,"visual/model_received",json!({"manifest":visual_dispatch,"actor":"observer","review_id":context["review_id"]})).await?;}
            let mut validation=Value::Null;
            if let Some(result)=serde_json::from_str::<Value>(raw.trim().trim_start_matches("```json").trim_end_matches("```").trim()).ok().filter(Value::is_object) {
                if let Some(check)=result.get("visual_check_result").filter(|_|!ids.is_empty()) {
                    let requests=vec![visual_dispatch.clone()];
                    validation=match crate::visual_artifacts::validate_check(state.workspace.root(),&visual_context,check,&requests,"observer") {
                        Ok(check)=>{emit(state.workspace.root(),task_id,"visual/check_result",json!({"result":check,"review_id":context["review_id"]})).await?;check},
                        Err(error)=>{let failure=json!({"error":error.to_string(),"review_id":context["review_id"],"actor":"observer","visual_request":visual_dispatch});
                            emit(state.workspace.root(),task_id,"visual/check_error",failure.clone()).await?;failure},
                    };
                }
            }
            crate::request_context::finish(trace,"completed",json!({"response_chars":raw.chars().count(),"response_text":raw,"usage":usage,"visual_validation":validation})).await;
            Ok(raw)
        },
        Err(error) => {
            if !ids.is_empty() {
                emit(state.workspace.root(),task_id,"visual/model_failed",json!({"manifest":visual_dispatch,"actor":"observer",
                    "review_id":context["review_id"],"error":format!("{error:#}")})).await?;
            }
            crate::request_context::finish(trace,if error.is::<tokio::time::error::Elapsed>() {"timeout"}else{"failed"},
                json!({"error":format!("{error:#}")})).await;
            if ids.is_empty() {Err(error)}else{Err(crate::visual_probe::VisualModelFailure {manifest:visual_dispatch,error}.into())}
        },
    }
}

fn observer_string_list(value: Option<&Value>, limit: usize, char_limit: usize) -> Vec<String> {
    value.and_then(Value::as_array).into_iter().flatten()
        .filter_map(Value::as_str)
        .map(|item| truncate(item.trim(), char_limit))
        .filter(|item| !item.is_empty())
        .take(limit)
        .collect()
}

fn supported_source_refs(value: Option<&Value>, trace_text: &str) -> Vec<String> {
    let normalized_trace = trace_text.replace('\\', "/").replace("//", "/");
    observer_string_list(value, 6, 240).into_iter().filter(|reference| {
        let path = reference.rsplit_once(':')
            .filter(|(_, suffix)| suffix.chars().any(|ch| ch.is_ascii_digit())
                && suffix.chars().all(|ch| ch.is_ascii_digit() || ch == '-'))
            .map(|(path, _)| path).unwrap_or(reference);
        let normalized = path.replace('\\', "/");
        let file = normalized.rsplit('/').next().unwrap_or("")
            .split("::").next().unwrap_or("");
        file.len() >= 4 && normalized_trace.contains(file)
    }).collect()
}

pub(crate) fn load_observer_work_trace(root: &std::path::Path, task_id: &str) -> anyhow::Result<Vec<Value>> {
    load_observer_trace_since(root,task_id,-1)
}

pub(crate) fn load_observer_trace_since(root: &std::path::Path, task_id: &str, after_seq:i64) -> anyhow::Result<Vec<Value>> {
    let conn = open_db(root)?;
    let mut stmt = conn.prepare(
        "SELECT seq,kind,data FROM agent_task_events WHERE task_id=?1 AND seq>?2 ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![task_id,after_seq], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    let raw_events = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut tool_names = HashMap::<String, String>::new();
    for (_, kind, raw) in &raw_events {
        if kind != "tool/call" { continue; }
        let Ok(data) = serde_json::from_str::<Value>(raw) else { continue };
        if let (Some(id), Some(name)) = (
            data.get("callId").and_then(Value::as_str),
            data.get("name").and_then(Value::as_str),
        ) {
            tool_names.insert(id.to_owned(), name.to_owned());
        }
    }
    let mut trace = Vec::new();
    for (seq, kind, raw) in raw_events {
        if !matches!(kind.as_str(), "flow/plan" | "worker/progress" | "tool/call" | "tool/result"
            | "assistant/message" | "observer/consult" | "observer/consult_reply"
            | "observer/plan_review" | "observer/progress_review"
            | "subagent/start" | "subagent/end" | "turn/end" | "user/message"
            | "organizer/decision" | "worker/yield" | "visual/model_received" | "visual/model_failed" | "visual/service_result" | "visual/check_result") {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&raw) else { continue };
        let mut entry = json!({"seq":seq,"type":kind});
        for field in ["turn", "step", "nodeId", "reason"] {
            if let Some(value) = data.get(field) { entry[field] = value.clone(); }
        }
        match kind.as_str() {
            "flow/plan" => {
                let nodes = data.get("nodes").and_then(Value::as_array).into_iter().flatten().take(128)
                    .map(|node| json!({
                        "id":node.get("id"),"title":node.get("title"),"kind":node.get("kind"),
                        "status":node.get("status"),"description":node.get("description"),
                        "parent_id":node.get("parent_id"),"objective":node.get("objective"),"done_when":node.get("done_when"),
                    })).collect::<Vec<_>>();
                let edges = data.get("edges").and_then(Value::as_array).into_iter().flatten().take(128)
                    .map(|edge| json!({"source":edge.get("source"),"target":edge.get("target")}))
                    .collect::<Vec<_>>();
                entry["nodes"] = json!(nodes);
                entry["edges"] = json!(edges);
            }
            "worker/progress" => {
                for field in ["purpose", "knownConditions", "nextAction", "nodeResult"] {
                    if let Some(value) = data.get(field) { entry[field] = value.clone(); }
                }
            }
            "tool/call" => {
                entry["tool"] = data.get("name").cloned().unwrap_or(Value::Null);
                entry["input"] = json!(observer_excerpt(data.get("arguments").and_then(Value::as_str).unwrap_or(""), 700));
            }
            "tool/result" => {
                let message = data.get("message").unwrap_or(&Value::Null);
                let call_id = message.pointer("/source/callId").and_then(Value::as_str)
                    .or_else(|| message.get("toolCallId").and_then(Value::as_str)).unwrap_or("");
                let tool_name=tool_names.get(call_id).cloned().or_else(||conn.query_row(
                    "SELECT json_extract(data,'$.name') FROM agent_task_events WHERE task_id=?1 AND kind='tool/call' AND json_extract(data,'$.callId')=?2 ORDER BY seq DESC LIMIT 1",
                    params![task_id,call_id],|row|row.get::<_,String>(0)).optional().ok().flatten()).unwrap_or_default();
                entry["tool"] = json!(tool_name);
                let output = data.pointer("/meta/result").map(Value::to_string)
                    .unwrap_or_else(|| model_text(message.get("content").unwrap_or(&Value::Null)));
                entry["outcome"] = json!(observer_excerpt(&output, 700));
                // Preserve a compact, versioned target even when the source excerpt is shortened.
                if let Some(result)=data.pointer("/meta/result") {
                    let tool=entry.get("tool").and_then(Value::as_str).unwrap_or("");
                    if tool.starts_with("read_") && tool.ends_with("_symbol") {
                        if let Some(symbol)=result.get("symbol") {
                            entry["codeTarget"]=json!({"scope":"symbol","symbol_id":symbol.get("id"),"symbol_name":symbol.get("name"),"file_path":symbol.get("file_path"),"language":tool.trim_start_matches("read_").trim_end_matches("_symbol"),"qualified_name":result.pointer("/description/qualified_name"),"code_hash":result.pointer("/description/code_hash")});
                        }
                    } else if tool=="read_file" {
                        let file=result.get("path").and_then(Value::as_str).unwrap_or("");
                        let language=std::path::Path::new(file).extension().and_then(|ext|ext.to_str()).and_then(crate::symbol_description::language);
                        if let Some(language)=language {entry["codeTarget"]=json!({"scope":"file","file_path":file,"language":language,"code_hash":result.get("code_hash")});}
                    }
                }
                if let Some(duration) = data.pointer("/meta/durationMs") { entry["durationMs"] = duration.clone(); }
                entry["failed"] = json!(message.get("isError").and_then(Value::as_bool) == Some(true));
                if entry["failed"]==true {entry["error_result"]=data.pointer("/meta/result").cloned().unwrap_or_else(||json!(output));}
            }
            "user/message" | "assistant/message" => {
                let message = data.get("message").unwrap_or(&Value::Null);
                let text = model_text(message.get("content").unwrap_or(&Value::Null));
                if !text.trim().is_empty() { entry["text"] = json!(if kind=="user/message" {text}else{observer_excerpt(&text, 700)}); }
            }
            "observer/consult" => entry["question"] = data.get("question").cloned().unwrap_or(Value::Null),
            "observer/consult_reply" => entry["answer"] = data.get("answer").cloned().unwrap_or(Value::Null),
            "observer/plan_review" | "observer/progress_review" => {
                entry["summary"] = data.get("summary").cloned().unwrap_or(Value::Null);
                entry["suggestions"] = data.get("suggestions").cloned().unwrap_or_else(|| json!([]));
            }
            "subagent/start" => entry["prompt"] = data.get("prompt").cloned().unwrap_or(Value::Null),
            "subagent/end" => entry["status"] = data.get("status").cloned().unwrap_or(Value::Null),
            "organizer/decision" => entry["decision"]=data["decision"].clone(),
            "worker/yield" => {
                entry["work_id"]=data["output"]["work_id"].clone();
                entry["worker_return"]=data["output"]["worker_return"].clone();
                entry["summary"]=data["output"]["summary"].clone();
                entry["visual_check_result"]=data["output"]["visual_check_result"].clone();
            },
            "visual/model_received" => entry["dispatch"]=data["manifest"].clone(),
            "visual/model_failed" => {entry["dispatch"]=data["manifest"].clone();entry["error"]=data["error"].clone();},
            "visual/service_result" => {entry["dispatch"]=data["manifest"].clone();entry["result"]=data["result"].clone();},
            "visual/check_result" => entry["result"]=data["result"].clone(),
            _ => {}
        }
        trace.push(entry);
    }
    Ok(trace)
}

pub(crate) fn observer_known_read_targets(trace:&[Value],task_id:&str,limit:usize)->Vec<Value> {
    let mut targets=trace.iter().filter(|entry|entry["failed"]!=true).filter_map(|entry| {
        let mut target=entry.get("codeTarget")?.clone();
        if target["code_hash"].as_str().is_none_or(str::is_empty) {return None;}
        if let Some(seq)=entry["seq"].as_i64() {
            target["source_event_seq"]=json!(seq);target["source_event_id"]=json!(format!("{task_id}:event:{seq}"));
        }
        target["source_task_id"]=json!(task_id);
        Some(target)
    }).collect::<Vec<_>>();
    targets.sort_by_key(|target|std::cmp::Reverse(target["source_event_seq"].as_i64().unwrap_or(0)));
    targets.truncate(limit);targets
}

fn observer_source_path(root:&std::path::Path,reference:&str)->Option<String> {
    let reference=reference.split("::").next().unwrap_or(reference);
    let file=reference.rsplit_once(':').filter(|(_,suffix)|!suffix.is_empty() && suffix.chars().all(|c|c.is_ascii_digit() || c=='-'))
        .map(|(file,_)|file).unwrap_or(reference);
    let path=crate::file_edit::workspace_path(root,file).ok()?.to_string_lossy().replace('\\',"/");
    Some(if cfg!(windows){path.to_ascii_lowercase()}else{path})
}

/// A plain file reference means its latest observed read. Explicit source_reads
/// select the exact read/hash instead. Conflicting or unordered versions remain
/// uncertain; checking today's file bytes cannot establish which read a claim used.
fn observer_fact_sources(root:&std::path::Path,item:&Value,refs:&[String],targets:&[Value])->Option<Vec<Value>> {
    if refs.is_empty(){return None;}
    let selectors=match item.get("source_reads") {
        None|Some(Value::Null)=>None,
        Some(Value::Array(items))=>if items.is_empty(){None}else{Some(items)},
        _=>return None,
    };
    let mut sources=std::collections::BTreeMap::new();
    for reference in refs {
        let path=observer_source_path(root,reference)?;
        let mut candidates=targets.iter().filter(|target|target["file_path"].as_str().and_then(|file|observer_source_path(root,file)).as_ref()==Some(&path)).collect::<Vec<_>>();
        if let Some(selectors)=selectors {
            let selected=selectors.iter().filter(|selector|selector["file_path"].as_str().and_then(|file|observer_source_path(root,file)).as_ref()==Some(&path)).collect::<Vec<_>>();
            if selected.is_empty() || selected.iter().any(|selector|selector["code_hash"].as_str().is_none_or(str::is_empty) && selector["source_event_seq"].as_i64().is_none() && selector["source_event_id"].as_str().is_none_or(str::is_empty)){return None;}
            let matches=|selector:&&Value,target:&&Value|["scope","symbol_id","qualified_name","code_hash","source_event_id","source_event_seq","source_task_id"].iter()
                .all(|field|selector.get(*field).is_none_or(|value|value.is_null() || value==&target[*field]));
            if selected.iter().any(|selector|!candidates.iter().any(|target|matches(selector,target))){return None;}
            candidates.retain(|target|selected.iter().any(|selector|matches(selector,target)));
            if candidates.iter().map(|target|target["code_hash"].as_str()).collect::<std::collections::BTreeSet<_>>().len()>1 {return None;}
        } else {
            let versions=candidates.iter().map(|target|target["code_hash"].as_str()).collect::<std::collections::BTreeSet<_>>();
            if versions.len()>1 && candidates.iter().any(|target|target["source_event_seq"].as_i64().is_none()) {return None;}
        }
        let latest=candidates.iter().map(|target|target["source_event_seq"].as_i64().unwrap_or(0)).max()?;
        candidates.retain(|target|target["source_event_seq"].as_i64().unwrap_or(0)==latest);
        if candidates.iter().map(|target|target["code_hash"].as_str()).collect::<std::collections::BTreeSet<_>>().len()!=1 {return None;}
        sources.insert(path,(*candidates.first()?).clone());
    }
    Some(sources.into_values().collect())
}

pub(crate) async fn save_observer_lessons(state:&AgentServiceState,task_id:&str,prompt:&str,report:&Value,trace:&[Value])->anyhow::Result<Value> {
        let known_read_targets=observer_known_read_targets(trace,task_id,24);
        let source_trace = serde_json::to_string(&trace.iter().filter(|entry| {
            matches!(entry.get("type").and_then(Value::as_str), Some("tool/call" | "tool/result"))
        }).collect::<Vec<_>>())?;
        let action_count = trace.iter().filter(|entry| entry.get("type").and_then(Value::as_str) == Some("tool/call")).count();
        let changed_inputs = trace.iter().filter(|entry| {
            entry.get("type").and_then(Value::as_str) == Some("tool/call")
                && matches!(entry.get("tool").and_then(Value::as_str), Some("write_file" | "replace_range" | "edit_file"))
        }).filter_map(|entry| entry.get("input").and_then(Value::as_str)).collect::<Vec<_>>().join("\n");
        let verification_command_seen = trace.iter().any(|entry| {
            entry.get("type").and_then(Value::as_str) == Some("tool/call")
                && matches!(entry.get("tool").and_then(Value::as_str),Some("run_program"|"run_project_script"))
                && entry.get("input").and_then(Value::as_str).is_some_and(|input| {
                    let lower = input.to_ascii_lowercase();
                    lower.contains("test") || lower.contains("check") || lower.contains("build")
                })
        });
        let summary = truncate(report.get("summary").and_then(Value::as_str).unwrap_or(""), 1000);
        let path_review = truncate(report.get("path_review").and_then(Value::as_str).unwrap_or(""), 2400);
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
        let memory_summary = truncate(memory.get("summary").and_then(Value::as_str).unwrap_or(""), 500);
        let confirmed = findings.iter().filter(|item| item.get("certainty").and_then(Value::as_str) == Some("confirmed"))
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
            let routes = route_shortcuts.iter().map(|item| format!(
                "- 场景：{}；先看：{}；避免：{}；例外：{}",
                item.get("situation").and_then(Value::as_str).unwrap_or(""),
                item.get("lookFirst").and_then(Value::as_str).unwrap_or(""),
                item.get("avoid").and_then(Value::as_str).unwrap_or(""),
                item.get("exceptions").and_then(Value::as_str).unwrap_or(""),
            )).collect::<Vec<_>>().join("\n");
            let files_changed = if !changed_inputs.is_empty() {
                observer_string_list(memory.get("files_changed"), 20, 300)
                    .into_iter().filter(|path| changed_inputs.contains(path)).collect()
            } else { Vec::new() };
            let source_refs = confirmed.iter().flat_map(|item| observer_string_list(item.get("sourceRefs"), 6, 240))
                .collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>();
            let request = crate::memory::RecordWorkMemoryRequest {
                workspace_root: state.workspace.root().display().to_string(),
                summary:memory_summary.clone(),
                files_changed,
                implementation:String::new(),
                tests:if verification_command_seen {
                    truncate(memory.get("tests").and_then(Value::as_str).unwrap_or(""), 900)
                } else { String::new() },
                risks:truncate(memory.get("risks").and_then(Value::as_str).unwrap_or(""), 1200),
                source_task_id:Some(task_id.to_owned()),
                kind:"observer".to_owned(),
                applies_to:truncate(memory.get("applies_to").and_then(Value::as_str).unwrap_or(prompt), 600),
                source_refs,
            };
            let workspace = state.workspace.clone();
            match tokio::task::spawn_blocking(move || {
                let mut last=None;
                let mut entries=fact_entries.into_iter().map(|(content,versions,refs)|("项目事实",content,versions,refs)).collect::<Vec<_>>();
                entries.push(("路径经验",routes,json!([]),vec![]));
                for (suffix,content,versions,refs) in entries {
                    if content.is_empty(){continue;}
                    let mut entry=request.clone();entry.summary=format!("{} · {suffix}",entry.summary);entry.implementation=truncate(&content,5000);
                    entry.source_refs=refs;
                    let response=workspace.record_work_memory(entry)?;
                    crate::memory::bind_observer_sources(workspace.root(),&response.recorded.summary,&response.recorded.implementation,&versions)?;
                    last=Some(response);
                }
                Ok::<_,anyhow::Error>(last)
            }).await {
                Ok(Ok(Some(response))) => {memory_recorded=true;memory_path=response.memory_path;},
                Ok(Ok(None))=>{},
                Ok(Err(error))=>memory_error=truncate(&error.to_string(),700),
                Err(error)=>memory_error=truncate(&error.to_string(),700),
            }
        }
        let mut description_requests=Vec::new();
        for context in report.get("symbol_contexts").and_then(Value::as_array).into_iter().flatten().take(4) {
            let file=context.get("file_path").and_then(Value::as_str).unwrap_or("");
            let scope=context.get("scope").and_then(Value::as_str).unwrap_or("symbol");
            let id=context.get("symbol_id").and_then(Value::as_str).unwrap_or("");
            let qualified=context.get("qualified_name").and_then(Value::as_str).unwrap_or("");
            let role=context.get("business_role").and_then(Value::as_str).unwrap_or("").trim();
            let target=known_read_targets.iter().find(|target|target.get("file_path").and_then(Value::as_str)==Some(file) && target.get("scope").and_then(Value::as_str)==Some(scope) && (scope=="file" || (!id.is_empty() && target.get("symbol_id").and_then(Value::as_str)==Some(id)) || (!qualified.is_empty() && target.get("qualified_name").and_then(Value::as_str)==Some(qualified))));
            if role.is_empty() {continue;}
            if let Some(target)=target {
                description_requests.push(crate::memory::RecordSymbolBusinessContextRequest {
                    workspace_root:state.workspace.root().display().to_string(),
                    symbol_id:target.get("symbol_id").and_then(Value::as_str).unwrap_or("").to_owned(),
                    symbol_name:target.get("symbol_name").and_then(Value::as_str).unwrap_or("").to_owned(),
                    language:target.get("language").and_then(Value::as_str).unwrap_or("").to_owned(),
                    file_path:file.to_owned(), belongs_to_area:truncate(context.get("belongs_to_area").and_then(Value::as_str).unwrap_or(""),160),
                    business_role:truncate(role,500),common_tasks:Vec::new(),read_when:truncate(context.get("read_when").and_then(Value::as_str).unwrap_or(""),350),avoid_when:String::new(),risks:String::new(),confidence:0.8,
                    description:crate::symbol_description::RecordOptions {qualified_name:target.get("qualified_name").and_then(Value::as_str).unwrap_or("").to_owned(),keywords:observer_string_list(context.get("keywords"),12,60),scope:scope.to_owned(),source:"observer".to_owned(),expected_code_hash:target.get("code_hash").and_then(Value::as_str).map(str::to_owned)},
                });
            }
        }
        let workspace=state.workspace.clone();
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

fn history_from_events(root: &std::path::Path, events: &[(String, Value)]) -> (Vec<Value>, usize, bool) {
    let mut history = Vec::new();
    let mut last_turn = 0;
    let mut subagent_used = false;
    let mut pending_calls: Vec<String> = Vec::new();
    let close_pending = |history: &mut Vec<Value>, pending: &mut Vec<String>| {
        for call_id in pending.drain(..) {
            history.push(json!({"role":"tool","tool_call_id":call_id,"content":"The previous turn ended before this tool returned."}));
        }
    };
    for (kind, data) in events {
        let before=history.len();
        match kind.as_str() {
            "turn/start" => {
                close_pending(&mut history, &mut pending_calls);
                last_turn = last_turn.max(data.get("turn").and_then(Value::as_u64).unwrap_or(1) as usize);
            }
            "user/message" => {
                let content = data.pointer("/message/content").unwrap_or(&Value::Null);
                let model_content = data.get("model_content").and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::composer_catalog::expand_skill_prompt(root, &model_text(content)));
                history.push(json!({"role":"user","content":model_content}));
            }
            "visual/input_result" if data["actor"]=="worker" => {
                if let Some(message)=data.get("message").filter(|message|message.is_object()) {
                    history.push(message.clone());
                }
            }
            "assistant/message" => {
                close_pending(&mut history, &mut pending_calls);
                let content = data.pointer("/message/content").unwrap_or(&Value::Null);
                let mut calls = Vec::new();
                if let Some(blocks) = content.as_array() {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool-call") { continue; }
                        let Some(id) = block.get("id").and_then(Value::as_str) else { continue; };
                        let Some(name) = block.get("name").and_then(Value::as_str) else { continue; };
                        let arguments = block.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                        calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}}));
                        pending_calls.push(id.to_owned());
                    }
                }
                let text = model_text(content);
                if calls.is_empty() {
                    history.push(json!({"role":"assistant","content":text}));
                } else {
                    history.push(json!({"role":"assistant","content":if text.is_empty() { Value::Null } else { json!(text) },"tool_calls":calls}));
                }
            }
            "subagent/start" => subagent_used = true,
            "tool/result" => {
                let call_id = data.pointer("/message/source/callId").or_else(|| data.pointer("/message/toolCallId"))
                    .and_then(Value::as_str);
                if let Some(index) = call_id.and_then(|id| pending_calls.iter().position(|pending| pending == id)) {
                    let call_id = pending_calls.remove(index);
                    let content = data.pointer("/message/content").unwrap_or(&Value::Null);
                    history.push(json!({"role":"tool","tool_call_id":call_id,"content":model_text(content)}));
                    history.extend(crate::session_history::history_image_messages(&data["meta"]["result"]));
                }
            }
            "turn/end" => close_pending(&mut history, &mut pending_calls),
            _ => {}
        }
        if matches!(kind.as_str(),"assistant/message"|"tool/result"|"visual/input_result") && data["_replay_source"].is_object() {
            for message in &mut history[before..] {
                *message=crate::session_history::with_source(message.clone(),&data["_replay_source"]);
            }
        }
    }
    close_pending(&mut history, &mut pending_calls);
    (history, last_turn + 1, subagent_used)
}

#[test]
fn resumed_worker_tool_history_retains_original_source_and_failure() {
    let data=json!({"turn":2,"step":3,"actor":"worker","identity":{"task_id":"task","request_id":1,"work_id":"upload"},
        "message":{"toolCallId":"call","isError":true,"content":[{"type":"text","text":"原始错误\n完整末尾"}]}});
    let mut saved=data.clone();saved["_replay_source"]=crate::session_history::event_source("task",9,123,"tool/result",&data);
    let (history,_,_)=history_from_events(std::path::Path::new("."),&[
        ("assistant/message".into(),json!({"message":{"content":[{"type":"tool-call","id":"call","name":"view_image","arguments":"{}"}]}})),
        ("tool/result".into(),saved)]);
    let message=&history[1];
    assert_eq!(message["tool_call_id"],"call");
    let text=message["content"].as_str().unwrap();
    let source:Value=serde_json::from_str(text.split_once("\n\n").unwrap().0.strip_prefix("Source: ").unwrap()).unwrap();
    assert_eq!(source["seq"],9);assert_eq!(source["turn"],2);assert_eq!(source["is_error"],true);
    assert_eq!(source["identity"],data["identity"]);
    assert_eq!(crate::session_history::original_text(text),"原始错误\n完整末尾");
}

#[test]
fn restored_history_keeps_the_original_lightweight_image_message() {
    let message=json!({"role":"user","content":[{"type":"text","text":"Previously delivered image"},
        {"type":"image_ref","artifact_id":"visual-1","view_original":false}]});
    let (history,_,_)=history_from_events(std::path::Path::new("."),&[
        ("visual/input_result".to_owned(),json!({"actor":"worker","message":message}))]);
    assert_eq!(history,vec![message]);
    assert!(!history[0].to_string().contains("data:image"));
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
    let files = tokio::task::spawn_blocking(move || crate::composer_catalog::search_files(&root, &q))
        .await
        .unwrap_or_default();
    Json(json!({"files": files})).into_response()
}

pub async fn slash_commands(State(state): State<AgentServiceState>) -> impl IntoResponse {
    let root = state.workspace.root().to_path_buf();
    let commands = tokio::task::spawn_blocking(move || crate::composer_catalog::slash_commands(&root))
        .await
        .unwrap_or_default();
    Json(json!({"commands": commands})).into_response()
}

pub async fn continue_task(
    State(state): State<AgentServiceState>,
    Path(task_id): Path<String>,
    Json(request): Json<ContinueTaskRequest>,
) -> impl IntoResponse {
    let mut state = state.with_latest_config().await;
    if state.plugin_cancel.is_cancelled() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"agent task plugin is stopped"}))).into_response();
    }
    let prompt = request.prompt.trim().to_owned();
    if prompt.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"prompt must not be empty"}))).into_response();
    }
    if state.provider_url.is_empty() || state.default_model.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"agent model provider is not configured"}))).into_response();
    }
    let root = state.workspace.root().to_path_buf();
    let read_id = task_id.clone();
    let default_model = state.default_model.clone();
    let default_provider = state.provider_name.clone();
    let default_effort = state.reasoning_effort.clone();
    let default_fast_mode = state.fast_mode;
    let routes = state.provider_routes.clone();
    let title = prompt.clone();
    let req_model = request.model.clone();
    let req_provider = request.provider.clone();
    let req_effort = request.reasoning_effort.clone();
    let req_fast = request.fast_mode;
    let req_permission = request.permission_mode;
    let resume_target=request.resume_target.clone();
    let loaded = tokio::task::spawn_blocking(move || -> anyhow::Result<ResumeLoad> {
        let mut conn = open_db(&root)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let task: Option<(String, String, Option<String>, Option<String>, Option<String>, i64)> = tx.query_row(
            "SELECT model,status,parent_task_id,provider_name,reasoning_effort,fast_mode FROM agent_tasks WHERE id=?1", [&read_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).optional()?;
        let Some((model, status, parent_id, saved_provider, saved_effort, saved_fast_mode)) = task else { return Ok(ResumeLoad::NotFound); };
        if parent_id.is_some() { return Ok(ResumeLoad::ChildTask); }
        if !matches!(status.as_str(), "draft" | "completed" | "failed" | "cancelled" | "max_steps" | "interrupted") {
            return Ok(ResumeLoad::Busy);
        }
        let events = {
            let mut stmt = tx.prepare("SELECT seq,timestamp,kind,data FROM agent_task_events WHERE task_id=?1 ORDER BY seq")?;
            let rows = stmt.query_map([&read_id], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?)))?;
            rows.map(|row| {
                let (seq,time,kind,text) = row?;
                let mut data:Value=serde_json::from_str(&text)?;
                data["_replay_source"]=crate::session_history::event_source(&read_id,seq,time,&kind,&data);
                Ok((kind,data))
            }).collect::<anyhow::Result<Vec<_>>>()?
        };
        let (history, turn, subagent_used) = history_from_events(&root, &events);
        if let Some(target)=&resume_target {
            let scheduler=events.iter().rev().find_map(|(kind,data)| {
                let saved=match kind.as_str() {"execution/commit"|"flow/session"=>data.get("scheduler"),"scheduler/state"=>data.get("state"),_=>None}?;
                serde_json::from_value::<crate::work_scheduler::WorkScheduler>(saved.clone()).ok()
            });
            let valid=scheduler.as_ref().is_some_and(|scheduler|scheduler.request_completed!=Some(true)
                && scheduler.request_started_turn==target.request_id && scheduler.current==target.work_id
                && scheduler.frames.get(&target.work_id).is_some_and(|frame|frame.order.revision==target.revision
                    && frame.status!=crate::work_scheduler::WorkStatus::Done && frame.invalidated_by_plan_revision.is_none()));
            if !valid {return Ok(ResumeLoad::InvalidNode("该节点已完成、已废弃或不再是当前暂停节点；请刷新任务流。".into()));}
            append_event_tx(&tx,&read_id,"flow/resume_requested",&json!({"turn":turn,"target":target}),None)?;
        }
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
        let changed = tx.execute(
            "UPDATE agent_tasks SET model=?2,status='running',updated_at=?3,prompt=CASE WHEN prompt='' THEN ?5 ELSE prompt END,provider_name=?6,reasoning_effort=?7,fast_mode=?8 WHERE id=?1 AND status=?4",
            params![read_id, model, now(), status, title, provider_name, reasoning_effort, fast_mode],
        )?;
        if changed == 0 { return Ok(ResumeLoad::Busy); }
        tx.commit()?;
        Ok(ResumeLoad::Ready { model, provider_name, reasoning_effort, fast_mode, permission_mode, turn, history, subagent_used })
    }).await;
    let (model, provider_name, reasoning_effort, fast_mode, permission_mode, turn, history, subagent_used) = match loaded {
        Ok(Ok(ResumeLoad::Ready { model, provider_name, reasoning_effort, fast_mode, permission_mode, turn, history, subagent_used })) => (model, provider_name, reasoning_effort, fast_mode, permission_mode, turn, history, subagent_used),
        Ok(Ok(ResumeLoad::NotFound)) => return (StatusCode::NOT_FOUND, Json(json!({"error":"task not found"}))).into_response(),
        Ok(Ok(ResumeLoad::ChildTask)) => return (StatusCode::BAD_REQUEST, Json(json!({"error":"subagent tasks cannot receive follow-up messages"}))).into_response(),
        Ok(Ok(ResumeLoad::Busy)) => return (StatusCode::CONFLICT, Json(json!({"error":"task is still running"}))).into_response(),
        Ok(Ok(ResumeLoad::ModelUnavailable)) => return (StatusCode::CONFLICT, Json(json!({"error":"task model is no longer configured"}))).into_response(),
        Ok(Ok(ResumeLoad::InvalidNode(error))) => return (StatusCode::CONFLICT, Json(json!({"error":error}))).into_response(),
        _ => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"could not load task history"}))).into_response(),
    };
    if !state.use_provider(&provider_name) {
        return (StatusCode::CONFLICT, Json(json!({"error":"task provider is no longer configured"}))).into_response();
    }
    state.reasoning_effort = reasoning_effort;
    state.fast_mode = fast_mode;
    state.permission_mode = permission_mode;
    let cancel = state.plugin_cancel.child_token();
    let run_id = uuid_like();
    state.cancellations.lock().await.insert(task_id.clone(), ActiveTask { run_id: run_id.clone(), token: cancel.clone(),
        flow_node: Arc::new(Mutex::new(format!("turn_{turn}:1"))), node_interrupt: Arc::new(Mutex::new(None)) });
    let runner_state = state.clone();
    let runner_id = task_id.clone();
    let max_steps = request.max_steps.unwrap_or(DEFAULT_MAX_STEPS).clamp(1, MAX_STEP_LIMIT);
    spawn_supervised_task(runner_state, runner_id, run_id, model, prompt,
        max_steps, cancel, turn, history, subagent_used, false);
    (StatusCode::ACCEPTED, Json(json!({"task_id":task_id,"status":"running","turn":turn}))).into_response()
}

pub async fn get_events(
    State(state): State<AgentServiceState>,
    Path(task_id): Path<String>,
) -> impl IntoResponse {
    let root = state.workspace.root().to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<AgentEvent>> {
        let conn = open_db(&root)?;
        let mut stmt = conn.prepare("SELECT seq,timestamp,kind,data,surface_op FROM agent_task_events WHERE task_id=?1 ORDER BY seq")?;
        let rows = stmt.query_map([&task_id], |row| {
            let data: String = row.get(3)?;
            Ok((row.get::<_,i64>(0)?, row.get::<_,i64>(1)?, row.get::<_,String>(2)?, data, row.get::<_,Option<String>>(4)?))
        })?;
        rows.map(|row| {
            let (seq,time,kind,data,surface_op) = row?;
            Ok(AgentEvent { seq, time, kind, data: serde_json::from_str(&data)?, surface_op })
        }).collect()
    }).await;
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
                let read = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<AgentEvent>, Option<String>)> {
                    let conn = open_db(&root)?;
                    let mut stmt = conn.prepare("SELECT seq,timestamp,kind,data,surface_op FROM agent_task_events WHERE task_id=?1 AND seq>?2 ORDER BY seq")?;
                    let rows = stmt.query_map(params![read_id,last_seq], |row| {
                        let data: String = row.get(3)?;
                        Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,data,row.get::<_,Option<String>>(4)?))
                    })?;
                    let events = rows.map(|row| {
                        let (seq,time,kind,data,surface_op)=row?;
                        Ok(AgentEvent {seq,time,kind,data:serde_json::from_str(&data)?,surface_op})
                    }).collect::<anyhow::Result<Vec<_>>>()?;
                    let status = conn.query_row("SELECT status FROM agent_tasks WHERE id=?1",[read_id],|row|row.get::<_,String>(0)).optional()?;
                    Ok((events,status))
                }).await;
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
    let root = state.workspace.root().to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = open_db(&root)?;
        Ok(conn.query_row("SELECT id,prompt,model,status,created_at,updated_at,parent_task_id,provider_name,reasoning_effort,fast_mode FROM agent_tasks WHERE id=?1", [&task_id], |r| Ok(json!({"task_id":r.get::<_,String>(0)?,"prompt":r.get::<_,String>(1)?,"model":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"created_at":r.get::<_,i64>(4)?,"updated_at":r.get::<_,i64>(5)?,"parent_task_id":r.get::<_,Option<String>>(6)?,"provider_name":r.get::<_,Option<String>>(7)?,"reasoning_effort":r.get::<_,Option<String>>(8)?,"fast_mode":r.get::<_,i64>(9)? != 0}))).optional()?)
    }).await;
    match result {
        Ok(Ok(Some(task))) => Json(task).into_response(),
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
        Ok(Ok(Some(task))) => Json(task).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, Json(json!({"error":"task not found"}))).into_response(),
        Ok(Err(err)) => (StatusCode::CONFLICT, Json(json!({"error": err.to_string()}))).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"could not update task"}))).into_response(),
    }
}

pub async fn list_tasks(
    State(state): State<AgentServiceState>,
    Query(query): Query<ListTasksQuery>,
) -> impl IntoResponse {
    let root = state.workspace.root().to_path_buf();
    let limit = query.limit.unwrap_or(50).clamp(1, 100) as i64;
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = open_db(&root)?;
        let mut stmt = conn.prepare("SELECT id,prompt,model,status,created_at,updated_at,parent_task_id,provider_name,reasoning_effort,fast_mode FROM agent_tasks ORDER BY updated_at DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit], |r| Ok(json!({"task_id":r.get::<_,String>(0)?,"prompt":r.get::<_,String>(1)?,"model":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"created_at":r.get::<_,i64>(4)?,"updated_at":r.get::<_,i64>(5)?,"parent_task_id":r.get::<_,Option<String>>(6)?,"provider_name":r.get::<_,Option<String>>(7)?,"reasoning_effort":r.get::<_,Option<String>>(8)?,"fast_mode":r.get::<_,i64>(9)? != 0})))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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
        return (StatusCode::NOT_FOUND, Json(json!({"error":"task is not running"}))).into_response();
    };
    let current = active.flow_node.lock().await.clone();
    if current != node_id {
        return (StatusCode::CONFLICT, Json(json!({"error":"node is no longer active","active_node":current}))).into_response();
    }
    *active.node_interrupt.lock().await = Some(node_id.clone());
    active.token.cancel();
    (StatusCode::ACCEPTED, Json(json!({"task_id":task_id,"node_id":node_id,"status":"interrupting"}))).into_response()
}

pub(crate) fn uuid_like() -> String {
    // Unique enough for task IDs while keeping this service dependency-free.
    format!(
        "{:x}{:x}",
        now(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
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

pub(crate) async fn emit(root: &std::path::Path, task_id: &str, kind: &str, data: Value) -> anyhow::Result<()> {
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
    }).await
}

async fn finish(state: &AgentServiceState, task_id: &str, status: &str) -> anyhow::Result<()> {
    crate::browser_control::cleanup_after_finish(state.workspace.root(),task_id,status).await;
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
    history: Vec<Value>,
    subagent_used: bool,
    ban_run_command: bool,
) {
    tokio::spawn(async move {
        let worker = tokio::spawn(run_task(state.clone(), task_id.clone(), model, prompt,
            max_steps, cancel, false, turn, history, subagent_used, ban_run_command));
        let failure = match worker.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(format!("task worker stopped unexpectedly: {error}")),
        };
        if let Some(message) = failure {
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
pub(crate) async fn consume_model_stream(
    response: reqwest::Response,
    root: &std::path::Path,
    task_id: &str,
    turn: usize,
    step: usize,
    cancel: &CancellationToken,
) -> anyhow::Result<(Value, Value)> {
    let stream_started = Instant::now();
    let mut accumulator = crate::model_stream::ChatStream::new(crate::model_stream::Limits::WORKER, stream_started);
    let mut bytes = response.bytes_stream();
    let mut delta_text = String::new();
    let mut delta_reasoning = String::new();
    let mut last_flush = Instant::now();
    let mut flushed = false;

    while !accumulator.received_done() {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(anyhow::anyhow!("cancelled")),
            next = bytes.next() => match next {
                Some(Ok(chunk)) => chunk,
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
        };
        let delta = accumulator.push(&chunk)?;
        delta_text.push_str(&delta.text);
        delta_reasoning.push_str(&delta.reasoning);
        let ready = !delta_text.is_empty() || !delta_reasoning.is_empty();
        let due = !flushed || last_flush.elapsed() >= Duration::from_millis(80) || delta_text.len() + delta_reasoning.len() >= 32;
        if ready && due {
            emit(root, task_id, "assistant/delta", json!({"turn":turn,"step":step,"text":delta_text,"reasoning":delta_reasoning})).await?;
            delta_text.clear();
            delta_reasoning.clear();
            last_flush = Instant::now();
            flushed = true;
        }
    }
    if !delta_text.is_empty() || !delta_reasoning.is_empty() {
        emit(root, task_id, "assistant/delta", json!({"turn":turn,"step":step,"text":delta_text,"reasoning":delta_reasoning})).await?;
    }
    let completed = accumulator.finish()?;
    let mut stats = completed.stats;
    stats["format"] = stats["response_format"].clone();
    stats["first_delta_after_headers_ms"] = stats["first_delta_ms"].clone();
    stats["stream_ms"] = stats["response_complete_ms"].clone();
    stats["terminated"] = json!(completed.terminated);
    Ok((completed.message, stats))
}

/// File tools mentioned by name in the dynamic tool guidance.
const FILE_GUIDANCE_TOOLS: &[&str] = &[
    "workspace_info",
    "list_dir",
    "read_file",
    "read_file_lines",
    "write_file",
    "replace_range",
    "edit_file",
];

/// Symbol index tools follow the list_*/search_*_symbols, read_*_symbol,
/// index_*_workspace, and *_index_status naming pattern.
fn is_symbol_index_tool(name: &str) -> bool {
    name == "search_code_map" || ((name.starts_with("list_") || name.starts_with("search_")) && name.ends_with("_symbols"))
        || (name.starts_with("read_") && name.ends_with("_symbol"))
        || (name.starts_with("index_") && name.ends_with("_workspace"))
        || name.ends_with("_index_status")
}

/// Guidance lines for the tools actually offered in one model request. The
/// text is rebuilt per step, so a capability that disappears from the tool
/// list (e.g. a turn-level run_command ban) also disappears from the prompt;
/// never describe a tool that is not offered.
fn tool_guidance(names: &[&str]) -> Vec<String> {
    let has = |name: &str| names.contains(&name);
    let mut lines = Vec::new();
    let files: Vec<&str> = FILE_GUIDANCE_TOOLS.iter().copied().filter(|name| has(name)).collect();
    if !files.is_empty() {
        lines.push(format!(
            "When the answer depends on current project content or a requested edit, use the file tools ({}). Tokens like @relative/path identify workspace files or directories; read referenced content before making claims about its implementation. A workspace or path mention alone does not require inspection for a general discussion.",
            files.join(", "),
        ));
    }
    if has("edit_file") || has("write_file") || has("replace_range") {
        lines.push("For localized changes prefer edit_file: supply unique old_text/new_text anchors, combine non-overlapping edits to one file in one call, and use the file code_hash already available from a source read, notebook retrieval or the last successful write. Insertion retains its anchor in new_text; deletion uses empty new_text. write_file requires expected_code_hash when overwriting; replace_range requires expected_old_text or expected_code_hash. Conflicts apply no edits and are not permission denials: refresh only the affected material and correct the edit. All structured writes are restricted to the selected workspace, including absolute paths. changed=false is a no-op. index_refresh failures mean the file was saved but indexing failed; do not repeat the write for that reason.".to_owned());
    }
    if names.iter().any(|name| crate::worker_read_cache::is_source_read(name)) {
        lines.push("Source reads return byte-preserving pages: actual start_line/end_line, complete and next_start_line. EOF clamping is explicit. Continue a partial long line with start_column=next_column. Full requested material is saved before coverage filtering. Retrieve assigned source via recall_work with explicit material_ids and include_material=true. Ask Organizer for missing historical information; historical results are not current-state proof. force_read with reread_reason bypasses metadata-based snapshot caching when required.".to_owned());
    }
    if names.iter().any(|name| is_symbol_index_tool(name)) {
        lines.push(
            "For code navigation, prefer indexed symbol tools: choose the shortest lookup for the missing fact: read a known file/range directly; use read_*_symbol with file_path + name for a known definition; search_*_symbols for an unknown definition. Multi-keyword search defaults to any-term matching, ranks before pagination, and supports file/directory/kind filters. Lists are compact pages with local symbols hidden; only request another page or detailed/context output when necessary. Freshness and coverage are returned automatically, so do not routinely call index/status tools. Call relationships are heuristic; check the relevant source for ambiguous targets."
                .to_owned(),
        );
    }
    if has("search_code_map") {
        lines.push("For an unknown functionality, search_code_map returns responsibilities and entry points grouped by source file or saved functional area across languages. A group says what related code does; do not request call-chain context as a routine prerequisite. Read a listed definition directly when details are needed. Optionally preserve a learned definition/file role with record_symbol_business_context, using belongs_to_area to identify related code. Missing descriptions are unknown, never invented from names. Do not annotate an entire repository, write general task memories, or add investigation solely to populate the map.".to_owned());
    }
    if has("record_symbol_business_context") {
        lines.push("Symbol search/list/read results include description.text, status, source, scope, qualified_name and code_hash. Ordinary symbol searches match responsibilities and task keywords as well as identifiers; no separate business-context lookup is required. Current descriptions are navigation hints; stale ones need checking against the present source. After understanding an important entry point or module, optionally save one concise reusable responsibility with record_symbol_business_context, task-wording keywords, and expected_code_hash copied from the read result (description.code_hash for symbol reads, code_hash for file reads). Use file_path + qualified_name, or a symbol_id; scope=file describes the module rather than every function. Do not enumerate code, make extra reads, or postpone the answer just to annotate it. Obey any user restriction on saving memory; the Worker works independently of Observer descriptions.".to_owned());
    }
    if has("search_text") {
        lines.push(
            "Use search_text directly for literals, UI strings, config keys, error messages, log lines, imports and syntax patterns. Literal mode is the default: set regex=true for A|B alternatives, anchors or character classes. A zero literal match for A|B proves neither A nor B absent; fix the search mode instead of making a repository-absence claim. Do not force these through symbol search first. If symbol search finds nothing useful, inspect its coverage warnings and make one targeted text lookup; do not keep reformulating broad inventories."
                .to_owned(),
        );
    }

    if has("install_dependencies") { lines.push("Prefer install_dependencies for npm dependencies. Use run_project_script for named package.json scripts: foreground for build/check, background=true with a local ready_url/ready_port for dev/start. Query incremental logs through get_project_process using process_id and after_seq=next_seq. Process running does not prove readiness. Stop only owned processes through stop_project_process.".to_owned()); }
    if has("http_probe") {
        lines.push("For a local URL, font API endpoint, or page connectivity check, call http_probe; do not build HTTP requests with PowerShell or another shell. reachable=false with http_status=null means no HTTP response arrived, never HTTP status 0 or a server error code. Any received response, including 404, has reachable=true and its real http_status; check_passed is true only for HTTP 2xx. sampled_at is UTC; reuse a same-URL sample while its age is within reuse_window_ms and service state is unchanged. If the host returns reused=true, it reused the saved sample without a network request; use that result and stop probing. Probe again only if state may have changed or the old sample cannot answer the new question, and explain that reason in the tool call. Use get_project_process for Agent-managed process ownership, status and logs; an empty managed-process list says nothing about an independently started URL.".to_owned());
    }
    if has("run_program") {
        lines.push("Use run_program for a needed installed native development program only when no dedicated tool covers the action. Choose one supported program name (cargo, git, node, python) and pass every argument as a separate args item; the host does not invoke a shell. Do not pass PowerShell or another shell, and do not route PowerShell through Node or Python. Use project_path inside the workspace. Declare the matching check key as program:<name>:<normalized project_path>:<args JSON>, for example program:cargo:.:[\"check\"]. Only outcome=exited with process_exit_code=0 and process_success=true passes; stderr text alone is not failure. Do not treat stdout as business success. Timeout, cancellation, launch failure, truncation, and invalid UTF-8 are explicit; raw captured bytes are available as base64 when decoding fails. npm scripts and background servers must use the structured project tools.".to_owned());
    }

    lines
}

fn worker_system_prompt(
    root: &std::path::Path,
    is_child: bool,
    subagent_used: bool,
    tools: &[Value],
) -> String {
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
        .filter(|name|worker_tool_allowed(name))
        .collect();
    let role = if is_child {
        "You are a subagent completing one delegated task. Report the result to the parent. You cannot delegate further."
    } else if subagent_used {
        "This task has already used its one allowed subagent. Do not delegate again."
    } else if names.contains(&"spawn_subagent") {
        "You may use spawn_subagent once for a focused independent task. It shares the workspace, records its own trajectory, and cannot delegate further. Wait for its result before continuing."
    } else {
        "You are the sole worker for this task."
    };
    let worker = include_str!("../prompts/worker_system.md").trim();
    let method = include_str!("../prompts/shared_reasoning.md").trim();
    let guidance = tool_guidance(&names);
    let capabilities = if guidance.is_empty() {
        String::new()
    } else {
        let listed = guidance
            .iter()
            .map(|line| format!("- {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n\nTool guidance for the tools actually offered in this request:\n{listed}")
    };
    format!(
        "{worker}\n\n{method}{capabilities}\n\nRuntime context:\n- Workspace: {}\n- Operating system: {}\n- Role: {}",
        root.display(),
        std::env::consts::OS,
        role,
    )
}

/// Remove segments that quote or exemplify text instead of issuing an
/// instruction: fenced code blocks, inline code spans (backtick-delimited),
/// and text inside quote pairs (ASCII ", or the Chinese pairs “” ‘’ 「」 『』).
/// Replaced spans leave a separator so text on either side cannot form a new command ban.
/// Heuristic limits: ASCII single quotes/apostrophes are not treated as
/// quotes (to avoid swallowing text after apostrophes like "don't"),
/// quoted passages spanning multiple lines are only stripped up to end
/// of line, and nested or unterminated fences may misclassify later lines.
fn strip_quoted_segments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_fence = false;
    for line in input.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push('\n');
            continue;
        }
        let mut in_code = false;
        let mut quote: Option<char> = None;
        for c in line.chars() {
            if in_code {
                if c == '`' {
                    in_code = false;
                }
                continue;
            }
            if let Some(q) = quote {
                let closes = match q {
                    '"' => c == '"',
                    '“' => c == '”',
                    '‘' => c == '’',
                    '「' => c == '」',
                    '『' => c == '』',
                    _ => false,
                };
                if closes {
                    quote = None;
                }
                continue;
            }
            match c {
                '`' => { out.push(' '); in_code = true; }
                '"' | '“' | '‘' | '「' | '『' => { out.push(' '); quote = Some(c); }
                _ => out.push(c),
            }
        }
        out.push('\n');
    }
    out
}

fn user_bans_run_command(prompt: &str) -> bool {
    let stripped = strip_quoted_segments(prompt);
    let lower = stripped.to_lowercase();
    lower.contains("不要运行命令")
        || lower.contains("禁止运行命令")
        || lower.contains("不要执行命令")
        || lower.contains("禁止执行命令")
        || lower.contains("do not run commands")
        || lower.contains("don't run commands")
        || lower.contains("ban run command")
        || lower.contains("ban run_command")
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

#[cfg(test)]
mod task_window_tests {
    use super::*;

    #[test]
    fn worker_material_recall_rejects_global_queries_and_unassigned_ids_but_keeps_own_reads() {
        let mut scheduler=crate::work_scheduler::WorkScheduler::default();
        scheduler.enqueue(vec![crate::work_scheduler::WorkOrder{id:"work".into(),node_id:"node".into(),
            goal:"edit assigned source".into(),done_when:"report".into(),material_ids:vec![7],..Default::default()}],false,false).unwrap();
        scheduler.activate_next().unwrap();
        scheduler.observe("read_file",&json!({"path":"own.txt"}),&json!({"notebook_material":{"id":8}}),false);
        for _ in 0..20 { scheduler.observe("workspace_info",&json!({}),&json!({}),false); }
        assert!(scheduler.frame().unwrap().operations.iter().all(|operation|operation["material"].is_null()));
        let sources=Default::default();
        assert_eq!(worker_material_ids(&json!({"material_ids":[7,8]}),&scheduler,&sources).unwrap(),vec![7,8]);
        for args in [json!({"material_ids":[9]}),json!({"query":"global"}),json!({"material_ids":[7],"include_history":true}),
            json!({"tool_call_ids":["older"]}),json!({"tree_node_ids":["other"]})] {
            assert!(worker_material_ids(&args,&scheduler,&sources).is_err());
        }
    }

    #[test]
    fn worker_material_recall_accepts_bound_upstream_but_not_other_frames() {
        let mut scheduler=crate::work_scheduler::WorkScheduler::default();
        scheduler.enqueue(vec![crate::work_scheduler::WorkOrder{id:"prior".into(),node_id:"prior_node".into(),
            goal:"read".into(),done_when:"report".into(),..Default::default()}],false,false).unwrap();scheduler.activate_next().unwrap();
        scheduler.return_work(&json!({"summary":"read source","material_ids":[11]})).unwrap();
        scheduler.enqueue(vec![crate::work_scheduler::WorkOrder{id:"next".into(),node_id:"next_node".into(),
            goal:"use source".into(),done_when:"report".into(),upstream_ids:vec!["prior".into()],..Default::default()}],false,false).unwrap();scheduler.activate_next().unwrap();
        assert_eq!(worker_material_ids(&json!({"material_ids":[11]}),&scheduler,&Default::default()).unwrap(),vec![11]);
        scheduler.return_work(&json!({"summary":"done"})).unwrap();
        scheduler.enqueue(vec![crate::work_scheduler::WorkOrder{id:"unbound".into(),node_id:"unbound_node".into(),
            goal:"different work".into(),done_when:"report".into(),..Default::default()}],false,false).unwrap();scheduler.activate_next().unwrap();
        assert!(worker_material_ids(&json!({"material_ids":[11]}),&scheduler,&Default::default()).is_err());
    }

    #[test]
    fn worker_continuation_sees_one_upload_then_successor_sees_one_handoff() {
        let mut scheduler=crate::work_scheduler::WorkScheduler::default();
        scheduler.enqueue(vec![crate::work_scheduler::WorkOrder{id:"upload".into(),node_id:"upload_node".into(),
            goal:"load the PPT".into(),done_when:"return loaded state".into(),
            completion:crate::work_scheduler::Completion::Output,..Default::default()}],false,false).unwrap();
        scheduler.activate_next().unwrap();
        let result=json!({"upload_attempt_id":"one-upload","status":"file_assigned"});
        scheduler.observe("browser_upload",&json!({"path":"demo.pptx"}),&result,false);
        let messages=vec![json!({"role":"system","content":"Worker"}),
            json!({"role":"user","content":format!("Current work packet:\n{}",scheduler.worker_input("load the PPT"))}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"upload-call","type":"function",
                "function":{"name":"browser_upload","arguments":"{\"path\":\"demo.pptx\"}"}}]}),
            json!({"role":"tool","tool_call_id":"upload-call","content":result.to_string()})];
        let owners=HashMap::from([(2,scheduler.scope())]);
        let (request,_)=notebook_worker_request(&messages,2,&[2],&owners,"",&scheduler.scope(),&Default::default());
        assert_eq!(request.iter().flat_map(|message|message["tool_calls"].as_array().into_iter().flatten())
            .filter(|call|call["function"]["name"]=="browser_upload").count(),1);
        assert_eq!(request.iter().filter(|message|message["role"]=="tool" && message["tool_call_id"]=="upload-call").count(),1);
        assert_eq!(request.iter().map(Value::to_string).collect::<String>().matches("one-upload").count(),1);
        scheduler.return_work(&json!({"summary":"upload finished","exported_data":{"upload_attempt_id":"one-upload"}})).unwrap();
        scheduler.enqueue(vec![crate::work_scheduler::WorkOrder{id:"read".into(),node_id:"read_node".into(),
            goal:"read the page".into(),done_when:"return page state".into(),
            upstream_ids:vec!["upload".into()],completion:crate::work_scheduler::Completion::Output,..Default::default()}],false,false).unwrap();
        scheduler.activate_next().unwrap();
        let mut next_messages=messages.clone();next_messages[1]["content"]=json!(scheduler.worker_input("read the page").to_string());
        let (next,_)=notebook_worker_request(&next_messages,2,&[2],&owners,"",&scheduler.scope(),&Default::default());
        assert_eq!(next.len(),2,"prior invocation's tool calls do not become the successor's own actions");
        assert_eq!(next.iter().map(Value::to_string).collect::<String>().matches("one-upload").count(),1);
        assert_eq!(scheduler.frames["upload"].operations.len(),1);
    }

    #[test]
    fn current_node_keeps_more_than_two_steps_and_long_tool_failure_below_capacity() {
        let mut messages=vec![json!({"role":"system","content":"task"}),json!({"role":"user","content":"goal"})];
        let mut starts=Vec::new();let mut owners=HashMap::new();
        for index in 0..5 {
            starts.push(messages.len());owners.insert(messages.len(),"node".to_owned());
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call_{index}"),
                "function":{"name":"run_program","arguments":"{}"}}]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("call_{index}"),
                "content":if index==4 {"original failure ".repeat(2500)}else{format!("result_{index}")}}));
        }
        let (request,metadata)=notebook_worker_request(&messages,2,&starts,&owners,"","node",&Default::default());
        assert_eq!(request,messages,"no fixed two-step or per-message cutoff below capacity");
        assert_eq!(metadata["omitted_raw_message_count"],0);
        assert_eq!(metadata["history_mode"],"within_window");
    }

    #[test]
    fn overflow_keeps_all_invocation_originals_for_organizer_selection() {
        let messages=vec![json!({"role":"system","content":"task"}),json!({"role":"user","content":"goal"}),
            json!({"role":"assistant","content":"旧".repeat(crate::context_window::MAX_TOKENS)}),
            json!({"role":"assistant","tool_calls":[{"id":"new"}]}),json!({"role":"tool","tool_call_id":"new","content":"latest original failure"})];
        let owners=HashMap::from([(2,"node".to_owned()),(3,"node".to_owned())]);
        let (request,metadata)=notebook_worker_request(&messages,2,&[2,3],&owners,"","node",&Default::default());
        assert_eq!(request,messages,"projection never pre-crops an oversized execution");
        assert_eq!(metadata["history_mode"],"requires_task_selection");assert_eq!(metadata["omitted_raw_message_count"],0);
    }

}


fn notebook_worker_request(messages: &[Value], base: usize, _starts: &[usize], owners: &HashMap<usize,String>, _contract: &str, node: &str, _sources: &crate::task_notebook::SourceWorkingSet) -> (Vec<Value>, Value) {
    let mut scoped=messages[..base].to_vec();
    let mut owner=String::new();
    let mut skipped=0usize;
    for (index,message) in messages.iter().enumerate().skip(base) {
        if let Some(actual)=owners.get(&index) {owner=actual.clone();}
        if !node.is_empty() && owner!=node {skipped+=1;continue;}
        scoped.push(message.clone());
    }
    let overflow=crate::context_window::estimate(&json!(scoped))>crate::context_window::MAX_TOKENS.saturating_sub(8192);
    let recent_start=base;
    let metadata=json!({"mode":"original_messages","active_node":node,"raw_message_count":messages.len(),
        "base_message_count":base,"recent_start_raw_index":recent_start,"omitted_raw_message_count":skipped,
        "history_mode":if overflow {"requires_task_selection"}else{"within_window"},
        "source_results_replaced_by_material_refs":0,"shortened_messages":[],
        "limits":{"context_tokens":crate::context_window::MAX_TOKENS}});
    (scoped,metadata)
}

fn worker_tool_allowed(name: &str) -> bool {
    !matches!(name, "read_session_history" | "consult_observer" | "respond_observer"
        | "read_task_result" | "read_flow_page"
        | "list_work_memory" | "search_work_memory" | "list_architecture_memory" | "search_architecture_memory")
}

fn worker_material_ids(args: &Value, scheduler: &crate::work_scheduler::WorkScheduler, sources: &crate::task_notebook::SourceWorkingSet) -> anyhow::Result<Vec<i64>> {
    for key in ["query", "include_history", "history_before", "history_limit", "tree_node_ids", "tool_call_ids", "result_field", "result_offset"] {
        anyhow::ensure!(args.get(key).is_none(), "Worker cannot query shared history through recall_work; return missing inputs to Organizer");
    }
    let requested = args["material_ids"].as_array().ok_or_else(||anyhow::anyhow!("recall_work requires explicit material_ids supplied by Organizer or read in this invocation"))?;
    anyhow::ensure!(!requested.is_empty(), "recall_work requires nonempty material_ids");
    let mut allowed = sources.materials().iter().filter_map(|material|material["id"].as_i64()).collect::<HashSet<_>>();
    if let Some(frame) = scheduler.frame() {
        let order = &frame.order;
        allowed.extend(frame.read_material_ids.iter().copied());
        allowed.extend(frame.operations.iter().filter_map(|operation|operation["material"]["id"].as_i64()));
        allowed.extend(order.material_ids.iter().copied());
        allowed.extend(order.material_ranges.iter().filter_map(|range|range["id"].as_i64().or_else(||range["material_id"].as_i64())));
    }
    for delivery in scheduler.worker_input("")["upstream_outputs"].as_array().into_iter().flatten() {
        allowed.extend(delivery["material_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64));
    }
    requested.iter().map(|id| {
        let id=id.as_i64().ok_or_else(||anyhow::anyhow!("material_ids must be integers"))?;
        anyhow::ensure!(allowed.contains(&id), "Material {id} is outside this assignment; ask Organizer to supply it");
        Ok(id)
    }).collect()
}

fn recall_work_tool() -> Value {
    json!({"type":"function","function":{"name":"recall_work",
        "description":"Retrieve exact source materials supplied by Organizer or read in this invocation. Shared history, other task returns and global findings are unavailable to Worker; return missing inputs to Organizer. Use focused line ranges; replace_context changes the active source selection.",
        "parameters":{"type":"object","required":["material_ids"],"additionalProperties":false,"properties":{
            "action_id":{"type":"string"},"replace_context":{"type":"boolean"},
            "material_ids":{"type":"array","minItems":1,"items":{"type":"integer"}},"include_material":{"type":"boolean"},
            "start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1},
            "start_column":{"type":"integer","minimum":1},
            "force_read":{"type":"boolean","default":false},
            "max_chars":{"type":"integer","minimum":1000,"maximum":60000}}}
    }})
}

fn normalize_worker_flow_plan(
    plan: &Value,
    current_node_id: &str,
    previous_status: &HashMap<String, String>,
) -> anyhow::Result<(Vec<Value>, Vec<Value>, HashSet<String>)> {
    let raw_nodes = plan.get("nodes").and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("plan.nodes must be an array"))?;
    anyhow::ensure!(!raw_nodes.is_empty(), "a Flow plan must contain at least one node");
    anyhow::ensure!(raw_nodes.len() <= 32, "a Flow plan may contain at most 32 nodes");
    let mut ids = HashSet::new();
    let mut nodes = Vec::new();
    for raw in raw_nodes {
        let id = raw.get("id").and_then(Value::as_str).unwrap_or("").trim();
        anyhow::ensure!(!id.is_empty() && id.len() <= 80, "each Flow node needs a short nonempty id");
        anyhow::ensure!(id.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')),
            "Flow node ids may contain only letters, numbers, '.', '_' and '-'");
        anyhow::ensure!(ids.insert(id.to_owned()), "duplicate Flow node id: {id}");
        let title = raw.get("title").and_then(Value::as_str).unwrap_or("").trim();
        anyhow::ensure!(!title.is_empty(), "Flow node {id} needs a title");
        nodes.push(json!({
            "id":id,
            "title":truncate(title, 160),
            "kind":truncate(raw.get("kind").and_then(Value::as_str).unwrap_or("step"), 48),
            "description":truncate(raw.get("description").and_then(Value::as_str).unwrap_or(""), 1200),
            "status":previous_status.get(id).map(String::as_str).unwrap_or("pending"),
        }));
    }
    anyhow::ensure!(ids.contains(current_node_id), "current_node_id must refer to a node in the plan");
    let raw_edges = plan.get("edges").and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("plan.edges must be an array"))?;
    anyhow::ensure!(raw_edges.len() <= 64, "a Flow plan may contain at most 64 edges");
    let mut edges = Vec::new();
    for (index, raw) in raw_edges.iter().enumerate() {
        let source = raw.get("source").and_then(Value::as_str).unwrap_or("").trim();
        let target = raw.get("target").and_then(Value::as_str).unwrap_or("").trim();
        anyhow::ensure!(ids.contains(source) && ids.contains(target), "Flow edges must refer to nodes in the plan");
        edges.push(json!({
            "id":raw.get("id").and_then(Value::as_str).filter(|id| !id.trim().is_empty())
                .map(str::to_owned).unwrap_or_else(|| format!("edge_{index}_{source}_{target}")),
            "source":source,
            "target":target,
            "label":truncate(raw.get("label").and_then(Value::as_str).unwrap_or(""), 100),
        }));
    }
    let mut indegree = ids.iter().map(|id| (id.as_str(), 0usize)).collect::<HashMap<_, _>>();
    let mut children = HashMap::<&str, Vec<&str>>::new();
    for edge in &edges {
        let source = edge.get("source").and_then(Value::as_str).unwrap_or("");
        let target = edge.get("target").and_then(Value::as_str).unwrap_or("");
        *indegree.get_mut(target).expect("edge target was validated") += 1;
        children.entry(source).or_default().push(target);
    }
    let mut queue = indegree.iter().filter_map(|(id, degree)| (*degree == 0).then_some(*id)).collect::<Vec<_>>();
    let mut visited = 0usize;
    while let Some(id) = queue.pop() {
        visited += 1;
        for child in children.get(id).into_iter().flatten() {
            let degree = indegree.get_mut(child).expect("edge target was validated");
            *degree -= 1;
            if *degree == 0 { queue.push(child); }
        }
    }
    anyhow::ensure!(visited == ids.len(), "Flow plans must be acyclic");
    Ok((nodes, edges, ids))
}

fn flow_plan_shape(nodes: &[Value], edges: &[Value]) -> Value {
    json!({
        "nodes":nodes.iter().map(|node| json!({
            "id":node.get("id"),"title":node.get("title"),"kind":node.get("kind"),
            "description":node.get("description"),"parent_id":node.get("parent_id"),
            "objective":node.get("objective"),"done_when":node.get("done_when"),"constraints":node.get("constraints"),
        })).collect::<Vec<_>>(),
        "edges":edges.iter().map(|edge| json!({
            "id":edge.get("id"),"source":edge.get("source"),"target":edge.get("target"),
            "label":edge.get("label"),
        })).collect::<Vec<_>>(),
    })
}

fn repeatable_read_tool(name: &str) -> bool {
    matches!(name,
        "workspace_info" | "list_dir" | "read_file" | "read_file_lines" | "search_text" | "search_code_map"
        | "list_go_symbols" | "search_go_symbols" | "read_go_symbol" | "go_index_status"
        | "list_rust_symbols" | "search_rust_symbols" | "read_rust_symbol" | "rust_index_status"
        | "list_ts_symbols" | "search_ts_symbols" | "read_ts_symbol" | "ts_index_status"
        | "list_python_symbols" | "search_python_symbols" | "read_python_symbol" | "python_index_status"
        | "list_work_memory" | "search_work_memory" | "list_architecture_memory"
        | "search_architecture_memory" | "list_symbol_business_context" | "search_symbol_business_context"
    )
}

async fn refresh_scheduler_versions(scheduler:&mut crate::work_scheduler::WorkScheduler, workspace:&crate::tools::Workspace) -> anyhow::Result<()> {
    let previous=scheduler.versions();let root=workspace.root().to_path_buf();
    let versions=tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mut versions=std::collections::BTreeMap::new();
        for path in previous.keys() {
            let resolved=crate::file_edit::workspace_path(&root,path)?;
            let hash=match std::fs::read(resolved) {
                Ok(bytes)=>crate::symbol_description::content_hash(&bytes),
                Err(error) if error.kind()==std::io::ErrorKind::NotFound=>"missing".to_owned(),
                Err(error)=>return Err(error.into()),
            };
            versions.insert(path.clone(),hash);
        }
        Ok(versions)
    }).await??;
    scheduler.update_versions(&versions);Ok(())
}

/// Restore only explicitly completed requests into a fresh plan. Local queue exhaustion
/// and a blocked run must retain their deliveries and execution pointer.
async fn request_goal_from_events(root:std::path::PathBuf,task:String,request_turn:usize)->anyhow::Result<Option<String>> {
    tokio::task::spawn_blocking(move ||->anyhow::Result<Option<String>> {
        let conn=open_db(&root)?;
        let raw=conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='user/message' AND json_extract(data,'$.turn')=?2 ORDER BY seq LIMIT 1",
            params![task,request_turn],|row|row.get::<_,String>(0)).optional()?;
        Ok(raw.and_then(|raw|serde_json::from_str::<Value>(&raw).ok())
            .and_then(|data|data["model_content"].as_str().or_else(||data.pointer("/message/content/0/text").and_then(Value::as_str)).map(str::to_owned)))
    }).await?
}

fn restore_execution_session(
    previous_tree: &crate::flow_tree::TaskTree,
    mut previous_scheduler: Option<crate::work_scheduler::WorkScheduler>,
) -> (crate::flow_tree::TaskTree, crate::work_scheduler::WorkScheduler, bool) {
    let mut restored_tree=previous_tree.clone();
    restored_tree.normalize_snapshot_version();
    if let Some(scheduler) = previous_scheduler.as_mut() {
        scheduler.normalize_snapshot_version();
        scheduler.refresh_http_check_validity();
    }
    let scheduler_completed = previous_scheduler.as_ref().is_some_and(|s| match s.request_completed {
        Some(completed) => completed && s.handoff.is_none(),
        // Legacy snapshots had no request marker and retained their last output
        // as handoff even after finish. Restore only an explicit saved Organizer
        // assessment; node/queue completion alone is execution state, not success.
        None => s.finished && previous_tree.root_finished() && previous_tree.goal_assessment()==Some(true) && s.all_done()
            && s.handoff.as_ref().is_none_or(|h| h["done"] == true && h["blocked"] != true
                && h["outcome"] != "blocked" && h["need_split"] != true && h["outcome"] != "need_split"
                && h["upstream_problem"].is_null() && h["outcome"] != "upstream_problem"),
    });
    let explicit_request_marker=previous_scheduler.as_ref().is_some_and(|scheduler|scheduler.request_completed.is_some());
    let completed = if explicit_request_marker {
        // New sessions persist the request-level finish independently from
        // whether the Flow grouping root needed a legacy aggregate update.
        scheduler_completed
    } else if restored_tree.enabled() {
        restored_tree.root_finished() && restored_tree.goal_assessment()==Some(true)
            && (previous_scheduler.is_none() || scheduler_completed)
    } else { scheduler_completed };
    if completed {
        let scheduler=previous_scheduler.map(|scheduler|scheduler.after_completed_request(previous_tree)).unwrap_or_default();
        (crate::flow_tree::TaskTree::default(), scheduler, true)
    } else {
        restored_tree.resume_unachieved_request();
        let mut scheduler = previous_scheduler.unwrap_or_default();
        scheduler.finished = false;
        scheduler.request_completed = Some(false);
        (restored_tree, scheduler, false)
    }
}

/// This is the same transaction used by run_task: Scheduler and TaskTree either
/// both accept the decision or neither state is committed.
fn apply_organizer_decision(
    scheduler: &crate::work_scheduler::WorkScheduler,
    task_tree: &crate::flow_tree::TaskTree,
    previous_tree: &crate::flow_tree::TaskTree,
    root: &std::path::Path,
    prompt: &str,
    mut decision: Value,
    can_write: bool,
    can_check: bool,
) -> anyhow::Result<(crate::work_scheduler::WorkScheduler, crate::flow_tree::TaskTree, Value)> {
    let mut next=scheduler.clone();let mut tree=task_tree.clone();
    let continuation=scheduler.pending_handoff().is_some_and(|h| h["intent"] == "session_continuation");
    anyhow::ensure!(!continuation || decision.get("request_action").is_some(),
        "request_action is required at a session_continuation handoff; choose continue, subtask, or replace");
    anyhow::ensure!(decision.get("request_action").is_none_or(Value::is_string), "request_action must be a string");
    let request_action = decision["request_action"].as_str().unwrap_or("continue").to_owned();
    let allowed_request_actions=if continuation {&["continue","subtask","replace"][..]} else {&["continue"][..]};
    anyhow::ensure!(allowed_request_actions.contains(&request_action.as_str()),
        "request_action='{request_action}' is invalid in this lifecycle stage; allowed_request_actions: {}",allowed_request_actions.join(", "));
    decision["request_action"] = json!(request_action);
    if request_action == "replace" {
        anyhow::ensure!(decision["action"] == "work" && scheduler.pending_handoff().is_some_and(|h| h["intent"] == "session_continuation"),
            "replace starts a new user goal only with action=work at a new-message handoff");
        anyhow::ensure!(decision["preserve_current"] != true, "replace cancels the old goal; preserve_current applies to subtask repair");
        anyhow::ensure!(decision.pointer("/flow_update/resume_tree") != Some(&json!(true)), "replace cannot resume the cancelled tree");
        next = scheduler.replace_request(&tree, decision["reason"].as_str().unwrap_or("User replaced the goal"));
        tree = crate::flow_tree::TaskTree::default();
    } else if request_action == "subtask" {
        anyhow::ensure!(decision["action"] == "work", "subtask requires action=work");
        decision["preserve_current"] = json!(true);
    }
    if let Some(update)=decision.get("flow_update").filter(|value|value.is_object()&&!value.as_object().unwrap().is_empty()) {
        if update["resume_tree"]==true&&!tree.enabled(){
            anyhow::ensure!(next.archived_requests.is_empty(), "cancelled requests stay archived; create a tree for the current request");
            tree=previous_tree.clone();
        }
        if let Some(result)=update.get("node_result") {
            anyhow::ensure!(tree.aggregate_result_allowed(result["node_id"].as_str().unwrap_or("")),"Organizer may aggregate only completed children; leaf completion belongs to the host");
        }
        tree.apply(update)?;
    }
    match decision["action"].as_str().unwrap_or("") {
        "work"=>{
            let mut orders:Vec<crate::work_scheduler::WorkOrder>=serde_json::from_value(decision["orders"].clone()).unwrap_or_default();
            for order in &mut orders {
                if let Some(spec)=order.project_observation.as_mut() {
                    anyhow::ensure!(spec.is_object(),"project_observation must be an object with an explicit list coverage");
                    let force=spec["force_refresh"]==true;
                    if force {
                        anyhow::ensure!(spec["purpose"].as_str().is_some_and(|purpose|!purpose.trim().is_empty()&&purpose.chars().count()<=300),
                            "project_observation.force_refresh requires a concrete purpose");
                    }
                    let coverage=spec["coverage"].as_str().map(str::to_owned).unwrap_or_else(||if spec["project_path"].is_string(){"project_list".to_owned()}else{"workspace_list".to_owned()});
                    anyhow::ensure!(matches!(coverage.as_str(),"workspace_list"|"project_list"),
                        "project_observation.coverage must be workspace_list or project_list");
                    let mut observation_args=spec.clone();
                    observation_args.as_object_mut().unwrap().remove("purpose");
                    observation_args.as_object_mut().unwrap().remove("force_refresh");
                    observation_args["coverage"]=json!(coverage);
                    if coverage=="workspace_list" {
                        anyhow::ensure!(observation_args.get("project_path").is_none_or(Value::is_null),
                            "workspace_list observations cannot include project_path");
                        observation_args.as_object_mut().unwrap().remove("project_path");
                    } else {
                        anyhow::ensure!(observation_args["project_path"].is_string(),
                            "project_list observations require project_path");
                    }
                    crate::project_process::normalize_args(root,&mut observation_args)?;
                    if !force {
                        if let Some(prior)=next.reusable_project_observation(root,&observation_args) {
                            let old=prior["processes"].as_array().into_iter().flatten().filter_map(|p|p["process_id"].as_str()).collect::<Vec<_>>().join(", ");
                            anyhow::bail!("duplicate process observation for project scope: fresh delivery from work '{}' at sampled_at={} already covers this scope (process IDs: [{}]); consume that delivery or declare force_refresh=true with a concrete new purpose",
                                scheduler.frames.values().find(|frame|frame.project_observation==prior).map(|frame|frame.order.id.as_str()).unwrap_or("unknown"),
                                prior["sampled_at"],old);
                        }
                    }
                    spec["coverage"]=json!(coverage);
                    if coverage=="project_list" {spec["project_path"]=observation_args["project_path"].clone();}
                }
            }
            for order in &mut orders {
                for target in &mut order.edit_targets {
                    let resolved=crate::file_edit::workspace_path(&root,target)?;
                    let canonical_root=root.canonicalize()?;
                    *target=resolved.strip_prefix(canonical_root)?.to_string_lossy().replace('\\',"/");
                }
            }
            let simple=orders.len()==1 && orders[0].completion==crate::work_scheduler::Completion::Output
                && orders[0].final_answer && orders[0].checks.is_empty() && orders[0].edit_targets.is_empty();
            if !simple {
                tree.ensure_request_goal(&prompt)?;
                for (order_index,order) in orders.iter_mut().enumerate() {
                    if let Some(status)=tree.status(&order.node_id) {
                        anyhow::ensure!(status!="done"&&status!="completed"&&status!="skipped"&&status!="deprecated",
                            "field_path=orders[{order_index}].node_id: node_id='{}' has status='{status}' and is sealed; use action=revisit for a defective delivery or add a new ready node under an unfinished ancestor",order.node_id);
                        anyhow::ensure!(status!="blocked"&&status!="paused",
                            "field_path=orders[{order_index}].node_id: node_id='{}' has status='{status}'; resume this blocked/paused node with flow_update.node_updates[{{id,resume:true}}] before dispatch",order.node_id);
                        anyhow::ensure!(tree.work_node_ready(&order.node_id),
                            "field_path=orders[{order_index}].node_id: node_id='{}' has status='{status}' but still has unfinished child nodes; complete those children before assigning work to the parent",order.node_id);
                    } else {
                        order.node_id=format!("work_{}",order.id);
                        tree.ensure_work_child(&order.node_id,&order.goal,&order.done_when,&order.constraints)?;
                    }
                }
            }
            for order in &mut orders {
                anyhow::ensure!(!tree.enabled()||tree.work_node_ready(&order.node_id),"assign an unfinished leaf or a parent whose children are complete");
                if !tree.enabled() {
                    let node_id=format!("work_{}",order.id);
                    anyhow::ensure!(order.node_id=="direct"||order.node_id==node_id||order.node_id==order.id,"bounded work uses node_id=direct or its existing work node id");
                    order.node_id=node_id;
                }
            }
            decision["orders"] = serde_json::to_value(&orders)?;
            let changes = next.apply(&decision,can_write,can_check)?;
            tree.deprecate_nodes(&changes.invalidated_node_ids)?;
            decision["execution_changes"] = json!({"invalidated_work_ids": changes.invalidated_work_ids, "invalidated_node_ids": changes.invalidated_node_ids});
        },
        "select"|"revisit"|"continue"|"finish"|"blocked"=>{
            if decision["action"]=="select" {
                let task_id=decision["task_id"].as_str().filter(|id|!id.is_empty())
                    .ok_or_else(||anyhow::anyhow!("select requires task_id using an exact work/order ID; node IDs belong to revisit"))?;
                anyhow::ensure!(decision.get("node_id").is_none(),"select accepts task_id only; do not substitute a Flow node ID");
                anyhow::ensure!(scheduler.available_select_task_ids().iter().any(|id|id==task_id),
                    "select task_id '{task_id}' is not an active unfinished work order; available task IDs: [{}]",scheduler.available_select_task_ids().join(", "));
                if let Some(revision)=decision["target_revision"].as_u64() {
                    anyhow::ensure!(scheduler.frames.get(task_id).is_some_and(|frame|frame.order.revision==revision as usize),"resume_task revision no longer matches this work_id");
                }
                if tree.enabled() {
                    let node=&scheduler.frames[task_id].order.node_id;
                    let paused=matches!(tree.snapshot()["nodes"][node]["status"].as_str(),Some("blocked"|"paused"));
                    tree.apply(&if paused {json!({"node_updates":[{"id":node,"resume":true}],"current_node_id":node})}
                        else {json!({"current_node_id":node})})?;
                }
            }
            if decision["action"]=="revisit" {
                let target=decision["target_node_id"].as_str().filter(|id|!id.is_empty())
                    .ok_or_else(||anyhow::anyhow!("revisit requires target_node_id using an exact Flow node ID"))?;
                anyhow::ensure!(decision.get("node_id").is_none(),"revisit accepts target_node_id only");
                let frame_exists=scheduler.frames.values().any(|frame|frame.invalidated_by_plan_revision.is_none()&&frame.order.node_id==target);
                anyhow::ensure!(frame_exists&&(!tree.enabled()||tree.ids().contains(target)),
                    "revisit target_node_id '{target}' is unavailable; available revisit targets: [{}]",scheduler.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none()).map(|frame|frame.order.node_id.as_str()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", "));
            }
            if decision["action"]=="finish"||decision["action"]=="blocked" {
                anyhow::ensure!(decision["action"]=="blocked"||decision.get("achieved").is_none_or(|value|value==&json!(true)),
                    "finish_request cannot claim the overall goal was achieved while marking the delivery blocked");
            }
            let changes = next.apply(&decision,can_write,can_check)?;
            tree.deprecate_nodes(&changes.invalidated_node_ids)?;
            decision["execution_changes"] = json!({"invalidated_work_ids": changes.invalidated_work_ids, "invalidated_node_ids": changes.invalidated_node_ids});
            if decision["action"]=="finish"||decision["action"]=="blocked" {
                tree.finish_request(decision["summary"].as_str().unwrap_or(""),decision["action"]=="finish")?;
            }
            if decision["action"]=="revisit" && tree.enabled() {
                if let Some(rewind)=next.rewind_records.last() {
                    tree.revisit_node(&rewind.target_node,&rewind.invalidated_node_ids)?;
                }
            }
        },_=>anyhow::bail!("invalid Organizer action"),
    }
    if !next.finished && tree.enabled() {tree.apply(&json!({"current_node_id":next.node()}))?;}
    Ok((next,tree,decision))
}

fn organizer_error_fingerprint(rule_id:&str,field_path:&str,submitted_value_present:bool,rejected_value:&Value)->String {
    json!({"rule_id":rule_id,"field_path":field_path,"submitted_value_present":submitted_value_present,
        "rejected_value":rejected_value}).to_string()
}

fn organizer_resume_decision_error(handoff:Option<&Value>)->Option<Value> {
    let mut current=handoff;
    while let Some(item)=current {
        if let Some(error)=item.get("last_decision_error").filter(|error|!error.is_null()) {return Some(error.clone());}
        if let Some(error)=item.pointer("/organizer_failure/last_decision_error").filter(|error|!error.is_null()) {return Some(error.clone());}
        current=item.get("previous_handoff");
    }
    None
}

fn organizer_resume_request_error(handoff:Option<&Value>)->Option<Value> {
    let mut current=handoff;
    while let Some(item)=current {
        if item["failure_stage"]=="organizer_request" {
            if let Some(error)=item.get("failure").filter(|error|!error.is_null()) {return Some(error.clone());}
        }
        if item.pointer("/organizer_failure/failure_stage").and_then(Value::as_str)==Some("organizer_request") {
            if let Some(error)=item.pointer("/organizer_failure/failure").filter(|error|!error.is_null()) {return Some(error.clone());}
        }
        current=item.get("previous_handoff");
    }
    None
}

fn organizer_resume_contract_fingerprints(handoff:Option<&Value>)->Vec<String> {
    let mut fingerprints=Vec::new();
    let mut current=handoff;
    while let Some(item)=current {
        if let Some(items)=item.get("contract_error_fingerprints").and_then(Value::as_array) {
            fingerprints.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        if let Some(items)=item.pointer("/organizer_failure/contract_error_fingerprints").and_then(Value::as_array) {
            fingerprints.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        if let Some(fingerprint)=item.pointer("/last_decision_error/fingerprint").and_then(Value::as_str) {
            fingerprints.push(fingerprint.to_owned());
        }
        if let Some(fingerprint)=item.pointer("/organizer_failure/last_decision_error/fingerprint").and_then(Value::as_str) {
            fingerprints.push(fingerprint.to_owned());
        }
        current=item.get("previous_handoff");
    }
    fingerprints
}

fn attach_organizer_failure(handoff:&mut Option<Value>,failure:Value) {
    attach_handoff_error(handoff,"organizer_failure",failure);
}

fn attach_handoff_error(handoff:&mut Option<Value>,field:&str,error:Value) {
    if handoff.is_none() {*handoff=Some(json!({}));}
    if let Some(value)=handoff.as_mut() {
        if !value.is_object() {
            let previous=std::mem::replace(value,json!({}));
            value["previous_handoff"]=previous;
        }
        if let Some(object)=value.as_object_mut() {
            object.insert(field.into(),error);
        }
    }
}

fn organizer_contract_feedback(error:&anyhow::Error,decision:&Value,scheduler:&crate::work_scheduler::WorkScheduler,tree:&crate::flow_tree::TaskTree)->Value {
    let message=format!("{error:#}");
    let process=scheduler.organizer_input();
    if let Some(details)=crate::work_organizer::contract_details(error) {
        let code=details["rule_id"].as_str().unwrap_or("INVALID_ORGANIZER_RESPONSE");
        let field_path=details["field_path"].as_str().unwrap_or("arguments");
        let rejected=details.get("rejected_value").cloned().unwrap_or(Value::Null);
        let submitted=details["submitted_value_present"].as_bool().unwrap_or(false);
        let fingerprint=organizer_error_fingerprint(code,field_path,submitted,&rejected);
        let expected=match field_path {
            "goal"=>"Provide a concrete, nonempty goal for this one task.",
            "return_when"=>"Provide a nonempty condition that tells the Worker what result to return.",
            "reason"=>"State briefly why this task is the next useful step.",
            "completion"=>"completion is optional legacy metadata. Declare checks explicitly and use execution_scope.edit_targets for authorized writes.",
            "summary"=>"Provide a nonempty user-facing summary to finish_request.",
            "achieved"=>"Set achieved to true only when the user goal is complete; otherwise set false and list unresolved conditions.",
            "method"|"tool_calls"=>"Return one exposed scheduling action or a read-only Organizer lookup method.",
            _=>"Correct the named field to match its tool schema and the current execution path.",
        };
        let allowed=if field_path=="method"||field_path=="tool_calls" {json!(["schedule_task","read_task_result","read_flow_page","read_session_history","search_work_memory","search_architecture_memory","search_symbol_business_context","search_project_symbols","read_project_symbol","read_source_material","revisit_task","finish_request"])}
            else if field_path=="completion" {json!(["output","write","check","write_check",null])}
            else {Value::Null};
        return json!({"error_code":code,"rule_id":code,"field_path":field_path,"rejected_value":rejected,
            "submitted_value_present":submitted,"allowed_values":allowed,"expected_shape":expected,
            "relevant_node_ids":tree.ids().into_iter().collect::<Vec<_>>(),"error":message,"fingerprint":fingerprint,
            "correction_instruction":"Make a targeted correction to the named field. This is an Organizer contract error, not a network retry; do not repeat project investigation."});
    }
    if let Some(field_path)=message.split("field_path=").nth(1).and_then(|path|path.split(|ch:char|ch.is_whitespace()||ch==':'||ch==',').next()).filter(|path|!path.is_empty()) {
        let rejected_pointer=organizer_field_path_pointer(field_path);
        let submitted=decision.pointer(&rejected_pointer);
        let rejected=submitted.cloned().unwrap_or(Value::Null);
        let code=if field_path=="flow_update"||field_path.starts_with("flow_update.") {"REMOVED_FLOW_PROTOCOL"}
            else if field_path.contains(".dependency_inputs[")||field_path.contains(".upstream_ids[") {"INVALID_DEPENDENCY_INPUT"}
            else if field_path.ends_with(".browser_document_path") {"INVALID_PPTX_CONTRACT"}
            else if field_path.ends_with(".checks")&&message.contains("supported identifier") {"INVALID_CHECK_IDENTIFIER"}
            else if field_path.ends_with(".node_id")&&message.contains("sealed") {"COMPLETED_NODE_IMMUTABLE"}
            else {"INVALID_WORK_FIELD"};
        let fingerprint=organizer_error_fingerprint(code,field_path,submitted.is_some(),&rejected);
        let (expected_shape,allowed_values)=if code=="REMOVED_FLOW_PROTOCOL" {
            ("Flow state and node results are host-managed. Use an exposed scheduling method or a direct read-only Organizer lookup.",json!(["schedule_task","read_task_result","read_flow_page","read_session_history","search_work_memory","search_architecture_memory","search_symbol_business_context","search_project_symbols","read_project_symbol","read_source_material","revisit_task","finish_request"]))
        } else if code=="INVALID_DEPENDENCY_INPUT" {
            ("Use schedule_task.inputs with an exact work_id or node_id of a returned task, optional matching revision, and only fields listed on the delivery. TASK_NOT_RETURNED means wait for that task's return; TASK_DEPRECATED means it is history only. Page observations and upload availability are separate: an empty upload list does not mean no page exists, and null page availability means not checked. Read the preceding result and recorded page before deciding whether an open or upload is needed. A browser upload receipt is a dependency only while its session, page and attempt are active.",json!({"available_dependency_deliveries":process["available_dependency_deliveries"],"browser_upload_availability":process["browser_upload_availability"],"browser_observations":process["current_facts"]["browser"],"browser_uploads":process["current_facts"]["browser_uploads"],"browser_scope":process["current_facts"]["browser_scope"]}))
        } else if code=="INVALID_PPTX_CONTRACT" {
            ("Set execution_scope.browser_document_path to the exact workspace-relative .pptx path; this field cannot contain a page URL.",json!({"required_fields":["execution_scope.browser_document_path"],"available_dependency_deliveries":process["available_dependency_deliveries"],"browser_upload_availability":process["browser_upload_availability"]}))
        } else if code=="INVALID_CHECK_IDENTIFIER" {
            ("Use a supported, authorized check identifier. Checks are optional and do not classify the task.",json!(["npm:","npm-start:","npm-install:","program:","http-probe:"]))
        } else if code=="COMPLETED_NODE_IMMUTABLE" {
            ("Completed work is sealed. Schedule a new task, or use revisit_task with a target from process.available_revisit_targets.",process["available_revisit_targets"].clone())
        } else {
            ("Correct only the named schedule_task field using the active execution path and existing delivery fields.",json!({"execution_path":process["execution_path"],"available_dependency_deliveries":process["available_dependency_deliveries"]}))
        };
        let dependency_error=crate::work_scheduler::DEPENDENCY_ERROR_KINDS.iter().find(|kind|message.contains(*kind)).copied();
        return json!({"error_code":code,"rule_id":code,"field_path":field_path,"rejected_value":rejected,"allowed_values":allowed_values,
            "dependency_error":dependency_error,
            "submitted_value_present":submitted.is_some(),"expected_shape":expected_shape,"relevant_node_ids":tree.ids().into_iter().collect::<Vec<_>>(),"error":message,
            "fingerprint":fingerprint,"correction_instruction":"Make one targeted correction to this field using the listed IDs and delivery fields. Do not repeat project investigation or discard sealed results."});
    }
    let continuation=scheduler.pending_handoff().is_some_and(|handoff|handoff["intent"]=="session_continuation");
    let (error_code,field_path,expected_shape,allowed_values)=if message.contains("request_action") {
        ("INVALID_REQUEST_ACTION","request_action",if continuation {"At session_continuation use one supplied action; replace requires schedule_task."}else{"On a normal turn omit request_action or use continue."},
            if continuation {json!(["continue","subtask","replace"])}else{json!(["continue"])})
    } else if message.contains("flow_update")||message.contains("node_result") {
        ("REMOVED_FLOW_PROTOCOL","method","Flow updates and node results are host-managed. Use a scheduling method or a direct read-only Organizer lookup.",
            json!(["schedule_task","read_task_result","read_flow_page","read_session_history","search_work_memory","search_architecture_memory","search_symbol_business_context",
                "search_project_symbols","read_project_symbol","read_source_material","revisit_task","finish_request"]))
    } else if message.contains("target_node_id")||message.contains("revisit target") {
        ("INVALID_REVISIT_TARGET","target_node_id","Use an ID from process.available_revisit_targets.",process["available_revisit_targets"].clone())
    } else if message.contains("duplicate process observation")||message.contains("project_observation") {
        ("DUPLICATE_PROCESS_OBSERVATION","execution_scope.project_observation","Reuse the fresh observation through schedule_task.inputs, or request a new sample with a concrete purpose.",json!(["reuse fresh delivery","request new sample with purpose"]))
    } else {
        ("INVALID_WORK_CONTRACT","schedule_task","Correct only the rejected scheduling field using the host error and existing delivery data.",json!([]))
    };
    let rejected_pointer=match field_path {
        "request_action"=>"/request_action",
        "target_node_id"=>"/target_node_id",
        "execution_scope.project_observation"=>"/execution_scope/project_observation",
        "method"=>"",
        "schedule_task"=>"",
        _=>"",
    };
    let submitted=decision.pointer(rejected_pointer).or_else(||if field_path=="execution_scope.project_observation" {
        decision.pointer("/orders/0/project_observation")
    }else{None});
    let rejected=submitted.cloned().unwrap_or(Value::Null);
    let fingerprint=organizer_error_fingerprint(error_code,field_path,submitted.is_some(),&rejected);
    json!({"error_code":error_code,"rule_id":error_code,"field_path":field_path,"rejected_value":rejected,"allowed_values":allowed_values,
        "submitted_value_present":submitted.is_some(),"expected_shape":expected_shape,"relevant_node_ids":tree.ids().into_iter().collect::<Vec<_>>(),"error":message,"fingerprint":fingerprint,
        "correction_instruction":"Make one targeted correction using the supplied delivery fields; do not repeat project investigation or discard sealed results."})
}
fn organizer_field_path_pointer(path:&str)->String {
    format!("/{}",path.replace('[',"/").replace(']',"").replace('.',"/"))
}

fn organizer_read_action(action:&str)->bool {
    matches!(action,"read_many"|"read_task_result"|"read_flow_page"|"read_session_history"|"search_work_memory"|"search_architecture_memory"
        |"search_symbol_business_context"|"search_project_symbols"|"read_project_symbol"|"read_source_material")
}

async fn execute_organizer_read(
    state:&AgentServiceState,
    task_id:&str,
    scheduler:&crate::work_scheduler::WorkScheduler,
    notebook:&mut crate::task_notebook::Notebook,
    decision:&Value,
    turn:usize,
    step:usize,
    source_chars:&mut usize,
)->anyhow::Result<Value> {
    if decision["action"]=="read_many" {
        let mut results=Vec::new();
        for read in decision["reads"].as_array().into_iter().flatten() {
            let result=execute_organizer_read_single(state,task_id,scheduler,notebook,read,turn,step,source_chars).await
                .unwrap_or_else(|error|json!({"error":format!("{error:#}")}));
            results.push(json!({"request":read,"result":result}));
        }
        return Ok(json!({"results":results}));
    }
    execute_organizer_read_single(state,task_id,scheduler,notebook,decision,turn,step,source_chars).await
}

async fn execute_organizer_read_single(
    state:&AgentServiceState, task_id:&str, scheduler:&crate::work_scheduler::WorkScheduler,
    notebook:&mut crate::task_notebook::Notebook, decision:&Value, turn:usize, step:usize,
    _source_chars:&mut usize,
)->anyhow::Result<Value> {
    let action=decision["action"].as_str().unwrap_or("").to_owned();
    match action.as_str() {
        "read_task_result"=>scheduler.read_task_result(decision),
        "read_flow_page"=>scheduler.read_flow_page(decision),
        "read_session_history"=>crate::session_history::read(state.workspace.root(),task_id,decision).await,
        "search_work_memory"|"search_architecture_memory"|"search_symbol_business_context"=>{
            let workspace=state.workspace.clone();
            let query=decision["query"].as_str().unwrap_or("").trim().to_owned();
            let limit=decision["limit"].as_u64().unwrap_or(3).clamp(1,4) as usize;
            let search_action=action.clone();
            tokio::task::spawn_blocking(move||->anyhow::Result<Value>{
                let root=workspace.root().display().to_string();
                let value=match search_action.as_str() {
                    "search_work_memory"=>serde_json::to_value(workspace.search_work_memory(crate::memory::SearchWorkMemoryRequest{workspace_root:root,query,limit})?)?,
                    "search_architecture_memory"=>serde_json::to_value(workspace.search_architecture_memory(crate::memory::SearchArchitectureMemoryRequest{workspace_root:root,query,limit})?)?,
                    _=>serde_json::to_value(workspace.search_symbol_business_context(crate::memory::SearchSymbolBusinessContextRequest{workspace_root:root,query,limit})?)?,
                };
                Ok(value)
            }).await?
        },
        "search_project_symbols"|"read_project_symbol"=>{
            let language=decision["language"].as_str().ok_or_else(||anyhow::anyhow!("language is required"))?;
            let tool=if action=="search_project_symbols" {format!("search_{language}_symbols")} else {format!("read_{language}_symbol")};
            let mut args=if action=="search_project_symbols" {
                json!({"query":decision["query"],"limit":decision["limit"],"detailed":false,"match_mode":"any","include_locals":false})
            } else {
                json!({"symbol_id":decision["symbol_id"],"file_path":decision["file_path"],"name":decision["name"],"include_context":decision["include_context"]})
            };
            if !decision["file_path"].is_null()&&action=="search_project_symbols" {args["file_path"]=decision["file_path"].clone();}
            args["workspace_root"]=json!(state.workspace.root().display().to_string());
            let response=crate::mcp::call_tool(&state.workspace,json!({"name":tool,"arguments":args})).await?;
            let mut value=response.get("structuredContent").cloned().unwrap_or(response);
            if action=="read_project_symbol" {
                let material=notebook.save(turn,step,scheduler.node(),&tool,&value).await?;
                if let Some(material)=material {value["notebook_material"]=material;}

            }
            Ok(value)
        },
        "read_source_material"=>{
            let mut args=decision.clone();args["include_material"]=json!(true);
            notebook.recall(&args).await

        },
        _=>anyhow::bail!("unsupported Organizer read action '{action}'"),
    }
}

pub(crate) async fn run_task(
    mut state: AgentServiceState,
    task_id: String,
    model: String,
    prompt: String,
    max_steps: usize,
    cancel: CancellationToken,
    is_child: bool,
    turn: usize,
    _history: Vec<Value>,
    subagent_used: bool,
    ban_run_command: bool,
) -> anyhow::Result<()> {
    let ban_run_command = ban_run_command || user_bans_run_command(&prompt);
    // Mock unit scenarios use a short timeout; real-provider opt-in tests use
    // the documented model request budget rather than a one-second fixture budget.
    #[cfg(test)]
    let organizer_timeout=if std::env::var("CODEX_REAL_MODEL_ACCEPTANCE").as_deref()==Ok("1") {Duration::from_secs(60)}else{ORGANIZER_REQUEST_TIMEOUT};
    #[cfg(not(test))]
    let organizer_timeout=ORGANIZER_REQUEST_TIMEOUT;
    let root = state.workspace.root().to_path_buf();
    let model_prompt = crate::composer_catalog::expand_prompt(&root, &prompt);
    let observer_model = if state.observer_inherits_model { model.clone() } else { state.observer_model.clone() };
    emit(&root, &task_id, "turn/start", json!({
        "turn":turn,"provider":state.provider_name,"reasoning_effort":state.reasoning_effort,
        "fast_mode":state.fast_mode,"permission_mode":state.permission_mode,"max_steps":max_steps,
    })).await?;
    emit(&root, &task_id, "observer/config", json!({
        "turn":turn,
        "enabled":state.observer_enabled,
        "provider":if state.observer_inherits_model { "follow-worker" } else { state.observer_provider.as_str() },
        "model":if state.observer_enabled { observer_model.as_str() } else { "" },
    })).await?;
    emit_surface(&root, &task_id, "user/message", json!({
        "turn":turn,"model_content":model_prompt,
        "message":{"id":uuid_like(),"role":"user","content":[{"type":"text","text":prompt}],"source":{"kind":"user"}}
    }), Some("append")).await?;

    let mut messages = vec![json!({"role":"system","content":""}), json!({"role":"user","content":model_prompt})];
    let base_messages = messages.len();
    let mut step_starts = Vec::new();
    let mut step_owners=HashMap::<usize,String>::new();
    let mut subagent_spawned = subagent_used;
    let mut active_flow_node = String::new();
    let mut latest_flow_plan = Value::Null;
    let mut task_tree: crate::flow_tree::TaskTree;
    let mut recent_actions = Vec::<Value>::new();
    let mut observer = crate::observer_service::ObserverSession::start(state.clone(),observer_model.clone(),task_id.clone(),&cancel);
    let history_root = root.clone();
    let history_task = task_id.clone();
    let (mut work_state, legacy_revisions) = tokio::task::spawn_blocking(move || -> anyhow::Result<(crate::worker_work_state::WorkState, Vec<Value>)> {
        let conn = open_db(&history_root)?;
        let previous = conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind IN ('worker/work_state','flow/session') ORDER BY seq DESC LIMIT 1", [&history_task], |row| row.get::<_,String>(0)).optional()?;
        let mut work:crate::worker_work_state::WorkState=previous.and_then(|text|serde_json::from_str::<Value>(&text).ok())
            .and_then(|data|serde_json::from_value(data.get("worker_state").or_else(||data.get("state")).cloned().unwrap_or(Value::Null)).ok()).unwrap_or_default();
        let mut statement=conn.prepare("SELECT seq,timestamp,data FROM agent_task_events WHERE task_id=?1 AND kind='worker/progress' ORDER BY seq")?;
        let mut observations=Vec::new();
        for row in statement.query_map([&history_task],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?)))? {
            let (seq,time,raw)=row?;
            if let Ok(mut record)=serde_json::from_str::<Value>(&raw) {record["seq"]=json!(seq);record["time"]=json!(time);observations.push(record);}
        }
        let revisions=work.import_legacy(&observations);
        Ok((work,revisions))
    }).await??;
    for revision in legacy_revisions {emit(&root,&task_id,"worker/finding_revision",revision).await?;}
    work_state.resume(&prompt);
    emit(&root,&task_id,"worker/work_state",json!({"turn":turn,"step":0,"state":work_state.snapshot()})).await?;
    let mut repeated_read_calls = 0usize;
    let mut successful_file_writes = 0usize;
    let mut executed_tool_calls = 0usize;
    let mut read_results = HashMap::<String, (String,String)>::new();
    let mut source_coverage = crate::worker_read_cache::ReadCoverage::default();
    let mut notebook = crate::task_notebook::Notebook::load(&root,&task_id).await?;
    let mut source_working_set=crate::task_notebook::SourceWorkingSet::default();
    let (commit_root, commit_task) = (root.clone(), task_id.clone());
    let restored_commit = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<(crate::flow_tree::TaskTree, crate::work_scheduler::WorkScheduler)>> {
        let conn = open_db(&commit_root)?;
        let raw = conn.query_row(
            "SELECT data FROM agent_task_events WHERE task_id=?1 AND kind IN ('execution/commit','flow/session') ORDER BY seq DESC LIMIT 1",
            [&commit_task], |row| row.get::<_, String>(0)
        ).optional()?;
        if let Some(raw_str) = raw {
            if let Ok(data) = serde_json::from_str::<Value>(&raw_str) {
                let tree_opt: Option<crate::flow_tree::TaskTree> = serde_json::from_value(data["task_tree"].clone()).ok();
                let sched_opt: Option<crate::work_scheduler::WorkScheduler> = serde_json::from_value(data["scheduler"].clone()).ok();
                if let (Some(t), Some(s)) = (tree_opt, sched_opt) {
                    return Ok(Some((t, s)));
                }
            }
        }
        Ok(None)
    }).await??;

    let previous_tree = if let Some((ref tree, _)) = restored_commit {
        tree.clone()
    } else {
        let (tree_root, tree_task) = (root.clone(), task_id.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<crate::flow_tree::TaskTree> {
            let conn = open_db(&tree_root)?;
            let raw = conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='flow/tree_state' ORDER BY seq DESC LIMIT 1",
                [tree_task], |row| row.get::<_, String>(0)).optional()?;
            Ok(raw.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()).and_then(|data| serde_json::from_value(data["state"].clone()).ok()).unwrap_or_default())
        }).await??
    };
    let (sched_root, sched_task) = (root.clone(), task_id.clone());
    let mut previous_scheduler = if let Some((_, sched)) = restored_commit {
        Some(sched)
    } else {
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<crate::work_scheduler::WorkScheduler>> {
            let conn = open_db(&sched_root)?;
            let raw = conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='scheduler/state' ORDER BY seq DESC LIMIT 1",
                [sched_task], |row| row.get::<_, String>(0)).optional()?;
            Ok(raw.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()).and_then(|data| serde_json::from_value(data["state"].clone()).ok()))
        }).await??
    };

    if let Some(previous)=previous_scheduler.as_mut().filter(|scheduler|scheduler.request_goal.is_empty()) {
        let request_turn=if previous.request_started_turn>0 {previous.request_started_turn}else{turn.saturating_sub(1).max(1)};
        previous.request_goal=request_goal_from_events(root.clone(),task_id.clone(),request_turn).await?.unwrap_or_default();
    }
    let (restored_tree, mut scheduler, is_completed_previous_session) =
        restore_execution_session(&previous_tree, previous_scheduler);
    task_tree = restored_tree;
    if is_completed_previous_session {
        work_state=crate::worker_work_state::WorkState::default();
        work_state.resume(&prompt);
    }

    if scheduler.request_started_turn == 0 {
        scheduler.request_started_turn = if !is_completed_previous_session && turn > 1
            && (task_tree.enabled() || !scheduler.frames.is_empty()) { turn - 1 } else { turn };
    }
    // Read the original human message by request identity, not the latest
    // follow-up such as "continue". Legacy snapshots didn't retain this text.
    if scheduler.request_goal.is_empty() {
        scheduler.request_goal=request_goal_from_events(root.clone(),task_id.clone(),scheduler.request_started_turn).await?
            .unwrap_or_else(||prompt.clone());
    }
    // Legacy unscoped advice belongs to the restored goal, never a new goal
    // after successful completion. New inbox snapshots persist that identity.
    observer.set_scope(&scheduler,&prompt);

    if !is_completed_previous_session && turn > 1 && !prompt.is_empty()
        && (task_tree.enabled() || !scheduler.frames.is_empty() || !scheduler.request_goal.is_empty()) {
        // Every unfinished goal gets a request-level decision, even if its last
        // dispatched packet already finished. Preserve that packet's delivery.
        let resumable = scheduler.frame().filter(|f| f.status != crate::work_scheduler::WorkStatus::Done
            && f.invalidated_by_plan_revision.is_none());
        scheduler.handoff = Some(json!({
            "done": false, "intent": "session_continuation",
            "resumable_work": resumable.map(|f| &f.order.id),
            "resumable_node": resumable.map(|f| &f.order.node_id),
            "revision": resumable.map(|f| f.order.revision),
            "previous_handoff": scheduler.pending_handoff(),
            "reason": format!("User sent new input '{prompt}' before the current goal ended. Choose request_action=continue, subtask, or replace; preserve sealed deliveries when continuing.")
        }));
    }
    scheduler.prepare_legacy_check_migration();
    let (resume_root,resume_task)=(root.clone(),task_id.clone());
    let resume_target=tokio::task::spawn_blocking(move ||->anyhow::Result<Option<ResumeTarget>> {
        let conn=open_db(&resume_root)?;
        let raw=conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='flow/resume_requested' AND json_extract(data,'$.turn')=?2 ORDER BY seq DESC LIMIT 1",
            params![resume_task,turn],|row|row.get::<_,String>(0)).optional()?;
        Ok(raw.and_then(|raw|serde_json::from_str::<Value>(&raw).ok()).and_then(|data|serde_json::from_value(data["target"].clone()).ok()))
    }).await??;
    if let Some(target)=resume_target {
        anyhow::ensure!(scheduler.request_started_turn==target.request_id && scheduler.current==target.work_id
            && scheduler.revision()==target.revision,"resume target changed before execution");
        anyhow::ensure!(scheduler.select_task(Some(&target.work_id))?,"requested node could not resume");
        if task_tree.enabled() {
            let node=scheduler.node();
            let paused=matches!(task_tree.snapshot()["nodes"][node]["status"].as_str(),Some("blocked"|"paused"));
            task_tree.apply(&if paused {json!({"node_updates":[{"id":node,"resume":true}],"current_node_id":node})}
                else {json!({"current_node_id":node})})?;
        }
        emit(&root,&task_id,"flow/node_resumed",json!({"turn":turn,"target":target,"main_task":scheduler.main_task_summary()})).await?;
    }
    emit(&root,&task_id,"flow/request_overview",json!({"turn":turn,"main_task":scheduler.main_task_summary()})).await?;
    let mut initial_plan=scheduler.unified_flow_plan(task_tree.enabled().then_some(&task_tree));
    initial_plan["turn"]=json!(turn);initial_plan["replaceCurrentTurn"]=json!(true);
    crate::observer_service::commit(&root,&task_id,json!({"commit_id":format!("{turn}:0:continuation"),"turn":turn,"step":0,
        "scheduler":scheduler.snapshot(),"task_tree":task_tree.snapshot(),"flow_plan":initial_plan}),Vec::new()).await?;
    let mut organizer_error=organizer_resume_decision_error(scheduler.pending_handoff()).unwrap_or(Value::Null);
    let mut organizer_request_error=organizer_resume_request_error(scheduler.pending_handoff()).unwrap_or(Value::Null);
    let mut organizer_contract_fingerprints=HashSet::new();
    organizer_contract_fingerprints.extend(organizer_resume_contract_fingerprints(scheduler.pending_handoff()));
    if let Some(fingerprint)=organizer_error["fingerprint"].as_str() {organizer_contract_fingerprints.insert(fingerprint.to_owned());}
    let mut organizer_request_failures=0usize;
    let mut organizer_read_results=Vec::<Value>::new();
    let mut organizer_source_chars=0usize;
    let mut active_scope=String::new();
    let mut delivered_commands=HashMap::<String,usize>::new();
    for step in 1..=max_steps {
        if cancel.is_cancelled() {
            observer.finish("cancelled").await?;finish_cancelled_run(&state, &root, &task_id, turn, &active_flow_node).await?;
            return Ok(());
        }
        scheduler.refresh_http_check_validity();
        emit(&root, &task_id, "step/start", json!({"turn":turn,"step":step})).await?;

        // Completed work hands off to Organizer to evaluate and choose next action; no automatic activate_next bypass.
        if (scheduler.needs_organizer() || crate::work_executor::WorkExecutor::should_yield(&scheduler)) && (step<max_steps || scheduler.order().is_none()) {
            scheduler.refresh_browser_upload_validity(&root,&task_id).await;
            scheduler.save_selection(source_working_set.snapshot());
            emit(&root,&task_id,"organizer/start",json!({"turn":turn,"step":step,"nodeId":scheduler.node()})).await?;
            let mut organizer_input=json!({"schema_version":1,"turn":turn,"human_request":model_prompt,
                "capabilities":{"permissions":state.permission_mode.guidance(),"can_write":state.permission_mode.allows("edit_file"),
                    "available_tools":local_tools(&state.workspace,state.tool_catalog.as_deref(),ban_run_command).iter()
                        .filter_map(|tool|tool.pointer("/function/name").and_then(Value::as_str)).filter(|name|state.permission_mode.allows(name) && worker_tool_allowed(name)).collect::<Vec<_>>(),
                    "allowed_request_actions":if scheduler.pending_handoff().is_some_and(|handoff|handoff["intent"]=="session_continuation") {vec!["continue","subtask","replace"]} else {vec!["continue"]},
                    "can_check":can_run_check(&state,ban_run_command),
                    "execution_contract":{"completion":"optional legacy metadata; it does not classify work or prove success",
                        "task_return":"only the Worker seals its invocation with yield_work; host observations are dated facts",
                        "checks":"optional explicit host observations; may coexist with browser, visual, or write work"},
                    "visual_input":{"provider":state.provider_name,"model":model,
                        "capability":serde_json::to_value(state.visual.capability(&state.provider_name,&model)).unwrap_or(Value::Null),
                        "fallback_available":state.visual.fallback.is_some()}},
                "process":scheduler.organizer_input_with_freshness(&root),
                "history":{"tool":"read_session_history","scope":"Current conversation by default; scope=project searches other conversations in this workspace. Read relevant originals using returned task_id/read_reference only when needed."},
                "last_decision_error":organizer_error,
                "last_request_error":organizer_request_error,"read_task_results":organizer_read_results});
            let mut request_failures_for_decision=0usize;
            let decision=loop {
                let request=crate::work_organizer::decide(&state,&model,&task_id,turn,step,scheduler.node(),organizer_input.clone(),
                    &cancel,organizer_timeout).await;
                if cancel.is_cancelled() {
                    observer.finish("cancelled").await?;finish_cancelled_run(&state,&root,&task_id,turn,&active_flow_node).await?;
                    return Ok(());
                }
                match request {
                    Ok(decision) if decision["action"].as_str().is_some_and(organizer_read_action)=>{
                        let read=execute_organizer_read(&state,&task_id,&scheduler,&mut notebook,&decision,turn,step,&mut organizer_source_chars).await;
                        match read {
                            Ok(result)=>{
                                let query_key=crate::source_read::query_fingerprint(&decision);
                                if organizer_read_results.iter().any(|old|old["query_key"]==query_key) {
                                    organizer_input["last_read_notice"]=json!("This exact read has already been returned in read_task_results. Use its prior result and choose a scheduling decision or a different cursor/query.");
                                    continue;
                                }
                                let entry=json!({"action":decision["action"],"query_key":query_key,"request":decision,"result":result});
                                organizer_read_results.push(entry);
                                organizer_input["read_task_results"]=json!(organizer_read_results);
                                emit(&root,&task_id,"organizer/result_read",json!({"turn":turn,"step":step,"result":organizer_read_results.last()})).await?;
                                continue;
                            },
                            Err(error)=>{
                                let feedback=organizer_contract_feedback(&error,&decision,&scheduler,&task_tree);
                                let fingerprint=feedback["fingerprint"].as_str().unwrap_or("").to_owned();
                                let repeated=!fingerprint.is_empty()&&!organizer_contract_fingerprints.insert(fingerprint);
                                organizer_error=feedback.clone();organizer_input["last_decision_error"]=feedback.clone();
                                emit(&root,&task_id,"organizer/error",json!({"turn":turn,"step":step,"feedback":feedback,"repeated_fingerprint":repeated})).await?;
                                if repeated {
                                    let failure=json!({"failure_stage":"organizer_contract_validation","last_decision_error":organizer_error,
                                        "contract_error_fingerprints":organizer_contract_fingerprints.iter().cloned().collect::<Vec<_>>(),
                                        "current_work":scheduler.id(),"current_node":scheduler.node(),"revision":scheduler.revision(),"resumable":true});
                                    persist_organizer_failure(&root,&task_id,turn,step,&mut scheduler,&task_tree,"organizer_contract_validation",failure).await?;
                                    fail_turn(&state,&root,&task_id,turn,step,&observer_model,&prompt,
                                        "Organizer repeated the same invalid read reference; saved work can be resumed.".to_owned(),
                                        "ORGANIZER_CONTRACT_REPEATED","organizer_contract_validation").await?;
                                    observer.finish("failed").await?;
                                    return Ok(());
                                }
                                continue;
                            }
                        }
                    },
                    Ok(decision)=>{organizer_request_error=Value::Null;break decision;},
                    Err(error) if crate::work_organizer::contract_details(&error).is_some()=>{
                        let feedback=organizer_contract_feedback(&error,&Value::Null,&scheduler,&task_tree);
                        let fingerprint=feedback["fingerprint"].as_str().unwrap_or("").to_owned();
                        let repeated=!fingerprint.is_empty()&&!organizer_contract_fingerprints.insert(fingerprint.clone());
                        organizer_error=feedback.clone();organizer_input["last_decision_error"]=feedback.clone();
                        emit(&root,&task_id,"organizer/error",json!({"turn":turn,"step":step,"feedback":feedback,
                            "repeated_fingerprint":repeated,"source":"organizer_tool_response"})).await?;
                        if repeated {
                            let failure=json!({"failure_stage":"organizer_contract_validation","last_decision_error":organizer_error,
                                "contract_error_fingerprints":organizer_contract_fingerprints.iter().cloned().collect::<Vec<_>>(),
                                "current_work":scheduler.id(),"current_node":scheduler.node(),"revision":scheduler.revision(),"resumable":true});
                            persist_organizer_failure(&root,&task_id,turn,step,&mut scheduler,&task_tree,"organizer_contract_validation",failure).await?;
                            fail_turn(&state,&root,&task_id,turn,step,&observer_model,&prompt,
                                "Organizer repeated the same malformed method, rule, field and submitted value; saved work is resumable.".to_owned(),
                                "ORGANIZER_CONTRACT_REPEATED","organizer_contract_validation").await?;
                            observer.finish("failed").await?;
                            return Ok(());
                        }
                        continue;
                    },
                    Err(error)=>{
                        organizer_request_failures+=1;
                        request_failures_for_decision+=1;
                        let failure=json!({"error_code":"ORGANIZER_REQUEST_FAILED","stage":"organizer_request",
                            "attempt":request_failures_for_decision,"request_failures_this_run":organizer_request_failures,
                            "retry_limit":ORGANIZER_REQUEST_RETRY_LIMIT,
                            "error":format!("{error:#}")});
                        emit(&root,&task_id,"organizer/request_error",json!({"turn":turn,"step":step,"failure":failure,
                            "retrying":request_failures_for_decision<=ORGANIZER_REQUEST_RETRY_LIMIT})).await?;
                        if request_failures_for_decision<=ORGANIZER_REQUEST_RETRY_LIMIT {
                            organizer_input["last_request_error"]=failure;
                            scheduler.refresh_browser_upload_validity(&root,&task_id).await;
                            organizer_input["process"]=scheduler.organizer_input_with_freshness(&root);
                            continue;
                        }
                        let failure_record=json!({"failure_stage":"organizer_request","failure":failure,
                            "last_decision_error":organizer_error,"contract_error_fingerprints":organizer_contract_fingerprints.iter().cloned().collect::<Vec<_>>(),
                            "current_work":scheduler.id(),"current_node":scheduler.node(),"revision":scheduler.revision(),"resumable":true});
                        persist_organizer_failure(&root,&task_id,turn,step,&mut scheduler,&task_tree,"organizer_request",failure_record).await?;
                        fail_turn(&state,&root,&task_id,turn,step,&observer_model,&prompt,
                            "Organizer request failed after a bounded retry; unfinished work is saved for continuation.".to_owned(),
                            "ORGANIZER_REQUEST_RETRY_EXHAUSTED","organizer_request").await?;
                        observer.finish("failed").await?;
                        return Ok(());
                    }
                }
            };
            // The Organizer request may take long enough for its browser
            // receipt to expire. Recheck immediately before accepting a plan.
            scheduler.refresh_browser_upload_validity(&root,&task_id).await;
            let attempted_decision=decision.clone();
            let mut decision=decision;
            decision["decision_id"]=json!(format!("{task_id}:{turn}:{step}"));
            let applied=apply_organizer_decision(&scheduler, &task_tree, &previous_tree, &root, &prompt, decision,
                state.permission_mode.allows("edit_file"), can_run_check(&state,ban_run_command));
            match applied {
                Ok((mut next,tree,decision))=>{
                    if next.request_started_turn == 0 { next.request_started_turn = turn; }
                    if next.request_goal.is_empty() {next.request_goal=prompt.clone();}
                    if decision["request_action"] == "replace" {
                        let archive = next.archived_requests.last_mut().unwrap();
                        archive.worker_state = Some(work_state.snapshot());
                        emit(&root,&task_id,"flow/request_archived",json!({"turn":turn,"step":step,"id":archive.id,
                            "started_turn":archive.started_turn,"plan_revision":archive.plan_revision,
                            "node_ids":archive.flow_plan["nodes"].as_array().into_iter().flatten().map(|n|n["original_id"].clone()).collect::<Vec<_>>()})).await?;
                        work_state = crate::worker_work_state::WorkState::default();
                        work_state.resume(&prompt);
                        source_working_set = crate::task_notebook::SourceWorkingSet::default();
                        read_results.clear(); source_coverage.clear();
                        messages.truncate(base_messages); step_starts.clear(); step_owners.clear();
                        emit(&root,&task_id,"worker/work_state",json!({"turn":turn,"step":step,"state":work_state.snapshot()})).await?;
                    }
                    scheduler=next;task_tree=tree;organizer_error=Value::Null;organizer_contract_fingerprints.clear();
                    observer.set_scope(&scheduler,&prompt);
                    if decision["action"]=="work" || decision["action"]=="select" || decision["action"]=="revisit" || (decision["action"]=="continue" && decision["orders"].is_array()) {active_scope.clear();}
                    if decision["action"]=="revisit" {
                        if let Some(rewind)=scheduler.rewind_records.last() {
                            emit(&root,&task_id,"scheduler/rewind",json!({"turn":turn,"step":step,"rewind":rewind})).await?;
                        }
                    }
                    emit(&root,&task_id,"organizer/decision",json!({"turn":turn,"step":step,"decision":decision})).await?;
                },
                Err(error)=>{
                    let feedback=organizer_contract_feedback(&error,&attempted_decision,&scheduler,&task_tree);
                    let fingerprint=feedback["fingerprint"].as_str().unwrap_or("").to_owned();
                    let repeated=!fingerprint.is_empty()&&organizer_contract_fingerprints.contains(&fingerprint);
                    if !fingerprint.is_empty() {organizer_contract_fingerprints.insert(fingerprint);}
                    organizer_error=feedback.clone();
                    let failure=json!({"failure_stage":"organizer_contract_validation","last_decision_error":feedback,
                        "contract_error_fingerprints":organizer_contract_fingerprints.iter().cloned().collect::<Vec<_>>(),
                        "repeated_rule_field_value":repeated,"current_work":scheduler.id(),"current_node":scheduler.node(),"resumable":true,
                        "revision":scheduler.revision()});
                    persist_organizer_failure(&root,&task_id,turn,step,&mut scheduler,&task_tree,"organizer_contract_validation",failure).await?;
                    emit(&root,&task_id,"organizer/error",json!({"turn":turn,"step":step,"feedback":feedback,"repeated_fingerprint":repeated})).await?;
                    if repeated {
                        fail_turn(&state,&root,&task_id,turn,step,&observer_model,&prompt,
                            format!("Organizer repeated the same invalid rule, field and submitted value: {}",feedback["error"].as_str().unwrap_or("unknown contract error")),
                            "ORGANIZER_CONTRACT_REPEATED","organizer_contract_validation").await?;
                        observer.finish("failed").await?;
                        return Ok(());
                    }
                    emit(&root,&task_id,"step/end",json!({"turn":turn,"step":step})).await?;
                    continue;
                }
            }
        }
        let mut assignment_committed=false;
        if !scheduler.finished && scheduler.current_process().is_some() && scheduler.scope()!=active_scope {
            active_scope=scheduler.scope();active_flow_node=scheduler.node().to_owned();
            let order=scheduler.order().unwrap().clone();
            work_state.activate_unit(scheduler.id(),&active_flow_node,order.edit_targets.clone());
            if task_tree.enabled(){task_tree.apply(&json!({"current_node_id":active_flow_node}))?;}
            scheduler.set_goal_boundary(if task_tree.enabled(){task_tree.worker_boundary(&active_flow_node)}else{Value::Null});
            source_working_set.restore(&scheduler.selection(),&mut notebook,&order.edit_targets).await?;
            // Source is selected by the current frame, never inherited from siblings.
            if !order.material_ids.is_empty() {
                match notebook.recall(&json!({"material_ids":order.material_ids,"include_material":true,"max_chars":20_000})).await {
                    Ok(pages)=>source_working_set.add(&pages["materials"],&order.edit_targets),
                    Err(error)=>{emit(&root,&task_id,"organizer/material_unavailable",json!({"turn":turn,"step":step,"work_id":order.id,"error":error.to_string()})).await?;},
                }
            }
            for range in &order.material_ranges {
                match notebook.recall(&json!({"material_ids":[range["id"]],"include_material":true,"start_line":range["start_line"],"end_line":range["end_line"],"max_chars":20_000})).await {
                    Ok(pages)=>source_working_set.add(&pages["materials"],&order.edit_targets),
                    Err(error)=>{emit(&root,&task_id,"organizer/material_unavailable",json!({"turn":turn,"step":step,"work_id":order.id,"range":range,"error":error.to_string()})).await?;},
                }
            }
            set_active_flow_node(&state,&task_id,turn,&active_flow_node).await;
            emit(&root,&task_id,"organizer/assignment",json!({"turn":turn,"step":step,"nodeId":active_flow_node,"workId":order.id,"order":order})).await?;
            latest_flow_plan=scheduler.unified_flow_plan(if task_tree.enabled(){Some(&task_tree)}else{None});
            crate::observer_service::commit(&root,&task_id,json!({"commit_id":format!("{turn}:{step}:{}:assignment",scheduler.plan_revision),
                "turn":turn,"step":step,"scheduler":scheduler.snapshot(),"task_tree":task_tree.snapshot(),"flow_plan":latest_flow_plan.clone(),
                "active_node_id":latest_flow_plan["active_node_id"].clone(),"active_path":latest_flow_plan["active_path"].clone()}),
                Vec::new()).await?;
            assignment_committed=true;
        }
        let mut next_flow_plan=scheduler.unified_flow_plan(if task_tree.enabled(){Some(&task_tree)}else{None});
        if next_flow_plan["nodes"].as_array().is_some_and(Vec::is_empty) {
            next_flow_plan["nodes"]=json!([{"id":"request","title":truncate(&prompt,160),"kind":"worker","objective":prompt,
                "status":if scheduler.finished{"done"}else{"running"}}]);
            next_flow_plan["active_node_id"]=json!(if scheduler.finished{""}else{"request"});
        }
        if latest_flow_plan!=next_flow_plan {latest_flow_plan=next_flow_plan;}
        if !assignment_committed {
            crate::observer_service::commit(&root,&task_id,json!({
                "commit_id":format!("{turn}:{step}:{}:dispatch",scheduler.plan_revision),"turn":turn,"step":step,
                "scheduler":scheduler.snapshot(),"task_tree":task_tree.snapshot(),"flow_plan":latest_flow_plan.clone(),
                "active_node_id":latest_flow_plan["active_node_id"].clone(),"active_path":latest_flow_plan["active_path"].clone()
            }),Vec::new()).await?;
        }
        if scheduler.finished {
            let answer=scheduler.final_result.clone();
            emit_surface(&root,&task_id,"assistant/message",json!({"turn":turn,"step":step,"actor":"organizer",
                "message":{"id":uuid_like(),"role":"assistant","content":[{"type":"text","text":answer}],
                    "source":{"kind":"model","provider":state.provider_url,"model":model}},"stream":[]}),Some("append")).await?;
            emit(&root,&task_id,"step/end",json!({"turn":turn,"step":step})).await?;
            emit(&root,&task_id,"turn/end",json!({"turn":turn,"reason":{"kind":"completed",
                "goal_achieved":scheduler.request_completed,"unresolved":scheduler.request_unresolved}})).await?;
            finish(&state,&task_id,"completed").await?;
            observer.finish("completed").await?;
            return Ok(());
        }
        let mut tools = local_tools(&state.workspace, state.tool_catalog.as_deref(), ban_run_command);
        tools.retain(|tool| tool.pointer("/function/name").and_then(Value::as_str)
            .is_some_and(|name| state.permission_mode.allows(name) && worker_tool_allowed(name)));
        tools.push(recall_work_tool());tools.push(crate::work_organizer::yield_tool());
        if state.enable_subagent
            && !state.expert_provider_url.is_empty()
            && !state.expert_model.is_empty()
            && !is_child
            && !subagent_spawned
        {
            tools.push(subagent_tool());
        }
        if step == 1 {
            emit(&root, &task_id, "worker/capabilities", json!({
                "turn":turn,"permission_mode":state.permission_mode,"command_banned_by_user":ban_run_command,
                "tools":tools.iter().filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str)).collect::<Vec<_>>(),
            })).await?;
        }
        // Keep the ordinary capability description even when the final request
        // deliberately offers no tools so a budget limit is not a permission denial.
        let capability_tools = tools.clone();
        let final_step = step == max_steps && max_steps > 1;
        if final_step || scheduler.finished {
            tools.clear();
        }
        messages[0]["content"]=json!(format!("{}\n{}",worker_system_prompt(state.workspace.root(),is_child,subagent_spawned,&capability_tools),include_str!("../prompts/worker_unit.md")));
        source_working_set.refresh(&mut notebook,work_state.edit_targets()).await?;
        scheduler.input_versions(source_working_set.materials());
        for revision in work_state.refresh_source_refs(source_working_set.materials(),&source_working_set.metadata(),turn,step) {
            emit(&root,&task_id,"worker/finding_revision",revision).await?;
        }
        let context_node=scheduler.node().to_owned();
        let commands=scheduler.frame().map(|frame| {
            if frame.organizer_commands.is_empty() {vec![frame.order.original_command.clone().unwrap_or_else(||serde_json::to_string(&frame.order).unwrap())]}
            else {frame.organizer_commands.clone()}
        }).unwrap_or_default();
        messages[1]=crate::session_history::handoff_message(&root,&task_id,"organizer",&json!(commands.first().cloned().unwrap_or_default())).await?;
        let delivered=delivered_commands.entry(scheduler.scope()).or_insert(1);
        for command in commands.iter().skip(*delivered) {
            step_owners.insert(messages.len(),scheduler.scope());
            messages.push(crate::session_history::handoff_message(&root,&task_id,"organizer",&json!(command)).await?);
        }
        *delivered=commands.len();
        let (mut scoped_messages,mut compaction)=notebook_worker_request(&messages,base_messages,&step_starts,&step_owners,"",&scheduler.scope(),&source_working_set);
        let upstream=scheduler.worker_input("")["upstream_outputs"].as_array().cloned().unwrap_or_default();
        for (index,delivery) in upstream.iter().enumerate() {
            let original=delivery["id"].as_str().and_then(|id|scheduler.frames.get(id))
                .and_then(|frame|frame.output.as_ref()).and_then(|output|output.get("original_return").or_else(||output.get("worker_return"))).unwrap_or(delivery);
            scoped_messages.insert(base_messages+index,crate::session_history::handoff_message(&root,&task_id,&format!("worker_return_{}",delivery["id"].as_str().unwrap_or("").chars().take(50).collect::<String>()),original).await?);
        }
        compaction["pinned_material_ids"]=json!(source_working_set.materials().iter().map(|material|material["id"].clone()).collect::<Vec<_>>());
        compaction["pinned_source_chars"]=json!(source_working_set.materials().iter().filter_map(|material|material["returned_chars"].as_u64()).sum::<u64>());
        compaction["work_unit"]=json!({"id":scheduler.id(),"scope":scheduler.scope(),"input":scheduler.worker_input(&prompt)});
        scoped_messages.push(json!({"role":"system","content":format!("Runtime permissions: {}. A completed work unit is sealed by the host; a new unit has its own inputs.",state.permission_mode.guidance())}));
        let request_work=notebook.link_findings(work_state.unit_context(scheduler.id(),scheduler.order().map_or(&[],|o|o.finding_ids.as_slice())));
        compaction["work_projection"]=request_work["projection"].clone();

        if final_step {
            scoped_messages.push(json!({"role":"system","content":
                format!("The execution step limit ({max_steps}) has been reached. Tools are intentionally omitted from this final response request for budget reasons; this is NOT a filesystem or permission denial and does NOT mean prior tools were read-only. Report what was actually done and what remains unfinished. State that this run stopped at the step limit; do not report implementation completed unless writes actually succeeded. Answer now without requesting more tools.")
            }));
        }
        step_starts.push(messages.len());
        let mut body = json!({
            "model":model,"messages":scoped_messages,"tools":tools,"tool_choice":"auto","stream":true
        });
        compaction["request_sizes"]=json!({
            "message_chars":body["messages"].as_array().into_iter().flatten().map(|message|message["content"].as_str().map_or(0,|text|text.chars().count())).sum::<usize>(),
            "system_chars":body["messages"].as_array().into_iter().flatten().filter(|message|message["role"]=="system").map(|message|message["content"].as_str().map_or(0,|text|text.chars().count())).sum::<usize>(),
            "tool_schema_chars":body["tools"].to_string().chars().count(),
            "work_state_chars":request_work.to_string().chars().count(),
            "tool_count":body["tools"].as_array().map_or(0,Vec::len),
        });
        if final_step || scheduler.finished {
            if let Some(object) = body.as_object_mut() {
                object.remove("tools");
                object.remove("tool_choice");
            }
        }
        if let Some(effort) = state.reasoning_effort.as_deref() {
            body["reasoning_effort"] = json!(effort);
        }
        if state.fast_mode {
            body["service_tier"] = json!("fast");
        }
        compaction["context_window"]=crate::context_rebuild::prepare_worker(&state,&model,&task_id,&scheduler.scope(),&mut body,
            json!({"turn":turn,"step":step,"nodeId":context_node,"request_id":scheduler.request_started_turn}),&cancel,Duration::from_secs(180)).await?;
        let mut visual_context=worker_visual_context(&task_id,&scheduler);visual_context.turn=Some(turn);
        let visual_ids=scheduler.visual_input_ids();
        let visual_dispatch=crate::visual_artifacts::prepare_request(&state,"worker",&model,&visual_context,&visual_ids,scheduler.order().map(|o|o.visual_goal.as_deref().unwrap_or(&o.goal)).unwrap_or(&prompt),&mut body).await?;
        if !visual_ids.is_empty() {
            messages.push(crate::visual_artifacts::conversation_message(&visual_dispatch));
        }
        crate::context_window::prepare(&mut body,base_messages)?;
        compaction["visual_dispatch"]=visual_dispatch.clone();
        let process_identity = crate::work_executor::WorkExecutor::capture_identity(&scheduler, step);
        let order_rev = process_identity.revision;
        let work_id = process_identity.work_id.clone();
        let mut request_trace = crate::request_context::record(&root,&task_id,json!({
            "actor":"worker","stage":"execution","turn":turn,"step":step,"nodeId":context_node,
            "workId":work_id,"request_id":scheduler.request_started_turn,"revision":order_rev,"plan_revision":process_identity.plan_revision,
            "permission_mode":state.permission_mode,"final_step":final_step,"compaction":compaction,
            "visual_dispatch":visual_dispatch,
            "work":{"tool_calls":executed_tool_calls,"successful_file_writes":successful_file_writes,"repeated_read_calls":repeated_read_calls},
        }),&body).await;
        let executor_runtime = crate::work_executor::WorkExecutorRuntime {
            client: &state.client,
            provider_url: &state.provider_url,
            api_key: &state.api_key,
            root: &root,
            task_id: &task_id,
            turn,
            step,
            cancel: &cancel,
            visual_dispatch:&visual_dispatch,
        };
        let mut end_text = None;
        let round = crate::work_executor::WorkExecutor::next(
            &mut scheduler, &mut task_tree, &mut work_state, &mut source_working_set,
            &process_identity, &executor_runtime, &body, &mut request_trace,
            async |scheduler, task_tree, work_state, source_working_set, step_res| {
                let message = step_res.message;
                let text = step_res.text;
                let calls = step_res.tool_calls;
                if !visual_ids.is_empty() {emit(&root,&task_id,"visual/model_received",json!({"turn":turn,"step":step,"manifest":visual_dispatch})).await?;}

                let mut visible_content = Vec::new();
                if !text.is_empty() {
                    visible_content.push(json!({"type":"text","text":text}));
                }
                for call in &calls {
                    visible_content.push(json!({
                        "type":"tool-call","id":call.call_id,"name":call.name,"arguments":call.arguments
                    }));
                }
                messages.push(message);
                if let Some(start)=step_starts.last() {
                    step_owners.insert(*start,scheduler.scope());
                }

                if calls.is_empty() {
                    if !final_step && !scheduler.finished {
                        if text.trim().is_empty() {
                            // An empty model response is a host-observed protocol
                            // failure, not a Worker business conclusion or result.
                            scheduler.request_handoff("Worker response contained neither text nor a yield_work return.");
                            emit(&root,&task_id,"worker/yield",json!({"turn":turn,"step":step,"actor":"worker","identity":visual_context.identity,"workId":process_identity.work_id,"nodeId":active_flow_node,
                                "output":Value::Null,"failure_stage":"worker_empty_response"})).await?;
                            return Ok(crate::work_executor::ToolRoundOutput::stop(crate::work_executor::RoundDisposition::Yield));
                        }
                        // Plain text is the Worker's actual return. Seal only that
                        // invocation, then let Organizer decide the next step and
                        // whether the request goal was achieved.
                        scheduler.return_work(&json!({"summary":text}))?;
                        scheduler.retain_original_return(&text);
                        emit(&root,&task_id,"worker/yield",json!({"turn":turn,"step":step,"actor":"worker","identity":visual_context.identity,"workId":process_identity.work_id,"nodeId":active_flow_node,"output":scheduler.output()})).await?;
                        return Ok(crate::work_executor::ToolRoundOutput::stop(crate::work_executor::RoundDisposition::Yield));
                    }
                    if !visible_content.is_empty() {
                        emit_surface(&root,&task_id,"assistant/message",json!({"turn":turn,"step":step,"actor":"worker","identity":visual_context.identity,"workId":process_identity.work_id,
                            "message":{"id":uuid_like(),"role":"assistant","content":visible_content,
                                "source":{"kind":"model","provider":state.provider_url,"model":model}},"stream":[]}),Some("append")).await?;
                    }
                    if !task_tree.enabled() && !active_flow_node.is_empty() {
                        emit(&root,&task_id,"flow/node_state",json!({"turn":turn,"id":active_flow_node,
                            "status":if scheduler.done() {"done"} else {"running"}})).await?;
                    }
                    end_text = Some(text);
                    return Ok(crate::work_executor::ToolRoundOutput::stop(crate::work_executor::RoundDisposition::End));
                }

                if !visible_content.is_empty() {
                    emit_surface(&root,&task_id,"assistant/message",json!({
                        "turn":turn,"step":step,"actor":"worker","identity":visual_context.identity,"workId":process_identity.work_id,
                        "message":{"id":uuid_like(),"role":"assistant","content":visible_content,
                            "source":{"kind":"model","provider":state.provider_url,"model":model}},
                        "stream":[]
                    }),Some("append")).await?;
                }

                let repeats_at_start=repeated_read_calls;
                let mut project_operations=0;
                let mut step_tools = calls.iter().map(|call|call.name.clone()).collect::<Vec<String>>();
                for call in calls {
                    let mut args: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
                    if matches!(call.name.as_str(),"edit_file"|"replace_range"|"write_file") {
                        if let Some(path)=args["path"].as_str() {
                            if let Ok(resolved)=crate::file_edit::workspace_path(&root,path) {
                                if let Ok(relative)=resolved.strip_prefix(root.canonicalize()?) {args["path"]=json!(relative.to_string_lossy().replace('\\',"/"));}
                            }
                        }
                    }
                    let read_key = (repeatable_read_tool(&call.name) && !crate::worker_read_cache::is_source_read(&call.name))
                        .then(|| format!("{}:{}", call.name, args));
                    let cached_read = read_key.as_ref().and_then(|key| read_results.get(key)).cloned();
                    emit(&root,&task_id,"tool/call",json!({
                        "turn":turn,"step":step,"actor":"worker","identity":visual_context.identity,"workId":process_identity.work_id,"callId":call.call_id,"name":call.name,
                        "arguments":call.arguments,"flowNodeId":active_flow_node
                    })).await?;
                    let started = Instant::now();
                    // Run a check against the exact recorded versions, including changes
                    // made by an external editor since the previous model round.
                    if crate::project_process::is_check(&call.name) {refresh_scheduler_versions(&mut *scheduler,&state.workspace).await?;}
                    let project_args_error=if matches!(call.name.as_str(),"install_dependencies"|"run_project_script"|"get_project_process") {crate::project_process::normalize_args(state.workspace.root(),&mut args).err()}
                        else if call.name=="run_program" {crate::program_execution::normalize_args(state.workspace.root(),&mut args).err()} else {None};
                    let order_requests_process_refresh=scheduler.order().and_then(|order|order.project_observation.as_ref())
                        .is_some_and(|spec|spec["force_refresh"]==true);
                    let reused_process_observation=if call.name=="get_project_process"&&!order_requests_process_refresh
                        &&crate::project_process::can_reuse_process_observation(&args) {
                        scheduler.reusable_project_observation(state.workspace.root(),&args)
                    }else{None};
                    let reused_http_probe=if call.name=="http_probe" {scheduler.reusable_http_probe(&args)} else {None};
                    let http_probe_reused=reused_http_probe.is_some();
                    let command_versions=scheduler.versions();
                    let output: anyhow::Result<Value> = if call.name == "run_command" {
                        Ok(json!({"outcome":"disabled","error":"run_command has been removed and was not executed. Use run_program with an allowlisted program and args array, or use a dedicated project, file, HTTP, or browser tool."}))
                    } else if (call.name == "run_program" || crate::project_process::is_execution(&call.name)) && ban_run_command {
                        Err(anyhow::anyhow!("project execution is blocked because the current turn explicitly prohibits command execution"))
                    } else if !state.permission_mode.allows(&call.name) {
                        Err(anyhow::anyhow!("permission_denied: {:?} does not allow {}", state.permission_mode, call.name))
                    } else if !tools.iter().any(|tool|tool.pointer("/function/name").and_then(Value::as_str)==Some(call.name.as_str())) {
                        Err(anyhow::anyhow!("tool_unavailable: {} was not offered in this step",call.name))
                    } else if let Some(error)=project_args_error {
                        Err(error)
                    } else if let Err(error)=crate::work_executor::WorkExecutor::ensure_active_work(&scheduler) {
                        Err(error)
                    } else if let Some(observation)=reused_process_observation {
                        Ok(json!({"processes":observation["processes"],"process_observation":observation,"reused":true,
                            "guidance":"Reused a fresh scoped process observation from an earlier work packet; no new host process query was run."}))
                    } else if let Some(observation)=reused_http_probe {
                        Ok(observation)
                    } else if call.name=="yield_work" {
                        if args["findings"].is_array() {
                            let report=json!({"current_node_id":scheduler.node(),"purpose":scheduler.order().map(|o|&o.goal),"findings":args["findings"]});
                            for revision in work_state.report_at(&report,turn,step,source_working_set.materials(),&prompt) {emit(&root,&task_id,"worker/finding_revision",revision).await?;}
                        }
                        let mut ret = if let Some(check)=args.get("visual_check_result").filter(|check|!check.is_null()) {
                            let context=worker_visual_context(&task_id,scheduler);
                            match crate::visual_artifacts::validate_check(&root,&context,check,scheduler.frame().map(|f|f.visual_requests.as_slice()).unwrap_or(&[]),"worker") {
                                Ok(verified)=>{scheduler.frame_mut().unwrap().visual_check_result=verified.clone();emit(&root,&task_id,"visual/check_result",json!({"turn":turn,"step":step,"result":verified})).await?;
                                    crate::work_executor::WorkExecutor::handle_yield_work(&mut *scheduler,&args)},
                                Err(error)=>Err(error),
                            }
                        }else{crate::work_executor::WorkExecutor::handle_yield_work(&mut *scheduler, &args)};
                        if let Ok(ref mut output) = ret {
                            scheduler.retain_original_return(&call.arguments);
                            output["original_return"]=json!(call.arguments);
                            emit(&root,&task_id,"worker/yield",json!({"turn":turn,"step":step,"actor":"worker","identity":visual_context.identity,"workId":process_identity.work_id,"nodeId":active_flow_node,"output":output})).await?;
                        }
                        ret
                    } else if crate::worker_read_cache::is_source_read(&call.name)
                        && args.get("force_read").and_then(Value::as_bool) == Some(true)
                        && args.get("reread_reason").and_then(Value::as_str).is_none_or(|reason| reason.trim().is_empty()) {
                        Err(anyhow::anyhow!("force_read requires a concrete reread_reason, such as exact source needed for an edit or source no longer available in current context"))
                    } else if call.name == "recall_work" {
                        match worker_material_ids(&args, scheduler, source_working_set) {
                            Err(error) => Err(error),
                            Ok(_) => match notebook.recall(&args).await {
                                Ok(materials) => {
                                    if args["include_material"] == true {
                                        source_working_set.select_action(&args);
                                        source_working_set.add(&materials["materials"],work_state.edit_targets());
                                    }
                                    Ok(json!({"notebook":materials,"source_context":source_working_set.metadata()}))
                                },
                                Err(error) => Err(error),
                            },
                        }
                    } else if call.name == "spawn_subagent" {
                        if !state.enable_subagent
                            || state.expert_provider_url.is_empty()
                            || state.expert_model.is_empty()
                            || is_child
                            || subagent_spawned
                        {
                            Err(anyhow::anyhow!("subagent is unavailable: only one child per parent task, with no nesting"))
                        } else {
                            run_subagent(&state, &task_id, &model, args.clone(), &cancel, &mut subagent_spawned, ban_run_command).await
                        }
                    } else if (call.name == "run_program" || crate::project_process::is_execution(&call.name)) && ban_run_command {
                        Err(anyhow::anyhow!("project execution is blocked because the current turn explicitly prohibits command execution"))
                    } else {
                        let workspace = state.workspace.clone();
                        let catalog = state.tool_catalog.clone();
                        let name = call.name.clone();
                        let tool_args = args.clone();
                        let mut visual_context=worker_visual_context(&task_id,scheduler);visual_context.source_tool_call_id=call.call_id.clone();visual_context.source_event_id=format!("{task_id}:tool:{}",call.call_id);
                        let tracked = crate::workspace_changes::is_mutation(&name);
                        let tracking_state = state.clone();
                        let tracking_task = task_id.clone();
                        let tracking_cancel = cancel.clone();
                        let mut tool_worker = tokio::spawn(async move {
                            if tracked {
                                crate::workspace_changes::execute_tracked(tracking_state, tracking_task, turn, name, tool_args, tracking_cancel).await
                            } else if crate::browser_control::is_tool(&name) {
                                crate::browser_control::execute_scoped(&workspace,&visual_context,&name,&tool_args).await
                            } else {
                                execute_tool(workspace, catalog.as_deref(), &name, tool_args).await
                            }
                        });
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                if tracked {
                                    match tool_worker.await {
                                        Ok(result)=>result,
                                        Err(error)=>Err(anyhow::anyhow!("tool {} stopped during cancellation: {error}",call.name)),
                                    }
                                } else {
                                    tool_worker.abort();
                                    Err(anyhow::anyhow!("Tool execution cancelled."))
                                }
                            }
                            result = &mut tool_worker => match result {
                                Ok(result) => result,
                                Err(error) => return Err(anyhow::anyhow!("tool {} stopped unexpectedly: {error}", call.name)),
                            },
                        }
                    };
                    let (mut result, execution_error) = match output {
                        Ok(value) => {
                            let failed_child = call.name == "spawn_subagent" && value["status"] != "completed";
                            (value, failed_child)
                        }
                        Err(error) => (json!({"error":format!("{error:#}")}), true),
                    };
                    let is_error = execution_error || if call.name=="run_program" {
                        result["outcome"]!="exited" || result["process_success"]!=true
                    } else if crate::project_process::is_tool(&call.name) {
                        result.get("error").is_some() || result["termination_reason"]=="wait_error"
                            || result["status"].as_i64().is_some_and(|code|code!=0) && result["termination_reason"]!="requested_stop"
                    } else {observer_output_failed(&result.to_string())};
                    let material_recalled = !is_error && call.name=="recall_work" && result.pointer("/notebook/materials").and_then(Value::as_array)
                        .is_some_and(|materials|materials.iter().any(|material|material["available"]==true));
                    let query_result_recalled=!is_error && result.get("tool_results").is_some();
                    if material_recalled { step_tools.push("notebook/source".to_owned()); }
                    if query_result_recalled {step_tools.push("notebook/query_result".to_owned());}
                    executed_tool_calls += 1;
                    if !matches!(call.name.as_str(),"respond_observer"|"consult_observer"|"yield_work") { project_operations += 1; }
                    if !is_error && result["changed"] != false && matches!(call.name.as_str(), "write_file" | "replace_range" | "edit_file") {
                        successful_file_writes += 1;
                    }
                    if !is_error && crate::worker_read_cache::is_source_read(&call.name) {
                        // Related type bodies are independent notebook materials. They
                        // must survive after the parent symbol's tool message leaves.
                        match notebook.save(turn,step,&active_flow_node,&call.name,&result).await {
                            Ok(Some(material)) => {
                                result["notebook_material"] = material.clone();
                                let source_args=json!({"material_ids":[material["id"]],"include_material":true,
                                    "start_line":result.get("start_line").or_else(||result.pointer("/symbol/start_line")),
                                    "end_line":result.get("end_line").or_else(||result.pointer("/symbol/end_line")),
                                    "start_column":result["start_column"],
                                    "max_chars":result["returned_chars"].as_u64().unwrap_or(24_000).clamp(1000,58_000)});
                                match notebook.recall(&source_args).await {
                                    Ok(pages)=>{source_working_set.add(&pages["materials"],work_state.edit_targets());result["source_context"]=source_working_set.metadata();},
                                    Err(error)=>tracing::warn!(%task_id,%error,"could not retain read page in source working set"),
                                }
                            },
                            Ok(None) => {},
                            Err(error) => { tracing::warn!(%task_id,%error,"could not retain source material"); result["notebook_capture_error"]=json!("Source material could not be saved; this result is still usable."); },
                        }
                        let mut related=result.get("related_types").and_then(Value::as_array).cloned().unwrap_or_default();
                        for page in &mut related {
                            match notebook.save(turn,step,&active_flow_node,"read_ts_symbol",page).await {
                                Ok(Some(material))=>{
                                    page["notebook_material"]=material.clone();
                                    let source_args=json!({"material_ids":[material["id"]],"include_material":true,
                                        "start_line":page["start_line"],"end_line":page["end_line"],"max_chars":12_000});
                                    if let Ok(pages)=notebook.recall(&source_args).await {source_working_set.add(&pages["materials"],work_state.edit_targets());}
                                },
                                Ok(None)=>{},
                                Err(error)=>{page["notebook_capture_error"]=json!(error.to_string());},
                            }
                        }
                        if !related.is_empty() {result["related_types"]=json!(related);}
                        result["source_context"]=source_working_set.metadata();
                    }
                    if !is_error && matches!(call.name.as_str(), "write_file" | "replace_range" | "edit_file") {
                        // Release invalid old pages before adding the edited definition.
                        // Otherwise the old and new versions compete for the same budget.
                        source_working_set.refresh(&mut notebook,work_state.edit_targets()).await?;
                        for revision in work_state.refresh_source_refs(source_working_set.materials(),&source_working_set.metadata(),turn,step) {
                            emit(&root,&task_id,"worker/finding_revision",revision).await?;
                        }
                        if let Some(path) = args["path"].as_str() {
                            let (workspace,path) = (state.workspace.clone(),path.to_owned());
                            let post_edit = tokio::task::spawn_blocking(move || workspace.read_file(crate::tools::ReadFileRequest {workspace_root:None,path,max_bytes:16*1024*1024})).await;
                            if let Ok(Ok(source))=post_edit {
                                match notebook.save(turn,step,&active_flow_node,"read_file",&serde_json::to_value(&source)?).await {
                                    Ok(Some(material))=>{
                                        result["notebook_material"]=material.clone();
                                        let source=source.content.as_str();
                                        let mut ranges=Vec::<(u64,u64)>::new();
                                        let anchors=args["edits"].as_array().into_iter().flatten().filter_map(|edit|edit["new_text"].as_str())
                                            .chain(args["replacement"].as_str()).filter(|text|!text.is_empty());
                                        for text in anchors {
                                            for (offset,_) in source.match_indices(text).take(8) {
                                                let start=source[..offset].bytes().filter(|byte|*byte==b'\n').count() as u64+1;
                                                let end=start+text.bytes().filter(|byte|*byte==b'\n').count() as u64;
                                                ranges.push((start.saturating_sub(12).max(1),end+12));
                                            }
                                        }
                                        if ranges.is_empty(){let start=args["start_line"].as_u64().unwrap_or(1);ranges.push((start.saturating_sub(12).max(1),start+160));}
                                        ranges.sort_unstable();
                                        let mut merged=Vec::<(u64,u64)>::new();
                                        for (start,end) in ranges {if let Some(last)=merged.last_mut().filter(|last|start<=last.1+1){last.1=last.1.max(end);}else{merged.push((start,end));}}
                                        merged.truncate(16);
                                        result["changes"]=json!(merged.iter().map(|(start,end)|json!({"path":args["path"],"start_line":start,"end_line":end,"material_id":material["id"],"code_hash":result["code_hash"]})).collect::<Vec<_>>());
                                        for (start,end) in merged {
                                            let pages=notebook.recall(&json!({"material_ids":[material["id"]],"include_material":true,"start_line":start,"end_line":end,"max_chars":24_000})).await;
                                            if let Ok(pages)=pages {source_working_set.add(&pages["materials"],work_state.edit_targets());}
                                        }
                                    },
                                    Ok(None)=>{},
                                    Err(error)=>{tracing::warn!(%task_id,%error,"could not capture post-edit notebook source");result["notebook_capture_error"]=json!("File changed, but its post-edit material snapshot could not be saved.");},
                                }
                            } else {
                                result["notebook_capture_error"]=json!("File changed, but its post-edit material snapshot is unavailable or exceeds 16 MiB. Use a focused source read if exact code is needed.");
                            }
                        }
                    }
                    let overlap_read=if !is_error {source_coverage.filter(&call.name,&args,&mut result)} else {false};
                    let query_fingerprint=crate::source_read::query_fingerprint(&result);
                    let repeated_query=!is_error && cached_read.as_ref().is_some_and(|previous|previous.0==query_fingerprint);
                    if repeated_query {
                        result["repeated_query"]=json!({"previous_tool_call_id":cached_read.as_ref().unwrap().1,"freshly_executed":true,
                            "guidance":"This query was checked against current state and returned the same information. Use the known findings unless a concrete unanswered question requires more discovery."});
                    }
                    if overlap_read || repeated_query || http_probe_reused {repeated_read_calls=repeated_read_calls.saturating_add(1);}
                    if !is_error {
                        if crate::project_process::may_change_files(&call.name) {source_coverage.clear();crate::source_read::clear();}
                        else if result["changed"]!=false && matches!(call.name.as_str(),"write_file"|"replace_range"|"edit_file") {
                            if let Some(path)=result["path"].as_str().or_else(||args["path"].as_str()){source_coverage.forget(path);}
                        }
                    }
                    if crate::project_process::is_check(&call.name) {refresh_scheduler_versions(&mut *scheduler,&state.workspace).await?;}
                    if crate::project_process::is_check(&call.name) && call.name!="install_dependencies" && call.name!="http_probe" && command_versions!=scheduler.versions() {result["verification_inputs_changed"]=json!(true);}
                    if call.name=="http_probe" {scheduler.record_http_probe(&args,&mut result,&call.call_id);}
                    // Deliver the new tool result intact. Capacity is handled
                    // by the complete request window, not a per-result cutoff.
                    let tool_text=result.to_string();
                    if let Some(key)=read_key {
                        if !is_error {if read_results.len()>=128 {read_results.clear();}read_results.insert(key,(query_fingerprint,call.call_id.clone()));}
                    }
                    let result_message = json!({
                        "id":uuid_like(),"role":"tool","source":{"kind":"tool","callId":call.call_id},
                        "toolCallId":call.call_id,"content":[{"type":"text","text":tool_text}],"isError":is_error
                    });
                    emit_surface(&root,&task_id,"tool/result",json!({
                        "turn":turn,"step":step,"actor":"worker","identity":visual_context.identity,"workId":process_identity.work_id,"flowNodeId":active_flow_node,"message":result_message,
                        "meta":{"durationMs":started.elapsed().as_millis(),"result":result},
                    }),Some("append")).await?;
                    messages.push(crate::session_history::sourced_tool_result(&root,&task_id,crate::format_translate::openai_chat_tool_result_message(&call, &tool_text),&call.call_id).await?);
                    if call.name=="read_session_history" {messages.extend(crate::session_history::history_image_messages(&result));}

                    scheduler.observe(&call.name,&args,&result,is_error);
                    // A failed check returns to this Worker's next implementation/check
                    // round in the same frame. Only explicit blockers/splits hand off.
                    work_state.observe(step, &call.name, &args, &result, is_error, repeated_query || overlap_read || http_probe_reused);
                    if !is_error && repeatable_read_tool(&call.name) && !crate::worker_read_cache::is_source_read(&call.name) {work_state.record_query(step,&call.name,&args,&result,&call.call_id);}
                    if crate::worker_read_cache::is_source_read(&call.name) || crate::project_process::is_tool(&call.name) || matches!(call.name.as_str(), "write_file" | "replace_range" | "edit_file" | "run_program") {
                        emit(&root, &task_id, "worker/work_state", json!({"turn":turn,"step":step,"state":work_state.snapshot()})).await?;
                    }
                    recent_actions.push(json!({
                        "tool":call.name,"arguments":args,"failed":is_error,
                        "repeated":repeated_query || overlap_read || http_probe_reused,"outcome":truncate(&tool_text, 900),
                    }));
                    if recent_actions.len() > 12 {
                        recent_actions.drain(..recent_actions.len() - 12);
                    }
                    if cancel.is_cancelled() {
                        return Ok(crate::work_executor::ToolRoundOutput { operations_count: project_operations,
                            repeated_reads: repeated_read_calls.saturating_sub(repeats_at_start), disposition: crate::work_executor::RoundDisposition::Cancelled });
                    }
                }
                work_state.finish_step(&step_tools.iter().map(String::as_str).collect::<Vec<_>>());
                Ok(crate::work_executor::ToolRoundOutput {
                    operations_count: project_operations,
                    repeated_reads: repeated_read_calls.saturating_sub(repeats_at_start),
                    disposition: crate::work_executor::RoundDisposition::Continue,
                })
            },
        ).await;
        let round = match round {
            Ok(round) => round,
            Err(_error) if cancel.is_cancelled() => {
                emit(&root,&task_id,"step/end",json!({"turn":turn,"step":step})).await?;
                observer.finish("cancelled").await?;finish_cancelled_run(&state, &root, &task_id, turn, &active_flow_node).await?;
                return Ok(());
            }
            Err(error) => {
                let message=format!("{error:#}");
                crate::request_context::finish(request_trace.take(),"execution_error",json!({"error":message,"visual_dispatch":visual_dispatch})).await;
                let visual_failure=error.downcast_ref::<crate::visual_probe::VisualModelFailure>();
                if visual_failure.is_some() {
                    scheduler.visual_response_failed(&visual_dispatch,&message);
                    emit(&root,&task_id,"visual/model_failed",json!({"turn":turn,"step":step,"actor":"worker","manifest":visual_dispatch,"error":message})).await?;
                }
                attach_handoff_error(&mut scheduler.handoff,"execution_error",
                    json!({"stage":"worker_execution","error":message,"work_id":process_identity.work_id,
                        "node_id":process_identity.node_id,"revision":process_identity.revision,"visual_request":visual_dispatch}));
                let image_request_failed=visual_failure.is_some() && visual_dispatch["status"]=="direct";
                if image_request_failed {
                    if visual_failure.and_then(|failure|failure.error.downcast_ref::<crate::visual_probe::HttpModelFailure>()).is_some_and(|failure|
                        crate::visual_probe::explicitly_rejects_images(failure.status,&failure.body)) {
                        state.visual.capabilities.entry(state.provider_name.clone()).or_default().insert(model.clone(),crate::visual_artifacts::ImageCapability::Unsupported);
                    }
                    messages.push(json!({"role":"system","content":format!("The previous model request with image content failed: {message}. Its failed visual dispatch is {}. No tools from that request ran and the images were not visually verified. Use the raw failure and existing task facts to decide the next action; selecting an image again requires an explicit view_image call.",visual_dispatch)}));
                }
                let flow_plan=scheduler.unified_flow_plan(if task_tree.enabled(){Some(&task_tree)}else{None});
                crate::observer_service::commit(&root,&task_id,json!({
                    "commit_id":format!("{turn}:{step}:{}:execution-error",scheduler.plan_revision),
                    "turn":turn,"step":step,"scheduler":scheduler.snapshot(),"task_tree":task_tree.snapshot(),
                    "worker_state":work_state.snapshot(),"flow_plan":flow_plan.clone(),
                    "active_node_id":flow_plan["active_node_id"],"active_path":flow_plan["active_path"]
                }),Vec::new()).await?;
                if image_request_failed {
                    observer.set_scope(&scheduler,&prompt);
                    emit(&root,&task_id,"step/end",json!({"turn":turn,"step":step,"reason":"image_request_failed","error":message})).await?;
                    // Return the failure to Organizer's ordinary decision path.
                    // No fixed image retry or host business judgment is added.
                    continue;
                }
                fail_turn(&state, &root, &task_id, turn, step, &observer_model, &prompt, message,
                    "WORKER_EXECUTION_FAILED","worker_execution").await?;
                observer.finish("failed").await?;
                return Ok(());
            }
        };
        emit(&root,&task_id,"execution/tick",json!({"turn":turn,"step":step,"tick":round.tick})).await?;
        if task_tree.enabled() {
            if task_tree.active()==scheduler.node(){task_tree.remember_sources(source_working_set.snapshot(),work_state.edit_targets());}
        }
        let mut sealed_flow_plan=scheduler.unified_flow_plan(if task_tree.enabled(){Some(&task_tree)}else{None});
        if sealed_flow_plan["nodes"].as_array().is_some_and(Vec::is_empty) {
            sealed_flow_plan["nodes"]=json!([{"id":"request","title":truncate(&prompt,160),"kind":"worker","objective":prompt,
                "status":if scheduler.finished{"done"}else{"running"}}]);
            sealed_flow_plan["active_node_id"]=json!(if scheduler.finished{""}else{"request"});
        }
        latest_flow_plan=sealed_flow_plan;
        crate::observer_service::commit(&root,&task_id,json!({
            "commit_id":format!("{turn}:{step}:{}:sealed",scheduler.plan_revision),"turn":turn,"step":step,
            "scheduler":scheduler.snapshot(),"task_tree":task_tree.snapshot(),
            "flow_plan":latest_flow_plan.clone(),
            "active_node_id":latest_flow_plan["active_node_id"],"active_path":latest_flow_plan["active_path"]
        }),Vec::new()).await?;
        observer.set_scope(&scheduler,&prompt);
        emit(&root,&task_id,"worker/source_working_set",json!({"turn":turn,"step":step,"state":source_working_set.snapshot(),"retention":source_working_set.metadata()})).await?;
        emit(&root, &task_id, "worker/work_state", json!({"turn":turn,"step":step,"state":work_state.snapshot()})).await?;
        emit(&root, &task_id, "step/end", json!({"turn":turn,"step":step})).await?;
        match round.disposition {
            crate::work_executor::RoundDisposition::End => {
                let status = if final_step { "max_steps" } else { "completed" };
                let reason = if final_step {
                    json!({"kind":"error","error":{"message":format!("reached max step limit ({max_steps})"),"code":"MAX_STEPS"}})
                } else { json!({"kind":"completed"}) };
                emit(&root,&task_id,"turn/end",json!({"turn":turn,"reason":reason})).await?;
                finish(&state,&task_id,status).await?;
                observer.finish(status).await?;
                return Ok(());
            },
            crate::work_executor::RoundDisposition::Cancelled => {
                observer.finish("cancelled").await?;finish_cancelled_run(&state,&root,&task_id,turn,&active_flow_node).await?;
                return Ok(());
            },
            crate::work_executor::RoundDisposition::Yield => continue,
            _ => {},
        }

    }

    emit(&root,&task_id,"turn/end",json!({
        "turn":turn,"reason":{"kind":"error","error":{
            "message":format!("reached max step limit ({max_steps})"),"code":"MAX_STEPS"
        }}
    })).await?;
    finish(&state, &task_id, "max_steps").await?;
    observer.finish("max_steps").await?;
    Ok(())
}
async fn persist_organizer_failure(
    root:&std::path::Path,
    task_id:&str,
    turn:usize,
    step:usize,
    scheduler:&mut crate::work_scheduler::WorkScheduler,
    task_tree:&crate::flow_tree::TaskTree,
    stage:&str,
    failure:Value,
)->anyhow::Result<()> {
    scheduler.finished=false;
    scheduler.request_completed=Some(false);
    attach_organizer_failure(&mut scheduler.handoff,failure);
    let flow_plan=scheduler.unified_flow_plan(if task_tree.enabled(){Some(task_tree)}else{None});
    crate::observer_service::commit(root,task_id,json!({
        "commit_id":format!("{turn}:{step}:{}:organizer-failure:{stage}",scheduler.plan_revision),
        "turn":turn,"step":step,"scheduler":scheduler.snapshot(),"task_tree":task_tree.snapshot(),
        "flow_plan":flow_plan.clone(),"active_node_id":flow_plan["active_node_id"].clone(),"active_path":flow_plan["active_path"].clone()
    }),Vec::new()).await?;
    Ok(())
}

async fn fail_turn(
    state: &AgentServiceState,
    root: &std::path::Path,
    task_id: &str,
    turn: usize,
    step: usize,
    _observer_model: &str,
    _prompt: &str,
    message: String,
    code:&str,
    stage:&str,
) -> anyhow::Result<()> {
    emit(root, task_id, "step/end", json!({"turn":turn,"step":step})).await?;
    emit(
        root,
        task_id,
        "turn/end",
        json!({"turn":turn,"reason":{"kind":"error","error":{"message":message,"code":code,"stage":stage}}}),
    )
    .await?;
    finish(state, task_id, "failed").await?;
    Ok(())
}

fn subagent_tool() -> Value {
    json!({"type":"function","function":{
        "name":"spawn_subagent",
        "description":"Delegate one focused task to a single subagent. The parent waits for completion. The child shares workspace tools, memory, and code indexes, records its own steps, and cannot spawn another agent. At most one child may be started per parent task.",
        "parameters":{"type":"object","required":["prompt"],"properties":{
            "prompt":{"type":"string","description":"Self-contained work request and expected report for the subagent"}
        }}
    }})
}

fn run_subagent<'a>(
    state: &'a AgentServiceState,
    parent_id: &'a str,
    parent_model: &'a str,
    args: Value,
    parent_cancel: &'a CancellationToken,
    spawned: &'a mut bool,
    ban_run_command: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Value>> + Send + 'a>> {
    Box::pin(async move {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        anyhow::ensure!(
            !prompt.is_empty(),
            "spawn_subagent requires a nonempty prompt"
        );
        let child_id = format!("task_{}", uuid_like());
        let root = state.workspace.root().to_path_buf();
        let insert_id = child_id.clone();
        let insert_parent = parent_id.to_owned();
        let child_model = if state.subagent_inherits_model { parent_model.to_owned() } else { state.expert_model.clone() };
        let child_provider_name = if state.subagent_inherits_model { state.provider_name.clone() } else { state.expert_provider_name.clone() };
        let child_reasoning_effort = state.subagent_inherits_model.then(|| state.reasoning_effort.clone()).flatten();
        let child_fast_mode = state.subagent_inherits_model && state.fast_mode;
        let insert_model = child_model.clone();
        let insert_provider_name = child_provider_name.clone();
        let insert_reasoning_effort = child_reasoning_effort.clone();
        let child_prompt = prompt.to_owned();
        let insert_prompt = child_prompt.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let conn = open_db(&root)?;
        conn.execute(
            "INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at,parent_task_id,provider_name,reasoning_effort,fast_mode) VALUES (?1,?2,?3,'running',?4,?4,?5,?6,?7,?8)",
            params![insert_id,insert_prompt,insert_model,now(),insert_parent,insert_provider_name,insert_reasoning_effort,child_fast_mode],
        )?;
        Ok(())
    }).await??;

        let root = state.workspace.root().to_path_buf();
        if let Err(error) = emit(
            &root,
            parent_id,
            "subagent/start",
            json!({"child_task_id":child_id,"prompt":prompt}),
        )
        .await {
            finish(state, &child_id, "failed").await?;
            return Err(error);
        }
        *spawned = true;
        let child_cancel = parent_cancel.child_token();
        let child_run_id = uuid_like();
        state
            .cancellations
            .lock()
            .await
            .insert(child_id.clone(), ActiveTask { run_id: child_run_id.clone(), token: child_cancel.clone(),
                flow_node: Arc::new(Mutex::new("turn_1:1".into())), node_interrupt: Arc::new(Mutex::new(None)) });
        let mut child_state = state.clone();
        child_state.provider_name = child_provider_name;
        child_state.provider_url = state.expert_provider_url.clone();
        child_state.api_key = state.expert_api_key.clone();
        child_state.default_model = child_model.clone();
        child_state.reasoning_effort = child_reasoning_effort;
        child_state.fast_mode = child_fast_mode;
        let runner_id = child_id.clone();
        let runner_run_id = child_run_id.clone();
        let runner_model = child_model;
        let handle = tokio::spawn(async move {
            let result = Box::pin(run_task(
                child_state.clone(),
                runner_id.clone(),
                runner_model,
                child_prompt,
                SUBAGENT_MAX_STEPS,
                child_cancel,
                true,
                1,
                Vec::new(),
                false,
                ban_run_command,
            ))
            .await;
            clear_active_task(&child_state, &runner_id, &runner_run_id).await;
            result
        });
        if let error @ (Err(_) | Ok(Err(_))) = handle.await {
            clear_active_task(state, &child_id, &child_run_id).await;
            finish(state, &child_id, "failed").await?;
            if let Err(cleanup_error) = cleanup_finished_task_data(state.workspace.root(), &child_id).await {
                tracing::warn!(task_id = %child_id, error = %cleanup_error, "could not clean failed subagent execution data");
            }
            emit(
                &root,
                parent_id,
                "subagent/end",
                json!({"child_task_id":child_id,"status":"failed"}),
            )
            .await?;
            anyhow::bail!("subagent {child_id} stopped unexpectedly: {error:?}");
        }

        cleanup_finished_task_data(state.workspace.root(), &child_id).await?;

        let read_id = child_id.clone();
        let root = state.workspace.root().to_path_buf();
        let (status, answer) = tokio::task::spawn_blocking(move || -> anyhow::Result<(String, String)> {
        let conn = open_db(&root)?;
        let status: String = conn.query_row("SELECT status FROM agent_tasks WHERE id=?1", [&read_id], |r| r.get(0))?;
        let raw: Option<String> = conn.query_row(
            "SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='assistant/message' ORDER BY seq DESC LIMIT 1",
            [&read_id], |r| r.get(0),
        ).optional()?;
        let answer = raw.and_then(|data| serde_json::from_str::<Value>(&data).ok())
            .and_then(|data| data.pointer("/message/content").and_then(Value::as_array).cloned())
            .map(|blocks| blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        Ok((status, answer))
    }).await??;
        emit(
            &state.workspace.root().to_path_buf(),
            parent_id,
            "subagent/end",
            json!({"child_task_id":child_id,"status":status}),
        )
        .await?;
        Ok(json!({"child_task_id":child_id,"status":status,"answer":answer}))
    })
}

fn local_tools(
    _workspace: &crate::tools::Workspace,
    catalog: Option<&crate::plugin_builtin::ToolCatalog>,
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
    definitions.as_array().into_iter().flatten().filter(|tool| tool.get("name").and_then(Value::as_str).is_some_and(|n| (!ban_run_command || (n != "run_program" && !crate::project_process::is_execution(n))) && catalog.map_or(allowed.contains(&n), |catalog| catalog.contains(n)))).map(|tool| {
        let name=tool["name"].as_str().unwrap_or_default(); let description=tool["description"].as_str().unwrap_or_default();
        let mut schema=tool.get("inputSchema").cloned().unwrap_or_else(||json!({"type":"object","properties":{}}));
        if let Some(props)=schema.get_mut("properties").and_then(Value::as_object_mut) {props.remove("workspace_root");}
        if let Some(required)=schema.get_mut("required").and_then(Value::as_array_mut) {required.retain(|name| name.as_str()!=Some("workspace_root"));}
        if crate::worker_read_cache::is_source_read(name) {
            if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                props.insert("force_read".into(), json!({"type":"boolean","default":false,"description":"Return the exact requested source even if previously read. Use when preparing an edit or when needed source is no longer in current context; requires reread_reason."}));
                props.insert("reread_reason".into(), json!({"type":"string","description":"Concrete reason the previously read source is needed again. Required when force_read=true."}));
            }
        }
        json!({"type":"function","function":{"name":name,"description":description,"parameters":schema}})
    }).collect()
}

fn can_run_check(state: &AgentServiceState, ban_run_command: bool) -> bool {
    local_tools(&state.workspace, state.tool_catalog.as_deref(), ban_run_command).iter()
        .any(|tool| tool.pointer("/function/name").and_then(Value::as_str)
            .is_some_and(|name| crate::project_process::is_check(name) && state.permission_mode.allows(name)))
}

pub(crate) async fn execute_tool(
    workspace: Arc<crate::tools::Workspace>,
    catalog: Option<&crate::plugin_builtin::ToolCatalog>,
    name: &str,
    mut args: Value,
) -> anyhow::Result<Value> {
    if let Some(catalog) = catalog {
        return catalog.execute(workspace, name, args).await;
    }
    if crate::browser_control::is_tool(name) {return crate::browser_control::execute(&workspace,name,&args).await;}
    if crate::http_probe::is_tool(name) {return crate::http_probe::execute(&args).await;}
    if crate::project_process::is_tool(name) {return crate::project_process::execute(&workspace,name,args).await;}
    if name == "run_command" { anyhow::bail!("run_command has been removed and was not executed; use run_program or a dedicated tool"); }
    if crate::program_execution::is_tool(name) { return crate::program_execution::execute(&workspace, &args).await; }
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

    #[tokio::test]
    async fn temporary_observer_keeps_current_failure_intact_without_previous_context() {
        let root=std::env::temp_dir().join(format!("observer-original-window-{}",uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        open_db(&root).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        let captured=Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let received=captured.clone();
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let server=tokio::spawn(async move {
            axum::serve(listener,axum::Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>| {
                let received=received.clone();async move {
                    received.lock().unwrap().push(body);
                    Json(json!({"choices":[{"message":{"role":"assistant","content":"{\"assessment\":\"uncertain\",\"summary\":\"ORIGINAL_OBSERVER_REPLY\"}"}}]}))
                }
            }))).await.unwrap();
        });
        let mut state=flow_test_state(&root,address);state.observer_provider_url=state.provider_url.clone();
        let failure=format!("{} ORIGINAL_FAILURE_TAIL","诊断信息".repeat(5000));
        let input=json!({"stage":"handoff","identity":{"task_id":"task"},"request":{"goal":"load"},"recent_activity":[
            {"seq":1,"kind":"tool/call","call_id":"upload","payload":{"name":"browser_upload","arguments":"{\"path\":\"sample.pptx\"}"}},
            {"seq":2,"kind":"tool/result","call_id":"upload","is_error":true,"payload":{"reason":failure}}]});
        observer_json_response(&state,"fake-model","review",input,1000,"task",json!({})).await.unwrap();
        observer_json_response(&state,"fake-model","review",json!({"stage":"progress","identity":{"task_id":"task"},"recent_activity":[
            {"seq":3,"kind":"tool/result","payload":{"recovery":"NEW_RECOVERY"}}]}),1000,"task",json!({})).await.unwrap();
        let requests=captured.lock().unwrap();let text=requests[1]["messages"].to_string();
        assert!(!text.contains("Called method browser_upload"));
        assert!(!text.contains("ORIGINAL_FAILURE_TAIL"));
        assert!(!text.contains("ORIGINAL_OBSERVER_REPLY"));assert!(text.contains("NEW_RECOVERY"));
        assert!(requests[0]["messages"].as_array().unwrap().iter().any(|message|message["content"].as_str().and_then(|text|serde_json::from_str::<Value>(crate::session_history::original_text(text)).ok()).is_some_and(|value|value["reason"]==failure)));
        drop(requests);server.abort();std::fs::remove_dir_all(root).unwrap();
    }

    /// Test fixtures inspect the content the model actually received. This
    /// reader understands plain method messages, without requiring the old
    /// host-generated state JSON in production requests.
    pub(crate) fn test_model_context(body:&Value)->Value {
        let messages=body["messages"].as_array().unwrap();
        let first=&messages[1];
        if first["name"]=="organizer" {
            let command=crate::session_history::parse_plain_context(first["content"].as_str().unwrap());
            let mut work=command.clone();
            if work["done_when"].is_null() {work["done_when"]=work["return_when"].clone();}
            if work["final_answer"].is_null() {work["final_answer"]=work["execution_scope"]["final_answer"].clone();}
            if work["context"].is_null() {work["context"]=json!({});}
            for message in messages.iter().skip(2).filter(|message|message["name"]=="organizer") {
                let resumed=crate::session_history::parse_plain_context(message["content"].as_str().unwrap());
                if let Some(context)=resumed.get("context") {work["context"]=context.clone();}
            }
            return json!({"current_work":work,"upstream_outputs":messages.iter().filter(|message|message["name"].as_str().is_some_and(|name|name.starts_with("worker_return_")))
                .map(|message| {let content=crate::session_history::original_text(message["content"].as_str().unwrap());
                    let mut value=serde_json::from_str::<Value>(content).unwrap_or_else(|_|json!({"summary":content}));
                    value["id"]=json!(message["name"].as_str().unwrap().trim_start_matches("worker_return_"));value}).collect::<Vec<_>>()});
        }
        let initial=first["content"].as_str().unwrap_or("");
        if let Some(packet)=initial.strip_prefix("Current work packet:\n") {return crate::session_history::parse_plain_context(packet);}
        let mut value=crate::session_history::parse_plain_context(initial);
        if matches!(first["name"].as_str(),Some("user"|"runtime")) {
            value=messages.iter().find(|message|message["name"]=="runtime").map(|message|
                crate::session_history::parse_plain_context(message["content"].as_str().unwrap())).unwrap_or(json!({}));
            value["human_request"]=messages.iter().rev().find(|message|message["role"]=="user" && message["name"].is_null() && message["content"].is_string())
                .map(|message|json!(crate::session_history::original_text(message["content"].as_str().unwrap()))).unwrap_or_else(||json!(initial));
            value["process"]=json!({"request_id":value["request_id"],"current_work":null,"current_result":null,"resumable_tasks":value["resumable_tasks"]});
            value["observer_messages"]=json!([]);value["read_task_results"]=json!([]);
            let mut read_request=Value::Null;
            for message in messages.iter().skip(2) {
                let content=crate::session_history::original_text(message["content"].as_str().unwrap_or(""));
                let payload=serde_json::from_str::<Value>(content).unwrap_or_else(|_|json!(content));
                let name=message["name"].as_str().unwrap_or("");
                match name {
                    "organizer_call"=>{
                        if let Some(call)=payload["tool_calls"].as_array().and_then(|calls|calls.first()) {
                            read_request=crate::session_history::parse_plain_context(call["function"]["arguments"].as_str().unwrap_or("{}"));
                            if read_request["action"].is_null() {read_request["action"]=call["function"]["name"].clone();}
                        } else {read_request=payload;}
                    },
                    "organizer_assignment"=>{value["process"]["current_work"]=payload;value["process"]["current_result"]=Value::Null;},
                    name if name.starts_with("worker_return_")=>value["process"]["current_result"]=if payload.is_string(){json!({"summary":payload})}else{payload},
                    name if name.starts_with("observer_")=>value["observer_messages"].as_array_mut().unwrap().push(json!({"id":name.trim_start_matches("observer_"),"observer_return":payload})),
                    "organizer_result"=>value["read_task_results"].as_array_mut().unwrap().push(json!({"action":read_request["action"],"request":read_request,"result":payload})),
                    "runtime_error"=>{if !payload["failure"].is_null(){value["last_request_error"]=payload["failure"].clone();}
                        if !payload["feedback"].is_null(){value["last_decision_error"]=payload["feedback"].clone();}},
                    _=>{},
                }
            }
        } else if value["stage"]=="retrospective" {
            value["task_history"]["records"]=json!(messages.iter().skip(2).map(|message| {
                let name=message["name"].as_str().unwrap_or("message");
                let kind=name.replacen('_',"/",1);
                let content=crate::session_history::original_text(message["content"].as_str().unwrap_or(""));
                let payload=serde_json::from_str::<Value>(content).unwrap_or_else(|_|json!(content));
                json!({"kind":kind,"payload":payload})
            }).collect::<Vec<_>>());
        }
        value
    }
    use axum::{extract::State, routing::post};

    #[test]
    fn command_ban_matches_instructions_without_matching_examples() {
        assert!(user_bans_run_command("不要运行命令"));
        assert!(user_bans_run_command("禁止执行命令"));
        assert!(user_bans_run_command("Do Not Run Commands"));
        assert!(!user_bans_run_command("请解释“不要运行命令”这句话"));
        assert!(!user_bans_run_command("请解释‘不要运行命令’这句话"));
        assert!(!user_bans_run_command("`不要运行命令` 是一个示例"));
        assert!(!user_bans_run_command("```text\n不要运行命令\n```"));
        assert!(!user_bans_run_command("不要“示例”运行命令"));
        assert!(!user_bans_run_command("请说明如何运行命令"));
    }

    #[derive(Clone, Default)]
    struct CommandBanMock {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        offered_commands: Arc<Mutex<Vec<bool>>>,
    }

    async fn fake_command_ban_chat(
        State(mock): State<CommandBanMock>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let is_organizer = body.get("tools").and_then(Value::as_array).is_some_and(|tools| {
            tools.iter().any(|tool| tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))
        });
        if is_organizer {
            if let Some(finished)=mock_finish_returned_work(&body) {return finished;}
            let input_json = test_model_context(&body);
            let turn_num = input_json.get("turn").and_then(Value::as_u64).unwrap_or(1);
            let id = if turn_num <= 1 { "task".to_string() } else { format!("task_turn_{turn_num}") };
            let node_id = id.clone();
            return Json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_work",
                            "type": "function",
                            "function": {
                                "name": "organize_work",
                                "arguments": json!({
                                    "action": "work",
                                    "reason": "Process probe command",
                                    "orders": [{
                                        "id": id,
                                        "node_id": node_id,
                                        "goal": "Check the requested task",
                                        "done_when": "Run the requested command if available",
                                        "completion": "output",
                                        "final_answer": true
                                    }]
                                }).to_string()
                            }
                        }]
                    }
                }]
            }));
        }
        let offered = body.get("tools").and_then(Value::as_array).is_some_and(|tools| {
            tools.iter().any(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some("run_program"))
        });
        mock.offered_commands.lock().await.push(offered);
        let call = mock.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let message = if call == 0 {
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"call_blocked","type":"function","function":{
                    "name":"run_program","arguments":"{\"program\":\"python\",\"args\":[\"-c\",\"print('guard_probe')\"]}"
                }}
            ]})
        } else {
            json!({"role":"assistant","content":"done"})
        };
        Json(json!({"choices":[{"message":message}]}))
    }

    #[tokio::test]
    async fn command_ban_hides_tool_rejects_forced_call_and_resets_next_turn() {
        let root = std::env::temp_dir().join(format!("agent-command-ban-test-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = Arc::new(crate::tools::Workspace::new(root.clone()).unwrap());
        let mock = CommandBanMock::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/v1/chat/completions", post(fake_command_ban_chat))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = AgentServiceState {
            workspace,
            client: Client::new(),
            provider_url: format!("http://{address}/v1"),
            api_key: "fake".into(),
            provider_name: String::new(),
            provider_routes: HashMap::new(),
            default_model: "fake-model".into(),
            model_map: HashMap::new(),
            reasoning_effort: None,
            fast_mode: false,
            permission_mode: PermissionMode::default(),
            enable_subagent: false,
            expert_provider_name: String::new(),
            expert_provider_url: String::new(),
            expert_api_key: String::new(),
            expert_model: String::new(),
            subagent_inherits_model: true,
            observer_enabled: false,
            observer_provider: String::new(),
            observer_provider_url: String::new(),
            observer_api_key: String::new(),
            observer_model: String::new(),
            observer_inherits_model: true,
            visual:Default::default(),
            config: None,
            config_path: None,
            tool_catalog: None,
            plugin_cancel: CancellationToken::new(),
            cancellations: Arc::default(),
        };
        let task_id = format!("task_{}", uuid_like());
        {
            let conn = open_db(&root).unwrap();
            conn.execute(
                "INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'test','fake-model','running',?2,?2)",
                params![task_id, now()],
            ).unwrap();
        }

        run_task(state.clone(), task_id.clone(), "fake-model".into(), "不要运行命令".into(),
            4, CancellationToken::new(), false, 1, Vec::new(), false, false).await.unwrap();
        let conn = open_db(&root).unwrap();
        let raw: String = conn.query_row(
            "SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='tool/result' ORDER BY seq LIMIT 1",
            [&task_id], |row| row.get(0),
        ).unwrap();
        let result: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(result["message"]["isError"], true);
        assert!(result["message"]["content"][0]["text"].as_str().unwrap().contains("blocked"));
        drop(conn);

        run_task(state.clone(), task_id.clone(), "fake-model".into(), "继续回答".into(),
            1, CancellationToken::new(), false, 2, Vec::new(), false, false).await.unwrap();
        let child_id = format!("task_{}", uuid_like());
        {
            let conn = open_db(&root).unwrap();
            conn.execute(
                "INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at,parent_task_id) VALUES (?1,'child','fake-model','running',?2,?2,?3)",
                params![child_id, now(), task_id],
            ).unwrap();
        }
        run_task(state, child_id, "fake-model".into(), "读取工作区".into(),
            1, CancellationToken::new(), true, 1, Vec::new(), false, true).await.unwrap();
        assert_eq!(*mock.offered_commands.lock().await, vec![false, false, true, false]);
        server.abort();
        let _ = std::fs::remove_dir_all(root);
    }

    fn offered_tools(names: &[&str]) -> Vec<Value> {
        names
            .iter()
            .map(|name| json!({"type":"function","function":{"name":name,"description":"","parameters":{"type":"object"}}}))
            .collect()
    }

    #[test]
    fn worker_prompt_lists_only_offered_capabilities() {
        let root = std::path::Path::new("/ws");
        let full = [
            "list_dir",
            "read_file",
            "read_file_lines",
            "write_file",
            "replace_range",
        "edit_file",
            "search_text",
            "list_go_symbols",
            "search_rust_symbols",
            "read_ts_symbol",
            "index_python_workspace",
            "rust_index_status",
            "list_work_memory",
            "search_work_memory",
            "record_work_memory",
            "read_session_history",
            "run_program",
            "spawn_subagent",
        ];
        let prompt = worker_system_prompt(root, false, false, &offered_tools(&full));
        assert!(prompt.contains("You are a local coding agent"));
        assert!(prompt.contains("run_program"));
        assert!(prompt.contains("PowerShell"));
        assert!(prompt.contains("@relative/path"));
        assert!(prompt.contains("indexed symbol tools"));
        assert!(prompt.contains("search_text"));
        assert!(!prompt.contains("scope=project searches conversations"));
        assert!(prompt.contains("Observer reviews only after execution ends"));
        assert!(!prompt.contains("record a work memory"));
        assert!(prompt.contains("You may use spawn_subagent once"));

        // A turn-level command ban removes run_program from the offered
        // tools, so the prompt must not suggest it either.
        let banned: Vec<&str> = full.iter().copied().filter(|name| *name != "run_program").collect();
        let prompt = worker_system_prompt(root, false, false, &offered_tools(&banned));
        assert!(!prompt.contains("run_program"));
        assert!(prompt.contains("indexed symbol tools"));

        // Delegation guidance only appears when spawn_subagent is offered.
        let no_subagent: Vec<&str> =
            full.iter().copied().filter(|name| *name != "spawn_subagent").collect();
        let prompt = worker_system_prompt(root, false, false, &offered_tools(&no_subagent));
        assert!(!prompt.contains("spawn_subagent"));
        assert!(prompt.contains("sole worker"));
        let prompt = worker_system_prompt(root, true, false, &offered_tools(&no_subagent));
        assert!(prompt.contains("cannot delegate further"));
        assert!(!prompt.contains("may use spawn_subagent once"));
        let prompt = worker_system_prompt(root, false, true, &offered_tools(&no_subagent));
        assert!(prompt.contains("already used its one allowed subagent"));
        assert!(!prompt.contains("may use spawn_subagent once"));
    }

    #[test]
    fn worker_prompt_tracks_tool_set_changes() {
        let root = std::path::Path::new("/ws");
        let first = worker_system_prompt(root, false, false, &offered_tools(&["list_dir", "read_file", "run_program"]));
        assert!(first.contains("run_program"));
        // Next step the command ban hides run_program: guidance must follow.
        let second = worker_system_prompt(root, false, false, &offered_tools(&["list_dir", "read_file"]));
        assert!(!second.contains("run_program"));
        // A file-only tool set must not claim search, index, or memory tools.
        assert!(second.contains("list_dir"));
        assert!(!second.contains("search_text"));
        assert!(!second.contains("symbol tools"));
        assert!(!second.contains("work memory"));
        // Delegation guidance disappears once the one allowed child is used.
        let delegating = worker_system_prompt(root, false, false, &offered_tools(&["read_file", "spawn_subagent"]));
        assert!(delegating.contains("may use spawn_subagent once"));
        let after_spawn = worker_system_prompt(root, false, true, &offered_tools(&["read_file"]));
        assert!(!after_spawn.contains("may use spawn_subagent once"));
        assert!(after_spawn.contains("already used its one allowed subagent"));
        // A tool set with no known capabilities omits the guidance section.
        let empty = worker_system_prompt(root, false, false, &offered_tools(&[]));
        assert!(!empty.contains("Tool guidance for the tools actually offered"));
    }

    #[test]
    fn local_tools_and_prompt_follow_plugin_catalog() {
        let root = std::env::temp_dir().join(format!("agent-catalog-prompt-test-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = crate::tools::Workspace::new(root.clone()).unwrap();
        let catalog = crate::plugin_builtin::ToolCatalog::default();
        let handler: crate::plugin_builtin::ToolHandler =
            Arc::new(|_workspace, _name, _args| Box::pin(async { Ok(json!({"ok":true})) }));
        catalog.register(&["list_dir", "read_file", "run_program"], handler).unwrap();
        let tools = local_tools(&workspace, Some(&catalog), false);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"list_dir"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"run_program"));
        assert!(!names.contains(&"search_text"));
        assert!(!names.contains(&"record_work_memory"));
        let prompt = worker_system_prompt(std::path::Path::new("/ws"), false, false, &tools);
        assert!(prompt.contains("list_dir, read_file"));
        assert!(prompt.contains("run_program"));
        assert!(!prompt.contains("search_text"));
        assert!(!prompt.contains("work memory"));
        assert!(!prompt.contains("symbol tools"));
        // The command ban applies on top of the catalog-filtered set.
        let tools = local_tools(&workspace, Some(&catalog), true);
        let prompt = worker_system_prompt(std::path::Path::new("/ws"), false, false, &tools);
        assert!(!prompt.contains("run_program"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[derive(Clone, Default)]
    struct PromptCaptureMock {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        system_prompts: Arc<Mutex<Vec<String>>>,
        guidance_leaked: Arc<std::sync::atomic::AtomicBool>,
    }

    async fn fake_prompt_capture_chat(
        State(mock): State<PromptCaptureMock>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let is_organizer = body.get("tools").and_then(Value::as_array).is_some_and(|tools| {
            tools.iter().any(|tool| tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))
        });
        if is_organizer {
            if let Some(finished)=mock_finish_returned_work(&body) {return finished;}
            let input_json = test_model_context(&body);
            let turn_num = input_json.get("turn").and_then(Value::as_u64).unwrap_or(1);
            let id = if turn_num <= 1 { "inspect".to_string() } else { format!("inspect_turn_{turn_num}") };
            let node_id = id.clone();
            return Json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_work",
                            "type": "function",
                            "function": {
                                "name": "organize_work",
                                "arguments": json!({
                                    "action": "work",
                                    "reason": "Capture prompt",
                                    "orders": [{
                                        "id": id,
                                        "node_id": node_id,
                                        "goal": "Read the requested note",
                                        "done_when": "Open note.txt",
                                        "completion": "output",
                                        "final_answer": true
                                    }]
                                }).to_string()
                            }
                        }]
                    }
                }]
            }));
        }
        let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(content) = messages
            .first()
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
        {
            mock.system_prompts.lock().await.push(content.to_owned());
        }
        let leaked = messages
            .iter()
            .skip(1)
            .filter_map(|message| message.get("content").and_then(Value::as_str))
            .any(|content| content.contains("Tool guidance for the tools actually offered"));
        if leaked {
            mock.guidance_leaked.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let call = mock.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let message = if call == 0 {
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"call_read","type":"function","function":{
                    "name":"read_file","arguments":"{\"path\":\"note.txt\"}"
                }}
            ]})
        } else {
            json!({"role":"assistant","content":"done"})
        };
        Json(json!({"choices":[{"message":message}]}))
    }

    #[tokio::test]
    async fn worker_prompt_rebuilds_per_step_and_stays_out_of_history() {
        let root = std::env::temp_dir().join(format!("agent-prompt-test-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.txt"), "note").unwrap();
        let workspace = Arc::new(crate::tools::Workspace::new(root.clone()).unwrap());
        let mock = PromptCaptureMock::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/v1/chat/completions", post(fake_prompt_capture_chat))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = AgentServiceState {
            workspace,
            client: Client::new(),
            provider_url: format!("http://{address}/v1"),
            api_key: "fake".into(),
            provider_name: String::new(),
            provider_routes: HashMap::new(),
            default_model: "fake-model".into(),
            model_map: HashMap::new(),
            reasoning_effort: None,
            fast_mode: false,
            permission_mode: PermissionMode::default(),
            enable_subagent: false,
            expert_provider_name: String::new(),
            expert_provider_url: String::new(),
            expert_api_key: String::new(),
            expert_model: String::new(),
            subagent_inherits_model: true,
            observer_enabled: false,
            observer_provider: String::new(),
            observer_provider_url: String::new(),
            observer_api_key: String::new(),
            observer_model: String::new(),
            observer_inherits_model: true,
            visual:Default::default(),
            config: None,
            config_path: None,
            tool_catalog: None,
            plugin_cancel: CancellationToken::new(),
            cancellations: Arc::default(),
        };
        let task_id = format!("task_{}", uuid_like());
        {
            let conn = open_db(&root).unwrap();
            conn.execute(
                "INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'test','fake-model','running',?2,?2)",
                params![task_id, now()],
            )
            .unwrap();
        }

        run_task(state.clone(), task_id.clone(), "fake-model".into(), "不要运行命令".into(),
            4, CancellationToken::new(), false, 1, Vec::new(), false, false).await.unwrap();
        run_task(state, task_id.clone(), "fake-model".into(), "继续回答".into(),
            1, CancellationToken::new(), false, 2, Vec::new(), false, false).await.unwrap();

        let prompts = mock.system_prompts.lock().await;
        assert_eq!(prompts.len(), 3);
        // Turn 1 bans commands: both steps' prompts must omit run_program.
        assert!(!prompts[0].contains("run_program"));
        assert!(!prompts[1].contains("run_program"));
        // Turn 2 lifts the ban: the rebuilt prompt mentions run_program again.
        assert!(prompts[2].contains("run_program"));
        for prompt in prompts.iter() {
            assert!(prompt.contains("You are a local coding agent"));
            assert!(prompt.contains("Tool guidance for the tools actually offered"));
        }
        drop(prompts);
        // Tool guidance stays in the single leading system message; it is
        // never appended to the chat history as a user message.
        assert!(!mock.guidance_leaked.load(std::sync::atomic::Ordering::Relaxed));
        server.abort();
        let _ = std::fs::remove_dir_all(root);
    }

    #[derive(Clone, Default)]
    struct ObserverMock {
        observer_calls: Arc<std::sync::atomic::AtomicUsize>,
        worker_calls: Arc<std::sync::atomic::AtomicUsize>,
        saw_both_histories: Arc<std::sync::atomic::AtomicBool>,
    }

    async fn fake_observer_chat(
        State(mock): State<ObserverMock>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let is_organizer = body.get("tools").and_then(Value::as_array).is_some_and(|tools| {
            tools.iter().any(|tool| tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))
        });
        if is_organizer {
            if let Some(finished)=mock_finish_returned_work(&body) {return finished;}
            let input_json = test_model_context(&body);
            let has_work = input_json.pointer("/process/current_work").is_some_and(|w| !w.is_null());
            let arguments = if has_work {
                json!({
                    "action": "continue",
                    "reason": "Wait for worker to report the unavailable capability"
                })
            } else {
                json!({
                    "action": "work",
                    "reason": "Assign initial inquiry task",
                    "orders": [{
                        "id": "analysis",
                        "node_id": "analysis",
                        "goal": "Handle observer task",
                        "done_when": "Task completed",
                        "completion": "output",
                        "final_answer": true
                    }]
                })
            };
            return Json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_organize",
                            "type": "function",
                            "function": {
                                "name": "organize_work",
                                "arguments": arguments.to_string()
                            }
                        }]
                    }
                }]
            }));
        }
        let answer = if body.get("stream").and_then(Value::as_bool) == Some(false) {
            mock.observer_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let context = test_model_context(&body);
            let history=body["messages"].as_array().unwrap().iter().filter(|message|message["role"]=="tool")
                .filter_map(|message|message["content"].as_str()).collect::<Vec<_>>().join("\n");
            if context["stage"] == "retrospective" && history.is_empty() {
                assert!(context.pointer("/observer_context/relevant_past_conversations").is_none(),"history is not automatically replayed");
                return Json(json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{
                    "id":"history","type":"function","function":{"name":"read_session_history",
                        "arguments":json!({"scope":"project","query":"服务器","limit":12}).to_string()}}]}}]}));
            }
            let saw_old=history.contains("旧翻墙服务器");
            let saw_new=history.contains("新翻墙服务器");
            if saw_old && saw_new {
                mock.saw_both_histories.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            json!({"answer":"历史任务显示旧服务器已经废弃，新服务器仍在使用。"})
        } else {
            let call = mock.worker_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if call == 0 {
                json!({"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_consult","type":"function","function":{
                        "name":"consult_observer","arguments":"{\"question\":\"之前的任务里，旧服务器和新服务器分别是什么状态？\"}"
                    }}
                ]})
            } else {
                json!({"role":"assistant","content":"历史显示旧服务器已废弃，因此只分析仍在使用的新服务器。"})
            }
        };
        let message = if body.get("stream").and_then(Value::as_bool) == Some(false) {
            json!({"role":"assistant","content":answer.to_string()})
        } else {
            answer
        };
        Json(json!({"choices":[{"message":message}]}))
    }

    #[tokio::test]
    async fn observer_only_reviews_after_execution_and_can_read_history() {
        let root = std::env::temp_dir().join(format!("observer-history-test-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let old_id = format!("task_{}", uuid_like());
        let new_id = format!("task_{}", uuid_like());
        let current_id = format!("task_{}", uuid_like());
        {
            let conn = open_db(&root).unwrap();
            for (id, prompt, status) in [
                (&old_id, "旧翻墙服务器", "completed"),
                (&new_id, "新翻墙服务器", "completed"),
                (&current_id, "请分析我的翻墙服务器", "running"),
            ] {
                conn.execute(
                    "INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,?2,'fake-model',?3,?4,?4)",
                    params![id, prompt, status, now()],
                ).unwrap();
            }
        }
        append_event(&root, &old_id, "user/message", json!({"message":{"content":[{"type":"text","text":"旧翻墙服务器还在用吗？"}]}}), None).unwrap();
        append_event(&root, &old_id, "assistant/message", json!({"message":{"content":[{"type":"text","text":"旧翻墙服务器已经废弃，后续只用新服务器。"}]}}), None).unwrap();
        append_event(&root, &new_id, "user/message", json!({"message":{"content":[{"type":"text","text":"新翻墙服务器现在正在使用。"}]}}), None).unwrap();

        let mock = ObserverMock::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/v1/chat/completions", post(fake_observer_chat))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let workspace = Arc::new(crate::tools::Workspace::new(root.clone()).unwrap());
        let state = AgentServiceState {
            workspace,
            client: Client::new(),
            provider_url: format!("http://{address}/v1"),
            api_key: "fake".into(),
            provider_name: String::new(),
            provider_routes: HashMap::new(),
            default_model: "fake-model".into(),
            model_map: HashMap::new(),
            reasoning_effort: None,
            fast_mode: false,
            permission_mode: PermissionMode::default(),
            enable_subagent: false,
            expert_provider_name: String::new(),
            expert_provider_url: String::new(),
            expert_api_key: String::new(),
            expert_model: String::new(),
            subagent_inherits_model: true,
            observer_enabled: true,
            observer_provider: "fake".into(),
            observer_provider_url: format!("http://{address}/v1"),
            observer_api_key: "fake".into(),
            observer_model: String::new(),
            observer_inherits_model: true,
            visual:Default::default(),
            config: None,
            config_path: None,
            tool_catalog: None,
            plugin_cancel: CancellationToken::new(),
            cancellations: Arc::default(),
        };
        run_task(
            state, current_id.clone(), "fake-model".into(),
            "请分析我的翻墙服务器".into(), 4, CancellationToken::new(),
            false, 1, Vec::new(), false, false,
        ).await.unwrap();
        // Worker completion no longer waits for the independent Observer.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let completed: i64 = open_db(&root).unwrap().query_row(
                    "SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='observer/retrospective'",
                    [&current_id], |row| row.get(0),
                ).unwrap();
                if completed > 0 { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let conn = open_db(&root).unwrap();
        let status: String = conn.query_row(
            "SELECT status FROM agent_tasks WHERE id=?1", [&current_id], |row| row.get(0),
        ).unwrap();
        let consult_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='observer/consult'",
            [&current_id], |row| row.get(0),
        ).unwrap();
        let reply_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='observer/consult_reply'",
            [&current_id], |row| row.get(0),
        ).unwrap();
        let plan_review_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='observer/node_review' AND json_extract(data,'$.stage')='assignment' AND json_extract(data,'$.status')='completed'",
            [&current_id], |row| row.get(0),
        ).unwrap();
        let retrospective_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='observer/retrospective'",
            [&current_id], |row| row.get(0),
        ).unwrap();
        assert_eq!(status, "completed");
        assert_eq!(consult_count, 0);
        assert_eq!(reply_count, 0);
        assert_eq!(plan_review_count,0);
        assert_eq!(retrospective_count, 1);
        assert!(mock.saw_both_histories.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(mock.worker_calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        // The retrospective performs one explicit history lookup and then answers.
        assert_eq!(mock.observer_calls.load(std::sync::atomic::Ordering::Relaxed),2);
        assert!(conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='observer/history_read'",[&current_id],|row|row.get::<_,i64>(0)).unwrap()>0);
        server.abort();
        drop(conn);
        let _ = std::fs::remove_dir_all(root);
    }
    async fn fake_chat(State(calls): State<Arc<std::sync::atomic::AtomicUsize>>, Json(body): Json<Value>) -> Json<Value> {
        let is_organizer = body.get("tools").and_then(Value::as_array).is_some_and(|tools| {
            tools.iter().any(|tool| tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))
        });
        if is_organizer {
            if let Some(finished)=mock_finish_returned_work(&body) {return finished;}
            return Json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_work",
                            "type": "function",
                            "function": {
                                "name": "organize_work",
                                "arguments": json!({
                                    "action": "work",
                                    "reason": "Inspect workspace",
                                    "orders": [{
                                        "id": "inspect",
                                        "node_id": "inspect",
                                        "goal": "Inspect workspace and index state",
                                        "done_when": "Read both status tools",
                                        "completion": "output",
                                        "final_answer": true
                                    }]
                                }).to_string()
                            }
                        }]
                    }
                }]
            }));
        }
        let call = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if call == 0 {
            Json(
                json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_info","type":"function","function":{"name":"workspace_info","arguments":"{}"}},
                    {"id":"call_index","type":"function","function":{"name":"rust_index_status","arguments":"{}"}}
                ]}}]}),
            )
        } else {
            Json(
                json!({"choices":[{"message":{"role":"assistant","content":"Finished after checking both tools."}}]}),
            )
        }
    }

    #[tokio::test]
    async fn fake_model_runs_sequential_tools_and_records_dsh_events() {
        let root = std::env::temp_dir().join(format!("agent-service-test-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.txt"), "SNAPSHOT_BEFORE_EDIT").unwrap();
        let workspace = Arc::new(crate::tools::Workspace::new(root.clone()).unwrap());
        let model_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/v1/chat/completions", post(fake_chat))
            .with_state(model_calls.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = Client::new();
        let state = AgentServiceState {
            workspace,
            client,
            provider_url: format!("http://{address}/v1"),
            api_key: "fake".into(),
            provider_name: String::new(),
            provider_routes: HashMap::new(),
            default_model: "fake-model".into(),
            model_map: HashMap::new(),
            reasoning_effort: None,
            fast_mode: false,
            permission_mode: PermissionMode::default(),
            enable_subagent: false,
            expert_provider_name: String::new(),
            expert_provider_url: String::new(),
            expert_api_key: String::new(),
            expert_model: String::new(),
            subagent_inherits_model: true,
            observer_enabled: false,
            observer_provider: String::new(),
            observer_provider_url: String::new(),
            observer_api_key: String::new(),
            observer_model: String::new(),
            observer_inherits_model: true,
            visual:Default::default(),
            config: None,
            config_path: None,
            tool_catalog: None,
            plugin_cancel: CancellationToken::new(),
            cancellations: Arc::default(),
        };
        let task_id = format!("task_{}", uuid_like());
        {
            let conn = open_db(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'test','fake-model','running',?2,?2)", params![task_id,now()]).unwrap();
        }

        run_task(
            state,
            task_id.clone(),
            "fake-model".into(),
            "inspect @note.txt".into(),
            4,
            CancellationToken::new(),
            false,
            1,
            Vec::new(),
            false,
            false,
        )
        .await
        .unwrap();

        let conn = open_db(&root).unwrap();
        let types: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT kind FROM agent_task_events WHERE task_id=?1 ORDER BY seq")
                .unwrap();
            stmt.query_map([&task_id], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(model_calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(types.first().map(String::as_str), Some("turn/start"));
        assert_eq!(types.get(1).map(String::as_str), Some("observer/config"));
        assert_eq!(types.get(2).map(String::as_str), Some("user/message"));
        let first_tool = types.iter().position(|t| t == "tool/call").unwrap();
        assert_eq!(
            &types[first_tool..first_tool + 4],
            ["tool/call", "tool/result", "tool/call", "tool/result"]
        );
        assert_eq!(types.last().map(String::as_str), Some("turn/end"));

        let ticks: Vec<Value> = {
            let mut stmt = conn.prepare("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='execution/tick' ORDER BY seq").unwrap();
            stmt.query_map([&task_id], |row| row.get::<_,String>(0)).unwrap()
                .map(|row| serde_json::from_str::<Value>(&row.unwrap()).unwrap()).collect()
        };
        assert_eq!(ticks.len(),2);
        assert_eq!(ticks[0]["tick"]["operations_count"],2);
        assert_eq!(ticks[0]["tick"]["work_id"],"inspect");
        assert_eq!(ticks[1]["tick"]["done"],true);
        assert_eq!(ticks[1]["tick"]["output"]["summary"],"Finished after checking both tools.");

        let user_event: Value = conn.query_row(
            "SELECT json_object('type',kind,'seq',seq,'time',timestamp,'data',json(data),'surfaceOp',surface_op) FROM agent_task_events WHERE task_id=?1 AND kind='user/message'",
            [&task_id], |r| r.get(0),
        ).map(|raw: String| serde_json::from_str(&raw).unwrap()).unwrap();
        assert_eq!(user_event["type"], "user/message");
        assert_eq!(user_event["seq"], 2);
        assert_eq!(user_event["data"]["message"]["role"], "user");
        assert!(user_event["data"]["model_content"].as_str().unwrap().contains("SNAPSHOT_BEFORE_EDIT"));
        assert_eq!(user_event["surfaceOp"], "append");

        std::fs::write(root.join("note.txt"), "CONTENT_AFTER_EDIT").unwrap();
        let history_events: Vec<(String, Value)> = {
            let mut stmt = conn.prepare("SELECT kind,data FROM agent_task_events WHERE task_id=?1 ORDER BY seq").unwrap();
            stmt.query_map([&task_id], |row| {
                let kind: String = row.get(0)?;
                let data: String = row.get(1)?;
                Ok((kind, serde_json::from_str::<Value>(&data).unwrap()))
            }).unwrap().collect::<Result<_, _>>().unwrap()
        };
        let (history, _, _) = history_from_events(&root, &history_events);
        assert!(history[0]["content"].as_str().unwrap().contains("SNAPSHOT_BEFORE_EDIT"));
        assert!(!history[0]["content"].as_str().unwrap().contains("CONTENT_AFTER_EDIT"));

        let (status, surface_op): (String, Option<String>) = conn.query_row("SELECT status,(SELECT surface_op FROM agent_task_events WHERE task_id=?1 AND kind='assistant/message' ORDER BY seq DESC LIMIT 1) FROM agent_tasks WHERE id=?1", [&task_id], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(status, "completed");
        assert_eq!(surface_op.as_deref(), Some("append"));
        server.abort();
        let _ = std::fs::remove_dir_all(root);
    }


    fn is_worker_request(body: &Value) -> bool {
        body["stream"] == true && !body["tools"].as_array().into_iter().flatten()
            .any(|tool| tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))
    }

    #[derive(Default)]
    struct FlowScript {
        replies: std::sync::Mutex<std::collections::VecDeque<(bool, Value)>>,
        requests: std::sync::Mutex<Vec<Value>>,
        observer_gate: Option<(std::path::PathBuf, String)>,
    }

    async fn scripted_flow_chat(State(script): State<Arc<FlowScript>>, Json(body): Json<Value>) -> axum::response::Response {
        let organizer = body["tools"].as_array().into_iter().flatten().any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"));
        script.requests.lock().unwrap().push(body.clone());
        if !organizer && script.observer_gate.is_some() && body["stream"] == false {
            let input: Value = test_model_context(&body);
            if input["stage"]=="retrospective" && input["request"]["goal"]=="Observer final route memory" {
                if input["task_history"]["coverage"]=="partial" && !body["messages"].as_array().unwrap().iter().any(|message|message["role"]=="tool") {
                    return Json(json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{
                        "id":"route_history","type":"function","function":{"name":"read_session_history",
                            "arguments":json!({"role":"tool","query":"workspace_info","limit":12}).to_string()}}]}}]})).into_response();
                }
                return Json(json!({"choices":[{"message":{"role":"assistant","content":json!({
                    "summary":"Reuse the first workspace observation", "path_review":"The second identical workspace query added no evidence.",
                    "shortening_opportunities":["Reuse the first workspace observation instead of repeating it."],
                    "route_shortcuts":[{"situation":"Unchanged workspace discovery","look_first":"The existing workspace_info result", "avoid":"Repeat workspace_info without a new question"}],
                    "memory":{"record":true,"summary":"Reuse unchanged workspace discovery","applies_to":"Workspace discovery"}}).to_string()}}]})).into_response();
            }
            if input["request"]["goal"].as_str().is_some_and(|goal|goal.contains("Observer unavailable")) {
                return Json(json!({"choices":[{"message":{"role":"assistant","content":"invalid-observer-json"}}]})).into_response();
            }
            if input["request"]["goal"]=="Byte exact role forwarding" {
                let raw=" \n{ \"assessment\": \"needs_adjustment\", \"summary\": \"原始建议 [字段]\\n尾部\", \"suggestions\": [\"保留原文\"] }\n  ";
                return Json(json!({"choices":[{"message":{"role":"assistant","content":raw}}]})).into_response();
            }
            let answer = if input.get("stage").is_some() {
                let old = input["request"]["goal"].as_str().is_some_and(|goal|goal.contains("CANCELLED_DRAG_GOAL"));
                json!({"assessment":"needs_adjustment","issue_key":"direction","category":"sufficiency",
                    "summary":if old {"CANCELLED_DRAG_ADVICE".to_owned()}
                        else if input["request"]["goal"]=="Observer immediate long communication" {"CURRENT_REQUEST_ADVICE".repeat(1000)}
                        else {"CURRENT_REQUEST_ADVICE".to_owned()},
                    "z_failure":{"reason":"Original limitation at the end of a long message"},
                    "suggestions":[if old {"CANCELLED_DRAG_ADVICE: inspect the drag source ID"} else {"CURRENT_REQUEST_ADVICE: use the sealed inputs and shorten repeated investigation"}]})
            } else { json!({"summary":"No reusable experience","memory":{"record":false}}) };
            return Json(json!({"choices":[{"message":{"role":"assistant","content":answer.to_string()}}]})).into_response();
        }
        let (expected_organizer, mut message) = script.replies.lock().unwrap().pop_front().expect("unexpected model round");
        assert_eq!(organizer, expected_organizer, "model round reached the wrong actor: {}", body["messages"][1]["content"]);
        if organizer&&message["request_error"]==true {
            return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({"error":"transient organizer response failure"}))).into_response();
        }
        if organizer&&message["request_timeout"]==true {
            tokio::time::sleep(ORGANIZER_REQUEST_TIMEOUT+Duration::from_millis(200)).await;
            return Json(json!({"choices":[{"message":{"role":"assistant","content":"late organizer response"}}]})).into_response();
        }
        if organizer {
            let mut decision:Value=serde_json::from_str(message["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
            if decision["auto_apply_observer"]==true {
                let input:Value=test_model_context(&body);
                let mut responses=Vec::new();
                for advice in input["observer_messages"].as_array().into_iter().flatten() {
                    let application=if decision["action"]=="revisit" {json!({"field":"revisit","value":decision["target_node_id"]})}
                        else {
                            let order=&mut decision["orders"][0];
                            order["constraints"]=json!(["Use sealed inputs; do not repeat investigation"]);
                            json!({"work_id":order["id"],"field":"constraints","value":order["constraints"]})
                        };
                    if advice["id"].is_string() {responses.push(json!({"id":advice["id"],"disposition":"accepted","reason":"implemented in this actual decision",
                        "adopt_to_current_plan":decision["action"]=="revisit","application":application}));}
                }
                decision["observer_responses"]=json!(responses);decision.as_object_mut().unwrap().remove("auto_apply_observer");
                message["tool_calls"][0]["function"]["arguments"]=json!(decision.to_string());
            }
        }

        if let Some((root, task_id)) = &script.observer_gate {
            // Let reviews of already completed rounds finish during the next
            // model response. The first Worker does not wait for an assignment
            // review, and no production execution is gated by Observer.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let pending=open_db(root).unwrap().query_row(
                        "SELECT COUNT(*) FROM agent_observations WHERE task_id=?1 AND status IN ('pending','reviewing')",
                        [task_id],|row|row.get::<_,i64>(0)).unwrap();
                    if pending==0 {break;}
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("Observer did not complete an execution review");
        }
        Json(json!({"choices":[{"message":message}]})).into_response()
    }

    pub(crate) fn flow_test_state(root: &std::path::Path, address: std::net::SocketAddr) -> AgentServiceState {
        AgentServiceState {
            workspace: Arc::new(crate::tools::Workspace::new(root.to_path_buf()).unwrap()),
            client: Client::new(),
            provider_url: format!("http://{address}/v1"),
            api_key: "fake".into(),
            provider_name: String::new(),
            provider_routes: HashMap::new(),
            default_model: "fake-model".into(),
            model_map: HashMap::new(),
            reasoning_effort: None,
            fast_mode: false,
            permission_mode: PermissionMode::default(),
            enable_subagent: false,
            expert_provider_name: String::new(),
            expert_provider_url: String::new(),
            expert_api_key: String::new(),
            expert_model: String::new(),
            subagent_inherits_model: true,
            observer_enabled: false,
            observer_provider: String::new(),
            observer_provider_url: String::new(),
            observer_api_key: String::new(),
            observer_model: String::new(),
            observer_inherits_model: true,
            visual:Default::default(),
            config: None,
            config_path: None,
            tool_catalog: None,
            plugin_cancel: CancellationToken::new(),
            cancellations: Arc::default(),
        }

    }

    fn scheduling_reply(decision: Value) -> (bool, Value) {
        (true, json!({"role":"assistant","content":null,"tool_calls":[{"id":"organize","type":"function",
            "function":{"name":"organize_work","arguments":decision.to_string()}}]}))
    }

    fn text_reply(text: &str) -> (bool, Value) {
        (false, json!({"role":"assistant","content":text}))
    }

    #[test]
    fn structured_browser_failures_can_be_sealed_without_claiming_the_upload_succeeded() {
        use crate::work_scheduler::{WorkOrder,WorkScheduler};
        let upload_failure=json!({"ok":false,"status":"failed","error_code":"multiple_file_inputs","stage":"select_input","needed":"Choose one input."});
        let visibility_failure=json!({"ok":false,"error_code":"browser_visibility_locked","stage":"session_start"});
        let wait_timeout=json!({"ok":false,"status":"timeout","matched":false,"error_code":"wait_timeout"});
        let wait_invalid=json!({"ok":false,"status":"invalid_condition","error_code":"wait_condition_required"});
        for result in [&upload_failure,&visibility_failure,&wait_timeout,&wait_invalid] {
            assert!(observer_output_failed(&result.to_string()),"{result}");
        }
        assert!(!observer_output_failed(r#"{"ok":true,"status":"matched","matched":true}"#));

        let mut scheduler=WorkScheduler::default();let mut order=WorkOrder::default();
        order.id="browser_upload".into();order.node_id="browser_upload".into();order.goal="Upload presentation".into();order.done_when="Presentation loaded".into();
        order.constraints=vec!["requires_browser".into()];scheduler.enqueue(vec![order],false,false).unwrap();scheduler.activate_next().unwrap();
        scheduler.observe("browser_open",&json!({}),&json!({"page":{"browser_session_id":"s","page_id":"p","page_epoch":1}}),false);
        scheduler.observe("browser_read",&json!({"expect_text":"Welcome"}),&json!({"matched":true,"expect_text":"Welcome","page":{"browser_session_id":"s","page_id":"p","page_epoch":1}}),false);
        scheduler.observe("browser_upload",&json!({"path":"deck.pptx"}),&upload_failure,observer_output_failed(&upload_failure.to_string()));
        assert_eq!(scheduler.frame().unwrap().operations.last().unwrap()["failed"],true);
        let returned=scheduler.return_work(&json!({"outcome":"blocked","summary":"Upload failed because the selected input was ambiguous",
            "limitations":["The presentation was not uploaded"]})).unwrap();
        assert_eq!(scheduler.frame().unwrap().status,crate::work_scheduler::WorkStatus::Done);
        assert_eq!(returned["outcome"],"blocked");
        assert!(returned.get("expectation_met").is_none());
        assert_eq!(returned["operations"][2]["failed"],true);
    }

    #[test]
    fn missing_pptx_target_gets_a_precise_organizer_correction() {
        let scheduler=crate::work_scheduler::WorkScheduler::default();let tree=crate::flow_tree::TaskTree::default();
        let decision=json!({"orders":[{"constraints":["requires_pptx"]}]});
        let error=anyhow::anyhow!("field_path=orders[0].browser_document_path: requires_pptx needs the exact workspace-relative target file path");
        let feedback=organizer_contract_feedback(&error,&decision,&scheduler,&tree);
        assert_eq!(feedback["error_code"],"INVALID_PPTX_CONTRACT");
        assert_eq!(feedback["field_path"],"orders[0].browser_document_path");
        assert!(feedback["expected_shape"].as_str().unwrap().contains("exact workspace-relative .pptx path"));
    }

    #[test]
    fn pptx_work_accepts_explicit_checks_and_invalid_check_keys_point_to_the_submitted_field() {
        let order=crate::work_scheduler::WorkOrder{ id:"visual".into(),node_id:"visual".into(),goal:"Load PPTX and inspect picture".into(),
            done_when:"Presentation is loaded and checked".into(),completion:crate::work_scheduler::Completion::Output,
            checks:vec!["http-probe:http://127.0.0.1:3000/api/fonts".into()],constraints:vec!["requires_pptx".into()],
            browser_document_path:Some("samples/demo.pptx".into()),..Default::default()};
        let scheduler=crate::work_scheduler::WorkScheduler::default();let tree=crate::flow_tree::TaskTree::default();
        let mut compatible=crate::work_scheduler::WorkScheduler::default();
        compatible.enqueue(vec![order.clone()],false,true).unwrap();
        compatible.activate_next().unwrap();
        assert_eq!(compatible.order().unwrap().checks,order.checks,"visual and HTTP observation requirements may coexist");
        let invalid_check="npm run build";
        let mut invalid_order=order.clone();invalid_order.checks=vec![invalid_check.into()];
        let error=crate::work_scheduler::WorkScheduler::default().enqueue(vec![invalid_order],false,true).unwrap_err();
        let decision=json!({"orders":[{"id":"visual","node_id":"visual","completion":"output","checks":[invalid_check],
            "constraints":["requires_pptx"],"browser_document_path":"samples/demo.pptx"}]});
        let feedback=organizer_contract_feedback(&error,&decision,&scheduler,&tree);
        assert_eq!(feedback["field_path"],"orders[0].checks");
        assert_eq!(feedback["rejected_value"],json!([invalid_check]));
        assert_eq!(feedback["error_code"],"INVALID_CHECK_IDENTIFIER");
        assert!(feedback["expected_shape"].as_str().unwrap().contains("supported, authorized check identifier"));
        let repeated=organizer_contract_feedback(&error,&decision,&scheduler,&tree);
        assert_eq!(feedback["fingerprint"],repeated["fingerprint"]);
        let same_rule_different_message=anyhow::anyhow!("field_path=orders[0].checks: unsupported check identifier; supported identifier keys are required");
        let same_rule=organizer_contract_feedback(&same_rule_different_message,&decision,&scheduler,&tree);
        assert_eq!(feedback["fingerprint"],same_rule["fingerprint"]);
        let different_rule=anyhow::anyhow!("field_path=orders[0].checks: permission denied for checking under current permissions");
        let different=organizer_contract_feedback(&different_rule,&decision,&scheduler,&tree);
        assert_ne!(feedback["fingerprint"],different["fingerprint"]);
        let changed_value=json!({"orders":[{"id":"visual","node_id":"visual","completion":"output",
            "checks":["npm run test"],"constraints":["requires_pptx"],"browser_document_path":"samples/demo.pptx"}]});
        let changed=organizer_contract_feedback(&error,&changed_value,&scheduler,&tree);
        assert_ne!(feedback["fingerprint"],changed["fingerprint"]);

        let visual=crate::work_scheduler::WorkOrder{id:"visual_only".into(),node_id:"visual_only".into(),goal:"Inspect rendered picture".into(),
            done_when:"Picture checked".into(),completion:crate::work_scheduler::Completion::Check,visual_goal:Some("Inspect the rendered picture".into()),..Default::default()};
        let mut visual_scheduler=crate::work_scheduler::WorkScheduler::default();
        visual_scheduler.enqueue(vec![visual],false,true).unwrap();
        visual_scheduler.activate_next().unwrap();
        assert!(visual_scheduler.order().unwrap().visual_goal.is_some());

        let saved=json!({"failure_stage":"organizer_request","failure":{"stage":"organizer_request","attempt":2},
            "last_decision_error":{"rule_id":"INVALID_CHECK_COMPLETION_COMBINATION","field_path":"orders[0].checks","fingerprint":"prior"},
            "contract_error_fingerprints":["prior","older"]});
        let wrapped=json!({"intent":"session_continuation","previous_handoff":saved});
        assert_eq!(organizer_resume_decision_error(Some(&wrapped)).unwrap()["fingerprint"],"prior");
        assert_eq!(organizer_resume_request_error(Some(&wrapped)).unwrap()["stage"],"organizer_request");
        let mut fingerprints=organizer_resume_contract_fingerprints(Some(&wrapped));fingerprints.sort();fingerprints.dedup();
        assert_eq!(fingerprints,vec!["older".to_owned(),"prior".to_owned()]);
    }

    #[test]
    fn host_errors_preserve_the_original_handoff_and_full_error_chain_without_worker_outcomes() {
        let original=json!({"intent":"session_continuation","need_split":true,
            "upstream_problem":{"reason":"Worker reported an unavailable font endpoint"}});
        let mut handoff=Some(original.clone());
        let error=anyhow::anyhow!("original process error\n  at original.js:17").context("tool invocation failed");
        let raw=json!({"stage":"worker_execution","error":format!("{error:#}")});
        attach_handoff_error(&mut handoff,"execution_error",raw.clone());
        assert_eq!(handoff.as_ref().unwrap()["need_split"],true);
        assert_eq!(handoff.as_ref().unwrap()["upstream_problem"],original["upstream_problem"]);
        assert_eq!(handoff.as_ref().unwrap()["execution_error"],raw);
        let mut scheduler=crate::work_scheduler::WorkScheduler::default();
        scheduler.handoff=Some(json!({"intent":"session_continuation","previous_handoff":handoff}));
        assert_eq!(scheduler.organizer_input()["handoff"]["execution_error"],raw);
        let mut no_return=None;
        attach_organizer_failure(&mut no_return,json!({"failure_stage":"organizer_request","error":"timeout"}));
        for field in ["done","blocked","outcome"] {assert!(no_return.as_ref().unwrap().get(field).is_none());}
    }

    #[test]
    fn contract_failure_is_attached_without_losing_session_handoff_or_replace_action() {
        use crate::{flow_tree::TaskTree,work_scheduler::WorkScheduler};
        let root=std::env::current_dir().unwrap();
        let (mut scheduler,tree,_)=apply_organizer_decision(&WorkScheduler::default(),&TaskTree::default(),&TaskTree::default(),&root,
            "Original goal",json!({"action":"work","orders":[flow_order("task_a","direct")]}),false,false).unwrap();
        scheduler.handoff=Some(json!({"done":false,"intent":"session_continuation","reason":"new user input",
            "previous_handoff":{"need_split":true,"outcome":"upstream_problem","upstream_problem":{"reason":"font endpoint is not ready"},
                "suggested_children":[{"goal":"Check the font endpoint","done_when":"HTTP probe returns 2xx"}]}}));
        let decision_error=json!({"error_code":"INVALID_CHECK_COMPLETION_COMBINATION","field_path":"orders[0].checks",
            "rejected_value":["http-probe:http://127.0.0.1:3000/api/fonts"],"fingerprint":"rule-field-value"});
        attach_organizer_failure(&mut scheduler.handoff,json!({"failure_stage":"organizer_contract_validation",
            "last_decision_error":decision_error,"contract_error_fingerprints":["rule-field-value"]}));

        let handoff=scheduler.pending_handoff().unwrap();
        assert_eq!(handoff["intent"],"session_continuation");
        assert_eq!(handoff["previous_handoff"]["need_split"],true);
        assert_eq!(handoff["previous_handoff"]["upstream_problem"]["reason"],"font endpoint is not ready");
        assert_eq!(handoff["previous_handoff"]["suggested_children"][0]["goal"],"Check the font endpoint");
        assert_eq!(organizer_resume_decision_error(Some(handoff)).unwrap()["field_path"],"orders[0].checks");
        let mut fingerprints=organizer_resume_contract_fingerprints(Some(handoff));fingerprints.sort();fingerprints.dedup();
        assert_eq!(fingerprints,vec!["rule-field-value".to_owned()]);

        let (replacement,_,_)=apply_organizer_decision(&scheduler,&tree,&tree,&root,"Replacement goal",
            json!({"action":"work","request_action":"replace","reason":"the user replaced the goal",
                "orders":[flow_order("replacement","direct")]}),false,false).unwrap();
        assert!(replacement.frames.get("task_a").is_none());
        assert!(replacement.frames.get("replacement").is_some());
    }

    fn yield_reply(outcome: &str, summary: &str, exported: Value) -> (bool, Value) {
        (false, json!({"role":"assistant","content":null,"tool_calls":[{"id":"yield","type":"function",
            "function":{"name":"yield_work","arguments":json!({"outcome":outcome,"summary":summary,"exported_data":exported}).to_string()}}]}))
    }

    fn flow_order(id: &str, node: &str) -> Value {
        json!({"id":id,"node_id":node,"goal":format!("Do {id}"),"done_when":format!("{id} delivered"),"completion":"output"})
    }

    fn organizer_request_error_reply() -> (bool,Value) { (true,json!({"request_error":true})) }
    fn organizer_timeout_reply() -> (bool,Value) { (true,json!({"request_timeout":true})) }

    fn finish_flow_reply() -> (bool, Value) {
        scheduling_reply(json!({"action":"finish","reason":"all requested work is delivered","summary":"Request complete",
            "flow_update":{"current_node_id":"goal","node_result":{"node_id":"goal","status":"completed","summary":"All requested parts complete"}}}))
    }

    #[tokio::test]
    async fn agent_flow_check_task_keeps_tools_and_allows_mixed_operations() {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let server=tokio::spawn(async move {
            axum::serve(listener,axum::Router::new().fallback(||async {"healthy"})).await.unwrap();
        });
        let url=format!("http://{address}/health");
        let tool=|id:&str,name:&str,args:Value|json!({"id":id,"type":"function",
            "function":{"name":name,"arguments":args.to_string()}});
        let batch=|calls:Vec<Value>|(false,json!({"role":"assistant","content":null,"tool_calls":calls}));
        let mut order=flow_order("mixed","mixed");order["completion"]=json!("check");
        order["checks"]=json!([format!("http-probe:{url}")]);
        let replies=vec![scheduling_reply(json!({"action":"work","orders":[order]})),
            batch(vec![tool("write1","write_file",json!({"path":"note.txt","content":"FIRST_WRITE"})),
                tool("http1","http_probe",json!({"url":url}))]),
            batch(vec![tool("write2","write_file",json!({"path":"note.txt","content":"SECOND_WRITE",
                    "expected_code_hash":crate::symbol_description::content_hash(b"FIRST_WRITE")})),
                tool("read","read_file",json!({"path":"note.txt"})),
                tool("extra_http","http_probe",json!({"url":format!("http://{address}/other")}))]),
            yield_reply("completed","Mixed operations executed",json!({})),finish_flow_reply()];
        let (events,requests)=run_flow_script(replies,&[("Check service and inspect the workspace",8)]).await;
        for body in requests.iter().filter(|body|is_worker_request(body)) {
            let names=body["tools"].as_array().unwrap().iter().filter_map(|tool|tool.pointer("/function/name").and_then(Value::as_str)).collect::<Vec<_>>();
            for name in ["browser_read","browser_diagnostics","write_file","read_file","http_probe"] {
                assert!(names.contains(&name),"check tasks must not hide {name}");
            }
        }
        let results=events.iter().filter(|(kind,_)|kind=="tool/result").map(|(_,data)|data).collect::<Vec<_>>();
        for id in ["write1","write2","http1","read","extra_http"] {
            let result=results.iter().find(|data|data["message"]["toolCallId"]==id).unwrap();
            assert_eq!(result["message"]["isError"],false,"{id}: {result}");
        }
        assert!(results.iter().find(|data|data["message"]["toolCallId"]=="read").unwrap()["meta"]["result"].to_string().contains("SECOND_WRITE"));
        server.abort();
    }

    #[tokio::test]
    async fn agent_flow_user_permissions_control_tools_and_forced_calls() {
        for permission in [PermissionMode::ReadOnly,PermissionMode::WorkspaceWrite,PermissionMode::FullAccess] {
            let call=|id:&str,name:&str,args:Value|json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}});
            let replies=vec![scheduling_reply(json!({"action":"work","orders":[flow_order("inspect","inspect")]})),
                (false,json!({"role":"assistant","content":null,"tool_calls":[
                    call("write","write_file",json!({"path":"note.txt","content":"USER_PERMISSION_WRITE"})),
                    call("inspect","workspace_info",json!({})),
                    // Invalid arguments avoid launching a process even under full access.
                    call("native","run_program",json!({}))]})),
                yield_reply("completed","Permissions checked",json!({})),finish_flow_reply()];
            let (events,requests)=run_flow_script_with_options(replies,&[("Inspect tool permissions",6)],false,false,permission).await;
            let body=requests.iter().find(|body|is_worker_request(body)).unwrap();
            let names=body["tools"].as_array().unwrap().iter().filter_map(|tool|tool.pointer("/function/name").and_then(Value::as_str)).collect::<Vec<_>>();
            assert_eq!(names.contains(&"write_file"),!matches!(permission,PermissionMode::ReadOnly));
            for name in ["browser_read","browser_diagnostics","run_program","run_project_script"] {
                assert_eq!(names.contains(&name),matches!(permission,PermissionMode::FullAccess),"{permission:?}: {name}");
            }
            let result=|id:&str|events.iter().find(|(kind,data)|kind=="tool/result"&&data["message"]["toolCallId"]==id).unwrap().1.clone();
            assert_eq!(result("inspect")["message"]["isError"],false);
            assert_eq!(result("write")["message"]["isError"],matches!(permission,PermissionMode::ReadOnly));
            if matches!(permission,PermissionMode::ReadOnly) {assert!(result("write")["meta"]["result"]["error"].as_str().unwrap().contains("permission_denied"));}
            if !matches!(permission,PermissionMode::FullAccess) {assert!(result("native")["meta"]["result"]["error"].as_str().unwrap().contains("permission_denied"));}
        }
    }

    fn finish_simple_reply(summary:&str) -> (bool,Value) {
        scheduling_reply(json!({"action":"finish","reason":"the returned answer addresses the request","summary":summary,"achieved":true}))
    }

    #[tokio::test]
    async fn agent_flow_final_message_displays_unresolved_for_completed_and_blocked_requests() {
        // A summary may use the full tool allowance before limitations are appended.
        let summary="已完成加载。".repeat(1000);
        let limitations=json!(["当前模型未收到图像，无法确认幻灯片视觉呈现。","另一个事项尚未确认。"]);
        for achieved in [true,false] {
            let (events,requests)=run_flow_script(vec![
                scheduling_reply(json!({"action":"work","reason":"load the presentation","orders":[flow_order("load","direct")]})),
                yield_reply("completed","Presentation loaded",json!({})),
                (true,json!({"role":"assistant","content":null,"tool_calls":[{"id":"finish","type":"function",
                    "function":{"name":"finish_request","arguments":json!({"summary":summary,
                        "achieved":achieved,"unresolved":limitations}).to_string()}}]})),
            ],&[("Load presentation",4)]).await;
            let answer=events.iter().rev().find(|(kind,_)|kind=="assistant/message").unwrap().1
                .pointer("/message/content/0/text").unwrap().as_str().unwrap();
            assert!(answer.starts_with(&summary));
            for limitation in limitations.as_array().unwrap() {assert!(answer.contains(limitation.as_str().unwrap()));}
            let decision=events.iter().rev().find(|(kind,_)|kind=="organizer/decision").unwrap().1["decision"].clone();
            assert_eq!(decision["unresolved"],limitations);
            assert_eq!(decision["summary"],answer);
            let commit=committed_turn(&events,1);
            assert_eq!(commit["scheduler"]["final_result"],answer);
            assert_eq!(commit["scheduler"]["request_completed"],achieved);
            let ended=&events.iter().find(|(kind,_)|kind=="turn/end").unwrap().1;
            assert_eq!(ended["reason"]["kind"],"completed");
            assert_eq!(ended["reason"]["goal_achieved"],achieved);
            assert_eq!(ended["reason"]["unresolved"],limitations);
            assert_eq!(requests.iter().filter(|body|is_worker_request(body)).count(),1);
        }
    }

    fn mock_finish_returned_work(body:&Value) -> Option<Json<Value>> {
        let input:Value=test_model_context(body);
        let returned=input.pointer("/process/current_result")?.as_object()?;
        let summary=returned.get("summary")?.as_str()?;
        let (_,message)=finish_simple_reply(summary);
        Some(Json(json!({"choices":[{"message":message}]})))
    }

    async fn run_flow_script(replies: Vec<(bool, Value)>, turns: &[(&str, usize)]) -> (Vec<(String, Value)>, Vec<Value>) {
        run_flow_script_with_observer(replies,turns,false).await
    }

    async fn remove_flow_test_directory(root: &std::path::Path) -> std::io::Result<()> {
        for attempt in 0..10 {
            match std::fs::remove_dir_all(root) {
                Ok(()) => return Ok(()),
                Err(error) if cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)) && attempt < 9 => {
                    tokio::time::sleep(Duration::from_millis(25 * (attempt + 1))).await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn agent_flow_cleanup_waits_for_windows_share_lock_and_preserves_other_errors() {
        use std::os::windows::fs::OpenOptionsExt;
        let root=std::env::temp_dir().join(format!("agent-flow-review-cleanup-{}",uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let lock=std::fs::OpenOptions::new().write(true).create_new(true).share_mode(0).open(root.join("locked.db")).unwrap();
        let release=tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(lock);
        });
        remove_flow_test_directory(&root).await.unwrap();
        release.await.unwrap();
        assert!(!root.exists());
        assert_eq!(remove_flow_test_directory(&root).await.unwrap_err().kind(),std::io::ErrorKind::NotFound);
    }

    #[derive(Default)]
    struct LateObserverScript {
        old_started: tokio::sync::Notify,
        release_old: tokio::sync::Notify,
        calls:std::sync::atomic::AtomicUsize,
    }

    async fn late_observer_chat(State(script):State<Arc<LateObserverScript>>,Json(body):Json<Value>) -> Json<Value> {
        script.calls.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
        let input:Value=test_model_context(&body);
        if input["identity"]["request_id"]==1 {
            script.old_started.notify_one();
            script.release_old.notified().await;
        }
        Json(json!({"choices":[{"message":{"role":"assistant","content":json!({
            "issue_key":"same_issue","category":"sufficiency","summary":format!("request {} advice",input["identity"]["request_id"]),
            "suggestions":["complete this request"]}).to_string()}}]}))
    }


    async fn run_flow_script_with_observer(replies: Vec<(bool, Value)>, turns: &[(&str, usize)], observer_enabled: bool) -> (Vec<(String, Value)>, Vec<Value>) {
        run_flow_script_with_options(replies,turns,observer_enabled,false,PermissionMode::FullAccess).await
    }

    async fn run_flow_script_with_organizer_errors(replies: Vec<(bool, Value)>, turns: &[(&str, usize)]) -> (Vec<(String, Value)>, Vec<Value>) {
        run_flow_script_with_options(replies,turns,false,true,PermissionMode::FullAccess).await
    }

    async fn run_flow_script_with_options(replies: Vec<(bool, Value)>, turns: &[(&str, usize)], observer_enabled: bool, allow_organizer_errors:bool, permission_mode:PermissionMode) -> (Vec<(String, Value)>, Vec<Value>) {
        let root = std::env::temp_dir().join(format!("agent-flow-review-{}", uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let task_id = format!("task_{}", uuid_like());
        let script = Arc::new(FlowScript { replies: std::sync::Mutex::new(replies.into()),
            observer_gate:observer_enabled.then(||(root.clone(),task_id.clone())), ..Default::default() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = flow_test_state(&root, listener.local_addr().unwrap());
        state.permission_mode = permission_mode;
        state.observer_enabled = observer_enabled;
        state.observer_provider_url = state.provider_url.clone();
        let app = axum::Router::new().route("/v1/chat/completions", post(scripted_flow_chat)).with_state(script.clone());
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).with_graceful_shutdown(server_shutdown.cancelled_owned()).await.unwrap();
        });
        {
            let conn = open_db(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'test','fake-model','running',?2,?2)", params![task_id,now()]).unwrap();
        }
        {
            for (turn, (prompt, steps)) in turns.iter().enumerate() {
                run_task(state.clone(),task_id.clone(),"fake-model".into(),prompt.to_string(),*steps,
                    CancellationToken::new(),false,turn+1,Vec::new(),false,false).await.unwrap();
            }
        }
        let events = {
            let conn = open_db(&root).unwrap();
            let mut stmt = conn.prepare("SELECT kind,data FROM agent_task_events WHERE task_id=?1 ORDER BY seq").unwrap();
            stmt.query_map([&task_id], |row| {
                let data: String = row.get(1)?;
                Ok((row.get::<_,String>(0)?,serde_json::from_str::<Value>(&data).unwrap()))
            }).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
        };
        let errors = events.iter().filter(|(kind,_)| kind == "organizer/error" || kind == "worker/error").collect::<Vec<_>>();
        assert!(allow_organizer_errors||errors.is_empty(), "Agent rejected decisions: {errors:?}");
        assert!(script.replies.lock().unwrap().is_empty(), "Agent stopped before consuming the scenario: {:?}",
            events.iter().filter(|(kind,_)|matches!(kind.as_str(),"turn/end"|"tool/result"|"worker/error"|"organizer/error"))
                .map(|(kind,value)|json!({"kind":kind,"reason":value["reason"],"error":value.pointer("/meta/result/error")})).collect::<Vec<_>>());
        let requests = std::mem::take(&mut *script.requests.lock().unwrap());
        if observer_enabled {
            if let Ok(directory)=std::env::var("OBSERVER_REVIEW_RECORD_DIR") {
                let directory=std::path::PathBuf::from(directory);std::fs::create_dir_all(&directory).unwrap();
                let name=turns[0].0.chars().map(|c|if c.is_ascii_alphanumeric(){c.to_ascii_lowercase()}else{'_'}).take(70).collect::<String>();
                let worker_calls=requests.iter().filter(|b|is_worker_request(b)).count();
                let organizer_calls=requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))).count();
                let observer_calls=requests.len()-worker_calls-organizer_calls;
                let tools=events.iter().filter(|(k,_)|k=="tool/call").count();
                let evaluated=events.iter().filter(|(k,v)|k=="observer/node_review" && v["status"]=="completed").count();
                let elapsed:u64=events.iter().filter(|(k,v)|k=="observer/node_review" && v["status"]=="completed").map(|(_,v)|v["result"]["elapsed_ms"].as_u64().unwrap_or(0)).sum();
                let data=json!({"scenario":turns[0].0,"model":"scripted-local-http","worker_model_calls":worker_calls,"worker_tool_calls":tools,
                    "organizer_model_calls":organizer_calls,"observer_model_calls":observer_calls,"completed_reviews":evaluated,"observer_elapsed_ms":elapsed,
                    "extra_worker_receipt_rounds":0,"events":events,"model_requests":requests});
                std::fs::write(directory.join(format!("{name}.json")),serde_json::to_vec_pretty(&data).unwrap()).unwrap();
            }
        }
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5),server).await.expect("mock model server did not exit").unwrap();
        // Compare durable role inputs with bodies actually received by HTTP,
        // including Observer follow-up lookups. Check after terminal cleanup.
        {
            let mut conn=open_db(&root).unwrap();
            cleanup_terminal_task_data(&mut conn,Some(&task_id),false).unwrap();
            let mut saved=conn.prepare("SELECT actor,request_json FROM agent_request_contexts WHERE task_id=?1 ORDER BY id").unwrap()
                .query_map([&task_id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))
                .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
            for request in &requests {
                let actor=if request["stream"]==false {"observer"}
                    else if request["tools"].as_array().into_iter().flatten().any(|tool|tool["function"]["name"]=="schedule_task") {"organizer"}
                    else {"worker"};
                let index=saved.iter().position(|(role,body)|role==actor && serde_json::from_str::<Value>(body).unwrap()==*request)
                    .expect("an actual HTTP model request is missing its exact role context log");
                saved.remove(index);
            }
        }
        drop(state);
        remove_flow_test_directory(&root).await.unwrap();
        (events, requests)
    }

    fn committed_turn(events: &[(String, Value)], turn: usize) -> &Value {
        &events.iter().rev().find(|(kind,data)| kind == "execution/commit" && data["turn"] == turn).unwrap().1
    }

    fn cancelled_drag_reply() -> (bool, Value) {
        let (actor,mut reply)=yield_reply("blocked","CANCELLED_DRAG_GOAL is unfinished",json!({}));
        let args=&mut reply["tool_calls"][0]["function"]["arguments"];
        let mut fields:Value=serde_json::from_str(args.as_str().unwrap()).unwrap();
        fields["findings"]=json!([{"id":"drag_fact","text":"CANCELLED_DRAG_FACT","status":"confirmed"}]);
        *args=json!(fields.to_string());
        (actor,reply)
    }

    fn worker_packets(requests: &[Value]) -> Vec<Value> {
        requests.iter().filter(|body|body["messages"][1]["name"]=="organizer").map(test_model_context).collect()
    }

    fn assert_final_only_observer(events:&[(String,Value)],requests:&[Value],rounds:usize) {
        assert_eq!(requests.iter().filter(|body|body["stream"]==false).count(),rounds);
        assert_eq!(events.iter().filter(|(kind,_)|kind=="observer/retrospective").count(),rounds);
        assert!(!events.iter().any(|(kind,_)|matches!(kind.as_str(),"observer/node_review"|"observer/advice_delivered"|"observer/advice_consumed"|"observer/advice_response"|"observer/inbox_state")));
        for (index,(_,review)) in events.iter().enumerate().filter(|(_, (kind,_))|kind=="observer/retrospective_start") {
            assert!(events[..index].iter().any(|(kind,event)|kind=="turn/end" && event["turn"]==review["turn"]),"review starts after its execution ends");
        }
    }

    #[tokio::test]
    async fn agent_flow_forwards_original_command_and_observer_body_without_rewriting() {
        let command=" \n{ \"goal\": \"Read the workspace\", \"return_when\": \"Return its actual result\", \"reason\": \"必要信息\", \"context\": {\"text\": \"[字段]\\n保留原文\", \"nested\": [true, null, 2]}, \"constraints\": [] }\n  ";
        let returned=" \n{ \"summary\": \"Original result\", \"outcome\": \"completed\", \"exported_data\": {\"text\":\"[字段]\\n原文\"} }\n ";
        let schedule=(true,json!({"role":"assistant","content":null,"tool_calls":[{"id":"raw_schedule","type":"function",
            "function":{"name":"schedule_task","arguments":command}}]}));
        let (events,requests)=run_flow_script_with_observer(vec![schedule,
            (false,json!({"role":"assistant","content":null,"tool_calls":[{"id":"read","type":"function","function":{"name":"workspace_info","arguments":"{}"}}]})),
            (false,json!({"role":"assistant","content":null,"tool_calls":[{"id":"raw_yield","type":"function","function":{"name":"yield_work","arguments":returned}}]})),finish_simple_reply("Original result"),
        ],&[("Byte exact role forwarding",5)],true).await;
        let workers=requests.iter().filter(|request|request["messages"][1]["name"]=="organizer").collect::<Vec<_>>();
        assert_eq!(workers.len(),2);
        for worker in workers {
            assert_eq!(crate::session_history::original_text(worker["messages"][1]["content"].as_str().unwrap()),command,"the actual HTTP command must retain exact bytes");
            assert!(!worker["messages"].to_string().contains("原始建议"));
            assert!(!worker["messages"].to_string().contains("Current work packet"));
        }
        let organizer=requests.iter().filter(|request|request["tools"].as_array().into_iter().flatten()
            .any(|tool|tool["function"]["name"]=="schedule_task")).last().unwrap();
        assert!(organizer["messages"].as_array().unwrap().iter().any(|message|message["name"].as_str().is_some_and(|name|name.starts_with("worker_return_")) && crate::session_history::original_text(message["content"].as_str().unwrap())==returned));
        assert!(organizer["messages"].as_array().unwrap().iter().all(|message|
            !message["name"].as_str().is_some_and(|name|name.starts_with("observer_"))));
        assert_final_only_observer(&events,&requests,1);

    }

    #[tokio::test]
    async fn agent_flow_observer_failure_leaves_worker_and_organizer_independent() {
        let (events,requests)=run_flow_script_with_observer(vec![
            scheduling_reply(json!({"action":"work","reason":"independent work","orders":[flow_order("work","direct")]})),
            (false,json!({"role":"assistant","content":null,"tool_calls":[{"id":"read","type":"function","function":{"name":"workspace_info","arguments":"{}"}}]})),
            (false,json!({"role":"assistant","content":null,"tool_calls":[{"id":"list_after_read","type":"function","function":{"name":"list_dir","arguments":"{}"}}]})),
            text_reply("Work delivered despite Observer failure"),finish_flow_reply(),
        ],&[("Observer unavailable",5)],true).await;
        assert_eq!(requests.iter().filter(|b|is_worker_request(b)).count(),3);
        assert_eq!(committed_turn(&events,1)["scheduler"]["request_completed"],true);
        assert!(!events.iter().any(|(kind,_)|kind=="observer/node_review"));
        assert!(events.iter().any(|(kind,v)|kind=="observer/retrospective" && v["status"]=="unavailable"));
        assert!(requests.iter().filter(|b|is_worker_request(b)).all(|b|!b["messages"].to_string().contains("invalid-observer-json")));
        let organizer=requests.iter().filter(|b|b["tools"].as_array().into_iter().flatten().any(|tool|tool["function"]["name"]=="schedule_task")).last().unwrap();
        assert!(!organizer["messages"].to_string().contains("invalid-observer-json"));
        assert_final_only_observer(&events,&requests,1);

    }


    async fn record_observer_source_read(state:&AgentServiceState,task:&str,file:&str) {
        let call_id=format!("read_{}",uuid_like());
        let result=state.workspace.read_file(crate::tools::ReadFileRequest {workspace_root:None,path:file.into(),max_bytes:1024}).unwrap();
        emit(state.workspace.root(),task,"tool/call",json!({"turn":1,"step":1,"callId":call_id,"name":"read_file","arguments":json!({"path":file}).to_string()})).await.unwrap();
        emit(state.workspace.root(),task,"tool/result",json!({"turn":1,"step":1,"message":{"toolCallId":call_id,"source":{"callId":call_id},"content":"source read","isError":false},"meta":{"result":result}})).await.unwrap();
    }

    #[tokio::test]
    async fn observer_latest_fact_after_source_edit_uses_one_version_and_remains_searchable() {
        let root=std::env::temp_dir().join(format!("agent-observer-fact-versions-{}",uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let task="task";let state=flow_test_state(&root,"127.0.0.1:9".parse().unwrap());
        {let conn=open_db(&root).unwrap();conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','mapping','fake','running',0,0)",[]).unwrap();}
        let report=|finding:&str|json!({"work_findings":[{"finding":finding,"source_refs":["source.rs"],"certainty":"confirmed"}],
            "memory":{"record":true,"summary":"Mapping fact","applies_to":"mapping"}});
        std::fs::write(root.join("source.rs"),"old source").unwrap();record_observer_source_read(&state,task,"source.rs").await;
        let old_trace=load_observer_work_trace(&root,task).unwrap();
        let old_report=report("old mapping is present");
        assert_eq!(save_observer_lessons(&state,task,"mapping",&old_report,&old_trace).await.unwrap()["memoryRecorded"],true);
        std::fs::write(root.join("source.rs"),"new source").unwrap();record_observer_source_read(&state,task,"source.rs").await;
        let trace=load_observer_work_trace(&root,task).unwrap();let new_report=report("latest mapping is present");
        let saved=save_observer_lessons(&state,task,"mapping",&new_report,&trace).await.unwrap();
        assert_eq!(saved["memoryRecorded"],true);assert_eq!(saved["workFindings"][0]["sourceReads"].as_array().unwrap().len(),1);
        let read=&saved["workFindings"][0]["sourceReads"][0];
        assert_eq!(read["code_hash"],crate::symbol_description::content_hash(b"new source"));
        assert_eq!(read["source_event_seq"].as_i64(),trace.iter().filter(|e|e.get("codeTarget").is_some()).filter_map(|e|e["seq"].as_i64()).max());
        assert!(read["source_event_id"].as_str().unwrap().starts_with("task:event:"));
        let list=||state.workspace.list_work_memory(crate::memory::ListWorkMemoryRequest {workspace_root:root.to_string_lossy().into_owned(),limit:10}).unwrap().memories;
        let search=|query:&str|state.workspace.search_work_memory(crate::memory::SearchWorkMemoryRequest {workspace_root:root.to_string_lossy().into_owned(),query:query.into(),limit:10}).unwrap().matches;
        assert_eq!(list().len(),2);let current=search("mapping");assert_eq!(current.len(),1);assert!(current[0].implementation.contains("latest mapping"));
        let manifest:String=crate::database::init_db(&root).unwrap().query_row("SELECT sources FROM observer_memory_sources WHERE summary=?1 AND implementation=?2",params![current[0].summary,current[0].implementation],|row|row.get(0)).unwrap();
        let manifest:Value=serde_json::from_str(&manifest).unwrap();assert_eq!(manifest.as_array().unwrap().len(),1);assert_eq!(manifest[0],*read);
        save_observer_lessons(&state,task,"mapping",&new_report,&trace).await.unwrap();assert_eq!(list().len(),2,"same fact and version stay deduplicated");
        // Two facts may deliberately cite different versions; do not combine
        // their manifests into one memory that can never match a current file.
        let mut both=new_report.clone();let mut old_finding=old_report["work_findings"][0].clone();
        old_finding["source_reads"]=json!(observer_known_read_targets(&old_trace,task,8));
        both["work_findings"]=json!([old_finding,new_report["work_findings"][0]]);
        save_observer_lessons(&state,task,"mapping",&both,&trace).await.unwrap();assert_eq!(list().len(),2);assert_eq!(search("mapping").len(),1);
        // Multi-file facts retain one matching observed version for each file.
        std::fs::write(root.join("other.rs"),"other source").unwrap();record_observer_source_read(&state,task,"other.rs").await;
        let trace=load_observer_work_trace(&root,task).unwrap();let mut multiple=report("cross mapping is consistent");
        multiple["work_findings"][0]["source_refs"]=json!(["source.rs","other.rs"]);
        let saved=save_observer_lessons(&state,task,"mapping",&multiple,&trace).await.unwrap();assert_eq!(saved["workFindings"][0]["sourceReads"].as_array().unwrap().len(),2);assert_eq!(search("cross mapping").len(),1);
        std::fs::write(root.join("other.rs"),"changed other source").unwrap();assert!(search("cross mapping").is_empty());assert_eq!(list().len(),3);
        // Conflicting reads with no event order cannot prove which version a
        // fact refers to; keep it uncertain instead of choosing today's bytes.
        let mut unordered=trace.clone();for entry in &mut unordered {entry.as_object_mut().unwrap().remove("seq");}
        let unknown=save_observer_lessons(&state,task,"mapping",&report("unversioned mapping"),&unordered).await.unwrap();
        assert_eq!(unknown["workFindings"][0]["certainty"],"uncertain");assert_eq!(unknown["memoryRecorded"],false);
        let mut wrong=report("unsupported mapping");wrong["work_findings"][0]["source_reads"]=json!([{"file_path":"source.rs","code_hash":"unobserved"}]);
        let unknown=save_observer_lessons(&state,task,"mapping",&wrong,&trace).await.unwrap();assert_eq!(unknown["workFindings"][0]["certainty"],"uncertain");assert_eq!(unknown["memoryRecorded"],false);assert_eq!(list().len(),3);
        wrong["work_findings"][0]["source_reads"]=json!([read,{"file_path":"source.rs","code_hash":"unobserved"}]);
        let unknown=save_observer_lessons(&state,task,"mapping",&wrong,&trace).await.unwrap();assert_eq!(unknown["workFindings"][0]["certainty"],"uncertain");assert_eq!(unknown["memoryRecorded"],false);
        wrong["work_findings"][0]["source_reads"]=json!(observer_known_read_targets(&trace,task,24).into_iter().filter(|target|target["file_path"].as_str().unwrap().ends_with("source.rs")).collect::<Vec<_>>());
        let unknown=save_observer_lessons(&state,task,"mapping",&wrong,&trace).await.unwrap();assert_eq!(unknown["workFindings"][0]["certainty"],"uncertain");assert_eq!(unknown["memoryRecorded"],false);assert_eq!(list().len(),3);
        let mut incomplete=report("partially evidenced mapping");incomplete["work_findings"][0]["source_refs"]=json!(["source.rs","unread.rs"]);
        let unknown=save_observer_lessons(&state,task,"mapping",&incomplete,&trace).await.unwrap();assert_eq!(unknown["workFindings"][0]["certainty"],"uncertain");assert_eq!(unknown["memoryRecorded"],false);assert_eq!(list().len(),3);
        drop(state);remove_flow_test_directory(&root).await.unwrap();
    }


    #[tokio::test]
    async fn worker_cannot_call_shared_history_or_observer_even_when_forged() {
        let forged=(false,json!({"role":"assistant","content":null,"tool_calls":[
            {"id":"forged_history","type":"function","function":{"name":"read_session_history","arguments":"{}"}},
            {"id":"forged_consult","type":"function","function":{"name":"consult_observer","arguments":"{\"question\":\"past fact\"}"}},
            {"id":"forged_receipt","type":"function","function":{"name":"respond_observer","arguments":"{}"}}
        ]}));
        let (events,requests)=run_flow_script_with_observer(vec![
            scheduling_reply(json!({"action":"work","reason":"check role boundary","orders":[flow_order("worker","direct")]})),
            forged,yield_reply("blocked","Need a historical fact supplied by Organizer",json!({})),finish_flow_reply(),
        ],&[("Role boundary",5)],true).await;
        let workers=requests.iter().filter(|body|is_worker_request(body)).collect::<Vec<_>>();
        assert_eq!(workers.len(),2);
        assert!(workers.iter().all(|body|body["tools"].as_array().into_iter().flatten().all(|tool|
            worker_tool_allowed(tool["function"]["name"].as_str().unwrap()))));
        let errors=workers[1]["messages"].as_array().unwrap().iter().filter(|message|message["role"]=="tool")
            .map(Value::to_string).collect::<Vec<_>>();
        assert_eq!(errors.len(),3);assert!(errors.iter().all(|error|error.contains("tool_unavailable")));
        assert!(!events.iter().any(|(kind,_)|matches!(kind.as_str(),"observer/consult"|"observer/consult_reply"|"observer/advice_response")));
    }

    #[tokio::test]
    async fn agent_flow_observer_has_no_running_role_messages() {
        let read=(false,json!({"role":"assistant","content":null,"tool_calls":[{"id":"read","type":"function",
            "function":{"name":"workspace_info","arguments":"{}"}}]}));
        let (events,requests)=run_flow_script_with_observer(vec![
            scheduling_reply(json!({"action":"work","reason":"inspect once","orders":[flow_order("inspect","direct")]})),
            read,(false,json!({"role":"assistant","content":null,"tool_calls":[{"id":"list_after_read","type":"function","function":{"name":"list_dir","arguments":"{}"}}]})),yield_reply("completed","Inspection delivered",json!({})),finish_flow_reply(),
        ],&[("Observer immediate long communication",6)],true).await;
        let worker=requests.iter().filter(|body|is_worker_request(body)).collect::<Vec<_>>();
        assert_eq!(worker.len(),3);
        for body in &worker {
            assert!(!body.to_string().contains("CURRENT_REQUEST_ADVICE"));
            assert!(!body.to_string().contains("New Observer messages"));
            for tool in body["tools"].as_array().into_iter().flatten() {
                assert!(worker_tool_allowed(tool["function"]["name"].as_str().unwrap()));
            }
        }
        assert!(requests.iter().filter(|body|body["stream"]!=false).all(|body|!body["messages"].to_string().contains("CURRENT_REQUEST_ADVICE")));
        assert_eq!(events.iter().filter(|(kind,_)|kind=="organizer/start").count(),2);
        assert_final_only_observer(&events,&requests,1);

    }

    #[tokio::test]
    async fn agent_flow_observer_does_not_change_commands_or_add_rounds() {
        let mut b=flow_order("b","direct");b["upstream_ids"]=json!(["a"]);b["context"]=json!({"instruction":"ORGANIZER_SELECTED_COMMAND","source_event_seq":42});
        let mut c=flow_order("c","direct");c["upstream_ids"]=json!(["b"]);
        let replies=vec![
            scheduling_reply(json!({"action":"work","reason":"produce A","orders":[flow_order("a","direct")]})),
            yield_reply("completed","A sealed",json!({"answer":"use this"})),
            scheduling_reply(json!({"action":"work","auto_apply_observer":true,"reason":"use A in B","orders":[b]})),
            yield_reply("completed","B sealed",json!({"answer":"B"})),
            scheduling_reply(json!({"action":"work","auto_apply_observer":true,"reason":"use B in C","orders":[c]})),
            text_reply("C delivered"),finish_flow_reply(),
        ];
        let (_,baseline)=run_flow_script(replies.clone(),&[("Observer normal A B C",8)]).await;
        let (events,requests)=run_flow_script_with_observer(replies,&[("Observer normal A B C",8)],true).await;
        assert_eq!(baseline.iter().filter(|b|is_worker_request(b)).count(),requests.iter().filter(|b|is_worker_request(b)).count());

        let workers=requests.iter().filter(|body|is_worker_request(body)).collect::<Vec<_>>();
        assert_eq!(test_model_context(workers[1])["current_work"]["context"]["instruction"],"ORGANIZER_SELECTED_COMMAND");
        assert!(workers.iter().all(|body|!body.to_string().contains("CURRENT_REQUEST_ADVICE")));
        assert_eq!(requests.iter().filter(|b|is_worker_request(b)).count(),3);
        assert!(requests.iter().filter(|b|is_worker_request(b)).all(|b|!b["messages"][0]["content"].as_str().unwrap_or("").contains("observer_responses")));
        assert_eq!(requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))).count(),4);
        assert!(!events.iter().any(|(kind,_)|kind=="organizer/advice_error"));
        assert!(!events.iter().any(|(kind,v)|kind=="organizer/assignment" && v["order"]["constraints"].to_string().contains("Use sealed inputs")));
        assert!(!worker_packets(&requests)[2]["current_work"]["constraints"].to_string().contains("Use sealed inputs"));
        assert_final_only_observer(&events,&requests,1);

    }

    #[tokio::test]
    async fn agent_flow_observer_reviews_repeated_investigation_without_scheduling() {
        let (_,mut repeated)=yield_reply("need_split","Investigation stalled; known findings suffice for synthesis",json!({}));
        for index in 0..3 {repeated["tool_calls"].as_array_mut().unwrap().insert(index,json!({"id":format!("same_query_{index}"),"type":"function",
            "function":{"name":"workspace_info","arguments":"{}"}}));}
        let (events,requests)=run_flow_script_with_observer(vec![
            scheduling_reply(json!({"action":"work","reason":"bounded inquiry","orders":[flow_order("inquiry","direct")]})),(false,repeated),
            scheduling_reply(json!({"action":"work","reason":"use known facts without repeating queries","orders":[flow_order("synthesis","direct")]})),
            text_reply("Known findings synthesized"),finish_flow_reply(),
        ],&[("Observer repeated investigation",10)],true).await;
        assert_eq!(events.iter().filter(|(kind,v)|kind=="tool/call" && v["name"]=="workspace_info").count(),3);
        assert_eq!(requests.iter().filter(|b|is_worker_request(b)).count(),2);
        assert_final_only_observer(&events,&requests,1);
        let retrospective=requests.iter().find(|body|body["stream"]==false).unwrap();
        assert_eq!(test_model_context(retrospective)["task_history"]["records"].as_array().unwrap().iter()
            .filter(|record|record["kind"]=="tool/call" && record["payload"]["name"]=="workspace_info").count(),3);

    }

    #[tokio::test]
    async fn agent_flow_observer_simple_answer_is_assigned_and_assessed_by_organizer() {
        let mut answer=flow_order("answer","direct");answer["goal"]=json!("Explain closures directly");answer["final_answer"]=json!(true);
        let (events,requests)=run_flow_script_with_observer(vec![
            scheduling_reply(json!({"action":"work","reason":"Delegate the answer as one focused task","orders":[answer]})),
            text_reply("A closure captures its environment"),
            scheduling_reply(json!({"action":"finish","reason":"The Worker returned the requested explanation",
                "summary":"A closure captures its environment","achieved":true})),
        ],&[("Explain closures directly",5)],true).await;
        assert_eq!(requests.iter().filter(|b|is_worker_request(b)).count(),1);
        let organizer_requests=requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten()
            .any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))).collect::<Vec<_>>();
        assert_eq!(organizer_requests.len(),2,"Organizer assigns the Worker task and then assesses its return");
        let returned:Value=test_model_context(&organizer_requests[1]);
        assert_eq!(returned["process"]["current_result"]["summary"],"A closure captures its environment");
        assert!(returned["process"]["current_result"]["outcome"].is_null());
        assert!(returned["process"]["current_result"]["blocked"].is_null());
        assert!(requests.iter().filter(|b|b["stream"]==false).count()<=2);
        assert_final_only_observer(&events,&requests,1);
        let commit=committed_turn(&events,1);
        let frames=commit["scheduler"]["frames"].as_object().unwrap();
        assert_eq!(frames.len(),1,"Organizer creates exactly one answer task");
        assert_eq!(frames.values().next().unwrap()["order"]["final_answer"],true);
        assert_eq!(frames.values().next().unwrap()["status"],"done");
        assert_eq!(commit["scheduler"]["request_completed"],true,"request completion follows the Organizer's explicit finish decision");
    }

    #[tokio::test]
    async fn observer_final_review_reads_current_task_process_and_keeps_review_in_conversation() {
        let (_,mut work)=yield_reply("completed","Workspace discovery complete",json!({}));
        for id in ["first_lookup","repeated_lookup"] {work["tool_calls"].as_array_mut().unwrap().insert(0,
            json!({"id":id,"type":"function","function":{"name":"workspace_info","arguments":"{}"}}));}
        let (events,requests)=run_flow_script_with_observer(vec![
            scheduling_reply(json!({"action":"work","reason":"discover the workspace","orders":[flow_order("discover","direct")]})),
            (false,work),finish_simple_reply("Workspace discovery complete"),
        ],&[("Observer final route memory",4)],true).await;
        let observer_inputs:Vec<Value>=requests.iter().filter(|body|body["stream"]==false)
            .map(|body|test_model_context(&body)).collect();
        let finals:Vec<_>=observer_inputs.iter().filter(|input|input["stage"]=="retrospective").collect();
        assert_eq!(finals.len(),1,"the completed task process is already supplied to the final review");
        assert!(observer_inputs.iter().filter(|input|input["stage"]!="retrospective").all(|input|input.get("work_trace").is_none()
            && input.get("activity_summary").is_none() && input.get("node_operations").is_none()));
        assert!(finals[0].get("work_trace").is_none());
        assert!(finals[0].get("flow_overview").is_none(),"retrospective reads original messages instead of another host summary");
        assert_eq!(finals[0]["task_history"]["coverage"],"complete");
        assert_eq!(finals[0]["task_history"]["records"].as_array().unwrap().iter()
            .filter(|entry|entry["kind"]=="tool/call" && entry["payload"]["name"]=="workspace_info").count(),2);
        assert!(finals[0]["task_history"]["records"].as_array().unwrap().iter()
            .any(|entry|entry["kind"]=="debug/context_request" && entry["payload"]["actor"]=="worker"));
        assert!(!events.iter().any(|(kind,_)|kind=="observer/history_read"));
        let ended=events.iter().position(|(kind,_)|kind=="turn/end").unwrap();
        let reviewed=events.iter().position(|(kind,_)|kind=="observer/retrospective_start").unwrap();
        assert!(reviewed>ended,"the answer and task end precede the final review");
        let result=&events.iter().find(|(kind,_)|kind=="observer/retrospective").unwrap().1;
        assert_eq!(result["memoryRecorded"],false,"routine review must not write memory even if a model asks for it");
        assert_eq!(result["routeShortcuts"][0]["lookFirst"],"The existing workspace_info result");
        assert_eq!(result["observer_return"]["memory"]["record"],true);
        assert_eq!(requests.iter().filter(|body|is_worker_request(body)).count(),1);
    }

    #[test]
    fn retrospective_projection_keeps_old_necessary_original_for_role_selection() {
        let older=json!({"seq":2,"kind":"tool/result","payload":{"needed":"original dependency"}});
        let large=json!({"seq":8,"kind":"tool/result","payload":"旧".repeat(crate::context_window::MAX_TOKENS)});
        let latest=json!({"seq":12,"kind":"worker/yield","payload":"latest result"});
        let view=crate::session_history::process_view(&json!({"records":[older,large,latest]}));
        assert_eq!(view.sources,vec![Some(2),Some(8),Some(12)]);
        assert!(view.messages[0]["content"].as_str().unwrap().contains("original dependency"));
        assert!(crate::context_window::estimate(&json!(view.messages))>crate::context_window::MAX_TOKENS);
    }

    #[test]
    fn observer_trace_covers_all_actions_and_incremental_reads_start_after_the_cursor() {
        let root=std::env::temp_dir().join(format!("observer-full-trace-{}",uuid_like()));std::fs::create_dir_all(&root).unwrap();
        let task="task";
        {let conn=open_db(&root).unwrap();conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();}
        for index in 0..340 {append_event(&root,task,"tool/call",json!({"callId":format!("call_{index}"),"name":"workspace_info","arguments":"{}"}),None).unwrap();}
        let full=load_observer_work_trace(&root,task).unwrap();
        assert_eq!(full.len(),340,"final review retains middle actions as well as the ends");
        assert_eq!(full[0]["seq"],0);assert_eq!(full[339]["seq"],339);
        let recent=load_observer_trace_since(&root,task,330).unwrap();
        assert_eq!(recent.len(),9);assert_eq!(recent[0]["seq"],331);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_cleanup_preserves_tools_flow_and_messages_after_completion_and_restart() {
        let root=std::env::temp_dir().join(format!("agent-history-retention-{}",uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let mut conn=open_db(&root).unwrap();
        conn.execute_batch("CREATE TABLE agent_request_contexts(task_id TEXT,request_json TEXT);
            CREATE TABLE agent_context_debug(task_id TEXT,enabled INTEGER);").unwrap();
        let visible=[("flow/plan",json!({"nodes":[{"id":"probe"}]})),
            ("tool/call",json!({"callId":"probe_call","name":"http_probe","arguments":"{}"})),
            ("tool/result",json!({"toolCallId":"probe_call","message":{"isError":true,"content":[{"type":"text","text":"original failure"}]}})),
            ("assistant/message",json!({"message":{"content":[{"type":"tool-call","name":"http_probe"}]}})),
            ("assistant/message",json!({"message":{"content":[{"type":"text","text":"final answer with limitation"}]}})),
            ("worker/progress",json!({"purpose":"Check the service"})),
            ("observer/inbox_state",json!({"messages":["original advice"]})),
            ("debug/context_request",json!({"id":1,"actor":"worker","record_kind":"model_input"})),
            ("debug/context_end",json!({"id":1}))];
        for status in ["completed","failed","cancelled","max_steps","interrupted","running"] {
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'test','model',?1,0,0)",[status]).unwrap();
            for (kind,data) in &visible {append_event(&root,status,kind,data.clone(),None).unwrap();}
            append_event(&root,status,"scheduler/state",json!({"state":"temporary"}),None).unwrap();
            conn.execute("INSERT INTO agent_request_contexts VALUES (?1,'raw request')",[status]).unwrap();
            conn.execute("INSERT INTO agent_context_debug VALUES (?1,1)",[status]).unwrap();
        }
        cleanup_terminal_task_data(&mut conn,Some("completed"),false).unwrap();
        // Startup cleanup runs over all terminal tasks, including already cleaned tasks.
        cleanup_terminal_task_data(&mut conn,None,false).unwrap();
        for status in ["completed","failed","cancelled","max_steps","interrupted","running"] {
            let rows=conn.prepare("SELECT seq,kind,data FROM agent_task_events WHERE task_id=?1 ORDER BY seq").unwrap()
                .query_map([status],|row|Ok((row.get::<_,usize>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))
                .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
            for (index,(kind,data)) in visible.iter().enumerate() {
                assert_eq!(rows[index].0,index);assert_eq!(&rows[index].1,kind);
                assert_eq!(serde_json::from_str::<Value>(&rows[index].2).unwrap(),*data);
            }
            assert_eq!(rows.len(),visible.len()+usize::from(status=="running"));
            let contexts:i64=conn.query_row("SELECT COUNT(*) FROM agent_request_contexts WHERE task_id=?1",[status],|r|r.get(0)).unwrap();
            assert_eq!(contexts,1,"actual inputs survive completion and startup cleanup");
            let settings:i64=conn.query_row("SELECT COUNT(*) FROM agent_context_debug WHERE task_id=?1",[status],|r|r.get(0)).unwrap();
            assert_eq!(settings,1);
        }
        drop(conn);std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn agent_flow_ordinary_narration_with_work_continues_without_extra_report() {
        let narration="已收到交接；现在读取工作区信息，无须再提交一份进度报告。";
        let operation=(false,json!({"role":"assistant","content":narration,"tool_calls":[{"id":"inspect","type":"function","function":{"name":"workspace_info","arguments":"{}"}}]}));
        let (events,requests)=run_flow_script(vec![
            scheduling_reply(json!({"action":"work","reason":"one task","orders":[flow_order("task_a","work_a")]})),
            operation,
            yield_reply("done","task_a delivered",json!({"visual_check_result":null})),
            finish_flow_reply(),
        ],&[("Do task_a",6)]).await;
        let organizer=requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten()
            .any(|tool|tool.pointer("/function/name").and_then(Value::as_str)==Some("schedule_task"))).count();
        assert_eq!(organizer,2,"schedule once, then decide after the actual return");
        let workers=requests.iter().filter(|body|is_worker_request(body)).collect::<Vec<_>>();
        assert_eq!(workers.len(),2);
        assert!(requests.iter().all(|body|body["tools"].as_array().into_iter().flatten()
            .all(|tool|tool.pointer("/function/name").and_then(Value::as_str)!=Some("report_progress"))));
        assert!(workers[1]["messages"].to_string().contains(narration));
        assert!(requests.last().unwrap()["messages"].to_string().contains(narration));
        assert_eq!(events.iter().filter(|(kind,data)|kind=="assistant/message" && data["message"].to_string().contains(narration)).count(),1);
        assert!(!events.iter().any(|(kind,_)|kind=="worker/progress"));
        let frame=&committed_turn(&events,1)["scheduler"]["frames"]["task_a"];
        assert_eq!(frame["status"],"done");
        assert!(frame["superseded"].is_null());
    }

    #[tokio::test]
    async fn agent_flow_replacement_has_no_observer_inbox_and_reviews_each_ended_run() {
        let (events,requests) = run_flow_script_with_observer(vec![
            scheduling_reply(json!({"action":"work","reason":"implement drag","orders":[flow_order("task_a","direct"),flow_order("queued","queued")]})),
            (false,json!({"role":"assistant","content":null,"tool_calls":[{"id":"old_read","type":"function","function":{"name":"workspace_info","arguments":"{}"}}]})),
            cancelled_drag_reply(),text_reply("Old goal remains unfinished at the step limit"),
            scheduling_reply(json!({"action":"work","request_action":"replace","reason":"new human goal","orders":[flow_order("task_a","direct")]})),
            (false,json!({"role":"assistant","content":null,"tool_calls":[{"id":"new_read","type":"function","function":{"name":"workspace_info","arguments":"{}"}}]})),
            yield_reply("done","New request delivered",json!({})),
            finish_flow_reply(),
        ],&[("CANCELLED_DRAG_GOAL",3),("Cancel drag and explain the export format",4)],true).await;
        let inputs:Vec<Value> = requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task")))
            .map(|body|test_model_context(&body)).collect();
        let lifecycle_schemas=requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task")))
            .map(|body|body["tools"].as_array().into_iter().flatten().find(|tool|tool.pointer("/function/name").and_then(Value::as_str)==Some("schedule_task")).and_then(|tool|tool.pointer("/function/parameters/properties/request_action/enum")).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>();
        assert!(lifecycle_schemas.iter().any(|schema|schema==&json!(["continue","subtask","replace"])));
        assert!(!inputs.iter().any(|input|input["observer_messages"].to_string().contains("CURRENT_REQUEST_ADVICE")));
        assert!(!events.iter().any(|(kind,_)|kind=="observer/advice_delivered"));
        assert_final_only_observer(&events,&requests,2);
        let packets=worker_packets(&requests);
        assert_eq!(packets.len(),5);
        let new_worker=requests.iter().filter(|body|is_worker_request(body)).nth(2).unwrap();
        assert!(!new_worker.to_string().contains("CANCELLED_DRAG_ADVICE"));
        assert_eq!(committed_turn(&events,2)["scheduler"]["request_completed"],true);
    }

    #[tokio::test]
    async fn agent_flow_replaces_unfinished_complex_goal_with_simple_question_without_old_context() {
        let mut queued=flow_order("queued","direct");queued["upstream_ids"]=json!(["task_a"]);
        let mut answer=flow_order("task_a","direct");answer["goal"]=json!("Explain closures");answer["final_answer"]=json!(true);
        let mut another=flow_order("another","direct");another["final_answer"]=json!(true);
        let (events,requests)=run_flow_script(vec![
            scheduling_reply(json!({"action":"work","reason":"implement drag","orders":[flow_order("task_a","direct"),queued]})),
            cancelled_drag_reply(),
            scheduling_reply(json!({"action":"work","request_action":"replace","reason":"human cancelled the feature","orders":[answer]})),
            text_reply("Closures capture their environment"),
            finish_simple_reply("Closures capture their environment"),
            scheduling_reply(json!({"action":"work","reason":"answer the next question","orders":[another]})),
            text_reply("Another answer"),finish_simple_reply("Another answer"),
        ],&[("CANCELLED_DRAG_GOAL",1),("不用做拖拽了，先解释闭包",3),("Another new question",3)]).await;
        let first=committed_turn(&events,1);assert_eq!(first["scheduler"]["frames"]["task_a"]["status"],"done");
        assert!(first["scheduler"]["frames"]["task_a"].get("expectation_met").is_none());
        let second=committed_turn(&events,2);
        assert_eq!(second["scheduler"]["frames"].as_object().unwrap().len(),1);
        assert!(second["task_tree"]["nodes"].as_object().unwrap().is_empty());
        assert_eq!(second["scheduler"]["request_completed"],true);
        let archive=&second["scheduler"]["archived_requests"][0];
        assert_eq!(archive["task_tree"]["nodes"]["goal"]["status"],"deprecated");
        assert_eq!(archive["task_tree"]["nodes"]["goal"]["objective"],"CANCELLED_DRAG_GOAL");
        assert!(archive["scheduler"]["frames"]["task_a"]["invalidated_by_plan_revision"].is_number());
        assert!(archive["scheduler"]["frames"]["queued"]["invalidated_by_plan_revision"].is_number());
        assert!(archive["worker_state"].to_string().contains("CANCELLED_DRAG_FACT"));
        assert_eq!(archive["scheduler"]["archived_requests"],json!([]));
        let packets=worker_packets(&requests);
        assert!(packets[1].get("human_request").is_none());
        assert_eq!(packets[1]["current_work"]["goal"],"Explain closures");
        assert!(packets[1]["goal_boundary"].is_null());assert_eq!(packets[1]["upstream_outputs"],json!([]));
        let replacement_worker=&requests[3];
        assert!(!replacement_worker["messages"].to_string().contains("CANCELLED_DRAG_"));
        assert!(events.iter().any(|(kind,data)| kind=="flow/request_archived" && data["turn"]==2 && data["started_turn"]==1));
        let third=committed_turn(&events,3);
        let archives=third["scheduler"]["archived_requests"].as_array().unwrap();
        assert_eq!(archives.len(),2);
        assert_eq!(archives[0]["id"],"request_1");assert_eq!(archives[0]["started_turn"],1);
        assert_eq!(archives[0]["task_tree"]["nodes"]["goal"]["objective"],"CANCELLED_DRAG_GOAL");
        assert_eq!(archives[1]["id"],"request_2");assert_eq!(archives[1]["started_turn"],2);
        assert_eq!(archives[1]["scheduler"]["main_task"]["goal"],"不用做拖拽了，先解释闭包");
        assert_eq!(archives[1]["scheduler"]["main_task"]["completed"],true);
        let next_organizer:Value=test_model_context(&requests[5]);
        assert!(next_organizer["process"].get("previous_main_tasks").is_none());
        assert!(!next_organizer.to_string().contains("CANCELLED_DRAG_"));
        assert!(!next_organizer.to_string().contains("Closures capture their environment"));
        assert!(third["scheduler"]["frames"].get("task_a").is_none());
        let restored:crate::work_scheduler::WorkScheduler=serde_json::from_value(second["scheduler"].clone()).unwrap();
        let plan=restored.unified_flow_plan(None);let nodes=plan["nodes"].as_array().unwrap();
        assert!(nodes.iter().any(|n|n["id"]=="task_a"&&n["status"]=="done"));
        assert!(nodes.iter().any(|n|n["id"]=="request_1:task_a"&&n["status"]=="deprecated"));
        assert!(nodes.iter().filter(|n|n["id"].as_str().unwrap().starts_with("request_1:")).all(|n|n["status"]=="deprecated"));
    }

    #[tokio::test]
    async fn organizer_reads_prior_goal_history_only_on_request() {
        let mut first=flow_order("first","direct");first["final_answer"]=json!(true);
        let mut next=flow_order("next","direct");next["final_answer"]=json!(true);
        let old_answer="历史结论：HISTORY_ONLY_FACT";
        let (events,requests)=run_flow_script(vec![
            scheduling_reply(json!({"action":"work","orders":[first]})),
            text_reply(old_answer),finish_simple_reply(old_answer),
            scheduling_reply(json!({"action":"read_session_history","role":"worker","turn":1,"query":"历史结论"})),
            scheduling_reply(json!({"action":"work","orders":[next]})),
            text_reply("已按需回顾历史并完成新任务"),finish_simple_reply("已按需回顾历史并完成新任务"),
        ],&[("完成第一项分析",3),("开始新任务：请按需查阅之前的分析结论",3)]).await;
        let initial:Value=test_model_context(&requests[3]);
        assert!(initial["process"].get("previous_main_tasks").is_none());
        assert!(!initial.to_string().contains("HISTORY_ONLY_FACT"));
        let after_read:Value=test_model_context(&requests[4]);
        assert_eq!(after_read["read_task_results"][0]["action"],"read_session_history");
        assert!(after_read["read_task_results"][0]["result"].to_string().contains("HISTORY_ONLY_FACT"));
        assert_eq!(committed_turn(&events,2)["scheduler"]["archived_requests"].as_array().unwrap().len(),1);
    }

    #[tokio::test]
    async fn agent_flow_replaces_unfinished_complex_goal_with_new_complex_goal() {
        let new_goal="不用做拖拽了，改为建立新的导出流程";
        let (events,requests)=run_flow_script(vec![
            scheduling_reply(json!({"action":"work","reason":"implement old feature","orders":[flow_order("task_a","direct")]})),
            cancelled_drag_reply(),
            scheduling_reply(json!({"action":"work","request_action":"replace","reason":"human changed the goal","orders":[flow_order("task_a","direct")]})),
            text_reply("New export facts delivered"),finish_flow_reply(),
        ],&[("CANCELLED_DRAG_GOAL",1),(new_goal,5)]).await;
        let commit=committed_turn(&events,2);
        let tree:crate::flow_tree::TaskTree=serde_json::from_value(commit["task_tree"].clone()).unwrap();
        assert!(tree.root_finished());assert_eq!(commit["task_tree"]["nodes"]["goal"]["objective"],new_goal);
        assert_eq!(commit["scheduler"]["request_started_turn"],2);assert_eq!(commit["scheduler"]["request_completed"],true);
        assert_eq!(commit["scheduler"]["frames"].as_object().unwrap().len(),1);
        let packets=worker_packets(&requests);
        assert!(packets[1].get("goal_boundary").is_none());
        assert!(packets[1].get("human_request").is_none());
        assert!(packets[1].get("request_id").is_none());
        assert!(!requests[3]["messages"].to_string().contains("CANCELLED_DRAG_"));
        let final_organizer:Value=test_model_context(&requests[4]);
        assert!(final_organizer["previous_unfinished_tree"].is_null());
        assert!(final_organizer["process"].get("previous_main_tasks").is_none());
        assert!(!final_organizer.to_string().contains("CANCELLED_DRAG_"));
    }

    #[test]
    fn request_lifecycle_replacement_is_atomic_and_subtasks_preserve_original_goal() {
        use crate::{flow_tree::TaskTree,work_scheduler::WorkScheduler};
        let root=std::env::current_dir().unwrap();
        let (mut scheduler,tree,_)=apply_organizer_decision(&WorkScheduler::default(),&TaskTree::default(),&TaskTree::default(),&root,
            "Original goal",json!({"action":"work","orders":[flow_order("task_a","direct")]}),false,false).unwrap();
        let replacement=json!({"action":"work","request_action":"replace","orders":[flow_order("new","direct")]});
        assert!(apply_organizer_decision(&scheduler,&tree,&tree,&root,"New goal",replacement.clone(),false,false).is_err());
        scheduler.handoff=Some(json!({"intent":"session_continuation"}));
        let before=scheduler.snapshot();let tree_before=tree.snapshot();
        assert!(apply_organizer_decision(&scheduler,&tree,&tree,&root,"New goal",
            json!({"action":"work","orders":[flow_order("new","direct")]}),false,false).is_err());
        assert!(apply_organizer_decision(&scheduler,&tree,&tree,&root,"New goal",json!({"action":"work","request_action":"replace",
            "orders":[{"id":"bad","node_id":"direct","goal":"missing done_when"}]}),false,false).is_err());
        assert_eq!(scheduler.snapshot(),before);assert_eq!(tree.snapshot(),tree_before);
        let (subtask,subtree,_)=apply_organizer_decision(&scheduler,&tree,&tree,&root,"Do a repair first",
            json!({"action":"work","request_action":"subtask","orders":[flow_order("repair","direct")]}),false,false).unwrap();
        assert!(subtask.archived_requests.is_empty());assert!(subtask.frames["task_a"].invalidated_by_plan_revision.is_none());
        assert_eq!(subtree.worker_boundary(subtask.node())["request_goal"],"Original goal");
        let (replacement,_,_)=apply_organizer_decision(&scheduler,&tree,&tree,&root,"New goal",replacement,false,false).unwrap();
        assert!(replacement.frames.get("task_a").is_none());
        assert!(apply_organizer_decision(&replacement,&TaskTree::default(),&tree,&root,"New goal",
            json!({"action":"select","flow_update":{"resume_tree":true,"current_node_id":"work_task_a"}}),false,false).is_err());
        assert!(!replacement.organizer_input().to_string().contains("Original goal"));
    }

    #[test]
    fn organizer_dispatches_pending_node_directly_and_rejects_completed_node_assignment() {
        use crate::{flow_tree::TaskTree,work_scheduler::WorkScheduler};
        let root=std::env::current_dir().unwrap();let initial=json!({"action":"work","reason":"assign the pending upload node",
            "flow_update":{"plan":{"mode":"tree","nodes":[
                {"id":"goal","parent_id":null,"title":"Load deck","kind":"goal","objective":"Load a presentation","done_when":"The presentation is visible"},
                {"id":"upload","parent_id":"goal","title":"Upload deck","kind":"worker","objective":"Upload the real PPTX","done_when":"Slide list is nonempty"}
            ]},"current_node_id":"upload"},"orders":[flow_order("upload","upload")]});
        let (mut scheduler,mut tree,_)=apply_organizer_decision(&WorkScheduler::default(),&TaskTree::default(),&TaskTree::default(),&root,"Load a deck",initial,false,false).unwrap();
        assert_eq!(scheduler.id(),"upload");assert_eq!(scheduler.node(),"upload");assert_eq!(tree.status("upload"),Some("running"));
        let output=scheduler.return_work(&json!({"summary":"File assigned"})).unwrap();tree.record_work_return(&output).unwrap();
        assert_eq!(tree.status("upload"),Some("done"));
        let decision=json!({"action":"work","reason":"try to reopen the completed node","orders":[flow_order("upload_again","upload")]});
        let error=apply_organizer_decision(&scheduler,&tree,&tree,&root,"Load a deck",decision.clone(),false,false).err().unwrap();
        assert!(error.to_string().contains("field_path=orders[0].node_id")&&error.to_string().contains("status='done'"));
        let feedback=organizer_contract_feedback(&error,&decision,&scheduler,&tree);
        assert_eq!(feedback["error_code"],"COMPLETED_NODE_IMMUTABLE");assert_eq!(feedback["field_path"],"orders[0].node_id");
    }

    #[test]
    fn duplicate_fresh_process_observation_is_rejected_with_structured_feedback() {
        use crate::{flow_tree::TaskTree,work_scheduler::WorkScheduler};
        let root=std::env::current_dir().unwrap();
        let (mut scheduler,tree,_)=apply_organizer_decision(&WorkScheduler::default(),&TaskTree::default(),&TaskTree::default(),&root,
            "Start the project",json!({"action":"work","reason":"record process state","orders":[flow_order("probe","direct")]}),false,false).unwrap();
        let scope=json!({"project_path":".","script":"dev"});
        let sample=crate::project_process::build_process_observation(&root,&scope,&[]).unwrap();
        scheduler.frames.get_mut("probe").unwrap().project_observation=sample;
        let before=scheduler.snapshot();
        let decision=json!({"action":"work","reason":"repeat process check","orders":[{
            "id":"probe_again","node_id":"direct","goal":"Check process state again","done_when":"state checked","completion":"output",
            "project_observation":scope
        }]});
        let error=apply_organizer_decision(&scheduler,&tree,&tree,&root,"Start the project",decision.clone(),false,false).err().unwrap();
        assert!(error.to_string().contains("duplicate process observation"));
        assert_eq!(scheduler.snapshot(),before);
        let feedback=organizer_contract_feedback(&error,&decision,&scheduler,&tree);
        assert_eq!(feedback["error_code"],"DUPLICATE_PROCESS_OBSERVATION");
        assert_eq!(feedback["field_path"],"execution_scope.project_observation");
        assert_eq!(feedback["rejected_value"],scope);
        assert_eq!(feedback["submitted_value_present"],true);
        assert!(feedback["expected_shape"].as_str().unwrap().contains("schedule_task.inputs"));

        let workspace_scope=json!({"coverage":"workspace_list"});
        scheduler.frames.get_mut("probe").unwrap().project_observation=
            crate::project_process::build_process_observation(&root,&workspace_scope,&[]).unwrap();
        let workspace_decision=json!({"action":"work","reason":"repeat workspace process listing","orders":[{
            "id":"workspace_probe_again","node_id":"direct","goal":"Check all workspace processes again","done_when":"workspace list checked","completion":"output",
            "project_observation":workspace_scope
        }]});
        let error=apply_organizer_decision(&scheduler,&tree,&tree,&root,"Start the project",workspace_decision,false,false).err().unwrap();
        assert!(error.to_string().contains("duplicate process observation"));
    }

    #[test]
    fn organizer_contract_feedback_preserves_dependency_and_removed_flow_paths() {
        use crate::{flow_tree::TaskTree,work_scheduler::{WorkFrame,WorkOrder,WorkScheduler,WorkStatus}};
        let mut scheduler=WorkScheduler::default();
        let order=WorkOrder{id:"backend_work".into(),node_id:"backend".into(),revision:2,goal:"Start backend".into(),done_when:"API ready".into(),..Default::default()};
        scheduler.frames.insert("backend_work".into(),WorkFrame{order,status:WorkStatus::Done,output:Some(json!({"id":"backend_work","node_id":"backend","revision":2,"done":true,"summary":"API ready","exported_data":{"font_api":"/api/fonts"}})),..Default::default()});
        let mut tree=TaskTree::default();tree.apply(&json!({"plan":{"mode":"tree","nodes":[
            {"id":"goal","parent_id":null,"title":"Load presentation","kind":"goal","objective":"Load a presentation","done_when":"Slides loaded"},
            {"id":"upload","parent_id":"goal","title":"Upload presentation","kind":"worker","objective":"Upload a presentation","done_when":"Slides loaded"}
        ]},"current_node_id":"goal"})).unwrap();
        let dependency_decision=json!({"orders":[{"dependency_inputs":[{"work_id":"backend_work","fields":["missing"]}]}]});
        let dependency_error=anyhow::anyhow!("field_path=orders[0].dependency_inputs[0].fields: missing required field");
        let feedback=organizer_contract_feedback(&dependency_error,&dependency_decision,&scheduler,&tree);
        assert_eq!(feedback["error_code"],"INVALID_DEPENDENCY_INPUT");
        assert_eq!(feedback["field_path"],"orders[0].dependency_inputs[0].fields");
        assert_eq!(feedback["rejected_value"],json!(["missing"]));
        assert_eq!(feedback["allowed_values"]["available_dependency_deliveries"][0]["exported_fields"],json!(["font_api"]));

        let resume_decision=json!({"flow_update":{"node_updates":[{"id":"upload","resume":true}],"current_node_id":"upload"}});
        let resume_error=anyhow::anyhow!("field_path=flow_update.node_updates[0].resume: node_id='upload' has status='pending'");
        let feedback=organizer_contract_feedback(&resume_error,&resume_decision,&scheduler,&tree);
        assert_eq!(feedback["error_code"],"REMOVED_FLOW_PROTOCOL");
        assert_eq!(feedback["field_path"],"flow_update.node_updates[0].resume");
        assert_eq!(feedback["allowed_values"],json!(["schedule_task","read_task_result","read_flow_page","read_session_history","search_work_memory",
            "search_architecture_memory","search_symbol_business_context","search_project_symbols","read_project_symbol","read_source_material",
            "revisit_task","finish_request"]));
        assert!(feedback["expected_shape"].as_str().unwrap().contains("host-managed"));
    }

    #[tokio::test]
    async fn organizer_gets_one_targeted_contract_correction_and_stops_on_repeat() {
        let mut answer=flow_order("answer","direct");answer["final_answer"]=json!(true);
        let (events,requests)=run_flow_script_with_organizer_errors(vec![
            scheduling_reply(json!({"action":"work","request_action":"replace","reason":"initial decision","orders":[answer.clone()]})),
            scheduling_reply(json!({"action":"work","request_action":"continue","reason":"correct lifecycle field","orders":[answer]})),
            text_reply("The answer is ready"),
            finish_simple_reply("The answer is ready"),
        ],&[("Answer this question",5)]).await;
        let errors=events.iter().filter(|(kind,_)|kind=="organizer/error").collect::<Vec<_>>();
        assert_eq!(errors.len(),1);
        assert_eq!(errors[0].1["feedback"]["field_path"],"request_action");
        let organizer_inputs=requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task")))
            .map(test_model_context).collect::<Vec<_>>();
        assert_eq!(organizer_inputs.len(),3);
        assert_eq!(organizer_inputs[1]["last_decision_error"]["error_code"],"INVALID_REQUEST_ACTION");
        assert!(organizer_inputs[1]["last_decision_error"]["correction_instruction"].as_str().unwrap().contains("one targeted correction"));
        assert_eq!(organizer_inputs[2]["process"]["current_result"]["summary"],"The answer is ready");

        let (repeat_events,repeat_requests)=run_flow_script_with_organizer_errors(vec![
            scheduling_reply(json!({"action":"work","request_action":"replace","reason":"invalid first decision","orders":[flow_order("answer","direct")]})),
            scheduling_reply(json!({"action":"work","request_action":"replace","reason":"same invalid correction","orders":[flow_order("answer","direct")]})),
        ],&[("Answer this question",5)]).await;
        let repeated=repeat_events.iter().filter(|(kind,_)|kind=="organizer/error").collect::<Vec<_>>();
        assert_eq!(repeated.len(),2);
        assert_eq!(repeated[1].1["repeated_fingerprint"],true);
        assert_eq!(repeat_requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))).count(),2);
        assert!(!repeat_events.iter().any(|(kind,_)|kind=="tool/call"));
    }

    #[tokio::test]
    async fn organizer_request_failure_does_not_consume_contract_correction() {
        let mut answer=flow_order("answer","direct");answer["final_answer"]=json!(true);
        let (events,requests)=run_flow_script_with_organizer_errors(vec![
            organizer_request_error_reply(),
            scheduling_reply(json!({"action":"work","request_action":"replace","reason":"invalid lifecycle field","orders":[answer.clone()]})),
            scheduling_reply(json!({"action":"work","request_action":"continue","reason":"correct the lifecycle field","orders":[answer]})),
            text_reply("The answer is ready"),
            finish_simple_reply("The answer is ready"),
        ],&[("Answer this question",6)]).await;
        assert_eq!(events.iter().filter(|(kind,_)|kind=="organizer/request_error").count(),1);
        let contract_errors=events.iter().filter(|(kind,_)|kind=="organizer/error").collect::<Vec<_>>();
        assert_eq!(contract_errors.len(),1);
        assert_eq!(contract_errors[0].1["repeated_fingerprint"],false);
        let organizer_inputs=requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task")))
            .map(test_model_context).collect::<Vec<_>>();
        assert!(organizer_inputs[1]["last_request_error"]["stage"]=="organizer_request");
        assert!(organizer_inputs[1]["last_decision_error"].is_null());
        assert_eq!(organizer_inputs[2]["last_decision_error"]["error_code"],"INVALID_REQUEST_ACTION");
        assert!(events.iter().any(|(kind,data)|kind=="turn/end"&&data["reason"]["kind"]=="completed"));
    }

    #[tokio::test]
    async fn exhausted_organizer_requests_preserve_completed_work_and_report_failure_stage() {
        let invalid=flow_order("not_applied","not_applied");
        let (events,requests)=run_flow_script_with_organizer_errors(vec![
            scheduling_reply(json!({"action":"work","reason":"start one completed node","orders":[flow_order("started","started")]})),
            yield_reply("completed","Service started and ready at http://127.0.0.1:3000/",json!({"ready_url":"http://127.0.0.1:3000/","process_id":321})),
            scheduling_reply(json!({"action":"work","request_action":"replace","reason":"invalid lifecycle value","orders":[invalid]})),
            organizer_request_error_reply(),
            organizer_request_error_reply(),
        ],&[("Keep completed work when Organizer fails",5)]).await;
        let failures=events.iter().filter(|(kind,_)|kind=="organizer/request_error").collect::<Vec<_>>();
        assert_eq!(failures.len(),2);
        assert_eq!(failures[0].1["retrying"],true);
        assert_eq!(failures[1].1["retrying"],false);
        let saved=events.iter().filter(|(kind,_)|kind=="scheduler/state").last().unwrap();
        assert_eq!(saved.1["state"]["handoff"]["outcome"],"completed","preserve the Worker's explicit outcome during request failure");
        assert_eq!(saved.1["state"]["handoff"]["done"],true);
        assert_eq!(saved.1["state"]["handoff"]["organizer_failure"]["failure_stage"],"organizer_request");
        assert_eq!(saved.1["state"]["handoff"]["organizer_failure"]["resumable"],true);
        assert_eq!(saved.1["state"]["handoff"]["organizer_failure"]["last_decision_error"]["field_path"],"request_action");
        assert_eq!(saved.1["state"]["handoff"]["organizer_failure"]["contract_error_fingerprints"].as_array().unwrap().len(),1);
        let completed=saved.1["state"]["frames"].as_object().unwrap().values().find(|frame|frame["order"]["id"]=="started").unwrap();
        assert_eq!(completed["status"],"done");
        assert_eq!(completed["output"]["exported_data"]["ready_url"],"http://127.0.0.1:3000/");
        let organizer_calls=requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task"))).count();
        assert_eq!(organizer_calls,4);
    }

    #[tokio::test]
    async fn split_result_survives_consecutive_organizer_timeouts_and_continues_once() {
        let (_,mut split_result)=yield_reply("need_split","The endpoint check must be split from the upload flow",json!({"ready_url":"http://127.0.0.1:3000/"}));
        split_result["tool_calls"][0]["function"]["arguments"]=json!(json!({
            "outcome":"need_split","summary":"The endpoint check must be split from the upload flow",
            "exported_data":{"ready_url":"http://127.0.0.1:3000/"},
            "suggested_children":[{"goal":"Check the font endpoint","done_when":"Return its actual HTTP result",
                "dependency_inputs":[{"work_id":"task_a","fields":["ready_url"]}]}]
        }).to_string());
        let mut child=flow_order("font_check","direct");
        child["upstream_ids"]=json!(["task_a"]);
        child["dependency_inputs"]=json!([{"work_id":"task_a","fields":["ready_url"]}]);
        let (events,requests)=run_flow_script_with_organizer_errors(vec![
            scheduling_reply(json!({"action":"work","reason":"inspect the current endpoint","orders":[flow_order("task_a","direct")]})),
            (false,split_result),organizer_timeout_reply(),organizer_timeout_reply(),
            scheduling_reply(json!({"action":"work","request_action":"continue","reason":"continue from the sealed split result","orders":[child]})),
            text_reply("The font endpoint returned HTTP 200"),finish_flow_reply(),
        ],&[("Verify the font endpoint",6),("Continue verifying the font endpoint",6)]).await;

        let failures=events.iter().filter(|(kind,_)|kind=="organizer/request_error").collect::<Vec<_>>();
        assert_eq!(failures.len(),2);
        assert_eq!(failures[0].1["retrying"],true);
        assert_eq!(failures[1].1["retrying"],false);
        let organizer_inputs=requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten()
            .any(|tool|tool.pointer("/function/name").and_then(Value::as_str).is_some_and(|name|name=="schedule_task")))
            .map(test_model_context).collect::<Vec<_>>();
        let resumed_input=organizer_inputs.iter().find(|input|input["last_request_error"]["stage"]=="organizer_request").unwrap();
        assert_eq!(resumed_input["process"]["current_result"]["outcome"],"need_split");
        assert!(resumed_input["process"]["current_result"]["need_split"].is_null(),"do not derive unsubmitted flags from the Worker outcome");
        assert_eq!(resumed_input["process"]["current_result"]["suggested_children"][0]["goal"],"Check the font endpoint");
        let child_packet=worker_packets(&requests).into_iter().find(|packet|packet["current_work"]["id"]=="font_check").unwrap();
        assert!(child_packet["upstream_outputs"].to_string().contains("ready_url"));
        let final_commit=committed_turn(&events,2);
        assert_eq!(final_commit["scheduler"]["frames"]["task_a"]["status"],"done");
        assert_eq!(final_commit["scheduler"]["frames"]["task_a"]["output"]["outcome"],"need_split");
        assert_eq!(final_commit["scheduler"]["frames"]["font_check"]["status"],"done");
        assert_eq!(final_commit["scheduler"]["request_completed"],true);
        assert_eq!(final_commit["scheduler"]["frames"].as_object().unwrap().len(),2,"continuation must preserve the split delivery and dispatch one child");
    }

    #[test]
    fn split_handoff_survives_organizer_timeout_exhaustion_and_resumes() {
        use crate::{flow_tree::TaskTree,work_scheduler::WorkScheduler};
        let root=std::env::current_dir().unwrap();
        let (mut scheduler,tree,_)=apply_organizer_decision(&WorkScheduler::default(),&TaskTree::default(),&TaskTree::default(),&root,
            "Finish the split goal",json!({"action":"work","orders":[flow_order("task_a","direct")]}),false,false).unwrap();
        scheduler.return_work(&json!({"outcome":"need_split","summary":"Split verification from the remaining work",
            "suggested_children":[{"goal":"Verify the font endpoint","done_when":"The endpoint returns HTTP 2xx",
                "dependency_inputs":[{"node_id":"font_endpoint","fields":["http_observations"]}]}]})).unwrap();

        let timeout_attempts=(1..=ORGANIZER_REQUEST_RETRY_LIMIT+1).map(|attempt|json!({"error_code":"ORGANIZER_REQUEST_FAILED",
            "stage":"organizer_request","attempt":attempt,"retry_limit":ORGANIZER_REQUEST_RETRY_LIMIT,"error":"Organizer request timed out"})).collect::<Vec<_>>();
        let failure_record=json!({"failure_stage":"organizer_request","failure":timeout_attempts.last().unwrap(),
            "last_decision_error":Value::Null,"contract_error_fingerprints":[],"resumable":true});
        attach_organizer_failure(&mut scheduler.handoff,failure_record);
        scheduler.finished=false;scheduler.request_completed=Some(false);

        let snapshot=scheduler.snapshot();
        let restored:WorkScheduler=serde_json::from_value(snapshot).unwrap();
        let (restored_tree,mut restored,completed)=restore_execution_session(&tree,Some(restored));
        assert!(!completed);
        assert_eq!(organizer_resume_request_error(restored.pending_handoff()).unwrap()["stage"],"organizer_request");
        let previous_handoff=restored.pending_handoff().cloned();
        restored.handoff=Some(json!({"done":false,"intent":"session_continuation","previous_handoff":previous_handoff}));
        assert_eq!(organizer_resume_request_error(restored.pending_handoff()).unwrap()["stage"],"organizer_request");
        let resumed_input=restored.organizer_input();
        assert_eq!(resumed_input["handoff"]["intent"],"session_continuation");
        assert_eq!(resumed_input["current_result"]["outcome"],"need_split");
        assert_eq!(resumed_input["current_result"]["suggested_children"][0]["goal"],"Verify the font endpoint");
        assert_eq!(resumed_input["current_result"]["suggested_children"][0]["done_when"],"The endpoint returns HTTP 2xx");

        let mut child=flow_order("split_child","direct");child["upstream_ids"]=json!(["task_a"]);
        let (mut continued,_,_)=apply_organizer_decision(&restored,&restored_tree,&tree,&root,"Continue the split goal",
            json!({"action":"work","request_action":"continue","orders":[child]}),false,false).unwrap();
        assert_eq!(continued.id(),"split_child");
        assert_eq!(continued.frames["task_a"].status,crate::work_scheduler::WorkStatus::Done);
        assert!(continued.frames["task_a"].invalidated_by_plan_revision.is_none());
        assert_eq!(continued.frames["task_a"].output.as_ref().unwrap()["outcome"],"need_split");
        continued.return_work(&json!({"summary":"Focused child is complete"})).unwrap();
        assert_eq!(continued.frames["task_a"].status,crate::work_scheduler::WorkStatus::Done);
        assert_eq!(continued.frames["split_child"].order.upstream_ids,vec!["task_a"]);
    }

    #[tokio::test]
    async fn agent_flow_preserves_completed_child_at_budget_limit_then_starts_fresh_after_goal_completion() {
        let mut b = flow_order("task_b","work_task_b");
        b["upstream_ids"] = json!(["work_task_a"]);
        let mut answer = flow_order("answer","direct");
        answer["final_answer"] = json!(true);
        let (events, requests) = run_flow_script(vec![
            scheduling_reply(json!({"action":"work","reason":"first part","orders":[flow_order("task_a","direct")]})),
            text_reply("A delivered"), text_reply("Stopped at the round limit; B remains"),
            scheduling_reply(json!({"action":"work","request_action":"continue","reason":"continue with B using A","orders":[b]})),
            text_reply("B delivered"), finish_flow_reply(),
            scheduling_reply(json!({"action":"work","reason":"new simple request","orders":[answer]})), text_reply("Simple answer"),
            finish_simple_reply("Simple answer"),
            scheduling_reply(json!({"action":"work","reason":"another simple request","orders":[{
                "id":"next_answer","node_id":"direct","goal":"Answer next question","done_when":"Answer delivered","completion":"output","final_answer":true}]})),
            text_reply("Next simple answer"),
            finish_simple_reply("Next simple answer"),
        ], &[("Complete A and B",2),("继续",5),("A new simple question",3),("Another simple question",3)]).await;
        let first = committed_turn(&events,1);
        assert_eq!(first["scheduler"]["frames"]["task_a"]["status"], "done");
        assert_eq!(first["scheduler"]["request_completed"], false);
        let first_tree: crate::flow_tree::TaskTree = serde_json::from_value(first["task_tree"].clone()).unwrap();
        assert!(!first_tree.root_finished());
        let second = committed_turn(&events,2);
        assert_eq!(second["scheduler"]["frames"]["task_b"]["order"]["upstream_ids"], json!(["task_a"]));
        assert_eq!(second["scheduler"]["request_completed"], true);
        let third = committed_turn(&events,3);
        assert_eq!(third["scheduler"]["frames"].as_object().unwrap().len(),1);
        assert!(third["task_tree"]["nodes"].as_object().unwrap().is_empty());
        assert_eq!(third["scheduler"]["request_completed"],true);
        assert!(third["scheduler"]["handoff"].is_null());
        let fourth = committed_turn(&events,4);
        assert_eq!(fourth["scheduler"]["frames"].as_object().unwrap().len(),1);
        assert!(fourth["scheduler"]["frames"].get("next_answer").is_some());
        let b_request = requests.iter().find(|body| body["messages"][1]["name"]=="organizer" && body["messages"][1]["content"].as_str().is_some_and(|s|s.contains("task_b"))).unwrap();
        assert!(b_request["messages"].as_array().unwrap().iter().any(|message|message["name"]=="worker_return_task_a" && message["content"].as_str().is_some_and(|text|text.contains("A delivered"))));
    }

    #[tokio::test]
    async fn agent_flow_preserves_blocked_delivery_while_aggregating_replacement_work() {
        let (events, _) = run_flow_script(vec![
            scheduling_reply(json!({"action":"work","reason":"try A","orders":[flow_order("task_a","direct")]})),
            yield_reply("blocked","A cannot proceed",json!({})),
            scheduling_reply(json!({"action":"work","reason":"replace A with B","orders":[flow_order("task_b","direct")]})),
            text_reply("B delivered"), finish_flow_reply(),
        ], &[("Achieve the goal",6)]).await;
        let commit = committed_turn(&events,1);
        let tree: crate::flow_tree::TaskTree = serde_json::from_value(commit["task_tree"].clone()).unwrap();
        assert_eq!(tree.status("work_task_a"),Some("done"));
        assert_eq!(tree.status("work_task_b"),Some("done"));
        assert!(tree.root_finished());
        assert_eq!(commit["scheduler"]["frames"]["task_a"]["status"],"done");
        assert_eq!(commit["scheduler"]["frames"]["task_a"]["output"]["outcome"],"blocked");
        assert!(commit["scheduler"]["frames"]["task_a"].get("expectation_met").is_none());
        assert!(commit["scheduler"]["frames"]["task_a"]["invalidated_by_plan_revision"].is_null());
        assert!(events.iter().any(|(kind,data)| kind=="execution/tick" && data["tick"]["work_id"]=="task_b" && data["tick"]["done"]==true));
    }

    #[tokio::test]
    async fn agent_flow_node_dependency_uses_revisited_delivery_not_lexically_later_old_work() {
        let mut c = flow_order("task_c","direct");
        c["upstream_ids"] = json!(["node_b"]);
        c["dependency_inputs"] = json!([{"node_id":"node_b","revision":2,"fields":["url"]}]);
        let (events, requests) = run_flow_script_with_observer(vec![
            scheduling_reply(json!({"action":"work","reason":"produce B","orders":[flow_order("task_b","node_b")],
                "flow_update":{"current_node_id":"node_b","node_updates":[
                    {"id":"goal","parent_id":null,"title":"Request","objective":"Use repaired B","done_when":"all results delivered"},
                    {"id":"node_b","parent_id":"goal","title":"B","objective":"Produce B","done_when":"B delivered"}]}})),
            text_reply("B initial delivery"),
            scheduling_reply(json!({"action":"revisit","auto_apply_observer":true,"reason":"repair initial B","target_node_id":"node_b","repair_goal":"Repair B"})),
            yield_reply("completed","B revision 2 delivered",json!({"url":"http://127.0.0.1:4321"})),
            scheduling_reply(json!({"action":"work","reason":"consume repaired B","orders":[c]})),
            text_reply("C consumed revision 2"), finish_flow_reply(),
        ], &[("Use repaired B",8)],true).await;
        let commit = committed_turn(&events,1);
        assert_eq!(commit["scheduler"]["frames"]["task_c"]["order"]["upstream_ids"],json!(["node_b_r2"]));
        assert_eq!(commit["scheduler"]["frames"]["task_c"]["order"]["dependency_inputs"][0]["work_id"],"node_b_r2");
        let c_request = requests.iter().find(|body|body["messages"][1]["name"]=="organizer" && body["messages"][1]["content"].as_str().is_some_and(|s|s.contains("task_c"))).unwrap();
        assert!(c_request["messages"].as_array().unwrap().iter().any(|message|message["name"].as_str().is_some_and(|name|name.starts_with("worker_return_")) && message["content"].as_str().is_some_and(|text|text.contains("http://127.0.0.1:4321"))));
        let tree: crate::flow_tree::TaskTree = serde_json::from_value(commit["task_tree"].clone()).unwrap();
        assert!(tree.root_finished());
    }

    #[test]
    fn agent_decision_seals_split_and_repair_results_and_is_atomic() {
        use crate::{flow_tree::TaskTree, work_scheduler::WorkScheduler};
        let root = std::env::current_dir().unwrap();
        for outcome in ["need_split","upstream_problem"] {
            let (mut scheduler,mut tree,_) = apply_organizer_decision(&WorkScheduler::default(),&TaskTree::default(),&TaskTree::default(),&root,
                "Complete the original goal",json!({"action":"work","orders":[flow_order("task_a","direct")]}),false,false).unwrap();
            scheduler.return_work(&json!({"outcome":outcome,"summary":"Need focused repair"})).unwrap();
            tree.record_work_return(scheduler.output().unwrap()).unwrap();
            assert_eq!(scheduler.frames["task_a"].status,crate::work_scheduler::WorkStatus::Done);
            assert_eq!(tree.status("work_task_a"),Some("done"));
            let mut repair=flow_order("repair","direct");repair["upstream_ids"]=json!(["task_a"]);
            let (mut scheduler,mut tree,_) = apply_organizer_decision(&scheduler,&tree,&tree,&root,
                "Complete the original goal",json!({"action":"work","orders":[repair]}),false,false).unwrap();
            assert!(scheduler.frames["task_a"].invalidated_by_plan_revision.is_none());
            assert_eq!(scheduler.frames["task_a"].status,crate::work_scheduler::WorkStatus::Done);
            assert_eq!(tree.status("work_task_a"),Some("done"));
            scheduler.return_work(&json!({"summary":"repair delivered"})).unwrap();
            tree.record_work_return(scheduler.output().unwrap()).unwrap();
            assert_eq!(scheduler.frames["task_a"].status,crate::work_scheduler::WorkStatus::Done);
            assert_eq!(scheduler.frames["repair"].status,crate::work_scheduler::WorkStatus::Done);
            assert_eq!(scheduler.frames["repair"].order.upstream_ids,vec!["task_a"]);
            assert_eq!(tree.status("work_repair"),Some("done"));
            let scheduler_before=scheduler.snapshot(); let tree_before=tree.snapshot();
            assert!(apply_organizer_decision(&scheduler,&tree,&TaskTree::default(),&root,"Complete the original goal",
                json!({"action":"work","orders":[{"id":"bad","node_id":"direct","goal":"missing completion contract"}]}),false,false).is_err());
            assert_eq!(scheduler.snapshot(),scheduler_before); assert_eq!(tree.snapshot(),tree_before);
        }
    }

    #[test]
    fn direct_request_needs_explicit_success_and_blocked_runs_remain_resumable() {
        use crate::{flow_tree::TaskTree, work_scheduler::{WorkScheduler,WorkOrder}};
        let mut scheduler=WorkScheduler::default();
        scheduler.apply(&json!({"action":"work","orders":[WorkOrder {id:"simple".into(),node_id:"direct".into(),goal:"Answer".into(),done_when:"Answer delivered".into(),..Default::default()}]}),false,false).unwrap();
        scheduler.return_work(&json!({"summary":"local result"})).unwrap();
        assert!(scheduler.all_done());
        assert!(!restore_execution_session(&TaskTree::default(),Some(scheduler.clone())).2);
        scheduler.finish("Still blocked on remaining requirement",true).unwrap();
        let (_,resumed,completed)=restore_execution_session(&TaskTree::default(),Some(scheduler.clone()));
        assert!(!completed); assert!(!resumed.finished); assert!(resumed.frames.contains_key("simple"));
        scheduler.finish("Request complete",false).unwrap();
        assert!(restore_execution_session(&TaskTree::default(),Some(scheduler)).2);
    }

    #[test]
    fn legacy_execution_snapshot_requires_finished_root_not_just_finished_packets() {
        use crate::{flow_tree::TaskTree,work_scheduler::WorkScheduler};
        let root=std::env::current_dir().unwrap();
        let (mut scheduler,mut tree,_)=apply_organizer_decision(&WorkScheduler::default(),&TaskTree::default(),&TaskTree::default(),&root,
            "Finish the whole goal",json!({"action":"work","orders":[flow_order("legacy","direct")]}),false,false).unwrap();
        scheduler.return_work(&json!({"summary":"legacy child delivered"})).unwrap();
        tree.record_work_return(scheduler.output().unwrap()).unwrap();
        let legacy_handoff=scheduler.handoff.clone();
        scheduler.finish("legacy request finished",false).unwrap();
        scheduler.handoff=legacy_handoff;
        let mut snapshot=scheduler.snapshot();
        snapshot.as_object_mut().unwrap().remove("request_completed");
        let legacy:WorkScheduler=serde_json::from_value(snapshot).unwrap();
        assert_eq!(legacy.request_completed,None);
        assert!(!restore_execution_session(&tree,Some(legacy.clone())).2);
        tree.finish_request("legacy root explicitly assessed as achieved",true).unwrap();
        assert!(restore_execution_session(&tree,Some(legacy)).2);
        // A newly recorded blocked finish cannot use the legacy compatibility rule.
        scheduler.request_completed=Some(false);
        assert!(!restore_execution_session(&tree,Some(scheduler)).2);
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

    #[test]
    fn organizer_flow_lifecycle_and_dispatch_superseding_test() {
        use crate::flow_tree::TaskTree;
        use crate::work_scheduler::{WorkScheduler, WorkOrder, Completion};

        // 1. F02: Verify completed tree lifecycle: when previous tree is finished, subsequent turn starts with a fresh empty tree
        let mut previous_tree = TaskTree::default();
        previous_tree.ensure_request_goal("Turn 1 Goal").unwrap();
        previous_tree.ensure_work_child("work_t1", "child 1", "done", &[]).unwrap();
        previous_tree.record_work_return(&json!({
            "id": "work_t1", "node_id": "work_t1", "revision": 1, "plan_revision": 1, "done": true, "summary": "done"
        })).unwrap();
        previous_tree.apply(&json!({
            "current_node_id": "goal",
            "node_result": { "node_id": "goal", "status": "completed", "summary": "Turn 1 Goal achieved" }
        })).unwrap();
        assert!(previous_tree.root_finished());

        let mut previous_scheduler = WorkScheduler::default();
        previous_scheduler.finished = true;
        previous_scheduler.request_completed = Some(true);

        // Turn 2 start check:
        let (_, _, is_completed_previous_session) = restore_execution_session(&previous_tree, Some(previous_scheduler));
        assert!(is_completed_previous_session);

        let mut turn2_tree = if is_completed_previous_session {
            TaskTree::default()
        } else {
            previous_tree.clone()
        };
        assert!(!turn2_tree.enabled());

        // Now Turn 2 can cleanly create a new goal without being rejected by Turn 1's finished root!
        turn2_tree.ensure_request_goal("Turn 2 New Goal").unwrap();
        assert!(turn2_tree.enabled());
        turn2_tree.ensure_work_child("work_t2", "child 2", "done", &[]).unwrap();
        assert!(turn2_tree.work_node_ready("work_t2"));

        // 2. F01 & F04: Session continuation superseding and dependency normalization via apply("work")
        let mut scheduler = WorkScheduler::default();
        let order_1 = WorkOrder {
            id: "task_1".into(),
            node_id: "work_1".into(),
            goal: "original requirement".into(),
            done_when: "done".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        scheduler.apply(&json!({
            "action": "work",
            "orders": [order_1]
        }), false, false).unwrap();
        assert_eq!(scheduler.current, "task_1");

        // Simulate session interruption / continuation handoff
        scheduler.handoff = Some(json!({
            "done": false,
            "intent": "session_continuation",
            "resumable_work": "task_1",
            "resumable_node": "work_1",
            "revision": 1
        }));

        // User gave new requirements: Organizer responds with action: "work" and a new order replacing task_1,
        // and order_3 which depends on order_2 via dependency_inputs (omitting upstream_ids)
        let order_2 = WorkOrder {
            id: "task_2".into(),
            node_id: "work_2".into(),
            goal: "new requirement replacing task_1".into(),
            done_when: "done".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        let order_3 = WorkOrder {
            id: "task_3".into(),
            node_id: "work_3".into(),
            goal: "followup step with dependency".into(),
            done_when: "done".into(),
            completion: Completion::Output,
            dependency_inputs: vec![json!({"work_id": "task_2", "revision": 1, "fields": ["app_url"]})],
            ..WorkOrder::default()
        };
        scheduler.apply(&json!({
            "action": "work",
            "orders": [order_2, order_3]
        }), false, false).unwrap();

        // task_1 must be marked invalidated by plan_revision
        assert!(scheduler.frames["task_1"].invalidated_by_plan_revision.is_some());
        // task_2 must be current
        assert_eq!(scheduler.current, "task_2");
        // task_3 must have normalized upstream_ids to include task_2
        assert_eq!(scheduler.frames["task_3"].order.upstream_ids, vec!["task_2"]);

        // Complete task_2 with exported field
        scheduler.return_work(&json!({
            "summary": "task_2 done",
            "exported_data": {"app_url": "http://127.0.0.1:3000"}
        })).unwrap();

        // task_3 should now activate
        scheduler.activate_next().unwrap();
        assert_eq!(scheduler.current, "task_3");

        // Worker input for task_3 gets the resolved delivery!
        let w_input = scheduler.worker_input("run task 3");
        let upstream = w_input["upstream_outputs"].as_array().unwrap();
        assert_eq!(upstream[0]["app_url"], "http://127.0.0.1:3000");

        // Complete task_3
        scheduler.return_work(&json!({"summary": "task_3 done"})).unwrap();
        assert!(scheduler.all_done());
    }

    #[test]
    fn test_agent_scenarios_continuity_and_revisit() {
        use crate::flow_tree::TaskTree;
        use crate::work_scheduler::{WorkScheduler, WorkOrder, WorkStatus, Completion};

        // Scenario A: Interrupted then continue (without superseding)
        let mut scheduler = WorkScheduler::default();
        let mut tree = TaskTree::default();
        tree.ensure_request_goal("Goal").unwrap();
        tree.ensure_work_child("work_1", "step 1", "done", &[]).unwrap();

        let order = WorkOrder {
            id: "task_1".into(),
            node_id: "work_1".into(),
            goal: "step 1".into(),
            done_when: "done".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        scheduler.apply(&json!({
            "action": "work",
            "orders": [order]
        }), false, false).unwrap();
        assert_eq!(scheduler.current, "task_1");

        // Interrupt happens
        let (_, _, is_completed_previous_session) = restore_execution_session(&tree, Some(scheduler.clone()));
        assert!(!is_completed_previous_session);

        // Turn 2 resume handoff
        scheduler.handoff = Some(json!({
            "done": false,
            "intent": "session_continuation",
            "resumable_work": "task_1",
            "resumable_node": "work_1",
            "revision": 1
        }));

        // Organizer decides to continue the existing work
        scheduler.apply(&json!({
            "action": "continue",
            "task_id": "task_1"
        }), false, false).unwrap();

        assert_eq!(scheduler.current, "task_1");
        assert_eq!(scheduler.frames["task_1"].status, WorkStatus::Running);
        assert!(scheduler.handoff.is_none());

        // Scenario B: Revisit and new version progress reporting
        scheduler.return_work(&json!({"summary": "task_1 done initial"})).unwrap();
        assert_eq!(scheduler.frames["task_1"].status, WorkStatus::Done);

        // Enqueue task 2
        let order_2 = WorkOrder {
            id: "task_2".into(),
            node_id: "work_2".into(),
            goal: "step 2".into(),
            done_when: "done".into(),
            completion: Completion::Output,
            upstream_ids: vec!["task_1".into()],
            ..WorkOrder::default()
        };
        tree.ensure_work_child("work_2", "step 2", "done", &[]).unwrap();
        scheduler.apply(&json!({
            "action": "work",
            "orders": [order_2]
        }), false, false).unwrap();
        assert_eq!(scheduler.current, "task_2");

        // Revisit task_1 because upstream problem found
        scheduler.apply(&json!({
            "action": "revisit",
            "target_node_id": "work_1",
            "reason": "upstream fix required",
            "repair_goal": "fix output in step 1"
        }), false, false).unwrap();

        // task_2 must be marked invalidated
        assert!(scheduler.frames["task_2"].invalidated_by_plan_revision.is_some());
        // task_1 activated with new instance id "work_1_r2", revision 2 and plan_revision 2
        assert_eq!(scheduler.current, "work_1_r2");
        assert_eq!(scheduler.node(), "work_1");
        assert_eq!(scheduler.revision(), 2);
        assert_eq!(scheduler.plan_revision, 2);

        // Simulated worker/progress payload generated for new version
        let progress = json!({
            "workId": scheduler.id(),
            "nodeId": scheduler.node(),
            "revision": scheduler.revision(),
            "planRevision": scheduler.plan_revision,
            "purpose": scheduler.order().unwrap().goal.clone(),
        });
        assert_eq!(progress["workId"], "work_1_r2");
        assert_eq!(progress["nodeId"], "work_1");
        assert_eq!(progress["revision"], 2);
        assert_eq!(progress["planRevision"], 2);
    }
}
