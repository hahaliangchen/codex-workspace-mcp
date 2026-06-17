use std::task::Context;

use axum::{
    Json,
    body::{Body, Bytes},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures::stream::poll_fn;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::format_translate;

pub async fn forward_to_upstream(
    client: &Client,
    upstream_url: &str,
    api_key: &str,
    body: &Value,
    is_stream: bool,
    client_model: &str,
) -> Response {
    let full_req_json = serde_json::to_string_pretty(body).unwrap_or_default();
    crate::proxy_log::write_upstream_context(&format!(
        "UPSTREAM REQ\nmodel: {}\nurl: {}\nstream: {}\n\n{}",
        client_model, upstream_url, is_stream, full_req_json
    ))
    .await;

    info!(
        ">> UPSTREAM REQ  model={}  url={}  stream={}",
        client_model, upstream_url, is_stream
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
            info!(
                "<< UPSTREAM RESP  status={}  model={}",
                status.as_u16(),
                client_model
            );

            if is_stream {
                forward_chat_stream(resp, status, client_model).await
            } else {
                forward_non_streaming_body(resp, status).await
            }
        }
        Err(e) => {
            error!(%e, "!! CONNECT ERROR upstream request failed");
            (
                StatusCode::BAD_GATEWAY,
                [("content-type", "application/json")],
                Json(json!({"error": format!("upstream: {e}")})),
            )
                .into_response()
        }
    }
}

async fn forward_chat_stream(
    resp: reqwest::Response,
    status: reqwest::StatusCode,
    client_model: &str,
) -> Response {
    let model = client_model.to_owned();
    let converter = std::sync::Mutex::new(format_translate::StreamConverter::new(model));
    let stream = resp.bytes_stream();

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
                    error!(%e, "!! STREAM ERROR");
                    let _ = tx
                        .send(Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>))
                        .await;
                    break;
                }
            }
        }

        let remaining = converter.lock().unwrap().flush();
        if !remaining.is_empty() {
            let item: Result<Bytes, Box<dyn std::error::Error + Send + Sync>> =
                Ok(Bytes::from(remaining));
            let _ = tx.send(item).await;
        }
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
            error!(%e, "failed to build streaming response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })
}

async fn forward_non_streaming_body(
    resp: reqwest::Response,
    status: reqwest::StatusCode,
) -> Response {
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();

    match resp.bytes().await {
        Ok(body_bytes) => {
            info!("<< UPSTREAM BODY  {} bytes", body_bytes.len());
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
            error!(%e, "!! READ ERROR failed to read upstream body");
            (
                StatusCode::BAD_GATEWAY,
                [("content-type", "application/json")],
                Json(json!({"error": format!("upstream read: {e}")})),
            )
                .into_response()
        }
    }
}
