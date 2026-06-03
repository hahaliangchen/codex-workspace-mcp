use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use rusqlite::{params, Connection, Result};

pub fn init_db(workspace_root: &Path) -> Result<Connection> {
    let index_dir = workspace_root.join(".codex-workspace-mcp");
    if !index_dir.exists() {
        let _ = std::fs::create_dir_all(&index_dir);
    }
    let db_path = index_dir.join("codex_state.db");
    let conn = Connection::open(db_path)?;

    // Create tool_calls registry table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS tool_calls (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            arguments TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
        [],
    )?;

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

    Ok(conn)
}

pub fn insert_tool_call(conn: &Connection, id: &str, name: &str, arguments: &str) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .unwrap_or(0) as i64;
    conn.execute(
        "INSERT OR REPLACE INTO tool_calls (id, name, arguments, created_at) VALUES (?, ?, ?, ?)",
        params![id, name, arguments, now],
    )?;
    Ok(())
}

pub fn get_tool_call(conn: &Connection, id: &str) -> Result<Option<(String, String)>> {
    let mut stmt = conn.prepare("SELECT name, arguments FROM tool_calls WHERE id = ?")?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        let arguments: String = row.get(1)?;
        Ok(Some((name, arguments)))
    } else {
        Ok(None)
    }
}

pub fn delete_tool_call(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM tool_calls WHERE id = ?", params![id])?;
    Ok(())
}
