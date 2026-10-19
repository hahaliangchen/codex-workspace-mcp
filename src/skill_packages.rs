//! Rust skill/package contributions over the existing scoped plugin runtime.
//! Loading is explicit. No user-text classification or automatic tool selection.
use crate::builtin_plugins::{ToolCatalog, ToolHandler};
use crate::plugin_runtime::{Plugin, PluginContext, PluginRuntime, ScopeId};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

pub(crate) mod command;
pub(crate) mod remote;
use crate::plugin_protocol::{Manifest, NativeBackend, Origin, ToolPermission};

static ROOT_CATALOG: std::sync::OnceLock<RwLock<std::sync::Weak<ToolCatalog>>> =
    std::sync::OnceLock::new();
pub fn bind_root(catalog: &Arc<ToolCatalog>) {
    *ROOT_CATALOG.get_or_init(Default::default).write().unwrap() = Arc::downgrade(catalog);
}
pub fn root_catalog() -> Option<Arc<ToolCatalog>> {
    ROOT_CATALOG.get()?.read().unwrap().upgrade()
}
pub fn root_skills() -> Option<Value> {
    Some(root_catalog()?.packages.skills())
}
pub fn root_read(name: &str) -> Option<Result<Value>> {
    let catalog = root_catalog()?;
    if catalog.packages.entries.read().unwrap().contains_key(name) {
        Some(catalog.packages.read(name))
    } else {
        None
    }
}

#[derive(Clone)]
pub struct Package {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub description: String,
    pub scope: Option<ScopeId>,
    pub origin: Origin,
    native: Option<NativeBackend>,
    remote: Option<Arc<remote::Connection>>,
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn load(root: &Path) -> Result<Package> {
    load_with(root, Origin::External, None)
}
pub(crate) fn load_builtin(root: &Path, native: NativeBackend) -> Result<Package> {
    load_with(root, Origin::Builtin, Some(native))
}
fn reload_package(package: &Package) -> Result<Package> {
    if package.remote.is_some() {
        return Ok(package.clone());
    }
    load_with(&package.root, package.origin, package.native.clone())
}
fn load_with(root: &Path, origin: Origin, native: Option<NativeBackend>) -> Result<Package> {
    let root = root.canonicalize()?;
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(root.join("plugin.json"))?)?;
    ensure!(
        manifest.version == 1 && identifier(&manifest.id),
        "invalid skill package version or ID"
    );
    let path = crate::file_edit::workspace_path(&root, &manifest.skill)?;
    let raw = std::fs::read_to_string(path)?;
    let front = raw
        .strip_prefix("---\n")
        .and_then(|raw| raw.split_once("\n---"))
        .context("skill requires YAML frontmatter")?
        .0;
    let fields: Value = serde_yaml::from_str(front)?;
    ensure!(
        fields["name"] == manifest.id,
        "skill name must equal package ID"
    );
    let description = fields["description"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .context("skill description required")?
        .to_owned();
    let mut ids = std::collections::BTreeSet::new();
    for page in &manifest.pages {
        ensure!(
            identifier(&page.id) && ids.insert(&page.id),
            "invalid or duplicate page ID"
        );
        ensure!(
            matches!(page.renderer.as_str(), "react" | "html"),
            "unsupported page renderer"
        );
        if page.renderer == "react" && origin == Origin::Builtin {
            ensure!(
                native
                    .as_ref()
                    .is_some_and(|backend| backend.frontend_entries.contains(&page.entry)),
                "unregistered frontend entry"
            );
        } else {
            if page.renderer == "react" {
                ensure!(
                    matches!(
                        Path::new(&page.entry)
                            .extension()
                            .and_then(|extension| extension.to_str()),
                        Some("js" | "mjs")
                    ),
                    "external frontend entry must be a built JavaScript module"
                );
            }
            ensure!(
                crate::file_edit::workspace_path(&root, &page.entry)?.is_file(),
                "page entry missing"
            );
        }
    }
    ensure!(
        manifest
            .tools
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == manifest.tools.len(),
        "duplicate package tool"
    );
    let commands = manifest
        .command_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        commands.len() == manifest.command_tools.len(),
        "duplicate command tool"
    );
    for tool in &manifest.command_tools {
        tool.validate(&root)?;
        ensure!(
            manifest.tools.contains(&tool.name),
            "command tool missing from tools declaration"
        );
        ensure!(
            !crate::mcp::tool_definitions()
                .as_array()
                .unwrap()
                .iter()
                .any(|definition| definition["name"] == tool.name),
            "command tool shadows a host tool"
        );
    }
    for name in &manifest.tools {
        if commands.contains(name.as_str()) {
            continue;
        }
        let tool = native
            .as_ref()
            .and_then(|backend| backend.tools.get(name))
            .context("tool has no registered implementation")?;
        ensure!(
            tool.definition["name"] == *name && tool.definition["inputSchema"]["type"] == "object",
            "invalid native tool definition"
        );
    }
    if let Some(native) = &native {
        ensure!(
            native
                .tools
                .keys()
                .all(|name| manifest.tools.contains(name)),
            "native tool missing from manifest"
        );
    }
    ensure!(
        manifest.inject.iter().all(|key| !key.trim().is_empty())
            && manifest
                .inject
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == manifest.inject.len(),
        "invalid or duplicate service dependency"
    );
    Ok(Package {
        root,
        manifest,
        description,
        scope: None,
        origin,
        native,
        remote: None,
    })
}
#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    packages: Vec<PathBuf>,
    enabled: BTreeMap<String, bool>,
    removed: std::collections::BTreeSet<String>,
    remotes: BTreeMap<String, remote::Config>,
}
fn settings_path() -> PathBuf {
    crate::ai_proxy::dsh_config_path().with_file_name("skill-packages.json")
}
fn settings() -> Result<Settings> {
    let path = settings_path();
    if !path.exists() {
        return Ok(Settings::default());
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
#[derive(Default)]
pub struct Registry {
    entries: RwLock<BTreeMap<String, Package>>,
    pub transition: Mutex<()>,
}
impl Registry {
    pub fn is_remote(&self, id: &str) -> bool {
        self.entries
            .read()
            .unwrap()
            .get(id)
            .is_some_and(|p| p.remote.is_some())
    }
    pub fn declares_tool(&self, name: &str) -> bool {
        self.entries
            .read()
            .unwrap()
            .values()
            .any(|package| package.manifest.tools.iter().any(|tool| tool == name))
    }
    pub fn skills(&self) -> Value {
        json!(
            self.entries
                .read()
                .unwrap()
                .values()
                .filter(|package| package.scope.is_some())
                .map(
                    |package| json!({"name":package.manifest.id,"description":package.description})
                )
                .collect::<Vec<_>>()
        )
    }
    pub fn owns_tool(&self, id: &str, name: &str) -> bool {
        self.entries.read().unwrap().get(id).is_some_and(|package| {
            package.scope.is_some() && package.manifest.tools.iter().any(|tool| tool == name)
        })
    }
    /// When an enabled plugin declares `name` with `"executor": "page"`, the
    /// name its page knows the tool by (without the host's `plugin_id__` prefix):
    /// the plugin's page runs it, not the host or the plugin's service.
    pub fn page_tool(&self, name: &str) -> Option<String> {
        self.entries.read().unwrap().values().find_map(|package| {
            let declared = package.scope.is_some()
                && package.native.as_ref().is_some_and(|native| {
                    native
                        .tools
                        .get(name)
                        .is_some_and(|tool| tool.definition["executor"] == "page")
                });
            declared.then(|| {
                let prefix = format!("{}__", package.manifest.id.replace('-', "_"));
                name.strip_prefix(&prefix).unwrap_or(name).to_owned()
            })
        })
    }
    pub fn remote_tool(&self, name: &str) -> Option<Arc<remote::Connection>> {
        self.entries
            .read()
            .unwrap()
            .values()
            .find(|p| p.scope.is_some() && p.manifest.tools.iter().any(|t| t == name))
            .and_then(|p| p.remote.clone())
    }
    pub fn catalog(&self) -> Value {
        json!(self.entries.read().unwrap().values().map(|package|json!({
            "id":package.manifest.id,"name":package.manifest.id,"title":package.manifest.title,
            "description":package.description,"enabled":package.scope.is_some(),"scope":package.scope,
            "tools":package.manifest.tools,"pages":package.manifest.pages,"package_root":package.root,"origin":package.origin,"inject":package.manifest.inject,"remote":package.remote.is_some(),
        })).collect::<Vec<_>>())
    }
    pub fn read(&self, name: &str) -> Result<Value> {
        let package = self
            .entries
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .context("skill package not found")?;
        ensure!(package.scope.is_some(), "skill package is disabled");
        if let Some(remote) = &package.remote {
            return Ok(
                json!({"skill":name,"content":remote.skill,"tools":package.manifest.tools,"activation_scope":"current_user_turn","package_scope":package.scope}),
            );
        }
        let current = reload_package(&package)?;
        ensure!(
            serde_json::to_value(&current.manifest)? == serde_json::to_value(&package.manifest)?,
            "package manifest changed; reload it before loading"
        );
        let path = crate::file_edit::workspace_path(&package.root, &package.manifest.skill)?;
        let content = std::fs::read_to_string(&path)?;
        ensure!(content.len() <= 128000, "skill body too large");
        Ok(
            json!({"skill":name,"content":content,"tools":package.manifest.tools,"source":path,
            "content_hash":crate::symbol_description::content_hash(content.as_bytes()),"activation_scope":"current_user_turn","package_scope":package.scope}),
        )
    }
    pub fn attachment_extensions(&self, name: &str) -> Result<Vec<String>> {
        let entries = self.entries.read().unwrap();
        let package = entries.get(name).context("plugin unavailable")?;
        ensure!(package.scope.is_some(), "plugin disabled");
        Ok(package
            .manifest
            .attachment_extensions
            .clone()
            .unwrap_or_default())
    }
    pub fn tools(&self, active: &std::collections::BTreeSet<String>) -> Vec<Value> {
        let installed = self.entries.read().unwrap();
        active.iter().filter_map(|id|installed.get(id)).filter(|package|package.scope.is_some())
            .flat_map(Self::definitions)
            .map(|tool|json!({"type":"function","function":{"name":tool["name"],"description":tool["description"],"parameters":tool["inputSchema"]}})).collect()
    }
    fn definitions(package: &Package) -> Vec<Value> {
        package
            .manifest
            .tools
            .iter()
            .filter_map(|name| {
                if let Some(tool) = package
                    .manifest
                    .command_tools
                    .iter()
                    .find(|tool| &tool.name == name)
                {
                    Some(tool.definition())
                } else {
                    package
                        .native
                        .as_ref()?
                        .tools
                        .get(name)
                        .map(|tool| tool.definition.clone())
                }
            })
            .collect()
    }
    pub fn allows(
        &self,
        name: &str,
        mode: crate::agent_service::PermissionMode,
        ban_execution: bool,
    ) -> bool {
        let entries = self.entries.read().unwrap();
        for package in entries.values() {
            if package
                .manifest
                .command_tools
                .iter()
                .any(|tool| tool.name == name)
            {
                return ToolPermission::Execution.allows(mode, ban_execution);
            }
            if let Some(tool) = package
                .native
                .as_ref()
                .and_then(|backend| backend.tools.get(name))
            {
                return tool.permission.allows(mode, ban_execution);
            }
        }
        mode.allows(name)
    }
    pub fn scopes(&self, active: &std::collections::BTreeSet<String>) -> BTreeMap<String, ScopeId> {
        let entries = self.entries.read().unwrap();
        active
            .iter()
            .filter_map(|id| Some((id.clone(), entries.get(id)?.scope?)))
            .collect()
    }
    pub fn valid_active(
        &self,
        active: &std::collections::BTreeSet<String>,
        scopes: &BTreeMap<String, ScopeId>,
    ) -> std::collections::BTreeSet<String> {
        let current = self.scopes(active);
        active
            .iter()
            .filter(|id| current.get(*id).is_some() && current.get(*id) == scopes.get(*id))
            .cloned()
            .collect()
    }
    pub fn page(&self, id: &str, page_id: &str, resource: Option<&str>) -> Result<PathBuf> {
        let package = self
            .entries
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .context("package not found")?;
        ensure!(package.scope.is_some(), "package is disabled");
        let page = package
            .manifest
            .pages
            .iter()
            .find(|page| page.id == page_id)
            .context("page not found")?;
        ensure!(
            page.renderer == "html" || package.origin == Origin::External,
            "project frontend contributions are served by the shared frontend build"
        );
        let entry = crate::file_edit::workspace_path(&package.root, &page.entry)?;
        if let Some(resource) = resource {
            Ok(crate::file_edit::workspace_path(
                entry.parent().context("page has no directory")?,
                resource,
            )?)
        } else {
            Ok(entry)
        }
    }
    pub fn remote(&self, id: &str) -> Result<Arc<remote::Connection>> {
        let entries = self.entries.read().unwrap();
        let package = entries.get(id).context("plugin not found")?;
        ensure!(package.scope.is_some(), "plugin disabled");
        package.remote.clone().context("not a remote plugin")
    }
    pub fn remote_page(
        &self,
        id: &str,
        page: &str,
    ) -> Result<Option<(Arc<remote::Connection>, String)>> {
        let entries = self.entries.read().unwrap();
        let package = entries.get(id).context("plugin not found")?;
        ensure!(package.scope.is_some(), "plugin disabled");
        let entry = package
            .manifest
            .pages
            .iter()
            .find(|p| p.id == page)
            .context("page not found")?;
        Ok(package
            .remote
            .clone()
            .map(|remote| (remote, entry.entry.clone())))
    }
}

pub struct PackagesPlugin;
impl Plugin for PackagesPlugin {
    fn id(&self) -> &str {
        "skill-packages"
    }
    fn requires(&self) -> Vec<String> {
        ["tools"].into_iter().map(str::to_owned).collect()
    }
    fn start(&self, ctx: &PluginContext<'_>) -> Result<()> {
        let tools = ctx.require::<ToolCatalog>("tools")?;
        ctx.provide("skill-packages", tools.packages.clone())?;
        let registry = tools.packages.clone();
        ctx.effect(move || registry.entries.write().unwrap().clear());
        let config = settings()?;
        for package in crate::builtin_plugins::packages()? {
            let id = package.manifest.id.clone();
            tools
                .packages
                .entries
                .write()
                .unwrap()
                .insert(id.clone(), package.clone());
            if config.enabled.get(&id).copied().unwrap_or(true) {
                ctx.mount(PackagePlugin(package))?;
            }
        }
        let mut paths = config.packages;
        if let Some(path) = std::env::var_os("DSH_SKILL_PACKAGES_DIR") {
            for entry in std::fs::read_dir(path)? {
                let path = entry?.path();
                if path.join("plugin.json").is_file() {
                    paths.push(path);
                }
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for path in paths {
            let canonical = path.canonicalize()?;
            if !seen.insert(canonical.clone()) {
                continue;
            }
            if tools
                .packages
                .entries
                .read()
                .unwrap()
                .values()
                .any(|package| package.origin == Origin::Builtin && package.root == canonical)
            {
                continue;
            }
            let package = load(&path)?;
            let id = package.manifest.id.clone();
            if config.removed.contains(&id) {
                continue;
            }
            ensure!(
                !tools.packages.entries.read().unwrap().contains_key(&id),
                "duplicate package ID: {id}"
            );
            tools
                .packages
                .entries
                .write()
                .unwrap()
                .insert(id.clone(), package.clone());
            if config.enabled.get(&id).copied().unwrap_or(true) {
                ctx.mount(PackagePlugin(package))?;
            }
        }
        Ok(())
    }
}
pub struct PackagePlugin(pub Package);
impl Plugin for PackagePlugin {
    fn id(&self) -> &str {
        &self.0.manifest.id
    }
    fn requires(&self) -> Vec<String> {
        [
            vec!["tools".into(), "skill-packages".into()],
            self.0.manifest.inject.clone(),
        ]
        .concat()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
    }
    fn start(&self, ctx: &PluginContext<'_>) -> Result<()> {
        let tools = ctx.require::<ToolCatalog>("tools")?;
        ensure!(
            !tools
                .packages
                .entries
                .read()
                .unwrap()
                .values()
                .any(|package| package.manifest.id != self.0.manifest.id
                    && package.manifest.tools.iter().any(|name| self
                        .0
                        .manifest
                        .tools
                        .contains(name))),
            "tool name belongs to another installed plugin"
        );
        let names = self
            .0
            .manifest
            .tools
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let root = self.0.root.clone();
        let commands = self.0.manifest.command_tools.clone();
        let native = self.0.native.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let stop = cancel.clone();
        ctx.effect(move || stop.cancel());
        let handler: ToolHandler = Arc::new(move |workspace, name, args| {
            let root = root.clone();
            let commands = commands.clone();
            let native = native.clone();
            let cancel = cancel.clone();
            Box::pin(async move {
                ensure!(!cancel.is_cancelled(), "skill package is stopped");
                if let Some(tool) = commands.iter().find(|tool| tool.name == name) {
                    return command::execute(&root, workspace.root(), tool, args, &cancel).await;
                }
                let tool = native
                    .as_ref()
                    .and_then(|backend| backend.tools.get(&name))
                    .context("native contribution unavailable")?;
                tokio::select! {
                    result = (tool.handler)(workspace, name, args) => result,
                    _ = cancel.cancelled() => Err(anyhow::anyhow!("plugin stopped during native tool execution")),
                }
            })
        });
        tools.register(&names, handler)?;
        let unregister = tools.clone();
        let owned = self.0.manifest.tools.clone();
        ctx.effect(move || {
            unregister.unregister(&owned.iter().map(String::as_str).collect::<Vec<_>>())
        });
        ctx.provide(&format!("plugin:{}", self.0.manifest.id), Arc::new(()))?;
        let mut current = self.0.clone();
        current.scope = Some(ctx.scope());
        tools
            .packages
            .entries
            .write()
            .unwrap()
            .insert(current.manifest.id.clone(), current);
        let registry = tools.packages.clone();
        let id = self.0.manifest.id.clone();
        ctx.effect(move || {
            if let Some(package) = registry.entries.write().unwrap().get_mut(&id) {
                package.scope = None;
            }
            crate::mcp::notify_tools_changed();
        });
        crate::mcp::notify_tools_changed();
        Ok(())
    }
}

fn save_settings(config: &Settings) -> Result<()> {
    let path = settings_path();
    std::fs::create_dir_all(path.parent().context("settings directory missing")?)?;
    let original = crate::file_edit::snapshot(&path)?;
    crate::file_edit::commit(
        &path,
        original.as_deref(),
        &serde_json::to_vec_pretty(config)?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Install a local package without restarting the application or changing its files.
pub fn install(runtime: &PluginRuntime, registry: &Registry, path: &Path) -> Result<Value> {
    let _transition = registry.transition.lock().unwrap();
    let package = load(path)?;
    let id = package.manifest.id.clone();
    ensure!(
        !registry.entries.read().unwrap().contains_key(&id),
        "package already installed; use reload"
    );
    let mut config = settings()?;
    let parent = runtime
        .snapshot()
        .into_iter()
        .find(|p| p.id == "skill-packages")
        .context("package service unavailable")?
        .scope;
    let root = package.root.clone();
    let scope = runtime.mount(parent, PackagePlugin(package))?;
    if !config.packages.contains(&root) {
        config.packages.push(root);
    }
    config.enabled.insert(id.clone(), true);
    config.removed.remove(&id);
    if let Err(error) = save_settings(&config) {
        runtime.unmount(scope)?;
        registry.entries.write().unwrap().remove(&id);
        return Err(error);
    }
    Ok(registry.catalog())
}

pub fn reload(runtime: &PluginRuntime, registry: &Registry, id: &str) -> Result<Value> {
    let _transition = registry.transition.lock().unwrap();
    let old = registry
        .entries
        .read()
        .unwrap()
        .get(id)
        .cloned()
        .context("package not found")?;
    let current = reload_package(&old)?;
    ensure!(current.manifest.id == id, "reload cannot change package ID");
    if let Some(scope) = old.scope {
        let parent = runtime
            .snapshot()
            .into_iter()
            .find(|p| p.id == "skill-packages")
            .context("package service unavailable")?
            .scope;
        runtime.unmount(scope)?;
        if let Err(error) = runtime.mount(parent, PackagePlugin(current)) {
            // Restore the last known working contributions, even if on-disk files are invalid now.
            runtime.mount(parent, PackagePlugin(old))?;
            return Err(error);
        }
    } else {
        registry
            .entries
            .write()
            .unwrap()
            .insert(id.to_owned(), current);
    }
    Ok(registry.catalog())
}

pub fn remove(runtime: &PluginRuntime, registry: &Registry, id: &str) -> Result<Value> {
    let _transition = registry.transition.lock().unwrap();
    let old = registry
        .entries
        .read()
        .unwrap()
        .get(id)
        .cloned()
        .context("package not found")?;
    ensure!(
        old.origin == Origin::External,
        "project-owned plugins can be disabled, not removed from the installed package roster"
    );
    let mut config = settings()?;
    config
        .packages
        .retain(|path| path.canonicalize().ok().as_ref() != Some(&old.root));
    config.enabled.remove(id);
    config.remotes.remove(id);
    config.removed.insert(id.to_owned());
    let parent = runtime
        .snapshot()
        .into_iter()
        .find(|p| p.id == "skill-packages")
        .context("package service unavailable")?
        .scope;
    if let Some(scope) = old.scope {
        runtime.unmount(scope)?;
    }
    if let Err(error) = save_settings(&config) {
        if old.scope.is_some() {
            runtime.mount(parent, PackagePlugin(old))?;
        }
        return Err(error);
    }
    registry.entries.write().unwrap().remove(id);
    Ok(registry.catalog())
}

pub fn set_enabled(
    runtime: &PluginRuntime,
    registry: &Registry,
    id: &str,
    enabled: bool,
    persist: bool,
) -> Result<Value> {
    let _transition = registry.transition.lock().unwrap();
    let package = registry
        .entries
        .read()
        .unwrap()
        .get(id)
        .cloned()
        .context("package not found")?;
    let parent = runtime
        .snapshot()
        .into_iter()
        .find(|plugin| plugin.id == "skill-packages")
        .context("skill packages service is not mounted")?
        .scope;
    let mut config = if persist { Some(settings()?) } else { None };
    if enabled && package.scope.is_none() {
        runtime.mount(parent, PackagePlugin(reload_package(&package)?))?;
    }
    if !enabled && let Some(scope) = package.scope {
        runtime.unmount(scope)?;
    }
    if let Some(config) = config.as_mut() {
        config.enabled.insert(id.to_owned(), enabled);
        if let Err(error) = save_settings(config) {
            if enabled && package.scope.is_none() {
                let scope = {
                    registry
                        .entries
                        .read()
                        .unwrap()
                        .get(id)
                        .and_then(|package| package.scope)
                };
                if let Some(scope) = scope {
                    runtime.unmount(scope)?;
                }
            }
            if !enabled && package.scope.is_some() {
                runtime.mount(parent, PackagePlugin(package))?;
            }
            return Err(error.into());
        }
    }
    Ok(registry.catalog())
}

pub async fn call_tool(
    axum::extract::State(state): axum::extract::State<crate::agent_service::AgentServiceState>,
    axum::extract::Path((id, name)): axum::extract::Path<(String, String)>,
    axum::Json(args): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let result = async {
        let catalog = state.tool_catalog.context("tool registry unavailable")?;
        ensure!(
            catalog.packages.owns_tool(&id, &name),
            "package tool is unavailable"
        );
        ensure!(
            catalog.packages.allows(&name, state.permission_mode, false),
            "permission_denied: {name}"
        );
        catalog.execute(state.workspace, &name, args).await
    }
    .await;
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}

#[cfg(test)]
pub(crate) fn test_runtime() -> (Arc<PluginRuntime>, Arc<ToolCatalog>) {
    struct Services(Arc<ToolCatalog>);
    impl Plugin for Services {
        fn id(&self) -> &str {
            "skill-packages"
        }
        fn start(&self, ctx: &PluginContext<'_>) -> Result<()> {
            ctx.provide("tools", self.0.clone())?;
            ctx.provide("skill-packages", self.0.packages.clone())
        }
    }
    let runtime = Arc::new(PluginRuntime::new());
    let tools = Arc::new(ToolCatalog::default());
    let parent = runtime
        .mount(crate::plugin_runtime::ROOT, Services(tools.clone()))
        .unwrap();
    runtime
        .mount(
            parent,
            PackagePlugin(crate::builtin_plugins::packages().unwrap().remove(0)),
        )
        .unwrap();
    (runtime, tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skill_plugin_origin_is_explicit_and_native_contributions_use_the_common_protocol() {
        // Being downloaded inside this repository never makes a package builtin.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".codex-workspace-mcp")
            .join(format!(
                "plugin-protocol-{}",
                crate::agent_service::uuid_like()
            ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            "---\nname: protocol-test\ndescription: Protocol test\n---\nInstructions.",
        )
        .unwrap();
        let mut manifest = json!({"version":1,"id":"protocol-test","title":"Protocol","skill":"SKILL.md","inject":["plugin:code-flow"],"tools":["arbitrary_native"]});
        std::fs::write(root.join("plugin.json"), manifest.to_string()).unwrap();
        let native = Arc::new(crate::plugin_protocol::NativeContributions {
            tools: BTreeMap::from([(
                "arbitrary_native".into(),
                crate::plugin_protocol::NativeTool {
                    definition: json!({"name":"arbitrary_native","description":"Any registered native contribution","inputSchema":{"type":"object"}}),
                    handler: Arc::new(|_, _, args| Box::pin(async move { Ok(args) })),
                    permission: ToolPermission::ReadOnly,
                },
            )]),
            frontend_entries: vec![],
        });
        let builtin = load_builtin(&root, native).unwrap();
        assert!(builtin.origin == Origin::Builtin);
        assert!(
            load(&root).is_err(),
            "external discovery cannot borrow a project-native implementation"
        );
        let (runtime, catalog) = test_runtime();
        let parent = runtime
            .snapshot()
            .into_iter()
            .find(|p| p.id == "skill-packages")
            .unwrap()
            .scope;
        let scope = runtime.mount(parent, PackagePlugin(builtin)).unwrap();
        assert!(catalog.contains("arbitrary_native"));
        assert_eq!(
            catalog
                .packages
                .tools(&std::collections::BTreeSet::from(["protocol-test".into()]))[0]["function"]
                ["name"],
            "arbitrary_native"
        );
        assert!(
            set_enabled(&runtime, &catalog.packages, "code-flow", false, false).is_err(),
            "a live dependency prevents provider unloading"
        );
        runtime.unmount(scope).unwrap();
        assert!(!catalog.contains("arbitrary_native"));
        manifest["tools"] = json!([]);
        std::fs::write(root.join("plugin.json"), manifest.to_string()).unwrap();
        assert!(load(&root).unwrap().origin == Origin::External);
        manifest["origin"] = json!("builtin");
        std::fs::write(root.join("plugin.json"), manifest.to_string()).unwrap();
        assert!(
            load(&root).is_err(),
            "a manifest cannot promote itself to project membership"
        );
        runtime.shutdown().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn dynamic_command_tools_are_owned_permission_gated_and_cancelled_on_unload() {
        let root = std::env::temp_dir().join(format!(
            "skill-command-{}",
            crate::agent_service::uuid_like()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("SKILL.md"), "---\nname: command-test\ndescription: Test dynamic backend\n---\nRead this before calling the tool.").unwrap();
        std::fs::write(
            root.join("tool.py"),
            r#"import json,sys,time,pathlib
r=json.load(sys.stdin)
if r['params']['arguments'].get('wait'):
    pathlib.Path('started').write_text('started')
    time.sleep(30)
print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':r['params']}))
"#,
        )
        .unwrap();
        let manifest = json!({"version":1,"id":"command-test","title":"Command test","skill":"SKILL.md","tools":["package_echo"],"command_tools":[{"name":"package_echo","description":"Echo structured arguments","input_schema":{"type":"object","properties":{}},"program":"python3","args":["tool.py"],"timeout_seconds":1}]});
        std::fs::write(root.join("plugin.json"), manifest.to_string()).unwrap();
        let (runtime, catalog) = test_runtime();
        let parent = runtime
            .snapshot()
            .into_iter()
            .find(|p| p.id == "skill-packages")
            .unwrap()
            .scope;
        let scope = runtime
            .mount(parent, PackagePlugin(load(&root).unwrap()))
            .unwrap();
        let active = std::collections::BTreeSet::from(["command-test".to_owned()]);
        let scopes = catalog.packages.scopes(&active);
        assert_eq!(
            catalog.packages.tools(&active)[0]["function"]["name"],
            "package_echo"
        );
        assert!(!catalog.packages.allows(
            "package_echo",
            crate::agent_service::PermissionMode::ReadOnly,
            false
        ));
        assert!(!catalog.packages.allows(
            "package_echo",
            crate::agent_service::PermissionMode::WorkspaceWrite,
            false
        ));
        assert!(!catalog.packages.allows(
            "package_echo",
            crate::agent_service::PermissionMode::FullAccess,
            true
        ));
        let workspace = Arc::new(crate::tools::Workspace::new(&root).unwrap());
        let args = json!({"text":"$(touch should-not-exist); `echo bad`\n原文"});
        let result = catalog
            .execute(workspace.clone(), "package_echo", args.clone())
            .await
            .unwrap();
        assert_eq!(result["arguments"], args);
        assert_eq!(result["workspace_root"], root.to_string_lossy().as_ref());
        assert!(!root.join("should-not-exist").exists());
        // Invalid replacement preserves the original working plugin.
        std::fs::write(root.join("plugin.json"), "{}").unwrap();
        assert!(reload(&runtime, &catalog.packages, "command-test").is_err());
        assert!(catalog.contains("package_echo"));
        std::fs::write(root.join("plugin.json"), manifest.to_string()).unwrap();
        let error = catalog
            .execute(workspace.clone(), "package_echo", json!({"wait":true}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        std::fs::remove_file(root.join("started")).unwrap();
        let running = {
            let catalog = catalog.clone();
            tokio::spawn(async move {
                catalog
                    .execute(workspace, "package_echo", json!({"wait":true}))
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !root.join("started").exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        runtime.unmount(scope).unwrap();
        assert!(
            running
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("stopped")
        );
        assert!(!catalog.contains("package_echo"));
        assert!(catalog.packages.valid_active(&active, &scopes).is_empty());
        runtime
            .mount(parent, PackagePlugin(load(&root).unwrap()))
            .unwrap();
        assert!(catalog.packages.valid_active(&active, &scopes).is_empty());
        runtime.shutdown().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn package_owns_skill_tools_and_pages_across_unload_and_reload() {
        let (runtime, tools) = test_runtime();
        assert!(tools.contains("read_code_flow"));
        assert!(tools.packages.owns_tool("code-flow", "read_code_flow"));
        assert_eq!(tools.packages.skills()[0]["name"], "code-flow");
        let original = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("src/builtin_plugins/code_flow/SKILL.md"),
        )
        .unwrap();
        assert_eq!(
            tools.packages.read("code-flow").unwrap()["content"],
            original
        );
        assert!(tools.packages.tools(&Default::default()).is_empty());
        assert_eq!(
            tools
                .packages
                .tools(&std::collections::BTreeSet::from(["code-flow".into()]))
                .len(),
            1
        );
        set_enabled(&runtime, &tools.packages, "code-flow", false, false).unwrap();
        assert!(!tools.contains("read_code_flow"));
        assert!(tools.packages.read("code-flow").is_err());
        assert!(tools.packages.skills().as_array().unwrap().is_empty());
        assert_eq!(tools.packages.catalog()[0]["enabled"], false);
        assert!(
            tools
                .packages
                .tools(&std::collections::BTreeSet::from(["code-flow".into()]))
                .is_empty()
        );
        set_enabled(&runtime, &tools.packages, "code-flow", true, false).unwrap();
        assert!(tools.contains("read_code_flow"));
        runtime.shutdown().unwrap();
        assert!(!tools.contains("read_code_flow"));
        assert!(tools.packages.skills().as_array().unwrap().is_empty());
    }
    #[test]
    fn local_html_packages_and_resources_obey_ownership_and_path_boundaries() {
        let root =
            std::env::temp_dir().join(format!("skill-page-{}", crate::agent_service::uuid_like()));
        std::fs::create_dir_all(root.join("page")).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            "---\nname: sample\ndescription: Sample skill\n---\nBody.",
        )
        .unwrap();
        std::fs::write(root.join("page/index.html"), "<p>Owned page</p>").unwrap();
        let mut manifest = json!({"version":1,"id":"sample","title":"Sample","skill":"SKILL.md","pages":[{"id":"view","title":"Sample","entry":"page/index.html","renderer":"html"}]});
        std::fs::write(root.join("plugin.json"), manifest.to_string()).unwrap();
        let mut package = load(&root).unwrap();
        package.scope = Some(7);
        let registry = Registry::default();
        registry
            .entries
            .write()
            .unwrap()
            .insert("sample".into(), package);
        assert_eq!(
            std::fs::read_to_string(registry.page("sample", "view", None).unwrap()).unwrap(),
            "<p>Owned page</p>"
        );
        assert!(
            registry
                .page("sample", "view", Some("../SKILL.md"))
                .is_err()
        );
        assert!(registry.page("sample", "unknown", None).is_err());
        registry
            .entries
            .write()
            .unwrap()
            .get_mut("sample")
            .unwrap()
            .scope = None;
        assert!(registry.page("sample", "view", None).is_err());
        manifest["pages"][0]["entry"] = json!("../outside.html");
        std::fs::write(root.join("plugin.json"), manifest.to_string()).unwrap();
        assert!(load(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
