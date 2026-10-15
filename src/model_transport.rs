//! Transport diagnostics contain counts and protocol metadata, never prompts,
//! response text, authorization headers, or provider URL credentials.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};

struct Progress {
    phase: &'static str,
    headers_ms: Option<u64>,
    first_byte_ms: Option<u64>,
    first_delta_ms: Option<u64>,
    last_byte_ms: Option<u64>,
    bytes: usize,
    chunks: usize,
    http: Value,
    decoder: Value,
}

pub struct ModelRequestLog {
    root: PathBuf,
    started: Instant,
    identity: Value,
    progress: Mutex<Progress>,
}

impl ModelRequestLog {
    pub fn new(root: &Path, identity: Value, url: &str, body: &Value) -> Self {
        let origin = reqwest::Url::parse(url)
            .ok()
            .map(|url| url.origin().ascii_serialization());
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        let messages = body["messages"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut roles = BTreeMap::<String, usize>::new();
        for message in messages {
            let role = match message["role"].as_str() {
                Some(role @ ("system" | "user" | "assistant" | "tool" | "developer")) => role,
                _ => "other",
            };
            *roles.entry(role.into()).or_default() += 1;
        }
        let log = Self {
            root: root.into(),
            started: Instant::now(),
            identity,
            progress: Mutex::new(Progress {
                phase: "prepared",
                headers_ms: None,
                first_byte_ms: None,
                first_delta_ms: None,
                last_byte_ms: None,
                bytes: 0,
                chunks: 0,
                http: Value::Null,
                decoder: Value::Null,
            }),
        };
        let proxy_env_names = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        ]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .collect::<Vec<_>>();
        log.event("prepared", json!({
            "origin":origin,"endpoint":"chat/completions", "request_bytes":bytes.len(),
            "body_sha256":crate::visual_artifacts::hash(&bytes),
            "message_count":messages.len(),"role_counts":roles,
            "proxy_env_names":proxy_env_names,
            "tool_count":body["tools"].as_array().map_or(0,Vec::len),
            "tail_message_shapes":messages.iter().rev().take(8).map(|message|json!({
                "role":message["role"],
                "content_type":match &message["content"] { Value::Array(_)=>"array",Value::String(_)=>"string",Value::Null=>"null",_=>"other" },
                "tool_call_count":message["tool_calls"].as_array().map_or(0,Vec::len),
                "has_tool_call_id":message["tool_call_id"].is_string(),
            })).collect::<Vec<_>>(),
            "timeout_ms":180_000,"attempt":1,
        }));
        log
    }

    fn elapsed(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn phase(&self, phase: &'static str) {
        self.progress.lock().unwrap().phase = phase;
    }

    pub fn sending(&self) {
        self.phase("awaiting_headers");
        // send() covers DNS, connect, TLS, request upload, and response headers.
        // This event does not assert that the gateway received the request.
        self.event("send_started", json!({}));
    }

    pub fn headers(&self, response: &reqwest::Response) {
        let mut progress = self.progress.lock().unwrap();
        progress.headers_ms = Some(self.elapsed());
        progress.phase = if response.status().is_success() {
            "stream_read"
        } else {
            "http_error_body"
        };
        let mut headers = serde_json::Map::new();
        // No arbitrary response headers; in particular, never cookies or auth.
        for name in [
            "x-request-id",
            "request-id",
            "x-correlation-id",
            "traceparent",
            "cf-ray",
            "content-type",
            "content-length",
            "retry-after",
        ] {
            if let Some(value) = response.headers().get(name).and_then(|v| v.to_str().ok()) {
                headers.insert(
                    name.into(),
                    json!(
                        value
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(160)
                            .collect::<String>()
                    ),
                );
            }
        }
        progress.http = json!({"status":response.status().as_u16(),"version":format!("{:?}",response.version()),
            "remote_addr":response.remote_addr().map(|address|address.to_string()),"headers":headers});
        drop(progress);
        self.event("response_headers", json!({}));
    }

    pub fn chunk(&self, size: usize) {
        if size == 0 {
            return;
        }
        let mut progress = self.progress.lock().unwrap();
        let first = progress.first_byte_ms.is_none();
        let elapsed = self.elapsed();
        progress.first_byte_ms.get_or_insert(elapsed);
        progress.last_byte_ms = Some(elapsed);
        progress.bytes += size;
        progress.chunks += 1;
        drop(progress);
        if first {
            self.event("first_byte", json!({}));
        }
    }

    pub fn delta(&self, delta: &crate::model_stream::Delta) {
        if delta.is_empty() {
            return;
        }
        let mut progress = self.progress.lock().unwrap();
        let first = progress.first_delta_ms.is_none();
        progress.first_delta_ms.get_or_insert(self.elapsed());
        drop(progress);
        if first {
            self.event("first_delta", json!({}));
        }
    }

    pub fn decoded(&self, stats: &Value) {
        // Usage, text, model-generated tool arguments, and error payloads are
        // intentionally excluded from the shared diagnostic file.
        let mut summary = serde_json::Map::new();
        for key in [
            "received_bytes",
            "chunks",
            "data_events",
            "keepalives",
            "first_chunk_ms",
            "first_delta_ms",
            "terminated",
        ] {
            if stats[key].is_number() || stats[key].is_boolean() {
                summary.insert(key.into(), stats[key].clone());
            }
        }
        self.progress.lock().unwrap().decoder = Value::Object(summary);
    }

    pub fn waiting(&self) {
        self.event("waiting", json!({}));
    }

    pub fn eof(&self) {
        self.event("body_eof", json!({}));
    }

    pub fn finish(&self, status: &str, error: Option<&anyhow::Error>) -> Value {
        let mut summary = self.snapshot();
        summary["status"] = json!(status);
        if let Some(error) = error {
            summary["error_chain"] = json!(error_chain(error));
        }
        self.event(
            "finished",
            json!({"status":status,"error_chain":summary["error_chain"]}),
        );
        summary
    }

    fn snapshot(&self) -> Value {
        let progress = self.progress.lock().unwrap();
        json!({
            "phase":progress.phase,"elapsed_ms":self.elapsed(),"headers_ms":progress.headers_ms,
            "first_byte_ms":progress.first_byte_ms,"first_delta_ms":progress.first_delta_ms,
            "last_byte_ms":progress.last_byte_ms,"received_bytes":progress.bytes,
            "chunks":progress.chunks,"http":progress.http,"decoder":progress.decoder,
        })
    }

    fn event(&self, stage: &str, extra: Value) {
        let record = json!({"time":chrono::Utc::now().to_rfc3339(),"event":"model_transport",
            "identity":self.identity,"stage":stage,"transport":self.snapshot(),"detail":extra});
        tracing::info!(target:"model_transport", record=%record,"model request transport");
        if let Err(error) = crate::proxy_log::append_model_transport(&self.root, &record) {
            tracing::warn!(target:"model_transport",kind=?error.kind(),"could not append transport log");
        }
    }
}

// Only typed flags and known reason codes leave the error chain. Raw error
// strings can contain provider bodies, prompt fragments, or credential URLs.
fn error_chain(error: &anyhow::Error) -> Vec<Value> {
    error.chain().take(12).map(|cause| {
        if let Some(error) = cause.downcast_ref::<reqwest::Error>() {
            return json!({"kind":"reqwest","timeout":error.is_timeout(),"connect":error.is_connect(),
                "request":error.is_request(),"body":error.is_body(),"decode":error.is_decode(),
                "status":error.status().map(|s|s.as_u16())});
        }
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            return json!({"kind":"io","io_kind":format!("{:?}",error.kind()),"os_error":error.raw_os_error()});
        }
        if let Some(error) = cause.downcast_ref::<crate::visual_probe::HttpModelFailure>() {
            return json!({"kind":"http_status","status":error.status,"error_body_bytes":error.body.len(),
                "error_body_sha256":crate::visual_artifacts::hash(error.body.as_bytes())});
        }
        if let Some(error) = cause.downcast_ref::<crate::model_stream::StreamError>() {
            return json!({"kind":"stream_decoder","code":error.code});
        }
        let text = cause.to_string().to_ascii_lowercase();
        let reason = if text.contains("connection closed before message completed") { "incomplete_http_message" }
            else if text.contains("connection reset") { "connection_reset" }
            else if text.contains("connection refused") { "connection_refused" }
            else if text.contains("dns") || text.contains("name resolution") { "dns_failure" }
            else if text.contains("tls") || text.contains("certificate") { "tls_failure" }
            else if text.contains("timed out") || text.contains("timeout") { "timeout" }
            else if text.contains("cancelled") { "cancelled" }
            else if text.contains("without completion") { "missing_completion" }
            else if text.contains("truncated or filtered") { "truncated_or_filtered" }
            else { "unclassified" };
        json!({"kind":"source","reason":reason})
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_diagnostic_file_excludes_url_credentials_and_raw_errors() {
        let root = std::env::temp_dir().join(format!(
            "transport-log-{}",
            crate::visual_artifacts::new_id()
        ));
        let log = ModelRequestLog::new(
            &root,
            json!({"request_id":"test"}),
            "https://PRIVATE_USER:PRIVATE_PASSWORD@example.test/PRIVATE_PATH?key=PRIVATE_QUERY",
            &json!({"messages":[{"role":"user","content":"PRIVATE_PROMPT"}]}),
        );
        log.phase("http_error_body");
        log.waiting();
        let error = anyhow::Error::new(crate::visual_probe::HttpModelFailure {
            status: 429,
            body: "PRIVATE_PROVIDER_BODY".into(),
        })
        .context("PRIVATE_ERROR_CONTEXT");
        let summary = log.finish("failed", Some(&error));
        assert_eq!(summary["error_chain"][1]["status"], 429);
        let path = std::fs::read_dir(root.join("logs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let output = std::fs::read_to_string(&path).unwrap();
        for secret in [
            "PRIVATE_USER",
            "PRIVATE_PASSWORD",
            "PRIVATE_PATH",
            "PRIVATE_QUERY",
            "PRIVATE_PROMPT",
            "PRIVATE_PROVIDER_BODY",
            "PRIVATE_ERROR_CONTEXT",
        ] {
            assert!(!output.contains(secret), "{secret} leaked");
        }
        assert!(output.contains("https://example.test"));
        assert!(
            output
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transport_diagnostic_write_failure_does_not_fail_the_request() {
        let root = std::env::temp_dir().join(format!(
            "transport-log-{}",
            crate::visual_artifacts::new_id()
        ));
        std::fs::write(&root, "not a directory").unwrap();
        let log = ModelRequestLog::new(&root, json!({}), "http://example.test", &json!({}));
        log.sending();
        log.phase("completed");
        assert_eq!(log.finish("completed", None)["status"], "completed");
        std::fs::remove_file(root).unwrap();
    }
}
