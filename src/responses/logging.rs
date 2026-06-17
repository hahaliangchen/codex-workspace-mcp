use serde_json::Value;

const MAX_REQUEST_LOG_CHARS: usize = 1 * 1024;

pub async fn log_request_body(conversation_id: &str, body: &Value, client_model: &str) {
    let full_req_json = serde_json::to_string_pretty(body).unwrap_or_default();
    crate::proxy_log::write_codex_context(&format!(
        "CODEX REQ\nconversation_id: {}\nmodel: {}\n\n{}",
        conversation_id, client_model, full_req_json
    ))
    .await;

    let input_count = body
        .get("input")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0);
    let summary = latest_user_summary(body);

    tracing::info!(
        "CODEX -> AGENT /v1/responses model={} input_items={} latest_user={}",
        client_model,
        input_count,
        compact_text(&summary, MAX_REQUEST_LOG_CHARS)
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
