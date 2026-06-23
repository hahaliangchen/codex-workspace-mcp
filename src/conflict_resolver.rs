use reqwest::Client;
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::expert_surgery::ExpertProvider;

pub async fn call_flash_merge(
    provider: &ExpertProvider,
    base: &str,
    theirs: &str,
    ours: &str,
) -> anyhow::Result<String> {
    let system_prompt = "You are a precise code conflict merger. \
You receive three blocks of code: BASE, THEIRS, and OURS. \
BASE is the original code block. \
THEIRS is the code block after applying the expert patch. \
OURS is the current code block on disk containing concurrent user changes. \
Merge the changes of THEIRS into OURS, resolving any conflicts logically, preserving the user's edits in OURS and applying the logic changes from THEIRS. \
Return ONLY the final merged code block for that symbol, with no explanations, no markdown block wrappers (do not use ```rust), and no prose.";

    let user_prompt = format!(
        "<<<<<<< BASE\n{}\n=======\nTHEIRS\n{}\n>>>>>>>\n\n<<<<<<< OURS\n{}\n=======\n",
        base, theirs, ours
    );

    let body = json!({
        "model": provider.model,
        "stream": false,
        "temperature": 0,
        "messages": [
            {
                "role": "system",
                "content": system_prompt
            },
            {
                "role": "user",
                "content": user_prompt
            }
        ]
    });

    let response = Client::new()
        .post(format!("{}/chat/completions", provider.url))
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "Flash merge model request failed: status={} body={}",
            status,
            text
        );
    }
    let value: Value = serde_json::from_str(&text)?;
    let merged = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Flash merge model response missing message content"))?;

    let mut cleaned = merged.trim();
    if cleaned.starts_with("```") {
        if let Some(first_newline) = cleaned.find('\n') {
            cleaned = &cleaned[first_newline..];
        }
        if cleaned.ends_with("```") {
            cleaned = &cleaned[..cleaned.len() - 3];
        }
    }
    Ok(cleaned.trim().to_string())
}

pub fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn normalize_with_map(s: &str) -> (String, Vec<(usize, usize)>) {
    let mut norm = String::new();
    let mut map = Vec::new();
    let mut chars = s.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if ch == '\r' {
            if let Some((next_idx, '\n')) = chars.peek() {
                norm.push('\n');
                map.push((idx, next_idx + 1));
                chars.next();
            } else {
                norm.push('\n');
                map.push((idx, idx + 1));
            }
        } else {
            norm.push(ch);
            map.push((idx, idx + ch.len_utf8()));
        }
    }
    (norm, map)
}

pub fn find_unique_normalized_match(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let needle_norm = normalize_newlines(needle);
    let needle_trimmed = needle_norm.trim();
    if needle_trimmed.is_empty() {
        return None;
    }

    let (haystack_norm, map) = normalize_with_map(haystack);
    let matches: Vec<_> = haystack_norm.match_indices(needle_trimmed).collect();

    if matches.len() == 1 {
        let (char_idx, matched_str) = matches[0];
        let start_char = char_idx;
        let end_char = char_idx + matched_str.chars().count();

        let start_byte = map[start_char].0;
        let end_byte = map[end_char - 1].1;
        Some((start_byte, end_byte))
    } else {
        None
    }
}

pub fn locate_drifted_symbol(
    current_disk_content: &str,
    symbol_name: &str,
    symbol_signature: &str,
    original_start: usize,
) -> (usize, usize) {
    let mut best_start = None;
    let mut min_distance = usize::MAX;

    let sig_norm = normalize_newlines(symbol_signature);
    let sig_trimmed = sig_norm.trim();
    if !sig_trimmed.is_empty() {
        for (idx, _) in current_disk_content.match_indices(sig_trimmed) {
            let dist = (idx as isize - original_start as isize).abs() as usize;
            if dist < min_distance {
                min_distance = dist;
                best_start = Some(idx);
            }
        }
    }

    if best_start.is_none() {
        min_distance = usize::MAX;
        for (idx, _) in current_disk_content.match_indices(symbol_name) {
            let dist = (idx as isize - original_start as isize).abs() as usize;
            if dist < min_distance {
                min_distance = dist;
                best_start = Some(idx);
            }
        }
    }

    let start = best_start.unwrap_or(original_start);

    let mut brace_count = 0;
    let mut found_open = false;
    let mut end = start;
    for (idx, ch) in current_disk_content[start..].char_indices() {
        if ch == '{' {
            brace_count += 1;
            found_open = true;
        } else if ch == '}' {
            brace_count -= 1;
        }
        if found_open && brace_count == 0 {
            end = start + idx + 1;
            break;
        }
    }
    if !found_open || end == start {
        end = (start + (current_disk_content.len() - start)).min(current_disk_content.len());
    }
    (start, end)
}

pub fn load_default_provider() -> anyhow::Result<ExpertProvider> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let config_path = exe_dir.join("ai_proxy_config.json");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;
    let default_provider_name = config
        .get("default_provider")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing default_provider in ai_proxy_config.json"))?;
    let provider = config
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get(default_provider_name))
        .ok_or_else(|| anyhow::anyhow!("default provider '{}' not found", default_provider_name))?;

    let model = config
        .get("orchestrator_model")
        .and_then(Value::as_str)
        .or_else(|| {
            provider
                .get("model_map")
                .and_then(Value::as_object)
                .and_then(|map| map.values().next())
                .and_then(Value::as_str)
        })
        .ok_or_else(|| anyhow::anyhow!("default provider has no model configured"))?
        .to_string();

    Ok(ExpertProvider {
        url: provider
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("default provider missing url"))?
            .trim_end_matches('/')
            .to_string(),
        api_key: provider
            .get("api_key")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("default provider missing api_key"))?
            .to_string(),
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_unique_normalized_match_handles_newlines() {
        let haystack = "line1\r\nfn target() {\r\n    println!(\"hi\");\r\n}\r\nline3";
        let needle = "fn target() {\n    println!(\"hi\");\n}";
        let res = find_unique_normalized_match(haystack, needle).unwrap();
        assert_eq!(
            &haystack[res.0..res.1],
            "fn target() {\r\n    println!(\"hi\");\r\n}"
        );
    }

    #[test]
    fn locates_drifted_symbol_brace_matching() {
        let content = "fn unrelated() {}\npub fn target() {\n    let x = 1;\n    if true {\n        println!(\"yes\");\n    }\n}\nfn other() {}";
        let (start, end) = locate_drifted_symbol(content, "target", "pub fn target()", 20);
        assert_eq!(
            &content[start..end],
            "pub fn target() {\n    let x = 1;\n    if true {\n        println!(\"yes\");\n    }\n}"
        );
    }

    #[test]
    fn test_substring_relocation_on_drift() {
        let original = "fn a() {}\nfn target() {\n    let x = 1;\n}\nfn b() {}";
        let current_disk = "fn a() {}\n// Some new comments\n// More comments\nfn target() {\n    let x = 1;\n}\nfn b() {}";
        let search = "fn target() {\n    let x = 1;\n}";

        let match_range = find_unique_normalized_match(current_disk, search).unwrap();
        assert_eq!(
            &current_disk[match_range.0..match_range.1],
            "fn target() {\n    let x = 1;\n}"
        );
    }
}
