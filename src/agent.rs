use serde_json::{Value, json};

struct SubagentProvider {
    url: String,
    api_key: String,
    model: String,
}

fn get_subagent_provider() -> anyhow::Result<SubagentProvider> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let ai_config_path = exe_dir.join("ai_proxy_config.json");
    if !ai_config_path.exists() {
        anyhow::bail!("ai_proxy_config.json not found in exe dir");
    }
    let config_content = std::fs::read_to_string(&ai_config_path)?;
    let config: Value = serde_json::from_str(&config_content)?;

    let default_provider_name = config
        .get("default_provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing default_provider in config"))?;

    let provider_name = config
        .get("vision_provider")
        .and_then(|v| v.as_str())
        .unwrap_or(default_provider_name);

    let providers = config
        .get("providers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("missing providers in config"))?;

    let provider = providers
        .get(provider_name)
        .ok_or_else(|| anyhow::anyhow!("vision/default provider not found in providers"))?;

    if provider.get("supports_vision").and_then(|v| v.as_bool()) == Some(false) {
        anyhow::bail!(
            "provider '{}' is marked supports_vision=false; configure vision_provider with a multimodal provider",
            provider_name
        );
    }

    let url = provider
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing url in provider"))?
        .to_string();

    let api_key = provider
        .get("api_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing api_key in provider"))?
        .to_string();

    let model = resolve_provider_default_model(provider)?;

    Ok(SubagentProvider {
        url,
        api_key,
        model,
    })
}

fn resolve_provider_default_model(provider: &Value) -> anyhow::Result<String> {
    if let Some(model_map) = provider.get("model_map").and_then(|v| v.as_object()) {
        if let Some(first_upstream) = model_map.values().next().and_then(|v| v.as_str()) {
            return Ok(first_upstream.to_string());
        }
    }

    anyhow::bail!("vision provider model_map is empty")
}

pub async fn analyze_image_via_vision_agent(
    image_url: &str,
    focus_instruction: Option<&str>,
) -> anyhow::Result<String> {
    let provider_info = get_subagent_provider()?;

    let client = reqwest::Client::new();
    let upstream_url = format!("{}/chat/completions", provider_info.url);

    let system_prompt = "You are a highly precise visual analysis agent. \
                         Your task is to analyze the provided image in detail. \
                         If the image is a screenshot containing code, error messages, or logs, perform high-fidelity OCR and transcribe the text/code exactly. \
                         If it is a diagram or UI layout, describe the structure, elements, and labels clearly. \
                         Focus on technical details.";

    let mut text_instruction =
        "Analyze this image and describe/transcribe its contents in detail:".to_string();
    if let Some(focus) = focus_instruction {
        text_instruction = format!(
            "Re-examine this image based on user's feedback and focus on: {}",
            focus
        );
    }

    let messages = vec![
        json!({
            "role": "system",
            "content": system_prompt
        }),
        json!({
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": text_instruction
                },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": image_url
                    }
                }
            ]
        }),
    ];

    let request_body = json!({
        "model": provider_info.model,
        "messages": messages,
        "stream": false
    });

    let response = client
        .post(&upstream_url)
        .header("Authorization", format!("Bearer {}", provider_info.api_key))
        .json(&request_body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Vision agent API request failed (status {}): {}",
            status,
            body_text
        );
    }

    let response_json: Value = response.json().await?;
    let choice = response_json
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow::anyhow!("Vision agent: invalid response choices"))?;

    let message = choice
        .get("message")
        .ok_or_else(|| anyhow::anyhow!("Vision agent: missing message"))?;

    let content = message
        .get("content")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("Vision agent: missing content"))?;

    Ok(content.to_string())
}
