use super::{gen_msg_id, write_sse_event};
use super::responses_events::*;
use super::responses_parse::*;
use super::responses_tool_state::*;
use crate::tool_display::{self, ToolDisplayPhase};
use serde_json::Value;
use std::collections::HashMap;

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
    tool_status: HashMap<usize, ToolCallState>,
    tool_route_map: HashMap<String, (String, String)>,
    stream_prefix: Option<String>,
}

impl ResponsesStreamConverter {
    pub fn new(
        model: String,
        tool_route_map: HashMap<String, (String, String)>,
        stream_prefix: Option<String>,
    ) -> Self {
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
            usage: default_usage(),
            tool_status: HashMap::new(),
            tool_route_map,
            stream_prefix,
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

            match parse_sse_data_line(line) {
                SseData::Done => self.emit_completion_events(&mut out),
                SseData::Json(chunk_val) => self.process_chunk(&chunk_val, &mut out),
                SseData::Ignore => {}
            }
        }
        out
    }

    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.line_buf.is_empty() {
            let line = String::from_utf8_lossy(&self.line_buf);
            let line = line.trim_end();
            if let SseData::Json(chunk_val) = parse_sse_data_line(line) {
                self.process_chunk(&chunk_val, &mut out);
            }
            self.line_buf.clear();
        }
        self.emit_completion_events(&mut out);
        out
    }

    fn process_chunk(&mut self, chunk: &Value, out: &mut Vec<u8>) {
        if let Some(usage) = extract_usage(chunk) {
            self.usage = usage;
        }

        let choice = first_choice(chunk);
        let delta = choice_delta(choice);

        if !self.started {
            self.started = true;
            let created_event = build_response_created_event(&self.msg_id, &self.model);
            write_sse_event(out, "response.created", &created_event);
        }

        if let Some(d) = delta {
            if let Some(text) = extract_text_delta(d) {
                self.process_text_delta(text, out);
            }

            if let Some(tc_deltas) = extract_tool_deltas(d) {
                self.process_tool_delta(tc_deltas, out);
            }
        }

        if let Some(finish) = finish_reason(choice) {
            if finish != "null" {
                self.last_finish = Some(finish.to_owned());
            }
        }
    }

    fn process_text_delta(&mut self, text: &str, out: &mut Vec<u8>) {
        let Some(text_to_emit) = apply_stream_prefix(text, &mut self.stream_prefix) else {
            return;
        };

        self.accumulated_text.push_str(&text_to_emit);
        emit_text_structures(
            out,
            &self.msg_id,
            &mut self.text_item_added,
            &mut self.text_part_added,
        );
        emit_text_delta(out, &self.msg_id, &text_to_emit);
    }

    fn process_tool_delta(&mut self, tc_deltas: &Value, out: &mut Vec<u8>) {
        if let Some(arr) = tc_deltas.as_array() {
            for tc in arr {
                let idx = tool_call_index(tc);
                let target = ensure_accumulated_tool_call(&mut self.accumulated_tool_calls, idx);
                let state = self.tool_status.entry(idx).or_default();
                let Some(args_delta) = apply_tool_delta_to_state(tc, target, state) else {
                    continue;
                };

                let display = tool_display::display_for_tool(
                    &state.name,
                    &self.tool_route_map,
                    ToolDisplayPhase::Streaming,
                );

                emit_tool_item_added(out, &self.msg_id, idx, state, &display.name);
                emit_tool_args_delta(out, &self.msg_id, idx, state, &args_delta, &display);
            }
        }
    }

    fn emit_completion_events(&mut self, out: &mut Vec<u8>) {
        if !self.started {
            return;
        }

        self.finish_tool_calls(out);

        emit_text_completion(
            out,
            &self.msg_id,
            &self.accumulated_text,
            self.text_part_added,
            self.text_item_added,
        );

        let final_output_items = self.build_final_output_items();
        self.emit_response_completed(out, final_output_items);

        out.extend_from_slice(b"data: [DONE]\n\n");
        self.reset_after_completion();
    }

    fn finish_tool_calls(&mut self, out: &mut Vec<u8>) {
        let indexes: Vec<usize> = self.tool_status.keys().cloned().collect();
        for idx in indexes {
            let state = self.tool_status.get_mut(&idx).unwrap();
            emit_tool_completion(out, &self.msg_id, &self.tool_route_map, idx, state);
        }
    }

    fn build_final_output_items(&self) -> Vec<Value> {
        let mut final_output_items = Vec::new();
        if !self.accumulated_text.is_empty() {
            // id 与流式 SSE 事件中的 item_id 保持一致，Codex 重启后才能按 id 索引到历史消息
            // content type 必须是 output_text（Responses API 规范），否则 Codex 渲染器识别不了
            final_output_items.push(build_completed_text_output_item(
                &self.msg_id,
                &self.accumulated_text,
            ));
        }

        // Responses API 格式：每个工具调用是独立的 function_call 顶层条目，不嵌套在 message 里
        final_output_items.extend(collect_completed_tool_outputs(
            &self.msg_id,
            &self.accumulated_tool_calls,
            &self.tool_status,
            &self.tool_route_map,
        ));

        final_output_items
    }

    fn emit_response_completed(&self, out: &mut Vec<u8>, final_output_items: Vec<Value>) {
        let completed_event = build_response_completed_event(
            &self.msg_id,
            &self.model,
            self.usage.clone(),
            final_output_items,
        );
        write_sse_event(out, "response.completed", &completed_event);
    }

    fn reset_after_completion(&mut self) {
        self.started = false;
        self.text_item_added = false;
        self.text_part_added = false;
        self.accumulated_text.clear();
        self.accumulated_tool_calls.clear();
        self.tool_status.clear();
        self.last_finish = None;
    }
}

fn apply_stream_prefix(text: &str, stream_prefix: &mut Option<String>) -> Option<String> {
    if text.is_empty() {
        return None;
    }

    let mut text_to_emit = text.to_owned();
    if let Some(prefix) = stream_prefix.take() {
        text_to_emit = format!("{}{}", prefix, text_to_emit);
    }
    Some(text_to_emit)
}

fn emit_text_structures(
    out: &mut Vec<u8>,
    msg_id: &str,
    text_item_added: &mut bool,
    text_part_added: &mut bool,
) {
    if !*text_item_added {
        *text_item_added = true;
        let item_added = build_text_item_added_event(msg_id);
        write_sse_event(out, "response.output_item.added", &item_added);
    }

    if !*text_part_added {
        *text_part_added = true;
        let part_added = build_text_part_added_event(msg_id);
        write_sse_event(out, "response.content_part.added", &part_added);
    }
}

fn emit_text_delta(out: &mut Vec<u8>, msg_id: &str, text: &str) {
    let delta_event = build_text_delta_event(msg_id, text);
    write_sse_event(out, "response.output_text.delta", &delta_event);
}

fn emit_text_completion(
    out: &mut Vec<u8>,
    msg_id: &str,
    text: &str,
    text_part_added: bool,
    text_item_added: bool,
) {
    if text_part_added {
        let text_done = build_text_done_event(msg_id, text);
        write_sse_event(out, "response.output_text.done", &text_done);

        let part_done = build_text_part_done_event(msg_id, text);
        write_sse_event(out, "response.content_part.done", &part_done);
    }

    if text_item_added {
        let item_done = build_text_item_done_event(msg_id, text);
        write_sse_event(out, "response.output_item.done", &item_done);
    }
}

fn emit_tool_item_added(
    out: &mut Vec<u8>,
    msg_id: &str,
    idx: usize,
    state: &mut ToolCallState,
    display_name: &str,
) {
    if state.added_emitted {
        return;
    }

    state.added_emitted = true;
    let item_id = tool_item_id(msg_id, idx);
    let item_added =
        build_tool_item_added_event(msg_id, idx + 1, &item_id, &state.id, display_name);
    write_sse_event(out, "response.output_item.added", &item_added);
}

fn emit_tool_args_delta(
    out: &mut Vec<u8>,
    msg_id: &str,
    idx: usize,
    state: &ToolCallState,
    args_delta: &str,
    display: &tool_display::ToolDisplay,
) {
    if display.suppress_argument_delta {
        return;
    }

    let item_id = tool_item_id(msg_id, idx);
    let delta_event = build_tool_args_delta_event(msg_id, &item_id, idx + 1, &state.id, args_delta);
    write_sse_event(out, "response.function_call.arguments.delta", &delta_event);
}

fn emit_tool_completion(
    out: &mut Vec<u8>,
    msg_id: &str,
    route_map: &HashMap<String, (String, String)>,
    idx: usize,
    state: &mut ToolCallState,
) {
    let display =
        tool_display::display_for_tool(&state.name, route_map, ToolDisplayPhase::Completed);
    emit_tool_item_added(out, msg_id, idx, state, &display.name);

    if state.done_emitted {
        return;
    }

    state.done_emitted = true;
    let item_id = tool_item_id(msg_id, idx);
    let final_args = tool_display::final_arguments(&state.id, &state.name, &state.arguments, route_map);

    let args_done = build_tool_args_done_event(msg_id, &item_id, idx + 1, &state.id, &final_args);
    write_sse_event(out, "response.function_call.arguments.done", &args_done);

    let item_done =
        build_tool_item_done_event(msg_id, idx + 1, &item_id, &state.id, &display.name, &final_args);
    write_sse_event(out, "response.output_item.done", &item_done);
}
