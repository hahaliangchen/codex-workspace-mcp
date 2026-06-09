use std::sync::{Arc, Mutex};
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
use tracing::error;

use crate::format_translate;

pub async fn forward_to_upstream(
    client: &Client,
    upstream_url: &str,
    api_key: &str,
    body: &Value,
    is_stream: bool,
    client_model: &str,
    log: Arc<Mutex<std::fs::File>>,
) -> Response {
    crate::ai_proxy::log_write(
        &*log,
        None,
        None,
        None,
        &format!(
            ">> UPSTREAM REQ  model={}  url={}  stream={}  body={}",
            client_model,
            upstream_url,
            is_stream,
            crate::ai_proxy::fmt_body(serde_json::to_string(body).unwrap_or_default().as_bytes())
        ),
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
            crate::ai_proxy::log_write(
                &*log,
                None,
                None,
                None,
                &format!(
                    "<< UPSTREAM RESP  status={}  model={}",
                    status.as_u16(),
                    client_model
                ),
            );

            if is_stream {
                forward_chat_stream(resp, status, client_model, log).await
            } else {
                forward_non_streaming_body(resp, status, log).await
            }
        }
        Err(e) => {
            crate::ai_proxy::log_write(
                &*log,
                None,
                None,
                None,
                &format!("!! CONNECT ERROR  {}", e),
            );
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

pub async fn forward_raw_responses(
    client: &Client,
    upstream_url: &str,
    api_key: &str,
    body: &Value,
    is_stream: bool,
    client_model: &str,
    log: Arc<Mutex<std::fs::File>>,
    db: Arc<Mutex<rusqlite::Connection>>,
) -> Response {
    crate::proxy_log::write(
        &*log,
        Some(&*db),
        Some("REQ_OUT"),
        Some("user"),
        &format!(
            ">> RAW CODEX UPSTREAM REQ  model={}  url={}  stream={}  body={}",
            client_model,
            upstream_url,
            is_stream,
            crate::ai_proxy::fmt_body(serde_json::to_string(body).unwrap_or_default().as_bytes())
        ),
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
            crate::proxy_log::write(
                &*log,
                Some(&*db),
                Some("RESP_IN"),
                Some("assistant"),
                &format!(
                    "<< RAW CODEX UPSTREAM RESP  status={}  model={}",
                    status.as_u16(),
                    client_model
                ),
            );

            if !status.is_success() {
                let body_bytes = resp.bytes().await.unwrap_or_default();
                crate::proxy_log::write(
                    &*log,
                    Some(&*db),
                    Some("ERROR"),
                    Some("proxy"),
                    &format!(
                        "!! RAW CODEX UPSTREAM ERROR RESP  status={}  body={}",
                        status.as_u16(),
                        String::from_utf8_lossy(&body_bytes)
                    ),
                );
                return Response::builder()
                    .status(status)
                    .body(Body::from(body_bytes))
                    .unwrap();
            }

            if is_stream {
                forward_raw_stream(resp, status, log, db).await
            } else {
                forward_raw_body(resp, status, log, db).await
            }
        }
        Err(e) => {
            crate::proxy_log::write(
                &*log,
                Some(&*db),
                Some("ERROR"),
                Some("proxy"),
                &format!("!! RAW CONNECT ERROR  {}", e),
            );
            error!(%e, "raw upstream request failed");
            (
                StatusCode::BAD_GATEWAY,
                [("content-type", "application/json")],
                Json(json!({"error": format!("upstream: {e}")})),
            )
                .into_response()
        }
    }
}

async fn forward_raw_stream(
    resp: reqwest::Response,
    status: reqwest::StatusCode,
    log: Arc<Mutex<std::fs::File>>,
    db: Arc<Mutex<rusqlite::Connection>>,
) -> Response {
    let stream = resp.bytes_stream();
    let (tx, rx) = mpsc::channel::<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>>(32);
    let log_clone = log.clone();
    let db_clone = db.clone();

    tokio::spawn(async move {
        use futures::StreamExt;
        futures::pin_mut!(stream);

        crate::proxy_log::write(
            &*log_clone,
            Some(&*db_clone),
            Some("STREAM_START"),
            Some("proxy"),
            ">> SPAWNED raw responses stream handler",
        );

        while let Some(result) = stream.next().await {
            match result {
                Ok(bytes) => {
                    let item: Result<Bytes, Box<dyn std::error::Error + Send + Sync>> = Ok(bytes);
                    if tx.send(item).await.is_err() {
                        crate::proxy_log::write(
                            &*log_clone,
                            Some(&*db_clone),
                            Some("ERROR"),
                            Some("proxy"),
                            "!! CLIENT DISCONNECTED during raw stream",
                        );
                        break;
                    }
                }
                Err(e) => {
                    crate::proxy_log::write(
                        &*log_clone,
                        Some(&*db_clone),
                        Some("ERROR"),
                        Some("proxy"),
                        &format!("!! RAW UPSTREAM STREAM ERROR  {}", e),
                    );
                    let _ = tx
                        .send(Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>))
                        .await;
                    break;
                }
            }
        }
        crate::proxy_log::write(
            &*log_clone,
            Some(&*db_clone),
            Some("STREAM_FINISHED"),
            Some("proxy"),
            ">> FINISHED raw responses stream handler",
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
            error!(%e, "failed to build raw Responses stream response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })
}

async fn forward_raw_body(
    resp: reqwest::Response,
    status: reqwest::StatusCode,
    log: Arc<Mutex<std::fs::File>>,
    db: Arc<Mutex<rusqlite::Connection>>,
) -> Response {
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();

    match resp.bytes().await {
        Ok(body_bytes) => {
            crate::proxy_log::write(
                &*log,
                Some(&*db),
                Some("RESP_IN"),
                Some("assistant"),
                &format!("<< RAW CODEX UPSTREAM BODY  {} bytes", body_bytes.len()),
            );
            Response::builder()
                .status(status)
                .header("content-type", content_type)
                .body(Body::from(body_bytes))
                .unwrap_or_else(|e| {
                    error!(%e, "failed to build raw response");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                })
        }
        Err(e) => {
            crate::proxy_log::write(
                &*log,
                Some(&*db),
                Some("ERROR"),
                Some("proxy"),
                &format!("!! RAW READ ERROR  {}", e),
            );
            error!(%e, "failed to read raw upstream body");
            (
                StatusCode::BAD_GATEWAY,
                [("content-type", "application/json")],
                Json(json!({"error": format!("upstream read: {e}")})),
            )
                .into_response()
        }
    }
}

async fn forward_chat_stream(
    resp: reqwest::Response,
    status: reqwest::StatusCode,
    client_model: &str,
    log: Arc<Mutex<std::fs::File>>,
) -> Response {
    let log2 = log.clone();
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
                    crate::ai_proxy::log_write(
                        &*log2,
                        None,
                        None,
                        None,
                        &format!("!! STREAM ERROR  {}", e),
                    );
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
    log: Arc<Mutex<std::fs::File>>,
) -> Response {
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();

    match resp.bytes().await {
        Ok(body_bytes) => {
            crate::ai_proxy::log_write(
                &*log,
                None,
                None,
                None,
                &format!(
                    "<< UPSTREAM BODY  {} bytes  {}",
                    body_bytes.len(),
                    crate::ai_proxy::fmt_body(&body_bytes)
                ),
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
            crate::ai_proxy::log_write(&*log, None, None, None, &format!("!! READ ERROR  {}", e));
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
