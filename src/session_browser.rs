//! Durable sidebar state. Removing a project never removes its directory or chats.
use crate::agent_service::{self, AgentServiceState};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path as FsPath, PathBuf};

pub fn schema(conn: &Connection, root: &FsPath) -> Result<()> {
    let fresh = !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='agent_projects')",
        [],
        |r| r.get::<_, bool>(0),
    )?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_projects(path TEXT PRIMARY KEY,title TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS agent_session_meta(task_id TEXT PRIMARY KEY,title TEXT,pinned INTEGER NOT NULL DEFAULT 0,archived INTEGER NOT NULL DEFAULT 0,workspace TEXT);")?;
    let _ = conn.execute(
        "ALTER TABLE agent_session_meta ADD COLUMN kind TEXT NOT NULL DEFAULT 'code'",
        [],
    );
    if fresh {
        conn.execute(
            "INSERT OR IGNORE INTO agent_projects(path,title) VALUES (?1,?2)",
            params![
                root.to_string_lossy(),
                root.file_name().unwrap_or_default().to_string_lossy()
            ],
        )?;
    }
    Ok(())
}

pub fn decorate(conn: &Connection, root: &FsPath, mut task: Value) -> Result<Value> {
    let id = task["task_id"].as_str().context("task id required")?;
    let meta = conn
        .query_row(
            "SELECT title,pinned,archived,workspace,kind FROM agent_session_meta WHERE task_id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let plugin: Option<String> = conn
        .query_row(
            "SELECT plugin_id FROM plugin_sessions WHERE task_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let (title, pinned, archived, workspace, kind) = meta.unwrap_or_default();
    let awaiting = crate::tool_approvals::awaiting(conn, id);
    let chat = plugin.is_some() || kind == "chat";
    task["kind"] = json!(if chat { "chat" } else { "code" });
    task["title"] = json!(title);
    task["pinned"] = json!(pinned);
    task["archived"] = json!(archived);
    task["workspace"] = if chat {
        Value::Null
    } else {
        json!(workspace.unwrap_or_else(|| root.display().to_string()))
    };
    task["plugin_id"] = json!(plugin);
    task["awaiting_approval"] = json!(awaiting);
    Ok(task)
}

pub fn task_workspace(root: &FsPath, task: &str) -> Result<PathBuf> {
    let conn = agent_service::open_db(root)?;
    let path: Option<String> = conn
        .query_row(
            "SELECT workspace FROM agent_session_meta WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(path
        .map(PathBuf::from)
        .unwrap_or_else(|| root.to_path_buf()))
}

fn response(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

/// Plain text of a conversation event, for content search.
fn event_text(data: &Value) -> String {
    let content = &data["message"]["content"];
    let mut text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    if let Some(extra) = data["text"].as_str() {
        text.push_str(extra);
    }
    text
}

/// Up to `radius` characters either side of the first case-insensitive match.
fn snippet(text: &str, query: &str, radius: usize) -> Option<String> {
    let lower = text.to_lowercase();
    let at = lower.find(&query.to_lowercase())?;
    let chars = lower[..at].chars().count();
    let start = chars.saturating_sub(radius);
    let length = query.chars().count() + radius * 2;
    let body: String = text.chars().skip(start).take(length).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    Some(format!("{}{}{}", if start > 0 { "…" } else { "" }, body, if start + length < text.chars().count() { "…" } else { "" }))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
}
/// Sessions whose messages mention the query, newest match first, with a snippet.
pub async fn search(State(state): State<AgentServiceState>, axum::extract::Query(query): axum::extract::Query<SearchQuery>) -> Response {
    response((|| {
        let q = query.q.trim();
        ensure!(q.chars().count() >= 2, "至少输入两个字");
        let conn = agent_service::open_db(state.workspace.root())?;
        let pattern = format!("%{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(
            "SELECT task_id,data FROM agent_task_events WHERE kind IN ('user/message','assistant/message','user/interjection') AND data LIKE ?1 ESCAPE '\\' ORDER BY timestamp DESC,seq DESC LIMIT 2000",
        )?;
        let rows = stmt
            .query_map([pattern], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut seen = std::collections::HashSet::new();
        let mut matches = Vec::new();
        for (task, data) in rows {
            if seen.contains(&task) {
                continue;
            }
            let data: Value = serde_json::from_str(&data).unwrap_or_default();
            // LIKE also hits JSON keys and escapes; keep only real text matches.
            if let Some(snippet) = snippet(&event_text(&data), q, 24) {
                seen.insert(task.clone());
                matches.push(json!({"task_id":task,"snippet":snippet}));
                if matches.len() >= 50 {
                    break;
                }
            }
        }
        Ok(json!({"matches":matches}))
    })())
}

pub async fn projects(State(state): State<AgentServiceState>) -> Response {
    response((|| {
        let conn = agent_service::open_db(state.workspace.root())?;
        let mut stmt = conn.prepare("SELECT path,title FROM agent_projects ORDER BY rowid")?;
        let projects = stmt
            .query_map([], |r| {
                Ok(json!({"path":r.get::<_,String>(0)?,"title":r.get::<_,String>(1)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"projects":projects}))
    })())
}
#[derive(Deserialize)]
pub struct ProjectInput {
    path: String,
    title: Option<String>,
}
pub async fn add_project(
    State(state): State<AgentServiceState>,
    Json(input): Json<ProjectInput>,
) -> Response {
    response((|| {
        let path = std::fs::canonicalize(input.path.trim()).context("项目目录不存在")?;
        ensure!(path.is_dir(), "请选择项目目录");
        let title = input
            .title
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let conn = agent_service::open_db(state.workspace.root())?;
        conn.execute(
            "INSERT OR IGNORE INTO agent_projects(path,title) VALUES (?1,?2)",
            params![path.to_string_lossy(), title],
        )?;
        Ok(json!({"path":path,"title":title}))
    })())
}
pub async fn rename_project(
    State(state): State<AgentServiceState>,
    Json(input): Json<ProjectInput>,
) -> Response {
    response((|| {
        let title = input.title.context("请输入项目名称")?;
        ensure!(!title.trim().is_empty(), "请输入项目名称");
        let conn = agent_service::open_db(state.workspace.root())?;
        ensure!(
            conn.execute(
                "UPDATE agent_projects SET title=?2 WHERE path=?1",
                params![input.path, title.trim()]
            )? > 0,
            "项目不存在"
        );
        Ok(json!({"ok":true}))
    })())
}
pub async fn remove_project(
    State(state): State<AgentServiceState>,
    Json(input): Json<ProjectInput>,
) -> Response {
    response((|| {
        let conn = agent_service::open_db(state.workspace.root())?;
        conn.execute("DELETE FROM agent_projects WHERE path=?1", [input.path])?;
        Ok(json!({"ok":true}))
    })())
}
#[derive(Deserialize)]
pub struct SessionInput {
    title: Option<String>,
    pinned: Option<bool>,
    archived: Option<bool>,
}
pub async fn update_session(
    State(state): State<AgentServiceState>,
    Path(id): Path<String>,
    Json(input): Json<SessionInput>,
) -> Response {
    response((|| {
        let mut conn = agent_service::open_db(state.workspace.root())?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let status: String = tx
            .query_row("SELECT status FROM agent_tasks WHERE id=?1", [&id], |r| {
                r.get(0)
            })
            .optional()?
            .context("会话不存在")?;
        ensure!(
            input.archived != Some(true) || !matches!(status.as_str(), "running" | "cancelling"),
            "请先停止会话再归档"
        );
        tx.execute(
            "INSERT OR IGNORE INTO agent_session_meta(task_id) VALUES (?1)",
            [&id],
        )?;
        if let Some(title) = input.title {
            ensure!(!title.trim().is_empty(), "请输入会话名称");
            tx.execute(
                "UPDATE agent_session_meta SET title=?2 WHERE task_id=?1",
                params![id, title.trim()],
            )?;
        }
        if let Some(pinned) = input.pinned {
            tx.execute("UPDATE agent_session_meta SET pinned=?2,archived=CASE WHEN ?2 THEN 0 ELSE archived END WHERE task_id=?1", params![id,pinned])?;
        }
        if let Some(archived) = input.archived {
            tx.execute("UPDATE agent_session_meta SET archived=?2,pinned=CASE WHEN ?2 THEN 0 ELSE pinned END WHERE task_id=?1", params![id,archived])?;
        }
        tx.commit()?;
        Ok(json!({"ok":true}))
    })())
}

/// Delete the full task family and all its local records in one transaction.
/// Running work must stop first, so background writers cannot recreate history.
pub fn delete(conn: &mut Connection, id: &str) -> Result<Vec<String>> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut stmt = tx.prepare("WITH RECURSIVE family(id) AS (SELECT id FROM agent_tasks WHERE id=?1 UNION SELECT t.id FROM agent_tasks t JOIN family f ON t.parent_task_id=f.id) SELECT id FROM family")?;
    let ids = stmt
        .query_map([id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    ensure!(!ids.is_empty(), "会话不存在");
    for id in &ids {
        let status: String =
            tx.query_row("SELECT status FROM agent_tasks WHERE id=?1", [id], |r| {
                r.get(0)
            })?;
        ensure!(
            !matches!(status.as_str(), "running" | "cancelling"),
            "请先停止会话及其子任务再删除"
        );
    }
    // Several legacy runtime tables predate foreign keys. Remove their records
    // explicitly as well as the tables that already declare cascading deletes.
    let mut stmt = tx.prepare("SELECT name FROM sqlite_master WHERE type='table' AND (name GLOB 'agent_*' OR name GLOB 'ppt_session_*' OR name='plugin_sessions')")?;
    let tables = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    if tables.iter().any(|table| table == "agent_delegates")
        && tables
            .iter()
            .any(|table| table == "agent_delegate_executors")
    {
        for id in &ids {
            tx.execute("DELETE FROM agent_delegate_executors WHERE agent_id IN (SELECT id FROM agent_delegates WHERE task_id=?1)", [id])?;
        }
    }
    for table in tables.iter().filter(|t| t.as_str() != "agent_tasks") {
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));
        let mut stmt = tx.prepare(&format!("PRAGMA table_info({quoted})"))?;
        let columns = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for column in ["task_id", "host_task_id"] {
            if columns.iter().any(|c| c == column) {
                for id in &ids {
                    tx.execute(&format!("DELETE FROM {quoted} WHERE {column}=?1"), [id])?;
                }
            }
        }
    }
    for id in &ids {
        tx.execute("DELETE FROM agent_tasks WHERE id=?1", [id])?;
    }
    tx.commit()?;
    Ok(ids)
}
pub async fn delete_session(
    State(state): State<AgentServiceState>,
    Path(id): Path<String>,
) -> Response {
    if state.cancellations.lock().await.contains_key(&id) {
        return response(Err(anyhow::anyhow!("请等待会话停止后再删除")));
    }
    let root = state.workspace.root().to_path_buf();
    match tokio::task::spawn_blocking(move || {
        delete(&mut agent_service::open_db(&root)?, &id).map(|ids| json!({"deleted":ids}))
    })
    .await
    {
        Ok(result) => response(result),
        Err(error) => response(Err(error.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_search_snippets_center_on_the_match() {
        let text = "上次改 pptx 导图的时候，把封面换成了蓝色背景，然后又调整了目录页。";
        assert_eq!(snippet(text, "PPTX 导图", 4).unwrap(), "上次改 pptx 导图的时候，…");
        assert_eq!(snippet(text, "蓝色", 2).unwrap(), "…成了蓝色背景…");
        assert!(snippet(text, "不存在", 4).is_none());
        let data = json!({"message":{"content":[{"type":"text","text":"封面换成蓝色"}]}});
        assert_eq!(event_text(&data), "封面换成蓝色");
    }
    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_tasks(id TEXT PRIMARY KEY,status TEXT,parent_task_id TEXT);
            CREATE TABLE plugin_sessions(task_id TEXT PRIMARY KEY,plugin_id TEXT);
            CREATE TABLE agent_task_events(task_id TEXT,seq INTEGER);
            CREATE TABLE agent_runtime_windows(task_id TEXT,scope TEXT,state TEXT);
            CREATE TABLE ppt_session_jobs(host_task_id TEXT,remote_task_id TEXT);
            CREATE TABLE agent_attachments(task_id TEXT,receipt TEXT);
            CREATE TABLE agent_delegates(id TEXT,task_id TEXT);
            CREATE TABLE agent_delegate_executors(agent_id TEXT);",
        )
        .unwrap();
        schema(&conn, FsPath::new("/projects/current")).unwrap();
        conn
    }
    #[test]
    fn deletes_descendants_and_legacy_records_without_touching_other_sessions() {
        let mut conn = db();
        conn.execute_batch("INSERT INTO agent_tasks VALUES ('root','completed',NULL),('child','completed','root'),('grandchild','failed','child'),('other','draft',NULL);
            INSERT INTO plugin_sessions VALUES ('root','ppt-master');
            INSERT INTO agent_runtime_windows VALUES ('child','main','{}'),('other','main','{}');
            INSERT INTO agent_task_events VALUES ('root',1),('grandchild',1),('other',1);
            INSERT INTO ppt_session_jobs VALUES ('root','remote');
            INSERT INTO agent_attachments VALUES ('child','receipt');
            INSERT INTO agent_delegates VALUES ('delegate','root');
            INSERT INTO agent_delegate_executors VALUES ('delegate');
            INSERT INTO agent_session_meta(task_id,title) VALUES ('root','Custom name');").unwrap();
        let deleted = delete(&mut conn, "root").unwrap();
        assert_eq!(deleted.len(), 3);
        for table in [
            "plugin_sessions",
            "ppt_session_jobs",
            "agent_attachments",
            "agent_session_meta",
            "agent_delegates",
            "agent_delegate_executors",
        ] {
            assert_eq!(
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        assert_eq!(
            conn.query_row("SELECT id FROM agent_tasks", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "other"
        );
        assert_eq!(
            conn.query_row("SELECT task_id FROM agent_runtime_windows", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "other"
        );
        assert_eq!(
            conn.query_row("SELECT task_id FROM agent_task_events", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "other"
        );
    }
    #[test]
    fn active_descendant_rejects_deletion_atomically() {
        let mut conn = db();
        conn.execute_batch("INSERT INTO agent_tasks VALUES ('root','completed',NULL),('child','running','root'); INSERT INTO agent_task_events VALUES ('root',1);").unwrap();
        assert!(delete(&mut conn, "root").is_err());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM agent_tasks", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM agent_task_events", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn removing_the_default_project_is_durable_and_does_not_delete_sessions() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO agent_tasks VALUES ('root','completed',NULL); DELETE FROM agent_projects;",
        )
        .unwrap();
        schema(&conn, FsPath::new("/projects/current")).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM agent_projects", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let task = decorate(
            &conn,
            FsPath::new("/projects/current"),
            json!({"task_id":"root"}),
        )
        .unwrap();
        assert_eq!(task["workspace"], "/projects/current");
    }
    #[test]
    fn classifies_chat_and_project_sessions_and_keeps_title_separate_from_prompt() {
        let conn = db();
        conn.execute_batch("INSERT INTO plugin_sessions VALUES ('plugin','ppt-master');
            INSERT INTO agent_session_meta(task_id,title,pinned,archived,workspace,kind) VALUES ('code','Renamed',1,0,'/projects/second','code'),('chat',NULL,0,0,NULL,'chat');").unwrap();
        let task = decorate(
            &conn,
            FsPath::new("/projects/current"),
            json!({"task_id":"code","prompt":"Original user request"}),
        )
        .unwrap();
        assert_eq!(task["title"], "Renamed");
        assert_eq!(task["prompt"], "Original user request");
        assert_eq!(task["workspace"], "/projects/second");
        assert_eq!(task["pinned"], true);
        for id in ["plugin", "chat"] {
            let chat = decorate(
                &conn,
                FsPath::new("/projects/current"),
                json!({"task_id":id}),
            )
            .unwrap();
            assert_eq!(chat["kind"], "chat");
            assert!(chat["workspace"].is_null());
        }
    }
}
