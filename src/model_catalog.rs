//! Read-only model service shared by native plugins and page transports.
use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

pub struct ModelCatalog {
    pub config: Arc<RwLock<crate::ai_proxy::AiProxyConfig>>,
    pub path: Option<PathBuf>,
}

impl ModelCatalog {
    pub async fn list(&self) -> Result<Value> {
        if let Some(path) = self.path.clone() {
            let latest =
                tokio::task::spawn_blocking(move || crate::ai_proxy::load_config(&path)).await??;
            *self.config.write().await = latest;
        }
        Ok(crate::ai_proxy::catalog_view(&*self.config.read().await))
    }
}

pub async fn for_agent(state: &crate::agent_service::AgentServiceState) -> Result<Value> {
    ModelCatalog {
        config: state.config.clone().context("model catalog unavailable")?,
        path: state.config_path.clone(),
    }
    .list()
    .await
}

pub async fn for_plugin(
    state: &crate::agent_service::AgentServiceState,
    plugin: &str,
) -> Result<Value> {
    let _ = for_agent(state).await?;
    let config = state
        .config
        .as_ref()
        .context("model catalog unavailable")?
        .read()
        .await;
    Ok(crate::ai_proxy::plugin_catalog_view(&config, plugin))
}

pub async fn restrict(
    state: &mut crate::agent_service::AgentServiceState,
    plugin: &str,
) -> Result<()> {
    let Some(shared) = state.config.clone() else {
        return Ok(());
    };
    let scoped = crate::ai_proxy::restricted_config(&*shared.read().await, plugin)?;
    crate::ai_proxy::apply_agent_config(state, &scoped);
    // Never mutate the shared global configuration with a plugin projection.
    state.config = Some(Arc::new(RwLock::new(scoped)));
    state.config_path = None;
    Ok(())
}

/// Runs for every plugin-owned turn, including submissions through the general API.
pub async fn prepare_turn(
    state: &mut crate::agent_service::AgentServiceState,
    task: &str,
    request: &mut crate::agent_service::ContinueTaskRequest,
) -> Result<()> {
    let Some(plugin) = crate::plugin_sessions::owner(state.workspace.root(), task)? else {
        return Ok(());
    };
    if state.config.is_none() {
        return Ok(());
    } // Standalone test/runtime with no configured catalog.
    restrict(state, &plugin).await?;
    use rusqlite::OptionalExtension;
    let saved:Option<(String,Option<String>,Option<String>,bool)>=crate::agent_service::open_db(state.workspace.root())?
        .query_row("SELECT model,provider_name,reasoning_effort,fast_mode FROM agent_tasks WHERE id=?1",[task],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let saved = saved.context("plugin conversation not found")?;
    let explicit = request.provider.is_some() || request.model.is_some();
    let provider = request
        .provider
        .as_deref()
        .or(saved.1.as_deref())
        .unwrap_or(&state.provider_name);
    let model = request.model.as_deref().unwrap_or(&saved.0);
    let permitted = state
        .provider_routes
        .get(provider)
        .is_some_and(|r| r.models.contains_key(model));
    anyhow::ensure!(
        !explicit || permitted,
        "此插件未获授权使用所选模型，请重新选择可用模型"
    );
    let selection = if permitted {
        ModelRef {
            provider: provider.into(),
            model: model.into(),
        }
    } else {
        ModelRef {
            provider: state.provider_name.clone(),
            model: state.default_model.clone(),
        }
    };
    let directory = crate::ai_proxy::catalog_view(&*state.config.as_ref().unwrap().read().await);
    let cap = &directory["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["provider"] == selection.provider && m["id"] == selection.model)
        .context("plugin model unavailable")?["capabilities"];
    let effort = request
        .reasoning_effort
        .clone()
        .or_else(|| {
            directory["defaults"]["plugin_generation"][&plugin]["reasoning_effort"]
                .as_str()
                .map(str::to_owned)
        })
        .or(saved.2)
        .or(state.reasoning_effort.clone());
    request.reasoning_effort = Some(
        effort
            .filter(|e| {
                cap["reasoning_efforts"]
                    .as_array()
                    .is_some_and(|levels| levels.iter().any(|v| v == e))
            })
            .unwrap_or_default(),
    );
    request.fast_mode = Some(request.fast_mode.unwrap_or(saved.3) && cap["fast_mode"] == true);
    request.provider = Some(selection.provider);
    request.model = Some(selection.model);
    Ok(())
}

pub struct ModelCatalogPlugin;
impl crate::plugin_runtime::Plugin for ModelCatalogPlugin {
    fn id(&self) -> &'static str {
        "model-catalog"
    }
    fn requires(&self) -> Vec<String> {
        vec!["model-settings".into()]
    }
    fn start(&self, ctx: &crate::plugin_runtime::PluginContext<'_>) -> Result<()> {
        let settings = ctx.require::<crate::builtin_plugins::ModelSettings>("model-settings")?;
        ctx.provide(
            "model-catalog",
            Arc::new(ModelCatalog {
                config: settings.config.clone(),
                path: Some(settings.path.clone()),
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_runtime::{PluginRuntime, ROOT};
    #[tokio::test]
    async fn plugin_policy_filters_models_and_blocks_explicit_bypasses_for_all_task_uses() {
        let config:crate::ai_proxy::AiProxyConfig=serde_json::from_value(serde_json::json!({
            "orchestrator_provider":"gpt","orchestrator_model":"main",
            "visual_tool":{"model":"vision"},"context_agent":{"model":"vision","context_window":500000},
            "observer_provider":"gpt","observer_model":"vision",
            "providers":{
                "gpt":{"url":"http://gpt/v1","model_map":{"main":"main","vision":"vision"}},
                "qwen":{"url":"http://qwen/v1","model_map":{"local":"local","other":"other"}}
            },
            "plugin_models":{"code-flow":[{"provider":"qwen","model":"local"}],"denied":[]}
        })).unwrap();
        assert_eq!(
            crate::ai_proxy::plugin_catalog_view(&config, "unconfigured")["models"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert!(
            crate::ai_proxy::plugin_catalog_view(&config, "denied")["models"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let directory = crate::ai_proxy::plugin_catalog_view(&config, "code-flow");
        assert_eq!(directory["models"].as_array().unwrap().len(), 1);
        assert_eq!(directory["defaults"]["agent"]["model"], "local");
        assert_eq!(
            directory["defaults"]["visual_tool"]["model"],
            serde_json::Value::Null
        );
        let root = std::env::temp_dir().join(crate::visual_artifacts::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let (runtime, catalog) = crate::skill_packages::test_runtime();
        let scope = catalog.packages.catalog()[0]["scope"].clone();
        let mut state =
            crate::agent_service::tests::test_state(&root, "127.0.0.1:1".parse().unwrap());
        let shared = Arc::new(RwLock::new(config));
        state.config = Some(shared.clone());
        state.tool_catalog = Some(catalog);
        let response = crate::plugin_sessions::handle(
            state.clone(),
            "code-flow",
            serde_json::json!({"method":"create","package_scope":scope}),
        )
        .await
        .unwrap();
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap(),
        )
        .unwrap();
        let task = body["task_id"].as_str().unwrap();
        // An older conversation may still name a model revoked by the new policy.
        crate::agent_service::open_db(&root).unwrap().execute("UPDATE agent_tasks SET model='main',provider_name='gpt',reasoning_effort='high',fast_mode=1 WHERE id=?1",[task]).unwrap();
        let mut request = serde_json::from_value(serde_json::json!({"prompt":"continue"})).unwrap();
        let mut scoped = state.clone();
        prepare_turn(&mut scoped, task, &mut request).await.unwrap();
        assert_eq!(request.provider.as_deref(), Some("qwen"));
        assert_eq!(request.model.as_deref(), Some("local"));
        assert_eq!(request.reasoning_effort.as_deref(), Some(""));
        assert_eq!(request.fast_mode, Some(false));
        // Plugin defaults override an old saved effort, while explicit supported
        // choices remain available without changing other plugins or the host.
        let mut edited = serde_json::to_value(&*shared.read().await).unwrap();
        edited["plugin_generation"] = serde_json::json!({"code-flow":{"reasoning_effort":"low","visual_reasoning_effort":"low"}});
        edited["model_capabilities"] =
            serde_json::json!({"qwen":{"local":{"reasoning_efforts":["low","medium","high"]}}});
        *shared.write().await = serde_json::from_value(edited).unwrap();
        let mut light = serde_json::from_value(serde_json::json!({"prompt":"continue"})).unwrap();
        prepare_turn(&mut state.clone(), task, &mut light)
            .await
            .unwrap();
        assert_eq!(light.reasoning_effort.as_deref(), Some("low"));
        let mut explicit = serde_json::from_value(
            serde_json::json!({"prompt":"continue","reasoning_effort":"medium"}),
        )
        .unwrap();
        prepare_turn(&mut state.clone(), task, &mut explicit)
            .await
            .unwrap();
        assert_eq!(explicit.reasoning_effort.as_deref(), Some("medium"));

        assert!(!scoped.provider_routes.contains_key("gpt"));
        for role in [&scoped.visual_tool_settings, &scoped.context_agent_settings] {
            let resolved = role.resolve(&scoped, "local").unwrap();
            assert_eq!(resolved.provider_name, "qwen");
            assert_eq!(resolved.default_model, "local");
        }
        assert!(scoped.observer_inherits_model);
        let mut forbidden = serde_json::from_value(
            serde_json::json!({"prompt":"continue","provider":"gpt","model":"main"}),
        )
        .unwrap();
        assert!(
            prepare_turn(&mut state.clone(), task, &mut forbidden)
                .await
                .is_err()
        );
        // The same rejection applies through the general task API, before any model request.
        use axum::response::IntoResponse;
        let response = crate::agent_service::continue_task(
            axum::extract::State(state.clone()),
            axum::extract::Path(task.into()),
            axum::Json(forbidden),
        )
        .await
        .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            crate::ai_proxy::catalog_view(&*shared.read().await)["models"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
    }
    #[tokio::test]
    async fn injected_catalog_reloads_profile_and_disposes_with_its_plugin() {
        let root = std::env::temp_dir().join(crate::visual_artifacts::new_id());
        let path = root.join("profiles/web/cordis.patch.yml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let write = |id: &str| {
            std::fs::write(&path, serde_json::json!([
            {"id":"llm-pi-ai","config":{"providers":{"test":{"baseURL":"http://private/v1","models":[{"id":id}]}}}}
        ]).to_string()).unwrap()
        };
        write("first");
        let runtime = PluginRuntime::new();
        runtime
            .mount(
                ROOT,
                crate::builtin_plugins::ModelSettingsPlugin(Arc::new(
                    crate::builtin_plugins::ModelSettings {
                        config: Arc::new(RwLock::new(crate::ai_proxy::load_config(&path).unwrap())),
                        path: path.clone(),
                    },
                )),
            )
            .unwrap();
        let scope = runtime.mount(ROOT, ModelCatalogPlugin).unwrap();
        let catalog = runtime
            .service::<ModelCatalog>(ROOT, "model-catalog")
            .unwrap();
        assert_eq!(catalog.list().await.unwrap()["models"][0]["id"], "first");
        write("second");
        assert_eq!(catalog.list().await.unwrap()["models"][0]["id"], "second");
        runtime.unmount(scope).unwrap();
        assert!(
            runtime
                .service::<ModelCatalog>(ROOT, "model-catalog")
                .is_err()
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
