use rusqlite::{Connection, Result, params};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn init_db(workspace_root: &Path) -> Result<Connection> {
    let index_dir = workspace_root.join(".codex-workspace-mcp");
    if !index_dir.exists() {
        let _ = std::fs::create_dir_all(&index_dir);
    }
    let db_path = index_dir.join("codex_state.db");
    let conn = Connection::open(db_path)?;

    // Create memories table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memories (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            time_unix INTEGER NOT NULL,
            workspace_root TEXT NOT NULL,
            summary TEXT NOT NULL,
            implementation TEXT NOT NULL,
            tests TEXT NOT NULL,
            risks TEXT NOT NULL,
            files_changed TEXT NOT NULL
        )",
        [],
    )?;

    // Create rust_symbols table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS rust_symbols (
            id TEXT PRIMARY KEY,
            workspace_root TEXT NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            file_path TEXT NOT NULL,
            module_path TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            signature TEXT NOT NULL,
            docstring TEXT NOT NULL,
            visibility TEXT NOT NULL,
            impl_type TEXT,
            trait_name TEXT,
            calls_json TEXT NOT NULL
        )",
        [],
    )?;

    // Create go_symbols table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS go_symbols (
            id TEXT PRIMARY KEY,
            workspace_root TEXT NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            package_name TEXT NOT NULL,
            file_path TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            signature TEXT NOT NULL,
            docstring TEXT NOT NULL,
            receiver TEXT,
            receiver_name TEXT,
            receiver_type TEXT,
            calls_json TEXT NOT NULL,
            file_imports_json TEXT NOT NULL
        )",
        [],
    )?;

    // Create ts_symbols table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ts_symbols (
            id TEXT PRIMARY KEY,
            workspace_root TEXT NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            file_path TEXT NOT NULL,
            scope_path TEXT NOT NULL,
            parent_id TEXT,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            signature TEXT NOT NULL,
            docstring TEXT NOT NULL,
            export INTEGER NOT NULL,
            export_names_json TEXT NOT NULL,
            calls_json TEXT NOT NULL,
            import_bindings_json TEXT NOT NULL,
            imports_json TEXT NOT NULL,
            re_exports_json TEXT NOT NULL
        )",
        [],
    )?;

    // Create python_symbols table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS python_symbols (
            id TEXT PRIMARY KEY,
            workspace_root TEXT NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            file_path TEXT NOT NULL,
            class_name TEXT,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            signature TEXT NOT NULL,
            docstring TEXT NOT NULL,
            decorators_json TEXT NOT NULL,
            calls_json TEXT NOT NULL,
            file_imports_json TEXT NOT NULL
        )",
        [],
    )?;

    // Create index_metadata table — tracks when each language index was last built
    conn.execute(
        "CREATE TABLE IF NOT EXISTS index_metadata (
            workspace_root TEXT NOT NULL,
            lang TEXT NOT NULL,
            generated_at_unix INTEGER NOT NULL,
            PRIMARY KEY (workspace_root, lang)
        )",
        [],
    )?;

    // Indexes for fast searching
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_rust_name ON rust_symbols(name)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_go_name ON go_symbols(name)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_ts_name ON ts_symbols(name)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_py_name ON python_symbols(name)",
        [],
    )?;

    // Create api_logs table for detailed structural logs
    conn.execute(
        "CREATE TABLE IF NOT EXISTS api_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL,
            time_str TEXT NOT NULL,
            conversation_id TEXT,
            action TEXT NOT NULL,
            role TEXT NOT NULL,
            message TEXT NOT NULL,
            detail TEXT
        )",
        [],
    )?;
    let _ = conn.execute("ALTER TABLE api_logs ADD COLUMN conversation_id TEXT", []);
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_api_logs_ts ON api_logs(timestamp)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_api_logs_conversation ON api_logs(conversation_id)",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS conversation_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            time_str TEXT NOT NULL,
            source TEXT NOT NULL,
            role TEXT NOT NULL,
            message_type TEXT NOT NULL,
            content TEXT NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_conversation_messages_conv_ts
         ON conversation_messages(conversation_id, timestamp)",
        [],
    )?;

    Ok(conn)
}

pub fn insert_detailed_api_log_with_conversation(
    conn: &Connection,
    conversation_id: Option<&str>,
    time_str: &str,
    action: &str,
    role: &str,
    message: &str,
    detail: Option<&str>,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .unwrap_or(0) as i64;
    conn.execute(
        "INSERT INTO api_logs (timestamp, time_str, conversation_id, action, role, message, detail)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            now,
            time_str,
            conversation_id,
            action,
            role,
            message,
            detail
        ],
    )?;

    // Auto cleanup older than 24 hours (86400 seconds)
    let cutoff = now - 24 * 3600;
    conn.execute("DELETE FROM api_logs WHERE timestamp < ?", params![cutoff])?;
    Ok(())
}

pub fn insert_conversation_message(
    conn: &Connection,
    conversation_id: &str,
    time_str: &str,
    source: &str,
    role: &str,
    message_type: &str,
    content: &str,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .unwrap_or(0) as i64;
    conn.execute(
        "INSERT INTO conversation_messages
         (conversation_id, timestamp, time_str, source, role, message_type, content)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            conversation_id,
            now,
            time_str,
            source,
            role,
            message_type,
            content
        ],
    )?;
    let cutoff = now - 24 * 3600;
    conn.execute(
        "DELETE FROM conversation_messages WHERE timestamp < ?",
        params![cutoff],
    )?;
    Ok(())
}

/// Record (or update) the timestamp at which a language index was last fully built.
pub fn upsert_index_metadata(
    conn: &Connection,
    workspace_root: &str,
    lang: &str,
    generated_at_unix: u64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO index_metadata (workspace_root, lang, generated_at_unix) VALUES (?, ?, ?)
         ON CONFLICT(workspace_root, lang) DO UPDATE SET generated_at_unix = excluded.generated_at_unix",
        params![workspace_root, lang, generated_at_unix as i64],
    )?;
    Ok(())
}

/// Return the stored `generated_at_unix` for a language index, or `None` if it has never been built.
pub fn get_index_generated_at(conn: &Connection, workspace_root: &str, lang: &str) -> Option<u64> {
    conn.query_row(
        "SELECT generated_at_unix FROM index_metadata WHERE workspace_root = ? AND lang = ?",
        params![workspace_root, lang],
        |row| row.get::<_, i64>(0),
    )
    .ok()
    .map(|v| v as u64)
}
