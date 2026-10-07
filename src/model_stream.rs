//! Incremental assembly of an OpenAI Chat Completions response.
//!
//! The accumulator is transport-only: it never emits events or applies tool
//! calls. Worker and Organizer each consume its deltas through their own
//! output channels and act only on the completed message.
use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::{Value, json};

#[derive(Debug)]
pub struct StreamError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for StreamError {}

fn fail<T>(code: &'static str, message: impl Into<String>) -> anyhow::Result<T> {
    Err(anyhow::Error::new(StreamError { code, message: message.into() }))
}

pub fn stream_error_code(error: &anyhow::Error) -> Option<&'static str> {
    error.downcast_ref::<StreamError>().map(|error| error.code)
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bytes received for one response, SSE framing included.
    pub max_total_bytes: usize,
    /// One unterminated SSE line, or the whole body of a plain JSON response.
    pub max_line_bytes: usize,
    /// Accumulated arguments of one tool call.
    pub max_tool_argument_bytes: usize,
    /// Reasoning text is counted either way; only the Worker transcript keeps it.
    pub retain_reasoning: bool,
}

impl Limits {
    pub const WORKER: Limits = Limits {
        max_total_bytes: 64 * 1024 * 1024,
        max_line_bytes: 16 * 1024 * 1024,
        max_tool_argument_bytes: 16 * 1024 * 1024,
        retain_reasoning: true,
    };
}

/// Text produced by one `push`, in arrival order per channel.
#[derive(Debug, Default)]
pub struct Delta {
    pub text: String,
    pub reasoning: String,
    pub tool_call: bool,
}

impl Delta {
    pub fn is_empty(&self) -> bool { self.text.is_empty() && self.reasoning.is_empty() && !self.tool_call }
}

#[derive(Clone, Copy, PartialEq)]
enum Format { Unknown, Sse, Json }

#[derive(Default)]
struct PartialTool {
    id: String,
    name: String,
    arguments: String,
}

pub struct Completed {
    pub message: Value,
    pub stats: Value,
    /// The provider signalled the end of the response (`[DONE]`, a
    /// `finish_reason`, or a complete JSON body). A bare EOF is not a terminal.
    pub terminated: bool,
}

pub struct ChatStream {
    limits: Limits,
    started: Instant,
    format: Format,
    pending: Vec<u8>,
    body: Vec<u8>,
    data: Option<String>,
    event_name: String,
    text: String,
    reasoning: String,
    reasoning_chars: usize,
    tools: BTreeMap<usize, PartialTool>,
    usage: Value,
    finish_reason: Value,
    received_done: bool,
    received_bytes: usize,
    chunks: usize,
    data_events: usize,
    keepalives: usize,
    first_chunk_ms: Option<u64>,
    first_delta_ms: Option<u64>,
    first_tool_delta_ms: Option<u64>,
}

impl ChatStream {
    /// `started` is the request start, so every recorded time shares one origin.
    pub fn new(limits: Limits, started: Instant) -> Self {
        Self {
            limits, started, format: Format::Unknown, pending: Vec::new(), body: Vec::new(), data: None,
            event_name: String::new(), text: String::new(), reasoning: String::new(), reasoning_chars: 0,
            tools: BTreeMap::new(), usage: Value::Null, finish_reason: Value::Null, received_done: false,
            received_bytes: 0, chunks: 0, data_events: 0, keepalives: 0,
            first_chunk_ms: None, first_delta_ms: None, first_tool_delta_ms: None,
        }
    }

    fn elapsed_ms(&self) -> u64 { self.started.elapsed().as_millis() as u64 }

    pub fn received_done(&self) -> bool { self.received_done }

    pub fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Delta> {
        let mut delta = Delta::default();
        if chunk.is_empty() { return Ok(delta); }
        self.chunks += 1;
        if self.first_chunk_ms.is_none() { self.first_chunk_ms = Some(self.elapsed_ms()); }
        self.received_bytes += chunk.len();
        if self.received_bytes > self.limits.max_total_bytes {
            return fail("RESPONSE_TOO_LARGE", format!("model response exceeded {} bytes", self.limits.max_total_bytes));
        }
        if self.format == Format::Unknown {
            match chunk.iter().find(|byte| !byte.is_ascii_whitespace()) {
                Some(b'{') => self.format = Format::Json,
                Some(_) => self.format = Format::Sse,
                None => {}
            }
        }
        if self.format == Format::Json {
            self.body.extend_from_slice(chunk);
            if self.body.len() > self.limits.max_line_bytes {
                return fail("RESPONSE_TOO_LARGE", format!("JSON model response exceeded {} bytes", self.limits.max_line_bytes));
            }
            return Ok(delta);
        }
        self.pending.extend_from_slice(chunk);
        let mut consumed = 0;
        while let Some(offset) = self.pending[consumed..].iter().position(|byte| *byte == b'\n') {
            let end = consumed + offset;
            let line = self.pending[consumed..end].to_vec();
            consumed = end + 1;
            self.line(&line, &mut delta)?;
        }
        self.pending.drain(..consumed);
        if self.pending.len() > self.limits.max_line_bytes {
            return fail("LINE_TOO_LARGE", format!("an SSE line exceeded {} bytes", self.limits.max_line_bytes));
        }
        Ok(delta)
    }

    fn line(&mut self, line: &[u8], delta: &mut Delta) -> anyhow::Result<()> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // A complete line never splits a UTF-8 sequence, so decoding is exact.
        let Ok(line) = std::str::from_utf8(line) else {
            return fail("INVALID_UTF8", "model stream contained invalid UTF-8");
        };
        if line.is_empty() { return self.dispatch(delta); }
        if line.starts_with(':') { self.keepalives += 1; return Ok(()); }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                // Some providers omit the blank separator between events.
                let previous_complete = self.data.as_deref().is_some_and(|data| data == "[DONE]"
                    || serde_json::from_str::<serde::de::IgnoredAny>(data).is_ok());
                if previous_complete { self.dispatch(delta)?; }
                match self.data.as_mut() {
                    Some(data) => { data.push('\n'); data.push_str(value); },
                    None => self.data = Some(value.to_owned()),
                }
            },
            "event" => self.event_name = value.to_owned(),
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, delta: &mut Delta) -> anyhow::Result<()> {
        let event_name = std::mem::take(&mut self.event_name);
        let Some(data) = self.data.take() else { return Ok(()); };
        if data.trim().is_empty() { return Ok(()); }
        self.data_events += 1;
        if data.trim() == "[DONE]" { self.received_done = true; return Ok(()); }
        if self.received_done { return Ok(()); }
        let event: Value = match serde_json::from_str(&data) {
            Ok(event) => event,
            Err(error) => return fail("MALFORMED_EVENT", format!("model stream event is not JSON: {error}")),
        };
        if let Some(error) = event.get("error").filter(|error| !error.is_null()) {
            let message = error.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| error.to_string());
            return fail("UPSTREAM_ERROR", message);
        }
        if event_name == "error" { return fail("UPSTREAM_ERROR", data); }
        if let Some(usage) = event.get("usage").filter(|usage| !usage.is_null()) { self.usage = usage.clone(); }
        if let Some(reason) = event.pointer("/choices/0/finish_reason").filter(|reason| !reason.is_null()) {
            self.finish_reason = reason.clone();
        }
        let Some(choice_delta) = event.pointer("/choices/0/delta") else { return Ok(()); };
        let mut model_delta = false;
        if let Some(piece) = choice_delta.get("content").and_then(Value::as_str).filter(|piece| !piece.is_empty()) {
            self.text.push_str(piece);
            delta.text.push_str(piece);
            model_delta = true;
        }
        let reasoning = choice_delta.get("reasoning_content").and_then(Value::as_str)
            .or_else(|| choice_delta.get("reasoning").and_then(Value::as_str))
            .filter(|piece| !piece.is_empty());
        if let Some(piece) = reasoning {
            self.reasoning_chars += piece.chars().count();
            if self.limits.retain_reasoning { self.reasoning.push_str(piece); }
            delta.reasoning.push_str(piece);
            model_delta = true;
        }
        for call in choice_delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let entry = self.tools.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) { entry.id = id.to_owned(); }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()) {
                    // Some providers repeat the full name on every fragment.
                    if entry.name != name { entry.name.push_str(name); }
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    entry.arguments.push_str(arguments);
                    if entry.arguments.len() > self.limits.max_tool_argument_bytes {
                        return fail("ARGUMENTS_TOO_LARGE", format!("tool call arguments exceeded {} bytes", self.limits.max_tool_argument_bytes));
                    }
                }
            }
            delta.tool_call = true;
            model_delta = true;
            if self.first_tool_delta_ms.is_none() { self.first_tool_delta_ms = Some(self.elapsed_ms()); }
        }
        if model_delta && self.first_delta_ms.is_none() { self.first_delta_ms = Some(self.elapsed_ms()); }
        Ok(())
    }

    fn format_name(&self) -> &'static str {
        match self.format { Format::Json => "json", Format::Sse => "sse", Format::Unknown => "empty" }
    }

    pub fn tool_argument_bytes(&self) -> usize { self.tools.values().map(|tool| tool.arguments.len()).sum() }

    /// Statistics so far; also saved for a cancelled or timed-out request.
    pub fn stats(&self) -> Value {
        json!({"response_format":self.format_name(),"finish_reason":self.finish_reason,"received_done":self.received_done,
            "usage":self.usage,"received_bytes":self.received_bytes,"chunks":self.chunks,"data_events":self.data_events,
            "keepalives":self.keepalives,"first_chunk_ms":self.first_chunk_ms,"first_delta_ms":self.first_delta_ms,
            "first_tool_delta_ms":self.first_tool_delta_ms,"text_chars":self.text.chars().count(),
            "reasoning_chars":self.reasoning_chars,"tool_calls":self.tools.len(),"tool_argument_bytes":self.tool_argument_bytes()})
    }

    pub fn finish(mut self) -> anyhow::Result<Completed> {
        let complete_ms = self.elapsed_ms();
        match self.format {
            Format::Unknown => fail("INCOMPLETE_STREAM", "model response ended without any data"),
            Format::Json => {
                let body = std::mem::take(&mut self.body);
                let value: Value = match serde_json::from_slice(&body) {
                    Ok(value) => value,
                    Err(error) => return fail("INVALID_JSON_RESPONSE", format!("model response is neither SSE nor complete JSON: {error}")),
                };
                if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
                    let message = error.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| error.to_string());
                    return fail("UPSTREAM_ERROR", message);
                }
                let Some(message) = value.pointer("/choices/0/message").cloned() else {
                    return fail("NO_MESSAGE", "model response has no choices[0].message");
                };
                self.usage = value.get("usage").cloned().unwrap_or(Value::Null);
                self.finish_reason = value.pointer("/choices/0/finish_reason").cloned().unwrap_or(Value::Null);
                let mut stats = self.stats();
                stats["first_delta_ms"] = Value::Null;
                stats["first_tool_delta_ms"] = Value::Null;
                stats["tool_calls"] = json!(message["tool_calls"].as_array().map_or(0, Vec::len));
                stats["response_complete_ms"] = json!(complete_ms);
                Ok(Completed { message, stats, terminated: true })
            },
            Format::Sse => {
                let mut delta = Delta::default();
                if !self.pending.is_empty() {
                    let line = std::mem::take(&mut self.pending);
                    self.line(&line, &mut delta)?;
                }
                self.dispatch(&mut delta)?;
                let mut tool_calls = Vec::new();
                for (index, tool) in &self.tools {
                    if tool.name.is_empty() { continue; }
                    let id = if tool.id.is_empty() { format!("call_{}_{index}", crate::agent_service::uuid_like()) } else { tool.id.clone() };
                    let arguments = if tool.arguments.is_empty() { "{}".to_owned() } else { tool.arguments.clone() };
                    tool_calls.push(json!({"id":id,"type":"function","function":{"name":tool.name,"arguments":arguments}}));
                }
                let mut message = json!({"role":"assistant","content":if self.text.is_empty() {Value::Null} else {json!(self.text)}});
                if !self.reasoning.is_empty() { message["reasoning_content"] = json!(self.reasoning); }
                if !tool_calls.is_empty() { message["tool_calls"] = Value::Array(tool_calls); }
                let terminated = self.received_done || !self.finish_reason.is_null();
                let mut stats = self.stats();
                stats["response_complete_ms"] = json!(complete_ms);
                Ok(Completed { message, stats, terminated })
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits { max_total_bytes: 1 << 20, max_line_bytes: 1 << 16, max_tool_argument_bytes: 4096, retain_reasoning: false };

    fn feed(chunks: &[&[u8]]) -> anyhow::Result<Completed> {
        let mut stream = ChatStream::new(LIMITS, Instant::now());
        for chunk in chunks { stream.push(chunk)?; }
        stream.finish()
    }

    fn tool_event(index: usize, name: Option<&str>, arguments: &str) -> String {
        let mut function = json!({"arguments":arguments});
        if let Some(name) = name { function["name"] = json!(name); }
        let call = json!({"index":index,"id":if name.is_some() {json!("call_1")} else {Value::Null},"function":function});
        format!("data: {}\r\n\r\n", json!({"choices":[{"delta":{"tool_calls":[call]}}]}))
    }

    #[test]
    fn tool_arguments_split_across_frames_and_utf8_boundaries_are_exact() {
        let arguments = json!({"goal":"上传演示文稿并确认页码","reason":"服务已返回 200"}).to_string();
        let (head, tail) = arguments.split_at(arguments.find('文').unwrap());
        let stream = format!(": keepalive\n\n{}{}{}data: {}\n\ndata: [DONE]\n\n",
            tool_event(0, Some("schedule_task"), ""), tool_event(0, None, head), tool_event(0, None, tail),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"total_tokens":42}}));
        let bytes = stream.as_bytes();
        let split = stream.find('文').unwrap() + 1;
        // Split inside a multibyte character and inside an SSE line.
        let completed = feed(&[&bytes[..7], &bytes[7..split], &bytes[split..]]).unwrap();
        assert!(completed.terminated);
        let call = &completed.message["tool_calls"][0];
        assert_eq!(call["function"]["name"], "schedule_task");
        assert_eq!(serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
            serde_json::from_str::<Value>(&arguments).unwrap());
        assert_eq!(completed.stats["response_format"], "sse");
        assert_eq!(completed.stats["received_done"], true);
        assert_eq!(completed.stats["finish_reason"], "tool_calls");
        assert_eq!(completed.stats["usage"]["total_tokens"], 42);
        assert_eq!(completed.stats["keepalives"], 1);
        assert!(completed.stats["first_tool_delta_ms"].is_u64());
    }

    #[test]
    fn eof_without_terminal_is_not_terminated() {
        let stream = tool_event(0, Some("schedule_task"), "{\"goal\":\"half");
        let completed = feed(&[stream.as_bytes()]).unwrap();
        assert!(!completed.terminated);
    }

    #[test]
    fn upstream_error_frame_and_malformed_events_fail() {
        let error = feed(&[b"data: {\"error\":{\"message\":\"overloaded\"}}\n\n"]).err().unwrap();
        assert_eq!(stream_error_code(&error), Some("UPSTREAM_ERROR"));
        let error = feed(&[b"data: {not json\n\n"]).err().unwrap();
        assert_eq!(stream_error_code(&error), Some("MALFORMED_EVENT"));
        let error = feed(&[b"data: \xff\xfe\n\n"]).err().unwrap();
        assert_eq!(stream_error_code(&error), Some("INVALID_UTF8"));
    }

    #[test]
    fn plain_json_response_is_reported_as_json_without_stream_delta_times() {
        let body = json!({"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","type":"function",
            "function":{"name":"finish_request","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}).to_string();
        let completed = feed(&[&body.as_bytes()[..10], &body.as_bytes()[10..]]).unwrap();
        assert!(completed.terminated);
        assert_eq!(completed.stats["response_format"], "json");
        assert!(completed.stats["first_delta_ms"].is_null());
        assert_eq!(completed.message["tool_calls"][0]["function"]["name"], "finish_request");
    }

    #[test]
    fn events_without_blank_separators_and_oversized_arguments() {
        let stream = format!("data: {}\ndata: {}\ndata: [DONE]\n",
            json!({"choices":[{"delta":{"content":"a"}}]}), json!({"choices":[{"delta":{"content":"b"},"finish_reason":"stop"}]}));
        let completed = feed(&[stream.as_bytes()]).unwrap();
        assert_eq!(completed.message["content"], "ab");
        assert!(completed.terminated);
        let huge = tool_event(0, Some("schedule_task"), &"x".repeat(5000));
        let error = feed(&[huge.as_bytes()]).err().unwrap();
        assert_eq!(stream_error_code(&error), Some("ARGUMENTS_TOO_LARGE"));
    }
}
