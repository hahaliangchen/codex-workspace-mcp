//! Bidirectional Anthropic Messages ↔ OpenAI Chat Completions format translator.
//!
//! Public converters are re-exported from submodules.

mod anthropic;
mod anthropic_stream;
mod responses_chat;
pub mod codex_protocol;

pub use anthropic::{anthropic_to_openai, openai_to_anthropic};
pub use anthropic_stream::StreamConverter;
pub use responses_chat::{
    OpenAiChatToolCall, build_openai_chat_request, clean_unmatched_tool_calls,
    collect_all_tool_calls_from_openai_chat, collect_openai_chat_final_text,
    openai_chat_assistant_message, openai_chat_tool_result_message,
    responses_body_to_openai_chat_messages, responses_tools_to_openai_chat_tools,
};
use serde_json::Value;
use std::time::SystemTime;

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
