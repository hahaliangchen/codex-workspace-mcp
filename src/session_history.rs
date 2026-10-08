//! Read the existing conversation/event log on demand; no second history store.
use anyhow::{Result, ensure};
use rusqlite::params;
use serde_json::{Value, json};
use std::path::Path;

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
            "before_seq":{"type":"integer","minimum":0},"event_seq":{"type":"integer","minimum":0},
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
    let before=args["before_seq"].as_i64();
    let limit=if exact.is_some(){1}else{args["limit"].as_u64().unwrap_or(4).clamp(1,12) as usize};
    let max_chars=args["max_chars"].as_u64().unwrap_or(6000).clamp(256,12000) as usize;
    let offset=args["char_offset"].as_u64().unwrap_or(0) as usize;
    ensure!(offset==0||exact.is_some(),"char_offset requires event_seq");
    ensure!(!project||(exact.is_none()&&before.is_none()&&args["task_id"].is_null()),
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
                 WHEN e.kind LIKE 'observer/%' THEN 'observer'
                 WHEN e.kind LIKE 'organizer/%' THEN 'organizer'
                 WHEN e.kind LIKE 'tool/%' THEN 'tool' ELSE 'worker' END AS role,
            COALESCE(json_extract(e.data,'$.nodeId'),json_extract(e.data,'$.flowNodeId'),
                json_extract(e.data,'$.identity.node_id'),
                (SELECT json_extract(c.data,'$.flowNodeId') FROM agent_task_events c
                 WHERE c.task_id=e.task_id AND c.kind='tool/call'
                   AND json_extract(c.data,'$.callId')=json_extract(e.data,'$.message.toolCallId') ORDER BY c.seq DESC LIMIT 1)) AS node_id
        FROM agent_task_events e WHERE (?1 IS NULL OR e.task_id=?1) AND e.kind IN
          ('user/message','assistant/message','worker/progress','worker/yield','organizer/assignment',
           'organizer/decision','observer/node_review','observer/consult_reply','observer/retrospective','tool/call','tool/result')
          AND (e.kind!='observer/node_review' OR json_extract(e.data,'$.status') IN ('completed','failed','timeout','cancelled'))
          AND (e.kind!='tool/call' OR json_extract(e.data,'$.name')!='read_session_history')
          AND (e.kind!='tool/result' OR NOT EXISTS (
              SELECT 1 FROM agent_task_events c WHERE c.task_id=e.task_id AND c.kind='tool/call'
                AND json_extract(c.data,'$.callId')=json_extract(e.data,'$.message.toolCallId')
                AND json_extract(c.data,'$.name')='read_session_history'))
    ), filtered AS (SELECT * FROM history
      WHERE (?2='all' OR role=?2) AND (?3='' OR instr(lower(data),?3)>0)
        AND (?4='' OR node_id=?4) AND (?5 IS NULL OR json_extract(data,'$.turn')=?5)
        AND (?6 IS NULL OR seq=?6) AND (?7 IS NULL OR seq<?7)
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
        (limit+1) as i64,if project{project_offset}else{0}],|row|Ok((
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
            "user/message"|"assistant/message"=>&["/message/content"],
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

/// Keep one copy of a new Observer message in the default model input. Large
/// host visual manifests remain in history instead of being replayed as advice.
pub fn observer_message(item:&Value) -> Value {
    let original=item.get("observer_return").filter(|value|!value.is_null());
    let mut message=json!({"id":item["id"],"review_id":item["review_id"],"identity":item["identity"],"request_id":item["request_id"],
        "observed_at":item["observed_at"],"archived":item["archived"],"based_on_older_facts":item["based_on_older_facts"],
        "history_reference":{"tool":"read_session_history","task_id":item["identity"]["task_id"],"role":"observer","node_id":item["node_id"],"query":item["review_id"]}});
    let content=original.cloned().unwrap_or_else(||json!({"summary":item["summary"],"suggestions":item["suggestions"],"review_status":item["review_status"]}));
    let raw=content.to_string();let chars=raw.chars().count();
    if chars<=4000 {message["observer_return"]=content;}
    else {message["excerpt"]=json!(raw.chars().take(4000).collect::<String>());message["omitted_chars"]=json!(chars-4000);}
    message
}
