//! Durable task state. Transitions run in the same transaction as frame changes.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const ROOT: &str = "n0";
pub const MAX_DEPTH: usize = 4;

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
        done_when: text(args, "done_when")?,
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

pub fn plan(
    tx: &Transaction<'_>,
    task: &str,
    parent: &Node,
    args: &Value,
    turn: usize,
) -> Result<Value> {
    ensure!(
        path(tx, task, &parent.node_id)?.len() <= MAX_DEPTH,
        "TASK_DEPTH_LIMIT"
    );
    let items = args["tasks"].as_array().context("tasks must be an array")?;
    ensure!(items.len() <= 128, "too many planned tasks");
    let mut changed = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut next_ordinal = nodes(tx, task)?
        .iter()
        .filter(|n| n.parent_id.as_deref() == Some(&parent.node_id))
        .map(|n| n.ordinal + 1)
        .max()
        .unwrap_or(0);
    for item in items {
        let mut node = if let Some(id) = item["node_id"].as_str() {
            ensure!(seen.insert(id), "duplicate task update");
            let mut node = get(tx, task, id)?;
            ensure!(
                node.parent_id.as_deref() == Some(&parent.node_id) && node.status == "pending",
                "only unstarted direct children may be revised"
            );
            if item["cancel"].as_bool() == Some(true) {
                node.status = "cancelled".into();
            }
            for (field, target) in [
                ("title", &mut node.title),
                ("goal", &mut node.goal),
                ("done_when", &mut node.done_when),
            ] {
                if item.get(field).is_some() {
                    *target = text(item, field)?;
                }
            }
            if item.get("inputs").is_some() {
                node.inputs = validate_inputs(&item["inputs"])?;
            }
            if let Some(executor) = item.get("executor") {
                let executor = executor
                    .as_str()
                    .context("executor must be inline or delegate")?;
                ensure!(
                    matches!(executor, "inline" | "delegate"),
                    "invalid executor"
                );
                node.executor = executor.into();
            }
            node
        } else {
            let node = planned(parent, item, next_ordinal, "planned")?;
            next_ordinal += 1;
            node
        };
        if let Some(ordinal) = item["ordinal"].as_u64() {
            node.ordinal = usize::try_from(ordinal)?;
        }
        node.definition_version = definition_version(tx, task)?;
        let kind = if item["node_id"].is_string() {
            "task/node_revised"
        } else {
            "task/node_planned"
        };
        emit(tx, task, kind, &mut node, turn)?;
        changed.push(
            json!({"node_id":node.node_id,"title":node.title,"status":node.status,
            "ordinal":node.ordinal,"executor":node.executor,"metadata":node.metadata}),
        );
    }
    Ok(json!({"tasks":changed}))
}

pub fn confirmed_ppt_pages(
    tx: &Transaction<'_>,
    task: &str,
    version: i64,
    plan: &Value,
) -> Result<()> {
    if plan["domain"] != "ppt" {
        return Ok(());
    }
    let turn=tx.query_row("SELECT COALESCE(MAX(json_extract(data,'$.turn')),0) FROM agent_task_events WHERE task_id=?1",[task],|r|r.get::<_,usize>(0))?;
    let root = ensure_root(tx, task, turn)?;
    let previous = nodes(tx, task)?;
    for mut node in previous
        .iter()
        .filter(|n| {
            n.metadata["domain"] == "ppt"
                && n.metadata["plan_version"] != version
                && n.status == "pending"
        })
        .cloned()
    {
        node.status = "cancelled".into();
        emit(tx, task, "task/node_revised", &mut node, turn)?;
    }
    for (index, item) in plan["items"]
        .as_array()
        .context("PPT plan items missing")?
        .iter()
        .enumerate()
    {
        if previous.iter().any(|n| {
            n.metadata["plan_version"] == version && n.metadata["plan_page_id"] == item["id"]
        }) {
            continue;
        }
        let mut node = planned(
            &root,
            &json!({"title":format!("Page {}: {}",index+1,item["title"].as_str().unwrap_or("")),
            "goal":item["requirements"],"done_when":"Saved page matches the confirmed requirements and current visual checks pass"}),
            index,
            "planned",
        )?;
        node.metadata = json!({"domain":"ppt","plan_version":version,"project_id":plan["project_id"],
            "page_index":index+1,"plan_page_id":item["id"],"requirements":item["requirements"]});
        emit(tx, task, "task/node_planned", &mut node, turn)?;
    }
    Ok(())
}

pub fn current_ppt_page(conn: &Connection, task: &str, id: &str) -> Result<Option<Value>> {
    Ok(path(conn, task, id)?
        .into_iter()
        .rev()
        .find(|n| n.metadata["domain"] == "ppt")
        .map(|n| n.metadata))
}

pub fn validate_plan_version(conn: &Connection, task: &str, node: &Node) -> Result<()> {
    ensure!(
        crate::main_task_anchor::definition_matches(conn, task, node.definition_version)?,
        "TASK_DEFINITION_SUPERSEDED: return blocked and revise the plan from the parent"
    );
    let page = if node.metadata["domain"] == "ppt" {
        Some(node.metadata.clone())
    } else if let Some(parent) = node.parent_id.as_deref() {
        current_ppt_page(conn, task, parent)?
    } else {
        None
    };
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
    tx.execute(
        "INSERT OR IGNORE INTO agent_user_instructions VALUES (?1,?2,?3,?4,NULL,?5)",
        params![task, id, seq, origin, content.to_string()],
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

pub fn scope_message(
    tx: &Transaction<'_>,
    task: &str,
    current: &Node,
    args: &Value,
    turn: usize,
) -> Result<Value> {
    let id = text(args, "instruction_id")?;
    let scope = text(args, "node_id")?;
    ensure!(
        path(tx, task, &current.node_id)?
            .iter()
            .any(|n| n.node_id == scope),
        "scope must be current node or ancestor"
    );
    let old: Option<Option<String>> = tx
        .query_row(
            "SELECT scope_node FROM agent_user_instructions WHERE task_id=?1 AND id=?2",
            params![task, id],
            |r| r.get(0),
        )
        .optional()?;
    let old = old.context("instruction not found")?;
    ensure!(
        old.as_ref().is_none_or(|s| s == &scope),
        "instruction already scoped; original scope cannot be silently changed"
    );
    tx.execute(
        "UPDATE agent_user_instructions SET scope_node=?3 WHERE task_id=?1 AND id=?2",
        params![task, id, scope],
    )?;
    let result =
        json!({"instruction_id":id,"node_id":scope,"task_node_id":current.node_id,"turn":turn});
    crate::agent_service::append_event_tx(
        tx,
        task,
        "task/message_scoped",
        &result,
        Some("append"),
    )?;
    Ok(result)
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
        !children.iter().any(|n| n.status == "pending"),
        "PENDING_CHILD_TASKS: finish or cancel outstanding work"
    );
    ensure!(
        !children.iter().any(|n| n.executor == "delegate"
            && n.status == "done"
            && n.review.as_deref() != Some("accepted")),
        "UNACCEPTED_DELEGATE_RESULT: inspect and review child output first"
    );
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
        let id = value.as_str().context("outputs must contain asset IDs")?;
        let origin:Option<(String,i64)>=conn.query_row("SELECT COALESCE(e.node_id,'n0'),e.seq FROM agent_context_assets a JOIN agent_task_events e ON e.task_id=a.task_id AND e.seq=json_extract(a.reference_json,'$.history_reference.event_seq') WHERE a.task_id=?1 AND a.asset_id=?2",params![task,id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
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

pub fn is_tool(name: &str) -> bool {
    matches!(
        name,
        "plan_tasks"
            | "start_task"
            | "branch_task"
            | "return_task"
            | "split_task"
            | "scope_message"
    )
}

pub fn tools() -> Vec<Value> {
    let task = json!({"type":"object","properties":{"node_id":{"type":"string"},"title":{"type":"string"},"goal":{"type":"string"},"done_when":{"type":"string"},"inputs":{"type":"object","description":"Declared assets, pinned materials, nodes, history, and optional tool_groups IDs for this child frame. Omitted tool_groups starts with core tools only."},"executor":{"type":"string","enum":["inline","delegate"]},"ordinal":{"type":"integer","minimum":0},"cancel":{"type":"boolean"}}});
    let ret = json!({"status":{"type":"string","enum":["done","failed","blocked","needs_user"]},"result":{"type":"string"},"facts":{"type":"array","items":{"type":"object"}},"outputs":{"type":"array","items":{"type":"string"}},"evidence":{"type":"array","items":{"type":"integer"}},"open_issues":{"type":"string"},"question":{"type":"string"}});
    let entries = [
        (
            "plan_tasks",
            "Create, revise, reorder or cancel unstarted direct child tasks. A node is a verifiable unit of work, not an AI request. IDs are assigned by the host.",
            json!({"tasks":{"type":"array","items":task}}),
            json!(["tasks"]),
        ),
        (
            "start_task",
            "Enter one planned direct child. Supply a briefing after reviewing relevant prior results. Parent frame is suspended until return_task.",
            json!({"node_id":{"type":"string"},"briefing":{"type":"string"},"inputs":{"type":"object"}}),
            json!(["node_id", "briefing"]),
        ),
        (
            "branch_task",
            "Insert and immediately enter an unexpected subtask, such as repairing a failing dependency. Parent frame resumes after return.",
            json!({"title":{"type":"string"},"goal":{"type":"string"},"done_when":{"type":"string"},"reason":{"type":"string"},"briefing":{"type":"string"},"inputs":{"type":"object"}}),
            json!(["title", "goal", "done_when", "reason", "briefing"]),
        ),
        (
            "return_task",
            "Finish this node with a concise result and verifiable evidence. done requires successful own tool events, accepted direct child returns, or output assets. needs_user returns the question to the parent and ends this turn. Local clarifications use ordinary text and keep the node running. Resolve pending instruction scopes first.",
            ret,
            json!(["status", "result"]),
        ),
        (
            "split_task",
            "Fold completed work into a done child, retaining every original user message, child result, declared input, pinned material and this native tool exchange. Plan remaining direct children. Resolve pending instruction scopes first.",
            json!({"title":{"type":"string"},"completed":{"type":"object"},"tasks":{"type":"array","items":task}}),
            json!(["title", "completed", "tasks"]),
        ),
        (
            "scope_message",
            "Assign an original user instruction to this node, an ancestor subtree or n0 (root). Preserve its original conditions. Only main execution assigns scope. Must be called alone.",
            json!({"instruction_id":{"type":"string"},"node_id":{"type":"string"}}),
            json!(["instruction_id", "node_id"]),
        ),
    ];
    entries.into_iter().map(|(name,description,properties,required)|json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required}}})).collect()
}
