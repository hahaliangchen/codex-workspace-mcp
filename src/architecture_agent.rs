use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
pub struct AnalyzeArchitectureRequest {
    pub workspace_root: String,
    pub query: String,
    #[serde(default)]
    pub focus: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub record: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArchitectureAnalysis {
    pub area: String,
    pub summary: String,
    #[serde(default)]
    pub key_symbols: Vec<String>,
    #[serde(default)]
    pub key_files: Vec<String>,
    #[serde(default)]
    pub boundaries: String,
    #[serde(default)]
    pub common_tasks: Vec<String>,
    #[serde(default)]
    pub risks: String,
    #[serde(default)]
    pub minimal_change_scope: String,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub symbol_contexts: Vec<crate::memory::SymbolBusinessContext>,
}

#[derive(Debug, Serialize)]
pub struct AnalyzeArchitectureResponse {
    pub provider: String,
    pub model: String,
    pub recorded: bool,
    pub analysis: ArchitectureAnalysis,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProjectAnalysisDecision {
    #[serde(default)]
    pub requires_project_analysis: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub confidence: f64,
}

#[derive(Debug, Clone)]
struct ArchitectureProvider {
    name: String,
    url: String,
    api_key: String,
    model: String,
}

pub async fn analyze_architecture(
    workspace: &crate::tools::Workspace,
    request: AnalyzeArchitectureRequest,
) -> anyhow::Result<AnalyzeArchitectureResponse> {
    let workspace = workspace.with_selected_root(&request.workspace_root)?;
    let provider = get_architecture_provider()?;
    let prior_memories =
        workspace.search_architecture_memory(crate::memory::SearchArchitectureMemoryRequest {
            workspace_root: workspace.root().display().to_string(),
            query: request.query.clone(),
            limit: 5,
        })?;
    let prompt = build_architecture_prompt(&request, &prior_memories.matches);
    let raw = call_architecture_model(&provider, &prompt).await?;
    let analysis = parse_architecture_analysis(&raw)?;

    let recorded = if request.record {
        workspace.record_architecture_memory(crate::memory::RecordArchitectureMemoryRequest {
            workspace_root: workspace.root().display().to_string(),
            area: analysis.area.clone(),
            summary: analysis.summary.clone(),
            key_symbols: analysis.key_symbols.clone(),
            key_files: analysis.key_files.clone(),
            boundaries: analysis.boundaries.clone(),
            common_tasks: analysis.common_tasks.clone(),
            risks: analysis.risks.clone(),
        })?;
        for context in &analysis.symbol_contexts {
            workspace.record_symbol_business_context(
                crate::memory::RecordSymbolBusinessContextRequest {
                    workspace_root: workspace.root().display().to_string(),
                    symbol_id: context.symbol_id.clone(),
                    symbol_name: context.symbol_name.clone(),
                    language: context.language.clone(),
                    file_path: context.file_path.clone(),
                    belongs_to_area: if context.belongs_to_area.trim().is_empty() {
                        analysis.area.clone()
                    } else {
                        context.belongs_to_area.clone()
                    },
                    business_role: context.business_role.clone(),
                    common_tasks: context.common_tasks.clone(),
                    read_when: context.read_when.clone(),
                    avoid_when: context.avoid_when.clone(),
                    risks: context.risks.clone(),
                    confidence: context.confidence,
                },
            )?;
        }
        true
    } else {
        false
    };

    Ok(AnalyzeArchitectureResponse {
        provider: provider.name,
        model: provider.model,
        recorded,
        analysis,
    })
}

fn get_architecture_provider() -> anyhow::Result<ArchitectureProvider> {
    let config_path = ai_proxy_config_path();
    if !config_path.exists() {
        anyhow::bail!("ai_proxy_config.json not found in exe dir");
    }

    let config_content = std::fs::read_to_string(&config_path)?;
    let config: Value = serde_json::from_str(&config_content)?;
    let default_provider_name = config
        .get("default_provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing default_provider in config"))?;
    let provider_name = config
        .get("architecture_provider")
        .and_then(|v| v.as_str())
        .unwrap_or(default_provider_name);
    let providers = config
        .get("providers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("missing providers in config"))?;
    let provider = providers
        .get(provider_name)
        .ok_or_else(|| anyhow::anyhow!("architecture/default provider not found in providers"))?;

    let url = provider
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing url in architecture provider"))?
        .trim_end_matches('/')
        .to_string();
    let api_key = provider
        .get("api_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing api_key in architecture provider"))?
        .to_string();
    let requested_model = config
        .get("architecture_model")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
        .or_else(|| provider_default_model(provider))
        .ok_or_else(|| anyhow::anyhow!("architecture provider model_map is empty"))?;
    let model = resolve_provider_model(provider, &requested_model);

    Ok(ArchitectureProvider {
        name: provider_name.to_string(),
        url,
        api_key,
        model,
    })
}

fn ai_proxy_config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ai_proxy_config.json")
}

fn provider_default_model(provider: &Value) -> Option<String> {
    provider
        .get("model_map")
        .and_then(|v| v.as_object())
        .and_then(|m| m.values().next())
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
}

fn resolve_provider_model(provider: &Value, requested_model: &str) -> String {
    provider
        .get("model_map")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(requested_model))
        .and_then(|v| v.as_str())
        .unwrap_or(requested_model)
        .to_string()
}

pub async fn decide_project_analysis_requirement(
    query: &str,
) -> anyhow::Result<ProjectAnalysisDecision> {
    let provider = get_architecture_provider()?;
    let prompt = format!(
        "Latest user request:\n{}\n\nDecide whether this request requires analyzing the project's codebase, architecture, indexed symbols, logs, or workspace files before the main model answers. Return strict JSON only with fields: requires_project_analysis (boolean), reason (string), confidence (number 0..1).\n\nGuidance:\n- Return true for requests to inspect, debug, modify, review, explain, or search project code/files/logs/configuration.\n- Return false for simple conversation, general knowledge, or deterministic non-project requests that the main model or ordinary local tools can handle without architecture/code analysis, such as asking the current branch, current time, or a short clarification.\n- If unsure, return true.",
        query
    );
    let raw = call_architecture_model_with_system(
        &provider,
        "You are a cheap routing classifier for a coding agent. Only decide if a request needs project/codebase analysis. Return strict JSON only.",
        &prompt,
    )
    .await?;
    parse_project_analysis_decision(&raw)
}

fn build_architecture_prompt(
    request: &AnalyzeArchitectureRequest,
    prior_memories: &[crate::memory::ArchitectureMemory],
) -> String {
    let prior = if prior_memories.is_empty() {
        "No existing architecture memory matched.".to_string()
    } else {
        prior_memories
            .iter()
            .map(|m| {
                format!(
                    "Area: {}\nSummary: {}\nKey symbols: {}\nKey files: {}\nBoundaries: {}\nRisks: {}",
                    m.area,
                    m.summary,
                    m.key_symbols.join(", "),
                    m.key_files.join(", "),
                    m.boundaries,
                    m.risks
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    };
    let evidence = if request.evidence.is_empty() {
        "No additional code evidence was provided by the caller.".to_string()
    } else {
        request.evidence.join("\n\n---\n\n")
    };

    format!(
        "User task/query:\n{}\n\nFocus:\n{}\n\nExisting architecture memory:\n{}\n\nVerified code/index evidence:\n{}\n\nReturn only strict JSON with fields: area, summary, key_symbols, key_files, boundaries, common_tasks, risks, minimal_change_scope, confidence, symbol_contexts. symbol_contexts is optional and should contain objects with symbol_id, symbol_name, language, file_path, belongs_to_area, business_role, common_tasks, read_when, avoid_when, risks, confidence. Do not include markdown.",
        request.query, request.focus, prior, evidence
    )
}

async fn call_architecture_model(
    provider: &ArchitectureProvider,
    prompt: &str,
) -> anyhow::Result<String> {
    call_architecture_model_with_system(
        provider,
        "You are a cheap architecture-routing analyst for a coding agent. Your job is to map a user's task and verified code evidence to the smallest relevant feature/logic area. Do not invent files or symbols. Prefer concise, testable boundaries. Return strict JSON only.",
        prompt,
    )
    .await
}

async fn call_architecture_model_with_system(
    provider: &ArchitectureProvider,
    system_prompt: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let upstream_url = format!("{}/chat/completions", provider.url);
    let request_body = json!({
        "model": provider.model,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": prompt }
        ],
        "stream": false,
        "temperature": 0.1,
        "response_format": { "type": "json_object" }
    });

    let response = client
        .post(&upstream_url)
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .json(&request_body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Architecture agent API request failed (status {}): {}",
            status,
            body_text
        );
    }

    let response_json: Value = response.json().await?;
    let content = response_json
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("Architecture agent: missing content"))?;
    Ok(content.to_string())
}

fn parse_architecture_analysis(raw: &str) -> anyhow::Result<ArchitectureAnalysis> {
    let trimmed = raw.trim();
    let json_text = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    let normalized = normalize_architecture_analysis_json(serde_json::from_str(json_text)?)?;
    let analysis: ArchitectureAnalysis = serde_json::from_value(normalized)?;
    validate_architecture_analysis(&analysis)?;
    Ok(analysis)
}

fn parse_project_analysis_decision(raw: &str) -> anyhow::Result<ProjectAnalysisDecision> {
    let trimmed = raw.trim();
    let json_text = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    let mut decision: ProjectAnalysisDecision = serde_json::from_str(json_text)?;
    if decision.reason.trim().is_empty() {
        decision.reason = if decision.requires_project_analysis {
            "The request appears to require project/codebase analysis.".to_string()
        } else {
            "The request does not appear to require project/codebase analysis.".to_string()
        };
    }
    decision.confidence = decision.confidence.clamp(0.0, 1.0);
    Ok(decision)
}

fn normalize_architecture_analysis_json(mut value: Value) -> anyhow::Result<Value> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Architecture agent returned non-object JSON"))?;
    normalize_text_fields(
        object,
        &[
            "area",
            "summary",
            "boundaries",
            "risks",
            "minimal_change_scope",
        ],
    );
    normalize_string_array_fields(object, &["key_symbols", "key_files", "common_tasks"]);
    normalize_number_fields(object, &["confidence"]);

    if let Some(symbol_contexts) = object
        .get_mut("symbol_contexts")
        .and_then(|value| value.as_array_mut())
    {
        for context in symbol_contexts {
            if let Some(context_object) = context.as_object_mut() {
                normalize_text_fields(
                    context_object,
                    &[
                        "symbol_id",
                        "symbol_name",
                        "language",
                        "file_path",
                        "belongs_to_area",
                        "business_role",
                        "read_when",
                        "avoid_when",
                        "risks",
                    ],
                );
                normalize_string_array_fields(context_object, &["common_tasks"]);
                normalize_number_fields(context_object, &["confidence"]);
            }
        }
    }

    Ok(value)
}

fn normalize_string_array_fields(object: &mut serde_json::Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = object.get_mut(*field) {
            if let Some(items) = value_to_string_array(value) {
                *value = Value::Array(items.into_iter().map(Value::String).collect());
            }
        }
    }
}

fn normalize_number_fields(object: &mut serde_json::Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = object.get_mut(*field) {
            if let Some(number) = value_to_f64(value) {
                if let Some(json_number) = serde_json::Number::from_f64(number) {
                    *value = Value::Number(json_number);
                }
            }
        }
    }
}

fn normalize_text_fields(object: &mut serde_json::Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = object.get_mut(*field) {
            if let Some(text) = value_to_text(value) {
                *value = Value::String(text);
            }
        }
    }
}

fn value_to_string_array(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(text.clone()),
                    Value::Number(number) => Some(number.to_string()),
                    Value::Bool(flag) => Some(flag.to_string()),
                    Value::Object(map) => map
                        .get("text")
                        .or_else(|| map.get("content"))
                        .and_then(|value| value.as_str())
                        .map(ToOwned::to_owned),
                    _ => None,
                })
                .filter(|text| !text.trim().is_empty())
                .collect(),
        ),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Some(Vec::new())
            } else {
                Some(vec![trimmed.to_string()])
            }
        }
        Value::Number(number) => Some(vec![number.to_string()]),
        Value::Bool(flag) => Some(vec![flag.to_string()]),
        Value::Null => Some(Vec::new()),
        Value::Object(_) => None,
    }
}

fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "low" => Some(0.25),
            "medium" => Some(0.5),
            "high" => Some(0.75),
            other => other.parse::<f64>().ok(),
        },
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Null => Some(0.0),
        _ => None,
    }
}

fn value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(_) => None,
        Value::Array(items) => {
            let parts = items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(text.clone()),
                    Value::Number(number) => Some(number.to_string()),
                    Value::Bool(flag) => Some(flag.to_string()),
                    Value::Object(map) => map
                        .get("text")
                        .or_else(|| map.get("content"))
                        .and_then(|value| value.as_str())
                        .map(ToOwned::to_owned),
                    _ => None,
                })
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>();
            Some(parts.join("; "))
        }
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null => Some(String::new()),
        Value::Object(_) => None,
    }
}

fn validate_architecture_analysis(analysis: &ArchitectureAnalysis) -> anyhow::Result<()> {
    if analysis.area.trim().is_empty() {
        anyhow::bail!("Architecture agent returned empty area");
    }
    if analysis.summary.trim().is_empty() {
        anyhow::bail!("Architecture agent returned empty summary");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strict_json_analysis() {
        let raw = r#"{"area":"Responses format translation","summary":"Maps Responses input to chat messages.","key_symbols":["responses_body_to_openai_chat_messages"],"key_files":["src/format_translate/responses_chat.rs"],"boundaries":"Do not touch tool execution.","common_tasks":["转义功能"],"risks":"Role mapping regressions.","minimal_change_scope":"One module.","confidence":0.87,"symbol_contexts":[{"symbol_id":"rust:responses_body_to_openai_chat_messages","symbol_name":"responses_body_to_openai_chat_messages","language":"rust","file_path":"src/format_translate/responses_chat.rs","belongs_to_area":"Responses format translation","business_role":"Core conversion entry.","common_tasks":["转义功能"],"read_when":"Escaping issues","avoid_when":"Tool loop issues","risks":"Mapping regressions","confidence":0.8}]}"#;
        let parsed = parse_architecture_analysis(raw).unwrap();
        assert_eq!(parsed.area, "Responses format translation");
        assert_eq!(parsed.key_symbols.len(), 1);
        assert_eq!(parsed.symbol_contexts.len(), 1);
        assert_eq!(
            parsed.symbol_contexts[0].symbol_name,
            "responses_body_to_openai_chat_messages"
        );
    }

    #[test]
    fn parses_fenced_json_analysis() {
        let raw =
            "```json\n{\"area\":\"Agent Runtime\",\"summary\":\"Runs local tool loop.\"}\n```";
        let parsed = parse_architecture_analysis(raw).unwrap();
        assert_eq!(parsed.area, "Agent Runtime");
    }

    #[test]
    fn rejects_empty_area() {
        let raw = r#"{"area":"","summary":"x"}"#;
        assert!(parse_architecture_analysis(raw).is_err());
    }

    #[test]
    fn tolerates_array_values_for_text_fields() {
        let raw = r#"{"area":["Agent Runtime"],"summary":["Runs local tool loop."],"key_symbols":"run_agent_loop","key_files":null,"boundaries":["Do not touch MCP schemas."],"common_tasks":"工具循环","confidence":"medium","symbol_contexts":[{"symbol_id":["rust:run_agent_loop"],"symbol_name":["run_agent_loop"],"language":"rust","file_path":"src/agent_runtime.rs","belongs_to_area":["Agent Runtime"],"business_role":["Coordinates tools"],"common_tasks":"工具循环","read_when":["tool loop issues"],"avoid_when":["unrelated indexing"],"risks":["retry regressions"],"confidence":"0.7"}]}"#;
        let parsed = parse_architecture_analysis(raw).unwrap();
        assert_eq!(parsed.area, "Agent Runtime");
        assert_eq!(parsed.key_symbols, vec!["run_agent_loop"]);
        assert!(parsed.key_files.is_empty());
        assert_eq!(parsed.common_tasks, vec!["工具循环"]);
        assert_eq!(parsed.boundaries, "Do not touch MCP schemas.");
        assert_eq!(parsed.confidence, 0.5);
        assert_eq!(parsed.symbol_contexts[0].symbol_id, "rust:run_agent_loop");
        assert_eq!(parsed.symbol_contexts[0].common_tasks, vec!["工具循环"]);
        assert_eq!(parsed.symbol_contexts[0].read_when, "tool loop issues");
        assert_eq!(parsed.symbol_contexts[0].confidence, 0.7);
    }

    #[test]
    fn parses_project_analysis_decision() {
        let raw = r#"{"requires_project_analysis":false,"reason":"Simple branch request.","confidence":0.82}"#;
        let parsed = parse_project_analysis_decision(raw).unwrap();
        assert!(!parsed.requires_project_analysis);
        assert_eq!(parsed.reason, "Simple branch request.");
        assert_eq!(parsed.confidence, 0.82);
    }

    #[test]
    fn project_analysis_decision_defaults_reason_and_clamps_confidence() {
        let raw = "```json\n{\"requires_project_analysis\":true,\"confidence\":2.5}\n```";
        let parsed = parse_project_analysis_decision(raw).unwrap();
        assert!(parsed.requires_project_analysis);
        assert_eq!(parsed.confidence, 1.0);
        assert!(parsed.reason.contains("project/codebase analysis"));
    }

    #[test]
    fn resolves_architecture_model_through_model_map_when_present() {
        let provider = json!({
            "model_map": {
                "cheap-arch": "deepseek-v4-flash"
            }
        });
        assert_eq!(
            resolve_provider_model(&provider, "cheap-arch"),
            "deepseek-v4-flash"
        );
        assert_eq!(
            resolve_provider_model(&provider, "deepseek-v4-flash"),
            "deepseek-v4-flash"
        );
    }
}
