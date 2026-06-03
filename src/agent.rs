use serde_json::{json, Value};
use std::sync::Arc;
use crate::tools::Workspace;
use crate::mcp;

/// 将 Workspace 内置的 MCP 工具注入到提供给大模型的 tools 列表中
/// 这样大模型无需通过 tool_search 即可直接“知道”并调用这些最高优先级的原生工具。
pub fn inject_workspace_tools(converted_tools: &mut Vec<Value>) {
    let definitions = mcp::tool_definitions();
    if let Some(arr) = definitions.as_array() {
        for t in arr {
            let original_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = t.get("description").cloned().unwrap_or_else(|| json!(""));
            let parameters = t.get("inputSchema").cloned().unwrap_or_else(|| json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }));

            // 添加前缀 codex_workspace_mcp__ 避免冲突并标识特权工具
            let new_name = format!("codex_workspace_mcp__{}", original_name);

            converted_tools.push(json!({
                "type": "function",
                "function": {
                    "name": new_name,
                    "description": description,
                    "parameters": parameters
                }
            }));
        }
    }
}

/// 生成大模型的系统提示词约束，显式列出所有高优先级可用工具
pub fn generate_agent_constraints() -> String {
    let mut constraints = String::from(
        "[URGENT AGENT CONSTRAINTS]\n\
         You are operating in a proxied workspace environment with the following strict rules:\n\n\
         ## Rule 1: Workspace Tools (HIGHEST PRIORITY)\n\
         For ALL file reading, code search, directory listing, AST analysis tasks — \
         you MUST use the following native proxy tools. \
         Do NOT use shell commands, do NOT use resources/read:\n\n"
    );

    let definitions = mcp::tool_definitions();
    if let Some(arr) = definitions.as_array() {
        for t in arr {
            let original_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            // 跳过 Skills 工具本身，单独放到 Rule 2 里解释
            if original_name == "list_skills" || original_name == "read_skill" {
                continue;
            }
            let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
            // 只显示前80字节的描述，保持精简
            let short_desc: String = desc.chars().take(80).collect();
            constraints.push_str(&format!("- codex_workspace_mcp__{}: {}\n", original_name, short_desc));
        }
    }

    constraints.push_str(
        "\n## Rule 2: Skills (LAZY LOAD — do NOT guess)\n\
         Skills for specialized tasks (presentations, documents, spreadsheets, images) \
         are NOT pre-loaded. Before attempting such a task:\n\
         1. Call codex_workspace_mcp__list_skills to see what skills are available.\n\
         2. If a matching skill exists, call codex_workspace_mcp__read_skill(name=...) to get full instructions.\n\
         3. Follow the skill instructions exactly.\n\n\
         ## Rule 3: Shell is a fallback, NOT the first choice\n\
         - For workspace exploration (reading files, searching code, listing dirs): ALWAYS use codex_workspace_mcp__ tools first.\n\
         - Shell (run_terminal_cmd) is ONLY for tasks our tools cannot handle: git operations, npm/cargo builds, running scripts, system commands.\n\
         - Do NOT use resources/read for any workspace data.\n\
         - Do NOT assume skills exist without calling list_skills first.\n"
    );

    constraints
}

pub fn hex_encode(s: &str) -> String {
    s.as_bytes().iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn hex_decode(s: &str) -> String {
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&s[i..i.min(s.len()) + 2], 16).ok())
        .collect();
    String::from_utf8(bytes).unwrap_or_default()
}

/// 拦截处理：
/// 1. PROXY_PAYLOAD 伪装命令 → 解码并执行真实特权工具
/// 2. codex_workspace_mcp__shell → 返回一个特殊标记，告知上层把它当 run_terminal_cmd 执行
pub async fn intercept_and_execute(
    call_id: &str,
    mut output: String,
    normal_messages: &Vec<Value>,
    workspace: &Arc<Workspace>,
    log: &Arc<std::sync::Mutex<std::fs::File>>,
) -> String {
    let mut found_args = None;
    let mut found_name = None;

    // 从大模型的历史消息中找出对应 call_id 的工具调用参数
    for msg in normal_messages {
        if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    if tc.get("id") == Some(&json!(call_id)) {
                        found_name = tc.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        found_args = tc.get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        break;
                    }
                }
            }
        }
    }

    if let (Some(name), Some(args_str)) = (found_name, found_args) {
        // === 分支 1：PROXY_PAYLOAD（咱们通过 Shell Hook 伪装的特权工具调用）===
        if name == "run_terminal_cmd" {
            if let Ok(args_json) = serde_json::from_str::<Value>(&args_str) {
                if let Some(command) = args_json.get("command").and_then(|v| v.as_str()) {
                    if let Some(idx) = command.find("# PROXY_PAYLOAD: ") {
                        let payload_hex = command[idx + "# PROXY_PAYLOAD: ".len()..].trim();
                        let decoded_payload = hex_decode(payload_hex);
                        if let Some(split_idx) = decoded_payload.find('|') {
                            let real_name = &decoded_payload[..split_idx];
                            let real_args_str = &decoded_payload[split_idx + 1..];
                            crate::ai_proxy::log_write(log, &format!(
                                "   [AGENT] Shell Hook: '{}' → executing natively", real_name
                            ));
                            let arguments = serde_json::from_str(real_args_str).unwrap_or_else(|_| json!({}));
                            let params = json!({ "name": real_name, "arguments": arguments });
                            match mcp::call_tool(&**workspace, params).await {
                                Ok(res) => {
                                    output = match res.as_str() {
                                        Some(s) => s.to_string(),
                                        None => serde_json::to_string(&res).unwrap_or_else(|_| res.to_string()),
                                    };
                                    crate::ai_proxy::log_write(log, &format!(
                                        "   [AGENT] Shell Hook succeeded. len={}", output.len()
                                    ));
                                }
                                Err(e) => {
                                    output = format!("Agent execution failed: {}", e);
                                }
                            }
                        }
                    } else {
                        // 正常的 run_terminal_cmd 返回时，记录一下 AI 在调用时留下的自证理由
                        if let Some(justification) = args_json.get("justification").and_then(|v| v.as_str()) {
                            crate::ai_proxy::log_write(log, &format!(
                                "   [AGENT] Native Shell Invoked. AI Justification: {}", justification
                            ));
                        }
                    }
                }
            }
        }
    }

    output
}




/// 在将历史消息发给大模型前，遍历消息，将伪装的 run_terminal_cmd 还原为原生工具调用。
pub fn restore_history(messages: &mut Vec<Value>) {
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(tcs) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                for tc in tcs.iter_mut() {
                    if let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut()) {
                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let args_str = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        
                        if name == "run_terminal_cmd" {
                            if let Ok(args_json) = serde_json::from_str::<Value>(&args_str) {
                                if let Some(command) = args_json.get("command").and_then(|v| v.as_str()) {
                                    if let Some(idx) = command.find("# PROXY_PAYLOAD: ") {
                                        let payload_hex = command[idx + "# PROXY_PAYLOAD: ".len()..].trim();
                                        let decoded = hex_decode(payload_hex);
                                        if let Some(split_idx) = decoded.find('|') {
                                            let real_name = &decoded[..split_idx];
                                            let real_args_str = &decoded[split_idx + 1..];
                                            
                                            func.insert("name".to_string(), json!(real_name));
                                            func.insert("arguments".to_string(), json!(real_args_str));
                                        }
                                    } else {
                                        // 正常的 run_terminal_cmd 映射为我们定义的 codex_workspace_mcp__shell
                                        func.insert("name".to_string(), json!("codex_workspace_mcp__shell"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
