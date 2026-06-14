use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use ignore::WalkBuilder;
use tracing::{debug, info, warn};

use crate::tools::Workspace;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const QUIET_PERIOD: Duration = Duration::from_secs(1);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexLanguage {
    Rust,
    TypeScript,
    Python,
    Go,
}

pub trait IndexUpdater: Send + Sync + 'static {
    fn update_index(&self, workspace_root: &Path, lang: IndexLanguage) -> anyhow::Result<()>;
}

pub struct DefaultIndexUpdater;

impl IndexUpdater for DefaultIndexUpdater {
    fn update_index(&self, workspace_root: &Path, lang: IndexLanguage) -> anyhow::Result<()> {
        rebuild_index_for_language(workspace_root, lang)
    }
}

pub fn start_index_watcher(workspace: Arc<Workspace>) {
    let root = workspace.root().to_path_buf();
    let updater: Arc<dyn IndexUpdater> = Arc::new(DefaultIndexUpdater);

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let startup_root = root.clone();
        let startup_updater = Arc::clone(&updater);
        if let Err(error) = tokio::task::spawn_blocking(move || {
            auto_index_workspace(&startup_root, startup_updater.as_ref())
        })
        .await
        {
            warn!(error = %error, "index watcher startup task failed");
        }

        watch_workspace(root, updater).await;
    });
}

fn auto_index_workspace(root: &Path, updater: &dyn IndexUpdater) {
    for lang in detect_workspace_languages(root) {
        info!(?lang, "index watcher: building startup symbol index");
        if let Err(error) = updater.update_index(root, lang) {
            warn!(?lang, error = %error, "index watcher: startup index failed");
        }
    }
}

async fn watch_workspace(root: PathBuf, updater: Arc<dyn IndexUpdater>) {
    let mut last_seen = scan_source_mtimes(&root);
    let mut dirty = BTreeSet::new();
    let mut last_change_at: Option<SystemTime> = None;

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        let current = scan_source_mtimes(&root);
        for (path, current_mtime) in &current {
            if last_seen.get(path) != Some(current_mtime) {
                if let Some(lang) = language_for_path(path) {
                    dirty.insert(lang);
                    last_change_at = Some(SystemTime::now());
                }
            }
        }
        for path in last_seen.keys() {
            if !current.contains_key(path) {
                if let Some(lang) = language_for_path(path) {
                    dirty.insert(lang);
                    last_change_at = Some(SystemTime::now());
                }
            }
        }
        last_seen = current;

        let Some(change_at) = last_change_at else {
            continue;
        };
        if !dirty.is_empty()
            && SystemTime::now()
                .duration_since(change_at)
                .unwrap_or_default()
                >= QUIET_PERIOD
        {
            let langs = std::mem::take(&mut dirty);
            last_change_at = None;
            let rebuild_root = root.clone();
            let rebuild_updater = Arc::clone(&updater);
            if let Err(error) = tokio::task::spawn_blocking(move || {
                for lang in langs {
                    info!(
                        ?lang,
                        "index watcher: source change detected, rebuilding symbol index"
                    );
                    if let Err(error) = rebuild_updater.update_index(&rebuild_root, lang) {
                        warn!(?lang, error = %error, "index watcher: rebuild failed");
                    }
                }
            })
            .await
            {
                warn!(error = %error, "index watcher: rebuild task failed");
            }
        }
    }
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
                debug!(path = %path.display(), error = %error, "index watcher: could not read mtime");
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

fn rebuild_index_for_language(root: &Path, lang: IndexLanguage) -> anyhow::Result<()> {
    match lang {
        IndexLanguage::Rust => {
            crate::rust_index::index_workspace(root)?;
        }
        IndexLanguage::TypeScript => {
            crate::ts_index::index_workspace(root)?;
        }
        IndexLanguage::Python => {
            crate::python_index::index_workspace(root)?;
        }
        IndexLanguage::Go => {
            crate::go_index::index_workspace(root)?;
        }
    }
    Ok(())
}
