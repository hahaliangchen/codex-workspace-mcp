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

use crate::architecture_agent::AnalyzeArchitectureRequest;
use crate::go_index::{
    IndexGoWorkspaceRequest, ListGoSymbolsRequest, ReadGoSymbolRequest, SearchGoSymbolsRequest,
};
use crate::memory::{
    ListArchitectureMemoryRequest, ListSymbolBusinessContextRequest, ListWorkMemoryRequest,
    RecordArchitectureMemoryRequest, RecordSymbolBusinessContextRequest, RecordWorkMemoryRequest,
    SearchArchitectureMemoryRequest, SearchSymbolBusinessContextRequest, SearchWorkMemoryRequest,
};
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
    Workspace, WorkspaceInfoRequest, WriteFileRequest, EditFileRequest,
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
                }),
            ];

            // If Rust index exists, expose it as a resource
            if let Ok(st) = workspace.rust_index_status(IndexRustWorkspaceRequest {
                workspace_root: w_root.clone(),
            }) {
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
            if let Ok(st) = workspace.ts_index_status(IndexTsWorkspaceRequest {
                workspace_root: w_root.clone(),
            }) {
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
            if let Ok(st) = workspace.python_index_status(IndexPythonWorkspaceRequest {
                workspace_root: w_root.clone(),
            }) {
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
            if let Ok(st) = workspace.go_index_status(IndexGoWorkspaceRequest {
                workspace_root: w_root.clone(),
            }) {
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
            if let Ok(st) = workspace.list_work_memory(ListWorkMemoryRequest {
                workspace_root: w_root.clone(),
                limit: 100,
            }) {
                if !st.memories.is_empty() {
                    resources.push(json!({
                        "uri": "mcp://codex-workspace-mcp/work-memory",
                        "name": "Developer Work Memory Timeline",
                        "mimeType": "text/plain",
                        "description": "Read the timeline of recorded memories, changes, test logs, and risks in this project workspace."
                    }));
                }
            }

            if let Ok(st) = workspace.list_architecture_memory(ListArchitectureMemoryRequest {
                workspace_root: w_root.clone(),
                limit: 100,
            }) {
                if !st.memories.is_empty() {
                    resources.push(json!({
                        "uri": "mcp://codex-workspace-mcp/architecture-memory",
                        "name": "Architecture Memory",
                        "mimeType": "text/plain",
                        "description": "Read durable feature/logic area summaries, key symbols, boundaries, common tasks, and risks for this workspace."
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
                     Read known files directly, use symbol tools for definitions, and text search for literals. No index-status check is required.\n\
                     Symbol queries automatically build or incrementally refresh their language index."
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
                    let st = workspace.list_rust_symbols(ListRustSymbolsRequest { options: Default::default(), workspace_root: w_root.clone(), file_path: None, kind: None })?;
                    let mut md = String::from("# Rust AST Symbols Index\n\n");
                    md.push_str(&format!("Showing {} of {} symbols; next offset {:?}. Use list_*_symbols with file/directory filters or offset for further entries.\n\n",st.symbols.len(),st.page.total,st.page.next_offset));
                    for sym in st.symbols {
                        let impl_str = sym.impl_type.map(|t| format!(" (impl {})", t)).unwrap_or_default();
                        md.push_str(&format!("- **{}** ({:?}): `{}` in `{}` (L{}-L{}){}\n  > {}\n", 
                            sym.name, sym.kind, sym.signature, sym.file_path, sym.start_line, sym.end_line, impl_str, sym.docstring.trim().replace("\n", "\n  > ")));
                    }
                    md
                }
                "mcp://codex-workspace-mcp/ast/ts/symbols" => {
                    let st = workspace.list_ts_symbols(ListTsSymbolsRequest { options: Default::default(), workspace_root: w_root.clone(), file_path: None, kind: None })?;
                    let mut md = String::from("# TS/JS AST Symbols Index\n\n");
                    md.push_str(&format!("Showing {} of {} symbols; next offset {:?}. Use list_*_symbols with file/directory filters or offset for further entries.\n\n",st.symbols.len(),st.page.total,st.page.next_offset));
                    for sym in st.symbols {
                        md.push_str(&format!("- **{}** ({:?}): `{}` in `{}` (L{}-L{})\n  > {}\n", 
                            sym.name, sym.kind, sym.signature, sym.file_path, sym.start_line, sym.end_line, sym.docstring.trim().replace("\n", "\n  > ")));
                    }
                    md
                }
                "mcp://codex-workspace-mcp/ast/python/symbols" => {
                    let st = workspace.list_python_symbols(ListPythonSymbolsRequest { options: Default::default(), workspace_root: w_root.clone(), file_path: None, kind: None })?;
                    let mut md = String::from("# Python AST Symbols Index\n\n");
                    md.push_str(&format!("Showing {} of {} symbols; next offset {:?}. Use list_*_symbols with file/directory filters or offset for further entries.\n\n",st.symbols.len(),st.page.total,st.page.next_offset));
                    for sym in st.symbols {
                        md.push_str(&format!("- **{}** ({:?}): `{}` in `{}` (L{}-L{})\n  > {}\n", 
                            sym.name, sym.kind, sym.signature, sym.file_path, sym.start_line, sym.end_line, sym.docstring.trim().replace("\n", "\n  > ")));
                    }
                    md
                }
                "mcp://codex-workspace-mcp/ast/go/symbols" => {
                    let st = workspace.list_go_symbols(ListGoSymbolsRequest { options: Default::default(), workspace_root: w_root.clone(), file_path: None, kind: None })?;
                    let mut md = String::from("# Go AST Symbols Index\n\n");
                    md.push_str(&format!("Showing {} of {} symbols; next offset {:?}. Use list_*_symbols with file/directory filters or offset for further entries.\n\n",st.symbols.len(),st.page.total,st.page.next_offset));
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
                            if mem.kind == "observer" {
                                md.push_str("- **Recorded By**: Observer\n");
                            }
                            if let Some(task_id) = &mem.source_task_id {
                                md.push_str(&format!("- **Source Task**: {}\n", task_id));
                            }
                            if !mem.applies_to.is_empty() {
                                md.push_str(&format!("- **Applies To**: {}\n", mem.applies_to));
                            }
                            if !mem.source_refs.is_empty() {
                                md.push_str(&format!("- **Source Paths/Symbols**: {}\n", mem.source_refs.join(", ")));
                            }
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
                "mcp://codex-workspace-mcp/architecture-memory" => {
                    let st = workspace.list_architecture_memory(ListArchitectureMemoryRequest {
                        workspace_root: w_root.clone(),
                        limit: 100,
                    })?;
                    let mut md = String::from("# Architecture Memory\n\n");
                    if st.memories.is_empty() {
                        md.push_str("No architecture memory has been recorded in this workspace yet. Use `record_architecture_memory` after verifying a feature's key symbols and boundaries.\n");
                    } else {
                        for mem in st.memories {
                            let updated = chrono::DateTime::from_timestamp(mem.updated_at_unix as i64, 0)
                                .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                                .unwrap_or_else(|| format!("Unix Epoch {}", mem.updated_at_unix));
                            md.push_str(&format!("## {}\n", mem.area));
                            md.push_str(&format!("- **Updated**: {}\n", updated));
                            md.push_str(&format!("- **Summary**: {}\n", mem.summary));
                            if !mem.key_symbols.is_empty() {
                                md.push_str(&format!("- **Key Symbols**:\n  - {}\n", mem.key_symbols.join("\n  - ")));
                            }
                            if !mem.key_files.is_empty() {
                                md.push_str(&format!("- **Key Files**:\n  - {}\n", mem.key_files.join("\n  - ")));
                            }
                            if !mem.common_tasks.is_empty() {
                                md.push_str(&format!("- **Common Tasks**:\n  - {}\n", mem.common_tasks.join("\n  - ")));
                            }
                            if !mem.boundaries.is_empty() {
                                md.push_str(&format!("- **Boundaries**:\n  > {}\n", mem.boundaries.replace("\n", "\n  > ")));
                            }
                            if !mem.risks.is_empty() {
                                md.push_str(&format!("- **Risks**:\n  > {}\n", mem.risks.replace("\n", "\n  > ")));
                            }
                            md.push('\n');
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
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tools/call requires params.name"))?;

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    if crate::http_probe::is_tool(name) {
        let value=crate::http_probe::execute(&arguments).await?;
        return Ok(json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&value)?}],"structuredContent":value}));
    }
    if crate::browser_control::is_tool(name) {
        let value = crate::browser_control::execute(workspace, name, &arguments).await?;
        return crate::visual_artifacts::mcp_result(workspace.root(),&crate::visual_artifacts::VisualContext::mcp(workspace.root()),value);
    }
    if crate::project_process::is_tool(name) {
        let value=crate::project_process::execute(workspace,name,arguments).await?;
        return Ok(json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&value)?}],"structuredContent":value}));
    }

    if crate::worker_read_cache::is_source_read(name) {
        let workspace=Workspace::new(workspace.root())?;
        let tool_name=name.to_owned();let source_args=arguments.clone();
        let mut value=tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
            if source_args["force_read"]==true {crate::source_read::clear();}
            Ok(match tool_name.as_str() {
                "read_file"=>serde_json::to_value(workspace.read_file(serde_json::from_value::<ReadFileRequest>(source_args)?)?)?,
                "read_file_lines"=>serde_json::to_value(workspace.read_file_lines(serde_json::from_value::<ReadFileLinesRequest>(source_args)?)?)?,
                "read_rust_symbol"=>serde_json::to_value(workspace.read_rust_symbol(serde_json::from_value::<ReadRustSymbolRequest>(source_args)?)?)?,
                "read_go_symbol"=>serde_json::to_value(workspace.read_go_symbol(serde_json::from_value::<ReadGoSymbolRequest>(source_args)?)?)?,
                "read_ts_symbol"=>serde_json::to_value(workspace.read_ts_symbol(serde_json::from_value::<ReadTsSymbolRequest>(source_args)?)?)?,
                "read_python_symbol"=>serde_json::to_value(workspace.read_python_symbol(serde_json::from_value::<ReadPythonSymbolRequest>(source_args)?)?)?,
                _=>unreachable!(),
            })
        }).await??;
        crate::source_read::page(&mut value,&arguments)?;
        if value.get("symbol").is_some() {
            value["source_index_check"]=json!("current_source_hash_verified");
            if arguments["include_context"]==true {value["related_context_freshness"]=json!("indexed_navigation_hints; related files refresh when read");}
        }
        return Ok(json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&value)?}],"structuredContent":value}));
    }

    if name == "search_code_map" || name.ends_with("_symbols") || name.starts_with("index_") || name.ends_with("_index_status") {
        let selected_root=arguments.get("workspace_root").and_then(Value::as_str).filter(|value|!value.is_empty()).map(std::path::Path::new).unwrap_or(workspace.root());
        let workspace=Workspace::new(selected_root)?;
        let name=name.to_owned();
        let value=tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
            let value=match name.as_str() {
                "search_code_map" => crate::code_map::search(workspace.root(),serde_json::from_value(arguments)?)?,
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
        "list_python_symbols" => {
            serde_json::to_value(workspace.list_python_symbols(serde_json::from_value::<
                ListPythonSymbolsRequest,
            >(arguments)?)?)?
        }
        "search_python_symbols" => {
            serde_json::to_value(workspace.search_python_symbols(serde_json::from_value::<
                SearchPythonSymbolsRequest,
            >(arguments)?)?)?
        }
        "read_python_symbol" => {
            serde_json::to_value(workspace.read_python_symbol(serde_json::from_value::<
                ReadPythonSymbolRequest,
            >(arguments)?)?)?
        }
                _=>anyhow::bail!("unknown index tool: {name}"),
            };
            Ok(value)
        }).await??;
        return Ok(json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&value)?}],"structuredContent":value}));
    }

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
        "write_file" => {
            let workspace=Workspace::new(workspace.root())?;
            tokio::task::spawn_blocking(move || -> anyhow::Result<Value> { Ok(serde_json::to_value(workspace.write_file(serde_json::from_value::<WriteFileRequest>(arguments)?)?)?) }).await??
        },
        "replace_range" => {
            let workspace=Workspace::new(workspace.root())?;
            tokio::task::spawn_blocking(move || -> anyhow::Result<Value> { Ok(serde_json::to_value(workspace.replace_range(serde_json::from_value::<ReplaceRangeRequest>(arguments)?)?)?) }).await??
        },
        "edit_file" => {
            let workspace=Workspace::new(workspace.root())?;
            tokio::task::spawn_blocking(move || -> anyhow::Result<Value> { Ok(serde_json::to_value(workspace.edit_file(serde_json::from_value::<EditFileRequest>(arguments)?)?)?) }).await??
        },
        "expert_code_surgery" => {
            let request = serde_json::from_value::<crate::expert_surgery::ExpertCodeSurgeryRequest>(
                arguments,
            )?;
            let response =
                crate::expert_surgery::run_expert_code_surgery(workspace, request.clone()).await?;
            serde_json::to_value(response)?
        }
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
        "record_architecture_memory" => {
            serde_json::to_value(workspace.record_architecture_memory(
                serde_json::from_value::<RecordArchitectureMemoryRequest>(arguments)?,
            )?)?
        }
        "list_architecture_memory" => {
            serde_json::to_value(workspace.list_architecture_memory(serde_json::from_value::<
                ListArchitectureMemoryRequest,
            >(arguments)?)?)?
        }
        "search_architecture_memory" => {
            serde_json::to_value(workspace.search_architecture_memory(
                serde_json::from_value::<SearchArchitectureMemoryRequest>(arguments)?,
            )?)?
        }
        "analyze_architecture_memory" => serde_json::to_value(
            crate::architecture_agent::analyze_architecture(
                workspace,
                serde_json::from_value::<AnalyzeArchitectureRequest>(arguments)?,
            )
            .await?,
        )?,
        "record_symbol_business_context" => {
            serde_json::to_value(workspace.record_symbol_business_context(
                serde_json::from_value::<RecordSymbolBusinessContextRequest>(arguments)?,
            )?)?
        }
        "list_symbol_business_context" => {
            serde_json::to_value(workspace.list_symbol_business_context(
                serde_json::from_value::<ListSymbolBusinessContextRequest>(arguments)?,
            )?)?
        }
        "search_symbol_business_context" => {
            serde_json::to_value(workspace.search_symbol_business_context(
                serde_json::from_value::<SearchSymbolBusinessContextRequest>(arguments)?,
            )?)?
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
        "analyze_image" => {
            let image_ref = arguments
                .get("image_ref")
                .or_else(|| arguments.get("image_key"))
                .and_then(|v| v.as_str())
                .unwrap_or("latest");
            let focus_instruction = arguments.get("focus_instruction").and_then(|v| v.as_str());

            let raw_data = crate::vision_preprocess::resolve_visible_image_ref(Some(image_ref))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "当前可见上下文中找不到可重新分析的原始图片。请基于已有图像分析报告回答；如果用户需要新的视觉检查，请让用户重新上传图片。"
                    )
                })?;

            let result =
                crate::agent::analyze_image_via_vision_agent(&raw_data, focus_instruction).await?;

            serde_json::to_value(result)?
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
    let mut definitions=json!([
        {
            "name": "workspace_info",
            "description": "Return workspace root, platform, file access scope, and ignore summary. Requires workspace_root.",
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
            "description": "List a directory by relative workspace path or absolute filesystem path with optional recursion and filtering. By default this shows the real filesystem view, including gitignored files. Use only to understand project layout or locate files by path — for code symbol lookups prefer the index tools (search_go_symbols, search_ts_symbols, search_rust_symbols).",
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
                    "respect_gitignore": {
                        "type": "boolean",
                        "default": false,
                        "description": "When true, hide files ignored by .gitignore/.ignore. Defaults to false so logs and generated files remain visible."
                    }
                }
            }
        },
        {
            "name": "read_file",
            "description": "Read a UTF-8 file by path with a byte limit. Read known files directly. For one known definition, read_*_symbol with file_path + name avoids reading the whole file. No prior symbol search or index-status call is required.",
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
            "description": "Read an inclusive 1-indexed line range. Use known locations directly. Choose text search for literals and symbol search for unknown definitions.",
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
            "description": "Search workspace text. Literal matching is the default; set regex=true for patterns such as A|B, anchors, or character classes. Patterns match each line; invalid expressions return an error. Search directly for literals, UI strings, config keys, error messages, log lines, imports, and syntax patterns. Symbol tools are useful for definitions and relationships; text searches do not require trying a symbol tool first. By default this searches the real filesystem view, including gitignored files. Use `path` for one file/directory, or `paths` as an array for multiple files/directories; do not put multiple paths in one space-separated string.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Optional absolute project directory to use for this call. Defaults to the server startup directory."
                    },
                    "query": { "type": "string" },
                    "regex": { "type": "boolean", "default": false, "description": "Enable regular expressions explicitly. Use regex=true for alternatives such as loadPptx|loadVirtualDocument. Literal mode does not interpret | as OR. Regex matching is per line; invalid expressions return an error." },
                    "path": {
                        "type": "string",
                        "default": ".",
                        "description": "Single file or directory to search. For multiple targets, use `paths` instead."
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of files/directories to search. Prefer this over a space-separated `path` string when searching multiple targets."
                    },
                    "case_sensitive": { "type": "boolean", "default": false },
                    "respect_gitignore": {
                        "type": "boolean",
                        "default": false,
                        "description": "When true, skip files ignored by .gitignore/.ignore. Defaults to false so logs and generated files remain searchable."
                    },
                    "max_matches": { "type": "integer", "default": 100 }
                }
            }
        },
        {
            "name": "expert_code_surgery",
            "description": "Invoke the stateless top-model code surgery helper for one indexed code symbol. It uses AST to locate the target symbol context, queries the expert model for a rewritten code block, and returns the rewritten code block. It does not perform writes, conflict resolution, or verification check. The caller (the orchestrator) is responsible for applying the replacement using `replace_range` and executing verification tools.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "symbol_id", "instruction"],
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Absolute project directory containing the symbol index."
                    },
                    "language": {
                        "type": "string",
                        "enum": ["rust", "typescript", "python", "go"],
                        "description": "Language-specific symbol provider to use. If omitted, the runtime infers it from the symbol_id prefix and falls back to rust for legacy calls."
                    },
                    "symbol_id": {
                        "type": "string",
                        "description": "Exact symbol id from the matching search/list/read symbol tool for the chosen language."
                    },
                    "instruction": {
                        "type": "string",
                        "description": "Precise rewrite command for the target symbol. Include verified constraints and intended behavior; do not include chat history."
                    },
                    "related_symbol_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "default": [],
                        "description": "Optional explicit readonly symbol ids from the same language provider to include as relationship context. These blocks help the expert understand callers/callees/types, but only symbol_id may be replaced."
                    },
                    "architecture_query": {
                        "type": "string",
                        "description": "Optional feature/area query used to select durable architecture memory for the fixed prefix."
                    }
                }
            }
        },
        {
            "name": "write_file",
            "description": "Create a UTF-8 file or replace its complete contents inside the selected workspace. Existing files REQUIRE expected_code_hash from the latest source read or notebook retrieval. Prefer edit_file for localized changes. Returns changed, created, code_hash and index_refresh; a failed index refresh does not undo a saved file.",
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
                    "create_parent_dirs": { "type": "boolean", "default": true },
                    "expected_code_hash": {"type":"string","description":"Required when path already exists. Use the code_hash returned by a current source read or notebook retrieval."}
                }
            }
        },
        {
            "name": "replace_range",
            "description": "Replace an inclusive 1-indexed line range inside the selected workspace, preserving untouched bytes and the selected region newline style. Requires expected_old_text or expected_code_hash. Prefer edit_file when a unique text anchor is available. Conflicts apply no changes; retrieve only the affected source.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "start_line", "end_line", "replacement"],
                "properties": {
                    "workspace_root": {
                        "type": "string",
                        "description": "Optional absolute project directory to use for this call. Defaults to the server startup directory."
                    },
                    "path": { "type": "string" },
                    "start_line": { "type": "integer", "minimum":1 },
                    "end_line": { "type": "integer", "minimum":1 },
                    "replacement": { "type": "string" },
                    "expected_old_text": { "type": "string", "description":"Exact selected source excluding its final line delimiter; required unless expected_code_hash is provided." },
                    "expected_code_hash": { "type":"string","description":"Current file version from a source read or notebook retrieval." }
                }
            }
        },
        {
            "name": "edit_file",
            "description": "Apply 1-100 exact text edits to ONE existing UTF-8 file inside the selected workspace. Each old_text must match uniquely in the original file; use enough surrounding code. All anchors refer to the same original snapshot and may not overlap. All edits are validated before one atomic commit. For insertion keep the anchor in new_text; for deletion set new_text empty. LF anchors may match CRLF source; untouched bytes stay unchanged. Prefer this over line-number edits. Returns changed, code_hash and index_refresh; mismatches apply no changes.",
            "inputSchema": {
                "type":"object", "required":["path","edits"],
                "properties": {
                    "workspace_root":{"type":"string","description":"Optional absolute selected project directory."},
                    "path":{"type":"string"},
                    "expected_code_hash":{"type":"string","description":"Optional current file version. Recommended when the edits depend on code outside their anchors."},
                    "edits":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"object","required":["old_text","new_text"],"properties":{"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}}}}
                }
            }
        },
        {
            "name":"search_code_map",
            "description":"Locate functionality as a single definition, a source module, or a saved group of related files/definitions. Searches task wording and responsibility descriptions across supported languages and returns grouped responsibilities with direct code entry points. Does not infer functionality from call chains. No separate index or memory lookup is required. Missing or stale descriptions are explicit; confirm source details only when needed.",
            "inputSchema":{"type":"object","required":["workspace_root","query"],"properties":{
                "workspace_root":{"type":"string"},"query":{"type":"string","description":"Functionality or definition to locate, e.g. 节点拖拽 or 图片渲染."},
                "language":{"type":"string","enum":["rust","ts","typescript","javascript","python","go"]},
                "file_path":{"type":"string","description":"Optional exact workspace-relative source file."},
                "directory":{"type":"string","description":"Optional workspace-relative subtree."},
                "limit":{"type":"integer","minimum":1,"maximum":20,"default":8},
                "entry_limit":{"type":"integer","minimum":1,"maximum":20,"default":6},
                "offset":{"type":"integer","minimum":0,"default":0,"description":"Page each language's ranked symbol candidates; see language_pages."},
                "group_offset":{"type":"integer","minimum":0,"default":0,"description":"Page groups within the current candidate window."}
            }}
        },
        {
            "name": "list_go_symbols",
            "description": "Browse a compact, paginated symbol outline. Prefer a known file or directory filter. Default limit 40, max 100; local symbols hidden unless include_locals=true. Read page.next_offset to continue only if needed. Each result includes a concise responsibility and its current/stale/missing status. Index freshness and skipped/failed file coverage are included.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "directory": { "type": "string", "description": "Workspace-relative directory prefix." },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 40 },
                    "include_locals": { "type": "boolean", "default": false },
                    "detailed": { "type": "boolean", "default": false, "description": "Include signature/doc previews (240/480 characters); normally read only the selected definition." },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "file_path": { "type": "string" },
                    "kind": { "type": "string", "enum": ["function", "method", "struct", "interface", "type"] }
                }
            }
        },
        {
            "name": "search_go_symbols",
            "description": "Locate definitions by name, scope, signature, docstring, path, responsibility description or task keywords. Multi-keyword query defaults to any-term matching (up to 24 distinct terms), ranked by relevance before pagination. Use match_mode=all to require every term, or phrase for a literal substring. Use file/directory/kind filters to narrow results. Index updates automatically; unfiltered consecutive queries share a 2-second scan window, while known source reads verify their current hash. Chinese task terms include a bounded task-vocabulary expansion in any mode; all and phrase remain literal. Zero matches are not proof code is absent.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "file_path": { "type": "string", "description": "Exact workspace-relative file path." },
                    "directory": { "type": "string", "description": "Workspace-relative directory prefix." },
                    "kind": { "type": "string", "enum": ["function", "method", "struct", "interface", "type"] },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "match_mode": { "type": "string", "enum": ["any", "all", "phrase"], "default": "any" },
                    "include_locals": { "type": "boolean", "default": false },
                    "detailed": { "type": "boolean", "default": false },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
                }
            }
        },
        {
            "name": "read_go_symbol",
            "description": "Read a definition directly using file_path + name (qualified names accepted), or an existing symbol_id. Known file and name need no prior search/list call. Ambiguous names return candidate IDs. include_context=true adds heuristic caller/callee/import relationships, not compiler-verified dependencies. Includes a responsibility, qualified name and code_hash for optional version-checked annotation. The source index refreshes automatically.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "anyOf": [{ "required": ["symbol_id"] }, { "required": ["file_path", "name"] }],
                "properties": {
                    "file_path": { "type": "string", "description": "Workspace-relative file path; required with name when symbol_id is omitted." },
                    "name": { "type": "string", "description": "Definition name, optionally qualified, e.g. PptxParser.parse or Renderer::render_slide." },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "symbol_id": { "type": "string" },
                    "include_context": { "type": "boolean", "default": false }
                }
            }
        },
        {
            "name": "list_rust_symbols",
            "description": "Browse a compact, paginated symbol outline. Prefer a known file or directory filter. Default limit 40, max 100; local symbols hidden unless include_locals=true. Read page.next_offset to continue only if needed. Each result includes a concise responsibility and its current/stale/missing status. Index freshness and skipped/failed file coverage are included.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "directory": { "type": "string", "description": "Workspace-relative directory prefix." },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 40 },
                    "include_locals": { "type": "boolean", "default": false },
                    "detailed": { "type": "boolean", "default": false, "description": "Include signature/doc previews (240/480 characters); normally read only the selected definition." },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "file_path": { "type": "string" },
                    "kind": { "type": "string", "enum": ["function", "method", "struct", "enum", "trait", "type_alias", "const", "static", "module"] }
                }
            }
        },
        {
            "name": "search_rust_symbols",
            "description": "Locate definitions by name, scope, signature, docstring, path, responsibility description or task keywords. Multi-keyword query defaults to any-term matching (up to 24 distinct terms), ranked by relevance before pagination. Use match_mode=all to require every term, or phrase for a literal substring. Use file/directory/kind filters to narrow results. Index updates automatically; unfiltered consecutive queries share a 2-second scan window, while known source reads verify their current hash. Chinese task terms include a bounded task-vocabulary expansion in any mode; all and phrase remain literal. Zero matches are not proof code is absent.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "file_path": { "type": "string", "description": "Exact workspace-relative file path." },
                    "directory": { "type": "string", "description": "Workspace-relative directory prefix." },
                    "kind": { "type": "string", "enum": ["function", "method", "struct", "enum", "trait", "type_alias", "const", "static", "module"] },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "match_mode": { "type": "string", "enum": ["any", "all", "phrase"], "default": "any" },
                    "include_locals": { "type": "boolean", "default": false },
                    "detailed": { "type": "boolean", "default": false },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
                }
            }
        },
        {
            "name": "read_rust_symbol",
            "description": "Read a definition directly using file_path + name (qualified names accepted), or an existing symbol_id. Known file and name need no prior search/list call. Ambiguous names return candidate IDs. include_context=true adds heuristic caller/callee/import relationships, not compiler-verified dependencies. Includes a responsibility, qualified name and code_hash for optional version-checked annotation. The source index refreshes automatically.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "anyOf": [{ "required": ["symbol_id"] }, { "required": ["file_path", "name"] }],
                "properties": {
                    "file_path": { "type": "string", "description": "Workspace-relative file path; required with name when symbol_id is omitted." },
                    "name": { "type": "string", "description": "Definition name, optionally qualified, e.g. PptxParser.parse or Renderer::render_slide." },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "symbol_id": { "type": "string" },
                    "include_context": { "type": "boolean", "default": false }
                }
            }
        },
        {
            "name": "list_ts_symbols",
            "description": "Browse a compact, paginated symbol outline. Prefer a known file or directory filter. Default limit 40, max 100; local symbols hidden unless include_locals=true. Read page.next_offset to continue only if needed. Each result includes a concise responsibility and its current/stale/missing status. Index freshness and skipped/failed file coverage are included.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "directory": { "type": "string", "description": "Workspace-relative directory prefix." },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 40 },
                    "include_locals": { "type": "boolean", "default": false },
                    "detailed": { "type": "boolean", "default": false, "description": "Include signature/doc previews (240/480 characters); normally read only the selected definition." },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "file_path": { "type": "string" },
                    "kind": { "type": "string", "enum": ["function", "arrow_function", "class", "method", "interface", "type_alias", "enum", "const", "component"] }
                }
            }
        },
        {
            "name": "search_ts_symbols",
            "description": "Locate definitions by name, scope, signature, docstring, path, responsibility description or task keywords. Multi-keyword query defaults to any-term matching (up to 24 distinct terms), ranked by relevance before pagination. Use match_mode=all to require every term, or phrase for a literal substring. Use file/directory/kind filters to narrow results. Index updates automatically; unfiltered consecutive queries share a 2-second scan window, while known source reads verify their current hash. Chinese task terms include a bounded task-vocabulary expansion in any mode; all and phrase remain literal. Zero matches are not proof code is absent.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "file_path": { "type": "string", "description": "Exact workspace-relative file path." },
                    "directory": { "type": "string", "description": "Workspace-relative directory prefix." },
                    "kind": { "type": "string", "enum": ["function", "arrow_function", "class", "method", "interface", "type_alias", "enum", "const", "component"] },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "match_mode": { "type": "string", "enum": ["any", "all", "phrase"], "default": "any" },
                    "include_locals": { "type": "boolean", "default": false },
                    "detailed": { "type": "boolean", "default": false },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
                }
            }
        },
        {
            "name": "read_ts_symbol",
            "description": "Read a known TypeScript definition directly by file_path + name or symbol_id. For editing, include_related_types=true adds up to eight local type definitions (12000 source characters, two levels); include_outline=true adds parent and six nearest sibling positions/signatures. These are bounded navigation hints, not compiler-resolved types. include_context=true separately expands heuristic call relationships. Returns current source and version hash; no prerequisite list/search needed.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "anyOf": [{ "required": ["symbol_id"] }, { "required": ["file_path", "name"] }],
                "properties": {
                    "file_path": { "type": "string", "description": "Workspace-relative file path; required with name when symbol_id is omitted." },
                    "name": { "type": "string", "description": "Definition name, optionally qualified, e.g. PptxParser.parse or Renderer::render_slide." },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "symbol_id": { "type": "string" },
                    "include_context": { "type": "boolean", "default": false },
                    "include_related_types": { "type": "boolean", "default": false, "description": "Include bounded referenced local interface/type/enum bodies for an edit; external packages, namespace imports and re-exports are not expanded." },
                    "include_outline": { "type": "boolean", "default": false, "description": "Return parent and nearby definition positions/signatures for locating a change without listing the whole file." }
                }
            }
        },
        {
            "name": "list_python_symbols",
            "description": "Browse a compact, paginated symbol outline. Prefer a known file or directory filter. Default limit 40, max 100; local symbols hidden unless include_locals=true. Read page.next_offset to continue only if needed. Each result includes a concise responsibility and its current/stale/missing status. Index freshness and skipped/failed file coverage are included.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "directory": { "type": "string", "description": "Workspace-relative directory prefix." },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 40 },
                    "include_locals": { "type": "boolean", "default": false },
                    "detailed": { "type": "boolean", "default": false, "description": "Include signature/doc previews (240/480 characters); normally read only the selected definition." },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "file_path": { "type": "string" },
                    "kind": { "type": "string", "enum": ["function", "method", "class"] }
                }
            }
        },
        {
            "name": "search_python_symbols",
            "description": "Locate definitions by name, scope, signature, docstring, path, responsibility description or task keywords. Multi-keyword query defaults to any-term matching (up to 24 distinct terms), ranked by relevance before pagination. Use match_mode=all to require every term, or phrase for a literal substring. Use file/directory/kind filters to narrow results. Index updates automatically; unfiltered consecutive queries share a 2-second scan window, while known source reads verify their current hash. Chinese task terms include a bounded task-vocabulary expansion in any mode; all and phrase remain literal. Zero matches are not proof code is absent.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "file_path": { "type": "string", "description": "Exact workspace-relative file path." },
                    "directory": { "type": "string", "description": "Workspace-relative directory prefix." },
                    "kind": { "type": "string", "enum": ["function", "method", "class"] },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 },
                    "match_mode": { "type": "string", "enum": ["any", "all", "phrase"], "default": "any" },
                    "include_locals": { "type": "boolean", "default": false },
                    "detailed": { "type": "boolean", "default": false },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
                }
            }
        },
        {
            "name": "read_python_symbol",
            "description": "Read a definition directly using file_path + name (qualified names accepted), or an existing symbol_id. Known file and name need no prior search/list call. Ambiguous names return candidate IDs. include_context=true adds heuristic caller/callee/import relationships, not compiler-verified dependencies. Includes a responsibility, qualified name and code_hash for optional version-checked annotation. The source index refreshes automatically.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "anyOf": [{ "required": ["symbol_id"] }, { "required": ["file_path", "name"] }],
                "properties": {
                    "file_path": { "type": "string", "description": "Workspace-relative file path; required with name when symbol_id is omitted." },
                    "name": { "type": "string", "description": "Definition name, optionally qualified, e.g. PptxParser.parse or Renderer::render_slide." },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "symbol_id": { "type": "string" },
                    "include_context": { "type": "boolean", "default": false }
                }
            }
        },
        {
            "name": "record_work_memory",
            "description": "Record durable reusable work findings when useful. Do not create a routine summary for every task or duplicate an Observer retrospective memory.",
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
            "description": "List recent work summaries when a focused search term is unavailable. Reuse relevant entries and check their applicability.",
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
            "description": "Search prior project work and Observer memories with one focused query when they can shorten the task. Use source paths and applicability as pointers, and verify facts against current code.",
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
            "name": "record_architecture_memory",
            "description": "Create or update a durable architecture memory document for one feature/logic area in this workspace. Use after verifying code paths, and update it after large changes that alter responsibilities, key symbols, boundaries, or common tasks.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "area", "summary"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "area": { "type": "string", "description": "Stable feature/logic area name, e.g. 'Responses format translation'." },
                    "summary": { "type": "string", "description": "Concise description of what this area owns and how the flow works." },
                    "key_symbols": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Important symbol names or qualified functions/classes to inspect first." },
                    "key_files": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Primary files for this area." },
                    "boundaries": { "type": "string", "description": "What this area should not own; nearby systems to avoid unless evidence requires touching them." },
                    "common_tasks": { "type": "array", "items": { "type": "string" }, "default": [], "description": "User-facing task phrases that map to this area." },
                    "risks": { "type": "string", "description": "Coupling, compatibility, or regression risks." }
                }
            }
        },
        {
            "name": "list_architecture_memory",
            "description": "List durable architecture memory documents for this workspace. Use when you need an overview of known feature/logic areas before deciding where to inspect code.",
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
            "name": "search_architecture_memory",
            "description": "Search durable architecture memory by user task, feature name, symbol, file, boundary, or risk. Use before code changes to map business wording to key symbols and a minimal inspection scope.",
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
            "name": "analyze_architecture_memory",
            "description": "Ask the configured cheap architecture model to analyze a user task plus verified code/index evidence, then return a structured feature/logic map. Use when no useful architecture memory exists or a large change needs refreshed boundaries. Set record=true only after the provided evidence is grounded in actual code/index results.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "query"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "query": { "type": "string", "description": "User task or architecture question to map to project logic." },
                    "focus": { "type": "string", "description": "Optional narrower feature/logic focus." },
                    "evidence": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Verified snippets from architecture memory, symbol index results, read_*_symbol output, or short code excerpts. Do not pass whole-project source." },
                    "record": { "type": "boolean", "default": false, "description": "When true, upsert the returned analysis into architecture memory." }
                }
            }
        },
        {
            "name": "record_symbol_business_context",
            "description": "Save a reusable responsibility after reading a definition or module. Ordinary symbol searches will match this description and its task-wording keywords. Supply a symbol_id or file_path + qualified_name; scope=file describes a whole source module. Pass expected_code_hash from the read result to reject changed source. Optional: do not add reads or annotation calls merely to complete an inventory.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root", "business_role"],
                "properties": {
                    "qualified_name": { "type":"string", "description":"Qualified definition name with file_path, e.g. PptxParser.parsePicture." },
                    "keywords": { "type":"array", "items":{"type":"string"}, "maxItems":24, "description":"Task wording and feature aliases, including Chinese words when useful." },
                    "scope": { "type":"string", "enum":["symbol","file"], "default":"symbol" },
                    "source": { "type":"string", "enum":["worker","observer","architecture"], "default":"worker" },
                    "expected_code_hash": { "type":"string", "description":"Source version from description.code_hash or read_file.code_hash; changed source rejects the write." },
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "symbol_id": { "type": "string", "description": "Current symbol ID from a read/search result. Can be omitted when file_path and qualified_name are supplied." },
                    "symbol_name": { "type": "string" },
                    "language": { "type": "string" },
                    "file_path": { "type": "string" },
                    "belongs_to_area": { "type": "string" },
                    "business_role": { "type": "string" },
                    "common_tasks": { "type": "array", "items": { "type": "string" }, "default": [] },
                    "read_when": { "type": "string" },
                    "avoid_when": { "type": "string" },
                    "risks": { "type": "string" },
                    "confidence": { "type": "number", "default": 0.0 }
                }
            }
        },
        {
            "name": "list_symbol_business_context",
            "description": "List semantic business contexts for indexed symbols. Optionally filter by architecture area.",
            "inputSchema": {
                "type": "object",
                "required": ["workspace_root"],
                "properties": {
                    "workspace_root": { "type": "string", "description": "Absolute workspace root." },
                    "belongs_to_area": { "type": "string" },
                    "limit": { "type": "integer", "default": 10 }
                }
            }
        },
        {
            "name": "search_symbol_business_context",
            "description": "Inspect saved responsibility records, including keywords and stale flags. Ordinary search_*_symbols already searches these records and returns live definitions; no separate lookup is required for normal navigation.",
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
            "description": "Query the structured API and tool execution logs in SQLite from the past 24 hours. Helpful for diagnosing redundant tool calls or connection errors. Results include conversation_id when available.",
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
                        "description": "Optional SQLite WHERE clause filter (e.g. 'action = \"ERROR\"', 'conversation_id = \"previous_response_id:abc\"', or 'message LIKE \"%search%\"')."
                    }
                }
            }
        },
        {
            "name": "analyze_image",
            "description": "Analyze an image that is still visible in the current request context. Use this only when the current user explicitly asks to re-check an image/screenshot or needs fresh visual inspection. If no original image is visible, ask the user to upload it again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "image_ref": {
                        "type": "string",
                        "description": "Optional visible image reference. Use 'latest' by default, or a 1-based index like '1' for the first visible image in the current context."
                    },
                    "focus_instruction": {
                        "type": "string",
                        "description": "Optional instructions to tell the vision agent what to focus on or re-examine (e.g., 'focus on the bottom-right text')."
                    }
                }
            }
        }
    ]);
    definitions.as_array_mut().unwrap().extend(crate::project_process::definitions());
    definitions.as_array_mut().unwrap().extend(crate::http_probe::definitions());
    definitions.as_array_mut().unwrap().extend(crate::browser_control::definitions());
    for tool in definitions.as_array_mut().into_iter().flatten() {
        let name=tool["name"].as_str().unwrap_or("");
        if crate::worker_read_cache::is_source_read(name) {
            tool["description"]=json!(format!("{} Source pages preserve original bytes and line delimiters; complete/next_start_line identify pagination. max_chars defaults to 24000, max 60000. Reads are bounded to 16 MiB; force_read bypasses snapshot caching.",tool["description"].as_str().unwrap_or("")));
            let props=tool["inputSchema"]["properties"].as_object_mut().unwrap();
            props.insert("max_chars".into(),json!({"type":"integer","minimum":1000,"maximum":60000,"default":24000}));
            props.entry("start_line").or_insert(json!({"type":"integer","minimum":1,"description":"Optional page start within this file/definition. Use next_start_line to continue."}));
            props.entry("end_line").or_insert(json!({"type":"integer","minimum":1}));
            props.insert("start_column".into(),json!({"type":"integer","minimum":1,"description":"Continue a partial long line using next_column. Unicode columns include line delimiters; default 1."}));
            props.insert("force_read".into(),json!({"type":"boolean","default":false,"description":"Bypass source snapshot caching when a fresh disk read is required."}));
        }
    }
    definitions
}
