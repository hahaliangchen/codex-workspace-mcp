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
 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, delivered INTEGER NOT NULL DEFAULT 0);
 CREATE INDEX IF NOT EXISTS idx_visual_jobs_task ON agent_visual_jobs(task_id,delivered,status);",
    )?;
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
            for key in ["project_id", "page_index", "source_revision", "path"] {
                if !m[key].is_null() {
                    out[key] = m[key].clone();
                }
            }
            if out["source"] == "remote_ppt" && out["source_revision"].is_null() {
                if let Some(path) = m["path"]
                    .as_str()
                    .and_then(|p| p.strip_prefix("previews/"))
                    .and_then(|p| p.strip_suffix(".png"))
                {
                    if let Some((page, revision)) = path.split_once("-r") {
                        if let (Ok(page), Ok(revision)) =
                            (page.parse::<u64>(), revision.parse::<u64>())
                        {
                            out["page_index"] = json!(page);
                            out["source_revision"] = json!(revision);
                        }
                    }
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
    let existing:Option<(String,Option<String>)>=conn.query_row("SELECT id,result FROM agent_visual_jobs WHERE task_id=?1 AND turn=?2 AND arguments=?3 AND status IN ('queued','running','completed') ORDER BY created_at DESC LIMIT 1",params![context.task_id(),context.turn.unwrap_or(0),args_text],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
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
    conn.execute("INSERT INTO agent_visual_jobs(id,task_id,turn,status,call_id,arguments,sources,created_at,updated_at) VALUES (?1,?2,?3,'queued',?4,?5,?6,?7,?7)",params![id,context.task_id(),context.turn.unwrap_or(0),call,args_text,json!(source).to_string(),now])?;
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
        let mut result = json!({"job_id":job,"status":status,"sources":source,"result":outcome.as_ref().ok(),"error":outcome.as_ref().err().map(|e|format!("{e:#}"))});
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
    let mut stmt=conn.prepare("SELECT id,call_id,arguments,sources,result FROM agent_visual_jobs WHERE task_id=?1 AND delivered=0 AND status NOT IN ('queued','running') ORDER BY created_at,id")?;
    Ok(stmt.query_map([task],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?.map(|row|{
  let(id,call,args,sources,result)=row?;
  Ok(json!({"job_id":id,"call_id":call,"arguments":serde_json::from_str::<Value>(&args)?,"sources":serde_json::from_str::<Value>(&sources)?,"output":serde_json::from_str::<Value>(&result)?}))
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
    Ok(json!({"jobs":jobs,"pending":pending(root,task)?}))
}

pub fn cancel(root: &Path, task: &str) {
    for entry in running().lock().unwrap().values() {
        if entry.root == root && entry.task == task {
            entry.cancel.cancel();
        }
    }
}

pub fn outstanding(root: &Path, task: &str) -> Result<bool> {
    reap(root, task)?;
    Ok(db(root)?.query_row("SELECT count(*) FROM agent_visual_jobs WHERE task_id=?1 AND (status IN ('queued','running') OR delivered=0)",[task],|r|r.get::<_,usize>(0))?>0)
}

pub fn annotate_freshness(output: &mut Value, current: &Value, error: Option<&str>) {
    let sources = output["sources"].as_array().cloned().unwrap_or_default();
    let mut states = Vec::new();
    for source in sources {
        if source["source"] != "remote_ppt" {
            continue;
        }
        let project = source["project_id"].as_u64();
        let page = source["page_index"].as_u64();
        let revision = source["source_revision"].as_u64();
        let latest = current["slides"]
            .as_array()
            .and_then(|slides| slides.iter().find(|s| s["pageIndex"].as_u64() == page));
        let state =
            if current["project_id"].as_u64() != project || page.is_none() || revision.is_none() {
                "unknown"
            } else if let Some(latest) = latest {
                if latest["revision"].as_u64() == revision {
                    "current"
                } else {
                    "superseded"
                }
            } else {
                "unknown"
            };
        states.push(json!({"artifact_id":source["artifact_id"],"page_index":page,"checked_revision":revision,"current_revision":latest.map(|s|s["revision"].clone()),"freshness":state}));
    }
    output["version_checks"] = json!(states);
    if let Some(error) = error {
        output["version_check_error"] = json!(error);
    }
    output["evidence_scope"] = json!(
        "Analysis applies only to these saved pixels/revisions. Read the latest SVG/revision before editing. Superseded checks cannot approve the current page. A completed request alone is not a passing verdict; unresolved or failed checks remain incomplete."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn jobs_deduplicate_cancel_and_do_not_cross_task_boundaries() {
        use axum::{Json, Router, routing::post};
        let root =
            std::env::temp_dir().join(format!("vision-job-cancel-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let seen = Arc::new(Notify::new());
        let signaled = seen.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let signaled = signaled.clone();
                async move {
                    signaled.notify_one();
                    std::future::pending::<Json<Value>>().await
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(&root, listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        let mut context = VisualContext {
            turn: Some(1),
            identity: json!({"task_id":"task","request_id":1,"node_id":"check","runtime":"single_agent"}),
            ..Default::default()
        };
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let image =
            visual_artifacts::save(&root, &context, json!({"source":"test"}), png.get_ref())
                .unwrap();
        context
            .allowed_artifact_ids
            .push(image["artifact_id"].as_str().unwrap().to_owned());
        let args = json!({"artifact_ids":[image["artifact_id"]],"question":"Inspect pixels","background":true});
        let cancel = CancellationToken::new();
        let first = submit(&state, &context, &args, "call", "fake", &cancel).unwrap();
        let duplicate = submit(&state, &context, &args, "call-again", "fake", &cancel).unwrap();
        assert_eq!(first["job_id"], duplicate["job_id"]);
        tokio::time::timeout(std::time::Duration::from_secs(2), seen.notified())
            .await
            .unwrap();
        let mut foreign = context.clone();
        foreign.identity["task_id"] = json!("other");
        assert!(submit(&state, &foreign, &args, "foreign", "fake", &cancel).is_err());
        assert!(
            results(&root, "other", &json!({}), &cancel).await.unwrap()["jobs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while pending(&root, "task").unwrap() > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            ready(&root, "task").unwrap()[0]["output"]["status"],
            "cancelled"
        );
        server.abort();
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_versions_cannot_approve_new_pages_and_restarted_jobs_are_not_replayed() {
        let mut output = json!({"sources":[{"source":"remote_ppt","project_id":7,"page_index":2,"source_revision":1,"artifact_id":"old"}]});
        annotate_freshness(
            &mut output,
            &json!({"project_id":7,"slides":[{"pageIndex":2,"revision":2}]}),
            None,
        );
        assert_eq!(output["version_checks"][0]["freshness"], "superseded");
        annotate_freshness(&mut output, &Value::Null, Some("connection unavailable"));
        assert_eq!(output["version_checks"][0]["freshness"], "unknown");
        let root =
            std::env::temp_dir().join(format!("vision-job-restart-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let conn = db(&root).unwrap();
        conn.execute("INSERT INTO agent_visual_jobs(id,task_id,turn,status,call_id,arguments,sources,created_at,updated_at) VALUES ('stopped','task',1,'running','call','{}','[]',0,0)",[]).unwrap();
        assert_eq!(pending(&root, "task").unwrap(), 0);
        let completed = ready(&root, "task").unwrap();
        assert_eq!(completed[0]["output"]["status"], "interrupted");
        assert!(ready(&root, "foreign-task").unwrap().is_empty());
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
}
