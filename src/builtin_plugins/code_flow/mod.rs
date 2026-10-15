//! Code Flow's own backend contributions; the host knows no Code Flow methods.
mod backend;
use crate::{
    plugin_protocol::{NativeContributions, NativeTool, ToolPermission},
    skill_packages::Package,
};
use anyhow::Result;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub fn package() -> Result<Package> {
    let definition = backend::tool_definition();
    let name = definition["name"].as_str().unwrap().to_owned();
    let handler: crate::builtin_plugins::ToolHandler = Arc::new(|workspace, _, args| {
        Box::pin(async move {
            let root = args["workspace_root"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| workspace.root().to_owned());
            tokio::task::spawn_blocking(move || backend::read(&root, serde_json::from_value(args)?))
                .await?
        })
    });
    let contributions = Arc::new(NativeContributions {
        tools: BTreeMap::from([(
            name,
            NativeTool {
                definition,
                handler,
                permission: ToolPermission::ReadOnly,
            },
        )]),
        frontend_entries: vec!["code-flow.architecture".into()],
    });
    crate::skill_packages::load_builtin(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/builtin_plugins/code_flow"),
        contributions,
    )
}
