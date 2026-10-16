//! End-of-execution retrospectives over the shared original conversation.
//! Observer checks delivery after execution and returns its original review.
//! The implementation is retained independently of the single-Agent runtime.
use crate::agent_service::{self, AgentServiceState};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

// Current task view; 256k is a maximum, not a target snapshot size.
pub const INPUT_BUDGET: usize = crate::context_window::MAX_TOKENS;

/// Inputs for a retained Observer review, independent of task scheduling.
#[derive(Default)]
pub struct ReviewScope {
    pub request_id: usize,
    pub goal: String,
    pub final_result: String,
    pub request_completed: Option<bool>,
}

/// Observer is a one-shot retrospective after execution has ended.
/// There is no background reviewer, advisory queue or execution gate.
pub struct ObserverSession {
    enabled: bool,
    state: AgentServiceState,
    model: String,
    root: PathBuf,
    task: String,
    scope: Value,
    finished: bool,
    cancel: CancellationToken,
    review: Option<Value>,
}
impl ObserverSession {
    pub fn start(
        state: AgentServiceState,
        model: String,
        task: String,
        cancel: &CancellationToken,
    ) -> Self {
        Self {
            enabled: state.observer_enabled,
            root: state.workspace.root().to_path_buf(),
            state,
            model,
            task,
            scope: Value::Null,
            finished: false,
            cancel: cancel.clone(),
            review: None,
        }
    }
    pub fn set_scope(&mut self, scope: &ReviewScope, prompt: &str) {
        self.scope = json!({"request_id":scope.request_id,"goal":if scope.goal.is_empty(){prompt}else{&scope.goal},
            "final_result":scope.final_result,"request_completed":scope.request_completed});
    }
    /// Only an explicit successful Observer return requests another execution.
    /// A missing action, read failure or timeout never implies a business defect.
    pub fn followup(&self) -> Option<Value> {
        self.review
            .as_ref()
            .filter(|review| {
                review["status"] == "completed"
                    && review["observer_return"]["action"] == "continue"
                    && !self.cancel.is_cancelled()
            })
            .cloned()
    }
    pub fn next_execution(&mut self) {
        self.finished = false;
        self.review = None;
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
    pub async fn finish(&mut self, outcome: &str) -> Result<()> {
        if !self.enabled || self.finished || self.cancel.is_cancelled() {
            return Ok(());
        }
        self.finished = true;
        // A user cancellation or dropped execution must not start another model request.
        if !matches!(outcome, "completed" | "failed" | "max_steps") {
            return Ok(());
        }
        let history = crate::session_history::task_process(
            &self.root,
            &self.task,
            self.scope["request_id"].as_u64().unwrap_or(1) as usize,
        )
        .await?;
        let turn = history["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|record| record["turn"].as_u64())
            .max()
            .unwrap_or(1);
        agent_service::emit(
            &self.root,
            &self.task,
            "observer/retrospective_start",
            json!({"outcome":outcome,"request_id":self.scope["request_id"],"turn":turn}),
        )
        .await?;
        let result = match retrospective(
            &self.state,
            &self.model,
            &self.task,
            &self.scope,
            outcome,
            history,
            turn,
            &self.cancel,
        )
        .await
        {
            Ok(review) => review,
            Err(error) => {
                json!({"status":"unavailable","outcome":outcome,"message":format!("{error:#}"),"memoryRecorded":false,"turn":turn,"request_id":self.scope["request_id"]})
            }
        };
        agent_service::emit(
            &self.root,
            &self.task,
            "observer/retrospective",
            result.clone(),
        )
        .await?;
        self.review = Some(result);
        Ok(())
    }
}

async fn retrospective(
    state: &AgentServiceState,
    model: &str,
    task: &str,
    scope: &Value,
    outcome: &str,
    task_history: Value,
    turn: u64,
    cancel: &CancellationToken,
) -> Result<Value> {
    let input = json!({"stage":"retrospective","identity":{"task_id":task,"request_id":scope["request_id"],"node_id":"request"},
        "request":{"goal":scope["goal"]},"outcome":outcome,"goal_achieved":scope["request_completed"],
        "final_result":scope["final_result"],"task_history":task_history,
        "history":{"tool":"read_session_history","scope":"Read original process records, or include_context=true to inspect the exact saved Worker/Organizer model inputs referenced by debug/context_request. Input views are not additional executions."}});
    let raw=agent_service::observer_json_response_cancelled(state,model,include_str!("../prompts/observer_retrospective.md"),input,3072,task,
        json!({"stage":"retrospective","nodeId":"request","turn":turn,"request_id":scope["request_id"]}),cancel).await?;
    let report: Value = match serde_json::from_str::<Value>(
        raw.trim()
            .trim_start_matches("```json")
            .trim_end_matches("```")
            .trim(),
    ) {
        Ok(report) if report.is_object() => report,
        parsed => {
            return Ok(
                json!({"status":"unavailable","turn":turn,"request_id":scope["request_id"],
            "message":parsed.err().map(|error|error.to_string()).unwrap_or_else(||"Observer retrospective must return a JSON object".into()),
            "observer_return":raw,"memoryRecorded":false}),
            );
        }
    };
    // Routine retrospectives belong to conversation history. Keep the explicit
    // memory tools available, but do not turn a task review into a memory write.
    let original = report.get("observer_return").unwrap_or(&report);
    let mut result = json!({"status":"completed","summary":original["summary"],"pathReview":original["path_review"],
        "action":original["action"],"acceptance":original["acceptance"],"request_id":scope["request_id"],
        "shorteningOpportunities":original["shortening_opportunities"],"workFindings":original["work_findings"],
        "routeShortcuts":original["route_shortcuts"].as_array().into_iter().flatten().map(|item|{let mut item=item.clone();item["lookFirst"]=item["look_first"].clone();item}).collect::<Vec<_>>(),"memoryRecorded":false,"stored":false,"observer_return":original});
    result["turn"] = json!(turn);
    Ok(result)
}

/// Share existing read tools and their workspace boundaries. No new shell,
/// editing, browser navigation, upload, screenshot or automatic memory path.
pub fn tools(state: &AgentServiceState) -> Vec<Value> {
    let mut tools =
        agent_service::local_tools(&state.workspace, state.tool_catalog.as_deref(), true)
            .into_iter()
            .filter(|tool| {
                tool["function"]["name"]
                    .as_str()
                    .is_some_and(|name| read_tool(name) && state.permission_mode.allows(name))
            })
            .collect::<Vec<_>>();
    tools.push(crate::session_history::tool());
    tools
}
pub fn read_tool(name: &str) -> bool {
    matches!(
        name,
        "workspace_info"
            | "list_dir"
            | "read_file"
            | "read_file_lines"
            | "search_text"
            | "search_code_map"
            | "view_image"
            | "search_symbol_business_context"
            | "list_symbol_business_context"
            | "browser_read"
            | "browser_diagnostics"
    ) || ["go", "rust", "ts", "python"].iter().any(|language| {
        name == format!("search_{language}_symbols")
            || name == format!("list_{language}_symbols")
            || name == format!("read_{language}_symbol")
    })
}

/// A large acceptance starts from original role deliveries, not copied source
/// and every intermediate tool body. All omitted originals remain in history.
/// This is message routing; it does not judge requirements or summarize facts.
pub fn acceptance_history(history: &Value) -> Value {
    history.clone()
}

#[cfg(test)]
fn iteration_records(process: &Value, after_seq: i64, through_seq: i64) -> Vec<Value> {
    process["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry["seq"]
                .as_i64()
                .is_some_and(|seq| seq > after_seq && seq <= through_seq)
                && entry["kind"] != "observer/node_review"
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_is_an_explicit_original_return_not_a_status_or_keyword_inference() {
        let root = std::env::temp_dir();
        let state = agent_service::tests::test_state(&root, "127.0.0.1:9".parse().unwrap());
        let cancel = CancellationToken::new();
        let mut observer =
            ObserverSession::start(state.clone(), "fake".into(), "task".into(), &cancel);
        for report in [
            json!({"status":"unavailable","observer_return":{"action":"continue"}}),
            json!({"status":"completed","observer_return":{"summary":"需要继续修改"}}),
            json!({"status":"completed","observer_return":{"action":"deliver","acceptance":{"assessment":"blocked"}}}),
        ] {
            observer.review = Some(report);
            assert!(observer.followup().is_none());
        }
        let report = json!({"status":"completed","observer_return":{"action":"continue","handoff":{"original":"reason\n尾部","extra":[1,null,false]}}});
        observer.review = Some(report.clone());
        assert_eq!(observer.followup(), Some(report));
        cancel.cancel();
        assert!(observer.followup().is_none());
        for permission in [
            agent_service::PermissionMode::ReadOnly,
            agent_service::PermissionMode::FullAccess,
        ] {
            let mut state = state.clone();
            state.permission_mode = permission;
            let tools = tools(&state);
            assert!(
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "read_file")
            );
            assert!(
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "view_image")
            );
            assert!(
                tools
                    .iter()
                    .all(|tool| tool["function"]["name"] == "read_session_history"
                        || read_tool(tool["function"]["name"].as_str().unwrap()))
            );
            assert!(
                !tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "write_file"
                        || tool["function"]["name"] == "browser_upload")
            );
            assert_eq!(
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "browser_read"),
                matches!(permission, agent_service::PermissionMode::FullAccess)
            );
        }
    }

    #[test]
    fn acceptance_scope_uses_the_effective_human_goal_not_a_continue_message() {
        let state =
            agent_service::tests::test_state(&std::env::temp_dir(), "127.0.0.1:9".parse().unwrap());
        let mut observer = ObserverSession::start(
            state,
            "fake".into(),
            "task".into(),
            &CancellationToken::new(),
        );
        let mut scope = ReviewScope::default();
        scope.goal = "Current replacement requirement".into();
        observer.set_scope(&scope, "continue");
        assert_eq!(observer.scope["goal"], scope.goal);
    }
    #[test]
    fn large_acceptance_routes_original_returns_without_rewriting_or_claiming_full_coverage() {
        let input = json!({"coverage":"complete","total_records":5,"history_reference":{"task_id":"t"},"records":[
            {"seq":1,"kind":"user/message","payload":"original requirement"},
            {"seq":2,"kind":"tool/result","payload":{"raw":"源代码".repeat(1000)}},
            {"seq":3,"kind":"worker/yield","payload":{"reason":"原始原因", "extra":[false,null,1]}},
            {"seq":4,"kind":"visual/input_result","payload":{"content":[{"type":"image_ref","artifact_id":"image"}]}},
            {"seq":5,"kind":"tool/result","is_error":true,"payload":{"error":"原始失败原因", "diagnostics":[false,null,1]}}]});
        let routed = acceptance_history(&input);
        assert_eq!(routed["records"], input["records"]);
        assert!(routed.get("omitted_event_refs").is_none());
        assert_eq!(routed["total_records"], 5);
        assert_eq!(routed["history_reference"], input["history_reference"]);
        assert_eq!(routed["coverage"], "complete");
        assert_eq!(input["records"][1]["payload"]["raw"], "源代码".repeat(1000));
    }
    #[tokio::test]
    async fn cancellation_during_acceptance_keeps_error_without_dispatching_more_work() {
        let root = std::env::temp_dir().join(format!(
            "observer-cancel-{}",
            crate::visual_artifacts::new_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let conn = agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        drop(conn);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let notified = std::sync::Arc::new(tokio::sync::Notify::new());
        let arrived = notified.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move || {
                let arrived = arrived.clone();
                async move {
                    arrived.notify_one();
                    std::future::pending::<String>().await
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let cancel = CancellationToken::new();
        let cancellation = cancel.clone();
        let abort = tokio::spawn(async move {
            notified.notified().await;
            cancellation.cancel();
        });
        let mut state = agent_service::tests::test_state(&root, address);
        state.observer_enabled = true;
        state.observer_provider_url = state.provider_url.clone();
        let mut observer = ObserverSession::start(state, "fake".into(), "task".into(), &cancel);
        observer.set_scope(&ReviewScope::default(), "goal");
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            observer.finish("completed"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(observer.followup().is_none());
        let review = observer.review.as_ref().unwrap();
        assert_eq!(review["status"], "unavailable");
        assert!(
            review["message"]
                .as_str()
                .unwrap()
                .contains("cancelled by user"),
            "{review}"
        );
        abort.await.unwrap();
        server.abort();
        let _ = server.await;
        let conn = agent_service::open_db(&root).unwrap();
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_request_contexts WHERE json_extract(metadata,'$.status')='cancelled'",[],|row|row.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM agent_task_events WHERE kind='observer/followup'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn disabled_cancelled_interrupted_and_dropped_sessions_never_review() {
        let root = std::env::temp_dir().join(format!(
            "observer-no-background-{}",
            crate::visual_artifacts::new_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let conn = agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','completed',0,0)",[]).unwrap();
        drop(conn);
        let mut state = agent_service::tests::test_state(&root, "127.0.0.1:9".parse().unwrap());
        let mut scope = ReviewScope::default();
        scope.request_id = 1;
        for (enabled, outcome) in [
            (false, "completed"),
            (true, "cancelled"),
            (true, "interrupted"),
        ] {
            state.observer_enabled = enabled;
            let mut observer = ObserverSession::start(
                state.clone(),
                "fake".into(),
                "task".into(),
                &CancellationToken::new(),
            );
            observer.set_scope(&scope, "goal");
            observer.finish(outcome).await.unwrap();
        }
        drop(ObserverSession::start(
            state.clone(),
            "fake".into(),
            "task".into(),
            &CancellationToken::new(),
        ));
        tokio::task::yield_now().await;
        let conn = agent_service::open_db(&root).unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='agent_request_contexts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let requests: i64 = if exists {
            conn.query_row("SELECT count(*) FROM agent_request_contexts", [], |row| {
                row.get(0)
            })
            .unwrap()
        } else {
            0
        };
        assert_eq!(requests, 0);
        let reviews: i64 = conn
            .query_row(
                "SELECT count(*) FROM agent_task_events WHERE kind LIKE 'observer/%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reviews, 0);
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
}
