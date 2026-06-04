use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{error, info, warn};

use crate::go_index::{
    IndexGoWorkspaceRequest, ListGoSymbolsRequest, ReadGoSymbolRequest, SearchGoSymbolsRequest,
};
use crate::memory::{ListWorkMemoryRequest, RecordWorkMemoryRequest, SearchWorkMemoryRequest};
use crate::python_index::{
    IndexPythonWorkspaceRequest, ListPythonSymbolsRequest, ReadPythonSymbolRequest,
    SearchPythonSymbolsRequest,
};
use crate::rust_index::{
    IndexRustWorkspaceRequest, ListRustSymbolsRequest, ReadRustSymbolRequest,
    SearchRustSymbolsRequest,
};
use crate::tools::{
    ListDirRequest, ReadFileLinesRequest, ReadFileRequest, ReplaceRangeRequest, SearchTextRequest,
    Workspace, WorkspaceInfoRequest, WriteFileRequest,
};
use crate::ts_index::{
    IndexTsWorkspaceRequest, ListTsSymbolsRequest, ReadTsSymbolRequest, SearchTsSymbolsRequest,
};

pub async fn handle_mcp_get() -> Response {
    info!("mcp GET probe received");
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [("allow", "POST"), ("accept-post", "application/json")],
        "MCP Streamable HTTP endpoint accepts JSON-RPC over POST\n",
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

pub async fn handle_mcp(
    State(workspace): State<Arc<Workspace>>,
    Json(message): Json<Value>,
) -> Response {
    info!(body = %message, "mcp POST received");

    if message.is_array() {
        warn!("mcp batch request rejected");
        return json_error(None, -32600, "JSON-RPC batches are not supported yet");
    }

    let request = match serde_json::from_value::<JsonRpcRequest>(message) {
        Ok(request) => request,
        Err(error) => {
            error!(%error, "mcp request parse failed");
            return json_error(None, -32700, &format!("invalid JSON-RPC message: {error}"));
        }
    };

    if request.method.is_none() {
        info!(id = ?request.id, "mcp response/empty message acknowledged");
        return StatusCode::ACCEPTED.into_response();
    }

    let id = request.id.clone();
    let is_notification = id.is_none();
    info!(id = ?id, method = ?request.method, notification = is_notification, "mcp json-rpc dispatch");
    let result = dispatch(&workspace, request).await;
    if is_notification {
        return match result {
            Ok(_) => {
                info!("mcp notification accepted");
                StatusCode::ACCEPTED.into_response()
            }
            Err(error) => {
                error!(%error, "mcp notification failed");
                json_error(None, -32000, &error.to_string())
            }
        };
    }

    Json(match result {
        Ok(result) => {
            info!(id = ?id, "mcp json-rpc success");
            JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(result),
                error: None,
            }
        }
        Err(error) => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: {
                error!(%error, "mcp json-rpc failed");
                Some(JsonRpcError {
                    code: -32000,
                    message: error.to_string(),
                })
            },
        },
    })
    .into_response()
}

async fn dispatch(workspace: &Workspace, request: JsonRpcRequest) -> anyhow::Result<Value> {
    if request.jsonrpc.as_deref() != Some("2.0") {
        anyhow::bail!("jsonrpc must be \"2.0\"");
    }

    let method = request
        .method
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing method"))?;

    match method {
        "initialize" => {
            let protocol_version = request
                .params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18");
            Ok(json!({
                "protocolVersion": protocol_version,
                "serverInfo": {
                    "name": "codex-workspace-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "tools": {},
                    "resources": {}
                }
            }))
        }
        "tools/list" => Ok(json!({
            "tools": tool_definitions()
        })),
        "tools/call" => call_tool(workspace, request.params).await,
        "notifications/initialized" => Ok(json!({})),
        "ping" => Ok(json!({})),
        "resources/list" => {
            let w_root = workspace.root().display().to_string();
            let mut resources = vec![
                json!({
                    "uri": "mcp://codex-workspace-mcp/notice",
                    "name": "Notice: Workspace AST Code Index Info",
                    "mimeType": "text/plain",
                    "description": "Notice and instructions for AST code navigation. For code symbol lookups prefer AST index tools."
                }),
                json!({
                    "uri": "mcp://codex-workspace-mcp/ast/status",
                    "name": "AST Indexing Status Summary",
                    "mimeType": "text/plain",
                    "description": "Check the status of Rust, TS, Python, and Go AST code indexing files in the current project workspace."
                })
            ];

            // If Rust index exists, expose it as a resource
            if let Ok(st) = workspace.rust_index_status(IndexRustWorkspaceRequest { workspace_root: w_root.clone() }) {
                if st.exists {
                    resources.push(json!({
                        "uri": "mcp://codex-workspace-mcp/ast/rust/symbols",
                        "name": "Rust AST Symbols Index",
                        "mimeType": "text/plain",
                        "description": "Read all parsed Rust symbols, struct definitions, functions, and method signatures in this project."
                    }));
                }
            }

            // If TS index exists, expose it
            if let Ok(st) = workspace.ts_index_status(IndexTsWorkspaceRequest { workspace_root: w_root.clone() }) {
                if st.exists {
                    resources.push(json!({
                        "uri": "mcp://codex-workspace-mcp/ast/ts/symbols",
                        "name": "TS/JS AST Symbols Index",
                        "mimeType": "text/plain",
                        "description": "Read all parsed TS/JS symbols, class definitions, interfaces, functions, and signatures in this project."
                    }));
                }
            }

            // If Python index exists, expose it
            if let Ok(st) = workspace.python_index_status(IndexPythonWorkspaceRequest { workspace_root: w_root.clone() }) {
                if st.exists {
                    resources.push(json!({
                        "uri": "mcp://codex-workspace-mcp/ast/python/symbols",
                        "name": "Python AST Symbols Index",
                        "mimeType": "text/plain",
                        "description": "Read all parsed Python symbols, class definitions, function signatures, and docstrings in this project."
                    }));
                }
            }

            // If Go index exists, expose it
            if let Ok(st) = workspace.go_index_status(IndexGoWorkspaceRequest { workspace_root: w_root.clone() }) {
                if st.exists {
                    resources.push(json!({
                        "uri": "mcp://codex-workspace-mcp/ast/go/symbols",
                        "name": "Go AST Symbols Index",
                        "mimeType": "text/plain",
                        "description": "Read all parsed Go symbols, struct definitions, interface types, and functions in this project."
                    }));
                }
            }

            // If work memory has records, expose it as a timeline resource
            if let Ok(st) = workspace.list_work_memory(ListWorkMemoryRequest { workspace_root: w_root.clone(), limit: 100 }) {
                if !st.memories.is_empty() {
                    resources.push(json!({
                        "uri": "mcp://codex-workspace-mcp/work-memory",
                        "name": "Developer Work Memory Timeline",
                        "mimeType": "text/plain",
                        "description": "Read the timeline of recorded memories, changes, test logs, and risks in this project workspace."
                    }));
                }
            }

            Ok(json!({
                "resources": resources
            }))
        }
        "resources/read" => {
            let uri = request
                .params
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("resources/read requires params.uri"))?;

            let w_root = workspace.root().display().to_string();

            let text_content = match uri {
                "mcp://codex-workspace-mcp/notice" => {
                    "NOTICE TO AI: This MCP server provides powerful AST-based semantic code indexing tools for Rust, TypeScript/JavaScript, Python, and Go.\n\
                     You can query classes, functions, calls, definitions, and outlines across the workspace.\n\
                     If you need to analyze code, check mcp://codex-workspace-mcp/ast/status first to see which languages are indexed.\n\
                     If a language index is missing, you can run index_<lang>_workspace to build it."
                        .to_string()
                }
                "mcp://codex-workspace-mcp/ast/status" => {
                    let mut md = String::from("# AST Code Indexing Status Summary\n\n");
                    md.push_str("| Language | Index Exists | Files Indexed | Symbols Indexed | Last Generated |\n");
                    md.push_str("| --- | --- | --- | --- | --- |\n");

                    // Rust
                    if let Ok(st) = workspace.rust_index_status(IndexRustWorkspaceRequest { workspace_root: w_root.clone() }) {
                        let gen_time = st.generated_at_unix.map(|u| format!("{}", u)).unwrap_or_else(|| "-".to_string());
                        md.push_str(&format!("| Rust | {} | {} | {} | {} |\n", st.exists, st.files_indexed.unwrap_or(0), st.symbols_indexed.unwrap_or(0), gen_time));
                    }
                    // TS
                    if let Ok(st) = workspace.ts_index_status(IndexTsWorkspaceRequest { workspace_root: w_root.clone() }) {
                        let gen_time = st.generated_at_unix.map(|u| format!("{}", u)).unwrap_or_else(|| "-".to_string());
                        md.push_str(&format!("| TypeScript/JavaScript | {} | {} | {} | {} |\n", st.exists, st.files_indexed.unwrap_or(0), st.symbols_indexed.unwrap_or(0), gen_time));
                    }
                    // Python
                    if let Ok(st) = workspace.python_index_status(IndexPythonWorkspaceRequest { workspace_root: w_root.clone() }) {
                        let gen_time = st.generated_at_unix.map(|u| format!("{}", u)).unwrap_or_else(|| "-".to_string());
                        md.push_str(&format!("| Python | {} | {} | {} | {} |\n", st.exists, st.files_indexed.unwrap_or(0), st.symbols_indexed.unwrap_or(0), gen_time));
                    }
                    // Go
                    if let Ok(st) = workspace.go_index_status(IndexGoWorkspaceRequest { workspace_root: w_root.clone() }) {
                        let gen_time = st.generated_at_unix.map(|u| format!("{}", u)).unwrap_or_else(|| "-".to_string());
                        md.push_str(&format!("| Go | {} | {} | {} | {} |\n", st.exists, st.files_indexed.unwrap_or(0), st.symbols_indexed.unwrap_or(0), gen_time));
                    }
                    md
                }
                "mcp://codex-workspace-mcp/ast/rust/symbols" => {
                    let st = workspace.list_rust_symbols(ListRustSymbolsRequest { workspace_root: w_root.clone(), file_path: None, kind: None })?;
                    let mut md = String::from("# Rust AST Symbols Index\n\n");
                    for sym in st.symbols {
                        let impl_str = sym.impl_type.map(|t| format!(" (impl {})", t)).unwrap_or_default();
                        md.push_str(&format!("- **{}** ({:?}): `{}` in `{}` (L{}-L{}){}\n  > {}\n", 
                            sym.name, sym.kind, sym.signature, sym.file_path, sym.start_line, sym.end_line, impl_str, sym.docstring.trim().replace("\n", "\n  > ")));
                    }
                    md
                }
                "mcp://codex-workspace-mcp/ast/ts/symbols" => {
                    let st = workspace.list_ts_symbols(ListTsSymbolsRequest { workspace_root: w_root.clone(), file_path: None, kind: None })?;
                    let mut md = String::from("# TS/JS AST Symbols Index\n\n");
                    for sym in st.symbols {
                        md.push_str(&format!("- **{}** ({:?}): `{}` in `{}` (L{}-L{})\n  > {}\n", 
                            sym.name, sym.kind, sym.signature, sym.file_path, sym.start_line, sym.end_line, sym.docstring.trim().replace("\n", "\n  > ")));
                    }
                    md
                }
                "mcp://codex-workspace-mcp/ast/python/symbols" => {
                    let st = workspace.list_python_symbols(ListPythonSymbolsRequest { workspace_root: w_root.clone(), file_path: None, kind: None })?;
                    let mut md = String::from("# Python AST Symbols Index\n\n");
                    for sym in st.symbols {
                        md.push_str(&format!("- **{}** ({:?}): `{}` in `{}` (L{}-L{})\n  > {}\n", 
                            sym.name, sym.kind, sym.signature, sym.file_path, sym.start_line, sym.end_line, sym.docstring.trim().replace("\n", "\n  > ")));
                    }
                    md
                }
                "mcp://codex-workspace-mcp/ast/go/symbols" => {
                    let st = workspace.list_go_symbols(ListGoSymbolsRequest { workspace_root: w_root.clone(), file_path: None, kind: None })?;
                    let mut md = String::from("# Go AST Symbols Index\n\n");
                    for sym in st.symbols {
                        md.push_str(&format!("- **{}** ({:?}): `{}` in `{}` (L{}-L{})\n  > {}\n", 
                            sym.name, sym.kind, sym.signature, sym.file_path, sym.start_line, sym.end_line, sym.docstring.trim().replace("\n", "\n  > ")));
                    }
                    md
                }
                "mcp://codex-workspace-mcp/work-memory" => {
                    let st = workspace.list_work_memory(ListWorkMemoryRequest {
                        workspace_root: w_root.clone(),
                        limit: 100,
                    })?;
                    let mut md = String::from("# Developer Work Memory Timeline\n\n");
                    if st.memories.is_empty() {
                        md.push_str("No memories have been recorded in this workspace yet. You can use the `record_work_memory` tool to log your work progress, files changed, and risks.\n");
                    } else {
                        for (idx, mem) in st.memories.iter().enumerate() {
                            let local_time = chrono::DateTime::from_timestamp(mem.time_unix as i64, 0)
                                .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                                .unwrap_or_else(|| format!("Unix Epoch {}", mem.time_unix));

                            md.push_str(&format!("### Memory #{}: {}\n", st.memories.len() - idx, mem.summary));
                            md.push_str(&format!("- **Recorded Time**: {}\n", local_time));
                            if !mem.files_changed.is_empty() {
                                md.push_str(&format!("- **Files Changed**:\n  - {}\n", mem.files_changed.join("\n  - ")));
                            }
                            if !mem.implementation.is_empty() {
                                md.push_str(&format!("- **Implementation Details**:\n  > {}\n", mem.implementation.replace("\n", "\n  > ")));
                            }
                            if !mem.tests.is_empty() {
                                md.push_str(&format!("- **Tests Run**:\n  > {}\n", mem.tests.replace("\n", "\n  > ")));
                            }
                            if !mem.risks.is_empty() {
                                md.push_str(&format!("- **Potential Risks & Blockers**:\n  > {}\n", mem.risks.replace("\n", "\n  > ")));
                            }
                            md.push_str("\n---\n\n");
                        }
                    }
                    md
                }
                _ => anyhow::bail!("unknown resource: {}", uri),
            };

            Ok(json!({
                "contents": [
                    {
                        "uri": uri,
                        "mimeType": "text/plain",
                        "text": text_content
                    }
                ]
            }))
        }
        "resources/templates/list" => Ok(json!({
            "resourceTemplates": []
        })),
        "prompts/list" => Ok(json!({
            "prompts": []
        })),
        _ => anyhow::bail!("unknown method: {}", method),
    }
}

fn json_error(id: Option<Value>, code: i64, message: &str) -> Response {
    Json(JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
        }),
    })
    .into_response()
}

pub async fn call_tool(workspace: &Workspace, params: Value) -> anyhow::Result<Value> {
    let mut name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tools/call requires params.name"))?;
    
    // 智能前缀剥离，确保带前缀和不带前缀的工具名都能 100% 成功执行
    if name.starts_with("codex_workspace_mcp__") {
        name = &name["codex_workspace_mcp__".len()..];
    } else if name.starts_with("mcp__codex_workspace_mcp__") {
        name = &name["mcp__codex_workspace_mcp__".len()..];
    } else if name.starts_with("mcp__codex-workspace-mcp__") {
        name = &name["mcp__codex-workspace-mcp__".len()..];
    } else if name.starts_with("mcp__codex_workspace__") {
        name = &name["mcp__codex_workspace__".len()..];
    } else if name.starts_with("mcp__codex-workspace__") {
        name = &name["mcp__codex-workspace__".len()..];
    }

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let value = match name {
        "workspace_info" => serde_json::to_value(
            workspace.workspace_info(serde_json::from_value::<WorkspaceInfoRequest>(arguments)?)?,
        )?,
        "list_dir" => serde_json::to_value(
            workspace.list_dir(serde_json::from_value::<ListDirRequest>(arguments)?)?,
        )?,
        "read_file" => serde_json::to_value(
            workspace.read_file(serde_json::from_value::<ReadFileRequest>(arguments)?)?,
        )?,
        "read_file_lines" => serde_json::to_value(
            workspace
                .read_file_lines(serde_json::from_value::<ReadFileLinesRequest>(arguments)?)?,
        )?,
        "search_text" => serde_json::to_value(
            workspace.search_text(serde_json::from_value::<SearchTextRequest>(arguments)?)?,
        )?,
        "write_file" => serde_json::to_value(
            workspace.write_file(serde_json::from_value::<WriteFileRequest>(arguments)?)?,
        )?,
        "replace_range" => serde_json::to_value(
            workspace.replace_range(serde_json::from_value::<ReplaceRangeRequest>(arguments)?)?,
        )?,
        "index_go_workspace" => {
            serde_json::to_value(workspace.index_go_workspace(serde_json::from_value::<
                IndexGoWorkspaceRequest,
            >(arguments)?)?)?
        }
        "go_index_status" => {
            serde_json::to_value(workspace.go_index_status(serde_json::from_value::<
                IndexGoWorkspaceRequest,
            >(arguments)?)?)?
        }
        "list_go_symbols" => serde_json::to_value(
            workspace
                .list_go_symbols(serde_json::from_value::<ListGoSymbolsRequest>(arguments)?)?,
        )?,
        "search_go_symbols" => serde_json::to_value(
            workspace
                .search_go_symbols(serde_json::from_value::<SearchGoSymbolsRequest>(arguments)?)?,
        )?,
        "read_go_symbol" => serde_json::to_value(
            workspace.read_go_symbol(serde_json::from_value::<ReadGoSymbolRequest>(arguments)?)?,
        )?,
        "index_rust_workspace" => {
            serde_json::to_value(workspace.index_rust_workspace(serde_json::from_value::<
                IndexRustWorkspaceRequest,
            >(arguments)?)?)?
        }
        "rust_index_status" => {
            serde_json::to_value(workspace.rust_index_status(serde_json::from_value::<
                IndexRustWorkspaceRequest,
            >(arguments)?)?)?
        }
        "list_rust_symbols" => serde_json::to_value(
            workspace
                .list_rust_symbols(serde_json::from_value::<ListRustSymbolsRequest>(arguments)?)?,
        )?,
        "search_rust_symbols" => {
            serde_json::to_value(workspace.search_rust_symbols(serde_json::from_value::<
                SearchRustSymbolsRequest,
            >(arguments)?)?)?
        }
        "read_rust_symbol" => serde_json::to_value(
            workspace
                .read_rust_symbol(serde_json::from_value::<ReadRustSymbolRequest>(arguments)?)?,
        )?,
        "index_ts_workspace" => {
            serde_json::to_value(workspace.index_ts_workspace(serde_json::from_value::<
                IndexTsWorkspaceRequest,
            >(arguments)?)?)?
        }
        "ts_index_status" => {
            serde_json::to_value(workspace.ts_index_status(serde_json::from_value::<
                IndexTsWorkspaceRequest,
            >(arguments)?)?)?
        }
        "list_ts_symbols" => serde_json::to_value(
            workspace
                .list_ts_symbols(serde_json::from_value::<ListTsSymbolsRequest>(arguments)?)?,
        )?,
        "search_ts_symbols" => serde_json::to_value(
            workspace
                .search_ts_symbols(serde_json::from_value::<SearchTsSymbolsRequest>(arguments)?)?,
        )?,
        "read_ts_symbol" => serde_json::to_value(
            workspace.read_ts_symbol(serde_json::from_value::<ReadTsSymbolRequest>(arguments)?)?,
        )?,
        "index_python_workspace" => {
            serde_json::to_value(workspace.index_python_workspace(serde_json::from_value::<
                IndexPythonWorkspaceRequest,
            >(arguments)?)?)?
        }
        "python_index_status" => {
            serde_json::to_value(workspace.python_index_status(serde_json::from_value::<
                IndexPythonWorkspaceRequest,
            >(arguments)?)?)?
        }
        "list_python_symbols" => serde_json::to_value(
            workspace
                .list_python_symbols(serde_json::from_value::<ListPythonSymbolsRequest>(arguments)?)?,
        )?,
        "search_python_symbols" => {
            serde_json::to_value(workspace.search_python_symbols(serde_json::from_value::<
                SearchPythonSymbolsRequest,
            >(arguments)?)?)?
        }
        "read_python_symbol" => serde_json::to_value(
            workspace
                .read_python_symbol(serde_json::from_value::<ReadPythonSymbolRequest>(arguments)?)?,
        )?,
        "record_work_memory" => {
            serde_json::to_value(workspace.record_work_memory(serde_json::from_value::<
                RecordWorkMemoryRequest,
            >(arguments)?)?)?
        }
        "list_work_memory" => serde_json::to_value(
            workspace
                .list_work_memory(serde_json::from_value::<ListWorkMemoryRequest>(arguments)?)?,
        )?,
        "search_work_memory" => {
            serde_json::to_value(workspace.search_work_memory(serde_json::from_value::<
                SearchWorkMemoryRequest,
            >(arguments)?)?)?
        }
        // Skills 按需懒加载：列出所有可用技能（名称+一句话描述）
        "list_skills" => {
            let skills = crate::skills::list_skills();
            serde_json::to_value(skills)?
        }
        // Skills 按需懒加载：读取指定技能的完整 SKILL.md 内容
        "read_skill" => {
            let skill_name = arguments.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let content = crate::skills::read_skill(skill_name)?;
            json!({ "skill": skill_name, "content": content })
        }
        _ => anyhow::bail!("unknown tool: {name}"),
    };

    Ok(json!({
        "content": [
            {
                "type": "text",
                "text": serde_json::to_string_pretty(&value)?
            }
        ],
        "structuredContent": value
    }))
}

pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "workspace_info",
            "description": "Return workspace root, platform, allowed access scope, and ignore summary. Requires workspace_root.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Absolute project directory to use for this call."
                    }
                }
            }
        },
        {
            "name": "list_dir",
            "description": "List a directory inside the workspace with optional recursion and ignore filtering. Use only to understand project layout or locate files by path — for code symbol lookups prefer the index tools (search_go_symbols, search_ts_symbols, search_rust_symbols).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Optional absolute project directory to use for this call. Defaults to the server startup directory."
                    },
                    "path": { "type": "string", "default": "." },
                    "recursive": { "type": "boolean", "default": false },
                    "max_depth": { "type": "integer", "default": 1 },
                    "respect_gitignore": { "type": "boolean", "default": true }
                }
            }
        },
        {
            "name": "read_file",
            "description": "Read a UTF-8 file inside the workspace with a byte limit. Prefer read_go_symbol / read_ts_symbol / read_rust_symbol when you need a specific function or type — they return only the relevant range and include caller/callee context. Use read_file when you need the full file or when no index exists.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Optional absolute project directory to use for this call. Defaults to the server startup directory."
                    },
                    "path": { "type": "string" },
                    "max_bytes": { "type": "integer", "default": 1048576 }
                }
            }
        },
        {
            "name": "read_file_lines",
            "description": "Read a 1-indexed inclusive line range from a UTF-8 file. Use when you already know the exact line numbers (e.g., from an index result). For unknown locations prefer search_go_symbols / search_ts_symbols / search_rust_symbols first.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "start_line", "end_line"],
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Optional absolute project directory to use for this call. Defaults to the server startup directory."
                    },
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                }
            }
        },
        {
            "name": "search_text",
            "description": "Search raw text across workspace files. Best for UI strings, config keys, error messages, literals, or fallback when symbol index tools do not find enough code structure.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Optional absolute project directory to use for this call. Defaults to the server startup directory."
                    },
                    "query": { "type": "string" },
                    "path": { "type": "string", "default": "." },
                    "case_sensitive": { "type": "boolean", "default": false },
                    "max_matches": { "type": "integer", "default": 100 }
                }
            }
        },
        {
            "name": "write_file",
            "description": "Create or overwrite a UTF-8 file inside the workspace.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Optional absolute project directory to use for this call. Defaults to the server startup directory."
                    },
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "create_parent_dirs": { "type": "boolean", "default": true }
                }
            }
        },
        {
            "name": "replace_range",
            "description": "Replace an inclusive 1-indexed line range with optional old-text verification.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "start_line", "end_line", "replacement"],
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Optional absolute project directory to use for this call. Defaults to the server startup directory."
                    },
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" },
                    "replacement": { "type": "string" },
                    "expected_old_text": { "type": "string" }
                }
            }
        },
        {
            "name": "list_go_symbols",
            "description": "List indexed Go symbols with code positions. Note: Go index is automatically built and updated by the server. Prefer this over raw text search when browsing Go file structure, functions, methods, structs, interfaces, or types.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "file_path": { "type": "string" },
                    "kind": { "type": "string", "enum": ["function", "method", "struct", "interface", "type"] }
                }
            }
        },
        {
            "name": "search_go_symbols",
            "description": "Search indexed Go symbols by name, signature, docstring, package, or file path. Note: Go index is automatically built and updated by the server. Prefer this before search_text when investigating Go code structure or locating definitions.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 20 }
                }
            }
        },
        {
            "name": "read_go_symbol",
            "description": "Read an indexed Go symbol's exact code range. Set include_context=true when you need dependency edges, callers, callees, and suggested related symbols. Note: Go index is automatically built by the server.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "symbol_id"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "symbol_id": { "type": "string" },
                    "include_context": { "type": "boolean", "default": false }
                }
            }
        },
        {
            "name": "list_rust_symbols",
            "description": "List indexed Rust symbols with code positions. Note: Rust index is automatically built and updated by the server. Prefer this over raw text search when browsing Rust modules, functions, methods, structs, enums, traits, aliases, consts, or statics.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "file_path": { "type": "string" },
                    "kind": { "type": "string", "enum": ["function", "method", "struct", "enum", "trait", "type_alias", "const", "static", "module"] }
                }
            }
        },
        {
            "name": "search_rust_symbols",
            "description": "Search indexed Rust symbols by name, signature, docstring, module, impl type, or file path. Note: Rust index is automatically built and updated by the server. Prefer this before search_text when investigating Rust code structure or locating definitions.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 20 }
                }
            }
        },
        {
            "name": "read_rust_symbol",
            "description": "Read an indexed Rust symbol's exact code range. Set include_context=true when you need dependency edges, callers, callees, and suggested related symbols. Note: Rust index is automatically built by the server.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "symbol_id"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "symbol_id": { "type": "string" },
                    "include_context": { "type": "boolean", "default": false }
                }
            }
        },
        {
            "name": "list_ts_symbols",
            "description": "List indexed TS/JS symbols with code positions. Note: TS/JS index is automatically built and updated by the server. Prefer this over raw text search when browsing TS/JS file structure, functions, components, classes, methods, interfaces, types, enums, or consts.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "file_path": { "type": "string" },
                    "kind": { "type": "string", "enum": ["function", "arrow_function", "class", "method", "interface", "type_alias", "enum", "const", "component"] }
                }
            }
        },
        {
            "name": "search_ts_symbols",
            "description": "Search indexed TS/JS symbols by name, signature, docstring, imports, exports, or file path. Note: TS/JS index is automatically built and updated by the server. Prefer this before search_text when investigating TS/JS code structure, locating definitions, or following component/function dependencies.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 20 }
                }
            }
        },
        {
            "name": "read_ts_symbol",
            "description": "Read an indexed TS/JS symbol's exact code range. Set include_context=true when you need dependency edges, imports, callers, callees, and suggested related symbols. Note: TS/JS index is automatically built by the server.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "symbol_id"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "symbol_id": { "type": "string" },
                    "include_context": { "type": "boolean", "default": false }
                }
            }
        },
        {
            "name": "list_python_symbols",
            "description": "List indexed Python symbols with code positions. Note: Python index is automatically built and updated by the server. Prefer this over raw text search when browsing Python file structure, functions, methods, or classes.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "file_path": { "type": "string" },
                    "kind": { "type": "string", "enum": ["function", "method", "class"] }
                }
            }
        },
        {
            "name": "search_python_symbols",
            "description": "Search indexed Python symbols by name, signature, docstring, decorator, class name, or file path. Note: Python index is automatically built and updated by the server. Prefer this before search_text when investigating Python code structure or locating definitions.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 20 }
                }
            }
        },
        {
            "name": "read_python_symbol",
            "description": "Read an indexed Python symbol's exact code range. Set include_context=true when you need dependency edges, callers, callees, and suggested related symbols. Note: Python index is automatically built by the server.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "symbol_id"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "symbol_id": { "type": "string" },
                    "include_context": { "type": "boolean", "default": false }
                }
            }
        },
        {
            "name": "record_work_memory",
            "description": "Record a work summary after completing code changes or a significant investigation. Always call this when finishing a task — include what changed, why, and any risks. This is how context is preserved for future sessions.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "summary"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "summary": { "type": "string" },
                    "files_changed": { "type": "array", "items": { "type": "string" }, "default": [] },
                    "implementation": { "type": "string" },
                    "tests": { "type": "string" },
                    "risks": { "type": "string" }
                }
            }
        },
        {
            "name": "list_work_memory",
            "description": "List recent work summaries for a workspace. Call this at the start of a new task to recall what was previously done — saves re-investigating code that was already understood.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "limit": { "type": "integer", "default": 10 }
                }
            }
        },
        {
            "name": "search_work_memory",
            "description": "Search past work summaries by keyword. Call before investigating a topic to check whether prior work already covers it — avoids duplicating effort across sessions.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 10 }
                }
            }
        },
        {
            "name": "list_skills",
            "description": "List all available Codex skills with their names and one-line descriptions. Call this first before specialized tasks (presentations, documents, spreadsheets, images) to discover if a matching skill exists.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "read_skill",
            "description": "Read the full SKILL.md content for a specific skill by name. Use after list_skills to get the complete instructions for a skill before executing it.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill name as returned by list_skills (e.g. 'presentations', 'imagegen', 'documents')."
                    }
                }
            }
        },
        {
            "name": "shell",
            "description": "Execute a shell command. ONLY use this as a LAST RESORT for git operations, npm/cargo builds, running scripts, or system commands. Do NOT use this for reading files, listing directories, or searching code.",
            "inputSchema": {
                "type": "object",
                "required": ["justification", "command"],
                "properties": {
                    "justification": {
                        "type": "string",
                        "description": "You MUST answer this question first: 'Are there alternative native tools (like list_dir / read_file / search_text) for this task?'. If yes, you must explain why you are ignoring them. If no, explain why you MUST use shell."
                    },
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    }
                }
            }
        },
        {
            "name": "spawn_subagent",
            "description": "Spawn a specialized sub-agent with its own clean context in the background to handle a specific code analysis or search sub-task. Use this to save token context size of the main agent.",
            "inputSchema": {
                "type": "object",
                "required": ["role", "task"],
                "properties": {
                    "role": {
                        "type": "string",
                        "description": "The job description / role of the sub-agent (e.g. 'Rust File Parser', 'CSS Style Fixer')."
                    },
                    "task": {
                        "type": "string",
                        "description": "The specific task instructions for the sub-agent to fulfill."
                    }
                }
            }
        },
        {
            "name": "query_logs",
            "description": "Query the structured API and tool execution logs in SQLite from the past 24 hours. Helpful for diagnosing redundant tool calls or connection errors.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "default": 50,
                        "description": "Max log rows to return."
                    },
                    "query": {
                        "type": "string",
                        "description": "Optional SQLite WHERE clause filter (e.g. 'action = \"ERROR\"' or 'message LIKE \"%search%\"')."
                    }
                }
            }
        }
    ])
}
