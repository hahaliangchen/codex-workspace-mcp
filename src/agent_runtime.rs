use std::sync::Arc;
use std::task::Context;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::future::join_all;
use futures::stream::poll_fn;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::error;

const MAX_AGENT_STEPS: usize = 30;
const MAX_CONSECUTIVE_FAILURES: usize = 3;
const MAX_CONSECUTIVE_NON_FINAL_TEXT: usize = 2;
const MAX_TOOL_OUTPUT_CHARS: usize = 300 * 1024;
const FINALITY_PROBE_MAX_TOKENS: usize = 8;

tokio::task_local! {
    pub static SURGERY_SENDER: mpsc::Sender<crate::expert_surgery::SurgeryEvent>;
}

pub enum EventBusMessage {
    Start,
    TextDelta(String),
    Surgery(crate::expert_surgery::SurgeryEvent),
    DelegatedToolCall(crate::format_translate::OpenAiChatToolCall),
    Finished,
}

pub async fn run_responses_agent(
    client: Client,
    workspace: Arc<crate::tools::Workspace>,
    provider_url: String,
    api_key: String,
    body: Value,
    upstream_model: String,
    client_model: String,
    expert_provider: Option<crate::expert_surgery::ExpertProvider>,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>(32);
    let (event_bus_tx, mut event_bus_rx) = mpsc::channel::<EventBusMessage>(128);
    let (surgery_tx, mut surgery_rx) = mpsc::channel::<crate::expert_surgery::SurgeryEvent>(64);

    let session = AgentSession::new(client_model.clone());
    let response_id = session.response_id.clone();
    let client_model_clone = client_model.clone();

    // Spawn the Event Bus Task
    let tx_clone = tx.clone();
    tokio::spawn(async move {
        let mut stream = AgentSseWriter::new(response_id.clone(), client_model_clone);
        while let Some(msg) = event_bus_rx.recv().await {
            match msg {
                EventBusMessage::Start => {
                    let _ = tx_clone.send(Ok(Bytes::from(stream.start()))).await;
                }
                EventBusMessage::TextDelta(delta) => {
                    if !delta.is_empty() {
                        let _ = tx_clone
                            .send(Ok(Bytes::from(stream.text_delta(&delta))))
                            .await;
                    }
                }
                EventBusMessage::Surgery(event) => {
                    let narrative =
                        crate::format_translate::codex_protocol::event_to_narrative(&event);
                    let _ = tx_clone
                        .send(Ok(Bytes::from(stream.text_delta(&narrative))))
                        .await;
                }
                EventBusMessage::DelegatedToolCall(tool_call) => {
                    let _ = tx_clone
                        .send(Ok(Bytes::from(stream.delegated_tool_call(&tool_call))))
                        .await;
                }
                EventBusMessage::Finished => {
                    let _ = tx_clone.send(Ok(Bytes::from(stream.finish()))).await;
                    break;
                }
            }
        }
    });

    // Spawn forwarder task for surgery events
    let event_bus_tx_for_surgery = event_bus_tx.clone();
    tokio::spawn(async move {
        while let Some(event) = surgery_rx.recv().await {
            let _ = event_bus_tx_for_surgery
                .send(EventBusMessage::Surgery(event))
                .await;
        }
    });

    // Spawn main agent runner task
    let event_bus_tx_for_agent = event_bus_tx.clone();
    tokio::spawn(async move {
        let _ = event_bus_tx.send(EventBusMessage::Start).await;

        let visible_images = crate::vision_preprocess::visible_images_from_body(&body);
        let result = crate::vision_preprocess::scope_visible_images(
            visible_images,
            SURGERY_SENDER.scope(
                surgery_tx,
                run_agent_loop(
                    client,
                    workspace,
                    provider_url,
                    api_key,
                    body,
                    upstream_model,
                    event_bus_tx_for_agent.clone(),
                    expert_provider,
                ),
            ),
        )
        .await;

        if let Err(error) = result {
            let text = format!("\n[agent:error] {}\n", error);
            let _ = event_bus_tx.send(EventBusMessage::TextDelta(text)).await;
        }

        let _ = event_bus_tx.send(EventBusMessage::Finished).await;
    });

    let mut rx = rx;
    let rx_stream = poll_fn(move |cx: &mut Context<'_>| rx.poll_recv(cx));

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(rx_stream))
        .unwrap_or_else(|e| {
            error!(%e, "failed to build agent response stream");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })
}

async fn run_agent_loop(
    client: Client,
    workspace: Arc<crate::tools::Workspace>,
    provider_url: String,
    api_key: String,
    mut body: Value,
    upstream_model: String,
    event_bus_tx: mpsc::Sender<EventBusMessage>,
    expert_provider: Option<crate::expert_surgery::ExpertProvider>,
) -> anyhow::Result<()> {
    let upstream_url = format!("{}/chat/completions", provider_url.trim_end_matches('/'));
    let requested_cwd = request_cwd_from_body(&body);
    let workspace = select_request_workspace(&workspace, requested_cwd.as_deref());
    send_text(
        &event_bus_tx,
        &format!(
            "📂 当前工作区：{}{}\n",
            workspace.root().display(),
            requested_cwd
                .as_deref()
                .map(|cwd| format!("（Codex cwd: {cwd}）"))
                .unwrap_or_default()
        ),
    )
    .await;
    let project_gate = run_project_analysis_gate(&body, &event_bus_tx).await;
    let should_run_project_prefetch = project_gate
        .as_ref()
        .map(|gate| gate.requires_project_analysis)
        .unwrap_or(true);
    if !should_run_project_prefetch {
        let gate = project_gate.as_ref().unwrap();
        send_debug_text(
            &event_bus_tx,
            &format!(
                "⚡ 便宜模型判断无需项目分析：{}。跳过索引刷新和架构预取。\n",
                gate.reason
            ),
        )
        .await;
    } else {
        send_debug_text(&event_bus_tx, "🧭 正在刷新代码索引...\n").await;
        let index_workspace = Arc::clone(&workspace);
        match tokio::task::spawn_blocking(move || {
            crate::index_refresh::refresh_workspace_indexes(&index_workspace)
        })
        .await
        {
            Ok(summary) => {
                send_debug_text(&event_bus_tx, &format_index_refresh_summary(&summary)).await;
            }
            Err(error) => {
                send_debug_text(
                    &event_bus_tx,
                    &format!("⚠️ 代码索引刷新任务失败：{}。将继续处理请求。\n", error),
                )
                .await;
            }
        }
        run_architecture_prefetch(&workspace, &body, &event_bus_tx).await;
    }

    body["model"] = json!(upstream_model);
    body["stream"] = json!(false);

    let prepared_tools = crate::tool_prepare::prepare_responses_tools(body.get("tools"));
    log_blocked_tools(&prepared_tools.blocked);
    let local_tool_names: std::collections::HashSet<String> = prepared_tools
        .tools
        .iter()
        .filter_map(|tool| {
            tool.get("name")
                .and_then(|name| name.as_str())
                .map(ToOwned::to_owned)
        })
        .collect();
    let delegated_tool_names: std::collections::HashSet<String> = prepared_tools
        .delegated_tools
        .iter()
        .filter_map(|tool| {
            tool.get("name")
                .and_then(|name| name.as_str())
                .map(ToOwned::to_owned)
        })
        .collect();
    let mut all_tools = prepared_tools.tools.clone();
    all_tools.extend(prepared_tools.delegated_tools.clone());
    let chat_tools = crate::format_translate::responses_tools_to_openai_chat_tools(&all_tools);

    ensure_agent_instructions(&mut body);
    let mut chat_messages = crate::format_translate::responses_body_to_openai_chat_messages(&body);
    if let Some(gate) = project_gate {
        chat_messages.push(project_gate_chat_message(&gate));
    }

    send_debug_text(&event_bus_tx, "🤖 [agent] 已接管本轮请求，开始分析。\n").await;

    let mut total_tool_calls = 0usize;
    let mut consecutive_failures = 0usize;
    let mut consecutive_non_final_text = 0usize;
    let mut steps_run = 0usize;
    for step in 1..=MAX_AGENT_STEPS {
        steps_run = step;
        let request_body = crate::format_translate::build_openai_chat_request(
            &body,
            &upstream_model,
            &chat_messages,
            &chat_tools,
        );
        log_agent_to_upstream_request(step, &upstream_model, &upstream_url, &request_body);

        let response = client
            .post(&upstream_url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&request_body)
            .send()
            .await?;

        let status = response.status();
        let response_text = response.text().await.unwrap_or_default();
        log_upstream_to_agent_response(step, status.as_u16(), &response_text);
        if !status.is_success() {
            anyhow::bail!(
                "upstream agent request failed: status={} body={}",
                status,
                response_text
            );
        }

        let response_json: Value = serde_json::from_str(&response_text)?;
        let assistant_message =
            crate::format_translate::openai_chat_assistant_message(&response_json);
        let tool_calls =
            crate::format_translate::collect_all_tool_calls_from_openai_chat(&assistant_message);
        if tool_calls.is_empty() {
            let text = crate::format_translate::collect_openai_chat_final_text(&assistant_message);
            if text.trim().is_empty() {
                log_agent_summary(steps_run, total_tool_calls, text.chars().count());
                send_debug_text(
                    &event_bus_tx,
                    "\n⚠️ [agent] 上游没有继续调用工具，但也没有给出文本答案。\n",
                )
                .await;
                return Ok(());
            }

            let finality = if step < MAX_AGENT_STEPS {
                confirm_text_finality(
                    &client,
                    &upstream_url,
                    &api_key,
                    &upstream_model,
                    latest_user_text_from_responses_body(&body)
                        .as_deref()
                        .unwrap_or(""),
                    &text,
                )
                .await
            } else {
                FinalityDecision::Done
            };

            if matches!(finality, FinalityDecision::Done) {
                log_agent_summary(steps_run, total_tool_calls, text.chars().count());
                send_text(&event_bus_tx, &text).await;
                return Ok(());
            }

            consecutive_non_final_text += 1;
            if consecutive_non_final_text > MAX_CONSECUTIVE_NON_FINAL_TEXT {
                log_agent_summary(steps_run, total_tool_calls, text.chars().count());
                send_debug_text(
                    &event_bus_tx,
                    "⚠️ 上游连续返回未完成的无工具文本，已停止继续追问并输出最近一次文本。\n",
                )
                .await;
                send_text(&event_bus_tx, &text).await;
                return Ok(());
            }

            if step < MAX_AGENT_STEPS {
                chat_messages.push(assistant_message);
                chat_messages.push(json!({
                    "role": "system",
                    "content": continuation_nudge_for_non_final_text(&text, &finality)
                }));
                send_debug_text(&event_bus_tx, "⚠️ 上游文本未确认完成，继续推进一轮。\n").await;
                continue;
            }

            log_agent_summary(steps_run, total_tool_calls, text.chars().count());
            send_text(&event_bus_tx, &text).await;
            return Ok(());
        }

        total_tool_calls += tool_calls.len();
        consecutive_non_final_text = 0;
        let mut step_failed = false;
        chat_messages.push(assistant_message);

        // Pre-flight: check for delegated/unknown tools before spawning any work.
        for tool_call in &tool_calls {
            if delegated_tool_names.contains(&tool_call.name) {
                send_debug_text(&event_bus_tx, &format!("↪️ {}\n", &tool_call.name)).await;
                let _ = event_bus_tx
                    .send(EventBusMessage::DelegatedToolCall(tool_call.clone()))
                    .await;
                return Ok(());
            }
            if !local_tool_names.contains(&tool_call.name) {
                send_debug_text(
                    &event_bus_tx,
                    &format!(
                        "\n⚠️ [tool:unknown] 上游请求了未知工具 {}，本地无法执行，也未由 Codex 注册。\n",
                        &tool_call.name
                    ),
                )
                .await;
                return Ok(());
            }
        }

        // Fire 🔧 notifications in order.
        for tool_call in &tool_calls {
            if let Some(reason) = tool_call_reason(tool_call) {
                send_debug_text(&event_bus_tx, &format!("{}\n", reason)).await;
            }
            send_debug_text(&event_bus_tx, &format!("⚡ {}\n", &tool_call.name)).await;
        }

        // Execute all local tools concurrently.
        let workspace_ref = &workspace;
        let tool_futures: Vec<_> = tool_calls
            .into_iter()
            .map(|tc| {
                let workspace = Arc::clone(workspace_ref);
                let expert = expert_provider.clone();
                async move {
                    let output = execute_local_tool(&workspace, &tc, expert.as_ref()).await;
                    (tc, output)
                }
            })
            .collect();
        let results = join_all(tool_futures).await;

        // Push results into chat_messages in original order, send ✅ notifications.
        for (tc, output) in results {
            if output.starts_with("Agent local tool failed:") {
                step_failed = true;
            }
            let bounded = bound_tool_output(&output);
            chat_messages.push(crate::format_translate::openai_chat_tool_result_message(
                &tc, &bounded,
            ));
            send_debug_text(
                &event_bus_tx,
                &format!(
                    "✅ {} 完成，返回 {} 字符{}。继续分析...\n",
                    &tc.name,
                    bounded.chars().count(),
                    tool_completion_note(&tc.name, &bounded)
                ),
            )
            .await;
        }

        if step_failed {
            consecutive_failures += 1;
            if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                send_debug_text(&event_bus_tx, "\n⏹️ 连续3次工具调用失败，已停止。\n").await;
                log_agent_summary(steps_run, total_tool_calls, 0);
                return Ok(());
            }
        } else {
            consecutive_failures = 0;
        }
    }

    send_debug_text(
        &event_bus_tx,
        "\n⏹️ 达到最大工具循环次数（30次），已停止。\n",
    )
    .await;
    log_agent_summary(steps_run, total_tool_calls, 0);
    Ok(())
}

fn ensure_agent_instructions(body: &mut Value) {
    let prefix = "You are the cheap Master Orchestrator inside a local Agent Runtime. You own the stateful ReAct loop and the complete tool registry. Use local workspace tools directly whenever current workspace files, logs, code, configuration, or repository state are needed. Do not ask the outer Codex client to execute tools. The top model is not an agent here: it is physically exposed only as the expert_code_surgery tool, a stateless pure function LargeModel(arguments) -> SEARCH/REPLACE patch. The expert has no tool visibility and cannot do nested tool calls. For complex code rewrites, especially changes crossing responsibilities, symbols, languages, or files, first gather verified local evidence with the matching language index/search/read tools, choose the target language (rust, typescript, python, or go), then call expert_code_surgery with language, a precise symbol_id, and a rewrite instruction. Do not send chat history, plans, or unrelated tool outputs in that instruction; pass only the verified constraints the expert needs. For small mechanical edits, local tools may still be used. Every tool call includes a user-visible `reason` argument; fill it with one short sentence explaining why that tool is being called, written as progress narration rather than private chain-of-thought. Do not stop at a progress note, plan, or future-tense statement such as saying you will inspect files; either call the needed tools or provide the complete final answer. Prefer parallel tool calls for independent reads, searches, and lookups, but do not batch expert_code_surgery with unrelated write tools. Before code changes or architecture questions, search_symbol_business_context and search_architecture_memory for the relevant business wording and feature/logic area. If no useful semantic memory exists, gather minimal verified evidence with indexed symbol tools, then call analyze_architecture_memory to let the configured cheap architecture model map the business logic and symbol roles. Pass only verified architecture memory, symbol business contexts, symbol index results, read_*_symbol output, or short code excerpts as evidence; do not send whole-project source. Set record=true only when the evidence is grounded enough to create/update durable architecture memory and symbol business contexts. For large changes that alter responsibilities, key symbols, boundaries, common tasks, risks, or symbol roles, update semantic memory before finishing. For code navigation, prefer indexed symbol tools first: search_go_symbols/search_rust_symbols/search_ts_symbols/search_python_symbols, list_*_symbols, then read_*_symbol with include_context when dependencies, callers, callees, or imports are useful. Use search_text mainly for literals, UI strings, log lines, config keys, error messages, or as a fallback when indexed symbol tools do not locate the code. Before editing, assess the relevant architecture area, smallest plausible change, files/symbols to touch, boundaries to avoid, regression risks, and coupling level. Distinguish business-critical information from incidental protocol/display/history noise: if unmatched tool calls, stale history, missing optional metadata, or display-only artifacts are not needed for the model to complete the user's task, prefer dropping, ignoring, normalizing, or isolating them instead of expanding the design to preserve them. If the minimal fix crosses unrelated architecture areas or requires broad shared-infrastructure changes, pause and explain the coupling, risks, and smaller alternatives to the user before making a large edit.";
    let current = body
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    body["instructions"] = if current.is_empty() {
        json!(prefix)
    } else {
        json!(format!("{}\n\n{}", prefix, current))
    };
}

fn format_index_refresh_summary(summary: &crate::index_refresh::IndexRefreshSummary) -> String {
    if summary.languages_detected.is_empty() {
        return "✅ 代码索引刷新完成：当前工作区未检测到可索引源码文件。\n".to_string();
    }

    let refreshed = if summary.languages_refreshed.is_empty() {
        "无成功刷新语言".to_string()
    } else {
        summary
            .languages_refreshed
            .iter()
            .map(|item| {
                format!(
                    "{} {} files / {} symbols",
                    item.language, item.files_indexed, item.symbols_indexed
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };

    if summary.failures.is_empty() {
        format!("✅ 代码索引已刷新：{}。\n", refreshed)
    } else {
        let failures = summary
            .failures
            .iter()
            .map(|item| format!("{}: {}", item.language, item.error))
            .collect::<Vec<_>>()
            .join("; ");
        format!("⚠️ 代码索引部分刷新：{}；失败：{}。\n", refreshed, failures)
    }
}

async fn run_architecture_prefetch(
    workspace: &Arc<crate::tools::Workspace>,
    body: &Value,
    event_bus_tx: &mpsc::Sender<EventBusMessage>,
) {
    let Some(query) = latest_user_text_from_responses_body(body) else {
        return;
    };
    if query.trim().is_empty() {
        return;
    }

    send_debug_text(event_bus_tx, "🧠 flash 正在分析用户意图和业务范围...\n").await;
    match build_architecture_prefetch_request(workspace, &query) {
        Ok(request) => {
            match crate::architecture_agent::analyze_architecture(workspace, request).await {
                Ok(response) => {
                    let analysis = response.analysis;
                    send_debug_text(
                        event_bus_tx,
                        &format!(
                            "📌 flash 业务定位：{}；关键符号 {} 个，语义释义 {} 个，已记录={}。\n",
                            analysis.area,
                            analysis.key_symbols.len(),
                            analysis.symbol_contexts.len(),
                            response.recorded
                        ),
                    )
                    .await;
                    if !analysis.minimal_change_scope.trim().is_empty() {
                        send_debug_text(
                            event_bus_tx,
                            &format!("🧭 建议最小范围：{}。\n", analysis.minimal_change_scope),
                        )
                        .await;
                    }
                }
                Err(error) => {
                    send_debug_text(
                        event_bus_tx,
                        &format!("⚠️ flash 业务分析失败：{}。将继续交给主模型处理。\n", error),
                    )
                    .await;
                }
            }
        }
        Err(error) => {
            send_debug_text(
                event_bus_tx,
                &format!("⚠️ flash 证据准备失败：{}。将继续交给主模型处理。\n", error),
            )
            .await;
        }
    }
}

fn build_architecture_prefetch_request(
    workspace: &Arc<crate::tools::Workspace>,
    query: &str,
) -> anyhow::Result<crate::architecture_agent::AnalyzeArchitectureRequest> {
    let workspace_root = workspace.root().display().to_string();
    let lookup_query = compact_lookup_query(query);
    let architecture_matches =
        workspace.search_architecture_memory(crate::memory::SearchArchitectureMemoryRequest {
            workspace_root: workspace_root.clone(),
            query: lookup_query.clone(),
            limit: 3,
        })?;
    let symbol_context_matches = workspace.search_symbol_business_context(
        crate::memory::SearchSymbolBusinessContextRequest {
            workspace_root: workspace_root.clone(),
            query: lookup_query.clone(),
            limit: 5,
        },
    )?;
    let text_matches = workspace.search_text(crate::tools::SearchTextRequest {
        workspace_root: Some(workspace_root.clone()),
        query: lookup_query,
        path: ".".to_string(),
        paths: Vec::new(),
        case_sensitive: false,
        respect_gitignore: true,
        max_matches: 8,
    })?;

    let mut evidence = Vec::new();
    if !architecture_matches.matches.is_empty() {
        evidence.push(format!(
            "Matched architecture memory:\n{}",
            architecture_matches
                .matches
                .iter()
                .map(|item| format!(
                    "Area: {}\nSummary: {}\nKey symbols: {}\nBoundaries: {}",
                    item.area,
                    item.summary,
                    item.key_symbols.join(", "),
                    item.boundaries
                ))
                .collect::<Vec<_>>()
                .join("\n\n")
        ));
    }
    if !symbol_context_matches.matches.is_empty() {
        evidence.push(format!(
            "Matched symbol business contexts:\n{}",
            symbol_context_matches
                .matches
                .iter()
                .map(|item| format!(
                    "Symbol: {} ({})\nArea: {}\nRole: {}\nRead when: {}\nAvoid when: {}",
                    item.symbol_name,
                    item.file_path,
                    item.belongs_to_area,
                    item.business_role,
                    item.read_when,
                    item.avoid_when
                ))
                .collect::<Vec<_>>()
                .join("\n\n")
        ));
    }
    if !text_matches.matches.is_empty() {
        evidence.push(format!(
            "Indexed/text search candidates (index_used={}, index_matches={}, text_scan_used={}):\n{}",
            text_matches.index_used,
            text_matches.index_matches,
            text_matches.text_scan_used,
            text_matches
                .matches
                .iter()
                .map(|item| format!("{}:{}:{} {}", item.path, item.line, item.column, item.text))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    Ok(crate::architecture_agent::AnalyzeArchitectureRequest {
        workspace_root,
        query: query.to_string(),
        focus: "Map the latest user request to the smallest relevant code/business logic area. Build or verify semantic memory from the provided index evidence.".to_string(),
        evidence,
        record: true,
    })
}

fn compact_lookup_query(query: &str) -> String {
    let compact = query.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(80).collect()
}

fn select_request_workspace(
    workspace: &Arc<crate::tools::Workspace>,
    requested_cwd: Option<&str>,
) -> Arc<crate::tools::Workspace> {
    let Some(cwd) = requested_cwd else {
        return Arc::clone(workspace);
    };
    match workspace.with_selected_root(&cwd) {
        Ok(selected) => Arc::new(selected),
        Err(_) => Arc::clone(workspace),
    }
}

fn request_cwd_from_body(body: &Value) -> Option<String> {
    find_workspace_root_hint(body)
        .or_else(|| {
            latest_user_text_from_responses_body(body)
                .and_then(|text| extract_tag_text(&text, "cwd"))
        })
        .filter(|cwd| !cwd.trim().is_empty())
        .map(|cwd| cwd.trim().to_string())
}

fn find_workspace_root_hint(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in [
                "cwd",
                "workspace_root",
                "workspaceRoot",
                "workdir",
                "current_dir",
                "currentDir",
            ] {
                if let Some(text) = map.get(key).and_then(|value| value.as_str()) {
                    if !text.trim().is_empty() {
                        return Some(text.trim().to_string());
                    }
                }
            }

            for item in map.values() {
                if let Some(cwd) = find_workspace_root_hint(item) {
                    return Some(cwd);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_workspace_root_hint),
        Value::String(text) => extract_tag_text(text, "cwd")
            .filter(|cwd| !cwd.trim().is_empty())
            .map(|cwd| cwd.trim().to_string()),
        _ => None,
    }
}

fn extract_tag_text(text: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let start = text.find(&start_tag)? + start_tag.len();
    let end = text[start..].find(&end_tag)? + start;
    Some(text[start..end].to_string())
}

fn latest_user_text_from_responses_body(body: &Value) -> Option<String> {
    if let Some(input_arr) = body.get("input").and_then(|v| v.as_array()) {
        for item in input_arr.iter().rev() {
            if item.get("role").and_then(|v| v.as_str()) == Some("user") {
                let text = response_content_to_plain_text(item.get("content").unwrap_or(item));
                if !text.trim().is_empty() {
                    return Some(text);
                }
            }
        }
    }

    if let Some(messages_arr) = body.get("messages").and_then(|v| v.as_array()) {
        for item in messages_arr.iter().rev() {
            if item.get("role").and_then(|v| v.as_str()) == Some("user") {
                let text = response_content_to_plain_text(item.get("content").unwrap_or(item));
                if !text.trim().is_empty() {
                    return Some(text);
                }
            }
        }
    }

    None
}

fn response_content_to_plain_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content"))
                    .and_then(|v| v.as_str())
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn log_blocked_tools(blocked_tools: &[crate::tool_prepare::BlockedTool]) {
    for blocked in blocked_tools {
        let label = match blocked.kind {
            crate::tool_prepare::BlockedToolKind::Type => "type",
            crate::tool_prepare::BlockedToolKind::Name => "name",
        };
        tracing::info!(
            "agent runtime blocked unsafe tool by {}: '{}'",
            label,
            blocked.value
        );
    }
}

fn log_agent_summary(steps: usize, tool_calls: usize, final_chars: usize) {
    tracing::info!(
        "agent runtime completed steps={} tool_calls={} final_chars={}",
        steps,
        tool_calls,
        final_chars
    );
}

fn log_agent_to_upstream_request(
    step: usize,
    upstream_model: &str,
    upstream_url: &str,
    request_body: &Value,
) {
    tracing::info!(
        "AGENT -> UPSTREAM step={} model={} url={} {}",
        step,
        upstream_model,
        upstream_url,
        summarize_chat_request(request_body)
    );
}

fn log_upstream_to_agent_response(step: usize, status: u16, response_text: &str) {
    tracing::info!(
        "UPSTREAM -> AGENT step={} status={} {}",
        step,
        status,
        summarize_chat_response(response_text)
    );
}

fn summarize_chat_request(body: &Value) -> String {
    let message_summary = summarize_chat_messages(body.get("messages"));
    let tool_count = body
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0);
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "<none>".to_string());
    format!(
        "stream={} messages={} tools={}",
        stream, message_summary, tool_count
    )
}

fn summarize_chat_messages(value: Option<&Value>) -> String {
    let Some(messages) = value.and_then(|v| v.as_array()) else {
        return "<none>".to_string();
    };
    let mut role_counts = std::collections::BTreeMap::<String, usize>::new();
    let mut tool_call_count = 0usize;
    for message in messages {
        let role = message
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("<none>")
            .to_string();
        *role_counts.entry(role).or_insert(0) += 1;
        tool_call_count += message
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .map(|arr| arr.len())
            .unwrap_or(0);
    }
    format!(
        "len={} role_counts={} tool_calls={}",
        messages.len(),
        serde_json::to_string(&role_counts).unwrap_or_else(|_| "{}".to_string()),
        tool_call_count
    )
}

fn summarize_chat_response(response_text: &str) -> String {
    let Ok(response_json) = serde_json::from_str::<Value>(response_text) else {
        return format!(
            "body={}",
            crate::ai_proxy::fmt_body(response_text.as_bytes())
        );
    };
    let assistant_message = crate::format_translate::openai_chat_assistant_message(&response_json);
    let tool_calls =
        crate::format_translate::collect_all_tool_calls_from_openai_chat(&assistant_message);
    let text = crate::format_translate::collect_openai_chat_final_text(&assistant_message);
    let finish_reason = response_json
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("<none>");
    format!(
        "finish_reason={} assistant_text_chars={} tool_calls={} tool_names={}",
        finish_reason,
        text.chars().count(),
        tool_calls.len(),
        summarize_tool_call_names(&tool_calls)
    )
}

fn summarize_tool_call_names(tool_calls: &[crate::format_translate::OpenAiChatToolCall]) -> String {
    if tool_calls.is_empty() {
        return "[]".to_string();
    }
    let names = tool_calls
        .iter()
        .take(12)
        .map(|tool_call| tool_call.name.as_str())
        .collect::<Vec<_>>();
    let suffix = if tool_calls.len() > names.len() {
        format!("...+{}", tool_calls.len() - names.len())
    } else {
        String::new()
    };
    format!("[{}{}]", names.join(","), suffix)
}

async fn execute_local_tool(
    workspace: &Arc<crate::tools::Workspace>,
    tool_call: &crate::format_translate::OpenAiChatToolCall,
    expert_provider: Option<&crate::expert_surgery::ExpertProvider>,
) -> String {
    let mut arguments = tool_arguments_without_reason(tool_call);

    if tool_call.name == "expert_code_surgery" {
        if let Some(expert) = expert_provider {
            if let serde_json::Value::Object(ref mut map) = arguments {
                map.insert("expert_url".to_string(), json!(expert.url));
                map.insert("expert_api_key".to_string(), json!(expert.api_key));
                map.insert("expert_model".to_string(), json!(expert.model));
            }
        }
    }
    let params = json!({
        "name": tool_call.name,
        "arguments": arguments
    });

    match crate::mcp::call_tool(&**workspace, params).await {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        Err(error) => format!("Agent local tool failed: {}", error),
    }
}

fn tool_completion_note(name: &str, output: &str) -> String {
    if is_symbol_index_tool(name) {
        return "，已使用索引查询".to_string();
    }

    if name != "search_text" {
        return String::new();
    }

    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return "，执行全文扫描".to_string();
    };
    let structured = value.get("structuredContent").unwrap_or(&value);
    let index_used = structured
        .get("index_used")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let index_matches = structured
        .get("index_matches")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let text_scan_used = structured
        .get("text_scan_used")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match (index_used, text_scan_used) {
        (true, true) => format!(
            "，已使用索引查询（命中 {} 条）并执行全文扫描",
            index_matches
        ),
        (true, false) => format!("，已使用索引查询（命中 {} 条）", index_matches),
        (false, true) => "，执行全文扫描（未使用索引）".to_string(),
        (false, false) => String::new(),
    }
}

fn tool_call_reason(tool_call: &crate::format_translate::OpenAiChatToolCall) -> Option<String> {
    let arguments = serde_json::from_str::<Value>(&tool_call.arguments).ok()?;
    let reason = arguments.get("reason").and_then(|value| value.as_str())?;
    let reason = reason.trim();
    if reason.is_empty() {
        None
    } else {
        Some(reason.to_string())
    }
}

fn tool_arguments_without_reason(tool_call: &crate::format_translate::OpenAiChatToolCall) -> Value {
    let mut arguments =
        serde_json::from_str::<Value>(&tool_call.arguments).unwrap_or_else(|_| json!({}));
    if let Some(object) = arguments.as_object_mut() {
        object.remove("reason");
    }
    arguments
}

fn looks_like_incomplete_process_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let has_forward_intent = [
        "我先",
        "我会",
        "我将",
        "接下来",
        "先把",
        "先看",
        "拉出来",
        "看一下",
        "检查一下",
        "分析一下",
        "i'll",
        "i will",
        "let me",
        "i need to",
        "next,",
    ]
    .iter()
    .any(|needle| lower.contains(&needle.to_ascii_lowercase()));
    let mentions_more_work = [
        "完整",
        "对照",
        "看看",
        "确认",
        "检查",
        "分析",
        "拉出来",
        "inspect",
        "check",
        "analyze",
        "look at",
        "pull",
    ]
    .iter()
    .any(|needle| lower.contains(&needle.to_ascii_lowercase()));
    let has_final_signal = [
        "结论",
        "原因是",
        "根因",
        "已经",
        "已完成",
        "the cause is",
        "root cause",
        "done",
        "completed",
    ]
    .iter()
    .any(|needle| lower.contains(&needle.to_ascii_lowercase()));

    if !(has_forward_intent && mentions_more_work) {
        return false;
    }
    if !has_final_signal {
        return true;
    }

    let final_signal_pos = ["结论", "原因是", "根因", "the cause is", "root cause"]
        .iter()
        .filter_map(|needle| lower.find(&needle.to_ascii_lowercase()))
        .min();
    let process_pos = [
        "我先",
        "我会",
        "我将",
        "接下来",
        "先把",
        "先看",
        "let me",
        "i will",
    ]
    .iter()
    .filter_map(|needle| lower.rfind(&needle.to_ascii_lowercase()))
    .max();

    matches!((final_signal_pos, process_pos), (Some(final_pos), Some(process_pos)) if process_pos > final_pos)
}

fn is_symbol_index_tool(name: &str) -> bool {
    matches!(
        name,
        "search_go_symbols"
            | "search_rust_symbols"
            | "search_ts_symbols"
            | "search_python_symbols"
            | "list_go_symbols"
            | "list_rust_symbols"
            | "list_ts_symbols"
            | "list_python_symbols"
            | "read_go_symbol"
            | "read_rust_symbol"
            | "read_ts_symbol"
            | "read_python_symbol"
    )
}

fn bound_tool_output(output: &str) -> String {
    let count = output.chars().count();
    if count <= MAX_TOOL_OUTPUT_CHARS {
        return output.to_string();
    }
    let head = output
        .chars()
        .take(MAX_TOOL_OUTPUT_CHARS)
        .collect::<String>();
    format!(
        "{}\n[Agent truncated tool output: original {} chars, kept first {} chars]",
        head, count, MAX_TOOL_OUTPUT_CHARS
    )
}

async fn send_text(event_bus_tx: &mpsc::Sender<EventBusMessage>, text: &str) {
    if text.is_empty() {
        return;
    }
    let _ = event_bus_tx
        .send(EventBusMessage::TextDelta(text.to_string()))
        .await;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FinalityDecision {
    Done,
    Continue,
    Unknown,
}

async fn confirm_text_finality(
    client: &Client,
    upstream_url: &str,
    api_key: &str,
    upstream_model: &str,
    user_request: &str,
    assistant_text: &str,
) -> FinalityDecision {
    let request = json!({
        "model": upstream_model,
        "stream": false,
        "temperature": 0,
        "max_tokens": FINALITY_PROBE_MAX_TOKENS,
        "messages": [
            {
                "role": "system",
                "content": "You are a strict agent runtime finality checker. Decide whether the assistant text fully completes the user's request. Reply with exactly one lowercase token: done or continue. Reply done only if the text is ready to show as the final answer and does not promise future work. Reply continue if it is a plan, progress note, tool-intent message, incomplete answer, or says it will inspect/analyze/check/do something next."
            },
            {
                "role": "user",
                "content": format!(
                    "User request:\n{}\n\nAssistant text:\n{}\n\nIs the assistant text final? Reply exactly done or continue.",
                    user_request,
                    assistant_text
                )
            }
        ]
    });

    let Ok(response) = client
        .post(upstream_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&request)
        .send()
        .await
    else {
        return FinalityDecision::Unknown;
    };
    if !response.status().is_success() {
        return FinalityDecision::Unknown;
    }
    let Ok(response_text) = response.text().await else {
        return FinalityDecision::Unknown;
    };
    let Ok(response_json) = serde_json::from_str::<Value>(&response_text) else {
        return FinalityDecision::Unknown;
    };
    let assistant_message = crate::format_translate::openai_chat_assistant_message(&response_json);
    let decision = parse_finality_probe_text(
        &crate::format_translate::collect_openai_chat_final_text(&assistant_message),
    );
    if matches!(decision, FinalityDecision::Unknown)
        && looks_like_incomplete_process_text(assistant_text)
    {
        return FinalityDecision::Continue;
    }
    decision
}

fn parse_finality_probe_text(text: &str) -> FinalityDecision {
    let normalized = text
        .trim()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
        .to_ascii_lowercase();
    match normalized.as_str() {
        "done" => FinalityDecision::Done,
        "continue" => FinalityDecision::Continue,
        _ => FinalityDecision::Unknown,
    }
}

fn continuation_nudge_for_non_final_text(text: &str, finality: &FinalityDecision) -> String {
    let reason = match finality {
        FinalityDecision::Continue => {
            "A finality check decided this was not a complete final answer."
        }
        FinalityDecision::Unknown => {
            "A finality check could not confirm this was a complete final answer."
        }
        FinalityDecision::Done => "This should not happen.",
    };
    format!(
        "{} Your previous message had no tool calls and should not stop the agent loop yet. Previous message:\n{}\n\nContinue now: call the needed tools, or provide a complete final answer if no tools are needed. Do not stop at a plan or future-tense inspection note.",
        reason, text
    )
}

async fn run_project_analysis_gate(
    body: &Value,
    event_bus_tx: &mpsc::Sender<EventBusMessage>,
) -> Option<crate::architecture_agent::ProjectAnalysisDecision> {
    let query = latest_user_text_from_responses_body(body)?;
    if query.trim().is_empty() {
        return None;
    }

    send_debug_text(event_bus_tx, "🧠 便宜模型正在判断是否需要项目分析...\n").await;
    match crate::architecture_agent::decide_project_analysis_requirement(&query).await {
        Ok(decision) => {
            send_debug_text(
                event_bus_tx,
                &format!(
                    "📌 项目分析判断：requires_project_analysis={}；confidence={:.2}；reason={}。\n",
                    decision.requires_project_analysis, decision.confidence, decision.reason
                ),
            )
            .await;
            Some(decision)
        }
        Err(error) => {
            send_debug_text(
                event_bus_tx,
                &format!(
                    "⚠️ 项目分析判断失败：{}。将按需要项目分析继续处理。\n",
                    error
                ),
            )
            .await;
            None
        }
    }
}

fn project_gate_chat_message(gate: &crate::architecture_agent::ProjectAnalysisDecision) -> Value {
    json!({
        "role": "system",
        "content": format!(
            "Cheap model project-analysis routing decision: requires_project_analysis={}, confidence={:.2}, reason={}. If project analysis is not required, answer the user directly or use only the minimal ordinary tool needed for the request.",
            gate.requires_project_analysis, gate.confidence, gate.reason
        )
    })
}

async fn send_debug_text(event_bus_tx: &mpsc::Sender<EventBusMessage>, text: &str) {
    send_text(event_bus_tx, text).await;
}

struct AgentSession {
    response_id: String,
}

impl AgentSession {
    fn new(model: String) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|v| v.as_millis())
            .unwrap_or(0);
        Self {
            response_id: format!("resp_agent_{}_{}", now, sanitize_id(&model)),
        }
    }
}

struct AgentSseWriter {
    response_id: String,
    model: String,
    item_id: String,
    extra_output_items: Vec<Value>,
    started: bool,
    text: String,
}

impl AgentSseWriter {
    fn new(response_id: String, model: String) -> Self {
        Self {
            item_id: format!("msg_{}", response_id),
            response_id,
            model,
            extra_output_items: Vec::new(),
            started: false,
            text: String::new(),
        }
    }

    fn start(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        write_sse(
            &mut out,
            "response.created",
            json!({
                "type": "response.created",
                "response": {
                    "id": self.response_id,
                    "object": "response",
                    "status": "in_progress",
                    "model": self.model
                }
            }),
        );
        self.ensure_text_started(&mut out);
        out
    }

    fn text_delta(&mut self, delta: &str) -> Vec<u8> {
        let mut out = Vec::new();
        self.ensure_text_started(&mut out);
        self.text.push_str(delta);
        write_sse(
            &mut out,
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "response_id": self.response_id,
                "item_id": self.item_id,
                "output_index": 0,
                "content_index": 0,
                "delta": delta
            }),
        );
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        self.ensure_text_started(&mut out);
        let message_item = json!({
            "id": self.item_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": self.text}]
        });
        write_sse(
            &mut out,
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "response_id": self.response_id,
                "item_id": self.item_id,
                "output_index": 0,
                "content_index": 0,
                "text": self.text
            }),
        );
        write_sse(
            &mut out,
            "response.content_part.done",
            json!({
                "type": "response.content_part.done",
                "response_id": self.response_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": self.text}
            }),
        );
        write_sse(
            &mut out,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "response_id": self.response_id,
                "output_index": 0,
                "item": message_item
            }),
        );
        let mut output = vec![message_item];
        output.extend(self.extra_output_items.clone());
        write_sse(
            &mut out,
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "id": self.response_id,
                    "object": "response",
                    "status": "completed",
                    "model": self.model,
                    "output": output,
                    "usage": {"input_tokens":0,"output_tokens":0,"total_tokens":0}
                }
            }),
        );
        out.extend_from_slice(b"data: [DONE]\n\n");
        out
    }

    fn delegated_tool_call(
        &mut self,
        tool_call: &crate::format_translate::OpenAiChatToolCall,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let output_index = self.extra_output_items.len() + 1;
        let item = json!({
            "id": format!("fc_{}", sanitize_id(&tool_call.call_id)),
            "type": "function_call",
            "status": "completed",
            "call_id": tool_call.call_id,
            "name": tool_call.name,
            "arguments": tool_call.arguments
        });
        write_sse(
            &mut out,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "response_id": self.response_id,
                "output_index": output_index,
                "item": item
            }),
        );
        write_sse(
            &mut out,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "response_id": self.response_id,
                "output_index": output_index,
                "item": item
            }),
        );
        self.extra_output_items.push(item);
        out
    }

    fn ensure_text_started(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        write_sse(
            out,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "response_id": self.response_id,
                "output_index": 0,
                "item": {"id": self.item_id, "type":"message", "status":"in_progress", "role":"assistant", "content": []}
            }),
        );
        write_sse(
            out,
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "response_id": self.response_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type":"output_text", "text":""}
            }),
        );
    }
}

fn write_sse(out: &mut Vec<u8>, event: &str, value: Value) {
    out.extend_from_slice(format!("event: {}\n", event).as_bytes());
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(serde_json::to_string(&value).unwrap_or_default().as_bytes());
    out.extend_from_slice(b"\n\n");
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_note_marks_symbol_index_tools() {
        assert_eq!(
            tool_completion_note("search_rust_symbols", "{}"),
            "，已使用索引查询"
        );
    }

    #[test]
    fn completion_note_reports_search_text_index_and_scan() {
        let output = json!({
            "structuredContent": {
                "index_used": true,
                "index_matches": 2,
                "text_scan_used": true
            }
        })
        .to_string();

        assert_eq!(
            tool_completion_note("search_text", &output),
            "，已使用索引查询（命中 2 条）并执行全文扫描"
        );
    }

    #[test]
    fn completion_note_reports_search_text_scan_only() {
        let output = json!({
            "structuredContent": {
                "index_used": false,
                "index_matches": 0,
                "text_scan_used": true
            }
        })
        .to_string();

        assert_eq!(
            tool_completion_note("search_text", &output),
            "，执行全文扫描（未使用索引）"
        );
    }

    #[test]
    fn extracts_tool_call_reason_from_arguments() {
        let tool_call = crate::format_translate::OpenAiChatToolCall {
            call_id: "call_1".to_string(),
            name: "read_file_lines".to_string(),
            arguments: json!({
                "path": "src/agent_runtime.rs",
                "reason": "我先查看 agent 主循环，确认工具调用前的展示逻辑。"
            })
            .to_string(),
        };

        assert_eq!(
            tool_call_reason(&tool_call).as_deref(),
            Some("我先查看 agent 主循环，确认工具调用前的展示逻辑。")
        );

        let tool_call = crate::format_translate::OpenAiChatToolCall {
            call_id: "call_2".to_string(),
            name: "read_file_lines".to_string(),
            arguments: json!({"reason": "   "}).to_string(),
        };
        assert!(tool_call_reason(&tool_call).is_none());
    }

    #[test]
    fn removes_display_reason_from_executable_arguments() {
        let tool_call = crate::format_translate::OpenAiChatToolCall {
            call_id: "call_1".to_string(),
            name: "read_file_lines".to_string(),
            arguments: json!({
                "path": "src/agent_runtime.rs",
                "start_line": 1,
                "end_line": 8,
                "reason": "我先查看 agent 主循环。"
            })
            .to_string(),
        };

        let arguments = tool_arguments_without_reason(&tool_call);
        assert_eq!(arguments["path"].as_str(), Some("src/agent_runtime.rs"));
        assert!(arguments.get("reason").is_none());
    }

    #[test]
    fn delegated_tool_call_keeps_display_reason_for_codex() {
        let tool_call = crate::format_translate::OpenAiChatToolCall {
            call_id: "call_1".to_string(),
            name: "external_tool".to_string(),
            arguments: json!({
                "query": "abc",
                "reason": "用于展示，不应传给真实工具。"
            })
            .to_string(),
        };
        let mut writer = AgentSseWriter::new("resp_1".to_string(), "test-model".to_string());
        let bytes = writer.delegated_tool_call(&tool_call);
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains(r#"\"query\":\"abc\""#));
        assert!(text.contains("用于展示"));
        assert!(text.contains(r#"\"reason\""#));
    }

    #[test]
    fn detects_incomplete_process_text() {
        assert!(looks_like_incomplete_process_text(
            "我先把完整的架构提示词拉出来，对照之前出错的现象说清楚原因。"
        ));
        assert!(looks_like_incomplete_process_text(
            "Let me inspect the prompt and analyze the issue."
        ));
        assert!(looks_like_incomplete_process_text(
            "好的——其实 response_format 已经在了。格式问题的根因不在 API 参数，而在提示词和解析层面。我先把完整的架构提示词及两处 prompt（gate + 架构分析）一起拉出来，对照之前出错的三类现象说清楚原因。"
        ));
        assert!(!looks_like_incomplete_process_text(
            "结论：格式问题的根因是解析层对类型漂移不够宽容。"
        ));
    }

    #[test]
    fn parses_finality_probe_tokens() {
        assert_eq!(parse_finality_probe_text("done."), FinalityDecision::Done);
        assert_eq!(
            parse_finality_probe_text(" continue\n"),
            FinalityDecision::Continue
        );
        assert_eq!(
            parse_finality_probe_text("I think it is done"),
            FinalityDecision::Unknown
        );
    }

    #[test]
    fn continuation_nudge_preserves_non_final_text() {
        let nudge =
            continuation_nudge_for_non_final_text("我先看一下日志。", &FinalityDecision::Continue);
        assert!(nudge.contains("not a complete final answer"));
        assert!(nudge.contains("我先看一下日志。"));
    }

    #[test]
    fn project_gate_message_preserves_no_analysis_decision() {
        let message =
            project_gate_chat_message(&crate::architecture_agent::ProjectAnalysisDecision {
                requires_project_analysis: false,
                reason: "Simple repository state request.".to_string(),
                confidence: 0.91,
            });

        let content = message.get("content").and_then(|v| v.as_str()).unwrap();
        assert!(content.contains("requires_project_analysis=false"));
        assert!(content.contains("Simple repository state request."));
    }

    #[test]
    fn extracts_request_cwd_from_environment_context() {
        let body = json!({
            "input": [{
                "role": "user",
                "content": "<environment_context>\n  <cwd>C:\\project\\real-app</cwd>\n</environment_context>\n当前是什么分支？"
            }]
        });

        assert_eq!(
            request_cwd_from_body(&body).as_deref(),
            Some("C:\\project\\real-app")
        );
    }

    #[test]
    fn extracts_request_cwd_from_metadata_or_older_items() {
        let body = json!({
            "metadata": {
                "cwd": "D:\\workspace\\from-metadata"
            },
            "input": [{
                "role": "user",
                "content": "继续"
            }]
        });

        assert_eq!(
            request_cwd_from_body(&body).as_deref(),
            Some("D:\\workspace\\from-metadata")
        );

        let body = json!({
            "input": [
                {
                    "role": "user",
                    "content": "<environment_context><cwd>E:\\older\\project</cwd></environment_context>"
                },
                {
                    "role": "user",
                    "content": "改一下代码"
                }
            ]
        });

        assert_eq!(
            request_cwd_from_body(&body).as_deref(),
            Some("E:\\older\\project")
        );
    }
}
