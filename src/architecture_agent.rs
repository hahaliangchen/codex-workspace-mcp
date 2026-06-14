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
    let client = reqwest::Client::new();
    let upstream_url = format!("{}/chat/completions", provider.url);
    let system_prompt = "You are a cheap architecture-routing analyst for a coding agent. Your job is to map a user's task and verified code evidence to the smallest relevant feature/logic area. Do not invent files or symbols. Prefer concise, testable boundaries. Return strict JSON only.";
    let request_body = json!({
        "model": provider.model,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": prompt }
        ],
        "stream": false,
        "temperature": 0.1
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
    let analysis: ArchitectureAnalysis = serde_json::from_str(json_text)?;
    validate_architecture_analysis(&analysis)?;
    Ok(analysis)
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
