use axum::{
    Json,
    body::{Body, Bytes},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures::stream::poll_fn;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::task::Context;
use tokio::sync::mpsc;
use tracing::error;

pub struct PreparedResponsesChatRequest {
    pub body: Value,
    pub tool_route_map: HashMap<String, (String, String)>,
}

pub async fn prepare_chat_completions_request(
    body: &Value,
    upstream_model: String,
    workspace: Arc<crate::tools::Workspace>,
    log: Arc<Mutex<std::fs::File>>,
    db: Arc<Mutex<rusqlite::Connection>>,
) -> PreparedResponsesChatRequest {
    let final_messages =
        crate::responses::messages::prepare_chat_messages(body, workspace, log.clone()).await;

    let mut openai_body = json!({
        "model": upstream_model,
        "messages": final_messages,
        "stream": false
    });

    log_raw_tools(&log, body);

    let prepared_tools = crate::tool_prepare::prepare_tools_for_model(
        body.get("tools"),
        crate::tool_prepare::ToolFormat::ChatCompletions,
    );
    log_blocked_tools(&log, &db, &prepared_tools.blocked);

    if !prepared_tools.tools.is_empty() {
        openai_body["tools"] = json!(prepared_tools.tools);
    }
    copy_optional_request_fields(body, &mut openai_body);

    crate::ai_proxy::log_write(
        &*log,
        Some(&*db),
        Some("REQ_OUT"),
        Some("proxy"),
        &format!(
            "   forwarding ChatCompletions body: {}",
            crate::ai_proxy::fmt_body(
                serde_json::to_string(&openai_body)
                    .unwrap_or_default()
                    .as_bytes()
            )
        ),
    );

    openai_body["stream"] = json!(true);
    crate::ai_proxy::log_write(
        &*log,
        Some(&*db),
        Some("REQ_OUT"),
        Some("proxy"),
        &format!(
            "   forwarding ChatCompletions stream body: {}",
            crate::ai_proxy::fmt_body(
                serde_json::to_string(&openai_body)
                    .unwrap_or_default()
                    .as_bytes()
            )
        ),
    );

    PreparedResponsesChatRequest {
        body: openai_body,
        tool_route_map: prepared_tools.route_map,
    }
}

pub async fn forward_chat_completions_stream(
    client: &Client,
    provider_url: &str,
    api_key: &str,
    body: &Value,
    client_model: String,
    tool_route_map: HashMap<String, (String, String)>,
    stream_prefix: Option<String>,
    log: Arc<Mutex<std::fs::File>>,
    db: Arc<Mutex<rusqlite::Connection>>,
) -> Response {
    let upstream_url = format!("{}/chat/completions", provider_url);
    let resp = match client
        .post(&upstream_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::ai_proxy::log_write(
                &*log,
                Some(&*db),
                Some("ERROR"),
                Some("proxy"),
                &format!("!! CONNECT ERROR  {}", e),
            );
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
        crate::ai_proxy::log_write(
            &*log,
            Some(&*db),
            Some("ERROR"),
            Some("proxy"),
            &format!(
                "!! UPSTREAM STREAM ERROR RESP  status={}  body={}",
                status.as_u16(),
                String::from_utf8_lossy(&body_bytes)
            ),
        );
        return Response::builder()
            .status(status)
            .body(Body::from(body_bytes))
            .unwrap();
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>(32);
    let stream = resp.bytes_stream();
    let mut converter = crate::format_translate::ResponsesStreamConverter::new(
        client_model,
        tool_route_map,
        stream_prefix,
    );
    let log_clone = log.clone();
    let db_clone = db.clone();

    tokio::spawn(async move {
        use futures::StreamExt;
        futures::pin_mut!(stream);

        crate::ai_proxy::log_write(
            &*log_clone,
            Some(&*db_clone),
            Some("STREAM_START"),
            Some("proxy"),
            ">> SPAWNED responses stream handler",
        );

        while let Some(result) = stream.next().await {
            match result {
                Ok(bytes) => {
                    crate::ai_proxy::log_write(
                        &*log_clone,
                        None,
                        None,
                        None,
                        &format!(">> RECEIVED {} bytes from upstream stream", bytes.len()),
                    );
                    let converted = converter.feed(&bytes);
                    if !converted.is_empty() {
                        crate::ai_proxy::log_write(
                            &*log_clone,
                            None,
                            None,
                            None,
                            &format!(">> FORWARDING {} bytes to client", converted.len()),
                        );
                        let item: Result<Bytes, Box<dyn std::error::Error + Send + Sync>> =
                            Ok(Bytes::from(converted));
                        if tx.send(item).await.is_err() {
                            crate::ai_proxy::log_write(
                                &*log_clone,
                                Some(&*db_clone),
                                Some("ERROR"),
                                Some("proxy"),
                                "!! CLIENT DISCONNECTED during stream",
                            );
                            break;
                        }
                    }
                }
                Err(e) => {
                    crate::ai_proxy::log_write(
                        &*log_clone,
                        Some(&*db_clone),
                        Some("ERROR"),
                        Some("proxy"),
                        &format!("!! UPSTREAM STREAM ERROR  {}", e),
                    );
                    let _ = tx
                        .send(Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>))
                        .await;
                    break;
                }
            }
        }

        let remaining = converter.flush();
        if !remaining.is_empty() {
            crate::ai_proxy::log_write(
                &*log_clone,
                None,
                None,
                None,
                &format!(">> FORWARDING final {} bytes to client", remaining.len()),
            );
            let item: Result<Bytes, Box<dyn std::error::Error + Send + Sync>> =
                Ok(Bytes::from(remaining));
            let _ = tx.send(item).await;
        }
        crate::ai_proxy::log_write(
            &*log_clone,
            Some(&*db_clone),
            Some("STREAM_FINISHED"),
            Some("proxy"),
            ">> FINISHED responses stream handler",
        );
    });

    let mut rx = rx;
    let rx_stream = poll_fn(move |cx: &mut Context<'_>| rx.poll_recv(cx));

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

fn log_raw_tools(log: &Mutex<std::fs::File>, body: &Value) {
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        crate::ai_proxy::log_write(
            log,
            None,
            None,
            None,
            &format!(
                "   Codex raw tools: {}",
                serde_json::to_string(tools).unwrap_or_default()
            ),
        );
    }
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

fn copy_optional_request_fields(source: &Value, target: &mut Value) {
    for field in ["tool_choice", "temperature", "max_tokens"] {
        if let Some(value) = source.get(field) {
            target[field] = value.clone();
        }
    }
}
