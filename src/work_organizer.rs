//! Same model as Worker, separate context and a scheduling-only interface.
use anyhow::{Result, ensure, Context};
use serde_json::{json, Value};
use crate::agent_service::AgentServiceState;

pub fn yield_tool() -> Value {
    json!({"type":"function","function":{"name":"yield_work","description":"Return this work's actual output, concrete blocker, request to split, or upstream problem to Organizer. This yields execution; do not promise future work.","parameters":{"type":"object","properties":{
        "summary":{"type":"string"},"outcome":{"type":"string","enum":["completed","blocked","need_split","upstream_problem"]},
        "blocked":{"type":"boolean"},"need_split":{"type":"boolean"},
        "upstream_problem":{"type":"object","properties":{
            "node_id":{"type":"string"},"revision":{"type":"integer"},"field":{"type":"string"},"reason":{"type":"string"},"material_ids":{"type":"array","items":{"type":"integer"}}
        },"required":["node_id","reason"]},
        "findings":{"type":"array","items":{"type":"object"}},"material_ids":{"type":"array","items":{"type":"integer"}},
        "finding_ids":{"type":"array","items":{"type":"string"}},"suggested_children":{"type":"array","items":{"type":"object"}},
        "exported_data":{"type":"object","description":"Structured output key-values explicitly exported by this work unit for downstream dependency consumption."},
        "visual_check_result":{"type":"object","description":"Only after actual image input. Copy request_trace_id and checked_goal from Host visual dispatch. Capture or DOM matched never means pass. uncertain/unavailable must explain limitations.","properties":{
            "artifact_ids":{"type":"array","maxItems":2,"items":{"type":"string"}},"checked_goal":{"type":"string"},"expected_visible_result":{"type":"string"},
            "request_trace_id":{"type":"string"},"assessment":{"type":"string","enum":["pass","issue","uncertain","unavailable"]},
            "observed_facts":{"type":"array","items":{}},"issues":{"type":"array","items":{}},"limitations":{"type":"array","items":{}}
        },"required":["artifact_ids","checked_goal","assessment"]}
    },"required":["summary"]}}})
}

pub async fn decide(state:&AgentServiceState, model:&str, task_id:&str, turn:usize, step:usize, node:&str, input:Value) -> Result<Value> {
    let mut tool=json!({"type":"function","function":{"name":"organize_work","description":"Assign the next concrete work packet, select an existing queued task, backtrack to an upstream node, resume current work, or deliver the final answer.","parameters":{"type":"object","properties":{
        "action":{"type":"string","enum":["work","select","revisit","continue","finish","blocked"]},"reason":{"type":"string"},"summary":{"type":"string"},
        "request_action":{"type":"string","enum":["continue","subtask","replace"],"description":"Required at a session_continuation handoff. Request lifecycle: continue keeps the current user goal and deliveries; subtask assigns work under that goal; replace cancels the old goal and starts the latest human request. replace requires action=work on a session_continuation handoff."},
        "preserve_current":{"type":"boolean","description":"For action=work, keep the current unfinished task resumable while temporary repair or split children execute; otherwise new work replaces it."},
        "task_id":{"type":"string","description":"Task/order ID to activate from the existing queue when action=select"},
        "target_node_id":{"type":"string","description":"Upstream node ID to backtrack to when action=revisit"},
        "target_revision":{"type":"integer","description":"Target revision when action=revisit"},
        "repair_goal":{"type":"string","description":"Updated repair goal for the target node when action=revisit"},
        "flow_update":{"type":"object","description":"Optional TaskTree update. current_node_id is a sibling of plan and identifies a node that exists after this update. Use exactly one of plan.nodes or node_updates.","required":["current_node_id"],"properties":{
            "plan":{"type":"object","description":"Initialize or replace a complete task tree. current_node_id belongs beside plan in flow_update.","required":["mode","nodes"],"properties":{
                "mode":{"type":"string","enum":["tree"]},
                "nodes":{"type":"array","minItems":1,"maxItems":128,"items":{"type":"object","required":["id","parent_id","title","kind","objective","done_when"],"properties":{
                    "id":{"type":"string","maxLength":80},"parent_id":{"type":["string","null"]},"title":{"type":"string","maxLength":160},"kind":{"type":"string","maxLength":48},
                    "objective":{"type":"string","maxLength":1200},"done_when":{"type":"string","maxLength":800},"constraints":{"type":"array","maxItems":12,"items":{"type":"string","maxLength":500}}
                }}}
            }},
            "node_updates":{"type":"array","maxItems":128,"description":"Incremental updates. New nodes need parent_id, title, kind, objective and done_when; existing nodes may be updated by id. Completed nodes are immutable; add a node for next work or revisit a defective result.","items":{"type":"object","required":["id"],"properties":{
                "id":{"type":"string","maxLength":80},"parent_id":{"type":["string","null"]},"title":{"type":"string","maxLength":160},"kind":{"type":"string","maxLength":48},
                "objective":{"type":"string","maxLength":1200},"done_when":{"type":"string","maxLength":800},"constraints":{"type":"array","maxItems":12,"items":{"type":"string","maxLength":500}},"resume":{"type":"boolean","description":"Set only for an existing blocked or paused node. Pending nodes are assigned directly; completed/skipped nodes are sealed and repaired with action=revisit."}
            }}},
            "current_node_id":{"type":"string","maxLength":80,"description":"Required for any Flow update; must identify a node present after the update."},
            "node_result":{"type":"object","required":["node_id","status","summary"],"properties":{"node_id":{"type":"string"},"status":{"type":"string","enum":["completed","blocked","skipped"]},"summary":{"type":"string","maxLength":1800},"material_ids":{"type":"array","items":{"type":"integer"}},"finding_ids":{"type":"array","items":{"type":"string"}}}},
            "resume_tree":{"type":"boolean","description":"Restore the saved tree only when continuing the same unfinished human goal."}
        }},
        "observer_responses":{"type":"array","items":{"type":"object","properties":{
            "id":{"type":"string"},"disposition":{"type":"string","enum":["accepted","adjusted","declined","resolved"]},"reason":{"type":"string"},
            "adopt_to_current_plan":{"type":"boolean","description":"Explicitly reuse archived same-request advice without changing its original review identity."},
            "application":{"type":"object","properties":{"work_id":{"type":"string"},"field":{"type":"string"},"value":{}},"required":["field","value"]}
        },"required":["id","disposition","reason"]}},
        "orders":{"type":"array","maxItems":8,"items":{"type":"object","properties":{
            "id":{"type":"string"},"node_id":{"type":"string"},"goal":{"type":"string"},"done_when":{"type":"string"},
            "completion":{"type":"string","enum":["output","write","check","write_check"]},
            "browser_document_path":{"type":"string","description":"For constraints containing requires_pptx, give the exact workspace-relative PPTX path that must be assigned and confirmed loaded before this work can complete. A verifier node may consume a still-active browser_upload_receipt explicitly declared in dependency_inputs."},
            "edit_targets":{"type":"array","items":{"type":"string"}},"checks":{"type":"array","description":"Authorized host check identifiers: npm:<relative_project_path>:<script>, background server uses npm-start:<relative_project_path>:<script>; optionally append :<nonempty script args JSON>; npm-install:<relative_project_path>; http-probe:<exact local URL>; otherwise the exact shell command. Use . for the workspace root.","items":{"type":"string"}},
            "constraints":{"type":"array","items":{"type":"string"}},"upstream_ids":{"type":"array","items":{"type":"string"}},
            "finding_ids":{"type":"array","items":{"type":"string"}},"material_ids":{"type":"array","items":{"type":"integer"}},"final_answer":{"type":"boolean"},
            "visual_goal":{"type":"string","description":"Explicit rendered-picture verification goal. Use completion=output after code write/check units finish. Requires actual image input and a bound VisualCheckResult; DOM text is separate. Only authorized edit_targets permit repairs."},
            "project_observation":{"type":"object","description":"Declare a process-state observation used by this packet. Use coverage=workspace_list with no project_path for all workspace processes, or project_list with project_path for one project. The host also records real get_project_process results, so a declaration is optional for reuse and duplicate prevention. Set force_refresh with a concrete purpose only when new sampling is necessary.","properties":{"coverage":{"type":"string","enum":["workspace_list","project_list"]},"project_path":{"type":"string"},"script":{"type":"string"},"ready_url":{"type":"string"},"ready_port":{"type":"integer"},"force_refresh":{"type":"boolean"},"purpose":{"type":"string","maxLength":300}}},
            "dependency_inputs":{"type":"array","items":{"type":"object","properties":{"work_id":{"type":"string","description":"Exact work instance; invalidated IDs never fall back to another instance."},"node_id":{"type":"string","description":"Resolve the latest valid instance of this node if work_id is omitted."},"revision":{"type":"integer","description":"Specific delivery revision of the upstream task to bind to"},"fields":{"type":"array","items":{"type":"string"},"description":"Field names to extract from upstream exported_data or output"}},"anyOf":[{"required":["work_id"]},{"required":["node_id"]}]}}
            ,"material_ranges":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"id":{"type":"integer"},"start_line":{"type":"integer"},"end_line":{"type":"integer"}},"required":["id","start_line","end_line"]}}
        },"required":["id","node_id","goal","done_when","completion"]}}
    },"required":["action","reason"]}}});
    let allowed=input.pointer("/capabilities/allowed_request_actions").and_then(Value::as_array)
        .filter(|items|!items.is_empty()).cloned().unwrap_or_else(||vec![json!("continue")]);
    tool["function"]["parameters"]["properties"]["request_action"]["enum"]=json!(allowed);
    if allowed.len()>1 {
        let required=tool["function"]["parameters"]["required"].as_array_mut().expect("organizer required fields are an array");
        if !required.iter().any(|field|field=="request_action") {required.push(json!("request_action"));}
    }
    let mut body=json!({"model":model,"stream":false,"messages":[
        {"role":"system","content":include_str!("../prompts/organizer_system.md")},
        {"role":"user","content":input.to_string()}
    ],"tools":[tool],"tool_choice":{"type":"function","function":{"name":"organize_work"}}});
    if let Some(effort)=state.reasoning_effort.as_deref(){body["reasoning_effort"]=json!(effort);}
    if state.fast_mode {body["service_tier"]=json!("fast");}
    let trace=crate::request_context::record(state.workspace.root(),task_id,json!({"actor":"organizer","stage":"organization","turn":turn,"step":step,"nodeId":node,
        "request_id":input["process"]["request_id"],"workId":input["process"]["current_work"]["id"],"revision":input["process"]["current_revision"],"plan_revision":input["process"]["plan_revision"]}),&body).await;
    let result=async {
        let response=state.client.post(format!("{}/chat/completions",state.provider_url.trim_end_matches('/')))
            .bearer_auth(&state.api_key).json(&body).send().await?.error_for_status()?;
        let response:Value=response.json().await?;
        let calls=response.pointer("/choices/0/message/tool_calls").and_then(Value::as_array).context("Organizer returned no scheduling decision")?;
        ensure!(calls.len()==1&&calls[0].pointer("/function/name").and_then(Value::as_str)==Some("organize_work"),"Organizer must return one scheduling decision");
        let raw=calls[0].pointer("/function/arguments").and_then(Value::as_str).context("Organizer decision is missing arguments")?;
        ensure!(raw.len()<=60_000,"Organizer decision exceeds contract size");
        let decision:Value=serde_json::from_str(raw)?;
        ensure!(matches!(decision["action"].as_str(),Some("work"|"select"|"revisit"|"continue"|"finish"|"blocked"))&&!decision["reason"].as_str().unwrap_or("").trim().is_empty(),"Organizer needs an action and concrete reason");
        Ok::<_,anyhow::Error>(decision)
    }.await;
    crate::request_context::finish(trace,if result.is_ok(){"completed"}else{"failed"},json!({"decision":result.as_ref().ok(),"error":result.as_ref().err().map(ToString::to_string)})).await;
    result
}
