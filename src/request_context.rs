//! Durable snapshots of each role's finalized model input. Conversation events
//! reference these views of the shared history; a view is not another action.
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
        let enabled = conn.query_row("SELECT enabled FROM agent_context_debug WHERE task_id=?1", [&task_id], |row| row.get::<_,bool>(0)).optional()?.unwrap_or(true);
        Ok(json!({"enabled":enabled,"recording":true}))
    }).await)
}

#[derive(Deserialize)]
pub struct DebugSetting { enabled: bool }

// The legacy context-debug endpoint now controls panel visibility only.
// Hiding a panel must not stop recording any role's actual model inputs.
pub async fn set_setting(State(state): State<AgentServiceState>, RoutePath(task_id): RoutePath<String>, Json(input): Json<DebugSetting>) -> Response {
    let root = state.workspace.root().to_path_buf();
    respond(tokio::task::spawn_blocking(move || -> Result<Value> {
        let conn = connection(&root)?; exists(&conn, &task_id)?;
        conn.execute("INSERT INTO agent_context_debug(task_id,enabled) VALUES (?1,?2) ON CONFLICT(task_id) DO UPDATE SET enabled=excluded.enabled", params![task_id,input.enabled])?;
        Ok(json!({"enabled":input.enabled,"recording":true}))
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
        saved_context(&conn,&task_id,id)?.context("request context not found")
    }).await)
}

/// Resolve a conversation's input-view reference without duplicating the body
/// in the event stream. Older events may refer to already deleted snapshots.
pub fn saved_context(conn:&Connection,task_id:&str,id:i64)->Result<Option<Value>> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_request_contexts')",[],|row|row.get(0))?;
    if !exists {return Ok(None);}
    let row:Option<(String,String)>=conn.query_row("SELECT metadata,request_json FROM agent_request_contexts WHERE task_id=?1 AND id=?2",params![task_id,id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    row.map(|(meta,body)| {
        let mut metadata:Value=serde_json::from_str(&meta)?;metadata["id"]=json!(id);
        Ok(json!({"record_kind":"model_input","metadata":metadata,"body":serde_json::from_str::<Value>(&body)?}))
    }).transpose()
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
    let started = Instant::now();
    let body_text = body.to_string();
    metadata["request_bytes"] = json!(body_text.len());
    metadata["model"] = body["model"].clone();
    metadata["created_at"] = json!(now());
    metadata["status"] = json!("pending");
    metadata["record_kind"] = json!("model_input");
    metadata["context_access"] = match metadata["actor"].as_str() {
        Some("worker") => json!({"history_reader":false,"input_provider":"organizer","observer_direct":false}),
        Some("observer") => json!({"history_reader":true,"purpose":"retrospective","communicates_with":[]}),
        Some("organizer") => json!({"history_reader":true,"provides_worker_view":true}),
        _ => Value::Null,
    };
    metadata["tools"] = json!(body["tools"].as_array().into_iter().flatten().filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str)).collect::<Vec<_>>());
    let messages = body["messages"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    metadata["message_count"] = json!(messages.len());
    metadata["role_counts"] = json!(["system","user","assistant","tool"].into_iter()
        .map(|role| (role,messages.iter().filter(|message|message["role"] == role).count())).collect::<std::collections::BTreeMap<_,_>>());
    let db_root = root.clone(); let db_id = task_id.clone(); let db_meta = metadata.clone();
    let inserted = tokio::task::spawn_blocking(move || -> Result<(i64,Value)> {
        let mut conn = connection(&db_root)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // This cutoff describes the available pool, not a claim that every
        // earlier event was selected into the request. The exact view is body.
        let seq:i64=tx.query_row("SELECT COALESCE(MAX(seq),-1)+1 FROM agent_task_events WHERE task_id=?1",[&db_id],|row|row.get(0))?;
        let mut db_meta=db_meta;
        db_meta["history_pool"]=json!({"scope":"conversation","task_id":db_id,"before_seq":seq});
        db_meta["history_reference"]=json!({"scope":"conversation","task_id":db_id,"event_seq":seq});
        tx.execute("INSERT INTO agent_request_contexts(task_id,actor,turn,step,node_id,metadata,request_json) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![db_id,
            db_meta["actor"].as_str().unwrap_or("worker"),db_meta["turn"].as_u64().unwrap_or(0),db_meta["step"].as_u64().unwrap_or(0),
            db_meta["nodeId"].as_str().unwrap_or(""),db_meta.to_string(),body_text])?;
        let id=tx.last_insert_rowid();
        crate::agent_service::append_event_tx(&tx,&db_id,"debug/context_request",&json!({
            "id":id,"record_kind":"model_input","actor":db_meta["actor"],"turn":db_meta["turn"],
            "step":db_meta["step"],"nodeId":db_meta["nodeId"],"workId":db_meta["workId"],
            "identity":{"task_id":db_id,"request_id":db_meta["request_id"],"node_id":db_meta["nodeId"]},
            "history_pool":db_meta["history_pool"],"history_reference":db_meta["history_reference"],
            "note":"Exact role input view; references shared history, not a new tool action or independent observation."
        }),None)?;
        tx.commit()?;
        Ok((id,db_meta))
    }).await;
    match inserted {
        Ok(Ok((id,metadata))) => {
            let trace = Trace { root:root.clone(),task_id:task_id.clone(),id,started,metadata:metadata.clone(),finished:false };
            Some(trace)
        },
        _ => { tracing::warn!(%task_id,"could not record finalized model request"); None }
    }
}

pub async fn finish(trace: Option<Trace>, status: &str, outcome: Value) {
    if let Some(trace) = trace { trace.finish(status,outcome).await; }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roles_record_exact_views_of_one_shared_fact_even_when_panel_is_hidden() {
        let root=std::env::temp_dir().join(format!("role-context-pool-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        {
            let mut conn=connection(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','check collision','fake','running',0,0)",[]).unwrap();
            conn.execute("INSERT INTO agent_context_debug VALUES ('task',0)",[]).unwrap();
            let tx=conn.transaction().unwrap();
            crate::agent_service::append_event_tx(&tx,"task","tool/call",&json!({"turn":1,"callId":"drive","name":"drive","arguments":"{}"}),None).unwrap();
            crate::agent_service::append_event_tx(&tx,"task","tool/result",&json!({"turn":1,"message":{"toolCallId":"drive"},"meta":{"result":{"collision":true,"location":"road"}}}),None).unwrap();
            tx.commit().unwrap();
        }
        let facts=crate::session_history::read(&root,"task",&json!({"event_seq":1})).await.unwrap();
        let perspectives=[("worker","Check whether my driving caused the collision."),
            ("organizer","Ask Worker to check its driving."),
            ("observer","A vehicle fault may explain the collision; this is a hypothesis.")];
        let mut expected=Vec::new();
        for (index,(actor,perspective)) in perspectives.into_iter().enumerate() {
            let body=json!({"model":"fake","messages":[{"role":"system","content":perspective},
                {"role":"user","content":facts.to_string()},
                {"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,EXACT_IMAGE"}},{"type":"text","text":"原始材料".repeat(5000)}]}],"tools":[],"stream":true});
            let trace=record(&root,"task",json!({"actor":actor,"turn":1,"step":index+1,"request_id":1,"nodeId":"drive"}),&body).await.unwrap();
            let id=trace.id;
            trace.finish(if actor=="observer" {"failed"}else{"completed"},json!({"perspective":perspective})).await;
            let conn=connection(&root).unwrap();
            let saved=saved_context(&conn,"task",id).unwrap().unwrap();
            assert_eq!(saved["body"],body,"no truncation or reinterpretation of role input");
            assert_eq!(saved["metadata"]["actor"],actor);
            assert_eq!(saved["metadata"]["context_access"]["history_reader"],actor!="worker");
            if actor=="worker" {assert_eq!(saved["metadata"]["context_access"]["input_provider"],"organizer");}
            if actor=="observer" {assert_eq!(saved["metadata"]["context_access"]["communicates_with"],json!([]));}
            let seq=saved["metadata"]["history_reference"]["event_seq"].as_i64().unwrap();
            assert_eq!(saved["metadata"]["history_pool"]["before_seq"],seq);
            expected.push((actor,id,seq));
        }
        // Ordinary shared history retains one action and one objective result.
        // Input views are available on demand, never recursively fed as actions.
        let process=crate::session_history::task_process(&root,"task",1).await.unwrap();
        let records=process["records"].as_array().unwrap();
        assert_eq!(records.iter().filter(|record|record["kind"]!="debug/context_request").count(),2);
        assert_eq!(records.iter().filter(|record|record["kind"]=="debug/context_request").count(),3);
        let ordinary=crate::session_history::read(&root,"task",&json!({"limit":12})).await.unwrap();
        assert_eq!(ordinary["records"].as_array().unwrap().len(),2);
        for (actor,id,seq) in expected {
            let perspective=perspectives.iter().find(|(role,_)|*role==actor).unwrap().1;
            let listed=crate::session_history::read(&root,"task",&json!({"include_context":true,"role":actor,"query":perspective,"limit":12})).await.unwrap();
            assert_eq!(listed["records"].as_array().unwrap().len(),1);
            assert_eq!(listed["records"][0]["seq"],seq);
            assert_eq!(listed["records"][0]["role"],actor);
            // Read enough paged original text to recover the full large input.
            let mut offset=0;let mut content=String::new();
            loop {
                let page=crate::session_history::read(&root,"task",&json!({"event_seq":seq,"char_offset":offset,"max_chars":12000})).await.unwrap();
                let record=&page["records"][0];content.push_str(record["content"].as_str().unwrap());
                if let Some(next)=record["next_char_offset"].as_u64() {offset=next;}else{break;}
            }
            let view:Value=serde_json::from_str(&content).unwrap();
            assert_eq!(view["metadata"]["id"],id);
            assert_eq!(view["body"],saved_context(&connection(&root).unwrap(),"task",id).unwrap().unwrap()["body"]);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn repeated_requests_are_separate_views_not_repeated_tool_actions() {
        let root=std::env::temp_dir().join(format!("repeat-context-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        connection(&root).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        let body=json!({"model":"fake","messages":[{"role":"tool","tool_call_id":"same_upload","content":"same upload receipt"}]});
        for step in 1..=2 {
            let trace=record(&root,"task",json!({"actor":"worker","turn":1,"step":step}),&body).await.unwrap();
            trace.finish("completed",json!({})).await;
        }
        let conn=connection(&root).unwrap();
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_request_contexts",[],|r|r.get::<_,i64>(0)).unwrap(),2);
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='debug/context_request'",[],|r|r.get::<_,i64>(0)).unwrap(),2);
        drop(conn);std::fs::remove_dir_all(root).unwrap();
    }
}
