//! Durable, content-addressed material references. Snapshots remain immutable;
//! working views use handles and restore complete requested or referenced data.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
};

pub const REFERENCE: &str = "_asset_reference";

pub fn rewind(tx: &rusqlite::Transaction, task: &str, start: i64) -> Result<()> {
    for (table, predicate) in [
        (
            "agent_context_assets",
            "json_extract(reference_json,'$.history_reference.event_seq')>=?2",
        ),
        ("agent_context_asset_deliveries", "event_seq>=?2"),
        (
            "agent_context_materials",
            "json_extract(data,'$.through')>=?2",
        ),
    ] {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            tx.execute(
                &format!("DELETE FROM {table} WHERE task_id=?1 AND {predicate}"),
                params![task, start],
            )?;
        }
    }
    Ok(())
}

fn connection(root: &Path) -> Result<Connection> {
    let conn = crate::agent_service::open_db(root)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_context_assets(
        task_id TEXT NOT NULL, asset_id TEXT NOT NULL, reference_json TEXT NOT NULL,
        source_tool TEXT NOT NULL, source_arguments TEXT NOT NULL,
        payload_path TEXT NOT NULL, payload_hash TEXT NOT NULL,
        PRIMARY KEY(task_id,asset_id));
        CREATE TABLE IF NOT EXISTS agent_context_asset_deliveries(
        task_id TEXT NOT NULL,asset_id TEXT NOT NULL,event_seq INTEGER NOT NULL,
        PRIMARY KEY(task_id,asset_id,event_seq));",
    )?;
    Ok(conn)
}

pub fn is_material_tool(name: &str) -> bool {
    matches!(
        name,
        "read_attachment"
            | "read_file"
            | "parallel_read_batch"
            | "read_file_lines"
            | "search_text"
            | "search_code_map"
            | "read_code_flow"
            | "read_skill"
            | "browser_read"
            | "browser_screenshot"
            | "view_image"
            | "analyze_images"
            | "browser_diagnostics"
            | "run_program"
            | "run_project_script"
            | "get_project_process"
            | "http_probe"
    ) || (name.starts_with("read_") && name.ends_with("_symbol"))
        || name.contains("__") // Plugin results obey the same asset protocol.
        || name == "read_session_history"
}

/// Called after the unchanged original result has been committed to history.
/// Repeating the same read produces the same reference, not another identity.
pub fn register(
    root: &Path,
    task: &str,
    seq: i64,
    tool: &str,
    args: &Value,
    result: &Value,
) -> Result<Value> {
    let image_id = result["artifact_id"].as_str().or_else(|| {
        result
            .pointer("/visual_artifact/artifact_id")
            .and_then(Value::as_str)
    });
    let image = image_id
        .map(|id| crate::visual_artifacts::metadata(root, task, id))
        .transpose()?;
    let payload = image.as_ref().unwrap_or(result);
    let bytes = serde_json::to_vec(payload)?;
    let payload_hash = crate::visual_artifacts::hash(&bytes);
    let identity = if let Some(id) = image_id {
        json!({"task":task,"image":id})
    } else {
        json!({"task":task,"tool":tool,"arguments":args,"payload_hash":payload_hash})
    };
    let id = format!(
        "asset-{}",
        crate::visual_artifacts::hash(identity.to_string().as_bytes())
    );
    let mut conn = connection(root)?;
    if let Some(reference) = lookup(&conn, task, &id)? {
        return Ok(reference);
    }
    let relative = format!(".codex-workspace-mcp/assets/{payload_hash}.json");
    let path = crate::file_edit::workspace_path(root, &relative)?;
    std::fs::create_dir_all(path.parent().context("asset directory missing")?)?;
    // Publish a complete immutable blob. A crash while writing a temporary
    // file must not leave a truncated file at the permanent reference path.
    let temporary = path.with_extension(format!("{}.tmp", crate::agent_service::uuid_like()));
    let stored = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        match std::fs::hard_link(&temporary, &path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    crate::visual_artifacts::hash(&std::fs::read(&path)?) == payload_hash,
                    "asset file is incomplete or changed: {id}"
                );
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    })();
    let _ = std::fs::remove_file(&temporary);
    stored?;
    let source_path = result["path"]
        .as_str()
        .or_else(|| result["file_path"].as_str())
        .or_else(|| result.pointer("/data/path").and_then(Value::as_str))
        .or_else(|| result.pointer("/result/data/path").and_then(Value::as_str))
        .or_else(|| args["path"].as_str())
        .or_else(|| args["file_path"].as_str());
    let kind = if tool == "tool_arguments" {
        "tool_arguments"
    } else if image.is_some() {
        "image"
    } else if source_path.is_some()
        && (matches!(tool, "read_file" | "read_file_lines")
            || (tool.starts_with("read_") && tool.ends_with("_symbol")))
    {
        "file_snapshot"
    } else {
        "tool_output"
    };
    let mut reference = json!({"asset_id":id,"kind":kind,"content_hash":payload_hash,"bytes":bytes.len(),
        "snapshot_path":relative,"source_tool":tool,
        "history_reference":{"task_id":task,"event_seq":seq,"include_event":true},
        "read_reference":{"tool":"read_asset","arguments":{"asset_id":id}}});
    for key in [
        "project_id",
        "page_index",
        "revision",
        "expected_revision",
        "status",
        "task_id",
        "agent_id",
        "name",
    ] {
        if let Some(value) = result
            .get(key)
            .or_else(|| result.pointer(&format!("/data/{key}")))
            .or_else(|| result.pointer(&format!("/result/data/{key}")))
            .or_else(|| args.get(key))
            .filter(|v| !v.is_null() && !v.is_object() && !v.is_array())
        {
            reference[key] = value.clone();
        }
    }
    if tool == "tool_arguments" {
        reference["operation"] = result["tool_calls"][0]["function"]["name"].clone();
    }
    if tool == "read_skill" && result["skill"].is_string() {
        reference["skill"] = result["skill"].clone();
    }
    if let Some(paths) = args["paths"].as_array() {
        reference["source_paths"] = json!(paths);
    }
    if let Some(path) = source_path {
        reference["source_path"] = json!(path);
        reference["source_workspace"] = args
            .get("workspace_root")
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or_else(|| json!(root.to_string_lossy()));
        reference["source_version"] = result
            .get("code_hash")
            .or_else(|| result.get("content_hash"))
            .cloned()
            .unwrap_or(Value::Null);
        reference["source_range"] =
            json!({"start_line":result["start_line"],"end_line":result["end_line"]});
    }
    if let Some(image) = image {
        reference["artifact_id"] = image["artifact_id"].clone();
        reference["image_path"] = image["workspace_relative_path"].clone();
        reference["captured_at"] = image["captured_at"].clone();
    }
    if matches!(tool, "read_file" | "read_file_lines") {
        reference["refresh_reference"] =
            json!({"tool":"refresh_asset","arguments":{"asset_id":id}});
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let owner: Option<String> = tx
        .query_row(
            "SELECT COALESCE(node_id,'n0') FROM agent_task_events WHERE task_id=?1 AND seq=?2",
            params![task, seq],
            |r| r.get(0),
        )
        .optional()?;
    reference["task_node_id"] = json!(owner);
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO agent_context_assets VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            task,
            id,
            reference.to_string(),
            tool,
            args.to_string(),
            relative,
            payload_hash
        ],
    )?;
    if inserted == 1 {
        crate::agent_service::append_event_tx(
            &tx,
            task,
            "asset/registered",
            &json!({"reference":reference,"task_node_id":owner}),
            None,
        )?;
    }
    let saved = lookup(&tx, task, &id)?.context("registered asset missing")?;
    tx.commit()?;
    Ok(saved)
}
fn lookup(conn: &Connection, task: &str, id: &str) -> Result<Option<Value>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT reference_json FROM agent_context_assets WHERE task_id=?1 AND asset_id=?2",
            params![task, id],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|text| serde_json::from_str(&text).map_err(Into::into))
        .transpose()
}
pub fn reference(root: &Path, task: &str, id: &str, through: Option<i64>) -> Result<Value> {
    let result = lookup(&connection(root)?, task, id)?
        .context("ASSET_NOT_FOUND: use the asset_id supplied by this task")?;
    ensure!(
        through.is_none_or(|limit| result["history_reference"]["event_seq"]
            .as_i64()
            .is_some_and(|seq| seq <= limit)),
        "asset is beyond the frozen history boundary"
    );
    Ok(result)
}

/// Load handles once for assembling a child task; asset bodies stay on disk.
/// Always read the exact saved version. A live re-read requires refresh_asset.
pub fn read(root: &Path, task: &str, args: &Value, through: Option<i64>) -> Result<Value> {
    let id = args["asset_id"].as_str().context("asset_id is required")?;
    let reference = reference(root, task, id, through)?;
    ensure!(
        reference["storage"] != "plugin",
        "REMOTE_ASSET: read through the authenticated plugin resource adapter"
    );
    let path = crate::file_edit::workspace_path(
        root,
        reference["snapshot_path"]
            .as_str()
            .context("snapshot path missing")?,
    )?;
    let bytes = std::fs::read(path)
        .context("asset snapshot unavailable; original history_reference is retained")?;
    ensure!(
        crate::visual_artifacts::hash(&bytes) == reference["content_hash"].as_str().unwrap_or(""),
        "ASSET_CONTENT_CHANGED: saved snapshot does not match its hash; use original history_reference"
    );
    saved_delivery(&reference, &String::from_utf8(bytes)?, args)
}
pub fn remote_delivery(reference: &Value, bytes: &[u8], args: &Value) -> Result<Value> {
    ensure!(
        crate::visual_artifacts::hash(bytes) == reference["content_hash"].as_str().unwrap_or(""),
        "REMOTE_ASSET_VERSION_CHANGED: link no longer contains the referenced SVG version; inspect the original history_reference or explicitly read the current file"
    );
    let text = std::str::from_utf8(bytes)?;
    let mut result = saved_delivery(reference,&json!({"path":reference["source_path"],"content":text,"content_hash":reference["content_hash"]}).to_string(),args)?;
    result["version"] = json!("verified_remote_content");
    Ok(result)
}
fn saved_delivery(reference: &Value, text: &str, args: &Value) -> Result<Value> {
    let offset = match args.get("char_offset") {
        Some(value) => value
            .as_u64()
            .context("char_offset must be a nonnegative integer")? as usize,
        None => 0,
    };
    let total = text.chars().count();
    ensure!(
        offset <= total,
        "asset char_offset exceeds total_chars={total}"
    );
    let limit = match args.get("max_chars") {
        Some(value) => {
            let limit = value.as_u64().context("max_chars must be an integer")?;
            ensure!(
                (1..=24000).contains(&limit),
                "max_chars must be between 1 and 24000; omit it for the complete saved result"
            );
            Some(limit as usize)
        }
        None => None,
    };
    let content: String = text
        .chars()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    let end = offset + content.chars().count();
    let mut result = json!({"asset_reference":reference,"content":content,"char_offset":offset,"total_chars":total,
        "returned_chars":end-offset,"next_char_offset":if end<total{Some(end)}else{None},"version":"saved_snapshot"});
    if offset == 0 && end == total {
        result["original_result"] = serde_json::from_str(&text)?;
        result.as_object_mut().unwrap().remove("content");
    }
    Ok(result)
}
pub fn refresh_request(root: &Path, task: &str, id: &str) -> Result<(String, Value)> {
    let conn = connection(root)?;
    let (tool,args):(String,String)=conn.query_row("SELECT source_tool,source_arguments FROM agent_context_assets WHERE task_id=?1 AND asset_id=?2",params![task,id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.context("asset not found")?;
    ensure!(
        matches!(tool.as_str(), "read_file" | "read_file_lines"),
        "REFRESH_UNAVAILABLE: only file reads may refresh; read_asset restores a saved result without repeating actions"
    );
    Ok((tool, serde_json::from_str(&args)?))
}

pub fn tagged(mut message: Value, reference: &Value) -> Value {
    message[REFERENCE] = reference.clone();
    message["_asset_state"] = json!("loaded");
    message
}
pub fn delivered(result: &Value, reference: &Value) -> Value {
    json!({"asset_reference":reference,"original_result":result})
}
fn receipt(reference: &Value) -> String {
    let mut handle = serde_json::Map::new();
    for key in [
        "asset_id",
        "kind",
        "plugin",
        "project_id",
        "source_path",
        "source_paths",
        "revision",
        "content_hash",
        "download_url",
        "artifact_id",
        "agent_id",
        "name",
        "status",
        "bytes",
        "read_reference",
    ] {
        if let Some(value) = reference.get(key).filter(|v| !v.is_null()) {
            handle.insert(key.into(), value.clone());
        }
    }
    json!({"asset_reference":handle,"context_state":"reference_only",
        "read_tool":"read_asset"})
    .to_string()
}
pub fn reference_only(message: &mut Value) {
    let receipt = receipt(&message[REFERENCE]);
    if message["role"] == "assistant" && message["tool_calls"].is_array() {
        // These are completed historical calls, never executable requests.
        // Preserve native call IDs and matching result envelopes.
        for call in message["tool_calls"].as_array_mut().unwrap() {
            call["function"]["arguments"] = json!(receipt);
        }
    } else {
        message["content"] = json!(receipt);
    }
    message["_asset_state"] = json!("reference_only");
    message.as_object_mut().unwrap().remove("reasoning_content");
    message
        .as_object_mut()
        .unwrap()
        .remove("viewed_by_context_agent");
}
pub fn presentation_key(message: &Value) -> (String, String) {
    let payload = if let Some(calls) = message["tool_calls"].as_array() {
        Value::Array(calls.iter().map(|c| c["function"].clone()).collect())
    } else if let Some(text) = message["content"].as_str() {
        let value = serde_json::from_str::<Value>(text).unwrap_or(json!(text));
        value.get("original_result").cloned().unwrap_or(value)
    } else {
        message["content"].clone()
    };
    (
        message[REFERENCE]["asset_id"]
            .as_str()
            .unwrap_or("")
            .to_owned(),
        crate::visual_artifacts::hash(payload.to_string().as_bytes()),
    )
}
/// Mechanical edit of AI-selected assets. Native tool envelopes and IDs stay
/// intact; saved history and files are never modified. Unknown IDs reject all.
pub fn release(messages: &mut [Value], ids: &[String]) -> Result<()> {
    let known: BTreeSet<_> = messages
        .iter()
        .filter_map(|m| m[REFERENCE]["asset_id"].as_str())
        .collect();
    for id in ids {
        ensure!(known.contains(id.as_str()), "ASSET_NOT_IN_CONTEXT: {id}");
    }
    for message in messages {
        if message[REFERENCE]["asset_id"]
            .as_str()
            .is_some_and(|id| ids.iter().any(|selected| id == selected))
        {
            reference_only(message);
        }
    }
    Ok(())
}

/// Asset id of a complete saved result present as text in a working view;
/// pages selected with char_offset/max_chars and receipts do not count.
pub fn complete_body(message: &Value) -> Option<&str> {
    let text = message["content"].as_str()?;
    (message["_asset_state"] == "loaded"
        && message["role"] != "assistant"
        && serde_json::from_str::<Value>(text).is_ok_and(|v| v.get("original_result").is_some()))
    .then(|| message[REFERENCE]["asset_id"].as_str())
    .flatten()
}

/// Reports current placement only; it does not access the asset files.
pub fn catalog(messages: &[Value]) -> Value {
    let mut entries = BTreeMap::<String, Value>::new();
    for message in messages {
        let Some(id) = message[REFERENCE]["asset_id"].as_str() else {
            continue;
        };
        let entry = entries
            .entry(id.to_owned())
            .or_insert_with(|| json!({"asset_reference":message[REFERENCE],"presentations":[]}));
        entry["presentations"].as_array_mut().unwrap().push(
            json!({"state":message["_asset_state"],
            "delivery_reference":message["source"]["history_reference"],
            "content_chars":message["content"].as_str().map(|s|s.chars().count()),
            "tool_calls":message["tool_calls"].as_array().map(|calls|calls.iter().map(|call|json!({"id":call["id"],"name":call["function"]["name"]})).collect::<Vec<_>>())}),
        );
    }
    json!({"assets":entries.into_values().collect::<Vec<_>>(),"note":"Loaded material remains in context until an explicit AI edit. image_selected is not proof of pixel delivery."})
}

/// Keep delivery order in the durable asset index when per-step records leave
/// the working view. These are existing event facts, not new asset versions.
pub fn remember_deliveries(root: &Path, task: &str, deliveries: &Value) -> Result<()> {
    let mut conn = connection(root)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for delivery in deliveries.as_array().into_iter().flatten() {
        if let (Some(id), Some(seq)) = (
            delivery["reference"]["asset_id"].as_str(),
            delivery["event_seq"].as_i64(),
        ) {
            tx.execute(
                "INSERT OR IGNORE INTO agent_context_asset_deliveries(task_id,asset_id,event_seq)
                SELECT ?1,?2,seq FROM agent_task_events WHERE task_id=?1 AND seq=?3",
                params![task, id, seq],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn frame_index(root: &Path, task: &str, node: &str, messages: &[Value]) -> Result<Value> {
    let conn = connection(root)?;
    let mut entries = BTreeMap::<String, (Value, i64)>::new();
    for message in messages {
        if let Some(id) = message[REFERENCE]["asset_id"].as_str() {
            let seq = message
                .pointer("/source/seq")
                .or_else(|| message.pointer("/source/history_reference/event_seq"))
                .and_then(Value::as_i64)
                .unwrap_or_else(|| {
                    message[REFERENCE]["history_reference"]["event_seq"]
                        .as_i64()
                        .unwrap_or(-1)
                });
            if entries.get(id).is_none_or(|(_, previous)| *previous <= seq) {
                entries.insert(id.to_owned(), (message[REFERENCE].clone(), seq));
            }
        }
        for delivery in message["_step_assets"].as_array().into_iter().flatten() {
            if let (Some(id), Some(seq)) = (
                delivery["reference"]["asset_id"].as_str(),
                delivery["event_seq"].as_i64(),
            ) && entries.get(id).is_none_or(|(_, previous)| *previous <= seq)
            {
                entries.insert(id.to_owned(), (delivery["reference"].clone(), seq));
            }
        }
    }
    let rows=conn.prepare("WITH deliveries AS (
        SELECT asset_id,json_extract(reference_json,'$.history_reference.event_seq') AS seq FROM agent_context_assets WHERE task_id=?1
        UNION ALL SELECT asset_id,event_seq FROM agent_context_asset_deliveries WHERE task_id=?1
        ), scoped AS (SELECT d.asset_id,MAX(e.seq) AS seq FROM deliveries d
        JOIN agent_task_events e ON e.task_id=?1 AND e.seq=d.seq
        LEFT JOIN agent_task_nodes n ON n.task_id=e.task_id AND n.node_id=e.node_id
        WHERE (COALESCE(e.node_id,'n0')=?2 OR (n.parent_id=?2 AND n.status IN ('done','current')
            AND (n.executor='inline' OR json_extract(n.data,'$.review')='accepted')))
        GROUP BY d.asset_id)
        SELECT a.reference_json,s.seq FROM scoped s JOIN agent_context_assets a ON a.task_id=?1 AND a.asset_id=s.asset_id
        ORDER BY s.seq DESC LIMIT 65")?.query_map(params![task,node],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let possibly_more = rows.len() == 65;
    for (raw, seq) in rows {
        let reference: Value = serde_json::from_str(&raw)?;
        if let Some(id) = reference["asset_id"].as_str() {
            if entries.get(id).is_none_or(|(_, previous)| *previous <= seq) {
                entries.insert(id.to_owned(), (reference, seq));
            }
        }
    }
    // A newer result of the same generic invocation replaces the old handle
    // in this index only. Immutable snapshots and per-step references remain.
    let mut latest = BTreeMap::<String, (Value, i64)>::new();
    for (reference, seq) in entries.into_values() {
        let id = reference["asset_id"].as_str().unwrap_or("");
        let origin=conn.query_row("SELECT source_tool,source_arguments FROM agent_context_assets WHERE task_id=?1 AND asset_id=?2",
            params![task,id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional()?;
        let key = if let Some((tool, args)) = origin {
            let args: Value = serde_json::from_str(&args)?;
            json!([tool, args, reference["operation"]]).to_string()
        } else {
            id.to_owned()
        };
        if latest
            .get(&key)
            .is_none_or(|(_, previous)| *previous <= seq)
        {
            latest.insert(key, (reference, seq));
        }
    }
    let total = latest.len();
    let mut entries = latest.into_values().collect::<Vec<_>>();
    entries.sort_by_key(|(_, seq)| std::cmp::Reverse(*seq));
    let present = messages
        .iter()
        .filter_map(complete_body)
        .collect::<BTreeSet<_>>();
    let mut selected = Vec::new();
    let mut tokens = 0;
    for (reference, seq) in entries.into_iter().take(64) {
        let mut item = json!({"delivery_reference":{"task_id":task,"event_seq":seq}});
        for key in [
            "asset_id",
            "kind",
            "source_tool",
            "operation",
            "source_path",
            "revision",
        ] {
            if let Some(value) = reference.get(key) {
                item[key] = value.clone();
            }
        }
        item["in_context"] = json!(
            reference["asset_id"]
                .as_str()
                .is_some_and(|id| present.contains(id))
        );
        let size = crate::context_window::estimate(&item);
        if tokens + size > 3000 {
            break;
        }
        tokens += size;
        selected.push(item);
    }
    Ok(
        json!({"host_status":true,"not_a_user_request":true,"truncated":possibly_more||selected.len()<total,
        "assets":selected,"remaining_reference":{"tool":"read_session_history","arguments":{"node_id":node,"include_descendants":true}},
        "instruction":"Latest saved snapshots per tool and arguments. in_context=true: its complete saved result is already in this request; reuse it. read_asset(asset_id) restores the complete recorded data of any other entry; old versions remain in step history."}),
    )
}

/// The reviewer sees the entire conversation with asset bodies replaced by
/// handles. This is a view only; it does not release anything in execution.

/// A single compression request sees narrative and exact execution receipts,
/// never source bodies, pixels, historical reasoning or repeated catalogs.

/// Protect active skill instructions independently of a summary's choices.
/// Repeated reads of the same skill retain only its latest delivered version.
pub fn active_instruction_ids(messages: &[Value], active: &BTreeSet<String>) -> Vec<String> {
    let mut latest = BTreeMap::new();
    for m in messages {
        if m[REFERENCE]["source_tool"] != "read_skill" {
            continue;
        }
        let parsed = m["content"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or_default();
        let skill = m[REFERENCE]["skill"].as_str().or_else(|| {
            parsed
                .pointer("/original_result/skill")
                .and_then(Value::as_str)
        });
        if let Some(skill) = skill.filter(|skill| active.contains(*skill)) {
            latest.insert(
                skill.to_owned(),
                m[REFERENCE]["asset_id"].as_str().unwrap().to_owned(),
            );
        }
    }
    latest.into_values().collect()
}

/// Index generated code/SVG in historical arguments too. Reading an argument
/// asset restores data and can never replay the original external action.
pub fn index_arguments(root: &Path, task: &str, messages: &mut [Value]) -> Result<()> {
    for message in messages {
        if message[REFERENCE].is_object() || message["role"] != "assistant" {
            continue;
        }
        let Some(calls) = message["tool_calls"].as_array() else {
            continue;
        };
        if !calls.iter().any(|c| {
            c["function"]["arguments"]
                .as_str()
                .is_some_and(|a| a.len() >= 2048)
        }) {
            continue;
        }
        let Some(seq) = message["source"]["seq"].as_i64() else {
            continue;
        };
        let first = &calls[0];
        let args: Value = first["function"]["arguments"]
            .as_str()
            .and_then(|a| serde_json::from_str(a).ok())
            .unwrap_or(Value::Null);
        let mut reference = register(
            root,
            task,
            seq,
            "tool_arguments",
            &json!({"path":args["path"],
            "project_id":args["project_id"]}),
            &json!({"tool_calls":calls}),
        )?;
        if reference["storage"] != "plugin" {
            reference["kind"] = json!("tool_arguments");
            reference["operation"] = first["function"]["name"].clone();
            reference["project_id"] = args["project_id"].clone();
            reference["expected_revision"] = args["expected_revision"].clone();
        }
        *message = tagged(message.clone(), &reference);
    }
    Ok(())
}

/// Add references to old/pending tool messages without changing their original
/// historical results or re-running any tool. Existing loaded bodies stay loaded.
pub fn hydrate(root: &Path, task: &str, messages: &mut [Value]) -> Result<()> {
    index_arguments(root, task, messages)?;
    let conn = connection(root)?;
    for message in messages.iter_mut() {
        if message[REFERENCE].is_object() || message["role"] != "tool" {
            continue;
        }
        let (Some(seq), Some(call_id)) = (
            message["source"]["seq"].as_i64(),
            message["tool_call_id"].as_str(),
        ) else {
            continue;
        };
        let Some(record) = crate::session_history::load_events(
            &conn,
            task,
            &crate::session_history::EventQuery {
                exact: Some(seq),
                kinds: &["tool/result", "context/tool_result"],
                ..Default::default()
            },
        )?
        .pop() else {
            continue;
        };
        let event = record.data()?;
        if event["message"]["isError"] == true {
            continue;
        }
        let Some(result) = event.pointer("/meta/result") else {
            continue;
        };
        let Some(call) = crate::session_history::load_events(
            &conn,
            task,
            &crate::session_history::EventQuery {
                through: Some(seq - 1),
                kinds: &["tool/call", "context/tool_call"],
                call_id: Some(call_id),
                newest: Some(1),
                ..Default::default()
            },
        )?
        .pop() else {
            continue;
        };
        let call = call.data()?;
        let name = call["name"].as_str().unwrap_or("");
        let args = match &call["arguments"] {
            Value::String(text) => serde_json::from_str::<Value>(text)?,
            value => value.clone(),
        };
        let asset = if name == "read_asset" {
            let Some(id) = result["asset_reference"]["asset_id"].as_str() else {
                continue;
            };
            reference(root, task, id, None)?
        } else if name == "refresh_asset" {
            let (tool, args) = refresh_request(
                root,
                task,
                args["asset_id"]
                    .as_str()
                    .context("refresh reference missing")?,
            )?;
            register(root, task, seq, &tool, &args, result)?
        } else if is_material_tool(name) {
            register(root, task, seq, name, &args, result)?
        } else {
            continue;
        };
        let mut shown: Value = serde_json::from_str(
            message["content"]
                .as_str()
                .context("tool result text missing")?,
        )?;
        let withheld = shown["error_code"] == "MATERIAL_OVER_CAPACITY";
        if withheld {
            shown["asset_reference"] = asset.clone();
        } else if name != "read_asset" {
            shown = delivered(&shown, &asset);
        }
        shown["delivery_reference"] = json!({"task_id":task,"event_seq":seq});
        message["content"] = json!(shown.to_string());
        message[REFERENCE] = asset;
        message["_asset_state"] = json!(if withheld { "not_delivered" } else { "loaded" });
    }
    let images: BTreeMap<String, Value> = messages
        .iter()
        .filter_map(|m| {
            m[REFERENCE]["artifact_id"]
                .as_str()
                .map(|id| (id.to_owned(), m[REFERENCE].clone()))
        })
        .collect();
    for message in messages {
        if message[REFERENCE].is_object() {
            continue;
        }
        let image = message["content"]
            .as_array()
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find_map(|b| b["artifact_id"].as_str().and_then(|id| images.get(id)))
            })
            .cloned();
        if let Some(reference) = image {
            message[REFERENCE] = reference;
            message["_asset_state"] = json!("image_selected");
        }
    }
    Ok(())
}
