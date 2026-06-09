use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

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
                );
            }
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

            let val = json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments
                        }
                    }
                ]
            });
            current_normal_messages.push(val.clone());
            temp_items.push(TempItem::Normal(val));
        } else {
            temp_items.push(TempItem::None);
        }
    } else if item_type == "function_call_output" {
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
        push_regular_input_item(item, item_role, system_parts, current_normal_messages, temp_items);
    }
}

fn push_regular_input_item(
    item: &Value,
    item_role: &str,
    system_parts: &mut Vec<String>,
    current_normal_messages: &mut Vec<Value>,
    temp_items: &mut Vec<TempItem>,
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

                if item_type == "input_image" || item_type == "image_url" || obj.contains_key("image_url") {
                    if item_type == "image_url" && obj.get("image_url").map_or(false, |v| v.is_object()) {
                        normalized_arr.push(item.clone());
                    } else {
                        let url_str = obj.get("image_url")
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
