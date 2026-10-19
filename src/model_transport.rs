//! Transport diagnostics contain counts and protocol metadata, never prompts,
//! response text, authorization headers, or provider URL credentials.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
pub struct TimeoutPolicy {
    pub headers: Duration,
    pub first_output: Duration,
    pub idle: Duration,
}
impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            headers: Duration::from_secs(180),
            first_output: Duration::from_secs(180),
            idle: Duration::from_secs(180),
        }
    }
}

/// Streaming requests have no wall-clock deadline. The progress watchdog owns
/// waiting limits; unrelated HTTP calls retain their existing client policy.
pub fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()?)
}

/// Stable per working window, independent of turn/request/prompt length. A
/// replica router can preserve prefix-cache locality without seeing task IDs.
pub fn affinity(task: &str, scope: &str) -> String {
    crate::visual_artifacts::hash(json!([task, scope]).to_string().as_bytes())
}

#[derive(Debug)]
struct ModelTimeout {
    phase: &'static str,
    limit_ms: u128,
}
impl std::fmt::Display for ModelTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "model {} timeout: no progress within {} ms",
            self.phase, self.limit_ms
        )
    }
}
impl std::error::Error for ModelTimeout {}

/// Decode a model response without emitting execution-agent chat events.
/// Context and vision requests still refresh the same progress watchdog.
pub async fn consume_response(
    response: reqwest::Response,
    log: &ModelRequestLog,
) -> anyhow::Result<crate::model_stream::Completed> {
    use futures::StreamExt;
    let mut decoder = crate::model_stream::ChatStream::new(
        crate::model_stream::Limits::WORKER,
        std::time::Instant::now(),
    );
    let mut bytes = response.bytes_stream();
    while !decoder.received_done() {
        log.phase("stream_read");
        let Some(chunk) = bytes.next().await else {
            log.eof();
            break;
        };
        let chunk = chunk?;
        log.chunk(chunk.len());
        log.phase("stream_decode");
        let delta = decoder.push(&chunk)?;
        log.delta(&delta);
        log.decoded(&decoder.stats());
    }
    log.phase("stream_decode");
    let completed = decoder.finish()?;
    let mut stats = completed.stats.clone();
    stats["terminated"] = json!(completed.terminated);
    log.decoded(&stats);
    Ok(completed)
}

struct Progress {
    phase: &'static str,
    headers_ms: Option<u64>,
    first_byte_ms: Option<u64>,
    first_delta_ms: Option<u64>,
    last_byte_ms: Option<u64>,
    last_progress_ms: Option<u64>,
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
                last_progress_ms: None,
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
            "timeout_policy":{"connect_ms":30_000,"headers_ms":180_000,"first_output_ms":180_000,"idle_ms":180_000,"total_ms":null},"attempt":1,
        }));
        log
    }

    fn elapsed(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn phase(&self, phase: &'static str) {
        self.progress.lock().unwrap().phase = phase;
    }

    fn deadline(&self, policy: TimeoutPolicy) -> (Instant, &'static str, Duration) {
        let p = self.progress.lock().unwrap();
        let (since, phase, limit) = match (p.headers_ms, p.last_progress_ms) {
            (None, _) => (0, "awaiting_headers", policy.headers),
            (Some(headers), None) => (headers, "first_output", policy.first_output),
            (Some(_), Some(last)) => (last, "output_idle", policy.idle),
        };
        (
            self.started + Duration::from_millis(since) + limit,
            phase,
            limit,
        )
    }

    pub async fn watch<T>(
        &self,
        pending: impl std::future::Future<Output = anyhow::Result<T>>,
        cancel: &CancellationToken,
        policy: TimeoutPolicy,
    ) -> anyhow::Result<T> {
        tokio::pin!(pending);
        let mut heartbeat = tokio::time::interval_at(
            Instant::now() + Duration::from_secs(15),
            Duration::from_secs(15),
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let (deadline, _, _) = self.deadline(policy);
            tokio::select! {
                biased;
                _=cancel.cancelled()=>return Err(anyhow::anyhow!("cancelled")),
                result=&mut pending=>return result,
                _=tokio::time::sleep_until(deadline)=>{
                    // Polling the stream may have advanced progress after the
                    // sleep was created. Never expire that newer deadline.
                    let (current,phase,limit)=self.deadline(policy);
                    if Instant::now()>=current {
                        self.phase(phase);
                        return Err(ModelTimeout {phase,limit_ms:limit.as_millis()}.into());
                    }
                },
                _=heartbeat.tick()=>self.waiting(),
            }
        }
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
        let elapsed = self.elapsed();
        progress.first_delta_ms.get_or_insert(elapsed);
        progress.last_progress_ms = Some(elapsed);
        drop(progress);
        if first {
            self.event("first_delta", json!({}));
        }
    }

    pub fn decoded(&self, stats: &Value) {
        // Raw text, model-generated arguments, usage and error payloads stay
        // out of shared diagnostics. Numeric channel counts identify progress.
        let mut summary = serde_json::Map::new();
        for key in [
            "received_bytes",
            "chunks",
            "data_events",
            "keepalives",
            "first_chunk_ms",
            "first_delta_ms",
            "first_tool_delta_ms",
            "text_chars",
            "reasoning_chars",
            "tool_calls",
            "tool_argument_bytes",
            "received_done",
            "terminated",
        ] {
            if stats[key].is_number() || stats[key].is_boolean() {
                summary.insert(key.into(), stats[key].clone());
            }
        }
        let mut progress = self.progress.lock().unwrap();
        // Plain JSON fallback has no decodable token deltas until its closing
        // brace. Count actual body growth there; SSE comments never reset idle.
        if stats["response_format"] == "json"
            && stats["received_bytes"].as_u64().unwrap_or(0)
                > progress.decoder["received_bytes"].as_u64().unwrap_or(0)
        {
            progress.last_progress_ms = Some(self.elapsed());
        }
        progress.decoder = Value::Object(summary);
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
            "last_progress_ms":progress.last_progress_ms,
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
        if let Some(error)=cause.downcast_ref::<ModelTimeout>() {
            return json!({"kind":"model_timeout","phase":error.phase,"limit_ms":error.limit_ms});
        }
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
    fn replica_affinity_is_stable_and_separates_task_and_window() {
        let main = affinity("private-task", "main");
        assert_eq!(main, affinity("private-task", "main"));
        assert_ne!(main, affinity("other-task", "main"));
        assert_ne!(main, affinity("private-task", "delegate:child"));
        assert_ne!(main, affinity("private-task", "context:material"));
        assert_ne!(affinity("a:b", "c"), affinity("a", "b:c"));
        assert_eq!(main.len(), 64);
        assert!(main.bytes().all(|c| c.is_ascii_hexdigit()));
    }

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
    fn watchdog_log() -> (std::path::PathBuf, ModelRequestLog) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "model-watchdog-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let log = ModelRequestLog::new(&root, json!({}), "http://model.test/v1", &json!({}));
        (root, log)
    }

    #[tokio::test(start_paused = true)]
    async fn text_reasoning_and_tool_output_continue_past_three_and_five_minutes() {
        let (root, log) = watchdog_log();
        log.progress.lock().unwrap().headers_ms = Some(0);
        let started = Instant::now();
        let cancel = CancellationToken::new();
        log.watch(
            async {
                for delta in [
                    crate::model_stream::Delta {
                        text: "answer".into(),
                        ..Default::default()
                    },
                    crate::model_stream::Delta {
                        reasoning: "thinking".into(),
                        ..Default::default()
                    },
                    crate::model_stream::Delta {
                        tool_call: true,
                        ..Default::default()
                    },
                ] {
                    tokio::time::sleep(Duration::from_secs(90)).await;
                    log.delta(&delta);
                }
                tokio::time::sleep(Duration::from_secs(90)).await;
                Ok(())
            },
            &cancel,
            TimeoutPolicy::default(),
        )
        .await
        .unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(360));
        assert_eq!(log.snapshot()["last_progress_ms"], 270_000);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_bytes_do_not_extend_first_output_or_output_idle_deadlines() {
        for has_output in [false, true] {
            let (root, log) = watchdog_log();
            log.progress.lock().unwrap().headers_ms = Some(0);
            if has_output {
                log.delta(&crate::model_stream::Delta {
                    text: "first".into(),
                    ..Default::default()
                });
            }
            let started = Instant::now();
            let result = log
                .watch(
                    async {
                        for _ in 0..6 {
                            tokio::time::sleep(Duration::from_secs(45)).await;
                            log.chunk(12); // SSE comments/empty events, no model delta.
                        }
                        Ok(())
                    },
                    &CancellationToken::new(),
                    TimeoutPolicy::default(),
                )
                .await;
            let error = result.unwrap_err();
            assert_eq!(started.elapsed(), Duration::from_secs(180));
            let expected = if has_output {
                "output_idle"
            } else {
                "first_output"
            };
            assert_eq!(
                error.downcast_ref::<ModelTimeout>().unwrap().phase,
                expected
            );
            let summary = log.finish("failed", Some(&error));
            assert_eq!(summary["error_chain"][0]["kind"], "model_timeout");
            assert_eq!(summary["error_chain"][0]["phase"], expected);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn missing_headers_times_out_and_cancellation_interrupts_active_output() {
        let (root, log) = watchdog_log();
        let error = log
            .watch(
                std::future::pending::<anyhow::Result<()>>(),
                &CancellationToken::new(),
                TimeoutPolicy::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ModelTimeout>().unwrap().phase,
            "awaiting_headers"
        );
        std::fs::remove_dir_all(root).unwrap();
        let (root, log) = watchdog_log();
        log.progress.lock().unwrap().headers_ms = Some(0);
        let cancel = CancellationToken::new();
        let stopping = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            stopping.cancel();
        });
        let started = Instant::now();
        let result = log
            .watch(
                async {
                    loop {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        log.delta(&crate::model_stream::Delta {
                            tool_call: true,
                            ..Default::default()
                        });
                    }
                    #[allow(unreachable_code)]
                    Ok(())
                },
                &cancel,
                TimeoutPolicy::default(),
            )
            .await;
        assert_eq!(result.unwrap_err().to_string(), "cancelled");
        assert_eq!(started.elapsed(), Duration::from_secs(5));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn plain_json_body_growth_extends_idle_without_inventing_token_deltas() {
        let (root, log) = watchdog_log();
        log.progress.lock().unwrap().headers_ms = Some(0);
        log.watch(
            async {
                for count in 1..=4 {
                    tokio::time::sleep(Duration::from_secs(90)).await;
                    log.decoded(&json!({"response_format":"json","received_bytes":count*128}));
                }
                Ok(())
            },
            &CancellationToken::new(),
            TimeoutPolicy::default(),
        )
        .await
        .unwrap();
        assert!(log.snapshot()["first_delta_ms"].is_null());
        assert_eq!(log.snapshot()["last_progress_ms"], 360_000);
        std::fs::remove_dir_all(root).unwrap();
    }
}
