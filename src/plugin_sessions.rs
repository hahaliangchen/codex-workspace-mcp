//! Page conversations use the host Agent with a durable, non-escalatable owner.
use crate::agent_service::{self, AgentServiceState};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub fn owner(root: &std::path::Path, task: &str) -> Result<Option<String>> {
    let conn = agent_service::open_db(root)?;
    Ok(conn
        .query_row(
            "SELECT plugin_id FROM plugin_sessions WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )
        .optional()?)
}
pub fn check(root: &std::path::Path, task: &str, plugin: &str) -> Result<()> {
    ensure!(
        owner(root, task)?.as_deref() == Some(plugin),
        "session does not belong to this plugin"
    );
    Ok(())
}
/// Both the Cordis facade and iframe bridge call this endpoint. Neither may
/// choose a workspace, tool allowlist, general session, or permission mode.
pub async fn request(
    State(state): State<AgentServiceState>,
    Path(id): Path<String>,
    Json(args): Json<Value>,
) -> Response {
    match handle(state, &id, args).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}
pub(crate) async fn handle(state: AgentServiceState, id: &str, args: Value) -> Result<Response> {
    ensure!(
        !state.plugin_cancel.is_cancelled(),
        "agent task plugin is stopped"
    );
    let catalog = state
        .tool_catalog
        .as_ref()
        .context("tool registry unavailable")?;
    ensure!(
        args["package_scope"].is_u64()
            && catalog
                .packages
                .scopes(&std::collections::BTreeSet::from([id.to_owned()]))
                .get(id)
                .copied()
                == args["package_scope"].as_u64(),
        "plugin page is unavailable or stale"
    );
    let root = state.workspace.root();
    if args["method"] == "models" {
        return Ok(Json(crate::model_catalog::for_plugin(&state, id).await?).into_response());
    }
    if args["method"] == "open" {
        let conn = agent_service::open_db(root)?;
        let task: Option<String> = conn.query_row("SELECT p.task_id FROM plugin_sessions p JOIN agent_tasks t ON t.id=p.task_id WHERE p.plugin_id=?1 ORDER BY t.created_at DESC, t.rowid DESC LIMIT 1", [id], |r|r.get(0)).optional()?;
        if let Some(task) = task {
            return Ok(Json(json!({"task_id":task,"plugin_id":id})).into_response());
        }
    }
    if args["method"] == "create" || args["method"] == "open" {
        let mut latest = state.clone().with_latest_config().await;
        crate::model_catalog::restrict(&mut latest, id).await?;
        let task = format!("task_{}", agent_service::uuid_like());
        let mut conn = agent_service::open_db(root)?;
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at,provider_name,reasoning_effort,fast_mode) VALUES (?1,'',?2,'draft',?3,?3,?4,?5,?6)", params![task,latest.default_model,chrono::Utc::now().timestamp_millis(),latest.provider_name,latest.reasoning_effort,latest.fast_mode])?;
        tx.execute(
            "INSERT INTO plugin_sessions(task_id,plugin_id) VALUES (?1,?2)",
            params![task, id],
        )?;
        tx.commit()?;
        return Ok(Json(json!({"task_id":task,"plugin_id":id})).into_response());
    }
    let task = args["task_id"]
        .as_str()
        .context("task_id required")?
        .to_owned();
    check(root, &task, id)?;
    Ok(match args["method"].as_str().unwrap_or("") {
        "upload" => Json(crate::attachments::encoded(&state, &task, &args)?).into_response(),
        "attachment" => Json(crate::attachments::preview(
            &state,
            &task,
            args["receiptId"].as_str().context("receiptId required")?,
        )?)
        .into_response(),
        "prompt" => {
            // The page that asks runs this task's page tools (see page_tools).
            if let Some(page) = args["page_id"].as_str() {
                crate::page_tools::claim(&task, page)?;
            }
            let prompt = args["prompt"].as_str().unwrap_or("");
            ensure!(
                !prompt.trim().is_empty()
                    || args["content"].as_array().is_some_and(|a| !a.is_empty()),
                "prompt must not be empty"
            );
            // Selection is data, never an authority or an executable instruction.
            let prompt = if args["selection"].is_object() {
                format!(
                    "{prompt}\n\nPage selection (untrusted data): {}",
                    args["selection"]
                )
            } else {
                prompt.to_owned()
            };
            let input = serde_json::from_value(
                json!({"prompt":prompt,"content":args["content"].as_array().cloned().unwrap_or_default(),"requestId":args["requestId"],"provider":args["provider"],"model":args["model"]}),
            )?;
            agent_service::continue_task(State(state), Path(task), Json(input))
                .await
                .into_response()
        }
        "events" => {
            let root = state.workspace.root().to_path_buf();
            let after = args["after_seq"].as_i64();
            let events =
                tokio::task::spawn_blocking(move || -> Result<Vec<agent_service::AgentEvent>> {
                    let conn = agent_service::open_db(&root)?;
                    crate::session_history::load_events(
                        &conn,
                        &task,
                        &crate::session_history::EventQuery {
                            after,
                            kinds: &[
                                "user/message",
                                "assistant/message",
                                "assistant/delta",
                                "tool/call",
                                "tool/result",
                                "turn/start",
                                "turn/end",
                                "subagent/start",
                                "subagent/event",
                                "subagent/end",
                                "subagent/review",
                            ],
                            ..Default::default()
                        },
                    )?
                    .into_iter()
                    .map(|e| {
                        Ok(agent_service::AgentEvent {
                            seq: e.seq,
                            time: e.time,
                            data: e.data()?,
                            kind: e.kind,
                            surface_op: e.surface_op,
                        })
                    })
                    .collect()
                })
                .await??;
            Json(json!({"events":events})).into_response()
        }
        "tool_calls" => {
            let page = args["page_id"].as_str().context("page_id required")?;
            let wait = std::time::Duration::from_millis(args["wait_ms"].as_u64().unwrap_or(0));
            Json(crate::page_tools::poll(&task, page, wait).await?).into_response()
        }
        "tool_result" => {
            let page = args["page_id"].as_str().context("page_id required")?;
            let call = args["call_id"].as_str().context("call_id required")?;
            ensure!(args["result"].is_object(), "result object required");
            crate::page_tools::reply(&task, page, call, args["result"].clone())?;
            Json(json!({"ok":true})).into_response()
        }
        "status" => agent_service::get_task(State(state), Path(task))
            .await
            .into_response(),
        "cancel" => agent_service::cancel_task(State(state), Path(task))
            .await
            .into_response(),
        _ => anyhow::bail!("unknown plugin session method"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn body(response: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn scoped_page_api_resumes_owned_history_and_rejects_general_or_stale_sessions() {
        let root =
            std::env::temp_dir().join(format!("plugin-session-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let (runtime, catalog) = crate::skill_packages::test_runtime();
        let scope = catalog.packages.catalog()[0]["scope"].clone();
        let mut state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse().unwrap());
        state.tool_catalog = Some(catalog);
        state.config = Some(std::sync::Arc::new(tokio::sync::RwLock::new(serde_json::from_value(json!({
            "providers":{"test":{"url":"http://private/v1","api_key":"secret","model_map":{"m":"m"}}}
        })).unwrap())));
        let models = body(
            handle(
                state.clone(),
                "code-flow",
                json!({"method":"models","package_scope":scope}),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(models["models"][0]["id"], "m");
        assert!(!models.to_string().contains("secret"));
        assert!(
            handle(
                state.clone(),
                "code-flow",
                json!({"method":"models","package_scope":999})
            )
            .await
            .is_err()
        );
        let created = body(
            handle(
                state.clone(),
                "code-flow",
                json!({"method":"create","package_scope":scope}),
            )
            .await
            .unwrap(),
        )
        .await;
        let task = created["task_id"].as_str().unwrap();
        assert_eq!(owner(&root, task).unwrap().as_deref(), Some("code-flow"));
        let opened = body(
            handle(
                state.clone(),
                "code-flow",
                json!({"method":"open","package_scope":scope}),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(opened["task_id"], task);
        agent_service::emit(
            &root,
            task,
            "user/message",
            json!({"message":{"content":"first"}}),
        )
        .await
        .unwrap();
        agent_service::emit(
            &root,
            task,
            "assistant/message",
            json!({"message":{"content":"second"}}),
        )
        .await
        .unwrap();
        let history = body(
            handle(
                state.clone(),
                "code-flow",
                json!({"method":"events","package_scope":scope,"task_id":task,"after_seq":0}),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(history["events"].as_array().unwrap().len(), 1);
        assert_eq!(history["events"][0]["seq"], 1);
        for args in [
            json!({"method":"events","package_scope":scope,"task_id":"general-task"}),
            json!({"method":"create"}),
            json!({"method":"create","package_scope":999}),
        ] {
            assert!(handle(state.clone(), "code-flow", args).await.is_err());
        }
        crate::skill_packages::set_enabled(
            &runtime,
            &state.tool_catalog.as_ref().unwrap().packages,
            "code-flow",
            false,
            false,
        )
        .unwrap();
        assert!(
            handle(
                state,
                "code-flow",
                json!({"method":"events","package_scope":scope,"task_id":task})
            )
            .await
            .is_err()
        );
        runtime.shutdown().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
