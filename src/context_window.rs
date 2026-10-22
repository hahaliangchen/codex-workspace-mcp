use anyhow::Result;
use serde_json::{Value, json};

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct TokenCalibration {
    route: String,
    ratio: Option<f64>,
}

impl TokenCalibration {
    pub fn estimate(&self, value: &Value, route: &str) -> usize {
        let ratio = if self.route == route { self.ratio.unwrap_or(1.0) } else { 1.0 };
        (estimate(value) as f64 * ratio).ceil() as usize
    }

    pub fn observe(&mut self, route: &str, estimated: usize, actual: usize) {
        if estimated == 0 || actual == 0 { return; }
        self.route = route.to_owned();
        // Include modest headroom for tokenizer/framing differences. Missing or
        // implausible usage must not make the host's estimate unbounded.
        self.ratio = Some((actual as f64 / estimated as f64 * 1.05).clamp(0.1, 10.0));
    }
}

/// Provenance stays beside the message array, never in its original body.
#[derive(Clone, Default)]
pub struct HistoryView {
    pub messages: Vec<Value>,
    pub sources: Vec<Option<i64>>,
}
impl HistoryView {
    pub fn prepend(&mut self, message: Value) {
        self.messages.insert(0, message);
        self.sources.insert(0, None);
    }
}

/// Maximum field of view, not a target request size. Providers do not share a
/// tokenizer; use a conservative text estimate and reserve room for output.
pub const MAX_TOKENS: usize = 256_000;
pub fn estimate(value: &Value) -> usize {
    match value {
        Value::String(text) => {
            let ascii = text.bytes().filter(u8::is_ascii).count();
            let unicode = text.chars().filter(|ch| !ch.is_ascii()).count();
            ascii.div_ceil(3) + unicode * 2
        }
        Value::Array(items) => items.iter().map(estimate).sum::<usize>() + items.len() * 4,
        // Image transport bytes are not text tokens. This is an allowance for
        // the supported task image input, not a count of its base64 encoding.
        Value::Object(fields)
            if matches!(
                fields.get("type").and_then(Value::as_str),
                Some("image_url" | "image_ref")
            ) =>
        {
            4096
        }
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| key.len().div_ceil(3) + estimate(value) + 4)
            .sum(),
        _ => 4,
    }
}

/// Restore chronological history order, then guard capacity. The receiving role
/// selects necessary history through context_rebuild; roles never decide what
/// original content is removed.
pub fn prepare(body: &mut Value, _protected_prefix: usize) -> Result<Value> {
    if let Some(Value::Array(messages)) = body.get_mut("messages") {
        crate::session_history::order_context_messages(messages);
        for message in messages {
            if let Some(fields) = message.as_object_mut() {
                fields.remove("source");
            }
        }
    }
    let output = body["max_tokens"].as_u64().unwrap_or(8192) as usize;
    let input = estimate(body);
    let limit = MAX_TOKENS.saturating_sub(output);
    anyhow::ensure!(
        input <= limit,
        "Context exceeds the 256k capacity (estimated {input} input tokens, {output} reserved output tokens); task-directed history selection is required; no original message was truncated"
    );
    Ok(
        json!({"max_tokens":MAX_TOKENS,"estimated_input_tokens":input,"estimated_before_tokens":input,"omitted_old_messages":0,"mode":"within_window"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calibration_is_scoped_to_provider_model_and_ignores_missing_usage() {
        let mut calibration = TokenCalibration::default();
        let input = json!({"messages":[{"role":"user","content":"中文材料".repeat(500)}]});
        let raw = estimate(&input);
        assert_eq!(calibration.estimate(&input,"a/model"), raw);
        calibration.observe("a/model", raw, raw / 3);
        let adjusted = calibration.estimate(&input,"a/model");
        assert!(adjusted < raw / 2 && adjusted >= raw / 3);
        assert_eq!(calibration.estimate(&input,"b/model"), raw);
        calibration.observe("a/model", raw, 0);
        assert_eq!(calibration.estimate(&input,"a/model"), adjusted);
        let restored: TokenCalibration = serde_json::from_value(serde_json::to_value(&calibration).unwrap()).unwrap();
        assert_eq!(restored.estimate(&input,"a/model"), adjusted);
    }
    #[test]
    fn below_capacity_is_unchanged() {
        let mut body = json!({"messages":[{"role":"system","content":"goal"},{"role":"user","content":"原文 \n"}]});
        let original = body.clone();
        prepare(&mut body, 2).unwrap();
        assert_eq!(body, original);
    }
    #[test]
    fn overflow_never_crops_by_message_role() {
        for role in ["user", "assistant", "tool"] {
            let mut body = json!({"messages":[{"role":"system","content":"goal"},{"role":role,"content":"旧".repeat(MAX_TOKENS)},{"role":"user","content":"latest"}]});
            let original = body.clone();
            assert!(prepare(&mut body, 2).is_err());
            assert_eq!(body, original);
        }
    }
    #[test]
    fn image_references_and_wire_images_have_same_budget() {
        assert_eq!(
            estimate(&json!({"type":"image_ref","artifact_id":"v"})),
            estimate(&json!({"type":"image_url","image_url":{"url":"data:image/png;base64,abc"}}))
        );
    }
}
