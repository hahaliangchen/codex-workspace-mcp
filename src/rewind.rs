//! "Rewind to before turn N": undo that turn's and every later turn's file
//! changes (newest first), drop their conversation records, and restore what
//! the agent knew when turn N began. The user's turn-N message is returned so
//! it can be edited and sent again.
use crate::agent_service::{self, AgentServiceState};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_window_snapshots(task_id TEXT NOT NULL,turn INTEGER NOT NULL,state TEXT,PRIMARY KEY(task_id,turn));",
    )?;
    Ok(())
}

/// Record the main window as it stood before `turn` (None: nothing yet).
pub fn snapshot(conn: &Connection, task: &str, turn: usize, state: Option<&str>) -> Result<()> {
    schema(conn)?;
    conn.execute(
        "INSERT OR REPLACE INTO agent_window_snapshots(task_id,turn,state) VALUES (?1,?2,?3)",
        params![task, turn as i64, state],
    )?;
    Ok(())
}

/// Tables that record rows per (task_id, turn).
fn per_turn_tables(conn: &Connection) -> Result<Vec<String>> {
    let tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut found = Vec::new();
    for table in tables {
        let columns = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{}')", table.replace('\'', "''")))?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if columns.iter().any(|c| c == "task_id") && columns.iter().any(|c| c == "turn") {
            found.push(table);
        }
    }
    Ok(found)
}

/// The text the user wrote to start `turn`.
fn turn_prompt(conn: &Connection, task: &str, turn: usize) -> Result<String> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='user/message' AND json_extract(data,'$.turn')=?2 ORDER BY seq LIMIT 1",
            params![task, turn as i64],
            |r| r.get(0),
        )
        .optional()?;
    let Some(data) = data else { return Ok(String::new()) };
    let data: Value = serde_json::from_str(&data)?;
    let content = &data["message"]["content"];
    Ok(match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    })
}

pub fn rewind(root: &std::path::Path, task: &str, turn: usize) -> Result<Value> {
    ensure!(turn >= 1, "轮次无效");
    let conn = agent_service::open_db(root)?;
    schema(&conn)?;
    let status: String = conn
        .query_row("SELECT status FROM agent_tasks WHERE id=?1", [task], |r| r.get(0))
        .context("会话不存在")?;
    ensure!(!matches!(status.as_str(), "running" | "cancelling"), "conflict: 请先停止正在进行的任务再回退");
    let snapshot: Option<Option<String>> = conn
        .query_row("SELECT state FROM agent_window_snapshots WHERE task_id=?1 AND turn=?2", params![task, turn as i64], |r| r.get(0))
        .optional()?;
    let Some(snapshot) = snapshot else {
        bail!("conflict: 这一轮是在支持回退之前进行的，无法恢复当时的对话状态");
    };
    let start: i64 = conn
        .query_row(
            "SELECT MIN(seq) FROM agent_task_events WHERE task_id=?1 AND json_extract(data,'$.turn')>=?2",
            params![task, turn as i64],
            |r| r.get::<_, Option<i64>>(0),
        )?
        .context("找不到这一轮的记录")?;
    let prompt = turn_prompt(&conn, task, turn)?;

    // Files first, newest turn first, so each undo meets the state it produced.
    // A conflict stops here with the conversation untouched.
    let turns = conn
        .prepare("SELECT DISTINCT turn FROM agent_file_changes WHERE task_id=?1 AND turn>=?2 AND reverted=0 ORDER BY turn DESC")
        .and_then(|mut stmt| stmt.query_map(params![task, turn as i64], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>())
        .unwrap_or_default();
    let mut undone = Vec::new();
    for changed in &turns {
        if let Err(error) = crate::workspace_changes::undo(root, task, *changed as usize) {
            bail!("conflict: 撤销第 {changed} 轮的文件改动失败（已撤销：{undone:?}），对话未回退：{error}");
        }
        undone.push(*changed);
    }

    let mut conn = conn;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM agent_task_events WHERE task_id=?1 AND seq>=?2", params![task, start])?;
    // Every per-turn table (file changes, attachments, request logs, jobs…)
    // forgets turn N onward, or the replayed turn N would collide with it.
    for table in per_turn_tables(&tx)? {
        if table != "agent_window_snapshots" {
            tx.execute(&format!("DELETE FROM \"{table}\" WHERE task_id=?1 AND turn>=?2"), params![task, turn as i64])?;
        }
    }
    let _ = tx.execute("DELETE FROM agent_interjections WHERE task_id=?1 AND seq>=?2", params![task, start]);
    let _ = tx.execute("DELETE FROM tool_approval_pending WHERE task_id=?1", [task]);
    tx.execute("DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope<>'main'", [task])?;
    match &snapshot {
        Some(state) => {
            tx.execute(
                "INSERT INTO agent_runtime_windows VALUES (?1,'main',?2) ON CONFLICT(task_id,scope) DO UPDATE SET state=excluded.state",
                params![task, state],
            )?;
        }
        None => {
            tx.execute("DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope='main'", [task])?;
        }
    }
    tx.execute("DELETE FROM agent_window_snapshots WHERE task_id=?1 AND turn>=?2", params![task, turn as i64])?;
    tx.execute("UPDATE agent_tasks SET status=CASE WHEN ?2=1 THEN 'draft' ELSE status END,updated_at=?3 WHERE id=?1",
        params![task, turn as i64, agent_service::now()])?;
    tx.commit()?;
    Ok(json!({"turn":turn,"prompt":prompt,"undone_turns":undone}))
}

#[derive(Deserialize)]
pub struct RewindInput {
    turn: usize,
}
pub async fn post(
    State(state): State<AgentServiceState>,
    Path(task): Path<String>,
    Json(input): Json<RewindInput>,
) -> Response {
    let _guard = crate::workspace_changes::MUTATIONS.lock().await;
    let root = state.workspace.root().to_path_buf();
    let result = tokio::task::spawn_blocking(move || rewind(&root, &task, input.turn)).await;
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => {
            let message = error.to_string();
            let status = if message.starts_with("conflict:") { StatusCode::CONFLICT } else { StatusCode::BAD_REQUEST };
            (status, Json(json!({"error":message.trim_start_matches("conflict: ").to_owned()}))).into_response()
        }
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error.to_string()}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewind_restores_the_window_and_drops_later_records() {
        let root = std::env::temp_dir().join(format!("rewind-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let mut conn = agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','first','fake','completed',0,0)", []).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_runtime_windows(task_id TEXT NOT NULL,scope TEXT NOT NULL,state TEXT NOT NULL,PRIMARY KEY(task_id,scope));").unwrap();
        let tx = conn.transaction().unwrap();
        for (turn, text) in [(1, "first"), (2, "second")] {
            agent_service::append_event_tx(&tx, "t", "user/message", &json!({"turn":turn,"message":{"role":"user","content":text}}), None).unwrap();
            agent_service::append_event_tx(&tx, "t", "assistant/message", &json!({"turn":turn}), None).unwrap();
        }
        tx.commit().unwrap();
        conn.execute_batch("CREATE TABLE per_turn(task_id TEXT,turn INTEGER); INSERT INTO per_turn VALUES ('t',1),('t',2),('other',2);").unwrap();
        snapshot(&conn, "t", 1, None).unwrap();
        snapshot(&conn, "t", 2, Some("after-turn-1")).unwrap();
        conn.execute("INSERT INTO agent_runtime_windows VALUES ('t','main','after-turn-2')", []).unwrap();

        conn.execute("UPDATE agent_tasks SET status='running' WHERE id='t'", []).unwrap();
        assert!(rewind(&root, "t", 2).is_err());
        conn.execute("UPDATE agent_tasks SET status='completed' WHERE id='t'", []).unwrap();

        let result = rewind(&root, "t", 2).unwrap();
        assert_eq!(result["prompt"], "second");
        let kept: i64 = conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE task_id='t'", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 2);
        let window: String = conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope='main'", [], |r| r.get(0)).unwrap();
        assert_eq!(window, "after-turn-1");
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM per_turn", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2, "only task t's turn 2 row is removed");
        // Turn 2's snapshot is gone, so rewinding to it again is refused.
        assert!(rewind(&root, "t", 2).is_err());

        rewind(&root, "t", 1).unwrap();
        let windows: i64 = conn.query_row("SELECT COUNT(*) FROM agent_runtime_windows WHERE task_id='t'", [], |r| r.get(0)).unwrap();
        assert_eq!(windows, 0);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
