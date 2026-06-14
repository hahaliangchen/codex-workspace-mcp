use std::sync::{Arc, Mutex};
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
const MAX_TOOL_OUTPUT_CHARS: usize = 300 * 1024;

pub async fn run_responses_agent(
    client: Client,
    workspace: Arc<crate::tools::Workspace>,
    log: Arc<Mutex<std::fs::File>>,
    provider_url: String,
    api_key: String,
    body: Value,
    upstream_model: String,
    client_model: String,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>(32);
    let session = AgentSession::new(client_model.clone());

    tokio::spawn(async move {
        let mut stream = AgentSseWriter::new(session.response_id.clone(), client_model);
        let _ = tx.send(Ok(Bytes::from(stream.start()))).await;

        let visible_images = crate::vision_preprocess::visible_images_from_body(&body);
        let result = crate::vision_preprocess::scope_visible_images(
            visible_images,
            run_agent_loop(
                client,
                workspace,
                log,
                provider_url,
                api_key,
                body,
                upstream_model,
                &mut stream,
                &tx,
            ),
        )
        .await;

        if let Err(error) = result {
            let text = format!("\n[agent:error] {}\n", error);
            let _ = tx.send(Ok(Bytes::from(stream.text_delta(&text)))).await;
        }

        let _ = tx.send(Ok(Bytes::from(stream.finish()))).await;
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
    log: Arc<Mutex<std::fs::File>>,
    provider_url: String,
    api_key: String,
    mut body: Value,
    upstream_model: String,
    stream: &mut AgentSseWriter,
    tx: &mpsc::Sender<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>,
) -> anyhow::Result<()> {
    let upstream_url = format!("{}/chat/completions", provider_url.trim_end_matches('/'));
    body["model"] = json!(upstream_model);
    body["stream"] = json!(false);

    let prepared_tools = crate::tool_prepare::prepare_responses_tools(body.get("tools"));
    log_blocked_tools(&log, &prepared_tools.blocked);
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

    send_text(tx, stream, "🤖 [agent] 已接管本轮请求，开始分析。\n").await;

    let mut total_tool_calls = 0usize;
    let mut consecutive_failures = 0usize;
    let mut steps_run = 0usize;
    for step in 1..=MAX_AGENT_STEPS {
        steps_run = step;
        let request_body = crate::format_translate::build_openai_chat_request(
            &body,
            &upstream_model,
            &chat_messages,
            &chat_tools,
        );

        let response = client
            .post(&upstream_url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&request_body)
            .send()
            .await?;

        let status = response.status();
        let response_text = response.text().await.unwrap_or_default();
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
            log_agent_summary(&log, steps_run, total_tool_calls, text.chars().count());
            if text.trim().is_empty() {
                send_text(
                    tx,
                    stream,
                    "\n⚠️ [agent] 上游没有继续调用工具，但也没有给出文本答案。\n",
                )
                .await;
            } else {
                send_text(tx, stream, &text).await;
            }
            return Ok(());
        }

        total_tool_calls += tool_calls.len();
        let mut step_failed = false;
        chat_messages.push(assistant_message);

        // Pre-flight: check for delegated/unknown tools before spawning any work.
        for tool_call in &tool_calls {
            if delegated_tool_names.contains(&tool_call.name) {
                send_text(tx, stream, &format!("\n↪️ {}\n", &tool_call.name)).await;
                let _ = tx
                    .send(Ok(Bytes::from(stream.delegated_tool_call(tool_call))))
                    .await;
                return Ok(());
            }
            if !local_tool_names.contains(&tool_call.name) {
                send_text(
                    tx,
                    stream,
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
            send_text(tx, stream, &format!("\n🔧 {}\n", &tool_call.name)).await;
        }

        // Execute all local tools concurrently.
        let workspace_ref = &workspace;
        let tool_futures: Vec<_> = tool_calls
            .into_iter()
            .map(|tc| {
                let workspace = Arc::clone(workspace_ref);
                async move {
                    let output = execute_local_tool(&workspace, &tc).await;
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
            send_text(
                tx,
                stream,
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
                send_text(tx, stream, "\n⏹️ 连续3次工具调用失败，已停止。\n").await;
                log_agent_summary(&log, steps_run, total_tool_calls, 0);
                return Ok(());
            }
        } else {
            consecutive_failures = 0;
        }
    }

    send_text(tx, stream, "\n⏹️ 达到最大工具循环次数（30次），已停止。\n").await;
    log_agent_summary(&log, steps_run, total_tool_calls, 0);
    Ok(())
}

fn ensure_agent_instructions(body: &mut Value) {
    let prefix = "You are the upstream reasoning model inside a local Agent Runtime. Use the provided local workspace tools directly whenever current workspace files, logs, code, configuration, or repository state are needed. Do not ask the outer Codex client to execute tools. When you need information, call tools; when enough information is available, answer normally. Prefer parallel tool calls: batch independent reads, searches, and lookups into a single step whenever they do not depend on each other's results. Before code changes or architecture questions, search_symbol_business_context and search_architecture_memory for the relevant business wording and feature/logic area. If no useful semantic memory exists, gather minimal verified evidence with indexed symbol tools, then call analyze_architecture_memory to let the configured cheap architecture model map the business logic and symbol roles. Pass only verified architecture memory, symbol business contexts, symbol index results, read_*_symbol output, or short code excerpts as evidence; do not send whole-project source. Set record=true only when the evidence is grounded enough to create/update durable architecture memory and symbol business contexts. For large changes that alter responsibilities, key symbols, boundaries, common tasks, risks, or symbol roles, update semantic memory before finishing. For code navigation, prefer indexed symbol tools first: search_go_symbols/search_rust_symbols/search_ts_symbols/search_python_symbols, list_*_symbols, then read_*_symbol with include_context when dependencies, callers, callees, or imports are useful. Use search_text mainly for literals, UI strings, log lines, config keys, error messages, or as a fallback when indexed symbol tools do not locate the code. Before editing, assess the relevant architecture area, smallest plausible change, files/symbols to touch, boundaries to avoid, regression risks, and coupling level. Distinguish business-critical information from incidental protocol/display/history noise: if unmatched tool calls, stale history, missing optional metadata, or display-only artifacts are not needed for the model to complete the user's task, prefer dropping, ignoring, normalizing, or isolating them instead of expanding the design to preserve them. If the minimal fix crosses unrelated architecture areas or requires broad shared-infrastructure changes, pause and explain the coupling, risks, and smaller alternatives to the user before making a large edit.";
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

fn log_blocked_tools(
    log: &Mutex<std::fs::File>,
    blocked_tools: &[crate::tool_prepare::BlockedTool],
) {
    for blocked in blocked_tools {
        let label = match blocked.kind {
            crate::tool_prepare::BlockedToolKind::Type => "type",
            crate::tool_prepare::BlockedToolKind::Name => "name",
        };
        crate::proxy_log::write(
            log,
            true,
            Some("TOOL_BLOCKED"),
            Some("proxy"),
            &format!(
                "agent runtime blocked unsafe tool by {}: '{}'",
                label, blocked.value
            ),
        );
    }
}

fn log_agent_summary(
    log: &Mutex<std::fs::File>,
    steps: usize,
    tool_calls: usize,
    final_chars: usize,
) {
    crate::proxy_log::write(
        log,
        true,
        Some("AGENT_DONE"),
        Some("proxy"),
        &format!(
            "agent runtime completed steps={} tool_calls={} final_chars={}",
            steps, tool_calls, final_chars
        ),
    );
}

async fn execute_local_tool(
    workspace: &Arc<crate::tools::Workspace>,
    tool_call: &crate::format_translate::OpenAiChatToolCall,
) -> String {
    let arguments =
        serde_json::from_str::<Value>(&tool_call.arguments).unwrap_or_else(|_| json!({}));
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

async fn send_text(
    tx: &mpsc::Sender<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>,
    stream: &mut AgentSseWriter,
    text: &str,
) {
    if text.is_empty() {
        return;
    }
    let _ = tx.send(Ok(Bytes::from(stream.text_delta(text)))).await;
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
}
