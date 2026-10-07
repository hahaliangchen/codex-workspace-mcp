//! Optional recursive work organization. Organizer plans; the host commits execution results.
use std::collections::{BTreeMap, BTreeSet};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskTree {
    /// Legacy snapshots omit this field and deserialize as version 0.
    #[serde(default)]
    schema_version: u32,
    nodes: BTreeMap<String, TaskNode>,
    active: String,
    source_sets: BTreeMap<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TaskNode {
    id: String,
    parent_id: Option<String>,
    title: String,
    kind: String,
    objective: String,
    done_when: String,
    #[serde(default)]
    constraints: Vec<String>,
    status: String,
    result: Option<Value>,
    #[serde(default)]
    pub history_results: Vec<Value>,
}

fn text(value: &Value, key: &str, max: usize) -> String {
    value[key].as_str().unwrap_or("").trim().chars().take(max).collect()
}
fn terminal(status: &str) -> bool { matches!(status,"completed"|"skipped"|"deprecated") }

impl TaskTree {
    pub fn enabled(&self) -> bool { !self.nodes.is_empty() }
    pub fn active(&self) -> &str { &self.active }
    pub fn focus(&self) -> &str {
        if !self.active.is_empty(){&self.active}else{self.nodes.values().find(|node|node.parent_id.is_none()).map_or("",|node|node.id.as_str())}
    }
    pub fn snapshot(&self) -> Value {
        let mut snapshot=serde_json::to_value(self).unwrap_or(json!({}));
        snapshot["schema_version"]=json!(1);
        snapshot
    }
    pub fn normalize_snapshot_version(&mut self) { self.schema_version=1; }
    pub fn remember_sources(&mut self, mut snapshot: Value, targets: &[String]) {
        snapshot["edit_targets"]=json!(targets);
        if !self.active.is_empty() {self.source_sets.insert(self.active.clone(),snapshot);}
    }
    pub fn sources(&self) -> Value {self.source_sets.get(&self.active).cloned().unwrap_or(json!({}))}
    pub fn parent_of(&self, id: &str) -> Option<&str> {self.nodes.get(id).and_then(|node| node.parent_id.as_deref())}
    pub fn edit_targets(&self) -> Vec<String> {
        self.sources()["edit_targets"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
    }
    pub fn ids(&self) -> BTreeSet<String> {self.nodes.keys().cloned().collect()}
    pub fn status(&self, id: &str) -> Option<&str> {self.nodes.get(id).map(|node|node.status.as_str())}
    /// Keep abandoned branches visible while removing them from aggregate readiness.
    pub fn deprecate_nodes(&mut self, ids: &[String]) -> Result<()> {
        let mut next = self.clone();
        let mut abandoned = ids.iter().filter(|id| next.nodes.contains_key(*id)).cloned().collect::<BTreeSet<_>>();
        loop {
            let descendants = next.nodes.values().filter(|node| node.parent_id.as_ref().is_some_and(|parent| abandoned.contains(parent)))
                .map(|node| node.id.clone()).collect::<Vec<_>>();
            let before = abandoned.len();
            abandoned.extend(descendants);
            if before == abandoned.len() { break; }
        }
        for id in &abandoned {
            let node = next.nodes.get_mut(id).unwrap();
            ensure!(node.parent_id.is_some(), "cannot abandon the user request goal");
            if let Some(result) = node.result.take() { node.history_results.push(result); }
            node.status = "deprecated".into();
        }
        if abandoned.contains(&next.active) {
            let mut parent = next.nodes[&next.active].parent_id.clone();
            while parent.as_ref().is_some_and(|id| abandoned.contains(id)) {
                parent = next.nodes[parent.as_ref().unwrap()].parent_id.clone();
            }
            next.active = parent.unwrap_or_default();
        }
        *self = next;
        Ok(())
    }
    pub fn path(&self) -> Vec<String> {
        if self.active.is_empty() {return vec![];}
        self.path_to(&self.active).unwrap_or_default()
    }
    fn path_to(&self, id: &str) -> Result<Vec<String>> {
        let mut path=Vec::new();let mut cursor=Some(id.to_owned());let mut seen=BTreeSet::new();
        while let Some(id)=cursor {
            ensure!(seen.insert(id.clone()),"task tree cannot contain a parent cycle");
            let node=self.nodes.get(&id).ok_or_else(||anyhow::anyhow!("unknown task node: {id}"))?;
            path.push(id);cursor=node.parent_id.clone();
        }
        path.reverse();Ok(path)
    }
    pub fn revisit_node(&mut self, target_node_id: &str, invalidated_nodes: &[String]) -> Result<()> {
        ensure!(self.nodes.contains_key(target_node_id), "unknown task node to revisit: {target_node_id}");
        let target = self.nodes.get_mut(target_node_id).unwrap();
        if let Some(res) = target.result.take() {
            target.history_results.push(res);
        }
        target.status = "running".to_string();
        self.active = target_node_id.to_string();

        let mut to_deprecate = BTreeSet::new();
        for inv_id in invalidated_nodes {
            ensure!(self.nodes.contains_key(inv_id), "unknown task node to invalidate: {inv_id}");
            to_deprecate.insert(inv_id.clone());
        }
        // Recursively include all descendants of invalidated nodes
        let mut added = true;
        while added {
            added = false;
            for (id, node) in &self.nodes {
                if let Some(parent) = &node.parent_id {
                    if to_deprecate.contains(parent) && !to_deprecate.contains(id) {
                        to_deprecate.insert(id.clone());
                        added = true;
                    }
                }
            }
        }
        for dep_id in to_deprecate {
            if dep_id == target_node_id { continue; }
            if let Some(node) = self.nodes.get_mut(&dep_id) {
                if let Some(res) = node.result.take() {
                    node.history_results.push(res);
                }
                node.status = "deprecated".to_string();
            }
        }
        let path = self.path();
        for id in &path {
            if let Some(node) = self.nodes.get_mut(id) {
                if terminal(&node.status) || node.status == "blocked" {
                    node.status = if *id == self.active { "running" } else { "waiting_children" }.to_string();
                    if let Some(res) = node.result.take() {
                        node.history_results.push(res);
                    }
                }
            }
        }
        Ok(())
    }
    pub fn plan(&self) -> Value {
        json!({"mode":if self.enabled(){"tree"}else{"direct"},"nodes":self.nodes.values().collect::<Vec<_>>(),
            "edges":self.nodes.values().filter_map(|node|node.parent_id.as_ref().map(|parent|
                json!({"id":format!("contains_{parent}_{}",node.id),"source":parent,"target":node.id,"label":"子问题","deprecated":node.status=="deprecated"}))).collect::<Vec<_>>(),
            "active_node_id":self.active,"active_path":self.path()})
    }
    pub fn context(&self) -> Value {
        let path=self.path();
        let current=self.nodes.get(self.focus());
        let ancestors=path.iter().filter(|id|**id!=self.active).filter_map(|id|self.nodes.get(id))
            .map(|node|json!({"id":node.id,"title":node.title,"objective":node.objective,"done_when":node.done_when,"constraints":node.constraints,
                "material_pointers":self.source_sets.get(&node.id).and_then(|snapshot|snapshot["materials"].as_array()).map(|items|items.iter().take(12).collect::<Vec<_>>())})).collect::<Vec<_>>();
        let child_nodes=self.nodes.values().filter(|node|node.parent_id.as_deref()==Some(self.focus())).collect::<Vec<_>>();
        let mut result_budget=8_000usize;
        let children=child_nodes.iter().map(|node| {
            let summary=node.result.as_ref().map(|result|text(result,"summary",400.min(result_budget)));
            if let Some(summary)=&summary {result_budget=result_budget.saturating_sub(summary.chars().count());}
            json!({"id":node.id,"title":node.title,"status":node.status,
                "result":node.result.as_ref().map(|result|json!({"summary":summary,
                    "summary_compacted":summary.as_ref().is_some_and(|short|short.chars().count()<result["summary"].as_str().unwrap_or("").chars().count()),
                    "material_ids":result["material_ids"],"finding_ids":result["finding_ids"]}))})
        }).collect::<Vec<_>>();
        let root=self.nodes.values().find(|node|node.parent_id.is_none());
        json!({"mode":"tree","active_path":path,"current_node":current,"ancestor_goals":ancestors,
            "task":root.map(|node|json!({"id":node.id,"title":node.title,"objective":node.objective,"constraints":node.constraints,"status":node.status,"result":node.result})),
            "child_results_and_remaining_problems":children,"stored_node_count":self.nodes.len(),
            "context_rule":"Organizer selects the current node and relevant upstream outputs. The host completes work from actual returns/write/check facts. Ancestors supply goals and constraints; other branches' source stays in the notebook. Entering a child never completes its parent."})
    }
    pub fn recall_nodes(&self, args: &Value) -> Result<Value> {
        let ids=args["tree_node_ids"].as_array().ok_or_else(||anyhow::anyhow!("tree_node_ids must be an array"))?;
        ensure!(!ids.is_empty()&&ids.len()<=8,"retrieve 1 to 8 specific task nodes");
        let mut items=Vec::new();let mut deferred=Vec::new();let mut size=0;
        for id in ids {
            let id=id.as_str().ok_or_else(||anyhow::anyhow!("tree node IDs must be strings"))?;
            let node=self.nodes.get(id).ok_or_else(||anyhow::anyhow!("unknown task node: {id}"))?;
            let item=json!({"node":node,"path":self.path_to(id)?,"source_selection":self.source_sets.get(id)});
            let chars=item.to_string().chars().count();
            if size+chars>22_000 {deferred.push(id);continue;}
            size+=chars;items.push(item);
        }
        Ok(json!({"tree_nodes":items,"deferred_node_ids":deferred,"guidance":"These are saved node goals, actual results and material pointers, not raw source. Retrieve exact materials by ID only when needed."}))
    }
    pub fn root_finished(&self) -> bool {
        self.enabled() && self.nodes.values().filter(|node|node.parent_id.is_none()).all(|node|terminal(&node.status))
    }
    /// Record the request-level delivery from the host's finish_request call.
    /// Work nodes already carry their own sealed outcomes; this only closes
    /// the grouping root and never rewrites child results.
    pub fn finish_request(&mut self,summary:&str,achieved:bool)->Result<()> {
        let root_id=self.nodes.values().find(|node|node.parent_id.is_none()).map(|node|node.id.clone());
        let Some(root_id)=root_id else {return Ok(());};
        if achieved {
            let unfinished=self.nodes.values().filter(|node|node.id!=root_id&&!terminal(&node.status))
                .map(|node|node.id.clone()).collect::<Vec<_>>();
            ensure!(unfinished.is_empty(),"cannot mark the request goal achieved while Flow nodes remain unfinished: {}",unfinished.join(", "));
        }
        let root=self.nodes.get_mut(&root_id).unwrap();
        root.status=if achieved{"completed"}else{"blocked"}.to_owned();
        root.result=Some(json!({"summary":text(&json!({"summary":summary}),"summary",1800),
            "outcome":if achieved{"completed"}else{"blocked"},"expectation_met":achieved,"material_ids":[],"finding_ids":[]}));
        self.active=root_id;
        Ok(())
    }
    pub fn complete_work(&mut self, output:&Value) -> Result<()> {
        let id=output["node_id"].as_str().unwrap_or("");
        ensure!(output["done"]==true && self.active==id,"host completion must concern the active work node");
        self.apply(&json!({"current_node_id":id,"node_result":{"node_id":id,"status":"completed",
            "summary":output["summary"],"material_ids":output["material_ids"],"finding_ids":output["finding_ids"],
            "outcome":output["outcome"],"expectation_met":output["expectation_met"],
            "limitations":output["limitations"],"exported_data":output["exported_data"]}}))
    }
    pub fn work_node_ready(&self, id:&str) -> bool {
        self.nodes.get(id).is_some_and(|node|!terminal(&node.status)&&node.status!="blocked") &&
            !self.nodes.values().any(|child|child.parent_id.as_deref()==Some(id)&&!terminal(&child.status))
    }
    pub fn aggregate_result_allowed(&self,id:&str) -> bool {
        self.nodes.values().any(|child|child.parent_id.as_deref()==Some(id)) && self.work_node_ready(id)
    }
    /// The user request is the root. Concrete work is a child; a missing tree is created here.
    pub fn ensure_request_goal(&mut self, request: &str) -> Result<()> {
        if self.enabled() {return Ok(());}
        let objective=request.trim().chars().take(1200).collect::<String>();
        ensure!(!objective.is_empty(),"a composite request needs the user goal");
        let title=objective.chars().take(160).collect::<String>();
        self.apply(&json!({"node_updates":[{"id":"goal","parent_id":Value::Null,"title":title,"kind":"goal","objective":objective,
            "done_when":"Each child work unit has returned its own outcome. Unfinished parts stay unfinished."}],
            "current_node_id":"goal"}))
    }
    pub fn is_work_leaf(&self, id:&str) -> bool {
        self.nodes.get(id).is_some_and(|node|node.parent_id.is_some()) && self.work_node_ready(id)
    }
    pub fn ensure_work_child(&mut self, id:&str, goal:&str, done_when:&str, constraints:&[String]) -> Result<()> {
        if self.nodes.contains_key(id) {return Ok(());}
        let parent=self.nodes.values().find(|node|node.parent_id.is_none()).map(|node|node.id.clone())
            .ok_or_else(||anyhow::anyhow!("composite work needs the request goal before a child"))?;
        ensure!(id!=parent,"a work packet cannot replace the request goal");
        self.apply(&json!({"node_updates":[{"id":id,"parent_id":parent,"title":goal.chars().take(160).collect::<String>(),
            "kind":"worker","objective":goal.chars().take(1200).collect::<String>(),
            "done_when":done_when.chars().take(800).collect::<String>(),"constraints":constraints}],
            "current_node_id":id}))
    }
    /// Parent supplies the goal boundary. Sibling source and unfinished branches stay out of the worker packet.
    pub fn worker_boundary(&self, node_id:&str) -> Value {
        let node=self.nodes.get(node_id);
        let root=self.nodes.values().find(|node|node.parent_id.is_none());
        json!({"request_goal":root.map(|node|&node.objective),"request_done_when":root.map(|node|&node.done_when),
            "request_constraints":root.map(|node|&node.constraints),"current_goal":node.map(|node|&node.objective),
            "current_done_when":node.map(|node|&node.done_when),"current_constraints":node.map(|node|&node.constraints)})
    }

    /// Validate on a clone so an invalid report cannot partially change the tree.
    pub fn apply(&mut self, args: &Value) -> Result<()> {
        let mut next=self.clone();next.apply_inner(args)?;*self=next;Ok(())
    }
    fn apply_inner(&mut self, args: &Value) -> Result<()> {
        ensure!(args.is_object(), "flow_update must be an object");
        for key in args.as_object().into_iter().flatten().map(|(key, _)| key.as_str()) {
            ensure!(matches!(key,"node_updates"|"plan"|"current_node_id"|"node_result"|"resume_tree"),
                "flow_update.{key} is unknown; use node_updates, plan, current_node_id, node_result, or resume_tree");
        }
        if let Some(plan)=args.get("plan") {
            ensure!(plan.is_object(), "flow_update.plan must be an object with mode=tree and nodes");
            ensure!(plan["mode"]=="tree", "flow_update.plan.mode must be 'tree'");
            ensure!(plan.get("nodes").is_some(), "flow_update.plan.nodes is required");
            ensure!(plan.get("current_node_id").is_none(), "flow_update.plan.current_node_id is misplaced; set flow_update.current_node_id beside plan");
            for key in plan.as_object().into_iter().flatten().map(|(key, _)|key.as_str()) {
                ensure!(matches!(key,"mode"|"nodes"), "flow_update.plan.{key} is unsupported; put current_node_id beside plan");
            }
            ensure!(args.get("node_updates").is_none(), "provide either flow_update.plan.nodes or flow_update.node_updates, not both");
        }
        if let Some(value)=args.get("resume_tree") { ensure!(value.is_boolean(),"flow_update.resume_tree must be a boolean"); }
        if let Some(value)=args.get("current_node_id") { ensure!(value.is_string(),"flow_update.current_node_id must be a node ID string"); }
        if let Some(result)=args.get("node_result") {
            ensure!(result.is_object(),"flow_update.node_result must be an object with node_id, status and summary");
            ensure!(result["node_id"].as_str().is_some()&&result["status"].as_str().is_some()&&result["summary"].as_str().is_some(),
                "flow_update.node_result requires string node_id, status and summary fields");
            for key in result.as_object().into_iter().flatten().map(|(key, _)|key.as_str()) {
                ensure!(matches!(key,"node_id"|"status"|"summary"|"material_ids"|"finding_ids"|"outcome"|"expectation_met"|"limitations"|"exported_data"),
                    "flow_update.node_result.{key} is unsupported");
            }
        }
        let updates=args.get("node_updates").or_else(||args.pointer("/plan/nodes"));
        if let Some(updates)=updates {
            let updates=updates.as_array().ok_or_else(||anyhow::anyhow!("node_updates must be an array"))?;
            let complete_plan=args.get("plan").is_some();
            let update_path=if complete_plan{"flow_update.plan.nodes"}else{"flow_update.node_updates"};
            ensure!(updates.len()<=128,"a tree update may contain at most 128 nodes");
            let mut seen=BTreeSet::new();
            for (update_index,raw) in updates.iter().enumerate() {
                ensure!(raw.is_object(),"each flow_update node update must be an object");
                for key in raw.as_object().into_iter().flatten().map(|(key, _)|key.as_str()) {
                    ensure!(matches!(key,"id"|"parent_id"|"title"|"kind"|"objective"|"done_when"|"constraints"|"resume"),
                        "flow_update node field '{key}' is unsupported");
                }
                let id=text(raw,"id",80);
                ensure!(!id.is_empty() && id.chars().all(|ch|ch.is_ascii_alphanumeric()||matches!(ch,'_'|'-'|'.')),"invalid tree node id");
                ensure!(seen.insert(id.clone()),"duplicate node update: {id}");
                let prior=self.nodes.get(&id);
                if complete_plan || prior.is_none() {
                    for field in ["title","objective","done_when"] {
                        ensure!(raw[field].as_str().is_some_and(|value|!value.trim().is_empty()),
                            "flow_update {} node '{id}' requires nonempty string field '{field}'",if complete_plan{"plan.nodes"}else{"node_updates"});
                    }
                    if complete_plan {ensure!(raw["kind"].as_str().is_some(),"flow_update.plan.nodes node '{id}' requires string field 'kind'");}
                    ensure!(raw["parent_id"].is_string()||raw["parent_id"].is_null(),"flow_update node '{id}' requires parent_id as a node ID or null");
                }
                for field in ["title","kind","objective","done_when"] {
                    ensure!(raw.get(field).is_none_or(Value::is_string),"flow_update node '{id}' field '{field}' must be a string");
                }
                ensure!(raw.get("parent_id").is_none_or(|value|value.is_null()||value.is_string()),"parent_id must be a string or null");
                ensure!(raw.get("constraints").is_none_or(|value|value.as_array().is_some_and(|items|items.iter().all(Value::is_string))),
                    "flow_update node '{id}' constraints must be an array of strings");
                ensure!(raw.get("resume").is_none_or(Value::is_boolean),"flow_update node '{id}' resume must be a boolean");
                let field=|key:&str,old:&str,max| if raw.get(key).is_some(){text(raw,key,max)}else{old.to_owned()};
                let parent_id=if raw.get("parent_id").is_some(){raw["parent_id"].as_str().filter(|id|!id.is_empty()).map(str::to_owned)}else{prior.and_then(|node|node.parent_id.clone())};
                if let Some(prior)=prior {ensure!(parent_id==prior.parent_id,"reparenting an existing node is not allowed");}
                let title=field("title",prior.map_or("",|node|&node.title),160);
                let objective=field("objective",prior.map_or("",|node|&node.objective),1200);
                let done_when=field("done_when",prior.map_or("",|node|&node.done_when),800);
                ensure!(!title.is_empty()&&!objective.is_empty()&&!done_when.is_empty(),"tree node {id} needs title, objective and done_when");
                let kind=field("kind",prior.map_or("step",|node|&node.kind),48);
                let mut status=prior.map_or("pending",|node|&node.status).to_owned();
                let mut result=prior.and_then(|node|node.result.clone());
                if raw["resume"]==true {
                    ensure!(matches!(status.as_str(),"blocked"|"paused"),"field_path={update_path}[{update_index}].resume: node_id='{id}' has status='{status}'; resume is allowed only for blocked/paused nodes. Pending nodes are dispatched directly; completed nodes are sealed and require action=revisit for repair.");
                    status="pending".to_owned();result=None;
                }
                let constraints=raw["constraints"].as_array().map(|items|items.iter().filter_map(Value::as_str).take(12).map(|item|item.chars().take(500).collect()).collect())
                    .unwrap_or_else(||prior.map(|node|node.constraints.clone()).unwrap_or_default());
                if let Some(prior)=prior.filter(|node|terminal(&node.status)) {
                    ensure!(title==prior.title&&objective==prior.objective&&done_when==prior.done_when&&constraints==prior.constraints,
                        "completed Flow node '{id}' is immutable; add a new node under an unfinished ancestor for next work, or use action=revisit with target_node_id='{id}' when its delivered result needs repair");
                }
                let history_results = prior.map(|node| node.history_results.clone()).unwrap_or_default();
                self.nodes.insert(id.clone(),TaskNode{id,parent_id,title,kind,objective,done_when,constraints,status,result,history_results});
            }
        }
        ensure!(!self.nodes.is_empty()&&self.nodes.len()<=128,"task tree needs 1 to 128 nodes");
        ensure!(self.nodes.values().filter(|node|node.parent_id.is_none()).count()==1,"task tree needs exactly one root");
        for node in self.nodes.values() {self.path_to(&node.id)?;}
        for node in self.nodes.values().filter(|node|terminal(&node.status)) {
            ensure!(!self.nodes.values().any(|child|child.parent_id.as_deref()==Some(node.id.as_str())&&!terminal(&child.status)),"cannot add an unfinished child beneath a finished node");
        }
        let previous=self.active.clone();
        if let Some(result)=args.get("node_result") {
            let id=text(result,"node_id",80);let status=text(result,"status",40);let summary=text(result,"summary",1800);
            ensure!(id==previous || self.path_to(&previous).unwrap_or_default().contains(&id),"node_result must concern the active node or its ancestor");
            ensure!(matches!(status.as_str(),"completed"|"blocked"|"skipped")&&!summary.is_empty(),"node_result needs completed/blocked/skipped status and an actual result summary");
            if terminal(&status) {
                ensure!(!self.nodes.values().any(|child|child.parent_id.as_deref()==Some(id.as_str())&&!terminal(&child.status)),"complete or explicitly skip remaining children before completing the parent");
            }
            let node=self.nodes.get_mut(&id).ok_or_else(||anyhow::anyhow!("unknown result node"))?;
            let material_ids=result["material_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64).take(16).collect::<Vec<_>>();
            let finding_ids=result["finding_ids"].as_array().into_iter().flatten().filter_map(Value::as_str).take(16)
                .map(|id|id.chars().take(80).collect::<String>()).collect::<Vec<_>>();
            let mut delivery=json!({"summary":summary,"material_ids":material_ids,"finding_ids":finding_ids,
                "outcome":result["outcome"],"expectation_met":result["expectation_met"],
                "limitations":result["limitations"],"exported_data":result["exported_data"]});
            if result["expectation_met"].is_boolean() {delivery["expectation_met"]=result["expectation_met"].clone();}
            node.status=status;node.result=Some(delivery);
        }
        let mut target=text(args,"current_node_id",80);
        let legal_ids=self.nodes.keys().cloned().collect::<Vec<_>>().join(", ");
        ensure!(self.nodes.contains_key(&target),"flow_update.current_node_id='{target}' must identify a node after the update; available node IDs: [{legal_ids}]");
        if self.nodes[&target].status=="blocked" || terminal(&self.nodes[&target].status) {
            ensure!(args.pointer("/node_result/node_id").and_then(Value::as_str)==Some(target.as_str()),"cannot enter a finished/blocked node; report a new problem under an unfinished ancestor");
            target=self.nodes[&target].parent_id.clone().unwrap_or_default();
        }
        if !previous.is_empty()&&previous!=target {
            if let Some(node)=self.nodes.get_mut(&previous) {
                if node.status=="running" {node.status="paused".to_owned();}
            }
        }
        self.active=target;
        let path=self.path();
        for id in &path {
            let node=self.nodes.get_mut(id).unwrap();
            ensure!(!terminal(&node.status)&&node.status!="blocked","active path passes through a finished/blocked node");
            node.status=if *id==self.active{"running"}else{"waiting_children"}.to_owned();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::TaskTree;
    #[test]
    fn composite_request_keeps_the_goal_above_the_work_child() {
        let mut tree = TaskTree::default();
        tree.ensure_request_goal("启动应用并打开示例 PPT").unwrap();
        tree.ensure_work_child("work_start", "启动应用并确认页面", "页面返回目标应用", &[]).unwrap();
        let plan = tree.plan();
        let nodes = plan["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        assert!(nodes.iter().any(|node| node["id"] == "goal" && node["parent_id"].is_null()));
        assert!(nodes.iter().any(|node| node["id"] == "work_start" && node["parent_id"] == "goal"));
        assert!(tree.is_work_leaf("work_start"));
        assert!(!tree.is_work_leaf("goal"));
        let boundary = tree.worker_boundary("work_start");
        assert_eq!(boundary["request_goal"], "启动应用并打开示例 PPT");
        assert_eq!(boundary["current_goal"], "启动应用并确认页面");
    }

    #[test]
    fn flow_update_rejects_misplaced_ids_and_protects_completed_node_contracts() {
        let mut tree=TaskTree::default();
        tree.ensure_request_goal("Complete the goal").unwrap();
        tree.ensure_work_child("work_a","Inspect project","Project facts returned",&[]).unwrap();
        let before=tree.snapshot();
        let misplaced=tree.apply(&serde_json::json!({"plan":{"mode":"tree","nodes":[],"current_node_id":"work_a"},"current_node_id":"work_a"})).unwrap_err().to_string();
        assert!(misplaced.contains("flow_update.plan.current_node_id"));
        let unknown=tree.apply(&serde_json::json!({"current_node_id":"missing"})).unwrap_err().to_string();
        assert!(unknown.contains("available node IDs")&&unknown.contains("work_a"));
        assert_eq!(tree.snapshot(),before);

        tree.apply(&serde_json::json!({"node_result":{"node_id":"work_a","status":"completed","summary":"Project facts returned"},"current_node_id":"goal"})).unwrap();
        let error=tree.apply(&serde_json::json!({"node_updates":[{"id":"work_a","title":"Start project"}],"current_node_id":"goal"})).unwrap_err().to_string();
        assert!(error.contains("completed Flow node 'work_a' is immutable"));
        assert!(error.contains("action=revisit")&&error.contains("new node"));
    }

    #[test]
    fn pending_nodes_dispatch_without_resume_and_only_blocked_nodes_can_resume() {
        let mut tree=TaskTree::default();
        tree.apply(&serde_json::json!({"plan":{"mode":"tree","nodes":[
            {"id":"goal","parent_id":null,"title":"Goal","kind":"goal","objective":"Goal","done_when":"Done"},
            {"id":"upload","parent_id":"goal","title":"Upload","kind":"worker","objective":"Upload a file","done_when":"File assigned"}
        ]},"current_node_id":"goal"})).unwrap();
        assert_eq!(tree.status("upload"),Some("pending"));
        let pending=tree.apply(&serde_json::json!({"node_updates":[{"id":"upload","resume":true}],"current_node_id":"goal"})).unwrap_err().to_string();
        assert!(pending.contains("field_path=flow_update.node_updates[0].resume")&&pending.contains("status='pending'"));
        tree.apply(&serde_json::json!({"current_node_id":"upload"})).unwrap();
        tree.apply(&serde_json::json!({"current_node_id":"goal","node_result":{"node_id":"upload","status":"blocked","summary":"Upload control is unavailable"}})).unwrap();
        assert_eq!(tree.status("upload"),Some("blocked"));
        tree.apply(&serde_json::json!({"node_updates":[{"id":"upload","resume":true}],"current_node_id":"upload"})).unwrap();
        assert_eq!(tree.status("upload"),Some("running"));
    }
}
