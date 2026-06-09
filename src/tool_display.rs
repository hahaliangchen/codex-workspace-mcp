use serde_json::{Value, json};
use std::collections::HashMap;

const MCP_TOOL_PREFIX: &str = "codex_workspace_mcp__";
const CODEX_STREAMING_TERMINAL_TOOL: &str = "exec_command";
const CODEX_COMPLETED_TERMINAL_TOOL: &str = "exec_command";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDisplayPhase {
    Streaming,
    Completed,
}

#[derive(Debug, Clone)]
pub struct ToolDisplay {
    pub name: String,
    pub suppress_argument_delta: bool,
}

// Codex only knows a limited set of tool names in the Responses stream.
// Unknown MCP tools are disguised as an echo terminal command so the UI can
// show which proxy tool is running without trying to execute unsupported names
// like read_file_lines/search_text. Keep both streaming and completed events on
// exec_command; using run_terminal_cmd in completed history can produce
// "unsupported call" records that poison the next Chat Completions request.
pub fn display_for_tool(
    tool_name: &str,
    route_map: &HashMap<String, (String, String)>,
    phase: ToolDisplayPhase,
) -> ToolDisplay {
    if let Some((parent, _)) = route_map.get(tool_name) {
        return ToolDisplay {
            name: parent.clone(),
            suppress_argument_delta: true,
        };
    }

    if tool_name == "codex_workspace_mcp__shell" || tool_name == "shell" {
        return ToolDisplay {
            name: match phase {
                ToolDisplayPhase::Streaming => CODEX_STREAMING_TERMINAL_TOOL,
                ToolDisplayPhase::Completed => CODEX_COMPLETED_TERMINAL_TOOL,
            }
            .to_string(),
            suppress_argument_delta: false,
        };
    }

    if tool_name.starts_with(MCP_TOOL_PREFIX) {
        return ToolDisplay {
            name: match phase {
                ToolDisplayPhase::Streaming => CODEX_STREAMING_TERMINAL_TOOL,
                ToolDisplayPhase::Completed => CODEX_COMPLETED_TERMINAL_TOOL,
            }
            .to_string(),
            suppress_argument_delta: true,
        };
    }

    ToolDisplay {
        name: tool_name.to_string(),
        suppress_argument_delta: false,
    }
}

pub fn final_arguments(
    call_id: &str,
    tool_name: &str,
    arguments: &str,
    route_map: &HashMap<String, (String, String)>,
) -> String {
    if let Some((parent, real_sub_tool_name)) = route_map.get(tool_name) {
        if parent == tool_name && real_sub_tool_name == tool_name {
            return arguments.to_string();
        }

        let parsed_args = serde_json::from_str::<Value>(arguments).unwrap_or(json!({}));
        let repacked = json!({
            "name": real_sub_tool_name,
            "arguments": parsed_args
        });
        return serde_json::to_string(&repacked).unwrap_or_default();
    }

    if tool_name == "codex_workspace_mcp__shell" || tool_name == "shell" {
        let parsed_args = serde_json::from_str::<Value>(arguments).unwrap_or(json!({}));
        if let Some(justification) = parsed_args.get("justification").and_then(|v| v.as_str()) {
            tracing::info!(
                "   [AGENT] AI is using shell! Justification: {}",
                justification
            );
        }
        if let Some(cmd) = parsed_args.get("command") {
            let repacked = json!({
                "cmd": cmd
            });
            return serde_json::to_string(&repacked).unwrap_or_default();
        }
        return arguments.to_string();
    }

    if tool_name.starts_with(MCP_TOOL_PREFIX) {
        crate::tool_call_registry::insert(call_id, tool_name, arguments);
        return fake_terminal_args(tool_name);
    }

    arguments.to_string()
}

pub fn completed_output_tool(
    call_id: &str,
    tool_name: &str,
    arguments: &str,
    route_map: &HashMap<String, (String, String)>,
) -> (String, String) {
    if let Some((parent, _)) = route_map.get(tool_name) {
        (
            parent.clone(),
            final_arguments(call_id, tool_name, arguments, route_map),
        )
    } else if tool_name.starts_with(MCP_TOOL_PREFIX) {
        crate::tool_call_registry::insert(call_id, tool_name, arguments);
        (
            CODEX_COMPLETED_TERMINAL_TOOL.to_string(),
            fake_terminal_args(tool_name),
        )
    } else {
        (tool_name.to_string(), arguments.to_string())
    }
}

// This is intentionally a Codex-compatible terminal event, not the real tool
// input. The real MCP call payload is recovered by call_id from tool_call_registry.
fn fake_terminal_args(tool_name: &str) -> String {
    let display_name = tool_name.strip_prefix(MCP_TOOL_PREFIX).unwrap_or(tool_name);
    let fake_args = json!({
        "cmd": format!("echo '🤖 Agent 正在调用底层分析工具: {} ...'", display_name)
    });
    serde_json::to_string(&fake_args).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_tools_hide_argument_deltas_while_streaming() {
        let routes = HashMap::new();
        let display = display_for_tool(
            "codex_workspace_mcp__analyze_image",
            &routes,
            ToolDisplayPhase::Streaming,
        );

        assert_eq!(display.name, "exec_command");
        assert!(display.suppress_argument_delta);
    }

    #[test]
    fn mcp_tools_complete_as_codex_terminal_tool() {
        let routes = HashMap::new();
        let display = display_for_tool(
            "codex_workspace_mcp__analyze_image",
            &routes,
            ToolDisplayPhase::Completed,
        );

        assert_eq!(display.name, "exec_command");
        assert!(display.suppress_argument_delta);
    }

    #[test]
    fn completed_mcp_output_uses_fake_terminal_args_but_stores_real_call() {
        let routes = HashMap::new();
        let call_id = "test_completed_mcp_output_uses_fake_terminal_args";
        let tool_name = "codex_workspace_mcp__analyze_image";
        let real_args = r#"{"path":"a.png","detail":"high"}"#;
        let _ = crate::tool_call_registry::take(call_id);

        let (name, args) = completed_output_tool(call_id, tool_name, real_args, &routes);

        assert_eq!(name, "exec_command");
        let parsed: Value = serde_json::from_str(&args).unwrap();
        assert!(parsed["cmd"].as_str().unwrap().contains("analyze_image"));

        let stored = crate::tool_call_registry::take(call_id).unwrap();
        assert_eq!(stored.0, tool_name);
        assert_eq!(stored.1, real_args);
    }

    #[test]
    fn final_mcp_arguments_store_real_call_and_return_fake_terminal_args() {
        let routes = HashMap::new();
        let call_id = "test_final_mcp_arguments_store_real_call";
        let tool_name = "codex_workspace_mcp__read_file";
        let real_args = r#"{"path":"src/main.rs"}"#;
        let _ = crate::tool_call_registry::take(call_id);

        let args = final_arguments(call_id, tool_name, real_args, &routes);

        let parsed: Value = serde_json::from_str(&args).unwrap();
        assert!(parsed["cmd"].as_str().unwrap().contains("read_file"));

        let stored = crate::tool_call_registry::take(call_id).unwrap();
        assert_eq!(stored.0, tool_name);
        assert_eq!(stored.1, real_args);
    }

    #[test]
    fn routed_tools_repack_final_arguments() {
        let mut routes = HashMap::new();
        routes.insert(
            "codex_workspace_mcp__analyze_image".to_string(),
            (
                "mcp__codex_workspace_mcp".to_string(),
                "analyze_image".to_string(),
            ),
        );

        let args = final_arguments(
            "call_1",
            "codex_workspace_mcp__analyze_image",
            r#"{"path":"a.png"}"#,
            &routes,
        );
        let parsed: Value = serde_json::from_str(&args).unwrap();

        assert_eq!(parsed["name"], "analyze_image");
        assert_eq!(parsed["arguments"]["path"], "a.png");
    }

    #[test]
    fn completed_output_keeps_plain_shell_raw() {
        let routes = HashMap::new();
        let (name, args) =
            completed_output_tool("call_1", "shell", r#"{"command":"dir"}"#, &routes);

        assert_eq!(name, "shell");
        assert_eq!(args, r#"{"command":"dir"}"#);
    }
}
