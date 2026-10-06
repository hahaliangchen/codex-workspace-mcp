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
    #[serde(default)]
    pub next_browser_upload_id: u64,
    /// Recent same-URL HTTP samples are shared across work packets in this active request.
    #[serde(default)]
    pub http_probe_results: BTreeMap<String, Value>,
    #[serde(default)]
    pub http_probe_generation: u64,
}

impl Default for WorkScheduler {
    fn default() -> Self {
        Self {
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
            next_browser_upload_id: 0,
            http_probe_results: BTreeMap::new(),
            http_probe_generation: 0,
        }
    }
}

fn limited(s: &str, count: usize) -> String { s.chars().take(count).collect() }
fn path(s: &str) -> String { s.replace('\\',"/") }
fn command(s: &str) -> String { s.trim().replace("\r\n","\n") }
fn browser_paths_equal(left:&str,right:&str)->bool {
    let left=path(left);let right=path(right);
    if cfg!(windows) {left.eq_ignore_ascii_case(&right)} else {left==right}
}
fn browser_page_key(page:&Value)->Option<String> {
    Some(format!("{}\u{1f}{}",page["browser_session_id"].as_str()?,page["page_id"].as_str()?))
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

fn declared_dependency_frame<'a>(frames: &'a BTreeMap<String, WorkFrame>, dep: &Value, validate_revision: bool) -> Result<&'a WorkFrame, String> {
    let revision = dep.get("revision").and_then(Value::as_u64).map(|r| r as usize);
    let (target, label) = if let Some(id) = dep.as_str() {
        (DependencyTarget::Alias(id), id)
    } else if let Some(id) = dep["work_id"].as_str().filter(|id| !id.is_empty()) {
        (DependencyTarget::Work(id), id)
    } else if let Some(node) = dep["node_id"].as_str().filter(|id| !id.is_empty()) {
        (DependencyTarget::Node(node), node)
    } else {
        return Err("dependency input needs a work_id or node_id".into());
    };
    let frame = dependency_frame(frames, target, revision)
        .or_else(|| if validate_revision { None } else { dependency_frame(frames, target, None) })
        .ok_or_else(|| format!("upstream dependency '{label}' at revision {revision:?} has no active frame in plan"))?;
    if let Some(node) = dep["node_id"].as_str().filter(|id| !id.is_empty()) {
        if node != frame.order.node_id { return Err(format!("upstream work '{label}' does not belong to node '{node}'")); }
    }
    Ok(frame)
}

fn normalize_dependencies(frames: &BTreeMap<String, WorkFrame>, order: &mut WorkOrder) -> Result<()> {
    normalize_dependencies_at(frames,order,"orders[0]")
}

fn normalize_dependencies_at(frames: &BTreeMap<String, WorkFrame>, order: &mut WorkOrder, order_path:&str) -> Result<()> {
    for up in &mut order.upstream_ids {
        let frame = dependency_frame(frames, DependencyTarget::Alias(up), None)
            .ok_or_else(|| anyhow::anyhow!("field_path={order_path}.upstream_ids: unknown or invalidated exact upstream work_id '{up}'"))?;
        *up = frame.order.id.clone();
    }
    for (index,dep) in order.dependency_inputs.iter_mut().enumerate() {
        let frame = declared_dependency_frame(frames, dep, false).map_err(|error|{
            let field=if error.contains("revision")&&dep["revision"].is_number(){"revision"}else if error.contains("does not belong")&&dep["node_id"].is_string(){"node_id"}else if dep["work_id"].is_string(){"work_id"}else if dep["node_id"].is_string(){"node_id"}else{"work_id"};
            anyhow::anyhow!("field_path={order_path}.dependency_inputs[{index}].{field}: {error}")
        })?;
        let id = frame.order.id.clone();
        if dep.is_string() { *dep = json!({"work_id": id}); }
        else { dep["work_id"] = json!(id); }
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
    pub fn done(&self) -> bool { self.frame().is_some_and(|f| f.status == WorkStatus::Done) }
    pub fn current_process(&self) -> Option<&WorkFrame> { self.frame() }
    pub fn current_process_mut(&mut self) -> Option<&mut WorkFrame> { self.frame_mut() }
    pub fn needs_organizer(&self) -> bool { !self.finished && (self.handoff.is_some() || self.current.is_empty() || self.done()) }
    pub fn selection(&self) -> Value { self.frame().map(|f| f.source_selection.clone()).unwrap_or(json!({})) }
    pub fn save_selection(&mut self, selection: Value) { if let Some(f) = self.frames.get_mut(&self.current) { f.source_selection = selection; } }
    pub fn set_goal_boundary(&mut self, boundary: Value) { self.goal_boundary = boundary; }
    pub fn snapshot(&self) -> Value { serde_json::to_value(self).unwrap_or(json!({})) }

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
                    "completed"
                } else if self.finished || f.output.as_ref().is_some_and(|out| out["blocked"] == true) {
                    "blocked"
                } else if f.status == WorkStatus::Running {
                    "running"
                } else {
                    "pending"
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
            let requires_pptx=o.constraints.iter().any(|constraint|constraint=="requires_pptx");
            if requires_pptx {
                ensure!(o.completion==Completion::Output,"field_path=orders[{index}].completion: requires_pptx needs completion=output");
                let expected=o.browser_document_path.as_deref().filter(|path|!path.trim().is_empty()).ok_or_else(||anyhow::anyhow!("field_path=orders[{index}].browser_document_path: requires_pptx needs the exact workspace-relative target file path"))?;
                let normalized=path(expected.trim());
                ensure!(normalized.chars().count()<=2000&&!normalized.starts_with('/')&&!normalized.contains(':')&&!normalized.split('/').any(|part|part==".."||part==".")&&normalized.to_ascii_lowercase().ends_with(".pptx"),"field_path=orders[{index}].browser_document_path: target must be a workspace-relative .pptx file path without traversal segments");
                o.browser_document_path=Some(normalized);
            } else if let Some(expected)=o.browser_document_path.as_deref() {
                o.browser_document_path=Some(path(expected.trim()));
            }
            let visual = o.visual_goal.is_some() || o.constraints.iter().any(|s| s == "requires_visual");
            ensure!(!visual || o.completion == Completion::Output, "visual verification requires a separate output work order; write/check work seals after its declared operations");
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
            let writing = matches!(o.completion, Completion::Write | Completion::WriteCheck);
            let checking = matches!(o.completion, Completion::Check | Completion::WriteCheck);
            ensure!(!(writing || !o.edit_targets.is_empty()) || can_write, "write work is unavailable under current permissions");
            ensure!(!checking || can_check, "checks are unavailable under current tool permissions");
            ensure!(!writing || !o.edit_targets.is_empty(), "write work needs explicit target files");
            ensure!(!checking || (!o.checks.is_empty() && o.checks.iter().all(|s| !s.is_empty() && s.chars().count() <= 2000)), "checked work needs specific authorized commands");
            ensure!(checking || o.checks.is_empty(), "checks must use check/write_check completion");

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

    pub fn resolve_dependency_deliveries(&self, order: &WorkOrder) -> Result<Vec<Value>, String> {
        let mut results = Vec::new();
        let mut processed_targets = BTreeSet::new();

        // 1. Process explicit dependency_inputs with field and revision constraints
        for (index,dep) in order.dependency_inputs.iter().enumerate() {
            let req_rev = dep.get("revision").and_then(Value::as_u64).map(|r| r as usize);
            let reference_field=if dep["work_id"].is_string(){"work_id"}else if dep["node_id"].is_string(){"node_id"}else{"work_id"};
            let frame = declared_dependency_frame(&self.frames, dep, true)
                .map_err(|error|format!("field_path=orders[0].dependency_inputs[{index}].{reference_field}: {error}"))?;

            if frame.status != WorkStatus::Done {
                return Err(format!("field_path=orders[0].dependency_inputs[{index}].{reference_field}: work_id='{}' node_id='{}' revision={} is not completed (status: {:?})",
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
                    return Err(format!("field_path=orders[0].dependency_inputs[{index}].revision: work_id='{}' node_id='{}' requested revision {}, actual delivery is revision {}",
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
                        if let Some(val) = output.pointer(&format!("/exported_data/{field_name}")).or_else(|| output.get(field_name)) {
                            filtered[field_name] = val.clone();
                            exported[field_name] = val.clone();
                        } else {
                            missing.push(field_name.to_string());
                        }
                    }
                }
                if !missing.is_empty() {
                    return Err(format!("field_path=orders[0].dependency_inputs[{index}].fields: work_id='{}' node_id='{}' revision={} is missing required delivery fields: {}; available output fields: [{}]; available exported_data fields: [{}]",
                        frame.order.id,frame.order.node_id,frame.order.revision,missing.join(", "),
                        output.as_object().map(|fields|fields.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default(),
                        output["exported_data"].as_object().map(|fields|fields.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default()));
                }
                if !exported.as_object().unwrap().is_empty() {
                    filtered["exported_data"] = exported;
                }
            } else if let Some(exp) = output.get("exported_data") {
                filtered["exported_data"] = exp.clone();
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
                    return Err(format!("upstream dependency '{}' is not completed (status: {:?})", frame.order.id, frame.status));
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
                    deliverable["exported_data"] = exp.clone();
                }
                processed_targets.insert(frame.order.id.clone());
                results.push(deliverable);
            } else {
                return Err(format!("upstream dependency '{up_id}' has no active frame in plan"));
            }
        }

        Ok(results)
    }

    pub fn validate_dependency_inputs(&self, order: &WorkOrder) -> Result<(), String> {
        self.resolve_dependency_deliveries(order).map(|_| ())
    }

    pub fn activate_task(&mut self, target_id: &str) -> Result<bool> {
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
        self.handoff = None;
        Ok(true)
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

    pub fn finish_round(&mut self, project_operations: usize, repeated_reads: usize) {
        if self.finished { return; }
        let Some(frame) = self.frames.get_mut(&self.current) else { return; };
        frame.rounds += 1;
        frame.rounds_without_change += 1;
        frame.repeated_reads = if repeated_reads > 0 { frame.repeated_reads + repeated_reads } else { 0 };
        frame.idle_rounds = if project_operations == 0 { frame.idle_rounds + 1 } else { 0 };
        let reason = if frame.idle_rounds >= 1 {
            Some("Worker returned no actual project operation; decide a concrete operation or finish with its output")
        } else if frame.repeated_reads >= 3 {
            Some("Repeated reads returned no new information; use the current materials or narrow the missing question")
        } else if frame.rounds_without_change.saturating_sub(frame.reviewed_without_change) >= 12 {
            Some("This work unit ran twelve rounds without a changed file or a successful new check since its last organization; narrow the missing fact, split the problem or return a concrete blocker")
        } else {
            None
        };
        if let Some(reason) = reason { self.request_handoff(reason); }
    }

    pub fn all_done(&self) -> bool {
        !self.frames.is_empty() && self.queue.is_empty() && self.frames.values().filter(|f| f.invalidated_by_plan_revision.is_none()).all(|f| f.status == WorkStatus::Done)
    }

    pub fn finish(&mut self, summary: &str, blocked: bool) -> Result<()> {
        ensure!(!summary.trim().is_empty(), "finish requires an actual result or blocker summary");
        ensure!(blocked || self.all_done(), "unfinished work must be completed, resumed or reported blocked");
        self.finished = true;
        self.request_completed = Some(!blocked);
        if !blocked { self.handoff = None; }
        self.final_result = limited(summary, 6000);
        Ok(())
    }

    fn gate(f: &WorkFrame) -> bool {
        let written = !f.writes.is_empty() && f.order.edit_targets.iter().all(|p| f.writes.contains_key(&path(p)));
        let checked = f.check_errors.is_empty() && !f.order.checks.is_empty() && f.order.checks.iter().all(|c| f.checked.get(c) == Some(&f.epoch));
        let visual_required=f.order.visual_goal.is_some() || f.order.constraints.iter().any(|item|item=="requires_visual");
        if visual_required && !Self::visual_check_is_current(f) {return false;}
        match f.order.completion {
            Completion::Output => false,
            Completion::Write => written,
            Completion::Check => checked,
            Completion::WriteCheck => written && checked,
        }
    }

    fn page_matches(left:&Value,right:&Value)->bool {
        left["browser_session_id"].as_str().is_some()&&left["page_id"].as_str().is_some()&&left["page_epoch"].as_u64().is_some()
            &&left["browser_session_id"]==right["browser_session_id"]&&left["page_id"]==right["page_id"]&&left["page_epoch"]==right["page_epoch"]
    }

    fn same_browser_document(left:&Value,right:&Value)->bool {
        !left.is_null()&&!right.is_null()&&left["browser_session_id"]==right["browser_session_id"]&&left["page_id"]==right["page_id"]&&left["url"]==right["url"]
    }

    fn latest_current_browser_read(f:&WorkFrame)->Option<&Value> {
        f.browser_current_read.as_ref().filter(|read|read["matched"]==true
            &&read["expect_text"].as_str().is_some_and(|text|!text.trim().is_empty())
            &&Self::page_matches(&read["page"],&f.browser_page))
    }

    fn browser_upload_receipt_for(&self,f:&WorkFrame)->Option<BrowserUploadEvidence> {
        let is_active=|receipt:&BrowserUploadEvidence|browser_page_key(&receipt.page).is_some_and(|key|self.active_browser_uploads.get(&key)==Some(&receipt.upload_attempt_id));
        if let Some(receipt)=f.browser_upload_receipt.as_ref().filter(|receipt|is_active(receipt)) {return Some(receipt.clone());}
        for dep in &f.order.dependency_inputs {
            let fields=dep.get("fields").and_then(Value::as_array);
            if fields.is_some_and(|fields|!fields.iter().any(|field|field.as_str()==Some("browser_upload_receipt"))) {continue;}
            let Ok(source)=declared_dependency_frame(&self.frames,dep,true) else {continue;};
            if source.status!=WorkStatus::Done {continue;}
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

    fn visual_check_is_current(f:&WorkFrame)->bool {
        let result=&f.visual_check_result;let binding=&result["source_binding"];
        !result.is_null() && matches!(result["assessment"].as_str(),Some("pass"|"issue"))
            && binding["execution_epoch"]==json!(f.epoch)
            && binding["related_source_versions"]==json!(f.versions)
            && binding["current_page"]==f.browser_page
            && result["current_result_artifact_ids"].as_array().is_some_and(|ids|!ids.is_empty())
    }

    pub fn verification_due(&self) -> bool {
        self.frame().is_some_and(|f| f.check_errors.is_empty() && (f.order.completion == Completion::Check || (f.order.completion == Completion::WriteCheck && !f.writes.is_empty() && f.order.edit_targets.iter().all(|p| f.writes.contains_key(&path(p))))))
    }

    pub fn reusable_http_probe(&self,args:&Value)->Option<Value> {
        if args["reason"].as_str().is_some_and(|reason|!reason.trim().is_empty()) {return None;}
        let check=crate::http_probe::check_key(args);
        let observation=self.http_probe_results.get(&check)?;
        if observation["generation"].as_u64()!=Some(self.http_probe_generation) {return None;}
        let mut result=observation.get("result")?.clone();
        if !result.is_object() {return None;}
        if args["timeout_ms"].as_u64().is_some_and(|timeout|timeout.clamp(100,30_000)!=result["timeout_ms"].as_u64().unwrap_or(5_000)) {return None;}
        let sampled_at=chrono::DateTime::parse_from_rfc3339(result["sampled_at"].as_str()?).ok()?;
        let age=chrono::Utc::now().signed_duration_since(sampled_at.with_timezone(&chrono::Utc)).num_milliseconds();
        let age_ms=age.max(0) as u64;
        if age>=0 && age_ms>result["reuse_window_ms"].as_u64().unwrap_or(30_000) {return None;}
        result["reused"]=json!(true);
        result["reuse_age_ms"]=json!(age_ms);
        result["guidance"]=json!("A fresh result for this exact local URL was reused; use its sampled_at and response details instead of probing again.");
        Some(result)
    }

    fn may_edit(f: &WorkFrame) -> bool {
        matches!(f.order.completion, Completion::Write | Completion::WriteCheck) ||
            (f.order.completion==Completion::Output && f.order.visual_goal.is_some() && !f.order.edit_targets.is_empty()) ||
            (f.order.completion == Completion::Check && !f.order.edit_targets.is_empty() && !f.check_errors.is_empty())
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
            ensure!(previous.is_none()||reason||reusable,
                "this URL was already probed at {}; reuse that sample or include reason explaining why a new HTTP observation is needed",
                previous.and_then(|observation|observation.pointer("/result/sampled_at")).and_then(Value::as_str).unwrap_or("an earlier time"));
        }
        if crate::project_process::is_check(name) && name != "get_project_process" {
            let c = crate::project_process::check_key(name, args);
            if self.verification_due() {
                ensure!(f.order.checks.contains(&c), "verification work accepts only its declared check commands");
            }
            let http_refresh=name=="http_probe"&&args["reason"].as_str().is_some_and(|reason|!reason.trim().is_empty());
            let http_reuse=name=="http_probe"&&self.reusable_http_probe(args).is_some();
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
            let receipt_page=json!({"browser_session_id":page["browser_session_id"],"page_id":page["page_id"],"page_epoch":page["page_epoch"],"url":page["url"]});
            let receipt_file=json!({"name":result["file"]["name"],"size_bytes":result["file"]["size_bytes"],"files_length":result["file"]["files_length"]});
            let receipt=BrowserUploadEvidence{upload_attempt_id:attempt_id.clone(),path:path(result["uploaded"].as_str().unwrap()),change_event_received:true,page:receipt_page,file:receipt_file,
                work_id:existing.order.id.clone(),node_id:existing.order.node_id.clone(),revision:existing.order.revision,load_attempt_required:supplied_id.is_some()};
            if let Some(key)=browser_page_key(&page) {self.active_browser_uploads.insert(key,attempt_id);}
            Some(receipt)
        } else {None};
        if matches!(name,"run_command"|"install_dependencies"|"run_project_script"|"stop_project_process"|"write_file"|"replace_range"|"edit_file") {
            self.http_probe_generation=self.http_probe_generation.saturating_add(1);
        }
        if name=="http_probe" {
            let key=crate::http_probe::check_key(args);
            self.http_probe_results.insert(key,json!({"generation":self.http_probe_generation,"result":result}));
            while self.http_probe_results.len()>64 {
                if let Some(oldest)=self.http_probe_results.keys().next().cloned() {self.http_probe_results.remove(&oldest);} else {break;}
            }
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
        if name=="browser_read"&&!failed&&result["matched"]==true&&result["expect_text"].as_str().is_some_and(|text|!text.trim().is_empty()) {
            f.browser_current_read=Some(json!({"matched":result["matched"],"expect_text":result["expect_text"],"page":page,"document_loaded":result["document_loaded"],"text":result["text"]}));
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
                    f.current_visual_artifact_ids.clear();
                    f.visual_check_result=Value::Null;
                }
                f.versions.insert(file.clone(), hash.to_owned());
                if matches!(name, "edit_file" | "replace_range" | "write_file") && result["changed"] == true {
                    f.epoch += 1;
                    f.checked.clear();
                    f.rounds_without_change = 0;
                    f.reviewed_without_change = 0;
                    f.writes.insert(file, json!({"code_hash": hash, "changes": result["changes"], "material": result["notebook_material"]}));
                    f.current_visual_artifact_ids.clear();
                    f.visual_check_result=Value::Null;
                }
            }
        }
        if crate::project_process::is_check(name) {
            let check = if name == "run_command" {
                crate::project_process::check_key(name, args)
            } else {
                result["check_key"].as_str().map(str::to_owned).unwrap_or_else(|| crate::project_process::check_key(name, args))
            };
            if f.order.checks.contains(&check) {
                let service = result["background"] == true && result["operation"] == "script";
                let succeeded = if name=="http_probe" {
                    result["check_passed"]==true
                } else if service {
                    result["running"] == true && result["ready"] == true
                } else {
                    let exit_code=if name=="run_command" {result["process_exit_code"].as_i64()} else {result["status"].as_i64()};
                    exit_code == Some(0) && result["running"] != true && result["command_result_reports_failure"] != true
                };
                let pending = !failed && result["running"] == true && result["verification_inputs_changed"] != true && !succeeded;
                if !failed && succeeded && result["verification_inputs_changed"] != true {
                    f.checked.insert(check.clone(), f.epoch);
                    f.check_errors.remove(&check);
                    f.rounds_without_change = 0;
                    f.reviewed_without_change = 0;
                } else if !pending {
                    f.checked.remove(&check);
                    f.check_errors.insert(check, json!({
                        "exit_code": if name=="run_command" {result["process_exit_code"].clone()} else {result["status"].clone()},
                        "source_epoch": f.epoch,
                        "inputs_changed": result["verification_inputs_changed"] == true,
                        "error": result["error"].as_str().map(|s| limited(s, 2000)),
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
            "command": if name == "run_command" { args.get("command") } else { None },
            "failed": failed,
            "exit_code": if name=="run_command" {result["process_exit_code"].clone()} else {result["status"].clone()},
            "command_result": if name=="run_command" {result["command_result"].clone()} else {Value::Null},
            "command_result_reports_failure":result["command_result_reports_failure"],
            "changed": result["changed"],
            "changes": result["changes"],
            "material": result["notebook_material"],
            "process_id": result["process_id"],
            "project_path": result["project_path"],
            "script": result["script"],
            "process_observation":result["process_observation"],
            "failure_stage":result["failure_stage"],
            "diagnosis":result["diagnosis"],
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
        if versions != &f.versions {
            f.epoch += 1;
            f.checked.clear();
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

    pub fn close_if_satisfied(&mut self) -> bool {
        let work_id=self.current.clone();
        let Some(snapshot)=self.frames.get(&work_id).cloned() else {return false;};
        if snapshot.status==WorkStatus::Done||!Self::gate(&snapshot) {return false;}
        let requires_pptx=snapshot.order.constraints.iter().any(|item|item=="requires_pptx");
        let requires_browser=requires_pptx||snapshot.order.constraints.iter().any(|item|item=="requires_browser");
        if requires_browser&&Self::latest_current_browser_read(&snapshot).is_none() {return false;}
        if requires_pptx&&!self.browser_presentation_loaded(&snapshot) {return false;}
        let Some(f)=self.frames.get_mut(&work_id) else {return false;};
        f.status = WorkStatus::Done;
        let mut exported_data=json!({});
        if !f.project_observation.is_null() {
            let observation=f.project_observation.clone();
            exported_data=json!({"project_observation":observation,
                "project_path":observation.pointer("/scope/project_path").cloned().unwrap_or(Value::Null),
                "script":observation.pointer("/scope/script").cloned().unwrap_or(Value::Null),
                "ready_url":observation.pointer("/scope/ready_url").cloned().unwrap_or(Value::Null),
                "ready_port":observation.pointer("/scope/ready_port").cloned().unwrap_or(Value::Null),
                "process_id":observation.pointer("/processes/0/process_id").cloned().unwrap_or(Value::Null)});
        }
        Self::attach_active_browser_upload(&self.active_browser_uploads,f,&mut exported_data);
        let output = json!({
            "id": f.order.id,
            "node_id": f.order.node_id,
            "revision": f.order.revision,
            "plan_revision": f.order.plan_revision,
            "goal": f.order.goal,
            "done": true,
            "outcome": "completed",
            "summary": format!("Completed declared {:?} conditions for {}", f.order.completion, f.order.goal),
            "modified_files": f.writes,
            "checks": f.checked.keys().collect::<Vec<_>>(),
            "exported_data":exported_data,
            "project_observation":f.project_observation,
            "versions": f.versions,
            "operations": f.operations
        });
        f.output = Some(output.clone());
        self.handoff = Some(output);
        true
    }

    pub fn return_work(&mut self, args: &Value) -> Result<Value> {
        ensure!(args.to_string().len() <= 32_000, "work return is too large; return conclusions and material IDs");
        let summary = args["summary"].as_str().unwrap_or("").trim();
        ensure!(!summary.is_empty(), "yield_work requires actual findings/outcome or a concrete blocker");
        let frame_id=self.current.clone();
        let f = self.frames.get(&frame_id).cloned().ok_or_else(|| anyhow::anyhow!("no active work"))?;
        let is_upstream_prob = args["outcome"].as_str() == Some("upstream_problem") || args.get("upstream_problem").is_some();
        let is_blocked = args["blocked"] == true || args["outcome"].as_str() == Some("blocked");
        let is_need_split = args["need_split"] == true || args["outcome"].as_str() == Some("need_split");
        let done = !is_upstream_prob && (f.status == WorkStatus::Done || (f.order.completion == Completion::Output && !is_blocked && !is_need_split));
        if done && (f.order.visual_goal.is_some() || f.order.constraints.iter().any(|item|item=="requires_visual")) {
            ensure!(Self::visual_check_is_current(&f),
                "visual work needs a current, source-bound pass/issue result that cites a current image; recapture after page or source changes");
        }
        let requires_pptx=f.order.constraints.iter().any(|item|item=="requires_pptx");
        let requires_browser=requires_pptx||f.order.constraints.iter().any(|item|item=="requires_browser");
        if done&&requires_browser {
            ensure!(Self::latest_current_browser_read(&f).is_some(),"browser work stays unfinished until browser_read matches a nonempty expected phrase on the current page after its latest navigation or interaction; return the address, example file and remaining manual step as blocked");
        }
        if done&&requires_pptx {
            ensure!(self.browser_presentation_loaded(&f),"PPTX work stays unfinished until the exact browser_document_path has an active upload receipt (from this node or an explicitly declared dependency) and a current-page read confirms that same load attempt with valid slides and page numbers; return the parser blocker as blocked");
        }
        if done {self.frames.get_mut(&frame_id).unwrap().status=WorkStatus::Done;}
        let outcome = if is_upstream_prob {
            "upstream_problem"
        } else if is_blocked {
            "blocked"
        } else if is_need_split {
            "need_split"
        } else if done {
            "completed"
        } else {
            "running"
        };
        let mut exported_data=args.get("exported_data").cloned().unwrap_or_else(||json!({}));
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
            "outcome": outcome,
            "summary": limited(summary, 2400),
            "blocked": is_blocked,
            "need_split": is_need_split,
            "upstream_problem": args.get("upstream_problem"),
            "suggested_children": args["suggested_children"],
            "findings": args["findings"],
            "material_ids": args["material_ids"],
            "finding_ids": args["finding_ids"],
            "exported_data": exported_data,
            "project_observation":f.project_observation,
            "modified_files": f.writes,
            "checks": f.checked.keys().collect::<Vec<_>>(),
            "versions": f.versions,
            "operations": f.operations,
            "visual_artifact_ids":f.visual_artifact_ids,"visual_check_result":f.visual_check_result
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
            "done": self.done(),
            "organizer_handoff": self.frame().map(|f| &f.organizer_guidance),
            "verification_due": self.verification_due(),
            "actual_operations": self.frame().map(|f| &f.operations),
            "check_failures": self.frame().map(|f| &f.check_errors),
            "source_epoch": self.frame().map(|f| f.epoch),
            "outstanding_checks": self.order().map(|o| o.checks.iter().filter(|c| self.frame().unwrap().checked.get(*c) != Some(&self.frame().unwrap().epoch)).collect::<Vec<_>>()),
            "return_contract": "Do the current work using its smallest necessary materials. Use yield_work to return collected findings, a specific blocker, NeedSplit, or upstream_problem. Write/check completion is decided by the host's actual results. Do not replan, switch nodes, reopen completed work or announce readiness in separate rounds."
        })
    }

    pub fn organizer_input(&self) -> Value {
        let mut recent = self.frames.values().filter(|frame| frame.output.is_some()).collect::<Vec<_>>();
        recent.sort_by_key(|frame| std::cmp::Reverse(frame.sequence));
        let mut observations=self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none()&&!frame.project_observation.is_null())
            .map(|frame|json!({"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,
                "done":frame.status==WorkStatus::Done,"observation":frame.project_observation})).collect::<Vec<_>>();
        observations.sort_by_key(|item|std::cmp::Reverse(item["observation"]["sampled_at"].as_u64().unwrap_or(0)));
        let available_select_task_ids=self.available_select_task_ids();
        let available_revisit_targets=self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none())
            .map(|frame|frame.order.node_id.clone()).filter(|id|!id.is_empty()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        let immutable_completed_node_ids=self.frames.values().filter(|frame|frame.status==WorkStatus::Done&&frame.invalidated_by_plan_revision.is_none())
            .map(|frame|frame.order.node_id.clone()).filter(|id|!id.is_empty()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        let mut delivery_catalog=self.frames.values().filter(|frame|frame.status==WorkStatus::Done&&frame.invalidated_by_plan_revision.is_none())
            .filter_map(|frame|frame.output.as_ref().map(|output|{
                let exported_fields=output["exported_data"].as_object().map(|fields|fields.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
                let output_fields=output.as_object().map(|fields|fields.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
                json!({"work_id":frame.order.id,"node_id":frame.order.node_id,"revision":frame.order.revision,
                    "plan_revision":frame.order.plan_revision,"status":"done","summary":output["summary"],
                    "exported_fields":exported_fields,"output_fields":output_fields})
            })).collect::<Vec<_>>();
        delivery_catalog.sort_by(|a,b|a["work_id"].as_str().cmp(&b["work_id"].as_str()));
        let mut allowed_actions=vec!["work"];
        if self.frame().is_some_and(|frame|frame.status!=WorkStatus::Done&&frame.invalidated_by_plan_revision.is_none()) {allowed_actions.push("continue");}
        if !available_select_task_ids.is_empty() {allowed_actions.push("select");}
        if !available_revisit_targets.is_empty() {allowed_actions.push("revisit");}
        if self.all_done() {allowed_actions.push("finish");}
        allowed_actions.push("blocked");
        json!({
            "current_work": self.order(),
            "request_id":self.request_started_turn,
            "current_node": self.node(),
            "current_revision": self.revision(),
            "plan_revision": self.plan_revision,
            "handoff": self.handoff,
            "current_output": self.output(),
            "current_actual_operations": self.frame().map(|frame| &frame.operations),
            "current_round_count": self.frame().map(|frame| frame.rounds),
            "work_index": self.frames.values().map(|f| json!({
                "id": f.order.id,
                "node_id": f.order.node_id,
                "revision": f.order.revision,
                "plan_revision": f.order.plan_revision,
                "goal": f.order.goal,
                "done_when":f.order.done_when,
                "browser_document_path":f.order.browser_document_path,
                "status": f.status,
                "done": f.status == WorkStatus::Done,
                "deprecated": f.invalidated_by_plan_revision.is_some(),
                "invalidated_by_plan_revision": f.invalidated_by_plan_revision,
                "upstream_ids": f.order.upstream_ids,
                "dependency_inputs":f.order.dependency_inputs,
                "output_available":f.output.is_some()
            })).collect::<Vec<_>>(),
            "rewind_records": self.rewind_records,
            "recent_outputs": recent.iter().take(8).filter_map(|frame| frame.output.as_ref()).collect::<Vec<_>>(),
            "queue": self.queue,
            "available_select_task_ids":available_select_task_ids,
            "available_revisit_targets":available_revisit_targets,
            "immutable_completed_node_ids":immutable_completed_node_ids,
            "available_dependency_deliveries":delivery_catalog,
            "allowed_next_operations":allowed_actions,
            "project_process_observations":observations.into_iter().take(12).collect::<Vec<_>>()
        })
    }

    pub fn available_select_task_ids(&self) -> Vec<String> {
        let mut ids=self.queue.iter().filter(|id|self.frames.get(*id).is_some_and(|frame|frame.invalidated_by_plan_revision.is_none()&&frame.status!=WorkStatus::Done))
            .cloned().collect::<Vec<_>>();
        for frame in self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none()&&frame.status!=WorkStatus::Done) {
            if !ids.contains(&frame.order.id) {ids.push(frame.order.id.clone());}
        }
        ids
    }

    pub fn organizer_input_with_freshness(&self, root:&std::path::Path) -> Value {
        let mut input=self.organizer_input();
        if let Some(items)=input["project_process_observations"].as_array_mut() {
            for item in items {
                let observation=item["observation"].clone();
                item["fresh"]=json!(crate::project_process::observation_sample_is_current(root,&observation));
            }
        }
        input
    }

    pub fn reusable_project_observation(&self, root:&std::path::Path, args:&Value) -> Option<Value> {
        if !crate::project_process::can_reuse_process_observation(args) { return None; }
        self.frames.values().filter(|frame|frame.invalidated_by_plan_revision.is_none()&&!frame.project_observation.is_null()
            &&crate::project_process::observation_is_fresh(root,args,&frame.project_observation))
            .max_by_key(|frame|frame.project_observation["sampled_at"].as_u64().unwrap_or(0))
            .map(|frame|frame.project_observation.clone())
    }

    pub fn revisit(&mut self, target_node: &str, target_revision: Option<usize>, reason: &str, repair_goal: Option<&str>, can_write: bool, can_check: bool) -> Result<String> {
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

        let writing = matches!(new_order.completion, Completion::Write | Completion::WriteCheck);
        let checking = matches!(new_order.completion, Completion::Check | Completion::WriteCheck);
        ensure!(!(writing || !new_order.edit_targets.is_empty()) || can_write, "write work is unavailable under current permissions");
        ensure!(!checking || can_check, "checks are unavailable under current tool permissions");

        let sequence = self.frames.len() + 1;
        self.frames.insert(new_work_id.clone(), WorkFrame {
            order: new_order,
            sequence,
            status: WorkStatus::Running,
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
        let mut next = self.clone();
        next.apply_inner(decision, can_write, can_check)?;
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
                    if let Some(cur_frame) = self.frames.get_mut(&self.current) {
                        if cur_frame.invalidated_by_plan_revision.is_none() {
                            cur_frame.invalidated_by_plan_revision = Some(self.plan_revision);
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
                self.revisit(target_node, target_revision, reason, repair_goal, can_write, can_check)?;
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
        self.close_if_satisfied();
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
        self.close_if_satisfied();
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
    fn visual_contracts_require_output_and_expire_after_page_interaction() {
        let mut scheduler=WorkScheduler::default();let mut order=output_order();order.visual_goal=Some("confirm rendered slide".into());
        order.completion=Completion::WriteCheck;order.edit_targets=vec!["slide.ts".into()];order.checks=vec!["npm test".into()];
        assert!(scheduler.enqueue(vec![order.clone()],true,true).unwrap_err().to_string().contains("output"));
        order.completion=Completion::Output;order.checks.clear();scheduler.enqueue(vec![order],true,true).unwrap();scheduler.activate_next().unwrap();
        scheduler.observe("browser_open",&json!({}),&json!({"page":{"page_id":"p","page_epoch":1}}),false);
        scheduler.frames.get_mut(&scheduler.current).unwrap().visual_check_result=json!({"assessment":"pass"});
        let epoch=scheduler.frame().unwrap().epoch;
        scheduler.observe("browser_press_key",&json!({"key":"Delete"}),&json!({"page":{"page_id":"p","page_epoch":2}}),false);
        assert_eq!(scheduler.frame().unwrap().epoch,epoch+1);assert!(scheduler.frame().unwrap().visual_check_result.is_null());
        assert!(scheduler.return_work(&json!({"summary":"captured","status":"done"})).is_err());
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
    fn browser_work_cannot_complete_without_a_matching_read() {
        let mut scheduler = WorkScheduler::default();
        let mut order = output_order();
        order.constraints = vec!["requires_browser".into()];
        scheduler.enqueue(vec![order], false, false).unwrap();
        scheduler.activate_next().unwrap();
        assert!(scheduler.return_work(&json!({"summary":"opened the url"})).is_err());
        let old_page=json!({"browser_session_id":"s","page_id":"p","page_epoch":1,"url":"http://localhost:3000/"});
        let current_page=json!({"browser_session_id":"s","page_id":"p","page_epoch":2,"url":"http://localhost:3000/"});
        scheduler.observe("browser_open",&json!({}),&json!({"page":old_page}),false);
        scheduler.observe("browser_read", &json!({"expect_text":"Slide 1"}), &json!({"matched":false,"expect_text":"Slide 1","page":old_page}), false);
        assert!(scheduler.return_work(&json!({"summary":"page has no slides"})).is_err());
        scheduler.observe("browser_open",&json!({"url":"http://localhost:3000/other"}),&json!({"page":current_page}),false);
        assert!(scheduler.return_work(&json!({"summary":"old page match is stale"})).is_err());
        scheduler.observe("browser_read", &json!({"expect_text":"Slide 1"}), &json!({"matched":true,"expect_text":"Slide 1","page":current_page}), false);
        assert!(scheduler.return_work(&json!({"summary":"slide is visible"})).is_ok());
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
        assert!(scheduler.return_work(&json!({"summary":"welcome page opened"})).is_err());

        let upload=json!({"ok":true,"status":"file_assigned","uploaded":"samples/deck.pptx","input":{"change_event_received":true},"page":page(2)});
        scheduler.observe("browser_upload",&json!({"path":"samples/deck.pptx"}),&upload,false);
        assert!(scheduler.return_work(&json!({"summary":"file assigned"})).is_err());
        scheduler.observe("browser_wait",&json!({"document_loaded":true}),&json!({"ok":true,"status":"matched","document_loaded":true,"page":page(2)}),false);
        assert!(scheduler.return_work(&json!({"summary":"wait completed"})).is_err());

        let loaded_read=|epoch|json!({"matched":true,"expect_text":"Slide 1","slide_count":2,"page_indicator":["1 / 2"],
            "document_loaded":{"status":"loaded","slides_detected":2,"page_indicator":["1 / 2"],"loading_indicators":[]},"page":page(epoch)});
        scheduler.observe("browser_read",&json!({"expect_text":"Slide 1"}),&loaded_read(2),false);
        assert!(scheduler.return_work(&json!({"summary":"the real file is loaded"})).is_ok());

        let mut scheduler=WorkScheduler::default();let mut order=output_order();order.constraints=vec!["requires_pptx".into()];order.browser_document_path=Some("samples/deck.pptx".into());
        scheduler.enqueue(vec![order],false,false).unwrap();scheduler.activate_next().unwrap();
        scheduler.observe("browser_open",&json!({}),&json!({"page":page(1)}),false);
        scheduler.observe("browser_upload",&json!({}),&upload,false);
        scheduler.observe("browser_read",&json!({"expect_text":"Slide 1"}),&loaded_read(2),false);
        scheduler.observe("browser_open",&json!({"url":"http://localhost:3000/"}),&json!({"page":page(3)}),false);
        scheduler.observe("browser_read",&json!({"expect_text":"Slide 1"}),&loaded_read(3),false);
        assert!(scheduler.return_work(&json!({"summary":"reloaded page still has slides"})).is_err());
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
        assert!(!scheduler.done());

        // Organizer decides revisit to node_A
        let rev_result = scheduler.revisit("node_A", None, "backend API crashed, restart on 8080", Some("start backend on 8080"), false, false).unwrap();
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
        let new_work_id = scheduler.revisit("work_b", None, "change port configuration", None, false, false).unwrap();
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
        tree.complete_work(&json!({"node_id": "work_a", "done": true, "summary": "backend on 8080"})).unwrap();
        // Switch to work_b
        tree.apply(&json!({"current_node_id": "work_b"})).unwrap();

        // Revisit work_b in tree
        tree.revisit_node(&rewind.target_node, &rewind.invalidated_node_ids).unwrap();
        assert_eq!(tree.active(), "work_b");
        assert_eq!(tree.status("work_b"), Some("running"));
        // work_a was not invalidated, so it remains completed in tree
        assert_eq!(tree.status("work_a"), Some("completed"));
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
        let new_b = scheduler.revisit("work_b", None, "re-plan step 2 before execution", Some("improved step 2"), false, false).unwrap();
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
        let work=input["work_index"].as_array().unwrap().iter().find(|item|item["id"]=="start").unwrap();
        assert_eq!(work["node_id"],"start_backend");assert_eq!(work["revision"],1);assert_eq!(work["status"],"done");assert_eq!(work["output_available"],true);
        let delivery=&input["available_dependency_deliveries"][0];
        assert_eq!(delivery["work_id"],"start");assert_eq!(delivery["node_id"],"start_backend");assert_eq!(delivery["revision"],1);
        assert_eq!(delivery["exported_fields"],json!(["font_api","ready_url"]));
        assert!(input["allowed_next_operations"].as_array().unwrap().iter().any(|action|action=="finish"));
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
    }
}
