//! Durable, read-only vision jobs. The execution Agent remains the sole editor.
use crate::{
    agent_service::{self, AgentServiceState},
    visual_artifacts::{self, VisualContext},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

struct Running {
    task: String,
    root: std::path::PathBuf,
    cancel: CancellationToken,
}
static RUNNING: OnceLock<Mutex<BTreeMap<String, Running>>> = OnceLock::new();
static CHANGED: Notify = Notify::const_new();
fn running() -> &'static Mutex<BTreeMap<String, Running>> {
    RUNNING.get_or_init(Default::default)
}
fn db(root: &Path) -> Result<rusqlite::Connection> {
    let conn = agent_service::open_db(root)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_visual_jobs(
 id TEXT PRIMARY KEY, task_id TEXT NOT NULL, turn INTEGER NOT NULL, status TEXT NOT NULL,
 call_id TEXT NOT NULL, arguments TEXT NOT NULL, sources TEXT NOT NULL, result TEXT,
 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, delivered INTEGER NOT NULL DEFAULT 0,
 owner_node_id TEXT);
 CREATE INDEX IF NOT EXISTS idx_visual_jobs_task ON agent_visual_jobs(task_id,delivered,status);",
    )?;
    if !conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_visual_jobs') WHERE name='owner_node_id')", [], |r| r.get::<_,bool>(0))? {
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        if !tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_visual_jobs') WHERE name='owner_node_id')", [], |r| r.get::<_,bool>(0))? {
            tx.execute("ALTER TABLE agent_visual_jobs ADD COLUMN owner_node_id TEXT", [])?;
        }
        tx.commit()?;
    }
    Ok(conn)
}
struct Guard(String);
impl Drop for Guard {
    fn drop(&mut self) {
        running().lock().unwrap().remove(&self.0);
        CHANGED.notify_waiters();
    }
}

pub fn tool() -> Value {
    json!({"type":"function","function":{
 "name":"image_analysis_results","description":"Read this conversation's queued visual checks. Wait only when no independent generation work remains. Results are also delivered automatically before the next model request. Never treat accepted/running as checked.",
 "parameters":{"type":"object","properties":{"wait_ms":{"type":"integer","minimum":0,"maximum":30000}},"additionalProperties":false}}})
}

pub fn sources(root: &Path, context: &VisualContext, args: &Value) -> Result<Vec<Value>> {
    args["artifact_ids"]
        .as_array()
        .context("artifact_ids missing")?
        .iter()
        .map(|id| {
            let m = visual_artifacts::metadata(
                root,
                context.task_id(),
                id.as_str().context("artifact id missing")?,
            )?;
            let mut out =
                json!({"artifact_id":id,"image_hash":m["content_hash"],"source":m["source"]});
            for key in [
                "tool",
                "project_id",
                "page_index",
                "source_revision",
                "source_hash",
                "source_stable",
                "source_binding_error",
                "path",
            ] {
                if !m[key].is_null() {
                    out[key] = m[key].clone();
                }
            }
            Ok(out)
        })
        .collect()
}

pub fn submit(
    state: &AgentServiceState,
    context: &VisualContext,
    args: &Value,
    call: &str,
    model: &str,
    cancel: &CancellationToken,
) -> Result<Value> {
    ensure!(!cancel.is_cancelled(), "cancelled");
    crate::image_analysis::validate_background(state, context, args, model)?;
    let root = state.workspace.root();
    let conn = db(root)?;
    let count:usize=conn.query_row("SELECT count(*) FROM agent_visual_jobs WHERE task_id=?1 AND status IN ('queued','running')",[context.task_id()],|r|r.get(0))?;
    ensure!(
        count < 32,
        "VISION_QUEUE_FULL: collect pending results before submitting more"
    );
    let args_text = args.to_string();
    let owner = context.identity["task_node_id"].as_str();
    // A matching inspection owned by another step cannot deliver this call's result.
    let existing:Option<(String,Option<String>)>=conn.query_row("SELECT id,result FROM agent_visual_jobs WHERE task_id=?1 AND turn=?2 AND arguments=?3 AND owner_node_id IS ?4 AND status IN ('queued','running','completed') ORDER BY created_at DESC LIMIT 1",params![context.task_id(),context.turn.unwrap_or(0),args_text,owner],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((id, result)) = existing {
        let mut value = if let Some(result) = result {
            serde_json::from_str::<Value>(&result)?
        } else {
            json!({"status":"accepted","job_id":id,"message":"Existing inspection retained; continue independent work."})
        };
        value["deduplicated"] = json!(true);
        return Ok(value);
    }
    let id = agent_service::uuid_like();
    let source = sources(root, context, args)?;
    let now = visual_artifacts::now();
    let child = cancel.child_token();
    let mut executors = running().lock().unwrap();
    conn.execute("INSERT INTO agent_visual_jobs(id,task_id,turn,status,call_id,arguments,sources,created_at,updated_at,owner_node_id) VALUES (?1,?2,?3,'queued',?4,?5,?6,?7,?7,?8)",params![id,context.task_id(),context.turn.unwrap_or(0),call,args_text,json!(source).to_string(),now,owner])?;
    executors.insert(
        id.clone(),
        Running {
            task: context.task_id().to_owned(),
            root: root.to_path_buf(),
            cancel: child.clone(),
        },
    );
    drop(executors);
    let (state, context, args, call, model, job) = (
        state.clone(),
        context.clone(),
        args.clone(),
        call.to_owned(),
        model.to_owned(),
        id.clone(),
    );
    tokio::spawn(async move {
        let _guard = Guard(job.clone());
        static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let gate = LIMIT.get_or_init(|| Arc::new(Semaphore::new(2))).clone();
        let outcome=async {
   let _permit=tokio::select!{_ = child.cancelled()=>anyhow::bail!("cancelled"),p=gate.acquire_owned()=>p?};
   db(state.workspace.root())?.execute("UPDATE agent_visual_jobs SET status='running',updated_at=?2 WHERE id=?1",params![job,visual_artifacts::now()])?;
   crate::image_analysis::execute(&state,&context,&args,&call,&model,&child).await
  }.await;
        let status = if child.is_cancelled() {
            "cancelled"
        } else if outcome.is_ok() {
            "completed"
        } else {
            "failed"
        };
        let mut result = json!({"job_id":job,"status":status,"sources":source,"task_node_id":context.identity["task_node_id"],"agent_id":context.identity["agent_id"],"result":outcome.as_ref().ok(),"error":outcome.as_ref().err().map(|e|format!("{e:#}"))});
        if let Ok(value) = &outcome {
            result["analysis"] = value["analysis"].clone();
            if let Some(fields) = result["result"].as_object_mut() {
                fields.remove("analysis");
            }
        }
        if let Ok(mut conn) = db(state.workspace.root()) {
            if let Ok(tx) = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            {
                let stored = tx.execute(
                    "UPDATE agent_visual_jobs SET status=?2,result=?3,updated_at=?4 WHERE id=?1",
                    params![job, status, result.to_string(), visual_artifacts::now()],
                );
                if stored.is_ok() {
                    let _ = agent_service::append_event_tx(
                        &tx,
                        context.task_id(),
                        "visual/job_completed",
                        &result,
                        None,
                    );
                    let _ = tx.commit();
                }
            }
        }
    });
    Ok(
        json!({"status":"accepted","job_id":id,"artifact_ids":args_text.parse::<Value>()?["artifact_ids"],"message":"Visual check queued. Continue generating later pages. Results return automatically; collect pending checks before final delivery."}),
    )
}

fn reap(root: &Path, task: &str) -> Result<()> {
    let conn = db(root)?;
    let ids = conn
        .prepare(
            "SELECT id FROM agent_visual_jobs WHERE task_id=?1 AND status IN ('queued','running')",
        )?
        .query_map([task], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for id in ids {
        if !running().lock().unwrap().contains_key(&id) {
            let failed = json!({"job_id":id,"status":"interrupted","error":"Vision executor is unavailable after restart/interruption; this page was not checked. Resubmit explicitly."});
            conn.execute("UPDATE agent_visual_jobs SET status='interrupted',result=?2,updated_at=?3 WHERE id=?1 AND status IN ('queued','running')",params![id,failed.to_string(),visual_artifacts::now()])?;
        }
    }
    Ok(())
}

pub fn pending(root: &Path, task: &str) -> Result<usize> {
    reap(root, task)?;
    Ok(db(root)?.query_row("SELECT count(*) FROM agent_visual_jobs WHERE task_id=?1 AND status IN ('queued','running')",[task],|r|r.get(0))?)
}

pub fn ready(root: &Path, task: &str) -> Result<Vec<Value>> {
    reap(root, task)?;
    let conn = db(root)?;
    let mut stmt=conn.prepare("SELECT id,call_id,arguments,sources,result,owner_node_id FROM agent_visual_jobs WHERE task_id=?1 AND delivered=0 AND status NOT IN ('queued','running') ORDER BY created_at,id")?;
    Ok(stmt.query_map([task],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?)))?.map(|row|{
  let(id,call,args,sources,result,owner)=row?;
  Ok(json!({"job_id":id,"call_id":call,"owner_node_id":owner,"arguments":serde_json::from_str::<Value>(&args)?,"sources":serde_json::from_str::<Value>(&sources)?,"output":serde_json::from_str::<Value>(&result)?}))
 }).collect::<Result<Vec<_>>>()?)
}

pub async fn wait(root: &Path, task: &str, cancel: &CancellationToken, millis: u64) -> Result<()> {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(millis.min(30000));
    loop {
        let notified = CHANGED.notified();
        if !ready(root, task)?.is_empty() || pending(root, task)? == 0 {
            return Ok(());
        }
        tokio::select! {
            _ = cancel.cancelled()=>anyhow::bail!("cancelled"),
            _ = notified=>{},
            _ = tokio::time::sleep_until(deadline)=>return Ok(()),
        }
    }
}

pub async fn results(
    root: &Path,
    task: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let millis = args
        .get("wait_ms")
        .map(|v| v.as_u64().context("wait_ms must be a nonnegative integer"))
        .transpose()?
        .unwrap_or(0);
    ensure!(millis <= 30000, "wait_ms exceeds 30000");
    if millis > 0 {
        wait(root, task, cancel, millis).await?;
    }
    let conn = db(root)?;
    let mut stmt=conn.prepare("SELECT id,status,result,sources FROM agent_visual_jobs WHERE task_id=?1 ORDER BY created_at,id")?;
    let jobs=stmt.query_map([task],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?)))?.map(|row|{
  let(id,status,result,sources)=row?;Ok(json!({"job_id":id,"status":status,"sources":serde_json::from_str::<Value>(&sources)?,"error":result.map(|s|serde_json::from_str::<Value>(&s)).transpose()?.map(|v|v["error"].clone())}))
 }).collect::<Result<Vec<_>>>()?;
    let result = json!({"jobs":jobs,"pending":pending(root,task)?});
    Ok(result)
}

pub fn cancel(root: &Path, task: &str) {
    for entry in running().lock().unwrap().values() {
        if entry.root == root && entry.task == task {
            entry.cancel.cancel();
        }
    }
}

pub fn outstanding_calls(root: &Path, task: &str) -> Result<Vec<(String, Option<String>)>> {
    reap(root, task)?;
    Ok(db(root)?.prepare("SELECT call_id,owner_node_id FROM agent_visual_jobs WHERE task_id=?1 AND (status IN ('queued','running') OR delivered=0)")?
        .query_map([task],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn outstanding(root: &Path, task: &str) -> Result<bool> {
    reap(root, task)?;
    Ok(db(root)?.query_row("SELECT count(*) FROM agent_visual_jobs WHERE task_id=?1 AND (status IN ('queued','running') OR delivered=0)",[task],|r|r.get::<_,usize>(0))?>0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn legacy_queue_migrates_and_identical_checks_keep_separate_step_owners() {
        let root =
            std::env::temp_dir().join(format!("vision-owners-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let state = agent_service::tests::test_state(&root, "127.0.0.1:9".parse().unwrap());
        let conn = agent_service::open_db(&root).unwrap();
        conn.execute_batch("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','','fake','running',0,0);
            CREATE TABLE agent_visual_jobs(id TEXT PRIMARY KEY,task_id TEXT NOT NULL,turn INTEGER NOT NULL,status TEXT NOT NULL,call_id TEXT NOT NULL,arguments TEXT NOT NULL,sources TEXT NOT NULL,result TEXT,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,delivered INTEGER NOT NULL DEFAULT 0);
            INSERT INTO agent_visual_jobs VALUES ('old-job','t',1,'completed','old-call','{}','[]','{}',0,0,1);").unwrap();
        drop(conn);
        let conn = db(&root).unwrap();
        let old: (i64, Option<String>) = conn
            .query_row(
                "SELECT delivered,owner_node_id FROM agent_visual_jobs WHERE id='old-job'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(old, (1, None));
        drop(conn);
        let mut context = VisualContext {
            identity: json!({"task_id":"t","node_id":"capture","task_node_id":"step-a","agent_id":"worker-a"}),
            turn: Some(1),
            ..Default::default()
        };
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let artifact =
            visual_artifacts::save(&root, &context, json!({"source":"test"}), png.get_ref())
                .unwrap();
        context.allowed_artifact_ids = vec![artifact["artifact_id"].as_str().unwrap().to_owned()];
        let args = json!({"artifact_ids":context.allowed_artifact_ids,"question":"Inspect the same picture","background":true});
        let cancel = CancellationToken::new();
        let a = submit(&state, &context, &args, "call_1", "fake", &cancel).unwrap();
        let repeated = submit(&state, &context, &args, "call_1", "fake", &cancel).unwrap();
        assert_eq!(a["job_id"], repeated["job_id"]);
        context.identity["task_node_id"] = json!("step-b");
        context.identity["agent_id"] = json!("worker-b");
        let b = submit(&state, &context, &args, "call_1", "fake", &cancel).unwrap();
        assert_ne!(
            a["job_id"], b["job_id"],
            "another step must get its own completion"
        );
        let owners = outstanding_calls(&root, "t").unwrap();
        assert_eq!(owners.len(), 2);
        assert!(owners.contains(&("call_1".into(), Some("step-a".into()))));
        assert!(owners.contains(&("call_1".into(), Some("step-b".into()))));
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while [a["job_id"].as_str().unwrap(), b["job_id"].as_str().unwrap()]
                .iter()
                .any(|id| running().lock().unwrap().contains_key(*id))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(ready(&root, "t").unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }
}
