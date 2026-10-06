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
        let mut model = Self::model_round(scheduler, identity, runtime, body).await?;
        scheduler.visual_response_received(runtime.visual_dispatch);
        model.response_stats["model_round_ms"] = serde_json::json!(started.elapsed().as_millis() as u64);
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
        let response = response.error_for_status()?;
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
        let outcome = output.as_ref().and_then(|o| o["outcome"].as_str()).map(str::to_owned)
            .or_else(|| if done { Some("completed".to_string()) } else { None });
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
        scheduler.close_if_satisfied();
        if scheduler.output().is_some() {
            scheduler.attach_return_data(&work_state.unit_context(scheduler.id(), &[]), source_working_set.materials());
        }
        if scheduler.done() && task_tree.enabled() && task_tree.active() == scheduler.node() {
            task_tree.remember_sources(source_working_set.snapshot(), work_state.edit_targets());
            if let Some(output) = scheduler.output() {
                task_tree.complete_work(output)?;
            }
        }
        let tick = Self::make_tick(identity, operations_count, repeated_reads, scheduler);
        scheduler.accept(&tick);
        scheduler.save_selection(source_working_set.snapshot());
        Ok(Self::make_tick(identity, operations_count, repeated_reads, scheduler))
    }
}
