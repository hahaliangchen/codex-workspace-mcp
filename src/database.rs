use rusqlite::{Connection, OptionalExtension, Result, params};
use std::path::Path;

pub fn init_db(workspace_root: &Path) -> Result<Connection> {
    let index_dir = workspace_root.join(".codex-workspace-mcp");
    if !index_dir.exists() {
        let _ = std::fs::create_dir_all(&index_dir);
    }
    let db_path = index_dir.join("codex_state.db");
    let mut conn = Connection::open(db_path)?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(30));
    let _ = conn.execute("PRAGMA journal_mode=WAL;", []);
    crate::symbol_index_state::schema(&conn)?;

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
    // Older workspaces already have the original memories table.
    let _ = conn.execute("ALTER TABLE memories ADD COLUMN source_task_id TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE memories ADD COLUMN kind TEXT NOT NULL DEFAULT 'work'",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE memories ADD COLUMN applies_to TEXT NOT NULL DEFAULT ''",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE memories ADD COLUMN source_refs TEXT NOT NULL DEFAULT '[]'",
        [],
    );
    let legacy_memory_index: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='idx_memories_source_task'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if legacy_memory_index.is_some_and(|sql| !sql.contains("kind")) {
        conn.execute("DROP INDEX IF EXISTS idx_memories_source_task", [])?;
    }
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_source_task
         ON memories(workspace_root, source_task_id) WHERE source_task_id IS NOT NULL AND kind <> 'observer'",
        [],
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS observer_memory_sources (
        workspace_root TEXT NOT NULL, summary TEXT NOT NULL, implementation TEXT NOT NULL,
        sources TEXT NOT NULL, PRIMARY KEY(workspace_root,summary,implementation));",
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS architecture_memories (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_root TEXT NOT NULL,
            area TEXT NOT NULL,
            summary TEXT NOT NULL,
            key_symbols TEXT NOT NULL,
            key_files TEXT NOT NULL,
            boundaries TEXT NOT NULL,
            common_tasks TEXT NOT NULL,
            risks TEXT NOT NULL,
            created_at_unix INTEGER NOT NULL,
            updated_at_unix INTEGER NOT NULL,
            UNIQUE(workspace_root, area)
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_architecture_memories_workspace
         ON architecture_memories(workspace_root, updated_at_unix)",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS symbol_business_contexts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_root TEXT NOT NULL,
            symbol_id TEXT NOT NULL,
            symbol_name TEXT NOT NULL,
            language TEXT NOT NULL,
            file_path TEXT NOT NULL,
            belongs_to_area TEXT NOT NULL,
            business_role TEXT NOT NULL,
            common_tasks TEXT NOT NULL,
            read_when TEXT NOT NULL,
            avoid_when TEXT NOT NULL,
            risks TEXT NOT NULL,
            confidence REAL NOT NULL,
            updated_at_unix INTEGER NOT NULL,
            UNIQUE(workspace_root, symbol_id)
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_symbol_business_contexts_search
         ON symbol_business_contexts(workspace_root, symbol_name, belongs_to_area)",
        [],
    )?;

    crate::symbol_description::schema(&conn)?;

    // Create rust_symbols table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS rust_symbols (
            id TEXT NOT NULL,
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
            calls_json TEXT NOT NULL,
            PRIMARY KEY (workspace_root, id)
        )",
        [],
    )?;

    // Create go_symbols table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS go_symbols (
            id TEXT NOT NULL,
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
            file_imports_json TEXT NOT NULL,
            PRIMARY KEY (workspace_root, id)
        )",
        [],
    )?;

    // Create ts_symbols table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ts_symbols (
            id TEXT NOT NULL,
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
            re_exports_json TEXT NOT NULL,
            PRIMARY KEY (workspace_root, id)
        )",
        [],
    )?;

    // Create python_symbols table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS python_symbols (
            id TEXT NOT NULL,
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
            file_imports_json TEXT NOT NULL,
            PRIMARY KEY (workspace_root, id)
        )",
        [],
    )?;

    migrate_legacy_symbol_keys(&mut conn)?;

    for language in ["rust", "ts", "python", "go"] {
        conn.execute_batch(&format!("CREATE INDEX IF NOT EXISTS idx_{language}_workspace_file ON {language}_symbols(workspace_root,file_path,start_line); CREATE INDEX IF NOT EXISTS idx_{language}_workspace_name ON {language}_symbols(workspace_root,name);"))?;
    }
    conn.execute("CREATE INDEX IF NOT EXISTS idx_ts_workspace_parent ON ts_symbols(workspace_root,parent_id)",[])?;

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

    Ok(conn)
}

/// Older symbol tables used `id` as a global primary key even though IDs are
/// relative to a workspace. Preserve their rows and IDs while scoping the key.
fn migrate_legacy_symbol_keys(conn: &mut Connection) -> Result<()> {
    let mut legacy = Vec::new();
    for table in ["rust_symbols", "go_symbols", "ts_symbols", "python_symbols"] {
        let schema: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )?;
        if schema.contains("id TEXT PRIMARY KEY") {
            legacy.push((table, schema));
        }
    }
    if legacy.is_empty() {
        return Ok(());
    }

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (table, schema) in legacy {
        let columns = &schema[schema.find('(').expect("symbol table has columns") + 1
            ..schema
                .rfind(')')
                .expect("symbol table has closing parenthesis")];
        let columns = columns.replacen("id TEXT PRIMARY KEY", "id TEXT NOT NULL", 1);
        let migrated = format!("{table}_scope_key_migration");
        tx.execute_batch(&format!(
            "CREATE TABLE {migrated} ({columns}, PRIMARY KEY (workspace_root, id));
             INSERT INTO {migrated} SELECT * FROM {table};
             DROP TABLE {table};
             ALTER TABLE {migrated} RENAME TO {table};"
        ))?;
    }
    tx.commit()
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// Several delegates of one conversation write its events at the same time: every write
    /// waits its turn instead of failing with "database is locked", and no sequence number repeats.
    #[test]
    fn concurrent_writers_wait_for_the_lock_instead_of_failing() {
        let root =
            std::env::temp_dir().join(format!("db_lock_{}", crate::agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        {
            let conn = crate::agent_service::open_db(&root).unwrap();
            conn.execute(
                "INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','p','m','running',0,0)",
                [],
            )
            .unwrap();
        }
        let (writers, each) = (8, 40);
        let threads: Vec<_> = (0..writers)
            .map(|writer| {
                let root = root.clone();
                std::thread::spawn(move || -> Result<(), String> {
                    let mut conn =
                        crate::agent_service::open_db(&root).map_err(|e| e.to_string())?;
                    for step in 0..each {
                        // Read the next sequence number, then write: the pattern that fails in a deferred transaction.
                        let tx = conn
                            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                            .map_err(|e| e.to_string())?;
                        crate::agent_service::append_event_tx(
                            &tx,
                            "task",
                            "test/event",
                            &json!({"writer":writer,"step":step}),
                            None,
                        )
                        .map_err(|e| e.to_string())?;
                        tx.commit().map_err(|e| e.to_string())?;
                    }
                    Ok(())
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let conn = crate::agent_service::open_db(&root).unwrap();
        let (count, distinct): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(DISTINCT seq) FROM agent_task_events WHERE task_id='task'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (count, distinct),
            ((writers * each) as i64, (writers * each) as i64)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The same work in deferred transactions shows why they are not used for read-then-write.
    #[test]
    fn a_deferred_read_then_write_fails_while_another_connection_writes() {
        let root =
            std::env::temp_dir().join(format!("db_lock_{}", crate::agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let mut reader = crate::agent_service::open_db(&root).unwrap();
        let writer = crate::agent_service::open_db(&root).unwrap();
        writer
            .execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('a','p','m','running',0,0)", [])
            .unwrap();
        let tx = reader.transaction().unwrap();
        let _: i64 = tx
            .query_row("SELECT COUNT(*) FROM agent_tasks", [], |row| row.get(0))
            .unwrap();
        writer
            .execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('b','p','m','running',0,0)", [])
            .unwrap();
        let upgrade = tx.execute("UPDATE agent_tasks SET status='done' WHERE id='a'", []);
        assert!(upgrade.unwrap_err().to_string().contains("locked"));
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
}
