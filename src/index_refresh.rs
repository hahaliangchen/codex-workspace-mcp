use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock}, // 👈 引入 Arc
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

// 🔥 核心重构：将全局单一重锁升级为“多工作区锁表”，用项目绝对路径路由各自独立的锁
static WORKSPACE_REBUILD_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

#[derive(Debug, Clone)]
struct IndexSnapshot {
    mtimes: HashMap<PathBuf, SystemTime>,
    summary: IndexRefreshSummary,
}

static INDEX_MEMORY_CACHE: OnceLock<Mutex<HashMap<PathBuf, IndexSnapshot>>> = OnceLock::new();

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

fn refresh_workspace_indexes_at(root: &Path) -> IndexRefreshSummary {
    // 【第一阶段】：锁外并发元数据嗅探与多项目对账拦截（保持你原有的优秀非阻塞设计）
    let current_mtimes = scan_source_mtimes(root);
    let root_buf = root.to_path_buf();

    let cache_lock = INDEX_MEMORY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let cache = cache_lock.lock().unwrap();
        if let Some(snapshot) = cache.get(&root_buf) {
            if snapshot.mtimes == current_mtimes {
                debug!(
                    path = %root.display(),
                    "多工作区物理防线：精准命中本工作区缓存，短路排他大锁并秒回"
                );
                return snapshot.summary.clone();
            }
        }
    }

    // 【第二阶段】：从全局锁表里，定向获取（或现场初始化）属于当前 root 的独立互斥锁
    let ws_rebuild_lock = {
        let mut locks = WORKSPACE_REBUILD_LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        locks
            .entry(root_buf.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };

    // 🔥 关键性能质变：仅锁定当前项目！项目 A 的重建绝对不再阻塞项目 B 的重建，实现真正的跨项目多线程并行
    let _guard = ws_rebuild_lock.lock().unwrap();

    // 经典的双重检查锁定（针对当前工作区）
    {
        let cache = cache_lock.lock().unwrap();
        if let Some(snapshot) = cache.get(&root_buf) {
            if snapshot.mtimes == current_mtimes {
                return snapshot.summary.clone();
            }
        }
    }

    // 直接复用锁外已经收集好的 current_mtimes 榨出语言集合，白嫖零成本资产
    let languages: BTreeSet<IndexLanguage> = current_mtimes
        .keys()
        .filter_map(|path| language_for_path(path))
        .collect();

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

    // 【第三阶段】：历史记账
    {
        let mut cache = cache_lock.lock().unwrap();
        cache.insert(
            root_buf,
            IndexSnapshot {
                mtimes: current_mtimes,
                summary: final_summary.clone(),
            },
        );
    }

    final_summary
}

fn detect_workspace_languages(root: &Path) -> BTreeSet<IndexLanguage> {
    scan_source_mtimes(root)
        .keys()
        .filter_map(|path| language_for_path(path))
        .collect()
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