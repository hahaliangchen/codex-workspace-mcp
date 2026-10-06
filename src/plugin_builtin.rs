//! Built-in plugins bridging the existing workspace capabilities into the
//! scoped runtime. The MCP implementations remain the single tool backend.

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

use anyhow::{Result, ensure};
use futures::future::BoxFuture;
use reqwest::Client;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    agent_service::AgentServiceState,
    ai_proxy::AiProxyConfig,
    plugin_runtime::{Plugin, PluginContext},
    tools::Workspace,
};

#[derive(Default)]
pub struct LifecycleLog {
    entries: Mutex<VecDeque<Value>>,
}

impl LifecycleLog {
    fn push(&self, event: &str, data: &Value) {
        let mut entries = self.entries.lock().unwrap();
        entries.push_back(json!({"event":event,"data":data}));
        if entries.len() > 100 {
            entries.pop_front();
        }
    }
    pub fn snapshot(&self) -> Vec<Value> {
        self.entries.lock().unwrap().iter().cloned().collect()
    }
}

pub struct DiagnosticsPlugin;
impl Plugin for DiagnosticsPlugin {
    fn id(&self) -> &'static str {
        "diagnostics"
    }
    fn start(&self, context: &PluginContext<'_>) -> Result<()> {
        let log = Arc::new(LifecycleLog::default());
        context.provide("plugin-events", log.clone())?;
        let started = log.clone();
        context.on("plugin/started", move |data| {
            started.push("plugin/started", data)
        });
        context.on("plugin/stopped", move |data| {
            log.push("plugin/stopped", data)
        });
        Ok(())
    }
}

pub struct WorkspacePlugin(pub Arc<Workspace>);

impl Plugin for WorkspacePlugin {
    fn id(&self) -> &'static str {
        "workspace"
    }
    fn start(&self, context: &PluginContext<'_>) -> Result<()> {
        context.provide("workspace", self.0.clone())
    }
}

pub struct ModelSettings {
    pub path: PathBuf,
    pub config: Arc<tokio::sync::RwLock<AiProxyConfig>>,
}

pub struct ModelSettingsPlugin(pub Arc<ModelSettings>);

impl Plugin for ModelSettingsPlugin {
    fn id(&self) -> &'static str {
        "model-settings"
    }
    fn start(&self, context: &PluginContext<'_>) -> Result<()> {
        context.provide("model-settings", self.0.clone())
    }
}

pub type ToolHandler =
    Arc<dyn Fn(Arc<Workspace>, String, Value) -> BoxFuture<'static, Result<Value>> + Send + Sync>;

#[derive(Default)]
pub struct ToolCatalog {
    handlers: RwLock<BTreeMap<String, ToolHandler>>,
}

impl ToolCatalog {
    pub fn register(&self, names: &[&str], handler: ToolHandler) -> Result<()> {
        let mut installed = self.handlers.write().unwrap();
        for name in names {
            ensure!(
                !installed.contains_key(*name),
                "tool {name} is already registered"
            );
        }
        installed.extend(
            names
                .iter()
                .map(|name| ((*name).to_owned(), handler.clone())),
        );
        Ok(())
    }

    pub fn unregister(&self, names: &[&str]) {
        let mut installed = self.handlers.write().unwrap();
        for name in names {
            installed.remove(*name);
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.handlers.read().unwrap().contains_key(name)
    }

    pub async fn execute(
        &self,
        workspace: Arc<Workspace>,
        name: &str,
        args: Value,
    ) -> Result<Value> {
        let handler = self
            .handlers
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("tool {name} is not registered"))?;
        handler(workspace, name.to_owned(), args).await
    }
}

fn mcp_handler() -> ToolHandler {
    Arc::new(|workspace, name, mut args| {
        Box::pin(async move {
            if let Some(object) = args.as_object_mut() {
                object.insert(
                    "workspace_root".into(),
                    json!(workspace.root().display().to_string()),
                );
            }
            let response =
                crate::mcp::call_tool(&workspace, json!({"name":name,"arguments":args})).await?;
            Ok(response
                .get("structuredContent")
                .cloned()
                .unwrap_or(response))
        })
    })
}

fn command_handler() -> ToolHandler {
    Arc::new(|workspace, _, args| {
        Box::pin(async move {
            crate::agent_service::execute_run_command(&workspace, &args).await
        })
    })
}

pub struct ToolCatalogPlugin(pub Client);

impl Plugin for ToolCatalogPlugin {
    fn id(&self) -> &'static str {
        "tools"
    }
    fn requires(&self) -> &'static [&'static str] {
        &["workspace"]
    }
    fn start(&self, context: &PluginContext<'_>) -> Result<()> {
        context.provide("tools", Arc::new(ToolCatalog::default()))?;
        context.mount(FileToolsPlugin)?;
        context.mount(MemoryPlugin)?;
        context.mount(CodeIndexPlugin)?;
        context.mount(AgentTasksPlugin(self.0.clone()))?;
        Ok(())
    }
}

const FILE_TOOLS: &[&str] = &[
    "workspace_info",
    "list_dir",
    "read_file",
    "read_file_lines",
    "search_text",
    "write_file",
    "replace_range",
    "edit_file",
];
const COMMAND_TOOLS: &[&str] = &["run_command"];
const MEMORY_TOOLS: &[&str] = &[
    "record_work_memory",
    "list_work_memory",
    "search_work_memory",
    "record_architecture_memory",
    "list_architecture_memory",
    "search_architecture_memory",
    "record_symbol_business_context",
    "list_symbol_business_context",
    "search_symbol_business_context",
];
const INDEX_TOOLS: &[&str] = &[
    "search_code_map",
    "index_go_workspace",
    "go_index_status",
    "list_go_symbols",
    "search_go_symbols",
    "read_go_symbol",
    "index_rust_workspace",
    "rust_index_status",
    "list_rust_symbols",
    "search_rust_symbols",
    "read_rust_symbol",
    "index_ts_workspace",
    "ts_index_status",
    "list_ts_symbols",
    "search_ts_symbols",
    "read_ts_symbol",
    "index_python_workspace",
    "python_index_status",
    "list_python_symbols",
    "search_python_symbols",
    "read_python_symbol",
];

fn register_tools(
    context: &PluginContext<'_>,
    names: &'static [&'static str],
    handler: ToolHandler,
) -> Result<()> {
    let catalog = context.require::<ToolCatalog>("tools")?;
    catalog.register(names, handler)?;
    context.effect(move || catalog.unregister(names));
    Ok(())
}

pub struct FileToolsPlugin;
impl Plugin for FileToolsPlugin {
    fn id(&self) -> &'static str {
        "file-tools"
    }
    fn requires(&self) -> &'static [&'static str] {
        &["tools", "workspace"]
    }
    fn start(&self, context: &PluginContext<'_>) -> Result<()> {
        register_tools(context, FILE_TOOLS, mcp_handler())?;
        register_tools(context, crate::project_process::TOOLS, mcp_handler())?;
        register_tools(context, crate::http_probe::TOOLS, mcp_handler())?;
        register_tools(context, crate::browser_control::TOOLS, mcp_handler())?;
        register_tools(context, COMMAND_TOOLS, command_handler())?;
        context.provide("file-tools", Arc::new(()))
    }
}

pub struct MemoryPlugin;
impl Plugin for MemoryPlugin {
    fn id(&self) -> &'static str {
        "memory"
    }
    fn requires(&self) -> &'static [&'static str] {
        &["tools", "workspace"]
    }
    fn start(&self, context: &PluginContext<'_>) -> Result<()> {
        register_tools(context, MEMORY_TOOLS, mcp_handler())?;
        context.provide("memory", Arc::new(()))
    }
}

pub struct CodeIndexPlugin;
impl Plugin for CodeIndexPlugin {
    fn id(&self) -> &'static str {
        "code-index"
    }
    fn requires(&self) -> &'static [&'static str] {
        &["tools", "workspace"]
    }
    fn start(&self, context: &PluginContext<'_>) -> Result<()> {
        register_tools(context, INDEX_TOOLS, mcp_handler())?;
        context.provide("code-index", Arc::new(()))
    }
}

pub struct AgentTasksPlugin(pub Client);

impl Plugin for AgentTasksPlugin {
    fn id(&self) -> &'static str {
        "agent-tasks"
    }
    fn requires(&self) -> &'static [&'static str] {
        &[
            "workspace",
            "model-settings",
            "tools",
            "file-tools",
            "memory",
            "code-index",
        ]
    }
    fn start(&self, context: &PluginContext<'_>) -> Result<()> {
        let workspace = context.require::<Workspace>("workspace")?;
        let settings = context.require::<ModelSettings>("model-settings")?;
        let tools = context.require::<ToolCatalog>("tools")?;
        let plugin_cancel = CancellationToken::new();
        let dispose_cancel = plugin_cancel.clone();
        context.effect(move || dispose_cancel.cancel());
        let mut state = AgentServiceState {
            workspace,
            client: self.0.clone(),
            provider_url: String::new(),
            api_key: String::new(),
            provider_name: String::new(),
            provider_routes: Default::default(),
            default_model: String::new(),
            model_map: Default::default(),
            reasoning_effort: None,
            fast_mode: false,
            permission_mode: crate::agent_service::PermissionMode::default(),
            enable_subagent: false,
            expert_provider_name: String::new(),
            expert_provider_url: String::new(),
            expert_api_key: String::new(),
            expert_model: String::new(),
            subagent_inherits_model: true,
            observer_enabled: false,
            observer_provider: String::new(),
            observer_provider_url: String::new(),
            observer_api_key: String::new(),
            observer_model: String::new(),
            observer_inherits_model: true,
            visual:Default::default(),
            config: Some(settings.config.clone()),
            config_path: Some(settings.path.clone()),
            tool_catalog: Some(tools),
            plugin_cancel,
            cancellations: Arc::default(),
        };
        // Mounting captures a known-good model snapshot; each new task reloads it.
        if let Ok(config) = settings.config.try_read() {
            crate::ai_proxy::apply_agent_config(&mut state, &config);
        }
        context.provide("agent-tasks", Arc::new(state))
    }
}
