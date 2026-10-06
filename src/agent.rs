use serde_json::{Value, json};

struct SubagentProvider {
    url: String,
    api_key: String,
    model: String,
}

fn get_subagent_provider() -> anyhow::Result<SubagentProvider> {
    let route = crate::ai_proxy::selected_route(false)?;
    Ok(SubagentProvider { url: route.url, api_key: route.api_key, model: route.model })
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
