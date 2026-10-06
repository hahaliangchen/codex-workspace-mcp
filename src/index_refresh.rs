use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
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

// Serialize refreshes within one workspace; other workspaces remain independent.
static WORKSPACE_REBUILD_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexLanguage {
    Rust,
    TypeScript,
    Python,
    Go,
}

#[derive(Debug, Serialize, Clone)]
pub struct IndexRefreshSummary {
    pub languages_detected: Vec<String>,
    pub languages_refreshed: Vec<LanguageRefreshSummary>,
    pub failures: Vec<LanguageRefreshFailure>,
}

#[derive(Debug, Serialize, Clone)]
pub struct LanguageRefreshSummary {
    pub language: String,
    pub files_indexed: usize,
    pub symbols_indexed: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct LanguageRefreshFailure {
    pub language: String,
    pub error: String,
}

pub fn refresh_workspace_indexes(workspace: &Workspace) -> IndexRefreshSummary {
    refresh_workspace_indexes_at(workspace.root())
}

pub fn refresh_workspace_indexes_at(root: &Path) -> IndexRefreshSummary {
    // Detect source languages; language builders handle freshness and incremental updates.
    let current_mtimes = scan_source_mtimes(root);
    let root_buf = root.to_path_buf();

    // Use a separate coordination lock for each workspace.
    let ws_rebuild_lock = {
        let mut locks = WORKSPACE_REBUILD_LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        locks
            .entry(root_buf)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };

    let _guard = ws_rebuild_lock.lock().unwrap();

    // Reuse the discovery scan for language selection.
    let mut languages: BTreeSet<IndexLanguage> = current_mtimes
        .keys()
        .filter_map(|path| language_for_path(path))
        .collect();

    // Refresh initialized languages too, so deletion of the final source file
    // clears its stored symbols. Builders reparse only changed files.
    if let Ok(conn) = crate::database::init_db(root) {
        for (language, key) in [
            (IndexLanguage::Rust, "rust"),
            (IndexLanguage::TypeScript, "ts"),
            (IndexLanguage::Python, "python"),
            (IndexLanguage::Go, "go"),
        ] {
            if crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), key)
                .is_some()
            {
                languages.insert(language);
            }
        }
    }
    let languages_detected = current_mtimes
        .keys()
        .filter_map(|path| language_for_path(path))
        .collect::<BTreeSet<_>>()
        .iter()
        .map(|lang| lang.as_str().to_string())
        .collect();
    let mut languages_refreshed = Vec::new();
    let mut failures = Vec::new();

    for lang in languages {
        info!(?lang, "index refresh: checking source changes");
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

    final_summary
}

#[cfg(test)]
fn detect_workspace_languages(root: &Path) -> BTreeSet<IndexLanguage> {
    scan_source_mtimes(root)
        .keys()
        .filter_map(|path| language_for_path(path))
        .collect()
}

pub(crate) fn scan_source_mtimes(root: &Path) -> HashMap<PathBuf, SystemTime> {
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
