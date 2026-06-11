mod agent;
mod agent_runtime;
mod ai_proxy;
mod database;
mod format_translate;
mod go_index;
mod mcp;
mod memory;
mod proxy_log;
mod python_index;
mod responses;
mod rust_index;
mod skills;
mod tool_prepare;
mod tools;
mod ts_index;
mod upstream;
mod vision_preprocess;

use std::{env, fmt as std_fmt, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::Router;
use ignore::WalkBuilder;
use tokio::net::TcpListener;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};
use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

use crate::{
    mcp::{handle_mcp, handle_mcp_get},
    tools::Workspace,
};

struct ChinaTime;

impl FormatTime for ChinaTime {
    fn format_time(&self, writer: &mut Writer<'_>) -> std_fmt::Result {
        let offset = chrono::FixedOffset::east_opt(8 * 60 * 60).expect("valid China time offset");
        let now = chrono::Utc::now().with_timezone(&offset);
        write!(writer, "{}", now.format("%Y-%m-%dT%H:%M:%S%.6f%:z"))
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

fn build_app(workspace: Arc<Workspace>) -> Router {
    use axum::routing::get;

    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/mcp", get(handle_mcp_get).post(handle_mcp))
        .with_state(workspace)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
}

async fn run_server(listener: TcpListener, workspace: Arc<Workspace>) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    info!(%addr, root = %workspace.root().display(), "starting HTTP server");
    axum::serve(listener, build_app(workspace)).await?;
    Ok(())
}

fn workspace_has_file(root: &std::path::Path, names: &[&str], extensions: &[&str]) -> bool {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| {
                    !matches!(
                        name,
                        ".git"
                            | ".hg"
                            | ".svn"
                            | "node_modules"
                            | "target"
                            | "dist"
                            | "build"
                            | ".next"
                            | ".turbo"
                            | ".venv"
                            | "venv"
                            | "__pycache__"
                    )
                })
                .unwrap_or(true)
        });

    builder.build().filter_map(Result::ok).any(|entry| {
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            return false;
        }

        let file_name = entry.file_name().to_string_lossy();
        if names
            .iter()
            .any(|name| file_name.eq_ignore_ascii_case(name))
        {
            return true;
        }

        entry
            .path()
            .extension()
            .and_then(|value| value.to_str())
            .map(|ext| {
                extensions
                    .iter()
                    .any(|candidate| ext.eq_ignore_ascii_case(candidate))
            })
            .unwrap_or(false)
    })
}

fn auto_index_workspace(workspace: Arc<Workspace>) {
    let root = workspace.root().to_path_buf();
    let root_str = root.display().to_string();

    if workspace_has_file(&root, &["Cargo.toml"], &["rs"]) {
        info!("Auto-indexer: Rust files detected, building symbol index in background...");
        match workspace.index_rust_workspace(crate::rust_index::IndexRustWorkspaceRequest {
            workspace_root: root_str.clone(),
        }) {
            Ok(res) => info!(
                files = res.files_indexed,
                symbols = res.symbols_indexed,
                "Auto-indexer: Rust index built successfully"
            ),
            Err(e) => warn!(error = %e, "Auto-indexer: Rust indexing failed"),
        }
    }

    if workspace_has_file(
        &root,
        &["package.json", "tsconfig.json", "jsconfig.json"],
        &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
    ) {
        info!("Auto-indexer: TS/JS files detected, building symbol index in background...");
        match workspace.index_ts_workspace(crate::ts_index::IndexTsWorkspaceRequest {
            workspace_root: root_str.clone(),
        }) {
            Ok(res) => info!(
                files = res.files_indexed,
                symbols = res.symbols_indexed,
                "Auto-indexer: TS/JS index built successfully"
            ),
            Err(e) => warn!(error = %e, "Auto-indexer: TS/JS indexing failed"),
        }
    }

    if workspace_has_file(&root, &["go.mod"], &["go"]) {
        info!("Auto-indexer: Go files detected, building symbol index in background...");
        match workspace.index_go_workspace(crate::go_index::IndexGoWorkspaceRequest {
            workspace_root: root_str.clone(),
        }) {
            Ok(res) => info!(
                files = res.files_indexed,
                symbols = res.symbols_indexed,
                "Auto-indexer: Go index built successfully"
            ),
            Err(e) => warn!(error = %e, "Auto-indexer: Go indexing failed"),
        }
    }

    if workspace_has_file(
        &root,
        &["requirements.txt", "pyproject.toml", "setup.py"],
        &["py"],
    ) {
        info!("Auto-indexer: Python files detected, building symbol index in background...");
        match workspace.index_python_workspace(crate::python_index::IndexPythonWorkspaceRequest {
            workspace_root: root_str,
        }) {
            Ok(res) => info!(
                files = res.files_indexed,
                symbols = res.symbols_indexed,
                "Auto-indexer: Python index built successfully"
            ),
            Err(e) => warn!(error = %e, "Auto-indexer: Python indexing failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_timer(ChinaTime).init();

    let workspace_root = env::var("WORKSPACE_ROOT")
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);

    let workspace = Arc::new(Workspace::new(workspace_root)?);
    info!(root = %workspace.root().display(), "workspace initialized");

    // Spawns a background task to automatically index the workspace on startup.
    let workspace_for_indexing = workspace.clone();
    tokio::spawn(async move {
        // Give the HTTP server a moment to bind and start up
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if let Err(e) =
            tokio::task::spawn_blocking(move || auto_index_workspace(workspace_for_indexing)).await
        {
            warn!(error = %e, "Auto-indexer task failed");
        }
    });

    // Start AI proxy on port 3001 if config exists alongside the binary
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let ai_config_path = exe_dir.join("ai_proxy_config.json");
    if ai_config_path.exists() {
        match TcpListener::bind("127.0.0.1:3001").await {
            Ok(listener) => {
                info!(path = %ai_config_path.display(), "AI proxy starting on port 3001");
                let workspace_for_proxy = workspace.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        ai_proxy::run(listener, &ai_config_path, workspace_for_proxy).await
                    {
                        error!(%e, "AI proxy exited with error");
                    }
                });
            }
            Err(e) => warn!(%e, "could not start AI proxy on port 3001"),
        }
    }

    let bind = env::var("MCP_BIND").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let addr: SocketAddr = bind.parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, mode = "http", "listening");

    run_server(listener, workspace).await
}
