//! Read the existing conversation/event log on demand; no second history store.
use anyhow::{Context, Result};
#[cfg(test)]
use rusqlite::params;
use serde_json::{Value, json};
use std::path::Path;

mod store;
pub use store::{EventQuery, load_events, read_event, requirement_snapshot};

/// Forward text unchanged; serialize original structured values without interpretation.
/// Values are never summarized, shortened or interpreted here.
pub fn plain_context(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

#[cfg(test)]
pub fn original_text(text: &str) -> &str {
    if text.starts_with("Source: ") {
        text.split_once("\n\n")
            .map(|(_, body)| body)
            .unwrap_or(text)
    } else {
        text
    }
}

#[cfg(test)]
pub fn parse_plain_context(text: &str) -> Value {
    let text = original_text(text);
    if let Ok(value) = serde_json::from_str(text) {
        return value;
    }
    let mut fields = serde_json::Map::new();
    let mut name = None;
    let mut content = Vec::new();
    let save = |name: Option<String>,
                content: &mut Vec<&str>,
                fields: &mut serde_json::Map<String, Value>| {
        if let Some(name) = name {
            let text = content.join("\n").trim_end().to_owned();
            fields.insert(
                name,
                serde_json::from_str(&text).unwrap_or(Value::String(text)),
            );
        }
        content.clear();
    };
    for line in text.lines() {
        if line.starts_with('[')
            && line.ends_with(']')
            && line.len() > 2
            && line[1..line.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            save(name.take(), &mut content, &mut fields);
            name = Some(line[1..line.len() - 1].to_owned());
        } else {
            content.push(line);
        }
    }
    save(name, &mut content, &mut fields);
    Value::Object(fields)
}

/// The single extraction point for content persisted in history events.
/// Display wrappers are never treated as original content. Known persisted
/// fields are selected mechanically; absent authoritative fields are errors.
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

/// Return a complete message exactly as persisted inside the history event.
pub fn tool_result_call_id(data: &Value) -> Result<&str> {
    data.get("callId")
        .or_else(|| data.pointer("/message/toolCallId"))
        .or_else(|| data.pointer("/message/source/callId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("tool/result event is missing its original message.toolCallId")
}

fn replay_tool_result_payload(result: &Value, call_id: &str, source: &Value) -> Value {
    with_source(
        json!({"role":"tool","tool_call_id":call_id,"content":result.to_string()}),
        source,
    )
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

fn observer_process_message(kind: &str, payload: &Value, source: &Value) -> Result<Value> {
    match kind {
        "observer/advice_delivered" => {
            if let Some(original) = payload
                .pointer("/advice/original_message")
                .or_else(|| payload.pointer("/original_message"))
                .filter(|message| message.is_object())
            {
                replay_history_message(kind, original, source)
            } else {
                let advice = payload.get("advice").unwrap_or(payload);
                Ok(with_source(raw_message("observer_history", advice), source))
            }
        }
        "observer/node_review" => {
            let original = payload
                .pointer("/result/original_message")
                .or_else(|| payload.pointer("/result/observer_return"))
                .or_else(|| payload.pointer("/result"))
                .unwrap_or(payload);
            if original.get("role").is_some() {
                replay_history_message(kind, original, source)
            } else if original.get("content").is_some() {
                Ok(with_source(
                    routed_message(
                        "user",
                        &kind.replace('/', "_"),
                        original,
                        &source["identity"],
                    ),
                    source,
                ))
            } else {
                Ok(with_source(
                    raw_message("observer_node_review", original),
                    source,
                ))
            }
        }
        "observer/retrospective" => {
            let original = payload.pointer("/observer_return").unwrap_or(payload);
            Ok(with_source(
                raw_message("observer_retrospective", original),
                source,
            ))
        }
        _ => anyhow::bail!("unsupported observer history event: {kind}"),
    }
}

fn source_metadata(record: &Value) -> Value {
    let mut source = record.clone();
    source.as_object_mut().unwrap().remove("payload");
    source
}

pub(crate) fn event_source(task: &str, seq: i64, time: i64, kind: &str, data: &Value) -> Value {
    source_metadata(&source_record(task, seq, time, kind, data))
}

/// Keep provenance as an independent internal field. Never rewrite the body.
pub(crate) fn with_source(mut message: Value, record: &Value) -> Value {
    if !record.is_object() {
        return message;
    }
    message["source"] = source_metadata(record);
    message
}

/// Restore saved assistant calls to native protocol fields while preserving
/// their original call IDs, argument strings and text blocks.
pub fn replay_history_message(kind: &str, message: &Value, source: &Value) -> Result<Value> {
    let historical_call = kind == "organizer/method_message"
        || (kind == "assistant/message"
            && (message["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
                || message["content"].as_array().is_some_and(|blocks| {
                    blocks.iter().any(|block| block["type"] == "tool-call")
                })));
    if historical_call {
        let mut native = message.clone();
        if let Some(fields) = native.as_object_mut() {
            fields.remove("id");
            fields.remove("source");
        }
        native["role"] = json!("assistant");
        if message["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty())
        {
            native["tool_calls"] = message["tool_calls"].clone();
            if message.get("content").is_none() {
                native["content"] = Value::Null;
            }
        } else {
            let blocks = message["content"]
                .as_array()
                .context("saved assistant tool call content is not an array")?;
            let mut calls = Vec::new();
            let mut content = Vec::new();
            for block in blocks {
                if block["type"] == "tool-call" {
                    let id = block["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .context("saved tool call is missing its id")?;
                    let name = block["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .context("saved tool call is missing its name")?;
                    let arguments = block["arguments"]
                        .as_str()
                        .context("saved tool call is missing its original arguments")?;
                    calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}}));
                } else {
                    content.push(block.clone());
                }
            }
            anyhow::ensure!(
                !calls.is_empty(),
                "saved assistant call message contains no tool calls"
            );
            native["content"] = if content.is_empty() {
                Value::Null
            } else {
                json!(content)
            };
            native["tool_calls"] = json!(calls);
        }
        Ok(with_source(native, source))
    } else {
        Ok(with_source(message.clone(), source))
    }
}

pub fn replay_tool_call_ids(message: &Value) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("saved assistant tool call is missing its id")?
            .to_owned();
        anyhow::ensure!(
            !ids.contains(&id),
            "saved assistant tool call contains duplicate id {id}"
        );
        ids.push(id);
    }
    Ok(ids)
}

/// Whole original messages and tool results for the current human goal, in
/// chronological order. Transport copies, deltas and repeated Flow snapshots
/// are not separate actions. Large originals remain readable by event sequence.
pub async fn task_process(root: &Path, task: &str, started_turn: usize) -> Result<Value> {
    let root = root.to_path_buf();
    let task = task.to_owned();
    tokio::task::spawn_blocking(move ||->Result<Value>{
        let conn=crate::agent_service::open_db(&root)?;
        let rows=load_events(&conn,&task,&Default::default())?;
        let mut records=Vec::new();
        for event in rows {
            let data=event.data()?;
            let (seq,time,kind)=(event.seq,event.time,event.kind);
            saved_history_payload(&kind,&data)?;
            if kind=="tool/result" {tool_result_call_id(&data)?;}
            records.push(source_record(&task,seq,time,&kind,&data));
        }
        Ok(json!({"request_id":started_turn,"coverage":"complete","total_records":records.len(),
            "history_reference":{"scope":"conversation","task_id":task,
                "after_seq":records.first().map(|record|record["seq"].as_i64().unwrap()-1),
                "before_seq":records.last().map(|record|record["seq"].as_i64().unwrap()+1),"include_event":true},
            "records":records}))
    }).await?
}

/// Replay only messages addressed to Organizer, in their original order.
/// This is routing, not a summary of Worker operations or a rebuilt task state.
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

/// Wrap an original body without rewriting it; callers add source metadata separately.
pub fn raw_message(name: &str, payload: &Value) -> Value {
    json!({"role":"user","name":name,"content":plain_context(payload)})
}
/// Reuse the exact source event of a command or an upstream return. Provenance
/// is attached to the original body, never reconstructed from a summary.
fn routed_message(role: &str, name: &str, message: &Value, identity: &Value) -> Value {
    let content = if message["content"].is_string() {
        message["content"].clone()
    } else if let Some(blocks) = message["content"].as_array() {
        json!(
            blocks
                .iter()
                .filter(|block| block["type"] != "tool-call")
                .cloned()
                .collect::<Vec<_>>()
        )
    } else {
        json!(plain_context(message))
    };
    let mut routed = json!({"role":role,"name":name,"content":content});
    if identity.is_object() {
        routed["source_identity"] = identity.clone();
    }
    routed
}

/// Keep original image blocks typed, without host-authored captions.
pub fn history_image_messages(result: &Value) -> Vec<Value> {
    if result["artifact_id"].as_str().is_some() && result["visual_artifact"]["identity"].is_object()
    {
        return vec![json!({
            "role":"user",
            "name":"organizer_view_image",
            "source_identity":result["visual_artifact"]["identity"],
            "content":[{"type":"image_ref","artifact_id":result["artifact_id"],"view_original":result["view_original"]==true}]
        })];
    }
    result["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|record| {
            let original = record.get("image_message")?;
            Some(with_source(
                routed_message(
                    "user",
                    record["role"].as_str().unwrap_or("history"),
                    original,
                    &record["source_identity"],
                ),
                &record["source"],
            ))
        })
        .collect()
}

/// A history lookup is a new read, not a new occurrence of its old messages.
/// Move excerpt bodies into source-labelled messages and leave references in
/// the lookup receipt. Stored events and tool results are never changed.
fn project_history_read(value: &mut Value, originals: &mut Vec<Value>) {
    if matches!(value["scope"].as_str(), Some("conversation" | "project")) {
        if let Some(records) = value["records"].as_array_mut() {
            for record in records {
                let Some(text) = record["content"].as_str().map(str::to_owned) else {
                    continue;
                };
                let Some(seq) = record["seq"].as_i64() else {
                    continue;
                };
                let mut source = record["source"].clone();
                if !source.is_object() {
                    continue;
                }
                source["seq"] = json!(seq);
                for field in [
                    "char_offset",
                    "payload_path",
                    "total_chars",
                    "returned_chars",
                    "omitted_chars",
                ] {
                    source[field] = record[field].clone();
                }
                originals.push(with_source(
                    raw_message("retrieved_history", &json!(text)),
                    &source,
                ));
                let fields = record.as_object_mut().unwrap();
                fields.remove("content");
                fields.remove("image_message");
                fields.insert("content_location".into(),json!("Original excerpt is supplied in source order as a retrieved_history message; use seq and char_offset to locate it."));
            }
            return;
        }
    }
    // Existing receipts use result wrappers; native tool receipts serialize
    // the same result in content. Do not search arbitrary business fields.
    if let Some(result) = value.get_mut("result") {
        project_history_read(result, originals);
    }
    if let Some(Value::String(content)) = value.get_mut("content") {
        if let Ok(mut result) = serde_json::from_str::<Value>(content) {
            let before = originals.len();
            project_history_read(&mut result, originals);
            if originals.len() != before {
                *content = result.to_string();
            }
        }
    }
}

fn source_position(message: &Value) -> Option<(String, i64, u64)> {
    // The current Worker command is a function input, not historical replay.
    // Native assistant/tool exchanges retain their protocol order too.
    if message["role"] != "user" || message["name"] == "organizer" {
        return None;
    }
    let source = &message["source"];
    let task = source["history_reference"]["task_id"]
        .as_str()
        .or_else(|| source["identity"]["task_id"].as_str())?;
    Some((
        task.to_owned(),
        source["seq"].as_i64()?,
        source["char_offset"].as_u64().unwrap_or(0),
    ))
}

/// Merge all retrieved history into its original conversation order, even
/// across lookup batches/pages. Unrelated conversations have separate arrays;
/// never compare their local sequence numbers. Non-history messages stay put.
pub fn order_context_messages(messages: &mut Vec<Value>) {
    let mut originals = Vec::new();
    for message in messages.iter_mut() {
        // Only read/result envelopes can contain retrieved history. Source
        // bodies such as Worker returns are forwarded without interpretation.
        let replay_read = matches!(
            message["name"].as_str(),
            Some("organizer_result" | "worker_tool_result" | "observer_history_read")
        );
        if message["role"] != "tool" && !replay_read {
            continue;
        }
        let Some(content) = message["content"].as_str() else {
            continue;
        };
        let Ok(mut value) = serde_json::from_str::<Value>(content) else {
            continue;
        };
        let before = originals.len();
        project_history_read(&mut value, &mut originals);
        if originals.len() != before {
            message["content"] = json!(value.to_string());
        }
    }
    messages.extend(originals);
    let mut conversations = std::collections::HashMap::<String, usize>::new();
    let mut positions = Vec::new();
    let mut ordered = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let Some((task, seq, offset)) = source_position(message) else {
            continue;
        };
        let next = conversations.len();
        let group = *conversations.entry(task).or_insert(next);
        positions.push(index);
        ordered.push(((group, seq, offset), message.clone()));
    }
    ordered.sort_by_key(|(position, _)| *position);
    for (index, (_, message)) in positions.into_iter().zip(ordered) {
        messages[index] = message;
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
/// Forward original process messages and their provenance in source order.
pub fn process_messages(process: &Value) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    let mut pending = Vec::<String>::new();
    let mut deferred_observer_messages = Vec::<Value>::new();
    for record in process["records"].as_array().into_iter().flatten() {
        let kind = record["kind"].as_str().unwrap_or("message");
        let payload = &record["payload"];
        // These are audit/control records, not conversation turns. In
        // particular, a history read can contain a large raw context page;
        // it must remain available through read_session_history without being
        // silently injected into the next Organizer request.
        if matches!(
            kind,
            "debug/context_request"
                | "debug/context_end"
                | "context/selection_read"
                | "context/rebuilt"
        ) {
            continue;
        }
        let name = kind.replace('/', "_");
        if matches!(
            kind,
            "assistant/message"
                | "organizer/method_message"
                | "organizer/continuation_plan"
                | "organizer/context_note"
        ) {
            if !pending.is_empty() {
                return Err(anyhow::anyhow!(
                    "saved tool exchange is incomplete before another assistant message: {:?}",
                    pending
                ));
            }
            let message = match replay_history_message(kind, payload, record) {
                Ok(message) => message,
                Err(error) => return Err(error),
            };
            pending = match replay_tool_call_ids(&message) {
                Ok(ids) => ids,
                Err(error) => return Err(error),
            };
            messages.push(message);
            continue;
        }
        if kind == "tool/call" {
            let Some(call_id) = record["call_id"].as_str() else {
                return Err(anyhow::anyhow!(
                    "saved tool/call record is missing its call id"
                ));
            };
            if !pending.iter().any(|id| id == call_id) {
                return Err(anyhow::anyhow!(
                    "saved tool/call {call_id} has no matching assistant call"
                ));
            }
            continue;
        }
        if kind == "tool/result" {
            let Some(call_id) = record["call_id"].as_str() else {
                return Err(anyhow::anyhow!(
                    "saved tool/result record is missing its call id"
                ));
            };
            let Some(index) = pending.iter().position(|id| id == call_id) else {
                return Err(anyhow::anyhow!(
                    "saved tool result {call_id} has no matching assistant call"
                ));
            };
            pending.remove(index);
            messages.push(replay_tool_result_payload(payload, call_id, record));
            if pending.is_empty() {
                messages.extend(history_image_messages(payload));
                messages.append(&mut deferred_observer_messages);
            }
            continue;
        }
        if !pending.is_empty() {
            if matches!(
                kind,
                "user/message"
                    | "visual/input_result"
                    | "assistant/message"
                    | "organizer/method_message"
                    | "organizer/continuation_plan"
            ) {
                return Err(anyhow::anyhow!(
                    "saved tool exchange is incomplete before {kind}; missing results for {:?}",
                    pending
                ));
            }
            if matches!(
                kind,
                "observer/advice_delivered" | "observer/node_review" | "observer/retrospective"
            ) {
                deferred_observer_messages.push(observer_process_message(kind, payload, record)?);
                continue;
            }
            if kind == "organizer/context_note" {
                deferred_observer_messages.push(replay_history_message(kind, payload, record)?);
                continue;
            }
            // Technical events remain in the event log for history queries, but
            // they are not conversation messages and cannot split a native
            // assistant/tool exchange during protocol assembly.
            continue;
        }
        let message = if matches!(
            kind,
            "user/message"
                | "visual/input_result"
                | "organizer/continuation_plan"
                | "organizer/context_note"
        ) {
            replay_history_message(kind, payload, record)?
        } else if matches!(
            kind,
            "observer/advice_delivered" | "observer/node_review" | "observer/retrospective"
        ) {
            observer_process_message(kind, payload, record)?
        } else {
            raw_message(&name, payload)
        };
        messages.push(if message.get("source").is_some() {
            message
        } else {
            with_source(message, record)
        });
    }
    if !pending.is_empty() {
        return Err(anyhow::anyhow!(
            "saved tool exchange is missing results for {:?}",
            pending
        ));
    }
    Ok(messages)
}

pub fn process_view(process: &Value) -> Result<crate::context_window::HistoryView> {
    let messages = process_messages(process)?;
    let mut groups = std::collections::HashMap::<String, i64>::new();
    let mut sources = Vec::with_capacity(messages.len());
    for message in &messages {
        let source = message["source"]["seq"].as_i64();
        if message["role"] == "assistant" {
            if let Some(seq) = source {
                for id in replay_tool_call_ids(message)? {
                    groups.insert(id, seq);
                }
            }
            sources.push(source);
        } else if message["role"] == "tool" {
            sources.push(
                message["tool_call_id"]
                    .as_str()
                    .and_then(|id| groups.get(id).copied())
                    .or(source),
            );
        } else {
            sources.push(source);
        }
    }
    Ok(crate::context_window::HistoryView { messages, sources })
}

pub fn tool() -> Value {
    json!({"type":"function","function":{"name":"read_session_history",
    "description":"Read saved conversation history only when current inputs lack needed information. Default scope=conversation reads the current conversation or a returned task_id. scope=project searches conversations in this workspace, returning one dated matching excerpt and a read_reference per conversation. Filter by keyword, role, node or turn. Conversation records are chronological; before_seq pages toward older events, event_seq/char_offset retrieve exact text. Project results page with project_offset. Historical records do not prove current resource availability. This read does not create a Flow node or change execution.",
    "parameters":{"type":"object","properties":{
        "scope":{"type":"string","enum":["conversation","project"]},
        "task_id":{"type":"string","description":"Conversation returned by project search; defaults to the current conversation. Only conversations saved in this workspace can be read."},
        "project_offset":{"type":"integer","minimum":0,"description":"Offset for project search; use the returned next_search."},
        "query":{"type":"string","maxLength":1000},
        "role":{"type":"string","enum":["all","user","agent","context_agent","worker","organizer","observer","tool"]},
        "node_id":{"type":"string"},"turn":{"type":"integer","minimum":1},
        "request_id":{"type":"integer","minimum":1,"description":"Human goal ID from task history_reference; excludes late Observer records from a replaced goal."},
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

/// Observer speaks to Organizer in its original words. Inbox disposition,
/// normalized recommendations and history references are internal metadata.
pub fn observer_message(item: &Value) -> Value {
    if item["original_message"].is_object() {
        return item["original_message"].clone();
    }
    let original = item
        .get("observer_return")
        .filter(|value| !value.is_null())
        .unwrap_or(item);
    json!({"role":"assistant","content":plain_context(original)})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paged_history_keeps_original_order_slices_and_native_tool_protocol() {
        let source = |task: &str, seq: i64| {
            json!({"seq":seq,"time":seq*100,"actor":"worker",
            "history_reference":{"task_id":task,"event_seq":seq}})
        };
        let excerpt = |task: &str, seq: i64, offset: u64, text: &str| {
            json!({"seq":seq,"char_offset":offset,
            "content":text,"returned_chars":text.chars().count(),"source":source(task,seq)})
        };
        let lookup = |id: &str, records: Value| {
            json!({"role":"tool","tool_call_id":id,
            "content":json!({"scope":"conversation","records":records}).to_string()})
        };
        let command = with_source(
            raw_message("organizer", &json!("current assignment")),
            &source("a", 100),
        );
        let calls = json!({"role":"assistant","tool_calls":[{"id":"read","type":"function",
            "function":{"name":"read_session_history","arguments":"{}"}}]});
        let receipt = lookup(
            "read",
            json!([
                excerpt("a", 7, 40, " second slice \n"),
                excerpt("a", 3, 0, " earlier \n")
            ]),
        );
        let stored_receipt = receipt.clone();
        let mut messages = vec![
            json!({"role":"system","content":"instructions"}),
            command.clone(),
            with_source(
                raw_message("worker_return", &json!("later return")),
                &source("a", 9),
            ),
            calls.clone(),
            receipt,
            with_source(
                raw_message("worker_return", &json!("other conversation")),
                &source("b", 1),
            ),
            lookup(
                "page-2",
                json!([
                    excerpt("a", 7, 0, " first slice \n"),
                    excerpt("b", 0, 0, "other earlier")
                ]),
            ),
        ];
        order_context_messages(&mut messages);
        assert_eq!(messages[0]["content"], "instructions");
        assert_eq!(messages[1], command);
        assert_eq!(messages[3], calls);
        assert_eq!(messages[4]["tool_call_id"], "read");
        assert!(
            !messages[4]["content"]
                .as_str()
                .unwrap()
                .contains("second slice")
        );
        assert!(
            stored_receipt["content"]
                .as_str()
                .unwrap()
                .contains("second slice"),
            "saved lookup is intact"
        );
        let positions = messages
            .iter()
            .filter_map(source_position)
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![
                ("a".into(), 3, 0),
                ("a".into(), 7, 0),
                ("a".into(), 7, 40),
                ("a".into(), 9, 0),
                ("b".into(), 0, 0),
                ("b".into(), 1, 0)
            ]
        );
        let slices = messages
            .iter()
            .filter(|message| message["name"] == "retrieved_history")
            .map(|message| original_text(message["content"].as_str().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            slices,
            vec![
                " earlier \n",
                " first slice \n",
                " second slice \n",
                "other earlier"
            ]
        );
        let ordered = messages.clone();
        order_context_messages(&mut messages);
        assert_eq!(messages, ordered);
    }

    #[tokio::test]
    async fn provider_preparation_keeps_original_body_and_typed_images() {
        let root = std::env::temp_dir().join(format!(
            "wire-source-complete-{}",
            crate::visual_artifacts::new_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let state = crate::agent_service::tests::test_state(&root, "127.0.0.1:9".parse().unwrap());
        let identity = json!({"task_id":"task","request_id":1,"work_id":"old"});
        let original = json!({"content":[{"type":"text","text":" 原始文字 "},{"type":"image_url","image_url":{"url":"data:image/png;base64,EXACT_IMAGE"}}]});
        let record = json!({"seq":3,"actor":"worker","identity":identity,"turn":1,"kind":"visual/input_result","payload":original});
        let message = with_source(
            routed_message("user", "worker", &original, &identity),
            &record,
        );
        let mut body = json!({"messages":[message]});
        let context = crate::visual_artifacts::VisualContext {
            identity: json!({"task_id":"task","request_id":2}),
            ..Default::default()
        };
        crate::visual_artifacts::prepare_request(
            &state,
            "observer",
            "fake",
            &context,
            &[],
            "review",
            &mut body,
        )
        .await
        .unwrap();
        assert!(body["messages"][0].get("source_identity").is_none());
        assert_eq!(body["messages"][0]["source"]["identity"], identity);
        assert_eq!(body["messages"][0]["content"], original["content"]);
        crate::context_window::prepare(&mut body, 0).unwrap();
        assert!(body["messages"][0].get("source").is_none());
        assert_eq!(body["messages"][0]["content"], original["content"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plain_context_preserves_full_parameters_results_and_error_flags() {
        let content = "失败信息\n[]\n原始尾部".repeat(3000);
        let process = json!({"records":[
            {"seq":1,"kind":"assistant/message","payload":{"role":"assistant","content":null,"tool_calls":[{"id":"upload","type":"function","function":{"name":"browser_upload","arguments":"{\"path\":\"demo.pptx\"}"}}]}},
            {"seq":2,"kind":"tool/call","call_id":"upload","payload":{"name":"browser_upload","arguments":"{\"path\":\"demo.pptx\"}"}},
            {"seq":3,"kind":"tool/result","call_id":"upload","is_error":true,"payload":{"reason":content,"extra":[1,2,3]}}]});
        let messages = process_messages(&process).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "upload");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "upload");
        assert_eq!(
            serde_json::from_str::<Value>(messages[1]["content"].as_str().unwrap()).unwrap(),
            process["records"][2]["payload"]
        );
    }
    #[tokio::test]
    async fn final_task_process_preserves_failure_recovery_and_observer_messages_across_continuations()
     {
        let root = std::env::temp_dir().join(format!(
            "task-process-history-{}",
            crate::visual_artifacts::new_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let conn = crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','completed',0,0)",[]).unwrap();
        let long_failure = format!("{} ORIGINAL_FAILURE_TAIL", "detail".repeat(1200));
        let entries = vec![
            (
                "user/message",
                json!({"turn":1,"message":{"content":"OLD_GOAL"}}),
            ),
            (
                "user/message",
                json!({"turn":2,"message":{"content":"Open a visible browser"}}),
            ),
            (
                "assistant/message",
                json!({"turn":2,"actor":"worker","message":{"role":"assistant","content":[{"type":"tool-call","id":"open1","name":"browser_open","arguments":"{\"visible\":true}"}]}}),
            ),
            (
                "tool/call",
                json!({"turn":2,"callId":"open1","name":"browser_open","arguments":"{\"visible\":true}"}),
            ),
            (
                "tool/result",
                json!({"turn":2,"message":{"toolCallId":"open1","isError":true},
                "meta":{"result":{"ok":false,"error_code":"browser_visibility_locked","reason":long_failure}}}),
            ),
            (
                "observer/node_review",
                json!({"turn":2,"identity":{"request_id":1},"status":"completed","result":{"observer_return":{"summary":"OLD_ADVICE"}}}),
            ),
            (
                "observer/node_review",
                json!({"turn":2,"identity":{"request_id":2},"status":"failed",
                "result":{"observer_return":{"summary":"Original Observer failure","reason":long_failure}}}),
            ),
            (
                "assistant/message",
                json!({"turn":3,"actor":"worker","message":{"role":"assistant","content":[{"type":"tool-call","id":"close1","name":"browser_close","arguments":"{}"}]}}),
            ),
            (
                "tool/call",
                json!({"turn":3,"callId":"close1","name":"browser_close","arguments":"{}"}),
            ),
            (
                "tool/result",
                json!({"turn":3,"message":{"toolCallId":"close1","isError":false},"meta":{"result":{"closed":true}}}),
            ),
            (
                "assistant/message",
                json!({"turn":3,"actor":"worker","message":{"role":"assistant","content":[{"type":"tool-call","id":"open2","name":"browser_open","arguments":"{\"visible\":true}"}]}}),
            ),
            (
                "tool/call",
                json!({"turn":3,"callId":"open2","name":"browser_open","arguments":"{\"visible\":true}"}),
            ),
            (
                "tool/result",
                json!({"turn":3,"message":{"toolCallId":"open2","isError":false},"meta":{"result":{"ok":true,"display_mode":"visible_window"}}}),
            ),
            (
                "worker/yield",
                json!({"turn":3,"output":{"summary":"Host wrapper","worker_return":{"summary":"Recovered and opened successfully"}}}),
            ),
        ];
        for (seq, (kind, data)) in entries.iter().enumerate() {
            conn.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES ('task',?1,?1,?2,?3)",params![seq,kind,data.to_string()]).unwrap();
        }
        drop(conn);
        let process = task_process(&root, "task", 2).await.unwrap();
        let records = process["records"].as_array().unwrap();
        assert_eq!(process["coverage"], "complete");
        assert_eq!(records.len(), 14);
        assert!(
            records
                .windows(2)
                .all(|pair| pair[0]["seq"].as_u64() < pair[1]["seq"].as_u64())
        );
        assert!(process.to_string().contains("OLD_GOAL"));
        assert!(process.to_string().contains("OLD_ADVICE"));
        let failed = records
            .iter()
            .find(|record| record["call_id"] == "open1" && record["kind"] == "tool/result")
            .unwrap();
        assert_eq!(failed["is_error"], true);
        assert_eq!(failed["payload"]["reason"], long_failure);
        let observer = records
            .iter()
            .find(|record| {
                record["kind"] == "observer/node_review" && record["review_status"] == "failed"
            })
            .unwrap();
        assert_eq!(observer["review_status"], "failed");
        assert_eq!(observer["payload"]["reason"], long_failure);
        assert!(records.iter().any(|record| record["kind"] == "tool/result"
            && record["payload"]["display_mode"] == "visible_window"));
        let mut query = process["history_reference"].clone();
        query["limit"] = json!(12);
        let read = read(&root, "task", &query).await.unwrap();
        assert_eq!(
            read["records"].as_array().unwrap().first().unwrap()["seq"],
            2
        );
        assert_eq!(read["next_before_seq"], 2);
        query["before_seq"] = read["next_before_seq"].clone();
        let older = read_saved(&root, "task", &query).unwrap();
        assert_eq!(
            older["records"].as_array().unwrap().first().unwrap()["seq"],
            0
        );
        assert!(older.to_string().contains("OLD_GOAL"));
        assert!(read.to_string().contains("OLD_ADVICE"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observer_body_is_byte_exact_including_whitespace_and_host_fields_stay_out() {
        let raw = "  {\n  \"summary\": \"请检查驾驶\"\n}  ";
        let message = json!({"role":"assistant","content":raw});
        assert_eq!(
            observer_message(
                &json!({"original_message":message,"summary":"HOST_SUMMARY", "disposition":"accepted",
            "observer_return":{"summary":"normalized"}})
            ),
            message
        );
    }

    #[test]
    fn new_observer_failure_preserves_long_original_status_and_visual_metadata() {
        let item = json!({"review_status":"failed","observer_return":{"summary":"详情".repeat(6000),
            "limitations":["模型不支持视觉"]},"visual_request":{"request_trace_id":"request"},
            "identity":{"task_id":"task"}});
        let sent = observer_message(&item);
        assert_eq!(
            serde_json::from_str::<Value>(sent["content"].as_str().unwrap()).unwrap(),
            item["observer_return"]
        );
        for key in [
            "review_status",
            "visual_request",
            "identity",
            "history_reference",
        ] {
            assert!(sent.get(key).is_none());
        }
        assert!(sent.get("excerpt").is_none());
    }
}
