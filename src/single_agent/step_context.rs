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
/// FOLD_AT percent of the input budget; the oldest then fold until it is back
/// under FOLD_TO. Materials read in nearby rounds stay usable together.
const FOLD_AT: usize = 40;
const FOLD_TO: usize = 25;

/// Where completed steps start to fold; reported to the context ring.
pub(super) fn fold_at(input_budget: usize) -> usize {
    input_budget * FOLD_AT / 100
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
    if estimated <= s.input_budget * FOLD_AT / 100 {
        return Ok(stats);
    }
    let target = s.input_budget * FOLD_TO / 100;
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
        match load_saved_asset(s, &json!({"asset_id":id}), None).await {
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

pub(super) async fn prepare(s: &Session<'_>, w: &mut Window, definitions: &[Value]) -> Result<()> {
    if s.mode != Mode::Main || task_frames::no_task(s, w) {
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
        append(
            s,
            "context/step_view",
            json!({"turn":s.turn,"folded_steps":stats.folded,"merged_steps":stats.merged,"restored_assets":restored,"repeated_bodies":repeated,
                "estimated_input_tokens":stats.before,"after_input_tokens":w.input_tokens(s,&request(s,&w.messages,definitions)),
                "fold_at_tokens":s.input_budget*FOLD_AT/100,"fold_to_tokens":s.input_budget*FOLD_TO/100}),
            false,
        )
        .await?;
        save(s, w).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (
        std::path::PathBuf,
        AgentServiceState,
        CancellationToken,
        Window,
    ) {
        let root = std::env::temp_dir().join(format!("step-view-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse().unwrap());
        let mut conn = agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','','fake','running',0,0)",[]).unwrap();
        let tx = conn.transaction().unwrap();
        tree::ensure_root(&tx, "t", 1).unwrap();
        tx.commit().unwrap();
        let cancel = CancellationToken::new();
        let mut window = Window {
            messages: vec![
                json!({"role":"system","content":Mode::Main.prompt()}),
                json!({"role":"user","content":"USER_REQUIREMENT_KEEP"}),
            ],
            ..Default::default()
        };
        let s = session(&state, &cancel);
        let call = crate::model_stream::ToolCall {
            call_id: "start".into(),
            name: "create_task".into(),
            arguments: json!({"goal":"Process the selected material"}).to_string(),
        };
        window.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}));
        task_frames::create_task(&s, &mut window, &call, "start", 1)
            .await
            .unwrap();
        (root, state, cancel, window)
    }

    fn session<'a>(state: &'a AgentServiceState, cancel: &'a CancellationToken) -> Session<'a> {
        Session {
            agent_id: None,
            task_node_id: None,
            step_node_id: None,
            plugin: None,
            state,
            task: "t",
            model: "fake",
            turn: 1,
            cancel,
            ban_execution: false,
            mode: Mode::Main,
            scope: "main".into(),
            parent: None,
            input_budget: INPUT_BUDGET,
        }
    }

    /// A budget the current view fills to `percent`. Over FOLD_AT, completed
    /// rounds fold oldest first until the view is back under FOLD_TO.
    fn occupied<'a>(
        state: &'a AgentServiceState,
        cancel: &'a CancellationToken,
        w: &Window,
        percent: usize,
    ) -> Session<'a> {
        let s = session(state, cancel);
        let input_budget = w.input_tokens(&s, &request(&s, &w.messages, &[])) * 100 / percent;
        Session { input_budget, ..s }
    }

    /// No room at all: every completed round folds.
    fn exhausted<'a>(state: &'a AgentServiceState, cancel: &'a CancellationToken) -> Session<'a> {
        Session {
            input_budget: 1,
            ..session(state, cancel)
        }
    }

    async fn round(
        s: &Session<'_>,
        w: &mut Window,
        name: &str,
        args: Value,
        result: Value,
    ) -> Value {
        let request = agent_service::uuid_like();
        task_frames::begin_round(s, w, &request).await.unwrap();
        let mut s = s.clone();
        s.task_node_id = w.task_node_id.clone();
        s.step_node_id = w.current_step_node_id.clone();
        let call_id = agent_service::uuid_like();
        let message = json!({"role":"assistant","tool_calls":[{"id":call_id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
        let seq = append(
            &s,
            "assistant/message",
            json!({"turn":1,"message":message}),
            false,
        )
        .await
        .unwrap();
        let mut stored = sourced(message, "t", seq);
        assets::index_arguments(
            s.state.workspace.root(),
            "t",
            std::slice::from_mut(&mut stored),
        )
        .unwrap();
        w.messages.push(stored);
        append(
            &s,
            "tool/call",
            json!({"callId":call_id,"flowNodeId":request,"name":name,"arguments":args.to_string()}),
            false,
        )
        .await
        .unwrap();
        let seq=append(&s,"tool/result",json!({"flowNodeId":request,"message":{"role":"tool","toolCallId":call_id},"meta":{"result":result}}),false).await.unwrap();
        let reference =
            assets::register(s.state.workspace.root(), "t", seq, name, &args, &result).unwrap();
        w.messages.push(assets::tagged(sourced(json!({"role":"tool","tool_call_id":call_id,"content":assets::delivered(&result,&reference).to_string()}),"t",seq),&reference));
        task_frames::complete_round(&s, w).await.unwrap();
        reference
    }

    #[tokio::test]
    async fn over_capacity_older_rounds_become_briefs_recent_native_exchange_and_originals_remain_complete()
     {
        let (root, state, cancel, mut w) = fixture().await;
        let s = session(&state, &cancel);
        let old = "OLD_BODY_".repeat(600);
        let recent = "CURRENT_BODY_".repeat(400);
        let reference = round(
            &s,
            &mut w,
            "example__read",
            json!({"path":"alpha","payload":old}),
            json!({"status":"succeeded","body":old,"nested":{"arbitrary":[1,2,3]}}),
        )
        .await;
        round(&s,&mut w,"example__write",json!({"path":"beta"}),json!({"status":"failed","applied":true,"body":recent,"error":"unknown commit outcome"})).await;
        let conn = agent_service::open_db(&root).unwrap();
        let originals = crate::session_history::load_events(
            &conn,
            "t",
            &crate::session_history::EventQuery {
                kinds: &["assistant/message", "tool/call", "tool/result"],
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .map(|row| (row.seq, row.raw))
        .collect::<Vec<_>>();
        drop(conn);
        let s = occupied(&state, &cancel, &w, 50);
        prepare(&s, &mut w, &[]).await.unwrap();
        let wire = request(&s, &w.messages, &[]);
        assert!(!wire.to_string().contains(&old));
        assert!(wire.to_string().contains(&recent));
        assert!(wire.to_string().contains("USER_REQUIREMENT_KEEP"));
        let native = wire["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect::<Vec<_>>();
        assert_eq!(native.len(), 2); // the task-start receipt plus the most recent operation
        assert!(
            native.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("unknown commit outcome")
        );
        let briefs = w
            .messages
            .iter()
            .filter(|m| m["name"] == "step_result")
            .collect::<Vec<_>>();
        let ranges = w
            .messages
            .iter()
            .filter(|m| m["name"] == "step_range")
            .count();
        assert_eq!(briefs.len() + ranges, 1);
        let index =
            assets::frame_index(&root, "t", &task_frames::frame_task(&s, &w), &w.messages).unwrap();
        assert!(
            index["assets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["asset_id"] == reference["asset_id"])
        );
        assert_eq!(
            assets::read(&root, "t", &json!({"asset_id":reference["asset_id"]}), None).unwrap()["original_result"]
                ["body"],
            old
        );
        let conn = agent_service::open_db(&root).unwrap();
        for (seq, raw) in originals {
            assert_eq!(
                conn.query_row(
                    "SELECT data FROM agent_task_events WHERE task_id='t' AND seq=?1",
                    [seq],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
                raw
            );
        }
        let count = w.messages.len();
        prepare(&s, &mut w, &[]).await.unwrap();
        assert_eq!(w.messages.len(), count, "preparation is idempotent");
    }

    #[tokio::test]
    async fn repeated_rounds_bound_body_growth_and_keep_latest_delivery_when_content_reverts() {
        let (root, state, cancel, mut w) = fixture().await;
        let s = session(&state, &cancel);
        let first = json!({"content":"FIRST_".repeat(4000)});
        let old = round(
            &s,
            &mut w,
            "read_file",
            json!({"path":"same"}),
            first.clone(),
        )
        .await;
        for n in 0..8 {
            round(
                &s,
                &mut w,
                "read_file",
                json!({"path":"same"}),
                json!({"content":format!("VERSION_{n}_").repeat(4000)}),
            )
            .await;
        }
        let repeated = round(&s, &mut w, "read_file", json!({"path":"same"}), first).await;
        assert_eq!(
            old["asset_id"], repeated["asset_id"],
            "identical content reuses its immutable snapshot"
        );
        let before = w.input_tokens(&s, &request(&s, &w.messages, &[]));
        prepare(&s, &mut w, &[]).await.unwrap();
        let wire = request(&s, &w.messages, &[]);
        let after = w.input_tokens(&s, &wire);
        assert!(
            after < before && after <= s.input_budget * FOLD_AT / 100,
            "repeated rounds stay within the working view: {before} -> {after}"
        );
        let text = wire.to_string();
        assert!(!text.contains("VERSION_0_"), "the oldest rounds fold first");
        assert!(
            text.contains("VERSION_7_"),
            "recent rounds keep their native exchange"
        );
        assert_eq!(
            text.matches("FIRST_").count(),
            4000,
            "an identical saved result is carried once"
        );
        let index =
            assets::frame_index(&root, "t", &task_frames::frame_task(&s, &w), &w.messages).unwrap();
        let reads = index["assets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["source_tool"] == "read_file")
            .collect::<Vec<_>>();
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0]["asset_id"], repeated["asset_id"]);
        // Once the repeated delivery itself is folded, its delivery order still
        // takes priority over the snapshot's older first-registration sequence.
        round(
            &s,
            &mut w,
            "example__continue",
            json!({}),
            json!({"done":true}),
        )
        .await;
        let s = exhausted(&state, &cancel);
        prepare(&s, &mut w, &[]).await.unwrap();
        assert!(!request(&s, &w.messages, &[]).to_string().contains("FIRST_"));
        let index =
            assets::frame_index(&root, "t", &task_frames::frame_task(&s, &w), &w.messages).unwrap();
        assert_eq!(
            index["assets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["source_tool"] == "read_file")
                .unwrap()["asset_id"],
            repeated["asset_id"]
        );
        println!("step context request tokens: {before} -> {after}");
    }

    #[tokio::test]
    async fn restoring_one_asset_is_not_recursive_and_explicit_release_is_respected() {
        let (_root, state, cancel, mut w) = fixture().await;
        let s = session(&state, &cancel);
        let nested = round(
            &s,
            &mut w,
            "example__read",
            json!({"id":"nested"}),
            json!({"body":"NESTED_BODY"}),
        )
        .await;
        let outer = round(
            &s,
            &mut w,
            "example__read",
            json!({"id":"outer"}),
            json!({"body":"OUTER_BODY","asset_id":nested["asset_id"]}),
        )
        .await;
        round(
            &s,
            &mut w,
            "example__inspect",
            json!({}),
            json!({"asset_id":outer["asset_id"]}),
        )
        .await;
        let s = occupied(&state, &cancel, &w, 50);
        prepare(&s, &mut w, &[]).await.unwrap();
        prepare(&s, &mut w, &[]).await.unwrap();
        let wire = request(&s, &w.messages, &[]).to_string();
        assert!(wire.contains("OUTER_BODY"));
        assert!(!wire.contains("NESTED_BODY"));
        let mut inherited = w
            .messages
            .iter()
            .find(|m| m["name"] == "step_asset")
            .unwrap()
            .clone();
        inherited["_frame_input"] = json!(true);
        inherited["name"] = json!("saved_asset");
        w.messages.push(inherited);
        round(
            &s,
            &mut w,
            "release_assets",
            json!({"asset_ids":[outer["asset_id"]]}),
            json!({"released":true}),
        )
        .await;
        // The ordinary transient restoration expires; an explicitly declared
        // frame input survives until the frame/user explicitly releases it.
        prepare(&s, &mut w, &[]).await.unwrap();
        assert!(!w.messages.iter().any(|m| m["name"] == "step_asset"));
        assert!(w.messages.iter().any(|m| m["name"] == "saved_asset"
            && m["_frame_input"] == true
            && m.get(RESTORED_FOR).is_none()));
        assets::release(
            &mut w.messages,
            &[outer["asset_id"].as_str().unwrap().to_owned()],
        )
        .unwrap();
        assert!(
            !request(&s, &w.messages, &[])
                .to_string()
                .contains("OUTER_BODY")
        );
    }

    #[tokio::test]
    async fn references_restore_the_entire_recorded_version_without_a_model_read_or_action_replay()
    {
        let (root, state, cancel, mut w) = fixture().await;
        let s = session(&state, &cancel);
        let original = json!({"content":"VERSION_ONE_".repeat(3000),"fields":{"keep_everything":[true,7,{"unchanged":"value"}]}});
        let old = round(
            &s,
            &mut w,
            "read_file",
            json!({"path":"same-resource"}),
            original.clone(),
        )
        .await;
        let new = round(
            &s,
            &mut w,
            "read_file",
            json!({"path":"same-resource"}),
            json!({"content":"VERSION_TWO_".repeat(3000)}),
        )
        .await;
        round(&s,&mut w,"example__inspect",json!({}),json!({"status":"issue","evidence":{"asset_reference":{"asset_id":old["asset_id"]}},"finding":"Needs correction"})).await;
        let s = occupied(&state, &cancel, &w, 50);
        prepare(&s, &mut w, &[]).await.unwrap();
        let restored = w
            .messages
            .iter()
            .filter(|m| m["name"] == "step_asset")
            .collect::<Vec<_>>();
        assert_eq!(restored.len(), 1);
        let delivery: Value =
            serde_json::from_str(restored[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            delivery["original_result"], original,
            "no excerpt, field selection or replacement with a newer asset"
        );
        let index =
            assets::frame_index(&root, "t", &task_frames::frame_task(&s, &w), &w.messages).unwrap();
        let reads = index["assets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["source_tool"] == "read_file")
            .collect::<Vec<_>>();
        assert_eq!(reads.len(), 1);
        assert_eq!(
            reads[0]["asset_id"], new["asset_id"],
            "the index keeps the latest invocation snapshot while explicit restoration keeps its exact historical version"
        );
        let count = w.messages.len();
        prepare(&s, &mut w, &[]).await.unwrap();
        assert_eq!(w.messages.len(), count);
        let conn = agent_service::open_db(&root).unwrap();
        let reads:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='tool/call' AND json_extract(data,'$.name')='read_asset'",[],|row|row.get(0)).unwrap();
        assert_eq!(reads, 0);
        drop(conn);
        round(
            &s,
            &mut w,
            "example__continue",
            json!({}),
            json!({"done":true}),
        )
        .await;
        prepare(&s, &mut w, &[]).await.unwrap();
        assert!(
            !request(&s, &w.messages, &[])
                .to_string()
                .contains("VERSION_ONE_"),
            "restored data leaves the working view when it is no longer the recent step's input"
        );
    }

    /// One tool call per round: a specification and a template list arrive in
    /// successive rounds and must stay usable together (task_b15cadfb read
    /// them alternately for 85 rounds when each round folded the previous one).
    #[tokio::test]
    async fn materials_read_in_successive_rounds_stay_together_below_capacity() {
        let (_root, state, cancel, mut w) = fixture().await;
        let s = session(&state, &cancel);
        let spec = round(
            &s,
            &mut w,
            "example__read_files",
            json!({"paths":["spec.md"]}),
            json!({"files":[{"content":"SPEC_RULE_".repeat(2000)}]}),
        )
        .await;
        let template = round(
            &s,
            &mut w,
            "example__list_templates",
            json!({}),
            json!({"templates":[{"pages":"TEMPLATE_PAGE_".repeat(1000)}]}),
        )
        .await;
        for next in 0..3 {
            prepare(&s, &mut w, &[]).await.unwrap();
            let wire = request(&s, &w.messages, &[]);
            let text = wire.to_string();
            assert_eq!(text.matches("SPEC_RULE_").count(), 2000, "round {next}");
            assert_eq!(text.matches("TEMPLATE_PAGE_").count(), 1000, "round {next}");
            assert!(!w.messages.iter().any(|m| m["name"] == "step_result"));
            let index: Value = serde_json::from_str(
                wire["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| m["name"] == "task_asset_index")
                    .unwrap()["content"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            for id in [&spec["asset_id"], &template["asset_id"]] {
                let entry = index["assets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|a| &a["asset_id"] == id)
                    .unwrap();
                assert_eq!(entry["in_context"], true);
            }
            // Further small actions do not push either material out.
            round(
                &s,
                &mut w,
                "example__project",
                json!({"check":next}),
                json!({"status":"ready"}),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn over_capacity_the_oldest_rounds_fold_first_and_the_newest_exchange_stays() {
        let (_root, state, cancel, mut w) = fixture().await;
        let s = session(&state, &cancel);
        for n in 0..10 {
            round(
                &s,
                &mut w,
                "example__read",
                json!({"part":n}),
                json!({"body":format!("PART_{n}_").repeat(1200)}),
            )
            .await;
        }
        let s = occupied(&state, &cancel, &w, 50);
        prepare(&s, &mut w, &[]).await.unwrap();
        let wire = request(&s, &w.messages, &[]);
        let text = wire.to_string();
        let kept = (0..10)
            .map(|n| text.contains(&format!("PART_{n}_")))
            .collect::<Vec<_>>();
        let first = kept.iter().position(|kept| *kept).unwrap();
        assert!(
            first > 0 && kept[first..].iter().all(|kept| *kept),
            "the oldest rounds fold first: {kept:?}"
        );
        assert!(first < 9, "completed rounds that fit stay native: {kept:?}");
        assert_eq!(
            w.messages
                .iter()
                .filter(|m| m["name"] == "step_result")
                .count(),
            first
        );
        assert!(w.input_tokens(&s, &wire) <= s.input_budget * FOLD_AT / 100);
        let count = w.messages.len();
        prepare(&s, &mut w, &[]).await.unwrap();
        assert_eq!(
            w.messages.len(),
            count,
            "back under capacity nothing more folds"
        );
    }

    #[tokio::test]
    async fn an_identical_saved_result_delivered_again_is_carried_once() {
        let (_root, state, cancel, mut w) = fixture().await;
        let s = session(&state, &cancel);
        let page = json!({"html":"PAGE_THREE_".repeat(500)});
        let first = round(
            &s,
            &mut w,
            "example__get_design",
            json!({"page":3}),
            page.clone(),
        )
        .await;
        round(
            &s,
            &mut w,
            "example__check",
            json!({}),
            json!({"issues":[]}),
        )
        .await;
        let again = round(&s, &mut w, "example__get_design", json!({"page":3}), page).await;
        assert_eq!(first["asset_id"], again["asset_id"]);
        prepare(&s, &mut w, &[]).await.unwrap();
        let wire = request(&s, &w.messages, &[]);
        assert_eq!(wire.to_string().matches("PAGE_THREE_").count(), 500);
        let messages = wire["messages"].as_array().unwrap();
        for (index, message) in messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "tool")
        {
            let call = messages[..index]
                .iter()
                .rev()
                .find(|m| m["role"] == "assistant")
                .unwrap();
            assert!(
                call["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|c| c["id"] == message["tool_call_id"]),
                "native call pairing remains"
            );
        }
        let receipt = messages
            .iter()
            .find(|m| {
                m["role"] == "tool" && m["content"].as_str().unwrap().contains("reference_only")
            })
            .unwrap();
        assert!(
            receipt["content"]
                .as_str()
                .unwrap()
                .contains(first["asset_id"].as_str().unwrap())
        );
    }

    #[tokio::test]
    async fn long_chain_records_merge_under_pressure_and_extend_one_recoverable_prefix() {
        let (root, state, cancel, mut w) = fixture().await;
        let s = session(&state, &cancel);
        for n in 0..60 {
            round(
                &s,
                &mut w,
                "example__read",
                json!({"part":n}),
                json!({"value":n}),
            )
            .await;
        }
        let native = w.messages.clone();
        prepare(&s, &mut w, &[]).await.unwrap();
        assert_eq!(
            native, w.messages,
            "record count alone never triggers folding"
        );
        let latest = w.current_step_node_id.clone();
        let conn = agent_service::open_db(&root).unwrap();
        let newest = tree::children(&conn, "t", &task_frames::frame_task(&s, &w))
            .unwrap()
            .into_iter()
            .filter(|n| n.kind == "step")
            .max_by_key(|n| n.ordinal)
            .unwrap();
        let recent = w
            .messages
            .iter()
            .filter(|m| sequence(m, "t").is_some_and(|seq| seq >= newest.first_seq.unwrap()))
            .cloned()
            .collect::<Vec<_>>();
        drop(conn);
        let pressure = occupied(&state, &cancel, &w, 50);
        prepare(&pressure, &mut w, &[]).await.unwrap();
        assert!(
            w.input_tokens(&pressure, &request(&pressure, &w.messages, &[]))
                < pressure.input_budget * FOLD_TO / 100
        );
        assert_eq!(w.current_step_node_id, latest);
        for message in recent {
            assert!(
                w.messages.contains(&message),
                "newest exchange is untouched"
            );
        }
        let ranges = w
            .messages
            .iter()
            .filter(|m| m["name"] == "step_range")
            .collect::<Vec<_>>();
        assert_eq!(ranges.len(), 1);
        let first: Value = serde_json::from_str(ranges[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(first["from_step"], 1);
        assert_eq!(first["operations"], first["to_step"]);
        assert!(first.get("asset_ids").is_none() && first.get("steps").is_none());
        assert!(
            request(&pressure, &w.messages, &[])["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m.get("_step_range").is_none())
        );
        // The history pointer recovers only events inside this task's prefix.
        let mut args = first["details"]["arguments"].clone();
        args["limit"] = json!(12);
        let page = crate::session_history::read(&root, "t", &args)
            .await
            .unwrap();
        assert!(!page["records"].as_array().unwrap().is_empty());
        for record in page["records"].as_array().unwrap() {
            let seq = record["seq"].as_i64().unwrap();
            assert!(
                seq > args["after_seq"].as_i64().unwrap()
                    && seq < args["before_seq"].as_i64().unwrap()
            );
        }
        let once = w.messages.clone();
        prepare(&pressure, &mut w, &[]).await.unwrap();
        assert_eq!(once, w.messages);
        for n in 60..75 {
            round(
                &s,
                &mut w,
                "example__read",
                json!({"part":n}),
                json!({"value":n}),
            )
            .await;
        }
        let all = exhausted(&state, &cancel);
        prepare(&all, &mut w, &[]).await.unwrap();
        let ranges = w
            .messages
            .iter()
            .filter(|m| m["name"] == "step_range")
            .collect::<Vec<_>>();
        assert_eq!(
            ranges.len(),
            1,
            "the same prefix grows; no nested summaries"
        );
        let extended: Value = serde_json::from_str(ranges[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(extended["from_step"], first["from_step"]);
        assert_eq!(extended["to_step"], 74);
        assert_eq!(extended["operations"], 74);
        assert!(extended.to_string().len() <= first.to_string().len() + 30);
        assert!(!w.messages.iter().any(|m| m["name"] == "step_result"));
        let conn = agent_service::open_db(&root).unwrap();
        let owner = task_frames::frame_task(&s, &w);
        assert_eq!(
            tree::children(&conn, "t", &owner).unwrap().len(),
            76,
            "folding never creates tasks or changes the graph"
        );
    }

    #[tokio::test]
    async fn oversized_reference_restoration_returns_a_paging_receipt_without_loading_body() {
        let (_root, state, cancel, mut w) = fixture().await;
        let s = session(&state, &cancel);
        let reference = round(
            &s,
            &mut w,
            "example__read",
            json!({}),
            json!({"body":"HUGE_BODY_".repeat(30000)}),
        )
        .await;
        round(
            &s,
            &mut w,
            "example__inspect",
            json!({}),
            json!({"asset_id":reference["asset_id"]}),
        )
        .await;
        // Fixed inputs alone need more than 25%; folding must simply exhaust
        // what it can, and automatic restoration must respect hard capacity.
        let pressure = Session {
            input_budget: 18000,
            ..s.clone()
        };
        prepare(&pressure, &mut w, &[]).await.unwrap();
        let receipt = w
            .messages
            .iter()
            .find(|m| m["name"] == "step_asset")
            .unwrap();
        let data: Value = serde_json::from_str(receipt["content"].as_str().unwrap()).unwrap();
        assert_eq!(data["error_code"], "MATERIAL_OVER_CAPACITY");
        assert_eq!(data["delivered"], false);
        assert!(data["total_chars"].as_u64().unwrap() > 24000);
        assert_eq!(data["next_read"]["arguments"]["max_chars"], PAGE_CHARS);
        assert!(
            !request(&pressure, &w.messages, &[])
                .to_string()
                .contains("HUGE_BODY_")
        );
        let once = w.messages.clone();
        prepare(&pressure, &mut w, &[]).await.unwrap();
        assert_eq!(
            once, w.messages,
            "a withheld restoration is not retried every round"
        );
        let page = load_saved_asset(&pressure, &data["next_read"]["arguments"], None)
            .await
            .unwrap();
        assert_eq!(page["returned_chars"], PAGE_CHARS);
        assert!(page["next_char_offset"].as_u64().is_some());
    }
}
