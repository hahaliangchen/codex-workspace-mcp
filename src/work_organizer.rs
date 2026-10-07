//! Same model as Worker, separate context and a small scheduling interface.
use anyhow::Result;
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use crate::agent_service::AgentServiceState;

#[derive(Debug)]
pub struct OrganizerContractError { pub details:Value }

impl std::fmt::Display for OrganizerContractError {
    fn fmt(&self,formatter:&mut std::fmt::Formatter<'_>)->std::fmt::Result {
        write!(formatter,"{}",self.details["error"].as_str().unwrap_or("Organizer response violates its tool contract"))
    }
}
impl std::error::Error for OrganizerContractError {}

fn contract_error(rule:&str,field:&str,submitted:Option<&Value>,message:impl Into<String>)->anyhow::Error {
    anyhow::Error::new(OrganizerContractError{details:json!({"rule_id":rule,"error_code":rule,"field_path":field,
        "submitted_value_present":submitted.is_some(),"rejected_value":submitted.cloned().unwrap_or(Value::Null),"error":message.into()})})
}

pub fn contract_details(error:&anyhow::Error)->Option<&Value> {
    error.downcast_ref::<OrganizerContractError>().map(|error|&error.details)
}

pub fn yield_tool() -> Value {
    json!({"type":"function","function":{"name":"yield_work","description":"Return this work's actual output, concrete blocker, request to split, or upstream problem to Organizer. This seals the current invocation; do not promise future work.","parameters":{"type":"object","properties":{
        "summary":{"type":"string","minLength":1},"outcome":{"type":"string","enum":["completed","blocked","need_split","upstream_problem"]},
        "blocked":{"type":"boolean"},"need_split":{"type":"boolean"},
        "upstream_problem":{"type":"object","properties":{
            "node_id":{"type":"string"},"revision":{"type":"integer"},"field":{"type":"string"},"reason":{"type":"string"},"material_ids":{"type":"array","items":{"type":"integer"}}
        },"required":["node_id","reason"]},
        "findings":{"type":"array","items":{"type":"object"}},"material_ids":{"type":"array","items":{"type":"integer"}},
        "finding_ids":{"type":"array","items":{"type":"string"}},"limitations":{"type":"array","maxItems":16,"items":{"type":"string","maxLength":1200}},
        "suggested_children":{"type":"array","maxItems":16,
            "description":"For need_split, return concise child goals, dependencies and completion conditions for Organizer.",
            "items":{"type":"object","properties":{"id":{"type":"string"},"node_id":{"type":"string"},"title":{"type":"string"},
                "goal":{"type":"string","maxLength":1200},"done_when":{"type":"string","maxLength":800},
                "upstream_ids":{"type":"array","maxItems":16,"items":{"type":"string"}},
                "dependency_inputs":{"type":"array","maxItems":16,"items":{"type":"object"}},
                "depends_on":{"type":"array","items":{"type":"string"}},"dependencies":{"type":"array","items":{}}}}},
        "exported_data":{"type":"object","description":"Structured output explicitly exported for downstream work."},
        "visual_check_result":{"type":"object","description":"Only after actual image input. Copy the host request_trace_id and checked_goal. unavailable/uncertain must state limitations.","properties":{
            "artifact_ids":{"type":"array","maxItems":2,"items":{"type":"string"}},"checked_goal":{"type":"string"},"expected_visible_result":{"type":"string"},
            "request_trace_id":{"type":"string"},"assessment":{"type":"string","enum":["pass","issue","uncertain","unavailable"]},
            "observed_facts":{"type":"array","items":{}},"issues":{"type":"array","items":{}},"limitations":{"type":"array","items":{}}
        },"required":["artifact_ids","checked_goal","assessment"]}
    },"required":["summary"]}}})
}

fn request_action_schema(input:&Value)->Value {
    let allowed=input.pointer("/capabilities/allowed_request_actions").and_then(Value::as_array)
        .filter(|items|!items.is_empty()).cloned().unwrap_or_else(||vec![json!("continue")]);
    json!({"type":"string","enum":allowed,"description":"On a session_continuation handoff choose continue, subtask, or replace. Omit on an ordinary task."})
}

fn organizer_tools(input:&Value)->Vec<Value> {
    let schedule=json!({"type":"function","function":{"name":"schedule_task",
        "description":"Create exactly one new work node now. The host generates IDs, Flow events and dependency links. A service-start or HTTP/API check requires completion=check and declared checks. PPTX upload/load, in-browser font request inspection and screenshot review require completion=output with checks=[]. Split mixed task types into successive tasks.",
        "parameters":{"type":"object","properties":{
            "goal":{"type":"string","minLength":1,"maxLength":2400},"return_when":{"type":"string","minLength":1,"maxLength":1200},
            "reason":{"type":"string","minLength":1,"maxLength":1200},
            "completion":{"type":"string","enum":["output","write","check","write_check"]},
            "checks":{"type":"array","maxItems":4,"items":{"type":"string","maxLength":2000},"description":"Required for check/write_check. Supported identifiers: npm:, npm-start:, npm-install:, program:, http-probe:."},
            "inputs":{"type":"array","maxItems":16,"description":"Exact upstream work/node references and fields from available_dependency_deliveries.","items":{"type":"object","properties":{
                "work_id":{"type":"string"},"node_id":{"type":"string"},"revision":{"type":"integer"},"fields":{"type":"array","items":{"type":"string"}},
                "http_urls":{"type":"array","maxItems":16,"items":{"type":"string","maxLength":4096}}},"anyOf":[{"required":["work_id"]},{"required":["node_id"]}]}},
            "constraints":{"type":"array","maxItems":12,"items":{"type":"string","maxLength":700}},
            "execution_scope":{"type":"object","properties":{
                "edit_targets":{"type":"array","maxItems":16,"items":{"type":"string"}},"browser_document_path":{"type":"string"},
                "visual_goal":{"type":"string"},"final_answer":{"type":"boolean"},
                "material_ids":{"type":"array","maxItems":16,"items":{"type":"integer"}},
                "material_ranges":{"type":"array","maxItems":16,"items":{"type":"object","required":["id","start_line","end_line"],"properties":{"id":{"type":"integer"},"start_line":{"type":"integer"},"end_line":{"type":"integer"}}}},
                "finding_ids":{"type":"array","maxItems":16,"items":{"type":"string"}},
                "project_observation":{"type":"object"}}},
            "request_action":request_action_schema(input)
        },"required":["goal","return_when","reason","completion"]}}});
    let read=json!({"type":"function","function":{"name":"read_task_result",
        "description":"Read selected fields from a previously completed task result. Use only when the concise execution path or delivery catalog does not already answer the question. Raw materials remain in the notebook.",
        "parameters":{"type":"object","properties":{"work_id":{"type":"string"},"node_id":{"type":"string"},
            "fields":{"type":"array","maxItems":16,"items":{"type":"string"}}},
            "anyOf":[{"required":["work_id"]},{"required":["node_id"]}]}}});
    let finish=json!({"type":"function","function":{"name":"finish_request",
        "description":"Deliver the user-facing result. The task invocation may have ended without meeting the overall goal; report that honestly.",
        "parameters":{"type":"object","properties":{"summary":{"type":"string","minLength":1,"maxLength":6000},
            "achieved":{"type":"boolean"},"unresolved":{"type":"array","items":{"type":"string","maxLength":1200}},
            "reason":{"type":"string","maxLength":1200},"request_action":request_action_schema(input)},
            "required":["summary","achieved"]}}});
    let revisit=json!({"type":"function","function":{"name":"revisit_task",
        "description":"Re-execute an upstream node under a new revision. The host preserves its old result and deprecates downstream work.",
        "parameters":{"type":"object","properties":{"target_node_id":{"type":"string"},"target_revision":{"type":"integer"},
            "reason":{"type":"string","minLength":1},"repair_goal":{"type":"string","maxLength":2400},
            "replacement_checks":{"type":"array","maxItems":4,"items":{"type":"string","maxLength":2000}},
            "request_action":request_action_schema(input)},"required":["target_node_id","reason"]}}});
    vec![schedule,read,finish,revisit]
}

fn host_work_id(task_id:&str,turn:usize,step:usize)->String {
    let slug=task_id.chars().filter(|ch|ch.is_ascii_alphanumeric()||matches!(ch,'_'|'-'|'.')).take(32).collect::<String>();
    format!("task_{}_{}_{}",if slug.is_empty(){"request"}else{&slug},turn,step)
}

fn normalize_decision(name:&str,args:Value,task_id:&str,turn:usize,step:usize)->Result<Value> {
    if !args.is_object() {return Err(contract_error("INVALID_ARGUMENTS","arguments",Some(&args),"Organizer tool arguments must be an object"));}
    match name {
        "schedule_task"=>{
            for (field,rule,message) in [("goal","INVALID_TASK_GOAL","goal must be a nonempty string"),
                ("return_when","INVALID_RETURN_CONDITION","return_when must be a nonempty string"),
                ("reason","INVALID_SCHEDULING_REASON","reason must be a nonempty string")] {
                if args[field].as_str().is_none_or(|value|value.trim().is_empty()) {
                    return Err(contract_error(rule,field,args.get(field),message));
                }
            }
            let goal=args["goal"].as_str().unwrap().trim();
            let return_when=args["return_when"].as_str().unwrap().trim();
            let reason=args["reason"].as_str().unwrap().trim();
            if !matches!(args["completion"].as_str(),Some("output"|"write"|"check"|"write_check")) {
                return Err(contract_error("INVALID_COMPLETION","completion",args.get("completion"),"completion must be output, write, check, or write_check"));
            }
            if args.get("execution_scope").is_some_and(|scope|!scope.is_object()) {
                return Err(contract_error("INVALID_EXECUTION_SCOPE","execution_scope",args.get("execution_scope"),"execution_scope must be an object"));
            }
            let scope=args.get("execution_scope").cloned().unwrap_or_else(||json!({}));
            let id=host_work_id(task_id,turn,step);
            let order=json!({"id":id,"node_id":format!("work_{id}"),"goal":goal,"done_when":return_when,
                "completion":args["completion"],"checks":args.get("checks").cloned().unwrap_or_else(||json!([])),
                "constraints":args.get("constraints").cloned().unwrap_or_else(||json!([])),
                "dependency_inputs":args.get("inputs").cloned().unwrap_or_else(||json!([])),
                "edit_targets":scope.get("edit_targets").cloned().unwrap_or_else(||json!([])),
                "browser_document_path":scope.get("browser_document_path"),"visual_goal":scope.get("visual_goal"),
                "final_answer":scope.get("final_answer").cloned().unwrap_or(json!(false)),
                "material_ids":scope.get("material_ids").cloned().unwrap_or_else(||json!([])),
                "material_ranges":scope.get("material_ranges").cloned().unwrap_or_else(||json!([])),
                "finding_ids":scope.get("finding_ids").cloned().unwrap_or_else(||json!([])),
                "project_observation":scope.get("project_observation")});
            let mut decision=json!({"action":"work","reason":reason,"orders":[order]});
            if let Some(request_action)=args.get("request_action").filter(|value|!value.is_null()) {decision["request_action"]=request_action.clone();}
            if decision["request_action"]=="subtask" {decision["preserve_current"]=json!(true);}
            Ok(decision)
        },
        "read_task_result"=>{
            if args["work_id"].as_str().is_none_or(str::is_empty)&&args["node_id"].as_str().is_none_or(str::is_empty) {
                return Err(contract_error("RESULT_REFERENCE_REQUIRED","work_id",args.get("work_id"),"provide a completed work_id or node_id"));
            }
            Ok(json!({"action":"read_task_result","work_id":args["work_id"],"node_id":args["node_id"],
                "fields":args.get("fields").cloned().unwrap_or_else(||json!([]))}))
        },
        "finish_request"=>{
            let summary=args["summary"].as_str().unwrap_or("").trim();
            if summary.is_empty() {return Err(contract_error("FINISH_SUMMARY_REQUIRED","summary",args.get("summary"),"finish_request.summary is required and must be nonempty"));}
            let achieved=args["achieved"].as_bool().ok_or_else(||contract_error("FINISH_ACHIEVED_REQUIRED","achieved",args.get("achieved"),"finish_request.achieved must be a boolean"))?;
            let unresolved=args["unresolved"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::trim).filter(|s|!s.is_empty()).collect::<Vec<_>>();
            let final_summary=if achieved||unresolved.is_empty(){summary.to_owned()}else{format!("{summary}\n\n未解决：{}",unresolved.join("；"))};
            let mut decision=json!({"action":if achieved{"finish"}else{"blocked"},"achieved":achieved,
                "unresolved":unresolved,"summary":final_summary,"reason":args.get("reason").cloned().unwrap_or(json!(summary))});
            if let Some(request_action)=args.get("request_action").filter(|value|!value.is_null()) {decision["request_action"]=request_action.clone();}
            Ok(decision)
        },
        "revisit_task"=>{
            let target=args["target_node_id"].as_str().unwrap_or("").trim();
            let reason=args["reason"].as_str().unwrap_or("").trim();
            if target.is_empty() {return Err(contract_error("REVISIT_TARGET_REQUIRED","target_node_id",args.get("target_node_id"),"revisit_task.target_node_id is required"));}
            if reason.is_empty() {return Err(contract_error("REVISIT_REASON_REQUIRED","reason",args.get("reason"),"revisit_task.reason is required"));}
            let mut decision=json!({"action":"revisit","target_node_id":target,"target_revision":args["target_revision"],
                "reason":reason,"repair_goal":args.get("repair_goal"),"replacement_checks":args.get("replacement_checks")});
            if let Some(request_action)=args.get("request_action").filter(|value|!value.is_null()) {decision["request_action"]=request_action.clone();}
            Ok(decision)
        },
        _=>anyhow::bail!("unknown Organizer method: {name}"),
    }
}

pub fn is_organizer_tool(name:&str)->bool {
    matches!(name,"schedule_task"|"read_task_result"|"finish_request"|"revisit_task")
}

const MAX_METHOD_ARGUMENT_BYTES:usize=24_000;
const LIMITS:crate::model_stream::Limits=crate::model_stream::Limits{max_total_bytes:8*1024*1024,max_line_bytes:1024*1024,
    max_tool_argument_bytes:MAX_METHOD_ARGUMENT_BYTES,retain_reasoning:false};
const PROGRESS_INTERVAL:Duration=Duration::from_millis(1000);

/// Exactly one complete method from a finished assistant message.
fn decision_from_message(message:&Value,task_id:&str,turn:usize,step:usize)->Result<Value> {
    let calls=message.get("tool_calls").and_then(Value::as_array)
        .ok_or_else(||contract_error("TOOL_CALL_REQUIRED","tool_calls",Some(message),"Organizer must return one scheduling method"))?;
    if calls.len()!=1 {return Err(contract_error("ONE_METHOD_PER_DECISION","tool_calls",Some(&json!(calls)),"Organizer must return exactly one scheduling method per invocation"));}
    let name=calls[0].pointer("/function/name").and_then(Value::as_str)
        .ok_or_else(||contract_error("METHOD_REQUIRED","method",Some(&calls[0]),"Organizer method name is missing"))?;
    #[cfg(test)]
    if name=="organize_work" {
        // Legacy integration fixtures exercise the internal scheduler adapter only in tests.
        let raw=calls[0].pointer("/function/arguments").and_then(Value::as_str)
            .ok_or_else(||contract_error("ARGUMENTS_REQUIRED","arguments",Some(&calls[0]),"tool arguments are missing"))?;
        return serde_json::from_str(raw).map_err(|error|contract_error("INVALID_ARGUMENTS","arguments",Some(&json!(raw)),error.to_string()));
    }
    if !is_organizer_tool(name) {return Err(contract_error("UNSUPPORTED_METHOD","method",Some(&json!(name)),format!("Organizer returned unsupported method '{name}'")));}
    let raw=calls[0].pointer("/function/arguments").and_then(Value::as_str)
        .ok_or_else(||contract_error("ARGUMENTS_REQUIRED","arguments",Some(&calls[0]),"Organizer method arguments are missing"))?;
    if raw.len()>MAX_METHOD_ARGUMENT_BYTES {return Err(contract_error("ARGUMENTS_TOO_LARGE","arguments",Some(&json!(raw.len())),"Organizer method arguments exceed contract size"));}
    let args:Value=serde_json::from_str(raw).map_err(|error|contract_error("INVALID_ARGUMENTS","arguments",Some(&json!(raw)),error.to_string()))?;
    normalize_decision(name,args,task_id,turn,step)
}

/// A completed response is accepted only after the provider signalled its end.
fn completed_decision(completed:crate::model_stream::Completed,task_id:&str,turn:usize,step:usize)->Result<Value> {
    if !completed.terminated {
        anyhow::bail!("Organizer stream ended before [DONE] or a finish_reason; no scheduling method was applied");
    }
    if matches!(completed.stats["finish_reason"].as_str(),Some("length"|"content_filter")) {
        anyhow::bail!("Organizer response stopped early (finish_reason={}); no scheduling method was applied",completed.stats["finish_reason"]);
    }
    decision_from_message(&completed.message,task_id,turn,step)
}

enum Interrupted { Cancelled, TimedOut, Failed(anyhow::Error) }

struct Progress<'a> { state:&'a AgentServiceState, task_id:&'a str, turn:usize, step:usize, node:&'a str }

impl Progress<'_> {
    async fn emit(&self,phase:&str,stats:&Value,headers_ms:Option<u64>,elapsed_ms:u64) {
        let event=json!({"turn":self.turn,"step":self.step,"nodeId":self.node,"phase":phase,"elapsed_ms":elapsed_ms,
            "response_headers_ms":headers_ms,"first_chunk_ms":stats["first_chunk_ms"],"first_delta_ms":stats["first_delta_ms"],
            "first_tool_delta_ms":stats["first_tool_delta_ms"],"received_bytes":stats["received_bytes"],
            "reasoning_chars":stats["reasoning_chars"],"tool_argument_bytes":stats["tool_argument_bytes"]});
        if let Err(error)=crate::agent_service::emit(self.state.workspace.root(),self.task_id,"organizer/progress",event).await {
            tracing::warn!(task_id=self.task_id,%error,"could not save Organizer progress");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive(state:&AgentServiceState,body:&Value,cancel:&CancellationToken,deadline:tokio::time::Instant,started:Instant,
    accumulator:&mut crate::model_stream::ChatStream,headers_ms:&mut Option<u64>,status:&mut Option<u16>,progress:&Progress<'_>)
    ->std::result::Result<(),Interrupted> {
    let send=state.client.post(format!("{}/chat/completions",state.provider_url.trim_end_matches('/')))
        .bearer_auth(&state.api_key).json(body).send();
    let response=tokio::select! {
        biased;
        _=cancel.cancelled()=>return Err(Interrupted::Cancelled),
        _=tokio::time::sleep_until(deadline)=>return Err(Interrupted::TimedOut),
        response=send=>response.map_err(|error|Interrupted::Failed(error.into()))?,
    };
    *status=Some(response.status().as_u16());
    *headers_ms=Some(started.elapsed().as_millis() as u64);
    let response=response.error_for_status().map_err(|error|Interrupted::Failed(error.into()))?;
    let mut phase="waiting_first_delta";
    progress.emit(phase,&accumulator.stats(),*headers_ms,started.elapsed().as_millis() as u64).await;
    let mut last_progress=Instant::now();
    let mut bytes=response.bytes_stream();
    while !accumulator.received_done() {
        let chunk=tokio::select! {
            biased;
            _=cancel.cancelled()=>return Err(Interrupted::Cancelled),
            _=tokio::time::sleep_until(deadline)=>return Err(Interrupted::TimedOut),
            next=bytes.next()=>match next {
                Some(Ok(chunk))=>chunk,
                Some(Err(error))=>return Err(Interrupted::Failed(error.into())),
                None=>break,
            },
        };
        let delta=accumulator.push(&chunk).map_err(Interrupted::Failed)?;
        let next_phase=if delta.tool_call||phase=="receiving_decision" {"receiving_decision"}
            else if !delta.is_empty()||phase=="reasoning" {"reasoning"} else {phase};
        if next_phase!=phase||last_progress.elapsed()>=PROGRESS_INTERVAL {
            phase=next_phase;last_progress=Instant::now();
            progress.emit(phase,&accumulator.stats(),*headers_ms,started.elapsed().as_millis() as u64).await;
        }
    }
    Ok(())
}

/// Streams one Organizer call. Partial tool arguments are never acted on; the
/// host validates and applies the single method only after the stream ends.
#[allow(clippy::too_many_arguments)]
pub async fn decide(state:&AgentServiceState, model:&str, task_id:&str, turn:usize, step:usize, node:&str, input:Value,
    cancel:&CancellationToken, timeout:Duration) -> Result<Value> {
    let construction_started=Instant::now();
    let tools=organizer_tools(&input);
    let mut body=json!({"model":model,"stream":true,"messages":[
        {"role":"system","content":include_str!("../prompts/organizer_system.md")},
        {"role":"user","content":input.to_string()}
    ],"tools":tools,"tool_choice":"auto"});
    if let Some(effort)=state.reasoning_effort.as_deref(){body["reasoning_effort"]=json!(effort);}
    if state.fast_mode {body["service_tier"]=json!("fast");}
    let request_construction_ms=construction_started.elapsed().as_millis() as u64;
    let request_bytes=body.to_string().len();
    let metadata=json!({"actor":"organizer","stage":"organization","turn":turn,"step":step,"nodeId":node,
        "request_id":input["process"]["request_id"],"workId":input["process"]["current_work"]["id"],"revision":input["process"]["current_revision"],
        "plan_revision":input["process"]["plan_revision"],"request_construction_ms":request_construction_ms,"request_bytes":request_bytes});
    let trace=crate::request_context::record(state.workspace.root(),task_id,metadata.clone(),&body).await;
    let progress=Progress{state,task_id,turn,step,node};
    let request_started=Instant::now();
    let deadline=tokio::time::Instant::now()+timeout;
    let mut accumulator=crate::model_stream::ChatStream::new(LIMITS,request_started);
    let mut response_headers_ms=None;
    let mut response_status=None;
    progress.emit("waiting_response",&accumulator.stats(),None,0).await;
    let received=receive(state,&body,cancel,deadline,request_started,&mut accumulator,&mut response_headers_ms,&mut response_status,&progress).await;
    let mut stats=accumulator.stats();
    stats["response_complete_ms"]=Value::Null;
    let (result,outcome)=match received {
        Ok(())=>match accumulator.finish() {
            Ok(completed)=>{stats=completed.stats.clone();(completed_decision(completed,task_id,turn,step),None)},
            Err(error)=>(Err(error),None),
        },
        Err(Interrupted::Failed(error))=>(Err(error),None),
        Err(Interrupted::Cancelled)=>(Err(anyhow::anyhow!("cancelled")),Some("cancelled")),
        Err(Interrupted::TimedOut)=>(Err(anyhow::anyhow!("Organizer request timed out")),Some("timeout")),
    };
    let result=result.map_err(|error| match crate::model_stream::stream_error_code(&error) {
        Some("ARGUMENTS_TOO_LARGE")=>contract_error("ARGUMENTS_TOO_LARGE","arguments",None,"Organizer method arguments exceed contract size"),
        _=>error,
    });
    let contract_failure=result.as_ref().err().and_then(contract_details).cloned().unwrap_or(Value::Null);
    let request_failure=result.as_ref().err().filter(|_|contract_failure.is_null()).map(ToString::to_string);
    let outcome=outcome.unwrap_or(if result.is_ok(){"decision"}else if !contract_failure.is_null(){"contract_error"}else{"request_error"});
    let mut metrics=json!({"turn":turn,"step":step,"node_id":node,"request_bytes":request_bytes,
        "request_construction_ms":request_construction_ms,"response_headers_ms":response_headers_ms,
        "response_status":response_status,"response_body_bytes":stats["received_bytes"],
        "result":outcome,"contract_failure":contract_failure,"request_failure":request_failure});
    for (key,value) in stats.as_object().into_iter().flatten() {metrics[key]=value.clone();}
    if let Err(error)=crate::agent_service::emit(state.workspace.root(),task_id,"organizer/request_metrics",metrics.clone()).await {
        tracing::warn!(task_id,%error,"could not save Organizer request metrics");
    }
    let phase=match outcome {"decision"=>"completed","cancelled"=>"cancelled","timeout"=>"timeout",_=>"failed"};
    progress.emit(phase,&stats,response_headers_ms,request_started.elapsed().as_millis() as u64).await;
    let trace_status=match outcome {"decision"=>"completed","contract_error"=>"contract_error","cancelled"=>"cancelled","timeout"=>"timeout",_=>"failed"};
    crate::request_context::finish(trace,trace_status,
        json!({"decision":result.as_ref().ok(),"error":result.as_ref().err().map(ToString::to_string),"metrics":metrics})).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_organizer_decisions_have_structured_contract_errors() {
        let error=normalize_decision("schedule_task",json!({"return_when":"ready","reason":"check","completion":"check"}),
            "task",2,3).unwrap_err();
        let details=contract_details(&error).unwrap();
        assert_eq!(details["rule_id"],"INVALID_TASK_GOAL");
        assert_eq!(details["field_path"],"goal");
        assert_eq!(details["submitted_value_present"],false);
        assert!(contract_details(&anyhow::anyhow!("network timeout")).is_none());

        let finish=normalize_decision("finish_request",json!({"achieved":false}),"task",2,3).unwrap_err();
        let details=contract_details(&finish).unwrap();
        assert_eq!(details["field_path"],"summary");
        assert_eq!(details["rule_id"],"FINISH_SUMMARY_REQUIRED");
    }

    #[test]
    fn schedule_task_host_id_is_stable_for_a_replayed_organizer_call() {
        let args=json!({"goal":"Probe font API","return_when":"Return actual HTTP status","reason":"Known service is ready",
            "completion":"check","checks":["http-probe:http://127.0.0.1:3000/api/fonts"]});
        let first=normalize_decision("schedule_task",args.clone(),"task-abc",4,2).unwrap();
        let replay=normalize_decision("schedule_task",args,"task-abc",4,2).unwrap();
        assert_eq!(first,replay);
        assert_eq!(first["orders"].as_array().unwrap().len(),1);
        assert_eq!(first["orders"][0]["id"],"task_task-abc_4_2");
    }

    type Chunks=std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

    async fn sse_server(chunks:Vec<Vec<u8>>,hang:bool)->(std::net::SocketAddr,tokio::task::JoinHandle<()>) {
        let chunks:Chunks=std::sync::Arc::new(std::sync::Mutex::new(chunks));
        let app=axum::Router::new().route("/v1/chat/completions",axum::routing::post(
            move |axum::extract::State(chunks):axum::extract::State<Chunks>|async move {
                let items=chunks.lock().unwrap().clone().into_iter().map(|chunk|Ok::<_,std::io::Error>(axum::body::Bytes::from(chunk)));
                let body=futures::stream::iter(items).chain(futures::stream::unfold(hang,|hang|async move {
                    if hang {futures::future::pending::<()>().await;}
                    None::<(Result<axum::body::Bytes,std::io::Error>,bool)>
                }));
                axum::response::Response::builder().header("content-type","text/event-stream")
                    .body(axum::body::Body::from_stream(body)).unwrap()
            })).with_state(chunks);
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        (address,tokio::spawn(async move {axum::serve(listener,app).await.unwrap();}))
    }

    fn fixture()->std::path::PathBuf {
        let root=std::env::temp_dir().join(format!("organizer-stream-{}",crate::agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let conn=crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','p','fake','running',0,0)",[]).unwrap();
        root
    }

    fn metrics(root:&std::path::Path)->Vec<Value> {
        let conn=crate::agent_service::open_db(root).unwrap();
        let mut statement=conn.prepare("SELECT data FROM agent_task_events WHERE task_id='task' AND kind='organizer/request_metrics' ORDER BY seq").unwrap();
        statement.query_map([],|row|row.get::<_,String>(0)).unwrap().map(|raw|serde_json::from_str(&raw.unwrap()).unwrap()).collect()
    }

    fn tool_frame(index:usize,name:Option<&str>,arguments:&str)->String {
        let mut function=json!({"arguments":arguments});
        if let Some(name)=name {function["name"]=json!(name);}
        format!("data: {}\n\n",json!({"choices":[{"delta":{"tool_calls":[{"index":index,"id":format!("call_{index}"),"function":function}]}}]}))
    }

    async fn run(chunks:Vec<String>,hang:bool,cancel:CancellationToken,timeout:Duration)->(Result<Value>,Vec<Value>) {
        let root=fixture();
        let (address,server)=sse_server(chunks.into_iter().map(String::into_bytes).collect(),hang).await;
        let state=crate::agent_service::tests::flow_test_state(&root,address);
        let result=decide(&state,"fake-model","task",1,1,"work_a",json!({"process":{}}),&cancel,timeout).await;
        let recorded=metrics(&root);
        server.abort();
        let _=std::fs::remove_dir_all(root);
        (result,recorded)
    }

    #[tokio::test]
    async fn streamed_method_is_applied_once_after_the_terminal_frame() {
        let arguments=json!({"goal":"上传真实演示文稿并确认 8 页","return_when":"返回页码","reason":"服务已返回 200","completion":"output"}).to_string();
        let middle=arguments.find('文').unwrap();
        let mut stream=format!(": keepalive\n\n{}",tool_frame(0,Some("schedule_task"),&arguments[..middle]));
        stream.push_str(&tool_frame(0,None,&arguments[middle..]));
        stream.push_str(&format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),json!({"choices":[],"usage":{"total_tokens":9}})));
        let bytes=stream.into_bytes();
        let cut=bytes.windows(3).position(|window|window=="文".as_bytes()).unwrap()+1;
        let root=fixture();
        // The middle boundary falls inside the UTF-8 encoding of '文'.
        let (address,server)=sse_server(vec![bytes[..5].to_vec(),bytes[5..cut].to_vec(),bytes[cut..].to_vec()],false).await;
        let state=crate::agent_service::tests::flow_test_state(&root,address);
        let decision=decide(&state,"fake-model","task",3,2,"work_a",json!({"process":{}}),&CancellationToken::new(),Duration::from_secs(5)).await.unwrap();
        assert_eq!(decision["action"],"work");
        assert_eq!(decision["orders"].as_array().unwrap().len(),1);
        assert_eq!(decision["orders"][0]["goal"],"上传真实演示文稿并确认 8 页");
        let recorded=metrics(&root);
        assert_eq!(recorded.len(),1);
        let metric=&recorded[0];
        assert_eq!(metric["result"],"decision");
        assert_eq!(metric["response_format"],"sse");
        assert_eq!(metric["received_done"],true);
        assert_eq!(metric["finish_reason"],"tool_calls");
        assert_eq!(metric["usage"]["total_tokens"],9);
        for field in ["response_headers_ms","first_chunk_ms","first_delta_ms","first_tool_delta_ms","response_complete_ms"] {
            assert!(metric[field].is_u64(),"{field} must be recorded: {metric}");
        }
        server.abort();
        let _=std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn incomplete_error_and_multi_method_streams_produce_no_decision() {
        let half=vec![tool_frame(0,Some("schedule_task"),"{\"goal\":\"probe\"")];
        let (result,recorded)=run(half,false,CancellationToken::new(),Duration::from_secs(5)).await;
        let error=result.unwrap_err();
        assert!(contract_details(&error).is_none());
        assert!(error.to_string().contains("ended before"),"{error}");
        assert_eq!(recorded[0]["result"],"request_error");
        assert!(recorded[0]["response_complete_ms"].is_u64());

        let upstream=vec![tool_frame(0,Some("schedule_task"),"{"),"data: {\"error\":{\"message\":\"overloaded\"}}\n\n".to_owned()];
        let (result,recorded)=run(upstream,false,CancellationToken::new(),Duration::from_secs(5)).await;
        assert!(result.unwrap_err().to_string().contains("overloaded"));
        assert_eq!(recorded[0]["result"],"request_error");
        assert!(recorded[0]["response_complete_ms"].is_null());

        let finish=json!({"summary":"done","achieved":true}).to_string();
        let two=vec![tool_frame(0,Some("finish_request"),&finish),tool_frame(1,Some("finish_request"),&finish),
            format!("data: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}))];
        let (result,recorded)=run(two,false,CancellationToken::new(),Duration::from_secs(5)).await;
        assert_eq!(contract_details(&result.unwrap_err()).unwrap()["rule_id"],"ONE_METHOD_PER_DECISION");
        assert_eq!(recorded[0]["result"],"contract_error");

        let truncated=vec![tool_frame(0,Some("finish_request"),&finish),
            format!("data: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"delta":{},"finish_reason":"length"}]}))];
        let (result,_)=run(truncated,false,CancellationToken::new(),Duration::from_secs(5)).await;
        assert!(result.unwrap_err().to_string().contains("finish_reason"));
    }

    #[tokio::test]
    async fn cancellation_and_timeout_while_reading_end_the_request_with_partial_timing() {
        let first=vec![": keepalive\n\n".to_owned(),tool_frame(0,Some("schedule_task"),"{\"goal\":")];
        let root=fixture();
        let (address,server)=sse_server(first.iter().cloned().map(String::into_bytes).collect(),true).await;
        let state=crate::agent_service::tests::flow_test_state(&root,address);
        let cancel=CancellationToken::new();
        let (trigger,watched)=(cancel.clone(),root.clone());
        // Cancel only once the partial tool call has actually been received.
        tokio::spawn(async move {
            loop {
                let receiving={let conn=crate::agent_service::open_db(&watched).unwrap();
                    conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE kind='organizer/progress' AND json_extract(data,'$.phase')='receiving_decision'",[],|row|row.get::<_,i64>(0)).unwrap()>0};
                if receiving {trigger.cancel();break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let started=Instant::now();
        let result=decide(&state,"fake-model","task",1,1,"work_a",json!({"process":{}}),&cancel,Duration::from_secs(30)).await;
        let recorded=metrics(&root);
        server.abort();
        let _=std::fs::remove_dir_all(&root);
        assert!(started.elapsed()<Duration::from_secs(10));
        assert_eq!(result.unwrap_err().to_string(),"cancelled");
        assert_eq!(recorded[0]["result"],"cancelled");
        assert!(recorded[0]["first_tool_delta_ms"].is_u64());
        assert!(recorded[0]["response_complete_ms"].is_null());

        // Keepalives cannot extend the overall deadline.
        let (result,recorded)=run(first,true,CancellationToken::new(),Duration::from_millis(300)).await;
        assert_eq!(result.unwrap_err().to_string(),"Organizer request timed out");
        assert_eq!(recorded[0]["result"],"timeout");
        assert_eq!(recorded[0]["keepalives"],1);
    }
}
