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
    write_file: bool,
    write_db: bool,
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
/// file under `logs/`, while structured records can also mirror into SQLite.
pub fn init_async(log_dir: PathBuf, workspace_root: PathBuf) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(&log_dir)?;
    let log_path = startup_log_path(&log_dir);
    let _ = ASYNC_LOG_PATH.set(log_path.clone());

    let _ = ASYNC_LOGGER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<LogEvent>();
        std::thread::spawn(move || run_async_logger(log_path, workspace_root, rx));
        tx
    });

    Ok(ASYNC_LOG_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| startup_log_path(&log_dir)))
}

fn run_async_logger(log_path: PathBuf, workspace_root: PathBuf, rx: mpsc::Receiver<LogEvent>) {
    let mut log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    let db_conn = crate::database::init_db(Path::new(&workspace_root)).ok();

    for event in rx {
        if event.write_file {
            let line = format!("[{}] {}\n", event.ts, event.msg);
            if let Some(file) = log_file.as_mut() {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }

        if event.write_db {
            let action = event.action.as_deref().unwrap_or("INFO");
            let role = event.role.as_deref().unwrap_or("proxy");
            if action == "CONVERSATION" {
                if let (Some(conn), Some(conversation_id), Some(detail)) = (
                    db_conn.as_ref(),
                    event.conversation_id.as_deref(),
                    event.detail.as_deref(),
                ) {
                    let mut parts = detail.splitn(2, '\n');
                    let source = parts.next().unwrap_or("responses.turn");
                    let message_type = parts.next().unwrap_or("ai_dialogue");
                    let _ = crate::database::insert_conversation_message(
                        conn,
                        conversation_id,
                        &event.ts,
                        source,
                        role,
                        message_type,
                        &event.msg,
                    );
                }
                continue;
            }
            if should_skip_db_log(role, &event.msg) {
                continue;
            }
            if let Some(conn) = db_conn.as_ref() {
                let (short_msg, detail_opt) = event.detail.as_deref().map_or_else(
                    || split_db_message(&event.msg),
                    |detail| (event.msg.clone(), Some(detail)),
                );
                let _ = crate::database::insert_detailed_api_log_with_conversation(
                    conn,
                    event.conversation_id.as_deref(),
                    &event.ts,
                    action,
                    role,
                    &short_msg,
                    detail_opt,
                );
            }
        }
    }
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

/// Write a line to the shared log file and optionally SQLite database.
pub fn write(
    log: &Mutex<std::fs::File>,
    write_db: bool,
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
            write_file: true,
            write_db,
        });
        return;
    }

    let line = format!("[{}] {}\n", ts, msg);
    if let Ok(mut f) = log.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }

    if write_db {
        let action_str = action.unwrap_or("INFO");
        let role_str = role.unwrap_or("proxy");

        if should_skip_db_log(role_str, msg) {
            return;
        }

        let workspace_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Ok(conn) = crate::database::init_db(&workspace_root) {
            let (short_msg, detail_opt) = split_db_message(msg);
            let _ = crate::database::insert_detailed_api_log_with_conversation(
                &conn,
                current_conversation_id().as_deref(),
                &ts,
                action_str,
                role_str,
                &short_msg,
                detail_opt,
            );
        }
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
            write_file: false,
            write_db: true,
        });
    }
}

fn should_skip_db_log(role: &str, msg: &str) -> bool {
    role == "system"
        || msg.contains("You are Codex")
        || msg.contains("<permissions instructions>")
        || msg.contains("<skills_instructions>")
}

fn split_db_message(msg: &str) -> (String, Option<&str>) {
    if msg.len() > 300 {
        let truncated = msg.chars().take(300).collect::<String>();
        (truncated + " ... [TRUNCATED]", Some(msg))
    } else {
        (msg.to_owned(), None)
    }
}
