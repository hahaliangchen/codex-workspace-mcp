use serde_json::Value;
use std::sync::Mutex;

const MAX_RECORDED_TURN_CHARS: usize = 8 * 1024;
const MAX_REQUEST_LOG_CHARS: usize = 1 * 1024;
const MAX_DIAG_ITEMS: usize = 24;

pub fn log_request_body(
    log: &Mutex<std::fs::File>,
    conversation_id: &str,
    body: &Value,
    client_model: &str,
) {
    record_conversation_turn(conversation_id, body, client_model);
    log_codex_shape(log, body);

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

fn log_codex_shape(log: &Mutex<std::fs::File>, body: &Value) {
    let top_keys = body
        .as_object()
        .map(|obj| {
            let mut keys = obj.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys
        })
        .unwrap_or_default();

    let client_metadata_keys = sorted_object_keys(body.get("client_metadata"));
    let metadata_keys = sorted_object_keys(body.get("metadata"));
    let previous_response_id = body
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .unwrap_or("<none>");
    let conversation = body
        .get("conversation")
        .and_then(|v| v.as_str())
        .unwrap_or("<none>");
    let top_id = body.get("id").and_then(|v| v.as_str()).unwrap_or("<none>");
    let installation_id = body
        .get("client_metadata")
        .and_then(|v| v.get("x-codex-installation-id"))
        .and_then(|v| v.as_str())
        .unwrap_or("<none>");

    let input_summary = summarize_items(body.get("input"));
    let messages_summary = summarize_items(body.get("messages"));

    crate::ai_proxy::log_write(
        log,
        true,
        Some("CODEX_SHAPE"),
        Some("diagnostic"),
        &format!(
            "top_keys={}; client_metadata_keys={}; metadata_keys={}; id={}; conversation={}; previous_response_id={}; installation_id={}; input={}; messages={}",
            top_keys.join(","),
            client_metadata_keys.join(","),
            metadata_keys.join(","),
            top_id,
            conversation,
            previous_response_id,
            installation_id,
            input_summary,
            messages_summary
        ),
    );
}

fn sorted_object_keys(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            let mut keys = obj.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys
        })
        .unwrap_or_default()
}

fn summarize_items(value: Option<&Value>) -> String {
    let Some(items) = value.and_then(|v| v.as_array()) else {
        return "<none>".to_string();
    };

    let mut parts = Vec::new();
    for (index, item) in items.iter().take(MAX_DIAG_ITEMS).enumerate() {
        parts.push(format!("{}:{}", index, summarize_item_shape(item)));
    }
    if items.len() > MAX_DIAG_ITEMS {
        parts.push(format!("...+{}", items.len() - MAX_DIAG_ITEMS));
    }

    format!("len={} [{}]", items.len(), parts.join("; "))
}

fn summarize_item_shape(item: &Value) -> String {
    let Some(obj) = item.as_object() else {
        return value_kind(item).to_string();
    };

    let item_type = obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("<no-type>");
    let role = obj
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("<no-role>");
    let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("<no-id>");
    let call_id = obj
        .get("call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("<no-call-id>");
    let content_shape = summarize_content_shape(obj.get("content"));

    format!(
        "type={} role={} id={} call_id={} content={}",
        item_type, role, id, call_id, content_shape
    )
}

fn summarize_content_shape(value: Option<&Value>) -> String {
    match value {
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for (index, item) in items.iter().take(8).enumerate() {
                if let Some(obj) = item.as_object() {
                    let item_type = obj
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("<no-type>");
                    let has_image = obj.contains_key("image_url")
                        || obj
                            .get("type")
                            .and_then(|v| v.as_str())
                            .map(|t| matches!(t, "input_image" | "image_url" | "image"))
                            .unwrap_or(false);
                    parts.push(format!("{}:{} image={}", index, item_type, has_image));
                } else {
                    parts.push(format!("{}:{}", index, value_kind(item)));
                }
            }
            if items.len() > 8 {
                parts.push(format!("...+{}", items.len() - 8));
            }
            format!("array(len={} [{}])", items.len(), parts.join(", "))
        }
        Some(Value::String(text)) => format!("string(chars={})", text.chars().count()),
        Some(other) => value_kind(other).to_string(),
        None => "<none>".to_string(),
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
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
