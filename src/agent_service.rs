//! Standalone task API for the local Rust agent. Events are append-only so a
//! client can recover a task's trajectory after reconnecting.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures::stream;
use reqwest::Client;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_STEPS: usize = 30;
const MAX_STEP_LIMIT: usize = 100;
const MAX_TOOL_OUTPUT_CHARS: usize = 300_000;

#[derive(Clone)]
pub struct AgentServiceState {
    pub workspace: Arc<crate::tools::Workspace>,
    pub client: Client,
    pub provider_url: String,
    pub api_key: String,
    pub default_model: String,
    pub model_map: HashMap<String, String>,
    pub cancellations: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_steps: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ListTasksQuery {
    #[serde(default)]
    pub limit: Option<usize>,
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

fn open_db(root: &std::path::Path) -> anyhow::Result<rusqlite::Connection> {
    let conn = crate::database::init_db(root)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_tasks (
            id TEXT PRIMARY KEY, prompt TEXT NOT NULL, model TEXT NOT NULL,
            status TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS agent_task_events (
            task_id TEXT NOT NULL, seq INTEGER NOT NULL, timestamp INTEGER NOT NULL,
            kind TEXT NOT NULL, data TEXT NOT NULL, surface_op TEXT, PRIMARY KEY(task_id, seq),
            FOREIGN KEY(task_id) REFERENCES agent_tasks(id) ON DELETE CASCADE
         );",
    )?;
    let _ = conn.execute(
        "ALTER TABLE agent_task_events ADD COLUMN surface_op TEXT",
        [],
    );
    Ok(conn)
}

pub async fn recover_orphaned_tasks(workspace_root: &std::path::Path) -> anyhow::Result<()> {
    let root = workspace_root.to_path_buf();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let conn = open_db(&root)?;
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
            let seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM agent_task_events WHERE task_id=?1",
                [&task_id],
                |row| row.get(0),
            )?;
            conn.execute(
                "INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES (?1,?2,?3,'turn/end',?4)",
                params![task_id,seq,now(),json!({"turn":1,"reason":{"kind":"interrupted"}}).to_string()],
            )?;
            conn.execute(
                "UPDATE agent_tasks SET status='interrupted',updated_at=?2 WHERE id=?1",
                params![task_id, now()],
            )?;
        }
        Ok(())
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
    let conn = open_db(root)?;
    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM agent_task_events WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data,surface_op) VALUES (?1,?2,?3,?4,?5,?6)",
        params![task_id, seq, now(), kind, serde_json::to_string(&data)?, surface_op],
    )?;
    conn.execute(
        "UPDATE agent_tasks SET updated_at=?2 WHERE id=?1",
        params![task_id, now()],
    )?;
    Ok(())
}

pub async fn create_task(
    State(state): State<AgentServiceState>,
    Json(request): Json<CreateTaskRequest>,
) -> impl IntoResponse {
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
    let model = request
        .model
        .and_then(|requested| state.model_map.get(&requested).cloned().or(Some(requested)))
        .unwrap_or_else(|| state.default_model.clone());
    let task_id = format!("task_{}", uuid_like());
    let root = state.workspace.root().to_path_buf();
    let insert = tokio::task::spawn_blocking({
        let task_id = task_id.clone(); let prompt = prompt.clone(); let model = model.clone();
        let root = root.clone(); move || -> anyhow::Result<()> {
            let conn = open_db(&root)?;
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,?2,?3,'running',?4,?4)", params![task_id,prompt,model,now()])?;
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
    let cancel = CancellationToken::new();
    state
        .cancellations
        .lock()
        .await
        .insert(task_id.clone(), cancel.clone());
    let runner_state = state.clone();
    let runner_id = task_id.clone();
    let max_steps = request
        .max_steps
        .unwrap_or(DEFAULT_MAX_STEPS)
        .clamp(1, MAX_STEP_LIMIT);
    tokio::spawn(async move {
        run_task(
            runner_state.clone(),
            runner_id.clone(),
            model,
            prompt,
            max_steps,
            cancel,
        )
        .await;
        runner_state.cancellations.lock().await.remove(&runner_id);
    });
    (StatusCode::ACCEPTED, Json(json!({"task_id":task_id,"status":"running","events_url":format!("/agent/tasks/{task_id}/events")}))).into_response()
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
    headers: HeaderMap,
) -> impl IntoResponse {
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|seq| *seq >= 0)
        .unwrap_or(-1);
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
        Ok(conn.query_row("SELECT id,prompt,model,status,created_at,updated_at FROM agent_tasks WHERE id=?1", [&task_id], |r| Ok(json!({"task_id":r.get::<_,String>(0)?,"prompt":r.get::<_,String>(1)?,"model":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"created_at":r.get::<_,i64>(4)?,"updated_at":r.get::<_,i64>(5)?}))).optional()?)
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

pub async fn list_tasks(
    State(state): State<AgentServiceState>,
    Query(query): Query<ListTasksQuery>,
) -> impl IntoResponse {
    let root = state.workspace.root().to_path_buf();
    let limit = query.limit.unwrap_or(50).clamp(1, 100) as i64;
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = open_db(&root)?;
        let mut stmt = conn.prepare("SELECT id,prompt,model,status,created_at,updated_at FROM agent_tasks ORDER BY created_at DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit], |r| Ok(json!({"task_id":r.get::<_,String>(0)?,"prompt":r.get::<_,String>(1)?,"model":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"created_at":r.get::<_,i64>(4)?,"updated_at":r.get::<_,i64>(5)?})))?;
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
    if let Some(cancel) = state.cancellations.lock().await.get(&task_id).cloned() {
        cancel.cancel();
        return Json(json!({"task_id":task_id,"status":"cancelling"})).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error":"task is not running"})),
    )
        .into_response()
}

fn uuid_like() -> String {
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

async fn emit(root: &std::path::Path, task_id: &str, kind: &str, data: Value) {
    emit_surface(root, task_id, kind, data, None).await;
}

async fn emit_surface(
    root: &std::path::Path,
    task_id: &str,
    kind: &str,
    data: Value,
    surface_op: Option<&'static str>,
) {
    let root = root.to_path_buf();
    let task_id = task_id.to_owned();
    let kind = kind.to_owned();
    let _ =
        tokio::task::spawn_blocking(move || append_event(&root, &task_id, &kind, data, surface_op))
            .await;
}

async fn finish(state: &AgentServiceState, task_id: &str, status: &str) {
    let root = state.workspace.root().to_path_buf();
    let id = task_id.to_owned();
    let status = status.to_owned();
    let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let conn = open_db(&root)?;
        conn.execute(
            "UPDATE agent_tasks SET status=?2,updated_at=?3 WHERE id=?1",
            params![id, status, now()],
        )?;
        Ok(())
    })
    .await;
}

async fn run_task(
    state: AgentServiceState,
    task_id: String,
    model: String,
    prompt: String,
    max_steps: usize,
    cancel: CancellationToken,
) {
    let root = state.workspace.root().to_path_buf();
    let turn = 1;
    emit(&root, &task_id, "turn/start", json!({"turn":turn})).await;
    emit_surface(&root, &task_id, "user/message", json!({"turn":turn,"message":{"id":uuid_like(),"role":"user","content":[{"type":"text","text":prompt}],"source":{"kind":"user"}}}), Some("append")).await;
    let mut messages = vec![
        json!({"role":"system","content":format!("You are a local coding agent. Work only in workspace {}. Use tools as needed and provide a concise final answer.", state.workspace.root().display())}),
        json!({"role":"user","content":prompt}),
    ];
    let tools = local_tools(&state.workspace);
    for step in 1..=max_steps {
        if cancel.is_cancelled() {
            emit(
                &root,
                &task_id,
                "turn/end",
                json!({"turn":turn,"reason":{"kind":"aborted","reason":{"kind":"legacy"}}}),
            )
            .await;
            finish(&state, &task_id, "cancelled").await;
            return;
        }
        emit(
            &root,
            &task_id,
            "step/start",
            json!({"turn":turn,"step":step}),
        )
        .await;
        let body = json!({"model":model,"messages":messages,"tools":tools,"tool_choice":"auto","stream":false});
        let response = tokio::select! {
            _ = cancel.cancelled() => {
                emit(&root,&task_id,"step/end",json!({"turn":turn,"step":step})).await;
                emit(&root,&task_id,"turn/end",json!({"turn":turn,"reason":{"kind":"aborted","reason":{"kind":"legacy"}}})).await;
                finish(&state,&task_id,"cancelled").await;
                return;
            }
            result = state.client.post(format!(
                "{}/chat/completions",
                state.provider_url.trim_end_matches('/')
            ))
            .bearer_auth(&state.api_key)
            .json(&body)
            .send() => result,
        };
        let value = match response {
            Ok(response) if response.status().is_success() => {
                match response.json::<Value>().await {
                    Ok(v) => v,
                    Err(e) => {
                        fail_turn(&state, &root, &task_id, turn, step, e.to_string()).await;
                        return;
                    }
                }
            }
            Ok(response) => {
                let status = response.status();
                let message = response.text().await.unwrap_or_default();
                fail_turn(
                    &state,
                    &root,
                    &task_id,
                    turn,
                    step,
                    format!("model returned {status}: {message}"),
                )
                .await;
                return;
            }
            Err(e) => {
                fail_turn(&state, &root, &task_id, turn, step, e.to_string()).await;
                return;
            }
        };
        let Some(message) = value.pointer("/choices/0/message").cloned() else {
            fail_turn(
                &state,
                &root,
                &task_id,
                turn,
                step,
                "model response has no choices[0].message".into(),
            )
            .await;
            return;
        };
        let text = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let calls = crate::format_translate::collect_all_tool_calls_from_openai_chat(&message);
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(json!({"type":"text","text":text}));
        }
        for call in &calls {
            content.push(json!({"type":"tool-call","id":call.call_id,"name":call.name,"arguments":call.arguments}));
        }
        emit_surface(&root,&task_id,"assistant/message",json!({"turn":turn,"step":step,"message":{"id":uuid_like(),"role":"assistant","content":content,"source":{"kind":"model","provider":state.provider_url,"model":model}},"stream":[]}),Some("append")).await;
        messages.push(message);
        if calls.is_empty() {
            emit(
                &root,
                &task_id,
                "step/end",
                json!({"turn":turn,"step":step}),
            )
            .await;
            emit(
                &root,
                &task_id,
                "turn/end",
                json!({"turn":turn,"reason":{"kind":"completed"}}),
            )
            .await;
            finish(&state, &task_id, "completed").await;
            return;
        }
        for call in calls {
            let args: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
            emit(&root,&task_id,"tool/call",json!({"turn":turn,"step":step,"callId":call.call_id,"name":call.name,"arguments":call.arguments})).await;
            let started = std::time::Instant::now();
            let output = tokio::select! {
                _ = cancel.cancelled() => {
                    let cancelled_message = json!({"id":uuid_like(),"role":"tool","source":{"kind":"tool","callId":call.call_id},"toolCallId":call.call_id,"content":[{"type":"text","text":"Tool execution cancelled."}],"isError":true});
                    emit_surface(&root,&task_id,"tool/result",json!({"turn":turn,"step":step,"message":cancelled_message}),Some("append")).await;
                    emit(&root,&task_id,"step/end",json!({"turn":turn,"step":step})).await;
                    emit(&root,&task_id,"turn/end",json!({"turn":turn,"reason":{"kind":"aborted","reason":{"kind":"legacy"}}})).await;
                    finish(&state,&task_id,"cancelled").await;
                    return;
                }
                result = execute_tool(&state.workspace, &call.name, args) => result,
            };
            let (result, is_error) = match output {
                Ok(v) => (v, false),
                Err(e) => (json!({"error":e.to_string()}), true),
            };
            let content = serde_json::to_string(&result).unwrap_or_else(|_| "null".into());
            let bounded: String = content.chars().take(MAX_TOOL_OUTPUT_CHARS).collect();
            let result_message = json!({"id":uuid_like(),"role":"tool","source":{"kind":"tool","callId":call.call_id},"toolCallId":call.call_id,"content":[{"type":"text","text":bounded}],"isError":is_error});
            emit_surface(&root,&task_id,"tool/result",json!({"turn":turn,"step":step,"message":result_message,"meta":{"durationMs":started.elapsed().as_millis(),"result":result}}),Some("append")).await;
            messages.push(crate::format_translate::openai_chat_tool_result_message(
                &call, &bounded,
            ));
        }
        emit(
            &root,
            &task_id,
            "step/end",
            json!({"turn":turn,"step":step}),
        )
        .await;
    }
    emit(&root,&task_id,"turn/end",json!({"turn":turn,"reason":{"kind":"error","error":{"message":format!("reached max step limit ({max_steps})"),"code":"MAX_STEPS"}}})).await;
    finish(&state, &task_id, "max_steps").await;
}

async fn fail_turn(
    state: &AgentServiceState,
    root: &std::path::Path,
    task_id: &str,
    turn: usize,
    step: usize,
    message: String,
) {
    emit(root, task_id, "step/end", json!({"turn":turn,"step":step})).await;
    emit(
        root,
        task_id,
        "turn/end",
        json!({"turn":turn,"reason":{"kind":"error","error":{"message":message,"code":"UNKNOWN"}}}),
    )
    .await;
    finish(state, task_id, "failed").await;
}

fn local_tools(_workspace: &crate::tools::Workspace) -> Vec<Value> {
    let definitions = crate::mcp::tool_definitions();
    let allowed = [
        "workspace_info",
        "list_dir",
        "read_file",
        "read_file_lines",
        "search_text",
        "write_file",
        "replace_range",
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
    definitions.as_array().into_iter().flatten().filter(|tool| tool.get("name").and_then(Value::as_str).is_some_and(|n| allowed.contains(&n))).map(|tool| {
        let name=tool["name"].as_str().unwrap_or_default(); let description=tool["description"].as_str().unwrap_or_default();
        let mut schema=tool.get("inputSchema").cloned().unwrap_or_else(||json!({"type":"object","properties":{}}));
        if let Some(props)=schema.get_mut("properties").and_then(Value::as_object_mut) {props.remove("workspace_root");}
        if let Some(required)=schema.get_mut("required").and_then(Value::as_array_mut) {required.retain(|name| name.as_str()!=Some("workspace_root"));}
        json!({"type":"function","function":{"name":name,"description":description,"parameters":schema}})
    }).chain(std::iter::once(json!({"type":"function","function":{"name":"run_command","description":"Run a shell command inside the workspace. The command is stopped after its timeout.","parameters":{"type":"object","required":["command"],"properties":{"command":{"type":"string"},"timeout_seconds":{"type":"integer","default":30}}}}}))).collect()
}

async fn execute_tool(
    workspace: &crate::tools::Workspace,
    name: &str,
    mut args: Value,
) -> anyhow::Result<Value> {
    if name == "run_command" {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("run_command requires command"))?;
        let timeout = args
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .clamp(1, 120);
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(workspace.root())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(Duration::from_secs(timeout), child)
            .await
            .map_err(|_| anyhow::anyhow!("command timed out after {timeout}s"))??;
        return Ok(
            json!({"status":output.status.code(),"stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)}),
        );
    }
    if let Some(object) = args.as_object_mut() {
        object.insert(
            "workspace_root".into(),
            json!(workspace.root().display().to_string()),
        );
    }
    let response = crate::mcp::call_tool(workspace, json!({"name":name,"arguments":args})).await?;
    Ok(response
        .get("structuredContent")
        .cloned()
        .unwrap_or(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, routing::post};

    async fn fake_chat(State(calls): State<Arc<std::sync::atomic::AtomicUsize>>) -> Json<Value> {
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
            default_model: "fake-model".into(),
            model_map: HashMap::new(),
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
            "inspect the project".into(),
            4,
            CancellationToken::new(),
        )
        .await;

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
        assert_eq!(types.get(1).map(String::as_str), Some("user/message"));
        let first_tool = types.iter().position(|t| t == "tool/call").unwrap();
        assert_eq!(
            &types[first_tool..first_tool + 4],
            ["tool/call", "tool/result", "tool/call", "tool/result"]
        );
        assert_eq!(types.last().map(String::as_str), Some("turn/end"));

        let user_event: Value = conn.query_row(
            "SELECT json_object('type',kind,'seq',seq,'time',timestamp,'data',json(data),'surfaceOp',surface_op) FROM agent_task_events WHERE task_id=?1 AND kind='user/message'",
            [&task_id], |r| r.get(0),
        ).map(|raw: String| serde_json::from_str(&raw).unwrap()).unwrap();
        assert_eq!(user_event["type"], "user/message");
        assert_eq!(user_event["seq"], 1);
        assert_eq!(user_event["data"]["message"]["role"], "user");
        assert_eq!(user_event["surfaceOp"], "append");

        let (status, surface_op): (String, Option<String>) = conn.query_row("SELECT status,(SELECT surface_op FROM agent_task_events WHERE task_id=?1 AND kind='assistant/message' ORDER BY seq DESC LIMIT 1) FROM agent_tasks WHERE id=?1", [&task_id], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(status, "completed");
        assert_eq!(surface_op.as_deref(), Some("append"));
        server.abort();
        let _ = std::fs::remove_dir_all(root);
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
