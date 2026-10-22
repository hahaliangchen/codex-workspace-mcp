//! Durable snapshots of each role's finalized model input. Conversation events
//! reference these views of the shared history; a view is not another action.
use crate::agent_service::AgentServiceState;
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Path as RoutePath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn connection(root: &Path) -> Result<Connection> {
    let conn = crate::agent_service::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_context_debug(task_id TEXT PRIMARY KEY, enabled INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS agent_request_contexts(
            id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,
            actor TEXT NOT NULL, turn INTEGER NOT NULL, step INTEGER NOT NULL, node_id TEXT NOT NULL,
            metadata TEXT NOT NULL, request_json TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_request_context_task ON agent_request_contexts(task_id,id);")?;
    for (column, definition) in [("recording", "INTEGER NOT NULL DEFAULT 1"), ("retention_days", "INTEGER NOT NULL DEFAULT 7")] {
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_context_debug') WHERE name=?1)", [column], |row| row.get(0))?;
        if !exists {
            let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
            let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_context_debug') WHERE name=?1)", [column], |row| row.get(0))?;
            if !exists { tx.execute(&format!("ALTER TABLE agent_context_debug ADD COLUMN {column} {definition}"), [])?; }
            tx.commit()?;
        }
    }
    Ok(conn)
}

fn policy(conn: &Connection, task: &str) -> Result<Value> {
    let current_schema: bool = conn.query_row("SELECT COUNT(*)=2 FROM pragma_table_info('agent_context_debug') WHERE name IN ('recording','retention_days')", [], |row|row.get(0))?;
    if !current_schema { return Ok(json!({"enabled":true,"recording":true,"retention_days":7})); }
    let (enabled, recording, days) = conn.query_row("SELECT enabled,recording,retention_days FROM agent_context_debug WHERE task_id=?1", [task],
        |row| Ok((row.get::<_,bool>(0)?,row.get::<_,bool>(1)?,row.get::<_,u32>(2)?))).optional()?.unwrap_or((true,true,7));
    Ok(json!({"enabled":enabled,"recording":recording,"retention_days":days}))
}

fn expire_bodies(tx: &Connection, task: &str, days: u64, at: i64) -> Result<()> {
    let cutoff = at.saturating_sub(days.saturating_mul(86_400_000).min(i64::MAX as u64) as i64);
    tx.execute("UPDATE agent_request_contexts SET request_json='null',metadata=json_set(metadata,'$.body_recorded',json('false'),'$.body_expired',json('true'))
        WHERE task_id=?1 AND request_json<>'null' AND COALESCE(json_extract(metadata,'$.created_at'),0)<?2 AND json_extract(metadata,'$.status')<>'pending'",
        params![task,cutoff])?;
    Ok(())
}

fn exists(conn: &Connection, task_id: &str) -> Result<()> {
    ensure!(
        conn.query_row("SELECT 1 FROM agent_tasks WHERE id=?1", [task_id], |_| Ok(
            ()
        ))
        .optional()?
        .is_some(),
        "task not found"
    );
    Ok(())
}

pub async fn setting(
    State(state): State<AgentServiceState>,
    RoutePath(task_id): RoutePath<String>,
) -> Response {
    let root = state.workspace.root().to_path_buf();
    respond(
        tokio::task::spawn_blocking(move || -> Result<Value> {
            let conn = connection(&root)?;
            exists(&conn, &task_id)?;
            policy(&conn, &task_id)
        })
        .await,
    )
}

#[derive(Deserialize)]
pub struct DebugSetting {
    enabled: Option<bool>,
    recording: Option<bool>,
    retention_days: Option<u32>,
}

// The legacy context-debug endpoint now controls panel visibility only.
// Panel visibility and full-body retention are independent settings.
pub async fn set_setting(
    State(state): State<AgentServiceState>,
    RoutePath(task_id): RoutePath<String>,
    Json(input): Json<DebugSetting>,
) -> Response {
    if input.retention_days.is_some_and(|days| !(1..=365).contains(&days)) {
        return (StatusCode::BAD_REQUEST,Json(json!({"error":"retention_days must be between 1 and 365"}))).into_response();
    }
    let root = state.workspace.root().to_path_buf();
    respond(tokio::task::spawn_blocking(move || -> Result<Value> {
        let mut conn = connection(&root)?; exists(&conn, &task_id)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let old = policy(&tx, &task_id)?;
        tx.execute("INSERT INTO agent_context_debug(task_id,enabled,recording,retention_days) VALUES (?1,?2,?3,?4) ON CONFLICT(task_id) DO UPDATE SET enabled=excluded.enabled,recording=excluded.recording,retention_days=excluded.retention_days",
            params![task_id,input.enabled.unwrap_or(old["enabled"].as_bool().unwrap()),input.recording.unwrap_or(old["recording"].as_bool().unwrap()),input.retention_days.unwrap_or(old["retention_days"].as_u64().unwrap() as u32)])?;
        let result = policy(&tx, &task_id)?;
        expire_bodies(&tx, &task_id, result["retention_days"].as_u64().unwrap(), now())?;
        tx.commit()?;
        Ok(result)
    }).await)
}

#[derive(Deserialize)]
pub struct ListQuery {
    before: Option<i64>,
}

pub async fn list(
    State(state): State<AgentServiceState>,
    RoutePath(task_id): RoutePath<String>,
    Query(query): Query<ListQuery>,
) -> Response {
    let root = state.workspace.root().to_path_buf();
    respond(tokio::task::spawn_blocking(move || -> Result<Value> {
        let conn = connection(&root)?; exists(&conn, &task_id)?;
        expire_bodies(&conn, &task_id, policy(&conn, &task_id)?["retention_days"].as_u64().unwrap(), now())?;
        let mut stmt = conn.prepare("SELECT id,metadata FROM agent_request_contexts WHERE task_id=?1 AND id<?2 ORDER BY id DESC LIMIT 201")?;
        let raw = stmt.query_map(params![task_id,query.before.unwrap_or(i64::MAX)], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = raw.len() > 200;
        let mut items = Vec::new();
        for (id, text) in raw.into_iter().take(200) { let mut item: Value = serde_json::from_str(&text)?; item["id"] = json!(id); items.push(item); }
        Ok(json!({"items":items,"has_more":has_more}))
    }).await)
}

pub async fn get(
    State(state): State<AgentServiceState>,
    RoutePath((task_id, id)): RoutePath<(String, i64)>,
) -> Response {
    let root = state.workspace.root().to_path_buf();
    respond(
        tokio::task::spawn_blocking(move || -> Result<Value> {
            let conn = connection(&root)?;
            expire_bodies(&conn, &task_id, policy(&conn, &task_id)?["retention_days"].as_u64().unwrap(), now())?;
            saved_context(&conn, &task_id, id)?.context("request context not found")
        })
        .await,
    )
}

/// Resolve a conversation's input-view reference without duplicating the body
/// in the event stream. Older events may refer to already deleted snapshots.
pub fn saved_context(conn: &Connection, task_id: &str, id: i64) -> Result<Option<Value>> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_request_contexts')",[],|row|row.get(0))?;
    if !exists {
        return Ok(None);
    }
    let days = policy(conn, task_id)?["retention_days"].as_u64().unwrap();
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT metadata,request_json FROM agent_request_contexts WHERE task_id=?1 AND id=?2",
            params![task_id, id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(meta,body)| {
        let mut metadata:Value=serde_json::from_str(&meta)?;metadata["id"]=json!(id);
        let expired = metadata["status"]!="pending" && metadata["created_at"].as_i64().unwrap_or(0) < now().saturating_sub((days*86_400_000) as i64);
        let body = if expired {
            metadata["body_expired"] = json!(true);
            metadata["body_recorded"] = json!(false);
            Value::Null
        } else { serde_json::from_str::<Value>(&body)? };
        Ok(json!({"record_kind":"model_input","metadata":metadata,"body":body}))
    }).transpose()
}

fn respond(result: std::result::Result<Result<Value>, tokio::task::JoinError>) -> Response {
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) if error.to_string().ends_with("not found") => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not access request context log"})),
        )
            .into_response(),
    }
}

/// A dropped in-flight request (cancellation or timeout) remains distinguishable from a completed response.
pub struct Trace {
    root: PathBuf,
    task_id: String,
    id: i64,
    started: Instant,
    metadata: Value,
    finished: bool,
}

impl Trace {
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Reference to the durable context-request event created for this exact input.
    pub fn history_reference(&self) -> Value {
        self.metadata["history_reference"].clone()
    }

    /// New execution loops require the response and node outcome to be durable
    /// before acting on any returned tool calls.
    pub async fn finish_checked(mut self, status: &str, extra: Value) -> Result<()> {
        self.metadata["status"] = json!(status);
        self.metadata["elapsed_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.metadata["outcome"] = extra;
        persist_outcome(
            self.root.clone(),
            self.task_id.clone(),
            self.id,
            self.metadata.clone(),
        )
        .await?;
        self.finished = true;
        Ok(())
    }

    pub async fn finish(mut self, status: &str, extra: Value) {
        self.finished = true;
        self.metadata["status"] = json!(status);
        self.metadata["elapsed_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.metadata["outcome"] = extra;
        save_outcome(
            self.root.clone(),
            self.task_id.clone(),
            self.id,
            self.metadata.clone(),
        )
        .await;
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.metadata["status"] = json!("interrupted");
        self.metadata["elapsed_ms"] = json!(self.started.elapsed().as_millis() as u64);
        let (root, task_id, id, metadata) = (
            self.root.clone(),
            self.task_id.clone(),
            self.id,
            self.metadata.clone(),
        );
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(save_outcome(root, task_id, id, metadata));
        }
    }
}

async fn persist_outcome(root: PathBuf, task_id: String, id: i64, metadata: Value) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = connection(&root)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(tx.execute("UPDATE agent_request_contexts SET metadata=?1 WHERE id=?2 AND task_id=?3",
            params![metadata.to_string(),id,task_id])? == 1, "request context disappeared");
        crate::agent_service::append_event_tx(&tx,&task_id,"debug/context_end",&json!({"id":id,"task_node_id":metadata["task_node_id"]}),None)?;
        if metadata["request_flow"] == true {
            let status = match metadata["status"].as_str() {
                Some("completed") => "done",
                Some("cancelled" | "interrupted") => "interrupted",
                _ => "failed",
            };
            crate::agent_service::append_event_tx(&tx,&task_id,"flow/node_state",
                &json!({"turn":metadata["turn"],"id":metadata["nodeId"],"status":status,"context_id":id,"task_node_id":metadata["task_node_id"]}),None)?;
        }
        tx.commit()?;
        Ok(())
    }).await?
}
async fn save_outcome(root: PathBuf, task_id: String, id: i64, metadata: Value) {
    if let Err(error) = persist_outcome(root, task_id.clone(), id, metadata).await {
        tracing::warn!(%task_id,id,%error,"could not save model request outcome");
    }
}

/// Startup recovery closes requests whose process died before saving an outcome.
/// Original input is retained; no response is fabricated.
pub(crate) fn recover_interrupted(conn: &mut Connection, task: &str) -> Result<()> {
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_request_contexts')",[],|r|r.get(0))?;
    if !exists {
        return Ok(());
    }
    let pending = conn.prepare("SELECT id,metadata FROM agent_request_contexts WHERE task_id=?1 AND json_extract(metadata,'$.status')='pending'")?
        .query_map([task],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (id, raw) in pending {
        let mut metadata: Value = serde_json::from_str(&raw)?;
        metadata["status"] = json!("interrupted");
        metadata["outcome"] = json!({"error":"Host restarted before a complete model response was saved; delivery and completion are unknown."});
        tx.execute(
            "UPDATE agent_request_contexts SET metadata=?1 WHERE id=?2",
            params![metadata.to_string(), id],
        )?;
        crate::agent_service::append_event_tx(
            &tx,
            task,
            "debug/context_end",
            &json!({"id":id,"task_node_id":metadata["task_node_id"]}),
            None,
        )?;
        if metadata["request_flow"] == true {
            crate::agent_service::append_event_tx(
                &tx,
                task,
                "flow/node_state",
                &json!({"turn":metadata["turn"],"id":metadata["nodeId"],"status":"interrupted","context_id":id,"task_node_id":metadata["task_node_id"]}),
                None,
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Call after all request-body mutations and immediately before the HTTP send.
/// Headers, provider URL and bearer credentials are deliberately never captured.
pub async fn record(
    root: &Path,
    task_id: &str,
    mut metadata: Value,
    body: &Value,
) -> Option<Trace> {
    let root = root.to_path_buf();
    let task_id = task_id.to_owned();
    let started = Instant::now();
    let body_text = body.to_string();
    metadata["request_bytes"] = json!(body_text.len());
    metadata["model"] = body["model"].clone();
    metadata["created_at"] = json!(now());
    metadata["status"] = json!("pending");
    metadata["record_kind"] = json!("model_input");
    metadata["context_access"] = match metadata["actor"].as_str() {
        Some("worker") if metadata["agent_id"].is_string() => {
            json!({"history_reader":true,"shared_task_context":true,"input_provider":"main_agent","recursive_subagents":false})
        }
        Some("worker") => {
            json!({"history_reader":false,"input_provider":"organizer","observer_direct":false})
        }
        Some("observer") => {
            json!({"history_reader":true,"purpose":"retrospective","communicates_with":[]})
        }
        Some("organizer") => json!({"history_reader":true,"provides_worker_view":true}),
        Some("agent") => json!({"history_reader":true,"shared_task_context":true}),
        Some("context_agent") => {
            json!({"history_reader":true,"purpose":"context_organization","business_execution":false,"recursive_subagents":false})
        }
        Some("asset_reviewer") => {
            json!({"history_reader":false,"purpose":"asset_placement","asset_bodies_delivered":false,"business_execution":false,"recursive_subagents":false})
        }
        _ => Value::Null,
    };
    metadata["tools"] = json!(
        body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
            .collect::<Vec<_>>()
    );
    let messages = body["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    metadata["message_count"] = json!(messages.len());
    metadata["role_counts"] = json!(
        ["system", "user", "assistant", "tool"]
            .into_iter()
            .map(|role| (
                role,
                messages
                    .iter()
                    .filter(|message| message["role"] == role)
                    .count()
            ))
            .collect::<std::collections::BTreeMap<_, _>>()
    );
    let db_root = root.clone();
    let db_id = task_id.clone();
    let db_meta = metadata.clone();
    let inserted = tokio::task::spawn_blocking(move || -> Result<(i64,Value)> {
        let mut conn = connection(&db_root)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // This cutoff describes the available pool, not a claim that every
        // earlier event was selected into the request. The exact view is body.
        let seq=crate::agent_service::next_event_seq(&tx, &db_id)?;
        let mut db_meta=db_meta;
        let settings = policy(&tx, &db_id)?;
        let recording = settings["recording"].as_bool().unwrap();
        db_meta["body_recorded"] = json!(recording);
        expire_bodies(&tx, &db_id, settings["retention_days"].as_u64().unwrap(), now())?;
        db_meta["history_pool"]=json!({"scope":"conversation","task_id":db_id,"before_seq":seq});
        db_meta["history_reference"]=json!({"scope":"conversation","task_id":db_id,"event_seq":seq});
        tx.execute("INSERT INTO agent_request_contexts(task_id,actor,turn,step,node_id,metadata,request_json) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![db_id,
            db_meta["actor"].as_str().unwrap_or("worker"),db_meta["turn"].as_u64().unwrap_or(0),db_meta["step"].as_u64().unwrap_or(0),
            db_meta["nodeId"].as_str().unwrap_or(""),db_meta.to_string(),if recording {body_text.as_str()} else {"null"}])?;
        let id=tx.last_insert_rowid();
        crate::agent_service::append_event_tx(&tx,&db_id,"debug/context_request",&json!({
            "id":id,"record_kind":"model_input","actor":db_meta["actor"],"turn":db_meta["turn"],
            "step":db_meta["step"],"nodeId":db_meta["nodeId"],"workId":db_meta["workId"],"task_node_id":db_meta["task_node_id"],
            "identity":{"task_id":db_id,"request_id":db_meta["request_id"],"node_id":db_meta["nodeId"]},
            "history_pool":db_meta["history_pool"],"history_reference":db_meta["history_reference"],
            "note":"Exact role input view; references shared history, not a new tool action or independent observation."
        }),None)?;
        if db_meta["request_flow"] == true {
            let node = &db_meta["nodeId"];
            let mut edges = Vec::new();
            if let Some(previous) = db_meta["previous_node_id"].as_str() {
                edges.push(json!({"source":previous,"target":node,"label":"next request"}));
            } else if let Some(parent) = db_meta["task_node_id"].as_str().or_else(||db_meta["parent_node_id"].as_str()) {
                let label = if db_meta["actor"] == "worker" { "task delegate" } else { "context subagent" };
                edges.push(json!({"source":parent,"target":node,"label":label}));
            }
            crate::agent_service::append_event_tx(&tx,&db_id,"flow/node_add",&json!({"turn":db_meta["turn"],
                "task_node_id":db_meta["task_node_id"],"node":{"id":node,"kind":db_meta["actor"],"title":format!("{} · {}",db_meta["actor"].as_str().unwrap_or("agent"),id),"task_node_id":db_meta["task_node_id"],
                    "status":"running","parent_id":db_meta["parent_node_id"],"agent_id":db_meta["agent_id"],"request_id":db_meta["request_id"],"context_id":id},
                "edges":edges}),None)?;
        }
        tx.commit()?;
        Ok((id,db_meta))
    }).await;
    match inserted {
        Ok(Ok((id, metadata))) => {
            let trace = Trace {
                root: root.clone(),
                task_id: task_id.clone(),
                id,
                started,
                metadata: metadata.clone(),
                finished: false,
            };
            Some(trace)
        }
        _ => {
            tracing::warn!(%task_id,"could not record finalized model request");
            None
        }
    }
}

pub async fn finish(trace: Option<Trace>, status: &str, outcome: Value) {
    if let Some(trace) = trace {
        trace.finish(status, outcome).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn body_recording_can_be_disabled_without_losing_flow_or_audit_identity() {
        let root = std::env::temp_dir().join(format!("request-policy-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let conn = connection(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)", []).unwrap();
        conn.execute("INSERT INTO agent_context_debug(task_id,enabled,recording,retention_days) VALUES ('task',1,0,1)", []).unwrap();
        let trace = record(&root,"task",json!({"actor":"agent","turn":1,"step":1,"nodeId":"request-a","request_flow":true}),
            &json!({"model":"fake","messages":[{"role":"user","content":"private full input"}]})).await.unwrap();
        let id = trace.id();
        trace.finish_checked("completed",json!({})).await.unwrap();
        let saved = saved_context(&conn,"task",id).unwrap().unwrap();
        assert!(saved["body"].is_null());
        assert_eq!(saved["metadata"]["body_recorded"],false);
        assert_eq!(saved["metadata"]["status"],"completed");
        let nodes:usize = conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE kind='flow/node_add'", [], |row|row.get(0)).unwrap();
        assert_eq!(nodes,1);
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_expires_bodies_but_keeps_pending_inputs_and_history_references() {
        let root = std::env::temp_dir().join(format!("request-retention-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut conn = connection(&root).unwrap();
        for (id,status,created) in [(1,"completed",0),(2,"pending",0),(3,"completed",172_800_000)] {
            conn.execute("INSERT INTO agent_request_contexts(id,task_id,actor,turn,step,node_id,metadata,request_json) VALUES (?1,'task','agent',1,1,'n',?2,?3)",
                params![id,json!({"status":status,"created_at":created,"history_reference":{"event_seq":id}}).to_string(),json!({"messages":["full input"]}).to_string()]).unwrap();
        }
        let tx = conn.transaction().unwrap();
        expire_bodies(&tx,"task",1,172_800_000).unwrap();
        tx.commit().unwrap();
        let rows: Vec<(i64,String,String)> = conn.prepare("SELECT id,request_json,metadata FROM agent_request_contexts ORDER BY id").unwrap()
            .query_map([],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(rows[0].1,"null");
        assert_eq!(serde_json::from_str::<Value>(&rows[0].2).unwrap()["history_reference"]["event_seq"],1);
        assert_ne!(rows[1].1,"null");
        assert_ne!(rows[2].1,"null");
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn roles_record_exact_views_of_one_shared_fact_even_when_panel_is_hidden() {
        let root = std::env::temp_dir().join(format!(
            "role-context-pool-{}",
            crate::visual_artifacts::new_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        {
            let mut conn = connection(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','check collision','fake','running',0,0)",[]).unwrap();
            conn.execute("INSERT INTO agent_context_debug(task_id,enabled) VALUES ('task',0)", [])
                .unwrap();
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
            crate::agent_service::append_event_tx(
                &tx,
                "task",
                "tool/call",
                &json!({"turn":1,"callId":"drive","name":"drive","arguments":"{}"}),
                None,
            )
            .unwrap();
            crate::agent_service::append_event_tx(&tx,"task","tool/result",&json!({"turn":1,"message":{"toolCallId":"drive"},"meta":{"result":{"collision":true,"location":"road"}}}),None).unwrap();
            tx.commit().unwrap();
        }
        let facts = crate::session_history::read(&root, "task", &json!({"event_seq":1}))
            .await
            .unwrap();
        let perspectives = [
            ("worker", "Check whether my driving caused the collision."),
            ("organizer", "Ask Worker to check its driving."),
            (
                "observer",
                "A vehicle fault may explain the collision; this is a hypothesis.",
            ),
        ];
        let mut expected = Vec::new();
        for (index, (actor, perspective)) in perspectives.into_iter().enumerate() {
            let body = json!({"model":"fake","messages":[{"role":"system","content":perspective},
                {"role":"user","content":facts.to_string()},
                {"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,EXACT_IMAGE"}},{"type":"text","text":"原始材料".repeat(5000)}]}],"tools":[],"stream":true});
            let trace = record(
                &root,
                "task",
                json!({"actor":actor,"turn":1,"step":index+1,"request_id":1,"nodeId":"drive"}),
                &body,
            )
            .await
            .unwrap();
            let id = trace.id;
            trace
                .finish(
                    if actor == "observer" {
                        "failed"
                    } else {
                        "completed"
                    },
                    json!({"perspective":perspective}),
                )
                .await;
            let conn = connection(&root).unwrap();
            let saved = saved_context(&conn, "task", id).unwrap().unwrap();
            assert_eq!(
                saved["body"], body,
                "no truncation or reinterpretation of role input"
            );
            assert_eq!(saved["metadata"]["actor"], actor);
            assert_eq!(
                saved["metadata"]["context_access"]["history_reader"],
                actor != "worker"
            );
            if actor == "worker" {
                assert_eq!(
                    saved["metadata"]["context_access"]["input_provider"],
                    "organizer"
                );
            }
            if actor == "observer" {
                assert_eq!(
                    saved["metadata"]["context_access"]["communicates_with"],
                    json!([])
                );
            }
            let seq = saved["metadata"]["history_reference"]["event_seq"]
                .as_i64()
                .unwrap();
            assert_eq!(saved["metadata"]["history_pool"]["before_seq"], seq);
            expected.push((actor, id, seq));
        }
        // Shared history preserves every event, including the exact input views.
        let process = crate::session_history::task_process(&root, "task", 1)
            .await
            .unwrap();
        let records = process["records"].as_array().unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|record| record["kind"] != "debug/context_request")
                .count(),
            5
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record["kind"] == "debug/context_request")
                .count(),
            3
        );
        let ordinary = crate::session_history::read(&root, "task", &json!({"limit":12}))
            .await
            .unwrap();
        assert_eq!(ordinary["records"].as_array().unwrap().len(), 5);
        for (actor, id, seq) in expected {
            let perspective = perspectives
                .iter()
                .find(|(role, _)| *role == actor)
                .unwrap()
                .1;
            let listed = crate::session_history::read(
                &root,
                "task",
                &json!({"include_context":true,"role":actor,"query":perspective,"limit":12}),
            )
            .await
            .unwrap();
            assert_eq!(listed["records"].as_array().unwrap().len(), 1);
            assert_eq!(listed["records"][0]["seq"], seq);
            assert_eq!(listed["records"][0]["role"], actor);
            // Read enough paged original text to recover the full large input.
            let mut offset = 0;
            let mut content = String::new();
            loop {
                let page = crate::session_history::read(
                    &root,
                    "task",
                    &json!({"event_seq":seq,"char_offset":offset,"max_chars":12000}),
                )
                .await
                .unwrap();
                let record = &page["records"][0];
                content.push_str(record["content"].as_str().unwrap());
                if let Some(next) = record["next_char_offset"].as_u64() {
                    offset = next;
                } else {
                    break;
                }
            }
            let view: Value = serde_json::from_str(&content).unwrap();
            assert_eq!(view["metadata"]["id"], id);
            assert_eq!(
                view["body"],
                saved_context(&connection(&root).unwrap(), "task", id)
                    .unwrap()
                    .unwrap()["body"]
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn repeated_requests_are_separate_views_not_repeated_tool_actions() {
        let root = std::env::temp_dir().join(format!(
            "repeat-context-{}",
            crate::visual_artifacts::new_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        connection(&root).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        let body = json!({"model":"fake","messages":[{"role":"tool","tool_call_id":"same_upload","content":"same upload receipt"}]});
        for step in 1..=2 {
            let trace = record(
                &root,
                "task",
                json!({"actor":"worker","turn":1,"step":step}),
                &body,
            )
            .await
            .unwrap();
            trace.finish("completed", json!({})).await;
        }
        let conn = connection(&root).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM agent_request_contexts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM agent_task_events WHERE kind='tool/call'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM agent_task_events WHERE kind='debug/context_request'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
}
