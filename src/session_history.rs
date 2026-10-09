//! Read the existing conversation/event log on demand; no second history store.
use anyhow::{Result, ensure};
use rusqlite::params;
use serde_json::{Value, json};
use std::path::Path;

/// A lossless display of a method's fields, without its transport envelope.
/// Values are never summarized, shortened or interpreted here.
pub fn plain_context(value: &Value) -> String {
    if let Some(fields) = value.as_object() {
        fields.iter().map(|(name, value)| {
            let content = value.as_str().map(str::to_owned)
                .unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap());
            format!("[{name}]\n{content}")
        }).collect::<Vec<_>>().join("\n\n")
    } else {
        value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string())
    }
}

#[cfg(test)]
pub fn parse_plain_context(text: &str) -> Value {
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
        "worker/yield"=>&["/output/worker_return","/output"],
        "observer/node_review"=>&["/result/observer_return","/result"],
        "tool/result"=>&["/meta/result","/message/content"],
        "organizer/decision"=>&["/decision"],
        "organizer/assignment"=>&["/order"],
        "organizer/result_read"=>&["/result"],
        "observer/advice_delivered"=>&["/advice"],_=>&[]
    };
    paths.iter().find_map(|path|data.pointer(path).filter(|value|!value.is_null())).unwrap_or(data)
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
            AND (json_extract(data,'$.identity.request_id') IS NULL OR json_extract(data,'$.identity.request_id')=?2)
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
                |"organizer/assignment"|"organizer/decision"|"observer/node_review"|"observer/consult_reply"
                |"tool/call"|"tool/result"|"turn/end"|"worker/error"|"organizer/error"
                |"organizer/request_error"|"organizer/result_read"|"organizer/method_message"|"organizer/request_metrics"|"observer/history_read"|"visual/service_result"
                |"observer/retrospective"|"observer/retrospective_start"
                |"visual/input_result"|"worker/progress"|"visual/model_received"|"visual/model_failed"|"visual/check_result") {continue;}
            let data:Value=serde_json::from_str(&raw)?;
            if matches!(kind.as_str(),"visual/model_received"|"visual/service_result") && data["manifest"]["request_trace_id"].as_str().is_some_and(|id|visual_deliveries.iter().any(|saved|saved==id)) {continue;}
            if kind=="organizer/decision" && original_decisions.contains(&(data["turn"].clone(),data["step"].clone())) {continue;}
            if kind=="assistant/message" && message_text(&data["message"]).is_empty() {continue;}
            if kind=="observer/node_review" && !matches!(data["status"].as_str(),Some("completed"|"failed"|"timeout"|"cancelled")) {continue;}
            records.push(json!({"seq":seq,"time":time,"kind":kind,"turn":data["turn"],
                "identity":data["identity"],"node_id":data.get("nodeId").or_else(||data.get("flowNodeId")),
                "call_id":data.get("callId").or_else(||data.pointer("/message/toolCallId")),
                "work_id":data.get("workId").or_else(||data.pointer("/output/id")).or_else(||data.pointer("/order/id")),
                "is_error":data.pointer("/message/isError"),"review_status":data["status"],
                "payload":original_payload(&kind,&data),
                "history_reference":{"scope":"conversation","task_id":task,"event_seq":seq,"include_event":true}}));
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
pub async fn organizer_messages(root: &Path, task: &str, input: &Value) -> Result<Vec<Value>> {
    let root = root.to_path_buf();
    let task = task.to_owned();
    let request = input["process"]["request_id"].as_i64().unwrap_or(1);
    let mut initial_context = json!({"human_request":input["human_request"],
        "capabilities":input["capabilities"],
        "request_id":request,
        "resumable_tasks":input["process"]["resumable_tasks"],"turn":input["turn"]});
    tokio::task::spawn_blocking(move || -> Result<Vec<Value>> {
        let conn = crate::agent_service::open_db(&root)?;
        let mut stmt = conn.prepare("SELECT seq,timestamp,kind,data FROM agent_task_events WHERE task_id=?1
            AND COALESCE(json_extract(data,'$.turn'),json_extract(data,'$.identity.request_id'),0)>=?2
            AND (json_extract(data,'$.identity.request_id') IS NULL OR json_extract(data,'$.identity.request_id')=?2)
            ORDER BY seq")?;
        let rows = stmt.query_map(params![task,request], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,
            row.get::<_,String>(2)?,row.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let original_decisions=rows.iter().filter(|(_,_,kind,_)|kind=="organizer/method_message")
            .filter_map(|(_,_,_,raw)|serde_json::from_str::<Value>(raw).ok()).map(|data|(data["turn"].clone(),data["step"].clone())).collect::<Vec<_>>();
        let first_user=rows.iter().find(|(_,_,kind,_)|kind=="user/message").map(|(seq,_,_,raw)| {
            let data:Value=serde_json::from_str(raw).unwrap();
            let content=data["model_content"].as_str().map(str::to_owned).unwrap_or_else(||message_text(&data["message"]));
            initial_context["human_request"]=json!(content);*seq
        });
        let mut messages = vec![json!({"role":"user","content":plain_context(&initial_context)})];
        for (seq,time,kind,raw) in rows {
            let data: Value = serde_json::from_str(&raw)?;
            if kind=="observer/advice_delivered" && data["advice"]["identity"]["request_id"].as_i64()
                .or_else(||data["advice"]["request_id"].as_i64()).is_some_and(|id|id!=request) {continue;}
            if kind=="user/message" {
                if Some(seq)!=first_user {
                    let content=data["model_content"].as_str().map(str::to_owned).unwrap_or_else(||message_text(&data["message"]));
                    messages.push(json!({"role":"user","content":content}));
                }
                continue;
            }
            let has_original=original_decisions.contains(&(data["turn"].clone(),data["step"].clone()));
            if kind=="organizer/decision" && has_original {continue;}
            if kind=="organizer/method_message" {
                messages.push(json!({"role":"assistant","content":method_text(&data["message"])})); continue;
            }
            let (role, sender, payload) = match kind.as_str() {
                "assistant/message"|"visual/input_result" => {
                    let content=message_text(original_payload(&kind,&data));
                    if !content.is_empty() {messages.push(json!({"role":"user","content":format!("{} (message {seq}, time {time}):\n{content}",if kind=="visual/input_result" {"Visual input method returned"}else{"Worker message"})}));}
                    continue;
                },
                "tool/call" => ("user","Worker called method",json!({"call_id":data["callId"],"name":data["name"],"arguments":data["arguments"]})),
                "tool/result" => ("user","Worker method returned",json!({"call_id":data["message"]["toolCallId"],"is_error":data["message"]["isError"],"result":original_payload(&kind,&data)})),
                "worker/progress" => ("user","Worker message",data.clone()),
                "organizer/decision" => ("assistant", "Organizer called a method", original_payload(&kind,&data).clone()),
                "organizer/assignment" => ("user", "Task assignment method returned", original_payload(&kind,&data).clone()),
                "worker/yield" => ("user", "Worker returned", json!({"work_id":data["output"]["id"],
                    "node_id":data["nodeId"],"worker_return":original_payload(&kind,&data)})),
                "observer/advice_delivered" => ("user", "Observer message", original_payload(&kind,&data).clone()),
                "organizer/result_read" => {
                    let result=&data["result"];
                    if !has_original {messages.push(json!({"role":"assistant","content":format!("Called method:\n{}",plain_context(&result["request"]))}));}
                    ("user", "Method returned", result["result"].clone())
                },
                "organizer/error" | "organizer/request_error" | "worker/error" | "visual/model_failed" | "organizer/material_unavailable" => ("user", "Method/request failed", data.clone()),
                _ => continue,
            };
            messages.push(json!({"role":role,"content":format!("{sender} (message {seq}, time {time}):\n{}",plain_context(&payload))}));
        }
        Ok(messages)
    }).await?
}


fn message_text(message:&Value)->String {
    let content=&message["content"];
    if let Some(text)=content.as_str() {return text.to_owned();}
    content.as_array().into_iter().flatten().filter_map(|block|block["text"].as_str()).collect::<Vec<_>>().join("\n")
}
fn method_text(message:&Value)->String {
    let mut parts=vec![message_text(message)];
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        parts.push(format!("Called method {} (call {}):\n{}",call["function"]["name"].as_str().unwrap_or("unknown"),
            call["id"].as_str().unwrap_or(""),call["function"]["arguments"].as_str().unwrap_or("{}")));
    }
    parts.into_iter().filter(|text|!text.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// Full process text for a final review. Tool arguments and original results
/// are content; log envelopes and repeated UI snapshots are not model messages.
pub fn process_messages(process: &Value) -> Vec<Value> {
    process["records"].as_array().into_iter().flatten().map(|record| {
        let kind=record["kind"].as_str().unwrap_or("message");
        let payload=&record["payload"];
        let content=if kind=="tool/call" {
            format!("Called method {} (call {}):\n{}",payload["name"].as_str().unwrap_or("unknown"),
                record["call_id"].as_str().unwrap_or(""),payload["arguments"].as_str().unwrap_or("{}"))
        } else if kind=="organizer/method_message" {method_text(payload)}
        else if matches!(kind,"assistant/message"|"user/message"|"visual/input_result") {message_text(payload)}
        else {plain_context(payload)};
        json!({"role":"user","content":format!("{kind} (message {}, time {}, node {}, work {}, call {}, is_error {}, status {}):\n{content}",
            record["seq"],record["time"],record["node_id"],record["work_id"],record["call_id"],record["is_error"],record["review_status"])})
    }).collect()
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
            "include_event":{"type":"boolean","description":"Return the original event envelope instead of just the sender's message/return or tool output; useful for inspecting provenance."}
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
    if !project {
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM agent_tasks WHERE id=?1)",params![target],|row|row.get(0))?;
        ensure!(exists,"Conversation not found in this workspace: {target}");
    }
    let project_offset=args["project_offset"].as_i64().unwrap_or(0).max(0);
    let base="WITH history AS (
        SELECT e.task_id,e.seq,e.timestamp,e.kind,e.data,
            CASE WHEN e.kind='user/message' THEN 'user'
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
           'organizer/decision','observer/node_review','observer/consult_reply','observer/retrospective','tool/call','tool/result',
           'turn/end','worker/error','organizer/error','organizer/request_error','organizer/result_read','organizer/method_message','organizer/request_metrics','visual/service_result',
           'observer/history_read','observer/retrospective_start','visual/input_result','worker/progress','visual/model_received','visual/model_failed','visual/check_result')
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
      WHERE (?2='all' OR role=?2) AND (?3='' OR instr(lower(data),?3)>0)
        AND (?4='' OR node_id=?4) AND (?5 IS NULL OR json_extract(data,'$.turn')=?5)
        AND (?6 IS NULL OR seq=?6) AND (?7 IS NULL OR seq<?7) AND (?10 IS NULL OR seq>?10)
        AND (?11 IS NULL OR json_extract(data,'$.identity.request_id') IS NULL OR json_extract(data,'$.identity.request_id')=?11)
    )";
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
        (limit+1) as i64,if project{project_offset}else{0},after,request],|row|Ok((
        row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,
        row.get::<_,String>(4)?,row.get::<_,Option<String>>(5)?,row.get::<_,String>(6)?,
        row.get::<_,Option<String>>(7)?,row.get::<_,Option<String>>(8)?,row.get::<_,Option<i64>>(9)?
    )))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let more=rows.len()>limit;let mut records=Vec::new();let mut remaining=max_chars;
    // Allocate the shared excerpt budget across the selected records so an
    // oversized newest result cannot silently hide all the other matches.
    let per_record=(max_chars/rows.len().min(limit).max(1)).max(1);
    for (seq,time,kind,raw,actor,node_id,record_task,prompt,status,matched_events) in rows.into_iter().take(limit) {
        let data:Value=serde_json::from_str(&raw)?;
        let paths:&[&str]=if args["include_event"]==true {&[""]}else{match kind.as_str() {
            "user/message"|"assistant/message"|"visual/input_result"=>&["/message/content"],
            "worker/yield"=>&["/output/worker_return","/output"],
            "observer/node_review"=>&["/result/observer_return","/result"],
            "tool/result"=>&["/meta/result","/message/content"],
            "organizer/decision"=>&["/decision"],_=>&[""]
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
            "role":actor,"kind":kind,"node_id":node_id,"turn":data["turn"],"payload_path":pointer,
            "content":slice,"content_format":if payload.is_string(){"text"}else{"json"},
            "total_chars":total,"char_offset":record_offset,"returned_chars":returned,"omitted_chars":total.saturating_sub(returned),
            "next_char_offset":if next<total{Some(next)}else{None},
            "read_reference":{"scope":"conversation","task_id":record_task,"event_seq":seq,"include_event":args["include_event"]==true},
            "next_read":if next<total{json!({"task_id":record_task,"event_seq":seq,"char_offset":next,"include_event":args["include_event"]==true})}else{Value::Null}});
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
            "project_offset":project_offset+limit as i64,"limit":limit,"max_chars":max_chars,"include_event":args["include_event"]==true})
    }else{Value::Null};
    Ok(json!({"records":records,"has_more":more,"next_before_seq":next_before,"next_search":next_search,
        "scope":scope,"task_id":if project{None}else{Some(target)},
        "history_note":"Saved records in this workspace, not assertions about present resource availability. Project search returns the latest matching event per conversation, ordered by conversation update time; use read_reference to read that event or task_id with query/before_seq to read other records. Conversation records are chronological. Excerpts preserve original content; event_seq/char_offset retrieve omitted text. Project pagination can shift if conversations change during a search.",
        "returned_chars":max_chars-remaining,"max_chars":max_chars}))
}

/// New messages are delivered intact, including failure status and provenance.
/// Only the whole request window may omit older exchanges, never this message.
pub fn observer_message(item:&Value) -> Value {
    let mut message=item.clone();
    message["history_reference"]=json!({"tool":"read_session_history","task_id":item["identity"]["task_id"],
        "role":"observer","node_id":item["node_id"],"query":item["review_id"]});
    if message["observer_return"].is_null() {message["observer_return"]=item.clone();}
    message
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(text.matches("Called method schedule_task").count(),1);
        assert_eq!(text.matches("ONE_UPLOAD").count(),1);
        assert_eq!(text.matches("ORIGINAL_GOAL").count(),1);
        assert_eq!(text.matches("LATEST_FOLLOWUP").count(),1);
        for marker in ["ORIGINAL_ARGUMENT","WORKER_TAIL","OBSERVER_TAIL","[review_status]\nfailed","180000"] {assert!(text.contains(marker),"missing {marker}");}
        for marker in ["NORMALIZED_COPY","WRAPPER_COPY","UNRELATED_ADVICE"] {assert!(!text.contains(marker));}
        assert!(text.find("ONE_UPLOAD")<text.find("OBSERVER_TAIL"));
        let process=task_process(&root,"task",1).await.unwrap();
        let review_text=process_messages(&process).iter().map(|message|message["content"].as_str().unwrap()).collect::<Vec<_>>().join("\n");
        assert!(review_text.contains("Organizer request timed out"));
        assert_eq!(review_text.matches("Called method schedule_task").count(),1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plain_context_preserves_full_parameters_results_and_error_flags() {
        let content="失败信息\n[]\n原始尾部".repeat(3000);
        let process=json!({"records":[{"seq":1,"kind":"tool/call","call_id":"upload","payload":{"name":"browser_upload","arguments":"{\"path\":\"demo.pptx\"}"}},
            {"seq":2,"kind":"tool/result","call_id":"upload","is_error":true,"payload":{"reason":content,"extra":[1,2,3]}}]});
        let messages=process_messages(&process);
        assert!(messages[0]["content"].as_str().unwrap().contains("browser_upload"));
        assert!(messages[0]["content"].as_str().unwrap().contains("{\"path\":\"demo.pptx\"}"));
        let result=messages[1]["content"].as_str().unwrap();
        assert!(result.contains("is_error true"));assert!(result.contains(&content));
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
    fn new_observer_failure_preserves_long_original_status_and_visual_metadata() {
        let item=json!({"review_status":"failed","observer_return":{"summary":"详情".repeat(6000),
            "limitations":["模型不支持视觉"]},"visual_request":{"request_trace_id":"request"},
            "identity":{"task_id":"task"}});
        let sent=observer_message(&item);
        for (key,value) in item.as_object().unwrap() {assert_eq!(&sent[key],value);}
        assert!(sent.get("excerpt").is_none());
    }
}
