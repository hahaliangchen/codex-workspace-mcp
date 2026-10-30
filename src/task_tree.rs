//! Durable task state. Transitions run in the same transaction as frame changes.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const ROOT: &str = "n0";
/// Guard against corrupt parent links. Real nesting is bounded by MAX_TEMP_TASKS.
pub const MAX_DEPTH: usize = MAX_TEMP_TASKS * 2 + 8;
/// Temporary tasks may nest this deep below a main task.
pub const MAX_TEMP_TASKS: usize = 64;

#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    pub node_id: String,
    pub parent_id: Option<String>,
    pub kind: String,
    pub ordinal: usize,
    pub title: String,
    pub goal: String,
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

/// Direct children of a node, in planning order.
pub fn children(conn: &Connection, task: &str, parent: &str) -> Result<Vec<Node>> {
    let rows = conn
        .prepare(
            "SELECT data FROM agent_task_nodes WHERE task_id=?1 AND parent_id=?2 ORDER BY rowid",
        )?
        .query_map(params![task, parent], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut nodes = rows
        .iter()
        .map(|raw| Ok(serde_json::from_str::<Node>(raw)?))
        .collect::<Result<Vec<_>>>()?;
    nodes.sort_by_key(|n| n.ordinal);
    Ok(nodes)
}

/// Main tasks and function calls are context boundaries.
pub fn is_task(node: &Node) -> bool {
    node.kind == "task"
}

pub fn is_temp(node: &Node) -> bool {
    node.metadata["role"] == "temp"
}

/// The step a task is executing. Steps share their task's context.
pub fn current_step(conn: &Connection, task: &str, owner: &str) -> Result<Option<Node>> {
    Ok(children(conn, task, owner)?
        .into_iter()
        .find(|n| n.kind == "step" && n.status == "current"))
}

/// The finish node records a function return.
pub fn finish_node(conn: &Connection, task: &str, owner: &str) -> Result<Option<Node>> {
    Ok(children(conn, task, owner)?
        .into_iter()
        .find(|n| n.kind == "finish"))
}

/// The window that holds a node's context. The conversation level and its
/// main task share the main window; temporary tasks get their own.
pub fn frame_scope(node: &Node) -> String {
    if node.node_id == ROOT || (node.kind == "task" && !is_temp(node)) {
        "main".into()
    } else {
        scope(&node.node_id)
    }
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
        .filter(|n| n.executor == "inline" && n.agent_id.is_none() && n.status == "running" && n.metadata["parallel"] != true)
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

/// The conversation node. It is a container, not a task: each user request
/// becomes a main task the agent creates under it with create_task.
pub fn ensure_root(tx: &Transaction<'_>, task: &str, turn: usize) -> Result<Node> {
    if let Ok(node) = get(tx, task, ROOT) {
        return Ok(node);
    }
    let mut node = Node {
        node_id: ROOT.into(),
        parent_id: None,
        kind: "root".into(),
        ordinal: 0,
        title: "Conversation".into(),
        goal: "Each user request becomes a main task created by the agent".into(),
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

/// Every task ends at its finish node; steps are ordered before it.
pub const FINISH_ORDINAL: usize = 1_000_000;



pub fn planned(parent: &Node, args: &Value, ordinal: usize, kind: &str) -> Result<Node> {
    let executor = args["executor"].as_str().unwrap_or("inline");
    ensure!(
        matches!(executor, "inline" | "delegate"),
        "invalid executor"
    );
    let inputs = if args["inputs"].is_null() {
        json!({})
    } else {
        args["inputs"].clone()
    };
    ensure!(inputs.is_object(), "function inputs must be an object");
    Ok(Node {
        node_id: format!("n{}", crate::agent_service::uuid_like()),
        parent_id: Some(parent.node_id.clone()),
        kind: kind.into(),
        ordinal,
        title: text(args, "title")?,
        goal: text(args, "goal")?,
        inputs,
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

pub fn definition_version(conn: &Connection, task: &str) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT seq FROM agent_task_events WHERE task_id=?1 AND kind='task/definition' ORDER BY seq DESC LIMIT 1",[task],|r|r.get(0)).optional()?)
}


/// A workspace file named as a node's output must exist and come from this
/// node or an accepted child: a successful write of that path, or, for files a
/// program generated, a successful program run in this node.


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
        }
    }
    active(tx, task)?;
    Ok(())
}

/// The model creates goals; the host owns execution steps and finish.
pub fn is_tool(name: &str) -> bool {
    name == "create_task"
}

pub fn tool() -> Value {
    json!({"type":"function","function":{"name":"create_task",
        "description":"Create one main task scope with its goal and necessary background before substantive tool-based work. Call alone. Then express the complete work, checks and delivery in execute_program with one main. Helpers create serial function scopes and authorized fork/join creates parallel function scopes. Do not call create_task per operation or to build child task lists; the runtime owns function calls, steps and returns.",
        "parameters":{"type":"object","properties":{
            "goal":{"type":"string","minLength":1,"maxLength":16000,"description":"The main task objective."},
            "reason":{"type":"string","description":"Optional background for this task."},
            "briefing":{"type":"string","description":"Short workflow sketch and necessary background; executable source is submitted to execute_program."}},
            "required":["goal"],"additionalProperties":false}}})
}
