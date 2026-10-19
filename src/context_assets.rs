//! Durable, content-addressed material references. This module never ranks or
//! evicts assets during execution. Compaction supplies references and restores
//! only the model's requested bodies plus the currently active skill rules.
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
    if let Some(reference) = register_remote_file(root, task, seq, tool, args, result)? {
        return Ok(reference);
    }
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
/// Remote SVG source already belongs to the PPT service. Store only a handle
/// and fingerprint, not another local copy. Original exchanges remain in audit
/// history, including unsaved/failed write arguments.
fn register_remote_file(
    root: &Path,
    task: &str,
    seq: i64,
    tool: &str,
    args: &Value,
    result: &Value,
) -> Result<Option<Value>> {
    let generated = tool == "tool_arguments"
        && result["tool_calls"]
            .as_array()
            .is_some_and(|calls| calls.len() == 1)
        && result["tool_calls"][0]["function"]["name"] == "ppt_master__write_file";
    if !tool.starts_with("ppt_master__") && !generated {
        return Ok(None);
    }
    let call_args: Value = if generated {
        serde_json::from_str(
            result["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap_or("{}"),
        )?
    } else {
        args.clone()
    };
    let data = result
        .pointer("/result/data")
        .or_else(|| result.get("data"))
        .unwrap_or(result);
    let path = data["path"].as_str().or_else(|| call_args["path"].as_str());
    let content = data["content"]
        .as_str()
        .or_else(|| call_args["content"].as_str());
    let (Some(path), Some(content)) = (path, content) else {
        return Ok(None);
    };
    if !path.starts_with("svg_output/") || !path.ends_with(".svg") {
        return Ok(None);
    }
    let project = result["project_id"]
        .as_u64()
        .or_else(|| data["project_id"].as_u64())
        .or_else(|| call_args["project_id"].as_u64())
        .or(crate::ppt_sessions::project(root, task)?);
    let Some(project) = project.filter(|p| *p > 0) else {
        return Ok(None);
    };
    let hash = crate::visual_artifacts::hash(content.as_bytes());
    let identity =
        json!({"task":task,"plugin":"ppt-master","project":project,"path":path,"hash":hash});
    let id = format!(
        "asset-{}",
        crate::visual_artifacts::hash(identity.to_string().as_bytes())
    );
    let mut conn = connection(root)?;
    if let Some(reference) = lookup(&conn, task, &id)? {
        return Ok(Some(reference));
    }
    let mut reference = json!({"asset_id":id,"kind":"remote_file","storage":"plugin",
        "plugin":"ppt-master","project_id":project,"source_path":path,"content_hash":hash,"bytes":content.len(),
        "revision":data["revision"],"availability":"verify_on_read",
        "history_reference":{"task_id":task,"event_seq":seq,"include_event":true},
        "read_reference":{"tool":"read_asset","arguments":{"asset_id":id}},
        "resource":{"entry":"resource","project_id":project,"path":path}});
    if let Ok(Some(remote)) = crate::ppt_sessions::connection(root, task) {
        reference["download_url"] = json!(remote.resource_url(project, path)?);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT OR IGNORE INTO agent_context_assets VALUES (?1,?2,?3,?4,?5,'',?6)",
        params![
            task,
            id,
            reference.to_string(),
            tool,
            json!({"project_id":project,"path":path}).to_string(),
            hash
        ],
    )?;
    crate::agent_service::append_event_tx(
        &tx,
        task,
        "asset/registered",
        &json!({"reference":reference}),
        None,
    )?;
    tx.commit()?;
    Ok(Some(reference))
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
/// Hand off an asset without loading its body into the recipient's window.
pub fn referenced(message: Value, reference: &Value) -> Value {
    let mut message = tagged(message, reference);
    reference_only(&mut message);
    message
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
fn reference_only(message: &mut Value) {
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
fn presentation_key(message: &Value) -> (String, String) {
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
    let mut emitted_refs = BTreeSet::new();
    let mut bodies_written = BTreeSet::new();
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
                reference_only(&mut message);
            }
        }
        if decisions.get(id).is_some_and(|mode| mode == "reference") {
            if !refs_written.insert(id.to_owned()) {
                continue;
            }
            reference_only(&mut message);
        }
        // Keep distinct excerpts; deduplicate only identical payloads of the
        // same asset, ignoring delivery metadata and native call identifiers.
        if message["_asset_state"] != "reference_only"
            && !bodies_written.insert(presentation_key(&message))
        {
            continue;
        }
        if message["_asset_state"] == "reference_only" {
            if !emitted_refs.insert(id.to_owned()) {
                continue;
            }
            reference_only(&mut message);
        }
        if let Some(calls) = message.as_object_mut().unwrap().remove("tool_calls") {
            // An argument snapshot is data after a full narrative compaction,
            // rather than an assistant call without native result partners.
            message["content"] = if message["_asset_state"] == "reference_only" {
                json!(receipt(reference))
            } else {
                json!(
                    json!({"asset_reference":reference,
                    "original_result":{"tool_calls":calls},"assistant_note":message["content"]})
                    .to_string()
                )
            };
            message.as_object_mut().unwrap().remove("reasoning_content");
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
            "content_chars":message["content"].as_str().map(|s|s.chars().count()),
            "tool_calls":message["tool_calls"].as_array().map(|calls|calls.iter().map(|call|json!({"id":call["id"],"name":call["function"]["name"]})).collect::<Vec<_>>())}),
        );
    }
    json!({"assets":entries.into_values().collect::<Vec<_>>(),"note":"Loaded material remains in context until an explicit AI edit. image_selected is not proof of pixel delivery."})
}

/// The reviewer sees the entire conversation with asset bodies replaced by
/// handles. This is a view only; it does not release anything in execution.
pub fn review_view(messages: &[Value]) -> Result<Value> {
    let catalog = catalog(messages);
    let mut view = messages.to_vec();
    for message in &mut view {
        if let Some(id) = message[REFERENCE]["asset_id"].as_str().map(str::to_owned) {
            let parsed = message["content"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            let original = parsed.get("original_result").unwrap_or(&parsed);
            let mut evidence = json!({"asset_id":id,"body":"hidden in review only"});
            // Exact status/diagnostic text distinguishes accepted, running,
            // failed and checked pages. Never write a host summary of evidence.
            for key in [
                "status",
                "message",
                "error",
                "error_code",
                "analysis",
                "task_id",
                "job_id",
                "agent_id",
                "name",
                "acceptance",
                "artifact_id",
                "source_revision",
                "id",
                "tool",
                "project_id",
                "page_index",
                "path",
                "revision",
            ] {
                if let Some(value) = original
                    .get(key)
                    .filter(|v| v.is_string() || v.is_number() || v.is_boolean())
                {
                    if value.as_str().is_none_or(|s| s.chars().count() <= 4000) {
                        evidence[key] = value.clone();
                    }
                }
            }
            if let Some(report) = original
                .pointer("/report/result/report")
                .and_then(Value::as_str)
                .filter(|s| s.chars().count() <= 4000)
            {
                evidence["delegate_report"] = json!(report);
            }
            // Async receipts wrap their final status in result/data. Preserve
            // exact identity/status fields so an older running receipt cannot
            // obscure a later success for the same task/page.
            fn status_fields(value: &Value, depth: usize) -> Value {
                let mut fields = serde_json::Map::new();
                for key in [
                    "id",
                    "task_id",
                    "tool",
                    "status",
                    "project_id",
                    "page_index",
                    "path",
                    "revision",
                    "error",
                    "message",
                ] {
                    if let Some(v) = value.get(key).filter(|v| {
                        v.is_number()
                            || v.is_boolean()
                            || v.as_str().is_some_and(|s| s.chars().count() <= 1000)
                    }) {
                        fields.insert(key.into(), v.clone());
                    }
                }
                if depth > 0 {
                    for key in ["result", "data"] {
                        if value[key].is_object() {
                            let child = status_fields(&value[key], depth - 1);
                            if child.as_object().is_some_and(|o| !o.is_empty()) {
                                fields.insert(key.into(), child);
                            }
                        }
                    }
                }
                json!(fields)
            }
            for key in ["result", "data"] {
                if original[key].is_object() {
                    let fields = status_fields(&original[key], 2);
                    if fields.as_object().is_some_and(|o| !o.is_empty()) {
                        evidence[key] = fields;
                    }
                }
            }
            for key in ["sources", "version_checks"] {
                if let Some(values) = original[key]
                    .as_array()
                    .filter(|v| v.len() <= crate::visual_artifacts::MAX_IMAGES)
                {
                    evidence[key] = json!(
                        values
                            .iter()
                            .map(|v| {
                                let mut fields = serde_json::Map::new();
                                for key in [
                                    "artifact_id",
                                    "page_index",
                                    "project_id",
                                    "source_revision",
                                    "checked_revision",
                                    "current_revision",
                                    "freshness",
                                    "path",
                                ] {
                                    if let Some(value) = v.get(key).filter(|v| {
                                        v.is_number() || v.as_str().is_some_and(|s| s.len() < 1024)
                                    }) {
                                        fields.insert(key.into(), value.clone());
                                    }
                                }
                                json!(fields)
                            })
                            .collect::<Vec<_>>()
                    );
                }
            }
            if message["role"] == "assistant" && message["tool_calls"].is_array() {
                for call in message["tool_calls"].as_array_mut().unwrap() {
                    call["function"]["arguments"] = json!(
                        json!({"asset_id":id,"arguments":"hidden in review only"}).to_string()
                    );
                }
            } else {
                message["content"] = json!(evidence.to_string());
            }
            message["asset_id"] = json!(id);
            let object = message.as_object_mut().unwrap();
            object.remove(REFERENCE);
            object.remove("_asset_state");
            object.remove("viewed_by_context_agent");
        }
        message.as_object_mut().unwrap().remove("reasoning_content");
    }
    Ok(json!({"conversation":view,"asset_catalog":catalog,
        "instruction":"Asset bodies are hidden only in this review view. They are still loaded in execution. Short status and diagnostic text is attributed tool evidence. Choose reference only when not needed for the next actions; omitted assets remain unchanged. Never claim you inspected hidden source bodies or pixels."}))
}

/// A single compression request sees narrative and exact execution receipts,
/// never source bodies, pixels, historical reasoning or repeated catalogs.
pub fn compaction_view(messages: &[Value]) -> Result<Value> {
    let view = review_view(messages)?;
    let assets = view["asset_catalog"]["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            let mut handle = serde_json::Map::new();
            for key in [
                "asset_id",
                "kind",
                "source_tool",
                "source_path",
                "source_paths",
                "skill",
                "operation",
                "bytes",
                "revision",
                "project_id",
                "artifact_id",
                "content_hash",
            ] {
                if let Some(v) = entry["asset_reference"].get(key).filter(|v| !v.is_null()) {
                    handle.insert(key.into(), v.clone());
                }
            }
            json!(handle)
        })
        .collect::<Vec<_>>();
    let conversation = view["conversation"].as_array().unwrap().iter().map(|message| {
        let mut m = message.clone();
        if let Some(seq) = m["source"]["seq"].as_i64() {
            m["event_seq"] = json!(seq);
        }
        let object = m.as_object_mut().unwrap();
        object.remove("source");
        // Native IDs and transport envelopes are not needed to summarize a
        // completed operation. Preserve the actual name and short evidence.
        object.remove("tool_call_id");
        if let Some(calls) = object.remove("tool_calls") {
            m["operations"] = json!(calls.as_array().unwrap().iter()
                .map(|c| json!({"tool":c["function"]["name"], "arguments":c["function"]["arguments"]}))
                .collect::<Vec<_>>());
        }
        m
    }).collect::<Vec<_>>();
    Ok(json!({"conversation":conversation,"assets":assets}))
}

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

/// Validate the complete decision before changing a single message. No heuristic
/// chooses relevance; omitted/keep means retain exactly, including loaded pixels.
pub fn apply_review(messages: &[Value], plan: &Value) -> Result<Vec<Value>> {
    let mut seen = BTreeSet::new();
    let mut released = Vec::new();
    for item in plan.as_array().context("asset_context must be an array")? {
        let id = item["asset_id"].as_str().context("asset_id required")?;
        ensure!(seen.insert(id), "duplicate asset decision: {id}");
        ensure!(
            messages.iter().any(|m| m[REFERENCE]["asset_id"] == id),
            "ASSET_NOT_IN_CONTEXT: {id}"
        );
        match item["mode"].as_str() {
            Some("keep") => {}
            Some("reference") => {
                ensure!(
                    item["reason"]
                        .as_str()
                        .is_some_and(|r| !r.trim().is_empty()),
                    "reference decision requires a reason"
                );
                released.push(id.to_owned());
            }
            _ => anyhow::bail!("asset mode must be keep or reference"),
        }
    }
    let mut result = messages.to_vec();
    release(&mut result, &released)?;
    let mut bodies = BTreeSet::new();
    for message in &mut result {
        if message[REFERENCE]["asset_id"].is_string() {
            if message["_asset_state"] != "reference_only"
                && !bodies.insert(presentation_key(message))
            {
                // Keep native call/result partners intact; only the duplicate
                // payload becomes a receipt for the very same immutable asset.
                reference_only(message);
            }
        }
    }
    Ok(result)
}

/// A thorough review must account for every loaded asset, including explicit
/// reasons to keep uncertain material. Partial decisions cannot look complete.
pub fn validate_complete_review(messages: &[Value], plan: &Value) -> Result<()> {
    let decisions = plan.as_array().context("asset_context must be an array")?;
    let loaded: BTreeSet<_> = messages
        .iter()
        .filter(|m| m["_asset_state"] != "reference_only")
        .filter_map(|m| m[REFERENCE]["asset_id"].as_str())
        .collect();
    for id in loaded {
        ensure!(
            decisions.iter().any(|d| d["asset_id"] == id
                && matches!(d["mode"].as_str(), Some("keep" | "reference"))
                && d["reason"].as_str().is_some_and(|r| !r.trim().is_empty())),
            "ASSET_REVIEW_INCOMPLETE: decide keep/reference with a reason for loaded asset {id}; inspect every loaded asset in one plan"
        );
    }
    Ok(())
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
