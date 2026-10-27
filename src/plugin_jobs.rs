//! Asynchronous plugin tool tasks, part of the fixed plugin protocol.
//!
//! A plugin tool may answer `{"status":"accepted","task_id":...}` and keep
//! working in the background. The host records the task, queries it through
//! the same plugin's `<prefix>__task` tool between model requests, and delivers
//! the finished result to the AI. While such a task is pending, a turn that
//! would end waits for it instead. No plugin is special-cased here.
use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub tool: String,
    pub call_id: String,
    pub owner_node_id: Option<String>,
}

fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_plugin_jobs(
            task_id TEXT NOT NULL, job_id TEXT NOT NULL, tool TEXT NOT NULL,
            call_id TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL,
            owner_node_id TEXT,
            PRIMARY KEY(task_id, job_id))",
    )?;
    if !conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_plugin_jobs') WHERE name='owner_node_id')", [], |r| r.get::<_,bool>(0))? {
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        if !tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_plugin_jobs') WHERE name='owner_node_id')", [], |r| r.get::<_,bool>(0))? {
            tx.execute("ALTER TABLE agent_plugin_jobs ADD COLUMN owner_node_id TEXT", [])?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// The protocol's status query for a plugin tool: `<prefix>__task`.
pub fn poll_tool(tool: &str) -> Option<String> {
    tool.split_once("__")
        .filter(|(prefix, raw)| !prefix.is_empty() && *raw != "task")
        .map(|(prefix, _)| format!("{prefix}__task"))
}

/// A task answer that will not change any more.
pub fn finished(status: &str) -> bool {
    matches!(
        status,
        "succeeded" | "failed" | "cancelled" | "error" | "interrupted"
    )
}

pub fn record(
    root: &Path,
    task: &str,
    tool: &str,
    job: &str,
    call_id: &str,
    owner_node_id: Option<&str>,
) -> Result<()> {
    let owner_node_id = owner_node_id.map(str::to_owned);
    let (task, tool, job, call_id) = (
        task.to_owned(),
        tool.to_owned(),
        job.to_owned(),
        call_id.to_owned(),
    );
    crate::db_writer::write(root, move |conn| {
        schema(conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO agent_plugin_jobs(task_id,job_id,tool,call_id,status,created_at,owner_node_id) VALUES (?1,?2,?3,?4,'pending',?5,?6)",
            params![task, job, tool, call_id, crate::agent_service::now(), owner_node_id],
        )?;
        Ok(())
    })
}

/// The saved owner identifies the original execution step, even after a restart.
pub fn pending(root: &Path, task: &str) -> Result<Vec<Job>> {
    let conn = crate::agent_service::open_db(root)?;
    schema(&conn)?;
    let rows = conn
        .prepare("SELECT job_id,tool,call_id,owner_node_id FROM agent_plugin_jobs WHERE task_id=?1 AND status='pending' ORDER BY created_at")?
        .query_map([task], |r| Ok(Job { id: r.get(0)?, tool: r.get(1)?, call_id: r.get(2)?, owner_node_id: r.get(3)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn complete(root: &Path, task: &str, job: &str, status: &str) -> Result<()> {
    let (task, job, status) = (task.to_owned(), job.to_owned(), status.to_owned());
    crate::db_writer::write(root, move |conn| {
        schema(conn)?;
        conn.execute(
            "UPDATE agent_plugin_jobs SET status=?3 WHERE task_id=?1 AND job_id=?2",
            params![task, job, status],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_jobs_migrate_without_losing_their_pending_state() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE agent_plugin_jobs(task_id TEXT NOT NULL, job_id TEXT NOT NULL, tool TEXT NOT NULL, call_id TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY(task_id,job_id));
            INSERT INTO agent_plugin_jobs VALUES ('t','old-job','demo__render','call_1','pending',0);").unwrap();
        schema(&conn).unwrap();
        schema(&conn).unwrap();
        let state: (String, Option<String>) = conn
            .query_row(
                "SELECT status,owner_node_id FROM agent_plugin_jobs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, ("pending".into(), None));
    }

    #[test]
    fn the_status_query_is_the_plugins_own_task_tool() {
        assert_eq!(
            poll_tool("demo__init_project").as_deref(),
            Some("demo__task")
        );
        assert_eq!(poll_tool("demo__render").as_deref(), Some("demo__task"));
        assert_eq!(poll_tool("demo__task"), None);
        assert_eq!(poll_tool("read_file"), None);
        assert!(
            finished("succeeded")
                && finished("failed")
                && !finished("accepted")
                && !finished("running")
        );
    }

    #[test]
    fn recorded_tasks_stay_pending_until_completed() {
        let root =
            std::env::temp_dir().join(format!("plugin-jobs-{}", crate::agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        crate::agent_service::open_db(&root).unwrap();
        record(
            &root,
            "t",
            "demo__render",
            "job-1",
            "call-1",
            Some("step-1"),
        )
        .unwrap();
        record(
            &root,
            "t",
            "demo__render",
            "job-1",
            "call-1",
            Some("another-step"),
        )
        .unwrap();
        assert_eq!(
            pending(&root, "t").unwrap(),
            vec![Job {
                id: "job-1".into(),
                tool: "demo__render".into(),
                call_id: "call-1".into(),
                owner_node_id: Some("step-1".into())
            }]
        );
        complete(&root, "t", "job-1", "succeeded").unwrap();
        assert!(pending(&root, "t").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
