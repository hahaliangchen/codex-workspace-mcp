//! Host-owned file baselines for Agent edits. External MCP keeps its own contract.
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{fs, path::{Path, PathBuf}};

fn connection(root: &Path) -> Result<rusqlite::Connection> {
    let conn = crate::agent_service::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_file_baselines (
        task_id TEXT NOT NULL, workspace TEXT NOT NULL, path TEXT NOT NULL,
        exists_at_baseline INTEGER NOT NULL, bytes BLOB,
        PRIMARY KEY(task_id, workspace, path));
        CREATE TABLE IF NOT EXISTS agent_parallel_file_owners (
        task_id TEXT NOT NULL, workspace TEXT NOT NULL, path TEXT NOT NULL, owner TEXT NOT NULL,
        PRIMARY KEY(task_id, workspace, path));")?;
    Ok(conn)
}

fn key(workspace: &Path, raw: &str) -> Result<(String, String, PathBuf)> {
    let root = workspace.canonicalize()?;
    let path = crate::file_edit::workspace_path(&root, raw)?;
    let relative = path.strip_prefix(&root)?.to_string_lossy().replace('\\', "/");
    Ok((root.to_string_lossy().into_owned(), relative, path))
}

fn baseline(root: &Path, task: &str, workspace: &str, path: &str) -> Result<Option<Option<Vec<u8>>>> {
    let conn = connection(root)?;
    Ok(conn.query_row("SELECT exists_at_baseline, bytes FROM agent_file_baselines WHERE task_id=?1 AND workspace=?2 AND path=?3",
        params![task, workspace, path], |r| {
            let exists: bool = r.get(0)?;
            let bytes: Option<Vec<u8>> = r.get(1)?;
            Ok(if exists { bytes } else { None })
        }).optional()?)
}

fn save(root: &Path, task: &str, workspace: &str, path: &str, bytes: Option<&[u8]>, replace: bool) -> Result<()> {
    let conn = connection(root)?;
    let sql = if replace {
        "INSERT INTO agent_file_baselines(task_id,workspace,path,exists_at_baseline,bytes) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(task_id,workspace,path) DO UPDATE SET exists_at_baseline=excluded.exists_at_baseline,bytes=excluded.bytes"
    } else {
        "INSERT OR IGNORE INTO agent_file_baselines(task_id,workspace,path,exists_at_baseline,bytes) VALUES (?1,?2,?3,?4,?5)"
    };
    conn.execute(sql, params![task, workspace, path, bytes.is_some(), bytes])?;
    Ok(())
}

/// Only save a read snapshot if it matches the actual version delivered by the tool.
pub(super) fn observe(root: &Path, task: &str, workspace: &Path, args: &Value, result: &Value) -> Result<Option<Value>> {
    let Some(raw) = args["path"].as_str() else { return Ok(None); };
    let Some(hash) = result["code_hash"].as_str() else { return Ok(None); };
    let (work, rel, path) = key(workspace, raw)?;
    let current = crate::file_edit::snapshot(&path)?;
    if current.as_deref().map(crate::symbol_description::content_hash).as_deref() != Some(hash) {
        return Ok(Some(json!({"error_code":"ASSET_CONFLICT", "path":rel, "workspace":work,
            "message":"The file changed while it was being read. No edit was made; ask the user how to proceed."})));
    }
    save(root, task, &work, &rel, current.as_deref(), false)?;
    Ok(None)
}

pub(super) struct Prepared {
    workspace: String,
    relative: String,
    target: PathBuf,
    base: Option<Vec<u8>>,
    proposed: Vec<u8>,
    request_only: bool,
    pub args: Value,
}

/// Compute the intended content from the saved baseline, never from a conflicting file.
/// After an external change the edit may no longer fit the baseline; the request
/// itself is then the candidate, and the caller stops on the conflict.
pub(super) fn prepare(root: &Path, task: &str, workspace: &Path, name: &str, mut args: Value) -> Result<Prepared> {
    let raw = args["path"].as_str().context("file path required")?;
    let (work, rel, path) = key(workspace, raw)?;
    let base = match baseline(root, task, &work, &rel)? {
        Some(base) => base,
        None => {
            let current = crate::file_edit::snapshot(&path)?;
            save(root, task, &work, &rel, current.as_deref(), false)?;
            baseline(root, task, &work, &rel)?.context("file baseline missing")?
        }
    };
    let changed = crate::file_edit::snapshot(&path)?.as_deref() != base.as_deref();
    let (proposed, request_only) = match proposed_content(name, &args, base.as_deref()) {
        Ok(bytes) => (bytes, false),
        Err(_) if changed => (serde_json::to_vec_pretty(&args)?, true),
        Err(error) => return Err(error),
    };
    let object = args.as_object_mut().context("file arguments must be an object")?;
    object.remove("expected_code_hash");
    if let Some(bytes) = &base {
        object.insert("expected_code_hash".into(), json!(crate::symbol_description::content_hash(bytes)));
    }
    Ok(Prepared { workspace: work, relative: rel, target: path, base, proposed, request_only, args })
}

fn proposed_content(name: &str, args: &Value, base: Option<&[u8]>) -> Result<Vec<u8>> {
    Ok(match name {
        "write_file" => args["content"].as_str().context("content required")?.as_bytes().to_vec(),
        "edit_file" => {
            let source = std::str::from_utf8(base.context("cannot edit a missing baseline")?)?;
            let edits: Vec<crate::tools::TextEdit> = serde_json::from_value(args["edits"].clone())?;
            crate::file_edit::text_edits(source, &edits)?.into_bytes()
        }
        "replace_range" => {
            let source = std::str::from_utf8(base.context("cannot edit a missing baseline")?)?;
            let start = args["start_line"].as_u64().context("start_line required")? as usize;
            let end = args["end_line"].as_u64().context("end_line required")? as usize;
            let replacement = args["replacement"].as_str().context("replacement required")?;
            crate::file_edit::line_replacement(source, start, end, replacement, args["expected_old_text"].as_str())?.into_bytes()
        }
        _ => anyhow::bail!("unsupported managed file operation"),
    })
}

impl Prepared {
    pub(super) fn other_parallel_writer(&self, root: &Path, task: &str, owner: &str) -> Result<bool> {
        let conn = connection(root)?;
        let previous: Option<String> = conn.query_row(
            "SELECT owner FROM agent_parallel_file_owners WHERE task_id=?1 AND workspace=?2 AND path=?3",
            params![task,self.workspace,self.relative], |row| row.get(0),
        ).optional()?;
        Ok(previous.is_some_and(|previous| previous != owner))
    }
    pub(super) fn record_parallel_writer(&self, root: &Path, task: &str, owner: &str) -> Result<()> {
        let conn = connection(root)?;
        conn.execute("INSERT INTO agent_parallel_file_owners(task_id,workspace,path,owner) VALUES (?1,?2,?3,?4) ON CONFLICT(task_id,workspace,path) DO UPDATE SET owner=excluded.owner",
            params![task,self.workspace,self.relative,owner])?;
        Ok(())
    }
    pub(super) fn conflict(&self) -> Result<bool> {
        Ok(crate::file_edit::snapshot(&self.target)?.as_deref() != self.base.as_deref())
    }
    pub(super) fn preserve(&self, root: &Path) -> Result<Value> {
        let dir = root.join(".codex-workspace-mcp").join("conflicts");
        fs::create_dir_all(&dir)?;
        let suffix = self.target.file_name().context("file name required")?.to_string_lossy();
        let (kind, extension) = if self.request_only { ("request", ".request.json") } else { ("file", "") };
        let path = dir.join(format!("{}-{}{}", crate::agent_service::uuid_like(), suffix, extension));
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
        std::io::Write::write_all(&mut file, &self.proposed)?;
        file.sync_all()?;
        Ok(json!({"error_code":"ASSET_CONFLICT", "path":self.relative,
            "workspace":self.workspace,"candidate":kind,"candidate_path":path.to_string_lossy(),
            "message":"The file changed outside this task. The current file and the proposed change are preserved; ask the user how to proceed. Do not merge or retry automatically."}))
    }
    pub(super) fn committed(&self, root: &Path, task: &str) -> Result<bool> {
        let current = crate::file_edit::snapshot(&self.target)?;
        if current.as_deref() != Some(self.proposed.as_slice()) {
            return Ok(false);
        }
        save(root, task, &self.workspace, &self.relative, current.as_deref(), true)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("agent-managed-files-{}", crate::agent_service::uuid_like()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn conflict_preserves_current_and_proposed_content() -> Result<()> {
        let root = root();
        let path = root.join("document.txt");
        fs::write(&path, "original\n")?;
        let result = json!({"code_hash":crate::symbol_description::content_hash(b"original\n")});
        observe(&root, "task", &root, &json!({"path":"document.txt"}), &result)?;
        fs::write(&path, "external\n")?;
        let prepared = prepare(&root, "task", &root, "edit_file", json!({"path":"document.txt","edits":[{"old_text":"original","new_text":"agent"}]}))?;
        assert!(prepared.conflict()?);
        let saved = prepared.preserve(&root)?;
        assert_eq!(fs::read_to_string(&path)?, "external\n");
        assert_eq!(fs::read_to_string(saved["candidate_path"].as_str().unwrap())?, "agent\n");
        assert_eq!(saved["error_code"], "ASSET_CONFLICT");
        assert_eq!(saved["candidate"], "file");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn parallel_writers_record_the_owner_and_preserve_the_second_candidate() -> Result<()> {
        let root = root();
        let path = root.join("document.txt");
        fs::write(&path, "original")?;
        let first = prepare(&root, "task", &root, "write_file", json!({"path":"document.txt","content":"first"}))?;
        assert!(!first.other_parallel_writer(&root, "task", "one")?);
        fs::write(&path, "first")?;
        assert!(first.committed(&root, "task")?);
        first.record_parallel_writer(&root, "task", "one")?;
        let second = prepare(&root, "task", &root, "write_file", json!({"path":"document.txt","content":"second"}))?;
        assert!(!second.other_parallel_writer(&root, "task", "one")?);
        assert!(second.other_parallel_writer(&root, "task", "two")?);
        let conflict = second.preserve(&root)?;
        assert_eq!(fs::read_to_string(&path)?, "first");
        assert_eq!(fs::read_to_string(conflict["candidate_path"].as_str().unwrap())?, "second");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn successful_writes_update_baseline_without_model_hash() -> Result<()> {
        let root = root();
        let path = root.join("document.txt");
        fs::write(&path, "first")?;
        let result = json!({"code_hash":crate::symbol_description::content_hash(b"first")});
        observe(&root, "task", &root, &json!({"path":"document.txt"}), &result)?;
        let prepared = prepare(&root, "task", &root, "write_file", json!({"path":"document.txt","content":"second"}))?;
        assert!(!prepared.conflict()?);
        assert_eq!(prepared.args["expected_code_hash"], crate::symbol_description::content_hash(b"first"));
        fs::write(&path, "second")?;
        assert!(prepared.committed(&root, "task")?);
        let next = prepare(&root, "task", &root, "write_file", json!({"path":"document.txt","content":"third"}))?;
        assert!(!next.conflict()?);
        assert_eq!(next.args["expected_code_hash"], crate::symbol_description::content_hash(b"second"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn subsequent_read_does_not_replace_original_baseline() -> Result<()> {
        let root = root();
        let path = root.join("document.txt");
        fs::write(&path, "before")?;
        observe(&root, "task", &root, &json!({"path":"document.txt"}), &json!({"code_hash":crate::symbol_description::content_hash(b"before")}))?;
        fs::write(&path, "external")?;
        observe(&root, "task", &root, &json!({"path":"document.txt"}), &json!({"code_hash":crate::symbol_description::content_hash(b"external")}))?;
        let prepared = prepare(&root, "task", &root, "write_file", json!({"path":"document.txt","content":"agent"}))?;
        assert!(prepared.conflict()?);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn an_edit_written_against_an_external_change_stops_as_a_conflict() -> Result<()> {
        let root = root();
        let path = root.join("document.txt");
        fs::write(&path, "original\n")?;
        observe(&root, "task", &root, &json!({"path":"document.txt"}), &json!({"code_hash":crate::symbol_description::content_hash(b"original\n")}))?;
        fs::write(&path, "external\n")?;
        observe(&root, "task", &root, &json!({"path":"document.txt"}), &json!({"code_hash":crate::symbol_description::content_hash(b"external\n")}))?;
        // The edit fits only the external version, never the task's baseline.
        let prepared = prepare(&root, "task", &root, "edit_file", json!({"path":"document.txt","edits":[{"old_text":"external","new_text":"agent"}]}))?;
        assert!(prepared.conflict()?);
        let saved = prepared.preserve(&root)?;
        assert_eq!(saved["error_code"], "ASSET_CONFLICT");
        assert_eq!(saved["candidate"], "request");
        assert_eq!(fs::read_to_string(&path)?, "external\n");
        let request: Value = serde_json::from_str(&fs::read_to_string(saved["candidate_path"].as_str().unwrap())?)?;
        assert_eq!(request["edits"][0]["old_text"], "external");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_tools_manage_versions_and_preserve_a_conflicting_edit() -> Result<()> {
        let root = root();
        fs::write(root.join("document.txt"), "first")?;
        let state = crate::agent_service::tests::test_state(&root, "127.0.0.1:1".parse()?);
        let conn = crate::agent_service::open_db(&root)?;
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", [])?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let session = super::super::Session {
            agent_id: None, task_node_id: None, step_node_id: None, plugin: None,
            state: &state, task: "task", model: "fake", turn: 1, cancel: &cancel,
            ban_execution: false, scope: "main".into(), parent: None, input_budget: 100_000,
        };
        super::super::business_tool(&session, "node", "read_file", json!({"path":"document.txt"}), "read").await.context("read through agent")?;
        let written = super::super::business_tool(&session, "node", "write_file", json!({"path":"document.txt","content":"second"}), "write").await.context("write through agent")?;
        assert_eq!(written["changed"], true);
        fs::write(root.join("document.txt"), "external")?;
        let conflict = super::super::business_tool(&session, "node", "edit_file", json!({"path":"document.txt","edits":[{"old_text":"second","new_text":"proposed"}]}), "edit").await.context("conflicting edit through agent")?;
        assert_eq!(conflict["error_code"], "ASSET_CONFLICT");
        assert_eq!(fs::read_to_string(root.join("document.txt"))?, "external");
        assert_eq!(fs::read_to_string(conflict["candidate_path"].as_str().unwrap())?, "proposed");
        drop(conn);
        drop(state);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[tokio::test]
    async fn conflict_stops_the_turn_without_retry() -> Result<()> {
        use axum::{Json, Router, routing::post};
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let root = root();
        fs::write(root.join("document.txt"), "original")?;
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let external = root.join("document.txt");
        let app = Router::new().route("/v1/chat/completions", post(move |Json(_body): Json<Value>| {
            let n = requests.fetch_add(1, Ordering::SeqCst);
            let (name, args) = match n {
                0 => ("create_task", json!({"goal":"Update the file"})),
                1 => ("execute_program", json!({"source":"function main(){return work();} /** @virtual Update the file. */ function work(){}"})),
                2 => ("invoke_library", json!({"method":"io.read","args":{"path":"document.txt"}})),
                3 => {
                    fs::write(&external, "external").unwrap();
                    ("invoke_library", json!({"method":"io.write","args":{"path":"document.txt","content":"agent"}}))
                }
                _ => panic!("conflict must stop before another model request"),
            };
            let message = json!({"role":"assistant","tool_calls":[{"id":format!("call-{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
            async move { Json(json!({"choices":[{"finish_reason":"tool_calls","message":message}]})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let state = crate::agent_service::tests::test_state(&root, listener.local_addr()?);
        let conn = crate::agent_service::open_db(&root)?;
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", [])?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        super::super::run_task(state, "task".into(), "fake".into(), "Update the file".into(), 10, tokio_util::sync::CancellationToken::new(), 1, false).await?;
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 4);
        assert_eq!(fs::read_to_string(root.join("document.txt"))?, "external");
        let raw: String = conn.query_row("SELECT data FROM agent_task_events WHERE kind='tool/result' AND json_extract(data,'$.meta.result.error_code')='ASSET_CONFLICT'", [], |r| r.get(0))?;
        let result: Value = serde_json::from_str(&raw)?;
        let copy = result["meta"]["result"]["candidate_path"].as_str().context("candidate path")?;
        assert_eq!(fs::read_to_string(copy)?, "agent");
        let status: String = conn.query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r| r.get(0))?;
        assert_eq!(status, "failed");
        drop(conn);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }
}
