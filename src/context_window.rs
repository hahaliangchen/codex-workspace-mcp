use anyhow::Result;
use serde_json::{Value, json};

/// Provenance stays beside the message array, never in its original body.
#[derive(Clone,Default)]
pub struct HistoryView {pub messages:Vec<Value>,pub sources:Vec<Option<i64>>}
impl HistoryView {
    pub fn prepend(&mut self,message:Value) {self.messages.insert(0,message);self.sources.insert(0,None);}
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
            if matches!(fields.get("type").and_then(Value::as_str),Some("image_url"|"image_ref")) =>
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

/// Capacity guard only. The receiving role selects necessary history through
/// context_rebuild before this guard; role names never decide what is removed.
pub fn prepare(body:&mut Value,_protected_prefix:usize)->Result<Value> {
    let output=body["max_tokens"].as_u64().unwrap_or(8192) as usize;
    let input=estimate(body);let limit=MAX_TOKENS.saturating_sub(output);
    anyhow::ensure!(input<=limit,"Context exceeds the 256k capacity (estimated {input} input tokens, {output} reserved output tokens); task-directed history selection is required; no original message was truncated");
    Ok(json!({"max_tokens":MAX_TOKENS,"estimated_input_tokens":input,"estimated_before_tokens":input,"omitted_old_messages":0,"mode":"within_window"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn below_capacity_is_unchanged() {
        let mut body=json!({"messages":[{"role":"system","content":"goal"},{"role":"user","content":"原文 \n"}]});
        let original=body.clone();prepare(&mut body,2).unwrap();assert_eq!(body,original);
    }
    #[test]
    fn overflow_never_crops_by_message_role() {
        for role in ["user","assistant","tool"] {
            let mut body=json!({"messages":[{"role":"system","content":"goal"},{"role":role,"content":"旧".repeat(MAX_TOKENS)},{"role":"user","content":"latest"}]});
            let original=body.clone();assert!(prepare(&mut body,2).is_err());assert_eq!(body,original);
        }
    }
    #[test]
    fn image_references_and_wire_images_have_same_budget() {
        assert_eq!(estimate(&json!({"type":"image_ref","artifact_id":"v"})),estimate(&json!({"type":"image_url","image_url":{"url":"data:image/png;base64,abc"}})));
    }
}
