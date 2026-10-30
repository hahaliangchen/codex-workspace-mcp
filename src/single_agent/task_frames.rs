use super::*;
use crate::task_tree::{self as tree, Node};

fn node_id<'a>(s: &'a Session<'_>) -> &'a str {
    s.task_node_id.as_deref().unwrap_or(tree::ROOT)
}

/// The task whose context this window holds; the conversation node when none.
pub(super) fn frame_task(s: &Session<'_>, w: &Window) -> String {
    w.task_node_id
        .clone()
        .unwrap_or_else(|| node_id(s).to_owned())
}

/// Main execution at the conversation level, before it created its task.
pub(super) fn no_task(s: &Session<'_>, w: &Window) -> bool {
    s.is_primary() && frame_task(s, w) == tree::ROOT
}

fn task_title(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(text);
    line.chars().take(120).collect()
}

fn required_text(args: &Value, field: &str) -> Result<String> {
    let text = args[field]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .with_context(|| format!("{field} must be a nonempty string"))?;
    ensure!(
        text.chars().count() <= 16000,
        "{field} exceeds 16000 characters"
    );
    Ok(text.to_owned())
}

fn with_seq(result: &Value, seq: i64) -> Value {
    let mut delivery = if result.is_object() {
        result.clone()
    } else {
        json!({"result":result})
    };
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

/// Tasks are goal/context boundaries; execution steps are created on request.
fn new_task(
    parent: &Node,
    args: &Value,
    ordinal: usize,
    role: &str,
    turn: usize,
) -> Result<(Node, Node)> {
    let goal = required_text(args, "goal")?;
    let mut inputs = json!({});
    if role == "temp" && args["program_task"].is_object() {
        inputs["program_task"] = args["program_task"].clone();
    }
    let mut task = tree::planned(
        parent,
        &json!({"title":task_title(&goal),"goal":goal,"reason":args["reason"],"inputs":inputs}),
        ordinal,
        "task",
    )?;
    task.agent_id = parent.agent_id.clone();
    task.briefing = args["briefing"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .to_owned();
    task.metadata = json!({"role":role,"turn":turn});
    if parent.metadata["parallel"] == true {
        task.metadata["parallel"] = json!(true);
        task.metadata["parallel_owner"] = parent.metadata["parallel_owner"].clone();
    }
    let finish = tree::planned(
        &task,
        &json!({"title":"Finish","goal":"Return this task result to the waiting caller or serial iterator; never call AI here"}),
        tree::FINISH_ORDINAL,
        "finish",
    )?;
    Ok((task, finish))
}

/// Record exactly one execution step for an AI request in this frame.
pub(super) async fn begin_round(s: &Session<'_>, w: &mut Window, request: &str) -> Result<()> {
    if no_task(s, w) {
        return Ok(());
    }
    complete_round(s, w).await?;
    let (task, owner, agent, turn, request, scope, mut frame) = (
        s.task.to_owned(),
        frame_task(s, w),
        s.delegate_id().map(str::to_owned),
        s.turn,
        request.to_owned(),
        s.scope.clone(),
        w.clone(),
    );
    *w = db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let parent = tree::get(&tx, &task, &owner)?;
        ensure!(
            tree::is_task(&parent) && parent.status == "running" && parent.agent_id == agent,
            "TASK_EXECUTION_FRAME_MISMATCH"
        );
        let ordinal = tree::children(&tx, &task, &owner)?
            .iter()
            .filter(|n| n.kind == "step")
            .count();
        let title = format!("Round {}", ordinal + 1);
        let mut node = tree::planned(
            &parent,
            &json!({"title":title,"goal":"One AI request, its response, tool calls and results"}),
            ordinal,
            "step",
        )?;
        node.agent_id = agent;
        node.status = "current".into();
        node.metadata = json!({"request_id":request,"scope":scope,"phase":"executing"});
        tree::emit(&tx, &task, "task/node_started", &mut node, turn)?;
        frame.current_step_node_id = Some(node.node_id);
        write_window(&tx, &task, &scope, &frame)?;
        tx.commit()?;
        Ok(frame)
    })
    .await?;
    Ok(())
}

/// Store references to the native exchange; do not invent a lossy step summary.
pub(super) async fn complete_round(s: &Session<'_>, w: &mut Window) -> Result<()> {
    let Some(id) = w.current_step_node_id.clone() else {
        return Ok(());
    };
    let agent = s.delegate_id().map(str::to_owned);
    let (task, turn, scope, mut frame) = (s.task.to_owned(), s.turn, s.scope.clone(), w.clone());
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

fn record_call(
    tx: &rusqlite::Transaction<'_>,
    task: &str,
    turn: usize,
    step: usize,
    request: &str,
    call: &crate::model_stream::ToolCall,
    node: &str,
) -> Result<i64> {
    record_event(
        tx,
        task,
        "tool/call",
        json!({"actor":"agent","turn":turn,"step":step,"task_node_id":node,"flowNodeId":request,
        "callId":call.call_id,"name":call.name,"arguments":call.arguments,
        "description":tool_description(&[tree::tool()],&call.name,&call.arguments)}),
        None,
    )
}

pub(super) fn tool_description(definitions: &[Value], name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    args["label"]
        .as_str()
        .filter(|label| !label.trim().is_empty())
        .or_else(|| {
            definitions
                .iter()
                .find(|tool| tool["function"]["name"] == name)
                .and_then(|tool| tool["function"]["description"].as_str())
        })
        .unwrap_or(name)
        .to_owned()
}

/// Walk the caller tasks and their steps, carrying descriptions and handles,
/// never the tool output bodies or a model-generated recap.

/// Rebuilt beside the next request, independent of compressible chat history.
/// This is an execution ledger, not a host decision about what to do next.


/// Copy returned metadata as-is; failure is never interpreted as "not applied".

fn record_event(
    tx: &rusqlite::Transaction<'_>,
    task: &str,
    kind: &str,
    mut data: Value,
    surface: Option<&str>,
) -> Result<i64> {
    if let Some(owner) = data["task_node_id"].as_str()
        && let Some(step) = tree::current_step(tx, task, owner)?
    {
        data["task_node_id"] = json!(step.node_id);
    }
    if let Some(agent) = data["task_node_id"]
        .as_str()
        .map(|id| tree::get(tx, task, id))
        .transpose()?
        .and_then(|n| n.agent_id)
    {
        data["actor"] = json!("worker");
        let local:i64=tx.query_row("UPDATE agent_delegates SET event_count=event_count+1 WHERE task_id=?1 AND id=?2 RETURNING event_count-1",params![task,agent],|r|r.get(0))?;
        let data = json!({"agent_id":agent,"agent_seq":local,"turn":data["turn"],"task_node_id":data["task_node_id"],"event_type":kind,"event":data});
        return agent_service::append_event_tx(tx, task, "subagent/event", &data, surface);
    }
    agent_service::append_event_tx(tx, task, kind, &data, surface)
}

#[allow(clippy::too_many_arguments)]
fn record_result(
    tx: &rusqlite::Transaction<'_>,
    task: &str,
    turn: usize,
    step: &Value,
    request: &Value,
    call_id: &str,
    node: &str,
    result: &Value,
) -> Result<i64> {
    record_event(
        tx,
        task,
        "tool/result",
        json!({"actor":"agent","turn":turn,"step":step,"task_node_id":node,"flowNodeId":request,
        "message":{"role":"tool","toolCallId":call_id,"isError":result.get("error").is_some(),"content":[{"type":"text","text":result.to_string()}]},
        "meta":{"result":result}}),
        Some("append"),
    )
}

/// Establish the main scope, or consume the exact pending compiled function call.
/// The latter is private to the Runtime; its saved frame is the continuation.
/// Returns the active scope without creating a second task-list scheduler.
pub(super) async fn create_task(
    s: &Session<'_>,
    w: &mut Window,
    call: &crate::model_stream::ToolCall,
    request_id: &str,
    step: usize,
) -> Result<String> {
    let parent = frame_task(s, w);
    ensure!(parent != tree::ROOT, "the host opens the main task for each user request");
    create_function_task(s, w, &parent, call, request_id, step).await
}

/// Each user request is a main task. The host opens it with the request as
/// its goal before the first model request; the AI never creates it.
pub(super) async fn open_main_task(s: &Session<'_>, w: &mut Window, request: &str) -> Result<()> {
    let goal: String = request.trim().chars().take(16000).collect();
    let goal = if goal.is_empty() { "User request".to_owned() } else { goal };
    let (task, turn, scope, mut frame) = (s.task.to_owned(), s.turn, s.scope.clone(), w.clone());
    *w = db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut root = tree::get(&tx, &task, tree::ROOT)?;
        ensure!(root.status == "running", "TASK_STACK_FRAME_MISMATCH: the conversation is not at its top level");
        let ordinal = tree::children(&tx, &task, tree::ROOT)?.len();
        let (mut node, mut finish) = new_task(&root, &json!({"goal":goal}), ordinal, "main", turn)?;
        tree::emit(&tx, &task, "task/node_planned", &mut node, turn)?;
        tree::emit(&tx, &task, "task/node_planned", &mut finish, turn)?;
        root.status = "suspended".into();
        tree::emit(&tx, &task, "task/node_suspended", &mut root, turn)?;
        node.status = "running".into();
        tree::emit(&tx, &task, "task/node_started", &mut node, turn)?;
        frame.task_node_id = Some(node.node_id);
        frame.current_step_node_id = None;
        write_window(&tx, &task, &scope, &frame)?;
        tx.commit()?;
        Ok(frame)
    })
    .await?;
    Ok(())
}

/// Create an independent window for one asynchronous virtual function.
pub(super) async fn start_parallel_function(
    s: &Session<'_>, parent_frame: &Window, action: &crate::agent_program::Action, step: usize,
) -> Result<(String, Window)> {
    let crate::agent_program::runtime::ActionKind::Call { name, args } = &action.kind else {
        anyhow::bail!("parallel function requires an async virtual call");
    };
    let runtime = parent_frame.program.as_ref().context("parallel caller has no program")?;
    let snapshot = runtime.async_scope(&action.id).context("parallel call has no saved scope")?.clone();
    let function = super::program_flow::function_task(&runtime.program, name, args)?;
    let arguments = json!({"goal":name,"briefing":function["description"],"program_task":function});
    let parent_id = frame_task(s, parent_frame);
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let parent = tree::get(&conn, s.task, &parent_id)?;
    ensure!(parent.status == "running", "parallel caller is not running");
    let depth = tree::path(&conn, s.task, &parent_id)?.iter().filter(|node| tree::is_temp(node)).count();
    ensure!(depth < tree::MAX_TEMP_TASKS, "TASK_DEPTH_LIMIT");
    let ordinal = tree::children(&conn, s.task, &parent_id)?.len();
    let (mut node, mut finish) = new_task(&parent, &arguments, ordinal, "temp", s.turn)?;
    node.metadata["parallel"] = json!(true);
    node.metadata["parallel_owner"] = json!(node.node_id);
    node.metadata["parallel_call_id"] = json!(action.id);
    finish.metadata = json!({"parallel":{"task":parent_id,"scope":s.scope,"call_id":action.id,"step":step,"flowNodeId":action.id}});
    let mut inherited = parent_frame.clone();
    inherited.program = None;
    inherited.scope_values.extend(snapshot);
    let mut child = temporary_frame(s, &inherited, &node)?;
    child.parallel_root = Some(node.node_id.clone());
    let child_scope = tree::frame_scope(&node);
    let call = crate::model_stream::ToolCall { call_id:action.id.clone(), name:"create_task".into(), arguments:arguments.to_string() };
    let (task, turn, scope, child_copy, action_id, parent_id) = (s.task.to_owned(), s.turn, child_scope.clone(), child.clone(), action.id.clone(), parent_id);
    db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut node = node; let mut finish = finish;
        record_call(&tx, &task, turn, step, &action_id, &call, &parent_id)?;
        tree::emit(&tx, &task, "task/node_planned", &mut node, turn)?;
        tree::emit(&tx, &task, "task/node_planned", &mut finish, turn)?;
        node.status = "running".into();
        tree::emit(&tx, &task, "task/node_started", &mut node, turn)?;
        write_window(&tx, &task, &scope, &child_copy)?;
        tx.commit()?;
        Ok(())
    }).await?;
    Ok((child_scope, child))
}

pub(super) async fn interrupt_parallel_function(s: &Session<'_>, call_id: &str) -> Result<()> {
    let task = s.task.to_owned(); let call_id = call_id.to_owned(); let turn = s.turn;
    db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let nodes = tree::nodes(&tx, &task)?;
        if let Some(root) = nodes.iter().find(|node| node.metadata["parallel_call_id"] == call_id) {
            let root_id = root.node_id.clone();
            for mut node in nodes {
                if matches!(node.status.as_str(), "running" | "suspended" | "current")
                    && tree::path(&tx, &task, &node.node_id)?.iter().any(|ancestor| ancestor.node_id == root_id)
                {
                    node.status = "interrupted".into();
                    node.result = Some(json!({"message":"execution interrupted; effects were not replayed"}));
                    tree::emit(&tx, &task, "task/node_returned", &mut node, turn)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }).await
}

/// Enter exactly one compiled function. The suspended Runtime is the only
/// continuation; there is no parallel task-list/chain scheduler.
async fn create_function_task(
    s: &Session<'_>,
    w: &mut Window,
    parent_id: &str,
    call: &crate::model_stream::ToolCall,
    request_id: &str,
    step: usize,
) -> Result<String> {
    let prepared = (|| -> Result<(Node, Node)> {
        let args: Value = serde_json::from_str(&call.arguments)?;
        let pending = w
            .program
            .as_ref()
            .and_then(|p| p.pending.as_ref())
            .context("function call requires a suspended flow program")?;
        ensure!(
            pending.id == call.call_id && args["program_task"].is_object(),
            "function calls are created only by the flow runtime"
        );
        let conn = agent_service::open_db(s.state.workspace.root())?;
        let parent = tree::get(&conn, s.task, parent_id)?;
        ensure!(
            tree::is_task(&parent)
                && parent.status == "running"
                && parent.agent_id.as_deref() == s.delegate_id(),
            "TASK_STACK_FRAME_MISMATCH"
        );
        let depth = tree::path(&conn, s.task, &parent.node_id)?
            .iter()
            .filter(|n| tree::is_temp(n))
            .count();
        ensure!(depth < tree::MAX_TEMP_TASKS, "TASK_DEPTH_LIMIT");
        let owner =
            tree::current_step(&conn, s.task, &parent.node_id)?.unwrap_or_else(|| parent.clone());
        let (node, mut finish) = new_task(
            &owner,
            &args,
            tree::children(&conn, s.task, &owner.node_id)?.len(),
            "temp",
            s.turn,
        )?;
        finish.metadata = json!({"iterator":{"task":parent.node_id,"scope":s.scope,"call_id":call.call_id,"step":step,"flowNodeId":request_id}});
        Ok((node, finish))
    })();
    let prepared = match prepared {
        Ok((node, finish)) => temporary_frame(s, w, &node).map(|frame| (node, finish, frame)),
        Err(error) => Err(error),
    };
    let (task, turn, scope, mut parent_frame, call, request_id, parent_id) = (
        s.task.to_owned(),
        s.turn,
        s.scope.clone(),
        w.clone(),
        call.clone(),
        request_id.to_owned(),
        parent_id.to_owned(),
    );
    let (scope, frame) = db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        record_call(&tx, &task, turn, step, &request_id, &call, &parent_id)?;
        let switched = match prepared {
            Ok((mut node, mut finish, frame)) => {
                tree::emit(&tx, &task, "task/node_planned", &mut node, turn)?;
                tree::emit(&tx, &task, "task/node_planned", &mut finish, turn)?;
                let mut parent = tree::get(&tx, &task, &parent_id)?;
                parent.status = "suspended".into();
                tree::emit(&tx, &task, "task/node_suspended", &mut parent, turn)?;
                node.status = "running".into();
                tree::emit(&tx, &task, "task/node_started", &mut node, turn)?;
                write_window(&tx, &task, &scope, &parent_frame)?;
                let child_scope = tree::frame_scope(&node);
                write_window(&tx, &task, &child_scope, &frame)?;
                (child_scope, frame)
            }
            Err(error) => {
                let result = json!({"error":format!("{error:#}")});
                let seq = record_result(&tx, &task, turn, &json!(step), &json!(request_id), &call.call_id, &parent_id, &result)?;
                parent_frame.messages.push(sourced(json!({"role":"tool","tool_call_id":call.call_id,"content":with_seq(&result,seq).to_string()}), &task, seq));
                write_window(&tx, &task, &scope, &parent_frame)?;
                (scope, parent_frame)
            }
        };
        tx.commit()?;
        Ok(switched)
    }).await?;
    *w = frame;
    Ok(scope)
}

/// Function arguments are values, never reconstructed historical tool packets.
fn temporary_frame(s: &Session<'_>, parent: &Window, node: &Node) -> Result<Window> {
    let mut child = inherited_frame(s, parent, node);
    let task = node.inputs["program_task"]
        .as_object()
        .context("compiled function descriptor required")?;
    let task = Value::Object(task.clone());
    let input = super::program_flow::call_view(&task)?;
    let virtual_function = task["role"] != "body";
    if virtual_function {
        // A virtual function's context is its signature, contract and actual
        // arguments: nothing of its caller's conversation or scope comes along.
        let mut messages = vec![json!({"role":"system","content":s.prompt()})];
        inherit_material(parent, &mut messages);
        child.messages = messages;
        child.scope_values.clear();
    }
    child.messages.push(json!({"role":"user","name":"program_task_input","content":input.to_string(),"_frame_input":true}));
    child.program = super::program_flow::task_runtime(&task)?;
    child.program_task = Some(task);

    let mut session = s.clone();
    session.scope = tree::frame_scope(node);
    fit_input_bodies(&session, &mut child)?;
    Ok(child)
}

fn inherited_frame(s: &Session<'_>, parent: &Window, node: &Node) -> Window {
    let mut messages = vec![json!({"role":"system","content":s.prompt()})];
    inherit_material(parent, &mut messages);
    messages.extend(
        parent
            .messages
            .iter()
            .filter(|m| is_user_original(m))
            .cloned(),
    );
    Window {
        scope_values: super::function_scope::values(parent),
        messages,
        task_node_id: Some(node.node_id.clone()),
        active_skills: parent.active_skills.clone(),
        active_skill_scopes: parent.active_skill_scopes.clone(),
        token_calibration: parent.token_calibration.clone(),
        parallel_root: parent.parallel_root.clone(),
        ..Default::default()
    }
}

/// What a returning task saved, as handles: the latest result per invocation
/// in its scope, without the materials it was given or inherited.
fn returned_assets(s: &Session<'_>, w: &Window, node: &str) -> Value {
    let given = w
        .messages
        .iter()
        .filter(|m| m["_frame_input"] == true || m["_pinned"] == true)
        .filter_map(|m| m[crate::context_assets::REFERENCE]["asset_id"].as_str())
        .collect::<BTreeSet<_>>();
    let Ok(index) =
        crate::context_assets::frame_index(s.state.workspace.root(), s.task, node, &w.messages)
    else {
        return json!([]);
    };
    json!(
        index["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["asset_id"].as_str().is_some_and(|id| !given.contains(id)))
            .map(|a| {
                let mut a = a.clone();
                a.as_object_mut().unwrap().remove("in_context");
                a
            })
            .collect::<Vec<_>>()
    )
}

/// A no-tool response reaches the finish node of
/// the task in view, and that reply is the task's summary. A temporary task's
/// finish resumes its suspended Runtime: the internal function call returns
/// the summary and the waiting task continues. A main task's summary is the
/// answer to the user. Returns the context to continue in; None ends the turn.
pub(super) async fn finish(
    s: &Session<'_>,
    w: &mut Window,
    message: &Value,
) -> Result<Option<(String, Window)>> {
    let id = frame_task(s, w);
    // A saved proposal awaiting the user pauses the task instead of finishing it.
    if id == tree::ROOT || (s.is_primary() && approval_boundary(s)?.is_some()) {
        return Ok(None);
    }
    let agent = s.delegate_id().map(str::to_owned);
    let saved = returned_assets(s, w, &id);
    let text = message["content"].as_str().unwrap_or("").to_owned();
    let (task, turn, old_scope, frame) = (s.task.to_owned(), s.turn, s.scope.clone(), w.clone());
    let resumed=db(s.state.workspace.root(), move |conn| {
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
        let parallel = finish.as_ref().map(|f| f.metadata["parallel"].clone()).unwrap_or(Value::Null);
        node.status = "done".into();
        node.result = Some(summary.clone());
        let returned = tree::emit(&tx, &task, "task/node_returned", &mut node, turn)?;
        if let Some(mut finish) = finish {
            finish.status = "done".into();
            finish.result = Some(summary);
            tree::emit(&tx, &task, "task/node_returned", &mut finish, turn)?;
        }
        if parallel.is_object() {
            let parent = parallel["task"].as_str().context("parallel caller missing")?;
            let call_id = parallel["call_id"].as_str().context("parallel call id missing")?;
            let answer = json!({"task":node.title,"node_id":node.node_id,"return_event_seq":returned,"result":text,"assets":saved});
            record_result(&tx, &task, turn, &parallel["step"], &parallel["flowNodeId"], call_id, parent, &answer)?;
            let mut frame = frame;
            super::function_scope::release_finished(&mut frame);
            write_window(&tx, &task, &old_scope, &frame)?;
            tx.commit()?;
            return Ok(None);
        }
        if iterator.is_null() && node.agent_id.is_some() {
            let mut frame=frame;
            super::function_scope::release_finished(&mut frame);
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
            super::function_scope::release_finished(&mut frame);
            write_window(&tx, &task, &old_scope, &frame)?;
            tx.commit()?;
            return Ok(None);
        }
        let mut parent = tree::get(&tx, &task, iterator["task"].as_str().context("finish node holds no iterator")?)?;
        ensure!(parent.agent_id==agent,"TASK_CALLER_AGENT_MISMATCH");
        let scope = iterator["scope"].as_str().context("iterator scope missing")?.to_owned();
        let call_id = iterator["call_id"].as_str().context("iterator call missing")?.to_owned();
        let raw: String = tx
            .query_row("SELECT state FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2", params![task, scope], |r| r.get(0))
            .context("waiting task frame missing")?;
        let mut resumed: Window = serde_json::from_str(&raw)?;
        ensure!(resumed.task_node_id.as_deref()==Some(parent.node_id.as_str()),"TASK_CALLER_FRAME_MISMATCH");
        let answer = json!({"task":node.title,"node_id":node.node_id,"return_event_seq":returned,"result":text,"assets":saved});
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
    .await?;
    if resumed.is_none() {
        super::function_scope::release_finished(w);
    }
    Ok(resumed)
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
        return parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    raw.to_owned()
}

/// One memory per completed main task, including tasks resumed across turns.
/// Turns without a task remain question/answer memories. Details stay in history.
pub(super) fn memory_index(
    conn: &rusqlite::Connection,
    task: &str,
    turn: usize,
) -> Result<Option<Value>> {
    let tasks = tree::children(conn, task, tree::ROOT)?
        .into_iter()
        .filter(|node| {
            node.kind == "task"
                && !tree::is_temp(node)
                && node.metadata["turn"]
                    .as_u64()
                    .is_some_and(|started| started < turn as u64)
        })
        .collect::<Vec<_>>();
    let task_turns = tasks
        .iter()
        .filter_map(|node| node.metadata["turn"].as_i64())
        .collect::<BTreeSet<_>>();
    let task_ranges = tasks
        .iter()
        .filter_map(|node| {
            node.first_seq
                .map(|start| (start, node.returned_seq.unwrap_or(i64::MAX)))
        })
        .collect::<BTreeMap<_, _>>();
    let rows = conn
        .prepare("SELECT seq,kind,json_extract(data,'$.turn'),json_extract(data,'$.message.content'),json_type(data,'$.message.tool_calls') FROM agent_task_events WHERE task_id=?1 AND kind IN ('user/message','assistant/message') AND json_extract(data,'$.turn')<?2 ORDER BY seq")?
        .query_map(params![task, turn as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut turns = BTreeMap::<i64, (i64, Value)>::new();
    let mut task_owned_turns = task_turns;
    for (seq, kind, number, content, calls) in rows {
        let (Some(number), Some(content)) = (number, content) else {
            continue;
        };
        if task_ranges
            .range(..=seq)
            .next_back()
            .is_some_and(|(_, end)| seq <= *end)
        {
            task_owned_turns.insert(number);
        }
        let text = plain_text(&content);
        if text.trim().is_empty() {
            continue;
        }
        let (last_seq, entry) = turns
            .entry(number)
            .or_insert_with(|| (seq, json!({"kind":"qa","turn":number})));
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
            node.metadata["turn"].as_i64(),
            node.returned_seq,
            node.result
                .as_ref()
                .and_then(|result| result["result"].as_str()),
        ) else {
            continue;
        };
        let mut entry = json!({"kind":"task","turn":number,
            "task":{"node_id":node.node_id,"goal":clip(&node.goal,160),"status":node.status},
            "answer":summary.trim().chars().take(200).collect::<String>()});
        if let Some((_, original)) = turns.get(&number) {
            entry["request"] = original["request"].clone();
        }
        entries.push((seq, entry));
    }
    entries.extend(
        turns
            .into_iter()
            .filter(|(number, (_, entry))| {
                !task_owned_turns.contains(number)
                    && entry.get("request").is_some()
                    && entry.get("answer").is_some()
            })
            .map(|(_, entry)| entry),
    );
    entries.sort_by_key(|(seq, _)| *seq);
    if entries.is_empty() {
        return Ok(None);
    }
    let omitted = entries.len().saturating_sub(30);
    let entries = entries
        .into_iter()
        .skip(omitted)
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>();
    Ok(Some(
        json!({"host_status":true,"not_a_user_request":true,"entries":entries,"entries_omitted":omitted,
        "instruction":"Index of completed main tasks and task-free questions, oldest completion first. Each task has one entry with its goal, final summary and node_id even if it spanned multiple turns. Temporary tasks are included in their main task, not separate entries. Working context is not loaded here. Read details with read_session_history using the task node_id and include_descendants=true, or turn for a question/answer entry."}),
    ))
}

/// Each user request at the conversation level starts a clean main context:
/// the memory index of earlier turns plus the skills still loaded. Earlier working records stay in history.
pub(super) async fn fresh_main_window(
    s: &Session<'_>,
    previous: Option<&Window>,
    memory: Option<Value>,
) -> Result<Window> {
    let mut window = Window {
        task_node_id: Some(tree::ROOT.into()),
        ..Default::default()
    };
    window
        .messages
        .push(json!({"role":"system","content":MAIN_PROMPT}));
    if let Some(index) = memory {
        window.messages.push(json!({"role":"user","name":"conversation_memory","content":index.to_string(),"_frame_input":true}));
    }
    let Some(previous) = previous else {
        return Ok(window);
    };
    window.active_skills = previous.active_skills.clone();
    window.active_skill_scopes = previous.active_skill_scopes.clone();
    window.token_calibration = previous.token_calibration.clone();
    for id in
        crate::context_assets::active_instruction_ids(&previous.messages, &previous.active_skills)
    {
        let loaded = previous.messages.iter().rev().find(|m| {
            m[crate::context_assets::REFERENCE]["asset_id"] == id
                && !matches!(
                    m["_asset_state"].as_str(),
                    Some("reference_only" | "not_delivered")
                )
        });
        let mut message = if let Some(message) = loaded {
            let mut message = message.clone();
            message["role"] = json!("user");
            message["name"] = json!("saved_asset");
            message.as_object_mut().unwrap().remove("tool_call_id");
            message
        } else {
            match load_saved_asset(s, &json!({"asset_id":id})).await {
                Ok(result) => crate::context_assets::tagged(
                    json!({"role":"user","name":"saved_asset","content":result.to_string()}),
                    &result["asset_reference"],
                ),
                Err(_) => continue,
            }
        };
        message["_pinned"] = json!(true);
        window.messages.push(message);
    }
    Ok(window)
}

fn fit_input_bodies(s: &Session<'_>, child: &mut Window) -> Result<()> {
    let definitions = tools(s, child);
    let mut bodies = Vec::new();
    for (index, message) in child.messages.iter_mut().enumerate() {
        if message["_frame_input"] != true
            || message["_pinned"] == true
            || crate::context_assets::complete_body(message).is_none()
        {
            continue;
        }
        let body = message.clone();
        let delivery: Value = serde_json::from_str(message["content"].as_str().unwrap_or(""))?;
        let total_chars = delivery["total_chars"].as_u64().unwrap_or_else(|| {
            delivery
                .get("original_result")
                .unwrap_or(&delivery)
                .to_string()
                .chars()
                .count() as u64
        });
        crate::context_assets::reference_only(message);
        let mut receipt: Value = serde_json::from_str(message["content"].as_str().unwrap())?;
        receipt["total_chars"] = json!(total_chars);
        receipt["delivered"] = json!(false);
        receipt["next_read"] = json!({"asset_id":message[crate::context_assets::REFERENCE]["asset_id"],"char_offset":0,"max_chars":PAGE_CHARS});
        receipt["instruction"] = json!(
            "The complete body did not fit alongside the other task inputs. Read explicit pages as needed; return only the result required by the caller."
        );
        message["content"] = json!(receipt.to_string());
        bodies.push((index, body));
    }
    let fits = |child: &Window| {
        child
            .input_tokens(s, &request(s, &child.messages, &definitions))
            .saturating_add(4096)
            < s.input_budget
    };
    ensure!(
        fits(child),
        "TASK_INPUTS_OVER_CAPACITY: current arguments and fixed task inputs exceed the model context budget; parent frame is preserved"
    );
    for (index, body) in bodies {
        let reference = std::mem::replace(&mut child.messages[index], body);
        if !fits(child) {
            child.messages[index] = reference;
        }
    }
    Ok(())
}


/// Point a frame at the node it now executes: refresh its path and node card.
fn inherit_material(parent: &Window, messages: &mut Vec<Value>) {
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
                && let Some(existing) = messages
                    .iter_mut()
                    .find(|m| m[crate::context_assets::REFERENCE]["asset_id"] == id)
            {
                let loaded = !matches!(
                    message["_asset_state"].as_str(),
                    Some("reference_only" | "not_delivered")
                );
                let existing_loaded = !matches!(
                    existing["_asset_state"].as_str(),
                    Some("reference_only" | "not_delivered")
                );
                if loaded || !existing_loaded {
                    *existing = message;
                }
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
                    messages: vec![json!({"role":"system","content":MAIN_PROMPT})],
                    ..Default::default()
                },
                scope: "main".into(),
            };
            harness.persist().await;
            harness
        }
        fn session(&self) -> Session<'_> {
            Session {
                agent_id: None,
                task_node_id: None,
                step_node_id: None,
                plugin: None,
                state: &self.state,
                task: "t",
                model: "fake",
                turn: 1,
                cancel: &self.cancel,
                ban_execution: false,
                scope: self.scope.clone(),
                parent: None,
                input_budget: INPUT_BUDGET,
            }
        }
        async fn persist(&mut self) {
            save(&self.session(), &self.w).await.unwrap();
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
    }

    type Requests = Arc<Mutex<Vec<Value>>>;

    /// A scripted model: `reply(n, body)` answers the n-th request.
    fn completion_request(body: &Value) -> bool {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| {
                m["role"] == "assistant"
                    || m["role"] == "tool"
                    || m["name"] == "task_completion_check"
                    || m["name"] == "user_interjection"
                    || (m["role"] == "user" && m.get("name").is_none())
            })
            .is_some_and(|m| m["name"] == "task_completion_check")
    }

    async fn model(
        reply: impl Fn(usize, &Value) -> Value + Send + Sync + 'static,
    ) -> (std::net::SocketAddr, Requests, tokio::task::JoinHandle<()>) {
        let index = std::sync::atomic::AtomicUsize::new(0);
        raw_model(move |_, body| {
            let messages = body["messages"].as_array().unwrap();
            if completion_request(body) {
                return answer(
                    messages
                        .iter()
                        .rev()
                        .find(|m| m["role"] == "assistant")
                        .unwrap()["content"]
                        .as_str()
                        .unwrap(),
                );
            }
            reply(
                index.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                body,
            )
        })
        .await
    }

    async fn raw_model(
        reply: impl Fn(usize, &Value) -> Value + Send + Sync + 'static,
    ) -> (std::net::SocketAddr, Requests, tokio::task::JoinHandle<()>) {
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



    #[test]
    fn inherited_skill_material_is_deduplicated_by_asset_not_delivery_event() {
        let skill = |seq: i64, state: &str| {
            json!({"role":"tool","tool_call_id":format!("call-{seq}"),
            "content":format!("skill delivery {seq}"),"source":{"seq":seq},"_asset_state":state,
            (crate::context_assets::REFERENCE):{"asset_id":"asset-skill","source_tool":"read_skill"}})
        };
        let parent = Window {
            messages: vec![
                skill(1, "loaded"),
                skill(2, "loaded"),
                skill(3, "reference_only"),
            ],
            ..Default::default()
        };
        let mut messages = vec![];
        inherit_material(&parent, &mut messages);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "skill delivery 2");
        assert_eq!(messages[0]["name"], "saved_asset");
        assert_eq!(messages[0]["_pinned"], true);
        assert!(messages[0].get("tool_call_id").is_none());
        // An explicitly declared asset and inherited skill are still one input.
        inherit_material(&parent, &mut messages);
        assert_eq!(messages.len(), 1);
    }



    #[tokio::test]
    async fn a_plain_answer_needs_no_task() {
        let (address, seen, server) = model(|n, _| match n {
            0 => answer("4"),
            _ => panic!("one answer ends the turn"),
        })
        .await;
        let mut h = Harness::new().await;
        h.state = agent_service::tests::test_state(&h.root, address);
        h.user("What is 2+2?").await;
        let mut window = h.w.clone();
        assert!(matches!(
            run_loop(&h.session(), &mut window, 3).await.unwrap(),
            LoopResult::Answer
        ));
        server.abort();
        assert_eq!(seen.lock().unwrap().len(), 1);
        let nodes = tree::nodes(&agent_service::open_db(&h.root).unwrap(), "t").unwrap();
        assert_eq!(nodes.len(), 1, "no task was created");
        assert_eq!(nodes[0].status, "running");
    }



    /// 烧水热菜 on the real loop. The main task's steps share its context; the
    /// broken pot is a temporary task under the switch-on step with its own
    /// context. Its summary is the result of the create_task call that started
    /// it, and the step continues.




    /// A turn that stops before the finish node leaves the task open; the next
    /// message continues it in the same context instead of starting over.
    #[tokio::test]
    async fn a_stopped_turn_resumes_its_task_in_the_same_context() {
        let (address, seen, server) = model(|n, body| {
            let text = body["messages"].to_string();
            match n {
                0 => call(
                    "start",
                    "create_task",
                    json!({"goal":"Draft the plan","briefing":"Read the brief"}),
                ),
                1 => call("board", "read_task_board", json!({})),
                2 => {
                    assert!(
                        text.contains("Draft the plan") && text.contains("Draft a plan"),
                        "the same task context continues"
                    );
                    assert!(
                        !body["messages"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|m| m["name"] == "conversation_memory"),
                        "not a fresh context"
                    );
                    answer(&"计划已完成".repeat(50))
                }
                3 => {
                    let memory = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|m| m["name"] == "conversation_memory")
                        .unwrap();
                    let index: Value =
                        serde_json::from_str(memory["content"].as_str().unwrap()).unwrap();
                    let entries = index["entries"].as_array().unwrap();
                    assert_eq!(
                        entries.len(),
                        1,
                        "a resumed task has one memory, not one per turn"
                    );
                    assert_eq!(entries[0]["request"], "Draft a plan");
                    assert_eq!(entries[0]["task"]["goal"], "Draft the plan");
                    assert_eq!(entries[0]["task"]["status"], "done");
                    assert_eq!(
                        entries[0]["answer"],
                        "计划已完成".repeat(40),
                        "the final summary is truncated to 200 characters"
                    );
                    assert!(!entries.iter().any(|entry| entry["request"] == "continue"));
                    answer(&"问答摘要".repeat(60))
                }
                4 => {
                    let memory = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|m| m["name"] == "conversation_memory")
                        .unwrap();
                    let index: Value =
                        serde_json::from_str(memory["content"].as_str().unwrap()).unwrap();
                    let entries = index["entries"].as_array().unwrap();
                    assert_eq!(entries.len(), 2, "a task-free answer keeps its own memory");
                    assert_eq!(entries[0]["kind"], "task");
                    assert_eq!(entries[1]["kind"], "qa");
                    assert_eq!(entries[1]["turn"], 3);
                    assert_eq!(entries[1]["request"], "A separate question");
                    assert_eq!(entries[1]["answer"], "问答摘要".repeat(50));
                    answer("Done")
                }
                _ => panic!("unexpected request {n}"),
            }
        })
        .await;
        let mut h = Harness::new().await;
        h.state = agent_service::tests::test_state(&h.root, address);
        run_task(
            h.state.clone(),
            "t".into(),
            "fake".into(),
            "Draft a plan".into(),
            2,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        let conn = agent_service::open_db(&h.root).unwrap();
        assert_eq!(
            tree::active(&conn, "t").unwrap().unwrap().kind,
            "task",
            "the task stays open"
        );
        run_task(
            h.state.clone(),
            "t".into(),
            "fake".into(),
            "continue".into(),
            4,
            CancellationToken::new(),
            2,
            false,
        )
        .await
        .unwrap();
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
            h.state.clone(),
            "t".into(),
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
            agent_service::append_event_tx(
                &tx,
                "t",
                "user/message",
                &json!({"turn":turn,
                "message":{"role":"user","content":format!("Question {turn}")}}),
                None,
            )
            .unwrap();
            agent_service::append_event_tx(
                &tx,
                "t",
                "assistant/message",
                &json!({"turn":turn,
                "message":{"role":"assistant","content":format!("Answer {turn}")}}),
                None,
            )
            .unwrap();
        }
        // A question without a final answer is not a completed memory.
        agent_service::append_event_tx(
            &tx,
            "t",
            "user/message",
            &json!({"turn":33,
            "message":{"role":"user","content":"Unanswered"}}),
            None,
        )
        .unwrap();
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
    async fn parallel_frames_keep_identity_and_serial_returns_do_not_touch_a_sibling() {
        let h = Harness::new().await;
        let mut conn = agent_service::open_db(&h.root).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_delegates(id TEXT PRIMARY KEY,task_id TEXT,turn INTEGER,call_id TEXT,name TEXT,specification TEXT,status TEXT,created_at INTEGER,event_count INTEGER NOT NULL DEFAULT 0);").unwrap();
        let tx = conn.transaction().unwrap();
        let root = tree::get(&tx, "t", tree::ROOT).unwrap();
        let mut frames = Vec::new();
        for id in ["worker-a", "worker-b"] {
            tx.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES (?1,'t',1,?1,?1,'{}','running',0)",[id]).unwrap();
            let mut task = tree::planned(
                &root,
                &json!({"title":id,"goal":id,"executor":"delegate"}),
                frames.len(),
                "task",
            )
            .unwrap();
            task.agent_id = Some(id.into());
            task.status = "running".into();
            task.metadata = json!({"role":"delegate"});
            tree::emit(&tx, "t", "task/node_started", &mut task, 1).unwrap();
            let mut end = tree::planned(
                &task,
                &json!({"title":"Finish","goal":"End"}),
                tree::FINISH_ORDINAL,
                "finish",
            )
            .unwrap();
            tree::emit(&tx, "t", "task/node_planned", &mut end, 1).unwrap();
            frames.push(Window {
                task_node_id: Some(task.node_id),
                messages: vec![json!({"role":"system","content":MAIN_PROMPT})],
                ..Default::default()
            });
        }
        tx.commit().unwrap();
        drop(conn);
        let mut a = h.session();
        a.agent_id = Some("worker-a".into());
        a.scope = "delegate:worker-a".into();
        let mut b = h.session();
        b.agent_id = Some("worker-b".into());
        b.scope = "delegate:worker-b".into();
        let mut wa = frames.remove(0);
        let mut wb = frames.remove(0);
        begin_round(&a, &mut wa, "request-a").await.unwrap();
        begin_round(&b, &mut wb, "request-b").await.unwrap();
        let a_step = wa.current_step_node_id.clone().unwrap();
        let b_step = wb.current_step_node_id.clone().unwrap();
        // Call IDs may collide across Agents and later requests. Persist the
        // original step on each job instead of choosing the latest matching call.
        for (session, step, job) in [(&a, &a_step, "plugin-a"), (&b, &b_step, "plugin-b")] {
            let mut scoped = session.clone();
            scoped.step_node_id = Some(step.clone());
            append(
                &scoped,
                "tool/call",
                json!({"callId":"call_1","name":"demo__render","arguments":"{}"}),
                false,
            )
            .await
            .unwrap();
            crate::plugin_jobs::record(&h.root, "t", "demo__render", job, "call_1", Some(step))
                .unwrap();
        }
        assert_eq!(pending_plugin_jobs(&a, &wa).unwrap().len(), 1);
        assert_eq!(pending_plugin_jobs(&b, &wb).unwrap().len(), 1);
        assert!(
            crate::session_history::tool_call_node(
                &agent_service::open_db(&h.root).unwrap(),
                "t",
                "call_1"
            )
            .unwrap_err()
            .to_string()
            .contains("ASYNC_JOB_OWNER_AMBIGUOUS")
        );
        crate::visual_jobs::outstanding_calls(&h.root, "t").unwrap(); // initialize the queue
        let conn = agent_service::open_db(&h.root).unwrap();
        for (step, job) in [(&a_step, "vision-a"), (&b_step, "vision-b")] {
            conn.execute("INSERT INTO agent_visual_jobs(id,task_id,turn,status,call_id,arguments,sources,result,created_at,updated_at,owner_node_id) VALUES (?1,'t',1,'completed','call_1','{}','[]',?2,0,0,?3)",
                params![job,json!({"status":"completed","analysis":job}).to_string(),step]).unwrap();
        }
        drop(conn);
        let program=crate::agent_program::compile("function main(){return repairScope();} /** @virtual */ function repairScope(){ // Resolve this scope's missing prerequisite and return its result.\n}").unwrap();
        let mut runtime =
            crate::agent_program::Runtime::task_program(program, "main", vec![]).unwrap();
        let crate::agent_program::State::Action(action) = runtime.next() else {
            panic!()
        };
        let call=crate::model_stream::ToolCall{call_id:action.id,name:"create_task".into(),arguments:json!({"goal":"repairScope","program_task":{"name":"repairScope","description":"Repair the local prerequisite","parameters":[],"arguments":[]}}).to_string()};
        wa.program = Some(runtime);
        wa.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}));
        let child_scope = create_task(&a, &mut wa, &call, "request-a", 1)
            .await
            .unwrap();
        let mut child = a.clone();
        child.scope = child_scope;
        assert_eq!(child.delegate_id(), Some("worker-a"));
        assert!(!child.is_primary());
        begin_round(&child, &mut wa, "request-a-child")
            .await
            .unwrap();
        assert!(
            pending_plugin_jobs(&child, &wa).unwrap().is_empty(),
            "a serial child cannot collect its caller's plugin result"
        );
        assert!(
            !has_visual_jobs(&child, &wa).unwrap(),
            "a serial child cannot collect its caller's vision result"
        );
        let mut scoped = child.clone();
        scoped.step_node_id = wa.current_step_node_id.clone();
        let seq = append(
            &scoped,
            "tool/result",
            json!({"message":{"toolCallId":"child-read"},"meta":{"result":{"value":"A-only"}}}),
            false,
        )
        .await
        .unwrap();
        complete_round(&child, &mut wa).await.unwrap();
        let (scope, mut resumed) = finish(&child, &mut wa, &answer("Prerequisite repaired"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scope, "delegate:worker-a");
        assert!(
            resumed.messages.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("Prerequisite repaired")
        );
        let conn = agent_service::open_db(&h.root).unwrap();
        assert_eq!(tree::get(&conn, "t", &b_step).unwrap().status, "current");
        assert_eq!(
            tree::get(&conn, "t", wb.task_node_id.as_deref().unwrap())
                .unwrap()
                .status,
            "running"
        );
        assert_eq!(
            tree::active(&conn, "t").unwrap().unwrap().node_id,
            tree::ROOT
        );
        let owner: String = conn
            .query_row(
                "SELECT node_id FROM agent_task_events WHERE task_id='t' AND seq=?1",
                [seq],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(owner, b_step);
        assert_eq!(
            tree::get(&conn, "t", &owner).unwrap().agent_id.as_deref(),
            Some("worker-a")
        );
        let sibling:String=conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope='delegate:worker-b'",[],|r|r.get(0)).unwrap();
        let sibling: Window = serde_json::from_str(&sibling).unwrap();
        assert_eq!(
            sibling.current_step_node_id.as_deref(),
            Some(b_step.as_str())
        );
        assert!(
            !json!(sibling.messages)
                .to_string()
                .contains("Prerequisite repaired")
        );
        drop(conn);

        // A newer request reuses call_1 as well. The saved ownership must still
        // survive task switching, later requests and reopening the job tables.
        begin_round(&a, &mut resumed, "request-a-later")
            .await
            .unwrap();
        let mut later = a.clone();
        later.step_node_id = resumed.current_step_node_id.clone();
        append(
            &later,
            "tool/call",
            json!({"callId":"call_1","name":"read_file","arguments":"{}"}),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            pending_plugin_jobs(&a, &resumed)
                .unwrap()
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            vec!["plugin-a"]
        );
        assert_eq!(
            pending_plugin_jobs(&b, &wb)
                .unwrap()
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            vec!["plugin-b"]
        );

        struct ReadyPlugin;
        impl crate::plugin_io::SessionIo for ReadyPlugin {
            fn execute<'a>(
                &'a self,
                _state: &'a AgentServiceState,
                _task: &'a str,
                _workspace: Arc<crate::tools::Workspace>,
                _name: &'a str,
                args: Value,
            ) -> futures::future::BoxFuture<'a, Result<Value>> {
                Box::pin(async move { Ok(json!({"status":"succeeded","task_id":args["task_id"]})) })
            }
        }
        let plugin = format!("scoped-jobs-{}", agent_service::uuid_like());
        agent_service::open_db(&h.root)
            .unwrap()
            .execute("INSERT INTO plugin_sessions VALUES ('t',?1)", [&plugin])
            .unwrap();
        crate::plugin_io::register(&plugin, Arc::new(ReadyPlugin));
        assert!(deliver_plugin_jobs(&a, &mut resumed).await.unwrap());
        deliver_visual_jobs(&a, &mut resumed).await.unwrap();
        assert!(!has_visual_jobs(&a, &resumed).unwrap());
        assert!(
            has_visual_jobs(&b, &wb).unwrap(),
            "A must not consume B's pending result"
        );
        let a_messages = json!(resumed.messages).to_string();
        assert!(a_messages.contains("plugin-a") && a_messages.contains("vision-a"));
        assert!(!a_messages.contains("plugin-b") && !a_messages.contains("vision-b"));
        assert!(deliver_plugin_jobs(&b, &mut wb).await.unwrap());
        deliver_visual_jobs(&b, &mut wb).await.unwrap();
        let b_messages = json!(wb.messages).to_string();
        assert!(b_messages.contains("plugin-b") && b_messages.contains("vision-b"));
        assert!(!b_messages.contains("plugin-a") && !b_messages.contains("vision-a"));
        assert!(!deliver_plugin_jobs(&a, &mut resumed).await.unwrap());
        assert!(!has_visual_jobs(&b, &wb).unwrap());
        let conn = agent_service::open_db(&h.root).unwrap();
        let owners:Vec<String>=conn.prepare("SELECT node_id FROM agent_task_events WHERE task_id='t' AND kind='subagent/event' AND json_extract(data,'$.event_type')='tool/async_result' ORDER BY seq").unwrap()
            .query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(owners, vec![a_step.clone(), a_step, b_step.clone(), b_step]);
    }
}
