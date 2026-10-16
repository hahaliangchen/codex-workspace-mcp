//! Browser UI for the standalone Rust agent, served from the frontend build.

use std::path::PathBuf;

use axum::{
    Json,
    extract::State,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use serde_json::json;

use crate::agent_service::AgentServiceState;

/// Vite output of `frontend/`; `AGENT_WEB_DIR` points elsewhere for packaged installs.
pub fn web_dir() -> PathBuf {
    std::env::var_os("AGENT_WEB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/frontend/dist")))
}

pub async fn page() -> Response {
    match tokio::fs::read_to_string(web_dir().join("index.html")).await {
        Ok(html) => ([(header::CACHE_CONTROL, "no-cache")], Html(html)).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent frontend is not built. Run npm install and npm run build in frontend/, or set AGENT_WEB_DIR to the built frontend directory.",
        ).into_response(),
    }
}

pub async fn settings_page() -> Response {
    page().await
}

pub async fn info(State(state): State<AgentServiceState>) -> impl IntoResponse {
    let state = state.with_latest_config().await;
    let ready = !state.plugin_cancel.is_cancelled()
        && !state.provider_url.is_empty()
        && !state.default_model.is_empty();
    Json(json!({
        "ready": ready,
        "workspace": state.workspace.root().display().to_string(),
        "model": state.default_model,
    }))
}

pub async fn star_png() -> impl IntoResponse {
    let path = web_dir().join("star.png");
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return ([(header::CONTENT_TYPE, "image/png")], bytes).into_response();
    }
    let alt = web_dir().join("logo.png");
    if let Ok(bytes) = tokio::fs::read(&alt).await {
        return ([(header::CONTENT_TYPE, "image/png")], bytes).into_response();
    }
    (
        [(header::CONTENT_TYPE, "image/png")],
        include_bytes!("../frontend/src/assets/star.png").to_vec(),
    )
        .into_response()
}

pub async fn favicon_png() -> impl IntoResponse {
    let path = web_dir().join("favicon.png");
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return ([(header::CONTENT_TYPE, "image/png")], bytes).into_response();
    }
    star_png().await.into_response()
}
