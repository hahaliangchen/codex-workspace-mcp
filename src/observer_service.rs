//! Advisory node reviews. SQLite commits own boundary work; a bounded wakeup is
//! only a notification. There is one model call per session, never a Worker gate.
use crate::agent_service::{self, AgentServiceState};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;

pub const INPUT_BUDGET: usize = 12_000;

pub fn identity(task: &str, scheduler: &crate::work_scheduler::WorkScheduler, work: &str) -> Value {
    let order = scheduler.frames.get(work).map(|frame| &frame.order);
    json!({"task_id":task,"request_id":scheduler.request_started_turn,"work_id":work,
        "node_id":order.map(|o|&o.node_id),"revision":order.map(|o|o.revision),"plan_revision":order.map(|o|o.plan_revision)})
}

pub fn applies(identity: &Value, scope: &Value) -> bool {
    valid_instance(identity, scope) && identity["plan_revision"] == scope["plan_revision"]
}

fn valid_instance(identity: &Value, scope: &Value) -> bool {
    let work = identity["work_id"].as_str().unwrap_or("");
    let frame = &scope["frames"][work];
    identity["request_id"] == scope["request_id"]
        && frame["invalidated_by_plan_revision"].is_null()
        && !frame.is_null()
        && identity["plan_revision"] == frame["order"]["plan_revision"]
        && identity["revision"] == frame["order"]["revision"]
        && identity["node_id"] == frame["order"]["node_id"]
}

fn short(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}
const ARRAY_ITEMS: usize = 12;

/// Shortens long text and long lists but keeps every object key: a status code
/// or sample time must not vanish because it sorts after other fields.
fn compact(value: &Value, limit: usize) -> Value {
    match value {
        Value::String(text) => json!(short(text, limit)),
        Value::Array(items) => {
            let mut kept = items
                .iter()
                .take(ARRAY_ITEMS)
                .map(|v| compact(v, (limit / 2).max(160)))
                .collect::<Vec<_>>();
            if items.len() > ARRAY_ITEMS {
                kept.push(json!({"omitted_items":items.len()-ARRAY_ITEMS}));
            }
            json!(kept)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), compact(v, (limit / 2).max(160))))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// Late or replayed advice is matched against the node's newer facts. It stays
/// visible, but the Organizer is told the current facts take precedence.
pub fn annotate_advice(items: Vec<Value>, scheduler: &crate::work_scheduler::WorkScheduler) -> Vec<Value> {
    items
        .into_iter()
        .map(|mut item| {
            let work = item["identity"]["work_id"].as_str().unwrap_or("");
            let latest = scheduler.latest_fact_at(work);
            if let Some(observed) = item["observed_at"].as_u64() {
                if latest > observed {
                    item["based_on_older_facts"] = json!(true);
                    item["newer_facts_at"] = json!(latest);
                }
            }
            item
        })
        .collect()
}

fn fact_texts(facts: &Value) -> Value {
    json!(facts.as_array().into_iter().flatten().filter_map(|fact|fact["text"].as_str()).map(|text|short(text,400)).collect::<Vec<_>>())
}

/// The node's actual state: status, reports (intent), returned result and the
/// recorded HTTP/process/browser/check values with their times.
fn node_state(facts: &Value) -> Value {
    if facts.is_null() {
        return Value::Null;
    }
    let mut state = compact(facts, 1200);
    if let Some(object) = state.as_object_mut() {
        object.remove("operations");
    }
    state
}

/// Goal, decision and delivery summary are protected. Optional fields are
/// admitted by priority; pathological protected inputs report budget overflow.
pub fn pack(mut input: Value, path: &[Value], memories: &[Value]) -> Value {
    let mut packed = json!({"identity":input["identity"],"review_id":input["review_id"],"source_event_id":input["source_event_id"],"source_event_seq":input["source_event_seq"],
        "organizer_decision_id":input["organizer_decision_id"],"stage":input["stage"],"request":input["request"],
        "organizer_decision":compact(&input["organizer_decision"],1500),"delivery":{"summary":input["delivery"]["summary"],
            "status":input["delivery"]["status"],"exported_data":compact(&input["delivery"]["exported_data"],1200),
            "handoff_summary":input["delivery"]["handoff"]["summary"],"intent":input["delivery"]["handoff"]["intent"],
            "stall":input["delivery"]["handoff"]["stall"],
            "upstream_problem":compact(&input["delivery"]["handoff"]["upstream_problem"],800)},
        "current_node":{"goal":input["current_node"]["goal"],"done_when":input["current_node"]["done_when"]},
        "observed_at":input["observed_at"],"node_state":node_state(&input["node_facts"]),
        "current_facts":{"http":fact_texts(&input["current_facts"]["http"]),"browser":compact(&input["current_facts"]["browser"],600),
            "earlier_observations":fact_texts(&input["current_facts"]["historical_observations"])},
        "visual_artifacts":input["visual_artifacts"],"visual_check_result":input["visual_check_result"],"visual_capture":input["visual_capture"],"allowed_visual_artifact_ids":input["allowed_visual_artifact_ids"]});
    let optional = [
        ("node_operations", compact(&input["node_facts"]["operations"], 1600)),
        ("activity_summary", compact(&input["activity_summary"], 700)),
        ("resolved_inputs", compact(&input["resolved_inputs"], 1200)),
        ("materials", compact(&input["materials"], 400)),
        ("plan_overview", compact(&input["plan_overview"], 300)),
        ("known_read_targets", input["known_read_targets"].take()),
        (
            "path_summary",
            json!(
                path.iter()
                    .rev()
                    .take(6)
                    .map(|v| compact(v, 600))
                    .collect::<Vec<_>>()
            ),
        ),
        (
            "related_memory",
            json!(
                memories
                    .iter()
                    .take(3)
                    .map(|v| compact(v, 800))
                    .collect::<Vec<_>>()
            ),
        ),
    ];
    for (key, value) in optional {
        let mut candidate = packed.clone();
        candidate[key] = value;
        if candidate.to_string().chars().count() + 150 <= INPUT_BUDGET {
            packed = candidate;
        }
    }
    let size = packed.to_string().chars().count();
    packed["budget"] = json!({"limit_chars":INPUT_BUDGET,"protected_overflow":size>INPUT_BUDGET,"packed_chars":size});
    packed
}

pub fn observation(
    task: &str,
    scheduler: &crate::work_scheduler::WorkScheduler,
    work: &str,
    turn: usize,
    step: usize,
    stage: &str,
    prompt: &str,
    decision: &Value,
    activity: Value,
    delivery: Value,
    materials: Value,
) -> Value {
    let id = identity(task, scheduler, work);
    let order = scheduler.frames.get(work).map(|f| &f.order);
    let frame=scheduler.frames.get(work);
    let review_id = format!(
        "{}:{}:{}:{}:{}:{}:{}:{}",
        task,
        scheduler.request_started_turn,
        work,
        id["revision"],
        id["plan_revision"],
        stage,
        turn,
        step
    );
    let packet = scheduler.worker_input(prompt);
    let allowed=packet["upstream_outputs"].as_array().into_iter().flatten().flat_map(|output|output["visual_artifact_ids"].as_array().into_iter().flatten().chain(output["exported_data"]["visual_artifact_ids"].as_array().into_iter().flatten())).cloned().collect::<Vec<_>>();
    let source_event_id = if stage == "progress" {
        format!("progress:{review_id}")
    } else {
        format!(
            "{turn}:{step}:{}:{}",
            scheduler.plan_revision,
            if stage == "handoff" { "sealed" } else { stage }
        )
    };
    json!({"identity":id,"review_id":review_id,"source_event_id":source_event_id,
        "organizer_decision_id":decision["decision_id"],"stage":stage,"turn":turn,"step":step,
        "request":{"goal":prompt,"boundary":compact(&scheduler.goal_boundary,900),"constraints":order.map(|o|&o.constraints)},
        "organizer_decision":decision,"current_node":{"goal":order.map(|o|&o.goal),"done_when":order.map(|o|&o.done_when)},
        "execution_epoch":frame.map(|f|f.epoch),"related_source_versions":frame.map(|f|&f.versions),
        "visual_required":order.is_some_and(|o|o.visual_goal.is_some() || o.constraints.iter().any(|c|c=="requires_visual")),
        "visual_artifact_ids":frame.map(|f|&f.visual_artifact_ids),"host_current_artifact_ids":frame.map(|f|&f.current_visual_artifact_ids),"visual_check_result":frame.map(|f|&f.visual_check_result),"task_page":frame.map(|f|&f.browser_page),"allowed_visual_artifact_ids":allowed,
        "resolved_inputs":packet["upstream_outputs"],"http_observations":packet["http_observations"],"materials":materials,"activity_summary":activity,
        "node_facts":scheduler.observer_facts(work),"current_facts":scheduler.current_facts(),
        "observed_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
        "delivery":delivery,"plan_overview":scheduler.frames.values().filter(|f|f.invalidated_by_plan_revision.is_none()).take(16)
            .map(|f|json!({"work_id":f.order.id,"node_id":f.order.node_id,"goal":short(&f.order.goal,180),"status":f.status})).collect::<Vec<_>>(),
        "defer_until_handoff":stage=="assignment" && order.is_some_and(|o|o.final_answer && o.completion==crate::work_scheduler::Completion::Output)})
}

/// Insert the execution version and review obligations in the same transaction.
/// The review PK also makes replayed commits idempotent.
pub async fn commit(root: &Path, task: &str, data: Value, observations: Vec<Value>) -> Result<i64> {
    let root = root.to_path_buf();
    let task = task.to_owned();
    tokio::task::spawn_blocking(move ||->Result<i64> {
        let mut conn=agent_service::open_db(&root)?;
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let kind=if data["observation_only"]==true {"observer/progress_committed"}else{"execution/commit"};
        let existing=tx.query_row("SELECT seq FROM agent_task_events WHERE task_id=?1 AND kind=?2 AND json_extract(data,'$.commit_id')=?3",
            params![task,kind,data["commit_id"].as_str()],|row|row.get::<_,i64>(0)).optional()?;
        let source_seq=if let Some(seq)=existing{seq}else{
            let source_seq=agent_service::append_event_tx(&tx,&task,kind,&data,None)?;
            if let Some(scheduler)=data.get("scheduler").filter(|value|value.is_object()) {
                agent_service::append_event_tx(&tx,&task,"scheduler/state",&json!({"commit_id":data["commit_id"],
                    "turn":data["turn"],"step":data["step"],"state":scheduler}),None)?;
            }
            if let Some(plan)=data.get("flow_plan").filter(|value|value.is_object()) {
                agent_service::append_event_tx(&tx,&task,"flow/plan",&json!({"commit_id":data["commit_id"],
                    "turn":data["turn"],"replaceCurrentTurn":true,"nodes":plan["nodes"],"edges":plan["edges"],
                    "mode":plan["mode"],"active_node_id":plan["active_node_id"],"active_path":plan["active_path"],
                    "plan_revision":plan["plan_revision"],"rewind_records":plan["rewind_records"]}),None)?;
            }
            if let Some(tree)=data.get("task_tree").filter(|value|value["nodes"].as_object().is_some_and(|nodes|!nodes.is_empty())) {
                agent_service::append_event_tx(&tx,&task,"flow/tree_state",&json!({"commit_id":data["commit_id"],
                    "turn":data["turn"],"step":data["step"],"state":tree,
                    "active_node_id":data["active_node_id"],"active_path":data["active_path"]}),None)?;
            }
            source_seq
        };
        for mut input in observations {
            input["source_event_seq"]=json!(source_seq);
            let inserted=tx.execute("INSERT OR IGNORE INTO agent_observations(task_id,review_id,request_id,input,status) VALUES (?1,?2,?3,?4,'pending')",
                params![task,input["review_id"].as_str(),input["identity"]["request_id"].as_u64(),input.to_string()])?;
            if inserted>0 {agent_service::append_event_tx(&tx,&task,"observer/node_review",&event(&input,"pending",Value::Null),None)?;}
        }
        tx.commit()?;Ok(source_seq)
    }).await?
}

fn event(input: &Value, status: &str, result: Value) -> Value {
    json!({"review_id":input["review_id"],"identity":input["identity"],"source_event_id":input["source_event_id"],
        "organizer_decision_id":input["organizer_decision_id"],"stage":input["stage"],"turn":input["turn"],"step":input["step"],
        "nodeId":input["identity"]["node_id"],"workId":input["identity"]["work_id"],"status":status,
        "decision":input["organizer_decision"],"delivery":input["delivery"],"result":result})
}

async fn records(root: PathBuf, task: String) -> Result<Vec<(Value, String, Value)>> {
    tokio::task::spawn_blocking(move ||->Result<_> {
        let conn=agent_service::open_db(&root)?;
        let mut stmt=conn.prepare("SELECT input,status,COALESCE(result,'null') FROM agent_observations WHERE task_id=?1 ORDER BY rowid")?;
        let rows=stmt.query_map([task],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?;
        rows.map(|r|{let (i,s,v)=r?;Ok((serde_json::from_str(&i)?,s,serde_json::from_str(&v)?))}).collect()
    }).await?
}

async fn save(root: &Path, task: &str, input: &Value, status: &str, result: Value) -> Result<()> {
    let (root, task, input, status) = (
        root.to_path_buf(),
        task.to_owned(),
        input.clone(),
        status.to_owned(),
    );
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = agent_service::open_db(&root)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE agent_observations SET status=?3,result=?4 WHERE task_id=?1 AND review_id=?2",
            params![
                task,
                input["review_id"].as_str(),
                status,
                result.to_string()
            ],
        )?;
        agent_service::append_event_tx(
            &tx,
            &task,
            "observer/node_review",
            &event(&input, &status, result),
            None,
        )?;
        tx.commit()?;
        Ok(())
    })
    .await?
}

fn normalize(raw: Value, input: &Value, elapsed: u128) -> Value {
    let mut recommendations = Vec::new();
    for item in raw
        .get("recommendations")
        .or_else(|| raw.get("suggestions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(3)
    {
        let adjustment = item
            .as_str()
            .or_else(|| item["adjustment"].as_str())
            .unwrap_or("");
        if adjustment.trim().is_empty() {
            continue;
        }
        let target = item["target"].as_str().unwrap_or("organizer");
        let key_text = format!(
            "{target}:{}",
            adjustment.split_whitespace().collect::<Vec<_>>().join(" ")
        );
        recommendations.push(json!({"issue_key":item["issue_key"].as_str().or_else(||raw["issue_key"].as_str()).filter(|key|!key.trim().is_empty()).map(str::to_owned).unwrap_or_else(||format!("recommendation_{}",crate::symbol_description::content_hash(key_text.as_bytes()))),
            "target":item["target"].as_str().unwrap_or("organizer"),"adjustment":short(adjustment,700),
            "source_event_ids":[input["source_event_id"]]}));
    }
    let findings=raw["findings"].as_array().into_iter().flatten().take(3).map(|item|json!({"reason":short(item["reason"].as_str().or_else(||item.as_str()).unwrap_or(""),600),"source_event_ids":[input["source_event_id"]]})).collect::<Vec<_>>();
    json!({"assessment":raw["assessment"].as_str().filter(|s|matches!(*s,"on_track"|"needs_adjustment"|"uncertain")).unwrap_or("uncertain"),
        "summary":short(raw["summary"].as_str().unwrap_or(""),900),"findings":findings,
        "recommendations":recommendations,"reusable_lessons":raw["reusable_lessons"],"elapsed_ms":elapsed as u64,
        "visual_artifacts":input["visual_artifacts"],"visual_check_result":raw["visual_check_result"],"visual_capture":input["visual_capture"]})
}

pub struct ObserverSession {
    enabled: bool,
    root: PathBuf,
    task: String,
    wake: mpsc::Sender<()>,
    scope: watch::Sender<Value>,
    progress: watch::Sender<Option<Value>>,
    stop: CancellationToken,
    job: Option<tokio::task::JoinHandle<()>>,
    model_gate: std::sync::Arc<Semaphore>,
    progress_revision: AtomicU64,
    seen: BTreeSet<String>,
    finished: bool,
    cleanup_on_drop: bool,
}
impl Drop for ObserverSession {
    fn drop(&mut self) {
        self.stop.cancel();
        if self.enabled && !self.finished && self.cleanup_on_drop {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let mut cleanup = Self {
                    enabled: true,
                    root: self.root.clone(),
                    task: self.task.clone(),
                    wake: self.wake.clone(),
                    scope: self.scope.clone(),
                    progress: self.progress.clone(),
                    stop: self.stop.clone(),
                    job: self.job.take(),
                    model_gate: self.model_gate.clone(),
                    progress_revision: AtomicU64::new(self.progress_revision.load(Ordering::Relaxed)),
                    seen: BTreeSet::new(),
                    finished: false,
                    cleanup_on_drop: false,
                };
                // Unexpected Worker errors still terminate observation; this
                // cleanup only writes status metadata, never calls a model.
                runtime.spawn(async move {
                    if let Err(error) = cleanup.finish("interrupted").await {
                        tracing::warn!(%error,"could not finalize interrupted Observer records");
                    }
                });
            }
        }
    }
}

impl ObserverSession {
    pub fn start(
        state: AgentServiceState,
        model: String,
        task: String,
        cancel: &CancellationToken,
    ) -> Self {
        let enabled = state.observer_enabled;
        let root = state.workspace.root().to_path_buf();
        let (wake, receiver) = mpsc::channel(1);
        let (scope, scopes) = watch::channel(Value::Null);
        let (progress, progresses) = watch::channel(None);
        let stop = cancel.child_token();
        let model_gate = std::sync::Arc::new(Semaphore::new(1));
        let call_gate = model_gate.clone();
        let job = enabled.then(|| {
            tokio::spawn(run(
                state,
                model,
                task.clone(),
                receiver,
                scopes,
                progresses,
                stop.clone(),
                call_gate,
            ))
        });
        Self {
            enabled,
            root,
            task,
            wake,
            scope,
            progress,
            stop,
            job,
            model_gate,
            progress_revision: AtomicU64::new(0),
            seen: BTreeSet::new(),
            finished: false,
            cleanup_on_drop: true,
        }
    }
    pub fn set_scope(&self, scheduler: &crate::work_scheduler::WorkScheduler, prompt: &str) {
        self.scope.send_replace(json!({"request_id":scheduler.request_started_turn,"plan_revision":scheduler.plan_revision,
            "frames":scheduler.frames,"goal":prompt,"finished":scheduler.finished}));
        self.notify();
    }
    pub async fn model_permit(&self) -> Result<OwnedSemaphorePermit> {
        Ok(self.model_gate.clone().acquire_owned().await?)
    }
    pub fn notify(&self) {
        if self.enabled {
            let _ = self.wake.try_send(());
        }
    }
    pub fn progress(&self, mut input: Value) {
        if self.enabled {
            let revision=self.progress_revision.fetch_add(1,Ordering::Relaxed).saturating_add(1);
            let review_id=format!("{}:update-{revision}",input["review_id"].as_str().unwrap_or("progress"));
            input["review_id"]=json!(review_id);
            input["source_event_id"]=json!(format!("progress:{}",input["review_id"].as_str().unwrap_or("progress")));
            input["progress_sequence"]=json!(revision);
            self.progress.send_replace(Some(input));
            self.notify();
        }
    }
    pub async fn reviews(&mut self) -> Result<Vec<Value>> {
        if !self.enabled {
            return Ok(vec![]);
        }
        let rows = records(self.root.clone(), self.task.clone()).await?;
        let mut reviews = Vec::new();
        for (input, status, result) in rows {
            if status != "completed" {
                continue;
            }
            let id = input["review_id"].as_str().unwrap_or("").to_owned();
            if !self.seen.insert(id) {
                continue;
            }
            for item in result["recommendations"].as_array().into_iter().flatten() {
                reviews.push(json!({"request_id":input["identity"]["request_id"],"identity":input["identity"],"review_id":input["review_id"],
                    "source_event_id":input["source_event_id"],"source_event_seq":input["source_event_seq"],"turn":input["turn"],"stage":input["stage"],"step":input["step"],"node_id":input["identity"]["node_id"],
                    "observed_at":input["observed_at"],"execution_revision":input["identity"]["revision"],
                    "category":"planning","issue_key":item["issue_key"],"summary":result["summary"],"suggestions":[item["adjustment"]],"target":item["target"],"findings":result["findings"],
                    "visual_artifacts":result["visual_artifacts"],"visual_check_result":result["visual_check_result"]}));
            }
        }
        Ok(reviews)
    }
    pub async fn finish(&mut self, outcome: &str) -> Result<()> {
        if !self.enabled || self.finished {
            return Ok(());
        }
        self.finished = true;
        self.stop.cancel();
        if let Some(mut job) = self.job.take() {
            if tokio::time::timeout(Duration::from_secs(2), &mut job)
                .await
                .is_err()
            {
                job.abort();
                let _ = job.await;
            }
        }
        let scope = self.scope.borrow().clone();
        let rows = records(self.root.clone(), self.task.clone()).await?;
        let mut completed = 0;
        let mut unevaluated = 0;
        let mut elapsed = 0;
        let mut summaries = Vec::new();
        for (input, status, result) in &rows {
            if input["identity"]["request_id"] != scope["request_id"] {
                continue;
            }
            if matches!(status.as_str(), "pending" | "unassessed")
                && input["stage"] == "assignment"
                && rows.iter().any(|(other, _, _)| {
                    other["stage"] == "handoff" && other["identity"] == input["identity"]
                })
            {
                save(
                    &self.root,
                    &self.task,
                    input,
                    "merged",
                    json!({"reason":"included in handoff"}),
                )
                .await?;
                continue;
            }
            if matches!(status.as_str(), "pending" | "reviewing" | "unassessed") {
                save(
                    &self.root,
                    &self.task,
                    input,
                    if outcome == "cancelled" {
                        "cancelled"
                    } else {
                        "unassessed"
                    },
                    json!({"reason":outcome,"assessment":"uncertain"}),
                )
                .await?;
                unevaluated += 1;
            } else if status == "completed" {
                completed += 1;
                elapsed += result["elapsed_ms"].as_u64().unwrap_or(0);
                summaries.push(result["summary"].clone());
            } else if status != "merged" {
                unevaluated += 1;
            }
        }
        agent_service::emit(&self.root,&self.task,"observer/retrospective",json!({"status":"completed","outcome":outcome,
            "summary":format!("{completed} 次节点观察已完成，{unevaluated} 项未评估；不追加模型调用。"),
            "pathReview":summaries.iter().rev().take(6).filter_map(Value::as_str).collect::<Vec<_>>().join("\n"),
            "completedReviews":completed,"unevaluatedReviews":unevaluated,"observerElapsedMs":elapsed,"extraWorkerRounds":0})).await
    }
}

async fn run(
    state: AgentServiceState,
    model: String,
    task: String,
    mut wake: mpsc::Receiver<()>,
    scope: watch::Receiver<Value>,
    mut progress: watch::Receiver<Option<Value>>,
    stop: CancellationToken,
    model_gate: std::sync::Arc<Semaphore>,
) {
    let root = state.workspace.root();
    let mut cached_request = Value::Null;
    let mut memories = Vec::new();
    let mut last_progress: Option<(String, String)> = None;
    let mut attempted = BTreeSet::new();
    let mut rescan = true;
    loop {
        if !rescan {
            tokio::select! {biased;_=stop.cancelled()=>break,_=wake.recv()=>{}}
        }
        rescan = false;
        if stop.is_cancelled() {
            break;
        }
        let current = scope.borrow().clone();
        if current.is_null() || current["finished"] == true {
            continue;
        }
        let rows = match records(root.to_path_buf(), task.clone()).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%task,%error,"could not load durable Observer boundaries");
                rescan = true;
                tokio::select! {_=stop.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(100))=>{}}
                continue;
            }
        };
        let mut pending = Vec::new();
        let mut path = Vec::new();
        for (input, status, result) in &rows {
            if status == "completed" && valid_instance(&input["identity"], &current) {
                path.push(json!({"identity":input["identity"],"summary":result["summary"],"delivery":input["delivery"]}));
            }
            if !matches!(status.as_str(), "pending" | "unassessed" | "reviewing")
                || attempted.contains(input["review_id"].as_str().unwrap_or(""))
            {
                continue;
            }
            if !applies(&input["identity"], &current) {
                let _ = save(
                    root,
                    &task,
                    input,
                    "superseded",
                    json!({"assessment":"uncertain"}),
                )
                .await;
                continue;
            }
            if input["stage"] == "assignment"
                && rows.iter().any(|(other, _, _)| {
                    other["identity"] == input["identity"] && other["stage"] == "handoff"
                })
            {
                let _ = save(
                    root,
                    &task,
                    input,
                    "merged",
                    json!({"reason":"included in handoff"}),
                )
                .await;
                continue;
            }
            if input["defer_until_handoff"] == true {
                continue;
            }
            pending.push(input.clone());
        }
        pending.sort_by_key(|v| if v["stage"] == "handoff" { 0 } else { 1 });
        rescan = pending.len() > 1;
        let input = pending.first().cloned().or_else(|| {
            let value = progress.borrow_and_update().clone()?;
            if !applies(&value["identity"], &current) {
                return None;
            }
            let fingerprint = compact(&value["activity_summary"], 300).to_string();
            let instance = value["identity"].to_string();
            if last_progress.as_ref().is_some_and(|(previous, key)| previous == &instance && key == &fingerprint) {
                return None;
            }
            last_progress = Some((instance, fingerprint));
            Some(value)
        });
        let Some(mut input) = input else {
            continue;
        };
        if !attempted.insert(input["review_id"].as_str().unwrap_or("").to_owned()) {
            continue;
        }
        if stop.is_cancelled() {
            break;
        }
        if cached_request != current["request_id"] {
            cached_request = current["request_id"].clone();
            memories = tokio::select! {_=stop.cancelled()=>break,result=agent_service::observer_memory_context(state.workspace.clone(),current["goal"].as_str().unwrap_or("").to_owned())=>result};
        }
        // Only versioned targets already observed by Worker can support facts.
        let (trace_root, trace_task) = (root.to_path_buf(), task.clone());
        let trace = tokio::task::spawn_blocking(move || {
            agent_service::load_observer_work_trace(&trace_root, &trace_task)
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e["turn"].as_u64().unwrap_or(0) >= current["request_id"].as_u64().unwrap_or(0))
        .collect::<Vec<_>>();
        input["known_read_targets"] =
            json!(agent_service::observer_known_read_targets(&trace, &task, 8));
        if input["stage"] == "progress" {
            if let Ok(seq) = commit(
                root,
                &task,
                json!({"commit_id":input["source_event_id"],"observation_only":true}),
                vec![input.clone()],
            )
            .await
            {
                input["source_event_seq"] = json!(seq);
            }
        }
        if stop.is_cancelled() {
            break;
        }
        if !applies(&input["identity"], &scope.borrow().clone()) {
            let _ = save(
                root,
                &task,
                &input,
                "superseded",
                json!({"assessment":"uncertain"}),
            )
            .await;
            continue;
        }
        let _ = save(root, &task, &input, "reviewing", Value::Null).await;
        let packed = pack(input.clone(), &path, &memories);
        let started = Instant::now();
        let mut metadata = input["identity"].clone();
        metadata["nodeId"] = input["identity"]["node_id"].clone();
        metadata["workId"] = input["identity"]["work_id"].clone();
        for field in [
            "review_id",
            "source_event_id",
            "source_event_seq",
            "organizer_decision_id",
            "stage",
            "turn",
            "step",
        ] {
            metadata[field] = input[field].clone();
        }
        let result = tokio::select! {biased;_=stop.cancelled()=>{
        let _=save(root,&task,&input,"unassessed",json!({"assessment":"uncertain","reason":"observation stopped"})).await;break;},
        result=tokio::time::timeout(Duration::from_secs(12),async {
            let _permit=model_gate.acquire().await?;
            anyhow::ensure!(!stop.is_cancelled() && applies(&input["identity"],&scope.borrow().clone()),"observation no longer applies");
            let mut visual_input=input.clone();
            let mut visual_context=crate::visual_artifacts::VisualContext {identity:input["identity"].clone(),execution_epoch:input["execution_epoch"].as_u64().unwrap_or(0) as usize,related_source_versions:input["related_source_versions"].clone(),current_page:input["task_page"].clone(),source_event_id:input["source_event_id"].as_str().unwrap_or("").to_owned(),
                host_current_artifact_ids:input["host_current_artifact_ids"].as_array().into_iter().flatten().filter_map(|id|id.as_str().map(str::to_owned)).collect(),allowed_artifact_ids:input["allowed_visual_artifact_ids"].as_array().into_iter().flatten().filter_map(|id|id.as_str().map(str::to_owned)).collect(),..Default::default()};
            let mut images=Vec::new();
            for id in input["visual_artifact_ids"].as_array().into_iter().flatten().chain(if input["stage"]=="assignment" {input["allowed_visual_artifact_ids"].as_array()}else{None}.into_iter().flatten()).rev().take(crate::visual_artifacts::MAX_IMAGES) {
                if let Some(id)=id.as_str() {if let Ok(item)=crate::visual_artifacts::metadata(root,&task,id) {if crate::visual_artifacts::authorize(&item,&visual_context).is_ok() {images.push(item);}}}
            }
            images.reverse();
            let stale=!images.is_empty() && !images.iter().any(|item|crate::visual_artifacts::is_current_result(item,&visual_context));
            if input["visual_required"]==true && input["stage"]!="assignment" && (images.is_empty() || stale) {
                anyhow::ensure!(!stop.is_cancelled() && applies(&input["identity"],&scope.borrow().clone()),"visual instance changed before capture");
                match crate::browser_control::observe_page_snapshot(root,&visual_context,&input["task_page"]).await {
                    Ok(snapshot)=>{
                        let artifact=snapshot["visual_artifact"].clone();
                        let page=json!({"browser_session_id":artifact["browser_session_id"],"page_id":artifact["page_id"],"page_epoch":artifact["page_epoch"],"url":artifact["url"],"viewport":artifact["viewport"]});
                        let artifact_id=artifact["artifact_id"].as_str().unwrap_or("").to_owned();
                        visual_context.current_page=page.clone();visual_context.host_current_artifact_ids=if artifact_id.is_empty(){Vec::new()}else{vec![artifact_id.clone()]};
                        visual_input["task_page"]=page;visual_input["host_current_artifact_ids"]=json!(visual_context.host_current_artifact_ids);
                        images=vec![artifact];visual_input["visual_capture"]=json!({"tool":"observe_page_snapshot","calls":1,"status":"captured"});
                    },
                    Err(error)=>{images.clear();visual_input["visual_capture"]=json!({"tool":"observe_page_snapshot","calls":1,"status":"uncertain","reason":error.to_string()});},
                }
            }else {visual_input["visual_capture"]=json!({"calls":0,"reused":images.len()});}
            visual_input["visual_artifacts"]=json!(images);
            let mut packed=packed;for field in ["visual_artifacts","visual_capture","allowed_visual_artifact_ids","host_current_artifact_ids","execution_epoch","related_source_versions","task_page"] {packed[field]=visual_input[field].clone();}
            anyhow::ensure!(!stop.is_cancelled() && applies(&input["identity"],&scope.borrow().clone()),"visual instance changed before model request");
            let raw=agent_service::observer_json_response(&state,&model,include_str!("../prompts/observer_system.md"),packed,1400,&task,metadata).await?;
            Ok::<_,anyhow::Error>((raw,visual_input))
        })=>result};
        match result {
            Ok(Ok((raw,input))) => match serde_json::from_str::<Value>(
                raw.trim()
                    .trim_start_matches("```json")
                    .trim_end_matches("```")
                    .trim(),
            ) {
                Ok(raw) => {
                    let review = normalize(raw, &input, started.elapsed().as_millis());
                    let mut lessons = Value::Null;
                    if applies(&input["identity"], &scope.borrow().clone())
                        && !review["reusable_lessons"].is_null()
                    {
                        let mut reusable=review["reusable_lessons"].clone();
                        if review["visual_artifacts"].as_array().is_some_and(|images|!images.is_empty()) {
                            if let Some(facts)=reusable["work_findings"].as_array_mut() {for fact in facts {
                                if fact["evidence_kind"].is_null() {fact["evidence_kind"]=json!("visual_observation");}
                                fact["visual_artifact_ids"]=json!(review["visual_artifacts"].as_array().into_iter().flatten().map(|image|image["artifact_id"].clone()).collect::<Vec<_>>());
                            }}
                        }
                        lessons = agent_service::save_observer_lessons(
                            &state,
                            &task,
                            current["goal"].as_str().unwrap_or(""),
                            &reusable,
                            &trace,
                        )
                        .await
                        .unwrap_or(Value::Null);
                    }
                    let mut review = review;
                    review["lessons_recorded"] = lessons;
                    let _ = save(root, &task, &input, "completed", review).await;
                }
                Err(error) => {
                    let _ = save(
                        root,
                        &task,
                        &input,
                        "failed",
                        json!({"assessment":"uncertain","message":error.to_string()}),
                    )
                    .await;
                }
            },
            Ok(Err(error)) => {
                let _ = save(
                    root,
                    &task,
                    &input,
                    "failed",
                    json!({"assessment":"uncertain","message":short(&error.to_string(),500)}),
                )
                .await;
            }
            Err(_) => {
                let _ = save(
                    root,
                    &task,
                    &input,
                    "timeout",
                    json!({"assessment":"uncertain","message":"Observer timed out"}),
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_issue_keys_follow_adjustments_instead_of_array_positions() {
        let input = json!({"source_event_id":"assignment"});
        let first = normalize(
            json!({"recommendations":[{"adjustment":"reuse sealed inputs"},{"adjustment":"repair upstream"}]}),
            &input,
            1,
        );
        let reordered = normalize(
            json!({"recommendations":[{"adjustment":"repair upstream"},{"adjustment":"reuse  sealed inputs"}]}),
            &json!({"source_event_id":"handoff"}),
            2,
        );
        assert_eq!(
            first["recommendations"][0]["issue_key"],
            reordered["recommendations"][1]["issue_key"]
        );
        assert_eq!(
            first["recommendations"][1]["issue_key"],
            reordered["recommendations"][0]["issue_key"]
        );
        assert_ne!(
            first["recommendations"][0]["issue_key"],
            reordered["recommendations"][0]["issue_key"]
        );
    }
    #[test]
    fn observer_budget_keeps_goal_and_delivery_and_limits_optional_context() {
        let input = json!({"identity":{"request_id":1},"review_id":"review","stage":"handoff","request":{"goal":"do the requested change"},
            "organizer_decision":{"reason":"specific bounded step"},"current_node":{"goal":"implement","done_when":"delivered"},
            "delivery":{"summary":"actual delivery"},"activity_summary":{"raw":"large source".repeat(3000)}});
        let path = vec![json!({"summary":"old route".repeat(1000)}); 20];
        let memory = vec![json!({"summary":"related memory".repeat(1000)}); 20];
        let packed = pack(input, &path, &memory);
        assert_eq!(packed["request"]["goal"], "do the requested change");
        assert_eq!(packed["delivery"]["summary"], "actual delivery");
        assert!(packed.to_string().chars().count() <= INPUT_BUDGET);
        assert!(
            packed["path_summary"]
                .as_array()
                .is_none_or(|a| a.len() <= 6)
        );
        assert!(
            packed["related_memory"]
                .as_array()
                .is_none_or(|a| a.len() <= 3)
        );
    }
    fn started_service() -> crate::work_scheduler::WorkScheduler {
        let url = "http://127.0.0.1:38191/api/fonts";
        let check = crate::http_probe::check_key(&json!({"url":url}));
        let mut scheduler = crate::work_scheduler::WorkScheduler::default();
        scheduler.request_started_turn = 1;
        scheduler.apply(&json!({"action":"work","reason":"start","orders":[{"id":"start","node_id":"start","goal":"Start the font service",
            "done_when":"the font API answers","completion":"check","checks":[check]}]}),false,true).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        scheduler.frame_mut().unwrap().project_observation = json!({"sampled_at":now-1500,
            "processes":[{"process_id":"proc-7","script":"dev","running":true,"ready":true,"ready_url":"http://127.0.0.1:38191/"}]});
        scheduler.observe("http_probe",&json!({"url":url}),&json!({"url":url,
            "sampled_at":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"elapsed_ms":2,"reachable":true,
            "http_status":200,"error_kind":Value::Null,"error_message":Value::Null,"check_key":check,"check_passed":true,
            "timeout_ms":5000,"reuse_window_ms":crate::http_probe::REUSE_WINDOW_MS}),false);
        assert!(scheduler.close_if_satisfied());
        scheduler
    }

    #[test]
    fn observer_sees_the_actual_startup_result_instead_of_a_field_catalog() {
        let scheduler = started_service();
        let wide = (0..24).map(|index| (format!("key_{index:02}"), json!(index))).collect::<serde_json::Map<_, _>>();
        let input = observation("task", &scheduler, "start", 1, 2, "handoff", "start the editor", &json!({"reason":"start"}),
            Value::Null, json!({"summary":"started","status":"done","exported_data":{"wide":wide}}), Value::Null);
        let packed = pack(input, &[], &[]);
        let http = &packed["node_state"]["http_observations"][0];
        assert_eq!(http["http_status"], 200);
        assert!(http["sampled_at"].is_string());
        assert_eq!(packed["node_state"]["processes"][0]["process_id"], "proc-7");
        assert_eq!(packed["node_state"]["processes"][0]["running"], true);
        assert_eq!(packed["node_state"]["checks"][0]["passed"], true);
        assert_eq!(packed["delivery"]["exported_data"]["wide"]["key_23"], 23, "no key is dropped by position");
        assert!(packed["delivery"].get("exported_fields").is_none());
        assert!(packed["current_facts"]["http"][0].as_str().unwrap().contains("HTTP 200 at"));
        assert!(packed["observed_at"].is_u64());
    }

    #[tokio::test]
    async fn latest_progress_snapshot_replaces_failure_with_same_node_recovery_facts() {
        let url="http://127.0.0.1:38194/api/fonts";
        let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
        scheduler.apply(&json!({"action":"work","reason":"inspect and start the font service","orders":[
            {"id":"fonts","node_id":"fonts","goal":"Make the font service answer","done_when":"capture actual service and HTTP state","completion":"output"}]}),false,false).unwrap();
        scheduler.activate_next().unwrap();
        let failed_at=chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true);
        scheduler.observe("http_probe",&json!({"url":url}),&json!({"url":url,"sampled_at":failed_at,"elapsed_ms":5,"reachable":false,
            "http_status":Value::Null,"error_kind":"connection_refused","error_message":"connection refused","check_passed":false,
            "timeout_ms":5000,"reuse_window_ms":crate::http_probe::REUSE_WINDOW_MS}),false);
        let before=observation("task",&scheduler,"fonts",1,2,"progress","bring up the font service",&json!({"reason":"check the service"}),
            json!({"purpose":"Probe the endpoint","actions":[{"tool":"http_probe","failed":true,"sampled_at":failed_at}]}),Value::Null,Value::Null);

        let success_at=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let process_observation=json!({"sampled_at":success_at,"processes":[{"process_id":"font-service-8080","running":true,"ready":true,
            "ready_url":"http://127.0.0.1:38194/","ready_port":38194}]});
        scheduler.observe("run_project_script",&json!({"script":"dev","background":true}),&json!({"process_id":"font-service-8080","running":true,
            "ready":true,"ready_url":"http://127.0.0.1:38194/","process_observation":process_observation}),false);
        let after=observation("task",&scheduler,"fonts",1,3,"progress","bring up the font service",&json!({"reason":"check the service"}),
            json!({"purpose":"Start and verify the font service","next_action":"load the document","actions":[
                {"tool":"http_probe","failed":true,"sampled_at":failed_at},
                {"tool":"run_project_script","failed":false,"running":true,"sampled_at":success_at}]}),Value::Null,Value::Null);

        let (wake,_wake_rx)=mpsc::channel(1);let (scope,_scope_rx)=watch::channel(Value::Null);let (progress,mut progress_rx)=watch::channel(None);
        let session=ObserverSession{enabled:true,root:PathBuf::new(),task:"task".into(),wake,scope,progress,stop:CancellationToken::new(),job:None,
            model_gate:std::sync::Arc::new(Semaphore::new(1)),progress_revision:AtomicU64::new(0),seen:BTreeSet::new(),finished:false,cleanup_on_drop:false};
        session.progress(before);
        let first=progress_rx.borrow_and_update().clone().unwrap();
        session.progress(after);
        let latest=progress_rx.borrow_and_update().clone().unwrap();
        assert_ne!(first["review_id"],latest["review_id"],"each refreshed snapshot gets its own durable review identity");
        assert_eq!(latest["progress_sequence"],2);
        assert_eq!(latest["node_facts"]["processes"][0]["process_id"],"font-service-8080");
        assert_eq!(latest["node_facts"]["processes"][0]["running"],true);
        assert_eq!(latest["node_facts"]["process_observation_sampled_at"],success_at);
        assert!(latest["activity_summary"].to_string().contains("Start and verify the font service"));
        assert!(latest["activity_summary"].to_string().contains(&failed_at),"the action history retains the prior failure timestamp");
    }

    #[test]
    fn advice_written_before_newer_node_facts_is_marked_as_older() {
        let scheduler = started_service();
        let latest = scheduler.latest_fact_at("start");
        let advice = |observed_at: u64| json!({"id":"advice_1","identity":identity("task",&scheduler,"start"),
            "observed_at":observed_at,"summary":"the startup result lacks a status code"});
        let annotated = annotate_advice(vec![advice(latest - 5_000), advice(latest + 1)], &scheduler);
        assert_eq!(annotated[0]["based_on_older_facts"], true);
        assert_eq!(annotated[0]["newer_facts_at"], latest);
        assert!(annotated[1].get("based_on_older_facts").is_none());
    }

    #[test]
    fn oversized_primary_goal_is_explicit_instead_of_silently_truncated() {
        let goal = "目标".repeat(INPUT_BUDGET);
        let packed = pack(
            json!({"request":{"goal":goal},"delivery":{"summary":"real result"}}),
            &[],
            &[],
        );
        assert_eq!(packed["request"]["goal"], goal);
        assert_eq!(packed["budget"]["protected_overflow"], true);
    }
    #[tokio::test]
    async fn durable_fast_boundaries_and_commit_replay_keep_each_delivery_once() {
        let root = std::env::temp_dir().join(format!(
            "observer-durable-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let task = "task";
        {
            let conn = agent_service::open_db(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        }
        let mut scheduler = crate::work_scheduler::WorkScheduler::default();
        scheduler.request_started_turn = 1;
        for (step, work) in ["a", "b", "c"].iter().enumerate() {
            scheduler.apply(&json!({"action":"work","reason":"next","orders":[{"id":work,"node_id":work,"goal":work,"done_when":"delivered","completion":"output"}]}),false,false).unwrap();
            for stage in ["assignment", "handoff"] {
                let input = observation(
                    task,
                    &scheduler,
                    work,
                    1,
                    step + 1,
                    stage,
                    "goal",
                    &json!({"reason":"next"}),
                    Value::Null,
                    json!({"summary":"delivered"}),
                    Value::Null,
                );
                let data = json!({"commit_id":input["source_event_id"],"scheduler":scheduler.snapshot(),"task_tree":{}});
                commit(&root, task, data.clone(), vec![input.clone()])
                    .await
                    .unwrap();
                commit(&root, task, data, vec![input]).await.unwrap();
            }
        }
        let records = records(root.clone(), task.into()).await.unwrap();
        assert_eq!(records.len(), 6);
        assert_eq!(
            records
                .iter()
                .filter(|(i, _, _)| i["stage"] == "handoff")
                .count(),
            3
        );
        let conn = agent_service::open_db(&root).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM agent_task_events WHERE kind='execution/commit'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            6
        );
        for (input, _, _) in records {
            let data: String = conn
                .query_row(
                    "SELECT data FROM agent_task_events WHERE task_id=?1 AND seq=?2",
                    params![task, input["source_event_seq"].as_i64()],
                    |row| row.get(0),
                )
                .unwrap();
            let data: Value = serde_json::from_str(&data).unwrap();
            assert_eq!(
                data["scheduler"]["request_started_turn"],
                input["identity"]["request_id"]
            );
        }
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
}
