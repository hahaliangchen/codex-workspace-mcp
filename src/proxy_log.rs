use std::path::PathBuf;
use std::sync::OnceLock;

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
static MODEL_TRANSPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Append only the sanitized transport metadata supplied by model_transport.
/// Keep filesystem ownership here alongside the other proxy log sinks.
pub(crate) fn append_model_transport(
    root: &std::path::Path,
    record: &serde_json::Value,
) -> std::io::Result<()> {
    use std::io::Write;
    let _guard = MODEL_TRANSPORT_LOCK.lock().unwrap();
    let directory = root.join("logs");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!(
        "model-requests-{}.jsonl",
        chrono::Utc::now().format("%Y-%m-%d")
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    options.open(path)?.write_all(&line)
}

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
