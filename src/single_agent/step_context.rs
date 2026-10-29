//! A task's working view follows its execution steps. Originals remain in the
//! event log and immutable assets; this module never chooses a business action.
use super::*;
use crate::{context_assets as assets, task_tree as tree};

const RESTORED_FOR: &str = "_step_restored_for";

fn sequence(message: &Value, task: &str) -> Option<i64> {
    if message
        .pointer("/source/history_reference/task_id")
        .and_then(Value::as_str)
        .is_some_and(|owner| owner != task)
    {
        return None;
    }
    message
        .pointer("/source/history_reference/event_seq")
        .or_else(|| message.pointer("/source/seq"))
        .and_then(Value::as_i64)
}

fn protected(message: &Value) -> bool {
    message["role"] == "system"
        || task_frames::is_user_original(message)
        || message["_pinned"] == true
        || message["_frame_input"] == true
}

/// Removing an old native exchange must not leave a pinned tool result orphaned.
fn material_record(message: &Value) -> Value {
    let mut message = message.clone();
    if message["role"] == "tool" {
        message["role"] = json!("user");
        message["name"] = json!("saved_asset");
        message.as_object_mut().unwrap().remove("tool_call_id");
    }
    message
}

fn brief_step(s: &Session<'_>, step: &tree::Node, definitions: &[Value]) -> Result<Value> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let records = crate::session_history::load_events(
        &conn,
        s.task,
        &crate::session_history::EventQuery {
            node_id: Some(&step.node_id),
            kinds: &["tool/call", "tool/result", "subagent/event"],
            ..Default::default()
        },
    )?;
    let mut operations = Vec::new();
    let mut calls = BTreeMap::<String, (usize, String, Value)>::new();
    let mut ids = BTreeSet::new();
    let mut deliveries = Vec::new();
    for record in records {
        let data = record.data()?;
        let (kind, data) = if record.kind == "subagent/event" {
            (data["event_type"].as_str().unwrap_or(""), &data["event"])
        } else {
            (record.kind.as_str(), &data)
        };
        match kind {
            "tool/call" => {
                let name = data["name"].as_str().unwrap_or("").to_owned();
                let args = match &data["arguments"] {
                    Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
                    value => value.clone(),
                };
                let description = data["description"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        task_frames::tool_description(definitions, &name, &args.to_string())
                    });
                calls.insert(
                    data["callId"].to_string(),
                    (operations.len(), name.clone(), args.clone()),
                );
                operations.push(json!({"tool":name,"call_id":data["callId"],"description":description.chars().take(80).collect::<String>(),
                    "arguments":task_frames::brief_arguments(&args),"status":"pending"}));
            }
            "tool/result" => {
                let Some((index, name, args)) =
                    calls.get(&data["message"]["toolCallId"].to_string())
                else {
                    continue;
                };
                let result = &data["meta"]["result"];
                operations[*index]["status"] = result.get("status").cloned().unwrap_or_else(|| {
                    json!(if data["message"]["isError"] == true {
                        "error"
                    } else {
                        "returned"
                    })
                });
                for key in ["error", "message"] {
                    if let Some(value) = result.get(key) {
                        let text = value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string());
                        operations[*index][key] = json!(text.chars().take(160).collect::<String>());
                    }
                }
                // Preserve the result in full, including errors and small write
                // receipts. A later restore does not repeat the external action.
                let reference = if name == "read_asset" && result["asset_reference"].is_object() {
                    result["asset_reference"].clone()
                } else {
                    assets::register(
                        s.state.workspace.root(),
                        s.task,
                        record.seq,
                        name,
                        args,
                        result,
                    )?
                };
                if let Some(id) = reference["asset_id"].as_str() {
                    ids.insert(id.to_owned());
                    operations[*index]["asset_id"] = json!(id);
                }
                deliveries.push(json!({"reference":reference,"event_seq":record.seq}));
            }
            _ => {}
        }
    }
    Ok(
        json!({"host_status":true,"not_a_user_request":true,"node_id":step.node_id,"step":step.ordinal+1,
        "operations":operations,"asset_ids":ids,"_asset_deliveries":deliveries,
        "details":{"tool":"read_session_history","arguments":{"node_id":step.node_id,"include_descendants":true}}}),
    )
}

/// Earlier text deliveries of an identical saved result become reference
/// receipts (native call pairing stays); a pinned or declared copy, otherwise
/// the latest delivery, keeps the body.
fn single_bodies(w: &mut Window) -> usize {
    let keys = w
        .messages
        .iter()
        .map(|m| {
            (m["_asset_state"] == "loaded"
                && m["role"] != "assistant"
                && m["content"].is_string()
                && m[assets::REFERENCE]["asset_id"].is_string())
            .then(|| assets::presentation_key(m))
        })
        .collect::<Vec<_>>();
    let mut owners = BTreeMap::new();
    for (index, key) in keys.iter().enumerate() {
        if let Some(key) = key {
            let owner = owners
                .entry(key)
                .or_insert((index, protected(&w.messages[index])));
            if !owner.1 {
                *owner = (index, protected(&w.messages[index]));
            }
        }
    }
    let mut repeated = 0;
    for (index, key) in keys.iter().enumerate() {
        if let Some(key) = key
            && owners[key].0 != index
            && !protected(&w.messages[index])
        {
            assets::reference_only(&mut w.messages[index]);
            repeated += 1;
        }
    }
    repeated
}

/// Completed rounds keep their native exchange until the working view passes
/// FOLD_AT percent of the space remaining after fixed inputs; old exchanges
/// fold toward FOLD_TO of that working space. Fixed rules do not evict work.
const FOLD_AT: usize = 80;
const FOLD_TO: usize = 60;

/// Where completed steps start to fold; reported to the context ring.
pub(super) fn limits(s: &Session<'_>, w: &Window, definitions: &[Value]) -> (usize, usize, usize) {
    let fixed_messages = w
        .messages
        .iter()
        .filter(|message| protected(message))
        .cloned()
        .collect::<Vec<_>>();
    let fixed = w.input_tokens(s, &request(s, &fixed_messages, definitions));
    let working = s.input_budget.saturating_sub(fixed);
    (
        fixed,
        fixed + working * FOLD_AT / 100,
        fixed + working * FOLD_TO / 100,
    )
}

/// Fold only this task's older completed rounds, oldest first, and only over
/// capacity. The newest exchange (including all calls/results), deliveries
/// restored for it, human instructions and pinned rules stay. Returns the
/// folded steps, whether expired restorations left, and the estimate before.
#[derive(Default)]
struct FoldStats {
    folded: usize,
    merged: usize,
    expired: bool,
    before: usize,
}

fn fold(s: &Session<'_>, w: &mut Window, definitions: &[Value]) -> Result<FoldStats> {
    let owner = task_frames::frame_task(s, w);
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let steps = tree::children(&conn, s.task, &owner)?
        .into_iter()
        .filter(|node| node.kind == "step" && node.agent_id.as_deref() == s.delegate_id())
        .collect::<Vec<_>>();
    let Some(latest) = steps.iter().max_by_key(|node| node.ordinal) else {
        return Ok(FoldStats::default());
    };
    let Some(boundary) = latest.first_seq else {
        return Ok(FoldStats::default());
    };
    let old = steps
        .iter()
        .filter(|node| node.node_id != latest.node_id && node.status == "done")
        .map(|node| (node.node_id.clone(), node))
        .collect::<BTreeMap<_, _>>();
    let mut event_steps = BTreeMap::<i64, String>::new();
    let rows = conn.prepare("SELECT seq,node_id FROM agent_task_events WHERE task_id=?1 AND seq<?2 AND node_id IS NOT NULL")?
        .query_map(params![s.task,boundary], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (seq, node) in rows {
        if old.contains_key(&node) {
            event_steps.insert(seq, node);
        }
    }
    drop(conn);
    // A restoration serves the round it was made for.
    let mut expired = false;
    let mut current = Vec::with_capacity(w.messages.len());
    for message in std::mem::take(&mut w.messages) {
        if message.get(RESTORED_FOR).is_none() || message[RESTORED_FOR] == latest.node_id {
            current.push(message);
            continue;
        }
        expired = true;
        if protected(&message) {
            // A restored snapshot may subsequently become a declared input
            // of another frame; that frame owns its lifetime from now on.
            let mut retained = material_record(&message);
            retained.as_object_mut().unwrap().remove(RESTORED_FOR);
            current.push(retained);
        }
    }
    w.messages = current;
    let owned = |message: &Value| {
        sequence(message, s.task)
            .filter(|seq| *seq < boundary)
            .and_then(|seq| event_steps.get(&seq))
    };
    let estimated = w.input_tokens(s, &request(s, &w.messages, definitions));
    let mut stats = FoldStats {
        expired,
        before: estimated,
        ..Default::default()
    };
    let (_, threshold, target) = limits(s, w, definitions);
    if estimated <= threshold {
        return Ok(stats);
    }
    let mut remaining = estimated;
    let mut candidates = old.values().copied().collect::<Vec<_>>();
    candidates.sort_by_key(|node| node.ordinal);
    // First exhaust older native exchanges. Count the replacement records too,
    // using the same complete request assembly as the delivery boundary.
    for step in candidates {
        if remaining < target {
            break;
        }
        if !w
            .messages
            .iter()
            .any(|m| owned(m) == Some(&step.node_id) && !protected(m))
        {
            continue;
        }
        let mut brief = brief_step(s, step, definitions)?;
        let mut deliveries = brief
            .as_object_mut()
            .unwrap()
            .remove("_asset_deliveries")
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        let mut ids = brief["asset_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        for message in &w.messages {
            if owned(message) == Some(&step.node_id)
                && let Some(id) = message[assets::REFERENCE]["asset_id"].as_str()
            {
                ids.insert(id.to_owned());
                deliveries.push(json!({"reference":message[assets::REFERENCE],"event_seq":sequence(message,s.task)}));
            }
        }
        brief["asset_ids"] = json!(ids);
        let record = json!({"role":"user","name":"step_result","content":brief.to_string(),"_step_fold":step.node_id,"_step_assets":deliveries});
        let mut inserted = false;
        let mut output = Vec::with_capacity(w.messages.len());
        for message in &w.messages {
            if owned(message) != Some(&step.node_id) {
                output.push(message.clone());
            } else if protected(message) {
                output.push(material_record(message));
            } else if !inserted {
                output.push(record.clone());
                inserted = true;
            }
        }
        w.messages = output;
        stats.folded += 1;
        remaining = w.input_tokens(s, &request(s, &w.messages, definitions));
    }
    // Only after native exchanges are exhausted, merge the oldest records into
    // one growing prefix. The range contains no per-step data or asset list.
    let records = w
        .messages
        .iter()
        .filter_map(|m| {
            if protected(m) {
                return None;
            }
            let id = m["_step_fold"].as_str()?;
            let step = old.get(id)?;
            let brief: Value = serde_json::from_str(m["content"].as_str()?).ok()?;
            Some((*step, brief["operations"].as_array().map_or(0, Vec::len)))
        })
        .collect::<Vec<_>>();
    let mut records = records;
    records.sort_by_key(|(step, _)| step.ordinal);
    let mut proposed = w.messages.clone();
    let mut merged = 0;
    for (step, operations) in records {
        if remaining < target {
            break;
        }
        let prefix = proposed
            .iter()
            .position(|m| m["_step_range"]["node_id"] == owner);
        let index = proposed
            .iter()
            .position(|m| m["_step_fold"] == step.node_id)
            .unwrap();
        assets::remember_deliveries(
            s.state.workspace.root(),
            s.task,
            &proposed[index]["_step_assets"],
        )?;
        let previous = prefix.map(|i| proposed[i]["_step_range"].clone());
        let from_step = previous
            .as_ref()
            .and_then(|v| v["from_step"].as_u64())
            .unwrap_or(step.ordinal as u64 + 1);
        let after_seq = previous
            .as_ref()
            .and_then(|v| v["after_seq"].as_i64())
            .unwrap_or(step.first_seq.unwrap_or(0) - 1);
        let operations = previous
            .as_ref()
            .and_then(|v| v["operations"].as_u64())
            .unwrap_or(0)
            + operations as u64;
        let range = json!({"node_id":owner,"from_step":from_step,"to_step":step.ordinal+1,
            "operations":operations,"after_seq":after_seq,"before_seq":step.last_seq.or(step.first_seq).unwrap_or(0)+1});
        let content = json!({"host_status":true,"not_a_user_request":true,"node_id":owner,
            "from_step":range["from_step"],"to_step":range["to_step"],"operations":operations,
            "details":{"tool":"read_session_history","arguments":{"task_id":s.task,"node_id":owner,"include_descendants":true,
                "after_seq":range["after_seq"],"before_seq":range["before_seq"]}}});
        proposed[index] = json!({"role":"user","name":"step_range","content":content.to_string(),"_step_range":range});
        if let Some(i) = prefix {
            let message = proposed.remove(index);
            proposed[i] = message;
        }
        merged += 1;
        let size = w.input_tokens(s, &request(s, &proposed, definitions));
        // A tiny record may be smaller than the initial range envelope. Keep
        // accumulating a candidate, but never commit a larger working view.
        if size < remaining {
            w.messages = proposed.clone();
            stats.merged += merged;
            merged = 0;
            remaining = size;
        }
    }
    Ok(stats)
}

/// Only explicit structured references are followed. No error-text parsing or
/// plugin-specific matching, no choice of fields/excerpts inside a saved asset.
fn referenced_ids(value: &Value, ids: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => {
            if let Some(id) = fields.get("asset_id").and_then(Value::as_str) {
                ids.insert(id.to_owned());
            }
            if let Some(values) = fields.get("asset_ids").and_then(Value::as_array) {
                ids.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            for value in fields.values() {
                referenced_ids(value, ids);
            }
        }
        Value::Array(values) => {
            for value in values {
                referenced_ids(value, ids);
            }
        }
        _ => {}
    }
}

async fn restore_references(
    s: &Session<'_>,
    w: &mut Window,
    definitions: &[Value],
) -> Result<usize> {
    let owner = task_frames::frame_task(s, w);
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let Some(latest) = tree::children(&conn, s.task, &owner)?
        .into_iter()
        .filter(|node| node.kind == "step" && node.agent_id.as_deref() == s.delegate_id())
        .max_by_key(|node| node.ordinal)
    else {
        return Ok(0);
    };
    drop(conn);
    let Some(boundary) = latest.first_seq else {
        return Ok(0);
    };
    let mut ids = BTreeSet::new();
    let mut supplied = BTreeSet::new();
    let mut released = BTreeSet::new();
    for message in &w.messages {
        let recent = sequence(message, s.task).is_some_and(|seq| seq >= boundary)
            || message[RESTORED_FOR] == latest.node_id;
        if (message["_asset_state"] != "reference_only" || message["_frame_input"] == true)
            && let Some(id) = message[assets::REFERENCE]["asset_id"].as_str()
        {
            // Both delivered bodies and explicit over-capacity receipts have
            // already gone through the ordinary delivery capacity boundary.
            supplied.insert(id.to_owned());
        }
        // A returning child task lists handles; the caller reads what it needs.
        if !recent || message["_child_return"] == true {
            continue;
        }
        if message["name"] == "asset_restore_status" {
            if let Some(id) = message["content"]
                .as_str()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|status| status["asset_id"].as_str().map(str::to_owned))
            {
                supplied.insert(id);
            }
            continue;
        }
        // Restoring one snapshot supplies its full data; references *inside*
        // that restored data do not recursively fetch an entire asset graph.
        if message.get(RESTORED_FOR).is_some() {
            continue;
        }
        if let Some(text) = message["content"]
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
        {
            referenced_ids(&text, &mut ids);
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            if let Some(args) = call["function"]["arguments"]
                .as_str()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            {
                if call["function"]["name"] == "release_assets" {
                    referenced_ids(&args, &mut released);
                    continue;
                }
                referenced_ids(&args, &mut ids);
            }
        }
    }
    let mut restored = 0;
    for id in ids.difference(&supplied) {
        if released.contains(id) {
            continue;
        }
        match load_saved_asset(s, &json!({"asset_id":id})).await {
            Ok(delivery) => {
                let reference = &delivery["asset_reference"];
                let mut message = assets::tagged(
                    json!({"role":"user","name":"step_asset","content":delivery.to_string()}),
                    reference,
                );
                message[RESTORED_FOR] = json!(latest.node_id);
                let mut candidate = w.messages.clone();
                candidate.push(message.clone());
                if let Some(image) = reference["artifact_id"].as_str() {
                    let mut message = assets::tagged(
                        json!({"role":"user","content":[{"type":"image_ref","artifact_id":image,"view_original":true}]}),
                        reference,
                    );
                    message[RESTORED_FOR] = json!(latest.node_id);
                    candidate.push(message);
                }
                let required = w.input_tokens(s, &request(s, &candidate, definitions));
                if required.saturating_add(4096) < s.input_budget {
                    w.messages = candidate;
                } else {
                    message["content"] = json!(
                        material_over_capacity(
                            s,
                            &delivery,
                            reference["history_reference"]["event_seq"].as_i64(),
                            required
                        )
                        .to_string()
                    );
                    message["_asset_state"] = json!("not_delivered");
                    w.messages.push(message);
                }
                restored += 1;
            }
            Err(error) => {
                ensure!(!s.cancel.is_cancelled(), "cancelled");
                let mut message = json!({"role":"user","name":"asset_restore_status","content":json!({
                    "asset_id":id,"status":"restore_failed","error":format!("{error:#}")}).to_string()});
                message[RESTORED_FOR] = json!(latest.node_id);
                w.messages.push(message);
            }
        }
    }
    Ok(restored)
}

/// Reserve room for an incoming result before withholding it. Old working
/// exchanges may fold; current arguments and the pending call stay untouched.
pub(super) async fn make_room(
    s: &Session<'_>,
    w: &mut Window,
    definitions: &[Value],
    reserve: usize,
) -> Result<()> {
    let pressure = Session {
        input_budget: s.input_budget.saturating_sub(reserve),
        ..s.clone()
    };
    let stats = fold(&pressure, w, definitions)?;
    if stats.folded > 0 || stats.merged > 0 {
        let (fixed, at, to) = limits(&pressure, w, definitions);
        append(s,"context/step_view",json!({"turn":s.turn,"reason":"incoming_delivery","folded_steps":stats.folded,"merged_steps":stats.merged,
            "restored_assets":0,"repeated_bodies":0,"estimated_input_tokens":stats.before,
            "after_input_tokens":w.input_tokens(s,&request(s,&w.messages,definitions)),
            "fixed_input_tokens":fixed,"fold_at_tokens":at,"fold_to_tokens":to}),false).await?;
    }
    Ok(())
}

pub(super) async fn prepare(s: &Session<'_>, w: &mut Window, definitions: &[Value]) -> Result<()> {
    if task_frames::no_task(s, w) {
        return Ok(());
    };
    let repeated = single_bodies(w);
    let mut stats = fold(s, w, definitions)?;
    let restored = restore_references(s, w, definitions).await?;
    if restored > 0 {
        let after_restore = fold(s, w, definitions)?;
        stats.folded += after_restore.folded;
        stats.merged += after_restore.merged;
        stats.expired |= after_restore.expired;
    }
    if stats.folded > 0 || stats.merged > 0 || restored > 0 || repeated > 0 || stats.expired {
        let (fixed, at, to) = limits(s, w, definitions);
        append(
            s,
            "context/step_view",
            json!({"turn":s.turn,"folded_steps":stats.folded,"merged_steps":stats.merged,"restored_assets":restored,"repeated_bodies":repeated,
                "estimated_input_tokens":stats.before,"after_input_tokens":w.input_tokens(s,&request(s,&w.messages,definitions)),
                "fixed_input_tokens":fixed,
                "fold_at_tokens":at,"fold_to_tokens":to}),
            false,
        )
        .await?;
        save(s, w).await?;
    }
    Ok(())
}
