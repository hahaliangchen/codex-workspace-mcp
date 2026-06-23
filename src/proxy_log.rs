use std::path::PathBuf;
use std::sync::OnceLock;

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn init(log_dir: PathBuf) -> std::io::Result<()> {
    std::fs::create_dir_all(&log_dir)?;
    let _ = LOG_DIR.set(log_dir);
    Ok(())
}

pub async fn write_codex_context(content: &str) {
    if let Some(log_dir) = LOG_DIR.get() {
        let path = log_dir.join("codex_to_proxy.log");
        let _ = tokio::fs::write(path, content).await;
    }
}

pub async fn write_upstream_context(content: &str) {
    if let Some(log_dir) = LOG_DIR.get() {
        let path = log_dir.join("proxy_to_upstream.log");
        let _ = tokio::fs::write(path, content).await;
    }
}
