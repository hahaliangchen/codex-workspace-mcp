#![recursion_limit = "512"]

mod agent;
mod agent_runtime;
mod agent_service;
mod observer_service;
mod agent_web;
mod browser_control;
mod http_probe;
mod composer_catalog;
mod ai_proxy;
mod architecture_agent;
mod database;
mod expert_surgery;
mod format_translate;
mod go_index;
mod index_refresh;
mod mcp;
mod memory;
mod plugin_builtin;
mod plugin_runtime;
mod proxy_log;
mod request_context;
mod task_notebook;
mod flow_tree;
mod python_index;
mod responses;
mod rust_index;
mod sandbox_diagnostic;
mod skills;
mod symbol_provider;
mod symbol_query;
mod code_map;
mod symbol_description;
mod symbol_index_state;
mod tool_prepare;
mod tools;
mod file_edit;
mod source_read;
mod worker_read_cache;
mod worker_work_state;
mod work_scheduler;
mod work_organizer;
mod work_executor;
mod project_process;
mod workspace_changes;
mod ts_index;
mod upstream;
mod vision_preprocess;
mod visual_artifacts;
#[cfg(test)]
mod visual_tests;

use std::{env, fmt as std_fmt, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::Router;
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

    // Initialize database background write queue
    let _ = crate::database::init_db_writer(workspace.root());

    // Start the local agent and settings UI even before a model is configured.
    let ai_config_path = ai_proxy::dsh_config_path();
    let ai_config_path = if ai_config_path.is_absolute() {
        ai_config_path
    } else {
        env::current_dir()?.join(ai_config_path)
    };
    {
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
