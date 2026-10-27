//! Generic session transport hooks. Business plugins register their own wire
//! adapters; execution, context and vision never select a business workflow.
use crate::{agent_service::AgentServiceState, tools::Workspace};
use anyhow::{Context, Result, ensure};
use futures::future::BoxFuture;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock, RwLock},
};

pub trait SessionIo: Send + Sync {
    fn execute<'a>(
        &'a self,
        state: &'a AgentServiceState,
        task: &'a str,
        workspace: Arc<Workspace>,
        name: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>>;
    fn sync_attachments<'a>(
        &'a self,
        _state: &'a AgentServiceState,
        _task: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn read_asset<'a>(
        &'a self,
        _state: &'a AgentServiceState,
        _task: &'a str,
        _reference: &'a Value,
        _args: &'a Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async { anyhow::bail!("remote resource transport unavailable") })
    }
    fn import_picture<'a>(
        &'a self,
        _state: &'a AgentServiceState,
        _task: &'a str,
        _name: &'a str,
        _media: &'a str,
        _bytes: &'a [u8],
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }
    /// Review instructions the plugin attaches to the images its page tool `tool`
    /// produced (`images`: each image's own meta, in order):
    /// `{instructions, subjects: [one per image]}`, or None when it attaches none.
    fn review_brief<'a>(
        &'a self,
        _state: &'a AgentServiceState,
        _task: &'a str,
        _tool: &'a str,
        _args: &'a Value,
        _document: &'a Value,
        _images: &'a Value,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }
}
fn adapters() -> &'static RwLock<BTreeMap<String, Arc<dyn SessionIo>>> {
    static ADAPTERS: OnceLock<RwLock<BTreeMap<String, Arc<dyn SessionIo>>>> = OnceLock::new();
    ADAPTERS.get_or_init(Default::default)
}
pub fn register(plugin: &str, adapter: Arc<dyn SessionIo>) {
    adapters()
        .write()
        .unwrap()
        .entry(plugin.to_owned())
        .or_insert(adapter);
}
fn adapter(state: &AgentServiceState, task: &str) -> Result<Option<Arc<dyn SessionIo>>> {
    Ok(crate::plugin_sessions::owner(state.workspace.root(), task)?
        .and_then(|owner| adapters().read().unwrap().get(&owner).cloned()))
}
pub async fn execute(
    state: &AgentServiceState,
    task: &str,
    workspace: Arc<Workspace>,
    name: &str,
    args: Value,
) -> Result<Value> {
    if let Some(adapter) = adapter(state, task)? {
        return adapter.execute(state, task, workspace, name, args).await;
    }
    state
        .tool_catalog
        .as_ref()
        .context("tool registry unavailable")?
        .execute(workspace, name, args)
        .await
}
pub async fn sync_attachments(state: &AgentServiceState, task: &str) -> Result<()> {
    if let Some(adapter) = adapter(state, task)? {
        adapter.sync_attachments(state, task).await?
    }
    Ok(())
}
pub async fn read_asset(
    state: &AgentServiceState,
    task: &str,
    reference: &Value,
    args: &Value,
) -> Result<Value> {
    let owner = crate::plugin_sessions::owner(state.workspace.root(), task)?;
    ensure!(
        owner.as_deref() == reference["plugin"].as_str(),
        "REMOTE_ASSET_SCOPE_DENIED"
    );
    adapter(state, task)?
        .context("remote resource transport unavailable")?
        .read_asset(state, task, reference, args)
        .await
}
pub async fn import_picture(
    state: &AgentServiceState,
    task: &str,
    name: &str,
    media: &str,
    bytes: &[u8],
) -> Result<Option<Value>> {
    match adapter(state, task)? {
        Some(adapter) => {
            adapter
                .import_picture(state, task, name, media, bytes)
                .await
        }
        None => Ok(None),
    }
}
pub async fn review_brief(
    state: &AgentServiceState,
    task: &str,
    tool: &str,
    args: &Value,
    document: &Value,
    images: &Value,
) -> Result<Option<Value>> {
    match adapter(state, task)? {
        Some(adapter) => {
            adapter
                .review_brief(state, task, tool, args, document, images)
                .await
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn execution_and_context_modules_do_not_import_presentation_business_rules() {
        for (name, source) in [
            ("execution", include_str!("single_agent.rs")),
            ("plugin dispatch", include_str!("plugin_io.rs")),
            ("browser", include_str!("browser_control.rs")),
            ("attachments", include_str!("attachments.rs")),
            ("task frames", include_str!("single_agent/task_frames.rs")),
            ("step context", include_str!("single_agent/step_context.rs")),
            ("task tree", include_str!("task_tree.rs")),
            ("task board", include_str!("main_task_anchor.rs")),
            ("context assets", include_str!("context_assets.rs")),
            ("vision", include_str!("image_analysis.rs")),
            ("vision jobs", include_str!("visual_jobs.rs")),
            ("delegation", include_str!("collaboration_jobs.rs")),
            ("database", include_str!("agent_service.rs")),
            ("prompt", include_str!("../prompts/single_agent.md")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap();
            for dependency in [
                "ppt_sessions",
                "ppt_plan",
                "ppt_task_state",
                "ppt_master__",
                "ppt-master",
                "native_ppt",
                "remote_ppt",
            ] {
                assert!(
                    !production.contains(dependency),
                    "{name} imports business-specific dependency {dependency}"
                );
            }
        }
    }
    struct ExampleTransport;
    impl super::SessionIo for ExampleTransport {
        fn execute<'a>(
            &'a self,
            _state: &'a crate::agent_service::AgentServiceState,
            _task: &'a str,
            _workspace: std::sync::Arc<crate::tools::Workspace>,
            name: &'a str,
            args: serde_json::Value,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<serde_json::Value>> {
            Box::pin(async move { Ok(serde_json::json!({"tool":name,"arguments":args})) })
        }
    }
    #[tokio::test]
    async fn registered_transport_is_selected_by_session_owner_without_business_names() {
        let root = std::env::temp_dir().join(format!(
            "plugin-transport-{}",
            crate::agent_service::uuid_like()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let state = crate::agent_service::tests::test_state(&root, "127.0.0.1:9".parse().unwrap());
        let conn = crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        let owner = format!("example-{}", crate::agent_service::uuid_like());
        conn.execute("INSERT INTO plugin_sessions VALUES ('task',?1)", [&owner])
            .unwrap();
        super::register(&owner, std::sync::Arc::new(ExampleTransport));
        let result = super::execute(
            &state,
            "task",
            state.workspace.clone(),
            "example__run",
            serde_json::json!({"input":42}),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            serde_json::json!({"tool":"example__run","arguments":{"input":42}})
        );
        assert!(
            super::read_asset(
                &state,
                "task",
                &serde_json::json!({"plugin":"other"}),
                &serde_json::json!({})
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("SCOPE_DENIED")
        );
        super::sync_attachments(&state, "task").await.unwrap();
        assert_eq!(
            super::import_picture(&state, "task", "image", "image/png", &[])
                .await
                .unwrap(),
            None
        );
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
}
