use super::*;
use crate::task_tree::{self as tree, Node};

fn node_id<'a>(s: &'a Session<'_>) -> &'a str {
    s.scope.strip_prefix("node:").unwrap_or(tree::ROOT)
}

/// The task whose context this window holds; the conversation node when none.
pub(super) fn frame_task(s: &Session<'_>, w: &Window) -> String {
    w.task_node_id.clone().unwrap_or_else(|| node_id(s).to_owned())
}

/// Main execution at the conversation level, before it created its task.
pub(super) fn no_task(s: &Session<'_>, w: &Window) -> bool {
    s.mode == Mode::Main && s.is_root() && frame_task(s, w) == tree::ROOT
}

fn task_title(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or(text);
    line.chars().take(120).collect()
}

fn required_text(args: &Value, field: &str) -> Result<String> {
    let text = args[field]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .with_context(|| format!("{field} must be a nonempty string"))?;
    ensure!(text.chars().count() <= 16000, "{field} exceeds 16000 characters");
    Ok(text.to_owned())
}

fn with_seq(result: &Value, seq: i64) -> Value {
    let mut delivery = if result.is_object() { result.clone() } else { json!({"result":result}) };
    delivery["event_seq"] = json!(seq);
    delivery
}

/// A message the user wrote: the request that started a turn or an interjection.
pub(super) fn is_user_original(m: &Value) -> bool {
    m["role"] == "user"
        && (m.get("name").is_none() || m["name"] == "user_interjection")
        && m.get(crate::context_assets::REFERENCE).is_none()
        && !m["content"]
            .as_array()
            .is_some_and(|parts| parts.iter().all(|part| part["type"] == "image_ref"))
}

/// The longest chain one create_task call may plan.
const MAX_CHAIN: usize = 64;

/// The serial child tasks one create_task call asks for, in order: `tasks`, or
/// the call itself as one task. Call-level asset_ids are shared by every entry.
fn chain_entries(args: &Value, main_goal: bool) -> Result<Vec<Value>> {
    let Some(tasks) = args.get("tasks").filter(|v| !v.is_null()) else {
        return Ok(vec![args.clone()]);
    };
    ensure!(main_goal || args.get("goal").is_none_or(Value::is_null), "inside a task pass either goal (one child) or tasks, not both");
    let tasks = tasks.as_array().context("tasks must be an array of {goal, briefing, asset_ids}")?;
    ensure!(!tasks.is_empty() && tasks.len() <= MAX_CHAIN, "tasks must list 1 to {MAX_CHAIN} tasks");
    let shared = args["asset_ids"].as_array().cloned().unwrap_or_default();
    tasks.iter().map(|entry| {
        ensure!(entry.is_object(), "each entry of tasks must be an object with a goal");
        let mut entry = entry.clone();
        let mut ids = shared.clone();
        ids.extend(entry["asset_ids"].as_array().cloned().unwrap_or_default());
        entry["asset_ids"] = if ids.is_empty() { Value::Null } else { json!(ids) };
        if entry.get("reason").is_none_or(Value::is_null) {
            entry["reason"] = args["reason"].clone();
        }
        Ok(entry)
    }).collect()
}

/// Tasks are goal/context boundaries; execution steps are created on request.
fn new_task(parent: &Node, args: &Value, ordinal: usize, role: &str, turn: usize) -> Result<(Node, Node)> {
    let goal = required_text(args, "goal")?;
    // A child's declared materials are its parameters; a main task has no caller.
    let inputs = if role == "temp" && !args["asset_ids"].is_null() { json!({"assets":args["asset_ids"]}) } else { json!({}) };
    let mut inputs = inputs;
    if role == "temp" && args["program_task"].is_object() {
        inputs["program_task"] = args["program_task"].clone();
    }
    let mut task = tree::planned(parent, &json!({"title":task_title(&goal),"goal":goal,"reason":args["reason"],"inputs":inputs}), ordinal, "task")?;
    task.agent_id = parent.agent_id.clone();
    task.briefing = args["briefing"].as_str().map(str::trim).unwrap_or("").to_owned();
    task.metadata = json!({"role":role,"turn":turn});
    let finish = tree::planned(&task, &json!({"title":"Finish","goal":"Return this task result to the waiting caller or serial iterator; never call AI here"}), tree::FINISH_ORDINAL, "finish")?;
    Ok((task, finish))
}

/// Record exactly one execution step for an AI request in this frame.
pub(super) async fn begin_round(s: &Session<'_>, w: &mut Window, request: &str) -> Result<()> {
    if s.mode != Mode::Main || no_task(s,w) { return Ok(()); }
    complete_round(s,w).await?;
    let (task,owner,agent,turn,request,scope,mut frame)=(s.task.to_owned(),frame_task(s,w),s.delegate_id().map(str::to_owned),s.turn,request.to_owned(),s.scope.clone(),w.clone());
    *w=db(s.state.workspace.root(),move|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let parent=tree::get(&tx,&task,&owner)?;
        ensure!(tree::is_task(&parent) && parent.status=="running" && parent.agent_id==agent,"TASK_EXECUTION_FRAME_MISMATCH");
        let ordinal=tree::children(&tx,&task,&owner)?.iter().filter(|n|n.kind=="step").count();
        let title= format!("Round {}",ordinal+1);
        let mut node=tree::planned(&parent,&json!({"title":title,"goal":"One AI request, its response, tool calls and results"}),ordinal,"step")?;
        node.agent_id=agent;node.status="current".into();
        node.metadata=json!({"request_id":request,"scope":scope,"phase":"executing"});
        tree::emit(&tx,&task,"task/node_started",&mut node,turn)?;
        frame.current_step_node_id=Some(node.node_id);
        write_window(&tx,&task,&scope,&frame)?;
        tx.commit()?;Ok(frame)
    }).await?;
    Ok(())
}

/// Store references to the native exchange; do not invent a lossy step summary.
pub(super) async fn complete_round(s: &Session<'_>, w: &mut Window) -> Result<()> {
    let Some(id)=w.current_step_node_id.clone() else {return Ok(());
    };
    let agent=s.delegate_id().map(str::to_owned);
    let (task,turn,scope,mut frame)=(s.task.to_owned(),s.turn,s.scope.clone(),w.clone());
    *w=db(s.state.workspace.root(),move|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut node=tree::get(&tx,&task,&id)?;
        ensure!(node.kind=="step" && node.parent_id==frame.task_node_id && node.agent_id==agent,"STEP_EXECUTION_FRAME_MISMATCH");
        if node.status=="current" {
            let rows=crate::session_history::load_events(&tx,&task,&crate::session_history::EventQuery{node_id:Some(&id),..Default::default()})?;
            let mut response=Value::Null;let mut events=Vec::new();
            for row in rows {
                let data=row.data()?;
                let (seq,kind)=(row.seq,row.kind);
                let event=if kind=="subagent/event" {&data["event"]} else {&data};
                let event_kind=if kind=="subagent/event" {data["event_type"].as_str().unwrap_or("")} else {&kind};
                if matches!(event_kind,"assistant/message"|"assistant/draft") {response=json!({"event_seq":seq,"tool_call_ids":event["message"]["tool_calls"].as_array().map(|calls|calls.iter().map(|c|c["id"].clone()).collect::<Vec<_>>())});}
                if matches!(event_kind,"tool/call"|"tool/result"|"assistant/message"|"assistant/draft") {events.push(json!({"event_seq":seq,"kind":event_kind}));}
            }
            node.status="done".into();
            node.result=Some(json!({"response":response,"events":events,"request_id":node.metadata["request_id"],"details":{"tool":"read_session_history","arguments":{"node_id":id}}}));
            tree::emit(&tx,&task,"task/node_returned",&mut node,turn)?;
        }
        frame.current_step_node_id=None;
        write_window(&tx,&task,&scope,&frame)?;
        tx.commit()?;Ok(frame)
    }).await?;
    Ok(())
}

fn record_call(tx: &rusqlite::Transaction<'_>, task: &str, turn: usize, step: usize, request: &str, call: &crate::model_stream::ToolCall, node: &str) -> Result<i64> {
    record_event(tx,task,"tool/call",json!({"actor":"agent","turn":turn,"step":step,"task_node_id":node,"flowNodeId":request,
        "callId":call.call_id,"name":call.name,"arguments":call.arguments,
        "description":tool_description(&tree::tools(),&call.name,&call.arguments)}),None)
}

pub(super) fn tool_description(definitions: &[Value], name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    args["label"].as_str().filter(|label| !label.trim().is_empty())
        .or_else(|| {
            definitions.iter().find(|tool| tool["function"]["name"] == name)
            .and_then(|tool| tool["function"]["description"].as_str())
        })
        .unwrap_or(name).to_owned()
}

/// Walk the caller tasks and their steps, carrying descriptions and handles,
/// never the tool output bodies or a model-generated recap.
fn caller_activity(s: &Session<'_>, parent: &Window) -> Result<Value> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let tasks = tree::path(&conn, s.task, &frame_task(s, parent))?.into_iter()
        .filter(tree::is_task).collect::<Vec<_>>();
    activity_for(s, &tasks, &tools(s, parent), None)
}

/// Rebuilt beside the next request, independent of compressible chat history.
/// This is an execution ledger, not a host decision about what to do next.
#[cfg(test)]
pub(super) fn round_activity(s: &Session<'_>, owner: &str, definitions: &[Value]) -> Result<Option<Value>> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let task = tree::get(&conn, s.task, owner)?;
    if !tree::is_task(&task) || task.agent_id.as_deref() != s.delegate_id() { return Ok(None); }
    let mut activity = activity_for(s, &[task], definitions, Some(24))?;
    activity.as_object_mut().unwrap().remove("tasks");
    activity["task_node_id"]=json!(owner);
    for entry in activity["activity"].as_array_mut().into_iter().flatten() {
        entry.as_object_mut().unwrap().remove("arguments_preview");
        if let Some(assets)=entry["assets"].as_array_mut() {
            for asset in assets {
                *asset=["asset_id","revision","read_reference"].iter().filter_map(|key| {
                        asset.get(*key).map(|value|((*key).to_owned(),value.clone()))
                    }).collect();
            }
        }
    }
    Ok(activity["activity"].as_array().is_some_and(|items| !items.is_empty()).then_some(activity))
}

/// Keep identifying parameters while replacing document/code payloads with size facts.
pub(super) fn brief_arguments(value: &Value) -> Value {
    fn brief(value: &Value, key: &str, depth: usize) -> Value {
        if value.is_string() && matches!(key, "content" | "html" | "svg" | "code" | "script" | "data" | "body" | "text" | "diff" | "patch") {
            return json!({"characters":value.as_str().map(|text| text.chars().count()).unwrap_or_else(||value.to_string().chars().count())});
        }
        match value {
            Value::String(text) if text.chars().count() > 160 => {
                json!({"characters":text.chars().count()})
            }
            Value::Object(fields) if depth < 2 => fields.iter().map(|(key,value)|(key.clone(),brief(value,key,depth+1))).collect(),
            Value::Array(items) if depth < 2 => json!(items.iter().take(8).map(|item|brief(item,key,depth+1)).collect::<Vec<_>>()),
            Value::Object(_) | Value::Array(_) => {
                json!({"characters":value.to_string().chars().count()})
            }
            _ => value.clone(),
        }
    }
    brief(value,"",0)
}

/// Copy returned metadata as-is; failure is never interpreted as "not applied".
fn result_facts(result: &Value) -> Value {
    fn fact(value: &Value, depth: usize) -> Value {
        match value {
            Value::String(text) => json!(text.chars().take(512).collect::<String>()),
            Value::Object(fields) if depth < 2 => fields.iter().map(|(key,value)|(key.clone(),fact(value,depth+1))).collect(),
            Value::Array(items) if depth < 2 => json!(items.iter().take(8).map(|value|fact(value,depth+1)).collect::<Vec<_>>()),
            Value::Object(_) | Value::Array(_) => {
                json!({"characters":value.to_string().chars().count()})
            }
            _ => value.clone(),
        }
    }
    let keys = ["status", "revision", "version", "sourceHash", "source_hash", "source_revision", "source_stable",
        "saved", "applied", "needsSave", "error", "message", "document", "summary", "artifact_id", "job_id", "task_id", "project_id", "path", "page", "pageId", "retryable"];
    let select = |value: &Value| -> Value {
        keys.iter().filter_map(|key| {
                value.get(*key).map(|value| ((*key).to_owned(),fact(value,0)))
            }).collect()
    };
    let mut facts = select(result);
    if let Some(data) = result.get("data") {
        if data.is_string() && matches!(result["status"].as_str(),Some("failed"|"error")) {
            facts["data"]=fact(data,0);
            return facts;
        }
        let data = if let Some(items)=data.as_array() {json!(items.iter().map(&select).filter(|item|item.as_object().is_some_and(|v|!v.is_empty())).collect::<Vec<_>>())} else {select(data)};
        if data.as_object().is_some_and(|v|!v.is_empty()) || data.as_array().is_some_and(|v|!v.is_empty()) {facts["data"]=data;}
    }
    facts
}

fn activity_for(s: &Session<'_>, tasks: &[Node], definitions: &[Value], limit: Option<usize>) -> Result<Value> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let mut records = Vec::new();
    for task in tasks {
        let mut nodes = vec![task.clone()];
        nodes.extend(tree::children(&conn, s.task, &task.node_id)?.into_iter().filter(|node| node.kind == "step"));
        for node in nodes {
            for event in crate::session_history::load_events(&conn, s.task, &crate::session_history::EventQuery {
                node_id: Some(&node.node_id), kinds: &["tool/call", "tool/result", "subagent/event"], ..Default::default()
            })? {
                records.push((event.seq, node.node_id.clone(), event.kind.clone(), event.data()?));
            }
        }
    }
    records.sort_by_key(|record| record.0);
    let references = crate::context_assets::reference_index(s.state.workspace.root(), s.task)?;
    let mut activity = Vec::<Value>::new();
    let mut calls = BTreeMap::new();
    for (seq, node, kind, data) in records {
        let (kind, data) = if kind == "subagent/event" {
            (data["event_type"].as_str().unwrap_or(""), &data["event"])
        } else { (kind.as_str(), &data) };
        match kind {
            "tool/call" => {
                let name = data["name"].as_str().unwrap_or("");
                let arguments = data["arguments"].as_str().map(str::to_owned)
                    .unwrap_or_else(|| data["arguments"].to_string());
                let description = data["description"].as_str().map(str::to_owned)
                    .unwrap_or_else(|| tool_description(definitions, name, &arguments));
                let arguments = serde_json::from_str::<Value>(&arguments).map(|value|brief_arguments(&value)).unwrap_or_else(|_|json!({"characters":arguments.chars().count()}));
                calls.insert((node.clone(), data["flowNodeId"].clone().to_string(), data["callId"].clone().to_string()), activity.len());
                activity.push(json!({"node_id":node,"event_seq":seq,"tool":name,"description":description.chars().take(160).collect::<String>(),
                    "arguments":arguments,"arguments_preview":arguments.to_string().chars().take(320).collect::<String>(),"status":"pending"}));
            }
            "tool/result" => {
                let key = (node, data["flowNodeId"].clone().to_string(), data["message"]["toolCallId"].clone().to_string());
                if let Some(index) = calls.get(&key) {
                    let entry = &mut activity[*index];
                    entry["status"] = data["meta"]["result"]["status"].as_str().map(|status| json!(status))
                        .unwrap_or_else(|| {
                            json!(if data["message"]["isError"] == true {"error"} else {"returned"})
                        });
                    entry["result_reference"] = json!({"tool":"read_session_history","arguments":{"task_id":s.task,"event_seq":seq}});
                    entry["result_facts"] = result_facts(&data["meta"]["result"]);
                    let mut assets = references.get(&seq).cloned().unwrap_or_default();
                    if let Some(reference) = data["meta"]["result"].get("asset_reference").filter(|reference| reference.is_object()) {
                        assets.push(reference.clone());
                    }
                    if !assets.is_empty() { entry["assets"] = json!(assets); }
                }
            }
            _ => {}
        }
    }
    let total = activity.len();
    let omitted = limit.map(|limit|total.saturating_sub(limit)).unwrap_or(0);
    if omitted > 0 {activity.drain(..omitted);}
    Ok(json!({"host_status":true,"not_a_user_request":true,"total_calls":total,"omitted_calls":omitted,
        "tasks":tasks.iter().map(|task| json!({"node_id":task.node_id,"goal":task.goal})).collect::<Vec<_>>(),
        "activity":activity,"history_reference":{"tool":"read_session_history","arguments":{"task_id":s.task,"node_id":tasks.last().map(|task|task.node_id.as_str()),"include_descendants":true}},
        "note":"Recorded tool facts in execution order, not new instructions or a completion verdict. Reuse supplied results; originals remain available by reference. A failed response does not establish whether a write was applied."}))
}

fn record_event(tx: &rusqlite::Transaction<'_>, task: &str, kind: &str, mut data: Value, surface: Option<&str>) -> Result<i64> {
    if let Some(owner)=data["task_node_id"].as_str() && let Some(step)=tree::current_step(tx,task,owner)? {data["task_node_id"]=json!(step.node_id);}
    if let Some(agent)=data["task_node_id"].as_str().map(|id|tree::get(tx,task,id)).transpose()?.and_then(|n|n.agent_id) {
        data["actor"]=json!("worker");
        let local:i64=tx.query_row("UPDATE agent_delegates SET event_count=event_count+1 WHERE task_id=?1 AND id=?2 RETURNING event_count-1",params![task,agent],|r|r.get(0))?;
        let data=json!({"agent_id":agent,"agent_seq":local,"turn":data["turn"],"task_node_id":data["task_node_id"],"event_type":kind,"event":data});
        return agent_service::append_event_tx(tx,task,"subagent/event",&data,surface);
    }
    agent_service::append_event_tx(tx,task,kind,&data,surface)
}

#[allow(clippy::too_many_arguments)]
fn record_result(tx: &rusqlite::Transaction<'_>, task: &str, turn: usize, step: &Value, request: &Value, call_id: &str, node: &str, result: &Value) -> Result<i64> {
    record_event(tx,task,"tool/result",json!({"actor":"agent","turn":turn,"step":step,"task_node_id":node,"flowNodeId":request,
        "message":{"role":"tool","toolCallId":call_id,"isError":result.get("error").is_some(),"content":[{"type":"text","text":result.to_string()}]},
        "meta":{"result":result}}),Some("append"))
}

/// create_task. With no task running it creates the main task, and the current
/// context becomes that task's context. Inside a step it is a saga
/// `yield call(...)`: the step hands its iterator to the new task's finish node,
/// and the call has no result until that task finishes. Returns the scope of the
/// context to continue in.
pub(super) async fn create_task(s: &Session<'_>, w: &mut Window, call: &crate::model_stream::ToolCall, request_id: &str, step: usize) -> Result<String> {
    let parent = frame_task(s, w);
    if parent == tree::ROOT {
        // A main task created with its first chain waits for that chain like any caller.
        let chained = serde_json::from_str::<Value>(&call.arguments).is_ok_and(|args| !args["tasks"].is_null());
        create_main_task(s, w, call, request_id, step, chained).await?;
        let main = frame_task(s, w);
        if !chained || main == tree::ROOT {
            return Ok(s.scope.clone());
        }
        return create_temporary_task(s, w, &main, call, request_id, step, true).await;
    }
    create_temporary_task(s, w, &parent, call, request_id, step, false).await
}

async fn create_main_task(s: &Session<'_>, w: &mut Window, call: &crate::model_stream::ToolCall, request_id: &str, step: usize, chained: bool) -> Result<()> {
    let (task, turn, scope, mut frame, call, request_id) = (s.task.to_owned(), s.turn, s.scope.clone(), w.clone(), call.clone(), request_id.to_owned());
    *w = db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        record_call(&tx, &task, turn, step, &request_id, &call, tree::ROOT)?;
        tx.execute_batch("SAVEPOINT create_task")?;
        let created = (|| -> Result<(Value, String)> {
            let args: Value = serde_json::from_str(&call.arguments).context("invalid tool arguments JSON")?;
            let mut root = tree::get(&tx, &task, tree::ROOT)?;
            ensure!(root.status == "running", "TASK_STACK_FRAME_MISMATCH: the conversation is not at its top level");
            // Steps that older builds recorded directly under the conversation end here.
            for mut legacy in tree::children(&tx, &task, tree::ROOT)?.into_iter().filter(|n| n.kind == "step" && n.status == "current") {
                legacy.status = "done".into();
                tree::emit(&tx, &task, "task/node_returned", &mut legacy, turn)?;
            }
            let ordinal = tree::children(&tx, &task, tree::ROOT)?.len();
            let (mut node, mut finish) = new_task(&root, &args, ordinal, "main", turn)?;
            tree::emit(&tx, &task, "task/node_planned", &mut node, turn)?;
            tree::emit(&tx, &task, "task/node_planned", &mut finish, turn)?;
            root.status = "suspended".into();
            tree::emit(&tx, &task, "task/node_suspended", &mut root, turn)?;
            node.status = "running".into();
            tree::emit(&tx, &task, "task/node_started", &mut node, turn)?;
            Ok((json!({"task":{"node_id":node.node_id,"goal":node.goal},
                "instruction":"Execute this task using its supplied materials. Prepare missing skill/tool contracts, then submit a complete program with one main covering the user goal and helpers for its steps. Use ordinary helper(args) for serial tasks and fork/join for authorized parallel helpers; generator/yield and async syntax are rejected. Normal returns resume the caller or finish main; the host records execution without another model completion verdict."}),
                node.node_id))
        })();
        if created.is_err() {
            tx.execute_batch("ROLLBACK TO create_task")?;
        }
        tx.execute_batch("RELEASE create_task")?;
        let (result, owner, marker) = match created {
            Ok((result, node)) => (result, node, true),
            Err(error) => (json!({"error":format!("{error:#}")}), tree::ROOT.to_owned(), false),
        };
        // With a first chain the call is answered when that chain returns.
        if !(marker && chained) {
            let seq = record_result(&tx, &task, turn, &json!(step), &json!(request_id), &call.call_id, &owner, &result)?;
            frame.messages.push(sourced(json!({"role":"tool","tool_call_id":call.call_id,"content":with_seq(&result,seq).to_string()}), &task, seq));
        }
        if marker {
            frame.task_node_id = Some(owner);
            frame.current_step_node_id = None;
        }
        write_window(&tx, &task, &scope, &frame)?;
        tx.commit()?;
        Ok(frame)
    })
    .await?;
    Ok(())
}

/// Child tasks as a chain: every node of the call is planned at once and runs
/// in order; a single child is a chain of one. `recorded`: the call event was
/// already written when the same call created the main task.
async fn create_temporary_task(s: &Session<'_>, w: &mut Window, parent_id: &str, call: &crate::model_stream::ToolCall, request_id: &str, step: usize, recorded: bool) -> Result<String> {
    let prepared = async {
        let nodes = (|| {
            let args: Value = serde_json::from_str(&call.arguments).context("invalid tool arguments JSON")?;
            let conn = agent_service::open_db(s.state.workspace.root())?;
            let parent = tree::get(&conn, s.task, parent_id)?;
            ensure!(tree::is_task(&parent) && parent.status == "running" && parent.agent_id.as_deref() == s.delegate_id(), "TASK_STACK_FRAME_MISMATCH");
            let depth = tree::path(&conn, s.task, &parent.node_id)?.iter().filter(|n| tree::is_temp(n)).count();
            ensure!(depth < tree::MAX_TEMP_TASKS, "TASK_DEPTH_LIMIT: temporary tasks nest at most {} deep; finish this one first", tree::MAX_TEMP_TASKS);
            let entries = chain_entries(&args, recorded)?;
            // The chain hangs under the step that created it.
            let owner = tree::current_step(&conn, s.task, &parent.node_id)?.unwrap_or_else(|| parent.clone());
            let ordinal = tree::children(&conn, s.task, &owner.node_id)?.len();
            let mut nodes = entries.iter().enumerate()
                .map(|(k, entry)| new_task(&owner, entry, ordinal + k, "temp", s.turn))
                .collect::<Result<Vec<_>>>()?;
            // Every declared material exists before any task of the chain starts.
            let chained = !args["tasks"].is_null();
            for (k, (node, _)) in nodes.iter().enumerate() {
                for id in node.inputs["assets"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    crate::context_assets::reference(s.state.workspace.root(), s.task, id, None).with_context(|| {
                        if chained { format!("tasks[{k}].asset_ids: {id}") } else { format!("asset_ids: {id}") }
                    })?;
                }
            }
            let chain = nodes.iter().map(|(node, _)| json!(node.node_id)).collect::<Vec<_>>();
            for (index, (_, finish)) in nodes.iter_mut().enumerate() {
                finish.metadata = json!({"iterator":{"task":parent.node_id,"scope":s.scope,"call_id":call.call_id,"step":step,"flowNodeId":request_id,"chain":chain,"index":index}});
            }
            Ok::<_, anyhow::Error>(nodes)
        })()?;
        let first = &nodes[0].0;
        let mut inputs = declared_inputs(s, w, first).await?;
        inherit_scope_values(s, w, &mut inputs).await?;
        let frame = temporary_frame(s, w, first, inputs, chain_facts(&nodes.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(), 0, Vec::new()))?;
        Ok::<_, anyhow::Error>((nodes, frame))
    }
    .await;
    let (task, turn, scope, mut parent_frame, call, request_id, parent_id) = (s.task.to_owned(), s.turn, s.scope.clone(), w.clone(), call.clone(), request_id.to_owned(), parent_id.to_owned());
    let (scope, frame) = db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !recorded {
            record_call(&tx, &task, turn, step, &request_id, &call, &parent_id)?;
        }
        let switched = match prepared {
            Ok((mut nodes, frame)) => {
                for (node, finish) in nodes.iter_mut() {
                    tree::emit(&tx, &task, "task/node_planned", node, turn)?;
                    tree::emit(&tx, &task, "task/node_planned", finish, turn)?;
                }
                let mut parent = tree::get(&tx, &task, &parent_id)?;
                parent.status = "suspended".into();
                tree::emit(&tx, &task, "task/node_suspended", &mut parent, turn)?;
                let node = &mut nodes[0].0;
                node.status = "running".into();
                tree::emit(&tx, &task, "task/node_started", node, turn)?;
                // The caller's frame waits with its call unanswered.
                write_window(&tx, &task, &scope, &parent_frame)?;
                let child_scope = tree::frame_scope(node);
                write_window(&tx, &task, &child_scope, &frame)?;
                (child_scope, frame)
            }
            Err(error) => {
                // Not started: the call returns its error right away.
                let result = json!({"error":format!("{error:#}")});
                let seq = record_result(&tx, &task, turn, &json!(step), &json!(request_id), &call.call_id, &parent_id, &result)?;
                parent_frame.messages.push(sourced(json!({"role":"tool","tool_call_id":call.call_id,"content":with_seq(&result,seq).to_string()}), &task, seq));
                write_window(&tx, &task, &scope, &parent_frame)?;
                (scope, parent_frame)
            }
        };
        tx.commit()?;
        Ok(switched)
    })
    .await?;
    *w = frame;
    Ok(scope)
}

/// A temporary task's own context, like a function's scope: its goal and
/// briefing, the materials declared for it (its parameters, kept until it
/// returns), the skills and pinned materials it inherits and the user's words
/// in this main task. Parent values are readable; conversational working records stay behind. Its
/// chronological tool activity and saved-result references are assembled by
/// the host; ancestor goals also arrive with every request as the task path.
fn temporary_frame(s: &Session<'_>, parent: &Window, node: &Node, inputs: Vec<Value>, chain: Value) -> Result<Window> {
    let mut messages = vec![json!({"role":"system","content":s.prompt()})];
    inherit_material(parent, &mut messages);
    for message in parent.messages.iter().filter(|m| is_user_original(m)) {
        if !messages.contains(message) {
            messages.push(message.clone());
        }
    }
    messages.extend(inputs);
    messages.push(scope_environment(parent, &frame_task(s, parent)));
    if chain["earlier"].as_array().is_some_and(|items| !items.is_empty()) {
        messages.push(json!({"role":"user","name":"chain_returns","content":json!({"host_status":true,"not_a_user_request":true,
            "returns":chain["earlier"],"instruction":"Returns of the earlier tasks in this chain, in order. Read a handle only when you need its content."}).to_string(),
            "_frame_input":true}));
    }
    messages.push(json!({"role":"user","name":"caller_activity","content":caller_activity(s,parent)?.to_string(),"_frame_input":true}));
    let mut started = json!({"host_status":true,"not_a_user_request":true,
        "task":{"node_id":node.node_id,"goal":node.goal,"reason":node.spawned_from,"briefing":node.briefing,"asset_ids":node.inputs["assets"]},
        "instruction":"Execute this child task using the inherited parent environment and the declared inputs. Shared and declared values arrive as complete bodies when they fit, otherwise as references with lengths. Read absent bodies in explicit read_asset pages as needed; unread text has not been delivered. Reuse prepared capabilities and values instead of initializing them again. Return only the result needed by the caller, with saved asset handles; this child's local working context is then released."});
    if !chain.is_null() {
        started["chain"] = json!({"position":chain["position"],"total":chain["total"],"tasks":chain["tasks"],
            "note":"Tasks of one create_task call run in this order, each in its own scope. Do only this task's part; the next one starts when this one returns."});
    }
    messages.push(json!({"role":"user","name":"task_started","content":started.to_string(),"_frame_input":true}));
    let program_task = node
        .inputs
        .get("program_task")
        .filter(|v| v.is_object())
        .cloned();
    if let Some(task) = &program_task {
        let mut input=task.clone();
        input.as_object_mut().unwrap().remove("body");
        messages.push(json!({"role":"user","name":"program_task_input","content":input.to_string(),"_frame_input":true}));
    }
    let program=program_task.as_ref().filter(|t|t["body"].is_object())
        .map(|t|super::ts_program::fork_runtime(&json!({"program":t["body"]}))).transpose()?.flatten();
    let mut child = Window {
        program,
        program_task,
        messages,
        task_node_id: Some(node.node_id.clone()),
        loaded_tool_groups: parent.loaded_tool_groups.clone(),
        active_skills: parent.active_skills.clone(),
        active_skill_scopes: parent.active_skill_scopes.clone(),
        skill_turn: parent.skill_turn,
        token_calibration: parent.token_calibration.clone(),
        ..Default::default()
    };
    let mut session = s.clone();
    session.scope = tree::frame_scope(node);
    fit_input_bodies(&session, &mut child)?;
    Ok(child)
}

#[cfg(test)]
/// create_step: the current step is done and the next one starts in the same
/// context. The step's result is recorded on its node and carried into the next.
pub(super) async fn create_step(s: &Session<'_>, w: &mut Window, call: &crate::model_stream::ToolCall, request_id: &str, step: usize) -> Result<()> {
    let owner = frame_task(s, w);
    let (task, turn, scope, mut frame, call, request_id) = (s.task.to_owned(), s.turn, s.scope.clone(), w.clone(), call.clone(), request_id.to_owned());
    *w = db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        record_call(&tx, &task, turn, step, &request_id, &call, &owner)?;
        tx.execute_batch("SAVEPOINT create_step")?;
        let created = (|| -> Result<(Value, String)> {
            let args: Value = serde_json::from_str(&call.arguments).context("invalid tool arguments JSON")?;
            ensure!(owner != tree::ROOT, "NO_TASK: steps belong to a task; call create_task alone first");
            let result = required_text(&args, "result")?;
            let next = required_text(&args, "next")?;
            let parent = tree::get(&tx, &task, &owner)?;
            ensure!(tree::is_task(&parent) && parent.status == "running", "TASK_STACK_FRAME_MISMATCH");
            let finished = match tree::current_step(&tx, &task, &owner)? {
                Some(mut previous) => {
                    previous.status = "done".into();
                    previous.result = Some(json!({"result":result}));
                    tree::emit(&tx, &task, "task/node_returned", &mut previous, turn)?;
                    json!({"node_id":previous.node_id,"title":previous.title,"result":result})
                }
                None => json!({"result":result}),
            };
            let ordinal = tree::children(&tx, &task, &owner)?.iter().filter(|n| !matches!(n.kind.as_str(), "finish" | "end")).count();
            let mut created = tree::planned(&parent, &json!({"title":task_title(&next),"goal":next}), ordinal, "step")?;
            created.status = "current".into();
            tree::emit(&tx, &task, "task/node_started", &mut created, turn)?;
            Ok((json!({"previous_step":finished,"current_step":{"node_id":created.node_id,"title":created.title},
                "instruction":"Continue with the current step in this context."}), created.node_id))
        })();
        if created.is_err() {
            tx.execute_batch("ROLLBACK TO create_step")?;
        }
        tx.execute_batch("RELEASE create_step")?;
        let (result, marker) = match created {
            Ok((result, id)) => (result, Some(id)),
            Err(error) => (json!({"error":format!("{error:#}")}), None),
        };
        let seq = record_result(&tx, &task, turn, &json!(step), &json!(request_id), &call.call_id, &owner, &result)?;
        let mut message = sourced(json!({"role":"tool","tool_call_id":call.call_id,"content":with_seq(&result,seq).to_string()}), &task, seq);
        if let Some(id) = marker {
            message["_step_start"] = json!(id);
        }
        frame.messages.push(message);
        write_window(&tx, &task, &scope, &frame)?;
        tx.commit()?;
        Ok(frame)
    })
    .await?;
    Ok(())
}

/// When the finishing task has a planned successor in its chain: that node, the
/// caller's waiting frame with this task's user messages added, and the
/// successor's own frame (its declared materials plus the earlier returns).
async fn next_in_chain(s: &Session<'_>, w: &Window, id: &str, text: &str, saved: &Value) -> Result<Option<(Node, Window, Window)>> {
    let (chain, index, mut caller) = {
        let conn = agent_service::open_db(s.state.workspace.root())?;
        let Some(finish) = tree::finish_node(&conn, s.task, id)? else { return Ok(None);
        };
        let iterator = &finish.metadata["iterator"];
        let ids = iterator["chain"].as_array().cloned().unwrap_or_default();
        let index = iterator["index"].as_u64().unwrap_or(0) as usize;
        if index + 1 >= ids.len() {
            return Ok(None);
        }
        let chain = ids.iter().filter_map(Value::as_str).map(|cid| tree::get(&conn, s.task, cid)).collect::<Result<Vec<_>>>()?;
        let scope = iterator["scope"].as_str().context("iterator scope missing")?;
        let raw: String = conn
            .query_row("SELECT state FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2", params![s.task, scope], |r| r.get(0))
            .context("waiting task frame missing")?;
        (chain, index, serde_json::from_str::<Window>(&raw)?)
    };
    // What the user said meanwhile belongs to the whole main task.
    for original in w.messages.iter().filter(|m| is_user_original(m)) {
        if !caller.messages.iter().any(|m| {
            m["source"]["seq"] == original["source"]["seq"] && m["content"] == original["content"]
        }) {
            caller.messages.push(original.clone());
        }
    }
    let mut earlier = chain[..index].iter().map(child_return).collect::<Vec<_>>();
    earlier.push(json!({"task":chain[index].title,"node_id":chain[index].node_id,"result":text,"assets":saved}));
    let next = chain[index + 1].clone();
    let mut inputs = declared_inputs(s, &caller, &next).await?;
    inherit_scope_values(s, &caller, &mut inputs).await?;
    let frame = temporary_frame(s, &caller, &next, inputs, chain_facts(&chain, index + 1, earlier))?;
    Ok(Some((next, caller, frame)))
}

/// What a returning task saved, as handles: the latest result per invocation
/// in its scope, without the materials it was given or inherited.
fn returned_assets(s: &Session<'_>, w: &Window, node: &str) -> Value {
    let given = w.messages.iter()
        .filter(|m| m["_frame_input"] == true || m["_pinned"] == true)
        .filter_map(|m| m[crate::context_assets::REFERENCE]["asset_id"].as_str())
        .collect::<BTreeSet<_>>();
    let Ok(index) = crate::context_assets::frame_index(s.state.workspace.root(), s.task, node, &w.messages) else {
        return json!([]);
    };
    json!(index["assets"].as_array().into_iter().flatten()
        .filter(|a| a["asset_id"].as_str().is_some_and(|id| !given.contains(id)))
        .map(|a| {let mut a = a.clone(); a.as_object_mut().unwrap().remove("in_context"); a})
        .collect::<Vec<_>>())
}

/// A no-tool response reaches the finish node of
/// the task in view, and that reply is the task's summary. A temporary task's
/// finish runs the iterator it holds: the interrupted create_task call returns
/// the summary and the waiting task continues. A main task's summary is the
/// answer to the user. Returns the context to continue in; None ends the turn.
pub(super) async fn finish(s: &Session<'_>, w: &Window, message: &Value) -> Result<Option<(String, Window)>> {
    if s.mode != Mode::Main {
        return Ok(None);
    }
    let id = frame_task(s, w);
    // A saved proposal awaiting the user pauses the task instead of finishing it.
    if id == tree::ROOT || (s.is_root() && approval_boundary(s)?.is_some()) {
        return Ok(None);
    }
    let agent=s.delegate_id().map(str::to_owned);
    let saved = returned_assets(s, w, &id);
    let text = message["content"].as_str().unwrap_or("").to_owned();
    // A successor that cannot start ends the chain; the caller gets the facts.
    let next = match next_in_chain(s, w, &id, &text, &saved).await {
        Ok(next) => Ok(next),
        Err(error) => {
            ensure!(!s.cancel.is_cancelled(), "cancelled");
            Err(format!("{error:#}"))
        }
    };
    let (task, turn, old_scope, frame) = (s.task.to_owned(), s.turn, s.scope.clone(), w.clone());
    db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut node = tree::get(&tx, &task, &id)?;
        if !tree::is_task(&node) || node.status != "running" {
            return Ok(None);
        }
        ensure!(node.agent_id==agent,"TASK_EXECUTION_FRAME_MISMATCH");
        let summary = json!({"result":text,"assets":saved});
        if let Some(mut step) = tree::current_step(&tx, &task, &id)? {
            step.status = "done".into();
            tree::emit(&tx, &task, "task/node_returned", &mut step, turn)?;
        }
        let finish = tree::finish_node(&tx, &task, &id)?;
        let iterator = finish.as_ref().map(|f| f.metadata["iterator"].clone()).unwrap_or(Value::Null);
        node.status = "done".into();
        node.result = Some(summary.clone());
        let returned = tree::emit(&tx, &task, "task/node_returned", &mut node, turn)?;
        if let Some(mut finish) = finish {
            finish.status = "done".into();
            finish.result = Some(summary);
            tree::emit(&tx, &task, "task/node_returned", &mut finish, turn)?;
        }
        if iterator.is_null() && node.agent_id.is_some() {
            write_window(&tx, &task, &old_scope, &frame)?;
            tx.commit()?;
            return Ok(None);
        }
        if !tree::is_temp(&node) || iterator.is_null() {
            // A main task: the conversation is back at its top level.
            let mut root = tree::get(&tx, &task, tree::ROOT)?;
            if root.status == "suspended" {
                root.status = "running".into();
                tree::emit(&tx, &task, "task/node_resumed", &mut root, turn)?;
            }
            let mut frame = frame;
            frame.task_node_id = Some(tree::ROOT.into());
            frame.current_step_node_id = None;
            write_window(&tx, &task, &old_scope, &frame)?;
            tx.commit()?;
            return Ok(None);
        }
        let mut parent = tree::get(&tx, &task, iterator["task"].as_str().context("finish node holds no iterator")?)?;
        ensure!(parent.agent_id==agent,"TASK_CALLER_AGENT_MISMATCH");
        let scope = iterator["scope"].as_str().context("iterator scope missing")?.to_owned();
        if let Ok(Some((mut next, caller, next_frame))) = next {
            // The chain continues: the caller keeps waiting, the next task starts.
            next.status = "running".into();
            tree::emit(&tx, &task, "task/node_started", &mut next, turn)?;
            write_window(&tx, &task, &scope, &caller)?;
            let next_scope = tree::frame_scope(&next);
            write_window(&tx, &task, &next_scope, &next_frame)?;
            tx.execute("DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2", params![task, old_scope])?;
            tx.commit()?;
            return Ok(Some((next_scope, next_frame)));
        }
        let call_id = iterator["call_id"].as_str().context("iterator call missing")?.to_owned();
        let raw: String = tx
            .query_row("SELECT state FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2", params![task, scope], |r| r.get(0))
            .context("waiting task frame missing")?;
        let mut resumed: Window = serde_json::from_str(&raw)?;
        ensure!(resumed.task_node_id.as_deref()==Some(parent.node_id.as_str()),"TASK_CALLER_FRAME_MISMATCH");
        let chain = iterator["chain"].as_array().cloned().unwrap_or_default();
        let answer = if chain.len() > 1 {
            // Every task of the call, in order: its summary and handles. Tasks
            // that never started because the chain stopped are listed as such.
            let mut results = Vec::new();
            let mut not_started = Vec::new();
            for cid in chain.iter().filter_map(Value::as_str) {
                if cid == node.node_id {
                    results.push(child_return(&node));
                    continue;
                }
                let mut other = tree::get(&tx, &task, cid)?;
                if other.status == "done" {
                    results.push(child_return(&other));
                    continue;
                }
                other.status = "cancelled".into();
                tree::emit(&tx, &task, "task/node_returned", &mut other, turn)?;
                if let Some(mut finish) = tree::finish_node(&tx, &task, cid)? {
                    finish.status = "cancelled".into();
                    tree::emit(&tx, &task, "task/node_returned", &mut finish, turn)?;
                }
                not_started.push(json!({"task":other.title,"node_id":other.node_id}));
            }
            let mut answer = json!({"results":results});
            if let Err(error) = &next {
                answer["stopped"] = json!({"reason":"the next task of this chain could not start","error":error,"not_started":not_started});
            }
            answer
        } else {
            json!({"task":node.title,"node_id":node.node_id,"return_event_seq":returned,"result":text,"assets":saved})
        };
        let seq = record_result(&tx, &task, turn, &iterator["step"], &iterator["flowNodeId"], &call_id, &parent.node_id, &answer)?;
        let mut delivery = sourced(json!({"role":"tool","tool_call_id":call_id,"content":with_seq(&answer,seq).to_string()}), &task, seq);
        // Handles, not a request for bodies: the caller reads what it needs.
        delivery["_child_return"] = json!(true);
        resumed.messages.push(delivery);
        // What the user said meanwhile belongs to the whole main task.
        for original in frame.messages.iter().filter(|m| is_user_original(m)) {
            if !resumed.messages.iter().any(|m| m["source"]["seq"] == original["source"]["seq"] && m["content"] == original["content"]) {
                resumed.messages.push(original.clone());
            }
        }
        parent.status = "running".into();
        tree::emit(&tx, &task, "task/node_resumed", &mut parent, turn)?;
        write_window(&tx, &task, &scope, &resumed)?;
        tx.execute("DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2", params![task, old_scope])?;
        tx.commit()?;
        Ok(Some((scope, resumed)))
    })
    .await
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
}

/// Stored content may be a plain string or the JSON text of content parts.
fn plain_text(raw: &str) -> String {
    if raw.trim_start().starts_with('[')
        && let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(raw)
        && parts.iter().all(|part| part["type"].is_string())
    {
        return parts.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n");
    }
    raw.to_owned()
}

/// One memory per completed main task, including tasks resumed across turns.
/// Turns without a task remain question/answer memories. Details stay in history.
pub(super) fn memory_index(conn: &rusqlite::Connection, task: &str, turn: usize) -> Result<Option<Value>> {
    let tasks = tree::children(conn, task, tree::ROOT)?
        .into_iter()
        .filter(|node| {
            node.kind == "task" && !tree::is_temp(node)
            && node.metadata["turn"].as_u64().is_some_and(|started| started < turn as u64)
        })
        .collect::<Vec<_>>();
    let task_turns = tasks.iter().filter_map(|node| node.metadata["turn"].as_i64()).collect::<BTreeSet<_>>();
    let task_ranges = tasks.iter().filter_map(|node| {
        node.first_seq.map(|start| (start, node.returned_seq.unwrap_or(i64::MAX)))
    }).collect::<BTreeMap<_, _>>();
    let rows = conn
        .prepare("SELECT seq,kind,json_extract(data,'$.turn'),json_extract(data,'$.message.content'),json_type(data,'$.message.tool_calls') FROM agent_task_events WHERE task_id=?1 AND kind IN ('user/message','assistant/message') AND json_extract(data,'$.turn')<?2 ORDER BY seq")?
        .query_map(params![task, turn as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut turns = BTreeMap::<i64, (i64, Value)>::new();
    let mut task_owned_turns = task_turns;
    for (seq, kind, number, content, calls) in rows {
        let (Some(number), Some(content)) = (number, content) else { continue;
        };
        if task_ranges.range(..=seq).next_back().is_some_and(|(_, end)| seq <= *end) {
            task_owned_turns.insert(number);
        }
        let text = plain_text(&content);
        if text.trim().is_empty() {
            continue;
        }
        let (last_seq, entry) = turns.entry(number).or_insert_with(|| (seq, json!({"kind":"qa","turn":number})));
        if kind == "user/message" {
            if entry.get("request").is_none() {
                entry["request"] = json!(clip(text.trim(), 160));
            }
        } else if calls.as_deref() != Some("array") {
            entry["answer"] = json!(text.trim().chars().take(200).collect::<String>());
            *last_seq = seq;
        }
    }
    let mut entries = Vec::new();
    for node in tasks.iter().filter(|node| node.status == "done") {
        let (Some(number), Some(seq), Some(summary)) = (
            node.metadata["turn"].as_i64(), node.returned_seq,
            node.result.as_ref().and_then(|result| result["result"].as_str()),
        ) else { continue;
        };
        let mut entry = json!({"kind":"task","turn":number,
            "task":{"node_id":node.node_id,"goal":clip(&node.goal,160),"status":node.status},
            "answer":summary.trim().chars().take(200).collect::<String>()});
        if let Some((_, original)) = turns.get(&number) {
            entry["request"] = original["request"].clone();
        }
        entries.push((seq, entry));
    }
    entries.extend(turns.into_iter().filter(|(number, (_, entry))| {
                !task_owned_turns.contains(number)
        && entry.get("request").is_some() && entry.get("answer").is_some()
            }).map(|(_, entry)| entry));
    entries.sort_by_key(|(seq, _)| *seq);
    if entries.is_empty() {
        return Ok(None);
    }
    let omitted = entries.len().saturating_sub(30);
    let entries = entries.into_iter().skip(omitted).map(|(_, entry)| entry).collect::<Vec<_>>();
    Ok(Some(json!({"host_status":true,"not_a_user_request":true,"entries":entries,"entries_omitted":omitted,
        "instruction":"Index of completed main tasks and task-free questions, oldest completion first. Each task has one entry with its goal, final summary and node_id even if it spanned multiple turns. Temporary tasks are included in their main task, not separate entries. Working context is not loaded here. Read details with read_session_history using the task node_id and include_descendants=true, or turn for a question/answer entry."})))
}

/// Each user request at the conversation level starts a clean main context:
/// the memory index of earlier turns plus the skills and tool groups still
/// loaded. Earlier working records stay in history.
pub(super) async fn fresh_main_window(s: &Session<'_>, previous: Option<&Window>, memory: Option<Value>) -> Result<Window> {
    let mut window = Window { task_node_id: Some(tree::ROOT.into()), ..Default::default() };
    window.messages.push(json!({"role":"system","content":Mode::Main.prompt()}));
    if let Some(index) = memory {
        window.messages.push(json!({"role":"user","name":"conversation_memory","content":index.to_string(),"_frame_input":true}));
    }
    let Some(previous) = previous else { return Ok(window);
    };
    window.loaded_tool_groups = previous.loaded_tool_groups.clone();
    window.active_skills = previous.active_skills.clone();
    window.active_skill_scopes = previous.active_skill_scopes.clone();
    window.token_calibration = previous.token_calibration.clone();
    for id in crate::context_assets::active_instruction_ids(&previous.messages, &previous.active_skills) {
        let loaded = previous.messages.iter().rev().find(|m| {
            m[crate::context_assets::REFERENCE]["asset_id"] == id
                && !matches!(m["_asset_state"].as_str(), Some("reference_only" | "not_delivered"))
        });
        let mut message = if let Some(message) = loaded {
            let mut message = message.clone();
            message["role"] = json!("user");
            message["name"] = json!("saved_asset");
            message.as_object_mut().unwrap().remove("tool_call_id");
            message
        } else {
            match load_saved_asset(s, &json!({"asset_id":id}), None).await {
                Ok(result) => crate::context_assets::tagged(json!({"role":"user","name":"saved_asset","content":result.to_string()}), &result["asset_reference"]),
                Err(_) => continue,
            }
        };
        message["_pinned"] = json!(true);
        window.messages.push(message);
    }
    Ok(window)
}

pub(super) async fn delegate_context(
    s: &Session<'_>,
    w: &Window,
    args: &Value,
) -> Result<(Value, Value)> {
    let mut conn = agent_service::open_db(s.state.workspace.root())?;
    let parent_id = frame_task(s,w);
    let Some(parent) = (if parent_id==tree::ROOT {None} else {Some(tree::get(&conn,s.task,&parent_id)?)}) else {
        return Ok((
            args.clone(),
            prepare_delegate_context(s, w, &tools(s, w)).await?,
        ));
    };
    let node = {
        let mut inputs = args["inputs"]
            .as_object()
            .cloned()
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        if args["asset_ids"].is_array() {
            inputs["assets"] = args["asset_ids"].clone();
        }
        let node = tree::planned(
            &parent,
            &json!({"title":args["name"],"goal":args["task"],"done_when":args.get("done_when").unwrap_or(&args["task"]),"executor":"delegate","inputs":inputs}),
            tree::nodes(&conn, s.task)?.len(),
            "task",
        )?;
        // emit reads the next seq before writing; a deferred transaction would
        // fail immediately with SQLITE_BUSY instead of waiting for the lock.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut recorded = node.clone();
        recorded.briefing=args["context"].as_str().unwrap_or("").to_owned();
        recorded.metadata=json!({"role":"delegate","turn":s.turn});
        tree::emit(&tx, s.task, "task/node_planned", &mut recorded, s.turn)?;
        let mut finish=tree::planned(&recorded,&json!({"title":"Finish","goal":"End the delegated task and return its summary"}),tree::FINISH_ORDINAL,"finish")?;
        tree::emit(&tx,s.task,"task/node_planned",&mut finish,s.turn)?;
        tx.commit()?;
        recorded
    };
    drop(conn);
    let frame = child_material(s, w, &node).await?;
    let mut shared = serde_json::to_value(frame)?;
    shared["task_node"] = serde_json::to_value(&node)?;
    shared["caller_task_node_id"] = json!(parent.node_id);
    let mut assignment = args.clone();
    assignment["name"] = json!(node.title);
    assignment["task"] = json!(node.goal);
    assignment["task_node_id"] = json!(node.node_id);
    assignment["context"] = json!(node.briefing);
    Ok((assignment, shared))
}

#[cfg(test)]
pub(super) async fn delegate_return(
    s: &Session<'_>,
    w: &mut Window,
    call: &crate::model_stream::ToolCall,
    request_id: &str,
    step: usize,
) -> Result<bool> {
    let (task, turn, agent, scope, mut next, call, request_id) = (
        s.task.to_owned(),
        s.turn,
        s.delegate_id().context("not a delegate")?.to_owned(),
        s.scope.clone(),
        w.clone(),
        call.clone(),
        request_id.to_owned(),
    );
    let (window,returned)=db(s.state.workspace.root(),move|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let event=|kind:&str,data:Value|->Result<i64> {
            let local:i64=tx.query_row("UPDATE agent_delegates SET event_count=event_count+1 WHERE task_id=?1 AND id=?2 RETURNING event_count-1",params![task,agent],|r|r.get(0))?;
            agent_service::append_event_tx(&tx,&task,"subagent/event",&json!({"agent_id":agent,"agent_seq":local,"turn":turn,"event_type":kind,"event":data}),Some("append"))
        };
        event("tool/call",json!({"actor":"worker","turn":turn,"step":step,"flowNodeId":request_id,
            "callId":call.call_id,"name":call.name,"arguments":call.arguments}))?;
        let output=(||->Result<Value> {
            let args:Value=serde_json::from_str(&call.arguments)?;
            let id=next.task_node_id.as_deref().context("delegate has no task node")?;
            let mut node=tree::get(&tx,&task,id)?;
            ensure!(node.agent_id.as_deref()==Some(&agent) && node.status=="running","delegate task is not running");
            tree::validate_return(&tx,&task,&node,&args)?;
            node.status=args["status"].as_str().unwrap().into();node.result=Some(args.clone());
            let seq=tree::emit(&tx,&task,"task/node_returned",&mut node,turn)?;
            Ok(json!({"returned":id,"status":args["status"],"return_event_seq":seq}))
        })();
        let returned=output.is_ok();
        let result=output.unwrap_or_else(|error|json!({"error":format!("{error:#}")}));
        let seq=event("tool/result",json!({"actor":"worker","turn":turn,"step":step,"flowNodeId":request_id,
            "message":{"role":"tool","toolCallId":call.call_id,"isError":!returned,"content":[{"type":"text","text":result.to_string()}]},"meta":{"result":result}}))?;
        next.messages.push(sourced(json!({"role":"tool","tool_call_id":call.call_id,"content":json!({"event_seq":seq,"result":result}).to_string()}),&task,seq));
        write_window(&tx,&task,&scope,&next)?;
        tx.commit()?;
        Ok((next,returned))
    }).await?;
    *w = window;
    Ok(returned)
}

/// The materials declared for a task, loaded once each, in declaration order.
async fn declared_inputs(s: &Session<'_>, parent: &Window, node: &Node) -> Result<Vec<Value>> {
    let mut inputs = Vec::new();
    let mut seen = BTreeSet::new();
    for id in node.inputs["assets"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if seen.insert(id) {
            inputs.push(input_message(s, parent, id).await.with_context(|| format!("asset_ids: {id}"))?);
        }
    }
    Ok(inputs)
}

/// Parent return values behave like lexical variables. Resolve only this
/// frame's saved values (including values it inherited), never sibling working
/// histories. Snapshots survive folding and restart; fit_input_bodies keeps
/// oversized values as references. Explicit arguments take precedence.
async fn inherit_scope_values(
    s: &Session<'_>,
    parent: &Window,
    inputs: &mut Vec<Value>,
) -> Result<()> {
    let owner = frame_task(s, parent);
    // Most material reads already have snapshots. At the call boundary save
    // other successful returns (workspace info, discovery, capability loads)
    // as variables too. Do not add snapshot events to ordinary tool execution.
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let node = tree::get(&conn, s.task, &owner)?;
    let activity = activity_for(s, &[node], &tools(s, parent), None)?;
    for entry in activity["activity"].as_array().into_iter().flatten() {
        if entry["tool"] == "create_task"
            || entry["status"] == "error"
            || entry["status"] == "pending"
            || entry["assets"]
                .as_array()
                .is_some_and(|assets| !assets.is_empty())
        {
            continue;
        }
        let event = |seq: Option<i64>| -> Result<Value> {
            let seq = seq.context("scope value has no source event")?;
            crate::session_history::load_events(
                &conn,
                s.task,
                &crate::session_history::EventQuery {
                    exact: Some(seq),
                    ..Default::default()
                },
            )?
            .pop()
            .context("scope value event missing")?
            .data()
        };
        let mut call = event(entry["event_seq"].as_i64())?;
        let seq = entry["result_reference"]["arguments"]["event_seq"].as_i64();
        let mut returned = event(seq)?;
        if call["event"].is_object() {
            call = call["event"].clone();
        }
        if returned["event"].is_object() {
            returned = returned["event"].clone();
        }
        if returned["message"]["isError"] == true {
            continue;
        }
        let args = match &call["arguments"] {
            Value::String(text) => serde_json::from_str(text)?,
            value => value.clone(),
        };
        crate::context_assets::register(
            s.state.workspace.root(),
            s.task,
            seq.unwrap(),
            entry["tool"].as_str().context("scope value tool missing")?,
            &args,
            &returned["meta"]["result"],
        )?;
    }
    let index = crate::context_assets::frame_index(
        s.state.workspace.root(),
        s.task,
        &owner,
        &parent.messages,
    )?;
    let mut seen = inputs
        .iter()
        .chain(parent.messages.iter().filter(|m| m["_pinned"] == true))
        .filter_map(|m| {
            m[crate::context_assets::REFERENCE]["asset_id"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    for value in index["assets"].as_array().into_iter().flatten() {
        let Some(id) = value["asset_id"].as_str() else {
            continue;
        };
        if !seen.insert(id.to_owned()) || value["source_tool"] == "read_skill" {
            continue;
        }
        let mut message = input_message(s, parent, id).await?;
        let delivery: Value = serde_json::from_str(message["content"].as_str().unwrap_or("{}"))?;
        let result = delivery.get("original_result").unwrap_or(&delivery);
        if !result["error"].is_null() || result["isError"] == true {
            continue;
        }
        message["name"] = json!("scope_value");
        message["_scope_owner"] = parent
            .messages
            .iter()
            .find(|m| {
                m[crate::context_assets::REFERENCE]["asset_id"] == id
                    && m["_scope_owner"].is_string()
            })
            .map(|m| m["_scope_owner"].clone())
            .unwrap_or_else(|| json!(owner));
        inputs.push(message);
    }
    Ok(())
}

/// A child's place in its chain: position, the planned goals in order and the
/// returns of the earlier children. Null for a single child.
fn chain_facts(chain: &[Node], index: usize, earlier: Vec<Value>) -> Value {
    if chain.len() < 2 {
        return Value::Null;
    }
    json!({"position":index+1,"total":chain.len(),
        "tasks":chain.iter().map(|n| json!({"node_id":n.node_id,"goal":n.goal})).collect::<Vec<_>>(),
        "earlier":earlier})
}

/// What a finished child hands back: its summary and handles to what it saved.
fn child_return(node: &Node) -> Value {
    let result = node.result.clone().unwrap_or(Value::Null);
    json!({"task":node.title,"node_id":node.node_id,"return_event_seq":node.returned_seq,"result":result["result"],"assets":result.get("assets").cloned().unwrap_or(json!([]))})
}

/// A declared input for the receiving task: the caller's complete copy when it
/// holds one, otherwise the saved snapshot. It stays until that task returns.
async fn input_message(s: &Session<'_>, parent: &Window, id: &str) -> Result<Value> {
    let loaded = parent.messages.iter().rev().find(|m| crate::context_assets::complete_body(m) == Some(id));
    let mut message = if let Some(message) = loaded {
        let mut message = message.clone();
        message["role"] = json!("user");
        message["name"] = json!("saved_asset");
        let object = message.as_object_mut().unwrap();
        object.remove("tool_call_id");
        object.remove("_step_restored_for");
        message
    } else {
        let result = load_saved_asset(s, &json!({"asset_id":id}), None).await?;
        crate::context_assets::tagged(
            json!({"role":"user","name":"saved_asset","content":result.to_string()}),
            &result["asset_reference"],
        )
    };
    message["_frame_input"] = json!(true);
    Ok(message)
}

/// Fit the entire receiving request, not each asset in isolation. References
/// reserve space for every input first; bodies are then admitted in declaration
/// order against the existing hard input boundary. This chooses size, not task
/// relevance. Inherited pinned rules and user instructions are never changed.
fn fit_input_bodies(s: &Session<'_>, child: &mut Window) -> Result<()> {
    let definitions = tools(s, child);
    let mut bodies = Vec::new();
    for (index, message) in child.messages.iter_mut().enumerate() {
        if message["_frame_input"] != true || message["_pinned"] == true
            || crate::context_assets::complete_body(message).is_none() {
            continue;
        }
        let body = message.clone();
        let delivery: Value = serde_json::from_str(message["content"].as_str().unwrap_or(""))?;
        let total_chars = delivery["total_chars"].as_u64().unwrap_or_else(|| {
            delivery.get("original_result").unwrap_or(&delivery).to_string().chars().count() as u64
        });
        crate::context_assets::reference_only(message);
        let mut receipt: Value = serde_json::from_str(message["content"].as_str().unwrap())?;
        receipt["total_chars"] = json!(total_chars);
        receipt["delivered"] = json!(false);
        receipt["next_read"] = json!({"asset_id":message[crate::context_assets::REFERENCE]["asset_id"],"char_offset":0,"max_chars":PAGE_CHARS});
        receipt["instruction"] = json!("The complete body did not fit alongside the other task inputs. Read explicit pages as needed; return only the result required by the caller.");
        message["content"] = json!(receipt.to_string());
        bodies.push((index, body));
    }
    let fits = |child: &Window| {
        child.input_tokens(s, &request(s, &child.messages, &definitions)).saturating_add(4096) < s.input_budget
    };
    ensure!(fits(child), "TASK_INPUTS_OVER_CAPACITY: fixed instructions, briefing and input references exceed capacity; shorten the briefing or declare fewer inputs; parent frame preserved");
    for (index, body) in bodies {
        let reference = std::mem::replace(&mut child.messages[index], body);
        if !fits(child) {
            child.messages[index] = reference;
        }
    }
    Ok(())
}

async fn child_material(s: &Session<'_>, parent: &Window, node: &Node) -> Result<Window> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let chain = tree::path(
        &conn,
        s.task,
        node.parent_id.as_deref().context("child parent missing")?,
    )?;
    ensure!(chain.len() <= tree::MAX_DEPTH, "TASK_DEPTH_LIMIT");
    let mut pinned = BTreeSet::new();
    for ancestor in &chain {
        for id in ancestor.inputs["pinned"].as_array().into_iter().flatten() {
            pinned.insert(id.as_str().unwrap().to_owned());
        }
    }
    for id in node.inputs["pinned"].as_array().into_iter().flatten() {
        pinned.insert(id.as_str().unwrap().to_owned());
    }
    let mut ids = pinned.clone();
    for id in node.inputs["assets"].as_array().into_iter().flatten() {
        ids.insert(id.as_str().unwrap().to_owned());
    }
    // Only tasks (branches) and delegates get a frame; steps share their task's.
    let mut messages = vec![json!({"role":"system","content":s.prompt()})];
    for id in node.inputs["nodes"].as_array().into_iter().flatten() {
        let other = tree::get(&conn, s.task, id.as_str().unwrap())?;
        ensure!(other.result.is_some(), "declared input node has no return");
        ensure!(
            other.executor != "delegate" || other.review.as_deref() == Some("accepted"),
            "declared delegate input has not been accepted"
        );
        messages.push(json!({"role":"user","name":"task_input_node","content":json!({"node_id":other.node_id,"return_event_seq":other.returned_seq.or(other.last_seq),"result":other.result}).to_string(),"_frame_input":true}));
    }
    for seq in node.inputs["history"].as_array().into_iter().flatten() {
        let event = crate::session_history::load_events(
            &conn,
            s.task,
            &crate::session_history::EventQuery {
                exact: seq.as_i64(),
                ..Default::default()
            },
        )?
        .pop()
        .context("declared history event not found")?;
        messages.push(json!({"role":"user","name":"task_input_history","content":json!({"event_seq":event.seq,"kind":event.kind,"record":event.data()?}).to_string(),"_frame_input":true}));
    }
    drop(conn);
    for id in ids {
        let mut message = input_message(s, parent, &id).await?;
        message["_pinned"] = json!(pinned.contains(&id));
        messages.push(message);
    }
    inherit_material(parent,&mut messages);
    // A delegate works on the user's request too: the user's words come along.
    for message in parent.messages.iter().filter(|m| is_user_original(m)) {
        if !messages.contains(message) {
            messages.push(message.clone());
        }
    }
    // fork shares the caller's prepared environment, just like a serial call.
    // It does not copy the caller's working transcript or sibling local values.
    inherit_scope_values(s, parent, &mut messages).await?;
    let mut groups= parent.loaded_tool_groups.clone();
    groups.extend(tool_groups::from_inputs(s, &node.inputs)?);
    messages.push(scope_environment(parent, &frame_task(s, parent)));
    let mut child = Window {
        messages,
        task_node_id: Some(node.node_id.clone()),
        loaded_tool_groups: groups,
        active_skills: parent.active_skills.clone(),
        active_skill_scopes: parent.active_skill_scopes.clone(),
        skill_turn: parent.skill_turn,
        token_calibration: parent.token_calibration.clone(),
        ..Default::default()
    };
    focus_frame(&mut child,node,&chain)?;
    let mut session = s.clone();
    session.scope = tree::scope(&node.node_id);
    fit_input_bodies(&session, &mut child)?;
    Ok(child)
}

fn scope_environment(parent: &Window, owner: &str) -> Value {
    json!({"role":"user","name":"scope_environment","_frame_input":true,
        "content":json!({"host_status":true,"not_a_user_request":true,
            "parent_task":owner,"loaded_tool_groups":parent.loaded_tool_groups,
            "active_skills":parent.active_skills,
            "instruction":"Inherited parent-scope variables and capabilities. Reuse complete scope_value bodies and already loaded tools directly. Read a handle only for missing content or an explicitly needed fresh version. This is a saved environment, not shared mutable memory; local work stays here and only explicit results return."}).to_string()})
}

/// Point a frame at the node it now executes: refresh its path and node card.
fn focus_frame(frame:&mut Window,node:&Node,ancestors:&[Node])->Result<()> {
    frame.messages.retain(|m| {
        !matches!(m["name"].as_str(),Some("task_path"|"task_node"|"task_instruction"))
    });
    frame.messages.insert(1.min(frame.messages.len()),json!({"role":"system","name":"task_path","content":json!(ancestors.iter().map(|n|json!({"node_id":n.node_id,"title":n.title,"goal":n.goal,"reason":n.spawned_from})).collect::<Vec<_>>()).to_string()}));
    frame.messages.insert(2.min(frame.messages.len()),json!({"role":"system","name":"task_node","content":serde_json::to_string(node)?}));
    frame.task_node_id=Some(node.node_id.clone());
    frame.instruction_versions.clear();
    Ok(())
}

fn inherit_material(parent:&Window,messages:&mut Vec<Value>) {
    for message in &parent.messages {
        if message["_pinned"] == true
            || message[crate::context_assets::REFERENCE]["source_tool"] == "read_skill"
        {
            let mut message = message.clone();
            message["role"] = json!("user");
            message["name"] = json!("saved_asset");
            message.as_object_mut().unwrap().remove("tool_call_id");
            message["_pinned"] = json!(true);
            // Provenance/event sequence changes on every delivery, even when
            // it is the same complete skill asset. Carry one body, not every
            // read of it. A reference must never replace an available body.
            if message[crate::context_assets::REFERENCE]["source_tool"] == "read_skill"
                && let Some(id) = message[crate::context_assets::REFERENCE]["asset_id"].as_str()
                && let Some(existing) = messages.iter_mut().find(|m| m[crate::context_assets::REFERENCE]["asset_id"] == id)
            {
                let loaded = !matches!(message["_asset_state"].as_str(), Some("reference_only" | "not_delivered"));
                let existing_loaded = !matches!(existing["_asset_state"].as_str(), Some("reference_only" | "not_delivered"));
                if loaded || !existing_loaded { *existing = message; }
                continue;
            }
            if !messages.iter().any(|m| m == &message) {
                messages.push(message);
            }
        }
    }
}

fn write_window(tx: &rusqlite::Transaction<'_>, task: &str, scope: &str, w: &Window) -> Result<()> {
    tx.execute("INSERT INTO agent_runtime_windows VALUES (?1,?2,?3) ON CONFLICT(task_id,scope) DO UPDATE SET state=excluded.state",params![task,scope,serde_json::to_string(w)?])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Harness {
        root: std::path::PathBuf,
        state: AgentServiceState,
        cancel: CancellationToken,
        w: Window,
        scope: String,
    }
    impl Harness {
        async fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("task-frames-{}", agent_service::uuid_like()));
            std::fs::create_dir_all(&root).unwrap();
            let state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse().unwrap());
            let mut conn = agent_service::open_db(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','','fake','running',0,0)",[]).unwrap();
            let tx = conn.transaction().unwrap();
            tree::ensure_root(&tx, "t", 1).unwrap();
            tx.commit().unwrap();
            let mut harness = Self {
                root,
                state,
                cancel: CancellationToken::new(),
                w: Window {
                    messages: vec![json!({"role":"system","content":Mode::Main.prompt()})],
                    ..Default::default()
                },
                scope: "main".into(),
            };
            harness.persist().await;
            harness
        }
        fn session(&self) -> Session<'_> {
            Session {agent_id:None,task_node_id:None,step_node_id:None,
                plugin: None,
                state: &self.state,
                task: "t",
                model: "fake",
                turn: 1,
                cancel: &self.cancel,
                ban_execution: false,
                mode: Mode::Main,
                scope: self.scope.clone(),
                parent: None,
                input_budget: INPUT_BUDGET,
            }
        }
        async fn persist(&mut self) {
            save(&self.session(), &self.w).await.unwrap();
        }
        fn tool_call(&mut self, name: &str, args: Value) -> crate::model_stream::ToolCall {
            let call = crate::model_stream::ToolCall {
                call_id: agent_service::uuid_like(),
                name: name.into(),
                arguments: args.to_string(),
            };
            self.w.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":name,"arguments":call.arguments}}]}));
            call
        }
        async fn user(&mut self, text: &str) -> i64 {
            let seq = append(
                &self.session(),
                "user/message",
                json!({"turn":1,"message":{"role":"user","content":text}}),
                true,
            )
            .await
            .unwrap();
            self.w
                .messages
                .push(sourced(json!({"role":"user","content":text}), "t", seq));
            self.persist().await;
            seq
        }
        /// Create the main task as the model would; returns (task, first step).
        async fn start(&mut self, goal: &str, first: &str) -> (String, String) {
            let call = self.tool_call("create_task", json!({"goal":goal,"first_step":first}));
            self.persist().await;
            let mut w = self.w.clone();
            create_task(&self.session(), &mut w, &call, "request_test", 1).await.unwrap();
            self.w = w;
            let task = self.w.task_node_id.clone().unwrap();
            self.step("Legacy task started", first).await;
            (task.clone(), self.current(&task))
        }
        /// Finish the current step as the model would; returns the next step.
        async fn step(&mut self, result: &str, next: &str) -> String {
            let call = self.tool_call("create_step", json!({"result":result,"next":next}));
            self.persist().await;
            let mut w = self.w.clone();
            create_step(&self.session(), &mut w, &call, "request_test", 1).await.unwrap();
            self.w = w;
            let task = self.w.task_node_id.clone().unwrap();
            self.current(&task)
        }
        fn current(&self, task: &str) -> String {
            tree::current_step(&agent_service::open_db(&self.root).unwrap(), "t", task).unwrap().unwrap().node_id
        }
        fn active(&self) -> Node {
            tree::active(&agent_service::open_db(&self.root).unwrap(), "t")
                .unwrap()
                .unwrap()
        }
    }

    type Requests = Arc<Mutex<Vec<Value>>>;

    /// A scripted model: `reply(n, body)` answers the n-th request.
    fn completion_request(body:&Value)->bool {
        body["messages"].as_array().unwrap().iter().rev().find(|m| {
                m["role"]=="assistant" || m["role"]=="tool" || m["name"]=="task_completion_check" || m["name"]=="user_interjection" || (m["role"]=="user" && m.get("name").is_none())
            }).is_some_and(|m|m["name"]=="task_completion_check")
    }

    async fn model(reply: impl Fn(usize, &Value) -> Value + Send + Sync + 'static) -> (std::net::SocketAddr, Requests, tokio::task::JoinHandle<()>) {
        let index=std::sync::atomic::AtomicUsize::new(0);
        raw_model(move |_,body| {
            let messages=body["messages"].as_array().unwrap();
            if completion_request(body) {
                return answer(messages.iter().rev().find(|m|m["role"]=="assistant").unwrap()["content"].as_str().unwrap());
            }
            reply(index.fetch_add(1,std::sync::atomic::Ordering::SeqCst),body)
        }).await
    }

    async fn raw_model(reply: impl Fn(usize, &Value) -> Value + Send + Sync + 'static) -> (std::net::SocketAddr, Requests, tokio::task::JoinHandle<()>) {
        use axum::{Json, Router, routing::post};
        let seen: Requests = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let reply = Arc::new(reply);
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let (log, reply) = (log.clone(), reply.clone());
            async move {
                let n = {
                    let mut seen = log.lock().unwrap();
                    seen.push(body.clone());
                    seen.len()-1
                };
                let message = reply(n, &body);
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (address, seen, server)
    }

    fn call(id: &str, name: &str, args: Value) -> Value {
        json!({"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
    }

    fn answer(content: &str) -> Value {
        json!({"role":"assistant","content":content})
    }

    fn tool_names(body: &Value) -> Vec<String> {
        body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_owned()).collect()
    }

    fn tool_result<'a>(body: &'a Value, id: &str) -> &'a str {
        body["messages"].as_array().unwrap().iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
            .and_then(|m| m["content"].as_str())
            .unwrap_or_else(|| panic!("no result for {id}"))
    }

    #[test]
    fn inherited_skill_material_is_deduplicated_by_asset_not_delivery_event() {
        let skill=|seq:i64,state:&str| {
            json!({"role":"tool","tool_call_id":format!("call-{seq}"),
            "content":format!("skill delivery {seq}"),"source":{"seq":seq},"_asset_state":state,
            (crate::context_assets::REFERENCE):{"asset_id":"asset-skill","source_tool":"read_skill"}})
        };
        let parent=Window{messages:vec![skill(1,"loaded"),skill(2,"loaded"),skill(3,"reference_only")],..Default::default()};
        let mut messages=vec![];
        inherit_material(&parent,&mut messages);
        assert_eq!(messages.len(),1);
        assert_eq!(messages[0]["content"],"skill delivery 2");
        assert_eq!(messages[0]["name"],"saved_asset");
        assert_eq!(messages[0]["_pinned"],true);
        assert!(messages[0].get("tool_call_id").is_none());
        // An explicitly declared asset and inherited skill are still one input.
        inherit_material(&parent,&mut messages);
        assert_eq!(messages.len(),1);
    }

    #[tokio::test]
    async fn outputs_accept_workspace_files_this_node_produced() {
        let h=Harness::new().await;
        let write=|h:&Harness,path:&str| {
            std::fs::create_dir_all(h.root.join(path).parent().unwrap()).unwrap();
            std::fs::write(h.root.join(path),"x").unwrap();
        };
        write(&h,"build.py");
        let written=append(&h.session(),"tool/result",json!({"turn":1,"message":{"role":"tool","toolCallId":"w","isError":false},"meta":{"result":{"path":"build.py","code_hash":"fnv1a64:1","changed":true}}}),true).await.unwrap();
        // Written by this node: accepted as a path, alone or with evidence.
        let ok=validate(&h,json!({"status":"done","result":"Built","outputs":["./build.py"],"evidence":[written]}));
        assert!(ok.is_ok(),"{ok:?}");
        // Missing, outside the workspace, or produced by nobody here: refused with a reason.
        assert!(validate(&h,json!({"status":"done","result":"r","outputs":["nope.py"],"evidence":[written]})).unwrap_err().to_string().contains("OUTPUT_FILE_MISSING"));
        assert!(validate(&h,json!({"status":"done","result":"r","outputs":["../x"],"evidence":[written]})).unwrap_err().to_string().contains("OUTPUT_PATH_INVALID"));
        write(&h,"site/index.html");
        assert!(validate(&h,json!({"status":"done","result":"r","outputs":["site/index.html"],"evidence":[written]})).unwrap_err().to_string().contains("OUTPUT_NOT_PRODUCED_HERE"));
        // A file a successful program run in this node generated is accepted.
        append(&h.session(),"tool/result",json!({"turn":1,"message":{"role":"tool","toolCallId":"run","isError":false},"meta":{"result":{"process_exit_code":0}}}),true).await.unwrap();
        assert!(validate(&h,json!({"status":"done","result":"r","outputs":["site/index.html"],"evidence":[written]})).is_ok());
        // Asset IDs keep their own check.
        let missing=format!("{:#}",validate(&h,json!({"status":"done","result":"r","outputs":["asset-missing"],"evidence":[written]})).unwrap_err());
        assert!(missing.contains("output asset not found"),"{missing}");
        fn validate(h:&Harness,args:Value)->Result<()> {
            let conn=agent_service::open_db(&h.root).unwrap();
            tree::validate_return(&conn,"t",&h.active(),&args)
        }
    }

    /// A plugin saves a plan for the user's confirmation inside a task: the host
    /// pauses there. The task stays open, so the next message continues it.
    #[tokio::test]
    async fn a_saved_plan_pauses_the_task_instead_of_finishing_it() {
        let mut h=Harness::new().await;
        let root=h.root.clone();
        let (address,seen,server)=model(move|n,_| match n {
            0=>call("start","create_task",json!({"goal":"Create a presentation","first_step":"Draft the outline"})),
            1=>{
                crate::main_task_anchor::propose_plan(&root,"t",json!({"items":[{"id":"p1","title":"Cover","requirements":"Title and subtitle"}]})).unwrap();
                call("board","read_task_board",json!({}))
            },
            _=>panic!("the host must pause after the plan proposal"),
        }).await;
        h.state=agent_service::tests::test_state(&h.root,address);
        h.user("Create a presentation").await;
        let mut window=h.w.clone();
        assert!(matches!(run_loop(&h.session(),&mut window,5).await.unwrap(),LoopResult::Answer));
        server.abort();
        assert_eq!(seen.lock().unwrap().len(),2);
        let active=h.active();
        assert_eq!((active.kind.as_str(),active.status.as_str()),("task","running"),"the task waits for the user instead of finishing");
        assert!(approval_boundary(&h.session()).unwrap().is_some());
        assert_eq!(window.messages.last().unwrap()["role"],"assistant");
    }




    #[tokio::test]
    async fn delegate_return_commits_native_pair_and_requires_parent_acceptance() {
        let h = Harness::new().await;
        let mut conn = crate::collaboration_jobs::db(&h.root).unwrap();
        conn.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES ('worker','t',1,'spawn','work','{}','running',0)",[]).unwrap();
        let tx = conn.transaction().unwrap();
        let root = tree::get(&tx, "t", tree::ROOT).unwrap();
        let mut node = tree::planned(
            &root,
            &json!({"title":"worker","goal":"work","done_when":"check","executor":"delegate"}),
            0,
            "task",
        )
        .unwrap();
        node.agent_id = Some("worker".into());
        node.status = "running".into();
        tree::emit(&tx, "t", "task/node_started", &mut node, 1).unwrap();
        tx.commit().unwrap();
        drop(conn);
        // A user message during the delegation no longer blocks its return.
        crate::interjections::submit(&h.root, "t", "Also keep the old API").unwrap();
        let mut worker = h.w.clone();
        worker.task_node_id = Some(node.node_id.clone());
        let mut session = h.session();
        session.scope = "delegate:worker".into();
        let proof = append(
            &session,
            "tool/result",
            json!({"message":{"isError":false},"meta":{"result":{"passed":true}}}),
            true,
        )
        .await
        .unwrap();
        let args = json!({"status":"done","result":"verified worker output","evidence":[proof]});
        let call = crate::model_stream::ToolCall {
            call_id: "worker-return".into(),
            name: "return_task".into(),
            arguments: args.to_string(),
        };
        worker.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}));
        save(&session, &worker).await.unwrap();
        assert!(
            delegate_return(&session, &mut worker, &call, "request", 1)
                .await
                .unwrap()
        );
        let conn = agent_service::open_db(&h.root).unwrap();
        let returned = tree::get(&conn, "t", &node.node_id).unwrap();
        assert_eq!(returned.status, "done");
        let stored:Window=serde_json::from_str(&conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope='delegate:worker'",[],|r|r.get::<_,String>(0)).unwrap()).unwrap();
        assert_eq!(
            stored.messages.last().unwrap()["tool_call_id"],
            call.call_id
        );
        let owned: String = conn
            .query_row(
                "SELECT node_id FROM agent_task_events WHERE task_id='t' AND seq=?1",
                [proof],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owned, node.node_id);
    }

    /// Before its task exists the main execution may only create it. Steps
    /// are created by the agent, so one step spans many requests.
    #[tokio::test]
    async fn tools_require_a_task_and_host_records_each_ai_request() {
        let (address,seen,server)=model(|n,body| {
            let names=tool_names(body);
            for name in ["create_task","read_task_board"] {assert!(names.iter().any(|t|t==name),"{name} must be advertised");}
            for name in ["create_step","update_task_definition","branch_task","return_task","plan_tasks","start_task"] {assert!(!names.iter().any(|t|t==name),"{name} must not be advertised");}
            match n {
                0=>{
                    let status=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="task_status").unwrap();
                    assert!(status["content"].as_str().unwrap().contains("No task is running"));
                    call("early","read_file",json!({"path":"source.txt"}))
                },
                1=>{
                    assert!(tool_result(body,"early").contains("NO_TASK"));
                    call("start","create_task",json!({"goal":"Write result.txt from the source","first_step":"Read the source"}))
                },
                2=>call("files","load_tool_group",json!({"group":"files"})),
                3=>json!({"role":"assistant","tool_calls":[
                    {"id":"together","type":"function","function":{"name":"create_task","arguments":json!({"goal":"nested","reason":"test","briefing":"test"}).to_string()}},
                    {"id":"read","type":"function","function":{"name":"read_file","arguments":json!({"path":"source.txt"}).to_string()}}]}),
                4=>{
                    assert!(tool_result(body,"together").contains("called alone"));
                    assert!(tool_result(body,"read").contains("source document"));
                    call("again","read_file",json!({"path":"source.txt"}))
                },
                5=>call("next","read_task_board",json!({})),
                6=>{
                    assert!(tool_result(body,"next").contains("Round"));
                    call("write","write_file",json!({"path":"result.txt","content":"verified output"}))
                },
                7=>answer("Written result.txt from the source."),
                _=>panic!("no host continuation after the answer"),
            }
        }).await;
        let mut h=Harness::new().await;
        std::fs::write(h.root.join("source.txt"),"source document").unwrap();
        h.state=agent_service::tests::test_state(&h.root,address);
        h.user("Write an output using the source").await;
        let mut window=h.w.clone();
        assert!(matches!(run_loop(&h.session(),&mut window,12).await.unwrap(),LoopResult::Answer));
        server.abort();
        assert_eq!(seen.lock().unwrap().len(), 8);
        assert_eq!(std::fs::read_to_string(h.root.join("result.txt")).unwrap(),"verified output");
        let conn=agent_service::open_db(&h.root).unwrap();
        let nodes=tree::nodes(&conn,"t").unwrap();
        let main=nodes.iter().find(|n|n.kind=="task").unwrap();
        let steps=nodes.iter().filter(|n|n.kind=="step" && n.parent_id.as_deref()==Some(main.node_id.as_str())).collect::<Vec<_>>();
        assert_eq!(steps.len(), 6);
        assert!(steps.iter().all(|s|s.status=="done"));
        assert!(steps.iter().all(|s|s.metadata["request_id"].is_string()));
        assert_eq!(steps.last().unwrap().metadata["phase"], "executing");
        assert_eq!(main.status,"done");
        assert_eq!(main.result.as_ref().unwrap()["result"],"Written result.txt from the source.");
        assert!(nodes.iter().any(|n|n.kind=="finish" && n.parent_id.as_deref()==Some(main.node_id.as_str()) && n.status=="done"));
        assert_eq!(tree::get(&conn,"t",tree::ROOT).unwrap().status,"running","the conversation is back at its top level");
        let flow:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='flow/node_add'",[],|r|r.get(0)).unwrap();
        assert_eq!(flow, 8,"one Flow request node per model request");
        // Work belongs to the step it was done in.
        let owner:String=conn.query_row("SELECT node_id FROM agent_task_events WHERE task_id='t' AND kind='tool/call' AND json_extract(data,'$.callId')='write'",[],|r|r.get(0)).unwrap();
        assert_eq!(owner,steps[4].node_id);
    }

    #[tokio::test]
    async fn a_plain_answer_needs_no_task() {
        let (address,seen,server)=model(|n,_| match n {0=>answer("4"),_=>panic!("one answer ends the turn")}).await;
        let mut h=Harness::new().await;
        h.state=agent_service::tests::test_state(&h.root,address);
        h.user("What is 2+2?").await;
        let mut window=h.w.clone();
        assert!(matches!(run_loop(&h.session(),&mut window,3).await.unwrap(),LoopResult::Answer));
        server.abort();
        assert_eq!(seen.lock().unwrap().len(),1);
        let nodes=tree::nodes(&agent_service::open_db(&h.root).unwrap(),"t").unwrap();
        assert_eq!(nodes.len(),1,"no task was created");
        assert_eq!(nodes[0].status,"running");
    }

    /// 烧水热菜 on the real loop. The main task's steps share its context; the
    /// broken pot is a temporary task under the switch-on step with its own
    /// context. Its summary is the result of the create_task call that started
    /// it, and the step continues.
    #[tokio::test]
    async fn a_temporary_task_runs_in_its_own_context_and_its_summary_resumes_the_step() {
        let mut h=Harness::new().await;
        let root=h.root.clone();
        let (address,seen,server)=model(move|n,body| {
            let text=body["messages"].to_string();
            match n {
                0=>call("main","create_task",json!({"goal":"Heat the dinner","first_step":"Fill the pot with water"})),
                1=>{assert!(text.contains("SWITCH_ATTEMPT"));
                    call("filled","read_task_board",json!({}))},
                2=>{assert!(tool_result(body,"filled").contains("task_tree"));
                    call("repair","create_task",json!({"goal":"Repair the pot","first_step":"Inspect the pot base","reason":"Switch on failed: the pot base is loose","briefing":"The pot base is loose"}))},
                3=>{assert!(!text.contains("SWITCH_ATTEMPT"),"the repair has its own context");
                    assert!(text.contains("The pot base is loose"));
                    assert!(text.contains("Heat the dinner"),"the goals above it arrive as the task path");
                    crate::interjections::submit(&root,"t","Use the spare spring").unwrap();
                    call("inspect","read_task_board",json!({}))},
                4=>{assert!(text.contains("Use the spare spring"),"an interjection reaches the running context");
                    call("inspected","read_task_board",json!({}))},
                5=>answer("POT_REPAIRED: spare spring fitted"),
                6=>{assert!(text.contains("SWITCH_ATTEMPT"),"the main task resumes in its own context");
                    assert!(tool_result(body,"repair").contains("POT_REPAIRED"),"the call returns the repair's summary");
                    assert!(!text.contains("\"inspect\""),"the repair's working records stay behind");
                    assert!(text.contains("Use the spare spring"),"the user's words reach the whole main task");
                    answer("Heating; dinner served")},
                _=>panic!("unexpected request {n}"),
            }
        }).await;
        h.state=agent_service::tests::test_state(&h.root,address);
        h.w.messages.push(json!({"role":"user","name":"saved_asset","content":"SWITCH_ATTEMPT: no power"}));
        h.persist().await;
        let mut window=h.w.clone();
        let result=run_loop(&h.session(),&mut window,12).await;
        server.abort();
        assert!(matches!(result.unwrap(),LoopResult::Answer));
        assert_eq!(seen.lock().unwrap().len(), 7);
        let conn=agent_service::open_db(&h.root).unwrap();
        let nodes=tree::nodes(&conn,"t").unwrap();
        let main=nodes.iter().find(|n|n.kind=="task" && n.metadata["role"]=="main").unwrap();
        let repair=nodes.iter().find(|n|n.kind=="task" && n.metadata["role"]=="temp").unwrap();
        let steps=|owner:&str| {
            nodes.iter().filter(|n|n.kind=="step" && n.parent_id.as_deref()==Some(owner)).map(|n|(n.title.as_str(),n.status.as_str())).collect::<Vec<_>>()
        };
        assert_eq!(steps(&main.node_id).len(), 3);
        assert!(steps(&main.node_id).iter().all(|(_,status)|*status=="done"));
        assert_eq!(steps(&repair.node_id).len(), 3);
        let switch=nodes.iter().find(|n|Some(n.node_id.as_str())==repair.parent_id.as_deref()).unwrap();
        assert_eq!(repair.parent_id.as_deref(),Some(switch.node_id.as_str()),"the temporary task hangs under the step that met the problem");
        let finish=nodes.iter().find(|n|n.kind=="finish" && n.parent_id.as_deref()==Some(repair.node_id.as_str())).unwrap();
        assert_eq!((finish.metadata["iterator"]["task"].as_str(),finish.metadata["iterator"]["call_id"].as_str(),finish.metadata["iterator"]["scope"].as_str()),
            (Some(main.node_id.as_str()),Some("repair"),Some("main")),"the finish node holds the waiting step's iterator");
        assert_eq!((main.status.as_str(),repair.status.as_str(),finish.status.as_str()),("done","done","done"));
        assert_eq!(repair.result.as_ref().unwrap()["result"],"POT_REPAIRED: spare spring fitted");
        assert_eq!(tree::get(&conn,"t",tree::ROOT).unwrap().status,"running");
        let frames:i64=conn.query_row("SELECT COUNT(*) FROM agent_runtime_windows WHERE task_id='t' AND scope=?1",[tree::scope(&repair.node_id)],|r|r.get(0)).unwrap();
        assert_eq!(frames,0,"the finished task's context is dropped");
        let owner:String=conn.query_row("SELECT node_id FROM agent_task_events WHERE task_id='t' AND kind='tool/call' AND json_extract(data,'$.callId')='inspect'",[],|r|r.get(0)).unwrap();
        assert_eq!(owner,nodes.iter().find(|n|n.parent_id.as_deref()==Some(repair.node_id.as_str()) && n.kind=="step" && n.ordinal==0).unwrap().node_id,"work belongs to its step");
        let memory = memory_index(&conn, "t", 2).unwrap().unwrap();
        let entries = memory["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "the temporary repair is part of the main task's memory");
        assert_eq!(entries[0]["task"]["node_id"], main.node_id);
        assert_eq!(entries[0]["answer"], "Heating; dinner served", "use the main task's final summary");
    }

    #[tokio::test]
    async fn an_invalid_temporary_task_is_an_ordinary_tool_error() {
        let mut h=Harness::new().await;
        let (main,_)=h.start("Heat the dinner","Switch on the pot").await;
        let call=h.tool_call("create_task",json!({"tasks":[]}));
        h.persist().await;
        let mut w=h.w.clone();
        let scope=create_task(&h.session(),&mut w,&call,"request",1).await.unwrap();
        assert_eq!(scope,"main");
        assert!(w.messages.last().unwrap()["content"].as_str().unwrap().contains("tasks must list 1 to 64 tasks"));
        assert_eq!(h.active().node_id,main);
        assert_eq!(tree::nodes(&agent_service::open_db(&h.root).unwrap(),"t").unwrap().iter().filter(|n|n.kind=="task").count(),1);
    }

    /// A task as a function on the real loop: the caller's materials stay while
    /// it runs; a child receives only the materials passed as asset_ids (its
    /// parameters, without reading them); its own working materials are
    /// released when it returns; the call returns its summary with handles.
    #[tokio::test]
    async fn a_child_task_is_a_scope_with_declared_inputs_and_returns_handles() {
        let mut h=Harness::new().await;
        std::fs::write(h.root.join("spec.txt"),"SPEC_BODY ".repeat(40)).unwrap();
        std::fs::write(h.root.join("other.txt"),"OTHER_BODY").unwrap();
        std::fs::write(h.root.join("scratch.txt"),"SCRATCH_BODY").unwrap();
        fn asset(body:&Value,call:&str)->String {
            let delivery:Value=serde_json::from_str(tool_result(body,call)).unwrap();
            delivery["asset_reference"]["asset_id"].as_str().unwrap().to_owned()
        }
        let ids=Arc::new(Mutex::new((String::new(),String::new())));
        let seen_ids=ids.clone();
        let (address,_seen,server)=model(move|n,body| {
            let text=body["messages"].to_string();
            let mut ids=seen_ids.lock().unwrap();
            match n {
                0=>call("main","create_task",json!({"goal":"Write two sections"})),
                1=>call("files","load_tool_group",json!({"group":"files"})),
                2=>call("spec","read_file",json!({"path":"spec.txt"})),
                3=>call("other","read_file",json!({"path":"other.txt"})),
                4=>{ids.0=asset(body,"spec");
                    call("section","create_task",json!({"goal":"Write section one","reason":"Section one is a unit with its own materials","briefing":"Follow the spec","asset_ids":[ids.0]}))},
                5=>{assert!(text.contains("SPEC_BODY"),"the declared material arrives as the child's parameter");
                    assert!(text.contains("OTHER_BODY"),"parent-scope values are readable without being explicit arguments");
                    call("scratch","read_file",json!({"path":"scratch.txt"}))},
                6=>{ids.1=asset(body,"scratch");
                    assert!(text.contains("SPEC_BODY"),"parameters stay for the whole child task");
                    answer("SECTION_ONE_DONE")},
                7=>{let returned=tool_result(body,"section");
                    assert!(returned.contains("SECTION_ONE_DONE"),"the call returns the child's summary");
                    assert!(returned.contains(&ids.1),"with handles to what the child saved");
                    assert!(!returned.contains(&ids.0),"not the materials it was given");
                    assert!(!text.contains("SCRATCH_BODY"),"the child's working materials are released, not restored");
                    assert!(text.contains("SPEC_BODY") && text.contains("OTHER_BODY"),"the caller's scope is intact");
                    answer("Both sections planned")},
                _=>panic!("unexpected request {n}"),
            }
        }).await;
        h.state=agent_service::tests::test_state(&h.root,address);
        h.user("Write two sections").await;
        let mut window=h.w.clone();
        let outcome=run_loop(&h.session(),&mut window,16).await;
        server.abort();
        assert!(matches!(outcome.unwrap(),LoopResult::Answer));
        let conn=agent_service::open_db(&h.root).unwrap();
        let nodes=tree::nodes(&conn,"t").unwrap();
        let child=nodes.iter().find(|n|n.kind=="task" && n.metadata["role"]=="temp").unwrap();
        assert_eq!(child.inputs["assets"],json!([ids.lock().unwrap().0]));
        assert_eq!(child.status,"done");
        let frames:i64=conn.query_row("SELECT COUNT(*) FROM agent_runtime_windows WHERE task_id='t' AND scope=?1",[tree::scope(&child.node_id)],|r|r.get(0)).unwrap();
        assert_eq!(frames,0,"the child's scope is released");
        let count=|sql:&str|->i64 {conn.query_row(sql,[],|r|r.get(0)).unwrap()};
        assert_eq!(count("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='tool/call' AND json_extract(data,'$.name')='read_asset'"),0);
        assert_eq!(count("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='tool/call' AND json_extract(data,'$.name')='read_file' AND json_extract(data,'$.arguments') LIKE '%spec.txt%'"),1,"the spec was read once, by the caller");
    }

    #[tokio::test]
    async fn an_unknown_declared_material_is_an_ordinary_tool_error() {
        let mut h=Harness::new().await;
        let (main,_)=h.start("Write two sections","Read the spec").await;
        let call=h.tool_call("create_task",json!({"goal":"Write section one","reason":"Own materials","asset_ids":["asset-missing"]}));
        h.persist().await;
        let mut w=h.w.clone();
        let scope=create_task(&h.session(),&mut w,&call,"request",1).await.unwrap();
        assert_eq!(scope,"main");
        let error=w.messages.last().unwrap()["content"].as_str().unwrap();
        assert!(error.contains("asset_ids: asset-missing") && error.contains("ASSET_NOT_FOUND"),"{error}");
        assert_eq!(h.active().node_id,main);
        assert_eq!(tree::nodes(&agent_service::open_db(&h.root).unwrap(),"t").unwrap().iter().filter(|n|n.kind=="task").count(),1);
    }

    #[tokio::test]
    async fn child_inputs_fit_as_a_whole_and_oversized_material_remains_pageable() {
        let mut h = Harness::new().await;
        h.user("ORIGINAL_USER_WORDS").await;
        h.start("Process materials", "Inspect inputs").await;
        let make = |text: String| {
            crate::context_assets::register(&h.root, "t", 0, "example__read",
            &json!({"input":text.chars().next()}), &json!({"body":text})).unwrap()
        };
        let first = make("A".repeat(36000));
        let second = make("B".repeat(36000));
        let huge = make("HUGE_MATERIAL_".repeat(30000));
        let args = json!({"goal":"Extract only the required facts","asset_ids":[first["asset_id"],second["asset_id"],huge["asset_id"]]});
        let node = new_task(&h.active(), &args, 0, "temp", 1).unwrap().0;
        let inputs = declared_inputs(&h.session(), &h.w, &node).await.unwrap();
        let base = temporary_frame(&h.session(), &h.w, &node, vec![], Value::Null).unwrap();
        let base_tokens = base.input_tokens(&h.session(), &request(&h.session(), &base.messages, &tools(&h.session(), &base)));
        let s = Session { input_budget: base_tokens + 4096 + 18000, ..h.session() };
        for input in &inputs[..2] {
            let alone = temporary_frame(&s, &h.w, &node, vec![input.clone()], Value::Null).unwrap();
            assert!(crate::context_assets::complete_body(alone.messages.iter().find(|m| m["_frame_input"] == true && m[crate::context_assets::REFERENCE].is_object()).unwrap()).is_some());
        }
        let before = h.w.messages.clone();
        let child = temporary_frame(&s, &h.w, &node, inputs, Value::Null).unwrap();
        assert_eq!(h.w.messages, before, "the parent is unchanged");
        assert_eq!(child.messages.iter().filter(|m| crate::context_assets::complete_body(m).is_some()).count(), 1,
            "individually fitting assets must not overflow their combined request");
        let receipt = child.messages.iter().find(|m| m[crate::context_assets::REFERENCE]["asset_id"] == huge["asset_id"]).unwrap();
        assert_eq!(receipt["_asset_state"], "reference_only");
        assert_eq!(receipt["_frame_input"], true);
        let data: Value = serde_json::from_str(receipt["content"].as_str().unwrap()).unwrap();
        assert_eq!(data["total_chars"], json!({"body":"HUGE_MATERIAL_".repeat(30000)}).to_string().chars().count());
        assert_eq!(data["delivered"], false);
        let page = load_saved_asset(&s, &data["next_read"], None).await.unwrap();
        assert_eq!(page["returned_chars"], PAGE_CHARS);
        assert!(page["next_char_offset"].is_number());
        let wire = request(&s, &child.messages, &tools(&s, &child));
        assert!(child.input_tokens(&s, &wire) + 4096 < s.input_budget);
        assert!(wire.to_string().contains("ORIGINAL_USER_WORDS"));
        assert!(!wire.to_string().contains("HUGE_MATERIAL_"));
        let restored: Window = serde_json::from_value(serde_json::to_value(&child).unwrap()).unwrap();
        assert_eq!(restored.messages, child.messages);
    }

    #[tokio::test]
    async fn oversized_shared_instructions_still_reject_child_creation_without_changing_parent() {
        let mut h = Harness::new().await;
        h.start("Process source", "Inspect source").await;
        h.w.messages.push(json!({"role":"user","name":"saved_asset","_pinned":true,"content":"PINNED_RULE_".repeat(10000)}));
        let call = h.tool_call("create_task", json!({"goal":"Read the source"}));
        let mut w = h.w.clone();
        let s = Session { input_budget: 18000, ..h.session() };
        let scope = create_task(&s, &mut w, &call, "input-check", 1).await.unwrap();
        assert_eq!(scope, h.scope);
        assert_eq!(&w.messages[..h.w.messages.len()], h.w.messages.as_slice());
        assert!(w.messages.last().unwrap()["content"].as_str().unwrap().contains("TASK_INPUTS_OVER_CAPACITY"));
        assert_eq!(tree::nodes(&agent_service::open_db(&h.root).unwrap(), "t").unwrap().iter().filter(|n| tree::is_temp(n)).count(), 0);
    }

    #[tokio::test]
    async fn oversized_material_child_pages_fold_and_only_its_summary_returns_to_the_caller() {
        let mut h = Harness::new().await;
        let asset = crate::context_assets::register(&h.root, "t", 0, "example__read", &json!({}),
            &json!({"body":"PAGED_BODY_".repeat(40000)})).unwrap();
        let id = asset["asset_id"].as_str().unwrap().to_owned();
        let (address, _, server) = model(move |n, body| match n {
            0 => call("main", "create_task", json!({"goal":"Extract the needed fact"})),
            1 => call("extract", "create_task", json!({"goal":"Read the required source pages and return the fact","asset_ids":[id]})),
            2 => {
                assert!(!body["messages"].to_string().contains("PAGED_BODY_"));
                let inputs = body["messages"].as_array().unwrap().iter().filter(|m| m["name"] == "saved_asset").collect::<Vec<_>>();
                assert!(inputs.iter().any(|m| m["content"].as_str().unwrap().contains("reference_only")));
                call("page-0", "read_asset", json!({"asset_id":id,"char_offset":0,"max_chars":24000}))
            }
            3 | 4 => {
                let previous: Value = serde_json::from_str(tool_result(body, &format!("page-{}", n-3))).unwrap();
                assert_eq!(previous["returned_chars"], PAGE_CHARS);
                if n == 4 {
                    assert!(!body["messages"].as_array().unwrap().iter().any(|m| m["tool_call_id"] == "page-0"),
                        "older pages are foldable working content, not permanent input bodies");
                }
                call(&format!("page-{}", n-2), "read_asset", json!({"asset_id":id,"char_offset":previous["next_char_offset"],"max_chars":24000}))
            }
            5 => {
                let page: Value = serde_json::from_str(tool_result(body, "page-2")).unwrap();
                assert_eq!(page["returned_chars"], PAGE_CHARS);
                answer("EXTRACTED_FACT_ONLY")
            }
            6 => {
                assert!(tool_result(body, "extract").contains("EXTRACTED_FACT_ONLY"));
                assert!(!body["messages"].to_string().contains("PAGED_BODY_"));
                answer("The required fact is available")
            }
            _ => panic!("unexpected request {n}"),
        }).await;
        h.state = agent_service::tests::test_state(&h.root, address);
        h.user("Extract only the fact needed for this task").await;
        // Reserve space relative to fixed instructions: two 24k-character
        // pages plus the delivery reserve must fit before old pages fold.
        // Prompt wording changes must not accidentally test withheld delivery.
        let fixed = estimate(&request(
            &h.session(),
            &h.w.messages,
            &tools(&h.session(), &h.w),
        ));
        let s = Session { input_budget: fixed + 26000, ..h.session() };
        let mut w = h.w.clone();
        assert!(matches!(run_loop(&s, &mut w, 15).await.unwrap(), LoopResult::Answer));
        server.abort();
        let conn = agent_service::open_db(&h.root).unwrap();
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='context/compression_start'", [], |r| r.get::<_,usize>(0)).unwrap(), 0);
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_runtime_windows WHERE task_id='t' AND scope!='main'", [], |r| r.get::<_,usize>(0)).unwrap(), 0);
    }

    /// One create_task call plans a chain: every node exists at once, the tasks
    /// run in order in their own scopes, each sees the earlier returns, work met
    /// unexpectedly becomes a child of the task that met it, and the caller
    /// receives every return in order.
    #[tokio::test]
    async fn a_chain_of_tasks_runs_in_order_each_in_its_own_scope() {
        let mut h=Harness::new().await;
        std::fs::write(h.root.join("spec.txt"),"SPEC_BODY ".repeat(20)).unwrap();
        std::fs::write(h.root.join("draft.txt"),"DRAFT_ONE_BODY").unwrap();
        let root=h.root.clone();
        fn asset(body:&Value,call:&str)->String {
            let delivery:Value=serde_json::from_str(tool_result(body,call)).unwrap();
            delivery["asset_reference"]["asset_id"].as_str().unwrap().to_owned()
        }
        fn started(body:&Value)->Value {
            serde_json::from_str(body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="task_started").unwrap()["content"].as_str().unwrap()).unwrap()
        }
        let (address,_seen,server)=model(move|n,body| {
            let text=body["messages"].to_string();
            let statuses=|| {
                let mut tasks=tree::nodes(&agent_service::open_db(&root).unwrap(),"t").unwrap().into_iter()
                    .filter(|n|n.kind=="task" && n.metadata["role"]=="temp").collect::<Vec<_>>();
                tasks.sort_by_key(|n|n.ordinal);
                tasks.into_iter().map(|n|n.status).collect::<Vec<_>>()
            };
            match n {
                0=>call("main","create_task",json!({"goal":"Write three sections"})),
                1=>call("files","load_tool_group",json!({"group":"files"})),
                2=>call("spec","read_file",json!({"path":"spec.txt"})),
                3=>call("chain","create_task",json!({"asset_ids":[asset(body,"spec")],"tasks":[
                    {"goal":"Write section one"},{"goal":"Write section two"},{"goal":"Write section three"}]})),
                4=>{let tasks=statuses();
                    assert_eq!(tasks.len(),3,"every node of the call exists at once");
                    assert!(tasks[0]=="running" && tasks[1..].iter().all(|s|s!="running" && s!="done"),"{tasks:?}");
                    assert!(text.contains("SPEC_BODY"),"shared materials are each task's parameters");
                    let chain=&started(body)["chain"];
                    assert_eq!((chain["position"].as_u64(),chain["total"].as_u64()),(Some(1),Some(3)));
                    call("draft","read_file",json!({"path":"draft.txt"}))},
                5=>answer("SECTION_ONE_DONE"),
                6=>{assert!(text.contains("SECTION_ONE_DONE"),"the next task sees the earlier return");
                    assert!(!text.contains("DRAFT_ONE_BODY"),"the earlier task's working materials were released");
                    assert!(text.contains("SPEC_BODY"));
                    assert_eq!(started(body)["chain"]["position"].as_u64(),Some(2));
                    call("repair","create_task",json!({"goal":"Fix the broken reference in section two"}))},
                7=>answer("REFERENCE_FIXED"),
                8=>{assert!(tool_result(body,"repair").contains("REFERENCE_FIXED"),"unexpected work returns to the task that met it");
                    answer("SECTION_TWO_DONE")},
                9=>{assert!(text.contains("SECTION_ONE_DONE") && text.contains("SECTION_TWO_DONE"));
                    answer("SECTION_THREE_DONE")},
                10=>{let returned=tool_result(body,"chain");
                    let at=["SECTION_ONE_DONE","SECTION_TWO_DONE","SECTION_THREE_DONE"].map(|m|returned.find(m).unwrap());
                    assert!(at[0]<at[1] && at[1]<at[2],"every return, in order");
                    assert!(!text.contains("DRAFT_ONE_BODY") && text.contains("SPEC_BODY"));
                    answer("All sections written")},
                _=>panic!("unexpected request {n}"),
            }
        }).await;
        h.state=agent_service::tests::test_state(&h.root,address);
        h.user("Write three sections").await;
        let mut window=h.w.clone();
        let outcome=run_loop(&h.session(),&mut window,24).await;
        server.abort();
        assert!(matches!(outcome.unwrap(),LoopResult::Answer));
        let conn=agent_service::open_db(&h.root).unwrap();
        let nodes=tree::nodes(&conn,"t").unwrap();
        let chain=nodes.iter().filter(|n|n.goal.starts_with("Write section")).collect::<Vec<_>>();
        assert_eq!(chain.len(),3);
        assert!(chain.iter().all(|n|n.status=="done"));
        let repair=nodes.iter().find(|n|n.goal.starts_with("Fix the broken")).unwrap();
        let owner=nodes.iter().find(|n|Some(n.node_id.as_str())==repair.parent_id.as_deref()).unwrap();
        let two=chain.iter().find(|n|n.goal=="Write section two").unwrap();
        assert_eq!((owner.kind.as_str(),owner.parent_id.as_deref()),("step",Some(two.node_id.as_str())),"inserted work hangs under the step that met it");
        let frames:i64=conn.query_row("SELECT COUNT(*) FROM agent_runtime_windows WHERE task_id='t' AND scope LIKE 'node:%'",[],|r|r.get(0)).unwrap();
        assert_eq!(frames,0,"every child scope is released");
    }

    #[tokio::test]
    async fn the_main_task_and_its_first_chain_come_from_one_call() {
        let mut h=Harness::new().await;
        let (address,_seen,server)=model(move|n,body| {
            let text=body["messages"].to_string();
            match n {
                0=>call("plan","create_task",json!({"goal":"MAIN_GOAL two notes","tasks":[{"goal":"Write note A"},{"goal":"Write note B"}]})),
                1=>{assert!(text.contains("MAIN_GOAL") && text.contains("Write note A"),"the main goal arrives as the task path");
                    answer("NOTE_A_DONE")},
                2=>{assert!(text.contains("NOTE_A_DONE"));
                    answer("NOTE_B_DONE")},
                3=>{let returned=tool_result(body,"plan");
                    assert!(returned.contains("NOTE_A_DONE") && returned.contains("NOTE_B_DONE"));
                    answer("Both notes written")},
                _=>panic!("unexpected request {n}"),
            }
        }).await;
        h.state=agent_service::tests::test_state(&h.root,address);
        h.user("Write two notes").await;
        let mut window=h.w.clone();
        let outcome=run_loop(&h.session(),&mut window,12).await;
        server.abort();
        assert!(matches!(outcome.unwrap(),LoopResult::Answer));
        let conn=agent_service::open_db(&h.root).unwrap();
        let nodes=tree::nodes(&conn,"t").unwrap();
        let main=nodes.iter().find(|n|n.kind=="task" && n.metadata["role"]=="main").unwrap();
        assert_eq!(main.status,"done");
        let notes=nodes.iter().filter(|n|n.kind=="task" && n.metadata["role"]=="temp").collect::<Vec<_>>();
        assert_eq!(notes.len(),2);
        assert!(notes.iter().all(|n|n.parent_id.as_deref()==Some(main.node_id.as_str()) && n.status=="done"));
        let calls:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='tool/call' AND json_extract(data,'$.callId')='plan'",[],|r|r.get(0)).unwrap();
        assert_eq!(calls,1,"one call event for the main task and its chain");
    }

    #[tokio::test]
    async fn a_chain_with_an_unknown_material_is_not_created() {
        let mut h=Harness::new().await;
        let (main,_)=h.start("Write two parts","Plan the parts").await;
        let call=h.tool_call("create_task",json!({"tasks":[{"goal":"Write part one"},{"goal":"Write part two","asset_ids":["asset-missing"]}]}));
        h.persist().await;
        let mut w=h.w.clone();
        let scope=create_task(&h.session(),&mut w,&call,"request",1).await.unwrap();
        assert_eq!(scope,"main");
        let error=w.messages.last().unwrap()["content"].as_str().unwrap();
        assert!(error.contains("tasks[1].asset_ids: asset-missing") && error.contains("ASSET_NOT_FOUND"),"{error}");
        assert_eq!(h.active().node_id,main);
        assert_eq!(tree::nodes(&agent_service::open_db(&h.root).unwrap(),"t").unwrap().iter().filter(|n|n.kind=="task").count(),1,"no task of the chain exists");
    }

    /// A later task that cannot start ends its chain. The caller receives the
    /// finished results, which tasks did not start and why; the run continues.
    #[tokio::test]
    async fn a_chain_stops_when_its_next_task_cannot_start() {
        let mut h=Harness::new().await;
        let saved=crate::context_assets::register(&h.root,"t",0,"example__read",&json!({"id":"brief"}),&json!({"body":"BRIEF_BODY"})).unwrap();
        let id=saved["asset_id"].as_str().unwrap().to_owned();
        let snapshot=h.root.join(saved["snapshot_path"].as_str().unwrap());
        let (address,_seen,server)=model(move|n,body| match n {
            0=>call("main","create_task",json!({"goal":"Write three parts"})),
            1=>call("chain","create_task",json!({"tasks":[{"goal":"Write part one"},{"goal":"Write part two","asset_ids":[id]},{"goal":"Write part three"}]})),
            2=>{std::fs::remove_file(&snapshot).unwrap();answer("PART_ONE_DONE")},
            3=>{let returned:Value=serde_json::from_str(tool_result(body,"chain")).unwrap();
                assert_eq!(returned["results"].as_array().unwrap().len(),1);
                assert!(returned["results"][0]["result"].as_str().unwrap().contains("PART_ONE_DONE"));
                let stopped=&returned["stopped"];
                assert!(stopped["error"].as_str().unwrap().contains("asset_ids"),"{stopped}");
                assert_eq!(stopped["not_started"].as_array().unwrap().len(),2);
                answer("Reported which parts did not start")},
            _=>panic!("unexpected request {n}"),
        }).await;
        h.state=agent_service::tests::test_state(&h.root,address);
        h.user("Write three parts").await;
        let mut window=h.w.clone();
        let outcome=run_loop(&h.session(),&mut window,12).await;
        server.abort();
        assert!(matches!(outcome.unwrap(),LoopResult::Answer),"the run continues after the chain stops");
        let nodes=tree::nodes(&agent_service::open_db(&h.root).unwrap(),"t").unwrap();
        let status=|goal:&str| {
            nodes.iter().find(|n|n.goal==goal).unwrap().status.clone()
        };
        assert_eq!((status("Write part one"),status("Write part two"),status("Write part three")),("done".into(),"cancelled".into(),"cancelled".into()));
    }

    #[tokio::test]
    async fn parent_environment_survives_serial_children_and_child_locals_do_not_leak() {
        let mut h = Harness::new().await;
        std::fs::write(h.root.join("common.txt"), "MAIN_SHARED_VALUE").unwrap();
        std::fs::write(h.root.join("local.txt"), "ONLY_CHILD_A_LOCAL_VALUE").unwrap();
        let root = h.root.clone();
        let (address, seen, server) = model(move |n, body| {
            let text = body["messages"].to_string();
            match n {
                0 => call("main", "create_task", json!({"goal":"Process two outputs using shared preparation"})),
                1 => call("skills", "list_skills", json!({})),
                2 => call("workspace", "workspace_info", json!({})),
                3 => call("files", "load_tool_group", json!({"group":"files"})),
                4 => call("common", "read_file", json!({"path":"common.txt"})),
                5 => call("children", "create_task", json!({"tasks":[{"goal":"Write A"},{"goal":"Write B"}]})),
                6 => {
                    assert!(text.contains("MAIN_SHARED_VALUE"));
                    let values = body["messages"].as_array().unwrap().iter()
                        .filter(|m| m["name"] == "scope_value").collect::<Vec<_>>();
                    assert!(values.iter().any(|m| m.to_string().contains("workspace_root")));
                    assert!(values.iter().any(|m| m.to_string().contains("list_skills")));
                    assert!(tool_names(body).iter().any(|name| name == "write_file"));
                    call("local", "read_file", json!({"path":"local.txt"}))
                }
                7 => call("nested", "create_task", json!({"goal":"Inspect the local prerequisite"})),
                8 => {
                    assert!(text.contains("MAIN_SHARED_VALUE") && text.contains("ONLY_CHILD_A_LOCAL_VALUE"));
                    answer("Nested prerequisite checked")
                }
                9 => {
                    assert!(tool_result(body, "nested").contains("Nested prerequisite checked"));
                    call("a", "write_file", json!({"path":"a.txt","content":"A output"}))
                }
                10 => answer("A output saved in a.txt"),
                11 => {
                    assert!(text.contains("MAIN_SHARED_VALUE"));
                    assert!(!text.contains("ONLY_CHILD_A_LOCAL_VALUE"), "sibling receives A's return, not A's working body");
                    assert!(text.contains("A output saved in a.txt"));
                    assert!(tool_names(body).iter().any(|name| name == "write_file"));
                    call("b", "write_file", json!({"path":"b.txt","content":"B output"}))
                }
                12 => answer("B output saved in b.txt"),
                13 => {
                    assert!(tool_result(body, "children").contains("A output saved in a.txt"));
                    assert!(tool_result(body, "children").contains("B output saved in b.txt"));
                    assert!(text.contains("MAIN_SHARED_VALUE"));
                    assert!(!text.contains("ONLY_CHILD_A_LOCAL_VALUE"));
                    let conn = agent_service::open_db(&root).unwrap();
                    let windows: i64 = conn.query_row("SELECT count(*) FROM agent_runtime_windows WHERE task_id='t' AND scope LIKE 'node:%'", [], |r| r.get(0)).unwrap();
                    assert_eq!(windows, 0, "returned child windows are released");
                    answer("Both outputs delivered")
                }
                _ => panic!("unexpected request {n}"),
            }
        }).await;
        h.state = agent_service::tests::test_state(&h.root, address);
        h.user("Create two outputs; prepare common inputs in the main task")
            .await;
        let mut window = h.w.clone();
        let outcome = run_loop(&h.session(), &mut window, 24).await;
        server.abort();
        assert!(matches!(outcome.unwrap(), LoopResult::Answer));
        assert_eq!(
            std::fs::read_to_string(h.root.join("a.txt")).unwrap(),
            "A output"
        );
        assert_eq!(
            std::fs::read_to_string(h.root.join("b.txt")).unwrap(),
            "B output"
        );
        let requests = seen.lock().unwrap();
        assert_eq!(
            requests.len(),
            14,
            "one model request per action/return, without completion checks"
        );
        assert!(!requests.iter().any(completion_request));
    }

    #[tokio::test]
    async fn child_tasks_inherit_parent_values_and_ordered_caller_activity() {
        let mut h = Harness::new().await;
        std::fs::write(h.root.join("source.txt"), "ORIGINAL_PAGE_BODY").unwrap();
        // A different task in the same conversation must not leak into this index.
        {
            let mut conn = agent_service::open_db(&h.root).unwrap();
            let tx = conn.transaction().unwrap();
            let root = tree::get(&tx, "t", tree::ROOT).unwrap();
            let mut other = tree::planned(&root, &json!({"title":"Unrelated old task","goal":"Unrelated old task"}), 0, "task").unwrap();
            other.status = "done".into();
            tree::emit(&tx, "t", "task/node_returned", &mut other, 1).unwrap();
            agent_service::append_event_tx(&tx, "t", "tool/call", &json!({"task_node_id":other.node_id,
                "callId":"foreign","name":"read_file","description":"UNRELATED_ACTIVITY","arguments":"{}"}), None).unwrap();
            tx.commit().unwrap();
        }
        let (address, _, server) = model(|n, body| match n {
            0 => call("main", "create_task", json!({"goal":"Use the source and repair an independent prerequisite"})),
            1 => call("files", "load_tool_group", json!({"group":"files"})),
            2 => call("page", "read_file", json!({"path":"source.txt"})),
            3 => call("repair", "create_task", json!({"goal":"Repair the prerequisite","reason":"An independent prerequisite needs repair"})),
            4 => {
                let index: Value = serde_json::from_str(body["messages"].as_array().unwrap().iter()
                    .find(|message| message["name"] == "caller_activity").unwrap()["content"].as_str().unwrap()).unwrap();
                let activity = index["activity"].as_array().unwrap();
                assert!(activity.windows(2).all(|pair| pair[0]["event_seq"].as_i64() < pair[1]["event_seq"].as_i64()));
                let read = activity.iter().find(|entry| entry["tool"] == "read_file").unwrap();
                assert!(!read["description"].as_str().unwrap().is_empty());
                assert!(read["arguments_preview"].as_str().unwrap().contains("source.txt"));
                assert_eq!(read["status"], "returned");
                assert!(read["result_reference"]["arguments"]["event_seq"].is_i64());
                assert!(body["messages"].to_string().contains("ORIGINAL_PAGE_BODY"), "the child reuses its parent value without another read");
                assert!(!index.to_string().contains("UNRELATED_ACTIVITY"));
                call("reopen", "read_asset", read["assets"][0]["read_reference"]["arguments"].clone())
            }
            5 => {
                assert!(body["messages"].to_string().contains("ORIGINAL_PAGE_BODY"), "a duplicate read may be a receipt while the inherited body stays available");
                call("nested", "create_task", json!({"goal":"Check the repaired prerequisite","reason":"Independent final prerequisite check"}))
            }
            6 => {
                let index: Value = serde_json::from_str(body["messages"].as_array().unwrap().iter()
                    .find(|message| message["name"] == "caller_activity").unwrap()["content"].as_str().unwrap()).unwrap();
                let activity = index["activity"].as_array().unwrap();
                let read = activity.iter().position(|entry| entry["tool"] == "read_file").unwrap();
                let reopened = activity.iter().position(|entry| entry["tool"] == "read_asset").unwrap();
                assert!(read < reopened, "nested tasks see ancestor activity before caller activity");
                assert!(activity[reopened]["assets"][0]["asset_id"].is_string());
                assert!(body["messages"].to_string().contains("ORIGINAL_PAGE_BODY"), "nested children inherit ancestor values too");
                answer("Prerequisite checked")
            }
            7 => { assert!(tool_result(body, "nested").contains("Prerequisite checked")); answer("Prerequisite repaired") }
            8 => { assert!(tool_result(body, "repair").contains("Prerequisite repaired")); answer("Source used and prerequisite repaired") }
            _ => panic!("unexpected request {n}"),
        }).await;
        h.state = agent_service::tests::test_state(&h.root, address);
        h.user("Use the source").await;
        let mut window = h.w.clone();
        let outcome = run_loop(&h.session(), &mut window, 16).await;
        server.abort();
        assert!(matches!(outcome.unwrap(), LoopResult::Answer));
    }

    #[tokio::test]
    async fn every_round_receives_compact_activity_and_keeps_original_results_by_reference() {
        let mut h=Harness::new().await;
        std::fs::write(h.root.join("source.txt"),"PRIVATE_SOURCE_BODY").unwrap();
        let root=h.root.clone();
        let (address,seen,server)=model(move|n,body|match n {
            0=>call("start","create_task",json!({"goal":"Process source.txt"})),
            1=>call("files","load_tool_group",json!({"group":"files"})),
            2=>call("read","read_file",json!({"path":"source.txt"})),
            3=>{
                let messages=body["messages"].as_array().unwrap();
                assert!(!messages.iter().any(|m|m["name"]=="task_activity"),"the old duplicate ledger is no longer attached to every round");
                assert!(tool_result(body,"read").contains("PRIVATE_SOURCE_BODY"),"the overview does not replace native tool delivery");
                call("write","write_file",json!({"path":"processed.txt","content":"Processed source"}))
            }
            4=>{
                let messages=body["messages"].as_array().unwrap();
                assert!(!messages.iter().any(|m|m["name"]=="step_result"),"below capacity completed rounds keep their native exchange");
                assert!(tool_result(body,"read").contains("PRIVATE_SOURCE_BODY"),"an earlier read stays usable without reading it again");
                let index:Value=serde_json::from_str(messages.iter().find(|m|m["name"]=="task_asset_index").unwrap()["content"].as_str().unwrap()).unwrap();
                let entry=index["assets"].as_array().unwrap().iter().find(|a|a["source_tool"]=="read_file").unwrap();
                assert_eq!(entry["in_context"],true);
                let original=crate::context_assets::read(&root,"t",&json!({"asset_id":entry["asset_id"]}),None).unwrap();
                assert!(original.to_string().contains("PRIVATE_SOURCE_BODY"),"the complete original remains available");
                assert!(tool_result(body,"write").contains("processed.txt"),"the newest exchange stays in native form");
                answer("Source processed")
            }
            _=>panic!("unexpected model request {n}"),
        }).await;
        h.state=agent_service::tests::test_state(&h.root,address);
        h.user("Process source.txt").await;let mut window=h.w.clone();
        let outcome=run_loop(&h.session(),&mut window,10).await;
        server.abort();assert!(matches!(outcome.unwrap(),LoopResult::Answer));
        assert_eq!(std::fs::read_to_string(h.root.join("processed.txt")).unwrap(),"Processed source");
        let conn=agent_service::open_db(&h.root).unwrap();
        let reads:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='tool/call' AND json_extract(data,'$.name')='read_file'",[],|row|row.get(0)).unwrap();
        assert_eq!(reads,1);
        assert!(
            !seen.lock().unwrap().iter().any(|body|completion_request(body)));
        assert!(!window.messages.iter().any(|m|m["name"]=="task_activity"),"overviews stay outside persisted conversational history");
    }

    #[test]
    fn activity_facts_preserve_failed_write_metadata_without_inventing_a_save_outcome() {
        let facts=result_facts(&json!({"status":"failed","error":{"message":"connection lost"},"data":[{"applied":true,"revision":4,"html":"PRIVATE_HTML"},{"saved":false,"revision":5}]}));
        assert_eq!(facts["status"],"failed");assert_eq!(facts["data"][0]["applied"],true);
        assert_eq!(facts["data"][1]["saved"],false);assert_eq!(facts["data"][1]["revision"],5);
        assert_eq!(facts["error"]["message"],"connection lost");assert!(!facts.to_string().contains("PRIVATE_HTML"));
        assert_eq!(result_facts(&json!({"status":"failed","data":"Text overflow; no content was applied"}))["data"],"Text overflow; no content was applied");
        let arguments=brief_arguments(&json!({"page":4,"expected_revision":5,"html":"PRIVATE_HTML"}));
        assert_eq!(arguments["page"],4);assert_eq!(arguments["expected_revision"],5);
        assert_eq!(arguments["html"]["characters"],12);assert!(!arguments.to_string().contains("PRIVATE_HTML"));
    }

    #[tokio::test]
    async fn current_activity_is_bounded_and_rebuilt_even_without_native_chat_history() {
        let h=Harness::new().await;
        let mut conn=agent_service::open_db(&h.root).unwrap();let tx=conn.transaction().unwrap();
        let root=tree::get(&tx,"t",tree::ROOT).unwrap();
        let mut task=tree::planned(&root,&json!({"title":"Inspect records","goal":"Inspect records"}),0,"task").unwrap();task.status="running".into();
        tree::emit(&tx,"t","task/node_started",&mut task,1).unwrap();
        let mut step=tree::planned(&task,&json!({"title":"Call tools","goal":"Call tools"}),0,"step").unwrap();step.status="current".into();
        tree::emit(&tx,"t","task/node_started",&mut step,1).unwrap();
        for index in 0..32 {
            record_event(&tx,"t","tool/call",json!({"task_node_id":step.node_id,"flowNodeId":"request","callId":format!("call-{index}"),"name":"read_record","arguments":json!({"record":index}).to_string(),"description":"Read one record"}),None).unwrap();
            record_event(&tx,"t","tool/result",json!({"task_node_id":step.node_id,"flowNodeId":"request","message":{"toolCallId":format!("call-{index}")},"meta":{"result":{"status":"succeeded","data":{"revision":index,"content":"PRIVATE_RESULT"}}}}),None).unwrap();
        }
        tx.commit().unwrap();
        let overview=round_activity(&h.session(),&task.node_id,&[]).unwrap().unwrap();
        assert_eq!((overview["total_calls"].as_u64(),overview["omitted_calls"].as_u64()),(Some(32),Some(8)));
        let records=overview["activity"].as_array().unwrap();assert_eq!(records.len(),24);
        assert_eq!(records.first().unwrap()["arguments"]["record"],8);assert_eq!(records.last().unwrap()["result_facts"]["data"]["revision"],31);
        assert!(records.windows(2).all(|pair|pair[0]["event_seq"].as_i64()<pair[1]["event_seq"].as_i64()));
        assert!(!overview.to_string().contains("PRIVATE_RESULT"));
        let mut session=h.session();session.task_node_id=Some(task.node_id.clone());
        let wire=request(&session,&[json!({"role":"system","content":Mode::Main.prompt()})],&[]);
        assert!(!wire["messages"].as_array().unwrap().iter().any(|m|m["name"]=="task_activity"),"the full historical ledger is not duplicated in every request");
        assert_eq!(overview["history_reference"]["arguments"]["node_id"],task.node_id);
    }

    /// A new user request starts a clean main context: only the memory index
    /// of earlier turns, plus the tool groups still loaded. Details come from
    /// history by node.
    #[tokio::test]
    async fn each_request_starts_a_clean_context_with_the_memory_index() {
        let (address,seen,server)=model(|n,body| {
            let text=body["messages"].to_string();
            match n {
                0=>call("t1","create_task",json!({"goal":"Write the note","first_step":"Write note.txt"})),
                1=>call("files","load_tool_group",json!({"group":"files"})),
                2=>call("write","write_file",json!({"path":"note.txt","content":"NOTE_BODY"})),
                3=>answer("NOTE_WRITTEN to note.txt"),
                4=>{
                    assert!(!text.contains("NOTE_BODY"),"earlier working records are not loaded");
                    let memory=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="conversation_memory").expect("memory index");
                    let index:Value=serde_json::from_str(memory["content"].as_str().unwrap()).unwrap();
                    let earlier=&index["entries"][0];
                    assert_eq!((earlier["turn"].as_i64(),earlier["request"].as_str()),(Some(1),Some("Write a note")));
                    assert_eq!(earlier["kind"], "task");
                    assert_eq!(earlier["task"]["goal"], "Write the note");
                    assert!(earlier["answer"].as_str().unwrap().contains("NOTE_WRITTEN"));
                    assert!(tool_names(body).iter().any(|t|t=="write_file"),"loaded tool groups carry over");
                    call("t2","create_task",json!({"goal":"Report what the note says","first_step":"Read the earlier task"}))
                },
                5=>{
                    let memory=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="conversation_memory").unwrap();
                    let index:Value=serde_json::from_str(memory["content"].as_str().unwrap()).unwrap();
                    call("history","read_session_history",json!({"node_id":index["entries"][0]["task"]["node_id"],"include_descendants":true,"query":"write_file"}))
                },
                6=>{
                    let history=tool_result(body,"history");
                    assert!(history.contains("NOTE_BODY"),"details come from history by node: {history}");
                    answer("The note says NOTE_BODY")
                },
                _=>panic!("unexpected request {n}"),
            }
        }).await;
        let mut h=Harness::new().await;
        h.state=agent_service::tests::test_state(&h.root,address);
        run_task(h.state.clone(),"t".into(),"fake".into(),"Write a note".into(),10,CancellationToken::new(),1,false).await.unwrap();
        run_task(h.state.clone(),"t".into(),"fake".into(),"What did the note say?".into(),10,CancellationToken::new(),2,false).await.unwrap();
        server.abort();
        assert_eq!(seen.lock().unwrap().len(), 7);
        let conn=agent_service::open_db(&h.root).unwrap();
        let resets:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='context/main_reset'",[],|r|r.get(0)).unwrap();
        assert_eq!(resets,2);
        let tasks=tree::nodes(&conn,"t").unwrap().into_iter().filter(|n|n.kind=="task").collect::<Vec<_>>();
        assert_eq!(tasks.iter().map(|t|(t.metadata["turn"].as_i64(),t.status.as_str())).collect::<Vec<_>>(),[(Some(1),"done"),(Some(2),"done")]);
    }

    /// A turn that stops before the finish node leaves the task open; the next
    /// message continues it in the same context instead of starting over.
    #[tokio::test]
    async fn a_stopped_turn_resumes_its_task_in_the_same_context() {
        let (address,seen,server)=model(|n,body| {
            let text=body["messages"].to_string();
            match n {
                0=>call("start","create_task",json!({"goal":"Draft the plan","first_step":"Read the brief"})),
                1=>call("board","read_task_board",json!({})),
                2=>{
                    assert!(text.contains("Draft the plan") && text.contains("Draft a plan"),"the same task context continues");
                    assert!(!body["messages"].as_array().unwrap().iter().any(|m|m["name"]=="conversation_memory"),"not a fresh context");
                    answer(&"计划已完成".repeat(50))
                }
                3=>{
                    let memory=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="conversation_memory").unwrap();
                    let index:Value=serde_json::from_str(memory["content"].as_str().unwrap()).unwrap();
                    let entries=index["entries"].as_array().unwrap();
                    assert_eq!(entries.len(),1,"a resumed task has one memory, not one per turn");
                    assert_eq!(entries[0]["request"],"Draft a plan");
                    assert_eq!(entries[0]["task"]["goal"],"Draft the plan");
                    assert_eq!(entries[0]["task"]["status"],"done");
                    assert_eq!(entries[0]["answer"],"计划已完成".repeat(40),"the final summary is truncated to 200 characters");
                    assert!(!entries.iter().any(|entry|entry["request"]=="continue"));
                    answer(&"问答摘要".repeat(60))
                }
                4=>{
                    let memory=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="conversation_memory").unwrap();
                    let index:Value=serde_json::from_str(memory["content"].as_str().unwrap()).unwrap();
                    let entries=index["entries"].as_array().unwrap();
                    assert_eq!(entries.len(),2,"a task-free answer keeps its own memory");
                    assert_eq!(entries[0]["kind"],"task");
                    assert_eq!(entries[1]["kind"],"qa");
                    assert_eq!(entries[1]["turn"],3);
                    assert_eq!(entries[1]["request"],"A separate question");
                    assert_eq!(entries[1]["answer"],"问答摘要".repeat(50));
                    answer("Done")
                }
                _=>panic!("unexpected request {n}"),
            }
        }).await;
        let mut h=Harness::new().await;
        h.state=agent_service::tests::test_state(&h.root,address);
        run_task(h.state.clone(),"t".into(),"fake".into(),"Draft a plan".into(),2,CancellationToken::new(),1,false).await.unwrap();
        let conn=agent_service::open_db(&h.root).unwrap();
        assert_eq!(tree::active(&conn,"t").unwrap().unwrap().kind,"task","the task stays open");
        run_task(h.state.clone(),"t".into(),"fake".into(),"continue".into(),4,CancellationToken::new(),2,false).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 3);
        let main = tree::nodes(&conn, "t")
            .unwrap()
            .into_iter()
            .find(|n| n.kind == "task")
            .unwrap();
        assert_eq!(main.status, "done");
        let resets:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='context/main_reset'",[],|r|r.get(0)).unwrap();
        assert_eq!(resets, 1);
        drop(conn);
        run_task(
            h.state.clone(),
            "t".into(),
            "fake".into(),
            "A separate question".into(),
            4,
            CancellationToken::new(),
            3,
            false,
        )
        .await
        .unwrap();
        run_task(
            h.state.clone(),"t".into(),
            "fake".into(),
            "Another question".into(),
            4,
            CancellationToken::new(),
            4,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(seen.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn memory_keeps_the_latest_thirty_completed_entries() {
        let h = Harness::new().await;
        let mut conn = agent_service::open_db(&h.root).unwrap();
        let tx = conn.transaction().unwrap();
        for turn in 1..=32 {
            agent_service::append_event_tx(&tx, "t", "user/message", &json!({"turn":turn,
                "message":{"role":"user","content":format!("Question {turn}")}}), None).unwrap();
            agent_service::append_event_tx(&tx, "t", "assistant/message", &json!({"turn":turn,
                "message":{"role":"assistant","content":format!("Answer {turn}")}}), None).unwrap();
        }
        // A question without a final answer is not a completed memory.
        agent_service::append_event_tx(&tx, "t", "user/message", &json!({"turn":33,
            "message":{"role":"user","content":"Unanswered"}}), None).unwrap();
        tx.commit().unwrap();
        let memory = memory_index(&conn, "t", 34).unwrap().unwrap();
        let entries = memory["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 30);
        assert_eq!(memory["entries_omitted"], 2);
        assert_eq!(entries[0]["turn"], 3);
        assert_eq!(entries[29]["turn"], 32);
        assert!(entries.iter().all(|entry| entry["kind"] == "qa"));
    }

    #[tokio::test]
    async fn finish_returns_to_the_serial_iterator_without_another_model_call() {
        let (address,seen,server)=raw_model(|n,body| match n {
                0=> call(
                "chain",
                "create_task",
                json!({"goal":"Main function", "tasks":[
                {"goal":"First function"}, {"goal":"Second function"}]}),
            ),
            1 => answer("FIRST_RETURN"),
            2 => {
                assert!(!completion_request(body));
                assert!(body["messages"].to_string().contains("FIRST_RETURN"));
                let current: Value = serde_json::from_str(
                    body["messages"].as_array().unwrap().iter().find(|m| m["name"]== "task_started")
                        .unwrap()["content"]
                        .as_str()
                        .unwrap(),
                ).unwrap();
                    assert_eq!(current["task"]["goal"], "Second function");
                answer("SECOND_RETURN")
            }
            3 => {
                let result = tool_result(body, "chain");
                    assert!(result.contains("FIRST_RETURN") && result.contains("SECOND_RETURN"));
                answer("MAIN_RETURN")
            }
            _=>panic!("finish must not request an extra model decision"),
        }).await;
        let mut h=Harness::new().await;h.state=agent_service::tests::test_state(&h.root,address);
        h.user("Run the functions in order").await;let mut window=h.w.clone();
        let outcome =run_loop(&h.session(),&mut window, 6).await;
        server.abort();assert!(matches!(outcome.unwrap(), LoopResult::Answer));
        assert_eq!(seen.lock().unwrap().len(), 4);
        let conn = agent_service::open_db(&h.root).unwrap();
        let nodes = tree::nodes(&conn, "t").unwrap();
        assert!(
            nodes
                .iter()
                .filter(|n| n.kind == "finish")
                .all(|n| n.status == "done")
        );
        assert_eq!(
            nodes
                .iter()
                .find(|n| n.metadata["role"] == "main")
                .unwrap()
                .result.as_ref()
                .unwrap()["result"],
            "MAIN_RETURN"
        );
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='task/completion_check'", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
    }

    #[tokio::test]
    async fn a_blocked_function_returns_its_result_without_a_completion_check() {
        let summary = "未完成：外部编辑冲突。请用户选择保留的稿件后再继续。";
        let (address, seen, server) = raw_model(move |n, _| match n {
            0 => call(
                "start",
                "create_task",
                json!({"goal":"Complete the document"}),
            ),
            1 => answer(summary),
            _ =>panic!("a blocked function returns without another AI call"),
        }).await;
        let mut h=Harness::new().await;h.state=agent_service::tests::test_state(&h.root,address);
        h.user("Complete the document").await;let mut window=h.w.clone();
        let outcome =run_loop(&h.session(),&mut window, 4).await;
        server.abort();assert!(matches!(outcome.unwrap(), LoopResult::Answer));
        assert_eq!(seen.lock().unwrap().len(), 2);
        let conn = agent_service::open_db(&h.root).unwrap();
        let nodes = tree::nodes(&conn, "t").unwrap();
        let task = nodes.iter().find(|n| n.kind == "task").unwrap();
        assert_eq!(task.result.as_ref().unwrap()["result"], summary);
        assert!(
            nodes
                .iter()
                .any(|n| n.kind == "finish" && n.status == "done")
        );
        let saved: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&saved).unwrap()["task_node_id"],
            tree::ROOT
        );
    }

    #[tokio::test]
    async fn interrupted_native_join_recovers_before_the_next_model_request() {
        let (address, seen, server) = model(|_, body| {
            let messages = body["messages"].as_array().unwrap();
            let result = messages
                .iter()
                .find(|m| {
                    m["role"] == "tool"
                        && m["content"]
                            .as_str()
                            .is_some_and(|s| s.contains("\"joined\":true"))
                })
                .expect("saved join must finish before asking the model what to do");
            let value: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
            assert_eq!(value["delegates"][0]["status"], "completed");
            answer("Joined the saved handle; no child action was replayed")
        })
        .await;
        let mut h = Harness::new().await;
        h.state = agent_service::tests::test_state(&h.root, address);
        h.start("Resume the interrupted parent function", "Join its fork")
            .await;
        h.w.loaded_tool_groups.insert("collaboration".into());
        let owner = h.w.task_node_id.clone().unwrap();
        let conn = crate::collaboration_jobs::db(&h.root).unwrap();
        conn.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,delivered,review,created_at) VALUES ('saved-child','t',1,'fork','child',?1,'completed',1,'{}',0)",[json!({"caller_task_node_id":owner}).to_string()]).unwrap();
        // Crash point: the native join call is saved, but has no result yet.
        h.tool_call("join_agents", json!({"agent_ids":["saved-child"]}));
        h.persist().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            crate::single_agent::run_task(
                h.state.clone(),
                "t".into(),
                "fake".into(),
                "Resume".into(),
                4,
                CancellationToken::new(),
                2,
                false,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the host recovers the saved join without a model planning/polling turn"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM agent_delegates WHERE task_id='t'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT status FROM agent_tasks WHERE id='t'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "completed"
        );
    }

    #[tokio::test]
    async fn parallel_frames_keep_identity_and_serial_returns_do_not_touch_a_sibling() {
        let h=Harness::new().await;
        let mut conn=crate::collaboration_jobs::db(&h.root).unwrap();
        let tx=conn.transaction().unwrap();let root=tree::get(&tx,"t",tree::ROOT).unwrap();
        let mut frames=Vec::new();
        for id in ["worker-a","worker-b"] {
            tx.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES (?1,'t',1,?1,?1,'{}','running',0)",[id]).unwrap();
            let mut task=tree::planned(&root,&json!({"title":id,"goal":id,"executor":"delegate"}),frames.len(),"task").unwrap();
            task.agent_id=Some(id.into());task.status="running".into();task.metadata=json!({"role":"delegate"});
            tree::emit(&tx,"t","task/node_started",&mut task,1).unwrap();
            let mut end=tree::planned(&task,&json!({"title":"Finish","goal":"End"}),tree::FINISH_ORDINAL,"finish").unwrap();
            tree::emit(&tx,"t","task/node_planned",&mut end,1).unwrap();
            frames.push(Window{task_node_id:Some(task.node_id),messages:vec![json!({"role":"system","content":DELEGATE_PROMPT})],..Default::default()});
        }
        tx.commit().unwrap();drop(conn);
        let mut a=h.session();a.agent_id=Some("worker-a".into());a.scope="delegate:worker-a".into();
        let mut b=h.session();b.agent_id=Some("worker-b".into());b.scope="delegate:worker-b".into();
        let mut wa=frames.remove(0);let mut wb=frames.remove(0);
        begin_round(&a,&mut wa,"request-a").await.unwrap();begin_round(&b,&mut wb,"request-b").await.unwrap();
        let a_step=wa.current_step_node_id.clone().unwrap();
        let b_step=wb.current_step_node_id.clone().unwrap();
        // Call IDs may collide across Agents and later requests. Persist the
        // original step on each job instead of choosing the latest matching call.
        for (session,step,job) in [(&a,&a_step,"plugin-a"),(&b,&b_step,"plugin-b")] {
            let mut scoped=session.clone();scoped.step_node_id=Some(step.clone());
            append(&scoped,"tool/call",json!({"callId":"call_1","name":"demo__render","arguments":"{}"}),false).await.unwrap();
            crate::plugin_jobs::record(&h.root,"t","demo__render",job,"call_1",Some(step)).unwrap();
        }
        let a_activity=round_activity(&a,&frame_task(&a,&wa),&[]).unwrap().unwrap();
        let b_activity=round_activity(&b,&frame_task(&b,&wb),&[]).unwrap().unwrap();
        assert_eq!(a_activity["activity"].as_array().unwrap().len(),1);
        assert_eq!(a_activity["activity"][0]["node_id"],a_step);
        assert_eq!(b_activity["activity"][0]["node_id"],b_step);
        assert!(round_activity(&a,&frame_task(&b,&wb),&[]).unwrap().is_none(),"a sibling's task never becomes the current activity overview");
        assert!(crate::session_history::tool_call_node(&agent_service::open_db(&h.root).unwrap(),"t","call_1").unwrap_err().to_string().contains("ASYNC_JOB_OWNER_AMBIGUOUS"));
        crate::visual_jobs::outstanding_calls(&h.root,"t").unwrap(); // initialize the queue
        let conn=agent_service::open_db(&h.root).unwrap();
        for (step,job) in [(&a_step,"vision-a"),(&b_step,"vision-b")] {
            conn.execute("INSERT INTO agent_visual_jobs(id,task_id,turn,status,call_id,arguments,sources,result,created_at,updated_at,owner_node_id) VALUES (?1,'t',1,'completed','call_1','{}','[]',?2,0,0,?3)",
                params![job,json!({"status":"completed","analysis":job}).to_string(),step]).unwrap();
        }
        drop(conn);
        let call=crate::model_stream::ToolCall{call_id:"repair-call".into(),name:"create_task".into(),arguments:json!({"goal":"Repair the pot","reason":"Broken prerequisite","briefing":"No power"}).to_string()};
        wa.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}));
        let child_scope=create_task(&a,&mut wa,&call,"request-a",1).await.unwrap();
        let mut child=a.clone();child.scope=child_scope;
        assert_eq!(child.delegate_id(),Some("worker-a"));assert!(!child.is_root());
        begin_round(&child,&mut wa,"request-a-child").await.unwrap();
        assert!(pending_plugin_jobs(&child,&wa).unwrap().is_empty(),"a serial child cannot collect its caller's plugin result");
        assert!(!has_visual_jobs(&child,&wa).unwrap(),"a serial child cannot collect its caller's vision result");
        let mut scoped=child.clone();scoped.step_node_id=wa.current_step_node_id.clone();
        let seq=append(&scoped,"tool/result",json!({"message":{"toolCallId":"child-read"},"meta":{"result":{"value":"A-only"}}}),false).await.unwrap();
        complete_round(&child,&mut wa).await.unwrap();
        let (scope,mut resumed)=finish(&child,&wa,&answer("Pot repaired")).await.unwrap().unwrap();
        assert_eq!(scope,"delegate:worker-a");assert!(resumed.messages.last().unwrap()["content"].as_str().unwrap().contains("Pot repaired"));
        let conn=agent_service::open_db(&h.root).unwrap();
        assert_eq!(tree::get(&conn,"t",&b_step).unwrap().status,"current");
        assert_eq!(tree::get(&conn,"t",wb.task_node_id.as_deref().unwrap()).unwrap().status,"running");
        assert_eq!(tree::active(&conn,"t").unwrap().unwrap().node_id,tree::ROOT);
        let owner:String=conn.query_row("SELECT node_id FROM agent_task_events WHERE task_id='t' AND seq=?1",[seq],|r|r.get(0)).unwrap();
        assert_ne!(owner,b_step);
        assert_eq!(tree::get(&conn,"t",&owner).unwrap().agent_id.as_deref(),Some("worker-a"));
        let sibling:String=conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope='delegate:worker-b'",[],|r|r.get(0)).unwrap();
        let sibling:Window=serde_json::from_str(&sibling).unwrap();
        assert_eq!(sibling.current_step_node_id.as_deref(),Some(b_step.as_str()));
        assert!(!json!(sibling.messages).to_string().contains("Pot repaired"));
        drop(conn);

        // A newer request reuses call_1 as well. The saved ownership must still
        // survive task switching, later requests and reopening the job tables.
        begin_round(&a,&mut resumed,"request-a-later").await.unwrap();
        let mut later=a.clone();later.step_node_id=resumed.current_step_node_id.clone();
        append(&later,"tool/call",json!({"callId":"call_1","name":"read_file","arguments":"{}"}),false).await.unwrap();
        assert_eq!(pending_plugin_jobs(&a,&resumed).unwrap().iter().map(|job|job.id.as_str()).collect::<Vec<_>>(),vec!["plugin-a"]);
        assert_eq!(pending_plugin_jobs(&b,&wb).unwrap().iter().map(|job|job.id.as_str()).collect::<Vec<_>>(),vec!["plugin-b"]);

        struct ReadyPlugin;
        impl crate::plugin_io::SessionIo for ReadyPlugin {
            fn execute<'a>(&'a self,_state:&'a AgentServiceState,_task:&'a str,_workspace:Arc<crate::tools::Workspace>,_name:&'a str,args:Value)->futures::future::BoxFuture<'a,Result<Value>> {
                Box::pin(async move {Ok(json!({"status":"succeeded","task_id":args["task_id"]}))})
            }
        }
        let plugin=format!("scoped-jobs-{}",agent_service::uuid_like());
        agent_service::open_db(&h.root).unwrap().execute("INSERT INTO plugin_sessions VALUES ('t',?1)",[&plugin]).unwrap();
        crate::plugin_io::register(&plugin,Arc::new(ReadyPlugin));
        assert!(deliver_plugin_jobs(&a,&mut resumed).await.unwrap());
        deliver_visual_jobs(&a,&mut resumed).await.unwrap();
        assert!(!has_visual_jobs(&a,&resumed).unwrap());
        assert!(has_visual_jobs(&b,&wb).unwrap(),"A must not consume B's pending result");
        let a_messages=json!(resumed.messages).to_string();
        assert!(a_messages.contains("plugin-a") && a_messages.contains("vision-a"));
        assert!(!a_messages.contains("plugin-b") && !a_messages.contains("vision-b"));
        assert!(deliver_plugin_jobs(&b,&mut wb).await.unwrap());
        deliver_visual_jobs(&b,&mut wb).await.unwrap();
        let b_messages=json!(wb.messages).to_string();
        assert!(b_messages.contains("plugin-b") && b_messages.contains("vision-b"));
        assert!(!b_messages.contains("plugin-a") && !b_messages.contains("vision-a"));
        assert!(!deliver_plugin_jobs(&a,&mut resumed).await.unwrap());
        assert!(!has_visual_jobs(&b,&wb).unwrap());
        let conn=agent_service::open_db(&h.root).unwrap();
        let owners:Vec<String>=conn.prepare("SELECT node_id FROM agent_task_events WHERE task_id='t' AND kind='subagent/event' AND json_extract(data,'$.event_type')='tool/async_result' ORDER BY seq").unwrap()
            .query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(owners,vec![a_step.clone(),a_step,b_step.clone(),b_step]);
    }

}
