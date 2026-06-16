use serde_json::{Value, json};

#[derive(Debug)]
pub struct PreparedTools {
    pub tools: Vec<Value>,
    pub delegated_tools: Vec<Value>,
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

pub fn prepare_responses_tools(input_tools: Option<&Value>) -> PreparedTools {
    let mut blocked = Vec::new();
    let local_tools = workspace_tools_for_responses();
    let local_names = local_tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(|v| v.as_str()))
        .collect::<std::collections::HashSet<_>>();
    let mut delegated_tools = Vec::new();

    if let Some(tools) = input_tools.and_then(|v| v.as_array()) {
        for t in tools {
            if !t.is_object() {
                continue;
            }

            if let Some(blocked_tool) = blocked_tool(t) {
                blocked.push(blocked_tool);
            } else if let Some(sub_tools) = t.get("tools").and_then(|v| v.as_array()) {
                for sub_tool in sub_tools {
                    push_delegated_tool(sub_tool, &local_names, &mut delegated_tools);
                }
            } else {
                push_delegated_tool(t, &local_names, &mut delegated_tools);
            }
        }
    }

    PreparedTools {
        tools: local_tools,
        delegated_tools,
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
            let parameters = with_tool_reason_parameter(parameters);
            tools.push(json!({
                "type": "function",
                "name": original_name,
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

fn push_delegated_tool(
    t: &Value,
    local_names: &std::collections::HashSet<&str>,
    delegated_tools: &mut Vec<Value>,
) {
    let Some(name) = t
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .or_else(|| t.get("name").and_then(|v| v.as_str()))
    else {
        return;
    };

    if local_names.contains(name) {
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
        .or_else(|| t.get("parameters").cloned())
        .unwrap_or_else(|| {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        });
    let parameters = with_tool_reason_parameter(parameters);

    delegated_tools.push(json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": parameters
    }));
}

fn with_tool_reason_parameter(mut parameters: Value) -> Value {
    let Some(object) = parameters.as_object_mut() else {
        return parameters;
    };
    if object.get("type").and_then(|v| v.as_str()).is_none() {
        object.insert("type".to_string(), json!("object"));
    }
    let properties = object.entry("properties").or_insert_with(|| json!({}));
    let Some(properties) = properties.as_object_mut() else {
        return parameters;
    };
    properties.entry("reason".to_string()).or_insert_with(|| {
        json!({
            "type": "string",
            "description": "Short user-visible reason for this tool call. Explain what you are checking or changing before the tool runs."
        })
    });
    parameters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_reason_parameter_to_tool_schema() {
        let schema = with_tool_reason_parameter(json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            }
        }));

        assert_eq!(
            schema["properties"]["reason"]["type"].as_str(),
            Some("string")
        );
        assert_eq!(
            schema["properties"]["path"]["type"].as_str(),
            Some("string")
        );
    }

    #[test]
    fn preserves_existing_reason_parameter() {
        let schema = with_tool_reason_parameter(json!({
            "type": "object",
            "properties": {
                "reason": {"type": "string", "description": "custom"}
            }
        }));

        assert_eq!(
            schema["properties"]["reason"]["description"].as_str(),
            Some("custom")
        );
    }
}
