//! Local-only HTTP reachability probes, independent of Agent-managed processes.

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use reqwest::{Url, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::lookup_host;

pub const TOOLS: &[&str] = &["http_probe"];
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
pub const REUSE_WINDOW_MS: u64 = 30_000;
const MAX_BODY_SUMMARY_BYTES: usize = 2_048;

#[derive(Debug, Deserialize)]
struct ProbeRequest {
    url: String,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    reason: Option<String>,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[derive(Debug)]
struct ProbeFailure {
    kind: &'static str,
    message: String,
}

struct ProbeResponse {
    status: u16,
    redirect_target: Option<String>,
    body_summary: Option<String>,
    body_summary_truncated: bool,
}

pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

pub fn check_key(args: &Value) -> String {
    let raw = args["url"].as_str().unwrap_or("").trim();
    let normalized = raw.split_once('#').map(|(url, _)|url).unwrap_or(raw);
    format!("http-probe:{normalized}")
}

pub async fn execute(args: &Value) -> Result<Value> {
    let request: ProbeRequest = serde_json::from_value(args.clone())?;
    anyhow::ensure!(request.url.len() <= 4_096, "url must be at most 4096 bytes");
    anyhow::ensure!(request.reason.as_ref().is_none_or(|reason| reason.chars().count() <= 300), "reason must be at most 300 characters");
    let started = Instant::now();
    let sampled_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let timeout_ms = request.timeout_ms.clamp(100, MAX_TIMEOUT_MS);
    let result = probe_url(&request.url, Duration::from_millis(timeout_ms), started).await;
    let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;

    let mut value = match result {
        Ok(response) => json!({
            "sampled_at": sampled_at,
            "url": request.url,
            "elapsed_ms": elapsed_ms,
            "reachable": true,
            "http_status": response.status,
            "error_kind": Value::Null,
            "error_message": Value::Null,
            "redirect_target": response.redirect_target,
            "body_summary": response.body_summary,
            "body_summary_truncated": response.body_summary_truncated,
        }),
        Err(error) => json!({
            "sampled_at": sampled_at,
            "url": request.url,
            "elapsed_ms": elapsed_ms,
            "reachable": false,
            "http_status": Value::Null,
            "error_kind": error.kind,
            "error_message": error.message,
            "redirect_target": Value::Null,
            "body_summary": Value::Null,
            "body_summary_truncated": false,
        }),
    };
    value["check_key"] = json!(check_key(args));
    value["check_passed"] = json!(value["reachable"] == true
        && value["http_status"].as_u64().is_some_and(|status| (200..300).contains(&status)));
    value["timeout_ms"] = json!(timeout_ms);
    value["reuse_window_ms"] = json!(REUSE_WINDOW_MS);
    if let Some(reason) = request.reason.filter(|reason| !reason.trim().is_empty()) {
        value["reason"] = json!(reason.trim());
    }
    Ok(value)
}

async fn probe_url(url_text: &str, timeout: Duration, started: Instant) -> std::result::Result<ProbeResponse, ProbeFailure> {
    let mut url = Url::parse(url_text).map_err(|error| ProbeFailure {
        kind: "invalid_url",
        message: format!("The URL could not be parsed: {error}"),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ProbeFailure {
            kind: "unsupported_scheme",
            message: "Only local HTTP and HTTPS URLs can be probed.".to_owned(),
        });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ProbeFailure {
            kind: "credentials_not_allowed",
            message: "Credentials in the URL are not allowed.".to_owned(),
        });
    }
    url.set_fragment(None);
    let host = url.host_str().ok_or_else(|| ProbeFailure {
        kind: "invalid_url",
        message: "The URL does not contain a host.".to_owned(),
    })?.to_owned();
    let port = url.port_or_known_default().ok_or_else(|| ProbeFailure {
        kind: "invalid_url",
        message: "The URL does not specify a usable HTTP port.".to_owned(),
    })?;

    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(timeout);
    // Url serializes IPv6 hosts with brackets; strip them before parsing an IP literal.
    let host_ip = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host_ip.parse::<IpAddr>() {
        if !ip.is_loopback() {
            return Err(ProbeFailure {
                kind: "non_local_address",
                message: "Only loopback IP addresses are allowed.".to_owned(),
            });
        }
    } else {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(timeout_failure(timeout));
        }
        let resolved = tokio::time::timeout(remaining, lookup_host((host.as_str(), port)))
            .await
            .map_err(|_| timeout_failure(timeout))?
            .map_err(|error| ProbeFailure {
                kind: "dns",
                message: format!("The local hostname '{host}' could not be resolved: {error}"),
            })?
            .collect::<Vec<SocketAddr>>();
        if resolved.is_empty() {
            return Err(ProbeFailure {
                kind: "dns",
                message: format!("The local hostname '{host}' resolved to no addresses."),
            });
        }
        if resolved.iter().any(|address| !address.ip().is_loopback()) {
            return Err(ProbeFailure {
                kind: "non_local_address",
                message: format!("The hostname '{host}' resolved to a non-loopback address; the request was blocked."),
            });
        }
        builder = builder.resolve_to_addrs(&host, &resolved);
    }

    let client = builder.build().map_err(|error| ProbeFailure {
        kind: "request",
        message: format!("Could not prepare the local HTTP request: {error}"),
    })?;
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(timeout_failure(timeout));
    }
    let mut response = tokio::time::timeout(remaining, client.get(url.clone()).send())
        .await
        .map_err(|_| timeout_failure(timeout))?
        .map_err(|error| request_failure(&error))?;
    let status = response.status().as_u16();
    let redirect_target = if response.status().is_redirection() {
        response.headers().get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(|location| url.join(location).map(|target| target.to_string()).unwrap_or_else(|_| location.to_owned()))
    } else {
        None
    };
    let content_length = response.content_length();
    let textual = response.headers().get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_textual_content_type);
    let mut body = Vec::new();
    let mut body_finished = false;
    if textual {
        while body.len() < MAX_BODY_SUMMARY_BYTES {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() { break; }
            match tokio::time::timeout(remaining.min(Duration::from_millis(250)), response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    let take = (MAX_BODY_SUMMARY_BYTES - body.len()).min(chunk.len());
                    body.extend_from_slice(&chunk[..take]);
                    if take < chunk.len() { break; }
                }
                Ok(Ok(None)) => { body_finished = true; break; }
                Ok(Err(_)) | Err(_) => break,
            }
        }
    }
    let body_summary = (!body.is_empty()).then(|| String::from_utf8_lossy(&body).into_owned());
    let body_summary_truncated = body_summary.is_some()
        && (!body_finished || content_length.is_some_and(|length| length > body.len() as u64));
    Ok(ProbeResponse { status, redirect_target, body_summary, body_summary_truncated })
}

fn is_textual_content_type(value: &str) -> bool {
    let value = value.split(';').next().unwrap_or(value).trim().to_ascii_lowercase();
    value.starts_with("text/")
        || value == "application/json"
        || value.ends_with("+json")
        || value.contains("xml")
        || value.contains("javascript")
}

fn timeout_failure(timeout: Duration) -> ProbeFailure {
    ProbeFailure {
        kind: "timeout",
        message: format!("The local HTTP probe timed out after {} ms; no HTTP response was received.", timeout.as_millis()),
    }
}

fn request_failure(error: &reqwest::Error) -> ProbeFailure {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    let mut io_kind = None;
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            io_kind = Some(io.kind());
        }
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = std::error::Error::source(cause);
    }
    let lower = message.to_ascii_lowercase();
    let kind = if error.is_timeout() {
        "timeout"
    } else if io_kind == Some(std::io::ErrorKind::ConnectionRefused)
        || lower.contains("connection refused")
        || lower.contains("actively refused")
    {
        "connection_refused"
    } else if lower.contains("dns") || lower.contains("resolve") || lower.contains("name or service not known") {
        "dns"
    } else if lower.contains("tls") || lower.contains("certificate") || lower.contains("rustls") || lower.contains("handshake") {
        "tls"
    } else if error.is_connect() {
        "connection"
    } else {
        "request"
    };
    let diagnostic = match kind {
        "timeout" => "The local HTTP probe timed out before receiving an HTTP response.".to_owned(),
        "connection_refused" => "The local endpoint refused the connection; no HTTP response was received.".to_owned(),
        "dns" => format!("Local hostname resolution failed: {message}"),
        "tls" => format!("The local HTTPS handshake failed: {message}"),
        _ => format!("The local HTTP request failed before receiving an HTTP response: {message}"),
    };
    ProbeFailure { kind, message: diagnostic }
}

pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name":"http_probe",
        "description":"Probe a local HTTP(S) URL directly from the Rust host, independent of Agent-managed process state. Only loopback addresses are allowed; proxies are disabled and redirects are not followed. A 3xx Location is reported in redirect_target when available. Any received HTTP response sets reachable=true and reports its actual http_status (including 404); connection failures use http_status=null plus error_kind/error_message. check_passed is true only for HTTP 2xx. sampled_at is UTC RFC3339 and elapsed_ms is host-measured. reuse_window_ms is 30000: reuse a sample within that age for the same URL while service state is unchanged; an eligible repeat returns reused=true without a network request. Probe again only if state may have changed or the old sample cannot answer the new question; explain a necessary repeat in reason. A short text response summary is limited to 2048 bytes.",
        "inputSchema":{
            "type":"object",
            "required":["url"],
            "properties":{
                "url":{"type":"string","maxLength":4096,"description":"Local HTTP(S) URL using a loopback address or a hostname that resolves exclusively to loopback addresses."},
                "timeout_ms":{"type":"integer","minimum":100,"maximum":30000,"default":5000},
                "reason":{"type":"string","maxLength":300,"description":"When rechecking a URL that already has a recent result, state why its status may have changed or why that result is insufficient."}
            }
        }
    })]
}
