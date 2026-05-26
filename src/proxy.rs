use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info, warn};

/// Poll the health endpoint until the server responds or we time out.
async fn wait_until_ready(addr: SocketAddr, client: &reqwest::Client) -> bool {
    let health_url = format!("http://{addr}/health");
    for _ in 0..30 {
        match client.get(&health_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(%addr, "server healthy");
                return true;
            }
            Ok(resp) => {
                warn!(status = %resp.status(), "health check unexpected status, retrying…");
            }
            Err(e) => {
                warn!(%e, "health check failed, retrying…");
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    error!(%addr, "server did not become healthy in time");
    false
}

/// Run the stdio↔HTTP proxy loop.
///
/// * `addr` — the TCP address where the HTTP server is already listening.
///
/// Reads one JSON-RPC message per line from stdin, forwards it to
/// `POST http://{addr}/mcp`, and writes the response JSON (one line) to
/// stdout.  Notifications (requests without an `id`) are forwarded but no
/// response is written.
pub async fn run(addr: SocketAddr) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;

    if !wait_until_ready(addr, &client).await {
        anyhow::bail!("server at {addr} never became healthy");
    }

    let mcp_url = format!("http://{addr}/mcp");
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    info!(%addr, "stdio proxy ready, waiting for JSON-RPC requests from stdin");

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            info!("stdin closed, shutting down proxy");
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Determine whether this is a notification (no "id") so we can skip
        // writing a response.  We parse just enough to check for the "id"
        // field without fully deserialising into a typed struct.
        let is_notification = match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(ref val) => val.get("id").is_none(),
            Err(e) => {
                // Malformed input — write an error and continue.
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": -32700,
                        "message": format!("parse error: {e}")
                    }
                });
                let mut err_line = serde_json::to_string(&err)?;
                err_line.push('\n');
                stdout.write_all(err_line.as_bytes()).await?;
                continue;
            }
        };

        match client
            .post(&mcp_url)
            .header("content-type", "application/json")
            .body(trimmed.to_owned())
            .send()
            .await
        {
            Ok(resp) => {
                if is_notification {
                    // Notifications don't expect a response body.
                    continue;
                }
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    if body.trim().is_empty() {
                        continue;
                    }
                    let mut out = body.trim().to_owned();
                    out.push('\n');
                    stdout.write_all(out.as_bytes()).await?;
                } else {
                    // Server returned an error status — forward as JSON-RPC error.
                    let err = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": {
                            "code": -32603,
                            "message": format!("upstream HTTP {status}: {body}")
                        }
                    });
                    let mut err_line = serde_json::to_string(&err)?;
                    err_line.push('\n');
                    stdout.write_all(err_line.as_bytes()).await?;
                }
            }
            Err(e) => {
                error!(%e, "HTTP request to mcp server failed");
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": -32603,
                        "message": format!("proxy: {e}")
                    }
                });
                let mut err_line = serde_json::to_string(&err)?;
                err_line.push('\n');
                stdout.write_all(err_line.as_bytes()).await?;
            }
        }
    }

    Ok(())
}
