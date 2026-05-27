use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::task::Context;
use std::time::SystemTime;

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::FixedOffset;
use futures::stream::poll_fn;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower_http::cors::CorsLayer;
use tracing::{error, info};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProviderConfig {
    url: String,
    api_key: String,
    #[serde(default)]
    model_map: HashMap<String, String>,
    /// "openai" (default) or "anthropic".  Anthropic-type providers receive
    /// raw pass-through — no request/response format conversion.
    #[serde(default = "default_api_type")]
    api_type: String,
}

fn default_api_type() -> String {
    "openai".to_owned()
}

#[derive(Debug, Deserialize)]
struct AiProxyConfig {
    #[serde(default)]
    default_provider: Option<String>,
    providers: HashMap<String, ProviderConfig>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AiProxyState {
    config: Arc<AiProxyConfig>,
    client: Client,
    log: Arc<Mutex<std::fs::File>>,
    #[allow(dead_code)]
    log_path: Arc<PathBuf>,
}

// ---------------------------------------------------------------------------
// Logging helpers
// ---------------------------------------------------------------------------

/// Current time in China timezone (UTC+8).
fn now_china() -> String {
    let offset = FixedOffset::east_opt(8 * 60 * 60).unwrap();
    chrono::Utc::now()
        .with_timezone(&offset)
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

/// Write a line to the shared log file.  Panics are caught — logging is
/// best-effort and must never crash the proxy.
fn log_write(log: &Mutex<std::fs::File>, msg: &str) {
    let ts = now_china();
    let line = format!("[{}] {}\n", ts, msg);
    if let Ok(mut f) = log.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

macro_rules! log {
    ($log:expr, $($arg:tt)*) => {
        log_write(&*$log, &format!($($arg)*))
    };
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolve a client-visible model name → (provider config, upstream model name).
/// Looks up the model in the default provider's model_map; if not found,
/// passes the model name through to the default provider as-is.
fn resolve_model<'a>(
    config: &'a AiProxyConfig,
    model: &str,
) -> Result<(&'a ProviderConfig, String), Response> {
    let default = config.default_provider.as_deref().unwrap_or("");
    let provider = config.providers.get(default).ok_or_else(|| {
        error!(provider = %default, "default provider not found");
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("default provider not found: {}", default)})),
        )
            .into_response()
    })?;

    let upstream = provider.model_map.get(model).cloned().unwrap_or_else(|| model.to_owned());
    Ok((provider, upstream))
}

/// Convert an Anthropic Messages request body to an OpenAI chat/completions body.
fn anthropic_to_openai(body: &Value) -> Value {
    let mut openai = json!({});

    // model — kept as-is (will be mapped later)
    if let Some(m) = body.get("model") {
        openai["model"] = m.clone();
    }

    // messages — convert system from top-level to system message
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = body.get("system") {
        if let Some(text) = sys.as_str() {
            messages.push(json!({"role": "system", "content": text}));
        } else if let Some(arr) = sys.as_array() {
            // Anthropic allows system as array of text blocks
            let combined: String = arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if !combined.is_empty() {
                messages.push(json!({"role": "system", "content": combined}));
            }
        }
    }
    if let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content");
            messages.push(json!({"role": role, "content": content}));
        }
    }
    openai["messages"] = json!(messages);

    // max_tokens
    if let Some(v) = body.get("max_tokens") {
        openai["max_tokens"] = v.clone();
    }

    // temperature
    if let Some(v) = body.get("temperature") {
        openai["temperature"] = v.clone();
    }

    // stop_sequences → stop
    if let Some(v) = body.get("stop_sequences") {
        openai["stop"] = v.clone();
    }

    // stream
    if let Some(v) = body.get("stream") {
        openai["stream"] = v.clone();
    }

    // top_p
    if let Some(v) = body.get("top_p") {
        openai["top_p"] = v.clone();
    }

    // top_k (OpenAI doesn't have this, but some providers support it)
    if let Some(v) = body.get("top_k") {
        openai["top_k"] = v.clone();
    }

    // tools
    if let Some(v) = body.get("tools") {
        openai["tools"] = v.clone();
    }

    // tool_choice
    if let Some(v) = body.get("tool_choice") {
        openai["tool_choice"] = v.clone();
    }

    openai
}

/// Convert an OpenAI chat completion response to an Anthropic Messages response.
fn openai_to_anthropic(openai_body: &Value, model: &str) -> Value {
    let choice = openai_body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first());

    let content_text = choice
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        .unwrap_or("stop");

    let usage = openai_body.get("usage");
    let zero = json!(0);
    let input_tokens = usage.and_then(|u| u.get("prompt_tokens")).unwrap_or(&zero);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .unwrap_or(&zero);

    let msg_id = openai_body
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_owned();

    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": content_text}],
        "model": model,
        "stop_reason": match finish_reason {
            "stop" => "end_turn",
            "length" => "max_tokens",
            "tool_calls" => "tool_use",
            _ => "end_turn",
        },
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    })
}

/// Generate a unique message ID.
fn gen_msg_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("msg_{:x}", nanos)
}

/// Truncate body for logging (keep first ~2000 chars).
fn fmt_body(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    if s.len() > 2000 {
        format!("{}… ({} bytes)", &s[..2000], s.len())
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// SSE stream converter: OpenAI SSE → Anthropic SSE
// ---------------------------------------------------------------------------

/// Holds accumulated state while converting an OpenAI SSE byte stream into
/// Anthropic SSE byte chunks.
struct SseConverter {
    /// Accumulator for incomplete lines across byte-chunk boundaries.
    line_buf: Vec<u8>,
    model: String,
    msg_id: String,
    seen_content: bool,
}

impl SseConverter {
    fn new(model: String) -> Self {
        Self {
            line_buf: Vec::new(),
            model,
            msg_id: gen_msg_id(),
            seen_content: false,
        }
    }

    /// Feed raw bytes from upstream; returns Vec of output bytes ready to
    /// send to the client.  Call repeatedly with each chunk, then call
    /// `flush()` at the end.
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        self.line_buf.extend_from_slice(chunk);

        // Process complete lines
        loop {
            if let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
                let line_bytes = self.line_buf.drain(..=pos).collect::<Vec<_>>();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end_matches('\n').trim_end_matches('\r');

                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        // End of stream — emit stop events
                        if self.seen_content {
                            self.emit_stop_events(&mut out);
                        }
                    } else if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                        self.process_openai_chunk(&chunk, &mut out);
                    }
                }
                // Non-data lines, empty lines, comments — skip
            } else {
                break;
            }
        }
        out
    }

    /// Call after the upstream stream ends.
    fn flush(&mut self) -> Vec<u8> {
        // Process any remaining incomplete data in line_buf
        let mut out = Vec::new();
        if !self.line_buf.is_empty() {
            let line = String::from_utf8_lossy(&self.line_buf);
            let line = line.trim_end();
            if let Some(data) = line.strip_prefix("data: ") {
                if data != "[DONE]" {
                    if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                        self.process_openai_chunk(&chunk, &mut out);
                    }
                }
            }
            self.line_buf.clear();
        }
        // Always emit stop events at stream end if we started content
        if self.seen_content {
            self.emit_stop_events(&mut out);
        }
        out
    }

    fn process_openai_chunk(&mut self, chunk: &Value, out: &mut Vec<u8>) {
        let choice = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());

        let delta_content = choice
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str());

        if let Some(text) = delta_content {
            if !self.seen_content {
                // First content — emit message_start + content_block_start
                self.seen_content = true;
                self.emit_start_events(out);
            }
            // Emit content delta
            let delta = json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            });
            write_sse_event(out, "content_block_delta", &delta);
        }

        // Check if this is the final chunk (has finish_reason)
        let finish = choice.and_then(|c| c.get("finish_reason")).and_then(|f| f.as_str());
        if finish.is_some() && finish != Some("null") {
            if self.seen_content {
                self.emit_stop_events(out);
            }
        }
    }

    fn emit_start_events(&self, out: &mut Vec<u8>) {
        let start = json!({
            "type": "message_start",
            "message": {
                "id": self.msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0}
            }
        });
        write_sse_event(out, "message_start", &start);

        let block_start = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        });
        write_sse_event(out, "content_block_start", &block_start);
    }

    fn emit_stop_events(&self, out: &mut Vec<u8>) {
        let block_stop = json!({
            "type": "content_block_stop",
            "index": 0
        });
        write_sse_event(out, "content_block_stop", &block_stop);

        let msg_delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 0}
        });
        write_sse_event(out, "message_delta", &msg_delta);

        let msg_stop = json!({"type": "message_stop"});
        write_sse_event(out, "message_stop", &msg_stop);
    }
}

fn write_sse_event(out: &mut Vec<u8>, event: &str, data: &Value) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(serde_json::to_string(data).unwrap_or_default().as_bytes());
    out.extend_from_slice(b"\n\n");
}

// ---------------------------------------------------------------------------
// Forward helper — sends the OpenAI-format body to the provider, returns
// either a streaming or non-streaming response.
// ---------------------------------------------------------------------------

async fn forward_to_upstream(
    client: &Client,
    upstream_url: &str,
    api_key: &str,
    body: &Value,
    is_stream: bool,
    client_model: &str,
    log: Arc<Mutex<std::fs::File>>,
) -> Response {

    // Log the outgoing request
    log!(
        log,
        ">> UPSTREAM REQ  model={}  url={}  stream={}  body={}",
        client_model,
        upstream_url,
        is_stream,
        fmt_body(serde_json::to_string(body).unwrap_or_default().as_bytes())
    );

    match client
        .post(upstream_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            log!(
                log,
                "<< UPSTREAM RESP  status={}  model={}",
                status.as_u16(),
                client_model
            );

            if is_stream {
                // ---- streaming path ----
                let log2 = log.clone(); // Arc<Mutex<...>> clone for the spawned task
                let model = client_model.to_owned();
                let converter = std::sync::Mutex::new(SseConverter::new(model));
                let stream = resp.bytes_stream();

                // Use a channel to bridge spawned task → Stream for Body
                let (tx, rx) = mpsc::channel::<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>(32);

                tokio::spawn(async move {
                    use futures::StreamExt;
                    futures::pin_mut!(stream);

                    while let Some(result) = stream.next().await {
                        match result {
                            Ok(bytes) => {
                                let converted = converter.lock().unwrap().feed(&bytes);
                                if !converted.is_empty() {
                                    let item: Result<Bytes, Box<dyn std::error::Error + Send + Sync>> =
                                        Ok(Bytes::from(converted));
                                    if tx.send(item).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                log!(&log2, "!! STREAM ERROR  {}", e);
                                let _ = tx
                                    .send(Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>))
                                    .await;
                                break;
                            }
                        }
                    }

                    // Flush remaining
                    let remaining = converter.lock().unwrap().flush();
                    if !remaining.is_empty() {
                        let item: Result<Bytes, Box<dyn std::error::Error + Send + Sync>> =
                            Ok(Bytes::from(remaining));
                        let _ = tx.send(item).await;
                    }
                });

                let mut rx = rx;
                let rx_stream = poll_fn(move |cx: &mut Context<'_>| {
                    rx.poll_recv(cx)
                });

                Response::builder()
                    .status(status)
                    .header("content-type", "text/event-stream")
                    .header("cache-control", "no-cache")
                    .header("x-accel-buffering", "no")
                    .body(Body::from_stream(rx_stream))
                    .unwrap_or_else(|e| {
                        error!(%e, "failed to build streaming response");
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    })
            } else {
                // ---- non-streaming path ----
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json")
                    .to_owned();

                match resp.bytes().await {
                    Ok(body_bytes) => {
                        log!(
                            log,
                            "<< UPSTREAM BODY  {} bytes  {}",
                            body_bytes.len(),
                            fmt_body(&body_bytes)
                        );
                        Response::builder()
                            .status(status)
                            .header("content-type", content_type)
                            .body(Body::from(body_bytes))
                            .unwrap_or_else(|e| {
                                error!(%e, "failed to build response");
                                StatusCode::INTERNAL_SERVER_ERROR.into_response()
                            })
                    }
                    Err(e) => {
                        log!(log, "!! READ ERROR  {}", e);
                        error!(%e, "failed to read upstream body");
                        (
                            StatusCode::BAD_GATEWAY,
                            [("content-type", "application/json")],
                            Json(json!({"error": format!("upstream read: {e}")})),
                        )
                            .into_response()
                    }
                }
            }
        }
        Err(e) => {
            log!(log, "!! CONNECT ERROR  {}", e);
            error!(%e, "upstream request failed");
            (
                StatusCode::BAD_GATEWAY,
                [("content-type", "application/json")],
                Json(json!({"error": format!("upstream: {e}")})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /v1/models — list exposed model IDs from the default provider's model_map.
async fn list_models(State(state): State<AiProxyState>) -> impl IntoResponse {
    let default_p = state
        .config
        .default_provider
        .as_deref()
        .and_then(|d| state.config.providers.get(d));

    let models: Vec<Value> = default_p
        .map(|p| {
            p.model_map
                .keys()
                .map(|id| {
                    json!({
                        "id": id,
                        "object": "model",
                        "created": 0,
                        "owned_by": "ai-proxy"
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Json(json!({
        "object": "list",
        "data": models
    }))
}

/// POST /v1/chat/completions — OpenAI-compatible endpoint.
async fn chat_completions(
    State(state): State<AiProxyState>,
    Json(mut body): Json<Value>,
) -> Response {
    let client_model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_owned(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing model field"})),
            )
                .into_response();
        }
    };

    log!(&state.log, "=== /v1/chat/completions  model={}", client_model);

    let (provider, upstream_model) = match resolve_model(&state.config, &client_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    log!(
        &state.log,
        "   resolved: provider={} upstream_model={}",
        provider.url,
        upstream_model
    );

    body["model"] = json!(upstream_model);

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let upstream_url = format!("{}/chat/completions", provider.url);
    forward_to_upstream(
        &state.client,
        &upstream_url,
        &provider.api_key,
        &body,
        is_stream,
        &client_model,
        state.log.clone(),
    )
    .await
}

/// POST /v1/messages — Anthropic Messages API endpoint.
async fn messages(
    State(state): State<AiProxyState>,
    Json(body): Json<Value>,
) -> Response {
    let raw_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    log!(&state.log, "=== /v1/messages  model={}", raw_model);
    log!(
        &state.log,
        "   anthropic body: {}",
        fmt_body(serde_json::to_string(&body).unwrap_or_default().as_bytes())
    );

    let (provider, upstream_model) = match resolve_model(&state.config, raw_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    log!(
        &state.log,
        "   resolved: provider={} upstream_model={}",
        provider.url,
        upstream_model
    );

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let is_anthropic_provider = provider.api_type == "anthropic";

    let (forward_body, upstream_url) = if is_anthropic_provider {
        // Anthropic-native upstream: forward raw, no conversion.
        let mut raw_body = body.clone();
        raw_body["model"] = json!(upstream_model);
        log!(
            &state.log,
            "   anthropic native forward, url={}",
            provider.url
        );
        (raw_body, provider.url.clone())
    } else {
        // OpenAI-compatible upstream: convert Anthropic → OpenAI.
        let mut openai_body = anthropic_to_openai(&body);
        openai_body["model"] = json!(upstream_model);
        log!(
            &state.log,
            "   openai body: {}",
            fmt_body(serde_json::to_string(&openai_body).unwrap_or_default().as_bytes())
        );
        let url = format!("{}/chat/completions", provider.url);
        (openai_body, url)
    };

    let resp = forward_to_upstream(
        &state.client,
        &upstream_url,
        &provider.api_key,
        &forward_body,
        is_stream,
        raw_model,
        state.log.clone(),
    )
    .await;

    // For non-streaming OpenAI providers, convert response back to Anthropic.
    if !is_stream && !is_anthropic_provider && resp.status().is_success() {
        let status = resp.status();
        let body_bytes = match axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => {
                log!(&state.log, "!! CONVERT ERROR  {}", e);
                error!(%e, "failed to read response body for Anthropic conversion");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("response read: {e}")})),
                )
                    .into_response();
            }
        };

        log!(
            &state.log,
            "   openai resp body: {}",
            fmt_body(&body_bytes)
        );

        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(openai_resp) => {
                let anthropic_resp = openai_to_anthropic(&openai_resp, raw_model);
                log!(
                    &state.log,
                    "   anthropic resp: {}",
                    fmt_body(serde_json::to_string(&anthropic_resp).unwrap_or_default().as_bytes())
                );
                (
                    status,
                    [("content-type", "application/json")],
                    Json(anthropic_resp),
                )
                    .into_response()
            }
            Err(e) => {
                log!(&state.log, "!! PARSE ERROR  {}", e);
                error!(%e, "failed to parse upstream OpenAI response");
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("parse upstream: {e}")})),
                )
                    .into_response()
            }
        }
    } else {
        resp
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub async fn run(listener: TcpListener, config_path: &Path) -> anyhow::Result<()> {
    let config: AiProxyConfig =
        serde_json::from_str(&tokio::fs::read_to_string(config_path).await?)?;

    // Open log file next to config
    let log_path = config_path.with_file_name("ai_proxy.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let log = Arc::new(Mutex::new(log_file));
    let log_path = Arc::new(log_path);

    let total_maps: usize = config.providers.values().map(|p| p.model_map.len()).sum();
    log!(&log, "========== AI Proxy started ==========");
    log!(
        &log,
        "config: {} providers, {} total model mappings, default={:?}",
        config.providers.len(),
        total_maps,
        config.default_provider
    );

    let config = Arc::new(config);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let state = AiProxyState {
        config,
        client,
        log,
        log_path,
    };

    let app = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .with_state(state)
        .layer(CorsLayer::permissive());

    let addr = listener.local_addr()?;
    info!(%addr, "AI proxy starting");

    axum::serve(listener, app).await?;
    Ok(())
}
