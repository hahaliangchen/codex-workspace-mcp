//! Durable task state. Transitions run in the same transaction as frame changes.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const ROOT: &str = "n0";
/// Each unexpected task adds a step and a branch level, so 6 allows three nested repairs.
pub const MAX_DEPTH: usize = 6;

#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    pub node_id: String,
    pub parent_id: Option<String>,
    pub kind: String,
    pub ordinal: usize,
    pub title: String,
    pub goal: String,
    pub done_when: String,
    pub inputs: Value,
    pub executor: String,
    pub agent_id: Option<String>,
    pub status: String,
    pub spawned_from: Option<String>,
    pub briefing: String,
    pub result: Option<Value>,
    pub review: Option<String>,
    pub first_seq: Option<i64>,
    pub last_seq: Option<i64>,
    #[serde(default)]
    pub split_through: Option<i64>,
    #[serde(default)]
    pub source: Option<Value>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub returned_seq: Option<i64>,
    #[serde(default)]
    pub definition_version: Option<i64>,
}

pub fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_task_nodes(
        task_id TEXT NOT NULL,node_id TEXT NOT NULL,parent_id TEXT,executor TEXT NOT NULL,
        status TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(task_id,node_id));
        CREATE INDEX IF NOT EXISTS task_nodes_parent ON agent_task_nodes(task_id,parent_id);
        CREATE TABLE IF NOT EXISTS agent_user_instructions(
        task_id TEXT NOT NULL,id TEXT NOT NULL,event_seq INTEGER NOT NULL,origin_node TEXT NOT NULL,
        scope_node TEXT,content TEXT NOT NULL,PRIMARY KEY(task_id,id));",
    )?;
    Ok(())
}

pub fn nodes(conn: &Connection, task: &str) -> Result<Vec<Node>> {
    let rows = conn
        .prepare("SELECT data FROM agent_task_nodes WHERE task_id=?1 ORDER BY rowid")?
        .query_map([task], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.iter()
        .map(|raw| Ok(serde_json::from_str(raw)?))
        .collect()
}

pub fn get(conn: &Connection, task: &str, id: &str) -> Result<Node> {
    let raw: String = conn
        .query_row(
            "SELECT data FROM agent_task_nodes WHERE task_id=?1 AND node_id=?2",
            params![task, id],
            |r| r.get(0),
        )
        .optional()?
        .context("TASK_NODE_NOT_FOUND")?;
    Ok(serde_json::from_str(&raw)?)
}

pub fn scope(id: &str) -> String {
    if id == ROOT {
        "main".into()
    } else {
        format!("node:{id}")
    }
}

pub fn path(conn: &Connection, task: &str, id: &str) -> Result<Vec<Node>> {
    let mut current = get(conn, task, id)?;
    let mut result = vec![current.clone()];
    while let Some(parent) = current.parent_id.as_deref() {
        ensure!(result.len() <= MAX_DEPTH + 1, "TASK_TREE_CYCLE_OR_DEPTH");
        ensure!(
            !result.iter().any(|n| n.node_id == parent),
            "TASK_TREE_CYCLE"
        );
        current = get(conn, task, parent)?;
        result.push(current.clone());
    }
    result.reverse();
    Ok(result)
}

pub fn active(conn: &Connection, task: &str) -> Result<Option<Node>> {
    let running = nodes(conn, task)?
        .into_iter()
        .filter(|n| n.executor == "inline" && n.status == "running")
        .collect::<Vec<_>>();
    ensure!(running.len() <= 1, "TASK_STACK_MULTIPLE_RUNNING_NODES");
    let Some(node) = running.into_iter().next() else {
        return Ok(None);
    };
    let chain = path(conn, task, &node.node_id)?;
    ensure!(
        chain
            .iter()
            .take(chain.len() - 1)
            .filter(|n| n.kind != "step")
            .all(|n| n.status == "suspended"),
        "TASK_STACK_ANCESTOR_NOT_SUSPENDED"
    );
    Ok(Some(node))
}

fn cache(conn: &Connection, task: &str, node: &Node) -> Result<()> {
    conn.execute("INSERT INTO agent_task_nodes VALUES (?1,?2,?3,?4,?5,?6)
        ON CONFLICT(task_id,node_id) DO UPDATE SET parent_id=excluded.parent_id,executor=excluded.executor,status=excluded.status,data=excluded.data",
        params![task,node.node_id,node.parent_id,node.executor,node.status,serde_json::to_string(node)?])?;
    Ok(())
}

pub fn emit(
    tx: &Transaction<'_>,
    task: &str,
    kind: &str,
    node: &mut Node,
    turn: usize,
) -> Result<i64> {
    let seq = crate::agent_service::next_event_seq(tx, task)?;
    node.last_seq = Some(seq);
    if kind == "task/node_returned" {
        node.returned_seq = Some(seq);
    }
    if node.first_seq.is_none() {
        node.first_seq = Some(seq);
    }
    let mut data = serde_json::to_value(&*node)?;
    data["turn"] = json!(turn);
    data["task_node_id"] = json!(node.node_id);
    crate::agent_service::append_event_tx(tx, task, kind, &data, Some("append"))?;
    cache(tx, task, node)?;
    Ok(seq)
}

pub fn ensure_root(tx: &Transaction<'_>, task: &str, turn: usize) -> Result<Node> {
    if let Ok(node) = get(tx, task, ROOT) {
        ensure_end(tx,task,&node,turn)?;
        return Ok(node);
    }
    let mut node = Node {
        node_id: ROOT.into(),
        parent_id: None,
        kind: "root".into(),
        ordinal: 0,
        title: "Main task".into(),
        goal: "Use the confirmed task definition and original user instructions".into(),
        done_when: "Verify the user's requested outcome".into(),
        inputs: json!({}),
        executor: "inline".into(),
        agent_id: None,
        status: "running".into(),
        spawned_from: None,
        briefing: String::new(),
        result: None,
        review: None,
        first_seq: None,
        last_seq: None,
        split_through: None,
        source: None,
        metadata: Value::Null,
        returned_seq: None,
        definition_version: definition_version(tx, task)?,
    };
    emit(tx, task, "task/node_planned", &mut node, turn)?;
    ensure_end(tx, task, &node, turn)?;
    let originals=tx.prepare("SELECT seq,kind,data FROM agent_task_events WHERE task_id=?1 AND kind IN ('user/message','user/interjection') ORDER BY seq")?
        .query_map([task],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (seq, kind, raw) in originals {
        let event: Value = serde_json::from_str(&raw)?;
        let content = if kind == "user/message" {
            &event["message"]["content"]
        } else {
            &event["text"]
        };
        register_user(tx, task, seq, ROOT, content)?;
    }
    Ok(node)
}

fn text(args: &Value, field: &str) -> Result<String> {
    let value = args[field]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .with_context(|| format!("{field} must be a nonempty string"))?;
    ensure!(
        value.chars().count() <= 16000,
        "{field} exceeds 16000 characters"
    );
    Ok(value.to_owned())
}

pub fn validate_inputs(args: &Value) -> Result<Value> {
    let inputs = if args.is_null() {
        json!({})
    } else {
        args.clone()
    };
    ensure!(inputs.is_object(), "inputs must be an object");
    crate::single_agent::tool_groups::requested(&inputs["tool_groups"])?;
    if let Some(files) = inputs.get("files") {
        let files = files.as_array().context("inputs.files must be an array of workspace paths")?;
        ensure!(files.len() <= 128, "too many declared files");
        ensure!(
            files.iter().all(|f| f.as_str().is_some_and(|p| !p.trim().is_empty()
                && !std::path::Path::new(p).components().any(|c| matches!(c, std::path::Component::ParentDir)))),
            "inputs.files entries must be workspace paths without .."
        );
    }
    for field in ["assets", "nodes", "pinned", "history"] {
        if let Some(items) = inputs.get(field) {
            let items = items
                .as_array()
                .context("input material selectors must be arrays")?;
            ensure!(items.len() <= 128, "too many input selectors");
            ensure!(
                items.iter().all(|v| if field == "history" {
                    v.as_i64().is_some_and(|n| n >= 0)
                } else {
                    v.as_str().is_some_and(|s| !s.is_empty())
                }),
                "invalid input selector"
            );
        }
    }
    Ok(inputs)
}

/// Every task ends at its end node; steps are inserted before it.
pub const END_ORDINAL: usize = 1_000_000;

/// The end node of a task (root or branch). A task finishes only when it
/// reaches it through return_task; until then the host keeps it iterating.
/// Created with the task, or lazily for tasks saved before end nodes existed.
pub fn ensure_end(tx: &Transaction<'_>, task: &str, owner: &Node, turn: usize) -> Result<Node> {
    if let Some(mut end) = nodes(tx, task)?.into_iter().find(|n| n.parent_id.as_deref() == Some(&owner.node_id) && n.kind == "end") {
        if end.goal=="Finish this task: return_task with its result" {
            end.goal="End of execution; recorded by the host when the loop finishes".into();
            emit(tx,task,"task/node_revised",&mut end,turn)?;
        }
        return Ok(end);
    }
    let mut end = planned(owner, &json!({"title":"End","goal":"End of execution; recorded by the host when the loop finishes","done_when":owner.done_when}), END_ORDINAL, "end")?;
    emit(tx, task, "task/node_planned", &mut end, turn)?;
    Ok(end)
}

pub fn planned(parent: &Node, args: &Value, ordinal: usize, kind: &str) -> Result<Node> {
    let executor = args["executor"].as_str().unwrap_or("inline");
    ensure!(
        matches!(executor, "inline" | "delegate"),
        "invalid executor"
    );
    Ok(Node {
        node_id: format!("n{}", crate::agent_service::uuid_like()),
        parent_id: Some(parent.node_id.clone()),
        kind: kind.into(),
        ordinal,
        title: text(args, "title")?,
        goal: text(args, "goal")?,
        done_when: if args.get("done_when").is_some() {text(args, "done_when")?} else {text(args, "goal")?},
        inputs: validate_inputs(&args["inputs"])?,
        executor: executor.into(),
        agent_id: None,
        status: "pending".into(),
        spawned_from: args["reason"].as_str().map(str::to_owned),
        briefing: String::new(),
        result: None,
        review: None,
        first_seq: None,
        last_seq: None,
        split_through: None,
        source: None,
        metadata: Value::Null,
        returned_seq: None,
        definition_version: parent.definition_version,
    })
}

pub fn validate_plan_version(conn: &Connection, task: &str, node: &Node) -> Result<()> {
    ensure!(
        crate::main_task_anchor::definition_matches(conn, task, node.definition_version)?,
        "TASK_DEFINITION_SUPERSEDED: return blocked and revise the plan from the parent"
    );
    let page = if node.metadata["plan_version"].is_i64() {
        Some(node.metadata.clone())
    } else if let Some(parent)=node.parent_id.as_deref() {
        path(conn,task,parent)?.into_iter().rev().find(|n|n.metadata["plan_version"].is_i64()).map(|n|n.metadata)
    } else {None};
    if let Some(page) = page {
        let version:Option<i64>=conn.query_row("SELECT seq FROM agent_task_events WHERE task_id=?1 AND kind='task/plan' ORDER BY seq DESC LIMIT 1",[task],|r|r.get(0)).optional()?;
        ensure!(
            version == page["plan_version"].as_i64(),
            "TASK_PLAN_SUPERSEDED: return blocked and replan from the parent"
        );
    }
    Ok(())
}

pub fn definition_version(conn: &Connection, task: &str) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT seq FROM agent_task_events WHERE task_id=?1 AND kind='task/definition' ORDER BY seq DESC LIMIT 1",[task],|r|r.get(0)).optional()?)
}

pub fn definition_changed(tx: &Transaction<'_>, task: &str, version: i64) -> Result<()> {
    if let Ok(mut root) = get(tx, task, ROOT) {
        root.definition_version = Some(version);
        let turn=tx.query_row("SELECT COALESCE(MAX(json_extract(data,'$.turn')),0) FROM agent_task_events WHERE task_id=?1",[task],|r|r.get::<_,usize>(0))?;
        emit(tx, task, "task/node_revised", &mut root, turn)?;
    }
    Ok(())
}

pub fn no_unscoped(conn: &Connection, task: &str) -> Result<()> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM agent_user_instructions WHERE task_id=?1 AND scope_node IS NULL",
        [task],
        |r| r.get(0),
    )?;
    ensure!(
        count == 0,
        "UNSCOPED_USER_MESSAGES: call scope_message before return_task or split_task"
    );
    Ok(())
}

pub fn register_user(
    tx: &Transaction<'_>,
    task: &str,
    seq: i64,
    origin: &str,
    content: &Value,
) -> Result<()> {
    let id = format!("u{seq}");
    // A message received at the root can only be scoped to the root (scope is
    // the current node or an ancestor), so there is nothing for the model to decide.
    let scope = (origin == ROOT).then_some(ROOT);
    tx.execute(
        "INSERT OR IGNORE INTO agent_user_instructions VALUES (?1,?2,?3,?4,?5,?6)",
        params![task, id, seq, origin, scope, content.to_string()],
    )?;
    Ok(())
}

pub fn instructions(conn: &Connection, task: &str, id: &str) -> Result<Vec<Value>> {
    let chain = path(conn, task, id)?
        .iter()
        .map(|n| n.node_id.clone())
        .collect::<Vec<_>>();
    let rows=conn.prepare("SELECT id,event_seq,origin_node,scope_node,content FROM agent_user_instructions WHERE task_id=?1 ORDER BY event_seq")?
        .query_map([task],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,String>(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().filter(|(_,_,_,scope,_)|scope.as_ref().is_none_or(|s|chain.contains(s)))
        .map(|(id,seq,origin,scope,raw)|Ok(json!({"id":id,"event_seq":seq,"origin_node":origin,"scope_node":scope,
            "scope_pending":scope.is_none(),"original":serde_json::from_str::<Value>(&raw)?,
            "instruction":if scope.is_none(){"Scope pending. Visible for awareness; only the main Agent may assign scope. Do not treat this notice as global execution permission."}else{"Apply the original instruction, including its conditions, within this subtree."}}))).collect()
}

pub fn validate_return(conn: &Connection, task: &str, node: &Node, args: &Value) -> Result<()> {
    no_unscoped(conn, task)?;
    let status = args["status"].as_str().context("status required")?;
    ensure!(
        matches!(status, "done" | "failed" | "blocked" | "needs_user"),
        "invalid return status"
    );
    text(args, "result")?;
    ensure!(
        crate::context_window::estimate(args) <= 4096,
        "TASK_RETURN_TOO_LARGE: refine the result; original text was not truncated"
    );
    let children = nodes(conn, task)?
        .into_iter()
        .filter(|n| n.parent_id.as_deref() == Some(&node.node_id))
        .collect::<Vec<_>>();
    ensure!(
        !children
            .iter()
            .any(|n| matches!(n.status.as_str(), "running" | "suspended")),
        "CHILD_TASKS_STILL_RUNNING"
    );
    if matches!(status, "failed" | "blocked") {
        text(args, "open_issues")?;
    }
    if status == "needs_user" {
        text(args, "question")?;
    }
    if status != "done" {
        return Ok(());
    }
    validate_plan_version(conn, task, node)?;
    ensure!(
        !children.iter().any(|n| n.status == "pending" && n.kind != "end"),
        "PENDING_CHILD_TASKS: finish or cancel outstanding work"
    );
    ensure!(
        !children.iter().any(|n| n.executor == "delegate"
            && n.status == "done"
            && n.review.as_deref() != Some("accepted")),
        "UNACCEPTED_DELEGATE_RESULT: inspect and review child output first"
    );
    // A task is done only when its planned steps are. A branch is a yielded
    // call: its outcome, done or not, was already handed back to the caller,
    // which decided how to continue.
    ensure!(status != "done" || !children.iter().any(|n| n.kind == "planned" && matches!(n.status.as_str(),"failed"|"blocked"|"needs_user")),
        "UNRESOLVED_CHILD_TASKS: retry failed or blocked steps with start_task, or cancel them with plan_tasks, before completing the task");
    let accepted = |id: &str| {
        children.iter().any(|n| {
            n.node_id == id
                && n.status == "done"
                && (n.executor != "delegate" || n.review.as_deref() == Some("accepted"))
        })
    };
    let mut count = 0;
    let mut evidence = args["evidence"].as_array().cloned().unwrap_or_default();
    if let Some(facts) = args["facts"].as_array() {
        for fact in facts {
            evidence.extend(fact["evidence"].as_array().cloned().unwrap_or_default());
        }
    }
    for value in &evidence {
        let seq = value
            .as_i64()
            .context("evidence must contain event sequence numbers")?;
        let (owner,kind,raw):(String,String,String)=conn.query_row("SELECT COALESCE(node_id,'n0'),kind,data FROM agent_task_events WHERE task_id=?1 AND seq=?2",params![task,seq],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.context("evidence event not found")?;
        let envelope: Value = serde_json::from_str(&raw)?;
        let (kind, data) = if kind == "subagent/event" {
            (
                envelope["event_type"].as_str().unwrap_or(""),
                &envelope["event"],
            )
        } else {
            (kind.as_str(), &envelope)
        };
        let source = node.source.as_ref().is_some_and(|s| {
            s["node_id"] == owner
                && seq >= s["after_seq"].as_i64().unwrap_or(0)
                && seq <= s["through_seq"].as_i64().unwrap_or(-1)
        });
        let own = (owner == node.node_id || source)
            && (kind == "tool/result"
                && data["message"]["isError"] != true
                && data["meta"]["result"].get("error").is_none()
                && data["meta"]["result"]["process_exit_code"]
                    .as_i64()
                    .is_none_or(|code| code == 0)
                && !matches!(
                    data["meta"]["result"]["status"].as_str(),
                    Some("failed" | "error" | "interrupted" | "cancelled")
                ));
        let child = kind == "task/node_returned" && accepted(&owner) && data["status"] == "done";
        ensure!(own || child, "UNRELATED_OR_UNSUCCESSFUL_EVIDENCE: {seq}");
        count += 1;
    }
    for value in args["outputs"].as_array().into_iter().flatten() {
        let id = value.as_str().context("outputs must contain asset IDs or workspace file paths")?;
        if !id.starts_with("asset-") {
            output_file(conn, task, node, id, &accepted)?;
            count += 1;
            continue;
        }
        let origin:Option<(String,i64)>=conn.query_row("SELECT COALESCE(e.node_id,'n0'),e.seq FROM agent_context_assets a JOIN agent_task_events e ON e.task_id=a.task_id AND e.seq=json_extract(a.reference_json,'$.history_reference.event_seq') WHERE a.task_id=?1 AND a.asset_id=?2",params![task,id],|r|Ok((r.get(0)?,r.get(1)?))).optional()
            // The asset table is created with the first saved asset.
            .or_else(|e| if e.to_string().contains("no such table") {Ok(None)} else {Err(e)})?;
        let (owner, seq) = origin.context("output asset not found")?;
        let source = node.source.as_ref().is_some_and(|s| {
            s["node_id"] == owner
                && seq >= s["after_seq"].as_i64().unwrap_or(0)
                && seq <= s["through_seq"].as_i64().unwrap_or(-1)
        });
        ensure!(
            owner == node.node_id || source || accepted(&owner),
            "UNRELATED_OR_UNACCEPTED_OUTPUT"
        );
        count += 1;
    }
    ensure!(count > 0, "DONE_REQUIRES_EVIDENCE");
    Ok(())
}

/// A workspace file named as a node's output must exist and come from this
/// node or an accepted child: a successful write of that path, or, for files a
/// program generated, a successful program run in this node.
fn output_file(conn: &Connection, task: &str, node: &Node, path: &str, accepted: &dyn Fn(&str) -> bool) -> Result<()> {
    let root = conn.path().map(std::path::Path::new)
        .and_then(|db| db.parent()?.parent()).context("workspace root unavailable")?;
    let relative = std::path::Path::new(path).strip_prefix(root).unwrap_or(std::path::Path::new(path))
        .components().filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => Some("..".into()),
            _ => None,
        }).collect::<Vec<_>>();
    ensure!(!relative.is_empty() && !relative.iter().any(|p| p == ".."), "OUTPUT_PATH_INVALID: {path}");
    let relative = relative.join("/");
    ensure!(root.join(&relative).exists(), "OUTPUT_FILE_MISSING: {relative} does not exist in the workspace");
    let mine = |owner: &str, seq: i64| owner == node.node_id || accepted(owner) || node.source.as_ref().is_some_and(|s| {
        s["node_id"] == owner && seq >= s["after_seq"].as_i64().unwrap_or(0) && seq <= s["through_seq"].as_i64().unwrap_or(-1)
    });
    let rows = conn.prepare("SELECT COALESCE(node_id,'n0'),seq,data FROM agent_task_events WHERE task_id=?1 AND kind='tool/result'")?
        .query_map([task], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut generated = false;
    for (owner, seq, raw) in rows {
        if !mine(&owner, seq) { continue; }
        let data: Value = serde_json::from_str(&raw)?;
        let result = &data["meta"]["result"];
        if data["message"]["isError"] == true || result.get("error").is_some() { continue; }
        let written = result["path"].as_str().map(|p| p.replace('\\', "/").trim_start_matches("./").to_owned());
        if written.as_deref() == Some(relative.as_str()) && result.get("code_hash").is_some() {
            return Ok(());
        }
        generated |= owner == node.node_id && result["process_exit_code"].as_i64() == Some(0);
    }
    ensure!(generated, "OUTPUT_NOT_PRODUCED_HERE: {relative} was not written by this node or an accepted child, and this node ran no successful program that could have generated it");
    Ok(())
}

pub fn board(conn: &Connection, task: &str) -> Result<Value> {
    let active = active(conn, task)?;
    Ok(json!({"active_node_id":active.as_ref().map(|n|&n.node_id),
        "path":active.as_ref().map(|n|path(conn,task,&n.node_id)).transpose()?,"nodes":nodes(conn,task)?}))
}

pub fn frame_status(conn: &Connection, task: &str, id: &str) -> Result<Option<Value>> {
    let Some(node) = nodes(conn, task)?.into_iter().find(|n| n.node_id == id) else {
        return Ok(None);
    };
    let mut related = nodes(conn, task)?
        .into_iter()
        .filter(|n| {
            n.parent_id.as_deref() == Some(id)
                || node.parent_id.is_some() && n.parent_id == node.parent_id
        })
        .collect::<Vec<_>>();
    related.sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then(a.node_id.cmp(&b.node_id)));
    let total = related.len();
    let mut selected = Vec::new();
    let mut tokens = 0;
    for other in related.into_iter().take(64) {
        let visible_result = other.result.as_ref().filter(|_| {
            other.executor != "delegate" || other.review.as_deref() == Some("accepted")
        });
        let item = json!({"node_id":other.node_id,"parent_id":other.parent_id,"title":other.title.chars().take(200).collect::<String>(),
            "status":other.status,"executor":other.executor,"review":other.review,
            "result_excerpt":visible_result.and_then(|r|r["result"].as_str()).map(|s|s.chars().take(200).collect::<String>()),
            "return_reference":{"event_seq":other.returned_seq.or(other.last_seq)},"outputs":other.result.as_ref().map(|r|&r["outputs"])});
        let size = crate::context_window::estimate(&item);
        if tokens + size > 3000 {
            break;
        }
        tokens += size;
        selected.push(item);
    }
    Ok(Some(
        json!({"node_id":id,"status":node.status,"related_tasks":selected,"truncated":selected.len()<total,
        "full_board_reference":{"tool":"read_task_board","arguments":{}},"not_a_user_request":true}),
    ))
}

pub fn finish_delegate(
    tx: &Transaction<'_>,
    task: &str,
    agent: &str,
    result: &Value,
    turn: usize,
) -> Result<()> {
    let Some(mut node) = nodes(tx, task)?
        .into_iter()
        .find(|n| n.agent_id.as_deref() == Some(agent))
    else {
        return Ok(());
    };
    if node.status == "running" {
        node.status = "blocked".into();
        node.result = Some(
            json!({"status":"blocked","result":"Delegate ended without a verified return","open_issues":result}),
        );
        emit(tx, task, "task/node_returned", &mut node, turn)?;
    }
    Ok(())
}

pub fn rebuild(tx: &Transaction<'_>, task: &str) -> Result<()> {
    tx.execute("DELETE FROM agent_task_nodes WHERE task_id=?1", [task])?;
    tx.execute(
        "DELETE FROM agent_user_instructions WHERE task_id=?1",
        [task],
    )?;
    let rows = crate::session_history::load_events(tx, task, &Default::default())?;
    for row in rows {
        let data = row.data()?;
        if row.kind.starts_with("task/node_") && data["executor"].is_string() {
            cache(tx, task, &serde_json::from_value::<Node>(data)?)?;
        } else if matches!(row.kind.as_str(), "user/message" | "user/interjection") {
            let owner: String = tx.query_row(
                "SELECT COALESCE(node_id,'n0') FROM agent_task_events WHERE task_id=?1 AND seq=?2",
                params![task, row.seq],
                |r| r.get(0),
            )?;
            register_user(
                tx,
                task,
                row.seq,
                &owner,
                data.get("message")
                    .map(|m| &m["content"])
                    .unwrap_or(&data["text"]),
            )?;
        } else if row.kind == "task/message_scoped" {
            tx.execute(
                "UPDATE agent_user_instructions SET scope_node=?3 WHERE task_id=?1 AND id=?2",
                params![
                    task,
                    data["instruction_id"].as_str(),
                    data["node_id"].as_str()
                ],
            )?;
        }
    }
    active(tx, task)?;
    Ok(())
}

/// Task-tree tools. The main loop has branch_task to start a task with its own
/// context; a delegate hands its result back with return_task.
pub fn is_tool(name: &str) -> bool {
    matches!(name, "branch_task" | "return_task")
}

pub fn tools() -> Vec<Value> {
    let ret = json!({"status":{"type":"string","enum":["done","failed","blocked","needs_user"]},"result":{"type":"string"},"facts":{"type":"array","items":{"type":"object"}},"outputs":{"type":"array","items":{"type":"string"},"description":"What this delegate produced: workspace file paths it wrote or generated, or asset IDs (asset-...)."},"evidence":{"type":"array","items":{"type":"integer"}},"open_issues":{"type":"string"},"question":{"type":"string"}});
    vec![json!({"type":"function","function":{"name":"branch_task","description":"Start a separate task for something unexpected that deserves its own context, such as repairing a broken dependency. It runs in a fresh context with the task goal and your briefing; when it answers without tool calls, that answer comes back here as its result and this task continues where it was. Call it alone.",
        "parameters":{"type":"object","properties":{"title":{"type":"string"},"goal":{"type":"string"},"reason":{"type":"string","description":"Why it needs its own task"},"briefing":{"type":"string","description":"What it needs to know from here"}},"required":["title","goal","reason"]}}}),
        json!({"type":"function","function":{"name":"return_task","description":"Finish this delegated assignment with its result. result and facts are what the main Agent sees: state what changed, not how. done requires successful tool events of this assignment or its output files/assets; failed/blocked state what went wrong in open_issues; needs_user returns a question.",
        "parameters":{"type":"object","properties":ret,"required":["status","result"]}}})]
}
