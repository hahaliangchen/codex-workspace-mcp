#![recursion_limit = "512"]

mod agent_service;
mod agent_web;
mod ai_proxy;
mod attachments;
mod browser_control;
mod builtin_plugins;
mod code_map;
mod composer_catalog;
mod context_assets;
mod context_rebuild;
mod context_window;
mod database;
mod file_edit;
mod go_index;
mod http_probe;
mod image_analysis;
mod index_refresh;
mod mcp;
mod memory;
mod model_settings;
mod model_stream;
mod model_transport;
mod observer_service;
mod plugin_protocol;
mod plugin_runtime;
mod plugin_sessions;
mod ppt_sessions;
mod program_execution;
mod project_process;
mod proxy_log;
mod python_index;
mod request_context;
mod rust_index;
mod session_history;
mod single_agent;
mod skill_packages;
mod source_read;
mod symbol_description;
mod symbol_index_state;
mod symbol_query;
mod tools;
mod ts_index;
mod visual_artifacts;
mod visual_probe;
#[cfg(test)]
mod visual_tests;
mod workspace_changes;

use std::{env, fmt as std_fmt, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::Router;
use tokio::net::TcpListener;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};
use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

use crate::{
    mcp::{handle_mcp, handle_mcp_delete, handle_mcp_get},
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
        .route(
            "/mcp",
            get(handle_mcp_get)
                .post(handle_mcp)
                .delete(handle_mcp_delete),
        )
        .with_state(workspace)
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::permissive()
                .expose_headers([axum::http::HeaderName::from_static("mcp-session-id")]),
        )
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

    if env::args().any(|arg| arg == "--probe-image-input") {
        let result = ai_proxy::probe_default_image_input(&ai_proxy::dsh_config_path()).await?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    let workspace_root = env::var("WORKSPACE_ROOT")
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);

    let workspace = Arc::new(Workspace::new(workspace_root)?);
    info!(root = %workspace.root().display(), "workspace initialized");

    // Start the local agent and settings UI even before a model is configured.
    let ai_config_path = ai_proxy::dsh_config_path();
    let ai_config_path = if ai_config_path.is_absolute() {
        ai_config_path
    } else {
        env::current_dir()?.join(ai_config_path)
    };
    {
        let agent_bind = env::var("AGENT_BIND").unwrap_or_else(|_| "127.0.0.1:3001".to_string());
        match TcpListener::bind(&agent_bind).await {
            Ok(listener) => {
                info!(path = %ai_config_path.display(), bind = %agent_bind, "Agent service starting");
                let workspace_for_proxy = workspace.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        ai_proxy::run(listener, &ai_config_path, workspace_for_proxy).await
                    {
                        error!(%e, "Agent service exited with error");
                    }
                });
            }
            Err(e) => warn!(%e, bind = %agent_bind, "could not start Agent service"),
        }
    }

    let bind = env::var("MCP_BIND").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let addr: SocketAddr = bind.parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, mode = "http", "listening");

    run_server(listener, workspace).await
}
