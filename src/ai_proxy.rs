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
