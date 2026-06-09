use serde_json::{json, Value};

#[derive(Debug, PartialEq)]
pub(super) enum SseData {
    Done,
    Json(Value),
    Ignore,
}

pub(super) fn parse_sse_data_line(line: &str) -> SseData {
    let Some(data) = line.strip_prefix("data: ") else {
        return SseData::Ignore;
    };

    if data == "[DONE]" {
        return SseData::Done;
    }

    serde_json::from_str::<Value>(data)
        .map(SseData::Json)
        .unwrap_or(SseData::Ignore)
}

pub(super) fn first_choice(chunk: &Value) -> Option<&Value> {
    chunk
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
}

pub(super) fn choice_delta(choice: Option<&Value>) -> Option<&Value> {
    choice.and_then(|c| c.get("delta"))
}

pub(super) fn extract_text_delta(delta: &Value) -> Option<&str> {
    delta.get("content").and_then(|c| c.as_str())
}

pub(super) fn extract_tool_deltas(delta: &Value) -> Option<&Value> {
    delta.get("tool_calls")
}

pub(super) fn finish_reason(choice: Option<&Value>) -> Option<&str> {
    choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
}

pub(super) fn extract_usage(chunk: &Value) -> Option<Value> {
    let u = chunk.get("usage")?;
    if u.is_null() {
        return None;
    }

    let prompt = u
        .get("prompt_tokens")
        .or_else(|| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion = u
        .get("completion_tokens")
        .or_else(|| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = u
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(prompt + completion);

    Some(json!({
        "input_tokens": prompt,
        "output_tokens": completion,
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": total
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_usage_accepts_responses_token_names() {
        let chunk = json!({
            "usage": {
                "input_tokens": 7,
                "output_tokens": 5
            }
        });

        let usage = extract_usage(&chunk).unwrap();

        assert_eq!(usage["input_tokens"], 7);
        assert_eq!(usage["prompt_tokens"], 7);
        assert_eq!(usage["output_tokens"], 5);
        assert_eq!(usage["completion_tokens"], 5);
        assert_eq!(usage["total_tokens"], 12);
    }

    #[test]
    fn parse_sse_data_line_detects_done() {
        assert_eq!(parse_sse_data_line("data: [DONE]"), SseData::Done);
    }

    #[test]
    fn parse_sse_data_line_parses_json_payload() {
        let parsed = parse_sse_data_line(r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#);

        match parsed {
            SseData::Json(value) => {
                assert_eq!(extract_text_delta(choice_delta(first_choice(&value)).unwrap()), Some("hi"));
            }
            other => panic!("expected json payload, got {:?}", other),
        }
    }

    #[test]
    fn choice_helpers_extract_delta_and_finish_reason() {
        let chunk = json!({
            "choices": [{
                "delta": {
                    "content": "hello",
                    "tool_calls": [{"index": 0}]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let choice = first_choice(&chunk);
        let delta = choice_delta(choice).unwrap();

        assert_eq!(extract_text_delta(delta), Some("hello"));
        assert!(extract_tool_deltas(delta).is_some());
        assert_eq!(finish_reason(choice), Some("tool_calls"));
    }
}
