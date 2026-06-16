use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, mpsc};

use chrono::FixedOffset;

#[derive(Debug)]
struct LogEvent {
    ts: String,
    conversation_id: Option<String>,
    msg: String,
    detail: Option<String>,
    action: Option<String>,
    role: Option<String>,
}

static ASYNC_LOGGER: OnceLock<mpsc::Sender<LogEvent>> = OnceLock::new();
static ASYNC_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static CURRENT_CONVERSATION_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// Current time in China timezone (UTC+8).
pub fn now_china() -> String {
    let offset = FixedOffset::east_opt(8 * 60 * 60).unwrap();
    chrono::Utc::now()
        .with_timezone(&offset)
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

fn startup_log_file_name() -> String {
    let offset = FixedOffset::east_opt(8 * 60 * 60).unwrap();
    chrono::Utc::now()
        .with_timezone(&offset)
        .format("%Y%m%d_%H%M.log")
        .to_string()
}

fn startup_log_path(log_dir: &Path) -> PathBuf {
    let base_name = startup_log_file_name();
    let base_path = log_dir.join(&base_name);
    if !base_path.exists() {
        return base_path;
    }

    let stem = base_name.trim_end_matches(".log");
    for idx in 2.. {
        let path = log_dir.join(format!("{}_{}.log", stem, idx));
        if !path.exists() {
            return path;
        }
    }
    unreachable!()
}

/// Start a background logger. Each process start writes to a fresh timestamped
/// file under `logs/`.
pub fn init_async(log_dir: PathBuf, _workspace_root: PathBuf) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(&log_dir)?;
    let log_path = startup_log_path(&log_dir);
    let _ = ASYNC_LOG_PATH.set(log_path.clone());

    let _ = ASYNC_LOGGER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<LogEvent>();
        std::thread::spawn(move || run_async_logger(log_path, rx));
        tx
    });

    Ok(ASYNC_LOG_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| startup_log_path(&log_dir)))
}

fn run_async_logger(log_path: PathBuf, rx: mpsc::Receiver<LogEvent>) {
    let mut log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    for event in rx {
        let line = format_log_line(&event);
        if let Some(file) = log_file.as_mut() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

fn format_log_line(event: &LogEvent) -> String {
    let mut tags = Vec::new();
    if let Some(conversation_id) = event.conversation_id.as_deref() {
        tags.push(format!("conversation={conversation_id}"));
    }
    if let Some(action) = event.action.as_deref() {
        tags.push(format!("action={action}"));
    }
    if let Some(role) = event.role.as_deref() {
        tags.push(format!("role={role}"));
    }

    let tag_text = if tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", tags.join(" "))
    };
    let detail = event
        .detail
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("\n{}", value))
        .unwrap_or_default();

    format!("[{}]{} {}{}\n", event.ts, tag_text, event.msg, detail)
}

pub fn set_conversation_id(conversation_id: Option<String>) {
    let lock = CURRENT_CONVERSATION_ID.get_or_init(|| Mutex::new(None));
    if let Ok(mut current) = lock.lock() {
        *current = conversation_id;
    }
}

fn current_conversation_id() -> Option<String> {
    CURRENT_CONVERSATION_ID
        .get()
        .and_then(|lock| lock.lock().ok().and_then(|current| current.clone()))
}

/// Write a line to the shared log file.
pub fn write(
    log: &Mutex<std::fs::File>,
    _write_db: bool,
    action: Option<&str>,
    role: Option<&str>,
    msg: &str,
) {
    let ts = now_china();
    if let Some(sender) = ASYNC_LOGGER.get() {
        let _ = sender.send(LogEvent {
            ts,
            conversation_id: current_conversation_id(),
            msg: msg.to_string(),
            detail: None,
            action: action.map(ToOwned::to_owned),
            role: role.map(ToOwned::to_owned),
        });
        return;
    }

    let line = format!("[{}] {}\n", ts, msg);
    if let Ok(mut f) = log.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

pub fn write_conversation_message(
    conversation_id: &str,
    source: &str,
    role: &str,
    message_type: &str,
    content: &str,
) {
    if let Some(sender) = ASYNC_LOGGER.get() {
        let _ = sender.send(LogEvent {
            ts: now_china(),
            conversation_id: Some(conversation_id.to_string()),
            msg: content.to_string(),
            detail: Some(format!("{source}\n{message_type}")),
            action: Some("CONVERSATION".to_string()),
            role: Some(role.to_string()),
        });
    }
}

pub fn write_user_history_snapshot(conversation_id: &str, content: &str) {
    let Some(log_path) = ASYNC_LOG_PATH.get() else {
        return;
    };
    let Some(log_dir) = log_path.parent() else {
        return;
    };
    let file_name = format!(
        "user_history_{}.log",
        sanitize_file_component(conversation_id)
    );
    let path = log_dir.join(file_name);
    let _ = std::fs::write(path, content);
}

fn sanitize_file_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars().take(120) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}
