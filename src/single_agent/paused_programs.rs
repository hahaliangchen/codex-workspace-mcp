//! A message from the user pauses a running program instead of ending it. The
//! rest was planned before the user spoke, so the main AI hears the message
//! and decides: its new program may resume the paused one as a Promise, run
//! beside or after the new work, or leave it, in which case it is discarded
//! when the turn ends. The paused program moves into a frame of its own and
//! its frames keep their state, so resuming continues from the same step.
use super::*;
use crate::agent_program::Action;
use crate::agent_program::runtime::ActionKind;
use crate::function_frames::{self as tree, Node};

pub(super) const RESUME: &str = "resume_previous_program";
const PAUSED: &str = "paused";

pub(super) fn resume_tool() -> Value {
    function(RESUME, "Continue the program the user's message paused, from the step where it stopped, and return its return value. A paused program that is not resumed by the end of this turn is discarded.",
        json!({}), json!([]))
}

/// The frame holding a paused program; it returns to whoever resumes it.
pub(super) fn is_previous(w: &Window) -> bool {
    w.program_task.as_ref().is_some_and(|task| task["role"] == "previous")
}

fn load(conn: &rusqlite::Connection, task: &str, scope: &str) -> Result<Option<Window>> {
    let raw: Option<String> = conn
        .query_row("SELECT state FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2", params![task, scope], |r| r.get(0))
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into)).transpose()
}

/// The frame a program's call runs in, while that call has not returned.
fn callee<'a>(nodes: &'a [Node], call_id: &str) -> Option<&'a Node> {
    nodes.iter().find(|node| {
        node.metadata["caller"]["call_id"] == call_id && matches!(node.status.as_str(), "running" | "suspended" | PAUSED)
    })
}

/// A frame and every frame its calls opened, through any depth.
fn chain(nodes: &[Node], root: &str) -> Vec<Node> {
    let mut members = BTreeSet::from([root.to_owned()]);
    loop {
        let before = members.len();
        for node in nodes {
            if node.metadata["caller"]["task"].as_str().is_some_and(|caller| members.contains(caller)) {
                members.insert(node.node_id.clone());
            }
        }
        if members.len() == before {
            return nodes.iter().filter(|node| members.contains(&node.node_id)).cloned().collect();
        }
    }
}

/// Where a frame stopped: the line and call of each step its code had in
/// flight, followed by where that call stopped, or the AI still at work in a
/// function without code.
pub(super) fn stop_point(s: &Session<'_>, w: &Window) -> Result<String> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let nodes = tree::nodes(&conn, s.task)?;
    describe(&conn, s.task, &nodes, w)
}
fn describe(conn: &rusqlite::Connection, task: &str, nodes: &[Node], w: &Window) -> Result<String> {
    let Some(runtime) = w.program.as_ref() else {
        return Ok(match program_flow::role(w) {
            program_flow::Role::Implement => "（AI 正在编写实现）".into(),
            _ => "（AI 处理中）".into(),
        });
    };
    let actions: Vec<Action> = runtime.pending.iter().cloned().chain(runtime.inflight_actions()).collect();
    let mut steps = Vec::new();
    for action in actions {
        // Arguments tell apart calls to one function made side by side.
        let callee_name = match &action.kind {
            ActionKind::Call { name, args } => {
                let args = args.iter().map(Value::to_string).collect::<Vec<_>>().join(", ");
                let args = if args.chars().count() > 40 { format!("{}…", args.chars().take(40).collect::<String>()) } else { args };
                format!("{name}({args})")
            }
            ActionKind::Tool { name, .. } if name == RESUME => "previous.resume()".into(),
            ActionKind::Task { name, .. } | ActionKind::Tool { name, .. } => name.clone(),
            _ => "并行读取".into(),
        };
        let mut below = String::new();
        // A resume runs the paused program; a call runs in its own frame.
        let frame = match &action.kind {
            ActionKind::Tool { name, .. } if name == RESUME => match &w.resumable {
                Some(scope) => load(conn, task, scope)?,
                None => None,
            },
            _ => match callee(nodes, &action.id) {
                Some(node) => load(conn, task, &tree::frame_scope(node))?,
                None => None,
            },
        };
        if let Some(frame) = frame {
            let inner = describe(conn, task, nodes, &frame)?;
            below = if inner.starts_with('（') || inner.is_empty() { inner } else { format!(" → {inner}") };
        }
        steps.push(((action.at.line, action.at.column), format!("第 {} 行 {callee_name}{below}", action.at.line)));
    }
    // In source order; calls made side by side on one line by what they say.
    steps.sort();
    Ok(steps.into_iter().map(|(_, step)| step).collect::<Vec<_>>().join("、"))
}

/// Move the main window's program into a paused frame of its own and leave the
/// main AI free to answer the user. Returns where the program stopped.
pub(super) async fn pause_main(s: &Session<'_>, w: &mut Window) -> Result<String> {
    program_flow::recover(s, w).await?;
    let stopped_at = stop_point(s, w)?;
    let runtime = w.program.take().context("no running program to pause")?;
    let calls: BTreeSet<String> = runtime.pending.iter().map(|action| action.id.clone())
        .chain(runtime.inflight_actions().into_iter().map(|action| action.id)).collect();
    let main_id = task_frames::frame_task(s, w);
    let mut previous = w.clone();
    previous.program = Some(runtime);
    previous.program_task = Some(json!({"name":"previous","role":"previous"}));
    previous.messages = Vec::new();
    previous.program_value = None;
    previous.parallel_root = None;
    let (task, turn) = (s.task.to_owned(), s.turn);
    let scope = db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let main = tree::get(&tx, &task, &main_id)?;
        let ordinal = tree::children(&tx, &task, &main_id)?.len();
        let mut node = task_frames::new_task(&main, &json!({"goal":"paused program"}), ordinal, "temp", turn)?;
        // It runs beside the main AI, so it never counts as the task in view.
        node.metadata["parallel"] = json!(true);
        node.metadata["parallel_owner"] = json!(node.node_id);
        node.metadata["previous"] = json!(true);
        node.status = PAUSED.into();
        tree::emit(&tx, &task, "task/node_paused", &mut node, turn)?;
        let scope = tree::frame_scope(&node);
        previous.task_node_id = Some(node.node_id.clone());
        let mut nodes = tree::nodes(&tx, &task)?;
        // The paused calls now return to the paused program's own frame.
        for callee in nodes.iter_mut() {
            if callee.metadata["caller"]["task"] == main_id.as_str()
                && callee.metadata["caller"]["call_id"].as_str().is_some_and(|id| calls.contains(id))
            {
                callee.metadata["caller"]["task"] = json!(node.node_id);
                callee.metadata["caller"]["scope"] = json!(scope);
            }
        }
        for mut member in chain(&nodes, &node.node_id) {
            if member.node_id == node.node_id || !matches!(member.status.as_str(), "running" | "suspended") {
                continue;
            }
            if member.status == "running" {
                member.status = PAUSED.into();
            }
            member.metadata["parallel"] = json!(true);
            tree::emit(&tx, &task, "task/node_paused", &mut member, turn)?;
        }
        let mut main = tree::get(&tx, &task, &main_id)?;
        if main.status != "running" {
            main.status = "running".into();
            tree::emit(&tx, &task, "task/node_resumed", &mut main, turn)?;
        }
        task_frames::write_window(&tx, &task, &scope, &previous)?;
        tx.commit()?;
        Ok(scope)
    })
    .await?;
    w.resumable = Some(scope);
    w.program_value = None;
    append(s, "program/paused", json!({"turn":s.turn,"stopped_at":stopped_at,"scope":w.resumable}), false).await?;
    Ok(stopped_at)
}

fn set_chain_status(tx: &rusqlite::Transaction<'_>, task: &str, root: &str, from: &str, to: &str, turn: usize) -> Result<()> {
    let kind = if to == PAUSED { "task/node_paused" } else { "task/node_resumed" };
    for mut member in chain(&tree::nodes(tx, task)?, root) {
        if member.status == from {
            member.status = to.into();
            tree::emit(tx, task, kind, &mut member, turn)?;
        }
    }
    Ok(())
}

/// Run the paused program this window may resume until it returns: Some(its
/// value, whether it failed), or None when the user spoke again and it paused.
pub(super) async fn resume(s: &Session<'_>, w: &Window) -> Result<Option<(Value, bool)>> {
    let scope = w.resumable.clone().context("no paused program to resume")?;
    let (task, turn) = (s.task.to_owned(), s.turn);
    let (start_scope, mut frame, previous_id) = db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let previous = load(&tx, &task, &scope)?.context("the paused program already finished or was discarded")?;
        let previous_id = previous.task_node_id.clone().context("paused program frame has no task")?;
        set_chain_status(&tx, &task, &previous_id, PAUSED, "running", turn)?;
        // It continues where it stopped: in its innermost serial call.
        let nodes = tree::nodes(&tx, &task)?;
        let (mut at, mut frame) = (scope, previous);
        while let Some(action) = frame.program.as_ref().and_then(|program| program.pending.clone())
            && let Some(node) = callee(&nodes, &action.id)
            && let Some(child) = load(&tx, &task, &tree::frame_scope(node))?
        {
            at = tree::frame_scope(node);
            frame = child;
        }
        tx.commit()?;
        Ok((at, frame, previous_id))
    })
    .await?;
    let mut session = s.clone();
    session.scope = start_scope;
    session.task_node_id = frame.task_node_id.clone();
    let result = Box::pin(run_loop(&session, &mut frame, 80, false)).await;
    let (task, turn, scope) = (s.task.to_owned(), s.turn, w.resumable.clone().unwrap_or_default());
    let returned = match &result {
        Ok(LoopResult::Answer) if is_previous(&frame) => frame.program_value.clone(),
        _ => None,
    };
    if matches!(result, Ok(LoopResult::Paused)) {
        db(s.state.workspace.root(), move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            set_chain_status(&tx, &task, &previous_id, "running", PAUSED, turn)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
        return Ok(None);
    }
    let Some(value) = returned else {
        let message = match result {
            Err(error) => format!("{error:#}"),
            _ => "the paused program did not finish".into(),
        };
        discard_frame(s, &previous_id).await?;
        return Ok(Some((json!({"message":message}), true)));
    };
    db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut node = tree::get(&tx, &task, &previous_id)?;
        node.status = "done".into();
        node.result = Some(value_summary(&tx, &task, &scope)?);
        tree::emit(&tx, &task, "task/node_returned", &mut node, turn)?;
        tx.execute("DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2", params![task, scope])?;
        tx.commit()?;
        Ok(())
    })
    .await?;
    Ok(Some(match value.get("$task_error") {
        Some(error) => (error.clone(), true),
        None => (value, false),
    }))
}
fn value_summary(tx: &rusqlite::Transaction<'_>, task: &str, scope: &str) -> Result<Value> {
    Ok(load(tx, task, scope)?.and_then(|frame| frame.program_value).map(|value| json!({"result":value})).unwrap_or(Value::Null))
}

/// End a paused frame and everything under it without running any of it.
async fn discard_frame(s: &Session<'_>, root: &str) -> Result<()> {
    let (task, turn, root) = (s.task.to_owned(), s.turn, root.to_owned());
    db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for mut member in chain(&tree::nodes(&tx, &task)?, &root) {
            if matches!(member.status.as_str(), "running" | "suspended" | PAUSED) {
                member.status = "interrupted".into();
                member.result = Some(json!({"message":"paused and not resumed; its remaining steps did not run"}));
                tree::emit(&tx, &task, "task/node_returned", &mut member, turn)?;
                tx.execute("DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2", params![task, tree::frame_scope(&member)])?;
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

/// At the end of a turn, programs paused and not resumed are discarded.
pub(super) async fn discard_unresumed(s: &Session<'_>, w: &mut Window) -> Result<()> {
    let task = s.task.to_owned();
    let paused = db(s.state.workspace.root(), move |conn| {
        Ok(tree::nodes(conn, &task)?.into_iter()
            .filter(|node| node.metadata["previous"] == true && matches!(node.status.as_str(), "running" | "suspended" | PAUSED))
            .map(|node| node.node_id).collect::<Vec<_>>())
    })
    .await?;
    for root in &paused {
        discard_frame(s, root).await?;
    }
    if w.resumable.take().is_some() {
        save(s, w).await?;
    }
    Ok(())
}

/// Concurrent calls of a resumed program that paused with it, each from its
/// innermost frame, to run again.
pub(super) fn paused_calls(s: &Session<'_>, w: &Window) -> Result<Vec<(String, Window)>> {
    let Some(runtime) = w.program.as_ref() else { return Ok(Vec::new()) };
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let nodes = tree::nodes(&conn, s.task)?;
    let mut calls = Vec::new();
    for action in runtime.inflight_actions() {
        let Some(node) = callee(&nodes, &action.id) else { continue };
        let mut at = tree::frame_scope(node);
        let Some(mut frame) = load(&conn, s.task, &at)? else { continue };
        while let Some(action) = frame.program.as_ref().and_then(|program| program.pending.clone())
            && let Some(node) = callee(&nodes, &action.id)
            && let Some(child) = load(&conn, s.task, &tree::frame_scope(node))?
        {
            at = tree::frame_scope(node);
            frame = child;
        }
        calls.push((at, frame));
    }
    Ok(calls)
}
