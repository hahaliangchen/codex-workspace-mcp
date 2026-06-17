use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
    workspace: Arc<crate::tools::Workspace>,
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

pub(crate) fn conversation_id_from_body(_body: &Value, workspace_root: &Path) -> String {
    format!(
        "workspace:{}",
        sanitize_conversation_id(&workspace_root.to_string_lossy())
    )
}

fn sanitize_conversation_id(id: &str) -> String {
    let mut out = String::new();
    for ch in id.chars().take(120) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_owned()
    } else {
        out
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

    crate::vision_preprocess::set_visible_images_from_body(&body);
    let had_image_input = crate::vision_preprocess::has_latest_user_image_input(&body);
    let mut image_stats = crate::vision_preprocess::ImageProcessStats::default();
    crate::vision_preprocess::process_latest_user_images(&mut body, &mut image_stats).await;

    info!("=== /v1/chat/completions  model={}", client_model);

    let (provider, mut upstream_model) = match resolve_model(&state.config, &client_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    if had_image_input {
        let old_model = upstream_model.clone();
        upstream_model = crate::vision_preprocess::adjust_model_for_vision(&upstream_model);
        if old_model != upstream_model {
            info!(
                "   [DYNAMIC ROUTING] Image detected. Switched model from {} to {}",
                old_model, upstream_model
            );
        }
    }

    info!(
        "   resolved: provider={} upstream_model={}",
        provider.url, upstream_model
    );

    body["model"] = json!(upstream_model);

    // Normalize role="developer" to "system" for upstream compatibility
    if let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) {
        for msg in &mut *messages {
            if let Some(role) = msg.get_mut("role") {
                if role.as_str() == Some("developer") {
                    *role = json!("system");
                }
            }
        }
        format_translate::clean_unmatched_tool_calls(messages);
    }

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    info!(
        "   chat completions body: {}",
        fmt_body(serde_json::to_string(&body).unwrap_or_default().as_bytes())
    );

    let upstream_url = format!("{}/chat/completions", provider.url);
    crate::upstream::forward_to_upstream(
        &state.client,
        &upstream_url,
        &provider.api_key,
        &body,
        is_stream,
        &client_model,
    )
    .await
}

/// POST /v1/messages — Anthropic Messages API endpoint.
async fn messages(State(state): State<AiProxyState>, Json(mut body): Json<Value>) -> Response {
    crate::vision_preprocess::set_visible_images_from_body(&body);
    let had_image_input = crate::vision_preprocess::has_latest_user_image_input(&body);
    let mut image_stats = crate::vision_preprocess::ImageProcessStats::default();
    crate::vision_preprocess::process_latest_user_images(&mut body, &mut image_stats).await;
    let raw_model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");

    info!("=== /v1/messages  model={}", raw_model);
    info!(
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
            info!(
                "   [DYNAMIC ROUTING] Image detected. Switched model from {} to {}",
                old_model, upstream_model
            );
        }
    }

    info!(
        "   resolved: provider={} upstream_model={}",
        provider.url, upstream_model
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
        info!("   anthropic native forward, url={}", provider.url);
        (raw_body, provider.url.clone())
    } else {
        // OpenAI-compatible upstream: convert Anthropic → OpenAI.
        let mut openai_body = format_translate::anthropic_to_openai(&body);
        openai_body["model"] = json!(upstream_model);
        info!(
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
    )
    .await;

    // For non-streaming OpenAI providers, convert response back to Anthropic.
    if !is_stream && !is_anthropic_provider && resp.status().is_success() {
        let status = resp.status();
        let body_bytes = match axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => {
                error!(%e, "failed to read response body for Anthropic conversion");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("response read: {e}")})),
                )
                    .into_response();
            }
        };

        info!("   openai resp body: {}", fmt_body(&body_bytes));

        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(openai_resp) => {
                let anthropic_resp = format_translate::openai_to_anthropic(&openai_resp, raw_model);
                info!(
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
async fn responses(State(state): State<AiProxyState>, Json(body): Json<Value>) -> Response {
    let conversation_id = conversation_id_from_body(&body, state.workspace.root());
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

    let (provider, upstream_model) = match resolve_model(&state.config, &client_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    crate::responses::logging::log_request_body(&conversation_id, &body, &client_model).await;
    info!("   responses request entering local agent runtime");
    crate::agent_runtime::run_responses_agent(
        state.client.clone(),
        state.workspace.clone(),
        provider.url.clone(),
        provider.api_key.clone(),
        body,
        upstream_model,
        client_model,
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

    let total_maps: usize = config.providers.values().map(|p| p.model_map.len()).sum();

    let config = Arc::new(config);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let log_dir = config_path.with_file_name("logs");
    crate::proxy_log::init(log_dir)?;

    info!("========== AI Proxy started ==========");
    info!(
        "config: {} providers, {} total model mappings, default={:?}",
        config.providers.len(),
        total_maps,
        config.default_provider
    );

    let state = AiProxyState {
        config,
        client,
        workspace,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_id_uses_workspace_as_context_boundary() {
        let body = json!({
            "conversation": "conv_123",
            "previous_response_id": "resp_456"
        });

        let id = conversation_id_from_body(&body, Path::new("D:/workspace"));

        assert_eq!(id, "workspace:D:_workspace");
    }
}
