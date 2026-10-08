//! Stable task state and an acknowledged Observer inbox, independent of the
//! sliding window of raw model messages.
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn timestamp() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64 }
fn active(fact: &Value) -> bool { fact["record_state"]!="historical" && fact["status"]!="obsolete" }
fn current(fact: &Value) -> bool { active(fact) && fact["record_state"]!="conflicted" && fact["verification_state"]!="legacy_unreviewed" }

fn short(text: &str, limit: usize) -> String { text.chars().take(limit).collect() }
fn strings(value: Option<&Value>, limit: usize) -> Vec<String> {
    value.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str)
        .map(|text| short(text.trim(), 500)).filter(|text| !text.is_empty()).take(limit).collect()
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkState {
    current_node: String,
    current_unit: String,
    purpose: String,
    next_action: String,
    known_conditions: Vec<String>,
    open_questions: Vec<String>,
    work_organization: Option<Value>,
    findings: BTreeMap<String, Value>,
    files: BTreeMap<String, Value>,
    completed_actions: Vec<Value>,
    query_results: Vec<Value>,
    investigation_streak: usize,
    repeated_streak: usize,
    finding_revision: u64,
    coordination_only_steps: usize,
    workspace_edits_undone: Vec<Value>,
    finding_history: Vec<Value>,
    edit_targets: Vec<String>,
    steps_without_execution: usize,
    execution_this_step: bool,
    last_report_step: usize,
    observations_since_report: Vec<Value>,
}

impl WorkState {
    pub fn import_legacy(&mut self, observations: &[Value]) -> Vec<Value> {
        let mut revisions=Vec::new();
        for (id,fact) in &mut self.findings {
            if fact.get("record_state").is_some() {continue;}
            let original=observations.iter().find(|record|record["findings"].as_array().is_some_and(|findings|
                findings.iter().any(|finding|finding["id"]==*id && finding["text"]==fact["text"])));
            fact["record_state"]=json!(if fact["status"]=="obsolete"{"historical"}else{"current"});
            fact["verification_state"]=json!("legacy_unreviewed");fact["topic"]=json!(id);
            if let Some(original)=original {
                fact["recorded_at"]=original["time"].clone();fact["turn"]=original["turn"].clone();fact["step"]=original["step"].clone();
                fact["origin_event_seq"]=original["seq"].clone();
            }
            revisions.push(json!({"event":"legacy_imported","at":fact["recorded_at"],"turn":fact["turn"],"step":fact["step"],"finding":fact}));
        }
        revisions
    }
    pub fn record_query(&mut self,step:usize,name:&str,args:&Value,result:&Value,call_id:&str) {
        let key=crate::symbol_description::content_hash(format!("{name}:{args}").as_bytes());
        self.query_results.retain(|record|record["key"]!=key);
        self.query_results.push(json!({"key":key,"tool":name,"tool_call_id":call_id,"step":step,"node_id":self.current_node,"historical":true,
            "query":args["query"].as_str().map(|text|short(text,300)),"path":args.get("path").or_else(||args.get("file_path")).and_then(Value::as_str).map(|text|short(text,200)),
            "matches":result["matches"].as_array().map(Vec::len),"entries":result["entries"].as_array().map(Vec::len),"truncated":result.get("truncated").or_else(||result.pointer("/group_page/truncated")),
            "functional_groups":result["groups"].as_array().map(|groups|groups.iter().take(4).map(|group|json!({"name":group["name"],"responsibility":group["responsibility"].as_str().map(|text|short(text,300)),"status":group["description_status"],"key_files":group["key_files"].as_array().map(|files|files.iter().take(4).collect::<Vec<_>>()),"entry_points":group["entries"].as_array().map(|entries|entries.iter().take(3).map(|entry|json!({"id":entry["id"],"file_path":entry["file_path"],"responsibility":entry.pointer("/description/text"),"status":entry.pointer("/description/status")})).collect::<Vec<_>>())})).collect::<Vec<_>>())}));
        if self.query_results.len()>64 {self.query_results.remove(0);}
    }
    pub fn resume(&mut self, prompt: &str) {
        self.current_unit.clear();
        self.current_node.clear();
        self.edit_targets.clear();
        self.purpose = short(prompt, 1200);
        self.next_action.clear();
        self.known_conditions.clear();
        self.open_questions.clear();
        self.work_organization=None;
        self.investigation_streak = 0;
        self.repeated_streak = 0;
        self.coordination_only_steps = 0;
        self.steps_without_execution = 0;
        self.execution_this_step = false;
        self.last_report_step=0;
        self.observations_since_report.clear();
        for fact in self.findings.values_mut() {
            // A new user turn does not invalidate the preceding investigation.
            // Actual file changes mark linked findings needs_recheck in observe.
            fact["historical"] = json!(true);
            if fact.get("record_state").is_none() {
                fact["record_state"]=json!("current");fact["verification_state"]=json!("legacy_unreviewed");
            }
        }
        for file in self.files.values_mut() { file["historical"] = json!(true); }
        for action in &mut self.completed_actions { action["historical"] = json!(true); }
    }

    #[cfg(test)]
    pub fn report(&mut self, args: &Value) { self.report_at(args,0,0,&[],""); }
    pub fn edit_targets(&self) -> &[String] { &self.edit_targets }
    pub fn activate_node(&mut self, node: &str, targets: Vec<String>) {
        self.current_node=node.to_owned();self.edit_targets=targets;
    }
    pub fn activate_unit(&mut self, id: &str, node: &str, targets: Vec<String>) {
        if self.current_unit!=id {
            self.purpose.clear();self.next_action.clear();self.known_conditions.clear();self.open_questions.clear();
            self.investigation_streak=0;self.coordination_only_steps=0;self.steps_without_execution=0;
        }
        self.current_unit=id.to_owned();self.activate_node(node,targets);
    }
    pub fn report_at(&mut self, args: &Value, turn: usize, step: usize, materials: &[Value], user_prompt: &str) -> Vec<Value> {
        let mut events=Vec::new();
        self.last_report_step=step;
        self.observations_since_report.clear();
        self.current_node = args["current_node_id"].as_str().unwrap_or("").to_owned();
        self.purpose = short(args["purpose"].as_str().unwrap_or(""), 1200);
        self.next_action = short(args["next_action"].as_str().unwrap_or(""), 1200);
        self.known_conditions = strings(args.get("known_conditions"), 12);
        if let Some(decision)=args.get("work_organization") {
            self.work_organization=Some(json!({"mode":decision["mode"],"reason":short(decision["reason"].as_str().unwrap_or(""),700),"turn":turn,"step":step}));
        }
        if args.get("edit_targets").is_some() { self.edit_targets=strings(args.get("edit_targets"),16); }
        // Omission preserves unresolved questions; [] explicitly clears them.
        if args.get("open_questions").is_some() {
            self.open_questions = strings(args.get("open_questions"), 8);
        }
        for update in args.get("findings").and_then(Value::as_array).into_iter().flatten().take(12) {
            let id = short(update["id"].as_str().unwrap_or("").trim(), 80);
            let text = short(update["text"].as_str().unwrap_or("").trim(), 700);
            if id.is_empty() || text.is_empty() { continue; }
            let status = update["status"].as_str().unwrap_or("confirmed");
            if !matches!(status, "confirmed" | "assumed" | "needs_recheck" | "obsolete") { continue; }
            let topic=update["topic"].as_str().filter(|topic|!topic.trim().is_empty()).map(|topic|short(topic,120))
                .or_else(||self.findings.get(&id).and_then(|fact|fact["topic"].as_str()).map(str::to_owned)).unwrap_or_else(||id.clone());
            let supersedes=strings(update.get("supersedes"),16);
            let mut conflicts=strings(update.get("conflicts_with"),16);
            // Equal topics with different wording may be complementary. Only
            // explicit incompatible claims create a conflict, not string inequality.
            let evidence_ids=update.pointer("/verification/material_ids").or_else(||update.get("material_ids"))
                .and_then(Value::as_array).cloned().unwrap_or_default();
            let sources=evidence_ids.iter().filter_map(|id|materials.iter().find(|material|material["id"]==*id && material["available"]==true))
                .map(|material|json!({"material_id":material["id"],"path":material["path"],"code_hash":material["code_hash"],
                    "start_line":material["start_line"],"end_line":material["end_line"],
                    "relative_start":material["relative_start"],"relative_end":material["relative_end"]})).collect::<Vec<_>>();
            let basis=update.pointer("/verification/basis").and_then(Value::as_str).unwrap_or("");
            let summary=update.pointer("/verification/summary").and_then(Value::as_str).unwrap_or("");
            let quote=update.pointer("/verification/quote").and_then(Value::as_str).unwrap_or("");
            let verified=!summary.trim().is_empty() && ((basis=="source" && !sources.is_empty() && sources.len()==evidence_ids.len())
                || (basis=="user" && !quote.trim().is_empty() && user_prompt.contains(quote)));
            let previous=self.findings.get(&id).cloned();
            if let Some(previous)=previous.as_ref().filter(|previous|previous["record_state"]=="conflicted") {
                for other in strings(previous.get("conflicts_with"),16) { if !conflicts.contains(&other) { conflicts.push(other); } }
            }
            let resolve = !conflicts.is_empty() && (verified || status == "confirmed") && conflicts.iter().all(|other| supersedes.contains(other));
            let unresolved = !conflicts.is_empty() && !resolve;
            let carried = previous.as_ref().filter(|old| !verified && current(old) && old["status"] == "confirmed" && old["verification_state"] == "verified"
                && old["source_version_changed"] != true && old["text"] == text && old["files"] == json!(strings(update.get("files"), 6)));
            // An ordinary finding is the Worker's conclusion. Extra verification
            // is required for resolving an explicit conflict or invalidated source, not as
            // a ceremony for every non-conflicting observation or wording evolution.
            let effective_status = if status == "confirmed" && !verified && previous.as_ref().is_some_and(|old| old["source_version_changed"] == true) { "needs_recheck" } else { status };
            let mut fact = json!({"id":id,"topic":topic,"text":text,"status":effective_status,"files":strings(update.get("files"), 6),
                "node_id":self.current_node,"work_id":self.current_unit,"material_ids":update["material_ids"].as_array().cloned().unwrap_or_default(),
                "record_state":if unresolved{"conflicted"}else if status=="obsolete"{"historical"}else{"current"},
                "verification_state":if verified{"verified"}else{"worker_claim"},"source_refs":sources,
                "verification":update["verification"],"conflicts_with":conflicts,"supersedes":supersedes});
            if let Some(old) = carried {
                for field in ["verification_state","source_refs","verification","confirmed_at","material_ids"] {fact[field]=old[field].clone();}
            }
            let unchanged = self.findings.get(&id).is_some_and(|previous| previous["text"] == fact["text"]
                && previous["status"] == fact["status"] && previous["files"] == fact["files"]
                && previous["topic"] == fact["topic"] && previous["record_state"] == fact["record_state"]
                && previous["conflicts_with"] == fact["conflicts_with"] && previous["supersedes"] == fact["supersedes"]
                && (!verified || (previous["verification_state"] == "verified" && previous["source_refs"] == fact["source_refs"])));
            if !unchanged {
                let at = timestamp();
                // Report wording is not execution progress. Preserve every replaced
                // revision, while only the effective revision enters normal context.
                if let Some(mut old) = previous {
                    old["record_state"] = json!("historical"); old["historical_at"] = json!(at);
                    old["replacement_id"] = json!(id); old["history_reason"] = json!(if unresolved { "candidate requires confirmation" } else { "new revision" });
                    old.as_object_mut().map(|object| object.remove("conflict_candidates"));
                    if unresolved { fact["conflict_candidates"] = json!([old.clone()]); }
                    self.finding_history.push(old.clone()); events.push(json!({"event":"revision_archived","at":at,"turn":turn,"step":step,"finding":old}));
                }
                let related = conflicts.iter().chain(supersedes.iter()).filter(|other| *other != &id).collect::<std::collections::BTreeSet<_>>();
                for other_id in related {
                    if let Some(other) = self.findings.get_mut(other_id) {
                        if supersedes.contains(other_id) {
                            other["record_state"] = json!("historical"); other["historical_at"] = json!(at); other["replacement_id"] = json!(id);
                            other["history_reason"] = json!(if !summary.is_empty() { summary } else { "superseded by newer revision" });
                            self.finding_history.push(other.clone()); events.push(json!({"event":"superseded","at":at,"turn":turn,"step":step,"finding":other}));
                        } else if unresolved {
                            other["record_state"] = json!("conflicted");
                            let mut links = strings(other.get("conflicts_with"), 16);
                            if !links.contains(&id) { links.push(id.clone()); } other["conflicts_with"] = json!(links);
                            events.push(json!({"event":"conflict_detected","at":at,"turn":turn,"step":step,"finding":other}));
                        }
                    }
                }
                self.finding_revision += 1;
                fact["revision"] = json!(self.finding_revision);
                fact["recorded_at"]=json!(at);fact["turn"]=json!(turn);fact["step"]=json!(step);
                if verified {fact["confirmed_at"]=json!(at);}
                events.push(json!({"event":if unresolved{"conflict_detected"}else if verified{"confirmed"}else{"recorded"},"at":at,"turn":turn,"step":step,"finding":fact}));
                self.findings.insert(id, fact);
            }
        }
        if self.finding_history.len()>32 { self.finding_history.drain(..self.finding_history.len()-32); }
        events
    }

    pub fn observe(&mut self, step: usize, name: &str, args: &Value, result: &Value, failed: bool, repeated: bool) {
        let materials=result.pointer("/notebook/materials").and_then(Value::as_array).map(|pages|pages.iter().map(|page|json!({
            "id":page["id"],"path":page["path"],"start_line":page["start_line"],"end_line":page["end_line"],"status":page["status"]})).collect::<Vec<_>>());
        self.observations_since_report.push(json!({"node_id":self.current_node,"step":step,"tool":name,"path":result.get("path").or_else(||args.get("path")),
            "start_line":result["start_line"],"end_line":result["end_line"],"material_id":result.pointer("/notebook_material/id"),
            "materials":materials,"changed":result["changed"],"failed":failed,"error":result["error"],"repeated":repeated}));
        if self.observations_since_report.len()>12 {self.observations_since_report.remove(0);}
        if failed { return; }
        if crate::worker_read_cache::is_source_read(name) {
            let path = result.get("path").or_else(|| result.pointer("/symbol/file_path")).and_then(Value::as_str);
            let hash = result.get("code_hash").or_else(|| result.pointer("/description/code_hash")).and_then(Value::as_str);
            if let (Some(path), Some(hash)) = (path, hash) {
                let path = path.replace('\\', "/");
                let changed = self.files.get(&path).is_some_and(|old| old["code_hash"] != hash);
                if changed {
                    for fact in self.findings.values_mut() {
                        if fact["files"].as_array().is_some_and(|files| files.iter().any(|file|
                            file.as_str().is_some_and(|file| file.replace('\\', "/") == path))) {
                            fact["source_version_changed"] = json!(true);
                        }
                    }
                }
                {
                    let reads = self.files.get(&path).and_then(|file|file["reads"].as_u64()).unwrap_or(0) + 1;
                    self.files.insert(path.clone(), json!({"path":path,"code_hash":hash,"last_step":step,
                        "reads":reads,"historical":false,"last_read":result.get("read_coverage").cloned()
                            .unwrap_or_else(|| json!({"start_line":args.get("start_line"),"end_line":args.get("end_line")}))}));
                }
            }
        }
        let investigation = name=="recall_work" || crate::worker_read_cache::is_source_read(name) || name.starts_with("search_")
            || name.starts_with("list_") || name.ends_with("_index_status");
        if investigation {
            self.investigation_streak += 1;
            self.repeated_streak = if repeated { self.repeated_streak + 1 } else { 0 };
        } else if crate::project_process::is_execution(name) || (matches!(name, "write_file" | "replace_range" | "edit_file") && result["changed"]!=false) {
            self.investigation_streak = 0;
            self.repeated_streak = 0;
            self.execution_this_step = true;
            self.completed_actions.push(json!({"step":step,"tool":name,
                "historical":false,
                "node_id":self.current_node,
                "work_id":self.current_unit,"process_id":result["process_id"],"script":result["script"],"program":result["program"],"args":result["args"],"ready":result["ready"],"changes":result["changes"],
                "exit_code":if name=="run_program" {result["process_exit_code"].clone()} else {result["status"].clone()},
                "process_success":result["process_success"],"outcome":result["outcome"],"changed":result["changed"],
                "target":if name=="run_program" {Some(short(&format!("{} {}",result["program"].as_str().unwrap_or(""),result["args"]),200))}
                    else {args.get("path").or_else(||args.get("command")).and_then(Value::as_str).map(|text|short(text,200))},
                "result":short(&result.to_string(), 400)}));
            let changed_path = result.get("path").or_else(||args.get("path")).and_then(Value::as_str).map(|path|path.replace('\\', "/"));
            for (path, file) in &mut self.files {
                if crate::project_process::may_change_files(name) || (result["changed"] != false && changed_path.as_ref() == Some(path)) {
                    file["historical"] = json!(true);
                }
            }
            for fact in self.findings.values_mut() {
                // A build/command is not itself proof that every source fact
                // changed. Direct writes invalidate only the affected file;
                // later read hashes detect actual changes made by commands.
                if !crate::project_process::may_change_files(name) && result["changed"] != false && fact["status"] == "confirmed"
                    && changed_path.as_ref().is_some_and(|path|fact["files"].as_array().is_some_and(|files|
                        files.iter().any(|file|file.as_str().is_some_and(|file|file.replace('\\', "/") == *path)))) {
                    fact["source_version_changed"] = json!(true);
                }
            }
        }
    }

    pub fn snapshot(&self) -> Value { serde_json::to_value(self).unwrap_or_else(|_|json!({})) }
    pub fn finding_receipts(&self,args:&Value) -> Value {
        json!(args["findings"].as_array().into_iter().flatten().filter_map(|update|update["id"].as_str())
            .filter_map(|id|self.findings.get(id)).map(|fact|json!({"id":fact["id"],"revision":fact["revision"],
                "status":fact["status"],"record_state":fact["record_state"],"verification_state":fact["verification_state"],"conflicts_with":fact["conflicts_with"]})).collect::<Vec<_>>())
    }
    pub fn refresh_source_refs(&mut self, sources: &[Value], retention: &Value, turn:usize, step:usize) -> Vec<Value> {
        let mut events=Vec::new();
        for fact in self.findings.values_mut().filter(|fact|active(fact)) {
            let mut changed=false;
            let mut all_refs_verified=fact["source_refs"].as_array().is_some_and(|refs|!refs.is_empty());
            if let Some(refs)=fact["source_refs"].as_array_mut() {
                for reference in refs {
                    if let Some(source)=sources.iter().find(|source|source["id"]==reference["material_id"]
                        && reference["relative_start"].is_number() && reference["relative_end"].is_number()
                        && source["relative_start"].as_u64()<=reference["relative_start"].as_u64()
                        && source["relative_end"].as_u64()>=reference["relative_end"].as_u64()) {
                        // retrieve() has verified an unchanged exact region. A file
                        // edit elsewhere does not invalidate this source finding.
                        reference["code_hash"]=source["code_hash"].clone();
                        let base=source["start_line"].as_u64().unwrap_or(1).saturating_sub(source["relative_start"].as_u64().unwrap_or(0));
                        reference["start_line"]=json!(base+reference["relative_start"].as_u64().unwrap_or(0));
                        reference["end_line"]=json!(base+reference["relative_end"].as_u64().unwrap_or(0));
                    } else {
                        all_refs_verified=false;
                        if retention["evicted"].as_array().is_some_and(|items|items.iter().any(|item|
                            item["id"]==reference["material_id"] && matches!(item["reason"].as_str(),Some("changed"|"missing"|"ambiguous")))) {changed=true;}
                    }
                }
            }
            if changed && fact["verification_state"]!="source_changed" {
                fact["verification_state"]=json!("source_changed");fact["source_version_changed"]=json!(true);
                if fact["status"]=="confirmed" {fact["status"]=json!("needs_recheck");}
                events.push(json!({"event":"source_changed","at":timestamp(),"turn":turn,"step":step,"finding":fact}));
            } else if all_refs_verified && fact["source_version_changed"]==true && fact["verification_state"]!="source_changed" {
                fact["source_version_changed"]=json!(false);
                events.push(json!({"event":"source_refs_refreshed","at":timestamp(),"turn":turn,"step":step,"finding":fact}));
            }
        }
        events
    }
    pub fn notify_revert(&mut self, summary: &Value) {
        let paths=summary["files"].as_array().into_iter().flatten()
            .filter_map(|file|file["path"].as_str()).collect::<Vec<_>>();
        for fact in self.findings.values_mut() {
            if fact["status"] != "obsolete" && fact["files"].as_array().is_some_and(|files|
                files.iter().any(|file|file.as_str().is_some_and(|file|paths.contains(&file.replace('\\', "/").as_str())))) {
                fact["status"]=json!("needs_recheck");
            }
        }
        for path in &paths { if let Some(file)=self.files.get_mut(*path) { file["historical"]=json!(true); } }
        self.workspace_edits_undone.push(json!({"turn":summary["turn"],"files":paths,
            "guidance":"The user undid the recorded file changes in this turn. Do not assume its implementation remains present; use the current source when needed."}));
    }
    pub fn finish_step(&mut self, tools: &[&str]) {
        // Reports and receipts cannot reset this counter by rephrasing findings.
        // It measures actual tool choices, independently of Observer presence.
        let coordination_only = !tools.is_empty() && tools.iter().all(|name|
            matches!(*name, "report_progress" | "respond_observer" | "consult_observer" | "recall_work" | "notebook/source" | "notebook/query_result"));
        self.coordination_only_steps = if coordination_only { self.coordination_only_steps + 1 } else { 0 };
        self.steps_without_execution=if self.execution_this_step {0}else{self.steps_without_execution+1};
        self.execution_this_step=false;
    }
    #[cfg(test)]
    pub fn context(&self) -> Value {
        // Store the complete record, inject a compact view. The Worker can
        // recall an older file/finding without reopening project source.
        let mut files = self.files.values().collect::<Vec<_>>();
        files.sort_by_key(|file| (file["historical"] == true, std::cmp::Reverse(file["last_step"].as_u64().unwrap_or(0))));
        let mut findings = self.findings.values().filter(|fact|current(fact)).collect::<Vec<_>>();
        findings.sort_by_key(|fact|std::cmp::Reverse(fact["revision"].as_u64().unwrap_or(0)));
        json!({"purpose":self.purpose,"next_action":self.next_action,
            "known_conditions":self.known_conditions,"open_questions":self.open_questions,
            "findings":findings.into_iter().take(24).collect::<Vec<_>>(),
            "files":files.into_iter().take(24).map(|file|json!({"path":file["path"],"code_hash":file["code_hash"],
                "reads":file["reads"],"historical":file["historical"],"last_step":file["last_step"]})).collect::<Vec<_>>(),
            "completed_actions":self.completed_actions.iter().rev().take(8).collect::<Vec<_>>(),
            "stored_files":self.files.len(),"stored_findings":self.findings.len(),
            "coordination_only_steps":self.coordination_only_steps,
            "workspace_edits_undone":self.workspace_edits_undone.iter().rev().take(4).collect::<Vec<_>>(),
            "recall":"Use recall_work with a file path, finding ID or topic when a relevant earlier finding is absent from this compact view. Do not reread source just to recover a prior conclusion."})
    }
    pub fn material_query(&self, node: &str) -> String {
        let mut findings=self.findings.values().filter(|fact|active(fact)).collect::<Vec<_>>();
        findings.sort_by_key(|fact|std::cmp::Reverse((fact["node_id"]==node,fact["revision"].as_u64().unwrap_or(0))));
        let files=findings.into_iter().take(8).flat_map(|fact|fact["files"].as_array().into_iter().flatten())
            .filter_map(Value::as_str).collect::<std::collections::BTreeSet<_>>();
        files.into_iter().collect::<Vec<_>>().join(" ")
    }
    pub fn node_context(&self, node: &str) -> Value {
        let mut findings=self.findings.values().filter(|fact|current(fact)).collect::<Vec<_>>();
        findings.sort_by_key(|fact|std::cmp::Reverse((fact["node_id"]==node,fact["revision"].as_u64().unwrap_or(0))));
        let relevant=findings.into_iter().take(12).collect::<Vec<_>>();
        json!({"current_node":node,"edit_targets":self.edit_targets,"work_organization":self.work_organization,
            // Reports remain in the notebook/UI. Do not replay old promises or
            // speculative questions as ongoing instructions in every request.
            "recent_operations":self.observations_since_report,
            "findings":relevant,
            "unresolved_conflicts":self.findings.values().filter(|fact|active(fact) && fact["record_state"]=="conflicted").collect::<Vec<_>>(),
            "legacy_claims_to_reconcile":self.findings.values().filter(|fact|active(fact) && fact["verification_state"]=="legacy_unreviewed").collect::<Vec<_>>(),
            "execution":{ "steps_without_execution":self.steps_without_execution,"investigation_calls":self.investigation_streak,"coordination_only_steps":self.coordination_only_steps },
            "completed_actions":self.completed_actions.iter().rev().take(4).collect::<Vec<_>>(),
            "query_results":self.query_results.iter().rev().filter(|record|record["node_id"]==node).take(4).collect::<Vec<_>>(),
            "stored_findings":self.findings.len(),"stored_files":self.files.len(),
            "workspace_edits_undone":self.workspace_edits_undone.iter().rev().take(2).collect::<Vec<_>>(),
            "recall":"Older conclusions and exact version-checked materials are available with recall_work. source_version_changed records a file update, not a mandate to re-investigate every conclusion."})
    }
    /// Model projection only; the notebook and UI retain every full record.
    pub fn request_context(&self, node: &str, prompt: &str, turn: usize, source_files: &[String]) -> Value {
        let terms=crate::symbol_query::terms(prompt,crate::symbol_query::MatchMode::Any);
        let related=|fact:&Value| {
            let same_turn=fact["turn"].as_u64()==Some(turn as u64);
            let same_file=fact["files"].as_array().into_iter().flatten().filter_map(Value::as_str)
                .any(|file|self.edit_targets.iter().chain(source_files).any(|path|path.replace('\\',"/")==file.replace('\\',"/")));
            let text=format!("{} {} {}",fact["topic"].as_str().unwrap_or(""),fact["text"].as_str().unwrap_or(""),fact["files"]).to_lowercase();
            let hits=terms.iter().filter(|term|term.chars().count()>=3 && text.contains(term.as_str())).count();
            (same_turn,same_file,hits)
        };
        let mut ranked=self.findings.values().filter(|fact|active(fact)).filter_map(|fact| {
            let score=related(fact);
            (score.0 || score.1 || score.2>0).then_some((score,fact))
        }).collect::<Vec<_>>();
        ranked.sort_by_key(|(score,fact)|std::cmp::Reverse((fact["node_id"].as_str()==Some(node),*score,fact["revision"].as_u64().unwrap_or(0))));
        let compact=|fact:&Value| {
            let mut item=serde_json::Map::new();
            for key in ["id","node_id","topic","text","status","record_state","verification_state","historical",
                "turn","step","recorded_at","revision","files","material_ids","source_refs","conflicts_with","supersedes"] {
                if let Some(value)=fact.get(key) {item.insert(key.to_owned(),value.clone());}
            }
            Value::Object(item)
        };
        let mut context=self.node_context(node);
        context["findings"]=json!(ranked.iter().filter(|(_,fact)|current(fact)).take(12).map(|(_,fact)|compact(fact)).collect::<Vec<_>>());
        context["unresolved_conflicts"]=json!(ranked.iter().filter(|(_,fact)|fact["record_state"]=="conflicted").take(8).map(|(_,fact)|compact(fact)).collect::<Vec<_>>());
        context["legacy_claims_to_reconcile"]=json!(ranked.iter().filter(|(_,fact)|fact["verification_state"]=="legacy_unreviewed").take(4).map(|(_,fact)|compact(fact)).collect::<Vec<_>>());
        context["projection"]=json!({"scope":"current turn, selected files, or matching task terms",
            "matched_records":ranked.len(),"all_records_available_with":"recall_work"});
        context
    }
    pub fn node_request_context(&self, node: &str, prompt: &str, turn: usize, source_files: &[String]) -> Value {
        // Scope before ranking/limits: sibling findings cannot displace this node's facts.
        let mut scoped=self.clone();
        scoped.findings.retain(|_,fact|fact["node_id"].as_str()==Some(node) ||
            (fact["turn"].as_u64()!=Some(turn as u64) && fact["files"].as_array().into_iter().flatten().filter_map(Value::as_str)
                .any(|file|source_files.iter().chain(&self.edit_targets).any(|path|path.replace('\\',"/")==file.replace('\\',"/")))));
        let mut context=scoped.request_context(node,prompt,turn,source_files);
        if let Some(items)=context["completed_actions"].as_array_mut() {items.retain(|action|action["node_id"].as_str()==Some(node));}
        if let Some(items)=context["recent_operations"].as_array_mut() {items.retain(|action|action["node_id"].as_str()==Some(node));}
        context["projection"]["scope"]=json!("current task-tree node; ancestors and child results are supplied separately");
        context
    }
    pub fn unit_context(&self, id:&str, selected:&[String]) -> Value {
        json!({"findings":self.findings.values().filter(|fact|current(fact) &&
            (fact["work_id"].as_str()==Some(id)||selected.iter().any(|key|fact["id"].as_str()==Some(key.as_str()))))
            .take(24).collect::<Vec<_>>(),"projection":{"scope":"current work unit and Organizer-selected conclusions"}})
    }
    pub fn recall(&self, query: &str) -> Value {
        let terms = query.to_lowercase().replace('\\', "/").split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        let relevant = |value: &Value| {
            let text = value.to_string().to_lowercase().replace('\\', "/");
            terms.iter().any(|term|text.contains(term))
        };
        json!({"findings":self.findings.values().filter(|fact|current(fact) && relevant(fact)).take(12).collect::<Vec<_>>(),
            "unresolved_conflicts":self.findings.values().filter(|fact|active(fact) && !current(fact) && relevant(fact)).take(12).collect::<Vec<_>>(),
            "files":self.files.values().filter(|file|relevant(file)).take(12).collect::<Vec<_>>(),
            "completed_actions":self.completed_actions.iter().rev().filter(|action|relevant(action)).take(8).collect::<Vec<_>>(),
            "query_results":self.query_results.iter().rev().filter(|record|relevant(record)).take(8).collect::<Vec<_>>(),
            "guidance":"These are prior Worker records, not new source verification. Recheck only facts that matter to the current task when source or task scope changed."})
    }
    pub fn attention(&self) -> Option<&'static str> {
        if self.coordination_only_steps >= 2 {
            Some("The last consecutive steps contained only reports, receipts or internal consultation/recall, with no project operation. Repeating 'I will implement now' is not execution. Take the next substantive action within the current authorization: perform the known edit, make one targeted lookup needed for a safe edit, or provide the requested discussion/analysis answer. If genuinely blocked, identify the actual missing input or failed operation. Do not spend another round promising action or asking Observer for permission. Do not edit a read-only task or guess source you have not read.")
        } else if self.repeated_streak >= 3 || self.investigation_streak >= 12 {
            Some("Investigation is continuing without a new reported finding or a completed execution action. Use known findings for the next concrete authorized edit, or synthesize the requested analysis answer. Investigate further only for a specific unresolved dependency that changes that action; do not add a reporting-only round just to restate this reminder. This is a direction reminder, not a tool limit or a requirement to edit a read-only task.")
        } else { None }
    }
    pub fn defer_coordination(&self) -> bool { self.coordination_only_steps>=2 || self.investigation_streak>=12 || self.repeated_streak>=3 }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ObserverInbox { items: BTreeMap<String, Value>, counter: usize, request_id: usize, scope: Value, observations: Vec<Value> }

fn advice_content(review:&Value)->Value {
    let text=|value:&Value|value.as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    let mut suggestions=review["suggestions"].as_array().into_iter().flatten().map(text).collect::<Vec<_>>();
    suggestions.sort();suggestions.dedup();
    let mut findings=review["findings"].as_array().into_iter().flatten().map(|item|text(item.get("reason").unwrap_or(item))).collect::<Vec<_>>();
    findings.sort();findings.dedup();
    json!({"summary":text(&review["summary"]),"suggestions":suggestions,"findings":findings,"target":text(&review["target"]),
        "observer_return":review["observer_return"],"visual_check_result":review["visual_check_result"],"visual_request":review["visual_request"],"review_status":review["review_status"]})
}

fn same_advice_issue(item:&Value,review:&Value,request_id:usize)->bool {
    let key=review["issue_key"].as_str().unwrap_or("general_direction");
    let category=review["category"].as_str().unwrap_or("other");
    item["request_id"]==request_id && item["identity"]==review["identity"] && (item["issue_key"]==key
        || (category=="sufficiency" && item["category"]==category && item["node_id"]==review["node_id"]))
}

fn advice_source_is_not_newer(review:&Value,observed:&Value)->bool {
    if let (Some(incoming),Some(previous))=(review["source_event_seq"].as_u64(),observed["source_event_seq"].as_u64()) {
        return incoming<=previous;
    }
    // Older snapshots can lack a committed event sequence. Steps reset each turn.
    let order=|value:&Value|->Option<(u64,u64,u8)> {
        let stage=match value["stage"].as_str()? {"assignment"=>0,"progress"=>1,"handoff"=>2,_=>return None};
        Some((value["turn"].as_u64()?,value.get("step").unwrap_or(&value["origin_step"]).as_u64()?,stage))
    };
    matches!((order(review),order(observed)),(Some(incoming),Some(previous)) if incoming<=previous)
}

impl ObserverInbox {
    pub fn set_scope(&mut self,scheduler:&crate::work_scheduler::WorkScheduler) {
        self.start_request(scheduler.request_started_turn);
        self.scope=json!({"request_id":scheduler.request_started_turn,"plan_revision":scheduler.plan_revision,"frames":scheduler.frames});
        for item in self.items.values_mut() {
            if item["identity"].is_null() && item["archived"]!=true {
                let work=scheduler.frames.values().find(|frame|frame.order.node_id==item["node_id"].as_str().unwrap_or("") && frame.invalidated_by_plan_revision.is_none());
                if let Some(frame)=work {item["identity"]=crate::observer_service::identity("",scheduler,&frame.order.id);item["legacy_scope_migrated"]=json!(true);}
                else {item["archived"]=json!(true);}
            }
            let identity=item.get("adopted_identity").unwrap_or(&item["identity"]);
            if !identity.is_null() && !crate::observer_service::applies(identity,&self.scope) {item["archived"]=json!(true);}
        }
    }
    pub fn unhandled(&self)->Vec<Value> {self.pending().into_iter().filter(|item|item["disposition"]=="unread"&&item["organizer_consumption"].is_null()).collect()}
    /// The same original messages go to Worker and Organizer. Archival and
    /// receipt metadata label history; they do not delete the sender's words.
    pub fn messages(&self)->Vec<Value> {
        let mut messages=self.items.values().filter(|item|item["request_id"]==self.request_id).cloned().collect::<Vec<_>>();
        messages.sort_by_key(|item|item["id"].as_str().and_then(|id|id.strip_prefix("advice_")).and_then(|id|id.parse::<usize>().ok()).unwrap_or(0));
        messages
    }
    pub fn historical(&self)->Vec<Value> {
        self.items.values().filter(|item|item["request_id"]==self.request_id && item["archived"]==true).rev().take(4).cloned().collect()
    }
    /// A late delivery may still matter to downstream work. Notify the current
    /// Organizer without turning its old advice into a current obligation.
    pub fn late_delivery_notifications(&self)->Vec<Value> {
        self.items.values().filter(|item| {
            let identity=&item["identity"];
            let frame=&self.scope["frames"][identity["work_id"].as_str().unwrap_or("")];
            item["request_id"]==self.request_id && item["archived"]==true && item["stage"]=="handoff"
                && item["superseded_by_advice_id"].is_null()
                && !matches!(item["disposition"].as_str(),Some("resolved"|"declined"))
                && !frame.is_null() && frame["status"]=="done" && frame["invalidated_by_plan_revision"].is_null()
                && identity["revision"]==frame["order"]["revision"] && identity["plan_revision"]==frame["order"]["plan_revision"]
                && identity["node_id"]==frame["order"]["node_id"]
                && !self.items.values().any(|adopted|adopted["origin_advice_id"]==item["id"] && self.is_current(adopted))
        }).rev().take(4).map(|item|json!({"request_id":self.request_id,"plan_revision":self.scope["plan_revision"],
            "source_advice_id":item["id"],"source_review_id":item["review_id"],"source_identity":item["identity"],
            "summary":item["summary"],"suggestions":item["suggestions"],"requires_explicit_readoption":true})).collect()
    }
    pub fn respond_for_decision(&mut self,args:&Value,decision:&Value,scheduler:&crate::work_scheduler::WorkScheduler)->anyhow::Result<Vec<Value>> {
        let responses=args["responses"].as_array().ok_or_else(||anyhow::anyhow!("responses must be an array"))?;
        for response in responses {
            if !matches!(response["disposition"].as_str(),Some("accepted"|"adjusted")){continue;}
            let application=&response["application"];
            let field=application["field"].as_str().unwrap_or("");
            let implemented=if field=="revisit" {decision["action"]=="revisit" && application["value"]==decision["target_node_id"]}
                else if field=="summary" {decision["action"]=="finish" && application["value"]==decision["summary"]}
                else {
                    let work=application["work_id"].as_str().unwrap_or("");
                    matches!(field,"constraints"|"finding_ids"|"material_ids"|"upstream_ids"|"dependency_inputs"|"goal")
                        && scheduler.frames.get(work).is_some_and(|f|serde_json::to_value(&f.order).ok().is_some_and(|o|!application["value"].is_null() && o[field]==application["value"]))
                };
            anyhow::ensure!(implemented,"accepted/adjusted Observer advice must name an actual decision/order field and its implemented value");
        }
        let mut staged=self.clone();let mut scoped_args=args.clone();
        for response in scoped_args["responses"].as_array_mut().unwrap() {
            if response["adopt_to_current_plan"]!=true {continue;}
            anyhow::ensure!(matches!(response["disposition"].as_str(),Some("accepted"|"adjusted")),"readoption requires a concrete accepted/adjusted decision");
            let id=response["id"].as_str().unwrap_or("");
            let mut item=staged.items.get(id).cloned().ok_or_else(||anyhow::anyhow!("unknown historical advice"))?;
            anyhow::ensure!(item["request_id"]==staged.request_id && item["archived"]==true,"only an archived version of this same request can be explicitly readopted");
            let work=response["application"]["work_id"].as_str().unwrap_or(scheduler.id());
            anyhow::ensure!(scheduler.frames.contains_key(work),"readoption must target a current work instance");
            item["origin_advice_id"]=json!(id);
            item["adopted_identity"]=crate::observer_service::identity(item["identity"]["task_id"].as_str().unwrap_or(""),scheduler,work);
            staged.counter+=1;let new_id=format!("advice_{}",staged.counter);
            item["id"]=json!(new_id);item["archived"]=json!(false);item["delivered"]=json!(true);item["disposition"]=json!("unread");
            staged.items.insert(new_id.clone(),item);response["id"]=json!(new_id);
        }
        let mut updates=staged.respond(&scoped_args)?;
        for item in &mut updates {
            item["decision_id"]=decision["decision_id"].clone();
            if let Some(response)=scoped_args["responses"].as_array().unwrap().iter().find(|r|r["id"]==item["id"]) {item["application"]=response["application"].clone();}
            staged.items.insert(item["id"].as_str().unwrap().to_owned(),item.clone());
        }*self=staged;Ok(updates)
    }
    /// A request keeps its identity across turns and plan revisions. Old advice
    /// remains inspectable history, including observations that arrive late.
    pub fn start_request(&mut self, request_id: usize) {
        for item in self.items.values_mut() {
            if item["request_id"].is_null() {
                item["request_id"] = json!(if self.request_id == 0 { request_id } else { self.request_id });
            }
            if item["request_id"] != request_id {
                item["archived"] = json!(true);
            }
        }
        self.request_id = request_id;
    }
    fn is_current(&self, item: &Value) -> bool {
        item["request_id"] == self.request_id && item["archived"] != true
            && (item["identity"].is_null() || self.scope.is_null() || crate::observer_service::applies(item.get("adopted_identity").unwrap_or(&item["identity"]),&self.scope))
    }
    pub fn decisions(&self, node: &str) -> Vec<Value> {
        self.items.values().filter(|item|self.is_current(item) && item["disposition"]!="unread" && item["node_id"]==node)
            .rev().take(4).map(|item|json!({"id":item["id"],"disposition":item["disposition"],"decision":item["reason"],
                "latest_note":short(item["summary"].as_str().unwrap_or(""),200)})).collect()
    }
    pub fn start_turn(&mut self) {
        self.items.retain(|_, item| item["archived"] == true || !item["review_id"].is_null() || (item["disposition"] != "resolved" && item["disposition"] != "declined"));
    }
    pub fn insert(&mut self, review: &Value) {
        let key = review["issue_key"].as_str().unwrap_or("general_direction");
        let category=review["category"].as_str().unwrap_or("other");
        let request_id = review["request_id"].as_u64().map(|id|id as usize).unwrap_or(self.request_id);
        // Explicit readoption copies belong to a different current plan; only
        // source advice versions participate in this source's replacement chain.
        let matching=self.items.values().filter(|item|item["origin_advice_id"].is_null() && same_advice_issue(item,review,request_id)).collect::<Vec<_>>();
        let observed=self.observations.iter().chain(matching.iter().copied()).filter(|item|same_advice_issue(item,review,request_id)).collect::<Vec<_>>();
        let replayed=observed.iter().any(|item|["review_id","source_event_id"].iter().any(|field|
            review[*field].as_str().is_some_and(|id|!id.is_empty() && item[*field]==id)));
        if replayed || observed.iter().any(|item|advice_source_is_not_newer(review,item)) {return;}
        let previous=matching.into_iter().max_by_key(|item|item["id"].as_str().and_then(|id|id.strip_prefix("advice_")).and_then(|id|id.parse::<usize>().ok()).unwrap_or(0)).cloned();
        // Track even unchanged reminders: after restore their older source must
        // not roll back a later judgment merely because no receipt was created.
        if ["review_id","source_event_id"].iter().any(|field|review[*field].as_str().is_some_and(|id|!id.is_empty()))
            || review["source_event_seq"].is_u64() || (review["turn"].is_u64() && review["step"].is_u64()) {
            self.observations.push(json!({"request_id":request_id,"identity":review["identity"],"issue_key":key,"category":category,"node_id":review["node_id"],
                "review_id":review["review_id"],"source_event_id":review["source_event_id"],"source_event_seq":review["source_event_seq"],
                "turn":review["turn"],"step":review["step"],"stage":review["stage"]}));
        }
        let content=advice_content(review);
        // Source/stage changes alone do not reopen an already handled reminder.
        // Only the latest judgment can suppress duplicates; A → B → A is new.
        if previous.as_ref().is_some_and(|item|advice_content(item)==content) {return;}
        let previous=previous.map(|item|item["id"].clone());
        self.counter += 1;
        let id = format!("advice_{}", self.counter);
        if let Some(previous)=previous.as_ref().and_then(Value::as_str).and_then(|id|self.items.get_mut(id)) {
            previous["archived"]=json!(true);previous["superseded_by_advice_id"]=json!(id);
        }
        self.items.insert(id.clone(), json!({"id":id,"issue_key":key,
            "request_id":request_id,"identity":review["identity"],"review_id":review["review_id"],"source_event_id":review["source_event_id"],"source_event_seq":review["source_event_seq"],"turn":review["turn"],"stage":review["stage"],
            "observed_at":review["observed_at"],"execution_revision":review["execution_revision"],
            "archived":request_id != self.request_id || (!review["identity"].is_null() && !self.scope.is_null() && !crate::observer_service::applies(&review["identity"],&self.scope)),
            "category":category,"review_status":review["review_status"],"visual_artifacts":review["visual_artifacts"],"visual_check_result":review["visual_check_result"],"visual_request":review["visual_request"],
            "observer_return":review["observer_return"],
            "summary":review["summary"],"suggestions":review["suggestions"],"findings":review["findings"],"target":review["target"],"supersedes_advice_id":previous,
            "origin_step":review["step"],"latest_step":review["step"],"node_id":review["node_id"],
            "delivered":false,"disposition":"unread","reason":""}));
    }

    pub fn deliver(&mut self) -> Vec<Value> {
        let mut newly = Vec::new();
        for item in self.items.values_mut().filter(|item|item["request_id"] == self.request_id && item["archived"] != true && item["disposition"] == "unread").take(8) {
            if item["delivered"] != true {
                item["delivered"] = json!(true);
                newly.push(item.clone());
            }
        }
        newly
    }

    pub fn all_unread(&self) -> Vec<String> {
        self.items.values().filter(|item|self.is_current(item) && item["disposition"] == "unread" && item["organizer_consumption"].is_null())
            .filter_map(|item|item["id"].as_str().map(str::to_owned)).collect()
    }

    /// The next normal Organizer decision is the consumption point for delivered
    /// Observer advice. Recording it on the host avoids a separate acknowledge
    /// round and keeps the original advice plus the consuming decision auditable.
    pub fn record_organizer_consumption(&mut self, advice_ids:&[String],decision:&Value,turn:usize,step:usize)->Vec<Value> {
        if !matches!(decision["action"].as_str(),Some("work"|"select"|"continue"|"revisit"|"finish"|"blocked")) {return Vec::new();}
        let action=decision["action"].as_str().unwrap_or("");
        let work_ids=decision["orders"].as_array().into_iter().flatten().filter_map(|order|order["id"].as_str()).collect::<Vec<_>>();
        let mut consumed=Vec::new();
        for id in advice_ids {
            let Some(item)=self.items.get_mut(id) else {continue;};
            if item["delivered"]!=true||item["disposition"]!="unread"||!item["organizer_consumption"].is_null() {continue;}
            item["organizer_consumption"]=json!({"turn":turn,"step":step,"decision_id":decision["decision_id"],
                "action":action,"work_ids":work_ids});
            consumed.push(item.clone());
        }
        consumed
    }

    pub fn snapshot(&self) -> Value { serde_json::to_value(self).unwrap_or_else(|_|json!({})) }

    pub fn pending(&self) -> Vec<Value> {
        self.items.values().filter(|item|self.is_current(item) && item["disposition"] != "resolved" && item["disposition"] != "declined")
            .filter(|item|item["disposition"] != "unread" || item["delivered"] == true).cloned().collect()
    }

    pub fn respond(&mut self, args: &Value) -> anyhow::Result<Vec<Value>> {
        let responses = args["responses"].as_array().ok_or_else(|| anyhow::anyhow!("responses must be an array"))?;
        let mut updates = Vec::new();
        let mut seen_ids = std::collections::BTreeSet::new();
        for response in responses {
            let id = response["id"].as_str().unwrap_or("");
            let disposition = response["disposition"].as_str().unwrap_or("");
            let reason = response["reason"].as_str().unwrap_or("").trim();
            anyhow::ensure!(self.items.get(id).is_some_and(|item|self.is_current(item) && item["delivered"] == true), "unknown, archived or undelivered advice: {id}");
            anyhow::ensure!(matches!(disposition, "accepted" | "adjusted" | "declined" | "resolved"), "invalid disposition");
            anyhow::ensure!(!reason.is_empty(), "explain how you will act, why you disagree, or how the issue was resolved");
            anyhow::ensure!(seen_ids.insert(id), "duplicate advice id in one response: {id}");
            // Receiving an already-read reminder does not create a new message.
            // Rephrasing acceptance is not work and must not produce another receipt.
            if self.items[id]["disposition"] == disposition { continue; }
            anyhow::ensure!(self.items[id]["disposition"] != "resolved" && self.items[id]["disposition"] != "declined",
                "advice {id} is already closed; proceed with the current task");
            let mut item = self.items[id].clone();
            item["disposition"] = json!(disposition);
            item["reason"] = json!(short(reason, 700));
            updates.push(item);
        }
        for item in &updates { self.items.insert(item["id"].as_str().unwrap().to_owned(), item.clone()); }
        Ok(updates)
    }
}

#[cfg(test)]
mod observer_request_tests {
    use super::*;

    fn review(request_id: usize, key: &str, summary: &str) -> Value {
        json!({"request_id":request_id,"issue_key":key,"category":"sufficiency","node_id":"reused_node",
            "step":1,"summary":summary,"suggestions":[summary]})
    }

    #[test]
    fn replacement_archives_accepted_advice_and_late_results_without_cross_request_merge() {
        let mut inbox = ObserverInbox::default();
        inbox.start_request(1);
        inbox.insert(&review(1,"same_issue","old suggestion"));
        let id=inbox.deliver()[0]["id"].clone();
        inbox.respond(&json!({"responses":[{"id":id,"disposition":"accepted","reason":"will act"}]})).unwrap();
        inbox.start_turn();
        inbox.start_request(1);
        assert_eq!(inbox.pending().len(),1,"continuation keeps accepted advice");
        inbox.start_request(2);
        assert!(inbox.pending().is_empty());
        assert!(inbox.decisions("reused_node").is_empty());
        assert!(inbox.respond(&json!({"responses":[{"id":id,"disposition":"resolved","reason":"new goal"}]})).is_err());
        inbox.insert(&review(2,"same_issue","new suggestion"));
        inbox.insert(&review(1,"same_issue","late old clarification"));
        let mut distinct=review(1,"late_issue","late distinct old suggestion");
        distinct["category"]=json!("scope");
        inbox.insert(&distinct);
        assert_eq!(inbox.deliver().len(),1);
        assert_eq!(inbox.pending()[0]["summary"],"new suggestion");
        assert_eq!(inbox.all_unread().len(),1);
        assert_eq!(inbox.snapshot()["items"].as_object().unwrap().len(),4);
        assert_eq!(inbox.snapshot()["items"][id.as_str().unwrap()]["summary"],"old suggestion");
        assert_eq!(inbox.snapshot()["items"][id.as_str().unwrap()]["disposition"],"accepted");
        let mut restored:ObserverInbox=serde_json::from_value(inbox.snapshot()).unwrap();
        restored.start_turn();
        assert_eq!(restored.pending().len(),1);
        assert_eq!(restored.snapshot()["items"].as_object().unwrap().len(),4);
    }

    #[test]
    fn organizer_decision_consumes_delivered_advice_without_a_response_round() {
        let mut inbox=ObserverInbox::default();inbox.start_request(7);
        inbox.insert(&review(7,"scope","Pass the service URL to the font check"));
        let delivered=inbox.deliver();let ids=delivered.iter().filter_map(|item|item["id"].as_str().map(str::to_owned)).collect::<Vec<_>>();
        assert_eq!(inbox.unhandled().len(),1);
        let decision=json!({"decision_id":"task:7:2","action":"work","orders":[{"id":"work_7_2"}]});
        let consumed=inbox.record_organizer_consumption(&ids,&decision,7,2);
        assert_eq!(consumed.len(),1);
        assert_eq!(consumed[0]["organizer_consumption"]["decision_id"],"task:7:2");
        assert!(inbox.unhandled().is_empty());
        assert!(inbox.all_unread().is_empty());
        assert_eq!(inbox.snapshot()["items"][ids[0].as_str()]["summary"],"Pass the service URL to the font check");
    }

    #[test]
    fn changed_handoff_advice_after_handling_reaches_organizer_with_new_provenance() {
        for disposition in ["accepted","resolved","declined"] {
            let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
            scheduler.apply(&json!({"action":"work","reason":"initial","orders":[{"id":"w","node_id":"n","goal":"work","done_when":"delivered","completion":"output","constraints":["reuse sealed inputs"]}]}),false,false).unwrap();
            let mut inbox=ObserverInbox::default();inbox.set_scope(&scheduler);
            let mut assignment=review(1,"route","reuse sealed inputs");assignment["category"]=json!("planning");
            assignment["identity"]=crate::observer_service::identity("task",&scheduler,"w");
            assignment["review_id"]=json!("assignment_review");assignment["source_event_id"]=json!("assignment_source");assignment["stage"]=json!("assignment");
            inbox.insert(&assignment);let old=inbox.deliver()[0].clone();
            let responses=inbox.respond_for_decision(&json!({"responses":[{"id":old["id"],"disposition":disposition,"reason":"handled this actual decision",
                "application":{"work_id":"w","field":"constraints","value":["reuse sealed inputs"]}}]}),&json!({"action":"work","decision_id":"first_decision"}),&scheduler).unwrap();
            let handled=responses[0].clone();inbox.start_turn();
            let mut reminder=assignment.clone();reminder["review_id"]=json!("reminder_review");reminder["source_event_id"]=json!("reminder_source");reminder["stage"]=json!("progress");reminder["step"]=json!(2);
            // Identical reminder at a new boundary must not demand another receipt.
            inbox.insert(&reminder);assert!(inbox.deliver().is_empty());assert!(inbox.unhandled().is_empty());
            let mut handoff=reminder.clone();handoff["review_id"]=json!("handoff_review");handoff["source_event_id"]=json!("handoff_source");handoff["stage"]=json!("handoff");handoff["step"]=json!(3);
            handoff["summary"]=json!("delivery exposes an upstream defect");handoff["suggestions"]=json!(["consider upstream revisit before downstream work"]);
            inbox.insert(&handoff);let delivered=inbox.deliver();assert_eq!(delivered.len(),1);
            let pending=inbox.unhandled();assert_eq!(pending.len(),1);let current=&pending[0];
            assert_ne!(current["id"],handled["id"]);assert_eq!(current["disposition"],"unread");
            assert_eq!(current["review_id"],"handoff_review");assert_eq!(current["source_event_id"],"handoff_source");assert_eq!(current["stage"],"handoff");
            assert_eq!(current["supersedes_advice_id"],handled["id"]);assert!(current["application"].is_null());
            let historical=inbox.snapshot()["items"][handled["id"].as_str().unwrap()].clone();
            for field in ["summary","suggestions","disposition","review_id","source_event_id","stage","application","decision_id"] {assert_eq!(historical[field],handled[field]);}
            assert_eq!(historical["archived"],true);
            inbox.insert(&handoff);assert!(inbox.deliver().is_empty());assert_eq!(inbox.unhandled().len(),1);
            let mut restored:ObserverInbox=serde_json::from_value(inbox.snapshot()).unwrap();restored.start_turn();assert_eq!(restored.unhandled()[0]["review_id"],"handoff_review");
        }
    }

    #[test]
    fn returning_to_historical_advice_creates_a_new_unread_version() {
        for disposition in ["accepted","resolved","declined"] {
            let mut inbox=ObserverInbox::default();inbox.start_request(1);
            let mut a=review(1,"route","inputs are sufficient; implement");a["category"]=json!("planning");
            a["identity"]=json!({"task_id":"task","request_id":1,"work_id":"w","node_id":"n","revision":1,"plan_revision":1});
            a["review_id"]=json!("review_1");a["source_event_id"]=json!("source_1");a["source_event_seq"]=json!(10);a["turn"]=json!(1);a["stage"]=json!("assignment");
            inbox.insert(&a);let first=inbox.deliver()[0].clone();
            inbox.respond(&json!({"responses":[{"id":first["id"],"disposition":disposition,"reason":"first decision"}]})).unwrap();
            let mut b=a.clone();b["summary"]=json!("source mapping is incomplete; investigate");b["suggestions"]=json!(["inspect the missing mapping"]);
            b["review_id"]=json!("review_2");b["source_event_id"]=json!("source_2");b["source_event_seq"]=json!(20);b["step"]=json!(2);b["stage"]=json!("progress");
            inbox.insert(&b);let second=inbox.deliver()[0].clone();
            inbox.respond(&json!({"responses":[{"id":second["id"],"disposition":disposition,"reason":"second decision"}]})).unwrap();
            let before=inbox.snapshot();
            let mut third=a.clone();third["review_id"]=json!("review_3");third["source_event_id"]=json!("source_3");third["source_event_seq"]=json!(30);third["step"]=json!(3);third["stage"]=json!("handoff");
            inbox.insert(&third);
            let delivered=inbox.deliver();assert_eq!(delivered.len(),1,"A → B → A must reach Organizer after {disposition}");
            assert_eq!(inbox.unhandled().len(),1);let current=&inbox.unhandled()[0];
            assert_eq!(current["review_id"],"review_3");assert_eq!(current["source_event_id"],"source_3");assert_eq!(current["disposition"],"unread");
            assert_eq!(current["source_event_seq"],30);assert_eq!(current["turn"],1);
            assert_eq!(current["summary"],first["summary"]);assert_eq!(current["supersedes_advice_id"],second["id"]);
            let after=inbox.snapshot();assert_eq!(after["counter"],3);
            for id in [first["id"].as_str().unwrap(),second["id"].as_str().unwrap()] {
                for field in ["summary","suggestions","disposition","reason","review_id","source_event_id","stage"] {assert_eq!(after["items"][id][field],before["items"][id][field]);}
                assert_eq!(after["items"][id]["archived"],true);
            }
        }
    }

    #[test]
    fn replay_and_out_of_order_advice_cannot_roll_back_the_latest_judgment_after_restore() {
        for with_sequence in [true,false] {
            let mut inbox=ObserverInbox::default();inbox.start_request(1);
            let make=|id:&str,seq:u64,turn:u64,step:u64,summary:&str| {
                let mut item=review(1,"route",summary);item["category"]=json!("planning");item["stage"]=json!("progress");
                item["review_id"]=json!(format!("review_{id}"));item["source_event_id"]=json!(format!("source_{id}"));
                item["turn"]=json!(turn);item["step"]=json!(step);
                if with_sequence {item["source_event_seq"]=json!(seq);}item
            };
            let a=make("a",10,1,1,"A");inbox.insert(&a);let first=inbox.deliver()[0].clone();
            inbox.respond(&json!({"responses":[{"id":first["id"],"disposition":"resolved","reason":"handled A"}]})).unwrap();
            let reminder=make("reminder",15,1,2,"A");inbox.insert(&reminder);assert!(inbox.deliver().is_empty());
            let b=make("b",20,1,3,"B");inbox.insert(&b);let second=inbox.deliver()[0].clone();
            let mut inbox:ObserverInbox=serde_json::from_value(inbox.snapshot()).unwrap();inbox.start_turn();
            // Old sources, including an A reminder that never created an advice item.
            for old in [&a,&reminder,&b] {inbox.insert(old);assert!(inbox.deliver().is_empty());}
            let late=make("late",18,1,2,"late different content");inbox.insert(&late);assert!(inbox.deliver().is_empty());
            // Identity alone must reject replay even if its ordering metadata changes.
            let mut replay=make("reminder",50,3,1,"forged newer content");replay["source_event_id"]=json!("different_source");inbox.insert(&replay);
            replay["review_id"]=json!("different_review");replay["source_event_id"]=reminder["source_event_id"].clone();inbox.insert(&replay);
            assert!(inbox.deliver().is_empty());assert_eq!(inbox.unhandled()[0]["id"],second["id"]);assert_eq!(inbox.snapshot()["counter"],2);
            let third=make("third",30,1,4,"A");inbox.insert(&third);let third_advice=inbox.deliver()[0].clone();
            assert_eq!(third_advice["supersedes_advice_id"],second["id"]);
            // A → A in a new turn advances the replay watermark without a receipt.
            let same=make("same",40,2,1,"A");inbox.insert(&same);assert!(inbox.deliver().is_empty());
            let mut restored:ObserverInbox=serde_json::from_value(inbox.snapshot()).unwrap();
            restored.insert(&make("older_than_same",35,1,5,"D"));assert!(restored.deliver().is_empty());
            assert_eq!(restored.unhandled()[0]["id"],third_advice["id"]);assert_eq!(restored.snapshot()["counter"],3);
            // Recommendations from one review can address separate issues.
            let mut other=same.clone();other["issue_key"]=json!("other_issue");other["summary"]=json!("another issue");
            restored.insert(&other);assert_eq!(restored.deliver().len(),1);assert_eq!(restored.unhandled().len(),2);
            // Snapshots predating the ledger still use existing advice provenance.
            let mut legacy=inbox.snapshot();legacy.as_object_mut().unwrap().remove("observations");
            let mut legacy:ObserverInbox=serde_json::from_value(legacy).unwrap();legacy.insert(&a);legacy.insert(&late);
            assert!(legacy.deliver().is_empty());assert_eq!(legacy.snapshot()["counter"],3);
        }
    }

    #[test]
    fn node_versions_are_isolated_and_explicit_readoption_records_actual_application() {
        let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
        scheduler.apply(&json!({"action":"work","reason":"initial","orders":[{"id":"w","node_id":"n","goal":"bounded work","done_when":"delivered","completion":"output"}]}),false,false).unwrap();
        let mut inbox=ObserverInbox::default();inbox.set_scope(&scheduler);
        let mut old=review(1,"issue","old advice");old["node_id"]=json!("n");old["identity"]=crate::observer_service::identity("task",&scheduler,"w");
        inbox.insert(&old);let old_id=inbox.deliver()[0]["id"].clone();
        scheduler.plan_revision=2;let order=&mut scheduler.frames.get_mut("w").unwrap().order;order.revision=2;order.plan_revision=2;order.constraints=vec!["reuse old suggestion explicitly".into()];
        inbox.set_scope(&scheduler);assert!(inbox.pending().is_empty());
        let mut current=old.clone();current["identity"]=crate::observer_service::identity("task",&scheduler,"w");current["summary"]=json!("new advice");
        inbox.insert(&current);assert_eq!(inbox.deliver().len(),1);
        let before=inbox.snapshot();let decision=json!({"action":"work","decision_id":"decision_2"});
        assert!(inbox.respond_for_decision(&json!({"responses":[{"id":old_id,"disposition":"accepted","reason":"acknowledged"}]}),&decision,&scheduler).is_err());
        assert_eq!(before,inbox.snapshot());
        let responses=inbox.respond_for_decision(&json!({"responses":[{"id":old_id,"disposition":"accepted","reason":"implemented in current order","adopt_to_current_plan":true,
            "application":{"work_id":"w","field":"constraints","value":["reuse old suggestion explicitly"]}}]}),&decision,&scheduler).unwrap();
        assert_eq!(responses[0]["identity"]["revision"],1);assert_eq!(responses[0]["adopted_identity"]["revision"],2);assert_eq!(responses[0]["decision_id"],"decision_2");
        assert_eq!(inbox.snapshot()["items"][old_id.as_str().unwrap()]["archived"],true);
        assert_eq!(inbox.unhandled().len(),1);
        let mut late=old.clone();late["summary"]=json!("new judgment for the original instance");late["review_id"]=json!("late_original_review");
        inbox.insert(&late);
        assert_eq!(inbox.snapshot()["items"][responses[0]["id"].as_str().unwrap()]["archived"],false,"late source versions cannot archive an explicitly adopted current decision");
        assert_eq!(inbox.unhandled().len(),1);
    }

    #[test]
    fn late_delivery_notifies_current_plan_without_relabeling_or_reviving_invalidated_work() {
        let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
        scheduler.apply(&json!({"action":"work","reason":"upstream","orders":[{"id":"w","node_id":"n","goal":"deliver","done_when":"sealed","completion":"output"}]}),false,false).unwrap();
        scheduler.frames.get_mut("w").unwrap().status=crate::work_scheduler::WorkStatus::Done;
        let mut inbox=ObserverInbox::default();inbox.set_scope(&scheduler);
        let mut old=review(1,"upstream_issue","sealed input may need repair");old["stage"]=json!("handoff");
        old["identity"]=crate::observer_service::identity("task",&scheduler,"w");old["review_id"]=json!("review_w");
        scheduler.plan_revision+=1;inbox.set_scope(&scheduler);inbox.insert(&old);inbox.insert(&old);
        assert!(inbox.pending().is_empty());
        let notices=inbox.late_delivery_notifications();assert_eq!(notices.len(),1);
        assert_eq!(notices[0]["plan_revision"],scheduler.plan_revision);
        assert_eq!(notices[0]["source_identity"],old["identity"]);
        assert_eq!(notices[0]["requires_explicit_readoption"],true);
        scheduler.frames.get_mut("w").unwrap().invalidated_by_plan_revision=Some(scheduler.plan_revision);inbox.set_scope(&scheduler);
        assert!(inbox.late_delivery_notifications().is_empty());
        scheduler.frames.get_mut("w").unwrap().invalidated_by_plan_revision=None;scheduler.frames.get_mut("w").unwrap().order.revision+=1;inbox.set_scope(&scheduler);
        assert!(inbox.late_delivery_notifications().is_empty());
        scheduler.frames.get_mut("w").unwrap().order.revision-=1;scheduler.request_started_turn=2;inbox.set_scope(&scheduler);
        assert!(inbox.late_delivery_notifications().is_empty());
    }

    #[test]
    fn legacy_inbox_is_bound_once_and_then_archived_on_new_request() {
        let mut inbox:ObserverInbox=serde_json::from_value(json!({"counter":1,"items":{"advice_1":{
            "id":"advice_1","issue_key":"legacy","node_id":"direct","disposition":"unread","delivered":true}}})).unwrap();
        inbox.start_request(1);
        assert_eq!(inbox.pending().len(),1);
        inbox.start_request(2);
        assert!(inbox.pending().is_empty());
        assert_eq!(inbox.snapshot()["items"]["advice_1"]["archived"],true);
    }
}
