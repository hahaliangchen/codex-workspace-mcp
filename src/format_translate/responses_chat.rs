use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct OpenAiChatToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

pub fn responses_tools_to_openai_chat_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(|v| v.as_str())?;
            let description = tool
                .get("description")
                .cloned()
                .unwrap_or_else(|| json!(""));
            let parameters = tool.get("parameters").cloned().unwrap_or_else(|| {
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })
            });
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters
                }
            }))
        })
        .collect()
}

pub fn build_openai_chat_request(
    responses_body: &Value,
    upstream_model: &str,
    messages: &[Value],
    tools: &[Value],
) -> Value {
    let mut request = json!({
        "model": upstream_model,
        "stream": false,
        "messages": messages
    });

    for key in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "stop",
    ] {
        if let Some(value) = responses_body.get(key) {
            request[key] = value.clone();
        }
    }
    if let Some(value) = responses_body
        .get("max_tokens")
        .or_else(|| responses_body.get("max_output_tokens"))
    {
        request["max_tokens"] = value.clone();
    }
    if !tools.is_empty() {
        request["tools"] = json!(tools);
    }

    request
}

pub fn responses_body_to_openai_chat_messages(body: &Value) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.trim().is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }

    let input = body.get("input").unwrap_or(&Value::Null);
    match input {
        Value::Array(items) => {
            for item in items {
                append_response_input_item_as_chat_message(&mut messages, item);
            }
        }
        Value::String(text) => messages.push(json!({"role": "user", "content": text})),
        other if !other.is_null() => {
            messages.push(json!({"role": "user", "content": response_content_to_text(other)}));
        }
        _ => {}
    }

    clean_unmatched_tool_calls(&mut messages);
    messages
}

pub fn openai_chat_assistant_message(response: &Value) -> Value {
    response
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .cloned()
        .unwrap_or_else(|| json!({"role": "assistant", "content": ""}))
}

pub fn collect_all_tool_calls_from_openai_chat(
    assistant_message: &Value,
) -> Vec<OpenAiChatToolCall> {
    assistant_message
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let function = item.get("function")?;
            let name = function.get("name").and_then(|v| v.as_str())?.to_string();
            Some(OpenAiChatToolCall {
                call_id: item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name,
                arguments: function
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}")
                    .to_string(),
            })
        })
        .collect()
}

pub fn openai_chat_tool_result_message(tool_call: &OpenAiChatToolCall, content: &str) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": tool_call.call_id,
        "content": content
    })
}

pub fn collect_openai_chat_final_text(assistant_message: &Value) -> String {
    response_content_to_text(assistant_message.get("content").unwrap_or(&Value::Null))
}

fn append_response_input_item_as_chat_message(messages: &mut Vec<Value>, item: &Value) {
    if item.get("type").and_then(|v| v.as_str()) == Some("function_call_output") {
        messages.push(json!({
            "role": "tool",
            "tool_call_id": item.get("call_id").cloned().unwrap_or_else(|| json!("")),
            "content": response_content_to_text(item.get("output").unwrap_or(&Value::Null))
        }));
        return;
    }

    if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let arguments = item
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        messages.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": item.get("call_id").cloned().unwrap_or_else(|| json!("")),
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments
                }
            }]
        }));
        return;
    }

    let role = normalize_openai_chat_role(item.get("role").and_then(|v| v.as_str()));
    let content = response_content_to_text(item.get("content").unwrap_or(item));
    if !content.trim().is_empty() {
        messages.push(json!({"role": role, "content": content}));
    }
}

fn normalize_openai_chat_role(role: Option<&str>) -> &'static str {
    match role {
        Some("system") | Some("developer") => "system",
        Some("assistant") => "assistant",
        Some("tool") => "tool",
        Some("user") | Some("latest_reminder") => "user",
        _ => "user",
    }
}

fn response_content_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part
                    .get("text")
                    .or_else(|| part.get("content"))
                    .and_then(|v| v.as_str())
                {
                    return Some(text.to_string());
                }
                match part.get("type").and_then(|v| v.as_str()) {
                    Some("input_text") | Some("output_text") => part
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(ToOwned::to_owned),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn clean_unmatched_tool_calls(messages: &mut Vec<Value>) {
    let mut valid_messages = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let mut msg = messages[i].clone();

        let role = msg.get("role").and_then(|v| v.as_str());
        if role == Some("assistant") {
            let mut j = i + 1;
            let mut immediate_tool_messages = Vec::new();
            while j < messages.len() {
                if messages[j].get("role").and_then(|v| v.as_str()) == Some("tool") {
                    immediate_tool_messages.push(messages[j].clone());
                    j += 1;
                } else {
                    break;
                }
            }

            let mut matched_tool_call_ids = std::collections::HashSet::new();
            for t in &immediate_tool_messages {
                if let Some(id) = t.get("tool_call_id").and_then(|v| v.as_str()) {
                    matched_tool_call_ids.insert(id.to_string());
                }
            }

            if let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                tool_calls.retain(|tc| {
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    matched_tool_call_ids.contains(id)
                });

                if tool_calls.is_empty() {
                    if let Some(obj) = msg.as_object_mut() {
                        obj.remove("tool_calls");
                        if obj.get("content").map_or(true, |c| c.is_null()) {
                            obj.insert("content".to_string(), json!(""));
                        }
                    }
                }
            }

            valid_messages.push(msg.clone());

            let assistant_tool_calls_now = valid_messages
                .last()
                .unwrap()
                .get("tool_calls")
                .and_then(|v| v.as_array());
            let valid_ids: std::collections::HashSet<String> = if let Some(tc_arr) =
                assistant_tool_calls_now
            {
                tc_arr
                    .iter()
                    .filter_map(|tc| tc.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .collect()
            } else {
                std::collections::HashSet::new()
            };

            for tm in immediate_tool_messages {
                let tid = tm
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if valid_ids.contains(tid) {
                    valid_messages.push(tm);
                }
            }

            i = j;
            continue;
        } else if role == Some("tool") {
            // A tool message without an immediately preceding assistant message is invalid.
            i += 1;
            continue;
        } else {
            valid_messages.push(msg);
            i += 1;
        }
    }

    *messages = valid_messages;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn collects_function_calls_for_known_tools_only() {
        let local_tool_names = HashSet::from(["read_file".to_string()]);
        let calls = collect_all_tool_calls_from_openai_chat(&json!({
            "role": "assistant",
            "tool_calls": [
                {"id":"a","type":"function","function":{"name":"read_file","arguments":"{}"}},
                {"id":"b","type":"function","function":{"name":"external","arguments":"{}"}}
            ]
        }))
        .into_iter()
        .filter(|tool_call| local_tool_names.contains(&tool_call.name))
        .collect::<Vec<_>>();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "a");
    }

    #[test]
    fn final_text_reads_openai_chat_message_content() {
        assert_eq!(
            collect_openai_chat_final_text(&json!({"role":"assistant","content":"done"})),
            "done"
        );
        assert_eq!(
            collect_openai_chat_final_text(
                &json!({"role":"assistant","content":[{"type":"text","text":"hello"}]})
            ),
            "hello"
        );
    }

    #[test]
    fn maps_developer_role_to_system_for_chat_compatibility() {
        let messages = responses_body_to_openai_chat_messages(&json!({
            "input": [{"role": "developer", "content": "follow these rules"}]
        }));

        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "follow these rules");
    }

    #[test]
    fn cleans_unmatched_tool_calls_from_assistant_message() {
        let mut messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "write_file", "arguments": "{}"}}
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "file content"}),
        ];

        clean_unmatched_tool_calls(&mut messages);

        let tool_calls = messages[1]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");

        let mut messages2 = vec![json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {"id": "call_2", "type": "function", "function": {"name": "write_file", "arguments": "{}"}}
            ]
        })];

        clean_unmatched_tool_calls(&mut messages2);

        assert!(messages2[0].get("tool_calls").is_none());
        assert_eq!(messages2[0]["content"], "");
    }
}
