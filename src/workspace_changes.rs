//! Per-turn before/after images. Never infer completed edits from model arguments.
use std::{collections::BTreeMap, fs, io::Write, path::{Component, Path, PathBuf}, sync::atomic::{AtomicU64, Ordering}};
use anyhow::{Context, Result, bail, ensure};
use axum::{Json, extract::{Path as RoutePath, Query, State}, http::StatusCode, response::{IntoResponse, Response}};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use crate::agent_service::AgentServiceState;

static MUTATIONS: Mutex<()> = Mutex::const_new(());
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
const FILE_LIMIT: u64 = 2 * 1024 * 1024;
const SNAPSHOT_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Clone, PartialEq)]
struct Image { exists: bool, bytes: Option<Vec<u8>>, stamp: String }
type Snapshot = BTreeMap<String, Image>;

fn connection(root: &Path) -> Result<Connection> {
    let conn = crate::agent_service::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_file_changes (
        task_id TEXT NOT NULL, turn INTEGER NOT NULL, path TEXT NOT NULL,
        before_exists INTEGER NOT NULL, before_bytes BLOB, after_exists INTEGER NOT NULL, after_bytes BLOB,
        added INTEGER NOT NULL, deleted INTEGER NOT NULL, binary INTEGER NOT NULL, coarse INTEGER NOT NULL,
        unavailable INTEGER NOT NULL, conflicted INTEGER NOT NULL DEFAULT 0, reverted INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(task_id,turn,path));
        CREATE TABLE IF NOT EXISTS agent_change_turns(task_id TEXT NOT NULL,turn INTEGER NOT NULL,incomplete INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(task_id,turn));")?;
    Ok(conn)
}

// Reject traversal and symlink/junction escapes, including nonexistent destinations.
fn safe_path(root: &Path, raw: &str) -> Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let path = Path::new(raw);
    let candidate = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };
    ensure!(!candidate.components().any(|c| matches!(c, Component::ParentDir)), "path traversal is not allowed");
    ensure!(!fs::symlink_metadata(&candidate).is_ok_and(|metadata|metadata.file_type().is_symlink()), "symlink mutations cannot be tracked safely");
    let mut ancestor = candidate.as_path();
    while !ancestor.exists() { ancestor = ancestor.parent().context("path has no existing ancestor")?; }
    let resolved = fs::canonicalize(ancestor)?;
    ensure!(resolved.starts_with(&root), "path is outside the tracked workspace");
    let suffix = candidate.strip_prefix(ancestor)?;
    let final_path = if suffix.as_os_str().is_empty() { resolved } else { resolved.join(suffix) };
    ensure!(final_path.starts_with(&root), "path is outside the tracked workspace");
    Ok(final_path)
}

fn image(path: &Path, budget: &mut usize) -> Result<Image> {
    let metadata = match fs::metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Image { exists:false, bytes:None, stamp:String::new() }),
        Err(error) => return Err(error.into()),
    };
    ensure!(metadata.is_file(), "not a regular file: {}", path.display());
    let stamp = format!("{}:{:?}", metadata.len(), metadata.modified()?);
    let bytes = if metadata.len() <= FILE_LIMIT && metadata.len() as usize <= *budget {
        let bytes = fs::read(path)?;
        *budget = budget.saturating_sub(bytes.len());
        Some(bytes)
    } else { None };
    Ok(Image { exists:true, bytes, stamp })
}

fn capture(root: &Path, name: &str, args: &Value) -> Result<Snapshot> {
    let mut snapshot = Snapshot::new();
    let mut budget = SNAPSHOT_LIMIT;
    if !crate::project_process::may_change_files(name) {
        let path = safe_path(root, args["path"].as_str().context("missing file path")?)?;
        let relative = path.strip_prefix(fs::canonicalize(root)?)?.to_string_lossy().replace('\\', "/");
        snapshot.insert(relative, image(&path, &mut budget)?);
    } else {
        let root = fs::canonicalize(root)?;
        let mut walker = ignore::WalkBuilder::new(&root);
        walker.standard_filters(false).follow_links(false).filter_entry(|entry| !matches!(entry.file_name().to_str(),
            Some(".git" | ".codex-workspace-mcp" | "node_modules" | "target" | "dist" | "build" | ".venv" | "__pycache__")));
        for entry in walker.build() {
            let entry = entry?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) { continue; }
            ensure!(snapshot.len() < 20_000, "workspace exceeds change-tracking file limit");
            let path = safe_path(&root, &entry.path().to_string_lossy())?;
            let relative = path.strip_prefix(&root)?.to_string_lossy().replace('\\', "/");
            snapshot.insert(relative, image(&path, &mut budget)?);
        }
    }
    Ok(snapshot)
}

fn same(a: &Image, b: &Image) -> bool {
    a.exists == b.exists && match (&a.bytes, &b.bytes) {
        (Some(a), Some(b)) => a == b,
        _ => a.stamp == b.stamp,
    }
}

fn text(bytes: &Option<Vec<u8>>) -> Option<&str> {
    let bytes = bytes.as_deref().unwrap_or(&[]);
    if bytes.contains(&0) { return None; }
    std::str::from_utf8(bytes).ok()
}

fn counts(before: &str, after: &str) -> (usize, usize, bool) {
    let a: Vec<_> = before.split_inclusive('\n').collect();
    let b: Vec<_> = after.split_inclusive('\n').collect();
    let prefix=a.iter().zip(&b).take_while(|(a,b)|a==b).count();
    let mut a=&a[prefix..]; let mut b=&b[prefix..];
    while !a.is_empty() && !b.is_empty() && a.last() == b.last() { a=&a[..a.len()-1]; b=&b[..b.len()-1]; }
    if a.len().saturating_mul(b.len()) > 4_000_000 { return (b.len(), a.len(), true); }
    let mut row = vec![0usize; b.len()+1];
    for line in a {
        let mut diagonal = 0;
        for (j, other) in b.iter().enumerate() {
            let old = row[j+1];
            row[j+1] = if line == other { diagonal+1 } else { row[j].max(old) };
            diagonal = old;
        }
    }
    let common = row[b.len()];
    (b.len()-common, a.len()-common, false)
}

fn record(root: &Path, task: &str, turn: usize, before: Snapshot, after: Snapshot) -> Result<Value> {
    let mut conn = connection(root)?;
    let tx = conn.transaction()?;
    let missing = Image { exists:false, bytes:None, stamp:String::new() };
    let keys: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
    for path in keys {
        let old = before.get(path).unwrap_or(&missing);
        let new = after.get(path).unwrap_or(&missing);
        if same(old, new) { continue; }
        let prior = tx.query_row("SELECT before_exists,before_bytes,after_exists,after_bytes,conflicted FROM agent_file_changes WHERE task_id=?1 AND turn=?2 AND path=?3",
            params![task,turn,path], |row| Ok((row.get::<_,bool>(0)?, row.get::<_,Option<Vec<u8>>>(1)?, row.get::<_,bool>(2)?,row.get::<_,Option<Vec<u8>>>(3)?,row.get::<_,bool>(4)?))).optional()?;
        let (original_exists, original_bytes, conflict) = match prior {
            Some((exists, bytes, last_exists, last_bytes, conflict)) => (exists, bytes, conflict || last_exists != old.exists || last_bytes != old.bytes),
            None => (old.exists, old.bytes.clone(), false),
        };
        if original_exists == new.exists && original_bytes == new.bytes && (!new.exists || new.bytes.is_some()) {
            tx.execute("DELETE FROM agent_file_changes WHERE task_id=?1 AND turn=?2 AND path=?3", params![task,turn,path])?;
            continue;
        }
        let unavailable = (original_exists && original_bytes.is_none()) || (new.exists && new.bytes.is_none());
        let (added, deleted, binary, coarse) = match (text(&original_bytes), text(&new.bytes)) {
            (Some(a), Some(b)) if !unavailable => { let (added, deleted, coarse)=counts(a,b); (added,deleted,false,coarse) },
            _ => (0,0,true,false),
        };
        tx.execute("INSERT INTO agent_file_changes(task_id,turn,path,before_exists,before_bytes,after_exists,after_bytes,added,deleted,binary,coarse,unavailable,conflicted,reverted)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,0)
            ON CONFLICT(task_id,turn,path) DO UPDATE SET after_exists=excluded.after_exists,after_bytes=excluded.after_bytes,
            added=excluded.added,deleted=excluded.deleted,binary=excluded.binary,coarse=excluded.coarse,
            unavailable=excluded.unavailable,conflicted=excluded.conflicted,reverted=0",
            params![task,turn,path,original_exists,original_bytes,new.exists,new.bytes,added,deleted,binary,coarse,unavailable,conflict])?;
    }
    tx.commit()?;
    summary(root, task, turn)
}

pub fn summary(root: &Path, task: &str, turn: usize) -> Result<Value> {
    let conn = connection(root)?;
    ensure!(conn.query_row("SELECT 1 FROM agent_tasks WHERE id=?1", [task], |_| Ok(())).optional()?.is_some(), "task not found");
    let mut stmt = conn.prepare("SELECT path,added,deleted,binary,coarse,unavailable,conflicted,reverted,before_exists,after_exists FROM agent_file_changes WHERE task_id=?1 AND turn=?2 ORDER BY path")?;
    let files = stmt.query_map(params![task,turn], |row| Ok(json!({"path":row.get::<_,String>(0)?,"added":row.get::<_,usize>(1)?,"deleted":row.get::<_,usize>(2)?,
        "binary":row.get::<_,bool>(3)?,"coarse":row.get::<_,bool>(4)?,"unavailable":row.get::<_,bool>(5)?,"conflicted":row.get::<_,bool>(6)?,"reverted":row.get::<_,bool>(7)?,
        "created":!row.get::<_,bool>(8)?,"removed":!row.get::<_,bool>(9)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let reverted = !files.is_empty() && files.iter().all(|file|file["reverted"] == true);
    let incomplete=conn.query_row("SELECT incomplete FROM agent_change_turns WHERE task_id=?1 AND turn=?2",params![task,turn],|row|row.get::<_,bool>(0)).optional()?.unwrap_or(false);
    let undo_available = !incomplete && !files.is_empty() && !reverted && files.iter().all(|file|file["unavailable"] == false && file["conflicted"] == false);
    Ok(json!({"turn":turn,"total":files.len(),"added":files.iter().map(|file|file["added"].as_u64().unwrap_or(0)).sum::<u64>(),
        "deleted":files.iter().map(|file|file["deleted"].as_u64().unwrap_or(0)).sum::<u64>(),"files":files,"reverted":reverted,"undo_available":undo_available,"complete":!incomplete}))
}

pub fn is_mutation(name: &str) -> bool { matches!(name, "write_file" | "replace_range" | "edit_file") || crate::project_process::may_change_files(name) }

pub async fn execute_tracked(state: AgentServiceState, task: String, turn: usize, name: String, args: Value, cancel: CancellationToken) -> Result<Value> {
    let _guard = tokio::select! { _=cancel.cancelled()=>return Err(anyhow::anyhow!("Tool execution cancelled")), guard=MUTATIONS.lock()=>guard };
    let root = state.workspace.root().to_path_buf();
    if !crate::project_process::may_change_files(&name) {
        // Boundary checks are mandatory even if optional snapshot capture fails.
        safe_path(&root,args["path"].as_str().context("missing file path")?)?;
    }
    let capture_root=root.clone(); let capture_name=name.clone(); let capture_args=args.clone();
    let before = tokio::task::spawn_blocking(move || capture(&capture_root,&capture_name,&capture_args)).await?;
    let result = if crate::project_process::is_tool(&name) {
        crate::project_process::execute_cancellable(&state.workspace,&name,args.clone(),&cancel).await
    } else if crate::program_execution::is_tool(&name) {
        crate::program_execution::execute_cancellable(&state.workspace,&args,&cancel).await
    } else {
        // A started blocking file commit cannot be aborted. Await completion
        // before recording its after-image or releasing the mutation lock.
        if cancel.is_cancelled() { Err(anyhow::anyhow!("Tool execution cancelled")) }
        else { crate::agent_service::execute_tool(state.workspace.clone(),state.tool_catalog.as_deref(),&name,args.clone()).await }
    };
    let background_running=result.as_ref().is_ok_and(|value|value["running"]==true);
    let saved_root=root.clone(); let saved_task=task.clone();
    let changes = tokio::task::spawn_blocking(move || {
        let before = before?;
        let after = capture(&saved_root,&name,&args)?;
        record(&saved_root,&saved_task,turn,before,after)?;
        if background_running {
            // A running script can change files after this snapshot. Do not
            // advertise an atomic undo for an incomplete execution interval.
            connection(&saved_root)?.execute("INSERT INTO agent_change_turns(task_id,turn,incomplete) VALUES (?1,?2,1) ON CONFLICT(task_id,turn) DO UPDATE SET incomplete=1",params![saved_task,turn])?;
        }
        summary(&saved_root,&saved_task,turn)
    }).await?;
    match changes {
        Ok(summary) => { crate::agent_service::emit(&root,&task,"workspace/changes",summary).await?; }
        Err(error) => {
            let saved_root=root.clone(); let saved_task=task.clone();
            let incomplete=tokio::task::spawn_blocking(move || -> Result<Value> {
                let conn=connection(&saved_root)?;
                conn.execute("INSERT INTO agent_change_turns(task_id,turn,incomplete) VALUES (?1,?2,1) ON CONFLICT(task_id,turn) DO UPDATE SET incomplete=1",params![saved_task,turn])?;
                summary(&saved_root,&saved_task,turn)
            }).await?;
            if let Ok(summary)=incomplete { crate::agent_service::emit(&root,&task,"workspace/changes",summary).await?; }
            crate::agent_service::emit(&root,&task,"workspace/changes_error",json!({"turn":turn,"message":error.to_string()})).await?;
        }
    }
    result
}

pub async fn get_summary(State(state): State<AgentServiceState>, RoutePath((task,turn)): RoutePath<(String,usize)>) -> Response {
    let root=state.workspace.root().to_path_buf();
    response(tokio::task::spawn_blocking(move || summary(&root,&task,turn)).await)
}

#[derive(serde::Deserialize)]
pub struct DiffQuery { path: Option<String> }

pub async fn get_diff(State(state): State<AgentServiceState>, RoutePath((task,turn,index)): RoutePath<(String,usize,usize)>, Query(query): Query<DiffQuery>) -> Response {
    let root=state.workspace.root().to_path_buf();
    response(tokio::task::spawn_blocking(move || {
        let conn=connection(&root)?;
        let offset=if query.path.is_some() {0} else {index};
        let data=conn.query_row("SELECT path,before_bytes,after_bytes,binary,unavailable FROM agent_file_changes WHERE task_id=?1 AND turn=?2 AND (?3 IS NULL OR path=?3) ORDER BY path LIMIT 1 OFFSET ?4",
            params![task,turn,query.path,offset], |row| Ok((row.get::<_,String>(0)?,row.get::<_,Option<Vec<u8>>>(1)?,row.get::<_,Option<Vec<u8>>>(2)?,row.get::<_,bool>(3)?,row.get::<_,bool>(4)?))).optional()?.context("change not found")?;
        let (path,before,after,binary,unavailable)=data;
        Ok(json!({"path":path,"oldText":if binary || unavailable || before.is_none() {None} else {text(&before)},
            "newText":if binary || unavailable {None} else {text(&after)},"binary":binary,"unavailable":unavailable}))
    }).await)
}

fn restore(path: &Path, bytes: &Option<Vec<u8>>) -> Result<()> {
    if let Some(bytes)=bytes {
        if let Some(parent)=path.parent() { fs::create_dir_all(parent)?; }
        let name=path.file_name().context("missing filename")?.to_string_lossy();
        let tmp=path.with_file_name(format!(".{name}.agent-undo-{}-{}",std::process::id(),TEMP_ID.fetch_add(1,Ordering::Relaxed)));
        let mut created=false;
        let write = (|| -> Result<()> {
            let permissions=fs::metadata(path).ok().map(|metadata|metadata.permissions());
            let mut file=fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            created=true;
            file.write_all(bytes)?; file.sync_all()?; drop(file);
            if let Some(permissions)=permissions { fs::set_permissions(&tmp,permissions)?; }
            fs::rename(&tmp,path)?; Ok(())
        })();
        if write.is_err() && created { let _=fs::remove_file(&tmp); }
        write
    } else { if path.exists() { fs::remove_file(path)?; } Ok(()) }
}

fn undo(root: &Path, task: &str, turn: usize) -> Result<Value> {
    let conn=connection(root)?;
    let active:usize=conn.query_row("SELECT COUNT(*) FROM agent_tasks WHERE status IN ('running','cancelling')",[],|row|row.get(0))?;
    ensure!(active==0,"conflict: wait for running tasks to stop before undoing files");
    let current=summary(root,task,turn)?;
    if current["reverted"] == true { return Ok(current); }
    ensure!(current["undo_available"] == true,"conflict: no complete, uncontested snapshot is available for this turn");
    let mut stmt=conn.prepare("SELECT path,before_bytes,after_exists,after_bytes FROM agent_file_changes WHERE task_id=?1 AND turn=?2 AND reverted=0 ORDER BY path")?;
    let rows=stmt.query_map(params![task,turn], |row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<Vec<u8>>>(1)?,row.get::<_,bool>(2)?,row.get::<_,Option<Vec<u8>>>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut prepared=Vec::new(); let mut conflicts=Vec::new();
    for (relative,before,exists,after) in rows {
        let path=safe_path(root,&relative)?;
        let mut budget=usize::MAX;
        let actual=image(&path,&mut budget)?;
        if actual.exists != exists || actual.bytes != after { conflicts.push(relative.clone()); }
        prepared.push((relative,path,before,after));
    }
    if !conflicts.is_empty() { bail!("conflict: files changed after this turn: {}",conflicts.join(", ")); }
    let mut applied=Vec::new();
    for (index,(relative,path,before,after)) in prepared.iter().enumerate() {
        let mut restored=false;
        let outcome=(|| -> Result<()> {
            ensure!(safe_path(root,relative)? == *path,"conflict: {relative} changed its filesystem location");
            let mut budget=usize::MAX;
            let actual=image(path,&mut budget)?;
            ensure!(actual.bytes == *after && actual.exists == after.is_some(),"conflict: {relative} changed during undo");
            restore(path,before)?;
            restored=true;
            conn.execute("UPDATE agent_file_changes SET reverted=1 WHERE task_id=?1 AND turn=?2 AND path=?3",params![task,turn,relative])?;
            Ok(())
        })();
        if let Err(error)=outcome {
            let mut rollback_errors=Vec::new();
            if restored { applied.push(index); }
            for prior in applied.iter().rev().copied() {
                let (relative,path,before,after): &(String,PathBuf,Option<Vec<u8>>,Option<Vec<u8>>)=&prepared[prior];
                let rollback=(|| -> Result<()> {
                    ensure!(safe_path(root,relative)? == *path,"file location changed during rollback");
                    let mut budget=usize::MAX;
                    let actual=image(path,&mut budget)?;
                    ensure!(actual.exists == before.is_some() && actual.bytes == *before,"file changed externally during rollback");
                    restore(path,after)
                })();
                if let Err(error)=rollback { rollback_errors.push(format!("{relative}: {error}")); }
                else { conn.execute("UPDATE agent_file_changes SET reverted=0 WHERE task_id=?1 AND turn=?2 AND path=?3",params![task,turn,relative])?; }
            }
            if !rollback_errors.is_empty() {
                conn.execute("INSERT INTO agent_change_turns(task_id,turn,incomplete) VALUES (?1,?2,1) ON CONFLICT(task_id,turn) DO UPDATE SET incomplete=1",params![task,turn])?;
            }
            bail!("undo failed: {error}; rollback failures: {}",rollback_errors.join("; "));
        }
        applied.push(index);
    }
    summary(root,task,turn)
}

pub async fn undo_turn(State(state): State<AgentServiceState>, RoutePath((task,turn)): RoutePath<(String,usize)>) -> Response {
    let _guard=MUTATIONS.lock().await;
    let root=state.workspace.root().to_path_buf(); let saved_root=root.clone(); let saved_task=task.clone();
    let result=tokio::task::spawn_blocking(move || undo(&saved_root,&saved_task,turn)).await;
    if let Ok(Ok(ref summary))=result {
        let _=crate::agent_service::emit(&root,&task,"workspace/changes",summary.clone()).await;
        let saved_root=root.clone(); let saved_task=task.clone(); let snapshot=summary.clone();
        let updated=tokio::task::spawn_blocking(move || -> Result<Option<Value>> {
            let conn=connection(&saved_root)?;
            let previous=conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='worker/work_state' ORDER BY seq DESC LIMIT 1",[&saved_task],|row|row.get::<_,String>(0)).optional()?;
            let Some(previous)=previous else {return Ok(None)};
            let value:Value=serde_json::from_str(&previous)?;
            let mut state:crate::worker_work_state::WorkState=serde_json::from_value(value["state"].clone())?;
            state.notify_revert(&snapshot);
            Ok(Some(state.snapshot()))
        }).await;
        if let Ok(Ok(Some(state)))=updated {
            let _=crate::agent_service::emit(&root,&task,"worker/work_state",json!({"turn":turn,"state":state,"source":"user_undo"})).await;
        }
    }
    response(result)
}

fn response(result: std::result::Result<Result<Value>,tokio::task::JoinError>) -> Response {
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => {
            let message=error.to_string();
            let status=if message.starts_with("conflict:") {StatusCode::CONFLICT} else if message.contains("not found") {StatusCode::NOT_FOUND} else {StatusCode::INTERNAL_SERVER_ERROR};
            (status,Json(json!({"error":message}))).into_response()
        }
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":error.to_string()}))).into_response(),
    }
}
