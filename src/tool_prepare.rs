use std::collections::HashMap;

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy)]
pub enum ToolFormat {
    ChatCompletions,
    Responses,
}

#[derive(Debug)]
pub struct PreparedTools {
    pub tools: Vec<Value>,
    pub route_map: HashMap<String, (String, String)>,
    pub blocked: Vec<BlockedTool>,
}

#[derive(Debug)]
pub struct BlockedTool {
    pub kind: BlockedToolKind,
    pub value: String,
}

#[derive(Debug)]
pub enum BlockedToolKind {
    Type,
    Name,
}

pub fn prepare_tools_for_model(input_tools: Option<&Value>, format: ToolFormat) -> PreparedTools {
    match format {
        ToolFormat::ChatCompletions => prepare_chat_completions_tools(input_tools),
        ToolFormat::Responses => prepare_responses_tools(input_tools),
    }
}

fn prepare_chat_completions_tools(input_tools: Option<&Value>) -> PreparedTools {
    let mut route_map = HashMap::new();
    let mut blocked = Vec::new();
    let mut converted_tools = Vec::new();

    if let Some(tools) = input_tools.and_then(|v| v.as_array()) {
        for t in tools {
            if !t.is_object() {
                continue;
            }

            if let Some(blocked_tool) = blocked_tool(t) {
                blocked.push(blocked_tool);
                continue;
            }

            if let Some(sub_tools) = t.get("tools").and_then(|v| v.as_array()) {
                let ns_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                for sub_t in sub_tools {
                    push_chat_tool(sub_t, Some(ns_name), &mut route_map, &mut converted_tools);
                }
            } else {
                push_chat_tool(t, None, &mut route_map, &mut converted_tools);
            }
        }
    }

    let mut priority_tools = Vec::new();
    crate::agent::inject_workspace_tools(&mut priority_tools);
    priority_tools.extend(converted_tools);

    PreparedTools {
        tools: priority_tools,
        route_map,
        blocked,
    }
}

fn prepare_responses_tools(input_tools: Option<&Value>) -> PreparedTools {
    let mut route_map = HashMap::new();
    let mut blocked = Vec::new();
    let mut converted_tools = Vec::new();

    if let Some(tools) = input_tools.and_then(|v| v.as_array()) {
        for t in tools {
            if !t.is_object() {
                continue;
            }

            if let Some(blocked_tool) = blocked_tool(t) {
                blocked.push(blocked_tool);
                continue;
            }

            if let Some(sub_tools) = t.get("tools").and_then(|v| v.as_array()) {
                let ns_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                for sub_t in sub_tools {
                    push_responses_tool(sub_t, Some(ns_name), &mut route_map, &mut converted_tools);
                }
            } else {
                push_responses_tool(t, None, &mut route_map, &mut converted_tools);
            }
        }
    }

    let mut priority_tools = workspace_tools_for_responses();
    priority_tools.extend(converted_tools);

    PreparedTools {
        tools: priority_tools,
        route_map,
        blocked,
    }
}

fn workspace_tools_for_responses() -> Vec<Value> {
    let mut tools = Vec::new();
    let definitions = crate::mcp::tool_definitions();
    if let Some(arr) = definitions.as_array() {
        for t in arr {
            let original_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = t.get("description").cloned().unwrap_or_else(|| json!(""));
            let parameters = t.get("inputSchema").cloned().unwrap_or_else(|| {
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })
            });
            tools.push(json!({
                "type": "function",
                "name": format!("codex_workspace_mcp__{}", original_name),
                "description": description,
                "parameters": parameters
            }));
        }
    }
    tools
}

fn blocked_tool(t: &Value) -> Option<BlockedTool> {
    const BLOCKED_TYPES: &[&str] = &["shell", "code_execution", "bash", "computer_use"];
    const BLOCKED_NAMES: &[&str] = &[
        "run_terminal_cmd",
        "execute_command",
        "exec_command",
        "computer_use",
        "bash",
        "mcp__codex_workspace_mcp",
    ];

    let tool_type = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if BLOCKED_TYPES.contains(&tool_type) {
        return Some(BlockedTool {
            kind: BlockedToolKind::Type,
            value: tool_type.to_string(),
        });
    }

    let tool_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let func_name = t
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if BLOCKED_NAMES.contains(&tool_name) {
        return Some(BlockedTool {
            kind: BlockedToolKind::Name,
            value: tool_name.to_string(),
        });
    }
    if BLOCKED_NAMES.contains(&func_name) {
        return Some(BlockedTool {
            kind: BlockedToolKind::Name,
            value: func_name.to_string(),
        });
    }

    None
}

fn push_chat_tool(
    t: &Value,
    prefix: Option<&str>,
    route_map: &mut HashMap<String, (String, String)>,
    converted_tools: &mut Vec<Value>,
) {
    if !t.is_object() {
        return;
    }

    let mut name_val = t.get("name").cloned().unwrap_or(Value::Null);
    if let Some(prefix_str) = prefix {
        if let Some(n) = name_val.as_str() {
            let alias = format!("{}__{}", prefix_str, n);
            route_map.insert(alias.clone(), (prefix_str.to_string(), n.to_string()));
            name_val = json!(alias);
        }
    }

    if t.get("type").and_then(|v| v.as_str()) == Some("function") && t.get("function").is_some() {
        let mut tool_clone = t.clone();
        if prefix.is_some() {
            tool_clone["function"]["name"] = name_val;
        }
        converted_tools.push(tool_clone);
    } else if t.get("type").and_then(|v| v.as_str()) == Some("function") && t.get("name").is_some()
    {
        converted_tools.push(json!({
            "type": "function",
            "function": {
                "name": name_val,
                "description": t.get("description"),
                "parameters": t.get("parameters")
            }
        }));
    } else if t.get("name").is_some() && t.get("type").is_none() {
        converted_tools.push(json!({
            "type": "function",
            "function": {
                "name": name_val,
                "description": t.get("description"),
                "parameters": t.get("input_schema").or_else(|| t.get("parameters"))
            }
        }));
    } else if let Some(type_str) = t.get("type").and_then(|v| v.as_str()) {
        if type_str != "function" && type_str != "namespace" {
            let effective_name = if name_val.is_null() {
                json!(type_str)
            } else {
                name_val.clone()
            };
            let description = t.get("description").cloned().unwrap_or_else(|| json!(""));
            let parameters = t.get("parameters").cloned().unwrap_or_else(|| {
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })
            });
            converted_tools.push(json!({
                "type": "function",
                "function": {
                    "name": effective_name,
                    "description": description,
                    "parameters": parameters
                }
            }));
        }
    }
}

fn push_responses_tool(
    t: &Value,
    prefix: Option<&str>,
    route_map: &mut HashMap<String, (String, String)>,
    converted_tools: &mut Vec<Value>,
) {
    if !t.is_object() {
        return;
    }

    let mut name_val = t
        .get("function")
        .and_then(|f| f.get("name"))
        .cloned()
        .or_else(|| t.get("name").cloned())
        .unwrap_or(Value::Null);
    if let Some(prefix_str) = prefix {
        if let Some(n) = name_val.as_str() {
            let alias = format!("{}__{}", prefix_str, n);
            route_map.insert(alias.clone(), (prefix_str.to_string(), n.to_string()));
            name_val = json!(alias);
        }
    }

    if t.get("type").and_then(|v| v.as_str()) == Some("namespace") {
        return;
    }

    let description = t
        .get("function")
        .and_then(|f| f.get("description"))
        .cloned()
        .or_else(|| t.get("description").cloned())
        .unwrap_or_else(|| json!(""));
    let parameters = t
        .get("function")
        .and_then(|f| f.get("parameters"))
        .cloned()
        .or_else(|| t.get("parameters").cloned())
        .or_else(|| t.get("input_schema").cloned())
        .unwrap_or_else(|| {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        });

    let effective_name = if name_val.is_null() {
        t.get("type").cloned().unwrap_or_else(|| json!("tool"))
    } else {
        name_val
    };

    converted_tools.push(json!({
        "type": "function",
        "name": effective_name,
        "description": description,
        "parameters": parameters
    }));
}
