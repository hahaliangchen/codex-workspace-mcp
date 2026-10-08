//! Single-process work executor.
//! Executes a single step/tick of the active task process.
//! Scheduling decisions (selecting/creating/switching processes) belong to WorkScheduler and Organizer.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use anyhow::{Result, ensure};
use crate::work_scheduler::{WorkScheduler, WorkFrame};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CapturedIdentity {
    pub node_id: String,
    pub work_id: String,
    pub revision: usize,
    pub plan_revision: usize,
    pub step: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExecutionTick {
    pub step: usize,
    pub node_id: String,
    pub work_id: String,
    pub revision: usize,
    pub plan_revision: usize,
    pub operations_count: usize,
    pub repeated_reads: usize,
    pub done: bool,
    pub outcome: Option<String>,
    pub summary: Option<String>,
    pub output: Option<Value>,
    pub handoff: Option<Value>,
    pub should_yield_to_organizer: bool,
}

pub struct WorkExecutorRuntime<'a> {
    pub client: &'a reqwest::Client,
    pub provider_url: &'a str,
    pub api_key: &'a str,
    pub root: &'a std::path::Path,
    pub task_id: &'a str,
    pub turn: usize,
    pub step: usize,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub visual_dispatch:&'a Value,
}

pub struct ModelRoundOutput {
    pub message: Value,
    pub text: String,
    pub tool_calls: Vec<crate::format_translate::OpenAiChatToolCall>,
    pub response_stats: Value,
    pub headers_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RoundDisposition { Continue, Yield, End, Cancelled }

pub struct ToolRoundOutput {
    pub operations_count: usize,
    pub repeated_reads: usize,
    pub disposition: RoundDisposition,
}

impl ToolRoundOutput {
    pub fn stop(disposition: RoundDisposition) -> Self {
        Self { operations_count: 0, repeated_reads: 0, disposition }
    }
}

#[derive(Debug)]
pub struct ExecutionRound {
    pub tick: ExecutionTick,
    pub disposition: RoundDisposition,
}

pub struct WorkExecutor;

impl WorkExecutor {
    pub fn new() -> Self { Self }

    /// Returns the active task process from the scheduler.
    pub fn current_process<'a>(scheduler: &'a WorkScheduler) -> Option<&'a WorkFrame> {
        scheduler.current_process()
    }

    /// Captures the execution identity at invocation start of a step.
    pub fn capture_identity(scheduler: &WorkScheduler, step: usize) -> CapturedIdentity {
        CapturedIdentity {
            node_id: scheduler.node().to_string(),
            work_id: scheduler.id().to_string(),
            revision: scheduler.revision(),
            plan_revision: scheduler.plan_revision,
            step,
        }
    }

    /// Evaluates whether the active process should yield to the Organizer.
    /// Yield happens on:
    /// - Completion of current process (`done == true`)
    /// - Explicit handoff request (blocker, need_split, upstream_problem)
    /// - Stall condition (idle rounds or repeated reads exceeding threshold)
    pub fn should_yield(scheduler: &WorkScheduler) -> bool {
        scheduler.needs_organizer()
    }

    /// Validates whether a tool operation is permitted for the active task process.
    pub fn check_permits(scheduler: &WorkScheduler, tool_name: &str, args: &Value) -> Result<()> {
        scheduler.permits(tool_name, args)
    }

    /// Handles a yield_work tool call by delegating to scheduler.return_work.
    pub fn handle_yield_work(scheduler: &mut WorkScheduler, args: &Value) -> Result<Value> {
        scheduler.return_work(args)
    }

    /// Filters available tools for the active task process based on its completion contract and current state.
    pub fn filter_tools(scheduler: &WorkScheduler, tools: &mut Vec<Value>) {
        scheduler.filter_tools(tools);
    }

    /// Executes the model, the host's tool adapter, and local result archival within
    /// one round. Scheduling cannot run between tool execution and the returned Tick.
    pub async fn next(
        scheduler: &mut WorkScheduler,
        task_tree: &mut crate::flow_tree::TaskTree,
        work_state: &mut crate::worker_work_state::WorkState,
        source_working_set: &mut crate::task_notebook::SourceWorkingSet,
        identity: &CapturedIdentity,
        runtime: &WorkExecutorRuntime<'_>,
        body: &Value,
        trace: &mut Option<crate::request_context::Trace>,
        execute: impl AsyncFnOnce(
            &mut WorkScheduler,
            &mut crate::flow_tree::TaskTree,
            &mut crate::worker_work_state::WorkState,
            &mut crate::task_notebook::SourceWorkingSet,
            ModelRoundOutput,
        ) -> Result<ToolRoundOutput>,
    ) -> Result<ExecutionRound> {
        let started = std::time::Instant::now();
        let mut model = Self::model_round(scheduler, identity, runtime, body).await.map_err(|error|
            if runtime.visual_dispatch["status"] != "no_images" {
                crate::visual_probe::VisualModelFailure {manifest:runtime.visual_dispatch.clone(),error}.into()
            }else{error})?;
        model.response_stats["model_round_ms"] = serde_json::json!(started.elapsed().as_millis() as u64);
        if let Some(reason)=worker_response_rejection(&model.response_stats) {
            model.response_stats["worker_execution_rejected"]=serde_json::json!(reason);
            crate::request_context::finish(trace.take(),"invalid_response",model.response_stats.clone()).await;
            anyhow::bail!("Worker response rejected before tool execution: {reason}");
        }
        scheduler.visual_response_received(runtime.visual_dispatch);
        crate::request_context::finish(trace.take(), "completed", model.response_stats.clone()).await;
        let tools = execute(scheduler, task_tree, work_state, source_working_set, model).await?;
        let tick = Self::advance(identity, tools.operations_count, tools.repeated_reads,
            scheduler, task_tree, work_state, source_working_set)?;
        Ok(ExecutionRound { tick, disposition: tools.disposition })
    }

    async fn model_round(
        scheduler: &WorkScheduler,
        identity: &CapturedIdentity,
        runtime: &WorkExecutorRuntime<'_>,
        body: &Value,
    ) -> Result<ModelRoundOutput> {
        ensure!(
            identity.work_id == scheduler.id() && identity.node_id == scheduler.node()
                && identity.revision == scheduler.revision() && identity.plan_revision == scheduler.plan_revision,
            "execution identity mismatch before model round: expected {}/{}, got {}/{}",
            scheduler.id(), scheduler.node(), identity.work_id, identity.node_id
        );
        let model_request_started = std::time::Instant::now();
        let request_fut = runtime.client.post(format!(
            "{}/chat/completions", runtime.provider_url.trim_end_matches('/')
        )).bearer_auth(runtime.api_key).header("x-codex-visual-dispatch","task-owned").json(body).send();

        let response = tokio::select! {
            _ = runtime.cancel.cancelled() => {
                anyhow::bail!("cancelled");
            }
            res = request_fut => res?,
        };
        let headers_ms = model_request_started.elapsed().as_millis() as u64;
        let response = crate::visual_probe::successful_response(response).await?;
        let (message, mut response_stats) = crate::agent_service::consume_model_stream(
            response, runtime.root, runtime.task_id, runtime.turn, runtime.step, runtime.cancel
        ).await?;
        let text = message.get("content").and_then(Value::as_str).unwrap_or("").to_owned();
        let tool_calls = crate::format_translate::collect_all_tool_calls_from_openai_chat(&message);
        response_stats["headers_ms"] = serde_json::json!(headers_ms);
        response_stats["response_chars"] = serde_json::json!(text.chars().count());
        response_stats["tool_calls"] = serde_json::json!(tool_calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>());

        Ok(ModelRoundOutput {
            message,
            text,
            tool_calls,
            response_stats,
            headers_ms,
        })
    }

    /// Creates an ExecutionTick from the captured identity and scheduler outcome.
    pub fn make_tick(
        identity: &CapturedIdentity,
        operations_count: usize,
        repeated_reads: usize,
        scheduler: &WorkScheduler,
    ) -> ExecutionTick {
        let done = scheduler.done();
        let output = scheduler.output().cloned();
        let handoff = scheduler.pending_handoff().cloned();
        let outcome = output.as_ref().and_then(|o| o["outcome"].as_str()).map(str::to_owned);
        let summary = output.as_ref().and_then(|o| o["summary"].as_str()).map(str::to_owned);
        let should_yield = scheduler.needs_organizer();

        ExecutionTick {
            step: identity.step,
            node_id: identity.node_id.clone(),
            work_id: identity.work_id.clone(),
            revision: identity.revision,
            plan_revision: identity.plan_revision,
            operations_count,
            repeated_reads,
            done,
            outcome,
            summary,
            output,
            handoff,
            should_yield_to_organizer: should_yield,
        }
    }

    /// Advances the current process by validating execution state, attaching return data,
    /// synchronizing with TaskTree if complete, and accepting the ExecutionTick bound to
    /// the captured identity from the start of the round.
    pub fn advance(
        identity: &CapturedIdentity,
        operations_count: usize,
        repeated_reads: usize,
        scheduler: &mut WorkScheduler,
        task_tree: &mut crate::flow_tree::TaskTree,
        work_state: &mut crate::worker_work_state::WorkState,
        source_working_set: &mut crate::task_notebook::SourceWorkingSet,
    ) -> Result<ExecutionTick> {
        ensure!(identity.work_id == scheduler.id() && identity.node_id == scheduler.node()
            && identity.revision == scheduler.revision() && identity.plan_revision == scheduler.plan_revision,
            "execution identity changed during tool round");
        if scheduler.output().is_some() {
            scheduler.attach_return_data(&work_state.unit_context(scheduler.id(), &[]), source_working_set.materials());
        }
        if scheduler.done() && task_tree.enabled() && task_tree.active() == scheduler.node() {
            task_tree.remember_sources(source_working_set.snapshot(), work_state.edit_targets());
            if let Some(output) = scheduler.output() {
                task_tree.record_work_return(output)?;
            }
        }
        let tick = Self::make_tick(identity, operations_count, repeated_reads, scheduler);
        scheduler.accept(&tick);
        scheduler.save_selection(source_working_set.snapshot());
        Ok(Self::make_tick(identity, operations_count, repeated_reads, scheduler))
    }
}

fn worker_response_rejection(stats:&Value)->Option<&'static str> {
    if stats["terminated"]!=true {
        return Some("stream ended before [DONE] or finish_reason");
    }
    match stats["finish_reason"].as_str() {
        Some("length")=>Some("model response stopped at the token limit"),
        Some("content_filter")=>Some("model response was stopped by the content filter"),
        _=>None,
    }
}

#[cfg(test)]
mod response_validation_tests {
    use super::*;
    use axum::{extract::State,routing::post,Router};
    use serde_json::json;
    use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn execution_tick_keeps_missing_worker_outcome_unknown() {
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![crate::work_scheduler::WorkOrder{id:"worker".into(),node_id:"worker".into(),
            goal:"observe a page".into(),done_when:"report what is visible".into(),..Default::default()}],false,false).unwrap();
        scheduler.activate_next().unwrap();
        scheduler.return_work(&json!({"summary":"The invocation returned without a categorical assessment."})).unwrap();
        let identity=WorkExecutor::capture_identity(&scheduler,1);
        let tick=WorkExecutor::make_tick(&identity,1,0,&scheduler);
        assert!(tick.done);
        assert!(tick.outcome.is_none());
        assert!(tick.output.as_ref().unwrap()["outcome"].is_null());
    }

    #[derive(Clone)]
    struct ScriptResponse { body:String, content_type:&'static str }

    async fn response(State(script):State<ScriptResponse>)->axum::response::Response {
        let mut response=axum::response::Response::new(axum::body::Body::from(script.body));
        response.headers_mut().insert(axum::http::header::CONTENT_TYPE,axum::http::HeaderValue::from_static(script.content_type));
        response
    }

    fn tool_delta()->String {
        format!("data: {}\n\n",json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-write","type":"function",
            "function":{"name":"write_file","arguments":"{}"}}]}}]}))
    }

    fn finish_event(reason:&str)->String {
        format!("data: {}\n\n",json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}))
    }

    async fn execute_model_script(body:String,content_type:&'static str)->(anyhow::Result<ExecutionRound>,usize,Value) {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let app=Router::new().route("/v1/chat/completions",post(response)).with_state(ScriptResponse{body,content_type});
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
        let root=std::env::temp_dir().join(format!("worker-stream-{}",crate::agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        {
            let conn=crate::agent_service::open_db(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','stream test','fake','running',0,0)",[]).unwrap();
            conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_context_debug(task_id TEXT PRIMARY KEY, enabled INTEGER NOT NULL DEFAULT 0);").unwrap();
            conn.execute("INSERT INTO agent_context_debug(task_id,enabled) VALUES ('task',1)",[]).unwrap();
        }
        let mut scheduler=WorkScheduler::default();
        scheduler.enqueue(vec![crate::work_scheduler::WorkOrder{id:"worker".into(),node_id:"worker".into(),goal:"write a file".into(),
            done_when:"the response is valid".into(),completion:crate::work_scheduler::Completion::Output,..Default::default()}],false,false).unwrap();
        scheduler.activate_next().unwrap();
        let identity=WorkExecutor::capture_identity(&scheduler,1);
        let mut task_tree=crate::flow_tree::TaskTree::default();
        let mut work_state=crate::worker_work_state::WorkState::default();
        let mut source_set=crate::task_notebook::SourceWorkingSet::default();
        let cancel=CancellationToken::new();let client=reqwest::Client::new();
        let provider_url=format!("http://{address}/v1");let visual_dispatch=json!({});
        let runtime=WorkExecutorRuntime{client:&client,provider_url:&provider_url,api_key:"test",root:&root,task_id:"task",turn:1,step:1,cancel:&cancel,visual_dispatch:&visual_dispatch};
        let request=json!({"model":"fake","stream":true,"messages":[],"tools":[{"type":"function","function":{"name":"write_file"}}]});
        let calls=Arc::new(AtomicUsize::new(0));let executed=calls.clone();
        let mut trace=crate::request_context::record(&root,"task",json!({"actor":"worker","turn":1,"step":1,"nodeId":"worker"}),&request).await;
        let result=WorkExecutor::next(&mut scheduler,&mut task_tree,&mut work_state,&mut source_set,&identity,&runtime,&request,&mut trace,
            async move |_scheduler,_task_tree,_work_state,_source_set,model| {
                executed.fetch_add(1,Ordering::Relaxed);
                Ok(ToolRoundOutput{operations_count:model.tool_calls.len(),repeated_reads:0,disposition:RoundDisposition::Continue})
            }).await;
        let metadata={let conn=crate::agent_service::open_db(&root).unwrap();
            let text:String=conn.query_row("SELECT metadata FROM agent_request_contexts WHERE task_id='task'",[],|row|row.get(0)).unwrap();
            serde_json::from_str(&text).unwrap()};
        server.abort();let _=std::fs::remove_dir_all(root);
        (result,calls.load(Ordering::Relaxed),metadata)
    }

    #[test]
    fn worker_response_validation_rejects_incomplete_or_non_normal_finish() {
        assert!(worker_response_rejection(&serde_json::json!({"terminated":false,"finish_reason":null})).is_some());
        assert!(worker_response_rejection(&serde_json::json!({"terminated":true,"finish_reason":"length"})).is_some());
        assert!(worker_response_rejection(&serde_json::json!({"terminated":true,"finish_reason":"content_filter"})).is_some());
        assert!(worker_response_rejection(&serde_json::json!({"terminated":true,"finish_reason":"tool_calls"})).is_none());
        assert!(worker_response_rejection(&serde_json::json!({"terminated":true,"response_format":"json","finish_reason":null})).is_none(),
            "a complete JSON response remains compatible without SSE finish metadata");
    }

    #[tokio::test]
    async fn incomplete_and_abnormal_worker_streams_never_execute_tool_calls() {
        let incomplete=tool_delta();
        let (result,executed,metadata)=execute_model_script(incomplete,"text/event-stream").await;
        let error=result.unwrap_err().to_string();
        assert!(error.contains("before tool execution"),"{error}");
        assert_eq!(executed,0,"EOF without a terminal event must not execute a complete-looking tool call");
        assert_eq!(metadata["status"],"invalid_response");
        assert_eq!(metadata["outcome"]["terminated"],false,"partial stream metrics are retained on the failed request");
        assert!(metadata["outcome"]["response_complete_ms"].is_number());

        for reason in ["length","content_filter"] {
            let response=format!("{}{}",tool_delta(),finish_event(reason));
            let (result,executed,_metadata)=execute_model_script(response,"text/event-stream").await;
            assert!(result.unwrap_err().to_string().contains("before tool execution"));
            assert_eq!(executed,0,"finish_reason={reason} must not execute side effects");
        }

        let normal=format!("{}{}data: [DONE]\n\n",tool_delta(),finish_event("tool_calls"));
        let (result,executed,_metadata)=execute_model_script(normal,"text/event-stream").await;
        assert!(result.is_ok(),"a normally terminated stream executes its call: {result:?}");
        assert_eq!(executed,1);

        let complete_json=json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call-write","type":"function",
            "function":{"name":"write_file","arguments":"{}"}}]}}]}).to_string();
        let (result,executed,_metadata)=execute_model_script(complete_json,"application/json").await;
        assert!(result.is_ok(),"complete non-stream JSON remains supported: {result:?}");
        assert_eq!(executed,1);
    }
}
