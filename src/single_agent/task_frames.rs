use super::*;
use crate::task_tree::{self as tree, Node};

fn node_id<'a>(s: &'a Session<'_>) -> &'a str {
    s.scope.strip_prefix("node:").unwrap_or(tree::ROOT)
}

/// The no-tool response ends native execution. Record that existing end node;
/// this is a consequence of the loop ending, never a prerequisite for it.
pub(super) async fn record_loop_end(s:&Session<'_>,message:&Value)->Result<()> {
    if !s.is_root() || node_id(s)!=tree::ROOT || approval_boundary(s)?.is_some() {return Ok(())}
    let task=s.task.to_owned();let turn=s.turn;let message=message.clone();
    db(s.state.workspace.root(),move|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut root=tree::get(&tx,&task,tree::ROOT)?;
        close_step(&tx,&task,tree::ROOT,turn)?;
        let mut end=tree::ensure_end(&tx,&task,&root,turn)?;
        let result=json!({"status":"done","result":message["content"],"reason":"model_returned_without_tool_calls"});
        end.status="done".into();end.result=Some(result.clone());
        tree::emit(&tx,&task,"task/node_returned",&mut end,turn)?;
        root.status="done".into();root.result=Some(result);
        tree::emit(&tx,&task,"task/node_returned",&mut root,turn)?;
        tx.commit()?;Ok(())
    }).await
}

/// branch_task is a saga `yield call(...)`: the current task's iterator stops at
/// this call and is handed to the new task, kept on its end node. The call has
/// no result until that end node runs the iterator (finish_branch).
pub(super) async fn branch(s:&Session<'_>,w:&mut Window,call:&crate::model_stream::ToolCall,request_id:&str,step:usize)->Result<String> {
    let prepared=async {
        let args:Value=serde_json::from_str(&call.arguments).context("invalid tool arguments JSON")?;
        let conn=agent_service::open_db(s.state.workspace.root())?;
        let parent=tree::active(&conn,s.task)?.context("no running task")?;
        ensure!(parent.node_id==node_id(s),"TASK_STACK_FRAME_MISMATCH");
        // The new task hangs under the step that decided it.
        let owner=tree::nodes(&conn,s.task)?.into_iter().find(|n|n.parent_id.as_deref()==Some(&parent.node_id)&&n.kind=="step"&&n.status=="current").unwrap_or_else(||parent.clone());
        let mut child=tree::planned(&owner,&json!({"title":args["title"],"goal":args["goal"],"done_when":args["goal"],"reason":args["reason"]}),tree::nodes(&conn,s.task)?.len(),"branch")?;
        child.briefing=args["briefing"].as_str().unwrap_or(&child.goal).to_owned();
        drop(conn);
        let frame=child_material(s,w,&child).await?;
        Ok::<_,anyhow::Error>((child,frame))
    }.await;
    let (task,turn,scope,mut parent_frame,call)=(s.task.to_owned(),s.turn,s.scope.clone(),w.clone(),call.clone());
    let request_id=request_id.to_owned();
    let (scope,frame)=db(s.state.workspace.root(),move|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current=tree::active(&tx,&task)?.context("no running task")?;
        agent_service::append_event_tx(&tx,&task,"tool/call",&json!({"actor":"agent","turn":turn,"step":step,"task_node_id":current.node_id,"flowNodeId":request_id,"callId":call.call_id,"name":call.name,"arguments":call.arguments}),None)?;
        let switched=match prepared {
            Ok((mut child,frame))=>{
                tree::emit(&tx,&task,"task/node_planned",&mut child,turn)?;
                let mut end=tree::ensure_end(&tx,&task,&child,turn)?;
                end.metadata=json!({"iterator":{"task":current.node_id,"scope":scope,"call_id":call.call_id,"step":step,"flowNodeId":request_id}});
                tree::emit(&tx,&task,"task/node_revised",&mut end,turn)?;
                let mut parent=current;
                parent.status="suspended".into();tree::emit(&tx,&task,"task/node_suspended",&mut parent,turn)?;
                child.status="running".into();tree::emit(&tx,&task,"task/node_started",&mut child,turn)?;
                write_window(&tx,&task,&scope,&parent_frame)?;
                let child_scope=tree::scope(&child.node_id);
                write_window(&tx,&task,&child_scope,&frame)?;
                (child_scope,frame)
            }
            Err(error)=>{
                // Not started: the call returns its error right away.
                let result=json!({"error":format!("{error:#}")});
                let seq=agent_service::append_event_tx(&tx,&task,"tool/result",&json!({"actor":"agent","turn":turn,"step":step,"flowNodeId":request_id,
                    "message":{"role":"tool","toolCallId":call.call_id,"isError":true,"content":[{"type":"text","text":result.to_string()}]},"meta":{"result":result}}),Some("append"))?;
                parent_frame.messages.push(sourced(json!({"role":"tool","tool_call_id":call.call_id,"content":json!({"event_seq":seq,"result":result}).to_string()}),&task,seq));
                write_window(&tx,&task,&scope,&parent_frame)?;
                (scope,parent_frame)
            }
        };
        tx.commit()?;Ok(switched)
    }).await?;
    *w=frame;
    Ok(scope)
}

/// The loop of a branched task ended (no tool calls): it reaches its end node,
/// which runs the iterator it was given. The answer becomes the result of the
/// interrupted branch_task call, and that task continues from there. The
/// branch's working context is dropped.
pub(super) async fn finish_branch(s:&Session<'_>,message:&Value)->Result<Option<(String,Window)>> {
    if !s.is_root() {return Ok(None)}
    let conn=agent_service::open_db(s.state.workspace.root())?;
    let Some(active)=tree::active(&conn,s.task)? else {return Ok(None)};
    if active.kind!="branch" {return Ok(None)}
    drop(conn);
    let (task,turn,old_scope,text)=(s.task.to_owned(),s.turn,s.scope.clone(),message["content"].as_str().unwrap_or("").to_owned());
    db(s.state.workspace.root(),move|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut branch=tree::get(&tx,&task,&active.node_id)?;
        close_step(&tx,&task,&branch.node_id,turn)?;
        let result=json!({"status":"done","result":text});
        branch.status="done".into();branch.result=Some(result.clone());
        let seq=tree::emit(&tx,&task,"task/node_returned",&mut branch,turn)?;
        let mut end=tree::ensure_end(&tx,&task,&branch,turn)?;
        end.status="done".into();end.result=Some(result);
        tree::emit(&tx,&task,"task/node_returned",&mut end,turn)?;
        let iterator=end.metadata["iterator"].clone();
        let mut parent=tree::get(&tx,&task,iterator["task"].as_str().context("end node holds no iterator")?)?;
        let scope=iterator["scope"].as_str().context("iterator scope missing")?.to_owned();
        let call_id=iterator["call_id"].as_str().context("iterator call missing")?.to_owned();
        let raw:String=tx.query_row("SELECT state FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2",params![task,scope],|r|r.get(0)).context("waiting task frame missing")?;
        let mut frame:Window=serde_json::from_str(&raw)?;
        // The interrupted call returns the branch's answer.
        let answer=json!({"task":branch.title,"node_id":branch.node_id,"return_event_seq":seq,"result":text});
        let returned=agent_service::append_event_tx(&tx,&task,"tool/result",&json!({"actor":"agent","turn":turn,"step":iterator["step"],"task_node_id":parent.node_id,"flowNodeId":iterator["flowNodeId"],
            "message":{"role":"tool","toolCallId":call_id,"isError":false,"content":[{"type":"text","text":answer.to_string()}]},"meta":{"result":answer}}),Some("append"))?;
        frame.messages.push(sourced(json!({"role":"tool","tool_call_id":call_id,"content":json!({"event_seq":returned,"result":answer}).to_string()}),&task,returned));
        parent.status="running".into();tree::emit(&tx,&task,"task/node_resumed",&mut parent,turn)?;
        write_window(&tx,&task,&scope,&frame)?;
        tx.execute("DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2",params![task,old_scope])?;
        tx.commit()?;Ok(Some((scope,frame)))
    }).await
}

fn step_title(message:&Value,calls:&[crate::model_stream::ToolCall])->String {
    let text=message["content"].as_str().unwrap_or("").trim();
    let title=if text.is_empty() {calls.iter().map(|c|c.name.as_str()).collect::<Vec<_>>().join(", ")} else {text.lines().next().unwrap_or("").to_owned()};
    title.chars().take(120).collect()
}

/// The current step of a task is done once the iteration moves on.
fn close_step(tx:&rusqlite::Transaction<'_>,task:&str,owner:&str,turn:usize)->Result<()> {
    for mut step in tree::nodes(tx,task)?.into_iter().filter(|n|n.parent_id.as_deref()==Some(owner)&&n.kind=="step"&&n.status=="current") {
        step.status="done".into();tree::emit(tx,task,"task/node_returned",&mut step,turn)?;
    }
    Ok(())
}

/// Each AI response with tool calls is the task's next step: the host adds it
/// to the current task and the iteration executes it.
pub(super) async fn step(s:&Session<'_>,message:&Value,calls:&[crate::model_stream::ToolCall])->Result<()> {
    if s.mode!=Mode::Main || !s.is_root() {return Ok(())}
    let (task,turn,title)=(s.task.to_owned(),s.turn,step_title(message,calls));
    db(s.state.workspace.root(),move|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(owner)=tree::active(&tx,&task)? else {return Ok(())};
        close_step(&tx,&task,&owner.node_id,turn)?;
        let ordinal=tree::nodes(&tx,&task)?.iter().filter(|n|n.parent_id.as_deref()==Some(&owner.node_id)).count();
        let mut step=tree::planned(&owner,&json!({"title":title,"goal":title,"done_when":title}),ordinal,"step")?;
        step.status="current".into();
        tree::emit(&tx,&task,"task/node_started",&mut step,turn)?;
        tx.commit()?;Ok(())
    }).await
}

pub(super) async fn notices(s: &Session<'_>, w: &mut Window) -> Result<()> {
    if s.mode != Mode::Main {
        return Ok(());
    }
    let conn = agent_service::open_db(s.state.workspace.root())?;
    if tree::get(&conn, s.task, tree::ROOT).is_err() {
        return Ok(());
    }
    let mut changed = false;
    let id = w.task_node_id.as_deref().unwrap_or_else(|| node_id(s));
    // Child material is prepared before the start transaction commits.
    // Refresh its snapshot before delivery, including frames saved by older builds.
    if let Some(message) = w.messages.iter_mut().find(|m| m["name"] == "task_node") {
        let content = json!(serde_json::to_string(&tree::get(&conn, s.task, id)?)?);
        if message["content"] != content {
            message["content"] = content;
            changed = true;
        }
    }
    for instruction in tree::instructions(&conn, s.task, id)? {
        let id = instruction["id"].as_str().unwrap().to_owned();
        let version = instruction["scope_node"].to_string();
        if w.instruction_versions.get(&id) != Some(&version) {
            w.messages.push(
                json!({"role":"user","name":"task_instruction","content":instruction.to_string()}),
            );
            w.instruction_versions.insert(id, version);
            changed = true;
        }
    }
    drop(conn);
    if changed {
        save(s, w).await?;
    }
    Ok(())
}

pub(super) async fn delegate_context(
    s: &Session<'_>,
    w: &Window,
    args: &Value,
) -> Result<(Value, Value)> {
    let mut conn = agent_service::open_db(s.state.workspace.root())?;
    let Some(parent) = tree::active(&conn, s.task)? else {
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
            "planned",
        )?;
        // emit reads the next seq before writing; a deferred transaction would
        // fail immediately with SQLITE_BUSY instead of waiting for the lock.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut recorded = node.clone();
        tree::emit(&tx, s.task, "task/node_planned", &mut recorded, s.turn)?;
        tx.commit()?;
        recorded
    };
    drop(conn);
    let frame = child_material(s, w, &node).await?;
    let mut shared = serde_json::to_value(frame)?;
    shared["task_node"] = serde_json::to_value(&node)?;
    let mut assignment = args.clone();
    assignment["name"] = json!(node.title);
    assignment["task"] = json!(node.goal);
    assignment["task_node_id"] = json!(node.node_id);
    assignment["context"] = json!(node.briefing);
    Ok((assignment, shared))
}

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
        let loaded = parent.messages.iter().rev().find(|m| {
            m[crate::context_assets::REFERENCE]["asset_id"] == id
                && m["_asset_state"] != "reference_only"
                && m["_asset_state"] != "not_delivered"
        });
        let mut message = if let Some(message) = loaded {
            let mut message = message.clone();
            message["role"] = json!("user");
            message["name"] = json!("saved_asset");
            message.as_object_mut().unwrap().remove("tool_call_id");
            message
        } else {
            let result = load_saved_asset(s, &json!({"asset_id":id}), None).await?;
            crate::context_assets::tagged(
                json!({"role":"user","name":"saved_asset","content":result.to_string()}),
                &result["asset_reference"],
            )
        };
        message["_frame_input"] = json!(true);
        message["_pinned"] = json!(pinned.contains(&id));
        messages.push(message);
    }
    inherit_material(parent,&mut messages);
    let mut groups=tool_groups::from_inputs(s, &node.inputs)?;
    if node.kind == "task" {
        // A sequential task receives the production inputs of its caller,
        // without copying the caller's transcript or pretending to reread them.
        inherit_loaded_material(parent,&mut messages);
        groups.extend(parent.loaded_tool_groups.iter().cloned());
        let conn=agent_service::open_db(s.state.workspace.root())?;
        inherit_step_results(&conn,s.task,node.parent_id.as_deref().unwrap(),&mut messages)?;
    }
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
    reconcile_context_policy(&mut child, s.turn, s.input_budget);
    let mut session = s.clone();
    session.scope = tree::scope(&node.node_id);
    ensure!(
        child.input_tokens(
            &session,
            &request(&session, &child.messages, &tools(&session, &child))
        ) + 4096
            < s.input_budget,
        "TASK_INPUTS_OVER_CAPACITY: choose smaller declared inputs; parent frame preserved"
    );
    Ok(child)
}

/// Point a frame at the node it now executes: refresh its path and node card.
fn focus_frame(frame:&mut Window,node:&Node,ancestors:&[Node])->Result<()> {
    frame.messages.retain(|m| !matches!(m["name"].as_str(),Some("task_path"|"task_node"|"task_instruction")));
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
                && let Some(existing) = messages.iter_mut().find(|m| {
                    m[crate::context_assets::REFERENCE]["asset_id"] == id
                })
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

fn inherit_loaded_material(parent:&Window,messages:&mut Vec<Value>) {
    for message in &parent.messages {
        let Some(id)=message[crate::context_assets::REFERENCE]["asset_id"].as_str() else {continue};
        if matches!(message["_asset_state"].as_str(),Some("reference_only"|"not_delivered")) {continue}
        if messages.iter().any(|m|m[crate::context_assets::REFERENCE]["asset_id"]==id) {continue}
        let mut material=message.clone();
        material["role"]=json!("user");material["name"]=json!("saved_asset");
        material["_frame_input"]=json!(true);
        material.as_object_mut().unwrap().remove("tool_call_id");
        messages.push(material);
    }
}

fn inherit_step_results(conn:&rusqlite::Connection,task:&str,owner:&str,messages:&mut Vec<Value>)->Result<()> {
    let mut previous=tree::nodes(conn,task)?;
    previous.sort_by_key(|node|node.ordinal);
    for node in previous.into_iter().filter(|n|n.parent_id.as_deref()==Some(owner)
        && n.status=="done" && n.result.is_some()
        && (n.executor!="delegate" || n.review.as_deref()==Some("accepted"))) {
        if messages.iter().any(|m|m["name"]=="task_input_node"
            && m["content"].as_str().and_then(|v|serde_json::from_str::<Value>(v).ok()).is_some_and(|v|v["node_id"]==node.node_id)) {continue}
        messages.push(json!({"role":"user","name":"task_input_node","content":json!({"node_id":node.node_id,
            "return_event_seq":node.returned_seq.or(node.last_seq),"result":node.result}).to_string(),"_frame_input":true}));
    }
    Ok(())
}

fn write_window(tx: &rusqlite::Transaction<'_>, task: &str, scope: &str, w: &Window) -> Result<()> {
    tx.execute("INSERT INTO agent_runtime_windows VALUES (?1,?2,?3) ON CONFLICT(task_id,scope) DO UPDATE SET state=excluded.state",params![task,scope,serde_json::to_string(w)?])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Session {
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
                material: None,
                input_budget: INPUT_BUDGET,
            }
        }
        async fn persist(&mut self) {
            save(&self.session(), &self.w).await.unwrap();
        }
        async fn call(&mut self, name: &str, args: Value) -> Value {
            let call = crate::model_stream::ToolCall {
                call_id: agent_service::uuid_like(),
                name: name.into(),
                arguments: args.to_string(),
            };
            self.w.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}));
            self.persist().await;
            let mut w = self.w.clone();
            assert!(tool_groups::is_change(name), "the harness only drives tool-group changes");
            tool_groups::execute(&self.session(), &mut w, &call, &tools(&self.session(), &self.w), "request_test", 1).await.unwrap();
            self.w = w;
            let conn = agent_service::open_db(&self.root).unwrap();
            let row = crate::session_history::load_events(
                &conn,
                "t",
                &crate::session_history::EventQuery {
                    kinds: &["tool/result"],
                    call_id: Some(&call.call_id),
                    newest: Some(1),
                    ..Default::default()
                },
            )
            .unwrap()
            .pop()
            .unwrap();
            row.data().unwrap()["meta"]["result"].clone()
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
        fn active(&self) -> Node {
            tree::active(&agent_service::open_db(&self.root).unwrap(), "t")
                .unwrap()
                .unwrap()
        }
    }

    #[test]
    fn inherited_skill_material_is_deduplicated_by_asset_not_delivery_event() {
        let skill=|seq:i64,state:&str| json!({"role":"tool","tool_call_id":format!("call-{seq}"),
            "content":format!("skill delivery {seq}"),"source":{"seq":seq},"_asset_state":state,
            (crate::context_assets::REFERENCE):{"asset_id":"asset-skill","source_tool":"read_skill"}});
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

    #[tokio::test]
    async fn new_definition_proposal_pauses_before_the_model_can_start_more_steps() {
        use axum::{Json,Router,routing::post};
        use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
        let mut h=Harness::new().await;
        let source=h.user("Create a presentation").await;
        let calls=Arc::new(AtomicUsize::new(0));let requests=calls.clone();
        let app=Router::new().route("/v1/chat/completions",post(move||{
            let requests=requests.clone();async move {
                assert_eq!(requests.fetch_add(1,Ordering::SeqCst),0,"host must pause after the proposal");
                let args=json!({"objective":"Create a presentation","constraints":[],"completion_conditions":["Exported presentation"],"source_event_seqs":[source]});
                Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":"propose","type":"function","function":{"name":"update_task_definition","arguments":args.to_string()}}]}}]}))
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        h.state=agent_service::tests::test_state(&h.root,listener.local_addr().unwrap());
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        let mut window=h.w.clone();
        assert!(matches!(run_loop(&h.session(),&mut window,5).await.unwrap(),LoopResult::Answer));
        server.abort();
        assert_eq!(calls.load(Ordering::SeqCst),1);
        let conn=agent_service::open_db(&h.root).unwrap();
        assert_eq!(tree::nodes(&conn,"t").unwrap().iter().filter(|n|!matches!(n.kind.as_str(),"end"|"step")).count(),1);
        assert_eq!(tree::active(&conn,"t").unwrap().unwrap().status,"running");
        assert!(approval_boundary(&h.session()).unwrap().is_some());
    }

    #[test]
    fn frame_compaction_preserves_originals_and_recent_pairs_without_orphaned_pinned_tools() {
        let original = "original user text\n".repeat(20000);
        let mut messages = vec![
            json!({"role":"system","content":"prompt"}),
            json!({"role":"user","content":original}),
            json!({"role":"user","name":"task_instruction","content":"scoped original"}),
        ];
        for i in 0..3 {
            messages.push(json!({"role":"assistant","tool_calls":[{"id":format!("call-{i}"),"type":"function","function":{"name":"read_file","arguments":"{}"}}]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("call-{i}"),"content":format!("material-{i}"),"_pinned":i==0}));
        }
        let retained = preserved_frame_messages(&messages);
        assert!(retained.iter().any(|m| m["content"] == original));
        assert!(retained.iter().any(|m| m["name"] == "task_instruction"));
        assert!(retained.iter().any(|m| m["name"] == "saved_asset"
            && m["content"] == "material-0"
            && m["role"] == "user"));
        assert_eq!(
            &retained[retained.len() - 4..],
            &messages[messages.len() - 4..]
        );
        for (index, message) in retained
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "tool")
        {
            assert!(
                retained[index - 1]["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|call| call["id"] == message["tool_call_id"])
            );
        }
    }

    #[tokio::test]
    async fn soft_pressure_warns_once_without_starting_hard_compression() {
        let mut h = Harness::new().await;
        h.w.messages.push(json!({"role":"assistant","content":""}));
        let session = h.session();
        let mut w = h.w.clone();
        let definitions = tools(&session, &w);
        while w.input_tokens(&session, &request(&session, &w.messages, &definitions))
            < session.input_budget * 65 / 100
        {
            let mut content = w.messages.last_mut().unwrap()["content"]
                .as_str()
                .unwrap()
                .to_owned();
            content.push_str(&"work ".repeat(1024));
            w.messages.last_mut().unwrap()["content"] = json!(content);
        }
        auto_compact(&session, &mut w, 1, &definitions)
            .await
            .unwrap();
        auto_compact(&session, &mut w, 2, &definitions)
            .await
            .unwrap();
        assert_eq!(
            w.messages
                .iter()
                .filter(|m| m["name"] == "task_context_pressure")
                .count(),
            1
        );
        let conn = agent_service::open_db(&h.root).unwrap();
        let warnings:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='context/split_reminder'",[],|r|r.get(0)).unwrap();
        assert_eq!(warnings, 1);
        let compressed:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='context/compression_start'",[],|r|r.get(0)).unwrap();
        assert_eq!(compressed, 0);
    }

    #[tokio::test]
    async fn protected_material_above_hard_threshold_does_not_generate_empty_summaries() {
        let h=Harness::new().await;let session=h.session();let mut w=h.w.clone();
        w.messages.push(json!({"role":"user","name":"saved_asset","_frame_input":true,"_pinned":true,"content":""}));
        let definitions=tools(&session,&w);
        while w.input_tokens(&session,&request(&session,&w.messages,&definitions))<session.input_budget*88/100 {
            let mut content=w.messages.last().unwrap()["content"].as_str().unwrap().to_owned();content.push_str(&"fixed ".repeat(1024));
            w.messages.last_mut().unwrap()["content"]=json!(content);
        }
        let original=w.messages.clone();
        auto_compact(&session,&mut w,1,&definitions).await.unwrap();
        auto_compact(&session,&mut w,2,&definitions).await.unwrap();
        assert_eq!(&w.messages[..original.len()],original.as_slice());
        let conn=agent_service::open_db(&h.root).unwrap();
        let summaries:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='context/compression_start'",[],|r|r.get(0)).unwrap();assert_eq!(summaries,0);
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
            "planned",
        )
        .unwrap();
        node.agent_id = Some("worker".into());
        node.status = "running".into();
        tree::emit(&tx, "t", "task/node_started", &mut node, 1).unwrap();
        tx.commit().unwrap();
        drop(conn);
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

    #[tokio::test]
    async fn tool_results_drive_dynamic_requests_and_plain_answer_ends_even_with_pending_records() {
        use axum::{Json,Router,routing::post};
        use std::sync::{Arc,Mutex};
        let seen=Arc::new(Mutex::new(Vec::<Value>::new()));let log=seen.clone();
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let app=Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>| {
            let log=log.clone();async move {
                let mut requests=log.lock().unwrap();let n=requests.len();
                let messages=body["messages"].as_array().unwrap();
                assert!(!messages.iter().any(|m|m["name"]=="task_return_required"));
                assert!(!body["tools"].as_array().unwrap().iter().any(|t|crate::task_tree::is_tool(t["function"]["name"].as_str().unwrap())));
                let call=|id:&str,name:&str,args:Value| json!({"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
                let message=match n {
                    0=>call("read","read_file",json!({"path":"source.txt"})),
                    1=>{
                        let result=messages.iter().find(|m|m["tool_call_id"]=="read").expect("read result must reach the next request");
                        assert!(result.to_string().contains("source document"));
                        call("write","write_file",json!({"path":"result.txt","content":"verified output"}))
                    },
                    2=>{
                        assert!(messages.iter().any(|m|m["tool_call_id"]=="write"));
                        json!({"role":"assistant","content":"Written result.txt from the source."})
                    },
                    _=>panic!("no host-generated continuation after the no-tool answer"),
                };
                requests.push(body);
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
        let mut h=Harness::new().await;
        std::fs::write(h.root.join("source.txt"),"source document").unwrap();
        h.call("load_tool_group",json!({"group":"files"})).await;
        let source=h.user("Write an output using the source").await;
        let proposal=crate::main_task_anchor::update(&h.root,"t",&json!({"objective":"Write the output","constraints":[],"completion_conditions":["Output exists"],"source_event_seqs":[source]})).unwrap();
        crate::main_task_anchor::decide_definition(&h.root,"t",proposal["version"].as_i64().unwrap(),true,"user").unwrap();
        h.state=agent_service::tests::test_state(&h.root,address);
        let mut window=h.w.clone();
        assert!(matches!(run_loop(&h.session(),&mut window,10).await.unwrap(),LoopResult::Answer));
        server.abort();assert_eq!(seen.lock().unwrap().len(),3);
        assert_eq!(std::fs::read_to_string(h.root.join("result.txt")).unwrap(),"verified output");
        let conn=agent_service::open_db(&h.root).unwrap();
        let mut statement=conn.prepare("SELECT data FROM agent_task_events WHERE task_id='t' AND kind='flow/node_add' ORDER BY seq").unwrap();
        let nodes=statement.query_map([],|r|r.get::<_,String>(0)).unwrap().map(|r|serde_json::from_str::<Value>(&r.unwrap()).unwrap()).collect::<Vec<_>>();
        assert_eq!(nodes.len(),3,"one Flow node per actual model request");
        for pair in nodes.windows(2) {assert!(pair[1]["edges"].as_array().unwrap().iter().any(|edge|edge["source"]==pair[0]["node"]["id"] && edge["target"]==pair[1]["node"]["id"]));}
    }


    /// 烧水热菜 on the real loop: the main task branches a repair; the repair
    /// runs in a fresh context; its plain answer ends it and comes back as the
    /// result; the main task continues without the repair's working context.
    #[tokio::test]
    async fn a_branched_task_runs_in_its_own_context_and_its_answer_comes_back() {
        use axum::{Json,Router,routing::post};
        use std::sync::{Arc,Mutex};
        let seen=Arc::new(Mutex::new(Vec::<String>::new()));let log=seen.clone();
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let app=Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>| {
            let log=log.clone();async move {
                let text=body["messages"].to_string();
                let n=log.lock().unwrap().len();
                let call=|id:&str,name:&str,args:Value| json!({"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
                let message=match n {
                    0=>call("branch","branch_task",json!({"title":"Repair pot","goal":"Make the pot heat","reason":"Switch on failed: pot base loose","briefing":"Pot base is loose"})),
                    1=>{assert!(!text.contains("SWITCH_ATTEMPT"),"the repair has its own context");assert!(text.contains("Pot base is loose"));
                        call("inspect","read_task_board",json!({}))},
                    2=>json!({"role":"assistant","content":"POT_REPAIRED: contact spring replaced"}),
                    3=>{assert!(text.contains("SWITCH_ATTEMPT"),"the main task resumes in its own context");
                        // The interrupted branch_task call returns the repair's answer.
                        let messages=body["messages"].as_array().unwrap();
                        let returned=messages.iter().filter(|m|m["role"]=="tool" && m["tool_call_id"]=="branch").collect::<Vec<_>>();
                        assert_eq!(returned.len(),1);
                        assert!(returned[0]["content"].as_str().unwrap().contains("POT_REPAIRED"));
                        assert!(!text.contains("\"started\""),"the call did not return before the branch ended");
                        assert!(!text.contains("\"inspect\""),"the repair's working context is gone");
                        json!({"role":"assistant","content":"Heating; dinner served"})},
                    _=>panic!("unexpected request"),
                };
                log.lock().unwrap().push(message["tool_calls"][0]["function"]["name"].as_str().unwrap_or("text").to_owned());
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
        let mut h=Harness::new().await;
        h.state=agent_service::tests::test_state(&h.root,address);
        h.w.messages.push(json!({"role":"user","name":"saved_asset","content":"SWITCH_ATTEMPT: no power"}));
        h.persist().await;
        let mut window=h.w.clone();
        let result=run_loop(&h.session(),&mut window,10).await;
        server.abort();
        assert!(matches!(result.unwrap(),LoopResult::Answer));
        assert_eq!(*seen.lock().unwrap(),["branch_task","read_task_board","text","text"]);
        let conn=agent_service::open_db(&h.root).unwrap();
        let nodes=tree::nodes(&conn,"t").unwrap();
        let repair=nodes.iter().find(|n|n.kind=="branch").unwrap();
        let end=nodes.iter().find(|n|n.kind=="end" && n.parent_id.as_deref()==Some(repair.node_id.as_str())).unwrap();
        assert_eq!((end.metadata["iterator"]["task"].as_str(),end.metadata["iterator"]["call_id"].as_str(),end.metadata["iterator"]["scope"].as_str()),(Some(tree::ROOT),Some("branch"),Some("main")),"the end node holds the interrupted iterator");
        // Steps come from the AI's conclusions; the repair hangs under the step that decided it.
        let steps=|owner:&str| nodes.iter().filter(|n|n.kind=="step" && n.parent_id.as_deref()==Some(owner)).map(|n|n.title.as_str()).collect::<Vec<_>>();
        assert_eq!(steps(tree::ROOT),["branch_task"]);
        assert_eq!(steps(&repair.node_id),["read_task_board"]);
        let decided=nodes.iter().find(|n|Some(n.node_id.as_str())==repair.parent_id.as_deref()).unwrap();
        assert_eq!((repair.status.as_str(),decided.kind.as_str(),decided.parent_id.as_deref()),("done","step",Some(tree::ROOT)));
        assert!(nodes.iter().all(|n|n.status=="done"),"{:?}",nodes.iter().map(|n|(&n.title,&n.status)).collect::<Vec<_>>());
        let frames:i64=conn.query_row("SELECT COUNT(*) FROM agent_runtime_windows WHERE task_id='t' AND scope=?1",[tree::scope(&repair.node_id)],|r|r.get(0)).unwrap();
        assert_eq!(frames,0,"the finished task's context is dropped");
    }

    #[tokio::test]
    async fn an_invalid_branch_is_an_ordinary_tool_error() {
        let mut h=Harness::new().await;
        let call=crate::model_stream::ToolCall{call_id:"bad".into(),name:"branch_task".into(),arguments:json!({"title":"No goal","reason":"x"}).to_string()};
        h.w.messages.push(json!({"role":"assistant","tool_calls":[{"id":"bad","type":"function","function":{"name":"branch_task","arguments":call.arguments}}]}));
        h.persist().await;
        let mut w=h.w.clone();
        let scope=branch(&h.session(),&mut w,&call,"request",1).await.unwrap();
        assert_eq!(scope,"main");
        assert!(w.messages.last().unwrap()["content"].as_str().unwrap().contains("error"));
        assert_eq!(tree::active(&agent_service::open_db(&h.root).unwrap(),"t").unwrap().unwrap().node_id,tree::ROOT);
    }

}
