use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
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

pub fn now_china() -> String {
    crate::proxy_log::now_china()
}

pub fn log_write(
    log: &Mutex<std::fs::File>,
    db: Option<&Mutex<rusqlite::Connection>>,
    action: Option<&str>,
    role: Option<&str>,
    msg: &str,
) {
    crate::proxy_log::write(log, db, action, role, msg);
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

    let upstream = provider
        .model_map
        .get(model)
        .cloned()
        .unwrap_or_else(|| model.to_owned());
    Ok((provider, upstream))
}

/// Truncate body for logging (keep first ~2000 chars).
pub(crate) fn fmt_body(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    if s.len() > 2000 {
        format!("{}… ({} bytes)", &s[..2000], s.len())
    } else {
        s.to_string()
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
    crate::vision_preprocess::set_visible_images_from_body(&body);
    let had_image_input = crate::vision_preprocess::has_latest_user_image_input(&body);
    let mut image_stats = crate::vision_preprocess::ImageProcessStats::default();
    crate::vision_preprocess::process_latest_user_images(
        &mut body,
        &state.log,
        Some(&state.db),
        &mut image_stats,
    )
    .await;
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

    log!(
        &state.log,
        "=== /v1/chat/completions  model={}",
        client_model
    );

    let (provider, mut upstream_model) = match resolve_model(&state.config, &client_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    if had_image_input {
        let old_model = upstream_model.clone();
        upstream_model = crate::vision_preprocess::adjust_model_for_vision(&upstream_model);
        if old_model != upstream_model {
            log!(
                &state.log,
                "   [DYNAMIC ROUTING] Image detected. Switched model from {} to {}",
                old_model,
                upstream_model
            );
        }
    }

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
    crate::upstream::forward_to_upstream(
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
async fn messages(State(state): State<AiProxyState>, Json(mut body): Json<Value>) -> Response {
    crate::vision_preprocess::set_visible_images_from_body(&body);
    let had_image_input = crate::vision_preprocess::has_latest_user_image_input(&body);
    let mut image_stats = crate::vision_preprocess::ImageProcessStats::default();
    crate::vision_preprocess::process_latest_user_images(
        &mut body,
        &state.log,
        Some(&state.db),
        &mut image_stats,
    )
    .await;
    let raw_model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");

    log!(&state.log, "=== /v1/messages  model={}", raw_model);
    log!(
        &state.log,
        "   anthropic body: {}",
        fmt_body(serde_json::to_string(&body).unwrap_or_default().as_bytes())
    );

    let (provider, mut upstream_model) = match resolve_model(&state.config, raw_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    if had_image_input {
        let old_model = upstream_model.clone();
        upstream_model = crate::vision_preprocess::adjust_model_for_vision(&upstream_model);
        if old_model != upstream_model {
            log!(
                &state.log,
                "   [DYNAMIC ROUTING] Image detected. Switched model from {} to {}",
                old_model,
                upstream_model
            );
        }
    }

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
            fmt_body(
                serde_json::to_string(&openai_body)
                    .unwrap_or_default()
                    .as_bytes()
            )
        );
        let url = format!("{}/chat/completions", provider.url);
        (openai_body, url)
    };

    let resp = crate::upstream::forward_to_upstream(
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

        log!(&state.log, "   openai resp body: {}", fmt_body(&body_bytes));

        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(openai_resp) => {
                let anthropic_resp = format_translate::openai_to_anthropic(&openai_resp, raw_model);
                log!(
                    &state.log,
                    "   anthropic resp: {}",
                    fmt_body(
                        serde_json::to_string(&anthropic_resp)
                            .unwrap_or_default()
                            .as_bytes()
                    )
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

/// POST /v1/responses — OpenAI Responses API endpoint for Codex.
async fn responses(State(state): State<AiProxyState>, Json(mut body): Json<Value>) -> Response {
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

    let (provider, mut upstream_model) = match resolve_model(&state.config, &client_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    if provider.raw_codex {
        crate::responses::logging::log_request_body(
            &state.log,
            &state.db,
            &state.sys_log,
            &body,
            &client_model,
        );
        log_db!(
            &state,
            "PROXY",
            "proxy",
            "   raw_codex provider detected; skipping proxy image preprocessing"
        );
        return crate::responses::raw::forward_raw_codex_responses(
            &state.client,
            state.log.clone(),
            state.db.clone(),
            &provider.url,
            &provider.api_key,
            &body,
            upstream_model,
            &client_model,
        )
        .await;
    }

    crate::vision_preprocess::set_visible_images_from_body(&body);
    let had_image_input = crate::vision_preprocess::has_latest_user_image_input(&body);
    let mut image_stats = crate::vision_preprocess::ImageProcessStats::default();
    crate::vision_preprocess::process_latest_user_images(
        &mut body,
        &state.log,
        Some(&state.db),
        &mut image_stats,
    )
    .await;

    crate::responses::logging::log_request_body(
        &state.log,
        &state.db,
        &state.sys_log,
        &body,
        &client_model,
    );

    if had_image_input {
        let old_model = upstream_model.clone();
        upstream_model = crate::vision_preprocess::adjust_model_for_vision(&upstream_model);
        if old_model != upstream_model {
            log_db!(
                &state,
                "PROXY",
                "proxy",
                "   [DYNAMIC ROUTING] Image detected. Switched model from {} to {}",
                old_model,
                upstream_model
            );
        }
        log_db!(
            &state,
            "VISION_STATUS",
            "proxy",
            "   image preprocessing stats: seen={} analyzed={} cache_hits={} failed={}",
            image_stats.seen,
            image_stats.analyzed,
            image_stats.cache_hits,
            image_stats.failed
        );
    }

    log_db!(
        &state,
        "PROXY",
        "proxy",
        "   resolved: provider={} upstream_model={}",
        provider.url,
        upstream_model
    );

    log!(
        &state.log,
        "   Codex raw messages: {}",
        body.get("messages")
            .map(|m| serde_json::to_string(m).unwrap_or_default())
            .unwrap_or_default()
    );
    // input 详细内容见 system_prompt.log，主日志只打条目数量
    {
        let count = body
            .get("input")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        log!(
            &state.log,
            "   Codex raw input: [{} items → see system_prompt.log]",
            count
        );
    }

    crate::responses::logging::log_diagnostics(&state.log, &state.db, &body);

    let prepared_chat = crate::responses::chat::prepare_chat_completions_request(
        &body,
        upstream_model,
        state.workspace.clone(),
        state.log.clone(),
        state.db.clone(),
    )
    .await;
    let openai_body = prepared_chat.body;
    let tool_route_map = prepared_chat.tool_route_map;

    crate::responses::chat::forward_chat_completions_stream(
        &state.client,
        &provider.url,
        &provider.api_key,
        &openai_body,
        client_model,
        tool_route_map,
        image_stats.codex_prefix(),
        state.log.clone(),
        state.db.clone(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub async fn run(
    listener: TcpListener,
    config_path: &Path,
    workspace: Arc<crate::tools::Workspace>,
) -> anyhow::Result<()> {
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
