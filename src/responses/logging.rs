use serde_json::Value;
use std::sync::Mutex;

const MAX_RECORDED_TURN_CHARS: usize = 8 * 1024;
const MAX_REQUEST_LOG_CHARS: usize = 1 * 1024;

pub fn log_request_body(
    log: &Mutex<std::fs::File>,
    conversation_id: &str,
    body: &Value,
    client_model: &str,
) {
    record_conversation_turn(conversation_id, body, client_model);

    let input_count = body
        .get("input")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0);
    let summary = latest_user_summary(body);

    crate::ai_proxy::log_write(
        log,
        true,
        Some("REQ_IN"),
        Some("user"),
        &format!(
            "=== /v1/responses model={} input_items={} latest_user={}",
            client_model,
            input_count,
            compact_text(&summary, MAX_REQUEST_LOG_CHARS)
        ),
    );
}

fn record_conversation_turn(conversation_id: &str, body: &Value, client_model: &str) {
    let input_count = body
        .get("input")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0);
    let messages_count = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0);
    let summary = latest_user_summary(body);
    let content = compact_text(
        &format!(
            "model={} input_items={} messages={} latest_user={}",
            client_model, input_count, messages_count, summary
        ),
        MAX_RECORDED_TURN_CHARS,
    );

    crate::proxy_log::write_conversation_message(
        conversation_id,
        "responses.turn",
        "user",
        "ai_dialogue",
        &content,
    );
}

fn latest_user_summary(body: &Value) -> String {
    if let Some(input_arr) = body.get("input").and_then(|v| v.as_array()) {
        for item in input_arr.iter().rev() {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == "user" || item.get("type").and_then(|v| v.as_str()) == Some("message") {
                let text = collect_text(item);
                if !text.trim().is_empty() {
                    return normalize_ws(&text);
                }
            }
        }
    }

    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages.iter().rev() {
            if msg.get("role").and_then(|v| v.as_str()) == Some("user") {
                let text = collect_text(msg);
                if !text.trim().is_empty() {
                    return normalize_ws(&text);
                }
            }
        }
    }

    "<no user text>".to_string()
}

fn collect_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr.iter().map(collect_text).collect::<Vec<_>>().join(" "),
        Value::Object(obj) => {
            if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                return text.to_string();
            }
            if let Some(content) = obj.get("content") {
                return collect_text(content);
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact_text(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    format!(
        "{} ... [TRUNCATED {} -> {} chars]",
        text.chars().take(max_chars).collect::<String>(),
        char_count,
        max_chars
    )
}
