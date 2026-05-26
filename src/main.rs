mod go_index;
mod mcp;
mod memory;
mod proxy;
mod python_index;
mod rust_index;
mod tools;
mod ts_index;

use std::{env, fmt as std_fmt, net::SocketAddr, path::PathBuf, sync::Arc};

use axum::Router;
use tokio::net::TcpListener;
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
        let offset =
            chrono::FixedOffset::east_opt(8 * 60 * 60).expect("valid China time offset");
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
    let stdio_mode = env::args().any(|a| a == "--stdio");

    if stdio_mode {
        // In stdio mode stdout is the JSON-RPC channel; logs must go to
        // stderr and ANSI escape codes must be off.
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_timer(ChinaTime)
            .init();
    } else {
        tracing_subscriber::fmt().with_timer(ChinaTime).init();
    }

    let workspace_root = env::var("WORKSPACE_ROOT")
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);

    let workspace = Arc::new(Workspace::new(workspace_root)?);
    info!(root = %workspace.root().display(), "workspace initialized");

    if stdio_mode {
        // Bind to a random port so the proxy can discover it.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        info!(%addr, mode = "stdio", "starting server in background");

        let server = tokio::spawn(run_server(listener, workspace));

        // proxy::run returns when stdin closes.
        let result = proxy::run(addr).await;

        server.abort();
        result
    } else {
        let bind = env::var("MCP_BIND").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
        let addr: SocketAddr = bind.parse()?;
        let listener = TcpListener::bind(addr).await?;
        info!(%addr, mode = "http", "listening");

        run_server(listener, workspace).await
    }
}
