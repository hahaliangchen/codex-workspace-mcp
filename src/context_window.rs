use anyhow::Result;
use serde_json::{Value, json};

/// Maximum field of view, not a target request size. Providers do not share a
/// tokenizer; use a conservative text estimate and reserve room for output.
pub const MAX_TOKENS: usize = 256_000;
pub const HISTORY_GUIDANCE: &str = "Earlier conversation remains in read_session_history. Keep the current task and newest messages; retrieve only earlier facts needed for the next decision. The 256k context window is a maximum field of view, not a size to fill. Do not reconstruct all history merely because it exists.";

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
            if fields.get("type").and_then(Value::as_str) == Some("image_url") =>
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

/// On overflow, stop replaying old messages; keep the task and latest complete
/// assistant/tool exchange. The model chooses any needed historical reads.
/// Never shorten a newly delivered message or split a tool-call/result group.
pub fn prepare(body: &mut Value, protected_prefix: usize) -> Result<Value> {
    let before = estimate(body);
    let output = body["max_tokens"].as_u64().unwrap_or(8192) as usize;
    let limit = MAX_TOKENS.saturating_sub(output);
    let mut omitted = 0;
    if before > limit {
        let messages = body["messages"]
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("missing context messages"))?;
        let newest = (protected_prefix..messages.len())
            .rev()
            .find(|&index| messages[index]["role"] == "assistant")
            .unwrap_or(protected_prefix);
        if newest > protected_prefix {
            omitted = newest - protected_prefix;
            messages.drain(protected_prefix..newest);
            messages.insert(
                protected_prefix,
                json!({"role":"system","content":HISTORY_GUIDANCE}),
            );
        }
    }
    let after = estimate(body);
    anyhow::ensure!(
        after <= limit,
        "Current task and newest messages exceed the 256k context capacity (estimated {after} input tokens, {output} reserved output tokens); no new message was truncated"
    );
    Ok(
        json!({"max_tokens":MAX_TOKENS,"estimated_input_tokens":after,
        "estimated_before_tokens":before,"omitted_old_messages":omitted,
        "mode":if omitted > 0 {"history_on_demand"}else{"within_window"}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn below_capacity_keeps_long_new_message_and_all_earlier_exchanges() {
        let mut body = json!({"messages":[{"role":"system","content":"task"},
            {"role":"user","content":"goal"},{"role":"assistant","content":"earlier"},
            {"role":"system","content":"失败详情".repeat(5000)}]});
        let original = body.clone();
        prepare(&mut body, 2).unwrap();
        assert_eq!(body, original);
    }
    #[test]
    fn overflow_keeps_latest_exchange_and_new_failure_instead_of_filling_window() {
        let latest = json!({"role":"assistant","tool_calls":[{"id":"new"}]});
        let result = json!({"role":"tool","tool_call_id":"new","content":"original failure"});
        let advice = json!({"role":"system","content":"new Observer message"});
        let mut body = json!({"messages":[{"role":"system","content":"task"},{"role":"user","content":"goal"},
            {"role":"assistant","content":"旧".repeat(MAX_TOKENS)},latest,result,advice]});
        let metadata = prepare(&mut body, 2).unwrap();
        assert_eq!(metadata["mode"], "history_on_demand");
        assert_eq!(
            &body["messages"].as_array().unwrap()[3..],
            &[latest, result, advice]
        );
        assert!(estimate(&body) < 1000);
    }
    #[test]
    fn oversized_current_message_is_reported_without_silently_cutting_it() {
        let mut body = json!({"messages":[{"role":"system","content":"task"},
            {"role":"user","content":"新".repeat(MAX_TOKENS)}]});
        let original = body.clone();
        assert!(prepare(&mut body, 2).is_err());
        assert_eq!(body, original);
    }
}
