use serde_json::{Value, json};

pub(super) fn default_usage() -> Value {
    json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "prompt_tokens": 0,
        "completion_tokens": 0,
        "total_tokens": 0
    })
}

pub(super) fn build_response_created_event(response_id: &str, model: &str) -> Value {
    json!({
        "type": "response.created",
        "response": {
            "id": response_id,
            "object": "response",
            "status": "in_progress",
            "model": model,
        }
    })
}

pub(super) fn build_text_item_added_event(response_id: &str) -> Value {
    json!({
        "type": "response.output_item.added",
        "response_id": response_id,
        "output_index": 0,
        "item": {
            "id": text_item_id(response_id),
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "content": []
        }
    })
}

pub(super) fn build_text_part_added_event(response_id: &str) -> Value {
    json!({
        "type": "response.content_part.added",
        "response_id": response_id,
        "output_index": 0,
        "content_index": 0,
        "part": {
            "type": "output_text",
            "text": ""
        }
    })
}

pub(super) fn build_text_delta_event(response_id: &str, delta: &str) -> Value {
    json!({
        "type": "response.output_text.delta",
        "response_id": response_id,
        "item_id": text_item_id(response_id),
        "output_index": 0,
        "content_index": 0,
        "delta": delta
    })
}

pub(super) fn build_text_done_event(response_id: &str, text: &str) -> Value {
    json!({
        "type": "response.output_text.done",
        "response_id": response_id,
        "item_id": text_item_id(response_id),
        "output_index": 0,
        "content_index": 0,
        "text": text
    })
}

pub(super) fn build_text_part_done_event(response_id: &str, text: &str) -> Value {
    json!({
        "type": "response.content_part.done",
        "response_id": response_id,
        "output_index": 0,
        "content_index": 0,
        "part": {
            "type": "output_text",
            "text": text
        }
    })
}

pub(super) fn build_text_item_done_event(response_id: &str, text: &str) -> Value {
    json!({
        "type": "response.output_item.done",
        "response_id": response_id,
        "output_index": 0,
        "item": {
            "id": text_item_id(response_id),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [
                {
                    "type": "output_text",
                    "text": text
                }
            ]
        }
    })
}

pub(super) fn build_tool_item_added_event(
    response_id: &str,
    output_index: usize,
    item_id: &str,
    call_id: &str,
    name: &str,
) -> Value {
    json!({
        "type": "response.output_item.added",
        "response_id": response_id,
        "output_index": output_index,
        "item": {
            "id": item_id,
            "type": "function_call",
            "status": "in_progress",
            "call_id": call_id,
            "name": name,
            "arguments": ""
        }
    })
}

pub(super) fn build_tool_args_delta_event(
    response_id: &str,
    item_id: &str,
    output_index: usize,
    call_id: &str,
    delta: &str,
) -> Value {
    json!({
        "type": "response.function_call.arguments.delta",
        "response_id": response_id,
        "item_id": item_id,
        "output_index": output_index,
        "call_id": call_id,
        "delta": delta
    })
}

pub(super) fn build_tool_args_done_event(
    response_id: &str,
    item_id: &str,
    output_index: usize,
    call_id: &str,
    arguments: &str,
) -> Value {
    json!({
        "type": "response.function_call.arguments.done",
        "response_id": response_id,
        "item_id": item_id,
        "output_index": output_index,
        "call_id": call_id,
        "arguments": arguments
    })
}

pub(super) fn build_tool_item_done_event(
    response_id: &str,
    output_index: usize,
    item_id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Value {
    json!({
        "type": "response.output_item.done",
        "response_id": response_id,
        "output_index": output_index,
        "item": {
            "id": item_id,
            "type": "function_call",
            "status": "completed",
            "call_id": call_id,
            "name": name,
            "arguments": arguments
        }
    })
}

pub(super) fn build_completed_text_output_item(response_id: &str, text: &str) -> Value {
    json!({
        "id": text_item_id(response_id),
        "type": "message",
        "role": "assistant",
        "content": [
            {
                "type": "output_text",
                "text": text
            }
        ]
    })
}

pub(super) fn build_completed_tool_output_item(
    item_id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Value {
    json!({
        "id": item_id,
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "arguments": arguments
    })
}

pub(super) fn build_response_completed_event(
    response_id: &str,
    model: &str,
    usage: Value,
    output: Vec<Value>,
) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "object": "response",
            "status": "completed",
            "model": model,
            "usage": usage,
            "output": output
        }
    })
}

pub(super) fn text_item_id(response_id: &str) -> String {
    format!("item_txt_{}", response_id)
}

pub(super) fn tool_item_id(response_id: &str, idx: usize) -> String {
    format!("item_tool_{}_{}", response_id, idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_delta_event_uses_stable_item_id() {
        let event = build_text_delta_event("resp_1", "hello");

        assert_eq!(event["type"], "response.output_text.delta");
        assert_eq!(event["response_id"], "resp_1");
        assert_eq!(event["item_id"], "item_txt_resp_1");
        assert_eq!(event["delta"], "hello");
    }

    #[test]
    fn tool_args_done_event_preserves_arguments_string() {
        let event = build_tool_args_done_event(
            "resp_1",
            "item_tool_resp_1_0",
            1,
            "call_1",
            r#"{"path":"a.png"}"#,
        );

        assert_eq!(event["type"], "response.function_call.arguments.done");
        assert_eq!(event["item_id"], "item_tool_resp_1_0");
        assert_eq!(event["call_id"], "call_1");
        assert_eq!(event["arguments"], r#"{"path":"a.png"}"#);
    }

    #[test]
    fn completed_event_contains_output_items() {
        let output = vec![build_completed_text_output_item("resp_1", "done")];
        let event = build_response_completed_event("resp_1", "model-a", default_usage(), output);

        assert_eq!(event["type"], "response.completed");
        assert_eq!(event["response"]["id"], "resp_1");
        assert_eq!(event["response"]["model"], "model-a");
        assert_eq!(event["response"]["output"][0]["id"], "item_txt_resp_1");
        assert_eq!(event["response"]["output"][0]["content"][0]["text"], "done");
    }
}
