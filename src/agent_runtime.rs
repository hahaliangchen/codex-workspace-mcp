use std::sync::{Arc, Mutex};
use std::task::Context;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::stream::poll_fn;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::error;

const MAX_AGENT_STEPS: usize = 12;
const MAX_TOOL_OUTPUT_CHARS: usize = 30 * 1024;

pub async fn run_responses_agent(
    client: Client,
    workspace: Arc<crate::tools::Workspace>,
    log: Arc<Mutex<std::fs::File>>,
    db: Arc<Mutex<rusqlite::Connection>>,
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

        let result = run_agent_loop(
            client,
            workspace,
            log,
            db,
            provider_url,
            api_key,
            body,
            upstream_model,
            &mut stream,
            &tx,
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
    db: Arc<Mutex<rusqlite::Connection>>,
    provider_url: String,
    api_key: String,
    mut body: Value,
    upstream_model: String,
    stream: &mut AgentSseWriter,
    tx: &mpsc::Sender<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>,
) -> anyhow::Result<()> {
    let upstream_url = format!("{}/responses", provider_url.trim_end_matches('/'));
    body["model"] = json!(upstream_model);
    body["stream"] = json!(false);

    let prepared_tools = crate::tool_prepare::prepare_responses_tools(body.get("tools"));
    log_blocked_tools(&log, &db, &prepared_tools.blocked);
    if !prepared_tools.tools.is_empty() {
        body["tools"] = json!(prepared_tools.tools);
    }

    ensure_agent_instructions(&mut body);
    let mut input_history = body.get("input").cloned().unwrap_or_else(|| json!([]));
    if !input_history.is_array() {
        input_history = json!([{"role":"user","content": input_history}]);
    }

    send_text(tx, stream, "[agent] 已接管本轮请求，开始分析。\n").await;

    let mut steps_run = 0usize;
    let mut total_tool_calls = 0usize;
    for step in 1..=MAX_AGENT_STEPS {
        steps_run = step;
        body["input"] = input_history.clone();

        let response = client
            .post(&upstream_url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
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
        let output_items = response_json
            .get("output")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let tool_calls = collect_tool_calls(&output_items);
        if tool_calls.is_empty() {
            let text = collect_final_text(&response_json);
            log_agent_summary(&log, &db, steps_run, total_tool_calls, text.chars().count());
            if text.trim().is_empty() {
                send_text(
                    tx,
                    stream,
                    "[agent] 上游没有继续调用工具，但也没有给出文本答案。\n",
                )
                .await;
            } else {
                send_text(tx, stream, &text).await;
            }
            return Ok(());
        }

        total_tool_calls += tool_calls.len();
        append_output_items(&mut input_history, output_items);
        for tool_call in tool_calls {
            let display_name = tool_call.name.trim_start_matches("codex_workspace_mcp__");
            send_text(
                tx,
                stream,
                &format!(
                    "\n[tool] 调用 {}\n  参数: {}\n",
                    display_name,
                    compact_text(&tool_call.arguments, 240)
                ),
            )
            .await;

            let output = execute_local_tool(&workspace, &tool_call).await;
            let bounded = bound_tool_output(&output);
            send_text(
                tx,
                stream,
                &format!(
                    "[tool] {} 完成，返回 {} 字符。继续分析...\n",
                    display_name,
                    bounded.chars().count()
                ),
            )
            .await;

            push_input_item(
                &mut input_history,
                json!({
                    "type": "function_call_output",
                    "call_id": tool_call.call_id,
                    "output": bounded
                }),
            );
        }
    }

    send_text(
        tx,
        stream,
        "\n[agent] 达到最大工具循环次数，已停止。请缩小问题或补充下一步指令。\n",
    )
    .await;
    log_agent_summary(&log, &db, steps_run, total_tool_calls, 0);
    Ok(())
}

fn ensure_agent_instructions(body: &mut Value) {
    let prefix = "You are the upstream reasoning model inside a local Agent Runtime. Use provided codex_workspace_mcp__ tools whenever current workspace files, logs, code, configuration, or repository state are needed. Do not ask the outer Codex client to execute these local tools. When you need information, call tools; when enough information is available, answer normally.";
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
    db: &Mutex<rusqlite::Connection>,
    blocked_tools: &[crate::tool_prepare::BlockedTool],
) {
    for blocked in blocked_tools {
        let label = match blocked.kind {
            crate::tool_prepare::BlockedToolKind::Type => "type",
            crate::tool_prepare::BlockedToolKind::Name => "name",
        };
        crate::proxy_log::write(
            log,
            Some(db),
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
    db: &Mutex<rusqlite::Connection>,
    steps: usize,
    tool_calls: usize,
    final_chars: usize,
) {
    crate::proxy_log::write(
        log,
        Some(db),
        Some("AGENT_DONE"),
        Some("proxy"),
        &format!(
            "agent runtime completed steps={} tool_calls={} final_chars={}",
            steps, tool_calls, final_chars
        ),
    );
}

#[derive(Clone, Debug)]
struct AgentToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

fn collect_tool_calls(output_items: &[Value]) -> Vec<AgentToolCall> {
    output_items
        .iter()
        .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("function_call"))
        .filter_map(|item| {
            let name = item.get("name").and_then(|v| v.as_str())?.to_string();
            if !name.starts_with("codex_workspace_mcp__") {
                return None;
            }
            Some(AgentToolCall {
                call_id: item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name,
                arguments: item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}")
                    .to_string(),
            })
        })
        .collect()
}

async fn execute_local_tool(
    workspace: &Arc<crate::tools::Workspace>,
    tool_call: &AgentToolCall,
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

fn append_output_items(input_history: &mut Value, output_items: Vec<Value>) {
    for item in output_items {
        push_input_item(input_history, item);
    }
}

fn push_input_item(input_history: &mut Value, item: Value) {
    if let Some(arr) = input_history.as_array_mut() {
        arr.push(item);
    }
}

fn collect_final_text(response: &Value) -> String {
    if let Some(text) = response.get("output_text").and_then(|v| v.as_str()) {
        return text.to_string();
    }

    let mut parts = Vec::new();
    if let Some(output) = response.get("output").and_then(|v| v.as_array()) {
        for item in output {
            if item.get("type").and_then(|v| v.as_str()) != Some("message") {
                continue;
            }
            if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                for part in content {
                    if let Some(text) = part
                        .get("text")
                        .or_else(|| part.get("content"))
                        .and_then(|v| v.as_str())
                    {
                        parts.push(text.to_string());
                    }
                }
            }
        }
    }
    parts.join("\n")
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

fn compact_text(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    format!("{}...", text.chars().take(max_chars).collect::<String>())
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
    started: bool,
    text: String,
}

impl AgentSseWriter {
    fn new(response_id: String, model: String) -> Self {
        Self {
            item_id: format!("msg_{}", response_id),
            response_id,
            model,
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
                "item": {
                    "id": self.item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": self.text}]
                }
            }),
        );
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
                    "output": [{
                        "id": self.item_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": self.text}]
                    }],
                    "usage": {"input_tokens":0,"output_tokens":0,"total_tokens":0}
                }
            }),
        );
        out.extend_from_slice(b"data: [DONE]\n\n");
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
    fn collects_function_calls_for_mcp_tools_only() {
        let calls = collect_tool_calls(&[
            json!({"type":"function_call","call_id":"a","name":"codex_workspace_mcp__read_file","arguments":"{}"}),
            json!({"type":"function_call","call_id":"b","name":"external","arguments":"{}"}),
        ]);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "a");
    }

    #[test]
    fn final_text_reads_output_text_or_message_content() {
        assert_eq!(collect_final_text(&json!({"output_text":"done"})), "done");
        assert_eq!(
            collect_final_text(&json!({
                "output": [{"type":"message","content":[{"type":"output_text","text":"hello"}]}]
            })),
            "hello"
        );
    }
}
