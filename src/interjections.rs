//! Messages the user sends while a turn is running. They are delivered into
//! the main agent's working window before its next model request, so the user
//! can correct course without stopping the task.
use crate::agent_service::{self, AgentServiceState};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, Transaction, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_interjections(task_id TEXT NOT NULL,seq INTEGER NOT NULL,text TEXT NOT NULL,delivered INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(task_id,seq));",
    )?;
    Ok(())
}

/// Record an interjection for a running task; fails once the task is idle so
/// the caller sends an ordinary message instead.
pub fn submit(root: &std::path::Path, task: &str, text: &str) -> Result<i64> {
    ensure!(!text.trim().is_empty(), "消息不能为空");
    let mut conn = agent_service::open_db(root)?;
    schema(&conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let status: String = tx
        .query_row("SELECT status FROM agent_tasks WHERE id=?1", [task], |r| {
            r.get(0)
        })
        .context("会话不存在")?;
    ensure!(
        matches!(status.as_str(), "running"),
        "NOT_RUNNING: 任务已结束，请直接发送消息"
    );
    let seq = agent_service::append_event_tx(
        &tx,
        task,
        "user/interjection",
        &json!({"text":text}),
        Some("append"),
    )?;
    tx.execute(
        "INSERT INTO agent_interjections(task_id,seq,text) VALUES (?1,?2,?3)",
        params![task, seq, text],
    )?;
    tx.commit()?;
    Ok(seq)
}

pub fn pending(conn: &Connection, task: &str) -> Result<bool> {
    schema(conn)?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_interjections WHERE task_id=?1 AND delivered=0)",
        [task],
        |r| r.get(0),
    )?)
}

/// Mark undelivered interjections delivered and return them as window
/// messages, inside the caller's transaction that also saves the window.
pub fn take(tx: &Transaction, task: &str) -> Result<Vec<Value>> {
    schema(tx)?;
    let rows = tx
        .prepare("SELECT seq,text FROM agent_interjections WHERE task_id=?1 AND delivered=0 ORDER BY seq")?
        .query_map([task], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut messages = Vec::new();
    for (seq, text) in rows {
        tx.execute(
            "UPDATE agent_interjections SET delivered=1 WHERE task_id=?1 AND seq=?2",
            params![task, seq],
        )?;
        agent_service::append_event_tx(
            tx,
            task,
            "user/interjection_delivered",
            &json!({"interjection_seq":seq}),
            Some("append"),
        )?;
        messages.push(json!({"role":"user","name":"user_interjection",
            "content":format!("[The user added this while you were working. Treat it as a correction or addition to the current task and adjust your next steps accordingly.]\n{text}"),
            "source":{"seq":seq,"history_reference":{"task_id":task,"event_seq":seq}}}));
    }
    Ok(messages)
}

#[derive(Deserialize)]
pub struct InterjectionInput {
    text: String,
}
pub async fn post(
    State(state): State<AgentServiceState>,
    Path(task): Path<String>,
    Json(input): Json<InterjectionInput>,
) -> Response {
    match submit(state.workspace.root(), &task, &input.text) {
        Ok(seq) => Json(json!({"seq":seq})).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interjections_require_a_running_task_and_are_delivered_once() {
        let root =
            std::env::temp_dir().join(format!("interjections-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let conn = agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','','fake','completed',0,0)", []).unwrap();
        assert!(
            submit(&root, "t", "改用 B 方案")
                .unwrap_err()
                .to_string()
                .contains("NOT_RUNNING")
        );
        conn.execute("UPDATE agent_tasks SET status='running' WHERE id='t'", [])
            .unwrap();
        assert!(submit(&root, "t", "  ").is_err());
        submit(&root, "t", "改用 B 方案").unwrap();
        submit(&root, "t", "别动测试").unwrap();
        assert!(pending(&conn, "t").unwrap());
        let mut conn = agent_service::open_db(&root).unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let messages = take(&tx, "t").unwrap();
        tx.commit().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .ends_with("改用 B 方案")
        );
        assert_eq!(messages[1]["name"], "user_interjection");
        assert!(!pending(&conn, "t").unwrap());
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        assert!(take(&tx, "t").unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
