use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::task::Context;

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

use crate::format_translate;

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
    #[serde(default)]
    raw_codex: bool,
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
    /// 系统提示词专用日志，每次请求完整记录 developer/system 内容
    sys_log: Arc<Mutex<std::fs::File>>,
    workspace: Arc<crate::tools::Workspace>,
    db: Arc<Mutex<rusqlite::Connection>>,
}


// ---------------------------------------------------------------------------
// Logging helpers
// ---------------------------------------------------------------------------

/// Current time in China timezone (UTC+8).
pub fn now_china() -> String {
    let offset = FixedOffset::east_opt(8 * 60 * 60).unwrap();
    chrono::Utc::now()
        .with_timezone(&offset)
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

/// Write a line to the shared log file and optionally SQLite database. Panics are caught.
pub fn log_write(
    log: &Mutex<std::fs::File>,
    db: Option<&Mutex<rusqlite::Connection>>,
    action: Option<&str>,
    role: Option<&str>,
    msg: &str,
) {
    let ts = now_china();
    let line = format!("[{}] {}\n", ts, msg);
    if let Ok(mut f) = log.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }

    if let Some(db_lock) = db {
        let action_str = action.unwrap_or("INFO");
        let role_str = role.unwrap_or("proxy");

        // Filter out system messages and huge system-level templates to keep DB lightweight
        if role_str == "system" 
            || msg.contains("You are Codex") 
            || msg.contains("<permissions instructions>")
            || msg.contains("<skills_instructions>")
        {
            return;
        }

        if let Ok(conn) = db_lock.lock() {
            let (short_msg, detail_opt) = if msg.len() > 300 {
                let truncated = msg.chars().take(300).collect::<String>();
                (truncated + " ... [TRUNCATED]", Some(msg))
            } else {
                (msg.to_owned(), None)
            };

            let _ = crate::database::insert_detailed_api_log(
                &conn,
                &ts,
                action_str,
                role_str,
                &short_msg,
                detail_opt,
            );
        }
    }
}

macro_rules! log {
    ($log:expr, $($arg:tt)*) => {
        log_write(&*$log, None, None, None, &format!($($arg)*))
    };
}

macro_rules! log_db {
    ($state:expr, $action:expr, $role:expr, $($arg:tt)*) => {
        log_write(&*$state.log, Some(&*$state.db), Some($action), Some($role), &format!($($arg)*))
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
                let converter = std::sync::Mutex::new(format_translate::StreamConverter::new(model));
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
        let mut openai_body = format_translate::anthropic_to_openai(&body);
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
                let anthropic_resp = format_translate::openai_to_anthropic(&openai_resp, raw_model);
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

fn normalize_message_content(content: &Value) -> Value {
    if let Some(arr) = content.as_array() {
        let mut normalized_arr = Vec::new();
        for item in arr {
            if let Some(obj) = item.as_object() {
                let item_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                
                // 判断是否是图片类型
                if item_type == "input_image" || item_type == "image_url" || obj.contains_key("image_url") {
                    // 如果已经是标准格式： { "type": "image_url", "image_url": { "url": "..." } }
                    if item_type == "image_url" && obj.get("image_url").map_or(false, |v| v.is_object()) {
                        normalized_arr.push(item.clone());
                    } else {
                        // 提取 url，优先从 image_url 字段提取（可能是字符串），或者从 url 字段
                        let url_str = obj.get("image_url")
                            .and_then(|v| v.as_str())
                            .or_else(|| obj.get("url").and_then(|v| v.as_str()))
                            .unwrap_or("");
                        
                        let detail = obj.get("detail").and_then(|v| v.as_str()).unwrap_or("auto");
                        
                        normalized_arr.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": url_str,
                                "detail": detail
                            }
                        }));
                    }
                } else if item_type == "image" {
                    // 支持 Anthropic 格式的 image 片段转换
                    if let Some(source) = obj.get("source") {
                        if let (Some(media_type), Some(data)) = (
                            source.get("media_type").and_then(|v| v.as_str()),
                            source.get("data").and_then(|v| v.as_str())
                        ) {
                            normalized_arr.push(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", media_type, data)
                                }
                            }));
                        } else {
                            normalized_arr.push(item.clone());
                        }
                    } else {
                        normalized_arr.push(item.clone());
                    }
                } else {
                    // text 或其他普通类型，保持原样
                    normalized_arr.push(item.clone());
                }
            } else {
                normalized_arr.push(item.clone());
            }
        }
        Value::Array(normalized_arr)
    } else {
        content.clone()
    }
}

/// POST /v1/responses — OpenAI Responses API endpoint for Codex.
async fn responses(
    State(state): State<AiProxyState>,
    Json(body): Json<Value>,
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

    log_db!(&state, "REQ_IN", "user", "=== /v1/responses received from Codex  model={}", client_model);
    // Codex Responses body 在主日志只打顶层字段，省略 input 内容（input 中系统提示词见 system_prompt.log）
    {
        let mut body_brief = body.clone();
        if let Some(obj) = body_brief.as_object_mut() {
            if let Some(input_arr) = obj.get_mut("input") {
                if let Some(arr) = input_arr.as_array() {
                    let count = arr.len();
                    // 先把系统提示词写入 sys_log，再在主日志用占位
                    let ts = now_china();
                    let sep = format!("\n{} ===== /v1/responses model={} =====\n", ts, client_model);
                    if let Ok(mut sf) = state.sys_log.lock() {
                        let _ = sf.write_all(sep.as_bytes());
                        for (idx, item) in arr.iter().enumerate() {
                            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("-");
                            let item_str = serde_json::to_string_pretty(item).unwrap_or_default();
                            let line = format!("--- input[{}] role={} ---\n{}\n", idx, role, item_str);
                            let _ = sf.write_all(line.as_bytes());
                        }
                        let _ = sf.flush();
                    }
                    // 主日志只保留条目数量占位
                    *input_arr = Value::String(format!("[{} items → see system_prompt.log]", count));
                }
            }
        }
        log_db!(
            &state,
            "REQ_IN",
            "user",
            "   Codex Responses body: {}",
            fmt_body(serde_json::to_string(&body_brief).unwrap_or_default().as_bytes())
        );
    }

    let (provider, upstream_model) = match resolve_model(&state.config, &client_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    log_db!(
        &state,
        "PROXY",
        "proxy",
        "   resolved: provider={} upstream_model={}",
        provider.url,
        upstream_model
    );

    if provider.raw_codex {
        let mut forward_body = body.clone();
        forward_body["model"] = json!(upstream_model);

        let upstream_url = format!("{}/responses", provider.url);
        let is_stream = body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        log_db!(
            &state,
            "REQ_OUT",
            "user",
            ">> RAW CODEX UPSTREAM REQ  model={}  url={}  stream={}  body={}",
            client_model,
            upstream_url,
            is_stream,
            fmt_body(serde_json::to_string(&forward_body).unwrap_or_default().as_bytes())
        );

        match state.client
            .post(&upstream_url)
            .header("Authorization", format!("Bearer {}", provider.api_key))
            .json(&forward_body)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                log_db!(
                    &state,
                    "RESP_IN",
                    "assistant",
                    "<< RAW CODEX UPSTREAM RESP  status={}  model={}",
                    status.as_u16(),
                    client_model
                );

                if !status.is_success() {
                    let body_bytes = resp.bytes().await.unwrap_or_default();
                    log_db!(
                        &state,
                        "ERROR",
                        "proxy",
                        "!! RAW CODEX UPSTREAM ERROR RESP  status={}  body={}",
                        status.as_u16(),
                        String::from_utf8_lossy(&body_bytes)
                    );
                    return Response::builder()
                        .status(status)
                        .body(Body::from(body_bytes))
                        .unwrap();
                }

                if is_stream {
                    let stream = resp.bytes_stream();
                    let (tx, rx) = mpsc::channel::<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>(32);
                    let log_clone = state.log.clone();
                    let db_clone = state.db.clone();

                    tokio::spawn(async move {
                        use futures::StreamExt;
                        futures::pin_mut!(stream);

                        log_write(&*log_clone, Some(&*db_clone), Some("STREAM_START"), Some("proxy"), ">> SPAWNED raw responses stream handler");

                        while let Some(result) = stream.next().await {
                            match result {
                                Ok(bytes) => {
                                    let item: Result<Bytes, Box<dyn std::error::Error + Send + Sync>> = Ok(bytes);
                                    if tx.send(item).await.is_err() {
                                        log_write(&*log_clone, Some(&*db_clone), Some("ERROR"), Some("proxy"), "!! CLIENT DISCONNECTED during raw stream");
                                        break;
                                    }
                                }
                                Err(e) => {
                                    log_write(&*log_clone, Some(&*db_clone), Some("ERROR"), Some("proxy"), &format!("!! RAW UPSTREAM STREAM ERROR  {}", e));
                                    let _ = tx
                                        .send(Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>))
                                        .await;
                                    break;
                                }
                            }
                        }
                        log_write(&*log_clone, Some(&*db_clone), Some("STREAM_FINISHED"), Some("proxy"), ">> FINISHED raw responses stream handler");
                    });

                    let mut rx = rx;
                    let rx_stream = poll_fn(move |cx: &mut Context<'_>| {
                        rx.poll_recv(cx)
                    });

                    return Response::builder()
                        .status(status)
                        .header("content-type", "text/event-stream")
                        .header("cache-control", "no-cache")
                        .header("x-accel-buffering", "no")
                        .body(Body::from_stream(rx_stream))
                        .unwrap_or_else(|e| {
                            error!(%e, "failed to build raw Responses stream response");
                            StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        });
                } else {
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_owned();

                    match resp.bytes().await {
                        Ok(body_bytes) => {
                            log_db!(
                                &state,
                                "RESP_IN",
                                "assistant",
                                "<< RAW CODEX UPSTREAM BODY  {} bytes",
                                body_bytes.len()
                            );
                            return Response::builder()
                                .status(status)
                                .header("content-type", content_type)
                                .body(Body::from(body_bytes))
                                .unwrap_or_else(|e| {
                                    error!(%e, "failed to build raw response");
                                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                                });
                        }
                        Err(e) => {
                            log_db!(&state, "ERROR", "proxy", "!! RAW READ ERROR  {}", e);
                            error!(%e, "failed to read raw upstream body");
                            return (
                                StatusCode::BAD_GATEWAY,
                                [("content-type", "application/json")],
                                Json(json!({"error": format!("upstream read: {e}")})),
                            )
                                .into_response();
                        }
                    }
                }
            }
            Err(e) => {
                log_db!(&state, "ERROR", "proxy", "!! RAW CONNECT ERROR  {}", e);
                error!(%e, "raw upstream request failed");
                return (
                    StatusCode::BAD_GATEWAY,
                    [("content-type", "application/json")],
                    Json(json!({"error": format!("upstream: {e}")})),
                )
                    .into_response();
            }
        }
    }

    log!(
        &state.log,
        "   Codex raw messages: {}",
        body.get("messages").map(|m| serde_json::to_string(m).unwrap_or_default()).unwrap_or_default()
    );
    // input 详细内容见 system_prompt.log，主日志只打条目数量
    {
        let count = body.get("input").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        log!(&state.log, "   Codex raw input: [{} items → see system_prompt.log]", count);
    }

    // === 诊断日志：不截断，精准打关键字段 ===
    {
        let prev_id = body.get("previous_response_id").and_then(|v| v.as_str()).unwrap_or("<none>");
        log_db!(&state, "DIAG", "proxy", "   [DIAG] previous_response_id={}", prev_id);

        // 统计 input 里各 type 的数量（不截断）
        if let Some(input_arr) = body.get("input").and_then(|v| v.as_array()) {
            let mut type_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            let mut role_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for item in input_arr {
                let t = item.get("type").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                let r = item.get("role").and_then(|v| v.as_str()).unwrap_or("-").to_string();
                *type_counts.entry(t).or_insert(0) += 1;
                *role_counts.entry(r).or_insert(0) += 1;
            }
            log_db!(&state, "DIAG", "proxy", "   [DIAG] input total={} type_counts={:?} role_counts={:?}",
                input_arr.len(), type_counts, role_counts);

            // 打出所有非系统类条目（type != message 的或 role == assistant 的）
            for (idx, item) in input_arr.iter().enumerate() {
                let t = item.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
                let r = item.get("role").and_then(|v| v.as_str()).unwrap_or("-");
                if t == "function_call" || t == "function_call_output" || r == "assistant" {
                    let brief = serde_json::to_string(item).unwrap_or_default();
                    let brief_safe = brief.chars().take(300).collect::<String>();
                    log_db!(&state, "TOOL_MATCH", r, "   [DIAG] input[{}] type={} role={} : {}",
                        idx, t, r, brief_safe);
                }
            }
        }
        // 也看看 messages 字段
        let msgs_exist = body.get("messages").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        log_db!(&state, "DIAG", "proxy", "   [DIAG] messages field count={}", msgs_exist);
    }



    // Translate Responses format to standard Chat Completions messages
    let mut system_parts: Vec<String> = Vec::new();
    let mut normal_messages: Vec<Value> = Vec::new();

    // 1. instructions -> system message part
    if let Some(inst) = body.get("instructions").and_then(|v| v.as_str()) {
        if !inst.is_empty() {
            system_parts.push(inst.to_owned());
        }
    }

    // 2. Extract existing messages
    if let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content").unwrap_or(&Value::Null).clone();
            
            // 如果历史上下文中有 system 角色，我们也把它归并到 system_parts 中以保兼容
            if role == "system" {
                if let Some(s) = content.as_str() {
                    system_parts.push(s.to_owned());
                } else if let Some(arr) = content.as_array() {
                    for part in arr {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            system_parts.push(t.to_owned());
                        }
                    }
                }
            } else {
                let mut new_msg = json!({
                    "role": role,
                    "content": normalize_message_content(&content)
                });

                // 重点：如果是工具调用结果 (role == "tool")，我们需要把工具调用 ID 属性传下去！
                if role == "tool" {
                    if let Some(call_id) = msg.get("call_id") {
                        new_msg["tool_call_id"] = call_id.clone();
                    } else if let Some(tool_call_id) = msg.get("tool_call_id") {
                        new_msg["tool_call_id"] = tool_call_id.clone();
                    } else if let Some(id) = msg.get("id") {
                        new_msg["tool_call_id"] = id.clone();
                    }
                }

                // 重点：如果是 assistant 产生的工具调用定义 (assistant 消息里的 tool_calls)
                //      我们也必须原封不动地传回给下游，否则大模型会产生上下文断裂！
                if role == "assistant" {
                    if let Some(tcs) = msg.get("tool_calls") {
                        new_msg["tool_calls"] = tcs.clone();
                    }
                }

                normal_messages.push(new_msg);
            }
        }
    }

    // 3. input -> system part or normal user messages
    enum TempItem {
        Normal(Value),
        ToolOutput {
            call_id: Value,
            call_id_str: String,
            initial_output: String,
            messages_snapshot: Vec<Value>,
        },
        None,
    }

    let mut temp_items = Vec::new();
    let mut current_normal_messages = normal_messages.clone();

    if let Some(input_val) = body.get("input") {
        if let Some(input_str) = input_val.as_str() {
            // 平铺文本，直接按照 user 处理
            if input_str.contains("<permissions instructions>")
                || input_str.contains("<skills_instructions>")
                || input_str.contains("<app-context>")
                || input_str.contains("<system-reminder>")
            {
                system_parts.push(input_str.to_owned());
                temp_items.push(TempItem::None);
            } else {
                let val = json!({
                    "role": "user",
                    "content": input_str
                });
                current_normal_messages.push(val.clone());
                temp_items.push(TempItem::Normal(val));
            }
        } else if let Some(input_arr) = input_val.as_array() {
            for item in input_arr {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let item_role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                
                if item_type == "function_call" {
                    // 大模型在历史上发起的工具调用
                    if let Some(call_id) = item.get("call_id") {
                        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let arguments = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                        
                        let val = json!({
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [
                                {
                                    "id": call_id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": arguments
                                    }
                                }
                            ]
                        });
                        current_normal_messages.push(val.clone());
                        temp_items.push(TempItem::Normal(val));
                    } else {
                        temp_items.push(TempItem::None);
                    }
                } else if item_type == "function_call_output" {
                    // 工具执行结果的返回
                    if let Some(call_id) = item.get("call_id") {
                        let output = item.get("output").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let call_id_str = call_id.as_str().unwrap_or("").to_string();
                        
                        // 记录需要并发执行的工具返回，并保存当前 normal_messages 快照
                        temp_items.push(TempItem::ToolOutput {
                            call_id: call_id.clone(),
                            call_id_str,
                            initial_output: output,
                            messages_snapshot: current_normal_messages.clone(),
                        });
                        
                        // 预先占位，后面更新
                        current_normal_messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id.clone(),
                            "content": ""
                        }));
                    } else {
                        temp_items.push(TempItem::None);
                    }
                } else {
                    // 常规文本或多层嵌套文本 (可能带有图片)
                    let downstream_role = match item_role {
                        "developer" => "system",
                        "system" => "system",
                        "assistant" => "assistant",
                        _ => "user",
                    };

                    let mut pushed_any = false;

                    if let Some(content_arr) = item.get("content").and_then(|c| c.as_array()) {
                        let mut openai_content_parts = Vec::new();
                        
                        for part in content_arr {
                            // 1. 处理 text 片段
                            if let Some(t_str) = part.get("text").and_then(|t| t.as_str()) {
                                if t_str.contains("<permissions instructions>")
                                    || t_str.contains("<skills_instructions>")
                                    || t_str.contains("<app-context>")
                                    || t_str.contains("<system-reminder>")
                                {
                                    system_parts.push(t_str.to_owned());
                                } else if downstream_role == "system" {
                                    system_parts.push(t_str.to_owned());
                                } else {
                                    openai_content_parts.push(json!({
                                        "type": "text",
                                        "text": t_str
                                    }));
                                }
                            }
                            // 2. 处理 image_url 片段
                            else if part.get("image_url").is_some() || part.get("type").and_then(|t| t.as_str()) == Some("image_url") {
                                openai_content_parts.push(part.clone());
                            }
                            // 3. 支持 Anthropic 格式的 image 片段作为 fallback 转换
                            else if part.get("type").and_then(|t| t.as_str()) == Some("image") {
                                if let Some(source) = part.get("source") {
                                    if let (Some(media_type), Some(data)) = (
                                        source.get("media_type").and_then(|v| v.as_str()),
                                        source.get("data").and_then(|v| v.as_str())
                                    ) {
                                        openai_content_parts.push(json!({
                                            "type": "image_url",
                                            "image_url": {
                                                "url": format!("data:{};base64,{}", media_type, data)
                                            }
                                        }));
                                    }
                                }
                            }
                        }

                        if !openai_content_parts.is_empty() && downstream_role != "system" {
                            let val = json!({
                                "role": downstream_role,
                                "content": normalize_message_content(&json!(openai_content_parts))
                            });
                            current_normal_messages.push(val.clone());
                            temp_items.push(TempItem::Normal(val));
                            pushed_any = true;
                        }
                    }

                    // 4. 处理平铺的 text 字段
                    if let Some(t_str) = item.get("text").and_then(|t| t.as_str()) {
                        if t_str.contains("<permissions instructions>")
                            || t_str.contains("<skills_instructions>")
                            || t_str.contains("<app-context>")
                            || t_str.contains("<system-reminder>")
                        {
                            system_parts.push(t_str.to_owned());
                        } else {
                            let val = json!({
                                "role": downstream_role,
                                "content": t_str
                            });
                            current_normal_messages.push(val.clone());
                            temp_items.push(TempItem::Normal(val));
                            pushed_any = true;
                        }
                    }

                    if !pushed_any {
                        temp_items.push(TempItem::None);
                    }
                }
            }
        }
    }

    // 并发执行所有的工具调用拦截拦截逻辑
    let mut futures = Vec::new();
    for (idx, item) in temp_items.iter().enumerate() {
        if let TempItem::ToolOutput { call_id_str, initial_output, messages_snapshot, .. } = item {
            let call_id_str = call_id_str.clone();
            let initial_output = initial_output.clone();
            let messages_snapshot = messages_snapshot.clone();
            let workspace = state.workspace.clone();
            let log = state.log.clone();
            futures.push(async move {
                let final_output = crate::agent::intercept_and_execute(
                    &call_id_str,
                    initial_output,
                    &messages_snapshot,
                    &workspace,
                    &log
                ).await;
                (idx, final_output)
            });
        }
    }

    let results = futures::future::join_all(futures).await;
    let mut final_outputs = std::collections::HashMap::new();
    for (idx, final_output) in results {
        final_outputs.insert(idx, final_output);
    }

    // 将 temp_items 重新构建回 normal_messages
    for (idx, item) in temp_items.into_iter().enumerate() {
        match item {
            TempItem::Normal(val) => {
                normal_messages.push(val);
            }
            TempItem::ToolOutput { call_id, .. } => {
                let output = final_outputs.remove(&idx).unwrap_or_default();
                normal_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output
                }));
            }
            TempItem::None => {}
        }
    }


    // 4. Assemble final unified messages list
    let mut final_messages: Vec<Value> = Vec::new();
    
    // 追加我们强制要求的代理约束提示词
    system_parts.push(crate::agent::generate_agent_constraints());
    
    if !system_parts.is_empty() {
        let unified_system = system_parts.join("\n\n");
        final_messages.push(json!({
            "role": "system",
            "content": unified_system
        }));
    }
    crate::agent::restore_history(&mut normal_messages, state.workspace.root());
    final_messages.extend(normal_messages);

    // Build downstream Chat Completions body
    let mut openai_body = json!({
        "model": upstream_model,
        "messages": final_messages,
        "stream": false, // Force non-streaming for stable translation
    });

    let mut tool_route_map = std::collections::HashMap::new();

    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        log!(
            &state.log,
            "   Codex raw tools: {}",
            serde_json::to_string(tools).unwrap_or_default()
        );

        let mut converted_tools = Vec::new();
        
        // 辅助闭包：处理单个工具对象，将其转换并安全推入 converted_tools 中，支持加上前缀
        let mut push_converted_tool = |t: &Value, prefix: Option<&str>, route_map: &mut std::collections::HashMap<String, (String, String)>| {
            if !t.is_object() {
                return;
            }

            // 提取工具原始名称并智能注入前缀
            let mut name_val = t.get("name").cloned().unwrap_or(Value::Null);
            if let Some(prefix_str) = prefix {
                if let Some(n) = name_val.as_str() {
                    let alias = format!("{}__{}", prefix_str, n);
                    route_map.insert(alias.clone(), (prefix_str.to_string(), n.to_string()));
                    name_val = json!(alias);
                }
            }

            // 1. 如果它已经是标准的 OpenAI Chat Completions 嵌套格式且 type 是 function
            if t.get("type").and_then(|v| v.as_str()) == Some("function") && t.get("function").is_some() {
                let mut tool_clone = t.clone();
                if prefix.is_some() {
                    tool_clone["function"]["name"] = name_val;
                }
                converted_tools.push(tool_clone);
            } 
            // 2. 如果是平铺的 function 格式 (如 OpenAI Responses/Realtime API 的工具定义)
            else if t.get("type").and_then(|v| v.as_str()) == Some("function") && t.get("name").is_some() {
                converted_tools.push(json!({
                    "type": "function",
                    "function": {
                        "name": name_val,
                        "description": t.get("description"),
                        "parameters": t.get("parameters")
                    }
                }));
            }
            // 3. 如果是 Anthropic 格式的平铺定义 (有 name 且无 type，或者无 function)
            else if t.get("name").is_some() && t.get("type").is_none() {
                converted_tools.push(json!({
                    "type": "function",
                    "function": {
                        "name": name_val,
                        "description": t.get("description"),
                        "parameters": t.get("input_schema").or_else(|| t.get("parameters"))
                    }
                }));
            }
            // 4. Codex 特殊工具，例如 type: "tool_search", type: "web_search"
            else if let Some(type_str) = t.get("type").and_then(|v| v.as_str()) {
                if type_str != "function" && type_str != "namespace" {
                    // 这类工具没有 name 字段，用 type_str 作为 name，绝不能传 null
                    let effective_name = if name_val.is_null() {
                        json!(type_str)
                    } else {
                        name_val.clone()
                    };
                    converted_tools.push(json!({
                        "type": "function",
                        "function": {
                            "name": effective_name,
                            "description": t.get("description").unwrap_or(&json!("")),
                            "parameters": t.get("parameters").unwrap_or(&json!({
                                "type": "object",
                                "properties": {},
                                "additionalProperties": false
                            }))
                        }
                    }));
                }
            }
        };

        // 定义需要被屏蔽的 Codex 原生危险工具
        // 注意：我们把 shell 的控制权收归到 codex_workspace_mcp__shell，所以隐藏原生 shell 工具
        let blocked_types: &[&str] = &["shell", "code_execution", "bash", "computer_use"];
        let blocked_names: &[&str] = &["run_terminal_cmd", "execute_command", "exec_command", "computer_use", "bash", "mcp__codex_workspace_mcp"];

        for t in tools {
            if !t.is_object() {
                continue;
            }

            // 按 type 过滤掉危险工具
            let tool_type = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if blocked_types.contains(&tool_type) {
                log_db!(&state, "TOOL_BLOCKED", "proxy", "   [AGENT] Blocked Codex tool by type: '{}'", tool_type);
                continue;
            }
            // 按 name 过滤掉危险工具
            let tool_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let func_name = t.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
            if blocked_names.contains(&tool_name) || blocked_names.contains(&func_name) {
                log_db!(&state, "TOOL_BLOCKED", "proxy", "   [AGENT] Blocked Codex tool by name: '{}'", tool_name);
                continue;
            }

            // A. 如果是命名空间工具，展开子工具
            if let Some(sub_tools) = t.get("tools").and_then(|v| v.as_array()) {
                let ns_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                for sub_t in sub_tools {
                    push_converted_tool(sub_t, Some(ns_name), &mut tool_route_map);
                }
            } 
            // B. 常规独立工具
            else {
                push_converted_tool(t, None, &mut tool_route_map);
            }
        }

        // 将咱们的 Workspace 特权工具插入到列表头部（最高优先级，大模型最先看到）
        let mut priority_tools = Vec::new();
        crate::agent::inject_workspace_tools(&mut priority_tools);
        priority_tools.extend(converted_tools);
        let converted_tools = priority_tools;

        if !converted_tools.is_empty() {
            openai_body["tools"] = json!(converted_tools);
        }
    } else {
        let mut converted_tools = Vec::new();
        crate::agent::inject_workspace_tools(&mut converted_tools);
        if !converted_tools.is_empty() {
            openai_body["tools"] = json!(converted_tools);
        }
    }
    if let Some(tool_choice) = body.get("tool_choice") {
        openai_body["tool_choice"] = tool_choice.clone();
    }
    if let Some(temp) = body.get("temperature") {
        openai_body["temperature"] = temp.clone();
    }
    if let Some(max_t) = body.get("max_tokens") {
        openai_body["max_tokens"] = max_t.clone();
    }

    log_db!(
        &state,
        "REQ_OUT",
        "proxy",
        "   forwarding ChatCompletions body: {}",
        fmt_body(serde_json::to_string(&openai_body).unwrap_or_default().as_bytes())
    );

    let upstream_url = format!("{}/chat/completions", provider.url);

    // Force stream: true for downstream provider
    openai_body["stream"] = json!(true);

    log_db!(
        &state,
        "REQ_OUT",
        "proxy",
        "   forwarding ChatCompletions stream body: {}",
        fmt_body(serde_json::to_string(&openai_body).unwrap_or_default().as_bytes())
    );

    let resp = match state.client
        .post(&upstream_url)
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .json(&openai_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log_db!(&state, "ERROR", "proxy", "!! CONNECT ERROR  {}", e);
            error!(%e, "upstream stream request failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("upstream: {e}")})),
            )
                .into_response();
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body_bytes = resp.bytes().await.unwrap_or_default();
        log_db!(
            &state,
            "ERROR",
            "proxy",
            "!! UPSTREAM STREAM ERROR RESP  status={}  body={}",
            status.as_u16(),
            String::from_utf8_lossy(&body_bytes)
        );
        return Response::builder()
            .status(status)
            .body(Body::from(body_bytes))
            .unwrap();
    }

    // High performance SSE Streaming Relay!
    let (tx, rx) = mpsc::channel::<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>(32);
    let stream = resp.bytes_stream();
    let mut converter = format_translate::ResponsesStreamConverter::new(
        client_model,
        tool_route_map,
        state.workspace.root().to_path_buf(),
    );
    let log_clone = state.log.clone();
    let db_clone = state.db.clone();

    tokio::spawn(async move {
        use futures::StreamExt;
        futures::pin_mut!(stream);

        log_write(&*log_clone, Some(&*db_clone), Some("STREAM_START"), Some("proxy"), ">> SPAWNED responses stream handler");

        while let Some(result) = stream.next().await {
            match result {
                Ok(bytes) => {
                    log_write(&*log_clone, None, None, None, &format!(">> RECEIVED {} bytes from upstream stream", bytes.len()));
                    let converted = converter.feed(&bytes);
                    if !converted.is_empty() {
                        log_write(&*log_clone, None, None, None, &format!(">> FORWARDING {} bytes to client", converted.len()));
                        let item: Result<Bytes, Box<dyn std::error::Error + Send + Sync>> =
                            Ok(Bytes::from(converted));
                        if tx.send(item).await.is_err() {
                            log_write(&*log_clone, Some(&*db_clone), Some("ERROR"), Some("proxy"), "!! CLIENT DISCONNECTED during stream");
                            break;
                        }
                    }
                }
                Err(e) => {
                    log_write(&*log_clone, Some(&*db_clone), Some("ERROR"), Some("proxy"), &format!("!! UPSTREAM STREAM ERROR  {}", e));
                    let _ = tx
                        .send(Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>))
                        .await;
                    break;
                }
            }
        }

        let remaining = converter.flush();
        if !remaining.is_empty() {
            log_write(&*log_clone, None, None, None, &format!(">> FORWARDING final {} bytes to client", remaining.len()));
            let item: Result<Bytes, Box<dyn std::error::Error + Send + Sync>> =
                Ok(Bytes::from(remaining));
            let _ = tx.send(item).await;
        }
        log_write(&*log_clone, Some(&*db_clone), Some("STREAM_FINISHED"), Some("proxy"), ">> FINISHED responses stream handler");
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
            error!(%e, "failed to build Responses stream response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub async fn run(listener: TcpListener, config_path: &Path, workspace: Arc<crate::tools::Workspace>) -> anyhow::Result<()> {
    let config: AiProxyConfig =
        serde_json::from_str(&tokio::fs::read_to_string(config_path).await?)?;

    // Open log file next to config
    let log_path = config_path.with_file_name("ai_proxy.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    // 系统提示词专用日志（完整无截断）
    let sys_log_path = config_path.with_file_name("system_prompt.log");
    let sys_log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sys_log_path)?;

    let log = Arc::new(Mutex::new(log_file));
    let sys_log = Arc::new(Mutex::new(sys_log_file));
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

    let conn = crate::database::init_db(workspace.root())?;
    let db = Arc::new(Mutex::new(conn));

    let state = AiProxyState {
        config,
        client,
        log,
        log_path,
        sys_log,
        workspace,
        db,
    };


    let app = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .with_state(state)
        .layer(CorsLayer::permissive());

    let addr = listener.local_addr()?;
    info!(%addr, "AI proxy starting");

    axum::serve(listener, app).await?;
    Ok(())
}
