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
         - Do NOT assume skills exist without calling list_skills first.\n\n\
         ## Rule 4: AST Search & Documentation (Bilingual/Chinese Comments & Memory)\n\
         - **Direct Symbol Search**: Code symbol indexes are built and updated automatically by the server in the background. Do NOT check index status or try to index the workspace; directly use `search_*_symbols` / `list_*_symbols` / `read_*_symbol` to navigate code.\n\
         - **Detailed Chinese Comments**: When adding or updating code (structs, functions, modules), write clear, detailed docstrings/comments in Chinese (or bilingual). These comments are fully indexed and will help you or future agents find these features via keyword symbol searches later.\n\
         - **Detailed Chinese Memory Summaries**: When completing any task, you MUST call `record_work_memory` and write a detailed description of the technical implementation, design choices, and business logic in Chinese. This ensures subsequent agents can quickly retrieve and understand the project context using memory searches.\n"
    );

    constraints
}

#[allow(dead_code)]
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
/// 1. SQLite 记录册查询 → 执行真实特权工具（高优先级）
/// 2. PROXY_PAYLOAD 伪装命令 → 解码并执行真实特权工具（后备降级）
/// 3. codex_workspace_mcp__shell → 返回一个特殊标记，告知上层把它当 run_terminal_cmd 执行
pub async fn intercept_and_execute(
    call_id: &str,
    mut output: String,
    normal_messages: &Vec<Value>,
    workspace: &Arc<Workspace>,
    log: &Arc<std::sync::Mutex<std::fs::File>>,
) -> String {
    let mut found_name = None;
    let mut found_args = None;

    // 1. 优先尝试从本地 SQLite 数据库中恢复原始工具参数
    if let Ok(conn) = crate::database::init_db(workspace.root()) {
        if let Ok(Some((name, args))) = crate::database::get_tool_call(&conn, call_id) {
            crate::ai_proxy::log_write(log, &format!(
                "   [AGENT] SQLite Registry Match: ID '{}' -> tool '{}'", call_id, name
            ));
            found_name = Some(name);
            found_args = Some(args);
            // 消费后删除，保持表干净
            let _ = crate::database::delete_tool_call(&conn, call_id);
        }
    }

    // 2. 如果数据库中没有（例如 Proxy 重启或旧 Payload），降级回从历史消息中解码 hex 密文
    if found_name.is_none() {
        for msg in normal_messages {
            if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        if tc.get("id") == Some(&json!(call_id)) {
                            let name_str = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                            if name_str == "exec_command" || name_str == "run_terminal_cmd" {
                                if let Some(args_str) = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()) {
                                    if let Ok(args_json) = serde_json::from_str::<Value>(args_str) {
                                        let cmd_opt = args_json.get("cmd")
                                            .or_else(|| args_json.get("command"))
                                            .and_then(|v| v.as_str());
                                        if let Some(cmd) = cmd_opt {
                                            if let Some(idx) = cmd.find("# PROXY_PAYLOAD: ") {
                                                let payload_hex = cmd[idx + "# PROXY_PAYLOAD: ".len()..].trim();
                                                let decoded_payload = hex_decode(payload_hex);
                                                if let Some(split_idx) = decoded_payload.find('|') {
                                                    crate::ai_proxy::log_write(log, &format!(
                                                        "   [AGENT] Hex Fallback Match: ID '{}' -> tool '{}'", call_id, &decoded_payload[..split_idx]
                                                    ));
                                                    found_name = Some(decoded_payload[..split_idx].to_string());
                                                    found_args = Some(decoded_payload[split_idx + 1..].to_string());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    if let (Some(name), Some(args_str)) = (found_name, found_args) {
        crate::ai_proxy::log_write(log, &format!(
            "   [AGENT] Executing tool natively: {}", name
        ));
        let arguments = serde_json::from_str(&args_str).unwrap_or_else(|_| json!({}));
        let params = json!({ "name": name, "arguments": arguments });
        match crate::mcp::call_tool(&**workspace, params).await {
            Ok(res) => {
                output = match res.as_str() {
                    Some(s) => s.to_string(),
                    None => serde_json::to_string(&res).unwrap_or_else(|_| res.to_string()),
                };
                crate::ai_proxy::log_write(log, &format!(
                    "   [AGENT] Custom execution succeeded. len={}", output.len()
                ));
            }
            Err(e) => {
                output = format!("Agent execution failed: {}", e);
            }
        }
    }

    output
}

/// 在将历史消息发给大模型前，遍历消息，将伪装的 exec_command / run_terminal_cmd 还原为原生工具调用。
pub fn restore_history(messages: &mut Vec<Value>, workspace_root: &std::path::Path) {
    let conn = crate::database::init_db(workspace_root).ok();

    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(tcs) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                for tc in tcs.iter_mut() {
                    let tc_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let mut restored = false;

                    // 1. 优先从 SQLite 中恢复
                    if !tc_id.is_empty() {
                        if let Some(ref c) = conn {
                            if let Ok(Some((real_name, real_args))) = crate::database::get_tool_call(c, &tc_id) {
                                if let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut()) {
                                    func.insert("name".to_string(), json!(real_name));
                                    func.insert("arguments".to_string(), json!(real_args));
                                    restored = true;
                                }
                            }
                        }
                    }

                    // 2. 数据库中未找到，从消息的命令行 hex 字段中解码恢复 (Fallback)
                    if !restored {
                        if let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut()) {
                            let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            if name == "exec_command" || name == "run_terminal_cmd" {
                                if let Some(args_str) = func.get("arguments").and_then(|v| v.as_str()) {
                                    if let Ok(args_json) = serde_json::from_str::<Value>(args_str) {
                                        let cmd_opt = args_json.get("cmd")
                                            .or_else(|| args_json.get("command"))
                                            .and_then(|v| v.as_str());
                                        if let Some(cmd) = cmd_opt {
                                            if let Some(idx) = cmd.find("# PROXY_PAYLOAD: ") {
                                                let payload_hex = cmd[idx + "# PROXY_PAYLOAD: ".len()..].trim();
                                                let decoded = hex_decode(payload_hex);
                                                if let Some(split_idx) = decoded.find('|') {
                                                    let real_name = &decoded[..split_idx];
                                                    let real_args_str = &decoded[split_idx + 1..];
                                                    
                                                    func.insert("name".to_string(), json!(real_name));
                                                    func.insert("arguments".to_string(), json!(real_args_str));
                                                    restored = true;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // 3. 如果依然未被恢复，但它是一个标准的 exec_command，直接按逻辑还原为 shell 桥接工具
                    if !restored {
                        if let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut()) {
                            let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            if name == "exec_command" || name == "run_terminal_cmd" {
                                if let Some(args_str) = func.get("arguments").and_then(|v| v.as_str()) {
                                    if let Ok(args_json) = serde_json::from_str::<Value>(args_str) {
                                        let cmd_opt = args_json.get("cmd")
                                            .or_else(|| args_json.get("command"))
                                            .and_then(|v| v.as_str());
                                        if let Some(cmd) = cmd_opt {
                                            func.insert("name".to_string(), json!("codex_workspace_mcp__shell"));
                                            func.insert("arguments".to_string(), json!(json!({
                                                "command": cmd,
                                                "justification": "<from history>"
                                            }).to_string()));
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
}
