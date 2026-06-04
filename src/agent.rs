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
         - **Language-Aware Symbol Search**: Code symbol indexes are built automatically. Do NOT build indexes yourself. FIRST, briefly analyze what programming language the current project uses (e.g., Rust, Go, TS/JS, Python). THEN, strictly use the corresponding language's tools (e.g., `search_rust_symbols` for Rust, `search_go_symbols` for Go) to navigate code.\n\
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

    let conn = crate::database::init_db(workspace.root()).ok();

    // 1. 优先尝试从本地 SQLite 数据库中恢复原始工具参数
    if let Some(ref c) = conn {
        if let Ok(Some((name, args))) = crate::database::get_tool_call(c, call_id) {
            crate::ai_proxy::log_write(log, None, None, None, &format!(
                "   [AGENT] SQLite Registry Match: ID '{}' -> tool '{}'", call_id, name
            ));
            let ts = crate::ai_proxy::now_china();
            let _ = crate::database::insert_detailed_api_log(
                c,
                &ts,
                "TOOL_MATCH",
                "proxy",
                &format!("SQLite Registry Match: ID '{}' -> tool '{}'", call_id, name),
                None
            );
            found_name = Some(name);
            found_args = Some(args);
            // 消费后删除，保持表干净
            let _ = crate::database::delete_tool_call(c, call_id);
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
                                                    crate::ai_proxy::log_write(log, None, None, None, &format!(
                                                        "   [AGENT] Hex Fallback Match: ID '{}' -> tool '{}'", call_id, &decoded_payload[..split_idx]
                                                    ));
                                                    if let Some(ref c) = conn {
                                                        let ts = crate::ai_proxy::now_china();
                                                        let _ = crate::database::insert_detailed_api_log(
                                                            c,
                                                            &ts,
                                                            "TOOL_MATCH",
                                                            "proxy",
                                                            &format!("Hex Fallback Match: ID '{}' -> tool '{}'", call_id, &decoded_payload[..split_idx]),
                                                            None
                                                        );
                                                    }
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

    if let (Some(mut name), Some(args_str)) = (found_name, found_args) {
        if name.starts_with("codex_workspace_mcp__") {
            name = name["codex_workspace_mcp__".len()..].to_string();
        }
        crate::ai_proxy::log_write(log, None, None, None, &format!(
            "   [AGENT] Executing tool natively: {}", name
        ));
        if let Some(ref c) = conn {
            let ts = crate::ai_proxy::now_china();
            let _ = crate::database::insert_detailed_api_log(
                c,
                &ts,
                "TOOL_EXEC",
                "proxy",
                &format!("Executing tool natively: {}", name),
                Some(&args_str)
            );
        }
        let arguments = serde_json::from_str(&args_str).unwrap_or_else(|_| json!({}));

        let _is_custom = name == "query_logs" || name == "spawn_subagent";
        let custom_res = if name == "query_logs" {
            Some(execute_query_logs(conn.as_ref(), &arguments))
        } else if name == "spawn_subagent" {
            Some(execute_spawn_subagent(workspace, &arguments).await)
        } else {
            None
        };

        if let Some(res_val) = custom_res {
            match res_val {
                Ok(val) => {
                    output = val;
                    crate::ai_proxy::log_write(log, None, None, None, &format!(
                        "   [AGENT] Custom execution succeeded (internal). len={}", output.len()
                    ));
                    if let Some(ref c) = conn {
                        let ts = crate::ai_proxy::now_china();
                        let _ = crate::database::insert_detailed_api_log(
                            c,
                            &ts,
                            "TOOL_EXEC",
                            "proxy",
                            &format!("Custom execution succeeded (internal). len={}", output.len()),
                            Some(&output)
                        );
                    }
                }
                Err(e) => {
                    output = format!("Agent execution failed (internal): {}", e);
                    crate::ai_proxy::log_write(log, None, None, None, &format!(
                        "   [AGENT] Custom execution failed (internal). error={}", e
                    ));
                    if let Some(ref c) = conn {
                        let ts = crate::ai_proxy::now_china();
                        let _ = crate::database::insert_detailed_api_log(
                            c,
                            &ts,
                            "ERROR",
                            "proxy",
                            &format!("Custom execution failed (internal) for tool '{}'", name),
                            Some(&output)
                        );
                    }
                }
            }
        } else {
            let params = json!({ "name": name, "arguments": arguments });
            match crate::mcp::call_tool(&**workspace, params).await {
                Ok(res) => {
                    output = match res.as_str() {
                        Some(s) => s.to_string(),
                        None => serde_json::to_string(&res).unwrap_or_else(|_| res.to_string()),
                    };
                    crate::ai_proxy::log_write(log, None, None, None, &format!(
                        "   [AGENT] Custom execution succeeded. len={}", output.len()
                    ));
                    if let Some(ref c) = conn {
                        let ts = crate::ai_proxy::now_china();
                        let _ = crate::database::insert_detailed_api_log(
                            c,
                            &ts,
                            "TOOL_EXEC",
                            "proxy",
                            &format!("Custom execution succeeded. len={}", output.len()),
                            Some(&output)
                        );
                    }
                }
                Err(e) => {
                    output = format!("Agent execution failed: {}", e);
                    crate::ai_proxy::log_write(log, None, None, None, &format!(
                        "   [AGENT] Custom execution failed. error={}", e
                    ));
                    if let Some(ref c) = conn {
                        let ts = crate::ai_proxy::now_china();
                        let _ = crate::database::insert_detailed_api_log(
                            c,
                            &ts,
                            "ERROR",
                            "proxy",
                            &format!("Custom execution failed for tool '{}'", name),
                            Some(&output)
                        );
                    }
                }
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

struct SubagentProvider {
    url: String,
    api_key: String,
    model: String,
}

fn get_subagent_provider() -> anyhow::Result<SubagentProvider> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let ai_config_path = exe_dir.join("ai_proxy_config.json");
    if !ai_config_path.exists() {
        anyhow::bail!("ai_proxy_config.json not found in exe dir");
    }
    let config_content = std::fs::read_to_string(&ai_config_path)?;
    let config: Value = serde_json::from_str(&config_content)?;
    
    let default_provider_name = config.get("default_provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing default_provider in config"))?;
        
    let providers = config.get("providers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("missing providers in config"))?;
        
    let provider = providers.get(default_provider_name)
        .ok_or_else(|| anyhow::anyhow!("default provider not found in providers"))?;
        
    let url = provider.get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing url in provider"))?
        .to_string();
        
    let api_key = provider.get("api_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing api_key in provider"))?
        .to_string();
        
    let model = if let Some(model_map) = provider.get("model_map").and_then(|v| v.as_object()) {
        if let Some(first_upstream) = model_map.values().next().and_then(|v| v.as_str()) {
            first_upstream.to_string()
        } else {
            "gemini-2.5-pro".to_string()
        }
    } else {
        "gemini-2.5-pro".to_string()
    };
    
    Ok(SubagentProvider { url, api_key, model })
}

fn execute_query_logs(
    conn: Option<&rusqlite::Connection>,
    arguments: &Value,
) -> anyhow::Result<String> {
    let conn = conn.ok_or_else(|| anyhow::anyhow!("Database connection not available"))?;
    let limit = arguments.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let query_filter = arguments.get("query").and_then(|v| v.as_str());

    let sql = if let Some(filter) = query_filter {
        format!(
            "SELECT id, time_str, action, role, message, detail FROM api_logs WHERE {} ORDER BY id DESC LIMIT {}",
            filter, limit
        )
    } else {
        format!(
            "SELECT id, time_str, action, role, message, detail FROM api_logs ORDER BY id DESC LIMIT {}",
            limit
        )
    };

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let time_str: String = row.get(1)?;
        let action: String = row.get(2)?;
        let role: String = row.get(3)?;
        let message: String = row.get(4)?;
        let detail: Option<String> = row.get(5)?;
        Ok(json!({
            "id": id,
            "time_str": time_str,
            "action": action,
            "role": role,
            "message": message,
            "detail": detail,
        }))
    })?;

    let mut logs = Vec::new();
    for row in rows {
        logs.push(row?);
    }

    Ok(serde_json::to_string_pretty(&logs)?)
}

async fn execute_local_tool(
    workspace: &Arc<Workspace>,
    name: &str,
    arguments: &Value,
) -> anyhow::Result<String> {
    let mut name = name.to_string();
    if name.starts_with("codex_workspace_mcp__") {
        name = name["codex_workspace_mcp__".len()..].to_string();
    }
    
    if name == "query_logs" {
        let conn = crate::database::init_db(workspace.root()).ok();
        return execute_query_logs(conn.as_ref(), arguments);
    }
    if name == "spawn_subagent" {
        return Box::pin(execute_spawn_subagent(workspace, arguments)).await;
    }
    
    let params = json!({ "name": name, "arguments": arguments });
    let res = crate::mcp::call_tool(&**workspace, params).await?;
    let output = match res.as_str() {
        Some(s) => s.to_string(),
        None => serde_json::to_string(&res).unwrap_or_else(|_| res.to_string()),
    };
    Ok(output)
}

async fn execute_spawn_subagent(
    workspace: &Arc<Workspace>,
    arguments: &Value,
) -> anyhow::Result<String> {
    let role = arguments.get("role").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("missing role"))?;
    let task = arguments.get("task").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("missing task"))?;
    
    let provider_info = get_subagent_provider()?;
    let client = reqwest::Client::new();
    
    let mut system_prompt = format!(
        "You are a specialized sub-agent.\n\
         Your Role: {}\n\
         Your Goal: {}\n\n\
         You can invoke tools. Focus entirely on resolving this specific task and output a concise report/answer when done.",
        role, task
    );
    
    system_prompt.push_str("\n\n");
    system_prompt.push_str(&generate_agent_constraints());
    
    let mut messages = vec![
        json!({
            "role": "system",
            "content": system_prompt
        }),
        json!({
            "role": "user",
            "content": format!("Please start executing the task: {}", task)
        })
    ];
    
    let mut tools = Vec::new();
    inject_workspace_tools(&mut tools);
    
    let upstream_url = format!("{}/chat/completions", provider_info.url);
    
    let max_iterations = 12;
    let mut current_iteration = 0;
    
    loop {
        if current_iteration >= max_iterations {
            return Ok(format!(
                "Sub-agent reached max iteration limit ({}) without finishing. Current context:\n{:?}",
                max_iterations, messages.last()
            ));
        }
        current_iteration += 1;
        
        let request_body = json!({
            "model": provider_info.model,
            "messages": messages,
            "tools": tools,
            "stream": false
        });
        
        let response = client.post(&upstream_url)
            .header("Authorization", format!("Bearer {}", provider_info.api_key))
            .json(&request_body)
            .send()
            .await?;
            
        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Sub-agent upstream API request failed (status {}): {}", status, body_text);
        }
        
        let response_json: Value = response.json().await?;
        let choice = response_json.get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow::anyhow!("Sub-agent: invalid upstream response choices"))?;
            
        let message = choice.get("message")
            .ok_or_else(|| anyhow::anyhow!("Sub-agent: missing message in response"))?;
            
        let assistant_content = message.get("content").and_then(|c| c.as_str());
        let tool_calls = message.get("tool_calls").and_then(|t| t.as_array());
        
        let mut assistant_message_to_add = json!({
            "role": "assistant"
        });
        if let Some(c) = assistant_content {
            assistant_message_to_add["content"] = json!(c);
        } else {
            assistant_message_to_add["content"] = Value::Null;
        }
        if let Some(tc) = tool_calls {
            assistant_message_to_add["tool_calls"] = json!(tc);
        }
        messages.push(assistant_message_to_add);
        
        if let Some(tcs) = tool_calls {
            if tcs.is_empty() {
                if let Some(c) = assistant_content {
                    return Ok(c.to_string());
                }
                anyhow::bail!("Sub-agent returned empty content and empty tool_calls");
            }
            
            let mut futures = Vec::new();
            for (tc_idx, tc) in tcs.iter().enumerate() {
                let tc_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let arguments_str = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("{}").to_string();
                let workspace_clone = workspace.clone();
                
                futures.push(async move {
                    let arguments_val: Value = serde_json::from_str(&arguments_str).unwrap_or(json!({}));
                    let res = execute_local_tool(&workspace_clone, &name, &arguments_val).await;
                    (tc_idx, tc_id, res)
                });
            }
            
            let tool_results = futures::future::join_all(futures).await;
            
            for (_idx, tc_id, result) in tool_results {
                let output = match result {
                    Ok(s) => s,
                    Err(e) => format!("Error executing tool: {}", e),
                };
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tc_id,
                    "content": output
                }));
            }
        } else {
            return Ok(assistant_content.unwrap_or("").to_string());
        }
    }
}
