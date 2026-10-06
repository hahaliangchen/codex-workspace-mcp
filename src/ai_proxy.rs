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
use tracing::{error, info};

use crate::format_translate;
use crate::{plugin_builtin, plugin_runtime};

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
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModelCapabilities {
    #[serde(default)]
    reasoning_efforts: Vec<String>,
    #[serde(default)]
    default_effort: Option<String>,
    #[serde(default)]
    fast_mode: bool,
    #[serde(default)]
    image_input:crate::visual_artifacts::ImageCapability,
}

fn built_in_capabilities(model: &str) -> ModelCapabilities {
    if model == "gpt-6-luna" {
        ModelCapabilities {
            reasoning_efforts: ["none", "low", "medium", "high", "xhigh", "max"]
                .into_iter().map(str::to_owned).collect(),
            default_effort: Some("max".into()),
            fast_mode: true,
            image_input:Default::default(),
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
    enable_subagent: bool,
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
    #[serde(default)]
    expert_provider: Option<String>,
    #[serde(default)]
    expert_model: Option<String>,
    #[serde(default = "observer_enabled_by_default")]
    observer_enabled: bool,
    #[serde(default)]
    observer_provider: Option<String>,
    #[serde(default)]
    observer_model: Option<String>,
    #[serde(default)]
    visual_fallback_enabled:bool,
    #[serde(default)]
    visual_provider:Option<String>,
    #[serde(default)]
    visual_model:Option<String>,
    #[serde(default)]
    providers: HashMap<String, ProviderConfig>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

fn observer_enabled_by_default() -> bool {
    true
}

impl AiProxyConfig {
    fn resolve_expert_provider(&self) -> Option<crate::expert_surgery::ExpertProvider> {
        if !self.enable_subagent { return None; }
        // Legacy code-surgery routing is separate from the standalone task subagent.
        // Inheriting the task model must not silently expose another tool here.
        let provider_name = self.expert_provider.as_deref()?;
        let provider = self.providers.get(provider_name)?;
        let model = self.expert_model.clone()?;

        Some(crate::expert_surgery::ExpertProvider {
            url: provider.url.trim_end_matches('/').to_string(),
            api_key: provider.api_key.clone(),
            model,
        })
    }
}

pub(crate) fn apply_agent_config(
    state: &mut crate::agent_service::AgentServiceState,
    config: &AiProxyConfig,
) {
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
    state.provider_routes = config.providers.iter()
        .filter(|(_, provider)| provider.api_type == "openai-completions")
        .map(|(name, provider)| (name.clone(), crate::agent_service::AgentProviderRoute {
            url: provider.url.clone(), api_key: provider.api_key.clone(), models: provider.model_map.clone(),
        }))
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
    let subagent = config
        .expert_provider
        .as_deref()
        .and_then(|name| config.providers.get(name))
        .filter(|provider| provider.api_type == "openai-completions")
        .or(main);
    state.expert_provider_name = config.expert_provider.clone().unwrap_or_else(|| main_name.to_owned());
    state.expert_provider_url = subagent.map(|p| p.url.clone()).unwrap_or_default();
    state.expert_api_key = subagent.map(|p| p.api_key.clone()).unwrap_or_default();
    state.expert_model = config
        .expert_model
        .clone()
        .or_else(|| config.orchestrator_model.clone())
        .or_else(|| subagent.and_then(|p| p.model_map.values().next().cloned()))
        .unwrap_or_default();
    state.subagent_inherits_model = config.expert_provider.is_none() && config.expert_model.is_none();
    state.enable_subagent = config.enable_subagent && main.is_some() && subagent.is_some() && !state.expert_model.is_empty();

    let observer_name = config.observer_provider.as_deref().unwrap_or(main_name);
    let observer = config.providers.get(observer_name)
        .filter(|provider| provider.api_type == "openai-completions");
    state.observer_provider = observer.map(|_| observer_name.to_owned()).unwrap_or_default();
    state.observer_provider_url = observer.map(|p| p.url.clone()).unwrap_or_default();
    state.observer_api_key = observer.map(|p| p.api_key.clone()).unwrap_or_default();
    state.observer_model = config.observer_model.clone().unwrap_or_default();
    state.observer_inherits_model = config.observer_provider.is_none() && config.observer_model.is_none();
    state.observer_enabled = config.observer_enabled
        && observer.is_some()
        && (state.observer_inherits_model || !state.observer_model.is_empty());
    state.visual.capabilities=config.model_capabilities.iter().map(|(provider,models)|(provider.clone(),models.iter().flat_map(|(model,cap)| {
        let mapped=config.providers.get(provider).and_then(|p|p.model_map.get(model)).cloned().unwrap_or_else(||model.clone());
        [(model.clone(),cap.image_input),(mapped,cap.image_input)]
    }).collect())).collect();
    state.visual.fallback=if config.visual_fallback_enabled {
        config.visual_provider.as_ref().zip(config.visual_model.as_ref()).and_then(|(provider,model)| {
            let route=config.providers.get(provider)?;
            (route.api_type=="openai-completions" && state.visual.capability(provider,model)==crate::visual_artifacts::ImageCapability::Supported)
                .then(||crate::visual_artifacts::VisualRoute {provider:provider.clone(),model:route.model_map.get(model).cloned().unwrap_or_else(||model.clone()),url:route.url.clone(),api_key:route.api_key.clone()})
        })
    }else{None};
}

pub(crate) fn dsh_config_path() -> std::path::PathBuf {
    let home = std::env::var_os("DSH_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|v| std::path::PathBuf::from(v).join(".dsh")))
        .or_else(|| std::env::var_os("HOME").map(|v| std::path::PathBuf::from(v).join(".dsh")))
        .unwrap_or_else(|| std::path::PathBuf::from(".dsh"));
    let profile = std::env::var("DSH_PROFILE").unwrap_or_else(|_| "web".into());
    home.join("profiles").join(profile).join("cordis.patch.yml")
}

fn credentials_path(config_path: &Path) -> std::path::PathBuf {
    config_path.parent().and_then(Path::parent).and_then(Path::parent)
        .expect("DSH profile path has a home directory").join(".credentials.yaml")
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
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("configuration path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let suffix = format!("{}.{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
    let temporary = parent.join(format!(".{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), suffix));
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(serde_yaml::to_string(value)?.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result
}

fn plugin_config<'a>(patch: &'a Value, id: &str) -> Option<&'a Value> {
    patch.as_array()?.iter().find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))?.get("config")
}

fn set_plugin_config(patch: &mut Value, id: &str, config: Value) -> anyhow::Result<()> {
    let entries = patch.as_array_mut().ok_or_else(|| anyhow::anyhow!("DSH profile patch must be a YAML sequence"))?;
    if let Some(entry) = entries.iter_mut().find(|entry| entry.get("id").and_then(Value::as_str) == Some(id)) {
        entry.as_object_mut().ok_or_else(|| anyhow::anyhow!("DSH plugin entry is not a mapping"))?
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
        if line.starts_with('#') { continue; }
        let Some((name, value)) = line.split_once('=') else { continue };
        if name.trim() != reference { continue; }
        let value = value.trim();
        let value = if value.len() >= 2 && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\''))) {
            &value[1..value.len()-1]
        } else { value };
        if !value.is_empty() { return Some(value.to_owned()); }
    }
    None
}

fn credential_value(credentials: &Value, config_path: &Path, reference: &str) -> String {
    std::env::var(reference).ok().filter(|s| !s.is_empty())
        .or_else(|| credentials.pointer(&format!("/refs/{reference}")).and_then(Value::as_str).map(str::to_owned))
        .or_else(|| std::env::current_dir().ok().and_then(|dir| dotenv_value(&dir.join(".env"), reference)))
        .or_else(|| dotenv_value(&credentials_path(config_path).with_file_name(".env"), reference))
        .unwrap_or_default()
}

pub(crate) fn load_config(config_path: &Path) -> anyhow::Result<AiProxyConfig> {
    let patch = read_yaml(config_path, json!([]))?;
    anyhow::ensure!(patch.is_array(), "DSH profile patch must be a YAML sequence");
    let credentials = read_yaml(&credentials_path(config_path), json!({"version":1,"refs":{},"records":{}}))?;
    let mut config = AiProxyConfig::default();
    if let Some(providers) = plugin_config(&patch, "llm-pi-ai").and_then(|v| v.get("providers")).and_then(Value::as_object) {
        for (id, entry) in providers {
            let Some(url) = entry.get("baseURL").and_then(Value::as_str) else { continue };
            let Some(models) = entry.get("models").and_then(Value::as_array) else { continue };
            let model_map = models.iter().filter_map(|v| v.get("id").and_then(Value::as_str).map(|id| (id.to_owned(), id.to_owned()))).collect();
            let reference = entry.get("apiKeyEnv").and_then(Value::as_str).map(str::to_owned);
            let api_key = reference.as_deref().map(|r| credential_value(&credentials, config_path, r)).unwrap_or_default();
            config.providers.insert(id.clone(), ProviderConfig {
                url: url.to_owned(), api_key, api_key_ref: reference, model_map,
                api_type: entry.get("api").and_then(Value::as_str).unwrap_or("openai-completions").to_owned(),
                extra: entry.get("displayName").and_then(Value::as_str)
                    .map(|name| HashMap::from([("displayName".to_owned(), json!(name))]))
                    .unwrap_or_default(),
            });
        }
    }
    if let Some(main) = plugin_config(&patch, "agent-default-model") {
        config.orchestrator_provider = main.get("provider").and_then(Value::as_str).map(str::to_owned);
        config.orchestrator_model = main.get("model").and_then(Value::as_str).map(str::to_owned);
        config.default_provider = config.orchestrator_provider.clone();
    }
    if let Some(selection) = plugin_config(&patch, "subagent-model-selection-settings") {
        config.enable_subagent = selection.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        if let Some(route) = selection.get("allowedModels").and_then(Value::as_array).and_then(|a| a.first()) {
            let provider = route.get("provider").and_then(Value::as_str);
            let model = route.get("model").and_then(Value::as_str);
            if provider != config.orchestrator_provider.as_deref() || model != config.orchestrator_model.as_deref() {
                config.expert_provider = provider.map(str::to_owned);
                config.expert_model = model.map(str::to_owned);
            }
        }
    }
    let observer = read_yaml(&observer_settings_path(config_path), json!({}))?;
    config.observer_enabled = observer.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    config.observer_provider = observer.get("provider").and_then(Value::as_str).map(str::to_owned);
    config.observer_model = observer.get("model").and_then(Value::as_str).map(str::to_owned);
    let generation = read_yaml(&generation_settings_path(config_path), json!({}))?;
    config.reasoning_effort = generation.get("reasoning_effort").and_then(Value::as_str).map(str::to_owned);
    config.fast_mode = generation.get("fast_mode").and_then(Value::as_bool).unwrap_or(false);
    config.visual_fallback_enabled=generation["visual_fallback_enabled"].as_bool().unwrap_or(false);
    config.visual_provider=generation["visual_provider"].as_str().map(str::to_owned);
    config.visual_model=generation["visual_model"].as_str().map(str::to_owned);
    config.model_capabilities = generation.get("model_capabilities")
        .cloned().map(serde_json::from_value).transpose()?.unwrap_or_default();
    for (provider_id, provider) in &config.providers {
        let models = config.model_capabilities.entry(provider_id.clone()).or_default();
        for model_id in provider.model_map.keys() {
            models.entry(model_id.clone()).or_insert_with(|| built_in_capabilities(model_id));
        }
    }
    Ok(config)
}

pub(crate) struct DshRoute {
    pub name: String,
    pub url: String,
    pub api_key: String,
    pub model: String,
}

pub(crate) fn selected_route(expert: bool) -> anyhow::Result<DshRoute> {
    let config = load_config(&dsh_config_path())?;
    if expert { anyhow::ensure!(config.enable_subagent, "DSH subagent model selection is disabled"); }
    let (name, model) = if expert {
        (
            config.expert_provider.or(config.orchestrator_provider),
            config.expert_model.or(config.orchestrator_model),
        )
    } else {
        (config.orchestrator_provider, config.orchestrator_model)
    };
    let name = name.ok_or_else(|| anyhow::anyhow!("DSH model provider is not configured"))?;
    let provider = config.providers.get(&name).ok_or_else(|| anyhow::anyhow!("DSH provider {name} is unavailable"))?;
    anyhow::ensure!(provider.api_type == "openai-completions", "this Rust agent requires an OpenAI Chat Completions route");
    Ok(DshRoute {
        name, url: provider.url.clone(), api_key: provider.api_key.clone(),
        model: model.ok_or_else(|| anyhow::anyhow!("DSH model is not configured"))?,
    })
}

fn disk_revision(config_path: &Path) -> anyhow::Result<String> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in [config_path.to_path_buf(), credentials_path(config_path), observer_settings_path(config_path), generation_settings_path(config_path)] {
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
        "expert_provider": config.expert_provider,
        "expert_model": config.expert_model,
        "enable_subagent": config.enable_subagent,
        "observer_enabled": config.observer_enabled,
        "observer_provider": config.observer_provider,
        "observer_model": config.observer_model,
        "visual_fallback_enabled":config.visual_fallback_enabled,"visual_provider":config.visual_provider,"visual_model":config.visual_model,
    })
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
    expert_provider: Option<String>,
    expert_model: Option<String>,
    #[serde(default)]
    enable_subagent: bool,
    #[serde(default)]
    observer_enabled: Option<bool>,
    #[serde(default)]
    observer_provider: Option<String>,
    #[serde(default)]
    observer_model: Option<String>,
    #[serde(default)]
    visual_fallback_enabled:Option<bool>,
    #[serde(default)]
    visual_provider:Option<String>,
    #[serde(default)]
    visual_model:Option<String>,
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
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
            matches!(draft.api_type.as_str(), "openai-completions" | "openai-responses" | "anthropic-messages"),
            "provider {id} has unsupported protocol"
        );
        anyhow::ensure!(
            !draft.models.is_empty()
                && draft.models.iter().all(|model| !model.trim().is_empty()),
            "provider {id} needs at least one model"
        );
        anyhow::ensure!(draft.models.iter().collect::<std::collections::HashSet<_>>().len() == draft.models.len(), "provider {id} has duplicate model IDs");
        let configured = draft.model_capabilities.unwrap_or_else(|| config.model_capabilities.get(&id).cloned().unwrap_or_default());
        let mut capabilities = HashMap::new();
        for model in &draft.models {
            let capability = configured.get(model).cloned().unwrap_or_else(|| built_in_capabilities(model));
            anyhow::ensure!(capability.reasoning_efforts.iter().all(|effort| matches!(effort.as_str(), "none" | "low" | "medium" | "high" | "xhigh" | "max")), "provider {id} model {model} has an unsupported reasoning effort");
            anyhow::ensure!(capability.reasoning_efforts.iter().collect::<std::collections::HashSet<_>>().len() == capability.reasoning_efforts.len(), "provider {id} model {model} has duplicate reasoning efforts");
            anyhow::ensure!(capability.default_effort.as_ref().is_none_or(|effort| capability.reasoning_efforts.contains(effort)), "provider {id} model {model} default effort is not in its supported levels");
            capabilities.insert(model.clone(), capability);
        }
        model_capabilities.insert(id.clone(), capabilities);
        let previous = config.providers.get(&id);
        let requested_key = draft.api_key.filter(|v| !v.is_empty());
        if draft.clear_api_key { key_actions.insert(id.clone(), None); }
        else if let Some(value) = &requested_key { key_actions.insert(id.clone(), Some(value.clone())); }
        let key = if draft.clear_api_key {
            String::new()
        } else if let Some(value) = requested_key {
            anyhow::ensure!(
                value.len() <= 8192,
                "provider {id} has an invalid API key"
            );
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
            extra: HashMap::new(),
        });
        provider.url = url.as_str().trim_end_matches('/').to_owned();
        provider.api_key = key;
        provider.model_map = draft.models.into_iter().map(|model| (model.clone(), model)).collect();
        provider.api_type = draft.api_type;
        match nonempty(draft.display_name) {
            Some(name) => { provider.extra.insert("displayName".into(), json!(name)); }
            None => { provider.extra.remove("displayName"); }
        }
        providers.insert(id, provider);
    }
    config.providers = providers;
    config.model_capabilities = model_capabilities;
    config.orchestrator_provider = nonempty(request.orchestrator_provider);
    config.orchestrator_model = nonempty(request.orchestrator_model);
    config.expert_provider = nonempty(request.expert_provider);
    config.expert_model = nonempty(request.expert_model);
    config.enable_subagent = request.enable_subagent;
    if let Some(enabled) = request.observer_enabled {
        config.observer_enabled = enabled;
    }
    config.observer_provider = nonempty(request.observer_provider);
    config.observer_model = nonempty(request.observer_model);
    if let Some(enabled)=request.visual_fallback_enabled {config.visual_fallback_enabled=enabled;config.visual_provider=nonempty(request.visual_provider);config.visual_model=nonempty(request.visual_model);}
    config.default_provider = config.orchestrator_provider.clone();
    if let Some(effort) = request.reasoning_effort {
        let effort = effort.trim();
        anyhow::ensure!(effort.is_empty() || matches!(effort, "none" | "low" | "medium" | "high" | "xhigh" | "max"),
            "unsupported reasoning effort");
        config.reasoning_effort = (!effort.is_empty()).then(|| effort.to_owned());
    }
    if let Some(fast_mode) = request.fast_mode { config.fast_mode = fast_mode; }
    if config.visual_fallback_enabled {
        let capability=config.visual_provider.as_ref().and_then(|provider|config.model_capabilities.get(provider)).and_then(|models|config.visual_model.as_ref().and_then(|model|models.get(model)));
        anyhow::ensure!(capability.is_some_and(|cap|cap.image_input==crate::visual_artifacts::ImageCapability::Supported),"visual fallback requires an explicitly configured image-supported route");
    }
    let selected_capability = config.orchestrator_provider.as_ref()
        .and_then(|provider| config.model_capabilities.get(provider))
        .and_then(|models| config.orchestrator_model.as_ref().and_then(|model| models.get(model)));
    if let Some(capability) = selected_capability {
        anyhow::ensure!(config.reasoning_effort.as_ref().is_none_or(|effort| capability.reasoning_efforts.contains(effort)), "selected model does not support this reasoning effort");
        anyhow::ensure!(!config.fast_mode || capability.fast_mode, "selected model does not support fast mode");
    } else {
        config.reasoning_effort = None;
        config.fast_mode = false;
    }
    for name in [
        &config.orchestrator_provider,
        &config.expert_provider,
        &config.observer_provider,
    ]
    .into_iter()
    .flatten()
    {
        anyhow::ensure!(
            config.providers.contains_key(name),
            "selected provider {name} does not exist"
        );
    }
    anyhow::ensure!(config.orchestrator_provider.is_some() == config.orchestrator_model.is_some(), "choose both a main-agent provider and model");
    anyhow::ensure!(config.expert_provider.is_some() == config.expert_model.is_some(), "choose both a subagent provider and model");
    anyhow::ensure!(config.observer_provider.is_some() == config.observer_model.is_some(), "choose both an observer provider and model");
    for (provider, model) in [
        (&config.orchestrator_provider, &config.orchestrator_model),
        (&config.expert_provider, &config.expert_model),
        (&config.observer_provider, &config.observer_model),
    ] {
        if let (Some(provider), Some(model)) = (provider, model) {
            anyhow::ensure!(config.providers[provider].model_map.contains_key(model), "model {model} is not in provider {provider}'s catalog");
        }
    }
    if config.enable_subagent {
        anyhow::ensure!(
            config.orchestrator_provider.is_some() && config.orchestrator_model.is_some(),
            "configure the main model before enabling subagent"
        );
    }
    for name in [&config.orchestrator_provider, &config.expert_provider, &config.observer_provider]
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
    let mut credentials = read_yaml(&credentials_path(config_path), json!({"version":1,"refs":{},"records":{}}))?;
    anyhow::ensure!(patch.is_array(), "DSH profile patch must be a YAML sequence");
    anyhow::ensure!(credentials.get("version").and_then(Value::as_u64) == Some(1), "unsupported DSH credentials version");
    let refs = credentials.get_mut("refs").and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("DSH credentials refs must be a mapping"))?;
    let old_providers = plugin_config(&patch, "llm-pi-ai")
        .and_then(|v| v.get("providers")).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut dsh_providers = old_providers.clone();
    for id in old_providers.keys() {
        if config.providers.contains_key(id) || old_providers[id].get("models").is_none() || old_providers[id].get("baseURL").is_none() { continue }
        dsh_providers.remove(id);
    }
    for (id, provider) in &config.providers {
        let mut entry = old_providers.get(id).cloned().unwrap_or_else(|| json!({}));
        let item = entry.as_object_mut().ok_or_else(|| anyhow::anyhow!("provider {id} is not a mapping"))?;
        item.insert("baseURL".into(), json!(provider.url));
        item.insert("api".into(), json!(provider.api_type));
        if let Some(name) = provider.extra.get("displayName") { item.insert("displayName".into(), name.clone()); }
        else { item.remove("displayName"); }
        let mut models = Vec::new();
        for model in provider.model_map.values() {
            let previous = old_providers.get(id).and_then(|v| v.get("models")).and_then(Value::as_array)
                .and_then(|a| a.iter().find(|v| v.get("id").and_then(Value::as_str) == Some(model)));
            models.push(previous.cloned().unwrap_or_else(|| json!({"id":model})));
        }
        models.sort_by(|a,b| a["id"].as_str().cmp(&b["id"].as_str()));
        models.dedup_by(|a,b| a["id"] == b["id"]);
        item.insert("models".into(), json!(models));
        let reference = provider.api_key_ref.clone().unwrap_or_else(|| format!("{}_API_KEY", id.to_ascii_uppercase().replace('-', "_")));
        match key_actions.get(id) {
            Some(None) => {
                anyhow::ensure!(std::env::var_os(&reference).is_none(), "{reference} is supplied by the environment and cannot be cleared here");
                item.remove("apiKeyEnv");
                refs.remove(&reference);
            }
            Some(Some(key)) => {
                anyhow::ensure!(std::env::var_os(&reference).is_none(), "{reference} is supplied by the environment and cannot be replaced here");
                item.insert("apiKeyEnv".into(), json!(reference));
                refs.insert(reference, json!(key));
            }
            None => {}
        }
        dsh_providers.insert(id.clone(), entry);
    }
    let mut llm = plugin_config(&patch, "llm-pi-ai").cloned().unwrap_or_else(|| json!({}));
    llm.as_object_mut().ok_or_else(|| anyhow::anyhow!("llm-pi-ai config is not a mapping"))?
        .insert("providers".into(), Value::Object(dsh_providers));
    set_plugin_config(&mut patch, "llm-pi-ai", llm)?;
    if let (Some(provider), Some(model)) = (&config.orchestrator_provider, &config.orchestrator_model) {
        let mut default_model = plugin_config(&patch, "agent-default-model").cloned().unwrap_or_else(|| json!({}));
        let item = default_model.as_object_mut().ok_or_else(|| anyhow::anyhow!("agent-default-model config is not a mapping"))?;
        item.insert("provider".into(), json!(provider));
        item.insert("model".into(), json!(model));
        set_plugin_config(&mut patch, "agent-default-model", default_model)?;
    } else if let Some(entries) = patch.as_array_mut() {
        entries.retain(|entry| entry.get("id").and_then(Value::as_str) != Some("agent-default-model"));
    }
    let mut subagent = plugin_config(&patch, "subagent").cloned().unwrap_or_else(|| json!({}));
    let item = subagent.as_object_mut().ok_or_else(|| anyhow::anyhow!("subagent config is not a mapping"))?;
    item.insert("maxDepth".into(), json!(1));
    item.insert("maxActiveSubagents".into(), json!(1));
    set_plugin_config(&mut patch, "subagent", subagent)?;
    let allowed = match (
        config.expert_provider.as_ref().or(config.orchestrator_provider.as_ref()),
        config.expert_model.as_ref().or(config.orchestrator_model.as_ref()),
    ) {
        (Some(provider), Some(model)) => json!([{"provider":provider,"model":model}]),
        _ => json!([]),
    };
    let mut selection = plugin_config(&patch, "subagent-model-selection-settings").cloned().unwrap_or_else(|| json!({}));
    let item = selection.as_object_mut().ok_or_else(|| anyhow::anyhow!("subagent model selection config is not a mapping"))?;
    item.insert("enabled".into(), json!(config.enable_subagent));
    item.insert("allowedModels".into(), allowed);
    set_plugin_config(&mut patch, "subagent-model-selection-settings", selection)?;
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
        "reasoning_effort": config.reasoning_effort,
        "fast_mode": config.fast_mode,
        "model_capabilities": config.model_capabilities,
        "visual_fallback_enabled":config.visual_fallback_enabled,"visual_provider":config.visual_provider,"visual_model":config.visual_model,
    });
    if disk_revision(config_path)? != request.revision {
        return Ok(Err("settings changed while saving; reload before retrying".into()));
    }
    if let Some(parent) = secret_path.parent() { std::fs::create_dir_all(parent)?; }
    if let Some(parent) = config_path.parent() { std::fs::create_dir_all(parent)?; }
    if !key_actions.is_empty() { write_yaml_atomic(&secret_path, &credentials)?; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))?;
    }
    if patch != original_patch { write_yaml_atomic(config_path, &patch)?; }
    if observer_settings != original_observer { write_yaml_atomic(&observer_path, &observer_settings)?; }
    if generation_settings != original_generation { write_yaml_atomic(&generation_path, &generation_settings)?; }
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
    workspace: Arc<crate::tools::Workspace>,
    runtime: Arc<plugin_runtime::PluginRuntime>,
}

async fn plugin_tree(State(state): State<AiProxyState>) -> Json<Value> {
    let events = state.runtime.service::<plugin_builtin::LifecycleLog>(plugin_runtime::ROOT, "plugin-events")
        .map(|log| log.snapshot()).unwrap_or_default();
    Json(json!({"plugins":state.runtime.snapshot(),"events":events}))
}

async fn current_config(state: &AiProxyState) -> AiProxyConfig {
    let path = state.config_path.clone();
    if let Ok(Ok(latest)) = tokio::task::spawn_blocking(move || load_config(&path)).await {
        *state.config.write().await = latest.clone();
        latest
    } else {
        state.config.read().await.clone()
    }
}

/// Resolve a client-visible model name → (provider config, upstream model name).
/// Looks up the model in the default provider's model_map; if not found,
/// passes the model name through to the default provider as-is.
fn resolve_model<'a>(
    config: &'a AiProxyConfig,
    model: &str,
) -> Result<(&'a ProviderConfig, String), Response> {
    let default = config.default_provider.as_deref().unwrap_or("");
    let provider = config.providers.get(default).ok_or_else(|| {
        error!(provider = %default, "default provider not found");
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("default provider not found: {}", default)})),
        )
            .into_response()
    })?;

    let upstream = provider
        .model_map
        .get(model)
        .cloned()
        .unwrap_or_else(|| model.to_owned());
    Ok((provider, upstream))
}

/// Resolve the DSH default model for the stateful Agent Runtime.
fn resolve_orchestrator_model<'a>(
    config: &'a AiProxyConfig,
    client_model: &str,
) -> Result<(&'a ProviderConfig, String), Response> {
    if config.orchestrator_provider.is_none() && config.orchestrator_model.is_none() {
        return resolve_model(config, client_model);
    }

    let provider_name = config
        .orchestrator_provider
        .as_deref()
        .or(config.default_provider.as_deref())
        .unwrap_or("");
    let provider = config.providers.get(provider_name).ok_or_else(|| {
        error!(provider = %provider_name, "orchestrator provider not found");
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("orchestrator provider not found: {}", provider_name)})),
        )
            .into_response()
    })?;
    let upstream = config
        .orchestrator_model
        .clone()
        .or_else(|| provider.model_map.get(client_model).cloned())
        .unwrap_or_else(|| client_model.to_owned());
    Ok((provider, upstream))
}

/// Truncate body for logging (keep first ~2000 chars).
pub(crate) fn fmt_body(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    if s.len() > 2000 {
        format!("{}… ({} bytes)", &s[..2000], s.len())
    } else {
        s.to_string()
    }
}

pub(crate) fn conversation_id_from_body(_body: &Value, workspace_root: &Path) -> String {
    format!(
        "workspace:{}",
        sanitize_conversation_id(&workspace_root.to_string_lossy())
    )
}

fn sanitize_conversation_id(id: &str) -> String {
    let mut out = String::new();
    for ch in id.chars().take(120) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_owned()
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /v1/models — list exposed model IDs from the default provider's model_map.
async fn list_models(State(state): State<AiProxyState>) -> impl IntoResponse {
    let config = current_config(&state).await;
    let default_p = config
        .default_provider
        .as_deref()
        .and_then(|d| config.providers.get(d));

    let models: Vec<Value> = default_p
        .map(|p| {
            p.model_map
                .keys()
                .map(|id| {
                    json!({
                        "id": id,
                        "object": "model",
                        "created": 0,
                        "owned_by": "ai-proxy"
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Json(json!({
        "object": "list",
        "data": models
    }))
}

/// POST /v1/chat/completions — OpenAI-compatible endpoint.
async fn chat_completions(
    State(state): State<AiProxyState>,
    headers:HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let config = current_config(&state).await;
    let client_model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_owned(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing model field"})),
            )
                .into_response();
        }
    };

    let task_visual=headers.get("x-codex-visual-dispatch").and_then(|value|value.to_str().ok())==Some("task-owned");
    if !task_visual {crate::vision_preprocess::set_visible_images_from_body(&body);}
    let had_image_input = crate::vision_preprocess::has_latest_user_image_input(&body);
    let mut image_stats = crate::vision_preprocess::ImageProcessStats::default();
    if !task_visual {crate::vision_preprocess::process_latest_user_images(&mut body, &mut image_stats).await;}

    info!("=== /v1/chat/completions  model={}", client_model);

    let (provider, mut upstream_model) = match resolve_model(&config, &client_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    if had_image_input && !task_visual {
        let old_model = upstream_model.clone();
        upstream_model = crate::vision_preprocess::adjust_model_for_vision(&upstream_model);
        if old_model != upstream_model {
            info!(
                "   [DYNAMIC ROUTING] Image detected. Switched model from {} to {}",
                old_model, upstream_model
            );
        }
    }

    info!(
        "   resolved: provider={} upstream_model={}",
        provider.url, upstream_model
    );

    body["model"] = json!(upstream_model);

    // Normalize role="developer" to "system" for upstream compatibility
    if let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) {
        for msg in &mut *messages {
            if let Some(role) = msg.get_mut("role") {
                if role.as_str() == Some("developer") {
                    *role = json!("system");
                }
            }
        }
        format_translate::clean_unmatched_tool_calls(messages);
    }

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    info!(
        "   chat completions body: {}",
        fmt_body(serde_json::to_string(&body).unwrap_or_default().as_bytes())
    );

    let upstream_url = format!("{}/chat/completions", provider.url);
    crate::upstream::forward_to_upstream(
        &state.client,
        &upstream_url,
        &provider.api_key,
        &body,
        is_stream,
        &client_model,
    )
    .await
}

/// POST /v1/messages — Anthropic Messages API endpoint.
async fn messages(State(state): State<AiProxyState>,headers:HeaderMap, Json(mut body): Json<Value>) -> Response {
    let config = current_config(&state).await;
    let task_visual=headers.get("x-codex-visual-dispatch").and_then(|value|value.to_str().ok())==Some("task-owned");
    if !task_visual {crate::vision_preprocess::set_visible_images_from_body(&body);}
    let had_image_input = crate::vision_preprocess::has_latest_user_image_input(&body);
    let mut image_stats = crate::vision_preprocess::ImageProcessStats::default();
    if !task_visual {crate::vision_preprocess::process_latest_user_images(&mut body, &mut image_stats).await;}
    let raw_model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");

    info!("=== /v1/messages  model={}", raw_model);
    info!(
        "   anthropic body: {}",
        fmt_body(serde_json::to_string(&body).unwrap_or_default().as_bytes())
    );

    let (provider, mut upstream_model) = match resolve_model(&config, raw_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    if had_image_input && !task_visual {
        let old_model = upstream_model.clone();
        upstream_model = crate::vision_preprocess::adjust_model_for_vision(&upstream_model);
        if old_model != upstream_model {
            info!(
                "   [DYNAMIC ROUTING] Image detected. Switched model from {} to {}",
                old_model, upstream_model
            );
        }
    }

    info!(
        "   resolved: provider={} upstream_model={}",
        provider.url, upstream_model
    );

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let is_anthropic_provider = provider.api_type == "anthropic-messages";

    let (forward_body, upstream_url) = if is_anthropic_provider {
        // Anthropic-native upstream: forward raw, no conversion.
        let mut raw_body = body.clone();
        raw_body["model"] = json!(upstream_model);
        info!("   anthropic native forward, url={}", provider.url);
        (raw_body, provider.url.clone())
    } else {
        // OpenAI-compatible upstream: convert Anthropic → OpenAI.
        let mut openai_body = format_translate::anthropic_to_openai(&body);
        openai_body["model"] = json!(upstream_model);
        info!(
            "   openai body: {}",
            fmt_body(
                serde_json::to_string(&openai_body)
                    .unwrap_or_default()
                    .as_bytes()
            )
        );
        let url = format!("{}/chat/completions", provider.url);
        (openai_body, url)
    };

    let resp = crate::upstream::forward_to_upstream(
        &state.client,
        &upstream_url,
        &provider.api_key,
        &forward_body,
        is_stream,
        raw_model,
    )
    .await;

    // For non-streaming OpenAI providers, convert response back to Anthropic.
    if !is_stream && !is_anthropic_provider && resp.status().is_success() {
        let status = resp.status();
        let body_bytes = match axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => {
                error!(%e, "failed to read response body for Anthropic conversion");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("response read: {e}")})),
                )
                    .into_response();
            }
        };

        info!("   openai resp body: {}", fmt_body(&body_bytes));

        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(openai_resp) => {
                let anthropic_resp = format_translate::openai_to_anthropic(&openai_resp, raw_model);
                info!(
                    "   anthropic resp: {}",
                    fmt_body(
                        serde_json::to_string(&anthropic_resp)
                            .unwrap_or_default()
                            .as_bytes()
                    )
                );
                (
                    status,
                    [("content-type", "application/json")],
                    Json(anthropic_resp),
                )
                    .into_response()
            }
            Err(e) => {
                error!(%e, "failed to parse upstream OpenAI response");
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("parse upstream: {e}")})),
                )
                    .into_response()
            }
        }
    } else {
        resp
    }
}

/// POST /v1/responses — OpenAI Responses API endpoint for Codex.
async fn responses(State(state): State<AiProxyState>, Json(body): Json<Value>) -> Response {
    let config = current_config(&state).await;
    let conversation_id = conversation_id_from_body(&body, state.workspace.root());
    let client_model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_owned(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing model field"})),
            )
                .into_response();
        }
    };

    let (provider, upstream_model) = match resolve_orchestrator_model(&config, &client_model) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let expert_provider = config.resolve_expert_provider();
    crate::responses::logging::log_request_body(&conversation_id, &body, &client_model).await;
    info!("   responses request entering local agent runtime with orchestrator model");
    crate::agent_runtime::run_responses_agent(
        state.client.clone(),
        state.workspace.clone(),
        provider.url.clone(),
        provider.api_key.clone(),
        body,
        upstream_model,
        client_model,
        expert_provider,
    )
    .await
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

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

    info!("========== AI Proxy started ==========");
    info!("config: {} total model mappings", total_maps);

    let runtime = Arc::new(plugin_runtime::PluginRuntime::new());
    runtime.mount(plugin_runtime::ROOT, plugin_builtin::DiagnosticsPlugin)?;
    runtime.mount(plugin_runtime::ROOT, plugin_builtin::WorkspacePlugin(workspace.clone()))?;
    runtime.mount(plugin_runtime::ROOT, plugin_builtin::ModelSettingsPlugin(Arc::new(plugin_builtin::ModelSettings {
        path: config_path.to_path_buf(), config: config.clone(),
    })))?;
    let tools_scope = runtime.mount(plugin_runtime::ROOT, plugin_builtin::ToolCatalogPlugin(client.clone()))?;
    let agent_state = (*runtime.service::<crate::agent_service::AgentServiceState>(tools_scope, "agent-tasks")?).clone();
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
        .route("/agent/classic", get(crate::agent_web::classic_page))
        .route("/agent/app.css", get(crate::agent_web::app_css))
        .route("/agent/app.js", get(crate::agent_web::app_js))
        .route("/agent/browser-runtime.js", get(crate::agent_web::browser_runtime_js))
        .route("/agent/settings", get(crate::agent_web::settings_page))
        .route("/agent/settings.css", get(crate::agent_web::settings_css))
        .route("/agent/settings.js", get(crate::agent_web::settings_js))
        .route("/agent/dsh-base.css", get(crate::agent_web::dsh_base_css))
        .route(
            "/agent/dsh-design.css",
            get(crate::agent_web::dsh_design_css),
        )
        .route("/agent/info", get(crate::agent_web::info))
        .route("/agent/workspace/files", get(crate::agent_service::workspace_files))
        .route("/agent/commands", get(crate::agent_service::slash_commands))
        .route(
            "/agent/tasks",
            get(crate::agent_service::list_tasks).post(crate::agent_service::create_task),
        )
        .route(
            "/agent/sessions",
            post(crate::agent_service::create_draft_task),
        )
        .route(
            "/agent/tasks/{task_id}",
            get(crate::agent_service::get_task)
                .patch(crate::agent_service::update_task)
                .delete(crate::agent_service::cancel_task),
        )
        .route(
            "/agent/tasks/{task_id}/events",
            get(crate::agent_service::get_events),
        )
        .route("/agent/tasks/{task_id}/changes/{turn}", get(crate::workspace_changes::get_summary))
        .route("/agent/tasks/{task_id}/changes/{turn}/files/{index}", get(crate::workspace_changes::get_diff))
        .route("/agent/tasks/{task_id}/changes/{turn}/undo", post(crate::workspace_changes::undo_turn))
        .route("/agent/tasks/{task_id}/context-debug", get(crate::request_context::setting).put(crate::request_context::set_setting))
        .route("/agent/tasks/{task_id}/request-contexts", get(crate::request_context::list))
        .route("/agent/tasks/{task_id}/request-contexts/{id}", get(crate::request_context::get))
        .route("/agent/tasks/{task_id}/notebook", get(crate::task_notebook::list))
        .route("/agent/tasks/{task_id}/visual-artifacts/{id}/image", get(crate::visual_artifacts::image_route))
        .route("/agent/tasks/{task_id}/notebook/history", get(crate::task_notebook::history))
        .route("/agent/tasks/{task_id}/notebook/materials/{id}", get(crate::task_notebook::get))
        .route(
            "/agent/tasks/{task_id}/flow/nodes/{node_id}/interrupt",
            post(crate::agent_service::interrupt_flow_node),
        )
        .route(
            "/agent/tasks/{task_id}/messages",
            post(crate::agent_service::continue_task),
        )
        .route(
            "/agent/tasks/{task_id}/stream",
            get(crate::agent_service::stream_events),
        )
        .with_state(agent_state);

    let state = AiProxyState {
        config,
        config_path: config_path.to_path_buf(),
        settings_write: Arc::new(Mutex::new(())),
        client,
        workspace,
        runtime: runtime.clone(),
    };

    let app = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .route("/star.png", get(crate::agent_web::star_png))
        .route("/logo.png", get(crate::agent_web::star_png))
        .route("/favicon.png", get(crate::agent_web::favicon_png))
        .route("/favicon.ico", get(crate::agent_web::favicon_png))
        .route(
            "/agent/settings/data",
            get(get_agent_settings).put(put_agent_settings),
        )
        .route("/agent/settings/discover", post(discover_agent_models))
        .route("/agent/plugins", get(plugin_tree))
        .merge(agent_routes)
        .with_state(state)
        .layer(CorsLayer::permissive());

    let addr = listener.local_addr()?;
    info!(%addr, "AI proxy starting");

    let served = axum::serve(listener, app).await;
    runtime.shutdown()?;
    served?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_id_uses_workspace_as_context_boundary() {
        let body = json!({
            "conversation": "conv_123",
            "previous_response_id": "resp_456"
        });

        let id = conversation_id_from_body(&body, Path::new("D:/workspace"));

        assert_eq!(id, "workspace:D:_workspace");
    }

    fn provider(url: &str, models: &[(&str, &str)]) -> ProviderConfig {
        ProviderConfig {
            url: url.to_string(),
            api_key: "test-key".to_string(),
            api_key_ref: None,
            model_map: models
                .iter()
                .map(|(client, upstream)| (client.to_string(), upstream.to_string()))
                .collect(),
            api_type: "openai".to_string(),
            extra: HashMap::new(),
        }
    }

    #[test]
    fn orchestrator_routing_keeps_legacy_model_when_not_configured() {
        let config = AiProxyConfig {
            enable_subagent: false,
            default_provider: Some("default".to_string()),
            orchestrator_provider: None,
            orchestrator_model: None,
            reasoning_effort: None,
            fast_mode: false,
            model_capabilities: HashMap::new(),
            expert_provider: None,
            expert_model: None,
            observer_enabled: false,
            observer_provider: None,
            observer_model: None,
            visual_fallback_enabled:false,visual_provider:None,visual_model:None,
            providers: [(
                "default".to_string(),
                provider(
                    "https://default.example/v1",
                    &[("gpt-5-codex", "pro-model")],
                ),
            )]
            .into_iter()
            .collect(),
            extra: HashMap::new(),
        };

        let (provider, model) = resolve_orchestrator_model(&config, "gpt-5-codex").unwrap();

        assert_eq!(provider.url, "https://default.example/v1");
        assert_eq!(model, "pro-model");
    }

    #[test]
    fn orchestrator_routing_uses_configured_cheap_model() {
        let config = AiProxyConfig {
            enable_subagent: false,
            default_provider: Some("default".to_string()),
            orchestrator_provider: Some("cheap".to_string()),
            orchestrator_model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: None,
            fast_mode: false,
            model_capabilities: HashMap::new(),
            expert_provider: None,
            expert_model: None,
            observer_enabled: false,
            observer_provider: None,
            observer_model: None,
            visual_fallback_enabled:false,visual_provider:None,visual_model:None,
            providers: [
                (
                    "default".to_string(),
                    provider(
                        "https://default.example/v1",
                        &[("gpt-5-codex", "pro-model")],
                    ),
                ),
                (
                    "cheap".to_string(),
                    provider(
                        "https://cheap.example/v1",
                        &[("gpt-5-codex", "cheap-mapped")],
                    ),
                ),
            ]
            .into_iter()
            .collect(),
            extra: HashMap::new(),
        };

        let (provider, model) = resolve_orchestrator_model(&config, "gpt-5-codex").unwrap();

        assert_eq!(provider.url, "https://cheap.example/v1");
        assert_eq!(model, "deepseek-v4-flash");
    }
}
