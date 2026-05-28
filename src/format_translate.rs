//! Bidirectional Anthropic Messages ↔ OpenAI Chat Completions format translator.
//!
//! Three converters:
//! 1. Request:  `anthropic_to_openai`     — Anthropic Messages → OpenAI Chat
//! 2. Response: `openai_to_anthropic`     — OpenAI Chat → Anthropic Message
//! 3. Stream:   `StreamConverter`         — OpenAI SSE → Anthropic SSE

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

// ============================================================================
// Request: Anthropic Messages → OpenAI Chat Completions
// ============================================================================

pub fn anthropic_to_openai(body: &Value) -> Value {
    let mut openai = json!({});

    if let Some(m) = body.get("model") {
        openai["model"] = m.clone();
    }

    openai["messages"] = json!(convert_messages(body));

    passthrough_opt(body, &mut openai, "max_tokens");
    passthrough_opt(body, &mut openai, "temperature");
    passthrough_opt(body, &mut openai, "top_p");
    passthrough_opt(body, &mut openai, "top_k");
    passthrough_opt(body, &mut openai, "stream");

    // stop_sequences → stop
    if let Some(v) = body.get("stop_sequences") {
        openai["stop"] = v.clone();
    }

    // tools
    if let Some(v) = body.get("tools") {
        openai["tools"] = json!(convert_tools(v));
    }

    // tool_choice
    if let Some(v) = body.get("tool_choice") {
        openai["tool_choice"] = convert_tool_choice(v);
    }

    openai
}

fn passthrough_opt(src: &Value, dst: &mut Value, key: &str) {
    if let Some(v) = src.get(key) {
        dst[key] = v.clone();
    }
}

// ---- messages ------------------------------------------------------------

fn convert_messages(body: &Value) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();

    // Anthropic top-level system → OpenAI system message
    if let Some(sys) = body.get("system") {
        let text = flatten_system(sys);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    let msgs = match body.get("messages").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return messages,
    };

    for msg in msgs {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content");

        match role {
            "assistant" => {
                let (text, tool_calls) = split_assistant(content);
                let mut m = json!({"role": "assistant"});
                m["content"] = match text {
                    Some(t) => json!(t),
                    None => json!(null),
                };
                if let Some(tc) = tool_calls {
                    m["tool_calls"] = tc;
                }
                messages.push(m);
            }
            "user" => {
                let (plain_blocks, tool_results) = split_user(content);
                // Only push user message if there are non-tool-result blocks
                if !plain_blocks.is_empty()
                    || plain_blocks.is_empty() && tool_results.is_empty()
                {
                    messages.push(json!({"role": "user", "content": compact_text(plain_blocks)}));
                }
                for tr in tool_results {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tr["tool_call_id"],
                        "content": tr["content"]
                    }));
                }
            }
            _ => {
                let cleaned = strip_thinking(content.unwrap_or(&Value::Null));
                messages.push(json!({"role": role, "content": cleaned}));
            }
        }
    }

    // Normalize: some providers (DeepSeek v4) reject string content and
    // require [{"type":"text","text":"..."}] array format.
    for msg in &mut messages {
        if let Some(content) = msg.get("content") {
            if content.is_string() {
                if let Some(text) = content.as_str() {
                    msg["content"] = json!([{"type": "text", "text": text}]);
                }
            }
        }
    }

    messages
}

fn clean_prompt_text(text: &str) -> String {
    let mut cleaned = text.to_owned();

    // 1. Remove the entire <system-reminder> block detailing skills if it contains algorithmic-art or other unused skills
    if let Some(start_idx) = cleaned.find("<system-reminder>\nThe following skills are available") {
        if let Some(end_idx) = cleaned[start_idx..].find("</system-reminder>") {
            let actual_end = start_idx + end_idx + "</system-reminder>".len();
            cleaned.drain(start_idx..actual_end);
        } else {
            cleaned.drain(start_idx..);
        }
    }

    // 2. Strip standard security policies if present
    if let Some(sec_idx) = cleaned.find("IMPORTANT: Assist with authorized security testing") {
        if let Some(next_section) = cleaned[sec_idx..].find("\n\n") {
            cleaned.drain(sec_idx..(sec_idx + next_section + 2)); // include newlines
        }
    }

    // 3. Strip URL guessing rules
    if let Some(url_idx) = cleaned.find("IMPORTANT: You must NEVER generate or guess URLs") {
        if let Some(next_section) = cleaned[url_idx..].find("\n\n") {
            cleaned.drain(url_idx..(url_idx + next_section + 2)); // include newlines
        }
    }

    cleaned
}

fn flatten_system(sys: &Value) -> String {
    let raw = if let Some(s) = sys.as_str() {
        s.to_owned()
    } else if let Some(arr) = sys.as_array() {
        arr.iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };
    clean_prompt_text(&raw)
}

/// Split assistant content array into (optional_text, optional_openai_tool_calls).
fn split_assistant(content: Option<&Value>) -> (Option<String>, Option<Value>) {
    let blocks = match content.and_then(|c| c.as_array()) {
        Some(a) => a,
        None => return (content.and_then(|c| c.as_str()).map(String::from), None),
    };

    let mut texts = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    texts.push(t.to_owned());
                }
            }
            Some("tool_use") => {
                let args = b
                    .get("input")
                    .and_then(|i| serde_json::to_string(i).ok())
                    .unwrap_or_default();
                tool_calls.push(json!({
                    "id": b.get("id"),
                    "type": "function",
                    "function": {
                        "name": b.get("name"),
                        "arguments": args
                    }
                }));
            }
            _ => {} // thinking etc.
        }
    }

    let text = if texts.is_empty() {
        None
    } else {
        Some(texts.join(""))
    };

    let tc = if tool_calls.is_empty() {
        None
    } else {
        Some(json!(tool_calls))
    };

    (text, tc)
}

fn split_user(content: Option<&Value>) -> (Vec<Value>, Vec<Value>) {
    let blocks = match content.and_then(|c| c.as_array()) {
        Some(a) => a,
        None => {
            let v = content.unwrap_or(&Value::Null).clone();
            if let Some(s) = v.as_str() {
                return (vec![json!(clean_prompt_text(s))], vec![]);
            }
            return (vec![v], vec![]);
        }
    };

    let mut plain: Vec<Value> = Vec::new();
    let mut results: Vec<Value> = Vec::new();

    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("tool_result") => {
                let inner = b.get("content").cloned().unwrap_or(Value::Null);
                let flat = flatten_tool_result_content(&inner);
                results.push(json!({
                    "tool_call_id": b.get("tool_use_id"),
                    "content": flat
                }));
            }
            Some("thinking") => {} // drop
            Some("text") => {
                let mut b_cleaned = b.clone();
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    b_cleaned["text"] = json!(clean_prompt_text(t));
                }
                plain.push(b_cleaned);
            }
            _ => plain.push(b.clone()),
        }
    }

    (plain, results)
}

/// Flatten tool_result content (string or array of text blocks) → plain string.
fn flatten_tool_result_content(inner: &Value) -> String {
    match inner {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Compact a list of content blocks to a simple value: if it's a single text
/// block return its text directly; otherwise keep the array.
fn compact_text(blocks: Vec<Value>) -> Value {
    if blocks.is_empty() {
        return Value::Null;
    }
    if blocks.len() == 1 {
        let b = &blocks[0];
        if b.is_string() {
            return b.clone();
        }
        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                return json!(t);
            }
        }
    }
    json!(blocks)
}

fn strip_thinking(content: &Value) -> Value {
    match content {
        Value::String(_) => content.clone(),
        Value::Array(blocks) => {
            let cleaned: Vec<Value> = blocks
                .iter()
                .filter(|b| {
                    b.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t != "thinking")
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            json!(cleaned)
        }
        _ => content.clone(),
    }
}

// ---- tools ---------------------------------------------------------------

fn convert_tools(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.get("name"),
                            "description": t.get("description"),
                            "parameters": t.get("input_schema")
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---- tool_choice ---------------------------------------------------------

fn convert_tool_choice(v: &Value) -> Value {
    match v {
        Value::String(s) => match s.as_str() {
            "any" => json!("required"),
            "auto" => json!("auto"),
            _ => v.clone(),
        },
        Value::Object(obj) => match obj.get("type").and_then(|t| t.as_str()) {
            Some("any") => json!("required"),
            Some("tool") => json!({
                "type": "function",
                "function": {"name": obj.get("name")}
            }),
            _ => json!("auto"),
        },
        _ => v.clone(),
    }
}

// ============================================================================
// Response: OpenAI Chat Completion → Anthropic Message  (non-streaming)
// ============================================================================

pub fn openai_to_anthropic(openai_body: &Value, model: &str) -> Value {
    let choice = openai_body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first());

    let message = choice.and_then(|c| c.get("message"));
    let mut content_blocks: Vec<Value> = Vec::new();

    // text
    if let Some(text) = message.and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
        if !text.is_empty() {
            content_blocks.push(json!({"type": "text", "text": text}));
        }
    }

    // tool_calls → tool_use blocks
    if let Some(tcs) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|tc| tc.as_array())
    {
        for tc in tcs {
            let func = tc.get("function");
            let input: Value = func
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            content_blocks.push(json!({
                "type": "tool_use",
                "id": tc.get("id"),
                "name": func.and_then(|f| f.get("name")),
                "input": input
            }));
        }
    }

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        .unwrap_or("stop");

    let usage = openai_body.get("usage");
    let zero = json!(0);
    let input_tokens = usage.and_then(|u| u.get("prompt_tokens")).unwrap_or(&zero);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .unwrap_or(&zero);

    json!({
        "id": openai_body.get("id").and_then(|i| i.as_str()).unwrap_or(""),
        "type": "message",
        "role": "assistant",
        "content": content_blocks,
        "model": model,
        "stop_reason": map_finish_reason(finish_reason),
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    })
}

fn map_finish_reason(r: &str) -> &str {
    match r {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    }
}

// ============================================================================
// Streaming: OpenAI SSE  →  Anthropic SSE
// ============================================================================

fn gen_msg_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("msg_{:x}", nanos)
}

fn write_sse_event(out: &mut Vec<u8>, event: &str, data: &Value) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(serde_json::to_string(data).unwrap_or_default().as_bytes());
    out.extend_from_slice(b"\n\n");
}

pub struct StreamConverter {
    line_buf: Vec<u8>,
    model: String,
    msg_id: String,
    started: bool,
    next_block_index: usize,
    /// OpenAI tool_call index → Anthropic block index
    tc_index_to_block: HashMap<usize, usize>,
    /// Accumulated partial JSON for in-flight tool calls
    tc_args_buf: HashMap<usize, String>,
    /// Tool calls whose content_block_start has been emitted
    tc_started: HashSet<usize>,
    /// Have we seen any content at all?
    seen_any: bool,
    /// The finish_reason from the last chunk (used in final flush)
    last_finish: Option<String>,
}

impl StreamConverter {
    pub fn new(model: String) -> Self {
        Self {
            line_buf: Vec::new(),
            model,
            msg_id: gen_msg_id(),
            started: false,
            next_block_index: 0,
            tc_index_to_block: HashMap::new(),
            tc_args_buf: HashMap::new(),
            tc_started: HashSet::new(),
            seen_any: false,
            last_finish: None,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        self.line_buf.extend_from_slice(chunk);

        loop {
            let pos = match self.line_buf.iter().position(|&b| b == b'\n') {
                Some(p) => p,
                None => break,
            };
            let line_bytes = self.line_buf.drain(..=pos).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches('\n').trim_end_matches('\r');

            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    if self.seen_any {
                        self.emit_block_stops(&mut out);
                        self.emit_message_end(&mut out);
                    }
                } else if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                    self.process_chunk(&chunk, &mut out);
                }
            }
        }
        out
    }

    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();

        // Process any remaining data in buffer
        if !self.line_buf.is_empty() {
            let line = String::from_utf8_lossy(&self.line_buf);
            let line = line.trim_end();
            if let Some(data) = line.strip_prefix("data: ") {
                if data != "[DONE]" {
                    if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                        self.process_chunk(&chunk, &mut out);
                    }
                }
            }
            self.line_buf.clear();
        }

        if self.seen_any {
            self.emit_block_stops(&mut out);
            self.emit_message_end(&mut out);
        }
        out
    }

    fn process_chunk(&mut self, chunk: &Value, out: &mut Vec<u8>) {
        let choice = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());

        let delta = choice.and_then(|c| c.get("delta"));

        // Start message if first content
        if !self.started {
            self.started = true;
            self.emit_message_start(out);
        }

        // ---- text content ----
        let text = delta
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty());

        if let Some(t) = text {
            self.ensure_text_block_started(out);
            self.seen_any = true;
            write_sse_event(
                out,
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": t}
                }),
            );
        }

        // ---- tool calls ----
        let tc_deltas = delta.and_then(|d| d.get("tool_calls")).and_then(|tc| tc.as_array());
        if let Some(tc_deltas) = tc_deltas {
            for tc in tc_deltas {
                let tc_index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                self.process_tool_delta(tc, tc_index, out);
            }
        }

        // track finish reason (may be used in flush if we don't see [DONE])
        if let Some(finish) = choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(|f| f.as_str())
        {
            if finish != "null" {
                self.last_finish = Some(finish.to_owned());
                // text + tool_calls are handled by per-block content_block_stop
                if self.seen_any {
                    // If no tool calls active, emit stop events now
                    self.emit_block_stops(out);
                    self.emit_message_end(out);
                }
            }
        }
    }

    fn process_tool_delta(&mut self, tc: &Value, tc_index: usize, out: &mut Vec<u8>) {
        // First delta for this tool call — has id + function name
        if let Some(id) = tc.get("id") {
            let func = tc.get("function");
            let name = func.and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
            let args = func
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("");

            let block_idx = self.next_block_index;
            self.next_block_index += 1;
            self.tc_index_to_block.insert(tc_index, block_idx);

            // Emit content_block_start for this tool_use
            write_sse_event(
                out,
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": block_idx,
                    "content_block": {
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": {}
                    }
                }),
            );

            self.tc_started.insert(tc_index);
            if !args.is_empty() {
                self.tc_args_buf.insert(tc_index, args.to_owned());
                self.seen_any = true;
                write_sse_event(
                    out,
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": block_idx,
                        "delta": {"type": "input_json_delta", "partial_json": args}
                    }),
                );
            }
            return;
        }

        // Subsequent delta — just arguments
        let args = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
            .unwrap_or("");

        if args.is_empty() {
            return;
        }

        let block_idx = *self.tc_index_to_block.get(&tc_index).unwrap_or(&0);
        self.seen_any = true;

        // Ensure block is started (belt & suspenders)
        if !self.tc_started.contains(&tc_index) {
            self.tc_started.insert(tc_index);
            write_sse_event(
                out,
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": block_idx,
                    "content_block": {
                        "type": "tool_use",
                        "id": "",
                        "name": "",
                        "input": {}
                    }
                }),
            );
        }

        self.tc_args_buf
            .entry(tc_index)
            .and_modify(|s| s.push_str(args))
            .or_insert_with(|| args.to_owned());

        write_sse_event(
            out,
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": block_idx,
                "delta": {"type": "input_json_delta", "partial_json": args}
            }),
        );
    }

    /// If we haven't started any content block yet, start a text block (index 0).
    fn ensure_text_block_started(&mut self, out: &mut Vec<u8>) {
        if self.next_block_index == 0 && !self.tc_started.contains(&0) {
            self.next_block_index = 1; // reserve index 0 for text
            write_sse_event(
                out,
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""}
                }),
            );
        }
    }

    fn emit_message_start(&self, out: &mut Vec<u8>) {
        write_sse_event(
            out,
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.msg_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0}
                }
            }),
        );
    }

    fn emit_block_stops(&mut self, out: &mut Vec<u8>) {
        // Stop each active content block
        let indexes: Vec<usize> = {
            let mut v: Vec<usize> = (0..self.next_block_index).collect();
            v.sort();
            v
        };
        for idx in indexes {
            write_sse_event(
                out,
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": idx}),
            );
        }
        // Reset so we don't emit stops twice
        self.next_block_index = 0;
        self.tc_started.clear();
        self.tc_args_buf.clear();
        self.tc_index_to_block.clear();
    }

    fn emit_message_end(&mut self, out: &mut Vec<u8>) {
        let stop_reason = self
            .last_finish
            .as_deref()
            .map(map_finish_reason)
            .unwrap_or("end_turn");

        write_sse_event(
            out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {"output_tokens": 0}
            }),
        );

        write_sse_event(out, "message_stop", &json!({"type": "message_stop"}));

        self.seen_any = false;
        self.last_finish = None;
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: extract text from normalized content (always [{"type":"text","text":"..."}])
    fn as_text(content: &Value) -> &str {
        content
            .as_array()
            .and_then(|a| a.first())
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
    }

    // ---- request: messages -------------------------------------------------

    #[test]
    fn strips_thinking_blocks_from_content() {
        let input = json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "thinking", "thinking": "hmm"}
                ]}
            ]
        });
        let out = anthropic_to_openai(&input);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(as_text(&msgs[0]["content"]), "hello");
    }

    #[test]
    fn converts_system_prompt_to_message() {
        let input = json!({
            "model": "claude",
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_to_openai(&input);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(as_text(&msgs[0]["content"]), "You are helpful.");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn converts_tool_use_to_openai_tool_calls() {
        let input = json!({
            "model": "claude",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Let me check."},
                    {"type": "tool_use", "id": "tool_001", "name": "get_weather", "input": {"city": "Boston"}}
                ]}
            ]
        });
        let out = anthropic_to_openai(&input);
        let msg = &out["messages"].as_array().unwrap()[0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(as_text(&msg["content"]), "Let me check.");
        let tc = &msg["tool_calls"].as_array().unwrap()[0];
        assert_eq!(tc["id"], "tool_001");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "get_weather");
        assert_eq!(tc["function"]["arguments"], "{\"city\":\"Boston\"}");
    }

    #[test]
    fn converts_tool_result_to_separate_tool_message() {
        let input = json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tool_001", "content": "72F sunny"}
                ]}
            ]
        });
        let out = anthropic_to_openai(&input);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "tool_001");
        assert_eq!(as_text(&msgs[0]["content"]), "72F sunny");
    }

    #[test]
    fn user_with_text_and_tool_results_splits_correctly() {
        let input = json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "Thanks!"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": "result"}
                ]}
            ]
        });
        let out = anthropic_to_openai(&input);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(as_text(&msgs[0]["content"]), "Thanks!");
        assert_eq!(msgs[1]["role"], "tool");
    }

    #[test]
    fn user_with_plain_string_content_converts_correctly() {
        let input = json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": "."}
            ]
        });
        let out = anthropic_to_openai(&input);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn strips_system_reminders_and_unused_policies() {
        let text = "IMPORTANT: Assist with authorized security testing, defensive security... dual-use tools\n\nIMPORTANT: You must NEVER generate or guess URLs...\n\n<system-reminder>\nThe following skills are available for use with the Skill tool:\n\n- algorithmic-art: generative art\n- frontend-design: react css\n</system-reminder>\n\nKeep this text.";
        let cleaned = clean_prompt_text(text);
        assert!(!cleaned.contains("algorithmic-art"));
        assert!(!cleaned.contains("security testing"));
        assert!(!cleaned.contains("generate or guess URLs"));
        assert!(cleaned.contains("Keep this text."));
    }

    // ---- request: tools ----------------------------------------------------

    #[test]
    fn converts_anthropic_tools_to_openai() {
        let input = json!({
            "model": "claude",
            "messages": [],
            "tools": [
                {"name": "get_weather", "description": "Get weather", "input_schema": {"type": "object"}}
            ]
        });
        let out = anthropic_to_openai(&input);
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    // ---- request: tool_choice ----------------------------------------------

    #[test]
    fn converts_tool_choice_any_to_required() {
        let input = json!({
            "model": "claude",
            "messages": [],
            "tool_choice": {"type": "any"}
        });
        let out = anthropic_to_openai(&input);
        assert_eq!(out["tool_choice"], "required");
    }

    #[test]
    fn converts_tool_choice_named() {
        let input = json!({
            "model": "claude",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "specific_tool"}
        });
        let out = anthropic_to_openai(&input);
        assert_eq!(out["tool_choice"]["type"], "function");
        assert_eq!(out["tool_choice"]["function"]["name"], "specific_tool");
    }

    // ---- request: params ---------------------------------------------------

    #[test]
    fn converts_stop_sequences_to_stop() {
        let input = json!({
            "model": "claude",
            "messages": [],
            "stop_sequences": ["END"]
        });
        let out = anthropic_to_openai(&input);
        assert_eq!(out["stop"].as_array().unwrap()[0], "END");
    }

    // ---- response ----------------------------------------------------------

    #[test]
    fn converts_text_response() {
        let openai = json!({
            "id": "resp-1",
            "choices": [{"message": {"role": "assistant", "content": "Hello!"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let anth = openai_to_anthropic(&openai, "my-model");
        assert_eq!(anth["content"][0]["type"], "text");
        assert_eq!(anth["content"][0]["text"], "Hello!");
        assert_eq!(anth["stop_reason"], "end_turn");
        assert_eq!(anth["usage"]["input_tokens"], 10);
        assert_eq!(anth["usage"]["output_tokens"], 5);
    }

    #[test]
    fn converts_tool_calls_response() {
        let openai = json!({
            "id": "resp-2",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Boston\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let anth = openai_to_anthropic(&openai, "m");
        let block = &anth["content"].as_array().unwrap()[0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "call_1");
        assert_eq!(block["name"], "get_weather");
        assert_eq!(block["input"]["city"], "Boston");
        assert_eq!(anth["stop_reason"], "tool_use");
    }

    // ---- sse streaming -----------------------------------------------------

    #[test]
    fn stream_text_delta() {
        let mut c = StreamConverter::new("m".into());
        let out = c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}
"#,
        );
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("event: message_start"));
        assert!(s.contains("event: content_block_start"));
        assert!(s.contains("text_delta"));
        assert!(s.contains("Hello"));
    }

    #[test]
    fn stream_tool_call_delta() {
        let mut c = StreamConverter::new("m".into());
        let out = c.feed(
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"get_weather","arguments":"{\"city\":"}}]},"finish_reason":null}]}
"#,
        );
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("tool_use"));
        assert!(s.contains("call_x"));
        assert!(s.contains("get_weather"));
        assert!(s.contains("input_json_delta"));
        // partial_json value is JSON-string-escaped, so " becomes \"
        assert!(s.contains("{\\\"city\\\":"));
    }
}

// ============================================================================
// Responses API Streaming: OpenAI ChatCompletion SSE → OpenAI Responses API SSE
// ============================================================================

pub struct ResponsesStreamConverter {
    line_buf: Vec<u8>,
    model: String,
    msg_id: String,
    started: bool,
    text_item_added: bool,
    text_part_added: bool,
    accumulated_text: String,
    accumulated_tool_calls: Vec<Value>,
    last_finish: Option<String>,
    usage: Value,
}

impl ResponsesStreamConverter {
    pub fn new(model: String) -> Self {
        Self {
            line_buf: Vec::new(),
            model,
            msg_id: gen_msg_id(),
            started: false,
            text_item_added: false,
            text_part_added: false,
            accumulated_text: String::new(),
            accumulated_tool_calls: Vec::new(),
            last_finish: None,
            usage: json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "total_tokens": 0
            }),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        self.line_buf.extend_from_slice(chunk);

        loop {
            let pos = match self.line_buf.iter().position(|&b| b == b'\n') {
                Some(p) => p,
                None => break,
            };
            let line_bytes = self.line_buf.drain(..=pos).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches('\n').trim_end_matches('\r');

            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    self.emit_completion_events(&mut out);
                } else if let Ok(chunk_val) = serde_json::from_str::<Value>(data) {
                    self.process_chunk(&chunk_val, &mut out);
                }
            }
        }
        out
    }

    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.line_buf.is_empty() {
            let line = String::from_utf8_lossy(&self.line_buf);
            let line = line.trim_end();
            if let Some(data) = line.strip_prefix("data: ") {
                if data != "[DONE]" {
                    if let Ok(chunk_val) = serde_json::from_str::<Value>(data) {
                        self.process_chunk(&chunk_val, &mut out);
                    }
                }
            }
            self.line_buf.clear();
        }
        self.emit_completion_events(&mut out);
        out
    }

    fn process_chunk(&mut self, chunk: &Value, out: &mut Vec<u8>) {
        if let Some(u) = chunk.get("usage") {
            if !u.is_null() {
                let prompt = u.get("prompt_tokens")
                    .or_else(|| u.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let completion = u.get("completion_tokens")
                    .or_else(|| u.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let total = u.get("total_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(prompt + completion);

                self.usage = json!({
                    "input_tokens": prompt,
                    "output_tokens": completion,
                    "prompt_tokens": prompt,
                    "completion_tokens": completion,
                    "total_tokens": total
                });
            }
        }

        let choice = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());

        let delta = choice.and_then(|c| c.get("delta"));

        if !self.started {
            self.started = true;
            let created_event = json!({
                "type": "response.created",
                "response": {
                    "id": self.msg_id,
                    "object": "response",
                    "status": "in_progress",
                    "model": self.model,
                }
            });
            write_sse_event(out, "response.created", &created_event);
        }

        if let Some(d) = delta {
            if let Some(text) = d.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    self.accumulated_text.push_str(text);
                    self.ensure_text_structures(out);
                    
                    let delta_event = json!({
                        "type": "response.output_text.delta",
                        "response_id": self.msg_id,
                        "item_id": format!("item_txt_{}", self.msg_id),
                        "output_index": 0,
                        "content_index": 0,
                        "delta": text
                    });
                    write_sse_event(out, "response.output_text.delta", &delta_event);
                }
            }

            if let Some(tc_deltas) = d.get("tool_calls") {
                self.process_tool_delta(tc_deltas);
            }
        }

        if let Some(finish) = choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(|f| f.as_str())
        {
            if finish != "null" {
                self.last_finish = Some(finish.to_owned());
            }
        }
    }

    fn ensure_text_structures(&mut self, out: &mut Vec<u8>) {
        if !self.text_item_added {
            self.text_item_added = true;
            let item_added = json!({
                "type": "response.output_item.added",
                "response_id": self.msg_id,
                "output_index": 0,
                "item": {
                    "id": format!("item_txt_{}", self.msg_id),
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            });
            write_sse_event(out, "response.output_item.added", &item_added);
        }

        if !self.text_part_added {
            self.text_part_added = true;
            let part_added = json!({
                "type": "response.content_part.added",
                "response_id": self.msg_id,
                "output_index": 0,
                "content_index": 0,
                "part": {
                    "type": "text",
                    "text": ""
                }
            });
            write_sse_event(out, "response.content_part.added", &part_added);
        }
    }

    fn process_tool_delta(&mut self, tc_deltas: &Value) {
        if let Some(arr) = tc_deltas.as_array() {
            for tc in arr {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                while self.accumulated_tool_calls.len() <= idx {
                    self.accumulated_tool_calls.push(json!({
                        "id": "",
                        "type": "function",
                        "function": {
                            "name": "",
                            "arguments": ""
                        }
                    }));
                }

                let target = &mut self.accumulated_tool_calls[idx];
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    target["id"] = json!(id);
                }
                if let Some(func) = tc.get("function") {
                    if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                        target["function"]["name"] = json!(name);
                    }
                    if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                        let cur = target["function"]["arguments"].as_str().unwrap_or("");
                        target["function"]["arguments"] = json!(format!("{}{}", cur, args));
                    }
                }
            }
        }
    }

    fn emit_completion_events(&mut self, out: &mut Vec<u8>) {
        if !self.started {
            return;
        }

        if self.text_part_added {
            let text_done = json!({
                "type": "response.output_text.done",
                "response_id": self.msg_id,
                "item_id": format!("item_txt_{}", self.msg_id),
                "output_index": 0,
                "content_index": 0,
                "text": self.accumulated_text
            });
            write_sse_event(out, "response.output_text.done", &text_done);

            let part_done = json!({
                "type": "response.content_part.done",
                "response_id": self.msg_id,
                "output_index": 0,
                "content_index": 0,
                "part": {
                    "type": "text",
                    "text": self.accumulated_text
                }
            });
            write_sse_event(out, "response.content_part.done", &part_done);
        }
        if self.text_item_added {
            let item_done = json!({
                "type": "response.output_item.done",
                "response_id": self.msg_id,
                "output_index": 0,
                "item": {
                    "id": format!("item_txt_{}", self.msg_id),
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "text",
                            "text": self.accumulated_text
                        }
                    ]
                }
            });
            write_sse_event(out, "response.output_item.done", &item_done);
        }

        let mut final_output_items = Vec::new();
        if !self.accumulated_text.is_empty() {
            final_output_items.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": self.accumulated_text
                    }
                ]
            }));
        }

        let valid_tool_calls: Vec<Value> = self.accumulated_tool_calls
            .iter()
            .filter(|tc| {
                let name = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                !name.is_empty()
            })
            .cloned()
            .collect();

        if !valid_tool_calls.is_empty() {
            final_output_items.push(json!({
                "type": "message",
                "role": "assistant",
                "tool_calls": valid_tool_calls
            }));
        }

        let completed_event = json!({
            "type": "response.completed",
            "response": {
                "id": self.msg_id,
                "object": "response",
                "status": "completed",
                "model": self.model,
                "usage": self.usage,
                "output": final_output_items
            }
        });
        write_sse_event(out, "response.completed", &completed_event);

        out.extend_from_slice(b"data: [DONE]\n\n");

        self.started = false;
        self.text_item_added = false;
        self.text_part_added = false;
        self.accumulated_text.clear();
        self.accumulated_tool_calls.clear();
        self.last_finish = None;
    }
}

