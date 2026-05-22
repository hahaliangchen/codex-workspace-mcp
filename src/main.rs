mod go_index;
mod mcp;
mod memory;
mod rust_index;
mod tools;
mod ts_index;

use std::{env, fmt as std_fmt, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{Router, routing::get};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::info;
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_timer(ChinaTime).init();

    let workspace_root = env::var("WORKSPACE_ROOT")
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);
    let bind = env::var("MCP_BIND").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let addr: SocketAddr = bind.parse()?;

    let workspace = Arc::new(Workspace::new(workspace_root)?);
    info!(root = %workspace.root().display(), "workspace initialized");

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/mcp", get(handle_mcp_get).post(handle_mcp))
        .with_state(workspace)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "listening");
    axum::serve(listener, app).await?;

    Ok(())
}
