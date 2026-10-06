//! Opt-in snapshots of the finalized model request, separate from the ordinary event stream.
use std::{path::{Path, PathBuf}, time::{Instant, SystemTime, UNIX_EPOCH}};
use anyhow::{Context, Result, ensure};
use axum::{Json, extract::{Path as RoutePath, Query, State}, http::StatusCode, response::{IntoResponse, Response}};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use crate::agent_service::AgentServiceState;

fn now() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64 }

fn connection(root: &Path) -> Result<Connection> {
    let conn = crate::agent_service::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_context_debug(task_id TEXT PRIMARY KEY, enabled INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS agent_request_contexts(
            id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,
            actor TEXT NOT NULL, turn INTEGER NOT NULL, step INTEGER NOT NULL, node_id TEXT NOT NULL,
            metadata TEXT NOT NULL, request_json TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_request_context_task ON agent_request_contexts(task_id,id);")?;
    Ok(conn)
}

fn exists(conn: &Connection, task_id: &str) -> Result<()> {
    ensure!(conn.query_row("SELECT 1 FROM agent_tasks WHERE id=?1", [task_id], |_| Ok(())).optional()?.is_some(), "task not found");
    Ok(())
}

pub async fn setting(State(state): State<AgentServiceState>, RoutePath(task_id): RoutePath<String>) -> Response {
    let root = state.workspace.root().to_path_buf();
    respond(tokio::task::spawn_blocking(move || -> Result<Value> {
        let conn = connection(&root)?; exists(&conn, &task_id)?;
        let enabled = conn.query_row("SELECT enabled FROM agent_context_debug WHERE task_id=?1", [&task_id], |row| row.get::<_,bool>(0)).optional()?.unwrap_or(false);
        Ok(json!({"enabled":enabled}))
    }).await)
}

#[derive(Deserialize)]
pub struct DebugSetting { enabled: bool }

pub async fn set_setting(State(state): State<AgentServiceState>, RoutePath(task_id): RoutePath<String>, Json(input): Json<DebugSetting>) -> Response {
    let root = state.workspace.root().to_path_buf();
    respond(tokio::task::spawn_blocking(move || -> Result<Value> {
        let conn = connection(&root)?; exists(&conn, &task_id)?;
        conn.execute("INSERT INTO agent_context_debug(task_id,enabled) VALUES (?1,?2) ON CONFLICT(task_id) DO UPDATE SET enabled=excluded.enabled", params![task_id,input.enabled])?;
        Ok(json!({"enabled":input.enabled}))
    }).await)
}

#[derive(Deserialize)]
pub struct ListQuery { before: Option<i64> }

pub async fn list(State(state): State<AgentServiceState>, RoutePath(task_id): RoutePath<String>, Query(query): Query<ListQuery>) -> Response {
    let root = state.workspace.root().to_path_buf();
    respond(tokio::task::spawn_blocking(move || -> Result<Value> {
        let conn = connection(&root)?; exists(&conn, &task_id)?;
        let mut stmt = conn.prepare("SELECT id,metadata FROM agent_request_contexts WHERE task_id=?1 AND id<?2 ORDER BY id DESC LIMIT 201")?;
        let raw = stmt.query_map(params![task_id,query.before.unwrap_or(i64::MAX)], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = raw.len() > 200;
        let mut items = Vec::new();
        for (id, text) in raw.into_iter().take(200) { let mut item: Value = serde_json::from_str(&text)?; item["id"] = json!(id); items.push(item); }
        Ok(json!({"items":items,"has_more":has_more}))
    }).await)
}

pub async fn get(State(state): State<AgentServiceState>, RoutePath((task_id,id)): RoutePath<(String,i64)>) -> Response {
    let root = state.workspace.root().to_path_buf();
    respond(tokio::task::spawn_blocking(move || -> Result<Value> {
        let conn = connection(&root)?;
        let (meta,body): (String,String) = conn.query_row("SELECT metadata,request_json FROM agent_request_contexts WHERE task_id=?1 AND id=?2", params![task_id,id], |row| Ok((row.get(0)?,row.get(1)?))).optional()?.context("request context not found")?;
        let mut metadata: Value = serde_json::from_str(&meta)?;
        metadata["id"] = json!(id);
        Ok(json!({"metadata":metadata,"body":serde_json::from_str::<Value>(&body)?}))
    }).await)
}

fn respond(result: std::result::Result<Result<Value>, tokio::task::JoinError>) -> Response {
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) if error.to_string().ends_with("not found") => (StatusCode::NOT_FOUND,Json(json!({"error":error.to_string()}))).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":"could not access request context log"}))).into_response(),
    }
}

/// A dropped in-flight request (cancellation or timeout) remains distinguishable from a completed response.
pub struct Trace { root: PathBuf, task_id: String, id: i64, started: Instant, metadata: Value, finished: bool }

impl Trace {
    pub async fn finish(mut self, status: &str, extra: Value) {
        self.finished = true;
        self.metadata["status"] = json!(status);
        self.metadata["elapsed_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.metadata["outcome"] = extra;
        save_outcome(self.root.clone(), self.task_id.clone(), self.id, self.metadata.clone()).await;
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        if self.finished { return; }
        self.metadata["status"] = json!("interrupted");
        self.metadata["elapsed_ms"] = json!(self.started.elapsed().as_millis() as u64);
        let (root,task_id,id,metadata) = (self.root.clone(),self.task_id.clone(),self.id,self.metadata.clone());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(save_outcome(root,task_id,id,metadata));
        }
    }
}

async fn save_outcome(root: PathBuf, task_id: String, id: i64, metadata: Value) {
    let db_root = root.clone(); let text = metadata.to_string();
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        connection(&db_root)?.execute("UPDATE agent_request_contexts SET metadata=?1 WHERE id=?2", params![text,id])?;
        Ok(())
    }).await;
    if !matches!(result,Ok(Ok(()))) { tracing::warn!(%task_id,id,"could not save model request outcome"); return; }
    if let Err(error) = crate::agent_service::emit(&root,&task_id,"debug/context_end",json!({"id":id})).await {
        tracing::warn!(%task_id,%error,"could not publish context outcome");
    }
}

/// Call after all request-body mutations and immediately before the HTTP send.
/// Headers, provider URL and bearer credentials are deliberately never captured.
pub async fn record(root: &Path, task_id: &str, mut metadata: Value, body: &Value) -> Option<Trace> {
    let root = root.to_path_buf(); let task_id = task_id.to_owned();
    let db_root = root.clone(); let db_id = task_id.clone();
    let started = Instant::now();
    // Check opt-in before cloning/serializing the potentially large body.
    let enabled = tokio::task::spawn_blocking(move || -> Result<bool> {
        Ok(connection(&db_root)?.query_row("SELECT enabled FROM agent_context_debug WHERE task_id=?1", [&db_id], |row| row.get::<_,bool>(0)).optional()?.unwrap_or(false))
    }).await;
    match enabled {
        Ok(Ok(false)) => return None,
        Ok(Ok(true)) => {},
        _ => { tracing::warn!(%task_id,"could not check request context debug setting"); return None; }
    }
    let body_text = body.to_string();
    metadata["request_bytes"] = json!(body_text.len());
    metadata["model"] = body["model"].clone();
    metadata["created_at"] = json!(now());
    metadata["status"] = json!("pending");
    metadata["tools"] = json!(body["tools"].as_array().into_iter().flatten().filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str)).collect::<Vec<_>>());
    let messages = body["messages"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    metadata["message_count"] = json!(messages.len());
    metadata["role_counts"] = json!(["system","user","assistant","tool"].into_iter()
        .map(|role| (role,messages.iter().filter(|message|message["role"] == role).count())).collect::<std::collections::BTreeMap<_,_>>());
    let db_root = root.clone(); let db_id = task_id.clone(); let db_meta = metadata.clone();
    let inserted = tokio::task::spawn_blocking(move || -> Result<i64> {
        let conn = connection(&db_root)?;
        conn.execute("INSERT INTO agent_request_contexts(task_id,actor,turn,step,node_id,metadata,request_json) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![db_id,
            db_meta["actor"].as_str().unwrap_or("worker"),db_meta["turn"].as_u64().unwrap_or(0),db_meta["step"].as_u64().unwrap_or(0),
            db_meta["nodeId"].as_str().unwrap_or(""),db_meta.to_string(),body_text])?;
        Ok(conn.last_insert_rowid())
    }).await;
    match inserted {
        Ok(Ok(id)) => {
            let trace = Trace { root:root.clone(),task_id:task_id.clone(),id,started,metadata:metadata.clone(),finished:false };
            if let Err(error) = crate::agent_service::emit(&root,&task_id,"debug/context_request",json!({"id":id,"turn":metadata["turn"],"step":metadata["step"],"nodeId":metadata["nodeId"],"actor":metadata["actor"]})).await {
                tracing::warn!(%task_id,%error,"could not publish context index");
            }
            Some(trace)
        },
        _ => { tracing::warn!(%task_id,"could not record finalized model request"); None }
    }
}

pub async fn finish(trace: Option<Trace>, status: &str, outcome: Value) {
    if let Some(trace) = trace { trace.finish(status,outcome).await; }
}
