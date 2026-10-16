use std::path::PathBuf;

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
    Ok(())
}
