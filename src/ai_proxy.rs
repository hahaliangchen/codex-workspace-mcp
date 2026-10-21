use anyhow::Context as _;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock},
};
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::{builtin_plugins, plugin_runtime};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderConfig {
    url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    api_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key_ref: Option<String>,
    #[serde(default)]
    model_map: HashMap<String, String>,
    /// "openai" (default) or "anthropic".  Anthropic-type providers receive
    /// raw pass-through — no request/response format conversion.
    #[serde(default = "default_api_type")]
    api_type: String,
    /// Upstream model id -> declared `maxTokens` in the DSH model entry.
    #[serde(skip)]
    max_output_tokens: HashMap<String, usize>,
    #[serde(skip)]
    model_metadata: HashMap<String, Value>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModelCapabilities {
    #[serde(default)]
    tool_calling: Option<bool>,
    #[serde(default)]
    reasoning_efforts: Vec<String>,
    #[serde(default)]
    default_effort: Option<String>,
    #[serde(default)]
    fast_mode: bool,
    #[serde(default)]
    image_input: crate::visual_artifacts::ImageCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_probe: Option<Value>,
}

fn built_in_capabilities(model: &str) -> ModelCapabilities {
    if model == "gpt-6-luna" {
        ModelCapabilities {
            tool_calling: None,
            reasoning_efforts: ["none", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            default_effort: Some("max".into()),
            fast_mode: true,
            image_input: Default::default(),
            image_probe: None,
        }
    } else {
        ModelCapabilities::default()
    }
}

fn default_api_type() -> String {
    "openai-completions".to_owned()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AiProxyConfig {
    #[serde(default)]
    default_provider: Option<String>,
    #[serde(default)]
    orchestrator_provider: Option<String>,
    #[serde(default)]
    orchestrator_model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    fast_mode: bool,
    #[serde(default)]
    model_capabilities: HashMap<String, HashMap<String, ModelCapabilities>>,
    #[serde(default = "observer_enabled_by_default")]
    observer_enabled: bool,
    #[serde(default)]
    observer_provider: Option<String>,
    #[serde(default)]
    observer_model: Option<String>,
    #[serde(default)]
    visual_tool: crate::model_settings::ModelSettings,
    #[serde(default)]
    context_agent: crate::model_settings::ModelSettings,
    /// Task delegates; empty fields inherit the conversation's model.
    #[serde(default)]
    delegate_agent: crate::model_settings::ModelSettings,
    #[serde(default)]
    plugin_models: HashMap<String, Vec<crate::model_catalog::ModelRef>>,
    #[serde(default)]
    plugin_generation: HashMap<String, crate::model_settings::PluginGeneration>,
    #[serde(default)]
    providers: HashMap<String, ProviderConfig>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

fn observer_enabled_by_default() -> bool {
    true
}

pub(crate) fn apply_agent_config(
    state: &mut crate::agent_service::AgentServiceState,
    config: &AiProxyConfig,
) {
    state.visual_tool_settings = config.visual_tool.clone();
    state.context_agent_settings = config.context_agent.clone();
    state.delegate_agent_settings = config.delegate_agent.clone();
    let main_name = config
        .orchestrator_provider
        .as_deref()
        .or(config.default_provider.as_deref())
        .unwrap_or("");
    let main = config
        .providers
        .get(main_name)
        .filter(|provider| provider.api_type == "openai-completions");
    state.provider_name = main.map(|_| main_name.to_owned()).unwrap_or_default();
    state.provider_routes = config
        .providers
        .iter()
        .filter(|(_, provider)| provider.api_type == "openai-completions")
        .map(|(name, provider)| {
            (
                name.clone(),
                crate::agent_service::AgentProviderRoute {
                    prefix_system_messages: provider
                        .extra
                        .get("prefixSystemMessages")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    url: provider.url.clone(),
                    api_key: provider.api_key.clone(),
                    models: provider.model_map.clone(),
                    max_output_tokens: provider.max_output_tokens.clone(),
                    context_request_options: provider
                        .extra
                        .get("contextRequestOptions")
                        .and_then(Value::as_object)
                        .map(|models| {
                            models
                                .iter()
                                .map(|(name, options)| {
                                    (
                                        provider
                                            .model_map
                                            .get(name)
                                            .cloned()
                                            .unwrap_or_else(|| name.clone()),
                                        options.clone(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            )
        })
        .collect();
    state.provider_url = main.map(|p| p.url.clone()).unwrap_or_default();
    state.api_key = main.map(|p| p.api_key.clone()).unwrap_or_default();
    state.model_map = main.map(|p| p.model_map.clone()).unwrap_or_default();
    state.reasoning_effort = config.reasoning_effort.clone();
    state.fast_mode = config.fast_mode;
    state.default_model = config
        .orchestrator_model
        .clone()
        .or_else(|| main.and_then(|p| p.model_map.values().next().cloned()))
        .unwrap_or_default();
    let observer_name = config.observer_provider.as_deref().unwrap_or(main_name);
    let observer = config
        .providers
        .get(observer_name)
        .filter(|provider| provider.api_type == "openai-completions");
    state.observer_provider = observer
        .map(|_| observer_name.to_owned())
        .unwrap_or_default();
    state.observer_provider_url = observer.map(|p| p.url.clone()).unwrap_or_default();
    state.observer_api_key = observer.map(|p| p.api_key.clone()).unwrap_or_default();
    state.observer_model = config.observer_model.clone().unwrap_or_default();
    state.observer_inherits_model =
        config.observer_provider.is_none() && config.observer_model.is_none();
    state.observer_enabled = config.observer_enabled
        && observer.is_some()
        && (state.observer_inherits_model || !state.observer_model.is_empty());
    state.visual.capabilities = config
        .model_capabilities
        .iter()
        .map(|(provider, models)| {
            (
                provider.clone(),
                models
                    .iter()
                    .flat_map(|(model, cap)| {
                        let mapped = config
                            .providers
                            .get(provider)
                            .and_then(|p| p.model_map.get(model))
                            .cloned()
                            .unwrap_or_else(|| model.clone());
                        [(model.clone(), cap.image_input), (mapped, cap.image_input)]
                    })
                    .collect(),
            )
        })
        .collect();
}

pub(crate) fn dsh_config_path() -> std::path::PathBuf {
    let home = std::env::var_os("DSH_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|v| std::path::PathBuf::from(v).join(".dsh"))
        })
        .or_else(|| std::env::var_os("HOME").map(|v| std::path::PathBuf::from(v).join(".dsh")))
        .unwrap_or_else(|| std::path::PathBuf::from(".dsh"));
    let profile = std::env::var("DSH_PROFILE").unwrap_or_else(|_| "web".into());
    home.join("profiles").join(profile).join("cordis.patch.yml")
}

fn credentials_path(config_path: &Path) -> std::path::PathBuf {
    config_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("DSH profile path has a home directory")
        .join(".credentials.yaml")
}

fn observer_settings_path(config_path: &Path) -> std::path::PathBuf {
    config_path.with_file_name("codex-workspace-observer.yml")
}

fn generation_settings_path(config_path: &Path) -> std::path::PathBuf {
    config_path.with_file_name("agent-generation.yml")
}

fn read_yaml(path: &Path, fallback: Value) -> anyhow::Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(serde_yaml::from_str(&raw)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(fallback),
        Err(error) => Err(error.into()),
    }
}

fn write_yaml_atomic(path: &Path, value: &Value) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("configuration path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let suffix = format!(
        "{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        suffix
    ));
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(serde_yaml::to_string(value)?.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn plugin_config<'a>(patch: &'a Value, id: &str) -> Option<&'a Value> {
    patch
        .as_array()?
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))?
        .get("config")
}

fn set_plugin_config(patch: &mut Value, id: &str, config: Value) -> anyhow::Result<()> {
    let entries = patch
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("DSH profile patch must be a YAML sequence"))?;
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
    {
        entry
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("DSH plugin entry is not a mapping"))?
            .insert("config".into(), config);
    } else {
        entries.push(json!({"id":id,"config":config}));
    }
    Ok(())
}

fn dotenv_value(path: &Path, reference: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        if line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != reference {
            continue;
        }
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

fn credential_value(credentials: &Value, config_path: &Path, reference: &str) -> String {
    std::env::var(reference)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            credentials
                .pointer(&format!("/refs/{reference}"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|dir| dotenv_value(&dir.join(".env"), reference))
        })
        .or_else(|| {
            dotenv_value(
                &credentials_path(config_path).with_file_name(".env"),
                reference,
            )
        })
        .unwrap_or_default()
}

pub(crate) fn load_config(config_path: &Path) -> anyhow::Result<AiProxyConfig> {
    let patch = read_yaml(config_path, json!([]))?;
    anyhow::ensure!(
        patch.is_array(),
        "DSH profile patch must be a YAML sequence"
    );
    let credentials = read_yaml(
        &credentials_path(config_path),
        json!({"version":1,"refs":{},"records":{}}),
    )?;
    let mut config = AiProxyConfig::default();
    if let Some(providers) = plugin_config(&patch, "llm-pi-ai")
        .and_then(|v| v.get("providers"))
        .and_then(Value::as_object)
    {
        for (id, entry) in providers {
            let Some(url) = entry.get("baseURL").and_then(Value::as_str) else {
                continue;
            };
            let Some(models) = entry.get("models").and_then(Value::as_array) else {
                continue;
            };
            let model_map = models
                .iter()
                .filter_map(|v| {
                    v.get("id")
                        .and_then(Value::as_str)
                        .map(|id| (id.to_owned(), id.to_owned()))
                })
                .collect();
            let max_output_tokens = models
                .iter()
                .filter_map(|v| {
                    Some((
                        v.get("id")?.as_str()?.to_owned(),
                        usize::try_from(v.get("maxTokens")?.as_u64()?).ok()?,
                    ))
                })
                .filter(|(_, n)| *n > 0)
                .collect();
            let reference = entry
                .get("apiKeyEnv")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let api_key = reference
                .as_deref()
                .map(|r| credential_value(&credentials, config_path, r))
                .unwrap_or_default();
            config.providers.insert(
                id.clone(),
                ProviderConfig {
                    url: url.to_owned(),
                    api_key,
                    api_key_ref: reference,
                    model_map,
                    api_type: entry
                        .get("api")
                        .and_then(Value::as_str)
                        .unwrap_or("openai-completions")
                        .to_owned(),
                    max_output_tokens,
                    model_metadata: models
                        .iter()
                        .filter_map(|m| Some((m["id"].as_str()?.to_owned(), m.clone())))
                        .collect(),
                    extra: [
                        "displayName",
                        "prefixSystemMessages",
                        "contextRequestOptions",
                    ]
                    .into_iter()
                    .filter_map(|key| Some((key.to_owned(), entry.get(key)?.clone())))
                    .collect(),
                },
            );
        }
    }
    if let Some(main) = plugin_config(&patch, "agent-default-model") {
        config.orchestrator_provider = main
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_owned);
        config.orchestrator_model = main.get("model").and_then(Value::as_str).map(str::to_owned);
        config.default_provider = config.orchestrator_provider.clone();
    }
    let observer = read_yaml(&observer_settings_path(config_path), json!({}))?;
    config.observer_enabled = observer
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    config.observer_provider = observer
        .get("provider")
        .and_then(Value::as_str)
        .map(str::to_owned);
    config.observer_model = observer
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let generation = read_yaml(&generation_settings_path(config_path), json!({}))?;
    for (id, provider) in &mut config.providers {
        if let Some(prefix) =
            generation["provider_compatibility"][id]["prefix_system_messages"].as_bool()
        {
            provider
                .extra
                .insert("prefixSystemMessages".into(), json!(prefix));
        }
    }
    config.reasoning_effort = generation
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_owned);
    config.fast_mode = generation
        .get("fast_mode")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    config.visual_tool =
        serde_json::from_value(generation.get("visual_tool").cloned().unwrap_or(json!({})))?;
    config.context_agent = serde_json::from_value(
        generation
            .get("context_agent")
            .cloned()
            .unwrap_or(json!({})),
    )?;
    config.delegate_agent = serde_json::from_value(
        generation
            .get("delegate_agent")
            .cloned()
            .unwrap_or(json!({})),
    )?;
    config.visual_tool.normalize()?;
    config.context_agent.normalize()?;
    config.delegate_agent.normalize()?;
    config.plugin_models = serde_json::from_value(
        generation
            .get("plugin_models")
            .cloned()
            .unwrap_or(json!({})),
    )?;
    config.plugin_generation = serde_json::from_value(
        generation
            .get("plugin_generation")
            .cloned()
            .unwrap_or(json!({})),
    )?;
    for policy in config.plugin_generation.values_mut() {
        policy.normalize()?;
    }
    config.model_capabilities = generation
        .get("model_capabilities")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    for (provider_id, provider) in &config.providers {
        let models = config
            .model_capabilities
            .entry(provider_id.clone())
            .or_default();
        for model_id in provider.model_map.keys() {
            models
                .entry(model_id.clone())
                .or_insert_with(|| built_in_capabilities(model_id));
        }
        for (model, capability) in models {
            if capability.image_probe.as_ref().is_some_and(|probe| {
                provider.api_type != "openai-completions"
                    || probe["route_fingerprint"]
                        != crate::visual_probe::route_fingerprint(&provider.url, model)
            }) {
                capability.image_input = crate::visual_artifacts::ImageCapability::Unknown;
            }
            if capability.image_input == crate::visual_artifacts::ImageCapability::Supported
                && (provider.api_type != "openai-completions"
                    || !verified_image_support(capability, &provider.url, model))
            {
                capability.image_input = crate::visual_artifacts::ImageCapability::Unknown;
            }
        }
    }
    Ok(config)
}

fn disk_revision(config_path: &Path) -> anyhow::Result<String> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in [
        config_path.to_path_buf(),
        credentials_path(config_path),
        observer_settings_path(config_path),
        generation_settings_path(config_path),
    ] {
        match std::fs::read(path) {
            Ok(bytes) => bytes.hash(&mut hasher),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0u8.hash(&mut hasher),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(format!("{:016x}", hasher.finish()))
}

fn settings_view(config: &AiProxyConfig, revision: String) -> Value {
    let mut providers: Vec<_> = config
        .providers
        .iter()
        .map(|(id, provider)| {
            json!({
                "id": id, "url": provider.url, "api_type": provider.api_type,
                "prefix_system_messages":provider.extra.get("prefixSystemMessages").and_then(Value::as_bool).unwrap_or(false),
                "display_name": provider.extra.get("displayName").and_then(Value::as_str),
                "api_key_env": provider.api_key_ref,
                "models": provider.model_map.keys().collect::<Vec<_>>(), "has_api_key": !provider.api_key.is_empty(),
                "model_capabilities": config.model_capabilities.get(id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    providers.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    json!({
        "revision": revision,
        "providers": providers,
        "orchestrator_provider": config.orchestrator_provider,
        "orchestrator_model": config.orchestrator_model,
        "reasoning_effort": config.reasoning_effort,
        "fast_mode": config.fast_mode,
        "observer_enabled": config.observer_enabled,
        "observer_provider": config.observer_provider,
        "observer_model": config.observer_model,
                "visual_tool":config.visual_tool,"context_agent":config.context_agent,"delegate_agent":config.delegate_agent,
        "plugin_models":config.plugin_models,"plugin_generation":config.plugin_generation,
    })
}

/// Explicit projection: never expose URLs, credentials or raw probe responses.
pub(crate) fn catalog_view(config: &AiProxyConfig) -> Value {
    let mut models = Vec::new();
    for (provider, route) in &config.providers {
        for id in route.model_map.keys() {
            let cap = config
                .model_capabilities
                .get(provider)
                .and_then(|m| m.get(id))
                .cloned()
                .unwrap_or_else(|| built_in_capabilities(id));
            let metadata = route.model_metadata.get(id);
            let probe = cap.image_probe.as_ref().filter(|p| {
                route.api_type == "openai-completions"
                    && p["route_fingerprint"]
                        == crate::visual_probe::route_fingerprint(&route.url, id)
            });
            models.push(json!({
                "provider":provider, "id":id,
                "name":metadata.and_then(|m|m["name"].as_str()).unwrap_or(id),
                "provider_name":route.extra.get("displayName").and_then(Value::as_str).unwrap_or(provider),
                "enabled":true, "available_for_agent":route.api_type == "openai-completions",
                "api_type":route.api_type,
                "context_window":metadata.and_then(|m|m["contextWindow"].as_u64()),
                "max_output_tokens":route.max_output_tokens.get(id),
                "capabilities":{
                    "image_input":cap.image_input, "tool_calling":cap.tool_calling,
                    "reasoning_efforts":cap.reasoning_efforts, "default_effort":cap.default_effort,
                    "fast_mode":cap.fast_mode,
                },
                "verification":{
                    "image_input":probe.and_then(|p|p["status"].as_str()).unwrap_or("unverified"),
                    "checked_at":probe.and_then(|p|p["checked_at"].as_u64()),
                },
            }));
        }
    }
    models.sort_by(|a, b| {
        (a["provider"].as_str(), a["id"].as_str()).cmp(&(b["provider"].as_str(), b["id"].as_str()))
    });
    json!({"version":1,"models":models,"defaults":{
        "agent":{"provider":config.orchestrator_provider,"model":config.orchestrator_model},
        "visual_tool":config.visual_tool,"context_agent":config.context_agent,"delegate_agent":config.delegate_agent,"plugin_generation":config.plugin_generation,
        "observer":{"provider":config.observer_provider,"model":config.observer_model},
    }})
}

pub(crate) fn restricted_config(
    config: &AiProxyConfig,
    plugin: &str,
) -> anyhow::Result<AiProxyConfig> {
    let mut config = config.clone();
    if let Some(policy) = config.plugin_generation.get(plugin) {
        if let Some(effort) = &policy.reasoning_effort {
            config.reasoning_effort = Some(effort.clone());
        }
        if let Some(effort) = &policy.visual_reasoning_effort {
            config.visual_tool.reasoning_effort = Some(effort.clone());
        }
    }
    let config = &config;
    let Some(allowed) = config.plugin_models.get(plugin) else {
        return Ok(config.clone());
    };
    let permits = |provider: &str, model: &str| {
        allowed
            .iter()
            .any(|r| r.provider == provider && r.model == model)
    };
    let mut scoped = config.clone();
    scoped.providers.retain(|id, p| {
        p.model_map.retain(|model, _| permits(id, model));
        !p.model_map.is_empty() && p.api_type == "openai-completions"
    });
    let selected = config
        .orchestrator_provider
        .as_deref()
        .zip(config.orchestrator_model.as_deref())
        .filter(|(p, m)| {
            scoped
                .providers
                .get(*p)
                .is_some_and(|r| r.model_map.contains_key(*m))
        })
        .map(|(p, m)| crate::model_catalog::ModelRef {
            provider: p.into(),
            model: m.into(),
        })
        .or_else(|| {
            allowed
                .iter()
                .find(|r| {
                    scoped
                        .providers
                        .get(&r.provider)
                        .is_some_and(|p| p.model_map.contains_key(&r.model))
                })
                .cloned()
        })
        .context("插件没有可用的授权模型，请在宿主设置中配置")?;
    scoped.orchestrator_provider = Some(selected.provider.clone());
    scoped.orchestrator_model = Some(selected.model.clone());
    scoped.default_provider = Some(selected.provider.clone());
    for role in [&mut scoped.visual_tool, &mut scoped.context_agent, &mut scoped.delegate_agent] {
        if role.provider.is_some() || role.model.is_some() {
            let p = role
                .provider
                .as_ref()
                .or(config.orchestrator_provider.as_ref());
            let m = role.model.as_ref().or(config.orchestrator_model.as_ref());
            if p.zip(m).is_none_or(|(p, m)| {
                !scoped
                    .providers
                    .get(p)
                    .is_some_and(|r| r.model_map.contains_key(m))
            }) {
                *role = Default::default();
            } else {
                role.provider = p.cloned();
                role.model = m.cloned();
            }
        }
    }
    if scoped
        .observer_provider
        .as_deref()
        .zip(scoped.observer_model.as_deref())
        .is_some_and(|(p, m)| {
            !scoped
                .providers
                .get(p)
                .is_some_and(|r| r.model_map.contains_key(m))
        })
    {
        scoped.observer_provider = None;
        scoped.observer_model = None;
    }
    let cap = scoped
        .model_capabilities
        .get(&selected.provider)
        .and_then(|m| m.get(&selected.model));
    scoped.reasoning_effort = config
        .reasoning_effort
        .clone()
        .filter(|e| cap.is_some_and(|c| c.reasoning_efforts.contains(e)));
    if let Some(effort) = config
        .plugin_generation
        .get(plugin)
        .and_then(|p| p.visual_reasoning_effort.clone())
    {
        scoped.visual_tool.reasoning_effort = Some(effort);
    }
    scoped.fast_mode = config.fast_mode && cap.is_some_and(|c| c.fast_mode);
    Ok(scoped)
}

pub(crate) fn plugin_catalog_view(config: &AiProxyConfig, plugin: &str) -> Value {
    match restricted_config(config, plugin) {
        Ok(scoped) => catalog_view(&scoped),
        Err(_) => json!({"version":1,"models":[],"defaults":{}}),
    }
}

async fn get_model_catalog(State(state): State<AiProxyState>) -> Response {
    let catalog = crate::model_catalog::ModelCatalog {
        config: state.config,
        path: Some(state.config_path),
    };
    match catalog.list().await {
        Ok(value) => Json(value).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not load model catalog"})),
        )
            .into_response(),
    }
}

fn trusted_settings_request(headers: &HeaderMap) -> bool {
    if headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("cross-site") {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    reqwest::Url::parse(origin).ok().is_some_and(|url| {
        url.scheme() == "http"
            && url
                .host_str()
                .is_some_and(|name| matches!(name, "127.0.0.1" | "localhost"))
            && url.host_str().is_some_and(|name| {
                host.eq_ignore_ascii_case(&format!(
                    "{}:{}",
                    name,
                    url.port_or_known_default().unwrap_or(80)
                ))
            })
    })
}

async fn get_agent_settings(State(state): State<AiProxyState>, headers: HeaderMap) -> Response {
    if !trusted_settings_request(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"settings are only available to the local page"})),
        )
            .into_response();
    }
    let _guard = state.settings_write.lock().await;
    let path = state.config_path.clone();
    let loaded = tokio::task::spawn_blocking(move || -> anyhow::Result<(AiProxyConfig, String)> {
        Ok((load_config(&path)?, disk_revision(&path)?))
    })
    .await;
    match loaded {
        Ok(Ok((config, revision))) => {
            let view = settings_view(&config, revision);
            *state.config.write().await = config;
            Json(view).into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not load agent settings"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ProviderEdit {
    #[serde(default)]
    prefix_system_messages: Option<bool>,
    id: String,
    url: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default = "default_api_type")]
    api_type: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    model_capabilities: Option<HashMap<String, ModelCapabilities>>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    clear_api_key: bool,
}

#[derive(Deserialize)]
struct SettingsEdit {
    revision: String,
    providers: Vec<ProviderEdit>,
    orchestrator_provider: Option<String>,
    orchestrator_model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    fast_mode: Option<bool>,
    #[serde(default)]
    observer_enabled: Option<bool>,
    #[serde(default)]
    observer_provider: Option<String>,
    #[serde(default)]
    observer_model: Option<String>,
    #[serde(default)]
    visual_tool: Option<crate::model_settings::ModelSettings>,
    #[serde(default)]
    context_agent: Option<crate::model_settings::ModelSettings>,
    #[serde(default)]
    delegate_agent: Option<crate::model_settings::ModelSettings>,
    #[serde(default)]
    plugin_models: Option<HashMap<String, Vec<crate::model_catalog::ModelRef>>>,
    #[serde(default)]
    plugin_generation: Option<HashMap<String, crate::model_settings::PluginGeneration>>,
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn verified_image_support(capability: &ModelCapabilities, url: &str, model: &str) -> bool {
    capability.image_probe.as_ref().is_some_and(|probe| {
        probe["status"] == "verified"
            && probe["capability"] == "supported"
            && probe["request"]["contains_image_content"] == true
            && probe["route_fingerprint"] == crate::visual_probe::route_fingerprint(url, model)
    })
}

fn image_probe_route(
    config: &AiProxyConfig,
    provider: &str,
    model: &str,
) -> anyhow::Result<crate::visual_artifacts::VisualRoute> {
    let route = config
        .providers
        .get(provider)
        .ok_or_else(|| anyhow::anyhow!("provider is not configured; save settings first"))?;
    anyhow::ensure!(
        route.api_type == "openai-completions",
        "image probe currently requires an OpenAI Chat Completions route"
    );
    let model = route
        .model_map
        .get(model)
        .ok_or_else(|| anyhow::anyhow!("model is not configured; save settings first"))?;
    Ok(crate::visual_artifacts::VisualRoute {
        provider: provider.into(),
        model: model.clone(),
        url: route.url.clone(),
        api_key: route.api_key.clone(),
    })
}

fn persist_image_probe(
    path: &Path,
    revision: &str,
    provider: &str,
    model: &str,
    report: &Value,
) -> anyhow::Result<(AiProxyConfig, String)> {
    anyhow::ensure!(
        disk_revision(path)? == revision,
        "settings changed during image verification; run it again on the saved route"
    );
    let mut config = load_config(path)?;
    let route = image_probe_route(&config, provider, model)?;
    anyhow::ensure!(
        report["route_fingerprint"]
            == crate::visual_probe::route_fingerprint(&route.url, &route.model),
        "image probe route changed"
    );
    let capability = config
        .model_capabilities
        .entry(provider.into())
        .or_default()
        .entry(model.into())
        .or_default();
    capability.image_input = serde_json::from_value(report["capability"].clone())?;
    capability.image_probe = Some(report.clone());
    let generation_path = generation_settings_path(path);
    let mut generation = read_yaml(&generation_path, json!({}))?;
    generation["model_capabilities"] = serde_json::to_value(&config.model_capabilities)?;
    write_yaml_atomic(&generation_path, &generation)?;
    Ok((config, disk_revision(path)?))
}

#[derive(Deserialize)]
struct ImageProbeRequest {
    revision: String,
    provider: String,
    model: String,
}

async fn probe_agent_image_input(
    State(state): State<AiProxyState>,
    headers: HeaderMap,
    Json(input): Json<ImageProbeRequest>,
) -> Response {
    if !trusted_settings_request(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"settings are only available to the local page"})),
        )
            .into_response();
    }
    let result = async {
        let config = load_config(&state.config_path)?;
        anyhow::ensure!(
            disk_revision(&state.config_path)? == input.revision,
            "settings changed; reload before image verification"
        );
        let route = image_probe_route(&config, &input.provider, &input.model)?;
        let report = crate::visual_probe::probe(&state.client, &route).await?;
        let _guard = state.settings_write.lock().await;
        let (config, revision) = persist_image_probe(
            &state.config_path,
            &input.revision,
            &input.provider,
            &input.model,
            &report,
        )?;
        let view = settings_view(&config, revision);
        *state.config.write().await = config;
        Ok::<_, anyhow::Error>(json!({"probe":report,"settings":view}))
    }
    .await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}

pub(crate) async fn probe_default_image_input(path: &Path) -> anyhow::Result<Value> {
    let revision = disk_revision(path)?;
    let config = load_config(path)?;
    let provider = config
        .orchestrator_provider
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("main provider is not configured"))?;
    let model = config
        .orchestrator_model
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("main model is not configured"))?;
    let route = image_probe_route(&config, provider, model)?;
    let report = crate::visual_probe::probe(&Client::new(), &route).await?;
    // Store evidence without touching profile routing, credentials, or fallback settings.
    persist_image_probe(path, &revision, provider, model, &report)?;
    Ok(report)
}

fn save_settings(
    config_path: &Path,
    request: SettingsEdit,
) -> anyhow::Result<Result<(AiProxyConfig, String), String>> {
    if disk_revision(config_path)? != request.revision {
        return Ok(Err(
            "settings changed since this page loaded; reload before saving".into(),
        ));
    }
    let mut config = load_config(config_path)?;
    let mut providers = HashMap::new();
    let mut model_capabilities = HashMap::new();
    let mut key_actions: HashMap<String, Option<String>> = HashMap::new();
    for draft in request.providers {
        let id = draft.id.trim().to_owned();
        anyhow::ensure!(
            !id.is_empty()
                && id.len() <= 64
                && id.as_bytes()[0].is_ascii_lowercase()
                && id.as_bytes()[id.len() - 1].is_ascii_alphanumeric()
                && !id.contains("--")
                && id
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
            "provider ID must be lowercase words separated by single hyphens (up to 64 characters)"
        );
        anyhow::ensure!(!providers.contains_key(&id), "duplicate provider ID: {id}");
        let url = reqwest::Url::parse(draft.url.trim())?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "provider {id} requires an HTTP(S) URL"
        );
        anyhow::ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "provider {id} URL must not include credentials, query, or fragment"
        );
        anyhow::ensure!(
            matches!(
                draft.api_type.as_str(),
                "openai-completions" | "openai-responses" | "anthropic-messages"
            ),
            "provider {id} has unsupported protocol"
        );
        anyhow::ensure!(
            !draft.models.is_empty() && draft.models.iter().all(|model| !model.trim().is_empty()),
            "provider {id} needs at least one model"
        );
        anyhow::ensure!(
            draft
                .models
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == draft.models.len(),
            "provider {id} has duplicate model IDs"
        );
        let configured = draft.model_capabilities.unwrap_or_else(|| {
            config
                .model_capabilities
                .get(&id)
                .cloned()
                .unwrap_or_default()
        });
        let mut capabilities = HashMap::new();
        for model in &draft.models {
            let mut capability = configured
                .get(model)
                .cloned()
                .unwrap_or_else(|| built_in_capabilities(model));
            // A settings payload cannot manufacture successful probe evidence.
            capability.image_probe = config
                .model_capabilities
                .get(&id)
                .and_then(|models| models.get(model))
                .and_then(|previous| previous.image_probe.clone());
            if capability.image_probe.as_ref().is_some_and(|probe| {
                draft.api_type != "openai-completions"
                    || probe["route_fingerprint"]
                        != crate::visual_probe::route_fingerprint(url.as_str(), model)
            }) {
                capability.image_input = crate::visual_artifacts::ImageCapability::Unknown;
            }
            anyhow::ensure!(
                capability.image_input != crate::visual_artifacts::ImageCapability::Supported
                    || (draft.api_type == "openai-completions"
                        && verified_image_support(&capability, url.as_str(), model)),
                "provider {id} model {model}: verify image input on the saved route before marking it supported"
            );
            anyhow::ensure!(
                capability.reasoning_efforts.iter().all(|effort| matches!(
                    effort.as_str(),
                    "none" | "low" | "medium" | "high" | "xhigh" | "max"
                )),
                "provider {id} model {model} has an unsupported reasoning effort"
            );
            anyhow::ensure!(
                capability
                    .reasoning_efforts
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == capability.reasoning_efforts.len(),
                "provider {id} model {model} has duplicate reasoning efforts"
            );
            anyhow::ensure!(
                capability
                    .default_effort
                    .as_ref()
                    .is_none_or(|effort| capability.reasoning_efforts.contains(effort)),
                "provider {id} model {model} default effort is not in its supported levels"
            );
            capabilities.insert(model.clone(), capability);
        }
        model_capabilities.insert(id.clone(), capabilities);
        let previous = config.providers.get(&id);
        let requested_key = draft.api_key.filter(|v| !v.is_empty());
        if draft.clear_api_key {
            key_actions.insert(id.clone(), None);
        } else if let Some(value) = &requested_key {
            key_actions.insert(id.clone(), Some(value.clone()));
        }
        let key = if draft.clear_api_key {
            String::new()
        } else if let Some(value) = requested_key {
            anyhow::ensure!(value.len() <= 8192, "provider {id} has an invalid API key");
            value
        } else {
            previous.map(|p| p.api_key.clone()).unwrap_or_default()
        };
        let mut provider = previous.cloned().unwrap_or(ProviderConfig {
            url: String::new(),
            api_key: String::new(),
            api_key_ref: None,
            model_map: HashMap::new(),
            api_type: default_api_type(),
            max_output_tokens: HashMap::new(),
            model_metadata: HashMap::new(),
            extra: HashMap::new(),
        });
        provider.url = url.as_str().trim_end_matches('/').to_owned();
        provider.api_key = key;
        provider.model_map = draft
            .models
            .into_iter()
            .map(|model| (model.clone(), model))
            .collect();
        provider.api_type = draft.api_type;
        if let Some(prefix) = draft.prefix_system_messages {
            provider
                .extra
                .insert("prefixSystemMessages".into(), json!(prefix));
        }
        match nonempty(draft.display_name) {
            Some(name) => {
                provider.extra.insert("displayName".into(), json!(name));
            }
            None => {
                provider.extra.remove("displayName");
            }
        }
        providers.insert(id, provider);
    }
    config.providers = providers;
    config.model_capabilities = model_capabilities;
    config.orchestrator_provider = nonempty(request.orchestrator_provider);
    config.orchestrator_model = nonempty(request.orchestrator_model);
    if let Some(enabled) = request.observer_enabled {
        config.observer_enabled = enabled;
    }
    config.observer_provider = nonempty(request.observer_provider);
    config.observer_model = nonempty(request.observer_model);
    for (edit, target) in [
        (request.visual_tool, &mut config.visual_tool),
        (request.context_agent, &mut config.context_agent),
        (request.delegate_agent, &mut config.delegate_agent),
    ] {
        if let Some(mut edit) = edit {
            edit.normalize()?;
            *target = edit;
        }
        if target.provider.is_some() || target.model.is_some() {
            let provider = target
                .provider
                .as_ref()
                .or(config.orchestrator_provider.as_ref())
                .context("choose a provider for the dedicated task")?;
            let model = target
                .model
                .as_ref()
                .or(config.orchestrator_model.as_ref())
                .context("choose a model for the dedicated task")?;
            let route = config
                .providers
                .get(provider)
                .context("configured tool/compression provider is unavailable")?;
            anyhow::ensure!(
                route.api_type == "openai-completions",
                "configured tool/compression provider is incompatible"
            );
            anyhow::ensure!(
                route.model_map.contains_key(model),
                "dedicated task model {model} is not in provider {provider}'s catalog"
            );
            if let Some(cap) = config
                .model_capabilities
                .get(provider)
                .and_then(|m| m.get(model))
            {
                anyhow::ensure!(
                    cap.reasoning_efforts.is_empty()
                        || target
                            .reasoning_effort
                            .as_ref()
                            .is_none_or(|e| cap.reasoning_efforts.contains(e)),
                    "dedicated task model does not support this reasoning effort"
                );
            }
        }
    }
    config.default_provider = config.orchestrator_provider.clone();
    if let Some(policies) = request.plugin_models {
        config.plugin_models = policies;
    }
    if let Some(mut policies) = request.plugin_generation {
        for (id, policy) in &mut policies {
            anyhow::ensure!(
                !id.trim().is_empty(),
                "plugin generation policy requires a plugin ID"
            );
            policy.normalize()?;
        }
        config.plugin_generation = policies;
    }
    for (plugin, models) in &config.plugin_models {
        anyhow::ensure!(
            !plugin.is_empty(),
            "plugin model policy requires a plugin ID"
        );
        let mut unique = std::collections::HashSet::new();
        for model in models {
            anyhow::ensure!(
                unique.insert(model.clone()),
                "duplicate plugin model reference"
            );
            anyhow::ensure!(
                config
                    .providers
                    .get(&model.provider)
                    .is_some_and(|p| p.api_type == "openai-completions"
                        && p.model_map.contains_key(&model.model)),
                "插件 {plugin} 引用的模型未在可用模型目录中登记"
            );
        }
    }
    if let Some(effort) = request.reasoning_effort {
        let effort = effort.trim();
        anyhow::ensure!(
            effort.is_empty()
                || matches!(effort, "none" | "low" | "medium" | "high" | "xhigh" | "max"),
            "unsupported reasoning effort"
        );
        config.reasoning_effort = (!effort.is_empty()).then(|| effort.to_owned());
    }
    if let Some(fast_mode) = request.fast_mode {
        config.fast_mode = fast_mode;
    }
    let selected_capability = config
        .orchestrator_provider
        .as_ref()
        .and_then(|provider| config.model_capabilities.get(provider))
        .and_then(|models| {
            config
                .orchestrator_model
                .as_ref()
                .and_then(|model| models.get(model))
        });
    if let Some(capability) = selected_capability {
        anyhow::ensure!(
            config
                .reasoning_effort
                .as_ref()
                .is_none_or(|effort| capability.reasoning_efforts.contains(effort)),
            "selected model does not support this reasoning effort"
        );
        anyhow::ensure!(
            !config.fast_mode || capability.fast_mode,
            "selected model does not support fast mode"
        );
    } else {
        config.reasoning_effort = None;
        config.fast_mode = false;
    }
    for name in [&config.orchestrator_provider, &config.observer_provider]
        .into_iter()
        .flatten()
    {
        anyhow::ensure!(
            config.providers.contains_key(name),
            "selected provider {name} does not exist"
        );
    }
    anyhow::ensure!(
        config.orchestrator_provider.is_some() == config.orchestrator_model.is_some(),
        "choose both a main-agent provider and model"
    );
    anyhow::ensure!(
        config.observer_provider.is_some() == config.observer_model.is_some(),
        "choose both an observer provider and model"
    );
    for (provider, model) in [
        (&config.orchestrator_provider, &config.orchestrator_model),
        (&config.observer_provider, &config.observer_model),
    ] {
        if let (Some(provider), Some(model)) = (provider, model) {
            anyhow::ensure!(
                config.providers[provider].model_map.contains_key(model),
                "model {model} is not in provider {provider}'s catalog"
            );
        }
    }
    for name in [&config.orchestrator_provider, &config.observer_provider]
        .into_iter()
        .flatten()
    {
        anyhow::ensure!(
            config.providers[name].api_type == "openai-completions",
            "agent route {name} requires OpenAI Chat Completions protocol"
        );
    }
    let mut patch = read_yaml(config_path, json!([]))?;
    let original_patch = patch.clone();
    let mut credentials = read_yaml(
        &credentials_path(config_path),
        json!({"version":1,"refs":{},"records":{}}),
    )?;
    anyhow::ensure!(
        patch.is_array(),
        "DSH profile patch must be a YAML sequence"
    );
    anyhow::ensure!(
        credentials.get("version").and_then(Value::as_u64) == Some(1),
        "unsupported DSH credentials version"
    );
    let refs = credentials
        .get_mut("refs")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("DSH credentials refs must be a mapping"))?;
    let old_providers = plugin_config(&patch, "llm-pi-ai")
        .and_then(|v| v.get("providers"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut dsh_providers = old_providers.clone();
    for id in old_providers.keys() {
        if config.providers.contains_key(id)
            || old_providers[id].get("models").is_none()
            || old_providers[id].get("baseURL").is_none()
        {
            continue;
        }
        dsh_providers.remove(id);
    }
    for (id, provider) in &config.providers {
        let mut entry = old_providers.get(id).cloned().unwrap_or_else(|| json!({}));
        let item = entry
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("provider {id} is not a mapping"))?;
        item.insert("baseURL".into(), json!(provider.url));
        item.insert("api".into(), json!(provider.api_type));
        item.remove("prefixSystemMessages");
        if let Some(name) = provider.extra.get("displayName") {
            item.insert("displayName".into(), name.clone());
        } else {
            item.remove("displayName");
        }
        let mut models = Vec::new();
        for model in provider.model_map.values() {
            let previous = old_providers
                .get(id)
                .and_then(|v| v.get("models"))
                .and_then(Value::as_array)
                .and_then(|a| {
                    a.iter()
                        .find(|v| v.get("id").and_then(Value::as_str) == Some(model))
                });
            models.push(previous.cloned().unwrap_or_else(|| json!({"id":model})));
        }
        models.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        models.dedup_by(|a, b| a["id"] == b["id"]);
        item.insert("models".into(), json!(models));
        let reference = provider
            .api_key_ref
            .clone()
            .unwrap_or_else(|| format!("{}_API_KEY", id.to_ascii_uppercase().replace('-', "_")));
        match key_actions.get(id) {
            Some(None) => {
                anyhow::ensure!(
                    std::env::var_os(&reference).is_none(),
                    "{reference} is supplied by the environment and cannot be cleared here"
                );
                item.remove("apiKeyEnv");
                refs.remove(&reference);
            }
            Some(Some(key)) => {
                anyhow::ensure!(
                    std::env::var_os(&reference).is_none(),
                    "{reference} is supplied by the environment and cannot be replaced here"
                );
                item.insert("apiKeyEnv".into(), json!(reference));
                refs.insert(reference, json!(key));
            }
            None => {}
        }
        dsh_providers.insert(id.clone(), entry);
    }
    let mut llm = plugin_config(&patch, "llm-pi-ai")
        .cloned()
        .unwrap_or_else(|| json!({}));
    llm.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("llm-pi-ai config is not a mapping"))?
        .insert("providers".into(), Value::Object(dsh_providers));
    set_plugin_config(&mut patch, "llm-pi-ai", llm)?;
    if let (Some(provider), Some(model)) =
        (&config.orchestrator_provider, &config.orchestrator_model)
    {
        let mut default_model = plugin_config(&patch, "agent-default-model")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let item = default_model
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("agent-default-model config is not a mapping"))?;
        item.insert("provider".into(), json!(provider));
        item.insert("model".into(), json!(model));
        set_plugin_config(&mut patch, "agent-default-model", default_model)?;
    } else if let Some(entries) = patch.as_array_mut() {
        entries
            .retain(|entry| entry.get("id").and_then(Value::as_str) != Some("agent-default-model"));
    }
    let secret_path = credentials_path(config_path);
    let observer_path = observer_settings_path(config_path);
    let original_observer = read_yaml(&observer_path, json!({}))?;
    let observer_settings = json!({
        "enabled": config.observer_enabled,
        "provider": config.observer_provider,
        "model": config.observer_model,
    });
    let generation_path = generation_settings_path(config_path);
    let original_generation = read_yaml(&generation_path, json!({}))?;
    let generation_settings = json!({
        "provider_compatibility":config.providers.iter().map(|(id,p)|(id.clone(),json!({"prefix_system_messages":p.extra.get("prefixSystemMessages").and_then(Value::as_bool).unwrap_or(false)}))).collect::<HashMap<_,_>>(),
        "reasoning_effort": config.reasoning_effort,
        "fast_mode": config.fast_mode,
        "model_capabilities": config.model_capabilities,
                "visual_tool":config.visual_tool,"context_agent":config.context_agent,"delegate_agent":config.delegate_agent,
        "plugin_models":config.plugin_models,"plugin_generation":config.plugin_generation,
    });
    if disk_revision(config_path)? != request.revision {
        return Ok(Err(
            "settings changed while saving; reload before retrying".into()
        ));
    }
    if let Some(parent) = secret_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !key_actions.is_empty() {
        write_yaml_atomic(&secret_path, &credentials)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))?;
    }
    if patch != original_patch {
        write_yaml_atomic(config_path, &patch)?;
    }
    if observer_settings != original_observer {
        write_yaml_atomic(&observer_path, &observer_settings)?;
    }
    if generation_settings != original_generation {
        write_yaml_atomic(&generation_path, &generation_settings)?;
    }
    let revision = disk_revision(config_path)?;
    Ok(Ok((config, revision)))
}

async fn put_agent_settings(
    State(state): State<AiProxyState>,
    headers: HeaderMap,
    Json(request): Json<SettingsEdit>,
) -> Response {
    if !trusted_settings_request(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"settings are only available to the local page"})),
        )
            .into_response();
    }
    let _guard = state.settings_write.lock().await;
    let path = state.config_path.clone();
    let result = tokio::task::spawn_blocking(move || save_settings(&path, request)).await;
    match result {
        Ok(Ok(Ok((config, revision)))) => {
            let view = settings_view(&config, revision);
            *state.config.write().await = config;
            Json(view).into_response()
        }
        Ok(Ok(Err(message))) => {
            (StatusCode::CONFLICT, Json(json!({"error":message}))).into_response()
        }
        Ok(Err(error)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not save agent settings"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct DiscoverModelsRequest {
    id: String,
    url: String,
    #[serde(default)]
    api_key: Option<String>,
}

async fn discover_agent_models(
    State(state): State<AiProxyState>,
    headers: HeaderMap,
    Json(request): Json<DiscoverModelsRequest>,
) -> Response {
    if !trusted_settings_request(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"settings are only available to the local page"})),
        )
            .into_response();
    }
    let url = match reqwest::Url::parse(request.url.trim()) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() => url,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"enter a valid HTTP(S) provider URL"})),
            )
                .into_response();
        }
    };
    let saved_key = {
        let config = state.config.read().await;
        config
            .providers
            .get(&request.id)
            .filter(|provider| {
                provider.url.trim_end_matches('/') == url.as_str().trim_end_matches('/')
            })
            .map(|provider| provider.api_key.clone())
            .unwrap_or_default()
    };
    let key = request
        .api_key
        .filter(|key| !key.is_empty())
        .unwrap_or(saved_key);
    let endpoint = format!("{}/models", url.as_str().trim_end_matches('/'));
    let mut call = state.client.get(endpoint);
    if !key.is_empty() {
        call = call.bearer_auth(key);
    }
    let response = match tokio::time::timeout(std::time::Duration::from_secs(20), call.send()).await
    {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error":"could not reach provider model catalog"})),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({"error":"provider model catalog timed out"})),
            )
                .into_response();
        }
    };
    if !response.status().is_success() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error":format!("provider model catalog returned {}", response.status())})),
        )
            .into_response();
    }
    let value: Value = match response.json().await {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error":"provider returned an invalid model catalog"})),
            )
                .into_response();
        }
    };
    let mut models: Vec<String> = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .or_else(|| item.as_str())
        })
        .map(str::to_owned)
        .take(500)
        .collect();
    models.sort();
    models.dedup();
    Json(json!({"models":models})).into_response()
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AiProxyState {
    config: Arc<RwLock<AiProxyConfig>>,
    config_path: std::path::PathBuf,
    settings_write: Arc<Mutex<()>>,
    client: Client,
    runtime: Arc<plugin_runtime::PluginRuntime>,
}

async fn plugin_tree(State(state): State<AiProxyState>) -> Json<Value> {
    let events = state
        .runtime
        .service::<builtin_plugins::LifecycleLog>(plugin_runtime::ROOT, "plugin-events")
        .map(|log| log.snapshot())
        .unwrap_or_default();
    Json(json!({"plugins":state.runtime.snapshot(),"events":events}))
}

async fn skill_packages(State(state): State<AiProxyState>) -> Json<Value> {
    let registry = state
        .runtime
        .service::<builtin_plugins::ToolCatalog>(plugin_runtime::ROOT, "tools")
        .unwrap();
    Json(json!({"packages":registry.packages.catalog()}))
}
#[derive(Deserialize)]
struct PackageEnable {
    enabled: bool,
}
async fn enable_skill_package(
    State(state): State<AiProxyState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(input): Json<PackageEnable>,
) -> Response {
    let registry = state
        .runtime
        .service::<builtin_plugins::ToolCatalog>(plugin_runtime::ROOT, "tools")
        .unwrap();
    if input.enabled && registry.packages.is_remote(&id) {
        if let Err(error) =
            crate::skill_packages::remote::reconnect(&state.runtime, &registry.packages, &id).await
        {
            return package_response(Err(error));
        }
    }
    let result = crate::skill_packages::set_enabled(
        &state.runtime,
        &registry.packages,
        &id,
        input.enabled,
        true,
    );
    match result {
        Ok(packages) => Json(json!({"packages":packages})).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}
#[derive(Deserialize)]
struct PackageInstall {
    path: std::path::PathBuf,
}
async fn install_skill_package(
    State(state): State<AiProxyState>,
    Json(input): Json<PackageInstall>,
) -> Response {
    let catalog = state
        .runtime
        .service::<builtin_plugins::ToolCatalog>(plugin_runtime::ROOT, "tools")
        .unwrap();
    package_response(crate::skill_packages::install(
        &state.runtime,
        &catalog.packages,
        &input.path,
    ))
}
async fn connect_remote_package(
    State(state): State<AiProxyState>,
    Json(input): Json<crate::skill_packages::remote::Config>,
) -> Response {
    let catalog = state
        .runtime
        .service::<builtin_plugins::ToolCatalog>(plugin_runtime::ROOT, "tools")
        .unwrap();
    package_response(
        crate::skill_packages::remote::install(&state.runtime, &catalog.packages, input, true)
            .await,
    )
}
async fn ppt_page_request(
    State(state): State<AiProxyState>,
    axum::extract::Path(method): axum::extract::Path<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Response {
    let scope = state
        .runtime
        .snapshot()
        .into_iter()
        .find(|p| p.id == "agent-tasks")
        .map(|p| p.scope)
        .unwrap_or(plugin_runtime::ROOT);
    let agent = match state
        .runtime
        .service::<crate::agent_service::AgentServiceState>(scope, "agent-tasks")
    {
        Ok(agent) => agent,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"Agent service unavailable"})),
            )
                .into_response();
        }
    };
    crate::ppt_sessions::request(&state.runtime, (*agent).clone(), &method, headers, args).await
}
async fn reload_skill_package(
    State(state): State<AiProxyState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let catalog = state
        .runtime
        .service::<builtin_plugins::ToolCatalog>(plugin_runtime::ROOT, "tools")
        .unwrap();
    if catalog.packages.is_remote(&id) {
        return package_response(
            crate::skill_packages::remote::reconnect(&state.runtime, &catalog.packages, &id).await,
        );
    }
    package_response(crate::skill_packages::reload(
        &state.runtime,
        &catalog.packages,
        &id,
    ))
}
async fn remove_skill_package(
    State(state): State<AiProxyState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let catalog = state
        .runtime
        .service::<builtin_plugins::ToolCatalog>(plugin_runtime::ROOT, "tools")
        .unwrap();
    package_response(crate::skill_packages::remove(
        &state.runtime,
        &catalog.packages,
        &id,
    ))
}
fn package_response(result: anyhow::Result<Value>) -> Response {
    match result {
        Ok(packages) => Json(json!({"packages":packages})).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}
async fn skill_page_asset(
    State(state): State<AiProxyState>,
    axum::extract::Path((id, page, resource)): axum::extract::Path<(String, String, String)>,
) -> Response {
    let registry = state
        .runtime
        .service::<builtin_plugins::ToolCatalog>(plugin_runtime::ROOT, "tools")
        .unwrap();
    if let Ok(Some((connection, entry))) = registry.packages.remote_page(&id, &page) {
        if resource != "index" {
            return StatusCode::NOT_FOUND.into_response();
        }
        return match connection.asset(&entry, &[]).await {
            Ok((mime, bytes)) => {
                let mut response = (
                    [
                        (axum::http::header::CONTENT_TYPE, mime),
                        (axum::http::header::CACHE_CONTROL, "no-store".to_owned()),
                        (
                            axum::http::header::X_CONTENT_TYPE_OPTIONS,
                            "nosniff".to_owned(),
                        ),
                    ],
                    bytes,
                )
                    .into_response();
                if entry == "page" {
                    response.headers_mut().insert(axum::http::header::CONTENT_SECURITY_POLICY, axum::http::HeaderValue::from_static("default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; connect-src 'none'; form-action 'none'; base-uri 'none'"));
                }
                response
            }
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error":"remote page unavailable; check plugin connection"})),
            )
                .into_response(),
        };
    }
    let result = registry.packages.page(
        &id,
        &page,
        if resource == "index" {
            None
        } else {
            Some(&resource)
        },
    );
    match result {
        Ok(path) => match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let content_type = match path.extension().and_then(|extension| extension.to_str()) {
                    Some("html") => "text/html; charset=utf-8",
                    Some("js" | "mjs") => "text/javascript",
                    Some("css") => "text/css",
                    Some("png") => "image/png",
                    Some("svg") => "image/svg+xml",
                    _ => "application/octet-stream",
                };
                ([(axum::http::header::CONTENT_TYPE, content_type)], bytes).into_response()
            }
            Err(error) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error":error.to_string()})),
            )
                .into_response(),
        },
        Err(error) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

pub async fn run(
    listener: TcpListener,
    config_path: &Path,
    workspace: Arc<crate::tools::Workspace>,
) -> anyhow::Result<()> {
    let config = load_config(config_path)?;

    crate::agent_service::recover_orphaned_tasks(workspace.root()).await?;

    let total_maps: usize = config.providers.values().map(|p| p.model_map.len()).sum();

    let config = Arc::new(RwLock::new(config));
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let log_dir = workspace.root().join("logs");
    crate::proxy_log::init(log_dir)?;

    info!("========== Agent service started ==========");
    info!("config: {} total model mappings", total_maps);

    let runtime = Arc::new(plugin_runtime::PluginRuntime::new());
    runtime.mount(plugin_runtime::ROOT, builtin_plugins::DiagnosticsPlugin)?;
    runtime.mount(
        plugin_runtime::ROOT,
        builtin_plugins::WorkspacePlugin(workspace.clone()),
    )?;
    runtime.mount(
        plugin_runtime::ROOT,
        builtin_plugins::ModelSettingsPlugin(Arc::new(builtin_plugins::ModelSettings {
            path: config_path.to_path_buf(),
            config: config.clone(),
        })),
    )?;
    runtime.mount(
        plugin_runtime::ROOT,
        crate::model_catalog::ModelCatalogPlugin,
    )?;
    let tools_scope = runtime.mount(
        plugin_runtime::ROOT,
        builtin_plugins::ToolCatalogPlugin(client.clone()),
    )?;
    let agent_state = (*runtime
        .service::<crate::agent_service::AgentServiceState>(tools_scope, "agent-tasks")?)
    .clone();
    crate::skill_packages::bind_root(agent_state.tool_catalog.as_ref().unwrap());
    let remote_runtime = runtime.clone();
    let remote_registry = agent_state.tool_catalog.as_ref().unwrap().packages.clone();
    tokio::spawn(async move {
        if let Err(_) =
            crate::skill_packages::remote::restore(&remote_runtime, &remote_registry).await
        {
            tracing::warn!("remote plugin configuration could not be restored");
        }
    });
    let agent_routes = Router::new()
        .route("/agent", get(crate::agent_web::page))
        .route("/agent/", get(crate::agent_web::page))
        .nest_service(
            "/agent/assets",
            tower_http::services::ServeDir::new(crate::agent_web::web_dir().join("assets")),
        )
        .route("/agent/star.png", get(crate::agent_web::star_png))
        .route("/agent/logo.png", get(crate::agent_web::star_png))
        .route("/agent/favicon.png", get(crate::agent_web::favicon_png))
        .route("/agent/favicon.ico", get(crate::agent_web::favicon_png))
        .route("/agent/settings", get(crate::agent_web::settings_page))
        .route("/agent/info", get(crate::agent_web::info))
        .route(
            "/agent/workspace/files",
            get(crate::agent_service::workspace_files),
        )
        .route("/agent/commands", get(crate::agent_service::slash_commands))
        .route(
            "/agent/notes",
            get(crate::project_notes::get)
                .put(crate::project_notes::put)
                .post(crate::project_notes::post),
        )
        .route(
            "/agent/skill-packages/{id}/tools/{name}",
            post(crate::skill_packages::call_tool),
        )
        .route(
            "/agent/skill-packages/{id}/session",
            post(crate::plugin_sessions::request)
                .layer(axum::extract::DefaultBodyLimit::max(30 * 1024 * 1024)),
        )
        .route(
            "/agent/skill-packages/{id}/service",
            post(crate::skill_packages::remote::service),
        )
        .route(
            "/agent/tasks",
            get(crate::agent_service::list_tasks).post(crate::agent_service::create_task),
        )
        .route(
            "/agent/sessions",
            post(crate::agent_service::create_draft_task),
        )
        .route("/agent/workspaces", get(crate::session_browser::projects).post(crate::session_browser::add_project).patch(crate::session_browser::rename_project).delete(crate::session_browser::remove_project))
        .route("/agent/sessions/search", get(crate::session_browser::search))
        .route("/agent/memories", get(crate::memory_panel::get))
        .route("/agent/memories/{kind}/{id}", axum::routing::patch(crate::memory_panel::patch).delete(crate::memory_panel::delete))
        .route("/agent/sessions/{task_id}", axum::routing::delete(crate::session_browser::delete_session).patch(crate::session_browser::update_session))
        .route(
            "/agent/tasks/{task_id}",
            get(crate::agent_service::get_task)
                .patch(crate::agent_service::update_task)
                .delete(crate::agent_service::cancel_task),
        )
        .route("/agent/tasks/{task_id}/definition/decision", post(crate::agent_service::decide_task_definition))
        .route(
            "/agent/tasks/{task_id}/events",
            get(crate::agent_service::get_events),
        )
        .route(
            "/agent/tasks/{task_id}/changes/{turn}",
            get(crate::workspace_changes::get_summary),
        )
        .route(
            "/agent/tasks/{task_id}/changes/{turn}/files/{index}",
            get(crate::workspace_changes::get_diff),
        )
        .route(
            "/agent/tasks/{task_id}/changes/{turn}/undo",
            post(crate::workspace_changes::undo_turn),
        )
        .route(
            "/agent/tasks/{task_id}/context-debug",
            get(crate::request_context::setting).put(crate::request_context::set_setting),
        )
        .route(
            "/agent/tasks/{task_id}/request-contexts",
            get(crate::request_context::list),
        )
        .route(
            "/agent/tasks/{task_id}/request-contexts/{id}",
            get(crate::request_context::get),
        )
        .route(
            "/agent/tasks/{task_id}/visual-artifacts/{id}/image",
            get(crate::visual_artifacts::image_route),
        )
        .route(
            "/agent/tasks/{task_id}/rewind",
            post(crate::rewind::post),
        )
        .route(
            "/agent/tasks/{task_id}/interjections",
            post(crate::interjections::post),
        )
        .route(
            "/agent/tasks/{task_id}/approvals",
            post(crate::tool_approvals::decide),
        )
        .route(
            "/agent/tasks/{task_id}/skill-draft",
            post(crate::flow_skills::draft),
        )
        .route(
            "/agent/tasks/{task_id}/skills",
            post(crate::flow_skills::save_route),
        )
        .route(
            "/agent/tasks/{task_id}/flow/nodes/{node_id}/interrupt",
            post(crate::agent_service::interrupt_flow_node),
        )
        .route(
            "/agent/tasks/{task_id}/messages",
            post(crate::agent_service::continue_task)
                .layer(axum::extract::DefaultBodyLimit::max(280 * 1024 * 1024)),
        )
        .route(
            "/agent/tasks/{task_id}/stream",
            get(crate::agent_service::stream_events),
        )
        .route(
            "/api/session/uploadFileBinary",
            post(crate::attachments::upload).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/agent/tasks/{task_id}/attachments/{receipt}",
            get(crate::attachments::download),
        )
        .with_state(agent_state);

    let state = AiProxyState {
        config,
        config_path: config_path.to_path_buf(),
        settings_write: Arc::new(Mutex::new(())),
        client,
        runtime: runtime.clone(),
    };

    let app = Router::new()
        .route("/star.png", get(crate::agent_web::star_png))
        .route("/logo.png", get(crate::agent_web::star_png))
        .route("/favicon.png", get(crate::agent_web::favicon_png))
        .route("/favicon.ico", get(crate::agent_web::favicon_png))
        .route(
            "/agent/settings/data",
            get(get_agent_settings).put(put_agent_settings),
        )
        .route("/agent/models", get(get_model_catalog))
        .route("/agent/settings/discover", post(discover_agent_models))
        .route("/agent/settings/probe-image", post(probe_agent_image_input))
        .route("/agent/plugins", get(plugin_tree))
        .route(
            "/agent/skill-page-sdk.js",
            get(|| async {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/javascript; charset=utf-8",
                    )],
                    include_str!("../web/skill-page-sdk.js"),
                )
            }),
        )
        .route(
            "/agent/skill-packages",
            get(skill_packages).post(install_skill_package),
        )
        .route("/agent/skill-packages/remote", post(connect_remote_package))
        .route(
            "/agent/ppt/{method}",
            post(ppt_page_request).layer(axum::extract::DefaultBodyLimit::max(30 * 1024 * 1024)),
        )
        .route(
            "/agent/skill-packages/{id}",
            axum::routing::delete(remove_skill_package),
        )
        .route(
            "/agent/skill-packages/{id}/reload",
            post(reload_skill_package),
        )
        .route(
            "/agent/skill-packages/{id}/enabled",
            post(enable_skill_package),
        )
        .route(
            "/agent/skill-packages/{id}/pages/{page}/{*resource}",
            get(skill_page_asset),
        )
        .merge(agent_routes)
        .with_state(state)
        .layer(CorsLayer::permissive());

    let addr = listener.local_addr()?;
    info!(%addr, "Agent service starting");

    let served = axum::serve(listener, app).await;
    runtime.shutdown()?;
    served?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_probe_evidence_is_persisted_and_bound_to_the_saved_route() {
        let root = std::env::temp_dir().join(format!(
            "codex-image-config-{}",
            crate::visual_artifacts::new_id()
        ));
        let path = root.join("profiles/web/cordis.patch.yml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut patch = json!([{"id":"llm-pi-ai","config":{"providers":{"test":{"baseURL":"https://first.example/v1","api":"openai-completions","models":[{"id":"m"}]}}}},
            {"id":"agent-default-model","config":{"provider":"test","model":"m"}}]);
        write_yaml_atomic(&path, &patch).unwrap();
        let generation = generation_settings_path(&path);
        write_yaml_atomic(&generation, &json!({"custom_setting":"retain me"})).unwrap();
        let report = json!({"status":"verified","capability":"supported","request":{"contains_image_content":true},
            "route_fingerprint":crate::visual_probe::route_fingerprint("https://first.example/v1","m"),"response_body":"image-only challenge passed"});
        let original = disk_revision(&path).unwrap();
        let (_, revision) = persist_image_probe(&path, &original, "test", "m", &report).unwrap();
        assert_ne!(revision, original);
        assert_eq!(
            read_yaml(&generation, json!({})).unwrap()["custom_setting"],
            "retain me"
        );
        let config = load_config(&path).unwrap();
        let capability = &config.model_capabilities["test"]["m"];
        assert_eq!(
            capability.image_input,
            crate::visual_artifacts::ImageCapability::Supported
        );
        assert_eq!(capability.image_probe.as_ref().unwrap(), &report);
        assert!(
            persist_image_probe(&path, &original, "test", "m", &report).is_err(),
            "stale revisions cannot overwrite settings"
        );
        patch[0]["config"]["providers"]["test"]["api"] = json!("anthropic-messages");
        write_yaml_atomic(&path, &patch).unwrap();
        assert_eq!(
            load_config(&path).unwrap().model_capabilities["test"]["m"].image_input,
            crate::visual_artifacts::ImageCapability::Unknown,
            "a Chat Completions probe cannot verify another protocol"
        );
        patch[0]["config"]["providers"]["test"]["api"] = json!("openai-completions");
        patch[0]["config"]["providers"]["test"]["baseURL"] = json!("https://second.example/v1");
        write_yaml_atomic(&path, &patch).unwrap();
        assert_eq!(
            catalog_view(&load_config(&path).unwrap())["models"][0]["verification"]["image_input"],
            "unverified"
        );
        assert_eq!(
            load_config(&path).unwrap().model_capabilities["test"]["m"].image_input,
            crate::visual_artifacts::ImageCapability::Unknown
        );
        let edit:SettingsEdit=serde_json::from_value(json!({"revision":disk_revision(&path).unwrap(),"providers":[{"id":"test","url":"https://second.example/v1","api_type":"openai-completions","models":["other"],
            "model_capabilities":{"other":{"image_input":"supported","image_probe":report}}}]})).unwrap();
        assert!(
            save_settings(&path, edit)
                .unwrap_err()
                .to_string()
                .contains("verify image input"),
            "UI payloads cannot manufacture support evidence"
        );
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn tool_and_compression_settings_roundtrip_preserve_omitted_and_clear_explicitly() {
        let root = std::env::temp_dir().join(crate::visual_artifacts::new_id());
        let path = root.join("profiles/web/cordis.patch.yml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_yaml_atomic(&path,&json!([{"id":"llm-pi-ai","config":{"providers":{"test":{"baseURL":"http://localhost:1/v1","api":"openai-completions","models":[{"id":"m","maxTokens":65536}],"contextRequestOptions":{"m":{"chat_template_kwargs":{"enable_thinking":false}}}}}}},{"id":"agent-default-model","config":{"provider":"test","model":"m"}}])).unwrap();
        write_yaml_atomic(
            &credentials_path(&path),
            &json!({"version":1,"refs":{},"records":{}}),
        )
        .unwrap();
        let mut payload = json!({"revision":disk_revision(&path).unwrap(),"providers":[{"id":"test","url":"http://localhost:1/v1","models":["m","gpt-6.1-sol"]}],"orchestrator_provider":"test","orchestrator_model":"m","visual_tool":{"model":"gpt-6.1-sol","reasoning_effort":"medium"},"context_agent":{"model":"gpt-6.1-sol","reasoning_effort":"low","context_window":500000},"delegate_agent":{"model":"m","reasoning_effort":"medium"}});
        payload["plugin_models"] =
            json!({"ppt-master":[{"provider":"test","model":"gpt-6.1-sol"}]});
        payload["plugin_generation"] =
            json!({"ppt-master":{"reasoning_effort":"low","visual_reasoning_effort":"low"}});
        save_settings(&path, serde_json::from_value(payload.clone()).unwrap())
            .unwrap()
            .unwrap();
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.context_agent.context_window, Some(500000));
        assert_eq!(loaded.delegate_agent.model.as_deref(), Some("m"));
        assert_eq!(loaded.delegate_agent.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(
            loaded.plugin_generation["ppt-master"]
                .reasoning_effort
                .as_deref(),
            Some("low")
        );
        let scoped = restricted_config(&loaded, "ppt-master").unwrap();
        assert_eq!(scoped.visual_tool.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            loaded.visual_tool.reasoning_effort.as_deref(),
            Some("medium")
        );
        let mut unrestricted = loaded.clone();
        unrestricted.plugin_models.clear();
        assert_eq!(
            restricted_config(&unrestricted, "ppt-master")
                .unwrap()
                .reasoning_effort
                .as_deref(),
            Some("low")
        );
        assert_eq!(
            restricted_config(&unrestricted, "unrelated")
                .unwrap()
                .reasoning_effort,
            loaded.reasoning_effort
        );
        assert_eq!(
            catalog_view(&scoped)["defaults"]["plugin_generation"]["ppt-master"]["reasoning_effort"],
            "low"
        );
        assert_eq!(
            loaded.providers["test"].extra["contextRequestOptions"]["m"]["chat_template_kwargs"]["enable_thinking"],
            false
        );
        assert_eq!(
            plugin_catalog_view(&loaded, "ppt-master")["models"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let mut invalid = payload.clone();
        invalid["revision"] = json!(disk_revision(&path).unwrap());
        invalid["plugin_models"]["ppt-master"][0]["model"] = json!("unregistered");
        assert!(
            save_settings(&path, serde_json::from_value(invalid).unwrap())
                .unwrap_err()
                .to_string()
                .contains("未在可用模型目录中登记")
        );
        let mut invalid = payload.clone();
        invalid["revision"] = json!(disk_revision(&path).unwrap());
        invalid["providers"][0]["models"] = json!(["m"]);
        assert!(
            save_settings(&path, serde_json::from_value(invalid).unwrap())
                .unwrap_err()
                .to_string()
                .contains("not in provider")
        );
        let mut invalid = payload.clone();
        invalid["revision"] = json!(disk_revision(&path).unwrap());
        invalid["visual_tool"]["provider"] = json!("missing");
        assert!(
            save_settings(&path, serde_json::from_value(invalid).unwrap())
                .unwrap_err()
                .to_string()
                .contains("provider is unavailable")
        );
        // Saving settings keeps the model entry, so its declared limit survives.
        assert_eq!(loaded.providers["test"].max_output_tokens["m"], 65536);
        assert_eq!(
            loaded.visual_tool.reasoning_effort.as_deref(),
            Some("medium")
        );
        payload["revision"] = json!(disk_revision(&path).unwrap());
        payload.as_object_mut().unwrap().remove("visual_tool");
        payload.as_object_mut().unwrap().remove("context_agent");
        payload.as_object_mut().unwrap().remove("plugin_models");
        save_settings(&path, serde_json::from_value(payload.clone()).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            load_config(&path).unwrap().context_agent,
            loaded.context_agent
        );
        payload["revision"] = json!(disk_revision(&path).unwrap());
        payload["visual_tool"] = json!({});
        payload["context_agent"] = json!({});
        save_settings(&path, serde_json::from_value(payload).unwrap())
            .unwrap()
            .unwrap();
        let cleared = load_config(&path).unwrap();
        assert_eq!(
            plugin_catalog_view(&cleared, "ppt-master")["models"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(cleared.context_agent, Default::default());
        assert_eq!(cleared.visual_tool, Default::default());
        assert_eq!(cleared.orchestrator_model.as_deref(), Some("m"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn model_catalog_projects_registered_models_and_keeps_credentials_private() {
        let mut config: AiProxyConfig = serde_json::from_value(json!({
            "orchestrator_provider":"route", "orchestrator_model":"m",
            "visual_tool":{"provider":"route","model":"m"},
            "providers":{"route":{"url":"https://private.example/v1","api_key":"secret-value","api_key_ref":"PRIVATE_ENV","model_map":{"m":"upstream-m"},"api_type":"openai-completions","displayName":"My route"}},
            "model_capabilities":{"route":{"m":{"reasoning_efforts":["low","medium"],"image_input":"unknown","image_probe":{"status":"unverified","response_body":"private-response"}}}}
        })).unwrap();
        config
            .providers
            .get_mut("route")
            .unwrap()
            .model_metadata
            .insert(
                "m".into(),
                json!({"name":"Visible model","contextWindow":500000}),
            );
        config
            .providers
            .get_mut("route")
            .unwrap()
            .max_output_tokens
            .insert("m".into(), 65536);
        let view = catalog_view(&config);
        assert_eq!(view["models"].as_array().unwrap().len(), 1);
        assert_eq!(view["models"][0]["name"], "Visible model");
        assert_eq!(view["models"][0]["context_window"], 500000);
        assert_eq!(view["models"][0]["max_output_tokens"], 65536);
        assert_eq!(view["defaults"]["visual_tool"]["model"], "m");
        let encoded = view.to_string();
        for private in [
            "private.example",
            "secret-value",
            "PRIVATE_ENV",
            "private-response",
            "upstream-m",
        ] {
            assert!(!encoded.contains(private));
        }
        config.providers.get_mut("route").unwrap().model_map.clear();
        assert!(
            catalog_view(&config)["models"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
