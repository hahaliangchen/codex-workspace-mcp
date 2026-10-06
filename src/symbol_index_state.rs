use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

// Version 3 backfills per-definition versions for legacy descriptions during
// one ordinary refresh, only when their old whole-file version still matches.
const TRACKING_VERSION: i64 = 3;

const NOISE: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".venv",
    "venv",
    "__pycache__",
    ".codex-workspace-mcp",
];
type Locks = Mutex<HashMap<(PathBuf, String), Arc<Mutex<()>>>>;
static LOCKS: OnceLock<Locks> = OnceLock::new();
pub fn lock(root: &Path, language: &str) -> Arc<Mutex<()>> {
    LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry((root.to_path_buf(), language.to_owned()))
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}
pub fn schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS symbol_index_files (
        workspace_root TEXT NOT NULL, language TEXT NOT NULL, file_path TEXT NOT NULL,
        modified_ns TEXT NOT NULL, size_bytes INTEGER NOT NULL, status TEXT NOT NULL, error TEXT NOT NULL,
        PRIMARY KEY(workspace_root,language,file_path));
        CREATE TABLE IF NOT EXISTS symbol_index_tracking (
        workspace_root TEXT NOT NULL, language TEXT NOT NULL, version INTEGER NOT NULL,
        PRIMARY KEY(workspace_root,language));
        CREATE TABLE IF NOT EXISTS symbol_index_checks (
        workspace_root TEXT NOT NULL, language TEXT NOT NULL, checked_at_unix INTEGER NOT NULL,
        changed_files INTEGER NOT NULL, removed_files INTEGER NOT NULL,
        PRIMARY KEY(workspace_root,language));")
}
#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    modified: String,
    size: u64,
}
pub struct RefreshPlan {
    pub changed: BTreeSet<String>,
    pub removed: BTreeSet<String>,
    pub full: bool,
    files: BTreeMap<String, Fingerprint>,
}
impl RefreshPlan {
    /// A registered write knows its changed file; avoid a repository walk.
    /// Older index schemas still need their ordinary one-time migration.
    pub fn for_file(
        root: &Path,
        language: &str,
        conn: &Connection,
        path: &Path,
    ) -> rusqlite::Result<Self> {
        let version = conn
            .query_row(
                "SELECT version FROM symbol_index_tracking WHERE workspace_root=?1 AND language=?2",
                params![root.to_string_lossy(), language],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if version != Some(TRACKING_VERSION) {
            return Self::new(root, language, conn);
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?
            .to_string_lossy()
            .replace('\\', "/");
        let mut files = BTreeMap::new();
        let mut changed = BTreeSet::new();
        let mut removed = BTreeSet::new();
        // Match the regular scan's ignore rules without visiting other folders.
        let eligible = !relative.split('/').any(|part| NOISE.contains(&part))
            && path.parent().is_some_and(|parent| {
                ignore::WalkBuilder::new(parent)
                    .max_depth(Some(1))
                    .hidden(false)
                    .git_ignore(true)
                    .git_exclude(true)
                    .parents(true)
                    .build()
                    .filter_map(|entry| entry.ok())
                    .any(|entry| entry.path() == path)
            });
        if eligible && path.is_file() {
            let metadata = std::fs::metadata(path)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_nanos().to_string())
                .unwrap_or_default();
            files.insert(
                relative.clone(),
                Fingerprint {
                    modified,
                    size: metadata.len(),
                },
            );
            changed.insert(relative);
        } else {
            removed.insert(relative);
        }
        Ok(Self {
            changed,
            removed,
            full: false,
            files,
        })
    }
    pub fn for_directory(
        root: &Path,
        language: &str,
        conn: &Connection,
        directory: &Path,
    ) -> rusqlite::Result<Self> {
        let initialized = conn
            .query_row(
                "SELECT version FROM symbol_index_tracking WHERE workspace_root=?1 AND language=?2",
                params![root.to_string_lossy(), language],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            == Some(TRACKING_VERSION);
        if !initialized {
            return Self::new(root, language, conn);
        }
        Self::scoped(root, language, conn, Some(directory))
    }
    pub fn new(root: &Path, language: &str, conn: &Connection) -> rusqlite::Result<Self> {
        Self::scoped(root, language, conn, None)
    }
    fn scoped(
        root: &Path,
        language: &str,
        conn: &Connection,
        directory: Option<&Path>,
    ) -> rusqlite::Result<Self> {
        if directory.is_some_and(|path| path.exists() && !path.is_dir()) {
            return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                std::io::Error::other("directory filter must name a directory"),
            )));
        }
        let prefix = directory
            .map(|path| {
                path.strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .unwrap_or_default();
        let mut files = BTreeMap::new();
        let mut walker = ignore::WalkBuilder::new(directory.unwrap_or(root));
        walker
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .parents(true)
            .filter_entry(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_none_or(|name| !NOISE.contains(&name))
            });
        let ignored_scope = prefix.split('/').any(|part| NOISE.contains(&part));
        for entry in walker
            .build()
            .filter(|_| !ignored_scope && directory.is_none_or(|path| path.exists()))
        {
            let entry =
                entry.map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path();
            let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
            let relevant = match language {
                "rust" => extension == "rs",
                "ts" => matches!(extension, "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs"),
                "python" => extension == "py",
                "go" => extension == "go",
                _ => false,
            };
            if !relevant {
                continue;
            }
            {
                let metadata = entry
                    .metadata()
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|time| time.as_nanos().to_string())
                    .unwrap_or_default();
                files.insert(
                    path.strip_prefix(root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                    Fingerprint {
                        modified,
                        size: metadata.len(),
                    },
                );
            }
        }
        let full = conn
            .query_row(
                "SELECT version FROM symbol_index_tracking WHERE workspace_root=?1 AND language=?2",
                params![root.to_string_lossy(), language],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            != Some(TRACKING_VERSION);
        let mut previous = BTreeMap::new();
        let mut stmt = conn.prepare("SELECT file_path,modified_ns,size_bytes FROM symbol_index_files WHERE workspace_root=?1 AND language=?2")?;
        for row in stmt.query_map(params![root.to_string_lossy(), language], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Fingerprint {
                    modified: row.get(1)?,
                    size: row.get::<_, i64>(2)? as u64,
                },
            ))
        })? {
            let (path, fingerprint) = row?;
            if prefix.is_empty() || path.starts_with(&format!("{prefix}/")) {
                previous.insert(path, fingerprint);
            }
        }
        let changed = files
            .iter()
            .filter(|(path, fingerprint)| full || previous.get(*path) != Some(*fingerprint))
            .map(|(path, _)| path.clone())
            .collect();
        let removed = previous
            .keys()
            .filter(|path| !files.contains_key(*path))
            .cloned()
            .collect();
        Ok(Self {
            changed,
            removed,
            full,
            files,
        })
    }
    pub fn needs_update(&self) -> bool {
        self.full || !self.changed.is_empty() || !self.removed.is_empty()
    }
    pub fn prepare(
        &self,
        tx: &Transaction<'_>,
        root: &Path,
        language: &str,
        table: &str,
    ) -> rusqlite::Result<()> {
        if self.full {
            tx.execute(
                &format!("DELETE FROM {table} WHERE workspace_root=?1"),
                [root.to_string_lossy()],
            )?;
            tx.execute(
                "DELETE FROM symbol_index_files WHERE workspace_root=?1 AND language=?2",
                params![root.to_string_lossy(), language],
            )?;
        } else {
            for path in self.changed.iter().chain(&self.removed) {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE workspace_root=?1 AND file_path=?2"),
                    params![root.to_string_lossy(), path],
                )?;
                tx.execute("DELETE FROM symbol_index_files WHERE workspace_root=?1 AND language=?2 AND file_path=?3", params![root.to_string_lossy(),language,path])?;
            }
        }
        Ok(())
    }
    pub fn record(
        &self,
        tx: &Transaction<'_>,
        root: &Path,
        language: &str,
        path: &Path,
        status: &str,
        error: &str,
    ) -> rusqlite::Result<()> {
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if let Some(fingerprint) = self.files.get(&relative) {
            tx.execute("INSERT INTO symbol_index_files(workspace_root,language,file_path,modified_ns,size_bytes,status,error) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(workspace_root,language,file_path) DO UPDATE SET modified_ns=excluded.modified_ns,size_bytes=excluded.size_bytes,status=excluded.status,error=excluded.error", params![root.to_string_lossy(),language,relative,fingerprint.modified,fingerprint.size as i64,status,crate::symbol_query::preview(error,240)])?;
        }
        Ok(())
    }
    pub fn record_content(
        &self,
        tx: &Transaction<'_>,
        root: &Path,
        language: &str,
        path: &Path,
        content: &str,
    ) -> rusqlite::Result<()> {
        self.record(tx, root, language, path, "parsing", "")?;
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        tx.execute("UPDATE symbol_index_files SET content_hash=?4 WHERE workspace_root=?1 AND language=?2 AND file_path=?3",params![root.to_string_lossy(),language,relative,crate::symbol_description::content_hash(content.as_bytes())])?;
        Ok(())
    }
    pub fn record_check(
        &self,
        conn: &Connection,
        root: &Path,
        language: &str,
    ) -> rusqlite::Result<()> {
        conn.execute("INSERT OR REPLACE INTO symbol_index_checks(workspace_root,language,checked_at_unix,changed_files,removed_files) VALUES (?1,?2,?3,?4,?5)",params![root.to_string_lossy(),language,SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64,self.changed.len() as i64,self.removed.len() as i64])?;
        Ok(())
    }
    pub fn finish(
        &self,
        tx: &Transaction<'_>,
        root: &Path,
        language: &str,
    ) -> rusqlite::Result<()> {
        tx.execute("INSERT OR REPLACE INTO symbol_index_tracking(workspace_root,language,version) VALUES (?1,?2,?3)", params![root.to_string_lossy(),language,TRACKING_VERSION])?;
        for file in self.changed.iter().chain(&self.removed) {
            crate::symbol_description::synchronize_file(tx, root, language, file)?;
        }
        self.record_check(tx, root, language)
    }
}
#[derive(Debug, Serialize)]
pub struct CoverageIssue {
    pub file_path: String,
    pub status: String,
    pub error: String,
}
#[derive(Debug, Serialize)]
pub struct IndexHealth {
    pub files_indexed: usize,
    pub symbols_indexed: usize,
    pub unindexed_total: usize,
    pub coverage_issues: Vec<CoverageIssue>,
    pub tracking_initialized: bool,
    pub freshness_check: &'static str,
    pub query_rescan_interval_ms: u64,
    pub coverage_scope: &'static str,
    pub checked_at_unix: Option<u64>,
    pub changed_files_last_check: usize,
    pub removed_files_last_check: usize,
    pub source_files_seen: usize,
    pub coverage_issues_truncated: bool,
}
pub fn counts(
    conn: &Connection,
    root: &Path,
    language: &str,
    table: &str,
) -> rusqlite::Result<(usize, usize)> {
    let files = conn.query_row("SELECT count(*) FROM symbol_index_files WHERE workspace_root=?1 AND language=?2 AND status='indexed'",params![root.to_string_lossy(),language],|row| row.get::<_,i64>(0))?;
    let symbols = conn.query_row(
        &format!("SELECT count(*) FROM {table} WHERE workspace_root=?1"),
        [root.to_string_lossy()],
        |row| row.get::<_, i64>(0),
    )?;
    Ok((files as usize, symbols as usize))
}
pub fn health(root: &Path, language: &str, table: &str) -> rusqlite::Result<IndexHealth> {
    let conn = crate::database::init_db(root)?;
    let (files_indexed, symbols_indexed) = counts(&conn, root, language, table)?;
    let unindexed_total = conn.query_row("SELECT count(*) FROM symbol_index_files WHERE workspace_root=?1 AND language=?2 AND status!='indexed'",params![root.to_string_lossy(),language],|row| row.get::<_,i64>(0))? as usize;
    let mut stmt = conn.prepare("SELECT file_path,status,error FROM symbol_index_files WHERE workspace_root=?1 AND language=?2 AND status!='indexed' ORDER BY file_path LIMIT 20")?;
    let coverage_issues = stmt
        .query_map(params![root.to_string_lossy(), language], |row| {
            Ok(CoverageIssue {
                file_path: row.get(0)?,
                status: row.get(1)?,
                error: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let check = conn.query_row("SELECT checked_at_unix,changed_files,removed_files FROM symbol_index_checks WHERE workspace_root=?1 AND language=?2",params![root.to_string_lossy(),language],|row| Ok((row.get::<_,u64>(0)?,row.get::<_,usize>(1)?,row.get::<_,usize>(2)?))).optional()?;
    let source_files_seen = conn.query_row(
        "SELECT count(*) FROM symbol_index_files WHERE workspace_root=?1 AND language=?2",
        params![root.to_string_lossy(), language],
        |row| row.get::<_, usize>(0),
    )?;
    let coverage_issues_truncated = unindexed_total > coverage_issues.len();
    Ok(IndexHealth {
        files_indexed,
        symbols_indexed,
        unindexed_total,
        coverage_issues,
        tracking_initialized: check.is_some(),
        freshness_check: "file_mtime_and_size; exact_source_reads_verify_hash",
        query_rescan_interval_ms: 2000,
        coverage_scope: "supported_extensions_under_nonignored_paths; excludes_generated_macro_definitions",
        checked_at_unix: check.map(|value| value.0),
        changed_files_last_check: check.map(|value| value.1).unwrap_or(0),
        removed_files_last_check: check.map(|value| value.2).unwrap_or(0),
        source_files_seen,
        coverage_issues_truncated,
    })
}

/// Search only scans the requested subtree. Unfiltered successive searches
/// share a short scan window; registered writes and exact-source reads bypass it.
pub fn ensure_query_index<E: From<std::io::Error>>(
    root: &Path,
    language: &str,
    file: Option<&str>,
    directory: Option<&str>,
    refresh: impl FnOnce(Option<&Path>, bool) -> Result<(), E>,
) -> Result<(), E> {
    use std::time::{Duration, Instant};
    type Checks = Mutex<HashMap<(PathBuf, String), Instant>>;
    static CHECKS: OnceLock<Checks> = OnceLock::new();
    let conn =
        crate::database::init_db(root).map_err(|error| std::io::Error::other(error.to_string()))?;
    let initialized =
        crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), language).is_some();
    if !initialized {
        refresh(None, false)?;
        return Ok(());
    }
    if let Some(file) = file.filter(|value| !value.is_empty()) {
        let path = crate::file_edit::workspace_path(root, file)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        if !path.exists() || crate::source_read::needs_index_refresh(root, language, file) {
            refresh(Some(&path), false)?;
        }
        return Ok(());
    }
    if let Some(directory) = directory.filter(|value| !value.is_empty() && *value != ".") {
        let path = crate::file_edit::workspace_path(root, directory)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        return refresh(Some(&path), true);
    }
    let key = (root.to_path_buf(), language.to_owned());
    let checks = CHECKS.get_or_init(|| Mutex::new(HashMap::new()));
    let fresh = checks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .is_some_and(|at| at.elapsed() < Duration::from_secs(2));
    if !fresh {
        refresh(None, false)?;
        let mut checks = checks.lock().unwrap_or_else(|e| e.into_inner());
        checks.retain(|_, at| at.elapsed() < Duration::from_secs(2));
        checks.insert(key, Instant::now());
    }
    Ok(())
}
