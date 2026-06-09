use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::tools::Workspace;

pub async fn prepare_chat_messages(
    body: &Value,
    workspace: Arc<Workspace>,
    log: Arc<Mutex<std::fs::File>>,
) -> Vec<Value> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut normal_messages: Vec<Value> = Vec::new();

    if let Some(inst) = body.get("instructions").and_then(|v| v.as_str()) {
        if !inst.is_empty() {
            system_parts.push(inst.to_owned());
        }
    }

    if let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content").unwrap_or(&Value::Null).clone();

            if role == "system" {
                if let Some(s) = content.as_str() {
                    system_parts.push(s.to_owned());
                } else if let Some(arr) = content.as_array() {
                    for part in arr {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            system_parts.push(t.to_owned());
                        }
                    }
                }
            } else {
                let mut new_msg = json!({
                    "role": role,
                    "content": normalize_message_content(&content)
                });

                if role == "tool" {
                    if let Some(call_id) = msg.get("call_id") {
                        new_msg["tool_call_id"] = call_id.clone();
                    } else if let Some(tool_call_id) = msg.get("tool_call_id") {
                        new_msg["tool_call_id"] = tool_call_id.clone();
                    } else if let Some(id) = msg.get("id") {
                        new_msg["tool_call_id"] = id.clone();
                    }
                }

                if role == "assistant" {
                    if let Some(tcs) = msg.get("tool_calls") {
                        new_msg["tool_calls"] = tcs.clone();
                    }
                }

                normal_messages.push(new_msg);
            }
        }
    }

    let mut temp_items = Vec::new();
    let mut current_normal_messages = normal_messages.clone();
    let mut pending_tool_calls: Vec<Value> = Vec::new();

    if let Some(input_val) = body.get("input") {
        if let Some(input_str) = input_val.as_str() {
            if is_systemish_text(input_str) {
                system_parts.push(input_str.to_owned());
                temp_items.push(TempItem::None);
            } else {
                let val = json!({
                    "role": "user",
                    "content": input_str
                });
                current_normal_messages.push(val.clone());
                temp_items.push(TempItem::Normal(val));
            }
        } else if let Some(input_arr) = input_val.as_array() {
            for item in input_arr {
                push_input_item(
                    item,
                    &mut system_parts,
                    &mut current_normal_messages,
                    &mut temp_items,
                    &mut pending_tool_calls,
                );
            }
            flush_pending_tool_calls(
                &mut pending_tool_calls,
                &mut current_normal_messages,
                &mut temp_items,
            );
        }
    }

    let mut futures = Vec::new();
    for (idx, item) in temp_items.iter().enumerate() {
        if let TempItem::ToolOutput {
            call_id_str,
            initial_output,
            messages_snapshot,
            ..
        } = item
        {
            let call_id_str = call_id_str.clone();
            let initial_output = initial_output.clone();
            let messages_snapshot = messages_snapshot.clone();
            let workspace = workspace.clone();
            let log = log.clone();
            futures.push(async move {
                let final_output = crate::agent::intercept_and_execute(
                    &call_id_str,
                    initial_output,
                    &messages_snapshot,
                    &workspace,
                    &log,
                )
                .await;
                (idx, final_output)
            });
        }
    }

    let results = futures::future::join_all(futures).await;
    let mut final_outputs = std::collections::HashMap::new();
    for (idx, final_output) in results {
        final_outputs.insert(idx, final_output);
    }

    for (idx, item) in temp_items.into_iter().enumerate() {
        match item {
            TempItem::Normal(val) => {
                normal_messages.push(val);
            }
            TempItem::ToolOutput { call_id, .. } => {
                let output = final_outputs.remove(&idx).unwrap_or_default();
                normal_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output
                }));
            }
            TempItem::None => {}
        }
    }

    system_parts.push(crate::agent::generate_agent_constraints());

    let mut final_messages: Vec<Value> = Vec::new();
    if !system_parts.is_empty() {
        let unified_system = system_parts.join("\n\n");
        final_messages.push(json!({
            "role": "system",
            "content": unified_system
        }));
    }
    crate::agent::restore_history(&mut normal_messages, workspace.root());
    normal_messages = sanitize_tool_message_pairs(normal_messages);
    final_messages.extend(normal_messages);
    final_messages
}

enum TempItem {
    Normal(Value),
    ToolOutput {
        call_id: Value,
        call_id_str: String,
        initial_output: String,
        messages_snapshot: Vec<Value>,
    },
    None,
}

fn push_input_item(
    item: &Value,
    system_parts: &mut Vec<String>,
    current_normal_messages: &mut Vec<Value>,
    temp_items: &mut Vec<TempItem>,
    pending_tool_calls: &mut Vec<Value>,
) {
    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let item_role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");

    if item_type == "function_call" {
        if let Some(call_id) = item.get("call_id") {
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = item
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");

            pending_tool_calls.push(json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments
                }
            }));
        } else {
            temp_items.push(TempItem::None);
        }
    } else if item_type == "function_call_output" {
        flush_pending_tool_calls(pending_tool_calls, current_normal_messages, temp_items);

        if let Some(call_id) = item.get("call_id") {
            let output = item
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let call_id_str = call_id.as_str().unwrap_or("").to_string();

            temp_items.push(TempItem::ToolOutput {
                call_id: call_id.clone(),
                call_id_str,
                initial_output: output,
                messages_snapshot: current_normal_messages.clone(),
            });

            current_normal_messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id.clone(),
                "content": ""
            }));
        } else {
            temp_items.push(TempItem::None);
        }
    } else {
        push_regular_input_item(
            item,
            item_role,
            system_parts,
            current_normal_messages,
            temp_items,
            pending_tool_calls,
        );
    }
}

fn flush_pending_tool_calls(
    pending_tool_calls: &mut Vec<Value>,
    current_normal_messages: &mut Vec<Value>,
    temp_items: &mut Vec<TempItem>,
) {
    if pending_tool_calls.is_empty() {
        return;
    }

    let val = json!({
        "role": "assistant",
        "content": null,
        "tool_calls": std::mem::take(pending_tool_calls)
    });
    current_normal_messages.push(val.clone());
    temp_items.push(TempItem::Normal(val));
}

fn push_regular_input_item(
    item: &Value,
    item_role: &str,
    system_parts: &mut Vec<String>,
    current_normal_messages: &mut Vec<Value>,
    temp_items: &mut Vec<TempItem>,
    pending_tool_calls: &mut Vec<Value>,
) {
    let downstream_role = match item_role {
        "developer" => "system",
        "system" => "system",
        "assistant" => "assistant",
        _ => "user",
    };

    let mut pushed_any = false;

    if let Some(content_arr) = item.get("content").and_then(|c| c.as_array()) {
        let mut openai_content_parts = Vec::new();

        for part in content_arr {
            if let Some(t_str) = part.get("text").and_then(|t| t.as_str()) {
                if is_systemish_text(t_str) || downstream_role == "system" {
                    system_parts.push(t_str.to_owned());
                } else {
                    openai_content_parts.push(json!({
                        "type": "text",
                        "text": t_str
                    }));
                }
            } else if part.get("image_url").is_some()
                || part.get("type").and_then(|t| t.as_str()) == Some("image_url")
            {
                openai_content_parts.push(part.clone());
            } else if part.get("type").and_then(|t| t.as_str()) == Some("image") {
                if let Some(source) = part.get("source") {
                    if let (Some(media_type), Some(data)) = (
                        source.get("media_type").and_then(|v| v.as_str()),
                        source.get("data").and_then(|v| v.as_str()),
                    ) {
                        openai_content_parts.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", media_type, data)
                            }
                        }));
                    }
                }
            }
        }

        if !openai_content_parts.is_empty() && downstream_role != "system" {
            flush_pending_tool_calls(pending_tool_calls, current_normal_messages, temp_items);
            let val = json!({
                "role": downstream_role,
                "content": normalize_message_content(&json!(openai_content_parts))
            });
            current_normal_messages.push(val.clone());
            temp_items.push(TempItem::Normal(val));
            pushed_any = true;
        }
    }

    if let Some(t_str) = item.get("text").and_then(|t| t.as_str()) {
        if is_systemish_text(t_str) {
            system_parts.push(t_str.to_owned());
        } else {
            flush_pending_tool_calls(pending_tool_calls, current_normal_messages, temp_items);
            let val = json!({
                "role": downstream_role,
                "content": t_str
            });
            current_normal_messages.push(val.clone());
            temp_items.push(TempItem::Normal(val));
            pushed_any = true;
        }
    }

    if !pushed_any {
        temp_items.push(TempItem::None);
    }
}

fn normalize_message_content(content: &Value) -> Value {
    if let Some(arr) = content.as_array() {
        let mut normalized_arr = Vec::new();
        for item in arr {
            if let Some(obj) = item.as_object() {
                let item_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

                if item_type == "input_image"
                    || item_type == "image_url"
                    || obj.contains_key("image_url")
                {
                    if item_type == "image_url"
                        && obj.get("image_url").map_or(false, |v| v.is_object())
                    {
                        normalized_arr.push(item.clone());
                    } else {
                        let url_str = obj
                            .get("image_url")
                            .and_then(|v| v.as_str())
                            .or_else(|| obj.get("url").and_then(|v| v.as_str()))
                            .unwrap_or("");

                        let detail = obj.get("detail").and_then(|v| v.as_str()).unwrap_or("auto");

                        normalized_arr.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": url_str,
                                "detail": detail
                            }
                        }));
                    }
                } else if item_type == "image" {
                    if let Some(source) = obj.get("source") {
                        if let (Some(media_type), Some(data)) = (
                            source.get("media_type").and_then(|v| v.as_str()),
                            source.get("data").and_then(|v| v.as_str()),
                        ) {
                            normalized_arr.push(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", media_type, data)
                                }
                            }));
                        } else {
                            normalized_arr.push(item.clone());
                        }
                    } else {
                        normalized_arr.push(item.clone());
                    }
                } else {
                    normalized_arr.push(item.clone());
                }
            } else {
                normalized_arr.push(item.clone());
            }
        }
        Value::Array(normalized_arr)
    } else {
        content.clone()
    }
}

fn is_systemish_text(text: &str) -> bool {
    text.contains("<permissions instructions>")
        || text.contains("<skills_instructions>")
        || text.contains("<app-context>")
        || text.contains("<system-reminder>")
}

fn sanitize_tool_message_pairs(messages: Vec<Value>) -> Vec<Value> {
    let mut sanitized = Vec::new();
    let mut idx = 0;

    while idx < messages.len() {
        let msg = &messages[idx];
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

        if role == "tool" {
            idx += 1;
            continue;
        }

        let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) else {
            sanitized.push(msg.clone());
            idx += 1;
            continue;
        };

        if tool_calls.is_empty() {
            let mut without_empty_tool_calls = msg.clone();
            if let Some(obj) = without_empty_tool_calls.as_object_mut() {
                obj.remove("tool_calls");
            }
            sanitized.push(without_empty_tool_calls);
            idx += 1;
            continue;
        }

        let mut tool_messages = Vec::new();
        let mut cursor = idx + 1;
        let mut valid_pair = true;

        for tool_call in tool_calls {
            let expected_id = tool_call.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let Some(next_msg) = messages.get(cursor) else {
                valid_pair = false;
                break;
            };
            let is_matching_tool = next_msg.get("role").and_then(|v| v.as_str()) == Some("tool")
                && next_msg
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    == expected_id;
            if !is_matching_tool {
                valid_pair = false;
                break;
            }
            tool_messages.push(next_msg.clone());
            cursor += 1;
        }

        if valid_pair {
            sanitized.push(msg.clone());
            sanitized.extend(tool_messages);
            idx = cursor;
        } else {
            sanitized.push(tool_calls_summary_message(tool_calls));
            idx += 1;
        }
    }

    sanitized
}

fn tool_calls_summary_message(tool_calls: &[Value]) -> Value {
    let mut names = Vec::new();
    for tool_call in tool_calls {
        let name = tool_call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .or_else(|| tool_call.get("name").and_then(|v| v.as_str()))
            .unwrap_or("unknown_tool");
        names.push(name.to_string());
    }

    json!({
        "role": "assistant",
        "content": format!(
            "[Proxy sanitized invalid tool-call history: skipped unmatched tool call(s): {}]",
            names.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_tool_calls_across_empty_input_items() {
        let mut system_parts = Vec::new();
        let mut current_normal_messages = Vec::new();
        let mut temp_items = Vec::new();
        let mut pending_tool_calls = Vec::new();

        let call_a = json!({
            "type": "function_call",
            "call_id": "call_a",
            "name": "exec_command",
            "arguments": "{\"cmd\":\"echo a\"}"
        });
        let empty_reasoning_placeholder = json!({
            "type": "reasoning"
        });
        let call_b = json!({
            "type": "function_call",
            "call_id": "call_b",
            "name": "exec_command",
            "arguments": "{\"cmd\":\"echo b\"}"
        });
        let output_a = json!({
            "type": "function_call_output",
            "call_id": "call_a",
            "output": "a"
        });

        for item in [&call_a, &empty_reasoning_placeholder, &call_b, &output_a] {
            push_input_item(
                item,
                &mut system_parts,
                &mut current_normal_messages,
                &mut temp_items,
                &mut pending_tool_calls,
            );
        }

        let assistant_calls = current_normal_messages
            .iter()
            .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .collect::<Vec<_>>();

        assert_eq!(assistant_calls.len(), 1);
        assert_eq!(
            assistant_calls[0]["tool_calls"].as_array().unwrap().len(),
            2
        );
        assert_eq!(current_normal_messages[1]["role"], "tool");
        assert_eq!(current_normal_messages[1]["tool_call_id"], "call_a");
    }

    #[test]
    fn sanitizes_unmatched_assistant_tool_calls_to_text() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_a",
                    "type": "function",
                    "function": {
                        "name": "read_file_lines",
                        "arguments": "{}"
                    }
                }]
            }),
            json!({
                "role": "assistant",
                "content": "continued"
            }),
            json!({
                "role": "tool",
                "tool_call_id": "orphan",
                "content": "ignored"
            }),
        ];

        let sanitized = sanitize_tool_message_pairs(messages);

        assert_eq!(sanitized.len(), 2);
        assert!(sanitized[0].get("tool_calls").is_none());
        assert!(
            sanitized[0]["content"]
                .as_str()
                .unwrap()
                .contains("read_file_lines")
        );
        assert_eq!(sanitized[1]["content"], "continued");
    }

    #[test]
    fn keeps_valid_assistant_tool_call_pairs() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_a",
                    "type": "function",
                    "function": {
                        "name": "exec_command",
                        "arguments": "{}"
                    }
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_a",
                "content": "ok"
            }),
        ];

        let sanitized = sanitize_tool_message_pairs(messages);

        assert_eq!(sanitized.len(), 2);
        assert_eq!(sanitized[0]["tool_calls"][0]["id"], "call_a");
        assert_eq!(sanitized[1]["tool_call_id"], "call_a");
    }
}
