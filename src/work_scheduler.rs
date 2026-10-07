//! The host owns execution facts and handoff. Three states; verification is a condition, not a second workflow.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all="snake_case")]
pub enum WorkStatus { #[default] Ready, Running, Done }

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all="snake_case")]
pub enum Completion { #[default] Output, Write, Check, WriteCheck }

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkOrder {
    pub id: String,
    pub node_id: String,
    pub revision: usize,
    pub plan_revision: usize,
    pub goal: String,
    pub done_when: String,
    pub constraints: Vec<String>,
    pub upstream_ids: Vec<String>,
    pub dependency_inputs: Vec<Value>,
    pub finding_ids: Vec<String>,
    pub material_ids: Vec<i64>,
    pub material_ranges: Vec<Value>,
    pub edit_targets: Vec<String>,
    pub completion: Completion,
    pub checks: Vec<String>,
    pub final_answer: bool,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub browser_document_path: Option<String>,
    pub visual_goal: Option<String>,
    /// Declared scope for an Organizer-planned process observation, when known.
    #[serde(default)]
    pub project_observation: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkFrame {
    pub order: WorkOrder,
    pub status: WorkStatus,
    pub invalidated_by_plan_revision: Option<usize>,
    pub writes: BTreeMap<String, Value>,
    pub versions: BTreeMap<String, String>,
    pub epoch: usize,
    pub checked: BTreeMap<String, usize>,
    /// Bind unfinished HTTP checks to their exact sample; completed receipts are historical.
    #[serde(default)]
    pub http_check_samples: BTreeMap<String, String>,
    pub check_errors: BTreeMap<String, Value>,
    pub operations: Vec<Value>,
    pub sequence: usize,
    pub rounds: usize,
    pub rounds_without_change: usize,
    pub reviewed_without_change: usize,
    pub repeated_reads: usize,
    pub idle_rounds: usize,
    pub continues_without_progress: usize,
    pub output: Option<Value>,
    /// Legacy host-derived value. Kept for deserialization; new snapshots and
    /// Organizer inputs never serialize it as if it were a Worker conclusion.
    #[serde(default, skip_serializing)]
    pub expectation_met: Option<bool>,
    pub source_selection: Value,
    pub organizer_guidance: Value,
    pub visual_artifact_ids:Vec<String>,
    /// Latest screenshot IDs explicitly accepted for the current host page state.
    pub current_visual_artifact_ids:Vec<String>,
    pub visual_original_artifact_ids:Vec<String>,
    pub seen_visual_artifact_ids:Vec<String>,
    pub visual_requests:Vec<Value>,
    pub visual_check_result:Value,
    pub browser_page:Value,
    /// Host-observed upload evidence survives the bounded operation log and can be explicitly exported.
    #[serde(default)]
    pub browser_upload_receipt: Option<BrowserUploadEvidence>,
    /// Latest current-page read is stored separately so later operations cannot evict verification evidence.
    #[serde(default)]
    pub browser_current_read: Option<Value>,
    #[serde(default)]
    pub project_observation: Value,
    /// Stable references to HTTP samples produced or explicitly consumed by this node.
    #[serde(default)]
    pub http_observation_ids: Vec<String>,
    /// Latest Worker reports for this invocation: intended next steps, never results.
    #[serde(default)]
    pub progress: Vec<Value>,
    /// Why an unreturned task was explicitly deprecated, with its stage facts.
    #[serde(default)]
    pub superseded: Option<Value>,
    /// Host time when this invocation started and when it returned.
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub returned_at: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct BrowserUploadEvidence {
    pub upload_attempt_id: String,
    pub path: String,
    pub change_event_received: bool,
    pub page: Value,
    pub file: Value,
    pub work_id: String,
    pub node_id: String,
    pub revision: usize,
    /// New browser hosts require a matching application load attempt on the later read.
    pub load_attempt_required: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RewindRecord {
    pub id: String,
    pub source_node: String,
    #[serde(default)]
    pub source_work_id: String,
    pub source_revision: usize,
    pub target_node: String,
    #[serde(default)]
    pub target_work_id: String,
    pub target_revision: usize,
    pub reason: String,
    pub timestamp: u64,
    pub plan_revision: usize,
    pub invalidated_tasks: Vec<String>,
    #[serde(default)]
    pub invalidated_node_ids: Vec<String>,
}

/// Read-only history of a cancelled user goal, outside the active dependency graph.
#[derive(Clone, Deserialize, Serialize)]
pub struct ArchivedRequest {
    pub id: String,
    pub started_turn: usize,
    pub plan_revision: usize,
    pub reason: String,
    pub scheduler: Value,
    pub task_tree: Value,
    pub flow_plan: Value,
    #[serde(default)]
    pub worker_state: Option<Value>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkScheduler {
    /// Missing from legacy snapshots (version 0); current snapshots use version 2.
    #[serde(default)]
    pub schema_version: u32,
    pub request_started_turn: usize,
    pub archived_requests: Vec<ArchivedRequest>,
    pub frames: BTreeMap<String, WorkFrame>,
    pub queue: VecDeque<String>,
    pub current: String,
    pub handoff: Option<Value>,
    pub goal_boundary: Value,
    pub finished: bool,
    /// Explicit successful request completion; finishing a blocked run is resumable.
    #[serde(default)]
    pub request_completed: Option<bool>,
    pub final_result: String,
    pub plan_revision: usize,
    pub node_revisions: BTreeMap<String, usize>,
    pub rewind_records: Vec<RewindRecord>,
    pub activated_history: Vec<String>,
    pub revisit_counts: BTreeMap<String, usize>,
    /// Active upload evidence keyed by controlled browser session and page identity.
    #[serde(default)]
    pub active_browser_uploads: BTreeMap<String, String>,
    /// Last host validation for each browser page/upload identity. This is
    /// runtime availability, separate from the immutable completed work record.
    #[serde(default)]
    pub browser_upload_availability: BTreeMap<String, Value>,
    #[serde(default)]
    pub next_browser_upload_id: u64,
    /// Recent same-URL HTTP samples are shared across work packets in this active request.
    #[serde(default)]
    pub http_probe_results: BTreeMap<String, Value>,
    #[serde(default)]
    pub http_probe_generation: u64,
    /// Immutable HTTP samples. `http_probe_results` is only the current-URL index.
    #[serde(default)]
    pub http_observation_samples: BTreeMap<String, Value>,
    #[serde(default)]
    pub next_http_observation_id: u64,
    /// Host idempotency receipts for dispatch-changing Organizer decisions.
    #[serde(default)]
    pub applied_decisions: BTreeMap<String, String>,
    /// Stall handoffs per normalized goal; bounds re-dispatch of the same task.
    #[serde(default)]
    pub stalled_goals: BTreeMap<String, usize>,
}

impl Default for WorkScheduler {
    fn default() -> Self {
        Self {
            schema_version: WORK_SCHEDULER_SCHEMA_VERSION,
            request_started_turn: 0,
            archived_requests: Vec::new(),
            frames: BTreeMap::new(),
            queue: VecDeque::new(),
            current: String::new(),
            handoff: None,
            goal_boundary: Value::Null,
            finished: false,
            request_completed: Some(false),
            final_result: String::new(),
            plan_revision: 1,
            node_revisions: BTreeMap::new(),
            rewind_records: Vec::new(),
            activated_history: Vec::new(),
            revisit_counts: BTreeMap::new(),
            active_browser_uploads: BTreeMap::new(),
            browser_upload_availability: BTreeMap::new(),
            next_browser_upload_id: 0,
            http_probe_results: BTreeMap::new(),
            http_probe_generation: 0,
            http_observation_samples: BTreeMap::new(),
            next_http_observation_id: 0,
            applied_decisions: BTreeMap::new(),
            stalled_goals: BTreeMap::new(),
        }
    }
}

/// Consecutive Worker rounds with only reports/replies before a stall handoff.
pub const REPORT_ONLY_ROUND_LIMIT: usize = 3;
/// Stall handoffs one goal may cause before re-dispatching it is rejected.
const STALLED_GOAL_LIMIT: usize = 2;
const WORK_SCHEDULER_SCHEMA_VERSION: u32 = 2;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

const EXECUTION_PATH_STEPS: usize = 12;

fn rfc3339_ms(value: &Value) -> Option<u64> {
    value.as_str().and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok()).map(|time| time.timestamp_millis().max(0) as u64)
}
fn ms_label(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64).map(|time| time.format("%H:%M:%SZ").to_string()).unwrap_or_else(|| ms.to_string())
}
fn time_label(value: &Value) -> String {
    rfc3339_ms(value).map(ms_label).unwrap_or_else(|| "unknown time".to_owned())
}
fn http_outcome(sample: &Value) -> String {
    match sample["http_status"].as_u64() {
        Some(status) => format!("HTTP {status}"),
        None => format!("no HTTP response ({})", sample["error_kind"].as_str().unwrap_or("unreachable")),
    }
}

fn stall_key(goal: &str) -> String {
    goal.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn limited(s: &str, count: usize) -> String { s.chars().take(count).collect() }
fn push_organizer_omission(item:Value,items:&mut Vec<Value>,total:&mut usize) {
    *total=total.saturating_add(1);
    if items.len()<24 {items.push(item);}
}
fn organizer_compact(value:&Value,depth:usize)->Value {
    if depth>8 {return Value::Null;}
    match value {
        Value::Object(object)=>{
            let mut compact=serde_json::Map::new();
            for (key,value) in object {
                if matches!(key.as_str(),"operations"|"logs"|"stdout"|"stderr"|"stderr_recent"|"log_file"|"raw_output"|"source_text") {continue;}
                compact.insert(key.clone(),organizer_compact(value,depth+1));
            }
            Value::Object(compact)
        },
        Value::Array(items)=>Value::Array(items.iter().take(16).map(|item|organizer_compact(item,depth+1)).collect()),
        Value::String(text)=>Value::String(limited(text,1200)),
        _=>value.clone(),
    }
}
fn neutralize_legacy_worker_outcome(output:&mut Value) {
    if output["done"]==true {
        if output["outcome"]=="completed" {
            output["outcome"]=Value::Null;
            output["outcome_unverified_legacy"]=json!(true);
        }
        for field in ["blocked","need_split"] {
            if output[field]==false {output[field]=Value::Null;}
        }
    }
    if let Some(previous)=output.get_mut("previous_handoff") {neutralize_legacy_worker_outcome(previous);}
}
fn organizer_omissions(value:&Value,path:&str,depth:usize,items:&mut Vec<Value>,total:&mut usize) {
    if depth>8 {
        push_organizer_omission(json!({"path":path,"reason":"nesting limit","reference":"read_task_result with this work_id and field path"}),items,total);
        return;
    }
    match value {
        Value::Object(object)=>for (key,child) in object {
            let child_path=if path.is_empty(){key.clone()}else{format!("{path}.{key}")};
            if matches!(key.as_str(),"operations"|"logs"|"stdout"|"stderr"|"stderr_recent"|"log_file"|"raw_output"|"source_text") {
                push_organizer_omission(json!({"path":child_path,"reason":"raw execution/source material is kept in the event log or notebook","reference":"material_ids or the task notebook"}),items,total);
            } else {organizer_omissions(child,&child_path,depth+1,items,total);}
        },
        Value::Array(array)=>{
            if array.len()>16 {push_organizer_omission(json!({"path":path,"reason":"array compacted","omitted_items":array.len()-16,
                "reference":"read_task_result with this work_id and field path"}),items,total);}
            for (index,child) in array.iter().take(16).enumerate() {organizer_omissions(child,&format!("{path}[{index}]"),depth+1,items,total);}
        },
        Value::String(text)=>{
            let count=text.chars().count();
            if count>1200 {push_organizer_omission(json!({"path":path,"reason":"string compacted","omitted_chars":count-1200,
                "reference":"read_task_result with this work_id and field path"}),items,total);}
        },
        _=>{}
    }
}
fn compact_suggested_children(value:&Value)->Value {
    let Some(children)=value.as_array() else {return Value::Null;};
    Value::Array(children.iter().take(16).map(|child| {
        let mut compact=serde_json::Map::new();
        for field in ["id","node_id","title","goal","objective","done_when","upstream_ids","dependency_inputs","depends_on","dependencies"] {
            if let Some(value)=child.get(field).filter(|value|!value.is_null()) {
                compact.insert(field.to_owned(),organizer_compact(value,0));
            }
        }
        Value::Object(compact)
    }).collect())
}
fn handoff_matches_current_output(handoff:&Value,output:&Value)->bool {
    let mut semantic=handoff.clone();
    if let Some(object)=semantic.as_object_mut() {object.remove("organizer_failure");}
    semantic==*output
}
fn organizer_handoff(handoff:&Value)->Value {
    let mut compact=json!({});
    for field in ["done","blocked","need_split","outcome","failure_stage","resumable","intent",
        "current_work","current_node","resumable_work","resumable_node","revision","upstream_problem"] {
        if let Some(value)=handoff.get(field) {compact[field]=value.clone();}
    }
    if let Some(children)=handoff.get("suggested_children") {compact["suggested_children"]=compact_suggested_children(children);}
    if let Some(failure)=handoff.get("organizer_failure") {compact["organizer_failure"]=organizer_compact(failure,0);}
    fn execution_error(handoff:&Value)->Option<&Value> {
        handoff.get("execution_error").or_else(||handoff.get("previous_handoff").and_then(execution_error))
    }
    if let Some(error)=execution_error(handoff) {compact["execution_error"]=error.clone();}
    if let Some(reason)=handoff["reason"].as_str() {compact["reason"]=json!(limited(reason,500));}
    if let Some(failure)=handoff.get("failure") {
        compact["failure"]=json!({"error_code":failure["error_code"],"stage":failure["stage"],
            "attempt":failure["attempt"],"retry_limit":failure["retry_limit"],
            "error":limited(failure["error"].as_str().unwrap_or(""),500)});
    }
    compact
}
fn path(s: &str) -> String { s.replace('\\',"/") }
fn command(s: &str) -> String { s.trim().replace("\r\n","\n") }
fn supported_check_key(check:&str)->bool {
    ["npm:","npm-start:","npm-install:","program:","http-probe:"]
        .iter().any(|prefix|check.starts_with(prefix))
}
fn browser_paths_equal(left:&str,right:&str)->bool {
    let left=path(left);let right=path(right);
    if cfg!(windows) {left.eq_ignore_ascii_case(&right)} else {left==right}
}
fn browser_page_key(page:&Value)->Option<String> {
    Some(format!("{}\u{1f}{}",page["browser_session_id"].as_str()?,page["page_id"].as_str()?))
}
fn browser_upload_status_key(receipt:&Value)->Option<String> {
    Some(format!("{}\u{1e}{}",browser_page_key(&receipt["page"])?,receipt["upload_attempt_id"].as_str()?))
}
fn parse_page_indicator(value:&str)->Option<(u64,u64)> {
    let value=value.trim();
    let value=if value.get(..6).is_some_and(|prefix|prefix.eq_ignore_ascii_case("slide ")) {&value[6..]} else {value};
    let (current,total)=if let Some(parts)=value.split_once('/') {parts} else {
        let lower=value.to_ascii_lowercase();
        let index=lower.find(" of ")?;
        (&value[..index],&value[index+4..])
    };
    let (current,total)=(current.trim().parse::<u64>().ok()?,total.trim().parse::<u64>().ok()?);
    (current>0&&total>=current).then_some((current,total))
}
fn valid_page_indicator(items:&Value)->bool {
    items.as_array().is_some_and(|items|items.iter().any(|item|item.as_str().is_some_and(|text|parse_page_indicator(text).is_some())))
}

#[derive(Clone, Debug, Default)]
pub struct DecisionChanges {
    pub invalidated_work_ids: Vec<String>,
    pub invalidated_node_ids: Vec<String>,
}

#[derive(Clone, Copy)]
enum DependencyTarget<'a> { Work(&'a str), Node(&'a str), Alias(&'a str) }

/// Exact work IDs never fall back to another instance of the same node.
fn dependency_frame<'a>(frames: &'a BTreeMap<String, WorkFrame>, target: DependencyTarget<'_>, revision: Option<usize>) -> Option<&'a WorkFrame> {
    let valid = |f: &&WorkFrame| f.invalidated_by_plan_revision.is_none() && revision.is_none_or(|r| f.order.revision == r);
    match target {
        DependencyTarget::Work(id) => frames.get(id).filter(valid),
        DependencyTarget::Alias(id) if frames.contains_key(id) => frames.get(id).filter(valid),
        DependencyTarget::Node(node) | DependencyTarget::Alias(node) => frames.values()
            .filter(valid).filter(|f| f.order.node_id == node)
            .max_by_key(|f| (f.order.revision, f.sequence)),
    }
}

/// Stable prefixes of dependency validation errors, surfaced to the Organizer.
pub const DEPENDENCY_ERROR_KINDS: &[&str] = &["TASK_NOT_RETURNED","TASK_DEPRECATED","REVISION_MISMATCH","FIELD_NOT_EXPORTED",
    "INVALID_REFERENCE_TYPE","UNKNOWN_TASK","NODE_MISMATCH"];

/// Explain why a reference has no active frame instead of one generic message.
fn missing_dependency_reason(frames: &BTreeMap<String, WorkFrame>, target: DependencyTarget<'_>, label: &str, revision: Option<usize>) -> String {
    let candidates = frames.values().filter(|f| match target {
        DependencyTarget::Work(id) => f.order.id == id,
        DependencyTarget::Node(node) => f.order.node_id == node,
        DependencyTarget::Alias(id) => f.order.id == id || f.order.node_id == id,
    }).collect::<Vec<_>>();
    if candidates.is_empty() {
        return format!("UNKNOWN_TASK: no task with work_id or node_id '{label}' exists in this request");
    }
    let active = candidates.iter().filter(|f| f.invalidated_by_plan_revision.is_none()).max_by_key(|f| (f.order.revision, f.sequence));
    if let (Some(requested), Some(active)) = (revision, active) {
        return format!("REVISION_MISMATCH: '{label}' requested revision {requested}, but the active task is work_id='{}' revision {}",
            active.order.id, active.order.revision);
    }
    let deprecated = candidates.iter().max_by_key(|f| (f.order.revision, f.sequence)).unwrap();
    let reason = deprecated.superseded.as_ref().and_then(|record| record["reason"].as_str()).unwrap_or("superseded by a later plan revision");
    format!("TASK_DEPRECATED: work_id='{}' node_id='{}' revision {} was deprecated at plan revision {} ({}); it is history, not a valid input",
        deprecated.order.id, deprecated.order.node_id, deprecated.order.revision,
        deprecated.invalidated_by_plan_revision.unwrap_or_default(), limited(reason, 200))
}

fn declared_dependency_frame<'a>(frames: &'a BTreeMap<String, WorkFrame>, dep: &Value, validate_revision: bool) -> Result<&'a WorkFrame, String> {
    let revision = dep.get("revision").and_then(Value::as_u64).map(|r| r as usize);
    let (target, label) = if let Some(id) = dep.as_str() {
        (DependencyTarget::Alias(id), id)
    } else if let Some(id) = dep["work_id"].as_str().filter(|id| !id.is_empty()) {
        (DependencyTarget::Work(id), id)
    } else if let Some(node) = dep["node_id"].as_str().filter(|id| !id.is_empty()) {
        (DependencyTarget::Node(node), node)
    } else {
        return Err("INVALID_REFERENCE_TYPE: a dependency input needs a work_id or node_id string".into());
    };
    let frame = dependency_frame(frames, target, revision)
        .or_else(|| if validate_revision { None } else { dependency_frame(frames, target, None) })
        .ok_or_else(|| missing_dependency_reason(frames, target, label, revision))?;
    if let Some(node) = dep["node_id"].as_str().filter(|id| !id.is_empty()) {
        if node != frame.order.node_id { return Err(format!("NODE_MISMATCH: upstream work '{label}' does not belong to node '{node}'")); }
    }
    Ok(frame)
}

fn references_work(order: &WorkOrder, work_id: &str, node_id: &str) -> bool {
    order.upstream_ids.iter().any(|id| id == work_id || id == node_id)
        || order.dependency_inputs.iter().any(|dep| dep.as_str().is_some_and(|id| id == work_id || id == node_id)
            || dep["work_id"].as_str() == Some(work_id) || dep["node_id"].as_str() == Some(node_id))
}

fn normalize_dependencies(frames: &BTreeMap<String, WorkFrame>, order: &mut WorkOrder) -> Result<()> {
    normalize_dependencies_at(frames,order,"orders[0]")
}

fn normalize_dependencies_at(frames: &BTreeMap<String, WorkFrame>, order: &mut WorkOrder, order_path:&str) -> Result<()> {
    for up in &mut order.upstream_ids {
        let frame = dependency_frame(frames, DependencyTarget::Alias(up), None)
            .ok_or_else(|| anyhow::anyhow!("field_path={order_path}.upstream_ids: {}", missing_dependency_reason(frames, DependencyTarget::Alias(up), up, None)))?;
        *up = frame.order.id.clone();
    }
    for (index,dep) in order.dependency_inputs.iter_mut().enumerate() {
        let frame = declared_dependency_frame(frames, dep, false).map_err(|error|{
            let field=if error.starts_with("REVISION_MISMATCH")&&dep["revision"].is_number(){"revision"}else if error.starts_with("NODE_MISMATCH")&&dep["node_id"].is_string(){"node_id"}else if dep["work_id"].is_string(){"work_id"}else if dep["node_id"].is_string(){"node_id"}else{"work_id"};
            anyhow::anyhow!("field_path={order_path}.dependency_inputs[{index}].{field}: {error}")
        })?;
        let id = frame.order.id.clone();
        if dep.is_string() { *dep = json!({"work_id": id}); }
        else { dep["work_id"] = json!(id); }
        if dep.get("http_urls").is_some() {
            let urls=dep["http_urls"].as_array().ok_or_else(||anyhow::anyhow!("field_path={order_path}.dependency_inputs[{index}].http_urls: expected an array of exact HTTP URLs"))?;
            ensure!(!urls.is_empty() && urls.len()<=16, "field_path={order_path}.dependency_inputs[{index}].http_urls: provide 1 to 16 URLs");
            ensure!(urls.iter().all(|url|url.as_str().is_some_and(|url|!url.trim().is_empty()&&url.len()<=4096)),
                "field_path={order_path}.dependency_inputs[{index}].http_urls: each URL must be a nonempty string of at most 4096 bytes");
            if !dep["fields"].is_array() { dep["fields"]=json!([]); }
            let fields=dep["fields"].as_array_mut().unwrap();
            if !fields.iter().any(|field|field.as_str()==Some("http_observations")) {fields.push(json!("http_observations"));}
        }
        if !order.upstream_ids.contains(&id) { order.upstream_ids.push(id); }
    }
    order.upstream_ids.sort();
    order.upstream_ids.dedup();
    Ok(())
}

impl WorkScheduler {
    pub fn frame(&self) -> Option<&WorkFrame> { self.frames.get(&self.current) }
    pub fn frame_mut(&mut self) -> Option<&mut WorkFrame> { self.frames.get_mut(&self.current) }
    pub fn order(&self) -> Option<&WorkOrder> { self.frame().map(|f| &f.order) }
    pub fn id(&self) -> &str { &self.current }
    pub fn node(&self) -> &str { self.order().map_or("direct", |o| o.node_id.as_str()) }
    pub fn revision(&self) -> usize { self.order().map_or(1, |o| o.revision) }
    pub fn scope(&self) -> String { format!("{}@{}::{}", self.node(), self.revision(), self.current) }
    fn effectively_done(&self, frame: &WorkFrame) -> bool {
        frame.status == WorkStatus::Done
    }
    pub fn done(&self) -> bool { self.frame().is_some_and(|frame| self.effectively_done(frame)) }
    pub fn current_process(&self) -> Option<&WorkFrame> { self.frame() }
    pub fn current_process_mut(&mut self) -> Option<&mut WorkFrame> { self.frame_mut() }
    pub fn needs_organizer(&self) -> bool { !self.finished && (self.handoff.is_some() || self.current.is_empty() || self.done()) }
    pub fn selection(&self) -> Value { self.frame().map(|f| f.source_selection.clone()).unwrap_or(json!({})) }
    pub fn save_selection(&mut self, selection: Value) { if let Some(f) = self.frames.get_mut(&self.current) { f.source_selection = selection; } }
    pub fn set_goal_boundary(&mut self, boundary: Value) { self.goal_boundary = boundary; }
    pub fn snapshot(&self) -> Value { serde_json::to_value(self).unwrap_or(json!({})) }

    pub fn normalize_snapshot_version(&mut self) {
        if self.schema_version < WORK_SCHEDULER_SCHEMA_VERSION {
            for frame in self.frames.values_mut() {
                if let Some(output)=frame.output.as_mut() {neutralize_legacy_worker_outcome(output);}
            }
            if let Some(handoff)=self.handoff.as_mut() {neutralize_legacy_worker_outcome(handoff);}
        }
        self.schema_version = WORK_SCHEDULER_SCHEMA_VERSION;
    }

    /// A replacement creates a fresh request; cancelled frames cannot be selected,
    /// referenced as upstreams, or interfere when the new request reuses an ID.
    pub fn replace_request(&self, tree: &crate::flow_tree::TaskTree, reason: &str) -> Self {
        let id = format!("request_{}", self.archived_requests.len() + 1);
        let revision = self.plan_revision + 1;
        let mut old = self.clone();
        old.archived_requests.clear(); // Archives are flat, never nested snapshots.
        for frame in old.frames.values_mut() { frame.invalidated_by_plan_revision.get_or_insert(revision); }
        let mut tree_snapshot = tree.snapshot();
        if let Some(nodes) = tree_snapshot["nodes"].as_object_mut() {
            for node in nodes.values_mut() {
                node["previous_status"] = node["status"].clone();
                node["status"] = json!("deprecated");
            }
        }
        tree_snapshot["previous_active"] = tree_snapshot["active"].clone();
        tree_snapshot["active"] = json!("");
        let mut plan = self.active_flow_plan(if tree.enabled() { Some(tree) } else { None });
        let scoped = |value: &Value| json!(format!("{id}:{}", value.as_str().unwrap_or("")));
        if let Some(nodes) = plan["nodes"].as_array_mut() {
            for node in nodes {
                node["request_id"] = json!(self.request_started_turn);
                node["original_id"] = node["id"].clone();
                node["previous_status"] = node["status"].clone();
                for key in ["id", "node_id", "work_id", "parent_id"] {
                    if node[key].as_str().is_some_and(|s| !s.is_empty()) { node[key] = scoped(&node[key]); }
                }
                node["status"] = json!("deprecated");
                node["invalidated_by_plan_revision"] = json!(revision);
            }
        }
        if let Some(edges) = plan["edges"].as_array_mut() {
            for edge in edges {
                for key in ["id", "source", "target", "source_node", "target_node"] {
                    if edge[key].as_str().is_some_and(|s| !s.is_empty()) { edge[key] = scoped(&edge[key]); }
                }
                edge["deprecated"] = json!(true);
                edge["rewind"] = json!(false);
            }
        }
        plan["active_node_id"] = json!(""); plan["active_path"] = json!([]);
        let mut archives = self.archived_requests.clone();
        archives.push(ArchivedRequest { id, started_turn: self.request_started_turn, plan_revision: revision,
            reason: reason.to_owned(), scheduler: old.snapshot(), task_tree: tree_snapshot, flow_plan: plan, worker_state: None });
        Self { plan_revision: revision, archived_requests: archives, ..Default::default() }
    }

    /// A bounded work packet is already a Flow node; drawing it needs no extra planning call.
    pub fn flow_plan(&self) -> Value {
        let mut frames = self.frames.values().collect::<Vec<_>>();
        frames.sort_by_key(|f| f.sequence);
        let mut edges = Vec::new();
        for f in &frames {
            for id in &f.order.upstream_ids {
                if let Some(up) = self.frames.get(id) {
                    edges.push(json!({
                        "id": format!("depends_{}_{}", up.order.id, f.order.id),
                        "source": up.order.id,
                        "target": f.order.id,
                        "label": "前序结果",
                        "deprecated": f.invalidated_by_plan_revision.is_some() || up.invalidated_by_plan_revision.is_some(),
                    }));
                }
            }
        }
        for rewind in &self.rewind_records {
            let src = if !rewind.source_work_id.is_empty() { &rewind.source_work_id } else { &rewind.source_node };
            let tgt = if !rewind.target_work_id.is_empty() { &rewind.target_work_id } else { &rewind.target_node };
            edges.push(json!({
                "id": rewind.id.clone(),
                "source": src,
                "target": tgt,
                "source_node": rewind.source_node.as_str(),
                "target_node": rewind.target_node.as_str(),
                "label": format!("回溯: {}", rewind.reason),
                "deprecated": false,
                "rewind": true,
            }));
        }

        json!({
            "mode": if frames.len() > 1 { "dag" } else { "direct" },
            "plan_revision": self.plan_revision,
            "nodes": frames.iter().map(|f| json!({
                "id": f.order.id,
                "node_id": f.order.node_id,
                "work_id": f.order.id,
                "revision": f.order.revision,
                "plan_revision": f.order.plan_revision,
                "invalidated_by_plan_revision": f.invalidated_by_plan_revision,
                "title": if f.order.revision > 1 {
                    format!("{} (r{})", f.order.goal, f.order.revision)
                } else {
                    f.order.goal.clone()
                },
                "kind": "worker",
                "objective": f.order.goal,
                "description": f.order.goal,
                "done_when": f.order.done_when,
                "constraints": f.order.constraints,
                "status": if f.invalidated_by_plan_revision.is_some() {
                    "deprecated"
                } else if f.status == WorkStatus::Done {
                    "done"
                } else if f.status == WorkStatus::Running {
                    "running"
                } else {
                    "ready"
                },
                "result": f.output
            })).collect::<Vec<_>>(),
            "edges": edges,
            "rewind_records": self.rewind_records,
            "active_node_id": if self.done() || self.finished { "" } else { self.id() },
            "active_path": []
        })
    }

    pub fn unified_flow_plan(&self, tree: Option<&crate::flow_tree::TaskTree>) -> Value {
        let mut base = self.active_flow_plan(tree);
        for node in base["nodes"].as_array_mut().into_iter().flatten() {node["request_id"]=json!(self.request_started_turn);}
        for archive in &self.archived_requests {
            if let Some(nodes) = archive.flow_plan["nodes"].as_array() { base["nodes"].as_array_mut().unwrap().extend(nodes.clone()); }
            if let Some(edges) = archive.flow_plan["edges"].as_array() { base["edges"].as_array_mut().unwrap().extend(edges.clone()); }
        }
        base
    }

    fn active_flow_plan(&self, tree: Option<&crate::flow_tree::TaskTree>) -> Value {
        let mut base = self.flow_plan();
        if let Some(t) = tree.filter(|t| t.enabled()) {
            let tree_plan = t.plan();
            let mut nodes = base["nodes"].as_array().cloned().unwrap_or_default();
            let mut edges = base["edges"].as_array().cloned().unwrap_or_default();

            // Map tree node_id to the canvas node ID (active or latest work_id)
            let mut tree_to_work = BTreeMap::new();
            for n in &nodes {
                let nid = n["node_id"].as_str().unwrap_or("");
                let wid = n["id"].as_str().unwrap_or("");
                if !nid.is_empty() && !wid.is_empty() {
                    if !tree_to_work.contains_key(nid) || n["status"] != "deprecated" {
                        tree_to_work.insert(nid.to_string(), wid.to_string());
                    }
                }
            }

            let work_node_ids = nodes.iter().filter_map(|n| n["node_id"].as_str().map(str::to_owned)).collect::<std::collections::BTreeSet<_>>();
            if let Some(tree_nodes) = tree_plan["nodes"].as_array() {
                for tn in tree_nodes {
                    let tid = tn["id"].as_str().unwrap_or("");
                    if !work_node_ids.contains(tid) {
                        nodes.push(tn.clone());
                    }
                }
            }
            if let Some(tree_edges) = tree_plan["edges"].as_array() {
                for te in tree_edges {
                    let raw_src = te["source"].as_str().unwrap_or("");
                    let raw_tgt = te["target"].as_str().unwrap_or("");
                    let src = tree_to_work.get(raw_src).map(|s| s.as_str()).unwrap_or(raw_src);
                    let tgt = tree_to_work.get(raw_tgt).map(|s| s.as_str()).unwrap_or(raw_tgt);
                    let mut mapped = te.clone();
                    mapped["source"] = json!(src);
                    mapped["target"] = json!(tgt);
                    mapped["id"] = json!(format!("contains_{src}_{tgt}"));
                    edges.push(mapped);
                }
            }
            for n in &mut nodes {
                let node_id = n["node_id"].as_str().unwrap_or("").to_string();
                let work_id = n["id"].as_str().unwrap_or("").to_string();
                let is_dep = n["status"] == "deprecated";
                if let Some(parent) = t.parent_of(&node_id) {
                    let mapped_parent = tree_to_work.get(parent).map(|s| s.as_str()).unwrap_or(parent);
                    if n.get("parent_id").is_none() || n["parent_id"].is_null() {
                        n["parent_id"] = json!(mapped_parent);
                    }
                    if work_id != node_id {
                        let edge_id = format!("contains_{mapped_parent}_{work_id}");
                        if !edges.iter().any(|e| e["id"] == edge_id) {
                            edges.push(json!({
                                "id": edge_id,
                                "source": mapped_parent,
                                "target": work_id,
                                "label": "子问题",
                                "deprecated": is_dep
                            }));
                        }
                    }
                }
            }
            // Deduplicate edges by (source, target)
            let mut unique_edges = Vec::new();
            let mut seen_edges = BTreeSet::new();
            for e in edges {
                let s = e["source"].as_str().unwrap_or("").to_string();
                let tg = e["target"].as_str().unwrap_or("").to_string();
                if seen_edges.insert((s, tg)) {
                    unique_edges.push(e);
                }
            }
            base["mode"] = json!("tree");
            base["nodes"] = json!(nodes);
            base["edges"] = json!(unique_edges);
            base["active_node_id"] = if self.done() || self.finished { json!("") } else { json!(self.id()) };
            let mapped_active_path: Vec<Value> = tree_plan["active_path"].as_array().into_iter().flatten().map(|tid_val| {
                let tid = tid_val.as_str().unwrap_or("");
                if let Some(wid) = tree_to_work.get(tid) {
                    json!(wid)
                } else {
                    tid_val.clone()
                }
            }).collect();
            base["active_path"] = json!(mapped_active_path);
        }
        base
    }

    pub fn pending_handoff(&self) -> Option<&Value> { self.handoff.as_ref() }
    pub fn output(&self) -> Option<&Value> { self.frame().and_then(|f| f.output.as_ref()) }
    pub fn versions(&self) -> BTreeMap<String, String> { self.frame().map(|f| f.versions.clone()).unwrap_or_default() }

    pub fn enqueue(&mut self, mut orders: Vec<WorkOrder>, can_write: bool, can_check: bool) -> Result<()> {
        ensure!(!orders.is_empty() && orders.len() <= 8, "assign 1 to 8 concrete work orders");
        let mut staged = self.frames.clone();
        let mut queue = self.queue.clone();

        // Include this batch in the same resolver used for delivery validation.
        let mut candidates = staged.clone();
        let mut incoming_ids = BTreeSet::new();
        for (index, order) in orders.iter_mut().enumerate() {
            ensure!(incoming_ids.insert(order.id.clone()), "duplicate work id in assignment");
            if order.revision == 0 { order.revision = self.node_revisions.get(&order.node_id).copied().unwrap_or(1); }
            candidates.insert(order.id.clone(), WorkFrame { order: order.clone(), sequence: staged.len() + index + 1, ..Default::default() });
        }

        for (index,o) in orders.iter_mut().enumerate() {
            normalize_dependencies_at(&candidates,o,&format!("orders[{index}]"))?;

            ensure!(!o.id.is_empty() && o.id.len() <= 80 && o.id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')), "work needs a stable short id");
            ensure!(!o.node_id.is_empty() && !o.goal.trim().is_empty() && !o.done_when.trim().is_empty(), "work needs node_id, goal and done_when");
            ensure!(o.goal.chars().count() <= 2400 && o.done_when.chars().count() <= 1200 && o.constraints.len() <= 12 && o.constraints.iter().all(|s| s.chars().count() <= 700), "work contract is too large");
            if let Some(value)=o.browser_document_path.as_deref().filter(|value|value.contains("://")) {
                anyhow::bail!("field_path=orders[{index}].browser_document_path: this field is the workspace-relative .pptx file path, but '{}' is a page URL; put the editor page URL in goal or constraints and the PPTX file path here",limited(value,300));
            }
            let requires_pptx=o.constraints.iter().any(|constraint|constraint=="requires_pptx");
            if requires_pptx {
                let expected=o.browser_document_path.as_deref().filter(|path|!path.trim().is_empty()).ok_or_else(||anyhow::anyhow!("field_path=orders[{index}].browser_document_path: requires_pptx needs the exact workspace-relative target file path"))?;
                let normalized=path(expected.trim());
                ensure!(normalized.chars().count()<=2000&&!normalized.starts_with('/')&&!normalized.contains(':')&&!normalized.split('/').any(|part|part==".."||part==".")&&normalized.to_ascii_lowercase().ends_with(".pptx"),"field_path=orders[{index}].browser_document_path: target must be a workspace-relative .pptx file path without traversal segments");
                o.browser_document_path=Some(normalized);
            } else if let Some(expected)=o.browser_document_path.as_deref() {
                o.browser_document_path=Some(path(expected.trim()));
            }
            ensure!(o.visual_goal.as_ref().is_none_or(|s| !s.trim().is_empty() && s.chars().count() <= 2400), "visual_goal must be a focused nonempty goal");
            ensure!(o.upstream_ids.len() <= 16 && o.finding_ids.len() <= 16 && o.material_ids.len() <= 16 && o.material_ranges.len() <= 16 && o.edit_targets.len() <= 16 && o.checks.len() <= 4, "keep work inputs and checks focused");
            for range in &mut o.material_ranges {
                let id = range["id"].as_i64().filter(|id| *id > 0).ok_or_else(|| anyhow::anyhow!("material range needs a material id"))?;
                let start = range["start_line"].as_u64().filter(|line| *line > 0).ok_or_else(|| anyhow::anyhow!("material range needs start_line"))?;
                let end = range["end_line"].as_u64().filter(|end| *end >= start).ok_or_else(|| anyhow::anyhow!("material range needs end_line >= start_line"))?;
                *range = json!({"id": id, "start_line": start, "end_line": end});
            }
            ensure!(o.upstream_ids.iter().all(|id| id != &o.id), "work cannot depend on itself");
            o.edit_targets = o.edit_targets.iter().map(|s| path(s)).collect();
            o.checks = o.checks.iter().map(|s| command(s)).collect();
            let checking = !o.checks.is_empty();
            ensure!(o.edit_targets.is_empty() || can_write, "write work is unavailable under current permissions");
            ensure!(!checking || can_check, "checks are unavailable under current tool permissions");
            ensure!(!matches!(o.completion,Completion::Write|Completion::WriteCheck) || !o.edit_targets.is_empty(), "write work needs explicit target files");
            ensure!(o.checks.iter().all(|s| !s.is_empty() && s.chars().count() <= 2000), "field_path=orders[{index}].checks: each declared observation/check must be a specific nonempty identifier");
            ensure!(o.checks.iter().all(|check|supported_check_key(check)),
                "field_path=orders[{index}].checks: use a supported identifier: npm:, npm-start:, npm-install:, program:, or http-probe:; shell command strings are no longer accepted");

            if o.revision == 0 {
                let rev = self.node_revisions.entry(o.node_id.clone()).or_insert(1);
                o.revision = *rev;
            } else {
                let current_rev = self.node_revisions.entry(o.node_id.clone()).or_insert(o.revision);
                *current_rev = (*current_rev).max(o.revision);
            }
            if o.plan_revision == 0 {
                o.plan_revision = self.plan_revision;
            }

            if let Some(old) = staged.get_mut(&o.id) {
                ensure!(old.invalidated_by_plan_revision.is_none(), "invalidated work is sealed; assign a new work id");
                ensure!(old.status != WorkStatus::Done, "completed work is sealed; assign a new problem");
                ensure!(old.order.node_id == o.node_id && old.order.goal == o.goal && old.order.done_when == o.done_when && old.order.completion == o.completion && old.order.checks == o.checks && old.order.edit_targets == o.edit_targets && old.order.visual_goal == o.visual_goal && old.order.browser_document_path == o.browser_document_path && old.order.constraints.contains(&"requires_visual".to_owned()) == o.constraints.contains(&"requires_visual".to_owned()) && old.order.constraints.contains(&"requires_pptx".to_owned()) == o.constraints.contains(&"requires_pptx".to_owned()), "resume preserves the existing work contract");
                old.order = o.clone();
                old.status = WorkStatus::Ready;
                old.output = None;
                old.invalidated_by_plan_revision = None;
                old.reviewed_without_change = old.rounds_without_change;
            } else {
                ensure!(self.stalled_goals.get(&stall_key(&o.goal)).copied().unwrap_or(0) < STALLED_GOAL_LIMIT,
                    "field_path=orders[{index}].goal: STALLED_GOAL_REPEATED: this same task already stalled {STALLED_GOAL_LIMIT} times without returning; change the approach or input, or finish_request with the limitation");
                ensure!(o.node_id == "direct" || !staged.values().any(|f| f.invalidated_by_plan_revision.is_none() && f.status != WorkStatus::Done && f.order.node_id == o.node_id), "one Flow node is one active work unit; assign a new child for another operation");
                let sequence = staged.len() + 1;
                staged.insert(o.id.clone(), WorkFrame { order: o.clone(), sequence, ..Default::default() });
            }
            ensure!(!queue.contains(&o.id), "work is already queued");
            queue.push_back(o.id.clone());
        }
        ensure!(staged.len() <= 128, "a turn may contain at most 128 work units");
        for f in staged.values() {
            ensure!(f.order.upstream_ids.iter().all(|id| staged.contains_key(id)), "unknown upstream work id");
        }
        fn visit(id: &str, frames: &BTreeMap<String, WorkFrame>, visiting: &mut Vec<String>, visited: &mut BTreeSet<String>) -> Result<()> {
            if visited.contains(id) { return Ok(()); }
            ensure!(!visiting.iter().any(|v| v == id), "work dependencies must be acyclic");
            visiting.push(id.to_owned());
            for upstream in &frames[id].order.upstream_ids { visit(upstream, frames, visiting, visited)?; }
            visiting.pop();
            visited.insert(id.to_owned());
            Ok(())
        }
        let mut visited = BTreeSet::new();
        for id in staged.keys() { visit(id, &staged, &mut vec![], &mut visited)?; }
        if let Some(old) = staged.get_mut(&self.current) {
            if old.status == WorkStatus::Running && old.invalidated_by_plan_revision.is_none() {
                old.status = WorkStatus::Ready;
            }
        }
        self.frames = staged;
        self.queue = queue;
        self.handoff = None;
        self.current.clear();
        Ok(())
    }

    fn browser_receipt_is_active(&self,receipt:&Value)->bool {
        let Some(key)=browser_page_key(&receipt["page"]) else {return false;};
        let Some(attempt_id)=receipt["upload_attempt_id"].as_str() else {return false;};
        self.active_browser_uploads.get(&key).map(String::as_str)==Some(attempt_id)
            &&browser_upload_status_key(receipt).and_then(|key|self.browser_upload_availability.get(&key))
                .is_some_and(|status|status["available"]==true)
    }

    fn filtered_export_data(&self,_frame:&WorkFrame,exported:&Value)->Value {
        let mut filtered=exported.clone();
        if let Some(receipt)=filtered.get("browser_upload_receipt").cloned() {
            if !self.browser_receipt_is_active(&receipt) {
                if let Some(fields)=filtered.as_object_mut() {fields.remove("browser_upload_receipt");fields.remove("browser_upload_availability");}
            }
        }
        // A producer is retained as a citation for the Organizer; its full
        // material and operation history stays in the notebook/event log.
        filtered
    }

    fn compact_current_result(&self,frame:&WorkFrame,output:&Value)->Value {
        let modified_files=output["modified_files"].as_object().map(|files|files.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
        let compact=json!({"work_id":output["id"],"node_id":output["node_id"],"revision":output["revision"],
            "status":if self.effectively_done(frame){"done".to_owned()}else{format!("{:?}",frame.status).to_ascii_lowercase()},
            "done":output["done"],"execution_status":output["execution_status"],
            "outcome":output["outcome"],"summary":output["summary"],
            "checks":output["checks"],"modified_files":modified_files,"finding_ids":output["finding_ids"],
            "material_ids":output["material_ids"],"exported_data":self.filtered_export_data(frame,&output["exported_data"]),
            "limitations":output["limitations"],
            "visual_artifact_ids":output["visual_artifact_ids"],"visual_check_result":output["visual_check_result"],
            "blocked":output["blocked"],"need_split":output["need_split"],"upstream_problem":output["upstream_problem"],
            "suggested_children":compact_suggested_children(&output["suggested_children"])});
        // Keep only the result Organizer needs to route next. Full exports,
        // source and execution history remain available to Worker/notebook.
        let mut compacted=organizer_compact(&compact,0);
        let mut omissions=Vec::new();let mut omitted_total=0;
        organizer_omissions(&compact,"current_result",0,&mut omissions,&mut omitted_total);
        if omitted_total>0 {
            compacted["context_omissions"]=json!({"items":omissions,"omitted_item_count":omitted_total.saturating_sub(24),
                "reference":{"method":"read_task_result","work_id":frame.order.id,"fields":"request the listed field paths"}});
        }
        compacted
    }

    pub fn resolve_dependency_deliveries(&self, order: &WorkOrder) -> Result<Vec<Value>, String> {
        let mut results = Vec::new();
        let mut processed_targets = BTreeSet::new();

        // 1. Process explicit dependency_inputs with field and revision constraints
        for (index,dep) in order.dependency_inputs.iter().enumerate() {
            let req_rev = dep.get("revision").and_then(Value::as_u64).map(|r| r as usize);
            let reference_field=if dep["work_id"].is_string(){"work_id"}else if dep["node_id"].is_string(){"node_id"}else{"work_id"};
            let frame = declared_dependency_frame(&self.frames, dep, true)
                .map_err(|error|format!("field_path=orders[0].dependency_inputs[{index}].{reference_field}: {error}"))?;

            if !self.effectively_done(frame) {
                return Err(format!("field_path=orders[0].dependency_inputs[{index}].{reference_field}: TASK_NOT_RETURNED: work_id='{}' node_id='{}' revision={} has not returned yet (status: {:?}); use its result after its TaskReturn",
                    frame.order.id,frame.order.node_id,frame.order.revision,frame.status));
            }

            let output = match frame.output.as_ref() {
                Some(o) => o,
                None => {
                    return Err(format!("field_path=orders[0].dependency_inputs[{index}].{reference_field}: work_id='{}' is completed but has no output", frame.order.id));
                }
            };

            if let Some(req_r) = req_rev {
                if frame.order.revision != req_r {
                    return Err(format!("field_path=orders[0].dependency_inputs[{index}].revision: REVISION_MISMATCH: work_id='{}' node_id='{}' requested revision {}, actual delivery is revision {}",
                        frame.order.id,frame.order.node_id,req_r,frame.order.revision));
                }
            }

            let mut filtered = json!({
                "id": output["id"],
                "node_id": output["node_id"],
                "revision": output["revision"],
                "plan_revision": output["plan_revision"],
                "goal": output["goal"],
                "visual_artifact_ids":output["visual_artifact_ids"],"visual_check_result":output["visual_check_result"],
                "done": output["done"],
                "summary": output["summary"],
            });

            if let Some(fields) = dep.get("fields").and_then(Value::as_array) {
                let mut missing = Vec::new();
                let mut exported = json!({});
                for f in fields {
                    if let Some(field_name) = f.as_str() {
                        let http_value=if field_name=="http_observations" {
                            match self.http_observations_for_dependency(frame,dep.get("http_urls")) {
                                Ok(items) if !items.is_empty()=>Some(Value::Array(items)),
                                Ok(_)=>None,
                                Err(error)=>return Err(format!("field_path=orders[0].dependency_inputs[{index}].http_urls: {error}")),
                            }
                        } else if field_name=="browser_upload_receipt" {
                            let receipt=output.pointer("/exported_data/browser_upload_receipt");
                            receipt.filter(|receipt|self.browser_receipt_is_active(receipt)).cloned()
                        } else {None};
                        let delivered_value=if field_name=="http_observations" {
                            http_value.as_ref()
                        } else if field_name=="browser_upload_receipt" {
                            output.pointer("/exported_data/browser_upload_receipt").filter(|receipt|self.browser_receipt_is_active(receipt))
                        } else {
                            output.pointer(&format!("/exported_data/{field_name}")).or_else(||output.get(field_name))
                        };
                        if let Some(val) = delivered_value {
                            filtered[field_name] = val.clone();
                            exported[field_name] = val.clone();
                        } else {
                            missing.push(field_name.to_string());
                        }
                    }
                }
                if !missing.is_empty() {
                    return Err(format!("field_path=orders[0].dependency_inputs[{index}].fields: FIELD_NOT_EXPORTED: work_id='{}' node_id='{}' revision={} is missing required delivery fields: {}; available output fields: [{}]; available exported_data fields: [{}]",
                        frame.order.id,frame.order.node_id,frame.order.revision,missing.join(", "),
                        output.as_object().map(|fields|fields.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default(),
                        self.filtered_export_data(frame,&output["exported_data"]).as_object()
                            .map(|fields|fields.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default()));
                }
                if !exported.as_object().unwrap().is_empty() {
                    filtered["exported_data"] = exported;
                }
                if let Some(receipt)=filtered.pointer("/exported_data/browser_upload_receipt") {
                    if let Some(key)=browser_upload_status_key(receipt) {
                        filtered["browser_upload_availability"]=self.browser_upload_availability.get(&key).cloned().unwrap_or(json!({"available":false,"reason":"host_validation_not_run"}));
                    }
                }
            } else if dep.get("http_urls").is_some() {
                let observations=self.http_observations_for_dependency(frame,dep.get("http_urls"))
                    .map_err(|error|format!("field_path=orders[0].dependency_inputs[{index}].http_urls: {error}"))?;
                if observations.is_empty() {return Err(format!("field_path=orders[0].dependency_inputs[{index}].http_urls: no matching HTTP observations were delivered"));}
                filtered["http_observations"]=json!(observations);
                filtered["exported_data"]=json!({"http_observations":observations});
            } else if let Some(exp) = output.get("exported_data") {
                filtered["exported_data"] = self.filtered_export_data(frame,exp);
                if let Ok(observations)=self.http_observations_for_dependency(frame,None) {
                    if !observations.is_empty() {filtered["exported_data"]["http_observations"]=json!(observations);}
                }
            }

            processed_targets.insert(frame.order.id.clone());
            results.push(filtered);
        }

        // 2. Process any remaining upstream_ids not covered by dependency_inputs
        for up_id in &order.upstream_ids {
            if processed_targets.contains(up_id) {
                continue;
            }
            let matching_frame = dependency_frame(&self.frames, DependencyTarget::Alias(up_id), None);

            if let Some(frame) = matching_frame {
                if frame.status != WorkStatus::Done {
                    return Err(format!("TASK_NOT_RETURNED: upstream dependency '{}' has not returned yet (status: {:?})", frame.order.id, frame.status));
                }
                let output = match frame.output.as_ref() {
                    Some(o) => o,
                    None => {
                        return Err(format!("upstream dependency '{}' is completed but has no output", frame.order.id));
                    }
                };
                let mut deliverable = json!({
                    "id": output["id"],
                    "visual_artifact_ids":output["visual_artifact_ids"],"visual_check_result":output["visual_check_result"],
                    "node_id": output["node_id"],
                    "revision": output["revision"],
                    "plan_revision": output["plan_revision"],
                    "goal": output["goal"],
                    "done": output["done"],
                    "outcome": output["outcome"],
                    "summary": output["summary"],
                    "findings": output["findings"],
                    "material_ids": output["material_ids"],
                    "finding_ids": output["finding_ids"],
                    "modified_files": output["modified_files"],
                    "checks": output["checks"],
                });
                if let Some(exp) = output.get("exported_data") {
                    deliverable["exported_data"] = self.filtered_export_data(frame,exp);
                    if let Ok(observations)=self.http_observations_for_dependency(frame,None) {
                        if !observations.is_empty() {deliverable["exported_data"]["http_observations"]=json!(observations);}
                    }
                }
                processed_targets.insert(frame.order.id.clone());
                results.push(deliverable);
            } else {
                return Err(missing_dependency_reason(&self.frames, DependencyTarget::Alias(up_id), up_id, None));
            }
        }

        Ok(results)
    }

    pub fn validate_dependency_inputs(&self, order: &WorkOrder) -> Result<(), String> {
        self.resolve_dependency_deliveries(order).map(|_| ())
    }

    pub fn activate_task(&mut self, target_id: &str) -> Result<bool> {
        self.refresh_http_check_validity();
        let Some(index) = self.queue.iter().position(|id| id == target_id) else {
            return Ok(false);
        };
        let id = self.queue.remove(index).unwrap();
        let frame = self.frames.get(&id).ok_or_else(|| anyhow::anyhow!("unknown work frame: {id}"))?;
        ensure!(frame.invalidated_by_plan_revision.is_none(), "cannot activate invalidated work");
        ensure!(frame.order.upstream_ids.iter().all(|up| {
            self.frames.get(up).is_some_and(|u| u.status == WorkStatus::Done && u.invalidated_by_plan_revision.is_none())
        }), "selected work has unfinished upstream dependencies");
        if let Err(err) = self.validate_dependency_inputs(&frame.order) {
            anyhow::bail!("dependency inputs validation failed: {err}");
        }

        let upstream_versions = frame.order.upstream_ids.iter()
            .flat_map(|up_id| self.frames[up_id].versions.clone()).collect::<BTreeMap<_, _>>();
        self.current = id.clone();
        self.activated_history.push(id);
        let cur_frame = self.frames.get_mut(&self.current).unwrap();
        for (path, hash) in upstream_versions {
            cur_frame.versions.entry(path).or_insert(hash);
        }
        cur_frame.status = WorkStatus::Running;
        cur_frame.started_at.get_or_insert_with(now_ms);
        self.handoff = None;
        self.consume_bound_http_checks();
        Ok(true)
    }

    /// Clear legacy shell-check pass records from unfinished work and surface
    /// the contracts that need an Organizer-created revision.
    pub fn prepare_legacy_check_migration(&mut self)->Vec<Value> {
        let mut migrations=Vec::new();
        for frame in self.frames.values_mut().filter(|frame|frame.invalidated_by_plan_revision.is_none()&&frame.status!=WorkStatus::Done) {
            let legacy=frame.order.checks.iter().filter(|check|!supported_check_key(check)).cloned().collect::<Vec<_>>();
            if legacy.is_empty(){continue;}
            for check in &legacy {frame.checked.remove(check);frame.http_check_samples.remove(check);frame.check_errors.remove(check);}
            migrations.push(json!({"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,
                "checks":legacy,"action":"revisit this unfinished node with replacement_checks containing only supported check identifiers"}));
        }
        if !migrations.is_empty() {
            let mut handoff=self.handoff.take().unwrap_or(json!({"done":false,"outcome":"legacy_check_contract_migration",
                "reason":"This unfinished work contract contains retired shell command check keys. Rebuild its execution contract as a new node revision; do not interpret or execute the old command strings."}));
            handoff["legacy_check_contract_migration_required"]=json!(true);
            handoff["legacy_check_contracts"]=json!(migrations);
            self.handoff=Some(handoff);
        }
        migrations
    }

    pub fn select_task(&mut self, target: Option<&str>) -> Result<bool> {
        if let Some(target) = target {
            if !self.current.is_empty() && (self.current == target || self.node() == target) {
                if let Some(frame) = self.frames.get_mut(&self.current) {
                    if frame.status != WorkStatus::Done && frame.invalidated_by_plan_revision.is_none() {
                        frame.status = WorkStatus::Running;
                        self.handoff = None;
                        return Ok(true);
                    }
                }
            }
            if let Some(found_id) = self.queue.iter().find(|id| *id == target || self.frames.get(*id).is_some_and(|f| f.order.node_id == target)).cloned() {
                return self.activate_task(&found_id);
            }
            if let Some((found_id, _)) = self.frames.iter().find(|(id, f)| {
                (*id == target || f.order.node_id == target) && f.status != WorkStatus::Done && f.invalidated_by_plan_revision.is_none()
            }) {
                let id = found_id.clone();
                self.current = id.clone();
                if let Some(cur_frame) = self.frames.get_mut(&self.current) {
                    cur_frame.status = WorkStatus::Running;
                }
                self.handoff = None;
                return Ok(true);
            }
        }
        self.activate_next()
    }

    pub fn activate_next(&mut self) -> Result<bool> {
        if !self.current.is_empty() && !self.done() { return Ok(false); }
        let next = self.queue.iter().position(|id| {
            let f = &self.frames[id];
            f.invalidated_by_plan_revision.is_none() &&
                f.order.upstream_ids.iter().all(|upstream| {
                    self.frames.get(upstream).is_some_and(|u| u.status == WorkStatus::Done && u.invalidated_by_plan_revision.is_none())
                }) &&
                self.validate_dependency_inputs(&f.order).is_ok()
        });
        if let Some(index) = next {
            let id = self.queue[index].clone();
            return self.activate_task(&id);
        }
        if !self.queue.is_empty() {
            let first_id = &self.queue[0];
            let first_order = &self.frames[first_id].order;
            let reason = if let Err(err) = self.validate_dependency_inputs(first_order) {
                format!("queued work has unmet dependency inputs: {err}")
            } else {
                "queued work has unmet upstream results".to_string()
            };
            self.handoff = Some(json!({"done": false, "reason": reason}));
        }
        Ok(false)
    }

    pub fn continue_current(&mut self) -> Result<()> {
        ensure!(self.frame().is_some_and(|f| f.status != WorkStatus::Done && f.invalidated_by_plan_revision.is_none()), "there is no unfinished work to continue");
        self.handoff = None;
        let frame = self.frames.get_mut(&self.current).unwrap();
        frame.status = WorkStatus::Running;
        frame.output = None;
        frame.reviewed_without_change = frame.rounds_without_change;
        Ok(())
    }

    pub fn continue_decision(&mut self, decision: &Value, can_write: bool, can_check: bool) -> Result<()> {
        let current = self.order().ok_or_else(|| anyhow::anyhow!("no unfinished work to continue"))?.clone();
        let (rounds, idle, streak) = {
            let frame = self.frame().unwrap();
            (frame.rounds, frame.rounds_without_change, frame.continues_without_progress)
        };
        let mut changed_inputs = false;
        if let Some(orders) = decision.get("orders") {
            ensure!(orders.as_array().is_some_and(|orders| orders.len() == 1), "continue accepts exactly the current unfinished order; new work uses action=work");
            let mut order = current.clone();
            let patch = orders[0].as_object().ok_or_else(|| anyhow::anyhow!("continued order must be an object"))?;
            let mut merged = serde_json::to_value(&order)?;
            for (key, value) in patch { merged[key] = value.clone(); }
            order = serde_json::from_value(merged)?;
            normalize_dependencies_at(&self.frames, &mut order,"orders[0]")?;
            if current.node_id == format!("work_{}", current.id) && order.node_id == "direct" {
                order.node_id = current.node_id.clone();
            }
            ensure!(order.id == current.id, "continue must address the current work id");
            changed_inputs = order.constraints != current.constraints || order.material_ids != current.material_ids || order.finding_ids != current.finding_ids || order.material_ranges != current.material_ranges || order.final_answer != current.final_answer;
            ensure!(order.upstream_ids.iter().all(|id| self.frames.get(id).is_some_and(|f| f.status == WorkStatus::Done && f.invalidated_by_plan_revision.is_none())), "continued work needs finished upstream outputs");
            if let Err(err) = self.validate_dependency_inputs(&order) {
                anyhow::bail!("continued work has unmet dependency inputs: {err}");
            }
            self.enqueue(vec![order], can_write, can_check)?;
            self.queue.retain(|id| id != &current.id);
            self.current = current.id.clone();
        }
        if rounds > 0 && idle > 1 && !changed_inputs {
            ensure!(streak < 2, "this work was continued twice without a new file change, check, material or finding; assign the next child, use the sealed output, or report the blocker");
        }
        self.continue_current()?;
        let frame = self.frames.get_mut(&self.current).unwrap();
        frame.continues_without_progress = if rounds > 0 && idle > 1 && !changed_inputs { streak + 1 } else { 0 };
        frame.organizer_guidance = json!({
            "instruction": limited(decision["reason"].as_str().unwrap_or(""), 2400),
            "summary": limited(decision["summary"].as_str().unwrap_or(""), 2400),
            "final_answer": frame.order.final_answer
        });
        Ok(())
    }

    /// A report is part of the current task, not a return. Only a bounded run
    /// of rounds without any operation or TaskReturn hands off as a stall.
    pub fn finish_round(&mut self, project_operations: usize, repeated_reads: usize) {
        if self.finished { return; }
        let Some(frame) = self.frames.get_mut(&self.current) else { return; };
        if frame.status == WorkStatus::Done { return; }
        frame.rounds += 1;
        frame.rounds_without_change += 1;
        frame.repeated_reads = if repeated_reads > 0 { frame.repeated_reads + repeated_reads } else { 0 };
        // Rewording a report never resets this count; only an actual operation does.
        frame.idle_rounds = if project_operations == 0 { frame.idle_rounds + 1 } else { 0 };
        let (kind, reason) = if frame.idle_rounds >= REPORT_ONLY_ROUND_LIMIT {
            ("report_only", format!("Worker reported or replied for {} consecutive rounds without an actual operation or TaskReturn; the task has not returned. Continue it once with a narrower instruction, schedule a different approach, or finish with the limitation", frame.idle_rounds))
        } else if frame.repeated_reads >= 3 {
            ("repeated_reads", "Repeated reads returned no new information; use the current materials or narrow the missing question".to_owned())
        } else if frame.rounds_without_change.saturating_sub(frame.reviewed_without_change) >= 12 {
            ("no_change", "This work unit ran twelve rounds without a changed file or a successful new check since its last organization; narrow the missing fact, split the problem or return a concrete blocker".to_owned())
        } else {
            return;
        };
        let goal = stall_key(&frame.order.goal);
        let rounds = frame.idle_rounds;
        if self.handoff.is_none() { *self.stalled_goals.entry(goal).or_insert(0) += 1; }
        self.request_handoff(&reason);
        if let Some(handoff) = self.handoff.as_mut().filter(|handoff| handoff["done"] == false) {
            handoff["stall"] = json!({"kind":kind,"rounds_without_operation":rounds,"task_returned":false});
        }
    }

    pub fn all_done(&self) -> bool {
        !self.frames.is_empty() && self.queue.is_empty() && self.frames.values().filter(|f| f.invalidated_by_plan_revision.is_none()).all(|frame| self.effectively_done(frame))
    }

    pub fn finish(&mut self, summary: &str, blocked: bool) -> Result<()> {
        ensure!(!summary.trim().is_empty(), "finish requires an actual result or blocker summary");
        // This is Organizer's explicit assessment. Queue state is a host
        // execution fact, not a veto or substitute for the goal judgment.
        self.finished = true;
        self.request_completed = Some(!blocked);
        if !blocked { self.handoff = None; }
        self.final_result = limited(summary, 6000);
        Ok(())
    }

    fn http_check_sample_is_current(&self, frame: &WorkFrame, check: &str) -> bool {
        let Some(sample_id) = frame.http_check_samples.get(check) else { return false; };
        let Some(sample) = self.http_observation_samples.get(sample_id) else { return false; };
        let Some(url) = check.strip_prefix("http-probe:") else { return false; };
        if sample["sample_id"].as_str() != Some(sample_id)
            || crate::http_probe::check_key(&json!({"url": url}))
                != crate::http_probe::check_key(&json!({"url": sample["url"]}))
            || sample["check_passed"] != true
            || !self.http_sample_freshness(sample).0
        {
            return false;
        }
        let Some(producer_id) = sample["producer_work_id"].as_str() else { return false; };
        producer_id == frame.order.id
            || self.frames.get(producer_id).is_some_and(|producer| {
                producer.status == WorkStatus::Done && producer.invalidated_by_plan_revision.is_none()
            })
    }

    /// An unfinished check needs a reusable sample when it closes. Expiry never
    /// reopens completed work or withdraws its historical delivery; backtracking
    /// requires an explicit Organizer revisit.
    pub fn refresh_http_check_validity(&mut self) -> bool {
        let Some(frame) = self.frame().filter(|frame| frame.status != WorkStatus::Done
            && frame.invalidated_by_plan_revision.is_none()) else { return false; };
        let stale = frame.order.checks.iter()
            .filter(|check| check.starts_with("http-probe:") && frame.checked.get(*check) == Some(&frame.epoch))
            .filter(|check| !self.http_check_sample_is_current(frame, check))
            .map(|check| {
                let sample_id = frame.http_check_samples.get(check).cloned().unwrap_or_default();
                let reason = if sample_id.is_empty() { "missing_sample_binding" }
                    else if !self.http_observation_samples.contains_key(&sample_id) { "sample_missing" }
                    else {
                        let sample = &self.http_observation_samples[&sample_id];
                        self.http_sample_freshness(sample).2.unwrap_or("producer_not_completed")
                    };
                (check.clone(), sample_id, reason.to_owned())
            }).collect::<Vec<_>>();
        if stale.is_empty() { return false; }
        let Some(frame) = self.frames.get_mut(&self.current) else { return false; };
        for (check, sample_id, reason) in stale {
            frame.checked.remove(&check);
            frame.http_check_samples.remove(&check);
            frame.check_errors.insert(check.clone(), json!({
                "source_epoch": frame.epoch,
                "sample_id": if sample_id.is_empty() { Value::Null } else { json!(sample_id) },
                "invalidation_reason": reason,
                "error": format!("The HTTP check sample is no longer valid ({reason}); obtain a fresh sample before marking this work complete.")
            }));
        }
        true
    }

    /// Refresh runtime availability without changing historical work status or
    /// output. Invalid upload receipts stop flowing as usable dependency data.
    pub async fn refresh_browser_upload_validity(&mut self,root:&std::path::Path,task_id:&str)->bool {
        // Deprecated producers keep their receipt: the page it describes is a
        // host resource whose current state is still worth validating.
        let mut receipts=self.frames.values()
            .filter_map(|frame|frame.browser_upload_receipt.as_ref().and_then(|receipt|{
                let value=serde_json::to_value(receipt).ok()?;
                let key=browser_upload_status_key(&value)?;
                Some((frame.sequence,key,frame.order.id.clone(),frame.order.node_id.clone(),frame.order.revision,value))
            })).collect::<Vec<_>>();
        receipts.sort_by_key(|item|item.0);
        let mut seen=BTreeSet::new();
        let mut changed=false;
        for (_,key,work_id,node_id,revision,receipt) in receipts {
            if !seen.insert(key.clone()) {continue;}
            let mut status=crate::browser_control::validate_upload_receipt(root,task_id,&receipt).await;
            status["work_id"]=json!(work_id);
            status["node_id"]=json!(node_id);
            status["revision"]=json!(revision);
            if status["available"]!=true {
                if let Some(page_key)=browser_page_key(&receipt["page"]) {
                    if self.active_browser_uploads.get(&page_key).map(String::as_str)==receipt["upload_attempt_id"].as_str() {
                        self.active_browser_uploads.remove(&page_key);
                    }
                }
            }
            if self.browser_upload_availability.get(&key)!=Some(&status) {changed=true;}
            self.browser_upload_availability.insert(key,status);
        }
        changed
    }

    fn page_matches(left:&Value,right:&Value)->bool {
        left["browser_session_id"].as_str().is_some()&&left["page_id"].as_str().is_some()&&left["page_epoch"].as_u64().is_some()
            &&left["browser_session_id"]==right["browser_session_id"]&&left["page_id"]==right["page_id"]&&left["page_epoch"]==right["page_epoch"]
    }

    fn same_browser_document(left:&Value,right:&Value)->bool {
        !left.is_null()&&!right.is_null()&&left["browser_session_id"]==right["browser_session_id"]&&left["page_id"]==right["page_id"]&&left["url"]==right["url"]
    }

    fn latest_current_browser_read(f:&WorkFrame)->Option<&Value> {
        f.browser_current_read.as_ref().filter(|read|read["read_succeeded"]!=false
            &&Self::page_matches(&read["page"],&f.browser_page))
    }

    fn browser_upload_receipt_for(&self,f:&WorkFrame)->Option<BrowserUploadEvidence> {
        let is_active=|receipt:&BrowserUploadEvidence|browser_page_key(&receipt.page).is_some_and(|key|self.active_browser_uploads.get(&key)==Some(&receipt.upload_attempt_id));
        if let Some(receipt)=f.browser_upload_receipt.as_ref().filter(|receipt|is_active(receipt)) {return Some(receipt.clone());}
        for dep in &f.order.dependency_inputs {
            let fields=dep.get("fields").and_then(Value::as_array);
            if fields.is_some_and(|fields|!fields.iter().any(|field|field.as_str()==Some("browser_upload_receipt"))) {continue;}
            let Ok(source)=declared_dependency_frame(&self.frames,dep,true) else {continue;};
            if !self.effectively_done(source) {continue;}
            let Some(receipt)=source.browser_upload_receipt.as_ref().filter(|receipt|is_active(receipt)) else {continue;};
            let Some(exported)=source.output.as_ref().and_then(|output|output.pointer("/exported_data/browser_upload_receipt")) else {continue;};
            if serde_json::to_value(receipt).ok().as_ref()==Some(exported) {return Some(receipt.clone());}
        }
        None
    }

    fn browser_presentation_loaded(&self,f:&WorkFrame)->bool {
        let Some(expected_path)=f.order.browser_document_path.as_deref() else {return false;};
        let Some(receipt)=self.browser_upload_receipt_for(f) else {return false;};
        if !browser_paths_equal(&receipt.path,expected_path) || !receipt.change_event_received {return false;}
        let Some(read)=Self::latest_current_browser_read(f) else {return false;};
        let loaded=&read["document_loaded"];let read_page=&read["page"];
        let indicators_valid=loaded["valid_page_indicator"]==true||valid_page_indicator(&loaded["page_indicator"]);
        let attempt_matches=!receipt.load_attempt_required||(loaded["confirmation"]=="application_load_cycle"
            &&loaded["upload_attempt_id"]==receipt.upload_attempt_id&&loaded["upload_change_observed"]==true
            &&loaded["upload_load_status"]=="loaded");
        Self::same_browser_document(&receipt.page,read_page)&&Self::page_matches(read_page,&f.browser_page)
            &&read_page["page_epoch"].as_u64().zip(receipt.page["page_epoch"].as_u64()).is_some_and(|(current,uploaded)|current>=uploaded)
            &&loaded["status"]=="loaded"&&loaded["slides_detected"].as_u64().is_some_and(|count|count>0)
            &&indicators_valid&&loaded["error_indicators"].as_array().is_none_or(|errors|errors.is_empty())&&attempt_matches
    }

    fn attach_active_browser_upload(active:&BTreeMap<String,String>,f:&WorkFrame,exported_data:&mut Value) {
        let Some(receipt)=f.browser_upload_receipt.as_ref() else {return;};
        if !browser_page_key(&receipt.page).is_some_and(|key|active.get(&key)==Some(&receipt.upload_attempt_id)) {return;}
        if !exported_data.is_object() {let prior=std::mem::replace(exported_data,Value::Null);*exported_data=json!({"worker_data":prior});}
        if let Ok(value)=serde_json::to_value(receipt) {exported_data["browser_upload_receipt"]=value;}
    }

    fn attach_http_observations(observations:Vec<Value>,exported_data:&mut Value) {
        if let Some(fields)=exported_data.as_object_mut() {fields.remove("http_observations");}
        if observations.is_empty() {return;}
        if !exported_data.is_object() {
            let prior=std::mem::replace(exported_data,Value::Null);
            *exported_data=json!({"worker_data":prior});
        }
        exported_data["http_observations"]=json!(observations);
    }

    fn visual_check_is_current(f:&WorkFrame)->bool {
        let result=&f.visual_check_result;let binding=&result["source_binding"];
        !result.is_null() && matches!(result["assessment"].as_str(),Some("pass"|"issue"))
            && binding["execution_epoch"]==json!(f.epoch)
            && binding["related_source_versions"]==json!(f.versions)
            && binding["current_page"]==f.browser_page
            && result["current_result_artifact_ids"].as_array().is_some_and(|ids|!ids.is_empty())
    }

    pub fn verification_due(&self) -> bool {
        self.frame().is_some_and(|f| !f.order.checks.is_empty() && f.check_errors.is_empty()
            && (f.order.edit_targets.is_empty() || f.order.edit_targets.iter().all(|target|f.writes.contains_key(&path(target)))))
    }

    pub fn reusable_http_probe(&self,args:&Value)->Option<Value> {
        if args["reason"].as_str().is_some_and(|reason|!reason.trim().is_empty()) {return None;}
        let check=crate::http_probe::check_key(args);
        let observation=self.http_probe_results.get(&check)?;
        let sample_id=observation["sample_id"].as_str()?;
        let sample=self.http_observation_samples.get(sample_id)?;
        if !self.http_sample_freshness(sample).0 {return None;}
        let mut result=sample.clone();
        if !result.is_object() {return None;}
        if args["timeout_ms"].as_u64().is_some_and(|timeout|timeout.clamp(100,30_000)!=result["timeout_ms"].as_u64().unwrap_or(5_000)) {return None;}
        let age_ms=self.http_sample_freshness(sample).1;
        result["reused"]=json!(true);
        result["reuse_age_ms"]=json!(age_ms);
        result["fresh_for_cache_reuse"]=json!(true);
        result["guidance"]=json!("A fresh result for this exact local URL was reused; use its sampled_at and response details instead of probing again.");
        Some(result)
    }

    /// The host already knows the latest sample for this URL is no longer
    /// current, so a new observation needs no extra justification.
    fn http_previous_sample_invalid(&self,args:&Value)->bool {
        self.http_probe_results.get(&crate::http_probe::check_key(args)).and_then(|entry|entry["sample_id"].as_str())
            .and_then(|id|self.http_observation_samples.get(id)).is_some_and(|sample|!self.http_sample_freshness(sample).0)
    }

    /// Previous sample for the same URL, reported with a new observation so the
    /// Worker sees why it was not reused.
    pub fn http_previous_sample(&self,args:&Value)->Value {
        let Some(sample)=self.http_probe_results.get(&crate::http_probe::check_key(args)).and_then(|entry|entry["sample_id"].as_str())
            .and_then(|id|self.http_observation_samples.get(id)) else {return Value::Null;};
        let (fresh,age_ms,reason)=self.http_sample_freshness(sample);
        json!({"sample_id":sample["sample_id"],"sampled_at":sample["sampled_at"],"http_status":sample["http_status"],"error_kind":sample["error_kind"],
            "age_ms":age_ms,"fresh_for_cache_reuse":fresh,"cache_invalidation_reason":reason})
    }

    fn http_sample_freshness(&self, sample:&Value)->(bool,u64,Option<&'static str>) {
        let sampled_at=sample["sampled_at"].as_str().and_then(|value|chrono::DateTime::parse_from_rfc3339(value).ok());
        let age=sampled_at.map(|value|chrono::Utc::now().signed_duration_since(value.with_timezone(&chrono::Utc)).num_milliseconds());
        let age_ms=age.unwrap_or(0).max(0) as u64;
        let reason=if sample["host_instance_id"].as_str()!=Some(crate::project_process::host_instance_id()) {
            Some("host_restarted")
        } else if sample["process_event_generation"].as_u64()!=Some(crate::project_process::process_event_generation()) {
            Some("process_state_changed")
        } else if sample["scheduler_generation"].as_u64()!=Some(self.http_probe_generation) {
            Some("workspace_or_process_changed")
        } else if sample["producer_work_id"].as_str().is_none_or(|work_id|self.frames.get(work_id).is_none_or(|frame|frame.invalidated_by_plan_revision.is_some())) {
            Some("producer_deprecated")
        } else if self.http_probe_results.get(&crate::http_probe::check_key(&json!({"url":sample["url"]}))).and_then(|value|value["sample_id"].as_str())!=sample["sample_id"].as_str() {
            Some("superseded")
        } else if age.is_none() || age.is_some_and(|value|value<0) {
            Some("invalid_sample_time")
        } else if age_ms>sample["reuse_window_ms"].as_u64().unwrap_or(crate::http_probe::REUSE_WINDOW_MS) {
            Some("expired")
        } else {None};
        (reason.is_none(),age_ms,reason)
    }

    fn http_sample_delivery(&self,sample_id:&str)->Option<Value> {
        let sample=self.http_observation_samples.get(sample_id)?;
        let (fresh,age_ms,invalidation_reason)=self.http_sample_freshness(sample);
        let mut delivered=sample.clone();
        if let Some(object)=delivered.as_object_mut() {
            object.remove("body_summary");
            object.remove("body_summary_truncated");
            object.remove("scheduler_generation");
            object.remove("host_instance_id");
            object.remove("process_event_generation");
        }
        delivered["age_ms"]=json!(age_ms);
        delivered["fresh_for_cache_reuse"]=json!(fresh);
        delivered["cache_invalidation_reason"]=invalidation_reason.map_or(Value::Null,|reason|json!(reason));
        Some(delivered)
    }

    fn frame_http_observations(&self,frame:&WorkFrame)->Vec<Value> {
        frame.http_observation_ids.iter().filter_map(|id|self.http_sample_delivery(id)).collect()
    }

    fn append_frame_http_ids(&mut self,work_id:&str,sample_id:&str) {
        if let Some(frame)=self.frames.get_mut(work_id) {
            if !frame.http_observation_ids.iter().any(|id|id==sample_id) {
                frame.http_observation_ids.push(sample_id.to_owned());
                if frame.http_observation_ids.len()>128 {frame.http_observation_ids.remove(0);}
            }
        }
    }

    /// Attach provenance to a fresh HTTP result before it is shown to the Worker.
    /// Reused samples keep their original producer and sampled_at.
    pub fn record_http_probe(&mut self,args:&Value,result:&mut Value,tool_call_id:&str) {
        if !result.is_object() || result["url"].as_str().is_none() || result["sampled_at"].as_str().is_none() {return;}
        let sample_id=result["sample_id"].as_str().map(str::to_owned).filter(|id|self.http_observation_samples.contains_key(id));
        let sample_id=sample_id.unwrap_or_else(||{
            self.next_http_observation_id=self.next_http_observation_id.saturating_add(1);
            let id=format!("http_sample_{:08}",self.next_http_observation_id);
            let Some(frame)=self.frames.get(&self.current) else {return id;};
            result["sample_id"]=json!(id);
            result["producer_work_id"]=json!(frame.order.id);
            result["node_id"]=json!(frame.order.node_id);
            result["revision"]=json!(frame.order.revision);
            result["source_tool_call_id"]=json!(tool_call_id);
            result["scheduler_generation"]=json!(self.http_probe_generation);
            result["host_instance_id"]=json!(crate::project_process::host_instance_id());
            result["process_event_generation"]=json!(crate::project_process::process_event_generation());
            let previous=self.http_previous_sample(args);
            self.http_observation_samples.insert(id.clone(),result.clone());
            if !previous.is_null() {result["previous_sample"]=previous;}
            result["fresh_for_cache_reuse"]=json!(true);
            self.http_probe_results.insert(crate::http_probe::check_key(args),json!({"sample_id":id}));
            id
        });
        if self.http_observation_samples.contains_key(&sample_id) {self.append_frame_http_ids(&self.current.clone(),&sample_id);}
    }

    fn http_observations_for_dependency(&self,frame:&WorkFrame,urls:Option<&Value>)->Result<Vec<Value>,String> {
        let mut observations=self.frame_http_observations(frame);
        if let Some(urls)=urls.and_then(Value::as_array) {
            let requested=urls.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            observations.retain(|sample|requested.iter().any(|url|crate::http_probe::check_key(&json!({"url":url}))==crate::http_probe::check_key(&json!({"url":sample["url"]}))));
            let found=observations.iter().map(|sample|crate::http_probe::check_key(&json!({"url":sample["url"]}))).collect::<BTreeSet<_>>();
            let missing=requested.into_iter().filter(|url|!found.contains(&crate::http_probe::check_key(&json!({"url":url})))).collect::<Vec<_>>();
            if !missing.is_empty() {return Err(format!("requested HTTP observations are unavailable from this delivery: {}",missing.join(", ")));}
        }
        Ok(observations)
    }

    fn consume_bound_http_checks(&mut self) {
        let Some(order)=self.order().cloned() else {return;};
        let mut consumed=Vec::<(String,String,Value)>::new();
        for check in order.checks.iter().filter(|check|check.starts_with("http-probe:")) {
            let Some(url)=check.strip_prefix("http-probe:") else {continue;};
            let check_key=crate::http_probe::check_key(&json!({"url":url}));
            for dep in &order.dependency_inputs {
                if !dep["http_urls"].as_array().is_some_and(|urls|urls.iter().any(|candidate|candidate.as_str().is_some_and(|candidate|crate::http_probe::check_key(&json!({"url":candidate}))==check_key))) {continue;}
                let Ok(source)=declared_dependency_frame(&self.frames,dep,true) else {continue;};
                if !self.effectively_done(source) {continue;}
                let sample=source.http_observation_ids.iter().filter_map(|id|self.http_observation_samples.get(id))
                    .filter(|sample|crate::http_probe::check_key(&json!({"url":sample["url"]}))==check_key&&self.http_sample_freshness(sample).0)
                    .max_by_key(|sample|sample["sampled_at"].as_str().unwrap_or(""));
                if let Some(sample)=sample {consumed.push((check.clone(),sample["sample_id"].as_str().unwrap_or("").to_owned(),sample.clone()));break;}
            }
        }
        for (check,sample_id,sample) in consumed {
            if sample_id.is_empty() {continue;}
            self.append_frame_http_ids(&self.current.clone(),&sample_id);
            let passed=sample["check_passed"]==true;
            if let Some(frame)=self.frames.get_mut(&self.current) {
                if passed {
                    frame.checked.insert(check.clone(),frame.epoch);
                    frame.http_check_samples.insert(check.clone(),sample_id.clone());
                    frame.check_errors.remove(&check);
                } else {
                    frame.checked.remove(&check);
                    frame.http_check_samples.remove(&check);
                    frame.check_errors.insert(check.clone(),json!({"source_epoch":frame.epoch,"http_status":sample["http_status"],
                        "reachable":sample["reachable"],"error_kind":sample["error_kind"],"error_message":sample["error_message"],
                        "sample_id":sample_id,"error":"A bound fresh HTTP sample did not satisfy the declared 2xx check."}));
                }
                if !frame.operations.iter().any(|operation|operation["auto_consumed_http_sample_id"]==sample_id&&operation["check_key"]==check) {
                    frame.operations.push(json!({"tool":"http_probe","url":sample["url"],"check_key":check,"sample_id":sample_id,
                        "auto_consumed_http_sample_id":sample_id,"reused":true,"check_passed":passed,"http_status":sample["http_status"],
                        "sampled_at":sample["sampled_at"],"producer_work_id":sample["producer_work_id"]}));
                }
            }
        }
    }

    pub fn worker_http_observations(&self)->Value {
        let current=self.frame().map(|frame|self.frame_http_observations(frame)).unwrap_or_default();
        let upstream=self.order().and_then(|order|self.resolve_dependency_deliveries(order).ok()).unwrap_or_default();
        let mut bound_upstream=upstream.iter().flat_map(|delivery|delivery["http_observations"].as_array().into_iter().flatten()
            .chain(delivery.pointer("/exported_data/http_observations").and_then(Value::as_array).into_iter().flatten())).cloned().collect::<Vec<_>>();
        bound_upstream.sort_by(|left,right|left["sample_id"].as_str().cmp(&right["sample_id"].as_str()));
        bound_upstream.dedup_by(|left,right|left["sample_id"]==right["sample_id"]);
        json!({"current":current,"bound_upstream":bound_upstream})
    }

    fn http_observation_catalog(&self)->Vec<Value> {
        let mut samples=self.http_probe_results.values().filter_map(|entry|entry["sample_id"].as_str())
            .filter_map(|id|self.http_sample_delivery(id)).collect::<Vec<_>>();
        samples.sort_by(|left,right|right["sampled_at"].as_str().cmp(&left["sampled_at"].as_str()));
        samples.truncate(64);
        samples
    }

    fn may_edit(f: &WorkFrame) -> bool {
        !f.order.edit_targets.is_empty()
    }

    pub fn permits(&self, name: &str, args: &Value) -> Result<()> {
        ensure!(!self.finished && !self.done(), "work is done; the host is handing off its result");
        ensure!(self.handoff.is_none(), "work yielded; remaining operations must wait for Organizer's next assignment");
        let f = self.frame().ok_or_else(|| anyhow::anyhow!("no active work order"))?;
        if matches!(name, "edit_file" | "replace_range" | "write_file") {
            ensure!(Self::may_edit(f), "this work has no authorized repair targets; yield the concrete missing scope to Organizer");
            ensure!(f.order.edit_targets.iter().any(|p| path(p) == path(args["path"].as_str().unwrap_or(""))), "edit target is outside this work order; yield the concrete missing scope to Organizer");
            ensure!(!self.verification_due(), "the declared files were written; execute the outstanding checks now");
        }
        if name=="http_probe" {
            let c=crate::http_probe::check_key(args);
            let previous=self.http_probe_results.get(&c);
            let reason=args["reason"].as_str().is_some_and(|reason|!reason.trim().is_empty());
            let reusable=self.reusable_http_probe(args).is_some();
            ensure!(previous.is_none()||reason||reusable||self.http_previous_sample_invalid(args),
                "this URL was already probed at {}; reuse that sample or include reason explaining why a new HTTP observation is needed",
                previous.and_then(|entry|entry["sample_id"].as_str()).and_then(|id|self.http_observation_samples.get(id)).and_then(|sample|sample["sampled_at"].as_str()).unwrap_or("an earlier time"));
        }
        if crate::project_process::is_check(name) && name != "get_project_process" {
            let c = crate::project_process::check_key(name, args);
            if self.verification_due() {
                ensure!(f.order.checks.contains(&c), "verification work accepts only its declared check commands");
            }
            let http_refresh=name=="http_probe"&&args["reason"].as_str().is_some_and(|reason|!reason.trim().is_empty());
            let http_reuse=name=="http_probe"&&(self.reusable_http_probe(args).is_some()||self.http_previous_sample_invalid(args));
            ensure!(f.checked.get(&c) != Some(&f.epoch)||http_refresh||http_reuse, "this command already succeeded for the current versions; reuse its recorded result");
        }
        if self.verification_due() {
            ensure!(crate::project_process::is_check(name) || matches!(name, "yield_work" | "report_progress" | "stop_project_process"), "the current work awaits its declared checks; broader investigation belongs in another work order");
        }
        Ok(())
    }

    pub fn filter_tools(&self, tools: &mut Vec<Value>) {
        tools.retain(|tool| {
            let name = tool.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
            if self.verification_due() {
                return crate::project_process::is_check(name) || matches!(name, "yield_work" | "report_progress" | "stop_project_process");
            }
            if matches!(name, "edit_file" | "replace_range" | "write_file") {
                return self.frame().is_some_and(Self::may_edit);
            }
            true
        });
    }

    pub fn observe(&mut self, name: &str, args: &Value, result: &Value, failed: bool) {
        self.refresh_http_check_validity();
        let mut observed_http_sample_id = result["sample_id"].as_str().map(str::to_owned);
        if name=="http_probe" {
            let mut recorded=result.clone();
            let call_id=result["source_tool_call_id"].as_str().unwrap_or("legacy_observation").to_owned();
            self.record_http_probe(args,&mut recorded,&call_id);
            observed_http_sample_id = recorded["sample_id"].as_str().map(str::to_owned);
        }
        let work_id=self.current.clone();
        let Some(existing)=self.frames.get(&work_id) else { return; };
        if existing.status==WorkStatus::Done {return;}
        let previous_page=existing.browser_page.clone();
        let page = if !result["page"].is_null() {result["page"].clone()} else if name == "browser_screenshot" {
            let a=&result["visual_artifact"];json!({"browser_session_id":a["browser_session_id"],"page_id":a["page_id"],"page_epoch":a["page_epoch"],"url":a["url"],"viewport":a["viewport"]})
        } else {Value::Null};
        let mut invalidate_pages=Vec::new();
        if matches!(name,"browser_open"|"browser_upload"|"browser_close") {
            if !previous_page.is_null() {invalidate_pages.push(previous_page.clone());}
            if !page.is_null() {invalidate_pages.push(page.clone());}
            if !result["closed_page"].is_null() {invalidate_pages.push(result["closed_page"].clone());}
        }
        for identity in invalidate_pages.iter().filter_map(browser_page_key) {self.active_browser_uploads.remove(&identity);}
        let new_upload_receipt=if name=="browser_upload"&&!failed&&result["status"]=="file_assigned"
            &&result["input"]["change_event_received"]==true&&result["uploaded"].as_str().is_some()&&browser_page_key(&page).is_some() {
            self.next_browser_upload_id=self.next_browser_upload_id.saturating_add(1);
            let supplied_id=result["upload_attempt_id"].as_str().map(str::to_owned);
            let attempt_id=supplied_id.clone().unwrap_or_else(||format!("legacy-upload-{}",self.next_browser_upload_id));
            let receipt_page=json!({"browser_session_id":page["browser_session_id"],"page_id":page["page_id"],"page_epoch":page["page_epoch"],"url":page["url"],
                "request_id":page.pointer("/identity/request_id")});
            let receipt_file=json!({"name":result["file"]["name"],"size_bytes":result["file"]["size_bytes"],"files_length":result["file"]["files_length"]});
            let receipt=BrowserUploadEvidence{upload_attempt_id:attempt_id.clone(),path:path(result["uploaded"].as_str().unwrap()),change_event_received:true,page:receipt_page,file:receipt_file,
                work_id:existing.order.id.clone(),node_id:existing.order.node_id.clone(),revision:existing.order.revision,load_attempt_required:supplied_id.is_some()};
            if let Some(key)=browser_page_key(&page) {self.active_browser_uploads.insert(key,attempt_id);}
            Some(receipt)
        } else {None};
        // Native commands can be read-only (for example git status). Actual
        // source changes invalidate samples in update_versions; managed process
        // operations still invalidate them even without a source change.
        if matches!(name,"install_dependencies"|"run_project_script"|"stop_project_process")
            || matches!(name,"write_file"|"replace_range"|"edit_file") && !failed && result["changed"] != false
            || name == "run_program" && result["verification_inputs_changed"] == true {
            self.http_probe_generation=self.http_probe_generation.saturating_add(1);
        }
        let Some(f) = self.frames.get_mut(&work_id) else { return; };
        if matches!(name,"browser_open"|"browser_upload"|"browser_close") {f.browser_upload_receipt=None;}
        if let Some(receipt)=new_upload_receipt.clone() {f.browser_upload_receipt=Some(receipt);}
        if !failed && crate::project_process::is_tool(name) {
            if let Some(observation)=result.get("process_observation") {
                f.project_observation=observation.clone();
            }
        }
        // A failed capture can mean the page changed while pixels were being read.
        // Revoke prior current-image claims until a stable screenshot succeeds.
        if failed && name=="browser_screenshot" {
            f.epoch+=1;f.current_visual_artifact_ids.clear();f.visual_check_result=Value::Null;
        }
        let browser_mutation=matches!(name,"browser_open"|"browser_click"|"browser_press_key"|"browser_upload"|"browser_close");
        let identity_changed=!f.browser_page.is_null()&&!page.is_null()&&!Self::page_matches(&page,&f.browser_page);
        if identity_changed||browser_mutation||name=="browser_read"||failed&&(matches!(name,"browser_screenshot")||name=="browser_wait"&&args["document_loaded"]==true) {f.browser_current_read=None;}
        if name=="browser_close" || (failed&&(browser_mutation||matches!(name,"browser_read"|"browser_screenshot"))) {
            f.browser_page=Value::Null;f.epoch+=1;f.current_visual_artifact_ids.clear();f.visual_check_result=Value::Null;
        } else if !failed && !page.is_null() {
            if !f.browser_page.is_null() && (page["browser_session_id"]!=f.browser_page["browser_session_id"] || page["page_id"]!=f.browser_page["page_id"] || page["page_epoch"]!=f.browser_page["page_epoch"]) {
                f.epoch+=1;f.current_visual_artifact_ids.clear();f.visual_check_result=Value::Null;
            }
            f.browser_page=page.clone();
        }
        if name=="browser_read"&&!failed&&!page.is_null() {
            let expected=result["expect_text"].as_str().filter(|text|!text.trim().is_empty());
            f.browser_current_read=Some(json!({"read_succeeded":true,"matched":result["matched"],"expect_text":expected,
                "text_assertion":expected.map(|text|json!({"expected":text,"matched":result["matched"]})),
                "page":page,"document_loaded":result["document_loaded"],"text":result["text"],
                "observed_at":now_ms(),"work_id":f.order.id}));
        }
        if !failed && matches!(name,"browser_screenshot"|"view_image") {
            if let Some(id)=result["artifact_id"].as_str() {
                f.visual_artifact_ids.retain(|old|old!=id);f.visual_artifact_ids.push(id.to_owned());
                if f.visual_artifact_ids.len()>8 {f.visual_artifact_ids.remove(0);}
                if name=="view_image" {f.seen_visual_artifact_ids.retain(|old|old!=id);}
                f.visual_original_artifact_ids.retain(|old|old!=id);
                if result["view_original"]==true {f.visual_original_artifact_ids.push(id.to_owned());}
                if name=="browser_screenshot" {
                    let artifact=&result["visual_artifact"];
                    f.browser_page=json!({"browser_session_id":artifact["browser_session_id"],"page_id":artifact["page_id"],"page_epoch":artifact["page_epoch"],"url":artifact["url"],"viewport":artifact["viewport"]});
                    if artifact["artifact_id"]==id {
                        f.current_visual_artifact_ids.clear();f.current_visual_artifact_ids.push(id.to_owned());
                    }
                }
            }
        }
        let file = result.get("path").or_else(|| result.pointer("/symbol/file_path")).or_else(|| args.get("path")).and_then(Value::as_str).map(path);
        if !failed {
            if let (Some(file), Some(hash)) = (file.clone(), result.get("code_hash").or_else(|| result.pointer("/description/code_hash")).and_then(Value::as_str)) {
                if f.versions.get(&file).is_some_and(|previous| previous != hash) {
                    f.epoch += 1;
                    f.checked.clear();
                    f.http_check_samples.clear();
                    f.current_visual_artifact_ids.clear();
                    f.visual_check_result=Value::Null;
                }
                f.versions.insert(file.clone(), hash.to_owned());
                if matches!(name, "edit_file" | "replace_range" | "write_file") && result["changed"] == true {
                    f.epoch += 1;
                    f.checked.clear();
                    f.http_check_samples.clear();
                    f.rounds_without_change = 0;
                    f.reviewed_without_change = 0;
                    f.writes.insert(file, json!({"code_hash": hash, "changes": result["changes"], "material": result["notebook_material"]}));
                    f.current_visual_artifact_ids.clear();
                    f.visual_check_result=Value::Null;
                }
            }
        }
        if crate::project_process::is_check(name) {
            let check = if name == "run_program" {
                result["check_key"].as_str().map(str::to_owned).unwrap_or_else(||crate::program_execution::check_key(args))
            } else {
                result["check_key"].as_str().map(str::to_owned).unwrap_or_else(|| crate::project_process::check_key(name, args))
            };
            if f.order.checks.contains(&check) {
                let service = result["background"] == true && result["operation"] == "script";
                let succeeded = if name=="http_probe" {
                    result["check_passed"]==true && observed_http_sample_id.as_ref().is_some_and(|sample_id|self.http_observation_samples.contains_key(sample_id))
                } else if name=="run_program" {
                    result["outcome"]=="exited"&&result["process_exit_code"].as_i64()==Some(0)&&result["process_success"]==true
                } else if service {
                    result["running"] == true && result["ready"] == true
                } else {
                    result["status"].as_i64() == Some(0) && result["running"] != true
                };
                let pending = !failed && result["running"] == true && result["verification_inputs_changed"] != true && !succeeded;
                if !failed && succeeded && result["verification_inputs_changed"] != true {
                    f.checked.insert(check.clone(), f.epoch);
                    if name=="http_probe" {
                        if let Some(sample_id)=observed_http_sample_id.as_ref() {f.http_check_samples.insert(check.clone(),sample_id.clone());}
                    } else {
                        f.http_check_samples.remove(&check);
                    }
                    f.check_errors.remove(&check);
                    f.rounds_without_change = 0;
                    f.reviewed_without_change = 0;
                } else if !pending {
                    f.checked.remove(&check);
                    f.http_check_samples.remove(&check);
                    f.check_errors.insert(check, json!({
                        "exit_code": if name=="run_program" {result["process_exit_code"].clone()} else {result["status"].clone()},
                        "source_epoch": f.epoch,
                        "inputs_changed": result["verification_inputs_changed"] == true,
                        "error": result["error"].as_str().or_else(||result["error_message"].as_str()).map(|s| limited(s, 2000)),
                        "reachable":result["reachable"],
                        "http_status":result["http_status"],
                        "error_kind":result["error_kind"],
                        "error_message":result["error_message"],
                        "stderr": result["stderr"].as_str().map(|s| limited(s, 8000)),
                        "stdout": result["stdout"].as_str().map(|s| limited(s, 2000))
                    }));
                }
            }
        }
        let outcome = result.get("error").or_else(|| result.get("stderr").filter(|value| value.as_str().is_some_and(|s| !s.trim().is_empty())))
            .or_else(|| result.get("stdout")).map(|value| value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string())).unwrap_or_default();
        f.operations.push(json!({
            "tool": name,
            "page":page,
            "path": file,
            "program":if name=="run_program" {result.get("program")} else {None},
            "args":if name=="run_program" {result.get("args")} else {None},
            "command": Value::Null,
            "failed": failed,
            "exit_code": if name=="run_program" {result["process_exit_code"].clone()} else {result["status"].clone()},
            "process_success":result["process_success"],
            "command_result": Value::Null,
            "command_result_reports_failure":Value::Null,
            "changed": result["changed"],
            "changes": result["changes"],
            "material": result["notebook_material"],
            "process_id": result["process_id"],
            "project_path": result["project_path"],
            "script": result["script"],
            "process_observation":result["process_observation"],
            "host_runtime":result["host_runtime"],
            "ready": result["ready"],
            "check_key": result["check_key"],
            "http_probe_result":if name=="http_probe" {result.clone()} else {Value::Null},
            "matched": result["matched"],
            "expect_text":result["expect_text"],
            "operation_status":result["status"],
            "url":if name=="http_probe" {args.get("url")} else {None},
            "sampled_at":result["sampled_at"],
            "reuse_window_ms":result["reuse_window_ms"],
            "reachable":result["reachable"],
            "http_status":result["http_status"],
            "error_kind":result["error_kind"],
            "error_message":result["error_message"],
            "redirect_target":result["redirect_target"],
            "body_summary":result["body_summary"],
            "check_passed":result["check_passed"],
            "error_code":result["error_code"],
            "browser_stage":result["stage"],
            "display_mode":result["display_mode"],
            "uploaded_file":result["file"],
            "uploaded_path":result["uploaded"],
            "upload_attempt_id":result["upload_attempt_id"],
            "upload_input":result["input"],
            "document_loaded":result["document_loaded"],
            "browser_errors":result["errors"],
            "font_and_failed_resources":result["font_and_failed_resources"],
            "page_ready": result["page_ready"],
            "listener_owned": result["listener_owned"],
            "outcome": limited(&outcome, 1200)
        }));
        if f.operations.len() > 16 { f.operations.remove(0); }
    }

    pub fn update_versions(&mut self, versions: &BTreeMap<String, String>) {
        if self.frame().is_some_and(|frame| versions != &frame.versions) {
            self.http_probe_generation=self.http_probe_generation.saturating_add(1);
        }
        let Some(f) = self.frames.get_mut(&self.current) else { return; };
        if f.status == WorkStatus::Done { return; }
        if versions != &f.versions {
            f.epoch += 1;
            f.checked.clear();
            f.http_check_samples.clear();
            f.current_visual_artifact_ids.clear();
            f.visual_check_result=Value::Null;
            f.versions = versions.clone();
        }
    }

    pub fn input_versions(&mut self, pages: &[Value]) {
        let mut versions = self.versions();
        for page in pages {
            if let (Some(file), Some(hash)) = (page["path"].as_str(), page["code_hash"].as_str()) {
                versions.insert(path(file), hash.to_owned());
            }
        }
        self.update_versions(&versions);
    }

    pub fn return_work(&mut self, args: &Value) -> Result<Value> {
        self.refresh_http_check_validity();
        ensure!(args.to_string().len() <= 32_000, "work return is too large; return conclusions and material IDs");
        let summary = args["summary"].as_str().unwrap_or("").trim();
        ensure!(!summary.is_empty(), "yield_work requires actual findings/outcome or a concrete blocker");
        ensure!(args.get("limitations").is_none_or(|value|value.as_array().is_some_and(|items|items.len()<=16&&items.iter().all(Value::is_string))),
            "yield_work.limitations must contain at most 16 short strings");
        let frame_id=self.current.clone();
        let f = self.frames.get(&frame_id).cloned().ok_or_else(|| anyhow::anyhow!("no active work"))?;
        ensure!(f.status!=WorkStatus::Done,"this task return is already sealed; schedule a new task or revisit its node");
        // A valid TaskReturn seals this invocation even when its finding is
        // negative, blocked, upstream-dependent, or asks the Organizer to split.
        let done = true;
        if let Some(frame)=self.frames.get_mut(&frame_id) {
            frame.status=WorkStatus::Done;
            frame.returned_at=Some(now_ms());
            // The Worker reports its conclusion. Host observations are kept
            // alongside it; the host does not infer whether the goal passed.
            frame.expectation_met=None;
        }
        let mut exported_data=args.get("exported_data").cloned().unwrap_or(Value::Null);
        Self::attach_http_observations(self.frame_http_observations(&f),&mut exported_data);
        if !f.project_observation.is_null() {
            if !exported_data.is_object(){exported_data=json!({"worker_data":exported_data});}
            let observation=f.project_observation.clone();
            exported_data["project_observation"]=observation.clone();
            exported_data["project_path"]=observation.pointer("/scope/project_path").cloned().unwrap_or(Value::Null);
            exported_data["script"]=observation.pointer("/scope/script").cloned().unwrap_or(Value::Null);
            exported_data["ready_url"]=observation.pointer("/scope/ready_url").cloned().unwrap_or(Value::Null);
            exported_data["ready_port"]=observation.pointer("/scope/ready_port").cloned().unwrap_or(Value::Null);
            exported_data["process_id"]=observation.pointer("/processes/0/process_id").cloned().unwrap_or(Value::Null);
        }
        Self::attach_active_browser_upload(&self.active_browser_uploads,&f,&mut exported_data);
        let output = json!({
            "id": f.order.id,
            "node_id": f.order.node_id,
            "revision": f.order.revision,
            "plan_revision": f.order.plan_revision,
            "goal": f.order.goal,
            "done": done,
            "execution_status": "done",
            // These are Worker-reported business conclusions. Preserve an
            // omitted value as unknown; invocation completion is represented
            // only by done/execution_status above.
            "outcome": args.get("outcome").cloned().unwrap_or(Value::Null),
            "summary": args["summary"],
            // Keep the submitted return intact. Context projections and host
            // observations must never replace what the Worker actually said.
            "worker_return": args,
            "blocked": args.get("blocked").cloned().unwrap_or(Value::Null),
            "need_split": args.get("need_split").cloned().unwrap_or(Value::Null),
            "upstream_problem": args.get("upstream_problem").cloned().unwrap_or(Value::Null),
            "suggested_children": args["suggested_children"],
            "limitations": args.get("limitations").cloned().unwrap_or(Value::Null),
            "findings": args["findings"],
            "material_ids": args["material_ids"],
            "finding_ids": args["finding_ids"],
            "exported_data": exported_data,
            "project_observation":f.project_observation,
            "modified_files": f.writes,
            "checks": f.checked.keys().collect::<Vec<_>>(),
            "versions": f.versions,
            "operations": f.operations,
            "visual_artifact_ids":f.visual_artifact_ids,"visual_check_result":f.visual_check_result,
            "resource_availability": exported_data.get("browser_upload_availability")
        });
        self.frames.get_mut(&frame_id).unwrap().output = Some(output.clone());
        self.handoff = Some(output.clone());
        Ok(output)
    }

    pub fn visual_response_received(&mut self,manifest:&Value) {
        if manifest["status"]=="no_images" {return;}
        if let Some(frame)=self.frames.get_mut(&self.current) {
            if matches!(manifest["status"].as_str(),Some("direct"|"fallback")) {frame.visual_requests.push(manifest.clone());}
            if frame.visual_requests.len()>8 {frame.visual_requests.remove(0);}
            for id in manifest["selected_artifact_ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if !frame.seen_visual_artifact_ids.iter().any(|old|old==id) {frame.seen_visual_artifact_ids.push(id.to_owned());}
            }
        }
    }

    pub fn request_handoff(&mut self, reason: &str) {
        if self.handoff.is_none() {
            self.handoff = Some(json!({
                "done": self.done(),
                "reason": reason,
                "current_work": self.current,
                "current_node": self.node(),
                "revision": self.revision(),
            }));
        }
    }

    /// Keep the latest reports on the node. They describe intent and known
    /// conditions; only a Worker return seals the invocation.
    pub fn record_progress(&mut self, args: &Value, turn: usize, step: usize) {
        let Some(frame) = self.frames.get_mut(&self.current) else { return; };
        if frame.status == WorkStatus::Done { return; }
        frame.progress.push(json!({"turn":turn,"step":step,"at":now_ms(),
            "purpose":organizer_compact(&args["purpose"],0),"known_conditions":organizer_compact(&args["known_conditions"],0),
            "next_action":organizer_compact(&args["next_action"],0)}));
        if frame.progress.len() > 6 { frame.progress.remove(0); }
    }

    pub fn attach_return_data(&mut self, findings: &Value, materials: &[Value]) {
        let Some(frame) = self.frames.get_mut(&self.current) else { return; };
        let Some(output) = frame.output.as_mut() else { return; };
        let mut returned = output["findings"].as_array().cloned().unwrap_or_default();
        for fact in findings["findings"].as_array().into_iter().flatten() {
            if let Some(old) = returned.iter_mut().find(|old| old["id"].is_string() && old["id"] == fact["id"]) {
                *old = fact.clone();
            } else {
                returned.push(fact.clone());
            }
        }
        returned.truncate(24);
        output["findings"] = json!(returned);
        let mut ids = output["material_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64).take(16).collect::<Vec<_>>();
        for id in materials.iter().filter_map(|page| page["id"].as_i64()) {
            if !ids.contains(&id) { ids.push(id); }
        }
        for write in frame.writes.values() {
            if let Some(id) = write.pointer("/material/id").and_then(Value::as_i64) {
                if !ids.contains(&id) { ids.push(id); }
            }
        }
        ids.truncate(16);
        output["material_ids"] = json!(ids);
        let mut keys = output["finding_ids"].as_array().into_iter().flatten().filter_map(Value::as_str).take(16).map(str::to_owned).collect::<Vec<_>>();
        for fact in output["findings"].as_array().into_iter().flatten() {
            if let Some(id) = fact["id"].as_str() {
                if !keys.iter().any(|key| key == id) { keys.push(id.to_owned()); }
            }
        }
        keys.truncate(16);
        output["finding_ids"] = json!(keys);
        if !self.finished { self.handoff = Some(output.clone()); }
    }

    pub fn worker_input(&self, human_request: &str) -> Value {
        let upstream = self.order()
            .and_then(|o| self.resolve_dependency_deliveries(o).ok())
            .unwrap_or_default();
        json!({
            "human_request": human_request,
            "request_id":self.request_started_turn,
            "goal_boundary": self.goal_boundary,
            "current_work": self.order(),
            "upstream_outputs": upstream,
            "http_observations":self.worker_http_observations(),
            "done": self.done(),
            "organizer_handoff": self.frame().map(|f| &f.organizer_guidance),
            "verification_due": self.verification_due(),
            "actual_operations": self.frame().map(|f| &f.operations),
            "check_failures": self.frame().map(|f| &f.check_errors),
            "source_epoch": self.frame().map(|f| f.epoch),
            "outstanding_checks": self.frame().map(|frame| frame.order.checks.iter().filter(|check| frame.status != WorkStatus::Done
                && (frame.checked.get(*check) != Some(&frame.epoch)
                    || check.starts_with("http-probe:") && !self.http_check_sample_is_current(frame, check))).collect::<Vec<_>>()),
            "return_contract": "Do the current work using its smallest necessary materials. Use yield_work to explicitly end this invocation with findings, a specific blocker, NeedSplit, or upstream_problem. The host records actual writes, checks, and observations but does not decide whether the user goal passed. Do not replan, switch nodes, reopen completed work or announce readiness in separate rounds."
        })
    }

    /// Task invocations in the order they actually ran, then work not started.
    fn execution_order(&self) -> Vec<&WorkFrame> {
        let mut seen = BTreeSet::new();
        let activated = self.activated_history.iter().filter(|id| seen.insert(id.as_str()))
            .filter_map(|id| self.frames.get(id)).collect::<Vec<_>>();
        let mut unrecorded = self.frames.values().filter(|frame| !seen.contains(frame.order.id.as_str())).collect::<Vec<_>>();
        unrecorded.sort_by_key(|frame| frame.sequence);
        // Legacy snapshots have results without activation records; they ran before.
        let (ran, waiting): (Vec<_>, Vec<_>) = unrecorded.into_iter()
            .partition(|frame| frame.status != WorkStatus::Ready || frame.order.id == self.current);
        ran.into_iter().chain(activated).chain(waiting).collect()
    }

    fn path_status(&self, frame: &WorkFrame) -> &'static str {
        if frame.invalidated_by_plan_revision.is_some() { "deprecated" }
        else if self.effectively_done(frame) { "done" }
        else if frame.order.id == self.current { "running" }
        else if self.queue.contains(&frame.order.id) { "queued" }
        else { "ready" }
    }

    /// Chronological HTTP records. Freshness is cache metadata only; every
    /// sample remains a dated observation and no sample supersedes another.
    fn http_observations(&self) -> Vec<Value> {
        let mut samples=self.http_observation_samples.values().collect::<Vec<_>>();
        samples.sort_by_key(|sample|(rfc3339_ms(&sample["sampled_at"]).unwrap_or(0),sample["sample_id"].as_str().unwrap_or("").to_owned()));
        samples.into_iter().map(|sample| {
            let (fresh,age_ms,invalidation_reason)=self.http_sample_freshness(sample);
            json!({"url":sample["url"],"http_status":sample["http_status"],"reachable":sample["reachable"],
                "error_kind":sample["error_kind"],"error_message":sample["error_message"],"sampled_at":sample["sampled_at"],
                "sample_id":sample["sample_id"],"producer_work_id":sample["producer_work_id"],"fresh_for_cache_reuse":fresh,
                "age_ms":age_ms,"cache_invalidation_reason":invalidation_reason,
                "text":format!("{}: {} at {}",sample["url"].as_str().unwrap_or("unknown URL"),http_outcome(sample),time_label(&sample["sampled_at"]))})
        }).collect()
    }

    /// Host-owned browser resource state, independent of which task produced it.
    fn current_browser_state(&self) -> Vec<Value> {
        let mut states = Vec::new();
        for (page_key, attempt_id) in &self.active_browser_uploads {
            let Some(producer) = self.frames.values().find(|frame| frame.browser_upload_receipt.as_ref().is_some_and(|receipt|
                &receipt.upload_attempt_id == attempt_id && browser_page_key(&receipt.page).as_deref() == Some(page_key.as_str()))) else { continue; };
            let receipt = producer.browser_upload_receipt.as_ref().unwrap();
            let value = serde_json::to_value(receipt).unwrap_or(Value::Null);
            let availability = browser_upload_status_key(&value).and_then(|key| self.browser_upload_availability.get(&key)).cloned()
                .unwrap_or(json!({"available":false,"reason":"host_validation_not_run"}));
            let checked_epoch = availability["page_epoch"].as_u64();
            let read = self.frames.values().filter_map(|frame| frame.browser_current_read.as_ref())
                .filter(|read| browser_page_key(&read["page"]).as_deref() == Some(page_key.as_str())
                    && read["page"]["page_epoch"].as_u64() >= receipt.page["page_epoch"].as_u64()
                    && checked_epoch.is_none_or(|epoch| read["page"]["page_epoch"].as_u64() == Some(epoch))
                    && (!receipt.load_attempt_required || read["document_loaded"]["upload_attempt_id"] == json!(attempt_id)))
                .max_by_key(|read| read["observed_at"].as_u64().unwrap_or(0));
            let loaded = read.map(|read| { let loaded = &read["document_loaded"];
                json!({"status":loaded["status"],"slides_detected":loaded["slides_detected"],"page_indicator":loaded["page_indicator"],
                    "error_indicators":loaded["error_indicators"],"upload_attempt_id":loaded["upload_attempt_id"]}) });
            states.push(json!({"page":{"browser_session_id":receipt.page["browser_session_id"],"page_id":receipt.page["page_id"],"url":receipt.page["url"]},
                "document_path":receipt.path,"file_name":receipt.file["name"],"upload_attempt_id":attempt_id,
                "uploaded_by":{"work_id":producer.order.id,"node_id":producer.order.node_id,"revision":producer.order.revision,
                    "producer_deprecated":producer.invalidated_by_plan_revision.is_some()},
                "availability":{"available":availability["available"],"reason":availability["reason"],"checked_page_epoch":availability["page_epoch"]},
                "document_loaded":loaded,"read_by_work_id":read.map(|read| read["work_id"].clone()),
                "read_at":read.map(|read| read["observed_at"].clone()),
                "source":"host browser tool results; a deprecated producer does not mean the page lost its document"}));
        }
        states
    }

    /// Shared raw host observations and current resource availability.
    pub fn current_facts(&self) -> Value {
        json!({"http_observations":self.http_observations(),"browser":self.current_browser_state()})
    }

    /// Time of the newest host-recorded fact on a node. Observer advice written
    /// before it describes an older state of the same instance.
    pub fn latest_fact_at(&self, work_id: &str) -> u64 {
        let Some(frame) = self.frames.get(work_id) else { return 0; };
        let http = frame.http_observation_ids.iter().filter_map(|id| self.http_observation_samples.get(id))
            .filter_map(|sample| rfc3339_ms(&sample["sampled_at"]));
        let progress = frame.progress.iter().filter_map(|report| report["at"].as_u64());
        [frame.returned_at, frame.project_observation["sampled_at"].as_u64(),
            frame.browser_current_read.as_ref().and_then(|read| read["observed_at"].as_u64())]
            .into_iter().flatten().chain(http).chain(progress).max().unwrap_or(0)
    }

    /// Actual node state and delivered values for the Observer, selected by
    /// business meaning rather than by the first keys of each object.
    pub fn observer_facts(&self, work_id: &str) -> Value {
        let Some(frame) = self.frames.get(work_id) else { return Value::Null; };
        let output = frame.output.as_ref();
        let http = self.frame_http_observations(frame).into_iter().map(|sample| json!({"url":sample["url"],"http_status":sample["http_status"],
            "reachable":sample["reachable"],"error_kind":sample["error_kind"],"error_message":sample["error_message"],"sampled_at":sample["sampled_at"],
            "fresh_for_cache_reuse":sample["fresh_for_cache_reuse"],"age_ms":sample["age_ms"],"cache_invalidation_reason":sample["cache_invalidation_reason"],"producer_work_id":sample["producer_work_id"]}))
            .collect::<Vec<_>>();
        let processes = frame.project_observation["processes"].as_array().into_iter().flatten().map(|process| json!({
            "process_id":process["process_id"],"script":process["script"],"running":process["running"],"ready":process["ready"],
            "ready_url":process["ready_url"],"owner_work_id":process["owner_work_id"].as_str().unwrap_or(work_id),"started_at":process["started_at"]}))
            .collect::<Vec<_>>();
        let upload = frame.browser_upload_receipt.as_ref().map(|receipt| {
            let availability = serde_json::to_value(receipt).ok().as_ref().and_then(browser_upload_status_key)
                .and_then(|key| self.browser_upload_availability.get(&key)).cloned().unwrap_or(Value::Null);
            json!({"upload_attempt_id":receipt.upload_attempt_id,"path":receipt.path,"file_name":receipt.file["name"],
                "change_event_received":receipt.change_event_received,"page":receipt.page,"availability":availability})
        });
        let read = frame.browser_current_read.as_ref().map(|read| json!({"matched":read["matched"],"observed_at":read["observed_at"],
            "page":read["page"],"document_loaded":read["document_loaded"]}));
        let checks = frame.order.checks.iter().map(|check| json!({"check":check,
            "passed":frame.checked.get(check) == Some(&frame.epoch),"failure":frame.check_errors.get(check)})).collect::<Vec<_>>();
        let exported = output.map(|output| organizer_compact(&output["exported_data"], 0)).unwrap_or(Value::Null);
        json!({"work_id":work_id,"node_id":frame.order.node_id,"revision":frame.order.revision,"status":self.path_status(frame),
            "started_at":frame.started_at,"returned_at":frame.returned_at,
            "process_observation_sampled_at":frame.project_observation["sampled_at"],
            "result":output.map(|output| json!({"summary":output["summary"],"outcome":output["outcome"],"limitations":output["limitations"]})),
            "exported_data":exported,"reports":frame.progress,"checks":checks,"http_observations":http,"processes":processes,
            "browser":{"page":frame.browser_page,"upload":upload,"current_read":read},
            "operations":frame.operations.iter().rev().take(8).rev().map(|operation| organizer_compact(operation, 0)).collect::<Vec<_>>(),
            "superseded":frame.superseded,"latest_fact_at":self.latest_fact_at(work_id)})
    }

    fn execution_narrative(&self, ordered: &[&WorkFrame], omitted: usize, http: &[Value], browser: &[Value]) -> (String,usize) {
        let mut lines = vec!["Execution path in actual invocation order. A done node records a Worker return; it does not assert that the user goal passed.".to_owned()];
        if omitted > 0 { lines.push(format!("Earlier range 1-{omitted} omitted; use the flow directory page or read a sealed result by work_id.")); }
        for (index, frame) in ordered.iter().enumerate().skip(omitted) {
            let status = self.path_status(frame);
            let output = frame.output.as_ref();
            let summary = output.and_then(|output| output["summary"].as_str());
            lines.push(format!("Step {} - {} r{} [{status}]: {}", index + 1, frame.order.node_id, frame.order.revision, limited(&frame.order.goal, 180)));
            if let Some(output)=output {
                if frame.order.id==self.current {
                    lines.push("  Current Worker return is provided once in current_result; use read_task_result for omitted fields.".to_owned());
                } else {
                    lines.push(format!("  Worker return{} ({}): {}",frame.returned_at.map(|at|format!(" at {}",ms_label(at))).unwrap_or_default(),
                        output["outcome"].as_str().unwrap_or("reported"),limited(summary.unwrap_or("no summary"),420)));
                    for limitation in output["limitations"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                        lines.push(format!("  Worker-reported limitation: {}",limited(limitation,240)));
                    }
                }
                for operation in frame.operations.iter().rev().take(6).rev() {
                    let tool=operation["tool"].as_str().unwrap_or("host_tool");
                    let at=time_label(&operation["sampled_at"]);
                    let fact=operation["http_status"].as_u64().map(|status|format!("HTTP {status}"))
                        .or_else(||operation["process_id"].as_str().map(|id|format!("process_id {id}, running={}, ready={}",
                            operation["process_observation"]["processes"][0]["running"],operation["ready"])))
                        .or_else(||operation["browser_stage"].as_str().map(str::to_owned))
                        .or_else(||operation["outcome"].as_str().map(|text|limited(text,180)));
                    if let Some(fact)=fact {lines.push(format!("  Host action {tool} at {at}: {fact}."));}
                }
            } else if status=="running" {
                let report=frame.progress.last().and_then(|item|item["next_action"].as_str().or_else(||item["purpose"].as_str()))
                    .map(|text|format!(" Latest Worker report (intent): {}.",limited(text,220))).unwrap_or_default();
                lines.push(format!("  No Worker return recorded.{report}"));
            } else if status=="deprecated" {
                let reason=frame.superseded.as_ref().and_then(|record|record["reason"].as_str()).unwrap_or("superseded by a later plan revision");
                lines.push(format!("  Historical record, not a valid dependency input: {}.",limited(reason,180)));
            } else { lines.push("  Not started.".to_owned()); }
        }
        lines.push("HTTP observations, in recorded time order:".to_owned());
        if http.is_empty() { lines.push("- none recorded".to_owned()); }
        lines.extend(http.iter().filter_map(|fact|fact["text"].as_str()).map(|text|format!("- {text}")));
        for frame in ordered.iter().skip(omitted) {
            let observation=&frame.project_observation;
            for process in observation["processes"].as_array().into_iter().flatten() {
                lines.push(format!("Host process observation at {}: process_id={}, running={}, ready={}, ready_url={}",
                    observation["sampled_at"].as_u64().map(ms_label).unwrap_or_else(||"unknown time".to_owned()),
                    process["process_id"].as_str().unwrap_or("unknown"),process["running"],process["ready"],process["ready_url"].as_str().unwrap_or("unknown")));
            }
        }
        for state in browser {
            let loaded = &state["document_loaded"];
            lines.push(format!("Host browser observation at {}: page={}, document={}, load_status={}, slides={}, upload_attempt={}, session_available={}",
                state["read_at"].as_u64().map(ms_label).unwrap_or_else(||"unknown time".to_owned()),
                state["page"]["url"].as_str().unwrap_or("unknown"),state["document_path"].as_str().unwrap_or("unknown"),
                loaded["status"].as_str().unwrap_or("not confirmed"),loaded["slides_detected"],state["upload_attempt_id"],state["availability"]["available"]));
        }
        let full=lines.join("\n");
        let omitted_chars=full.chars().count().saturating_sub(16_000);
        (limited(&full, 16_000),omitted_chars)
    }

    pub fn organizer_input(&self) -> Value {
        let mut observations=self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none()&&!frame.project_observation.is_null())
            .map(|frame|json!({"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,
                "status":frame.status,"observation":organizer_compact(&frame.project_observation,0)})).collect::<Vec<_>>();
        observations.sort_by_key(|item|std::cmp::Reverse(item["observation"]["sampled_at"].as_u64().unwrap_or(0)));
        let ordered=self.execution_order();
        let omitted_steps=ordered.len().saturating_sub(EXECUTION_PATH_STEPS);
        let execution_path=ordered.iter().enumerate().skip(omitted_steps).map(|(index,frame)| {
            let output=frame.output.as_ref();
            let status=self.path_status(frame);
            let is_current_work=frame.order.id==self.current;
            let current=is_current_work&&output.is_some();
            let goal=limited(&frame.order.goal,420);
            let mut omissions=Vec::new();let mut omitted_total=0;
            if let Some(output)=output.filter(|_|!current) {organizer_omissions(output,"result",0,&mut omissions,&mut omitted_total);}
            json!({"step":index+1,"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,
                "status":status,"goal":goal,"goal_omitted_chars":frame.order.goal.chars().count().saturating_sub(420),
                "outcome":if current {Value::Null}else{output.map(|value|value["outcome"].clone()).unwrap_or(Value::Null)},
                "summary":if current {Value::Null}else{output.map(|value|json!(limited(value["summary"].as_str().unwrap_or(""),420))).unwrap_or(Value::Null)},
                "summary_omitted_chars":if current {Value::Null}else{output.and_then(|value|value["summary"].as_str()).map(|value|json!(value.chars().count().saturating_sub(420))).unwrap_or(Value::Null)},
                "limitations":if current {Value::Null}else{output.map(|value|organizer_compact(&value["limitations"],0)).unwrap_or(Value::Null)},
                "returned_at":frame.returned_at,
                "result_reference":if current {json!({"method":"current_result"})}else if is_current_work {json!({"method":"current_work","detail":"Worker has not returned"})}
                    else {json!({"method":"read_task_result","work_id":frame.order.id})},
                "context_omissions":if omitted_total>0 {json!({"items":omissions,"omitted_item_count":omitted_total.saturating_sub(24),
                    "reference":{"method":"read_task_result","work_id":frame.order.id,"fields":"request an omitted field explicitly"}})}else{Value::Null},
                "valid_input":status=="done",
                "deprecation":frame.invalidated_by_plan_revision.map(|plan_revision|json!({"plan_revision":plan_revision,
                    "reason":frame.superseded.as_ref().map(|record|record["reason"].clone())
                        .or_else(||self.rewind_records.iter().find(|rewind|rewind.invalidated_tasks.contains(&frame.order.id)||rewind.source_work_id==frame.order.id).map(|rewind|json!(limited(&rewind.reason,240))))})),
                "material_ids":if current {Value::Null}else{output.map(|value|value["material_ids"].clone()).unwrap_or(Value::Null)},
                "finding_ids":if current {Value::Null}else{output.map(|value|value["finding_ids"].clone()).unwrap_or(Value::Null)}})
        }).collect::<Vec<_>>();
        let current_browser_state=self.current_browser_state();
        let http_observations=self.http_observations();
        let (execution_narrative,execution_narrative_omitted_chars)=self.execution_narrative(&ordered,omitted_steps,&http_observations,&current_browser_state);
        let mut delivery_catalog=self.frames.values().filter(|frame|self.effectively_done(frame)&&frame.invalidated_by_plan_revision.is_none())
            .filter_map(|frame|frame.output.as_ref().map(|output|{
                let exported=self.filtered_export_data(frame,&output["exported_data"]);
                let exported_fields=exported.as_object().map(|fields|fields.keys().filter(|key|key.as_str()!="browser_upload_availability").cloned().collect::<Vec<_>>()).unwrap_or_default();
                let output_fields=output.as_object().map(|fields|fields.keys().filter(|key|!matches!(key.as_str(),"operations"|"versions"|"modified_files"|"exported_data")).cloned().collect::<Vec<_>>()).unwrap_or_default();
                json!({"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,
                    "status":"done","exported_fields":exported_fields,"output_fields":output_fields})
            })).collect::<Vec<_>>();
        delivery_catalog.sort_by(|a,b|a["work_id"].as_str().cmp(&b["work_id"].as_str()));
        let mut upload_frames=self.frames.values().filter(|frame|frame.browser_upload_receipt.is_some()).collect::<Vec<_>>();
        // Valid producers first so receipt[0] is the latest usable delivery.
        upload_frames.sort_by_key(|frame|(frame.invalidated_by_plan_revision.is_some(),frame.sequence));
        let upload_availability=upload_frames.into_iter()
            .filter_map(|frame|frame.browser_upload_receipt.as_ref().map(|receipt|{
                let value=serde_json::to_value(receipt).unwrap_or(Value::Null);
                let status=browser_upload_status_key(&value).and_then(|key|self.browser_upload_availability.get(&key)).cloned()
                    .unwrap_or(json!({"available":false,"reason":"host_validation_not_run"}));
                let current=self.browser_receipt_is_active(&value);
                json!({"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,
                    "upload_attempt_id":receipt.upload_attempt_id,"availability":{"available":status["available"],"reason":status["reason"]},
                    "scope":if current {"current page"} else {"this receipt only; see current_browser_state for the page now"},
                    "historical":!current,"producer_deprecated":frame.invalidated_by_plan_revision.is_some()})
            })).collect::<Vec<_>>();
        let available_revisit_targets=self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none())
            .map(|frame|frame.order.node_id.clone()).filter(|id|!id.is_empty()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        let current_frame=self.frame();
        let current_output=current_frame.and_then(|frame|frame.output.as_ref());
        let current_result=current_frame.zip(current_output).map(|(frame,output)|self.compact_current_result(frame,output));
        let handoff=self.handoff.as_ref().filter(|handoff|current_output.is_none_or(|output|!handoff_matches_current_output(handoff,output)))
            .map(organizer_handoff);
        let current_work=self.order().map(|order|json!({"id":order.id,"node_id":order.node_id,"revision":order.revision,
            "goal":order.goal,"return_when":order.done_when,"completion":order.completion,"constraints":order.constraints}));
        json!({
            "schema_version":WORK_SCHEDULER_SCHEMA_VERSION,
            "current_work": current_work,
            "request_id":self.request_started_turn,
            "current_node": self.node(),
            "current_revision": self.revision(),
            "plan_revision": self.plan_revision,
            "handoff": handoff,
            "current_result": current_result,
            "execution_narrative": execution_narrative,
            "execution_narrative_omitted_chars":execution_narrative_omitted_chars,
            "execution_narrative_reference":{"method":"read_flow_page","then":"read_task_result(work_id, fields) for full node details"},
            "execution_path": execution_path,
            "flow_directory":self.flow_directory_page(0,50),
            "current_facts":{"http_observations":http_observations,"browser":current_browser_state},
            "available_dependency_deliveries":delivery_catalog,
            "available_revisit_targets":available_revisit_targets,
            "browser_upload_availability":upload_availability,
            "recent_rewinds":self.rewind_records.iter().rev().take(4).map(|rewind|json!({"source_node":rewind.source_node,
                "target_node":rewind.target_node,"reason":limited(&rewind.reason,240),"invalidated_tasks":rewind.invalidated_tasks})).collect::<Vec<_>>(),
            "project_process_observations":observations.into_iter().take(6).collect::<Vec<_>>(),
            "legacy_check_contracts":self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none()&&frame.status!=WorkStatus::Done
                &&matches!(frame.order.completion,Completion::Check|Completion::WriteCheck))
                .filter_map(|frame|{let checks=frame.order.checks.iter().filter(|check|!supported_check_key(check)).cloned().collect::<Vec<_>>();
                    (!checks.is_empty()).then(||json!({"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,"checks":checks}))}).collect::<Vec<_>>()
        })
    }

    pub fn available_select_task_ids(&self) -> Vec<String> {
        let mut ids=self.queue.iter().filter(|id|self.frames.get(*id).is_some_and(|frame|frame.invalidated_by_plan_revision.is_none()&&!self.effectively_done(frame)))
            .cloned().collect::<Vec<_>>();
        for frame in self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none()&&!self.effectively_done(frame)) {
            if !ids.contains(&frame.order.id) {ids.push(frame.order.id.clone());}
        }
        ids
    }

    /// Compact, chronological flow listing for Organizer pagination. Full
    /// return payloads remain available through read_task_result.
    pub fn flow_directory_page(&self, offset:usize, limit:usize) -> Value {
        let ordered=self.execution_order();
        let total=ordered.len();
        let start=offset.min(total);
        let page_size=limit.clamp(1,50);
        let end=start.saturating_add(page_size).min(total);
        let entries=ordered.iter().enumerate().skip(start).take(end-start).map(|(index,frame)| {
            let output=frame.output.as_ref();
            let is_current_work=frame.order.id==self.current;
            let current=is_current_work&&output.is_some();
            let goal=limited(&frame.order.goal,360);
            let summary=output.and_then(|value|value["summary"].as_str());
            json!({"step":index+1,"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,
                "status":self.path_status(frame),"goal":goal,"goal_omitted_chars":frame.order.goal.chars().count().saturating_sub(360),
                "outcome":if current {Value::Null}else{output.map(|value|value["outcome"].clone()).unwrap_or(Value::Null)},
                "summary":if current {Value::Null}else{summary.map(|value|json!(limited(value,240))).unwrap_or(Value::Null)},
                "summary_omitted_chars":if current {Value::Null}else{summary.map(|value|json!(value.chars().count().saturating_sub(240))).unwrap_or(Value::Null)},
                "limitation_count":if current {Value::Null}else{output.and_then(|value|value["limitations"].as_array().map(|items|json!(items.len()))).unwrap_or(Value::Null)},
                "material_ids":if current {Value::Null}else{output.map(|value|value["material_ids"].clone()).unwrap_or(Value::Null)},
                "finding_ids":if current {Value::Null}else{output.map(|value|value["finding_ids"].clone()).unwrap_or(Value::Null)},
                "detail_reference":if current {json!({"method":"current_result"})}else if is_current_work {json!({"method":"current_work","detail":"Worker has not returned"})}
                    else {json!({"method":"read_task_result","work_id":frame.order.id})},
                "returned_at":frame.returned_at,"prior_actions":if current&&frame.status==WorkStatus::Running {
                    json!(frame.operations.iter().rev().take(8).rev().map(|operation|organizer_compact(operation,0)).collect::<Vec<_>>())
                } else {Value::Null}})
        }).collect::<Vec<_>>();
        json!({"entries":entries,"offset":start,"returned":end-start,"total":total,
            "next_cursor":(end<total).then_some(end),"omitted_range":(start>0).then(||json!({"from_step":1,"through_step":start})),
            "reference":"read_flow_page(cursor=next_cursor) to continue; read_task_result(work_id, fields) for an exact sealed return"})
    }

    pub fn read_flow_page(&self,args:&Value)->Result<Value> {
        let cursor=args.get("cursor").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit=args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
        ensure!(limit<=50,"field_path=limit: read_flow_page allows at most 50 nodes per page");
        Ok(self.flow_directory_page(cursor,limit.max(1)))
    }

    pub fn organizer_input_with_freshness(&self, root:&std::path::Path) -> Value {
        let mut input=self.organizer_input();
        if let Some(items)=input["project_process_observations"].as_array_mut() {
            for item in items {
                let observation=item["observation"].clone();
                item["fresh_for_cache_reuse"]=json!(crate::project_process::observation_sample_is_current(root,&observation));
            }
        }
        input
    }

    /// Read an exact sealed result by work instance, including deprecated
    /// historical revisions, or the latest valid node revision. A historical
    /// read is never eligible to serve as a new task's dependency input.
    pub fn read_task_result(&self,args:&Value)->Result<Value> {
        let requested_work=args["work_id"].as_str().filter(|id|!id.trim().is_empty());
        let requested_node=args["node_id"].as_str().filter(|id|!id.trim().is_empty());
        ensure!(requested_work.is_some()||requested_node.is_some(),"read_task_result needs work_id or node_id");
        let frame=if let Some(work_id)=requested_work {
            self.frames.get(work_id).filter(|frame|self.effectively_done(frame))
        } else {
            self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none()&&self.effectively_done(frame)
                &&Some(frame.order.node_id.as_str())==requested_node)
                .max_by_key(|frame|(frame.order.revision,frame.sequence))
        }.ok_or_else(||anyhow::anyhow!("requested task result is not an active sealed delivery"))?;
        let output=frame.output.as_ref().ok_or_else(||anyhow::anyhow!("sealed task has no saved output"))?;
        let requested_fields=args["fields"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>();
        let default_fields=["summary","outcome","limitations","findings","material_ids","finding_ids","exported_data","checks",
            "visual_artifact_ids","visual_check_result","upstream_problem","suggested_children"];
        let fields=if requested_fields.is_empty(){default_fields.as_slice()}else{requested_fields.as_slice()};
        let exported=&output["exported_data"];
        let mut selected=json!({});
        for field in fields {
            ensure!(!matches!(*field,"operations"|"versions"|"modified_files"|"raw_output"),"field_path=fields: '{field}' is stored as a material; retrieve it from the notebook by ID");
            let value=output.get(field).or_else(||exported.get(field))
                .ok_or_else(||anyhow::anyhow!("field_path=fields: '{field}' is not present in this delivery"))?;
            selected[field]=value.clone();
        }
        let resource_availability=exported.get("browser_upload_receipt").map(|receipt|{
            browser_upload_status_key(receipt).and_then(|key|self.browser_upload_availability.get(&key)).cloned()
                .unwrap_or(json!({"available":false,"reason":"host_validation_not_run"}))
        }).unwrap_or(Value::Null);
        let historical=frame.invalidated_by_plan_revision.is_some();
        Ok(json!({"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,
            "status":"done","historical":historical,"dependency_eligible":!historical,
            "selected_fields":selected,"resource_availability":resource_availability}))
    }

    pub fn reusable_project_observation(&self, root:&std::path::Path, args:&Value) -> Option<Value> {
        if !crate::project_process::can_reuse_process_observation(args) { return None; }
        self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none()&&!frame.project_observation.is_null()
            &&crate::project_process::observation_is_fresh(root,args,&frame.project_observation))
            .max_by_key(|frame|frame.project_observation["sampled_at"].as_u64().unwrap_or(0))
            .map(|frame|frame.project_observation.clone())
    }

    pub fn revisit(&mut self, target_node: &str, target_revision: Option<usize>, reason: &str, repair_goal: Option<&str>, replacement_checks:Option<&[String]>, can_write: bool, can_check: bool) -> Result<String> {
        ensure!(!target_node.trim().is_empty(), "revisit requires a target node");

        let count = self.revisit_counts.entry(target_node.to_string()).or_insert(0);
        *count += 1;
        ensure!(*count <= 4, "exceeded maximum backtrack limit for node {target_node}; report blocked");

        let (target_work_id, target_order) = {
            let active_matching = self.frames.values()
                .filter(|f| f.invalidated_by_plan_revision.is_none())
                .filter(|f| f.order.node_id == target_node || f.order.id == target_node)
                .filter(|f| target_revision.map_or(true, |r| f.order.revision == r))
                .max_by_key(|f| (f.order.revision, f.sequence));
            let matching = match active_matching {
                Some(f) => f,
                None => {
                    if let Some(req_rev) = target_revision {
                        self.frames.values()
                            .filter(|f| (f.order.node_id == target_node || f.order.id == target_node) && f.order.revision == req_rev)
                            .max_by_key(|f| f.sequence)
                            .ok_or_else(|| anyhow::anyhow!("unknown backtrack target revision {req_rev} for node: {target_node}"))?
                    } else {
                        anyhow::bail!("cannot backtrack: node {target_node} has no active instance in current plan");
                    }
                }
            };
            (matching.order.id.clone(), matching.order.clone())
        };
        let canonical_target_node = target_order.node_id.clone();
        let has_legacy_checks=target_order.checks.iter().any(|check|!supported_check_key(check));
        ensure!(!has_legacy_checks||replacement_checks.is_some(),
            "field_path=replacement_checks: revisiting work with retired shell checks requires replacement_checks using supported identifiers");

        let target_act_idx = self.activated_history.iter().position(|id| id == &target_work_id);
        let target_q_idx = self.queue.iter().position(|id| id == &target_work_id);

        self.plan_revision += 1;

        // Old target instance is superseded by the new revision being created
        if let Some(old_target_frame) = self.frames.get_mut(&target_work_id) {
            old_target_frame.invalidated_by_plan_revision = Some(self.plan_revision);
        }

        let mut downstream_set = BTreeSet::new();
        if let Some(act_idx) = target_act_idx {
            for id in self.activated_history.iter().skip(act_idx + 1) {
                if id != &target_work_id {
                    downstream_set.insert(id.clone());
                }
            }
            for id in &self.queue {
                if id != &target_work_id {
                    downstream_set.insert(id.clone());
                }
            }
        } else if let Some(q_idx) = target_q_idx {
            for id in self.queue.iter().skip(q_idx + 1) {
                if id != &target_work_id {
                    downstream_set.insert(id.clone());
                }
            }
        }

        let mut added = true;
        while added {
            added = false;
            for (id, frame) in &self.frames {
                if id == &target_work_id || frame.invalidated_by_plan_revision.is_some() || downstream_set.contains(id) {
                    continue;
                }
                let depends_on_target = frame.order.upstream_ids.iter().any(|u| {
                    u == &target_work_id || u == &canonical_target_node || downstream_set.contains(u)
                        || self.frames.get(u).map_or(false, |uf| uf.order.node_id == canonical_target_node)
                });
                if depends_on_target {
                    downstream_set.insert(id.clone());
                    added = true;
                }
            }
        }

        let mut invalidated = Vec::new();
        let mut invalidated_nodes = BTreeSet::new();
        for id in downstream_set {
            if let Some(frame) = self.frames.get_mut(&id) {
                if frame.invalidated_by_plan_revision.is_none() {
                    frame.invalidated_by_plan_revision = Some(self.plan_revision);
                    invalidated.push(id.clone());
                    if frame.order.node_id != canonical_target_node {
                        invalidated_nodes.insert(frame.order.node_id.clone());
                    }
                }
            }
        }
        self.queue.retain(|id| id != &target_work_id && !invalidated.contains(id));

        let (source_id, source_node, source_rev) = if let Some(cur) = self.frame() {
            (cur.order.id.clone(), cur.order.node_id.clone(), cur.order.revision)
        } else {
            ("unknown".to_string(), "unknown".to_string(), 1)
        };

        let new_rev = self.node_revisions.get(&canonical_target_node).copied().unwrap_or(target_order.revision) + 1;
        self.node_revisions.insert(canonical_target_node.clone(), new_rev);

        let new_work_id = format!("{}_r{}", canonical_target_node, new_rev);
        let mut new_order = target_order.clone();
        new_order.id = new_work_id.clone();
        new_order.node_id = canonical_target_node.clone();
        new_order.revision = new_rev;
        new_order.plan_revision = self.plan_revision;
        if let Some(goal) = repair_goal {
            if !goal.trim().is_empty() {
                new_order.goal = goal.trim().to_string();
            }
        }

        if let Some(checks)=replacement_checks {
            ensure!(!checks.is_empty()&&checks.len()<=4,"field_path=replacement_checks: provide one to four supported check identifiers");
            new_order.checks=checks.iter().map(|check|command(check)).collect();
            ensure!(new_order.checks.iter().all(|check|!check.is_empty()&&check.chars().count()<=2000&&supported_check_key(check)),
                "field_path=replacement_checks: provide npm:, npm-start:, npm-install:, program:, or http-probe: identifiers");
        }

        let checking = !new_order.checks.is_empty();
        ensure!(new_order.edit_targets.is_empty() || can_write, "write work is unavailable under current permissions");
        ensure!(!checking || can_check, "checks are unavailable under current tool permissions");

        let sequence = self.frames.len() + 1;
        self.frames.insert(new_work_id.clone(), WorkFrame {
            order: new_order,
            sequence,
            status: WorkStatus::Running,
            started_at: Some(now_ms()),
            ..Default::default()
        });

        let rewind = RewindRecord {
            id: format!("rewind_{}_{}", self.plan_revision, self.rewind_records.len() + 1),
            source_node,
            source_work_id: source_id,
            source_revision: source_rev,
            target_node: canonical_target_node.clone(),
            target_work_id: new_work_id.clone(),
            target_revision: new_rev,
            reason: reason.to_string(),
            timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
            plan_revision: self.plan_revision,
            invalidated_tasks: invalidated,
            invalidated_node_ids: invalidated_nodes.into_iter().collect(),
        };
        self.rewind_records.push(rewind);

        self.current = new_work_id.clone();
        self.activated_history.push(new_work_id.clone());
        self.handoff = None;
        Ok(new_work_id)
    }

    /// Commit atomically and expose node invalidations to the Agent's tree transaction.
    pub fn apply(&mut self, decision: &Value, can_write: bool, can_check: bool) -> Result<DecisionChanges> {
        let call_id=decision["decision_id"].as_str().filter(|id|!id.trim().is_empty());
        let fingerprint=serde_json::to_string(decision)?;
        if let Some(call_id)=call_id {
            if let Some(applied)=self.applied_decisions.get(call_id) {
                ensure!(applied==&fingerprint,"decision_id '{call_id}' was replayed with different scheduling arguments");
                return Ok(DecisionChanges::default());
            }
        }
        let mut next = self.clone();
        next.apply_inner(decision, can_write, can_check)?;
        if let Some(call_id)=call_id {next.applied_decisions.insert(call_id.to_owned(),fingerprint);}
        let invalidated_work_ids = next.frames.iter().filter(|(id, frame)| frame.invalidated_by_plan_revision.is_some()
            && self.frames.get(*id).is_some_and(|old| old.invalidated_by_plan_revision.is_none()))
            .map(|(id, _)| id.clone()).collect::<Vec<_>>();
        let invalidated_node_ids = invalidated_work_ids.iter().map(|id| next.frames[id].order.node_id.clone())
            .filter(|node| !next.frames.values().any(|f| f.order.node_id == *node && f.invalidated_by_plan_revision.is_none()))
            .collect::<BTreeSet<_>>().into_iter().collect();
        *self = next;
        Ok(DecisionChanges { invalidated_work_ids, invalidated_node_ids })
    }

    fn apply_inner(&mut self, decision: &Value, can_write: bool, can_check: bool) -> Result<()> {
        match decision["action"].as_str().unwrap_or("") {
            "work" => {
                let mut orders: Vec<WorkOrder> = serde_json::from_value(decision["orders"].clone()).unwrap_or_default();
                if orders.is_empty() {
                    if !self.queue.is_empty() {
                        self.activate_next()?;
                    }
                    return Ok(());
                }
                for order in &mut orders {
                    if order.revision == 0 {
                        let rev = self.node_revisions.entry(order.node_id.clone()).or_insert(1);
                        order.revision = *rev;
                    }
                    if order.plan_revision == 0 {
                        order.plan_revision = self.plan_revision;
                    }
                }
                let is_split = self.handoff.as_ref().map_or(false, |h| {
                    h["need_split"] == true || h["outcome"] == "need_split" || h["outcome"] == "upstream_problem"
                }) || decision["preserve_current"] == true;
                if !is_split && !self.current.is_empty() && !self.done() {
                    // A returned producer is sealed (done) and never reaches this
                    // branch. An unreturned task cannot be an input of its successor.
                    let (current_id, current_node) = (self.current.clone(), self.node().to_owned());
                    if let Some(index) = orders.iter().position(|order| references_work(order, &current_id, &current_node)) {
                        anyhow::bail!("field_path=orders[{index}].dependency_inputs: TASK_NOT_RETURNED: work_id='{current_id}' node_id='{current_node}' is still running and has not returned a result; let it return first, or schedule without this input");
                    }
                    let replacements = orders.iter().map(|order| order.id.clone()).collect::<Vec<_>>();
                    let plan_revision = self.plan_revision;
                    if let Some(cur_frame) = self.frames.get_mut(&self.current) {
                        if cur_frame.invalidated_by_plan_revision.is_none() {
                            cur_frame.invalidated_by_plan_revision = Some(plan_revision);
                            cur_frame.superseded = Some(json!({"reason":limited(decision["reason"].as_str().unwrap_or("Organizer scheduled a different task"),600),
                                "replaced_by":replacements,"task_returned":false,"rounds":cur_frame.rounds,
                                "operations":cur_frame.operations.len(),"last_progress":cur_frame.progress.last(),"at":now_ms()}));
                        }
                    }
                    self.current.clear();
                }
                self.enqueue(orders, can_write, can_check)?;
                if self.current.is_empty() || self.done() {
                    self.activate_next()?;
                }
                Ok(())
            },
            "select" => {
                let target = decision["task_id"].as_str().or_else(|| decision["node_id"].as_str());
                self.select_task(target)?;
                Ok(())
            },
            "revisit" => {
                let target_node = decision["target_node_id"].as_str()
                    .or_else(|| decision["node_id"].as_str())
                    .ok_or_else(|| anyhow::anyhow!("revisit requires target_node_id"))?;
                let target_revision = decision.get("target_revision").and_then(Value::as_u64).map(|r| r as usize);
                let reason = decision["reason"].as_str().unwrap_or("Backtracking to repair upstream deliverable");
                let repair_goal = decision["repair_goal"].as_str();
                let replacement_checks=decision.get("replacement_checks").and_then(Value::as_array)
                    .map(|checks|checks.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>());
                self.revisit(target_node, target_revision, reason, repair_goal, replacement_checks.as_deref(), can_write, can_check)?;
                Ok(())
            },
            "continue" => {
                self.continue_decision(decision, can_write, can_check)
            },
            "finish" | "blocked" => {
                let blocked = decision["action"] == "blocked";
                self.finish(decision["summary"].as_str().unwrap_or(""), blocked)
            },
            other => anyhow::bail!("unrecognized Organizer action: {other}"),
        }
    }

    pub fn accept(&mut self, tick: &crate::work_executor::ExecutionTick) {
        if !tick.work_id.is_empty() && tick.work_id != self.current {
            tracing::warn!(current=%self.current, tick=%tick.work_id, "ignoring tick for non-current work");
            return;
        }
        if !tick.node_id.is_empty() && tick.node_id != self.node() {
            tracing::warn!(current_node=%self.node(), tick_node=%tick.node_id, "ignoring tick for non-current node");
            return;
        }
        if tick.revision > 0 && tick.revision != self.revision() {
            tracing::warn!(current_rev=%self.revision(), tick_rev=%tick.revision, "ignoring tick for outdated task revision");
            return;
        }
        if tick.plan_revision > 0 && tick.plan_revision != self.plan_revision {
            tracing::warn!(current_rev=%self.plan_revision, tick_rev=%tick.plan_revision, "ignoring tick from outdated plan revision");
            return;
        }
        if let Some(out) = &tick.output {
            if let Some(f) = self.frames.get_mut(&self.current) {
                f.output = Some(out.clone());
            }
        }
        self.finish_round(tick.operations_count, tick.repeated_reads);
        if let Some(h) = &tick.handoff {
            self.handoff = Some(h.clone());
        }
    }

    pub fn accept_tick(&mut self, project_operations: usize, repeated_reads: usize, output: Option<Value>, handoff: Option<Value>) {
        if let Some(out) = output {
            if let Some(f) = self.frames.get_mut(&self.current) {
                f.output = Some(out);
            }
        }
        self.finish_round(project_operations, repeated_reads);
        if let Some(h) = handoff {
            self.handoff = Some(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_order() -> WorkOrder {
        WorkOrder {
            id: "start".into(),
            node_id: "work_start".into(),
            revision: 1,
            plan_revision: 1,
            goal: "confirm the page".into(),
            done_when: "the owned listener returns the app".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        }
    }

    #[test]
    fn return_records_done_without_inventing_an_outcome_or_false_flags() {
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![output_order()],false,false).unwrap();
        scheduler.activate_next().unwrap();
        let output=scheduler.return_work(&json!({"summary":"The Worker returned an observation, but made no categorical assessment."})).unwrap();
        assert_eq!(output["done"],true);
        assert_eq!(output["execution_status"],"done");
        assert!(output["outcome"].is_null());
        assert!(output["blocked"].is_null());
        assert!(output["need_split"].is_null());
        assert!(output["limitations"].is_null());
        assert!(output["exported_data"].is_null());

        let mut explicit=WorkScheduler::default();
        explicit.enqueue(vec![output_order()],false,false).unwrap();
        explicit.activate_next().unwrap();
        let output=explicit.return_work(&json!({"summary":"The Worker explicitly assessed this work.","outcome":"blocked"})).unwrap();
        assert_eq!(output["done"],true);
        assert_eq!(output["outcome"],"blocked");
        assert!(output["blocked"].is_null(),"the host must preserve the submitted fields without deriving an alias");
    }

    #[test]
    fn organizer_goal_assessment_is_recorded_without_deriving_it_from_queue_state() {
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![output_order()],false,false).unwrap();
        scheduler.activate_next().unwrap();
        assert!(!scheduler.all_done());
        scheduler.finish("Organizer explicitly assessed the user goal",false).unwrap();
        assert_eq!(scheduler.request_completed,Some(true));
        assert_eq!(scheduler.frame().unwrap().status,WorkStatus::Running);
        assert!(scheduler.output().is_none(),"finishing the request cannot fabricate a Worker return");
    }

    #[test]
    fn worker_return_is_saved_intact_and_readable_after_context_compaction() {
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![output_order()],false,false).unwrap();
        scheduler.activate_next().unwrap();
        let submitted=json!({"summary":format!("  {}  ","Worker failure description\n".repeat(150)),
            "outcome":"failed","limitations":["Font rendering has not been checked"],
            "exported_data":{"original_error":"MODULE_NOT_FOUND\n  at original.js:17"},
            "suggested_children":[{"goal":"Inspect the recorded error","done_when":"Report the cause"}]});
        let output=scheduler.return_work(&submitted).unwrap();
        assert_eq!(output["worker_return"],submitted);
        assert_eq!(output["summary"],submitted["summary"]);
        let exact=scheduler.read_task_result(&json!({"work_id":scheduler.id(),"fields":["worker_return"]})).unwrap();
        assert_eq!(exact["selected_fields"]["worker_return"],submitted);
        let frame=scheduler.frame().unwrap();
        let compact=scheduler.compact_current_result(frame,&output);
        assert!(compact["summary"].as_str().unwrap().len()<submitted["summary"].as_str().unwrap().len());
        assert_eq!(scheduler.output().unwrap()["worker_return"],submitted);
    }

    #[test]
    fn legacy_default_success_is_migrated_to_unknown_on_the_result_and_handoff() {
        let mut scheduler=WorkScheduler::default();
        scheduler.schema_version=1;
        let old=json!({"id":"start","node_id":"work_start","done":true,"execution_status":"done",
            "outcome":"completed","blocked":false,"need_split":false,"summary":"Old saved result"});
        scheduler.frames.insert("start".into(),WorkFrame{order:output_order(),status:WorkStatus::Done,output:Some(old.clone()),..Default::default()});
        scheduler.handoff=Some(json!({"previous_handoff":old,"done":true,"outcome":"completed","blocked":false,"need_split":false}));
        scheduler.normalize_snapshot_version();
        assert_eq!(scheduler.schema_version,WORK_SCHEDULER_SCHEMA_VERSION);
        let saved=scheduler.frames["start"].output.as_ref().unwrap();
        assert!(saved["outcome"].is_null());
        assert_eq!(saved["outcome_unverified_legacy"],true);
        assert!(saved["blocked"].is_null()&&saved["need_split"].is_null());
        assert!(scheduler.handoff.as_ref().unwrap()["outcome"].is_null());
        assert!(scheduler.handoff.as_ref().unwrap()["previous_handoff"]["outcome"].is_null());
    }

    #[test]
    fn project_observation_is_reused_and_sealed_into_the_delivery() {
        let root=std::env::current_dir().unwrap();
        let args=json!({"project_path":".","script":"dev"});
        let sample=crate::project_process::build_process_observation(&root,&args,&[]).unwrap();
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![output_order()],false,false).unwrap();
        scheduler.activate_next().unwrap();
        scheduler.frame_mut().unwrap().project_observation=sample.clone();
        assert_eq!(scheduler.reusable_project_observation(&root,&args),Some(sample.clone()));
        let mut different=args.clone();different["script"]=json!("start");
        assert!(scheduler.reusable_project_observation(&root,&different).is_none());
        let output=scheduler.return_work(&json!({"summary":"No matching managed process was found","exported_data":{"reason":"checked"}})).unwrap();
        assert_eq!(output["exported_data"]["project_observation"],sample);
        assert_eq!(output["exported_data"]["project_path"],".");
        assert_eq!(scheduler.organizer_input()["project_process_observations"][0]["work_id"],"start");
    }

    #[test]
    fn http_samples_are_bound_and_expiry_preserves_downstream_completion() {
        let mut scheduler=WorkScheduler::default();
        let mut producer=output_order();producer.id="http_source".into();producer.node_id="font_probe".into();
        scheduler.enqueue(vec![producer],false,false).unwrap();
        scheduler.activate_next().unwrap();
        let first_url="http://127.0.0.1:38101/api/fonts";
        let second_url="http://127.0.0.1:38102/other";
        for (url,status,error_kind) in [(first_url,json!(200),Value::Null),(second_url,json!(404),Value::Null)] {
            let result=json!({"url":url,"sampled_at":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
                "elapsed_ms":2,"reachable":true,"http_status":status,"error_kind":error_kind,"error_message":Value::Null,
                "check_key":crate::http_probe::check_key(&json!({"url":url})),"check_passed":status==200,"timeout_ms":5000,
                "reuse_window_ms":crate::http_probe::REUSE_WINDOW_MS,"body_summary":"response should stay out of delivery"});
            scheduler.observe("http_probe",&json!({"url":url}),&result,false);
        }
        for index in 0..20 {
            scheduler.observe("list_dir",&json!({"path":format!("folder_{index}")}),&json!({"entries":[]}),false);
        }
        assert_eq!(scheduler.frame().unwrap().operations.len(),16);
        let source_output=scheduler.return_work(&json!({"summary":"font endpoint and unrelated endpoint sampled"})).unwrap();
        let exported=source_output["exported_data"]["http_observations"].as_array().unwrap();
        assert_eq!(exported.len(),2);
        assert!(exported.iter().all(|sample|sample.get("body_summary").is_none()));
        assert!(exported.iter().all(|sample|sample["producer_work_id"]=="http_source"&&sample["node_id"]=="font_probe"&&sample["revision"]==1));

        let check=crate::http_probe::check_key(&json!({"url":first_url}));
        let consumer=WorkOrder{id:"font_check".into(),node_id:"font_consumer".into(),revision:1,plan_revision:1,
            goal:"reuse the checked font endpoint".into(),done_when:"the bound 2xx sample satisfies the check".into(),completion:Completion::Check,
            checks:vec![check.clone()],dependency_inputs:vec![json!({"work_id":"http_source","fields":["http_observations"],"http_urls":[first_url]})],..WorkOrder::default()};
        scheduler.enqueue(vec![consumer],false,true).unwrap();
        assert!(scheduler.activate_next().unwrap());
        assert!(!scheduler.done(),"host-consumed checks do not seal the Worker invocation");
        assert_eq!(scheduler.frame().unwrap().checked.get(&check),Some(&scheduler.frame().unwrap().epoch));
        assert!(scheduler.frame().unwrap().operations.iter().any(|operation|operation["auto_consumed_http_sample_id"].is_string()));
        assert_eq!(scheduler.http_observation_samples.len(),2);
        let input=scheduler.worker_input("verify the font endpoint");
        let bound=input["http_observations"]["bound_upstream"].as_array().unwrap();
        assert_eq!(bound.len(),1);
        assert_eq!(bound[0]["url"],first_url);
        assert_eq!(bound[0]["http_status"],200);
        assert_eq!(bound[0]["fresh_for_cache_reuse"],true);
        assert_eq!(bound[0]["producer_work_id"],"http_source");
        assert_eq!(bound[0]["sampled_at"],exported[0]["sampled_at"]);
        assert_eq!(scheduler.http_probe_results[&check]["sample_id"],bound[0]["sample_id"]);
        assert!(scheduler.organizer_input()["available_dependency_deliveries"].as_array().unwrap().iter()
            .any(|item|item["work_id"]=="http_source"&&item["exported_fields"].as_array().unwrap().iter().any(|field|field=="http_observations")));

        scheduler.return_work(&json!({"summary":"Worker returns the endpoint result after receiving its bound HTTP sample."})).unwrap();
        assert!(scheduler.done());
        let sample_id=bound[0]["sample_id"].as_str().unwrap().to_owned();
        let completed_output=scheduler.frame().unwrap().output.clone();
        scheduler.http_observation_samples.get_mut(&sample_id).unwrap()["sampled_at"]=
            json!((chrono::Utc::now()-chrono::Duration::seconds(60)).to_rfc3339_opts(chrono::SecondsFormat::Millis,true));
        assert!(scheduler.done(),"sample expiry does not revoke historical completion");
        assert!(scheduler.all_done());
        assert!(scheduler.reusable_http_probe(&json!({"url":first_url})).is_none());
        let organizer=scheduler.organizer_input();
        let work=organizer["execution_path"].as_array().unwrap().iter().find(|item|item["work_id"]=="font_check").unwrap();
        assert_eq!(work["status"],"done");
        assert!(work.get("expectation_met").is_none());
        assert_eq!(organizer["current_result"]["summary"],completed_output.as_ref().unwrap()["summary"]);
        assert!(organizer["handoff"].is_null());
        assert!(organizer.get("current_output").is_none());
        assert!(organizer.get("available_select_task_ids").is_none());
        assert!(!scheduler.refresh_http_check_validity());
        let frame=scheduler.frame().unwrap();
        assert_eq!(frame.status,WorkStatus::Done);
        assert_eq!(frame.output,completed_output);
        assert_eq!(frame.checked.get(&check),Some(&frame.epoch));
        assert_eq!(frame.http_check_samples.get(&check),Some(&sample_id));
        assert!(frame.check_errors.is_empty());
        let delivery=&scheduler.worker_input("read the historical result")["http_observations"]["bound_upstream"][0];
        assert_eq!(delivery["fresh_for_cache_reuse"],false);
        assert_eq!(delivery["cache_invalidation_reason"],"expired");
    }

    #[test]
    fn bound_http_404_remains_reachable_but_does_not_pass_a_2xx_check() {
        let mut scheduler=WorkScheduler::default();
        let mut producer=output_order();producer.id="http_404".into();producer.node_id="http_404".into();
        scheduler.enqueue(vec![producer],false,false).unwrap();scheduler.activate_next().unwrap();
        let url="http://127.0.0.1:38103/missing";
        let result=json!({"url":url,"sampled_at":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
            "elapsed_ms":1,"reachable":true,"http_status":404,"error_kind":Value::Null,"error_message":Value::Null,
            "check_key":crate::http_probe::check_key(&json!({"url":url})),"check_passed":false,"timeout_ms":5000,
            "reuse_window_ms":crate::http_probe::REUSE_WINDOW_MS});
        scheduler.observe("http_probe",&json!({"url":url}),&result,false);
        scheduler.return_work(&json!({"summary":"Endpoint replied with 404"})).unwrap();
        let check=crate::http_probe::check_key(&json!({"url":url}));
        let consumer=WorkOrder{id:"http_404_check".into(),node_id:"http_404_check".into(),goal:"require a 2xx status".into(),done_when:"the 2xx check passes".into(),
            completion:Completion::Check,checks:vec![check.clone()],dependency_inputs:vec![json!({"work_id":"http_404","http_urls":[url]})],..WorkOrder::default()};
        scheduler.enqueue(vec![consumer],false,true).unwrap();scheduler.activate_next().unwrap();
        assert_eq!(scheduler.done(),false);
        assert_eq!(scheduler.frame().unwrap().check_errors[&check]["http_status"],404);
        assert_eq!(scheduler.frame().unwrap().check_errors[&check]["reachable"],true);
        assert_eq!(scheduler.worker_input("inspect status")["http_observations"]["bound_upstream"][0]["http_status"],404);
    }

    #[test]
    fn negative_http_check_can_be_returned_and_sealed_without_becoming_a_pass() {
        for (id,url,status,reachable,error_kind) in [
            ("http_404_return","http://127.0.0.1:38104/missing",json!(404),true,Value::Null),
            ("http_refused_return","http://127.0.0.1:38105/api/fonts",Value::Null,false,json!("connection_refused")),
        ] {
            let check=crate::http_probe::check_key(&json!({"url":url}));
            let mut order=output_order();order.id=id.into();order.node_id=id.into();order.completion=Completion::Check;order.checks=vec![check];
            let mut scheduler=WorkScheduler::default();scheduler.enqueue(vec![order],false,true).unwrap();scheduler.activate_next().unwrap();
            let result=json!({"url":url,"sampled_at":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
                "elapsed_ms":4,"reachable":reachable,"http_status":status,"error_kind":error_kind,"error_message":if reachable{Value::Null}else{json!("connection refused")},
                "check_key":crate::http_probe::check_key(&json!({"url":url})),"check_passed":false,"timeout_ms":5000,
                "reuse_window_ms":crate::http_probe::REUSE_WINDOW_MS});
            scheduler.observe("http_probe",&json!({"url":url}),&result,false);
            let returned=scheduler.return_work(&json!({"summary":if reachable{"The endpoint returned HTTP 404"}else{"Connection refused; no HTTP response arrived"}})).unwrap();
            assert_eq!(returned["done"],true);assert_eq!(returned["execution_status"],"done");
            assert!(returned.get("expectation_met").is_none());assert_eq!(scheduler.frame().unwrap().status,WorkStatus::Done);
            assert!(scheduler.return_work(&json!({"summary":"a second return must not rewrite the sealed result"})).is_err());
        }
    }

    #[test]
    fn native_program_exit_nine_does_not_satisfy_its_declared_check() {
        let mut scheduler=WorkScheduler::default();
        let args=json!({"program":"python","args":["-c","import sys;sys.exit(9)"],"project_path":"."});
        let check=crate::program_execution::check_key(&args);
        let mut order=output_order();order.completion=Completion::Check;order.checks=vec![check.clone()];
        scheduler.enqueue(vec![order],false,true).unwrap();scheduler.activate_next().unwrap();
        let result=json!({"program":"python","args":args["args"],"outcome":"exited","process_exit_code":9,"process_success":false,
            "check_key":check,"stderr":"the tool may write diagnostics here"});
        scheduler.observe("run_program",&args,&result,true);
        assert_eq!(scheduler.frame().unwrap().checked.contains_key(&check),false);
        assert_eq!(scheduler.frame().unwrap().check_errors[&check]["exit_code"],9);
        assert_eq!(scheduler.done(),false);
    }

    #[test]
    fn legacy_unfinished_shell_check_is_migrated_without_replaying_completed_upstream() {
        let mut scheduler=WorkScheduler::default();
        let upstream=WorkFrame {order:WorkOrder{id:"upstream".into(),node_id:"upstream_node".into(),revision:1,plan_revision:1,
            goal:"completed upstream".into(),done_when:"sealed".into(),..WorkOrder::default()},status:WorkStatus::Done,sequence:1,..WorkFrame::default()};
        let mut legacy=WorkFrame {order:WorkOrder{id:"legacy".into(),node_id:"legacy_node".into(),revision:1,plan_revision:1,
            goal:"finish old checked work".into(),done_when:"supported check passes".into(),completion:Completion::Check,
            checks:vec!["npm run build".into()],..WorkOrder::default()},status:WorkStatus::Running,sequence:2,..WorkFrame::default()};
        legacy.checked.insert("npm run build".into(),legacy.epoch);
        scheduler.frames.insert("upstream".into(),upstream);
        scheduler.frames.insert("legacy".into(),legacy);
        scheduler.current="legacy".into();scheduler.activated_history=vec!["upstream".into(),"legacy".into()];
        let migrations=scheduler.prepare_legacy_check_migration();
        assert_eq!(migrations.len(),1);
        assert_eq!(scheduler.frames["legacy"].checked.contains_key("npm run build"),false);
        assert_eq!(scheduler.frames["legacy"].status,WorkStatus::Running,"a persisted shell-check pass cannot complete old work");
        assert_eq!(scheduler.organizer_input()["legacy_check_contracts"][0]["work_id"],"legacy");
        assert_eq!(scheduler.pending_handoff().unwrap()["legacy_check_contract_migration_required"],true);

        let replacement="program:python:.:[]";
        scheduler.apply(&json!({"action":"revisit","target_node_id":"legacy_node","target_revision":1,"reason":"replace retired shell check",
            "replacement_checks":[replacement]}),false,true).unwrap();
        assert_eq!(scheduler.current,"legacy_node_r2");
        assert_eq!(scheduler.order().unwrap().checks,vec![replacement]);
        assert_eq!(scheduler.frames["legacy"].invalidated_by_plan_revision,Some(2));
        assert_eq!(scheduler.frames["upstream"].invalidated_by_plan_revision,None,"completed upstream stays sealed and is not replayed");

        let mut rejected=WorkScheduler::default();
        let mut shell_order=output_order();shell_order.completion=Completion::Check;shell_order.checks=vec!["python -c 'exit(9)'".into()];
        assert!(rejected.enqueue(vec![shell_order],false,true).unwrap_err().to_string().contains("shell command strings are no longer accepted"));
    }

    #[test]
    fn workspace_process_list_reuses_across_orders_but_never_short_circuits_a_wait() {
        let root=std::env::current_dir().unwrap();
        let sample=crate::project_process::build_process_observation(&root,&json!({}),&[]).unwrap();
        assert_eq!(sample["scope"]["coverage"],"workspace_list");
        let mut first=output_order();first.id="list_first".into();first.node_id="work_list_first".into();
        let mut second=output_order();second.id="list_second".into();second.node_id="work_list_second".into();
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![first,second],false,false).unwrap();
        scheduler.activate_next().unwrap();
        scheduler.observe("get_project_process",&json!({}),&json!({"processes":[],"process_observation":sample.clone()}),false);
        scheduler.return_work(&json!({"summary":"Workspace process list captured","exported_data":{"process_observation":sample}})).unwrap();
        scheduler.activate_next().unwrap();

        assert!(scheduler.reusable_project_observation(&root,&json!({})).is_some());
        assert!(scheduler.reusable_project_observation(&root,&json!({"max_chars":4096})).is_some());
        assert!(scheduler.reusable_project_observation(&root,&json!({"wait_seconds":30})).is_none());
        assert!(scheduler.reusable_project_observation(&root,&json!({"ready_url":"http://127.0.0.1:4321"})).is_none());
        assert!(scheduler.reusable_project_observation(&root,&json!({"project_path":"."})).is_none());
    }

    #[test]
    fn visual_work_can_include_checks_and_expire_after_page_interaction() {
        let mut scheduler=WorkScheduler::default();let mut order=output_order();order.visual_goal=Some("confirm rendered slide".into());
        order.completion=Completion::WriteCheck;order.edit_targets=vec!["slide.ts".into()];order.checks=vec!["npm:.:test".into()];
        scheduler.enqueue(vec![order],true,true).unwrap();scheduler.activate_next().unwrap();
        scheduler.observe("browser_open",&json!({}),&json!({"page":{"page_id":"p","page_epoch":1}}),false);
        scheduler.frames.get_mut(&scheduler.current).unwrap().visual_check_result=json!({"assessment":"pass"});
        let epoch=scheduler.frame().unwrap().epoch;
        scheduler.observe("browser_press_key",&json!({"key":"Delete"}),&json!({"page":{"page_id":"p","page_epoch":2}}),false);
        assert_eq!(scheduler.frame().unwrap().epoch,epoch+1);assert!(scheduler.frame().unwrap().visual_check_result.is_null());
        let returned=scheduler.return_work(&json!({"summary":"captured","status":"done","limitations":["当前页面截图已捕获，但没有绑定到此版本的视觉判断"]})).unwrap();
        assert_eq!(returned["done"],true);assert!(returned.get("expectation_met").is_none());
    }

    #[test]
    fn continue_delivers_constraints_and_stops_after_two_empty_rounds() {
        let mut scheduler = WorkScheduler::default();
        scheduler.enqueue(vec![output_order()], false, false).unwrap();
        assert!(scheduler.activate_next().unwrap());
        scheduler.finish_round(1, 0);
        scheduler.finish_round(1, 0);
        scheduler.continue_decision(&json!({"reason":"use port 5173","orders":[{"id":"start","constraints":["ready_url http://127.0.0.1:5173"],"final_answer":false}]}), false, false).unwrap();
        let input = scheduler.worker_input("start the app");
        assert_eq!(input["current_work"]["constraints"][0], "ready_url http://127.0.0.1:5173");
        assert_eq!(input["organizer_handoff"]["instruction"], "use port 5173");
        assert_eq!(input["organizer_handoff"]["final_answer"], false);
        scheduler.finish_round(1, 0);
        scheduler.finish_round(1, 0);
        scheduler.continue_decision(&json!({"reason":"still the same page"}), false, false).unwrap();
        scheduler.finish_round(1, 0);
        scheduler.finish_round(1, 0);
        scheduler.continue_decision(&json!({"reason":"again"}), false, false).unwrap();
        scheduler.finish_round(1, 0);
        scheduler.finish_round(1, 0);
        assert!(scheduler.continue_decision(&json!({"reason":"third empty continue"}), false, false).is_err());
    }

    #[test]
    fn browser_work_can_return_negative_findings_without_claiming_expectations_met() {
        let mut scheduler = WorkScheduler::default();
        let mut order = output_order();
        order.constraints = vec!["requires_browser".into()];
        scheduler.enqueue(vec![order.clone()], false, false).unwrap();
        scheduler.activate_next().unwrap();
        let missing=scheduler.return_work(&json!({"summary":"页面已打开，但目标内容未能匹配","outcome":"blocked"})).unwrap();
        assert_eq!(missing["done"],true);assert!(missing.get("expectation_met").is_none());assert!(scheduler.done());

        let mut scheduler=WorkScheduler::default();scheduler.enqueue(vec![order.clone()],false,false).unwrap();scheduler.activate_next().unwrap();
        let old_page=json!({"browser_session_id":"s","page_id":"p","page_epoch":1,"url":"http://localhost:3000/"});
        let current_page=json!({"browser_session_id":"s","page_id":"p","page_epoch":2,"url":"http://localhost:3000/"});
        scheduler.observe("browser_open",&json!({}),&json!({"page":old_page}),false);
        scheduler.observe("browser_read", &json!({"expect_text":"Slide 1"}), &json!({"matched":false,"expect_text":"Slide 1","page":old_page}), false);
        scheduler.observe("browser_open",&json!({"url":"http://localhost:3000/other"}),&json!({"page":current_page.clone()}),false);
        let stale=scheduler.return_work(&json!({"summary":"旧页面的匹配结果已失效"})).unwrap();
        assert!(stale.get("expectation_met").is_none());

        let mut scheduler=WorkScheduler::default();scheduler.enqueue(vec![order],false,false).unwrap();scheduler.activate_next().unwrap();
        scheduler.observe("browser_read", &json!({"expect_text":"Slide 1"}), &json!({"matched":true,"expect_text":"Slide 1","page":current_page}), false);
        let current=scheduler.return_work(&json!({"summary":"slide is visible"})).unwrap();
        assert!(current.get("expectation_met").is_none());
    }

    #[test]
    fn pptx_work_requires_matching_upload_and_loaded_current_page_evidence() {
        let mut scheduler=WorkScheduler::default();let mut order=output_order();
        order.constraints=vec!["requires_pptx".into()];order.browser_document_path=Some(r"samples\deck.pptx".into());
        scheduler.enqueue(vec![order],false,false).unwrap();scheduler.activate_next().unwrap();
        assert_eq!(scheduler.order().unwrap().browser_document_path.as_deref(),Some("samples/deck.pptx"));
        let page=|epoch|json!({"browser_session_id":"s","page_id":"p","page_epoch":epoch,"url":"http://localhost:3000/"});
        let welcome=json!({"matched":true,"expect_text":"PPTX Editor Engine","slide_count":0,
            "page_indicator":[],"document_loaded":{"status":"not_loaded","slides_detected":0,"page_indicator":[]},"page":page(1)});
        scheduler.observe("browser_open",&json!({"url":"http://localhost:3000/"}),&json!({"page":page(1)}),false);
        scheduler.observe("browser_read",&json!({"expect_text":"PPTX Editor Engine"}),&welcome,false);
        let upload=json!({"ok":true,"status":"file_assigned","uploaded":"samples/deck.pptx","input":{"change_event_received":true},"page":page(2)});
        scheduler.observe("browser_upload",&json!({"path":"samples/deck.pptx"}),&upload,false);
        scheduler.observe("browser_wait",&json!({"document_loaded":true}),&json!({"ok":true,"status":"matched","document_loaded":true,"page":page(2)}),false);

        let loaded_read=|epoch|json!({"matched":true,"expect_text":"Slide 1","slide_count":2,"page_indicator":["1 / 2"],
            "document_loaded":{"status":"loaded","slides_detected":2,"page_indicator":["1 / 2"],"loading_indicators":[]},"page":page(epoch)});
        scheduler.observe("browser_read",&json!({"expect_text":"Slide 1"}),&loaded_read(2),false);
        let loaded=scheduler.return_work(&json!({"summary":"the real file is loaded"})).unwrap();
        assert_eq!(loaded["done"],true);assert!(loaded.get("expectation_met").is_none());

        let mut scheduler=WorkScheduler::default();let mut order=output_order();order.constraints=vec!["requires_pptx".into()];order.browser_document_path=Some("samples/deck.pptx".into());
        scheduler.enqueue(vec![order],false,false).unwrap();scheduler.activate_next().unwrap();
        scheduler.observe("browser_open",&json!({}),&json!({"page":page(1)}),false);
        scheduler.observe("browser_upload",&json!({}),&upload,false);
        scheduler.observe("browser_read",&json!({"expect_text":"Slide 1"}),&loaded_read(2),false);
        scheduler.observe("browser_open",&json!({"url":"http://localhost:3000/"}),&json!({"page":page(3)}),false);
        scheduler.observe("browser_read",&json!({"expect_text":"Slide 1"}),&loaded_read(3),false);
        let stale=scheduler.return_work(&json!({"summary":"旧上传页面已切换，无法确认当前文稿"})).unwrap();
        assert_eq!(stale["done"],true);assert!(stale.get("expectation_met").is_none());
    }

    #[test]
    fn no_text_browser_read_preserves_loaded_upload_and_page_evidence() {
        let mut scheduler=WorkScheduler::default();let mut order=output_order();
        order.constraints=vec!["requires_pptx".into()];order.browser_document_path=Some("samples/deck.pptx".into());
        scheduler.enqueue(vec![order],false,false).unwrap();scheduler.activate_next().unwrap();
        let page=|epoch|json!({"browser_session_id":"session-1","page_id":"tab-1","page_epoch":epoch,"url":"http://localhost:3000/"});
        scheduler.observe("browser_open",&json!({"url":"http://localhost:3000/"}),&json!({"page":page(1)}),false);
        scheduler.observe("browser_upload",&json!({"path":"samples/deck.pptx"}),&json!({"status":"file_assigned","uploaded":"samples/deck.pptx",
            "upload_attempt_id":"attempt-7","input":{"change_event_received":true},"file":{"name":"deck.pptx","size_bytes":123,"files_length":1},"page":page(2)}),false);
        let loaded=json!({"matched":true,"expect_text":"Slide 1","page":page(2),"document_loaded":{"status":"loaded","confirmation":"application_load_cycle",
            "upload_attempt_id":"attempt-7","upload_change_observed":true,"upload_load_status":"loaded","slides_detected":4,
            "valid_page_indicator":true,"page_indicator":["1 / 4"],"error_indicators":[]},"text":"Slide 1"});
        scheduler.observe("browser_read",&json!({"expect_text":"Slide 1"}),&loaded,false);
        scheduler.observe("browser_read",&json!({}),&json!({"matched":true,"page":page(2),"document_loaded":loaded["document_loaded"],
            "slide_count":4,"page_indicator":["1 / 4"],"text":"Slide 1"}),false);

        let read=scheduler.frame().unwrap().browser_current_read.as_ref().unwrap();
        assert_eq!(read["expect_text"],Value::Null);
        assert_eq!(read["document_loaded"]["status"],"loaded");
        assert_eq!(read["document_loaded"]["upload_attempt_id"],"attempt-7");
        assert_eq!(read["page"]["page_epoch"],2);
        assert!(scheduler.browser_presentation_loaded(scheduler.frame().unwrap()));
        let returned=scheduler.return_work(&json!({"summary":"deck loaded and controls inspected"})).unwrap();
        assert!(returned.get("expectation_met").is_none());
    }

    #[test]
    fn pptx_contract_requires_a_workspace_relative_target() {
        let mut scheduler=WorkScheduler::default();let mut order=output_order();order.constraints=vec!["requires_pptx".into()];
        let error=scheduler.enqueue(vec![order],false,false).unwrap_err().to_string();
        assert!(error.contains("field_path=orders[0].browser_document_path"),"{error}");
    }

    #[test]
    fn revisit_backtracks_and_invalidates_downstream_tasks() {
        let mut scheduler = WorkScheduler::default();
        let order_a = WorkOrder {
            id: "A".into(),
            node_id: "node_A".into(),
            goal: "start backend service".into(),
            done_when: "backend service is ready".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        let order_b = WorkOrder {
            id: "B".into(),
            node_id: "node_B".into(),
            goal: "start frontend dev server".into(),
            done_when: "frontend server is ready".into(),
            upstream_ids: vec!["A".into()],
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        let order_c = WorkOrder {
            id: "C".into(),
            node_id: "node_C".into(),
            goal: "load test document".into(),
            done_when: "document is rendered".into(),
            upstream_ids: vec!["B".into()],
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        scheduler.enqueue(vec![order_a, order_b, order_c], false, false).unwrap();
        assert_eq!(scheduler.plan_revision, 1);
        assert!(scheduler.activate_next().unwrap());
        assert_eq!(scheduler.current, "A");

        // Complete A
        scheduler.return_work(&json!({"summary": "backend on port 8000"})).unwrap();
        assert!(scheduler.done());

        // Organizer activates B
        assert!(scheduler.select_task(Some("B")).unwrap());
        assert_eq!(scheduler.current, "B");
        scheduler.return_work(&json!({"summary": "frontend on port 3000"})).unwrap();
        assert!(scheduler.done());

        // Organizer activates C
        assert!(scheduler.select_task(Some("C")).unwrap());
        assert_eq!(scheduler.current, "C");

        // C yields upstream_problem pointing to node_A
        let ret = scheduler.return_work(&json!({
            "summary": "backend returned 500 error",
            "outcome": "upstream_problem",
            "upstream_problem": {
                "node_id": "node_A",
                "reason": "backend API crashed"
            }
        })).unwrap();
        assert_eq!(ret["outcome"], "upstream_problem");
        assert!(scheduler.done());
        assert_eq!(ret["done"],true);
        assert!(ret.get("expectation_met").is_none());

        // Organizer decides revisit to node_A
        let rev_result = scheduler.revisit("node_A", None, "backend API crashed, restart on 8080", Some("start backend on 8080"), None, false, false).unwrap();
        assert_eq!(rev_result, "node_A_r2");
        assert_eq!(scheduler.current, "node_A_r2");
        assert_eq!(scheduler.revision(), 2);
        assert_eq!(scheduler.plan_revision, 2);

        // Check downstream invalidation: B and C must be invalidated with matching plan_revision
        assert_eq!(scheduler.frames["B"].invalidated_by_plan_revision, Some(2));
        assert_eq!(scheduler.frames["C"].invalidated_by_plan_revision, Some(2));

        // Check rewind record
        assert_eq!(scheduler.rewind_records.len(), 1);
        assert_eq!(scheduler.rewind_records[0].source_node, "node_C");
        assert_eq!(scheduler.rewind_records[0].target_node, "node_A");
        assert_eq!(scheduler.rewind_records[0].target_revision, 2);

        // Check flow plan output
        let plan = scheduler.flow_plan();
        let nodes = plan["nodes"].as_array().unwrap();
        let b_node = nodes.iter().find(|n| n["work_id"] == "B").unwrap();
        assert_eq!(b_node["status"], "deprecated");
        let a2_node = nodes.iter().find(|n| n["work_id"] == "node_A_r2").unwrap();
        assert_eq!(a2_node["status"], "running");
        assert_eq!(a2_node["revision"], 2);

        // Deprecated edges and rewind edges present
        let edges = plan["edges"].as_array().unwrap();
        assert!(edges.iter().any(|e| e["rewind"] == true && (e["source"] == "C" || e["source_node"] == "node_C") && (e["target"] == "node_A_r2" || e["target_node"] == "node_A")));
    }

    #[test]
    fn revisit_respects_execution_order_over_creation_sequence_and_maps_node_ids() {
        let mut scheduler = WorkScheduler::default();
        // Task B created FIRST (sequence 1)
        let order_b = WorkOrder {
            id: "task_b".into(),
            node_id: "work_b".into(),
            goal: "start frontend".into(),
            done_when: "frontend is running".into(),
            upstream_ids: vec!["task_a".into()],
            dependency_inputs: vec![json!({"work_id": "task_a", "revision": 1, "fields": ["port"]})],
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        // Task A created SECOND (sequence 2)
        let order_a = WorkOrder {
            id: "task_a".into(),
            node_id: "work_a".into(),
            goal: "start backend".into(),
            done_when: "backend is running".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        scheduler.enqueue(vec![order_b, order_a], false, false).unwrap();

        // Execution order: Task A runs FIRST
        assert!(scheduler.select_task(Some("task_a")).unwrap());
        assert_eq!(scheduler.current, "task_a");
        scheduler.return_work(&json!({
            "summary": "backend listening on port 8080",
            "exported_data": {
                "port": 8080,
                "host": "127.0.0.1"
            }
        })).unwrap();
        assert!(scheduler.done());

        // Task B runs SECOND
        assert!(scheduler.select_task(Some("task_b")).unwrap());
        assert_eq!(scheduler.current, "task_b");
        // Verify Task B receives Task A's exported_data field
        let input_b = scheduler.worker_input("launch system");
        let upstream_outs = input_b["upstream_outputs"].as_array().unwrap();
        assert_eq!(upstream_outs.len(), 1);
        assert_eq!(upstream_outs[0]["port"], 8080);
        assert_eq!(upstream_outs[0]["exported_data"]["port"], 8080);

        // Task B fails and triggers revisit to work_b
        let new_work_id = scheduler.revisit("work_b", None, "change port configuration", None, None, false, false).unwrap();
        assert_eq!(new_work_id, "work_b_r2");
        assert_eq!(scheduler.current, "work_b_r2");

        // KEY ASSERTION: Task A (creation sequence 2 > sequence 1 of Task B) was executed BEFORE Task B,
        // so Task A MUST NOT be invalidated by backtracking to Task B!
        assert!(scheduler.frames["task_a"].invalidated_by_plan_revision.is_none());
        // Old Task B instance IS superseded/invalidated
        assert_eq!(scheduler.frames["task_b"].invalidated_by_plan_revision, Some(2));

        // Rewind record target_node is the TaskTree node_id
        let rewind = scheduler.rewind_records.last().unwrap();
        assert_eq!(rewind.target_node, "work_b");
        assert_eq!(rewind.target_work_id, "work_b_r2");
        assert_eq!(rewind.target_revision, 2);

        // TaskTree integration test: TaskTree revisit_node receives target_node and invalidated_node_ids
        let mut tree = crate::flow_tree::TaskTree::default();
        tree.ensure_request_goal("overall goal").unwrap();
        tree.ensure_work_child("work_a", "start backend", "backend is running", &[]).unwrap();
        tree.ensure_work_child("work_b", "start frontend", "frontend is running", &[]).unwrap();
        // Complete work_a in tree
        tree.apply(&json!({"current_node_id": "work_a"})).unwrap();
        tree.record_work_return(&json!({"node_id": "work_a", "done": true, "summary": "backend on 8080"})).unwrap();
        // Switch to work_b
        tree.apply(&json!({"current_node_id": "work_b"})).unwrap();

        // Revisit work_b in tree
        tree.revisit_node(&rewind.target_node, &rewind.invalidated_node_ids).unwrap();
        assert_eq!(tree.active(), "work_b");
        assert_eq!(tree.status("work_b"), Some("running"));
        // work_a was not invalidated, so it remains completed in tree
        assert_eq!(tree.status("work_a"), Some("done"));
    }

    #[test]
    fn dependency_inputs_validation_blocks_activation_on_missing_fields_or_revision_mismatch() {
        let mut scheduler = WorkScheduler::default();
        let order_a = WorkOrder {
            id: "task_a".into(),
            node_id: "work_a".into(),
            goal: "start database".into(),
            done_when: "db ready".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        let order_b_bad_rev = WorkOrder {
            id: "task_b_bad_rev".into(),
            node_id: "work_b".into(),
            goal: "connect to database rev 2".into(),
            done_when: "connected".into(),
            upstream_ids: vec!["task_a".into()],
            dependency_inputs: vec![json!({"work_id": "task_a", "revision": 2})],
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        let order_c_bad_field = WorkOrder {
            id: "task_c_bad_field".into(),
            node_id: "work_c".into(),
            goal: "connect with missing field".into(),
            done_when: "connected".into(),
            upstream_ids: vec!["task_a".into()],
            dependency_inputs: vec![json!({"work_id": "task_a", "fields": ["database_url"]})],
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        let order_d_good = WorkOrder {
            id: "task_d_good".into(),
            node_id: "work_d".into(),
            goal: "connect with valid exported port".into(),
            done_when: "connected".into(),
            upstream_ids: vec!["task_a".into()],
            dependency_inputs: vec![json!({"work_id": "task_a", "revision": 1, "fields": ["db_port"]})],
            completion: Completion::Output,
            ..WorkOrder::default()
        };

        scheduler.enqueue(vec![order_a, order_b_bad_rev, order_c_bad_field, order_d_good], false, false).unwrap();
        scheduler.activate_next().unwrap();
        assert_eq!(scheduler.current, "task_a");

        // Complete A with exported_data
        scheduler.return_work(&json!({
            "summary": "db started on 5432",
            "exported_data": {"db_port": 5432}
        })).unwrap();
        assert!(scheduler.done());

        // Attempting to activate B (bad rev 2) must fail validation
        assert!(scheduler.select_task(Some("task_b_bad_rev")).is_err());

        // Attempting to activate C (missing field database_url) must fail validation
        assert!(scheduler.select_task(Some("task_c_bad_field")).is_err());

        // Attempting to activate D (rev 1 and field db_port satisfied) must succeed
        assert!(scheduler.select_task(Some("task_d_good")).unwrap());
        assert_eq!(scheduler.current, "task_d_good");
        let worker_input = scheduler.worker_input("run task d");
        let upstream = worker_input["upstream_outputs"].as_array().unwrap();
        assert_eq!(upstream[0]["db_port"], 5432);
        assert_eq!(upstream[0]["exported_data"]["db_port"], 5432);
    }

    #[test]
    fn revisit_when_target_is_in_queue_removes_old_target_from_queue() {
        let mut scheduler = WorkScheduler::default();
        let order_a = WorkOrder {
            id: "task_a".into(),
            node_id: "work_a".into(),
            goal: "step 1".into(),
            done_when: "step 1 done".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        let order_b = WorkOrder {
            id: "task_b".into(),
            node_id: "work_b".into(),
            goal: "step 2".into(),
            done_when: "step 2 done".into(),
            upstream_ids: vec!["task_a".into()],
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        let order_c = WorkOrder {
            id: "task_c".into(),
            node_id: "work_c".into(),
            goal: "step 3".into(),
            done_when: "step 3 done".into(),
            upstream_ids: vec!["task_b".into()],
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        scheduler.enqueue(vec![order_a, order_b, order_c], false, false).unwrap();
        scheduler.activate_next().unwrap();
        assert_eq!(scheduler.current, "task_a");

        // Complete A
        scheduler.return_work(&json!({"summary": "A finished"})).unwrap();

        // While task_b is still in queue (not activated yet), Organizer revisits work_b
        assert!(scheduler.queue.contains(&"task_b".to_string()));
        let new_b = scheduler.revisit("work_b", None, "re-plan step 2 before execution", Some("improved step 2"), None, false, false).unwrap();
        assert_eq!(new_b, "work_b_r2");
        assert_eq!(scheduler.current, "work_b_r2");

        // Old task_b MUST be removed from queue
        assert!(!scheduler.queue.contains(&"task_b".to_string()));
        // Downstream task_c must also be removed from queue
        assert!(!scheduler.queue.contains(&"task_c".to_string()));
        // Queue should now be empty
        assert!(scheduler.queue.is_empty());

        // Complete new_b
        scheduler.return_work(&json!({"summary": "new B finished"})).unwrap();
        assert!(scheduler.done());

        // All active tasks should be done, and invalidated tasks do not block all_done()
        assert!(scheduler.all_done());
    }

    #[test]
    fn session_continuation_resumes_unfinished_task_and_supersedes_on_new_work() {
        let mut scheduler = WorkScheduler::default();
        let order_a = WorkOrder {
            id: "task_a".into(),
            node_id: "work_a".into(),
            goal: "long running analysis".into(),
            done_when: "analysis completed".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        scheduler.enqueue(vec![order_a], false, false).unwrap();
        scheduler.activate_next().unwrap();
        assert_eq!(scheduler.current, "task_a");

        // Simulate session interruption: task_a remains Running and unfinished
        // Select_task with target matching current resumes it
        assert!(scheduler.select_task(Some("task_a")).unwrap());
        assert_eq!(scheduler.current, "task_a");

        // Now test Organizer choosing action: "work" with a brand new task
        let new_order = WorkOrder {
            id: "task_new".into(),
            node_id: "work_new".into(),
            goal: "different objective".into(),
            done_when: "different done".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        scheduler.apply(&json!({
            "action": "work",
            "orders": [new_order]
        }), false, false).unwrap();

        // Old task_a should now be marked invalidated by plan_revision
        assert!(scheduler.frames["task_a"].invalidated_by_plan_revision.is_some());
        // New task is now current
        assert_eq!(scheduler.current, "task_new");

        // Complete new task
        scheduler.return_work(&json!({"summary": "new objective done"})).unwrap();
        assert!(scheduler.done());
        // all_done() succeeds without being blocked by old unfinished task_a
        assert!(scheduler.all_done());
    }

    #[test]
    fn dependency_inputs_without_upstream_ids_are_normalized_and_injected_into_worker_input() {
        let mut scheduler = WorkScheduler::default();
        let order_a = WorkOrder {
            id: "task_a".into(),
            node_id: "work_a".into(),
            goal: "generate app configuration".into(),
            done_when: "app configuration ready".into(),
            completion: Completion::Output,
            ..WorkOrder::default()
        };
        // Order B declares dependency_inputs but OMITS upstream_ids entirely
        let order_b = WorkOrder {
            id: "task_b".into(),
            node_id: "work_b".into(),
            goal: "deploy app with config".into(),
            done_when: "deployed".into(),
            upstream_ids: vec![],
            dependency_inputs: vec![json!({"work_id": "task_a", "revision": 1, "fields": ["app_url", "api_key"]})],
            completion: Completion::Output,
            ..WorkOrder::default()
        };

        scheduler.enqueue(vec![order_a, order_b], false, false).unwrap();
        // task_b's upstream_ids must have been normalized to include task_a
        assert_eq!(scheduler.frames["task_b"].order.upstream_ids, vec!["task_a"]);

        scheduler.activate_next().unwrap();
        assert_eq!(scheduler.current, "task_a");

        // Complete A with exported fields
        scheduler.return_work(&json!({
            "summary": "config generated",
            "exported_data": {
                "app_url": "https://localhost:8080",
                "api_key": "secret123"
            }
        })).unwrap();

        // task_b should now be ready and activate
        scheduler.activate_next().unwrap();
        assert_eq!(scheduler.current, "task_b");

        // Worker input must have received the resolved delivery with the fields!
        let input = scheduler.worker_input("deploy the app");
        let upstream = input["upstream_outputs"].as_array().expect("upstream_outputs array");
        assert_eq!(upstream.len(), 1);
        assert_eq!(upstream[0]["app_url"], "https://localhost:8080");
        assert_eq!(upstream[0]["api_key"], "secret123");
        assert_eq!(upstream[0]["exported_data"]["app_url"], "https://localhost:8080");
    }

    #[test]
    fn dependency_resolver_pins_valid_instance_and_never_redirects_exact_invalidated_work() {
        let mut scheduler = WorkScheduler::default();
        let mut initial = output_order();
        initial.id = "task_b".into(); initial.node_id = "node_b".into();
        scheduler.apply(&json!({"action":"work","orders":[initial]}),false,false).unwrap();
        scheduler.return_work(&json!({"summary":"old delivery"})).unwrap();
        scheduler.apply(&json!({"action":"revisit","target_node_id":"node_b"}),false,false).unwrap();
        scheduler.return_work(&json!({"summary":"new delivery","exported_data":{"url":"new"}})).unwrap();
        let mut consumer=output_order();
        consumer.id="consumer".into(); consumer.node_id="node_c".into();
        consumer.upstream_ids=vec!["node_b".into()];
        consumer.dependency_inputs=vec![json!({"node_id":"node_b","fields":["url"]})];
        scheduler.apply(&json!({"action":"work","orders":[consumer.clone()]}),false,false).unwrap();
        let order=scheduler.order().unwrap();
        assert_eq!(order.upstream_ids,vec!["node_b_r2"]);
        assert_eq!(order.dependency_inputs[0]["work_id"],"node_b_r2");
        assert_eq!(scheduler.resolve_dependency_deliveries(order).unwrap()[0]["url"],"new");

        // An explicit old work ID with a valid node ID must still be rejected.
        consumer.dependency_inputs=vec![json!({"work_id":"task_b","node_id":"node_b"})];
        assert!(scheduler.resolve_dependency_deliveries(&consumer).is_err());
        let before=scheduler.snapshot();
        consumer.id="bad_consumer".into(); consumer.node_id="bad_node".into();
        assert!(scheduler.apply(&json!({"action":"work","orders":[consumer]}),false,false).is_err());
        assert_eq!(scheduler.snapshot(),before);
    }

    #[test]
    fn node_dependencies_choose_revision_then_sequence_and_honor_requested_revision() {
        let mut scheduler=WorkScheduler::default();
        for (id,revision,sequence) in [("z_old",1,1),("z_latest",2,3),("a_older_sequence",2,2)] {
            let mut order=output_order(); order.id=id.into(); order.node_id="node".into(); order.revision=revision;
            scheduler.frames.insert(id.into(),WorkFrame {order,status:WorkStatus::Done,sequence,
                output:Some(json!({"id":id,"revision":revision,"done":true,"summary":id})),..Default::default()});
        }
        let mut consumer=output_order();consumer.dependency_inputs=vec![json!({"node_id":"node"})];
        normalize_dependencies(&scheduler.frames,&mut consumer).unwrap();
        assert_eq!(consumer.upstream_ids,vec!["z_latest"]);
        assert_eq!(scheduler.resolve_dependency_deliveries(&consumer).unwrap()[0]["id"],"z_latest");
        consumer.upstream_ids.clear();consumer.dependency_inputs=vec![json!({"node_id":"node","revision":1})];
        normalize_dependencies(&scheduler.frames,&mut consumer).unwrap();
        assert_eq!(consumer.upstream_ids,vec!["z_old"]);
        assert_eq!(scheduler.resolve_dependency_deliveries(&consumer).unwrap()[0]["id"],"z_old");

        // An alias matching an existing work ID denotes that work; a typed node ID
        // remains available when a node and an unrelated work have the same name.
        let mut unrelated=output_order();unrelated.id="node".into();unrelated.node_id="elsewhere".into();
        scheduler.frames.insert("node".into(),WorkFrame {order:unrelated,invalidated_by_plan_revision:Some(2),..Default::default()});
        consumer.upstream_ids=vec!["node".into()];consumer.dependency_inputs.clear();
        assert!(normalize_dependencies(&scheduler.frames,&mut consumer).is_err());
        consumer.upstream_ids.clear();consumer.dependency_inputs=vec![json!({"node_id":"node"})];
        normalize_dependencies(&scheduler.frames,&mut consumer).unwrap();
        assert_eq!(consumer.upstream_ids,vec!["z_latest"]);
    }

    #[test]
    fn organizer_sees_exact_work_node_revision_status_and_delivery_fields() {
        let mut scheduler=WorkScheduler::default();let mut order=output_order();order.node_id="start_backend".into();order.final_answer=false;
        scheduler.enqueue(vec![order],false,false).unwrap();scheduler.activate_next().unwrap();
        scheduler.return_work(&json!({"summary":"Backend is ready","exported_data":{"ready_url":"http://127.0.0.1:3000","font_api":"/api/fonts"}})).unwrap();
        let input=scheduler.organizer_input();
        let work=input["execution_path"].as_array().unwrap().iter().find(|item|item["work_id"]=="start").unwrap();
        assert_eq!(work["node_id"],"start_backend");assert_eq!(work["revision"],1);assert_eq!(work["status"],"done");
        assert!(work["expectation_met"].is_null(),"an output task has no host-checkable condition, so its expectation stays unknown");
        let delivery=&input["available_dependency_deliveries"][0];
        assert_eq!(delivery["work_id"],"start");assert_eq!(delivery["node_id"],"start_backend");assert_eq!(delivery["revision"],1);
        assert_eq!(delivery["exported_fields"],json!(["font_api","ready_url"]));
        assert!(input.get("work_index").is_none());
        assert!(input.get("allowed_next_operations").is_none());
    }

    #[test]
    fn service_checks_and_pptx_visual_work_share_flexible_completion_metadata() {
        let mut startup=output_order();startup.completion=Completion::Check;startup.checks=vec!["npm-start:frontend:dev".into()];
        let mut scheduler=WorkScheduler::default();scheduler.enqueue(vec![startup],false,true).unwrap();

        let mut font_check=output_order();font_check.completion=Completion::Check;font_check.checks=vec!["http-probe:http://127.0.0.1:3000/api/fonts".into()];
        let mut scheduler=WorkScheduler::default();scheduler.enqueue(vec![font_check],false,true).unwrap();

        let mut pptx=output_order();pptx.constraints=vec!["requires_pptx".into()];pptx.browser_document_path=Some("samples/demo.pptx".into());
        pptx.visual_goal=Some("check the rendered slides".into());
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![pptx.clone()],false,true).unwrap();
        pptx.checks=vec!["http-probe:http://127.0.0.1:3000/api/fonts".into()];
        let mut combined=WorkScheduler::default();
        combined.enqueue(vec![pptx.clone()],false,true).unwrap();
        combined.activate_next().unwrap();
        assert_eq!(combined.order().unwrap().checks.len(),1,"visual inspection can carry an independently declared font HTTP observation");
        pptx.completion=Completion::Check;
        let mut check_visual=WorkScheduler::default();
        check_visual.enqueue(vec![pptx],false,true).unwrap();
        check_visual.activate_next().unwrap();
        assert!(check_visual.order().unwrap().visual_goal.is_some());

        let mut invalid_start=output_order();invalid_start.checks=vec!["npm-start:frontend:dev".into()];
        WorkScheduler::default().enqueue(vec![invalid_start],false,true).unwrap();
    }

    #[test]
    fn organizer_packet_has_one_compact_current_result_and_keeps_other_nodes_as_references() {
        let mut scheduler=WorkScheduler::default();scheduler.enqueue(vec![output_order()],false,false).unwrap();scheduler.activate_next().unwrap();
        scheduler.return_work(&json!({"summary":"Current result","material_ids":[17],"finding_ids":["f-1"],
            "suggested_children":[{"goal":"Start the service","done_when":"The service returns a ready response",
                "upstream_ids":["start_service"],"dependency_inputs":[{"work_id":"start_service","fields":["ready_url"]}],
                "debug_source":"SUGGESTION_RAW_DETAIL"}]})).unwrap();
        let huge="RAW_EXECUTION_LOG ".repeat(12_000);
        let long_export="EXPORTED_DETAIL ".repeat(12_000);
        let handoff={
            let output=scheduler.frames.get_mut("start").unwrap().output.as_mut().unwrap();
            output["operations"]=json!([{"result":huge.clone()}]);
            output["modified_files"]=json!({"src/main.rs":{"content":huge}});
            output["exported_data"]=json!({"font_check":{"url":"http://127.0.0.1:3000/api/fonts","detail":long_export.clone()},
                "debug":{"stdout":long_export.clone()},"source_text":long_export.clone()});
            output.clone()
        };
        scheduler.handoff=Some(handoff);
        scheduler.handoff.as_mut().unwrap()["organizer_failure"]=json!({"failure_stage":"organizer_contract_validation",
            "last_decision_error":{"field_path":"orders[0].checks"}});
        let input=scheduler.organizer_input();
        let raw=input.to_string();
        assert!(raw.len()<8_000,"organizer packet remained {} bytes",raw.len());
        assert!(!raw.contains("RAW_EXECUTION_LOG"));
        assert!(!raw.contains(long_export.as_str()));
        assert!(input["handoff"].is_null());
        assert!(input.get("current_output").is_none());
        assert_eq!(input["current_result"]["summary"],"Current result");
        assert_eq!(input["current_result"]["suggested_children"][0]["goal"],"Start the service");
        assert_eq!(input["current_result"]["suggested_children"][0]["done_when"],"The service returns a ready response");
        assert_eq!(input["current_result"]["suggested_children"][0]["dependency_inputs"][0]["work_id"],"start_service");
        assert!(input["current_result"]["suggested_children"][0].get("debug_source").is_none());
        assert_eq!(input["current_result"]["modified_files"],json!(["src/main.rs"]));
        assert_eq!(input["current_result"]["exported_data"]["font_check"]["url"],"http://127.0.0.1:3000/api/fonts");
        assert_eq!(input["current_result"]["exported_data"]["font_check"]["detail"].as_str().unwrap().chars().count(),1200);
        assert!(input["current_result"]["context_omissions"]["items"].as_array().unwrap().iter()
            .any(|item|item["path"]=="current_result.exported_data.font_check.detail"&&item["omitted_chars"]==json!(long_export.chars().count()-1200)),
            "compacted current-result fields must identify the omitted range and read reference");
        assert!(input["current_result"]["exported_data"]["debug"].get("stdout").is_none());
        assert!(input["current_result"]["exported_data"].get("source_text").is_none());
        assert!(input.get("node_summaries").is_none());
        let work=input["execution_path"].as_array().unwrap().iter().find(|entry|entry["work_id"]=="start").unwrap();
        assert!(work["summary"].is_null());
        assert!(work["outcome"].is_null());
        assert!(work["limitations"].is_null());
        assert_eq!(work["result_reference"]["method"],"current_result");
        assert!(!input["execution_narrative"].as_str().unwrap().contains("Worker return (completed): Current result"),
            "the current Worker return must only appear once");
        assert!(input["execution_narrative"].as_str().unwrap().contains("Current Worker return is provided once in current_result"));
        assert!(input.get("work_index").is_none());
        assert_eq!(scheduler.frames["start"].output.as_ref().unwrap()["exported_data"]["font_check"]["detail"].as_str().unwrap(),long_export.as_str());
        let consumer=WorkOrder{id:"font_result_consumer".into(),node_id:"font_result_consumer".into(),goal:"Use font result".into(),done_when:"Font result used".into(),
            dependency_inputs:vec![json!({"work_id":"start","fields":["font_check"]})],..Default::default()};
        let worker_upstream=scheduler.resolve_dependency_deliveries(&consumer).unwrap();
        assert_eq!(worker_upstream[0]["exported_data"]["font_check"]["detail"].as_str().unwrap(),long_export.as_str());
    }

    #[test]
    fn exact_result_read_can_retrieve_deprecated_history_without_making_it_a_dependency() {
        let old_output=json!({"id":"font_check_r1","node_id":"font_check","revision":1,"done":true,"execution_status":"done",
            "outcome":"blocked","summary":"The old font endpoint was unavailable","limitations":["HTTP sample at the time had no response"],
            "findings":[{"id":"old-finding","summary":"Historical finding"}],"material_ids":[19],"finding_ids":["old-finding"],
            "exported_data":{"font_url":"http://127.0.0.1:3000/api/fonts"},"checks":[],"visual_artifact_ids":[],"visual_check_result":Value::Null,
            "upstream_problem":Value::Null,"suggested_children":[]});
        let old_frame=WorkFrame {order:WorkOrder{id:"font_check_r1".into(),node_id:"font_check".into(),revision:1,plan_revision:1,
            goal:"Check the font service".into(),done_when:"Record the response".into(),..WorkOrder::default()},status:WorkStatus::Done,
            invalidated_by_plan_revision:Some(3),sequence:2,output:Some(old_output),..WorkFrame::default()};
        let mut scheduler=WorkScheduler::default();scheduler.frames.insert("font_check_r1".into(),old_frame);
        let read=scheduler.read_task_result(&json!({"work_id":"font_check_r1","fields":["summary","limitations","findings","font_url"]})).unwrap();
        assert_eq!(read["historical"],true);
        assert_eq!(read["dependency_eligible"],false);
        assert_eq!(read["selected_fields"]["summary"],"The old font endpoint was unavailable");
        assert_eq!(read["selected_fields"]["limitations"][0],"HTTP sample at the time had no response");
        assert_eq!(read["selected_fields"]["font_url"],"http://127.0.0.1:3000/api/fonts");
        let consumer=WorkOrder{id:"new_consumer".into(),node_id:"new_consumer".into(),goal:"Use the old result".into(),done_when:"Use only a current delivery".into(),
            dependency_inputs:vec![json!({"work_id":"font_check_r1"})],..WorkOrder::default()};
        assert!(scheduler.resolve_dependency_deliveries(&consumer).unwrap_err().contains("TASK_DEPRECATED"));
    }

    #[test]
    fn stale_upload_receipt_is_reported_and_removed_from_reusable_delivery_without_reopening_work() {
        let page=json!({"browser_session_id":"session-1","page_id":"page-1","page_epoch":2,"url":"http://127.0.0.1:3000/","request_id":4});
        let receipt=BrowserUploadEvidence{upload_attempt_id:"upload-1".into(),path:"samples/demo.pptx".into(),change_event_received:true,
            page:page.clone(),file:json!({"name":"demo.pptx"}),work_id:"upload_work".into(),node_id:"upload_node".into(),revision:1,load_attempt_required:true};
        let mut frame=WorkFrame{order:WorkOrder{id:"upload_work".into(),node_id:"upload_node".into(),revision:1,plan_revision:1,
            goal:"Upload the deck".into(),done_when:"File change was observed".into(),..Default::default()},status:WorkStatus::Done,sequence:1,
            browser_upload_receipt:Some(receipt.clone()),output:Some(json!({"id":"upload_work","node_id":"upload_node","revision":1,"done":true,
                "outcome":"completed","summary":"PPTX assigned","exported_data":{"browser_upload_receipt":receipt}})),..Default::default()};
        frame.epoch=1;
        let mut scheduler=WorkScheduler::default();scheduler.frames.insert("upload_work".into(),frame);scheduler.current="upload_work".into();
        let page_key=browser_page_key(&page).unwrap();scheduler.active_browser_uploads.insert(page_key, "upload-1".into());
        let consumer=WorkOrder{id:"verify_work".into(),node_id:"verify_node".into(),goal:"Verify the loaded deck".into(),done_when:"The same upload is loaded".into(),
            completion:Completion::Output,browser_document_path:Some("samples/demo.pptx".into()),constraints:vec!["requires_pptx".into()],
            dependency_inputs:vec![json!({"work_id":"upload_work","fields":["browser_upload_receipt"]})],..Default::default()};
        let unvalidated_error=scheduler.resolve_dependency_deliveries(&consumer).unwrap_err();
        assert!(unvalidated_error.contains("field_path=orders[0].dependency_inputs[0].fields"),"{unvalidated_error}");
        let unvalidated_input=scheduler.organizer_input();
        assert_eq!(unvalidated_input["browser_upload_availability"][0]["availability"]["available"],false);
        assert_eq!(unvalidated_input["browser_upload_availability"][0]["availability"]["reason"],"host_validation_not_run");
        assert!(unvalidated_input["current_result"]["exported_data"].get("browser_upload_receipt").is_none());
        scheduler.browser_upload_availability.insert(browser_upload_status_key(&serde_json::to_value(&receipt).unwrap()).unwrap(),
            json!({"available":false,"reason":"browser_session_closed","upload_attempt_id":"upload-1"}));
        let error=scheduler.resolve_dependency_deliveries(&consumer).unwrap_err();
        assert!(error.contains("browser_upload_receipt"),"{error}");
        assert_eq!(scheduler.frames["upload_work"].status,WorkStatus::Done);
        assert!(scheduler.done());
        let input=scheduler.organizer_input();
        assert_eq!(input["browser_upload_availability"][0]["availability"]["available"],false);
        assert_eq!(input["browser_upload_availability"][0]["availability"]["reason"],"browser_session_closed");
        assert!(input["current_result"]["exported_data"].get("browser_upload_receipt").is_none());
        assert!(input["available_dependency_deliveries"][0]["exported_fields"].as_array().unwrap().is_empty());
        scheduler.browser_upload_availability.insert(browser_upload_status_key(&serde_json::to_value(&receipt).unwrap()).unwrap(),
            json!({"available":true,"reason":"active_session_page_and_upload_attempt_match","upload_attempt_id":"upload-1"}));
        let valid_delivery=scheduler.resolve_dependency_deliveries(&consumer).unwrap();
        assert_eq!(valid_delivery[0]["exported_data"]["browser_upload_receipt"]["upload_attempt_id"],"upload-1");
        assert_eq!(valid_delivery[0]["browser_upload_availability"]["available"],true);
        assert_eq!(scheduler.frames["upload_work"].status,WorkStatus::Done);
    }

    #[test]
    fn organizer_flow_directory_paginates_beyond_fifty_while_context_keeps_only_last_twelve() {
        let mut scheduler=WorkScheduler::default();
        for index in 0..60 {
            let id=format!("work_{index:02}");
            let mut order=task(&id,&format!("Goal {index}"));
            order.node_id=format!("node_{index:02}");
            order.revision=1;
            scheduler.activated_history.push(id.clone());
            scheduler.frames.insert(id.clone(),WorkFrame{order,status:WorkStatus::Done,sequence:index+1,
                returned_at:Some(index as u64+1),output:Some(json!({"id":id,"node_id":format!("node_{index:02}"),"revision":1,
                    "done":true,"execution_status":"done","outcome":"completed","summary":format!("Result {index}"),
                    "limitations":[],"material_ids":[index as i64+1],"finding_ids":[format!("finding_{index}")],"exported_data":{}})),
                ..Default::default()});
        }
        scheduler.current="work_59".into();

        let input=scheduler.organizer_input();
        let recent=input["execution_path"].as_array().unwrap();
        assert_eq!(recent.len(),12);
        assert_eq!(recent[0]["step"],49);
        assert_eq!(recent[0]["work_id"],"work_48");
        assert_eq!(recent[11]["step"],60);
        assert!(recent[11]["summary"].is_null(),"the current Worker return appears once in current_result");
        assert_eq!(input["current_result"]["summary"],"Result 59");
        assert!(input.get("current_output").is_none());

        let first=&input["flow_directory"];
        assert_eq!(first["total"],60);
        assert_eq!(first["returned"],50);
        assert_eq!(first["next_cursor"],50);
        assert_eq!(first["entries"][0]["work_id"],"work_00");
        assert_eq!(first["entries"][49]["material_ids"],json!([50]));
        let second=scheduler.read_flow_page(&json!({"cursor":first["next_cursor"],"limit":50})).unwrap();
        assert_eq!(second["offset"],50);
        assert_eq!(second["returned"],10);
        assert_eq!(second["entries"][0]["work_id"],"work_50");
        assert!(second["next_cursor"].is_null());
        assert!(second["entries"][9]["summary"].is_null(),"the current return is not duplicated in the paged directory");
        assert_eq!(second["omitted_range"]["through_step"],50);
        assert!(scheduler.read_flow_page(&json!({"cursor":0,"limit":51})).unwrap_err().to_string().contains("at most 50"));
    }

    #[test]
    fn invalid_dependency_delivery_names_the_exact_field_and_expected_exported_fields() {
        let mut scheduler=WorkScheduler::default();let mut upstream=output_order();upstream.id="backend_work".into();upstream.node_id="backend".into();upstream.revision=3;
        scheduler.frames.insert("backend_work".into(),WorkFrame{order:upstream,status:WorkStatus::Done,output:Some(json!({"id":"backend_work","node_id":"backend","revision":3,"done":true,"summary":"Backend ready","exported_data":{"font_api":"/api/fonts"}})),..Default::default()});
        let mut consumer=output_order();consumer.id="ppt_upload".into();consumer.node_id="ppt_upload_node".into();
        consumer.dependency_inputs=vec![json!({"work_id":"backend_work","node_id":"backend","revision":3,"fields":["ready_url"]})];
        let error=scheduler.resolve_dependency_deliveries(&consumer).unwrap_err();
        assert!(error.contains("field_path=orders[0].dependency_inputs[0].fields"),"{error}");
        assert!(error.contains("available exported_data fields: [font_api]"),"{error}");
        assert!(error.contains("FIELD_NOT_EXPORTED"),"{error}");
    }

    fn task(id:&str,goal:&str)->WorkOrder {
        WorkOrder{id:id.into(),node_id:format!("work_{id}"),goal:goal.into(),done_when:"return the actual result".into(),
            completion:Completion::Output,..WorkOrder::default()}
    }

    fn http_sample(url:&str,status:Option<u16>,seconds_ago:i64)->Value {
        let at=(chrono::Utc::now()-chrono::Duration::seconds(seconds_ago)).to_rfc3339_opts(chrono::SecondsFormat::Millis,true);
        json!({"url":url,"sampled_at":at,"elapsed_ms":3,"reachable":status.is_some(),"http_status":status,
            "error_kind":if status.is_none(){json!("connection_refused")}else{Value::Null},
            "error_message":if status.is_none(){json!("connection refused")}else{Value::Null},
            "check_key":crate::http_probe::check_key(&json!({"url":url})),"check_passed":status.is_some_and(|code|(200..300).contains(&code)),
            "timeout_ms":5000,"reuse_window_ms":crate::http_probe::REUSE_WINDOW_MS})
    }

    #[test]
    fn report_only_rounds_continue_the_task_and_only_a_bounded_stall_hands_off() {
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![task("start","Start the editor")],false,false).unwrap();
        scheduler.activate_next().unwrap();
        scheduler.record_progress(&json!({"purpose":"Start the editor","next_action":"run the dev script"}),1,1);
        scheduler.finish_round(0,0);
        assert!(!scheduler.needs_organizer(),"one report-only round stays with the Worker");
        scheduler.finish_round(1,0);
        scheduler.finish_round(0,0);
        scheduler.finish_round(0,0);
        assert!(!scheduler.needs_organizer(),"an actual operation resets the stall count");
        assert_eq!(scheduler.frame().unwrap().progress.len(),1);
        scheduler.finish_round(0,0);
        let handoff=scheduler.pending_handoff().unwrap();
        assert_eq!(handoff["done"],false);
        assert_eq!(handoff["stall"]["kind"],"report_only");
        assert_eq!(handoff["stall"]["task_returned"],false);
        assert_eq!(scheduler.frame().unwrap().status,WorkStatus::Running,"a stall is not a TaskReturn");
        assert!(scheduler.frame().unwrap().output.is_none());

        // The same goal may stall twice; a third identical dispatch is refused.
        scheduler.apply(&json!({"action":"work","reason":"try once more","orders":[task("start_again","Start  the editor")]}),false,false).unwrap();
        for _ in 0..REPORT_ONLY_ROUND_LIMIT {scheduler.finish_round(0,0);}
        assert_eq!(scheduler.stalled_goals[&stall_key("Start the editor")],2);
        let before=scheduler.snapshot();
        let error=scheduler.apply(&json!({"action":"work","reason":"again","orders":[task("start_third","start the editor")]}),false,false).unwrap_err();
        assert!(error.to_string().contains("STALLED_GOAL_REPEATED"),"{error}");
        assert_eq!(scheduler.snapshot(),before,"a rejected decision leaves no partial change");
    }

    #[test]
    fn successor_waits_for_the_producer_return_and_then_receives_it() {
        let mut scheduler=WorkScheduler::default();
        let mut producer=task("probe","Probe the font API");producer.completion=Completion::Output;
        scheduler.enqueue(vec![producer],false,false).unwrap();
        scheduler.activate_next().unwrap();
        let consumer=|id:&str|{let mut order=task(id,"Use the probe result");
            order.dependency_inputs=vec![json!({"work_id":"probe","revision":1,"fields":["font_api"]})];order};

        let before=scheduler.snapshot();
        let error=scheduler.apply(&json!({"action":"work","reason":"next","orders":[consumer("use_early")]}),false,false).unwrap_err();
        assert!(error.to_string().contains("TASK_NOT_RETURNED"),"{error}");
        assert_eq!(scheduler.snapshot(),before,"the running producer, queue and frames are unchanged");

        scheduler.return_work(&json!({"summary":"Font API answered","exported_data":{"font_api":"/api/fonts"}})).unwrap();
        scheduler.apply(&json!({"action":"work","reason":"use the returned result","orders":[consumer("use")]}),false,false).unwrap();
        assert_eq!(scheduler.frames["probe"].status,WorkStatus::Done);
        assert!(scheduler.frames["probe"].invalidated_by_plan_revision.is_none(),"creating B never deprecates a returned A");
        assert_eq!(scheduler.current,"use");
        let upstream=&scheduler.worker_input("go")["upstream_outputs"][0];
        assert_eq!(upstream["exported_data"]["font_api"],"/api/fonts");
        assert_eq!(upstream["revision"],1);
    }

    #[test]
    fn superseding_an_unreturned_task_is_explicit_and_kept_as_history() {
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![task("upload","Upload the PPTX")],false,false).unwrap();
        scheduler.activate_next().unwrap();
        scheduler.record_progress(&json!({"purpose":"Upload","next_action":"choose the file input"}),1,2);
        scheduler.apply(&json!({"action":"work","reason":"the upload page changed; reopen first","orders":[task("reopen","Reopen the editor")]}),false,false).unwrap();
        let old=&scheduler.frames["upload"];
        assert!(old.invalidated_by_plan_revision.is_some());
        let record=old.superseded.as_ref().unwrap();
        assert_eq!(record["task_returned"],false);
        assert_eq!(record["replaced_by"],json!(["reopen"]));
        assert_eq!(record["last_progress"]["next_action"],"choose the file input");
        let input=scheduler.organizer_input();
        let narrative=input["execution_narrative"].as_str().unwrap();
        assert!(narrative.contains("Step 1 - work_upload r1 [deprecated]"),"{narrative}");
        assert!(narrative.contains("No Worker return recorded."),"{narrative}");
        assert!(narrative.contains("Step 2 - work_reopen r1 [running]"),"{narrative}");
        let path=input["execution_path"].as_array().unwrap();
        assert_eq!(path[0]["valid_input"],false);
        assert_eq!(path[0]["deprecation"]["reason"],"the upload page changed; reopen first");

        let mut dependent=task("after","Use the upload");
        dependent.dependency_inputs=vec![json!({"work_id":"upload"})];
        let error=scheduler.apply(&json!({"action":"work","reason":"x","orders":[dependent]}),false,false).unwrap_err();
        assert!(error.to_string().contains("TASK_NOT_RETURNED")||error.to_string().contains("TASK_DEPRECATED"),"{error}");
        let mut resolver=task("later","Use the upload");resolver.dependency_inputs=vec![json!({"work_id":"upload"})];
        let error=normalize_dependencies(&scheduler.frames,&mut resolver).unwrap_err().to_string();
        assert!(error.contains("TASK_DEPRECATED"),"{error}");
        let mut revision=task("later","Use reopen");revision.dependency_inputs=vec![json!({"node_id":"work_reopen","revision":7})];
        assert!(scheduler.resolve_dependency_deliveries(&revision).unwrap_err().contains("REVISION_MISMATCH"));
    }

    #[test]
    fn narrative_preserves_worker_returns_and_dated_http_facts_without_inferred_recovery() {
        let url="http://127.0.0.1:38190/api/fonts";
        let mut scheduler=WorkScheduler::default();
        let mut probe=task("probe","Probe the font service");probe.completion=Completion::Check;
        probe.checks=vec![crate::http_probe::check_key(&json!({"url":url}))];
        scheduler.enqueue(vec![probe],false,true).unwrap();
        scheduler.activate_next().unwrap();
        scheduler.observe("http_probe",&json!({"url":url}),&http_sample(url,None,4),false);
        scheduler.return_work(&json!({"summary":"Connection refused; no HTTP response",
            "limitations":[format!("Font endpoint {url} returned connection refused at this step")]})).unwrap();

        let mut start=task("start","Start the font service");start.completion=Completion::Check;
        start.checks=vec![crate::http_probe::check_key(&json!({"url":url}))];
        scheduler.apply(&json!({"action":"work","reason":"service is down","orders":[start]}),false,true).unwrap();
        let processes=json!([{"process_id":"proc-1","script":"dev","running":true,"ready":true,"ready_url":"http://127.0.0.1:38190/"}]);
        scheduler.frame_mut().unwrap().project_observation=json!({"sampled_at":now_ms()-2000,"processes":processes});
        scheduler.observe("http_probe",&json!({"url":url}),&http_sample(url,Some(200),1),false);
        assert_eq!(scheduler.frame().unwrap().status,WorkStatus::Running,"a successful host observation does not end the Worker invocation");
        assert!(scheduler.output().is_none());
        let worker_summary="Worker reports that the font endpoint returned HTTP 200";
        scheduler.return_work(&json!({"summary":worker_summary})).unwrap();
        assert_eq!(scheduler.output().unwrap()["summary"],worker_summary,"host observations do not rewrite the Worker return");

        let input=scheduler.organizer_input();
        let narrative=input["execution_narrative"].as_str().unwrap();
        let first=narrative.find("Step 1 - work_probe").unwrap();
        let second=narrative.find("Step 2 - work_start").unwrap();
        assert!(first<second,"{narrative}");
        assert!(narrative.contains("Connection refused"),"{narrative}");
        assert!(!narrative.contains("resolved")&&!narrative.contains("superseded by the later sample")
            &&!narrative.contains("Latest known facts:")&&!narrative.contains("Remaining limitations:"),"no inferred recovery or limitations synthesis: {narrative}");
        let facts=input["current_facts"]["http_observations"].as_array().unwrap();
        assert_eq!(facts.len(),2);
        assert_eq!(facts[0]["http_status"],Value::Null);
        assert_eq!(facts[1]["http_status"],200);
        assert!(facts[0]["text"].as_str().unwrap().contains("no HTTP response"));
        assert!(facts[1]["text"].as_str().unwrap().contains("HTTP 200 at"));
        assert!(facts.iter().all(|fact|fact.get("relation_to_service_start").is_none()));
    }

    #[test]
    fn http_history_keeps_timestamps_without_inferencing_a_service_start_relation() {
        let url="http://127.0.0.1:8080/api/fonts";
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![task("probe","Probe the font endpoint")],false,false).unwrap();scheduler.activate_next().unwrap();
        let earlier=now_ms().saturating_sub(5_000);
        scheduler.frame_mut().unwrap().project_observation=json!({"sampled_at":earlier,"processes":[
            {"process_id":"ui-3000","running":true,"ready":true,"ready_url":"http://127.0.0.1:3000/","ready_port":3000}]});
        scheduler.observe("http_probe",&json!({"url":url}),&http_sample(url,None,4),false);
        scheduler.return_work(&json!({"summary":"Font endpoint refused before its service was started"})).unwrap();

        let mut start=task("start_fonts","Start the font service");start.completion=Completion::Check;
        start.checks=vec![crate::http_probe::check_key(&json!({"url":url}))];
        scheduler.apply(&json!({"action":"work","reason":"start the exact font service","orders":[start]}),false,true).unwrap();
        let ready_at=now_ms().saturating_sub(1_000);
        scheduler.frame_mut().unwrap().project_observation=json!({"sampled_at":ready_at,"processes":[
            {"process_id":"ui-3000","running":true,"ready":true,"ready_url":"http://127.0.0.1:3000/","ready_port":3000},
            {"process_id":"fonts-8080","running":true,"ready":true,"ready_url":"http://127.0.0.1:8080/","ready_port":8080}]});
        scheduler.observe("http_probe",&json!({"url":url}),&http_sample(url,Some(200),0),false);

        let input=scheduler.organizer_input();
        let history=input["current_facts"]["http_observations"].as_array().unwrap();
        assert_eq!(history.len(),2);
        assert_eq!(history[1]["http_status"],200);
        assert!(history[1]["text"].as_str().unwrap().contains("HTTP 200 at"));
        assert!(history.iter().all(|sample|sample.get("relation_to_service_start").is_none()));
        let failed=scheduler.http_observation_samples.values().find(|sample|sample["http_status"].is_null()).unwrap();
        assert_eq!(history[0]["sampled_at"],failed["sampled_at"],"the original probe time stays attached to the failure");
    }

    #[test]
    fn current_page_state_survives_a_deprecated_producer_and_stale_old_receipt() {
        let mut scheduler=WorkScheduler::default();
        let page=|session:&str,epoch:u64|json!({"browser_session_id":session,"page_id":"p1","page_epoch":epoch,"url":"http://127.0.0.1:5173/"});
        let receipt=|attempt:&str,session:&str,work:&str|BrowserUploadEvidence{upload_attempt_id:attempt.into(),path:"decks/demo.pptx".into(),
            change_event_received:true,page:page(session,1),file:json!({"name":"demo.pptx"}),work_id:work.into(),node_id:format!("work_{work}"),revision:1,load_attempt_required:true};
        let mut old=task("old_upload","Upload in the first session");
        let mut current=task("upload","Upload in the current session");
        old.id="old_upload".into();current.id="upload".into();
        scheduler.frames.insert("old_upload".into(),WorkFrame{order:old,status:WorkStatus::Done,sequence:1,
            browser_upload_receipt:Some(receipt("a1","s-old","old_upload")),output:Some(json!({"summary":"uploaded"})),..Default::default()});
        scheduler.frames.insert("upload".into(),WorkFrame{order:current,status:WorkStatus::Running,sequence:2,invalidated_by_plan_revision:Some(1),
            browser_upload_receipt:Some(receipt("a2","s-new","upload")),..Default::default()});
        let mut inspect=task("inspect","Read the loaded deck");inspect.id="inspect".into();
        scheduler.frames.insert("inspect".into(),WorkFrame{order:inspect,status:WorkStatus::Done,sequence:3,output:Some(json!({"summary":"8 slides"})),
            browser_current_read:Some(json!({"matched":true,"expect_text":"8","page":page("s-new",1),"observed_at":now_ms(),"work_id":"inspect",
                "document_loaded":{"status":"loaded","slides_detected":8,"page_indicator":["1 / 8"],"upload_attempt_id":"a2"}})),..Default::default()});
        scheduler.active_browser_uploads.insert(browser_page_key(&page("s-new",1)).unwrap(),"a2".into());
        let key=|attempt:&str,session:&str|browser_upload_status_key(&serde_json::to_value(receipt(attempt,session,"x")).unwrap()).unwrap();
        scheduler.browser_upload_availability.insert(key("a1","s-old"),json!({"available":false,"reason":"browser_session_changed"}));
        scheduler.browser_upload_availability.insert(key("a2","s-new"),json!({"available":true,"reason":"active_session_page_and_upload_attempt_match","page_epoch":1}));

        let input=scheduler.organizer_input();
        let state=&input["current_facts"]["browser"][0];
        assert_eq!(state["document_loaded"]["slides_detected"],8);
        assert_eq!(state["uploaded_by"]["producer_deprecated"],true);
        assert_eq!(state["read_by_work_id"],"inspect");
        let receipts=input["browser_upload_availability"].as_array().unwrap();
        let stale=receipts.iter().find(|item|item["upload_attempt_id"]=="a1").unwrap();
        assert_eq!(stale["historical"],true,"the old session's receipt describes only itself");
        assert!(receipts.iter().any(|item|item["upload_attempt_id"]=="a2"&&item["historical"]==false));
        let narrative=input["execution_narrative"].as_str().unwrap();
        assert!(narrative.contains("document=decks/demo.pptx, load_status=loaded, slides=8"),"{narrative}");
        assert!(narrative.contains("session_available=true"),"{narrative}");

        let mut wrong=task("bad","Inspect fonts");wrong.browser_document_path=Some("http://127.0.0.1:5173/editor".into());
        let error=scheduler.enqueue(vec![wrong],false,false).unwrap_err().to_string();
        assert!(error.contains("browser_document_path")&&error.contains("page URL"),"{error}");
    }

    #[test]
    fn work_can_combine_network_and_visual_goals_and_unavailable_image_does_not_permanently_block_retry() {
        let mut fonts=task("fonts","Read the browser font requests to /api/fonts");fonts.visual_goal=Some("see fonts".into());
        let mut compatible=WorkScheduler::default();
        compatible.enqueue(vec![fonts],false,false).unwrap();

        let mut scheduler=WorkScheduler::default();
        let mut look=task("look","Capture the editor and check the rendered slide");look.visual_goal=Some("The first slide is rendered".into());
        scheduler.enqueue(vec![look.clone()],false,false).unwrap();scheduler.activate_next().unwrap();
        let returned=scheduler.return_work(&json!({"summary":"Screenshot captured; image input unavailable","limitations":["visual inspection not performed: image input unavailable"],
            "visual_check_result":{"assessment":"unavailable"}})).unwrap();
        assert!(returned.get("expectation_met").is_none());
        let mut again=look;again.id="look_again".into();
        scheduler.enqueue(vec![again],false,false).unwrap();
        assert!(scheduler.activate_next().unwrap());
        assert_eq!(scheduler.current,"look_again");
        assert_eq!(scheduler.frames["look"].status,WorkStatus::Done,"the unavailable visual return remains a sealed historical invocation");
    }

    #[test]
    fn invalidated_http_sample_allows_a_new_probe_and_reports_the_previous_one() {
        let url="http://127.0.0.1:38192/api/fonts";
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![task("probe","Probe fonts")],false,false).unwrap();scheduler.activate_next().unwrap();
        scheduler.observe("http_probe",&json!({"url":url}),&http_sample(url,None,1),false);
        assert!(scheduler.reusable_http_probe(&json!({"url":url})).is_some(),"a fresh sample is reused instead of probing again");
        let id=scheduler.frame().unwrap().http_observation_ids[0].clone();
        scheduler.http_observation_samples.get_mut(&id).unwrap()["sampled_at"]=
            json!((chrono::Utc::now()-chrono::Duration::seconds(120)).to_rfc3339_opts(chrono::SecondsFormat::Millis,true));
        scheduler.permits("http_probe",&json!({"url":url})).unwrap();
        let mut fresh=http_sample(url,Some(200),0);
        scheduler.record_http_probe(&json!({"url":url}),&mut fresh,"call-2");
        assert_eq!(fresh["previous_sample"]["cache_invalidation_reason"],"expired");
        assert_eq!(fresh["previous_sample"]["http_status"],Value::Null);
        assert_eq!(fresh["previous_sample"]["fresh_for_cache_reuse"],false);
        assert_eq!(fresh["fresh_for_cache_reuse"],true);
        let reused=scheduler.reusable_http_probe(&json!({"url":url})).unwrap();
        assert_eq!(reused["reused"],true);assert!(reused["reuse_age_ms"].is_u64());
    }

    #[test]
    fn document_path_rejects_page_urls() {
        let mut scheduler=WorkScheduler::default();
        let mut wrong=task("bad","Inspect fonts");wrong.browser_document_path=Some("http://127.0.0.1:5173/editor".into());
        let error=scheduler.enqueue(vec![wrong],false,false).unwrap_err().to_string();
        assert!(error.contains("browser_document_path")&&error.contains("page URL"),"{error}");
    }
}
