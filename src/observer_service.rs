//! End-of-execution retrospectives over the shared original conversation.
//! Observer has no running review, advisory queue or scheduling authority.
use crate::agent_service::{self, AgentServiceState};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path,PathBuf};
use tokio_util::sync::CancellationToken;

// Current task view; 256k is a maximum, not a target snapshot size.
pub const INPUT_BUDGET: usize = crate::context_window::MAX_TOKENS;

pub fn identity(task: &str, scheduler: &crate::work_scheduler::WorkScheduler, work: &str) -> Value {
    let order = scheduler.frames.get(work).map(|frame| &frame.order);
    json!({"task_id":task,"request_id":scheduler.request_started_turn,"work_id":work,
        "node_id":order.map(|o|&o.node_id),"revision":order.map(|o|o.revision),"plan_revision":order.map(|o|o.plan_revision)})
}

#[cfg(test)]
pub fn applies(identity: &Value, scope: &Value) -> bool {
    valid_instance(identity, scope) && identity["plan_revision"] == scope["plan_revision"]
}

#[cfg(test)]
fn observation_version_key(input:&Value)->String {
    let identity=&input["identity"];
    let instance=format!("{}:{}:{}:{}:{}",identity["task_id"],identity["request_id"],identity["work_id"],identity["revision"],identity["plan_revision"]);
    if input["turn"].is_u64() && input["step"].is_u64() {
        return format!("iteration:{instance}:{}:{}",input["turn"],input["step"]);
    }
    format!("review:{}",input["review_id"].as_str().unwrap_or(""))
}

#[cfg(test)]
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

/// Advice dates describe provenance; the recipient judges applicability.
#[cfg(test)]
pub fn annotate_advice(items: Vec<Value>, _scheduler: &crate::work_scheduler::WorkScheduler) -> Vec<Value> { items }

/// Forward original method arguments and sender returns without field cuts.
#[cfg(test)]
pub fn pack(input: Value, _path: &[Value], _memories: &[Value]) -> Value { input }

#[cfg(test)]
pub fn observation(
    task: &str,
    scheduler: &crate::work_scheduler::WorkScheduler,
    work: &str,
    turn: usize,
    step: usize,
    stage: &str,
    prompt: &str,
    decision: &Value,
    _activity: Value,
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
        "request":{"goal":prompt},
        "organizer_decision":decision,"current_node":{"goal":order.map(|o|&o.goal),"done_when":order.map(|o|&o.done_when),"constraints":order.map(|o|&o.constraints),"context":order.map(|o|&o.context)},
        "execution_epoch":frame.map(|f|f.epoch),"related_source_versions":frame.map(|f|&f.versions),
        "visual_required":order.is_some_and(|o|o.visual_goal.is_some() || o.constraints.iter().any(|c|c=="requires_visual")),
        "visual_artifact_ids":frame.map(|f|&f.visual_artifact_ids),"host_current_artifact_ids":frame.map(|f|&f.current_visual_artifact_ids),"visual_check_result":frame.map(|f|&f.visual_check_result),"task_page":frame.map(|f|&f.browser_page),"allowed_visual_artifact_ids":allowed,
        "resolved_inputs":packet["upstream_outputs"],"materials":materials,
        "observed_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
        "delivery":delivery,
        "defer_until_handoff":stage=="assignment" && order.is_some_and(|o|o.final_answer && o.completion==crate::work_scheduler::Completion::Output)})
}

/// Persist the execution version atomically; this does not schedule Observer.
pub async fn commit(root: &Path, task: &str, data: Value, observations: Vec<Value>) -> Result<i64> {
    #[cfg(not(test))]
    let _ = observations;
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
        #[cfg(test)]
        for mut input in observations {
            // Assignment messages are already in the task conversation. Observe
            // only after the execution iteration has produced its results.
            if input["stage"] == "assignment" { continue; }
            input["source_event_seq"]=json!(source_seq);
            let inserted=tx.execute("INSERT OR IGNORE INTO agent_observations(task_id,review_id,request_id,input,status) VALUES (?1,?2,?3,?4,'pending')",
                params![task,input["review_id"].as_str(),input["identity"]["request_id"].as_u64(),input.to_string()])?;
            if inserted>0 {agent_service::append_event_tx(&tx,&task,"observer/node_review",&event(&input,"pending",Value::Null),None)?;}
        }
        tx.commit()?;Ok(source_seq)
    }).await?
}

#[cfg(test)]
fn event(input: &Value, status: &str, result: Value) -> Value {
    json!({"review_id":input["review_id"],"identity":input["identity"],"source_event_id":input["source_event_id"],
        "organizer_decision_id":input["organizer_decision_id"],"stage":input["stage"],"turn":input["turn"],"step":input["step"],
        "nodeId":input["identity"]["node_id"],"workId":input["identity"]["work_id"],"status":status,
        "decision":input["organizer_decision"],"delivery":input["delivery"],"result":result})
}

#[cfg(test)]
async fn records(root: PathBuf, task: String) -> Result<Vec<(Value, String, Value)>> {
    tokio::task::spawn_blocking(move ||->Result<_> {
        let conn=agent_service::open_db(&root)?;
        let mut stmt=conn.prepare("SELECT input,status,COALESCE(result,'null') FROM agent_observations WHERE task_id=?1 ORDER BY rowid")?;
        let rows=stmt.query_map([task],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?;
        rows.map(|r|{let (i,s,v)=r?;Ok((serde_json::from_str(&i)?,s,serde_json::from_str(&v)?))}).collect()
    }).await?
}

#[cfg(test)]
fn normalize(raw: Value, input: &Value, elapsed: u128) -> Value {
    let mut recommendations = Vec::new();
    for item in raw
        .get("recommendations")
        .or_else(|| raw.get("suggestions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
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
        let mut recommendation=item.as_object().cloned().map(Value::Object).unwrap_or_else(||json!({"adjustment":adjustment}));
        recommendation["issue_key"]=json!(item["issue_key"].as_str().or_else(||raw["issue_key"].as_str()).filter(|key|!key.trim().is_empty()).map(str::to_owned).unwrap_or_else(||format!("recommendation_{}",crate::symbol_description::content_hash(key_text.as_bytes()))));
        if recommendation["target"].is_null() {recommendation["target"]=json!("organizer");}
        recommendations.push(recommendation);
    }
    let findings=raw["findings"].clone();
    json!({"assessment":raw["assessment"].as_str().filter(|s|matches!(*s,"on_track"|"needs_adjustment"|"uncertain")).unwrap_or("uncertain"),
        "summary":raw["summary"],"findings":findings,
        "observer_return":raw.get("observer_return").unwrap_or(&raw),
        "recommendations":recommendations,"reusable_lessons":raw["reusable_lessons"],"elapsed_ms":elapsed as u64,
        "visual_artifacts":input["visual_artifacts"],"visual_check_result":raw["visual_check_result"],"visual_request":raw["visual_request"],"visual_capture":input["visual_capture"]})
}

/// Observer is a one-shot retrospective after execution has ended.
/// There is no background reviewer, advisory queue or execution gate.
pub struct ObserverSession {
    enabled: bool,
    state: AgentServiceState,
    model: String,
    root: PathBuf,
    task: String,
    scope: Value,
    finished: bool,
}
impl ObserverSession {
    pub fn start(state:AgentServiceState,model:String,task:String,_cancel:&CancellationToken)->Self {
        Self {enabled:state.observer_enabled,root:state.workspace.root().to_path_buf(),state,model,task,scope:Value::Null,finished:false}
    }
    pub fn set_scope(&mut self,scheduler:&crate::work_scheduler::WorkScheduler,prompt:&str) {
        self.scope=json!({"request_id":scheduler.request_started_turn,"goal":prompt,
            "final_result":scheduler.final_result,"request_completed":scheduler.request_completed});
    }
    pub async fn finish(&mut self,outcome:&str)->Result<()> {
        if !self.enabled || self.finished {return Ok(());}
        self.finished=true;
        // A user cancellation or dropped execution must not start another model request.
        if !matches!(outcome,"completed"|"failed"|"max_steps") {return Ok(());}
        let history=crate::session_history::task_process(&self.root,&self.task,self.scope["request_id"].as_u64().unwrap_or(1) as usize).await?;
        let turn=history["records"].as_array().into_iter().flatten().filter_map(|record|record["turn"].as_u64()).max().unwrap_or(1);
        agent_service::emit(&self.root,&self.task,"observer/retrospective_start",
            json!({"outcome":outcome,"request_id":self.scope["request_id"],"turn":turn})).await?;
        let result=match retrospective(&self.state,&self.model,&self.task,&self.scope,outcome,history,turn).await {
            Ok(review)=>review,
            Err(error)=>json!({"status":"unavailable","outcome":outcome,"message":format!("{error:#}"),"memoryRecorded":false,"turn":turn,"request_id":self.scope["request_id"]}),
        };
        agent_service::emit(&self.root,&self.task,"observer/retrospective",result).await
    }
}

async fn retrospective(state:&AgentServiceState,model:&str,task:&str,scope:&Value,outcome:&str,task_history:Value,turn:u64)->Result<Value> {
    let input=json!({"stage":"retrospective","identity":{"task_id":task,"request_id":scope["request_id"],"node_id":"request"},
        "request":{"goal":scope["goal"]},"outcome":outcome,"goal_achieved":scope["request_completed"],
        "final_result":scope["final_result"],"task_history":task_history,
        "history":{"tool":"read_session_history","scope":"Read original process records, or include_context=true to inspect the exact saved Worker/Organizer model inputs referenced by debug/context_request. Input views are not additional executions."}});
    let raw=agent_service::observer_json_response(state,model,include_str!("../prompts/observer_retrospective.md"),input,3072,task,
        json!({"stage":"retrospective","nodeId":"request","turn":turn,"request_id":scope["request_id"]})).await?;
    let report:Value=match serde_json::from_str::<Value>(raw.trim().trim_start_matches("```json").trim_end_matches("```").trim()) {
        Ok(report) if report.is_object()=>report,
        parsed=>return Ok(json!({"status":"unavailable","turn":turn,"request_id":scope["request_id"],
            "message":parsed.err().map(|error|error.to_string()).unwrap_or_else(||"Observer retrospective must return a JSON object".into()),
            "observer_return":raw,"memoryRecorded":false})),
    };
    // Routine retrospectives belong to conversation history. Keep the explicit
    // memory tools available, but do not turn a task review into a memory write.
    let original=report.get("observer_return").unwrap_or(&report);
    let mut result=json!({"status":"completed","summary":original["summary"],"pathReview":original["path_review"],
        "shorteningOpportunities":original["shortening_opportunities"],"workFindings":original["work_findings"],
        "routeShortcuts":original["route_shortcuts"].as_array().into_iter().flatten().map(|item|{let mut item=item.clone();item["lookFirst"]=item["look_first"].clone();item}).collect::<Vec<_>>(),"memoryRecorded":false,"stored":false,"observer_return":original});
    result["turn"]=json!(turn);
    Ok(result)
}

#[cfg(test)]
fn iteration_records(process:&Value,after_seq:i64,through_seq:i64)->Vec<Value> {
    process["records"].as_array().into_iter().flatten()
        .filter(|entry|entry["seq"].as_i64().is_some_and(|seq|seq>after_seq && seq<=through_seq)
            && entry["kind"]!="observer/node_review")
        .cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn disabled_cancelled_interrupted_and_dropped_sessions_never_review() {
        let root=std::env::temp_dir().join(format!("observer-no-background-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let conn=agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','completed',0,0)",[]).unwrap();drop(conn);
        let mut state=agent_service::tests::flow_test_state(&root,"127.0.0.1:9".parse().unwrap());
        let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
        for (enabled,outcome) in [(false,"completed"),(true,"cancelled"),(true,"interrupted")] {
            state.observer_enabled=enabled;
            let mut observer=ObserverSession::start(state.clone(),"fake".into(),"task".into(),&CancellationToken::new());
            observer.set_scope(&scheduler,"goal");observer.finish(outcome).await.unwrap();
        }
        drop(ObserverSession::start(state.clone(),"fake".into(),"task".into(),&CancellationToken::new()));
        tokio::task::yield_now().await;
        let conn=agent_service::open_db(&root).unwrap();
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='agent_request_contexts')",[],|row|row.get(0)).unwrap();
        let requests:i64=if exists {conn.query_row("SELECT count(*) FROM agent_request_contexts",[],|row|row.get(0)).unwrap()}else{0};
        assert_eq!(requests,0);
        let reviews:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind LIKE 'observer/%'",[],|row|row.get(0)).unwrap();
        assert_eq!(reviews,0);
        drop(conn);std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ongoing_context_contains_new_activity_without_repeated_history() {
        let failure=json!({"capability":"unknown","image_input_unavailable":{"reason":"model has no configured image input"}});
        let recent=json!([{"seq":401,"tool":"browser_screenshot","failed":false},
            {"seq":402,"type":"visual/model_received","dispatch":failure}]);
        let packed=pack(json!({"request":{"goal":"load the presentation"},
            "node_facts":{"operations":[{"outcome":"old history".repeat(20_000)}]},
            "activity_summary":{"tools":[{"outcome":"old history".repeat(20_000)}]},
            "recent_activity":recent,"activity_since_seq":400,
            "plan_overview":[{"work_id":"earlier","status":"done"},{"work_id":"current","status":"running"}]}),&[json!({"delivery":"old original return".repeat(20_000)})],&[]);
        assert_eq!(packed["recent_activity"],recent);
        assert_eq!(packed["recent_activity"][1]["dispatch"],failure);
        assert_eq!(packed["plan_overview"].as_array().unwrap().len(),2);
        assert_eq!(packed["activity_summary"]["tools"][0]["outcome"],"old history".repeat(20_000));
        assert!(packed.get("node_operations").is_none());
        assert!(packed.get("path_summary").is_none());
        assert!(packed.to_string().chars().count()>2000,"pack must not silently discard original fields");
    }
    #[test]
    fn communication_keeps_original_returns_and_all_recommendations() {
        let worker_return=json!({"summary":"原始失败链".repeat(400),"limitations":["能力未知".repeat(400)],
            "diagnostic":{"reason":"original failure", "extra_fields":(0..20).collect::<Vec<_>>()}});
        let packed=pack(json!({"delivery":{"handoff":{"worker_return":worker_return}}}),&[],&[]);
        assert_eq!(packed["delivery"]["handoff"]["worker_return"],worker_return);
        let observer_return=json!({"summary":"Observer原始消息".repeat(400),"failure":{"reason":"unavailable model", "details":(0..20).collect::<Vec<_>>()},
            "findings":(0..5).map(|i|json!({"reason":format!("finding {i}"),"detail":"original detail"})).collect::<Vec<_>>(),
            "recommendations":(0..5).map(|i|json!({"adjustment":format!("{i}:{}","建议".repeat(800)),"extra":"original field"})).collect::<Vec<_>>()});
        let normalized=normalize(observer_return.clone(),&json!({"source_event_id":"event"}),1);
        assert_eq!(normalized["observer_return"],observer_return);
        assert_eq!(normalized["summary"],observer_return["summary"]);
        assert_eq!(normalized["recommendations"].as_array().unwrap().len(),5);
        assert_eq!(normalized["findings"].as_array().unwrap().len(),5);
        assert_eq!(normalized["recommendations"][4]["adjustment"],observer_return["recommendations"][4]["adjustment"]);
        let mut inbox=crate::worker_work_state::ObserverInbox::default();inbox.start_request(1);
        inbox.insert(&json!({"request_id":1,"summary":normalized["summary"],"suggestions":[],"observer_return":normalized["observer_return"]}));
        assert_eq!(inbox.messages()[0]["observer_return"],observer_return);
        let restored:crate::worker_work_state::ObserverInbox=serde_json::from_value(inbox.snapshot()).unwrap();
        assert_eq!(restored.messages(),inbox.messages());
    }
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
        scheduler.return_work(&json!({"summary":"Worker explicitly returned after the HTTP 200 observation."})).unwrap();
        scheduler
    }

    #[test]
    fn observer_sees_the_actual_startup_result_instead_of_a_field_catalog() {
        let scheduler = started_service();
        let wide = (0..24).map(|index| (format!("key_{index:02}"), json!(index))).collect::<serde_json::Map<_, _>>();
        let input = observation("task", &scheduler, "start", 1, 2, "handoff", "start the editor", &json!({"reason":"start"}),
            Value::Null, json!({"summary":"started","status":"done","exported_data":{"wide":wide}}), Value::Null);
        let packed = pack(input, &[], &[]);
        assert_eq!(packed["delivery"]["exported_data"]["wide"]["key_23"],23);
        assert!(packed.get("node_state").is_none(),"original calls and results are delivered by the conversation, not a generated fact catalog");
        assert!(packed.get("current_facts").is_none());
        assert!(packed["observed_at"].is_u64());
    }

    #[test]
    fn iteration_identity_survives_commit_and_stage_changes() {
        let identity=json!({"task_id":"task","request_id":7,"work_id":"fonts","revision":2,"plan_revision":4});
        let mut first=json!({"identity":identity,"turn":7,"step":2,"stage":"progress","review_id":"progress"});
        let key=observation_version_key(&first);
        first["source_event_seq"]=json!(991);
        first["stage"]=json!("handoff");
        first["review_id"]=json!("persisted");
        assert_eq!(observation_version_key(&first),key,"persisting or waking cannot invent another iteration");
        first["step"]=json!(3);
        assert_ne!(observation_version_key(&first),key,"the next real Worker round gets its own observation");
    }

    #[test]
    fn iteration_context_preserves_complete_messages_without_later_rounds() {
        let failure="failure-details".repeat(2000);
        let process=json!({"records":[
            {"seq":2,"kind":"tool/result","payload":{"old":"earlier"}},
            {"seq":3,"kind":"tool/result","payload":{"error":failure}},
            {"seq":4,"kind":"observer/node_review","payload":{"self":"review"}},
            {"seq":5,"kind":"worker/yield","payload":{"return":"complete"}},
            {"seq":6,"kind":"tool/result","payload":{"future":"later iteration"}}]});
        assert_eq!(iteration_records(&process,2,5),vec![process["records"][1].clone(),process["records"][3].clone()]);
    }


    #[test]
    fn advice_dates_are_transmitted_without_host_applicability_judgment() {
        let scheduler = started_service();
        let latest = scheduler.latest_fact_at("start");
        let advice = |observed_at: u64| json!({"id":"advice_1","identity":identity("task",&scheduler,"start"),
            "observed_at":observed_at,"summary":"the startup result lacks a status code"});
        let annotated = annotate_advice(vec![advice(latest - 5_000), advice(latest + 1)], &scheduler);
        assert_eq!(annotated[0],advice(latest-5_000));
        assert_eq!(annotated[1],advice(latest+1));
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
        assert!(packed.get("budget").is_none());
        let mut body=json!({"messages":[{"role":"system","content":"observe"},{"role":"user","content":crate::session_history::plain_context(&packed)}]});
        assert!(crate::context_window::prepare(&mut body,2).is_err());
        assert!(body["messages"][1]["content"].as_str().unwrap().contains(&goal));
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
        assert_eq!(records.len(), 3);
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
