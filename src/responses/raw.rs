use axum::response::Response;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub async fn forward_raw_codex_responses(
    client: &Client,
    log: Arc<Mutex<std::fs::File>>,
    db: Arc<Mutex<rusqlite::Connection>>,
    provider_url: &str,
    api_key: &str,
    body: &Value,
    upstream_model: String,
    client_model: &str,
) -> Response {
    let mut forward_body = body.clone();
    forward_body["model"] = json!(upstream_model);

    let raw_tools = crate::tool_prepare::prepare_tools_for_model(
        forward_body.get("tools"),
        crate::tool_prepare::ToolFormat::Responses,
    );
    log_blocked_tools(&log, &db, &raw_tools.blocked);

    if !raw_tools.tools.is_empty() {
        forward_body["tools"] = json!(raw_tools.tools);
    }

    let upstream_url = format!("{}/responses", provider_url);
    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    crate::upstream::forward_raw_responses(
        client,
        &upstream_url,
        api_key,
        &forward_body,
        is_stream,
        client_model,
        log,
        db,
    )
    .await
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
        crate::ai_proxy::log_write(
            log,
            Some(db),
            Some("TOOL_BLOCKED"),
            Some("proxy"),
            &format!(
                "   [AGENT] Blocked Codex tool by {}: '{}'",
                label, blocked.value
            ),
        );
    }
}
