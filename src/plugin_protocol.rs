//! Fixed contribution protocol shared by project-owned and installed plugins.
use crate::{agent_service::PermissionMode, builtin_plugins::ToolHandler};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Builtin,
    External,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    pub title: String,
    pub skill: String,
    #[serde(default)]
    pub inject: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub command_tools: Vec<crate::skill_packages::command::CommandTool>,
    #[serde(default)]
    pub pages: Vec<Page>,
    #[serde(default)]
    pub attachment_extensions: Option<Vec<String>>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub id: String,
    pub title: String,
    /// Registered frontend module key, or installed module/resource path.
    pub entry: String,
    pub renderer: String,
}

#[derive(Clone, Copy)]
pub enum ToolPermission {
    ReadOnly,
    WorkspaceWrite,
    Execution,
}
impl ToolPermission {
    pub fn allows(self, mode: PermissionMode, ban_execution: bool) -> bool {
        match self {
            Self::ReadOnly => true,
            Self::WorkspaceWrite => !matches!(mode, PermissionMode::ReadOnly),
            Self::Execution => !ban_execution && matches!(mode, PermissionMode::FullAccess),
        }
    }
}
#[derive(Clone)]
pub struct NativeTool {
    pub definition: Value,
    pub handler: ToolHandler,
    pub permission: ToolPermission,
}
/// A compiled implementation adapter. It contributes through exactly the same
/// package validation, mount and effect cleanup as installed implementations.
#[derive(Clone, Default)]
pub struct NativeContributions {
    pub tools: BTreeMap<String, NativeTool>,
    pub frontend_entries: Vec<String>,
}
pub type NativeBackend = Arc<NativeContributions>;
