//! Read immutable conversation events and source references; no role-specific
//! context rebuilding or execution protocol replay.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;
mod store;
pub use store::{EventQuery, load_events, tool_call_node};

pub fn saved_history_payload<'a>(kind: &str, data: &'a Value) -> Result<&'a Value> {
    let paths: &[&str] = match kind {
        "user/message"
        | "assistant/message"
        | "organizer/method_message"
        | "organizer/continuation_plan"
        | "organizer/context_note"
        | "visual/input_result" => &["/message"],
        "tool/result" => &["/meta/result"],
        "worker/yield" => &[
            "/output/original_return",
            "/output/worker_return",
            "/output",
        ],
        "observer/node_review" => &[
            "/result/original_message",
            "/result/observer_return",
            "/result",
        ],
        "organizer/decision" => &["/decision"],
        "organizer/assignment" => &["/order"],
        "organizer/result_read" => &["/result"],
        "observer/advice_delivered" => &["/advice"],
        // A completed retrospective has an authoritative raw model return.
        // Unavailable/failed records may only contain status and message;
        // preserve that full record instead of making it unreadable.
        "observer/retrospective" => &["/observer_return", ""],
        _ => &[],
    };
    paths
        .iter()
        .find_map(|path| data.pointer(path).filter(|value| !value.is_null()))
        .or_else(|| {
            if kind == "tool/result" {
                data.pointer("/meta/result")
            } else {
                None
            }
        })
        .or_else(|| if paths.is_empty() { Some(data) } else { None })
        .with_context(|| format!("{kind} event is missing its saved original payload field"))
}

fn source_record(task: &str, seq: i64, time: i64, kind: &str, data: &Value) -> Value {
    let mut record = json!({"seq":seq,"time":time,"kind":kind,"turn":data["turn"],"step":data["step"],
        "identity":data.get("identity").or_else(||data.pointer("/advice/identity")),"actor":data.get("actor").cloned().unwrap_or_else(||json!(source_actor(kind))),
        "node_id":data.get("nodeId").or_else(||data.get("flowNodeId")).or_else(||data.pointer("/identity/node_id")),
        "call_id":data.get("callId").or_else(||data.pointer("/message/toolCallId")).or_else(||data.pointer("/message/source/callId")),
        "work_id":data.get("workId").or_else(||data.get("work_id")).or_else(||data.pointer("/output/id")).or_else(||data.pointer("/order/id")).or_else(||data.pointer("/identity/work_id")),
        "duration_ms":data.pointer("/meta/durationMs"),"stage":data["stage"],"failure_stage":data["failure_stage"],
        "review_id":data.get("review_id").or_else(||data.pointer("/advice/review_id")),
        "is_error":data.pointer("/message/isError").or_else(||data.get("is_error")),"review_status":data.get("status").or_else(||data.pointer("/advice/review_status")),
        "payload":saved_history_payload(kind,data).unwrap_or(data),
        "history_reference":{"scope":"conversation","task_id":task,"event_seq":seq,"include_event":true}});
    if let Some(sources) = data.get("source_event_seqs") {
        record["derived_from_event_seqs"] = sources.clone();
    }
    record
}

fn source_metadata(record: &Value) -> Value {
    let mut source = record.clone();
    source.as_object_mut().unwrap().remove("payload");
    source
}

pub(crate) fn event_source(task: &str, seq: i64, time: i64, kind: &str, data: &Value) -> Value {
    source_metadata(&source_record(task, seq, time, kind, data))
}

fn source_actor(kind: &str) -> &str {
    match kind {
        "assistant/message" | "worker/yield" | "worker/progress" => "worker",
        "user/message" => "user",
        kind if kind.starts_with("organizer/") => "organizer",
        kind if kind.starts_with("observer/") => "observer",
        kind if kind.starts_with("tool/") => "worker",
        _ => "unknown",
    }
}

fn image_message(message: &Value) -> Option<Value> {
    let images = message["content"]
        .as_array()?
        .iter()
        .filter(|block| matches!(block["type"].as_str(), Some("image_ref" | "image_url")))
        .cloned()
        .collect::<Vec<_>>();
    (!images.is_empty()).then(|| json!({"content":images}))
}

pub fn tool() -> Value {
    json!({"type":"function","function":{"name":"read_session_history",
    "description":"Read saved conversation history only when current inputs lack needed information. Default scope=conversation reads the current conversation or a returned task_id. scope=project searches conversations in this workspace, returning one dated matching excerpt and a read_reference per conversation. Filter by keyword, role, node or turn. Conversation records are chronological; before_seq pages toward older events, event_seq/char_offset retrieve exact text. Project results page with project_offset. Historical records do not prove current resource availability. This read does not create a Flow node or change execution.",
    "parameters":{"type":"object","properties":{
        "scope":{"type":"string","enum":["conversation","project"]},
        "task_id":{"type":"string","description":"Conversation returned by project search; defaults to the current conversation. Only conversations saved in this workspace can be read."},
        "project_offset":{"type":"integer","minimum":0,"description":"Offset for project search; use the returned next_search."},
        "query":{"type":"string","maxLength":1000},
        "role":{"type":"string","minLength":1,"maxLength":64,"description":"Saved actor name, e.g. agent, worker, tool or visual_tool; default all. Historical actor names remain searchable."},
        "node_id":{"type":"string"},"turn":{"type":"integer","minimum":1},
        "include_descendants":{"type":"boolean","default":false,"description":"Include events owned by descendants of node_id. Split nodes also expose their explicit source interval."},
        "request_id":{"type":"integer","minimum":1,"description":"Goal ID from task history_reference, to restrict the saved event interval."},
        "before_seq":{"type":"integer","minimum":0},"after_seq":{"type":"integer","minimum":-1,"description":"Exclusive lower bound from task history_reference; retain when paging within that human goal."},"event_seq":{"type":"integer","minimum":0},
        "limit":{"type":"integer","minimum":1,"maximum":12},
        "char_offset":{"type":"integer","minimum":0},"max_chars":{"type":"integer","minimum":256,"maximum":12000},
        "include_event":{"type":"boolean","description":"Return the original event envelope instead of just the sender's message/return or tool output; useful for inspecting provenance."},
        "include_context":{"type":"boolean","description":"Include recorded model input views for audit, filterable by the receiving role. Default false. An exact event_seq also resolves a context reference. These views cite the shared history; they are not new actions or independent facts."}
    }}}})
}

pub async fn read(root: &Path, task: &str, args: &Value) -> Result<Value> {
    let root = root.to_path_buf();
    let task = task.to_owned();
    let args = args.clone();
    tokio::task::spawn_blocking(move || read_saved(&root, &task, &args)).await?
}

fn read_saved(root: &Path, task: &str, args: &Value) -> Result<Value> {
    let conn = crate::agent_service::open_db(root)?;
    let store::SearchSelection {
        rows,
        project,
        target,
        role,
        query,
        turn,
        limit,
        max_chars,
        offset,
        project_offset,
    } = store::search_events(&conn, task, args)?;
    let scope = if project { "project" } else { "conversation" };
    let more = rows.len() > limit;
    let mut records = Vec::new();
    let mut remaining = max_chars;
    // Allocate the shared excerpt budget across the selected records so an
    // oversized newest result cannot silently hide all the other matches.
    let per_record = (max_chars / rows.len().min(limit).max(1)).max(1);
    for (seq, time, kind, raw, actor, node_id, record_task, prompt, status, matched_events) in
        rows.into_iter().take(limit)
    {
        let mut data: Value = serde_json::from_str(&raw)?;
        saved_history_payload(&kind, &data)?;
        if kind == "debug/context_request" {
            data["request_context"] = if let Some(id) = data["id"].as_i64() {
                crate::request_context::saved_context(&conn,&record_task,id)?
                    .unwrap_or_else(||json!({"record_kind":"model_input","unavailable":true,"reason":"This older request body was not retained."}))
            } else {
                json!({"record_kind":"model_input","unavailable":true})
            };
        }
        let (payload, pointer) = if kind == "tool/result" && args["include_event"] != true {
            (saved_history_payload(&kind, &data)?, "/meta/result")
        } else {
            let paths: &[&str] = if args["include_event"] == true {
                &[""]
            } else {
                match kind.as_str() {
                    "user/message" | "assistant/message" | "visual/input_result" => {
                        &["/message/content"]
                    }
                    "organizer/context_note" | "organizer/continuation_plan" => {
                        &["/message/content"]
                    }
                    "worker/yield" => &[
                        "/output/original_return",
                        "/output/worker_return",
                        "/output",
                    ],
                    "observer/node_review" => &[
                        "/result/original_message",
                        "/result/observer_return",
                        "/result",
                    ],
                    "tool/result" => &["/meta/result"],
                    "organizer/decision" => &["/decision"],
                    "debug/context_request" => &["/request_context"],
                    _ => &[""],
                }
            };
            let (payload, pointer) = paths
                .iter()
                .find_map(|pointer| {
                    data.pointer(pointer)
                        .filter(|value| !value.is_null())
                        .map(|value| (value, *pointer))
                })
                .unwrap_or((&data, ""));
            (payload, pointer)
        };
        let text = if let Some(text) = payload.as_str() {
            text.to_owned()
        } else {
            payload.to_string()
        };
        let record_offset = if args["char_offset"].is_null() && !query.is_empty() {
            text.to_lowercase()
                .find(&query)
                .map(|index| {
                    text.to_lowercase()[..index]
                        .chars()
                        .count()
                        .saturating_sub(120)
                })
                .unwrap_or(offset)
        } else {
            offset
        };
        let total = text.chars().count();
        let slice = text
            .chars()
            .skip(record_offset)
            .take(per_record.min(remaining))
            .collect::<String>();
        let returned = slice.chars().count();
        remaining = remaining.saturating_sub(returned);
        let next = record_offset.saturating_add(returned);
        let mut record = json!({"task_id":record_task,"seq":seq,"sampled_at":chrono::DateTime::from_timestamp_millis(time).map(|date|date.to_rfc3339()),
            "role":actor,"kind":kind,"node_id":node_id,"turn":data["turn"],"source":source_metadata(&source_record(&record_task,seq,time,&kind,&data)),"payload_path":pointer,
            "content":slice,"content_format":if payload.is_string(){"text"}else{"json"},
            "total_chars":total,"char_offset":record_offset,"returned_chars":returned,"omitted_chars":total.saturating_sub(returned),
            "next_char_offset":if next<total{Some(next)}else{None},
            "read_reference":{"scope":"conversation","task_id":record_task,"event_seq":seq,"include_event":args["include_event"]==true},
            "next_read":if next<total{json!({"task_id":record_task,"event_seq":seq,"char_offset":next,"include_event":args["include_event"]==true})}else{Value::Null}});
        if let Some(images) = image_message(&data["message"]) {
            record["image_message"] = images;
            record["source_identity"] = data["identity"].clone();
        }
        if project {
            record["conversation"] = json!({"prompt_excerpt":prompt.unwrap_or_default().chars().take(320).collect::<String>(),
                "status":status,"matched_events":matched_events});
        }
        records.push(record);
    }
    let next_before = if more && !project {
        records.last().and_then(|record| record["seq"].as_i64())
    } else {
        None
    };
    if !project {
        records.reverse();
    }
    let next_search = if project && more {
        json!({"scope":"project","query":args["query"],"role":role,"node_id":args["node_id"],"turn":turn,
                "project_offset":project_offset+limit as i64,"limit":limit,"max_chars":max_chars,"include_event":args["include_event"]==true,"include_context":args["include_context"]==true})
    } else {
        Value::Null
    };
    Ok(
        json!({"records":records,"has_more":more,"next_before_seq":next_before,"next_search":next_search,
        "scope":scope,"task_id":if project{None}else{Some(target)},
        "history_note":"Saved records in this workspace, not assertions about present resource availability. Project search returns the latest matching event per conversation, ordered by conversation update time; use read_reference to read that event or task_id with query/before_seq to read other records. Conversation records are chronological. Excerpts preserve original content; event_seq/char_offset retrieve omitted text. Project pagination can shift if conversations change during a search.",
        "returned_chars":max_chars-remaining,"max_chars":max_chars}),
    )
}
