use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

use ignore::WalkBuilder;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::tools::Workspace;

const NOISE_DIRS: &[&str] = &[
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

static INDEX_REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static INDEX_MEMORY_CACHE: OnceLock<Mutex<Option<IndexSnapshot>>> = OnceLock::new();

#[derive(Debug, Clone)]
struct IndexSnapshot {
    last_mtimes: HashMap<PathBuf, SystemTime>,
    last_summary: IndexRefreshSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexLanguage {
    Rust,
    TypeScript,
    Python,
    Go,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRefreshSummary {
    pub languages_detected: Vec<String>,
    pub languages_refreshed: Vec<LanguageRefreshSummary>,
    pub failures: Vec<LanguageRefreshFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageRefreshSummary {
    pub language: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageRefreshFailure {
    pub language: String,
    pub error: String,
}

pub fn refresh_workspace_indexes(workspace: &Workspace) -> IndexRefreshSummary {
    refresh_workspace_indexes_at(workspace.root())
}

fn refresh_workspace_indexes_at(root: &Path) -> IndexRefreshSummary {
    let current_mtimes = scan_source_mtimes(root);
    if let Some(summary) = cached_summary_if_unchanged(&current_mtimes) {
        debug!(
            root = %root.display(),
            files = current_mtimes.len(),
            "index refresh: memory cache hit before rebuild lock"
        );
        return summary;
    }

    let _guard = INDEX_REFRESH_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();

    let locked_mtimes = scan_source_mtimes(root);
    if let Some(summary) = cached_summary_if_unchanged(&locked_mtimes) {
        debug!(
            root = %root.display(),
            files = locked_mtimes.len(),
            "index refresh: memory cache hit after rebuild lock"
        );
        return summary;
    }

    debug!(
        root = %root.display(),
        files = locked_mtimes.len(),
        "index refresh: memory cache miss; rebuilding indexes"
    );

    let languages = detect_languages_from_mtimes(&locked_mtimes);
    let languages_detected = languages
        .iter()
        .map(|lang| lang.as_str().to_string())
        .collect();
    let mut languages_refreshed = Vec::new();
    let mut failures = Vec::new();

    for lang in languages {
        info!(
            ?lang,
            "index refresh: rebuilding request-scoped symbol index"
        );
        match rebuild_index_for_language(root, lang) {
            Ok(summary) => languages_refreshed.push(summary),
            Err(error) => {
                warn!(?lang, error = %error, "index refresh: rebuild failed");
                failures.push(LanguageRefreshFailure {
                    language: lang.as_str().to_string(),
                    error: error.to_string(),
                });
            }
        }
    }

    let final_summary = IndexRefreshSummary {
        languages_detected,
        languages_refreshed,
        failures,
    };
    update_index_memory_cache(locked_mtimes, final_summary.clone());
    final_summary
}

fn detect_workspace_languages(root: &Path) -> BTreeSet<IndexLanguage> {
    detect_languages_from_mtimes(&scan_source_mtimes(root))
}

fn detect_languages_from_mtimes(mtimes: &HashMap<PathBuf, SystemTime>) -> BTreeSet<IndexLanguage> {
    mtimes
        .keys()
        .filter_map(|path| language_for_path(path))
        .collect()
}

fn cached_summary_if_unchanged(
    current_mtimes: &HashMap<PathBuf, SystemTime>,
) -> Option<IndexRefreshSummary> {
    let cache = INDEX_MEMORY_CACHE.get_or_init(|| Mutex::new(None));
    let snapshot = cache.lock().ok()?;
    let snapshot = snapshot.as_ref()?;
    if snapshot.last_mtimes == *current_mtimes {
        Some(snapshot.last_summary.clone())
    } else {
        None
    }
}

fn update_index_memory_cache(
    last_mtimes: HashMap<PathBuf, SystemTime>,
    last_summary: IndexRefreshSummary,
) {
    let cache = INDEX_MEMORY_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(mut snapshot) = cache.lock() {
        *snapshot = Some(IndexSnapshot {
            last_mtimes,
            last_summary,
        });
    }
}

fn scan_source_mtimes(root: &Path) -> HashMap<PathBuf, SystemTime> {
    let mut mtimes = HashMap::new();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| !NOISE_DIRS.contains(&name))
                .unwrap_or(true)
        });

    for entry in builder.build().filter_map(Result::ok) {
        let path = entry.path();
        if language_for_path(path).is_none() {
            continue;
        }
        match std::fs::metadata(path).and_then(|metadata| metadata.modified()) {
            Ok(mtime) => {
                mtimes.insert(path.to_path_buf(), mtime);
            }
            Err(error) => {
                debug!(path = %path.display(), error = %error, "index refresh: could not read mtime");
            }
        }
    }

    mtimes
}

fn language_for_path(path: &Path) -> Option<IndexLanguage> {
    match path.extension().and_then(|value| value.to_str()) {
        Some("rs") => Some(IndexLanguage::Rust),
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs") => Some(IndexLanguage::TypeScript),
        Some("py") => Some(IndexLanguage::Python),
        Some("go") => Some(IndexLanguage::Go),
        _ => None,
    }
}

impl IndexLanguage {
    fn as_str(self) -> &'static str {
        match self {
            IndexLanguage::Rust => "rust",
            IndexLanguage::TypeScript => "typescript",
            IndexLanguage::Python => "python",
            IndexLanguage::Go => "go",
        }
    }
}

fn rebuild_index_for_language(
    root: &Path,
    lang: IndexLanguage,
) -> anyhow::Result<LanguageRefreshSummary> {
    let (files_indexed, symbols_indexed) = match lang {
        IndexLanguage::Rust => {
            let response = crate::rust_index::index_workspace(root)?;
            (response.files_indexed, response.symbols_indexed)
        }
        IndexLanguage::TypeScript => {
            let response = crate::ts_index::index_workspace(root)?;
            (response.files_indexed, response.symbols_indexed)
        }
        IndexLanguage::Python => {
            let response = crate::python_index::index_workspace(root)?;
            (response.files_indexed, response.symbols_indexed)
        }
        IndexLanguage::Go => {
            let response = crate::go_index::index_workspace(root)?;
            (response.files_indexed, response.symbols_indexed)
        }
    };

    Ok(LanguageRefreshSummary {
        language: lang.as_str().to_string(),
        files_indexed,
        symbols_indexed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_workspace_languages_from_source_files() {
        let root = std::env::temp_dir().join(format!("codex_index_refresh_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src").join("app.ts"), "export const x = 1;\n").unwrap();

        let languages = detect_workspace_languages(&root);
        assert!(languages.contains(&IndexLanguage::Rust));
        assert!(languages.contains(&IndexLanguage::TypeScript));

        let _ = fs::remove_dir_all(root);
    }
}
