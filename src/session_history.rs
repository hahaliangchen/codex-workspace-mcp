//! Read the existing conversation/event log on demand; no second history store.
use anyhow::{Result, ensure};
use rusqlite::{params,OptionalExtension};
use serde_json::{Value, json};
use std::path::Path;

/// Forward text unchanged; serialize original structured values without interpretation.
/// Values are never summarized, shortened or interpreted here.
pub fn plain_context(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string())
}

#[cfg(test)]
pub fn original_text(text:&str)->&str {
    if text.starts_with("Source: ") {text.split_once("\n\n").map(|(_,body)|body).unwrap_or(text)}else{text}
}

#[cfg(test)]
pub fn parse_plain_context(text: &str) -> Value {
    let text=original_text(text);
    if let Ok(value) = serde_json::from_str(text) { return value; }
    let mut fields = serde_json::Map::new();
    let mut name = None;
    let mut content = Vec::new();
    let save = |name: Option<String>, content: &mut Vec<&str>, fields: &mut serde_json::Map<String,Value>| {
        if let Some(name) = name {
            let text = content.join("\n").trim_end().to_owned();
            fields.insert(name, serde_json::from_str(&text).unwrap_or(Value::String(text)));
        }
        content.clear();
    };
    for line in text.lines() {
        if line.starts_with('[') && line.ends_with(']') && line.len()>2
            && line[1..line.len()-1].bytes().all(|b|b.is_ascii_alphanumeric() || b==b'_') {
            save(name.take(), &mut content, &mut fields);
            name = Some(line[1..line.len()-1].to_owned());
        } else { content.push(line); }
    }
    save(name, &mut content, &mut fields);
    Value::Object(fields)
}

fn original_payload<'a>(kind:&str,data:&'a Value)->&'a Value {
    let paths:&[&str]=match kind {
        "user/message"|"assistant/message"|"organizer/method_message"|"visual/input_result"=>&["/message"],
        "worker/yield"=>&["/output/original_return","/output/worker_return","/output"],
        "observer/node_review"=>&["/result/original_message","/result/observer_return","/result"],
        "tool/result"=>&["/meta/result","/message/content"],
        "organizer/decision"=>&["/decision"],
        "organizer/assignment"=>&["/order"],
        "organizer/result_read"=>&["/result"],
        "observer/advice_delivered"=>&["/advice"],"observer/retrospective"=>&["/observer_return"],_=>&[]
    };
    paths.iter().find_map(|path|data.pointer(path).filter(|value|!value.is_null())).unwrap_or(data)
}

fn source_record(task:&str,seq:i64,time:i64,kind:&str,data:&Value)->Value {
    json!({"seq":seq,"time":time,"kind":kind,"turn":data["turn"],"step":data["step"],
        "identity":data.get("identity").or_else(||data.pointer("/advice/identity")),"actor":data.get("actor").cloned().unwrap_or_else(||json!(source_actor(kind))),
        "node_id":data.get("nodeId").or_else(||data.get("flowNodeId")).or_else(||data.pointer("/identity/node_id")),
        "call_id":data.get("callId").or_else(||data.pointer("/message/toolCallId")),
        "work_id":data.get("workId").or_else(||data.get("work_id")).or_else(||data.pointer("/output/id")).or_else(||data.pointer("/order/id")).or_else(||data.pointer("/identity/work_id")),
        "duration_ms":data.pointer("/meta/durationMs"),"stage":data["stage"],"failure_stage":data["failure_stage"],
        "review_id":data.get("review_id").or_else(||data.pointer("/advice/review_id")),
        "is_error":data.pointer("/message/isError").or_else(||data.get("is_error")),"review_status":data.get("status").or_else(||data.pointer("/advice/review_status")),
        "payload":original_payload(kind,data),
        "history_reference":{"scope":"conversation","task_id":task,"event_seq":seq,"include_event":true}})
}

fn source_metadata(record:&Value)->Value {
    let mut source=record.clone();source.as_object_mut().unwrap().remove("payload");source
}

pub(crate) fn event_source(task:&str,seq:i64,time:i64,kind:&str,data:&Value)->Value {
    source_metadata(&source_record(task,seq,time,kind,data))
}

/// Keep provenance visible in the provider-supported content envelope. This
/// adds no interpretation and never changes the original text or image blocks.
pub(crate) fn with_source(mut message:Value,record:&Value)->Value {
    if !record.is_object() {return message;}
    let header=format!("Source: {}",source_metadata(record));
    match message.get_mut("content") {
        Some(Value::String(text))=>*text=format!("{header}\n\n{text}"),
        Some(Value::Array(blocks))=>blocks.insert(0,json!({"type":"text","text":header})),
        _=>{},
    }
    message
}

/// Whole original messages and tool results for the current human goal, in
/// chronological order. Transport copies, deltas and repeated Flow snapshots
/// are not separate actions. Large originals remain readable by event sequence.
pub async fn task_process(root:&Path,task:&str,started_turn:usize)->Result<Value> {
    let root=root.to_path_buf();let task=task.to_owned();
    tokio::task::spawn_blocking(move ||->Result<Value>{
        let conn=crate::agent_service::open_db(&root)?;
        let mut stmt=conn.prepare("SELECT seq,timestamp,kind,data FROM agent_task_events
            WHERE task_id=?1 AND COALESCE(json_extract(data,'$.turn'),json_extract(data,'$.identity.request_id'),0)>=?2
            AND (?2=0 OR json_extract(data,'$.identity.request_id') IS NULL OR json_extract(data,'$.identity.request_id')=?2)
            ORDER BY seq")?;
        let rows=stmt.query_map(params![task,started_turn],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,
            row.get::<_,String>(2)?,row.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let original_decisions=rows.iter().filter(|(_,_,kind,_)|kind=="organizer/method_message")
            .filter_map(|(_,_,_,raw)|serde_json::from_str::<Value>(raw).ok()).map(|data|(data["turn"].clone(),data["step"].clone())).collect::<Vec<_>>();
        let visual_deliveries=rows.iter().filter(|(_,_,kind,_)|kind=="visual/input_result")
            .filter_map(|(_,_,_,raw)|serde_json::from_str::<Value>(raw).ok())
            .filter_map(|data|data["request_trace_id"].as_str().map(str::to_owned)).collect::<Vec<_>>();
        let mut records=Vec::new();
        for (seq,time,kind,raw) in rows {
            if !matches!(kind.as_str(),"user/message"|"assistant/message"|"worker/yield"
                |"organizer/assignment"|"organizer/decision"|"observer/node_review"|"observer/consult_reply"|"observer/advice_delivered"
                |"tool/call"|"tool/result"|"turn/end"|"worker/error"|"organizer/error"
                |"organizer/request_error"|"organizer/result_read"|"organizer/method_message"|"organizer/request_metrics"|"observer/history_read"|"visual/service_result"
                |"observer/retrospective"|"observer/retrospective_start"
                |"visual/input_result"|"worker/progress"|"visual/model_received"|"visual/model_failed"|"visual/check_result"|"debug/context_request") {continue;}
            let data:Value=serde_json::from_str(&raw)?;
            if matches!(kind.as_str(),"visual/model_received"|"visual/service_result") && data["manifest"]["request_trace_id"].as_str().is_some_and(|id|visual_deliveries.iter().any(|saved|saved==id)) {continue;}
            if kind=="organizer/decision" && original_decisions.contains(&(data["turn"].clone(),data["step"].clone())) {continue;}
            if kind=="assistant/message" && message_text(&data["message"]).is_empty()
                && !data["message"]["content"].as_array().is_some_and(|blocks|blocks.iter().any(|block|matches!(block["type"].as_str(),Some("image_ref"|"image_url")))) {continue;}
            if kind=="observer/node_review" && !matches!(data["status"].as_str(),Some("completed"|"failed"|"timeout"|"cancelled")) {continue;}
            records.push(source_record(&task,seq,time,&kind,&data));
        }
        Ok(json!({"request_id":started_turn,"coverage":"complete","total_records":records.len(),
            "history_reference":{"scope":"conversation","task_id":task,"request_id":started_turn,
                "after_seq":records.first().map(|record|record["seq"].as_i64().unwrap()-1),
                "before_seq":records.last().map(|record|record["seq"].as_i64().unwrap()+1),"include_event":true},
            "records":records}))
    }).await?
}

/// Replay only messages addressed to Organizer, in their original order.
/// This is routing, not a summary of Worker operations or a rebuilt task state.
pub async fn organizer_messages(root:&Path,task:&str,input:&Value)->Result<Vec<Value>> {
    Ok(organizer_view(root,task,input,false).await?.messages)
}

pub async fn organizer_view(root: &Path, task: &str, input: &Value, all_history:bool) -> Result<crate::context_window::HistoryView> {
    let root = root.to_path_buf();
    let task = task.to_owned();
    let request = input["process"]["request_id"].as_i64().unwrap_or(1);
    let initial_context = json!({"capabilities":input["capabilities"],"request_id":request,
        "resumable_tasks":input["process"]["resumable_tasks"],"turn":input["turn"]});
    let human_request=input["human_request"].clone();
    tokio::task::spawn_blocking(move || -> Result<crate::context_window::HistoryView> {
        let conn = crate::agent_service::open_db(&root)?;
        let mut stmt = conn.prepare("SELECT seq,timestamp,kind,data FROM agent_task_events WHERE task_id=?1
            AND (?3 OR COALESCE(json_extract(data,'$.turn'),json_extract(data,'$.identity.request_id'),0)>=?2)
            AND (?3 OR json_extract(data,'$.identity.request_id') IS NULL OR json_extract(data,'$.identity.request_id')=?2)
            ORDER BY seq")?;
        let rows = stmt.query_map(params![task,request,all_history], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,
            row.get::<_,String>(2)?,row.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let original_decisions=rows.iter().filter(|(_,_,kind,_)|kind=="organizer/method_message")
            .filter_map(|(_,_,_,raw)|serde_json::from_str::<Value>(raw).ok()).map(|data|(data["turn"].clone(),data["step"].clone())).collect::<Vec<_>>();
        let has_user=rows.iter().any(|(_,_,kind,_)|kind=="user/message");
        let mut messages=vec![json!({"role":"system","name":"runtime","content":plain_context(&initial_context)})];
        if !has_user {messages.push(json!({"role":"user","content":plain_context(&human_request)}));}
        let mut sources=vec![None;messages.len()];
        let mut delivered_reviews=std::collections::HashSet::<String>::new();
        for (seq,time,kind,raw) in rows {
            let before=messages.len();
            (|| -> Result<()> {
            let data: Value = serde_json::from_str(&raw)?;
            if !all_history && kind=="observer/advice_delivered" && data["advice"]["identity"]["request_id"].as_i64()
                .or_else(||data["advice"]["request_id"].as_i64()).is_some_and(|id|id!=request) {return Ok(());}
            if kind=="user/message" {
                let content=data["model_content"].as_str().map(str::to_owned).unwrap_or_else(||message_text(&data["message"]));
                messages.push(json!({"role":"user","content":content}));
                if let Some(images)=image_message(&data["message"]) {
                    messages.push(routed_message("user","user",&images,&data["identity"]));
                }
                return Ok(());
            }
            let has_original=original_decisions.contains(&(data["turn"].clone(),data["step"].clone()));
            if kind=="organizer/decision" && has_original {return Ok(());}
            if kind=="organizer/method_message" {
                messages.push(raw_message("organizer_call",&data["message"])); return Ok(());
            }
            let (sender, payload) = match kind.as_str() {
                "assistant/message"|"visual/input_result" => {
                    if kind=="assistant/message" && message_text(&data["message"]).is_empty() && image_message(&data["message"]).is_none() {return Ok(());}
                    messages.push(routed_message("user",data["actor"].as_str().unwrap_or("worker"),original_payload(&kind,&data),&data["identity"]));
                    return Ok(());
                },
                "tool/call" => ("worker_tool_call", data.clone()),
                "tool/result" => {
                    messages.push(raw_message("worker_tool_result",&json!({"role":"tool",
                        "tool_call_id":data["message"]["toolCallId"],"content":plain_context(original_payload(&kind,&data))})));
                    messages.extend(history_image_messages(original_payload(&kind,&data)));return Ok(());
                },
                "worker/progress" => ("worker",data.clone()),
                "organizer/decision" => ("organizer_call",original_payload(&kind,&data).clone()),
                "organizer/assignment" => ("organizer_assignment",original_payload(&kind,&data).clone()),
                "worker/yield" => ("worker_return",original_payload(&kind,&data).clone()),
                "observer/retrospective" => ("observer_retrospective",original_payload(&kind,&data).clone()),
                "observer/advice_delivered" => {
                    if let Some(review)=data["advice"]["review_id"].as_str().filter(|id|!id.is_empty()) {
                        if !delivered_reviews.insert(review.to_owned()) {return Ok(());}
                    }
                    let original=observer_message(&data["advice"]);
                    messages.push(routed_message("user",&format!("observer_{}",data["advice"]["id"].as_str().unwrap_or("advice")),&original,&data["advice"]["identity"]));return Ok(());
                },
                "organizer/result_read" => {
                    let result=&data["result"];
                    if !has_original {messages.push(raw_message("organizer_call",&result["request"]));}
                    messages.push(raw_message("organizer_result",&result["result"]));
                    messages.extend(history_image_messages(&result["result"]));return Ok(());
                },
                "organizer/error" | "organizer/request_error" | "worker/error" | "visual/model_failed" | "organizer/material_unavailable" => ("runtime_error", data.clone()),
                _ => return Ok(()),
            };
            let name=if sender=="worker_return" {format!("worker_return_{}",data["output"]["id"].as_str().unwrap_or("").chars().take(50).collect::<String>())}else{sender.to_owned()};
            messages.push(raw_message(&name,&payload));
            Ok(())
            })()?;
            let data:Value=serde_json::from_str(&raw)?;
            let record=source_record(&task,seq,time,&kind,&data);
            for message in &mut messages[before..] {*message=with_source(message.clone(),&record);}
            sources.extend(std::iter::repeat_n(Some(seq),messages.len()-before));
        }
        Ok(crate::context_window::HistoryView {messages,sources})
    }).await?
}


fn message_text(message:&Value)->String {
    let content=&message["content"];
    if let Some(text)=content.as_str() {return text.to_owned();}
    content.as_array().into_iter().flatten().filter_map(|block|block["text"].as_str()).collect::<Vec<_>>().join("\n")
}
fn source_actor(kind:&str)->&str {
    match kind {"assistant/message"|"worker/yield"|"worker/progress"=>"worker", "user/message"=>"user",
        kind if kind.starts_with("organizer/")=>"organizer",kind if kind.starts_with("observer/")=>"observer",
        kind if kind.starts_with("tool/")=>"worker",_=>"unknown"}
}

/// Wrap an original body without rewriting it; callers add source metadata separately.
pub fn raw_message(name:&str,payload:&Value)->Value {
    json!({"role":"user","name":name,"content":plain_context(payload)})
}
/// Reuse the exact source event of a command or an upstream return. Provenance
/// is attached to the original body, never reconstructed from a summary.
pub async fn handoff_message(root:&Path,task:&str,name:&str,original:&Value)->Result<Value> {
    let (root,task,name,original)=(root.to_path_buf(),task.to_owned(),name.to_owned(),original.clone());
    tokio::task::spawn_blocking(move ||->Result<Value> {
        let conn=crate::agent_service::open_db(&root)?;
        let kind=if name=="organizer" {"organizer/method_message"}else{"worker/yield"};
        let mut query=conn.prepare("SELECT seq,timestamp,data FROM agent_task_events WHERE task_id=?1 AND kind=?2 ORDER BY seq DESC")?;
        let rows=query.query_map(params![task,kind],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?)))?;
        for row in rows {
            let (seq,time,text)=row?;let data:Value=serde_json::from_str(&text)?;
            let matching_call=if name=="organizer" {data["message"]["tool_calls"].as_array().into_iter().flatten()
                .find(|call|call["function"]["arguments"]==original)}else{None};
            if matching_call.is_some() || (name!="organizer" && name==format!("worker_return_{}",data["output"]["id"].as_str().unwrap_or("").chars().take(50).collect::<String>()) && original_payload(kind,&data)==&original) {
                let mut source=source_record(&task,seq,time,kind,&data);
                if let Some(call)=matching_call {source["call_id"]=call["id"].clone();}
                return Ok(with_source(raw_message(&name,&original),&source));
            }
        }
        // Older execution snapshots may predate original message persistence.
        // Do not invent an event identity for those restored bodies.
        Ok(raw_message(&name,&original))
    }).await?
}

pub async fn sourced_tool_result(root:&Path,task:&str,message:Value,call_id:&str)->Result<Value> {
    let (root,task,call_id)=(root.to_path_buf(),task.to_owned(),call_id.to_owned());
    tokio::task::spawn_blocking(move ||->Result<Value> {
        let conn=crate::agent_service::open_db(&root)?;
        let source=conn.query_row("SELECT seq,timestamp,data FROM agent_task_events WHERE task_id=?1 AND kind='tool/result' AND json_extract(data,'$.message.toolCallId')=?2 ORDER BY seq DESC LIMIT 1",
            params![task,call_id],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?))).optional()?;
        if let Some((seq,time,text))=source {
            return Ok(with_source(message,&source_record(&task,seq,time,"tool/result",&serde_json::from_str(&text)?)));
        }
        Ok(message)
    }).await?
}

fn routed_message(role:&str,name:&str,message:&Value,identity:&Value)->Value {
    let content=if message["content"].is_string() {message["content"].clone()}
        else if let Some(blocks)=message["content"].as_array() {
            json!(blocks.iter().filter(|block|block["type"]!="tool-call").cloned().collect::<Vec<_>>())
        } else {json!(plain_context(message))};
    let mut routed=json!({"role":role,"name":name,"content":content});
    if identity.is_object() {routed["source_identity"]=identity.clone();}
    routed
}

/// Keep original image blocks typed, without host-authored captions.
pub fn history_image_messages(result:&Value)->Vec<Value> {
    result["records"].as_array().into_iter().flatten().filter_map(|record| {
        let original=record.get("image_message")?;
        Some(with_source(routed_message("user",record["role"].as_str().unwrap_or("history"),original,&record["source_identity"]),&record["source"]))
    }).collect()
}
fn image_message(message:&Value)->Option<Value> {
    let images=message["content"].as_array()?.iter().filter(|block|matches!(block["type"].as_str(),Some("image_ref"|"image_url"))).cloned().collect::<Vec<_>>();
    (!images.is_empty()).then(||json!({"content":images}))
}
/// Forward original process messages and their provenance in source order.
pub fn process_messages(process: &Value) -> Vec<Value> {
    process["records"].as_array().into_iter().flatten().flat_map(|record| {
        let kind=record["kind"].as_str().unwrap_or("message");
        let payload=&record["payload"];
        let name=kind.replace('/',"_");
        let message=if matches!(kind,"assistant/message"|"user/message"|"visual/input_result")
            || (kind=="observer/node_review" && payload.get("content").is_some()) {
            routed_message("user",&name,payload,&record["identity"])
        } else {raw_message(&name,payload)};
        let mut messages=vec![with_source(message,record)];
        if kind=="tool/result" {messages.extend(history_image_messages(payload).into_iter().map(|message|with_source(message,record)));}
        messages
    }).collect()
}

pub fn process_view(process:&Value)->crate::context_window::HistoryView {
    let mut view=crate::context_window::HistoryView::default();
    for record in process["records"].as_array().into_iter().flatten() {
        let messages=process_messages(&json!({"records":[record]}));
        view.sources.extend(std::iter::repeat_n(record["seq"].as_i64(),messages.len()));
        view.messages.extend(messages);
    }
    view
}

pub fn tool() -> Value {
    json!({"type":"function","function":{"name":"read_session_history",
        "description":"Read saved conversation history only when current inputs lack needed information. Default scope=conversation reads the current conversation or a returned task_id. scope=project searches conversations in this workspace, returning one dated matching excerpt and a read_reference per conversation. Filter by keyword, role, node or turn. Conversation records are chronological; before_seq pages toward older events, event_seq/char_offset retrieve exact text. Project results page with project_offset. Historical records do not prove current resource availability. This read does not create a Flow node or change execution.",
        "parameters":{"type":"object","properties":{
            "scope":{"type":"string","enum":["conversation","project"]},
            "task_id":{"type":"string","description":"Conversation returned by project search; defaults to the current conversation. Only conversations saved in this workspace can be read."},
            "project_offset":{"type":"integer","minimum":0,"description":"Offset for project search; use the returned next_search."},
            "query":{"type":"string","maxLength":1000},
            "role":{"type":"string","enum":["all","user","worker","organizer","observer","tool"]},
            "node_id":{"type":"string"},"turn":{"type":"integer","minimum":1},
            "request_id":{"type":"integer","minimum":1,"description":"Human goal ID from task history_reference; excludes late Observer records from a replaced goal."},
            "before_seq":{"type":"integer","minimum":0},"after_seq":{"type":"integer","minimum":-1,"description":"Exclusive lower bound from task history_reference; retain when paging within that human goal."},"event_seq":{"type":"integer","minimum":0},
            "limit":{"type":"integer","minimum":1,"maximum":12},
            "char_offset":{"type":"integer","minimum":0},"max_chars":{"type":"integer","minimum":256,"maximum":12000},
            "include_event":{"type":"boolean","description":"Return the original event envelope instead of just the sender's message/return or tool output; useful for inspecting provenance."},
            "include_context":{"type":"boolean","description":"Include recorded model input views for audit, filterable by the receiving role. Default false. An exact event_seq also resolves a context reference. These views cite the shared history; they are not new actions or independent facts."}
        }}}})
}

pub async fn read(root:&Path, task:&str, args:&Value) -> Result<Value> {
    let root=root.to_path_buf();let task=task.to_owned();let args=args.clone();
    tokio::task::spawn_blocking(move || read_saved(&root,&task,&args)).await?
}

fn read_saved(root:&Path, task:&str, args:&Value) -> Result<Value> {
    let scope=args["scope"].as_str().unwrap_or("conversation");
    ensure!(matches!(scope,"conversation"|"project"),"invalid history scope");
    let project=scope=="project";
    let target=args["task_id"].as_str().unwrap_or(task);
    let role=args["role"].as_str().unwrap_or("all");
    ensure!(matches!(role,"all"|"user"|"worker"|"organizer"|"observer"|"tool"),"invalid history role");
    let query=args["query"].as_str().unwrap_or("").trim().to_lowercase();
    let node=args["node_id"].as_str().unwrap_or("");
    let turn=args["turn"].as_i64();let exact=args["event_seq"].as_i64();
    let before=args["before_seq"].as_i64();let after=args["after_seq"].as_i64();let request=args["request_id"].as_i64();
    let limit=if exact.is_some(){1}else{args["limit"].as_u64().unwrap_or(4).clamp(1,12) as usize};
    let max_chars=args["max_chars"].as_u64().unwrap_or(6000).clamp(256,12000) as usize;
    let offset=args["char_offset"].as_u64().unwrap_or(0) as usize;
    ensure!(offset==0||exact.is_some(),"char_offset requires event_seq");
    ensure!(!project||(exact.is_none()&&before.is_none()&&after.is_none()&&args["task_id"].is_null()),
        "Project search returns conversation references; use scope=conversation with task_id for exact events or before_seq.");
    let conn=crate::agent_service::open_db(root)?;
    let has_contexts:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_request_contexts')",[],|row|row.get(0))?;
    let context_search=if has_contexts {
        "OR (kind='debug/context_request' AND instr(lower(COALESCE((SELECT request_json FROM agent_request_contexts c WHERE c.task_id=history.task_id AND c.id=json_extract(history.data,'$.id')),'')),?3)>0)"
    }else{""};
    if !project {
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM agent_tasks WHERE id=?1)",params![target],|row|row.get(0))?;
        ensure!(exists,"Conversation not found in this workspace: {target}");
    }
    let project_offset=args["project_offset"].as_i64().unwrap_or(0).max(0);
    let base=format!("WITH history AS (
        SELECT e.task_id,e.seq,e.timestamp,e.kind,e.data,
            CASE WHEN e.kind LIKE 'tool/%' THEN 'tool'
                 WHEN json_extract(e.data,'$.actor') IS NOT NULL THEN json_extract(e.data,'$.actor')
                 WHEN e.kind='debug/context_request' THEN 'unknown'
                 WHEN e.kind='user/message' THEN 'user'
                 WHEN e.kind='visual/input_result' THEN COALESCE(json_extract(e.data,'$.actor'),'worker')
                 WHEN e.kind LIKE 'observer/%' THEN 'observer'
                 WHEN e.kind LIKE 'organizer/%' THEN 'organizer'
                 WHEN e.kind LIKE 'tool/%' THEN 'tool' ELSE 'worker' END AS role,
            COALESCE(json_extract(e.data,'$.nodeId'),json_extract(e.data,'$.flowNodeId'),
                json_extract(e.data,'$.identity.node_id'),
                (SELECT json_extract(c.data,'$.flowNodeId') FROM agent_task_events c
                 WHERE c.task_id=e.task_id AND c.kind='tool/call'
                   AND json_extract(c.data,'$.callId')=json_extract(e.data,'$.message.toolCallId') ORDER BY c.seq DESC LIMIT 1)) AS node_id
        FROM agent_task_events e WHERE (?1 IS NULL OR e.task_id=?1) AND e.kind IN
          ('user/message','assistant/message','worker/yield','organizer/assignment',
           'organizer/decision','observer/node_review','observer/consult_reply','observer/advice_delivered','observer/retrospective','tool/call','tool/result',
           'turn/end','worker/error','organizer/error','organizer/request_error','organizer/result_read','organizer/method_message','organizer/request_metrics','visual/service_result',
           'observer/history_read','observer/retrospective_start','visual/input_result','worker/progress','visual/model_received','visual/model_failed','visual/check_result','debug/context_request')
          AND (e.kind!='debug/context_request' OR ?6 IS NOT NULL OR ?12)
          AND (e.kind NOT IN ('visual/model_received','visual/service_result') OR NOT EXISTS (
              SELECT 1 FROM agent_task_events v WHERE v.task_id=e.task_id AND v.kind='visual/input_result'
                AND json_extract(v.data,'$.request_trace_id')=json_extract(e.data,'$.manifest.request_trace_id')))
          AND (e.kind!='observer/node_review' OR json_extract(e.data,'$.status') IN ('completed','failed','timeout','cancelled'))
          AND (?6 IS NOT NULL OR e.kind!='tool/call' OR json_extract(e.data,'$.name')!='read_session_history')
          AND (?6 IS NOT NULL OR e.kind!='tool/result' OR NOT EXISTS (
              SELECT 1 FROM agent_task_events c WHERE c.task_id=e.task_id AND c.kind='tool/call'
                AND json_extract(c.data,'$.callId')=json_extract(e.data,'$.message.toolCallId')
                AND json_extract(c.data,'$.name')='read_session_history'))
    ), filtered AS (SELECT * FROM history
      WHERE (?2='all' OR role=?2) AND (?3='' OR instr(lower(data),?3)>0 {context_search})
        AND (?4='' OR node_id=?4) AND (?5 IS NULL OR json_extract(data,'$.turn')=?5)
        AND (?6 IS NULL OR seq=?6) AND (?7 IS NULL OR seq<?7) AND (?10 IS NULL OR seq>?10)
        AND (?11 IS NULL OR json_extract(data,'$.identity.request_id') IS NULL OR json_extract(data,'$.identity.request_id')=?11)
    )");
    let select=if project {
        ", ranked AS (SELECT *, ROW_NUMBER() OVER(PARTITION BY task_id ORDER BY seq DESC) AS rank,
           COUNT(*) OVER(PARTITION BY task_id) AS matched_events FROM filtered)
         SELECT r.seq,r.timestamp,r.kind,r.data,r.role,r.node_id,r.task_id,t.prompt,t.status,r.matched_events
         FROM ranked r JOIN agent_tasks t ON t.id=r.task_id WHERE r.rank=1
         ORDER BY t.updated_at DESC,t.id DESC LIMIT ?8 OFFSET ?9"
    }else{
        " SELECT seq,timestamp,kind,data,role,node_id,task_id,NULL,NULL,NULL FROM filtered
          ORDER BY seq DESC LIMIT ?8 OFFSET ?9"
    };
    let mut stmt=conn.prepare(&format!("{base}{select}"))?;
    let rows=stmt.query_map(params![if project{None}else{Some(target)},role,query,node,turn,exact,before,
        (limit+1) as i64,if project{project_offset}else{0},after,request,args["include_context"]==true],|row|Ok((
        row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,
        row.get::<_,String>(4)?,row.get::<_,Option<String>>(5)?,row.get::<_,String>(6)?,
        row.get::<_,Option<String>>(7)?,row.get::<_,Option<String>>(8)?,row.get::<_,Option<i64>>(9)?
    )))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let more=rows.len()>limit;let mut records=Vec::new();let mut remaining=max_chars;
    // Allocate the shared excerpt budget across the selected records so an
    // oversized newest result cannot silently hide all the other matches.
    let per_record=(max_chars/rows.len().min(limit).max(1)).max(1);
    for (seq,time,kind,raw,actor,node_id,record_task,prompt,status,matched_events) in rows.into_iter().take(limit) {
        let mut data:Value=serde_json::from_str(&raw)?;
        if kind=="debug/context_request" {
            data["request_context"]=if let Some(id)=data["id"].as_i64() {
                crate::request_context::saved_context(&conn,&record_task,id)?
                    .unwrap_or_else(||json!({"record_kind":"model_input","unavailable":true,"reason":"This older request body was not retained."}))
            }else{json!({"record_kind":"model_input","unavailable":true})};
        }
        let paths:&[&str]=if args["include_event"]==true {&[""]}else{match kind.as_str() {
            "user/message"|"assistant/message"|"visual/input_result"=>&["/message/content"],
            "worker/yield"=>&["/output/original_return","/output/worker_return","/output"],
            "observer/node_review"=>&["/result/original_message","/result/observer_return","/result"],
            "tool/result"=>&["/meta/result","/message/content"],
            "organizer/decision"=>&["/decision"],"debug/context_request"=>&["/request_context"],_=>&[""]
        }};
        let (payload,pointer)=paths.iter().find_map(|pointer|data.pointer(pointer).filter(|value|!value.is_null())
            .map(|value|(value,*pointer))).unwrap_or((&data,""));
        let text=if let Some(text)=payload.as_str(){text.to_owned()}else{payload.to_string()};
        let record_offset=if args["char_offset"].is_null()&&!query.is_empty() {
            text.to_lowercase().find(&query).map(|index|text.to_lowercase()[..index].chars().count().saturating_sub(120)).unwrap_or(offset)
        } else {offset};
        let total=text.chars().count();let slice=text.chars().skip(record_offset).take(per_record.min(remaining)).collect::<String>();
        let returned=slice.chars().count();remaining=remaining.saturating_sub(returned);
        let next=record_offset.saturating_add(returned);
        let mut record=json!({"task_id":record_task,"seq":seq,"sampled_at":chrono::DateTime::from_timestamp_millis(time).map(|date|date.to_rfc3339()),
            "role":actor,"kind":kind,"node_id":node_id,"turn":data["turn"],"source":source_metadata(&source_record(&record_task,seq,time,&kind,&data)),"payload_path":pointer,
            "content":slice,"content_format":if payload.is_string(){"text"}else{"json"},
            "total_chars":total,"char_offset":record_offset,"returned_chars":returned,"omitted_chars":total.saturating_sub(returned),
            "next_char_offset":if next<total{Some(next)}else{None},
            "read_reference":{"scope":"conversation","task_id":record_task,"event_seq":seq,"include_event":args["include_event"]==true},
            "next_read":if next<total{json!({"task_id":record_task,"event_seq":seq,"char_offset":next,"include_event":args["include_event"]==true})}else{Value::Null}});
        if let Some(images)=image_message(&data["message"]) {
            record["image_message"]=images;record["source_identity"]=data["identity"].clone();
        }
        if project {
            record["conversation"]=json!({"prompt_excerpt":prompt.unwrap_or_default().chars().take(320).collect::<String>(),
                "status":status,"matched_events":matched_events});
        }
        records.push(record);
    }
    let next_before=if more&&!project {records.last().and_then(|record|record["seq"].as_i64())}else{None};
    if !project {records.reverse();}
    let next_search=if project&&more {
        json!({"scope":"project","query":args["query"],"role":role,"node_id":args["node_id"],"turn":turn,
                "project_offset":project_offset+limit as i64,"limit":limit,"max_chars":max_chars,"include_event":args["include_event"]==true,"include_context":args["include_context"]==true})
    }else{Value::Null};
    Ok(json!({"records":records,"has_more":more,"next_before_seq":next_before,"next_search":next_search,
        "scope":scope,"task_id":if project{None}else{Some(target)},
        "history_note":"Saved records in this workspace, not assertions about present resource availability. Project search returns the latest matching event per conversation, ordered by conversation update time; use read_reference to read that event or task_id with query/before_seq to read other records. Conversation records are chronological. Excerpts preserve original content; event_seq/char_offset retrieve omitted text. Project pagination can shift if conversations change during a search.",
        "returned_chars":max_chars-remaining,"max_chars":max_chars}))
}

/// Observer speaks to Organizer in its original words. Inbox disposition,
/// normalized recommendations and history references are internal metadata.
pub fn observer_message(item:&Value) -> Value {
    if item["original_message"].is_object() {return item["original_message"].clone();}
    let original=item.get("observer_return").filter(|value|!value.is_null()).unwrap_or(item);
    json!({"role":"assistant","content":plain_context(original)})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source_header(message:&Value)->Value {
        let text=message["content"].as_str().unwrap();
        serde_json::from_str(text.split_once("\n\n").unwrap().0.strip_prefix("Source: ").unwrap()).unwrap()
    }

    #[tokio::test]
    async fn all_receivers_keep_original_body_actor_order_call_failure_and_review_status() {
        let root=std::env::temp_dir().join(format!("source-complete-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let conn=crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','check','fake','completed',0,0)",[]).unwrap();
        let identity=json!({"task_id":"task","request_id":1,"work_id":"upload","node_id":"upload","revision":2});
        let original=" \n模型不支持视觉，完整失败原因。".repeat(1200)+"\n原始结尾 ";
        let result=json!({"reason":original,"nested":{"untouched":[1,true,null]}});
        let entries=vec![
            ("tool/call",json!({"turn":2,"step":3,"actor":"worker","identity":identity,"callId":"read1","name":"view_image","arguments":"{}"})),
            ("tool/result",json!({"turn":2,"step":3,"actor":"worker","identity":identity,"message":{"toolCallId":"read1","isError":true},"meta":{"result":result}})),
            ("observer/advice_delivered",json!({"turn":2,"advice":{"id":"legacy","identity":identity,"review_status":"failed","observer_return":{"summary":original}}})),
            ("assistant/message",json!({"turn":2,"actor":"organizer","message":{"content":"最终说明"}})),
        ];
        for (seq,(kind,data)) in entries.iter().enumerate() {
            conn.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES ('task',?1,?2,?3,?4)",params![seq,1000+seq,kind,data.to_string()]).unwrap();
        }
        drop(conn);
        let process=task_process(&root,"task",1).await.unwrap();
        let messages=process_messages(&process);
        let organizer=organizer_messages(&root,"task",&json!({"process":{"request_id":1}})).await.unwrap();
        let history=read(&root,"task",&json!({"limit":12,"max_chars":12000})).await.unwrap();
        for (record,message) in process["records"].as_array().unwrap().iter().zip(&messages) {
            let seq=record["seq"].as_i64().unwrap();
            let source=source_header(message);
            assert_eq!(source,source_metadata(record));
            let forwarded=organizer.iter().find(|message|message["content"].as_str().is_some_and(|text|text.starts_with("Source: ")) && source_header(message)["seq"]==seq).unwrap();
            assert_eq!(source_header(forwarded),source);
            assert_eq!(history["records"].as_array().unwrap().iter().find(|item|item["seq"]==seq).unwrap()["source"],source);
        }
        let failure=&messages[1];
        assert_eq!(source_header(failure)["identity"],identity);
        assert_eq!(source_header(failure)["actor"],"worker");
        assert_eq!(source_header(failure)["turn"],2);
        assert_eq!(source_header(failure)["call_id"],"read1");
        assert_eq!(source_header(failure)["is_error"],true);
        assert_eq!(serde_json::from_str::<Value>(original_text(failure["content"].as_str().unwrap())).unwrap(),result);
        let native=sourced_tool_result(&root,"task",json!({"role":"tool","tool_call_id":"read1","content":result.to_string()}),"read1").await.unwrap();
        assert_eq!(source_header(&native),source_header(failure));
        assert_eq!(native["tool_call_id"],"read1");
        assert_eq!(source_header(&messages[2])["review_status"],"failed");
        assert_eq!(source_header(&messages[3])["actor"],"organizer");
        let organizer_only=read(&root,"task",&json!({"role":"organizer"})).await.unwrap();
        assert_eq!(organizer_only["records"][0]["content"],"最终说明");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn identical_upstream_returns_keep_their_actual_sender_work() {
        let root=std::env::temp_dir().join(format!("handoff-source-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let conn=crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','completed',0,0)",[]).unwrap();
        for (seq,id) in [(1,"first"),(2,"second")] {
            conn.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES ('task',?1,0,'worker/yield',?2)",
                params![seq,json!({"turn":1,"actor":"worker","workId":id,"output":{"id":id,"original_return":" SAME RETURN "}}).to_string()]).unwrap();
        }
        drop(conn);
        for (seq,id) in [(1,"first"),(2,"second")] {
            let message=handoff_message(&root,"task",&format!("worker_return_{id}"),&json!(" SAME RETURN ")).await.unwrap();
            assert_eq!(source_header(&message)["seq"],seq);assert_eq!(source_header(&message)["work_id"],id);
            assert_eq!(original_text(message["content"].as_str().unwrap())," SAME RETURN ");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn provider_preparation_keeps_visible_provenance_and_typed_images() {
        let root=std::env::temp_dir().join(format!("wire-source-complete-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let state=crate::agent_service::tests::flow_test_state(&root,"127.0.0.1:9".parse().unwrap());
        let identity=json!({"task_id":"task","request_id":1,"work_id":"old"});
        let original=json!({"content":[{"type":"text","text":" 原始文字 "},{"type":"image_url","image_url":{"url":"data:image/png;base64,EXACT_IMAGE"}}]});
        let record=json!({"seq":3,"actor":"worker","identity":identity,"turn":1,"kind":"visual/input_result","payload":original});
        let message=with_source(routed_message("user","worker",&original,&identity),&record);
        let mut body=json!({"messages":[message]});
        let context=crate::visual_artifacts::VisualContext{identity:json!({"task_id":"task","request_id":2}),..Default::default()};
        crate::visual_artifacts::prepare_request(&state,"observer","fake",&context,&[],"review",&mut body).await.unwrap();
        assert!(body["messages"][0].get("source_identity").is_none());
        let blocks=body["messages"][0]["content"].as_array().unwrap();
        let source:Value=serde_json::from_str(blocks[0]["text"].as_str().unwrap().strip_prefix("Source: ").unwrap()).unwrap();
        assert_eq!(source["identity"],identity);
        assert_eq!(source["actor"],"worker");
        assert_eq!(&blocks[1..],original["content"].as_array().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn organizer_replays_original_calls_returns_failures_and_review_status_once() {
        let root=std::env::temp_dir().join(format!("original-role-messages-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let conn=crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','load','fake','running',0,0)",[]).unwrap();
        let call=json!({"role":"assistant","content":null,"tool_calls":[{"id":"assign","function":{"name":"schedule_task","arguments":"{\"goal\":\"load\",\"extra\":\"ORIGINAL_ARGUMENT\"}"}}]});
        let worker=json!({"summary":"Loaded once","extra":{"receipt":"ONE_UPLOAD"},"limitations":[format!("{} WORKER_TAIL","failure detail ".repeat(1200))]});
        let observer=json!({"summary":format!("{} OBSERVER_TAIL","review detail ".repeat(1200)),"diagnostic":{"reason":"model lacks image input"}});
        let entries=vec![
            ("user/message",json!({"turn":1,"message":{"content":"ORIGINAL_GOAL"}})),
            ("organizer/method_message",json!({"turn":1,"step":1,"message":call})),
            ("organizer/decision",json!({"turn":1,"step":1,"decision":{"action":"work","reason":"NORMALIZED_COPY"}})),
            ("organizer/assignment",json!({"turn":1,"step":1,"order":{"id":"upload","goal":"load"}})),
            ("worker/yield",json!({"turn":1,"output":{"id":"upload","summary":"WRAPPER_COPY","worker_return":worker}})),
            ("observer/advice_delivered",json!({"turn":1,"advice":{"identity":{"request_id":1},"review_status":"failed","observer_return":observer}})),
            ("user/message",json!({"turn":2,"message":{"content":"LATEST_FOLLOWUP"}})),
            ("organizer/request_error",json!({"turn":2,"failure":{"error":"Organizer request timed out","elapsed_ms":180000}})),
            ("observer/advice_delivered",json!({"turn":1,"advice":{"identity":{"request_id":2},"summary":"UNRELATED_ADVICE"}})),
        ];
        for (seq,(kind,data)) in entries.iter().enumerate() {
            conn.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES ('task',?1,?1,?2,?3)",params![seq,kind,data.to_string()]).unwrap();
        }
        drop(conn);
        let messages=organizer_messages(&root,"task",&json!({"human_request":"LATEST_FOLLOWUP","process":{"request_id":1}})).await.unwrap();
        let text=messages.iter().filter_map(|message|message["content"].as_str()).collect::<Vec<_>>().join("\n");
        assert_eq!(text.matches("\"name\":\"schedule_task\"").count(),1);
        assert_eq!(text.matches("ONE_UPLOAD").count(),1);
        assert_eq!(text.matches("ORIGINAL_GOAL").count(),1);
        assert_eq!(text.matches("LATEST_FOLLOWUP").count(),1);
        for marker in ["ORIGINAL_ARGUMENT","WORKER_TAIL","OBSERVER_TAIL","180000"] {assert!(text.contains(marker),"missing {marker}");}
        for marker in ["NORMALIZED_COPY","WRAPPER_COPY","UNRELATED_ADVICE","Worker returned","Observer message","[review_status]"] {assert!(!text.contains(marker));}
        assert!(text.find("ONE_UPLOAD")<text.find("OBSERVER_TAIL"));
        let process=task_process(&root,"task",1).await.unwrap();
        let review_text=process_messages(&process).iter().map(|message|message["content"].as_str().unwrap()).collect::<Vec<_>>().join("\n");
        assert!(review_text.contains("Organizer request timed out"));
        assert_eq!(review_text.matches("\"name\":\"schedule_task\"").count(),1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn one_observer_reply_is_forwarded_once_even_with_multiple_recommendations() {
        let root=std::env::temp_dir().join(format!("original-advice-once-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut conn=crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        let raw=" \n{ \"recommendations\": [{\"adjustment\":\"检查驾驶\"},{\"adjustment\":\"检查车况\"}] }\n ";
        for (id,review) in [("one","review1"),("two","review1"),("three","review2")] {
            let tx=conn.transaction().unwrap();
            crate::agent_service::append_event_tx(&tx,"task","observer/advice_delivered",&json!({"turn":1,
                "advice":{"id":id,"review_id":review,"request_id":1,"original_message":{"role":"assistant","content":raw}}}),None).unwrap();
            tx.commit().unwrap();
        }
        let messages=organizer_messages(&root,"task",&json!({"process":{"request_id":1}})).await.unwrap();
        let advice=messages.iter().filter(|message|message["name"].as_str().is_some_and(|name|name.starts_with("observer_"))).collect::<Vec<_>>();
        assert_eq!(advice.len(),2,"one reply is not repeated for each extracted recommendation; distinct replies stay distinct");
        for message in advice {assert_eq!(original_text(message["content"].as_str().unwrap()),raw);}
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plain_context_preserves_full_parameters_results_and_error_flags() {
        let content="失败信息\n[]\n原始尾部".repeat(3000);
        let process=json!({"records":[{"seq":1,"kind":"tool/call","call_id":"upload","payload":{"name":"browser_upload","arguments":"{\"path\":\"demo.pptx\"}"}},
            {"seq":2,"kind":"tool/result","call_id":"upload","is_error":true,"payload":{"reason":content,"extra":[1,2,3]}}]});
        let messages=process_messages(&process);
        assert_eq!(messages.iter().map(|message|&message["name"]).collect::<Vec<_>>(),vec![&json!("tool_call"),&json!("tool_result")],"source array order is retained without adding index labels");
        assert!(messages[0]["content"].as_str().unwrap().contains("browser_upload"));
        assert_eq!(serde_json::from_str::<Value>(original_text(messages[0]["content"].as_str().unwrap())).unwrap(),process["records"][0]["payload"]);
        let result=messages[1]["content"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(original_text(result)).unwrap(),process["records"][1]["payload"]);
        assert!(!result.contains("\"payload\""));
    }
    #[tokio::test]
    async fn final_task_process_preserves_failure_recovery_and_observer_messages_across_continuations() {
        let root=std::env::temp_dir().join(format!("task-process-history-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let conn=crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','completed',0,0)",[]).unwrap();
        let long_failure=format!("{} ORIGINAL_FAILURE_TAIL","detail".repeat(1200));
        let entries=vec![
            ("user/message",json!({"turn":1,"message":{"content":"OLD_GOAL"}})),
            ("user/message",json!({"turn":2,"message":{"content":"Open a visible browser"}})),
            ("tool/call",json!({"turn":2,"callId":"open1","name":"browser_open","arguments":"{\"visible\":true}"})),
            ("tool/result",json!({"turn":2,"message":{"toolCallId":"open1","isError":true},
                "meta":{"result":{"ok":false,"error_code":"browser_visibility_locked","reason":long_failure}}})),
            ("observer/node_review",json!({"turn":2,"identity":{"request_id":1},"status":"completed","result":{"observer_return":{"summary":"OLD_ADVICE"}}})),
            ("observer/node_review",json!({"turn":2,"identity":{"request_id":2},"status":"failed",
                "result":{"observer_return":{"summary":"Original Observer failure","reason":long_failure}}})),
            ("tool/call",json!({"turn":3,"callId":"close1","name":"browser_close","arguments":"{}"})),
            ("tool/result",json!({"turn":3,"message":{"toolCallId":"close1","isError":false},"meta":{"result":{"closed":true}}})),
            ("tool/call",json!({"turn":3,"callId":"open2","name":"browser_open","arguments":"{\"visible\":true}"})),
            ("tool/result",json!({"turn":3,"message":{"toolCallId":"open2","isError":false},"meta":{"result":{"ok":true,"display_mode":"visible_window"}}})),
            ("worker/yield",json!({"turn":3,"output":{"summary":"Host wrapper","worker_return":{"summary":"Recovered and opened successfully"}}})),
        ];
        for (seq,(kind,data)) in entries.iter().enumerate() {
            conn.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES ('task',?1,?1,?2,?3)",params![seq,kind,data.to_string()]).unwrap();
        }
        drop(conn);
        let process=task_process(&root,"task",2).await.unwrap();let records=process["records"].as_array().unwrap();
        assert_eq!(process["coverage"],"complete");assert_eq!(records.len(),9);
        assert!(records.windows(2).all(|pair|pair[0]["seq"].as_u64()<pair[1]["seq"].as_u64()));
        assert!(!process.to_string().contains("OLD_GOAL"));assert!(!process.to_string().contains("OLD_ADVICE"));
        let failed=records.iter().find(|record|record["call_id"]=="open1" && record["kind"]=="tool/result").unwrap();
        assert_eq!(failed["is_error"],true);assert_eq!(failed["payload"]["reason"],long_failure);
        let observer=records.iter().find(|record|record["kind"]=="observer/node_review").unwrap();
        assert_eq!(observer["review_status"],"failed");assert_eq!(observer["payload"]["reason"],long_failure);
        assert_eq!(records[7]["payload"]["display_mode"],"visible_window");
        let mut query=process["history_reference"].clone();query["limit"]=json!(12);
        let read=read(&root,"task",&query).await.unwrap();
        assert_eq!(read["records"].as_array().unwrap().first().unwrap()["seq"],1);
        assert!(!read.to_string().contains("OLD_GOAL"));
        assert!(!read.to_string().contains("OLD_ADVICE"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn legacy_progress_and_original_visual_delivery_are_shared_without_transport_copies() {
        let root=std::env::temp_dir().join(format!("shared-delivery-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let conn=crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','completed',0,0)",[]).unwrap();
        let reason=format!("{} UNKNOWN_NO_FALLBACK_TAIL","完整能力限制 ".repeat(700));
        let entries=vec![
            ("worker/progress",json!({"turn":1,"purpose":"LEGACY_PROGRESS","extra":{"original":"PRESERVE_EXTRA"}})),
            ("visual/input_result",json!({"turn":1,"actor":"worker","request_trace_id":"visual-1","message":{"role":"system","content":reason}})),
            ("visual/model_received",json!({"turn":1,"manifest":{"request_trace_id":"visual-1","copy":"DUPLICATE_TELEMETRY"}})),
        ];
        for (seq,(kind,data)) in entries.iter().enumerate() {
            conn.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES ('task',?1,?1,?2,?3)",params![seq,kind,data.to_string()]).unwrap();
        }
        drop(conn);
        let process=task_process(&root,"task",1).await.unwrap();
        assert_eq!(process["records"].as_array().unwrap().len(),2);
        let observer=serde_json::to_string(&process_messages(&process)).unwrap();
        let organizer=serde_json::to_string(&organizer_messages(&root,"task",&json!({"process":{"request_id":1}})).await.unwrap()).unwrap();
        let history=read(&root,"task",&json!({"limit":12,"max_chars":12000})).await.unwrap().to_string();
        for text in [&observer,&organizer,&history] {
            for marker in ["LEGACY_PROGRESS","PRESERVE_EXTRA","UNKNOWN_NO_FALLBACK_TAIL"] {assert!(text.contains(marker),"missing {marker}");}
            assert!(!text.contains("DUPLICATE_TELEMETRY"));
        }
        let exact=read(&root,"task",&json!({"event_seq":1,"max_chars":12000})).await.unwrap();
        assert_eq!(exact["records"][0]["content"],reason);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observer_body_is_byte_exact_including_whitespace_and_host_fields_stay_out() {
        let raw="  {\n  \"summary\": \"请检查驾驶\"\n}  ";
        let message=json!({"role":"assistant","content":raw});
        assert_eq!(observer_message(&json!({"original_message":message,"summary":"HOST_SUMMARY", "disposition":"accepted",
            "observer_return":{"summary":"normalized"}})),message);
    }

    #[test]
    fn new_observer_failure_preserves_long_original_status_and_visual_metadata() {
        let item=json!({"review_status":"failed","observer_return":{"summary":"详情".repeat(6000),
            "limitations":["模型不支持视觉"]},"visual_request":{"request_trace_id":"request"},
            "identity":{"task_id":"task"}});
        let sent=observer_message(&item);
        assert_eq!(serde_json::from_str::<Value>(sent["content"].as_str().unwrap()).unwrap(),item["observer_return"]);
        for key in ["review_status","visual_request","identity","history_reference"] {assert!(sent.get(key).is_none());}
        assert!(sent.get("excerpt").is_none());
    }
}
