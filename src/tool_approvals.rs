//! "Ask before running" permission: file edits proceed, but commands, scripts
//! and the browser wait for the user's decision in the conversation. A user
//! can allow once, for the rest of the session, or for the project for good,
//! so the question comes up rarely.
use crate::agent_service::{self, AgentServiceState, PermissionMode};
use anyhow::{Result, bail};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path as FsPath, time::Duration};
use tokio_util::sync::CancellationToken;

const POLL: Duration = Duration::from_millis(400);
const DECISIONS: [&str; 4] = ["once", "session", "project", "deny"];

fn open(root: &FsPath) -> Result<Connection> {
    let conn = agent_service::open_db(root)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tool_approvals(task_id TEXT NOT NULL,call_id TEXT NOT NULL,decision TEXT NOT NULL,PRIMARY KEY(task_id,call_id));
         CREATE TABLE IF NOT EXISTS tool_approval_grants(scope TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS tool_approval_pending(task_id TEXT NOT NULL,call_id TEXT NOT NULL,PRIMARY KEY(task_id,call_id));",
    )?;
    Ok(conn)
}

/// Tools that "ask" mode runs only after approval: exactly those that
/// workspace-write mode would refuse, i.e. process execution and the browser.
pub fn needs_approval(
    mode: PermissionMode,
    name: &str,
    catalog: Option<&crate::builtin_plugins::ToolCatalog>,
    ban_execution: bool,
) -> bool {
    matches!(mode, PermissionMode::Ask)
        && !(PermissionMode::WorkspaceWrite.allows(name)
            && catalog.is_none_or(|c| {
                c.packages
                    .allows(name, PermissionMode::WorkspaceWrite, ban_execution)
            }))
}

fn grant_scopes(task: &str, project: Option<&FsPath>) -> Vec<String> {
    let mut scopes = vec![format!("task:{task}")];
    if let Some(project) = project {
        scopes.push(format!("project:{}", project.display()));
    }
    scopes
}

fn granted(conn: &Connection, scopes: &[String]) -> Result<bool> {
    for scope in scopes {
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM tool_approval_grants WHERE scope=?1",
                [scope],
                |r| r.get(0),
            )
            .optional()?;
        if found.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn append(root: &FsPath, task: &str, kind: &str, data: &Value) -> Result<()> {
    let mut conn = agent_service::open_db(root)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    agent_service::append_event_tx(&tx, task, kind, data, Some("append"))?;
    tx.commit()?;
    Ok(())
}

/// Block a tool call until the user allows it, or fail it when they decline.
#[allow(clippy::too_many_arguments)]
pub async fn wait(
    state: &AgentServiceState,
    task: &str,
    agent_id: Option<&str>,
    call_id: &str,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<()> {
    let root = state.workspace.root().to_path_buf();
    let project = crate::project_notes::task_project(&root, task)?;
    let scopes = grant_scopes(task, project.as_deref());
    if granted(&open(&root)?, &scopes)? {
        return Ok(());
    }
    let mut detail = args.to_string();
    if detail.len() > 2_000 {
        detail = format!("{}…", detail.chars().take(2_000).collect::<String>());
    }
    open(&root)?.execute(
        "INSERT OR IGNORE INTO tool_approval_pending VALUES (?1,?2)",
        params![task, call_id],
    )?;
    append(
        &root,
        task,
        "approval/request",
        &json!({"call_id":call_id,"tool":name,"arguments":detail,"agent_id":agent_id,
        "project":project}),
    )?;
    let waited: Result<String> = async {
        loop {
            if cancel.is_cancelled() {
                append(
                    &root,
                    task,
                    "approval/decision",
                    &json!({"call_id":call_id,"decision":"cancelled"}),
                )?;
                bail!("cancelled");
            }
            let decision: Option<String> = open(&root)?
                .query_row(
                    "SELECT decision FROM tool_approvals WHERE task_id=?1 AND call_id=?2",
                    params![task, call_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(decision) = decision {
                return Ok(decision);
            }
            // Another call in this session may have just granted a wider scope.
            if granted(&open(&root)?, &scopes)? {
                return Ok("granted".to_owned());
            }
            tokio::time::sleep(POLL).await;
        }
    }
    .await;
    // Cleared on every exit, so the task list never shows a stale "awaiting approval".
    open(&root)?.execute(
        "DELETE FROM tool_approval_pending WHERE task_id=?1 AND call_id=?2",
        params![task, call_id],
    )?;
    let decision = waited?;
    let conn = open(&root)?;
    match decision.as_str() {
        "session" => {
            conn.execute(
                "INSERT OR IGNORE INTO tool_approval_grants(scope) VALUES (?1)",
                [&scopes[0]],
            )?;
        }
        "project" => {
            conn.execute(
                "INSERT OR IGNORE INTO tool_approval_grants(scope) VALUES (?1)",
                [scopes.last().unwrap()],
            )?;
        }
        _ => {}
    }
    append(
        &root,
        task,
        "approval/decision",
        &json!({"call_id":call_id,"decision":decision}),
    )?;
    if decision == "deny" {
        bail!(
            "USER_DENIED: the user declined running {name}. Do not retry the same operation. Continue with work that needs no command execution, or tell the user what they need to run or allow."
        );
    }
    Ok(())
}

/// Whether the task is paused on an approval card (for the task list).
pub fn awaiting(conn: &Connection, task: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM tool_approval_pending WHERE task_id=?1)",
        [task],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

#[derive(Deserialize)]
pub struct DecisionInput {
    call_id: String,
    decision: String,
}
pub async fn decide(
    State(state): State<AgentServiceState>,
    Path(task): Path<String>,
    Json(input): Json<DecisionInput>,
) -> Response {
    let result = (|| {
        anyhow::ensure!(
            DECISIONS.contains(&input.decision.as_str()),
            "未知的审批选项"
        );
        open(state.workspace.root())?.execute(
            "INSERT OR IGNORE INTO tool_approvals(task_id,call_id,decision) VALUES (?1,?2,?3)",
            params![task, input.call_id, input.decision],
        )?;
        Ok(json!({"ok":true}))
    })();
    match result {
        Ok(value) => Json(value).into_response(),
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
    fn ask_mode_gates_only_execution() {
        assert!(needs_approval(
            PermissionMode::Ask,
            "run_program",
            None,
            false
        ));
        assert!(needs_approval(
            PermissionMode::Ask,
            "browser_open",
            None,
            false
        ));
        assert!(!needs_approval(
            PermissionMode::Ask,
            "write_file",
            None,
            false
        ));
        assert!(!needs_approval(
            PermissionMode::Ask,
            "read_file",
            None,
            false
        ));
        assert!(!needs_approval(
            PermissionMode::FullAccess,
            "run_program",
            None,
            false
        ));
        assert!(PermissionMode::Ask.allows("run_program"));
        assert!(!PermissionMode::WorkspaceWrite.allows("run_program"));
    }

    #[tokio::test]
    async fn decisions_unblock_and_session_grants_skip_later_questions() {
        let root = std::env::temp_dir().join(format!("approvals-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let state = agent_service::tests::test_state(&root, "127.0.0.1:9".parse().unwrap());
        agent_service::open_db(&root).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','','fake','running',0,0)",[]).unwrap();
        let cancel = CancellationToken::new();
        let decide_later = |call: &'static str, decision: &'static str| {
            let root = root.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                open(&root)
                    .unwrap()
                    .execute(
                        "INSERT INTO tool_approvals VALUES ('t',?1,?2)",
                        params![call, decision],
                    )
                    .unwrap();
            })
        };
        decide_later("c1", "deny");
        let denied = wait(&state, "t", None, "c1", "run_program", &json!({}), &cancel)
            .await
            .unwrap_err();
        assert!(denied.to_string().contains("USER_DENIED"), "{denied:#}");
        decide_later("c2", "session");
        wait(&state, "t", None, "c2", "run_program", &json!({}), &cancel)
            .await
            .unwrap();
        assert!(!awaiting(&open(&root).unwrap(), "t"));
        // Granted for the session: no question, returns at once.
        tokio::time::timeout(
            Duration::from_millis(50),
            wait(&state, "t", None, "c3", "run_program", &json!({}), &cancel),
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }
}
