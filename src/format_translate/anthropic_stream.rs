use super::{gen_msg_id, write_sse_event};
use crate::format_translate::anthropic::map_finish_reason;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub struct StreamConverter {
    line_buf: Vec<u8>,
    model: String,
    msg_id: String,
    started: bool,
    next_block_index: usize,
    /// OpenAI tool_call index -> Anthropic block index
    tc_index_to_block: HashMap<usize, usize>,
    /// Accumulated partial JSON for in-flight tool calls
    tc_args_buf: HashMap<usize, String>,
    /// Tool calls whose content_block_start has been emitted
    tc_started: HashSet<usize>,
    /// The raw names of the tools, used to track which tools we are faking
    tc_raw_names: HashMap<usize, String>,
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
            tc_raw_names: HashMap::new(),
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
        let tc_deltas = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(|tc| tc.as_array());
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
        // First delta for this tool call - has id + function name
        if let Some(id) = tc.get("id") {
            let func = tc.get("function");
            let mut name = func
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let raw_name = name.clone();
            self.tc_raw_names.insert(tc_index, raw_name);

            let args = func
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("");

            let block_idx = self.next_block_index;
            self.next_block_index += 1;
            self.tc_index_to_block.insert(tc_index, block_idx);

            // Shell Hook: 伪装为原生 shell 命令
            if name.starts_with("codex_workspace_mcp__") {
                name = "run_terminal_cmd".to_string();
            }

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

                // Shell Hook: 如果是我们伪装的工具，不发送增量参数（因为真实参数还没拼装完）
                if name != "run_terminal_cmd" {
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
            }
            return;
        }

        // Subsequent delta - just arguments
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

        // 为了知道当前工具到底叫什么，需要从 tc_raw_names 里查
        let raw_name = self
            .tc_raw_names
            .get(&tc_index)
            .cloned()
            .unwrap_or_default();

        // Shell Hook: 过滤掉我们伪装工具的中间参数增量
        if !raw_name.starts_with("codex_workspace_mcp__") {
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

#[cfg(test)]
mod tests {
    use super::*;

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
