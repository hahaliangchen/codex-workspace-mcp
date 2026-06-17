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

// 全局排他重建锁：确保底层的 tree-sitter 解析和 SQLite 事务写入在同一工作区下串行执行
static INDEX_REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

// 长驻留影子账本快照：记录每个工作区各自上一次成功索引时的文件物理状态
#[derive(Debug, Clone)]
struct IndexSnapshot {
    mtimes: HashMap<PathBuf, SystemTime>,
    summary: IndexRefreshSummary,
}

// 核心多工作区容器：从单例槽位升级为以当前 Workspace 根目录绝对路径为 Key 的哈希表，彻底规避多项目并存时的缓存颠簸
static INDEX_MEMORY_CACHE: OnceLock<Mutex<HashMap<PathBuf, IndexSnapshot>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexLanguage {
    Rust,
    TypeScript,
    Python,
    Go,
}

// 为所有 Summary 相关的对外结构体派生 Clone，允许响应线程在缓存命中时无损、零阻碍地克隆复用答案
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
    // -----------------------------------------------------------------------
    // 【第一阶段防线】：锁外并发元数据嗅探与多项目对账拦截
    // -----------------------------------------------------------------------
    // 1. 调用现有的高效 Walk 机制扫描当前物理盘现状。纯只读操作在系统级多线程下完全天然不冲突、不阻塞
    let current_mtimes = scan_source_mtimes(root);
    let root_buf = root.to_path_buf();

    // 2. 检查多工作区内存影子快照
    let cache_lock = INDEX_MEMORY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let cache = cache_lock.lock().unwrap();
        if let Some(snapshot) = cache.get(&root_buf) {
            // 利用 Rust 原生的 PartialEq 比较两个 Map 集合的内容拓扑。
            // 只要连续对话追问期间用户未删、未改、未增代码，这里在 0.1 毫秒内敏锐命中并高并发秒回
            if snapshot.mtimes == current_mtimes {
                debug!(
                    path = %root.display(),
                    "多工作区物理防线：精准命中本工作区缓存，短路排他大锁并秒回"
                );
                return snapshot.summary.clone();
            }
        }
    } // 优雅释放 cache 锁，响应线程绝不带着 cache 锁去挤压后面的串行互斥排队锁

    // -----------------------------------------------------------------------
    // 【第二阶段防线】：对账失败（代码有变动），进锁串行构建
    // -----------------------------------------------------------------------
    let _guard = INDEX_REFRESH_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();

    // 经典的双重检查锁定（Double-checked locking），防止高并发时的排队惊群线程进锁后进行重复全量扫描
    {
        let cache = cache_lock.lock().unwrap();
        if let Some(snapshot) = cache.get(&root_buf) {
            if snapshot.mtimes == current_mtimes {
                return snapshot.summary.clone();
            }
        }
    }

    // 🔥【重大性能提升】：直接复用锁外已经收集好的 current_mtimes 的 Keys，
    // 原地榨取出语言种类集合，彻底干掉原来 detect_workspace_languages(root) 导致的锁内二次全盘文件树重复扫描！
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

    // 踏踏实实走底层的真实语法树提取、增量判断并持久化写入对应的 SQLite 库
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

    // -----------------------------------------------------------------------
    // 【第三阶段】：记新账
    // -----------------------------------------------------------------------
    // 将这次最新、最准确的物理状态拓扑记录定向写回属于当前 root 的 map 槽位中，留待下次对账拦截
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

// 保持不变，向后兼容原有的外部调用或现有的测试用例
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
