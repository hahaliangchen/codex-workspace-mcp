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
const LEASE_MS: i64 = 30_000;
static RUNNING: OnceLock<Mutex<BTreeMap<String, (PathBuf, String, CancellationToken)>>> =
    OnceLock::new();
static INSTANCE: OnceLock<String> = OnceLock::new();
static CHANGED: Notify = Notify::const_new();
fn instance() -> &'static str { INSTANCE.get_or_init(agent_service::uuid_like) }
#[cfg(test)]
pub(crate) fn test_instance() -> &'static str { instance() }
fn now_ms() -> i64 { chrono::Utc::now().timestamp_millis() }
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
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_delegate_executors(
      agent_id TEXT PRIMARY KEY, owner_id TEXT NOT NULL, lease_until INTEGER NOT NULL,
      cancel_requested INTEGER NOT NULL DEFAULT 0); ")?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_resource_claims(
      resource TEXT PRIMARY KEY, task_id TEXT NOT NULL, owner TEXT NOT NULL,
      owner_agent TEXT, expires_at INTEGER NOT NULL); ")?;
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
            "Inside execute_program use yield fork(helper,args), not call(tools.spawn_agent,args). This native tool runs one child function on another Agent and immediately returns its agent_id handle. The child inherits the caller's prepared tool groups, skills and saved parent-scope values, within context capacity, plus this assignment's inputs. Supply only its goal, parameters, owned resources and expected return; do not repeat shared setup or the whole plan. Local working context stays in the child. At most five children run per conversation; child Agents cannot fork again. Use join_agents when the caller needs these returns. Existing permissions, plugin boundaries and Git isolation still apply. A returned function is not proof that its artifacts pass acceptance.",
            json!({"name":{"type":"string","maxLength":120},"task":{"type":"string","maxLength":24000},"context":{"type":"string","maxLength":48000},"inputs":{"type":"object","description":"Additional assets, pinned materials, node returns, history events and tool_groups. Parent tool groups and saved scope values are inherited even when omitted."},"asset_ids":{"type":"array","maxItems":64,"items":{"type":"string","minLength":1,"maxLength":200},"description":"Explicit argument handles; complete bodies are supplied when they fit, otherwise references allow paging."}}),
            json!(["name", "task"]),
        ),
        function(
            "join_agents",
            "Inside execute_program use yield join(handles), not call(tools.join_agents,args). This native API suspends this caller until ALL specified fork handles have returned, including failed or cancelled children. Call alone. The host waits without repeated model calls; child completions are delivered with concise return values and saved detail handles. This does not accept results or replay child actions. Inspect the returned artifacts and integrate once before final delivery. Use wait_agents only if an intermediate status is specifically needed.",
            json!({"agent_ids":{"type":"array","minItems":1,"maxItems":LIMIT,"uniqueItems":true,"items":{"type":"string","minLength":1}}}),
            json!(["agent_ids"]),
        ),
        function(
            "wait_agents",
            "Inspect task-agent status; optionally wait up to 30 seconds when no independent work remains. Finished status and report/asset-index references are delivered automatically before the next main-agent request, in durable completion order. Read bodies only as needed.",
            json!({"wait_ms":{"type":"integer","minimum":0,"maximum":30000}}),
            json!([]),
        ),
        function(
            "review_agent_result",
            "Record the MAIN agent's acceptance. For accepted, cite successful main-agent tool/result event_seq values in evidence_refs; Git delegates must have an inspected diff and completed merge. The host verifies cited receipts, branch head and merge ancestry. Describe remaining issues for needs_work or unresolved. Every terminal delegate must be reviewed before final delivery.",
            json!({"agent_id":{"type":"string"},"decision":{"type":"string","enum":["accepted","needs_work","unresolved"]},"evidence":{"type":"string","minLength":1,"maxLength":12000},"evidence_refs":{"type":"array","items":{"type":"integer"},"maxItems":64},"git_head":{"type":"string"}}),
            json!(["agent_id", "decision", "evidence"]),
        ),
        function("inspect_agent_changes", "Inspect a completed Git delegate's branch diff against its starting commit. Use its returned head in merge_agent_result and review_agent_result.",
            json!({"agent_id":{"type":"string"}}),json!(["agent_id"])),
        function("merge_agent_result", "Merge a completed Git delegate branch into the main checkout after inspecting its diff. A conflict leaves the main checkout for explicit resolution; complete or abort the merge before review.",
            json!({"agent_id":{"type":"string"},"expected_head":{"type":"string"}}),json!(["agent_id","expected_head"])),
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
        "spawn_agent"
            | "join_agents"
            | "wait_agents" | "review_agent_result" | "cancel_agent" | "inspect_agent_changes" | "merge_agent_result"
    )
}
pub fn ensure_active(root: &Path, task: &str, id: &str) -> Result<()> {
    let active: bool = db(root)?.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_delegates d JOIN agent_delegate_executors e ON e.agent_id=d.id WHERE d.task_id=?1 AND d.id=?2 AND d.status='running' AND e.owner_id=?3 AND e.cancel_requested=0)",
        params![task,id,instance()],|r|r.get(0))?;
    ensure!(active,"DELEGATE_EXECUTOR_FENCED: delegate is no longer owned by this host");
    Ok(())
}
pub struct ResourceClaim {
    root: PathBuf,
    resource: String,
    owner: String,
    release_on_drop: bool,
}
impl Drop for ResourceClaim {
    fn drop(&mut self) {
        if self.release_on_drop {
            if let Ok(conn)=db(&self.root) {
                let _=conn.execute("DELETE FROM agent_resource_claims WHERE resource=?1 AND owner=?2",
                    params![self.resource,self.owner]);
            }
        }
    }
}
pub fn resource_key(workspace: &Path, name: &str, args: &Value) -> Result<Option<String>> {
    if matches!(name,"write_file"|"edit_file"|"replace_range") {
        let path=args["path"].as_str().context("write path required")?;
        return Ok(Some(format!("local:{}",crate::file_edit::workspace_path(workspace,path)?.display())));
    }
    Ok(None)
}
pub fn claim_resource(root: &Path, task: &str, agent: Option<&str>, call_id: &str, resource: String) -> Result<ResourceClaim> {
    let owner=agent.map(|id|format!("delegate:{id}")).unwrap_or_else(||format!("main:{task}:{call_id}"));
    let mut conn=db(root)?;
    let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous=tx.query_row("SELECT owner,owner_agent,expires_at FROM agent_resource_claims WHERE resource=?1",[&resource],
        |r| {
                Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,i64>(2)?))
            },
        ).optional()?;
    if let Some((other,other_agent,expiry))=previous {
        if other!=owner {
            let active=if let Some(id)=other_agent {
                tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_delegates d JOIN agent_delegate_executors e ON e.agent_id=d.id WHERE d.id=?1 AND d.status='running' AND e.lease_until>?2)",
                    params![id,now_ms()],|r|r.get::<_,bool>(0))?
            } else {expiry>now_ms()};
            ensure!(!active,"RESOURCE_WRITE_OWNERSHIP_CONFLICT: {resource} is owned by {other}");
        }
    }
    tx.execute("INSERT INTO agent_resource_claims(resource,task_id,owner,owner_agent,expires_at) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(resource) DO UPDATE SET task_id=excluded.task_id,owner=excluded.owner,owner_agent=excluded.owner_agent,expires_at=excluded.expires_at",
        params![resource,task,owner,agent,now_ms()+30*60*1000])?;
    tx.commit()?;
    Ok(ResourceClaim{root:root.to_path_buf(),resource,owner,release_on_drop:agent.is_none()})
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
    let asset_ids = match args.get("asset_ids") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => {
            let items = value.as_array().context("asset_ids must be an array")?;
            ensure!(items.len() <= 64, "asset_ids supports at most 64 entries");
            items
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .filter(|id| !id.is_empty() && id.chars().count() <= 200)
                        .map(str::to_owned)
                        .context("asset_ids entries must be non-empty strings up to 200 characters")
                })
                .collect::<Result<Vec<_>>>()?
        }
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
    let git = if crate::plugin_sessions::owner(state.workspace.root(),task)?.is_none()
        && !ban_execution && !matches!(state.permission_mode, agent_service::PermissionMode::ReadOnly) {
        // Branch from the project this task works in, not the server's checkout.
        crate::delegate_git::prepare(&crate::session_browser::task_workspace(state.workspace.root(), task)?, &id)?
    } else { None };
    let (through, requirements) = crate::session_history::requirement_snapshot(&tx, task)?;
    let mut spec = json!({"agent_id":id,"name":name,"task":objective,"context":context,"parent_task_id":task,"parent_node_id":node,"history_through_seq":through,"requirement_event_seqs":requirements,"priority_asset_ids":asset_ids,"delegation_projection":shared_context["delegation_projection"].as_bool().unwrap_or(false),"delegation_source_tokens":shared_context["delegation_source_tokens"].clone(),"delegation_projected_tokens":shared_context["delegation_projected_tokens"].clone(),"delegation_fork_limit":shared_context["delegation_fork_limit"].clone(),"git":git,"authority":"delegated_task_not_a_human_message"});
    spec["caller_task_node_id"]=shared_context["caller_task_node_id"].clone();
    spec["program_function"] = json!(shared_context["program"].is_object());
    if let Some(node)=shared_context.get("task_node") {
        let mut node:crate::task_tree::Node=serde_json::from_value(node.clone())?;
        let saved=crate::task_tree::get(&tx,task,&node.node_id)?;
        ensure!(saved.status=="pending" && saved.executor=="delegate","delegate node is no longer pending");
        node.agent_id=Some(id.clone());node.status="running".into();
        spec["task_node_id"]=json!(node.node_id);
        crate::task_tree::emit(&tx,task,"task/node_started",&mut node,turn)?;
    }
    tx.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES (?1,?2,?3,?4,?5,?6,'running',?7)",params![id,task,turn,call,name,spec.to_string(),chrono::Utc::now().timestamp_millis()])?;
    tx.execute("INSERT INTO agent_delegate_executors(agent_id,owner_id,lease_until) VALUES (?1,?2,?3)",
        params![id,instance(),now_ms()+LEASE_MS])?;
    if let Some(assets) = shared_context.get("delegation_assets") {
        remember_assets(&tx, task, &id, assets)?;
    }
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
        let mut execution = tokio::spawn(crate::single_agent::run_delegate(
            state.clone(),
            task.clone(),
            model,
            turn,
            job.clone(),
            node,
            ban_execution,
            shared_context,
            child.clone(),
        ));
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(5));
        let outcome = loop {
            tokio::select! {
                result = &mut execution => break result,
                _ = heartbeat.tick() => {
                    if let Ok(conn) = db(state.workspace.root()) {
                        let cancel_requested = conn.query_row(
                            "SELECT cancel_requested FROM agent_delegate_executors WHERE agent_id=?1 AND owner_id=?2",
                            params![job,instance()], |r| r.get::<_,bool>(0)).unwrap_or(false);
                        if cancel_requested { child.cancel(); }
                        let updated = conn.execute("UPDATE agent_delegate_executors SET lease_until=?3 WHERE agent_id=?1 AND owner_id=?2 AND EXISTS (SELECT 1 FROM agent_delegates WHERE id=?1 AND status='running')",
                            params![job,instance(),now_ms()+LEASE_MS]);
                        if !matches!(updated,Ok(1)) { child.cancel(); }
                    }
                }
            }
        };
        let mut result = match outcome {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                json!({"status":if child.is_cancelled(){"cancelled"}else{"failed"},"error":format!("{error:#}")})
            }
            Err(error) => {
                json!({"status":"interrupted","error":format!("delegate executor stopped: {error}")})
            }
        };
        if let Ok(spec) = specification(state.workspace.root(), &task, &job) {
            match crate::delegate_git::finish(&spec) {
                Ok(Some(git)) => result["git"] = git,
                Ok(None) => {}
                Err(error) => {
                    result["status"] = json!("failed");
                    result["error"] = json!(format!("could not snapshot delegate Git changes: {error:#}"));
                }
            }
        }
        let mut persist = || -> Result<()> {
            let mut conn = db(state.workspace.root())?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            result["artifact_index"] = artifact_index(&tx, &task, &job)?;
            let updated = tx.execute(
                "UPDATE agent_delegates SET status=?3,result=?4 WHERE id=?1 AND status='running' AND EXISTS (SELECT 1 FROM agent_delegate_executors WHERE agent_id=?1 AND owner_id=?2)",
                params![job,instance(),result["status"].as_str().unwrap_or("failed"),result.to_string()])?;
            if updated == 0 { return Ok(()); }
            crate::task_tree::finish_delegate(&tx,&task,&job,&result,turn)?;
            let seq = agent_service::append_event_tx(
                &tx,
                &task,
                "subagent/end",
                &json!({"agent_id":job,"turn":turn,"result":result,"status":result["status"]}),
                Some("append"),
            )?;
            tx.execute("UPDATE agent_delegates SET end_seq=?2 WHERE id=?1",params![job,seq])?;
            tx.execute("DELETE FROM agent_resource_claims WHERE owner_agent=?1",[&job])?;
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
    // Status polling is a read. Taking the writer lock here makes a waiting
    // parent compete with its children saving events, heartbeats and outcomes.
    let ids = conn
        .prepare("SELECT d.id,e.owner_id,e.lease_until FROM agent_delegates d LEFT JOIN agent_delegate_executors e ON e.agent_id=d.id WHERE d.task_id=?1 AND d.status='running'")?
        .query_map([task], |r| Ok((r.get::<_, String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<i64>>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let orphaned = |id: &String, owner: &Option<String>, lease: &Option<i64>| {
        let local = owner.as_deref() == Some(instance());
        (local && !executors.contains_key(id)) || (!local && lease.is_none_or(|until| until < now_ms()))
    };
    if !ids.iter().any(|(id, owner, lease)| orphaned(id, owner, lease)) {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (id, _, _) in ids {
        // A foreign executor may have renewed its lease while we waited for
        // the writer lock. Recheck inside the recovery transaction.
        let current = tx.query_row(
            "SELECT e.owner_id,e.lease_until FROM agent_delegates d LEFT JOIN agent_delegate_executors e ON e.agent_id=d.id WHERE d.task_id=?1 AND d.id=?2 AND d.status='running'",
            params![task,id], |r| Ok((r.get::<_,Option<String>>(0)?,r.get::<_,Option<i64>>(1)?))).optional()?;
        if let Some((owner, lease)) = current && orphaned(&id, &owner, &lease) {
            let result = json!({"status":"interrupted","error":"No live executor after restart. Inspect saved artifacts; the host did not replay the task.","artifact_index":artifact_index(&tx,task,&id)?});
            tx.execute(
                "UPDATE agent_delegates SET status='interrupted',result=?2 WHERE id=?1",
                params![id, result.to_string()],
            )?;
            crate::task_tree::finish_delegate(&tx,task,&id,&result,0)?;
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
            tx.execute("DELETE FROM agent_resource_claims WHERE owner_agent=?1",[&id])?;
            discard_working_context(&tx, task, &id)?;
        }
    }
    tx.commit()?;
    Ok(())
}
pub fn statuses(root: &Path, task: &str) -> Result<Value> {
    reap(root, task)?;
    let conn = db(root)?;
    let mut stmt=conn.prepare("SELECT id,name,status,delivered,review,COALESCE(json_extract(specification,'$.program_function'),0) FROM agent_delegates WHERE task_id=?1 ORDER BY created_at,rowid")?;
    Ok(json!(stmt.query_map([task],|r|Ok(json!({"agent_id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"delivered":r.get::<_,bool>(3)?,"program_function":r.get::<_,bool>(5)?,"review":r.get::<_,Option<String>>(4)?.map(|s|serde_json::from_str::<Value>(&s).unwrap_or(Value::Null))})))?.collect::<rusqlite::Result<Vec<_>>>()?))
}
pub fn ready(root: &Path, task: &str) -> Result<Vec<Value>> {
    reap(root, task)?;
    let conn = db(root)?;
    let mut stmt=conn.prepare("SELECT id,name,result,end_seq FROM agent_delegates WHERE task_id=?1 AND status!='running' AND delivered=0 ORDER BY end_seq")?;
    Ok(stmt.query_map([task],|r|Ok(json!({"agent_id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"result":serde_json::from_str::<Value>(&r.get::<_,String>(2)?).unwrap_or(Value::Null),"end_seq":r.get::<_,i64>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?)
}
pub fn outstanding(root: &Path, task: &str) -> Result<bool> {
    reap(root, task)?;
    Ok(db(root)?.query_row("SELECT EXISTS(SELECT 1 FROM agent_delegates WHERE task_id=?1 AND (status='running' OR delivered=0 OR (review IS NULL AND COALESCE(json_extract(specification,'$.program_function'),0)=0)))",[task],|r|r.get(0))?)
}
/// Only work submitted by this task may delay or receive its completion.
pub fn outstanding_for(root: &Path, task: &str, owner: &str) -> Result<bool> {
    reap(root,task)?;
    Ok(db(root)?.query_row("SELECT EXISTS(SELECT 1 FROM agent_delegates WHERE task_id=?1 AND (status='running' OR delivered=0) AND (json_extract(specification,'$.caller_task_node_id')=?2 OR json_extract(specification,'$.caller_task_node_id') IS NULL))",params![task,owner],|r|r.get(0))?)
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

/// A join is an idempotent wait over durable fork handles, not a new model
/// turn or an acceptance decision. No connection/transaction lives across await.
pub async fn join(
    root: &Path,
    task: &str,
    owner: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let ids = args["agent_ids"]
        .as_array()
        .context("agent_ids must be an array")?;
    ensure!(
        !ids.is_empty() && ids.len() <= LIMIT,
        "join requires 1..={LIMIT} agent handles"
    );
    let ids = ids
        .iter()
        .map(|id| {
            id.as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .context("agent_ids must contain nonempty strings")
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len() == ids.len(),
        "duplicate join handle"
    );
    for id in &ids {
        let spec = specification(root, task, id)?;
        ensure!(
            spec["caller_task_node_id"]
                .as_str()
                .is_none_or(|caller| caller == owner),
            "JOIN_CALLER_MISMATCH"
        );
    }
    loop {
        ensure!(!cancel.is_cancelled(), "cancelled");
        // Register before examining statuses so notify_waiters cannot be lost.
        let changed = CHANGED.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        reap(root, task)?;
        let joined = {
            let conn = db(root)?;
            ids.iter().map(|id| conn.query_row(
                "SELECT name,status FROM agent_delegates WHERE task_id=?1 AND id=?2", params![task,id],
                |r| Ok(json!({"agent_id":id,"name":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?})))
            ).collect::<rusqlite::Result<Vec<_>>>()?
        };
        if joined.iter().all(|child| child["status"] != "running") {
            return Ok(json!({"joined":true,"delegates":joined,"acceptance":"unreviewed"}));
        }
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("cancelled"),
            _ = &mut changed => {},
            // Recheck leases after a missed notification or another host's exit.
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
        }
    }
}
fn terminal_git(root: &Path, task: &str, id: &str) -> Result<Value> {
    let conn = db(root)?;
    let (status,result): (String,Option<String>) = conn.query_row(
        "SELECT status,result FROM agent_delegates WHERE task_id=?1 AND id=?2",params![task,id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(status != "running", "delegate is still running");
    let spec = specification(root,task,id)?;
    ensure!(spec["git"].is_object(), "delegate has no Git branch");
    let recorded: Value=serde_json::from_str(result.as_deref().context("delegate result missing")?)?;
    let head=recorded["git"]["head"].as_str().context("delegate Git snapshot missing")?;
    ensure!(crate::delegate_git::diff(root,&spec)?["head"].as_str()==Some(head),
        "delegate branch changed after its archived result");
    Ok(spec)
}
pub fn inspect_changes(root: &Path, task: &str, id: &str) -> Result<Value> {
    crate::delegate_git::diff(root,&terminal_git(root,task,id)?)
}
pub fn merge_result(root: &Path, task: &str, id: &str, head: &str) -> Result<Value> {
    let spec = terminal_git(root,task,id)?;
    ensure!(head.len() == 40 && head.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected_head must be a reviewed Git commit");
    let inspected: bool = db(root)?.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_task_events c JOIN agent_task_events r ON r.task_id=c.task_id AND r.kind='tool/result' AND json_extract(r.data,'$.message.toolCallId')=json_extract(c.data,'$.callId') WHERE c.task_id=?1 AND c.kind='tool/call' AND json_extract(c.data,'$.name')='inspect_agent_changes' AND json_extract(json_extract(c.data,'$.arguments'),'$.agent_id')=?2 AND json_extract(r.data,'$.meta.result.head')=?3 AND json_extract(r.data,'$.message.isError')=0)",
        params![task,id,head],|r|r.get(0))?;
    ensure!(inspected,"inspect the delegate branch diff before merging");
    crate::delegate_git::merge(root,&spec,head)
}
fn checked_receipt(tx: &rusqlite::Transaction<'_>, task: &str, seq: i64, after: i64) -> Result<(String,Value)> {
    ensure!(seq > after, "evidence event predates delegate completion");
    let raw: String = tx.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND seq=?2 AND kind='tool/result'",
        params![task,seq],|r|r.get(0)).context("evidence_refs must cite main Agent tool results")?;
    let result: Value = serde_json::from_str(&raw)?;
    ensure!(result["message"]["isError"] == false,"evidence receipt is a failed tool result");
    let call_id = result["message"]["toolCallId"].as_str().context("evidence receipt has no call id")?;
    let name: String = tx.query_row("SELECT json_extract(data,'$.name') FROM agent_task_events WHERE task_id=?1 AND kind='tool/call' AND seq<?2 AND json_extract(data,'$.callId')=?3 ORDER BY seq DESC LIMIT 1",
        params![task,seq,call_id],|r|r.get(0)).context("evidence receipt has no main Agent tool call")?;
    Ok((name,result["meta"]["result"].clone()))
}
fn fresh_local_read(root: &Path, value: &Value) -> Result<bool> {
    let Some(path) = value["path"].as_str() else { return Ok(false); };
    let Some(hash) = value["code_hash"].as_str() else { return Ok(false); };
    let path = crate::file_edit::workspace_path(root,path)?;
    let current = match std::fs::read(path) { Ok(bytes)=>bytes, Err(_)=>return Ok(false) };
    Ok(crate::symbol_description::content_hash(&current)==hash)
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
    let spec = specification(root,task,id)?;
    // Delivered files live in the task's project; `root` only holds the store.
    let files = crate::session_browser::task_workspace(root,task)?;
    let mut conn = db(root)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let refs = args["evidence_refs"].as_array().cloned().unwrap_or_default();
    ensure!(refs.len() <= 64,"too many evidence_refs");
    let (end_seq,child_result): (i64,String) = tx.query_row("SELECT end_seq,result FROM agent_delegates WHERE task_id=?1 AND id=?2 AND status!='running' AND delivered=1",
        params![task,id],|r|Ok((r.get(0)?,r.get(1)?))).context("delegate unavailable: collect its terminal result before review")?;
    let child_result: Value=serde_json::from_str(&child_result)?;
    let mut receipts = Vec::new();
    for seq in &refs {
        receipts.push(checked_receipt(&tx,task,seq.as_i64().context("evidence_refs must be event sequence numbers")?,end_seq)?);
    }
    if decision == "accepted" {
        ensure!(!receipts.is_empty(),"accepted review requires cited main Agent tool results");
        if spec["git"].is_object() {
            let head = args["git_head"].as_str().context("accepted Git review requires git_head from inspected diff")?;
            ensure!(child_result["git"]["head"].as_str()==Some(head),
                "reviewed Git commit differs from the delegate's archived result");
            ensure!(receipts.iter().any(|(name,value)| name=="inspect_agent_changes" && value["head"].as_str()==Some(head) && value["branch"]==spec["git"]["branch"]),
                "accepted Git review must cite the inspected branch diff");
            ensure!(receipts.iter().any(|(name,value)| name=="merge_agent_result" && value["merged_head"].as_str()==Some(head)),
                "accepted Git review must cite the merge result");
            ensure!(crate::delegate_git::is_merged(root,&spec,head)?,"delegate commit is not present in the main branch");
            let changed=crate::delegate_git::changed_paths(root,&spec)?;
            let existing=changed.iter().filter(|path| files.join(path).is_file()).collect::<Vec<_>>();
            if !existing.is_empty() {
                ensure!(receipts.iter().any(|(name,value)|
                    matches!(name.as_str(),"read_file"|"read_file_lines")
                    && value["path"].as_str().is_some_and(|path|existing.iter().any(|changed|changed.as_str()==path))
                    && fresh_local_read(&files,value).unwrap_or(false)),
                    "accepted Git review must cite a current read of an integrated changed file: after merging, read_file one of {:?} and add that tool result's event_seq to evidence_refs",
                    existing.iter().take(8).collect::<Vec<_>>());
            }
        } else {
            let output_paths=child_result["artifact_index"]["assets"].as_array().into_iter().flatten()
                .filter(|asset| {
                    asset["source_tool"].as_str().is_some_and(|tool| {
                        matches!(tool,"write_file"|"edit_file"|"replace_range")
                    })
                })
                .filter_map(|asset|asset["source_path"].as_str()).collect::<Vec<_>>();
            let mut inspected = false;
            for (name,value) in &receipts {
                if name=="read_file" || name=="read_file_lines" {
                    inspected |= value["path"].as_str().is_some_and(|path| {
                        output_paths.is_empty() || output_paths.contains(&path)
                    })
                        && fresh_local_read(&files,value)?;
                } else if name.ends_with("__read_file") {
                    let path=value["data"]["path"].as_str().or_else(||value["path"].as_str());
                    inspected |= path.is_some_and(|path| {
                        output_paths.is_empty() || output_paths.contains(&path)
                    })
                        && (value["data"]["revision"].is_number() || value["revision"].is_number());
                } else if name=="analyze_images" {
                    inspected |= value["status"]=="succeeded" || value["status"]=="completed";
                }
            }
            ensure!(inspected,"accepted review must cite a current artifact inspection with a checked version");
        }
    }
    let value = json!({"decision":decision,"evidence":evidence,"evidence_refs":refs,"git_head":args["git_head"]});
    if let Some(node_id)=spec["task_node_id"].as_str() {
        let mut node=crate::task_tree::get(&tx,task,node_id)?;
        ensure!(decision!="accepted" || node.status=="done","cannot accept a delegate without a verified done return");
        node.review=Some(decision.to_owned());
        crate::task_tree::emit(&tx,task,"task/node_reviewed",&mut node,0)?;
    }
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
    db(root)?.execute("UPDATE agent_delegate_executors SET cancel_requested=1 WHERE agent_id=?1",[id])?;
    if let Some((path, parent, token)) = running().lock().unwrap().get(id) {
        ensure!(path == root && parent == task, "delegate scope denied");
        token.cancel();
    }
    Ok(json!({"agent_id":id,"cancellation_requested":true}))
}
pub fn cancel_all(root: &Path, task: &str) {
    if let Ok(conn) = db(root) {
        let _ = conn.execute("UPDATE agent_delegate_executors SET cancel_requested=1 WHERE agent_id IN (SELECT id FROM agent_delegates WHERE task_id=?1 AND status='running')",[task]);
    }
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
    async fn join_waits_for_all_selected_handles_and_preserves_failed_returns() {
        let root = std::env::temp_dir().join(format!("join-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let conn = db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','','fake','running',0,0)",[]).unwrap();
        for id in ["fast", "slow", "unrelated"] {
            conn.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES (?1,'t',1,?1,?1,?2,'running',0)",
                params![id,json!({"caller_task_node_id":"parent"}).to_string()]).unwrap();
            conn.execute(
                "INSERT INTO agent_delegate_executors VALUES (?1,'other-live-host',?2,0)",
                params![id, now_ms() + 60_000],
            )
            .unwrap();
        }
        let cancel = CancellationToken::new();
        let args = json!({"agent_ids":["slow","fast"]});
        let waiting = join(&root, "t", "parent", &args, &cancel);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        conn.execute(
            "UPDATE agent_delegates SET status='completed' WHERE id='fast'",
            [],
        )
        .unwrap();
        CHANGED.notify_waiters();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiting)
                .await
                .is_err(),
            "a fast return must not resume the caller before the other selected child"
        );
        conn.execute(
            "UPDATE agent_delegates SET status='failed' WHERE id='slow'",
            [],
        )
        .unwrap();
        CHANGED.notify_waiters();
        let value = tokio::time::timeout(std::time::Duration::from_secs(2), &mut waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(value["delegates"][0]["agent_id"], "slow");
        assert_eq!(value["delegates"][0]["status"], "failed");
        assert_eq!(value["delegates"][1]["status"], "completed");
        assert_eq!(value["acceptance"], "unreviewed");
        assert_eq!(
            join(&root, "t", "parent", &args, &cancel).await.unwrap(),
            value,
            "resuming a durable join must not replay the children"
        );
        assert!(
            join(&root, "t", "other-parent", &args, &cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("JOIN_CALLER_MISMATCH")
        );
        assert!(
            join(
                &root,
                "t",
                "parent",
                &json!({"agent_ids":["fast","fast"]}),
                &cancel
            )
            .await
            .is_err()
        );
        assert!(
            join(&root, "t", "parent", &json!({"agent_ids":[]}), &cancel)
                .await
                .is_err()
        );
        assert!(
            join(
                &root,
                "t",
                "parent",
                &json!({"agent_ids":["missing"]}),
                &cancel
            )
            .await
            .is_err()
        );
        cancel.cancel();
        assert!(
            join(
                &root,
                "t",
                "parent",
                &json!({"agent_ids":["unrelated"]}),
                &cancel
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
        );
        assert_eq!(
            conn.query_row(
                "SELECT status FROM agent_delegates WHERE id='unrelated'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "running"
        );
    }

    #[test]
    fn polling_live_delegates_does_not_wait_for_a_database_writer() {
        let root = std::env::temp_dir().join(format!("delegate-poll-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let mut conn = db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('poll-parent','test','fake','running',0,0)",[]).unwrap();
        conn.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES ('poll-child','poll-parent',1,'call','work','{}','running',0)",[]).unwrap();
        conn.execute("INSERT INTO agent_delegate_executors(agent_id,owner_id,lease_until) VALUES ('poll-child','other-host',?1)",[now_ms()+60_000]).unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        tx.execute("UPDATE agent_tasks SET updated_at=1 WHERE id='poll-parent'",[]).unwrap();
        let started = std::time::Instant::now();
        let status = statuses(&root, "poll-parent").unwrap();
        assert_eq!(status[0]["status"], "running");
        assert!(ready(&root, "poll-parent").unwrap().is_empty());
        assert!(outstanding(&root, "poll-parent").unwrap());
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "read-only polling must not wait for the writer timeout");
        tx.commit().unwrap();
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn resource_claim_blocks_other_writers_until_delegate_finishes() {
        let root=std::env::temp_dir().join(format!("delegate-claim-{}",agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let conn=db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        for id in ["a","b"] {
            conn.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES (?1,'task',1,?1,?1,'{}','running',0)",[id]).unwrap();
            conn.execute("INSERT INTO agent_delegate_executors(agent_id,owner_id,lease_until) VALUES (?1,'host',?2)",params![id,now_ms()+LEASE_MS]).unwrap();
        }
        let first=claim_resource(&root,"task",Some("a"),"call","local:/project/file.rs".into()).unwrap();
        assert!(claim_resource(&root,"task",Some("b"),"call","local:/project/file.rs".into()).is_err());
        assert!(claim_resource(&root,"task",None,"main-call","local:/project/file.rs".into()).is_err());
        drop(first);
        assert!(claim_resource(&root,"task",Some("b"),"call","local:/project/file.rs".into()).is_err());
        conn.execute("UPDATE agent_delegates SET status='completed' WHERE id='a'",[]).unwrap();
        assert!(claim_resource(&root,"task",Some("b"),"call","local:/project/file.rs".into()).is_ok());
        drop(conn);
        let _=std::fs::remove_dir_all(root);
    }
    #[test]
    fn accepted_git_review_requires_inspected_and_merged_commit_receipts() {
        let parent=std::env::temp_dir().join(format!("delegate-review-{}",agent_service::uuid_like()));
        let root=parent.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let git=|args:&[&str]| {
            let output=std::process::Command::new("git").arg("-C").arg(&root).args(args).output().unwrap();
            assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
        };
        git(&["init","-b","main"]);
        std::fs::write(root.join("file.txt"),"before\n").unwrap();
        git(&["add","-A"]);
        git(&["-c","user.name=Test","-c","user.email=test@local","commit","-m","base"]);
        let mut conn=db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('parent','test','fake','running',0,0)",[]).unwrap();
        let worktree=crate::delegate_git::prepare(&root,"child").unwrap().unwrap();
        let spec=json!({"git":worktree});
        std::fs::write(crate::delegate_git::workspace(&root,&spec).unwrap().join("file.txt"),"after\n").unwrap();
        let head=crate::delegate_git::finish(&spec).unwrap().unwrap()["head"].as_str().unwrap().to_owned();
        let end={let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
            let seq=agent_service::append_event_tx(&tx,"parent","subagent/end",&json!({"agent_id":"child"}),Some("append")).unwrap();
            tx.commit().unwrap();seq};
        conn.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,result,delivered,end_seq,created_at) VALUES ('child','parent',1,'spawn','work',?1,'completed',?2,1,?3,0)",params![spec.to_string(),json!({"status":"completed","git":{"head":head},"artifact_index":{"assets":[]}}).to_string(),end]).unwrap();
        let rejected=review(&root,"parent",&json!({"agent_id":"child","decision":"accepted","evidence":"looks fine"}));
        assert!(rejected.is_err());
        let record=|conn:&mut rusqlite::Connection,call:&str,name:&str,args:Value,result:Value| -> i64 {
            let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
            agent_service::append_event_tx(&tx,"parent","tool/call",&json!({"callId":call,"name":name,"arguments":args.to_string()}),None).unwrap();
            let seq=agent_service::append_event_tx(&tx,"parent","tool/result",&json!({"message":{"toolCallId":call,"isError":false},"meta":{"result":result}}),None).unwrap();
            tx.commit().unwrap();seq
        };
        let inspected=inspect_changes(&root,"parent","child").unwrap();
        let inspect_seq=record(&mut conn,"inspect","inspect_agent_changes",json!({"agent_id":"child"}),inspected);
        let merged=merge_result(&root,"parent","child",&head).unwrap();
        let merge_seq=record(&mut conn,"merge","merge_agent_result",json!({"agent_id":"child","expected_head":head}),merged);
        let bytes=std::fs::read(root.join("file.txt")).unwrap();
        let read_seq=record(&mut conn,"read","read_file",json!({"path":"file.txt"}),
            json!({"path":"file.txt","code_hash":crate::symbol_description::content_hash(&bytes),"content":"after\n"}));
        let accepted=review(&root,"parent",&json!({"agent_id":"child","decision":"accepted","evidence":"Reviewed the branch diff, merged commit and integrated file","evidence_refs":[inspect_seq,merge_seq,read_seq],"git_head":head})).unwrap();
        assert_eq!(accepted["review"]["decision"],"accepted");
        let path=crate::delegate_git::workspace(&root,&spec).unwrap();
        git(&["worktree","remove","--force",path.to_str().unwrap()]);
        drop(conn);
        let _=std::fs::remove_dir_all(parent);
    }
    #[test]
    fn live_foreign_executor_is_not_reaped_and_expired_one_is() {
        let root=std::env::temp_dir().join(format!("delegate-lease-{}",agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let conn=db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('parent','x','fake','running',0,0)",[]).unwrap();
        conn.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES ('foreign','parent',1,'call','work','{}','running',0)",[]).unwrap();
        conn.execute("INSERT INTO agent_delegate_executors(agent_id,owner_id,lease_until) VALUES ('foreign','other-host',?1)",[now_ms()+LEASE_MS]).unwrap();
        assert_eq!(statuses(&root,"parent").unwrap()[0]["status"],"running");
        conn.execute("UPDATE agent_delegate_executors SET lease_until=?1 WHERE agent_id='foreign'",[now_ms()-1]).unwrap();
        assert_eq!(statuses(&root,"parent").unwrap()[0]["status"],"interrupted");
        let _=std::fs::remove_dir_all(root);
    }
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
        let args = json!({"name":"isolated task","task":"Do assigned work only","context":"shared constraints","asset_ids":["asset-a","asset-b"]});
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
        let spec = specification(&root, "parent", &ids[0]).unwrap();
        assert_eq!(spec["priority_asset_ids"], json!(["asset-a", "asset-b"]));
        assert!(spec["requirement_event_seqs"].is_array());
        assert_eq!(spec["delegation_projection"].as_bool(), Some(false));
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
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
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
