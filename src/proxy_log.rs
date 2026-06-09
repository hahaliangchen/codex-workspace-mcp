use std::io::Write;
use std::sync::Mutex;

use chrono::FixedOffset;

/// Current time in China timezone (UTC+8).
pub fn now_china() -> String {
    let offset = FixedOffset::east_opt(8 * 60 * 60).unwrap();
    chrono::Utc::now()
        .with_timezone(&offset)
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

/// Write a line to the shared log file and optionally SQLite database.
pub fn write(
    log: &Mutex<std::fs::File>,
    db: Option<&Mutex<rusqlite::Connection>>,
    action: Option<&str>,
    role: Option<&str>,
    msg: &str,
) {
    let ts = now_china();
    let line = format!("[{}] {}\n", ts, msg);
    if let Ok(mut f) = log.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }

    if let Some(db_lock) = db {
        let action_str = action.unwrap_or("INFO");
        let role_str = role.unwrap_or("proxy");

        if should_skip_db_log(role_str, msg) {
            return;
        }

        if let Ok(conn) = db_lock.lock() {
            let (short_msg, detail_opt) = split_db_message(msg);

            let _ = crate::database::insert_detailed_api_log(
                &conn,
                &ts,
                action_str,
                role_str,
                &short_msg,
                detail_opt,
            );
        }
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
