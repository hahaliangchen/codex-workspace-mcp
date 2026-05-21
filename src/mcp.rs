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
                    "tools": {}
                }
            }))
        }
        "tools/list" => Ok(json!({
            "tools": tool_definitions()
        })),
        "tools/call" => call_tool(workspace, request.params).await,
        "notifications/initialized" => Ok(json!({})),
        "ping" => Ok(json!({})),
        "resources/list" => Ok(json!({
            "resources": []
        })),
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

async fn call_tool(workspace: &Workspace, params: Value) -> anyhow::Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tools/call requires params.name"))?;
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

fn tool_definitions() -> Value {
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
            "description": "List a directory inside the workspace with optional recursion and ignore filtering.",
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
            "description": "Read a UTF-8 file inside the workspace with a byte limit.",
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
            "description": "Read a 1-indexed inclusive line range from a UTF-8 file.",
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
            "description": "Search text across workspace files and return structured matches.",
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
            "name": "index_go_workspace",
            "description": "Build or rebuild the Go symbol index for the selected workspace.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." }
                }
            }
        },
        {
            "name": "go_index_status",
            "description": "Check whether the Go symbol index exists for the selected workspace.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." }
                }
            }
        },
        {
            "name": "list_go_symbols",
            "description": "List Go symbols with optional file and kind filters. Builds the Go index automatically if missing.",
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
            "description": "Search Go symbols by name, signature, docstring, or file path. Builds the Go index automatically if missing.",
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
            "description": "Read an indexed Go symbol with optional caller/callee context.",
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
            "name": "index_ts_workspace",
            "description": "Build or rebuild the TS/JS symbol index for the selected workspace.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." }
                }
            }
        },
        {
            "name": "ts_index_status",
            "description": "Check whether the TS/JS symbol index exists for the selected workspace.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." }
                }
            }
        },
        {
            "name": "list_ts_symbols",
            "description": "List TS/JS symbols with optional file and kind filters. Builds the TS/JS index automatically if missing.",
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
            "description": "Search TS/JS symbols by name, signature, docstring, or file path. Builds the TS/JS index automatically if missing.",
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
            "description": "Read an indexed TS/JS symbol with optional caller/callee context.",
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
            "description": "Record an AI work summary for a workspace after code changes or investigation.",
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
            "description": "List recent AI work memories for a workspace.",
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
            "description": "Search AI work memories for a workspace.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 10 }
                }
            }
        }
    ])
}
