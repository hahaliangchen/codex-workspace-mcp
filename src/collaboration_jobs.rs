//! Task delegates share the conversation's authority, not its working window.
//! The parent owns planning and acceptance; arrival order never rewrites history.
use crate::agent_service::{self, AgentServiceState};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const LIMIT: usize = 5;
static RUNNING: OnceLock<Mutex<BTreeMap<String, (PathBuf, String, CancellationToken)>>> =
    OnceLock::new();
static CHANGED: Notify = Notify::const_new();
fn running() -> &'static Mutex<BTreeMap<String, (PathBuf, String, CancellationToken)>> {
    RUNNING.get_or_init(Default::default)
}
pub fn db(root: &Path) -> Result<rusqlite::Connection> {
    let conn = agent_service::open_db(root)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_delegates(
      id TEXT PRIMARY KEY, task_id TEXT NOT NULL, turn INTEGER NOT NULL, call_id TEXT NOT NULL,
      name TEXT NOT NULL, specification TEXT NOT NULL, status TEXT NOT NULL, result TEXT,
      delivered INTEGER NOT NULL DEFAULT 0, review TEXT, event_count INTEGER NOT NULL DEFAULT 0, end_seq INTEGER, created_at INTEGER NOT NULL,
      UNIQUE(task_id,call_id));
      CREATE INDEX IF NOT EXISTS idx_delegates_parent ON agent_delegates(task_id,status);",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_delegate_assets(
      task_id TEXT NOT NULL, agent_id TEXT NOT NULL, asset_id TEXT NOT NULL,
      reference_json TEXT NOT NULL, PRIMARY KEY(task_id,agent_id,asset_id));",
    )?;
    Ok(conn)
}
/// Save handles across child compressions; this table never contains bodies.
pub(crate) fn remember_assets(
    tx: &rusqlite::Transaction<'_>,
    task: &str,
    id: &str,
    assets: &Value,
) -> Result<()> {
    for asset in assets.as_array().into_iter().flatten() {
        let reference = &asset["asset_reference"];
        if let Some(asset_id) = reference["asset_id"].as_str() {
            tx.execute(
                "INSERT OR IGNORE INTO agent_delegate_assets VALUES (?1,?2,?3,?4)",
                params![task, id, asset_id, reference.to_string()],
            )?;
        }
    }
    Ok(())
}
fn artifact_index(tx: &rusqlite::Transaction<'_>, task: &str, id: &str) -> Result<Value> {
    let mut stmt = tx.prepare("SELECT reference_json FROM agent_delegate_assets WHERE task_id=?1 AND agent_id=?2 ORDER BY rowid")?;
    let refs = stmt
        .query_map(params![task, id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|s| serde_json::from_str::<Value>(&s).map_err(Into::into))
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"assets":refs,"note":"Recorded input/output references, not proof of successful persistence or acceptance. Inspect current files/diffs or authenticated remote versions before accepting."}),
    )
}
/// Run only in the transaction that archives a terminal outcome. Original
/// events, request traces, owned files and immutable assets are not removed.
fn discard_working_context(tx: &rusqlite::Transaction<'_>, task: &str, id: &str) -> Result<()> {
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_runtime_windows')", [], |r| r.get(0))?;
    if !exists {
        return Ok(());
    }
    let scope = format!("delegate:{id}");
    let materials_exist: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_context_materials')", [], |r| r.get(0))?;
    if materials_exist {
        // Ownership is explicit; never remove a sibling's or the main Agent's
        // compression window. Legacy unowned snapshots remain audit data.
        tx.execute("DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope IN (SELECT 'context:' || id FROM agent_context_materials WHERE task_id=?1 AND json_extract(data,'$.owner_scope')=?2)",params![task,scope])?;
        tx.execute("DELETE FROM agent_context_materials WHERE task_id=?1 AND json_extract(data,'$.owner_scope')=?2",params![task,scope])?;
    }
    tx.execute(
        "DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2",
        params![task, scope],
    )?;
    Ok(())
}
fn function(name: &str, description: &str, properties: Value, required: Value) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
}
pub fn tools() -> Vec<Value> {
    vec![
        function(
            "spawn_agent",
            "Delegate one independent task. At most five task agents run per conversation; context compression does not consume these slots. Returns immediately. The host forks the current execution context, including confirmed requirements, loaded materials, execution receipts, active skills and asset references. Delegates need not reread material already present. Supply only the specific task, owned files/pages and acceptance criteria; do not repeat shared requirements, design specs or outlines. The optional context field is for assignment-specific exceptions or references only. Delegates inherit this conversation's model, permissions and plugin/project binding; they cannot create further delegates. You remain responsible for verifying their actual results.",
            json!({"name":{"type":"string","maxLength":120},"task":{"type":"string","maxLength":24000},"context":{"type":"string","maxLength":48000}}),
            json!(["name", "task"]),
        ),
        function(
            "wait_agents",
            "Inspect task-agent status; optionally wait up to 30 seconds when no independent work remains. Finished status and report/asset-index references are delivered automatically before the next main-agent request, in durable completion order. Read bodies only as needed.",
            json!({"wait_ms":{"type":"integer","minimum":0,"maximum":30000}}),
            json!([]),
        ),
        function(
            "review_agent_result",
            "Record the MAIN agent's acceptance after checking a delegate's actual artifacts by reference. For code, personally inspect changed files/diffs, versions and relevant verification evidence. A child report or completed status is not proof of success. Use needs_work to reject and explicitly dispatch corrective work, or unresolved when blocked; describe the gap in the final answer. Every terminal delegate must be reviewed before final delivery.",
            json!({"agent_id":{"type":"string"},"decision":{"type":"string","enum":["accepted","needs_work","unresolved"]},"evidence":{"type":"string","minLength":1,"maxLength":12000}}),
            json!(["agent_id", "decision", "evidence"]),
        ),
        function(
            "cancel_agent",
            "Cancel one of this conversation's task agents. Already executed actions are not undone. Collect and review its outcome before final delivery.",
            json!({"agent_id":{"type":"string"}}),
            json!(["agent_id"]),
        ),
    ]
}
pub fn is_tool(name: &str) -> bool {
    matches!(
        name,
        "spawn_agent" | "wait_agents" | "review_agent_result" | "cancel_agent"
    )
}
pub fn specification(root: &Path, task: &str, id: &str) -> Result<Value> {
    let raw: String = db(root)?.query_row(
        "SELECT specification FROM agent_delegates WHERE task_id=?1 AND id=?2",
        params![task, id],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&raw)?)
}
struct Guard(String);
impl Drop for Guard {
    fn drop(&mut self) {
        running().lock().unwrap().remove(&self.0);
        CHANGED.notify_waiters();
    }
}
pub fn submit(
    state: &AgentServiceState,
    task: &str,
    turn: usize,
    call: &str,
    node: &str,
    model: &str,
    args: &Value,
    ban_execution: bool,
    shared_context: Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    ensure!(!cancel.is_cancelled(), "cancelled");
    let name = args["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 120)
        .context("name required (1..120 characters)")?;
    let objective = args["task"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 24000)
        .context("task required (1..24000 characters)")?;
    let context = match args.get("context") {
        None => "",
        Some(value) => value
            .as_str()
            .filter(|s| s.chars().count() <= 48000)
            .context("context must be a string (up to 48000 characters)")?,
    };
    reap(state.workspace.root(), task)?;
    let mut conn = db(state.workspace.root())?;
    // Hold the executor registry lock until the durable reservation is registered.
    let mut executors = running().lock().unwrap();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(id) = tx
        .query_row(
            "SELECT id FROM agent_delegates WHERE task_id=?1 AND call_id=?2",
            params![task, call],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(json!({"agent_id":id,"status":"already_submitted"}));
    }
    let active: usize = tx.query_row(
        "SELECT count(*) FROM agent_delegates WHERE task_id=?1 AND status='running'",
        [task],
        |r| r.get(0),
    )?;
    ensure!(
        active < LIMIT,
        "AGENT_CONCURRENCY_LIMIT: five delegates are already running; collect results before dispatching more"
    );
    let id = agent_service::uuid_like();
    let (through, _) = crate::session_history::requirement_snapshot(&tx, task)?;
    let spec = json!({"agent_id":id,"name":name,"task":objective,"context":context,"parent_task_id":task,"parent_node_id":node,"history_through_seq":through,"authority":"delegated_task_not_a_human_message"});
    tx.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES (?1,?2,?3,?4,?5,?6,'running',?7)",params![id,task,turn,call,name,spec.to_string(),chrono::Utc::now().timestamp_millis()])?;
    agent_service::append_event_tx(
        &tx,
        task,
        "subagent/start",
        &json!({"agent_id":id,"name":name,"turn":turn,"parent_node_id":node,"specification":spec,"status":"running"}),
        Some("append"),
    )?;
    tx.commit()?;
    let child = cancel.child_token();
    executors.insert(
        id.clone(),
        (
            state.workspace.root().to_path_buf(),
            task.to_owned(),
            child.clone(),
        ),
    );
    drop(executors);
    let (state, task, model, node, job) = (
        state.clone(),
        task.to_owned(),
        model.to_owned(),
        node.to_owned(),
        id.clone(),
    );
    tokio::spawn(async move {
        let _guard = Guard(job.clone());
        let outcome = tokio::spawn(crate::single_agent::run_delegate(
            state.clone(),
            task.clone(),
            model,
            turn,
            job.clone(),
            node,
            ban_execution,
            shared_context,
            child.clone(),
        ))
        .await;
        let mut result = match outcome {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                json!({"status":if child.is_cancelled(){"cancelled"}else{"failed"},"error":format!("{error:#}")})
            }
            Err(error) => {
                json!({"status":"interrupted","error":format!("delegate executor stopped: {error}")})
            }
        };
        let mut persist = || -> Result<()> {
            let mut conn = db(state.workspace.root())?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            result["artifact_index"] = artifact_index(&tx, &task, &job)?;
            let seq = agent_service::append_event_tx(
                &tx,
                &task,
                "subagent/end",
                &json!({"agent_id":job,"turn":turn,"result":result,"status":result["status"]}),
                Some("append"),
            )?;
            tx.execute(
                "UPDATE agent_delegates SET status=?2,result=?3,end_seq=?4 WHERE id=?1",
                params![
                    job,
                    result["status"].as_str().unwrap_or("failed"),
                    result.to_string(),
                    seq
                ],
            )?;
            discard_working_context(&tx, &task, &job)?;
            tx.commit()?;
            Ok(())
        };
        if let Err(error) = persist() {
            tracing::error!(agent_id=%job,error=%error,"could not persist delegate completion; restart recovery must treat its outcome as unresolved");
        }
    });
    Ok(
        json!({"status":"accepted","agent_id":id,"name":name,"message":"Delegate started. Continue independent work; collect results, verify artifacts and review before final delivery."}),
    )
}
fn reap(root: &Path, task: &str) -> Result<()> {
    // Same lock order as submit; never mistake an unregistered reservation for a crash.
    let executors = running().lock().unwrap();
    let mut conn = db(root)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let ids = tx
        .prepare("SELECT id FROM agent_delegates WHERE task_id=?1 AND status='running'")?
        .query_map([task], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        if !executors.contains_key(&id) {
            let result = json!({"status":"interrupted","error":"No live executor after restart. Inspect saved artifacts; the host did not replay the task.","artifact_index":artifact_index(&tx,task,&id)?});
            tx.execute(
                "UPDATE agent_delegates SET status='interrupted',result=?2 WHERE id=?1",
                params![id, result.to_string()],
            )?;
            let seq = agent_service::append_event_tx(
                &tx,
                task,
                "subagent/end",
                &json!({"agent_id":id,"status":"interrupted","result":result}),
                Some("append"),
            )?;
            tx.execute(
                "UPDATE agent_delegates SET end_seq=?2 WHERE id=?1",
                params![id, seq],
            )?;
            discard_working_context(&tx, task, &id)?;
        }
    }
    tx.commit()?;
    Ok(())
}
pub fn statuses(root: &Path, task: &str) -> Result<Value> {
    reap(root, task)?;
    let conn = db(root)?;
    let mut stmt=conn.prepare("SELECT id,name,status,delivered,review FROM agent_delegates WHERE task_id=?1 ORDER BY created_at,rowid")?;
    Ok(json!(stmt.query_map([task],|r|Ok(json!({"agent_id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"delivered":r.get::<_,bool>(3)?,"review":r.get::<_,Option<String>>(4)?.map(|s|serde_json::from_str::<Value>(&s).unwrap_or(Value::Null))})))?.collect::<rusqlite::Result<Vec<_>>>()?))
}
pub fn ready(root: &Path, task: &str) -> Result<Vec<Value>> {
    reap(root, task)?;
    let conn = db(root)?;
    let mut stmt=conn.prepare("SELECT id,name,result,end_seq FROM agent_delegates WHERE task_id=?1 AND status!='running' AND delivered=0 ORDER BY end_seq")?;
    Ok(stmt.query_map([task],|r|Ok(json!({"agent_id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"result":serde_json::from_str::<Value>(&r.get::<_,String>(2)?).unwrap_or(Value::Null),"end_seq":r.get::<_,i64>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?)
}
pub fn outstanding(root: &Path, task: &str) -> Result<bool> {
    reap(root, task)?;
    Ok(db(root)?.query_row("SELECT EXISTS(SELECT 1 FROM agent_delegates WHERE task_id=?1 AND (status='running' OR delivered=0 OR review IS NULL))",[task],|r|r.get(0))?)
}
pub async fn wait(root: &Path, task: &str, cancel: &CancellationToken, millis: u64) -> Result<()> {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(millis.min(30000));
    loop {
        let notified = CHANGED.notified();
        let active: bool = db(root)?.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_delegates WHERE task_id=?1 AND status='running')",
            [task],
            |r| r.get(0),
        )?;
        if !ready(root, task)?.is_empty() || !active {
            return Ok(());
        }
        tokio::select! {_=cancel.cancelled()=>anyhow::bail!("cancelled"),_=notified=>{},_=tokio::time::sleep_until(deadline)=>return Ok(())}
    }
}
pub fn review(root: &Path, task: &str, args: &Value) -> Result<Value> {
    let id = args["agent_id"].as_str().context("agent_id required")?;
    let decision = args["decision"]
        .as_str()
        .filter(|s| matches!(*s, "accepted" | "needs_work" | "unresolved"))
        .context("valid decision required")?;
    let evidence = args["evidence"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 12000)
        .context("acceptance evidence or unresolved gaps required")?;
    let mut conn = db(root)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let value = json!({"decision":decision,"evidence":evidence});
    let n=tx.execute("UPDATE agent_delegates SET review=?3 WHERE task_id=?1 AND id=?2 AND status!='running' AND delivered=1",params![task,id,value.to_string()])?;
    ensure!(
        n == 1,
        "delegate unavailable: collect this conversation's terminal result before review"
    );
    agent_service::append_event_tx(
        &tx,
        task,
        "subagent/review",
        &json!({"agent_id":id,"review":value}),
        Some("append"),
    )?;
    tx.commit()?;
    Ok(json!({"agent_id":id,"review":value}))
}
pub fn cancel_one(root: &Path, task: &str, id: &str) -> Result<Value> {
    specification(root, task, id)?;
    if let Some((path, parent, token)) = running().lock().unwrap().get(id) {
        ensure!(path == root && parent == task, "delegate scope denied");
        token.cancel();
    }
    Ok(json!({"agent_id":id,"cancellation_requested":true}))
}
pub fn cancel_all(root: &Path, task: &str) {
    for (path, parent, token) in running().lock().unwrap().values() {
        if path == root && parent == task {
            token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn five_slots_exclude_compression_and_enforce_parent_cancellation_scope() {
        use axum::{Router, routing::post};
        let root =
            std::env::temp_dir().join(format!("delegation-cap-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(&root, listener.local_addr().unwrap());
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async { std::future::pending::<axum::Json<Value>>().await }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = db(&root).unwrap();
        for id in ["parent", "foreign"] {
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'test','fake','running',0,0)",[id]).unwrap();
        }
        conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_runtime_windows(task_id TEXT NOT NULL,scope TEXT NOT NULL,state TEXT NOT NULL,PRIMARY KEY(task_id,scope));INSERT INTO agent_runtime_windows VALUES ('parent','context:compression','{}');").unwrap();
        let cancel = CancellationToken::new();
        let args = json!({"name":"isolated task","task":"Do assigned work only","context":"shared constraints"});
        let mut ids = Vec::new();
        for i in 0..5 {
            let result = submit(
                &state,
                "parent",
                1,
                &format!("call-{i}"),
                "parent-node",
                "fake",
                &args,
                true,
                json!({}),
                &cancel,
            )
            .unwrap();
            ids.push(result["agent_id"].as_str().unwrap().to_owned());
        }
        assert!(
            submit(
                &state,
                "parent",
                1,
                "sixth",
                "parent-node",
                "fake",
                &args,
                true,
                json!({}),
                &cancel
            )
            .unwrap_err()
            .to_string()
            .contains("AGENT_CONCURRENCY_LIMIT")
        );
        assert_eq!(
            submit(
                &state,
                "parent",
                1,
                "call-0",
                "parent-node",
                "fake",
                &args,
                true,
                json!({}),
                &cancel
            )
            .unwrap()["agent_id"],
            ids[0]
        );
        assert!(cancel_one(&root, "foreign", &ids[0]).is_err());
        assert!(
            review(
                &root,
                "parent",
                &json!({"agent_id":ids[0],"decision":"accepted","evidence":"premature claim"})
            )
            .is_err()
        );
        cancel_all(&root, "parent");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if statuses(&root, "parent")
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|v| v["status"] != "running")
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(ready(&root, "parent").unwrap().len(), 5);
        assert!(ready(&root, "foreign").unwrap().is_empty());
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_runtime_windows WHERE task_id='parent' AND scope LIKE 'delegate:%'",[],|r|r.get::<_,usize>(0)).unwrap(),0);
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_runtime_windows WHERE task_id='parent' AND scope='context:compression'",[],|r|r.get::<_,usize>(0)).unwrap(),1);
        server.abort();
        std::fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn restart_interrupts_delegates_without_replaying_or_auto_accepting_them() {
        let root = std::env::temp_dir().join(format!(
            "delegation-recovery-{}",
            agent_service::uuid_like()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut conn = db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('parent','test','fake','running',0,0)",[]).unwrap();
        conn.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES ('orphan','parent',1,'call','work','{}','running',0)",[]).unwrap();
        conn.execute_batch("CREATE TABLE agent_runtime_windows(task_id TEXT,scope TEXT,state TEXT,PRIMARY KEY(task_id,scope));
            CREATE TABLE agent_context_materials(task_id TEXT,id TEXT,data TEXT,PRIMARY KEY(task_id,id));
            INSERT INTO agent_runtime_windows VALUES ('parent','main','{}'),('parent','delegate:orphan','{}'),('parent','delegate:sibling','{}'),('foreign','delegate:orphan','{}');
            CREATE TRIGGER fail_cleanup BEFORE DELETE ON agent_runtime_windows WHEN OLD.task_id='parent' AND OLD.scope='delegate:orphan' BEGIN SELECT RAISE(ABORT,'simulated archive transaction failure'); END;").unwrap();
        for (task, id, owner) in [
            ("parent", "child-compression", Some("delegate:orphan")),
            ("parent", "main-compression", Some("main")),
            ("parent", "sibling-compression", Some("delegate:sibling")),
            ("foreign", "foreign-compression", Some("delegate:orphan")),
            ("parent", "legacy", None),
        ] {
            conn.execute(
                "INSERT INTO agent_runtime_windows VALUES (?1,?2,'{}')",
                params![task, format!("context:{id}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO agent_context_materials VALUES (?1,?2,?3)",
                params![task, id, json!({"owner_scope":owner}).to_string()],
            )
            .unwrap();
        }
        let tx = conn.transaction().unwrap();
        remember_assets(&tx,"parent","orphan",&json!([{"asset_reference":{"asset_id":"saved-before-compression","source_path":"edited.rs","read_reference":{"tool":"read_asset","arguments":{"asset_id":"saved-before-compression"}}}}])).unwrap();
        // Empty later windows must not erase previously indexed references.
        remember_assets(&tx, "parent", "orphan", &json!([])).unwrap();
        tx.commit().unwrap();
        assert!(statuses(&root, "parent").is_err());
        assert_eq!(
            conn.query_row(
                "SELECT status FROM agent_delegates WHERE id='orphan'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "running"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM agent_task_events WHERE kind='subagent/end'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM agent_context_materials", [], |r| {
                r.get::<_, usize>(0)
            })
            .unwrap(),
            5,
            "failed archival must roll back all context cleanup"
        );
        conn.execute_batch("DROP TRIGGER fail_cleanup;").unwrap();
        assert_eq!(
            statuses(&root, "parent").unwrap()[0]["status"],
            "interrupted"
        );
        assert!(outstanding(&root, "parent").unwrap());
        assert_eq!(ready(&root, "parent").unwrap().len(), 1);
        assert_eq!(ready(&root, "parent").unwrap().len(), 1);
        assert_eq!(
            ready(&root, "parent").unwrap()[0]["result"]["artifact_index"]["assets"][0]["asset_id"],
            "saved-before-compression"
        );
        let windows = conn
            .prepare("SELECT task_id,scope FROM agent_runtime_windows ORDER BY task_id,scope")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(!windows.contains(&("parent".into(), "delegate:orphan".into())));
        assert!(!windows.contains(&("parent".into(), "context:child-compression".into())));
        for (task, scope) in [
            ("parent", "main"),
            ("parent", "delegate:sibling"),
            ("foreign", "delegate:orphan"),
            ("parent", "context:main-compression"),
            ("parent", "context:sibling-compression"),
            ("foreign", "context:foreign-compression"),
            ("parent", "context:legacy"),
        ] {
            assert!(
                windows.contains(&(task.into(), scope.into())),
                "unrelated window removed: {task}/{scope}"
            );
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM agent_context_materials", [], |r| {
                r.get::<_, usize>(0)
            })
            .unwrap(),
            4
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM agent_task_events WHERE kind='subagent/end'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert!(review(&root,"parent",&json!({"agent_id":"orphan","decision":"unresolved","evidence":"must collect first"})).is_err());
        conn.execute(
            "UPDATE agent_delegates SET delivered=1 WHERE id='orphan'",
            [],
        )
        .unwrap();
        review(&root,"parent",&json!({"agent_id":"orphan","decision":"unresolved","evidence":"No executor after restart; original actions not repeated"})).unwrap();
        assert!(!outstanding(&root, "parent").unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
}
