//! Durable, content-addressed material references. This module never ranks or
//! evicts assets. Only an explicit AI context edit can replace a body by a ref.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
};

pub const REFERENCE: &str = "_asset_reference";

fn connection(root: &Path) -> Result<Connection> {
    let conn = crate::agent_service::open_db(root)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_context_assets(
        task_id TEXT NOT NULL, asset_id TEXT NOT NULL, reference_json TEXT NOT NULL,
        source_tool TEXT NOT NULL, source_arguments TEXT NOT NULL,
        payload_path TEXT NOT NULL, payload_hash TEXT NOT NULL,
        PRIMARY KEY(task_id,asset_id));",
    )?;
    Ok(conn)
}

pub fn is_material_tool(name: &str) -> bool {
    matches!(
        name,
        "read_attachment"
            | "read_file"
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
        .or_else(|| args["path"].as_str())
        .or_else(|| args["file_path"].as_str());
    let kind = if image.is_some() {
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
            &json!({"reference":reference}),
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
/// Always read the exact saved version. A live re-read requires refresh_asset.
pub fn read(root: &Path, task: &str, args: &Value, through: Option<i64>) -> Result<Value> {
    let id = args["asset_id"].as_str().context("asset_id is required")?;
    let reference = reference(root, task, id, through)?;
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
    let text = String::from_utf8(bytes)?;
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
    json!({"asset_reference":reference,"context_state":"reference_only",
        "instruction":"The body remains saved. Use read_asset only if its contents are needed again."}).to_string()
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
            message["content"] = json!(receipt(&message[REFERENCE]));
            message["_asset_state"] = json!("reference_only");
            message
                .as_object_mut()
                .unwrap()
                .remove("viewed_by_context_agent");
        }
    }
    Ok(())
}
/// Preserve every asset exactly as currently loaded unless the AI explicitly
/// requested reference-only. Compression turns completed tool results into
/// attributed data messages, not summaries written by the host.
pub fn retained(messages: &[Value], plan: &Value) -> Result<Vec<Value>> {
    let mut decisions = BTreeMap::new();
    let mut excerpts = BTreeMap::<String, BTreeSet<i64>>::new();
    if !plan.is_null() {
        for item in plan.as_array().context("asset_context must be an array")? {
            let id = item["asset_id"]
                .as_str()
                .context("asset_context requires asset_id")?;
            let mode = item["mode"]
                .as_str()
                .context("asset_context requires mode")?;
            ensure!(
                matches!(mode, "keep" | "reference" | "excerpt"),
                "asset mode must be keep, reference or excerpt"
            );
            if mode == "excerpt" {
                let selected=item["event_seqs"].as_array().context("excerpt mode requires event_seqs from already loaded delivery_reference values")?.iter()
                    .map(|value|value.as_i64().context("excerpt event_seq must be an integer")).collect::<Result<BTreeSet<_>>>()?;
                ensure!(!selected.is_empty(), "excerpt selection must not be empty");
                for seq in &selected {
                    ensure!(
                        messages.iter().any(|m| m[REFERENCE]["asset_id"] == id
                            && m["source"]["seq"] == *seq
                            && m["_asset_state"] == "loaded"
                            && m[REFERENCE]["kind"] != "image"),
                        "EXCERPT_NOT_LOADED: asset {id}, event {seq}; read the desired page first, then select its delivery_reference"
                    );
                }
                excerpts.insert(id.to_owned(), selected);
            }
            ensure!(
                decisions.insert(id.to_owned(), mode.to_owned()).is_none(),
                "duplicate asset_context decision: {id}"
            );
        }
    }
    let known: BTreeSet<_> = messages
        .iter()
        .filter_map(|m| m[REFERENCE]["asset_id"].as_str())
        .collect();
    for id in decisions.keys() {
        ensure!(known.contains(id.as_str()), "ASSET_NOT_IN_CONTEXT: {id}");
    }
    let mut refs_written = BTreeSet::new();
    let mut kept = Vec::new();
    for original in messages.iter().filter(|m| m[REFERENCE].is_object()) {
        let reference = &original[REFERENCE];
        let id = reference["asset_id"]
            .as_str()
            .context("asset reference has no id")?;
        let mut message = original.clone();
        if let Some(selected) = excerpts.get(id) {
            let selected_body = original["source"]["seq"]
                .as_i64()
                .is_some_and(|seq| selected.contains(&seq))
                && original["_asset_state"] == "loaded";
            if !selected_body {
                if !refs_written.insert(id.to_owned()) {
                    continue;
                }
                message["content"] = json!(receipt(reference));
                message["_asset_state"] = json!("reference_only");
                message
                    .as_object_mut()
                    .unwrap()
                    .remove("viewed_by_context_agent");
            }
        }
        if decisions.get(id).is_some_and(|mode| mode == "reference") {
            if !refs_written.insert(id.to_owned()) {
                continue;
            }
            message["content"] = json!(receipt(reference));
            message["_asset_state"] = json!("reference_only");
            message
                .as_object_mut()
                .unwrap()
                .remove("viewed_by_context_agent");
        }
        let object = message.as_object_mut().unwrap();
        object.remove("tool_call_id");
        object.insert("role".into(), json!("user"));
        object.insert("name".into(), json!("saved_asset"));
        kept.push(message);
    }
    Ok(kept)
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
            "content_chars":message["content"].as_str().map(|s|s.chars().count())}),
        );
    }
    json!({"assets":entries.into_values().collect::<Vec<_>>(),"note":"Loaded material remains in context until an explicit AI edit. image_selected is not proof of pixel delivery."})
}

/// Add references to old/pending tool messages without changing their original
/// historical results or re-running any tool. Existing loaded bodies stay loaded.
pub fn hydrate(root: &Path, task: &str, messages: &mut [Value]) -> Result<()> {
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
