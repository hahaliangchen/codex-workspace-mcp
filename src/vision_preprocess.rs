use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use futures::FutureExt;
use serde_json::{Value, json};

static IMAGE_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
static CURRENT_VISIBLE_IMAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

pub fn get_cached_description(hash: &str) -> Option<String> {
    IMAGE_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(hash)
        .cloned()
}

pub fn insert_cached_description(hash: &str, description: &str) {
    IMAGE_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(hash.to_string(), description.to_string());
}

pub fn has_image_input(val: &Value) -> bool {
    match val {
        Value::Object(map) => {
            if let Some(t) = map.get("type").and_then(|v| v.as_str()) {
                if t == "image_url" || t == "input_image" || t == "image" {
                    return true;
                }
            }
            if map.contains_key("image_url") {
                return true;
            }
            for v in map.values() {
                if has_image_input(v) {
                    return true;
                }
            }
            false
        }
        Value::Array(arr) => {
            for v in arr {
                if has_image_input(v) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn extract_image_url(val: &Value) -> Option<String> {
    let map = val.as_object()?;

    if let Some(t) = map.get("type").and_then(|v| v.as_str()) {
        if t == "image_url" || t == "input_image" {
            if let Some(img_url) = map.get("image_url") {
                if let Some(url_str) = img_url.as_str() {
                    return Some(url_str.to_string());
                }
                if let Some(url_str) = img_url.get("url").and_then(|v| v.as_str()) {
                    return Some(url_str.to_string());
                }
            }
            if let Some(url) = map.get("url").and_then(|v| v.as_str()) {
                return Some(url.to_string());
            }
        } else if t == "image" {
            if let Some(source) = map.get("source") {
                if let (Some(media_type), Some(data)) = (
                    source.get("media_type").and_then(|v| v.as_str()),
                    source.get("data").and_then(|v| v.as_str()),
                ) {
                    return Some(format!("data:{};base64,{}", media_type, data));
                }
            }
        }
    } else if let Some(img_url) = map.get("image_url") {
        if let Some(url_str) = img_url.as_str() {
            return Some(url_str.to_string());
        }
        if let Some(url_str) = img_url.get("url").and_then(|v| v.as_str()) {
            return Some(url_str.to_string());
        }
    }

    None
}

fn collect_image_urls(val: &Value, out: &mut Vec<String>) {
    if let Some(url) = extract_image_url(val) {
        out.push(url);
        return;
    }

    match val {
        Value::Object(map) => {
            for v in map.values() {
                collect_image_urls(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_image_urls(v, out);
            }
        }
        _ => {}
    }
}

pub fn set_visible_images_from_body(body: &Value) -> usize {
    let mut images = Vec::new();
    collect_image_urls(body, &mut images);
    let count = images.len();
    *CURRENT_VISIBLE_IMAGES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap() = images;
    count
}

pub fn resolve_visible_image_ref(image_ref: Option<&str>) -> Option<String> {
    let images = CURRENT_VISIBLE_IMAGES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap();
    if images.is_empty() {
        return None;
    }

    let Some(image_ref) = image_ref.map(str::trim).filter(|s| !s.is_empty()) else {
        return images.last().cloned();
    };

    if image_ref.eq_ignore_ascii_case("latest") || image_ref == "最近" {
        return images.last().cloned();
    }

    let numeric = image_ref
        .strip_prefix("image_")
        .or_else(|| image_ref.strip_prefix("img_"))
        .unwrap_or(image_ref);
    if let Ok(index) = numeric.parse::<usize>() {
        if (1..=images.len()).contains(&index) {
            return images.get(index - 1).cloned();
        }
    }

    None
}

pub fn has_latest_user_image_input(body: &Value) -> bool {
    if let Some(input_arr) = body.get("input").and_then(|v| v.as_array()) {
        return input_arr
            .iter()
            .rev()
            .find(|item| item.get("role").and_then(|v| v.as_str()) == Some("user"))
            .map(has_image_input)
            .unwrap_or(false);
    }

    if let Some(messages_arr) = body.get("messages").and_then(|v| v.as_array()) {
        return messages_arr
            .iter()
            .rev()
            .find(|item| item.get("role").and_then(|v| v.as_str()) == Some("user"))
            .map(has_image_input)
            .unwrap_or(false);
    }

    has_image_input(body)
}

pub fn adjust_model_for_vision(upstream_model: &str) -> String {
    if upstream_model.starts_with("mimo-v2.5-") {
        return "mimo-v2.5".to_string();
    }
    upstream_model.to_string()
}

#[derive(Debug, Default, Clone)]
pub struct ImageProcessStats {
    pub seen: usize,
    pub analyzed: usize,
    pub cache_hits: usize,
    pub failed: usize,
}

pub fn process_and_replace_images<'a>(
    val: &'a mut Value,
    log: &'a Arc<Mutex<std::fs::File>>,
    db: Option<&'a Mutex<rusqlite::Connection>>,
    image_stats: &'a mut ImageProcessStats,
) -> futures::future::BoxFuture<'a, ()> {
    async move {
        if let Some(url) = extract_image_url(val) {
            image_stats.seen += 1;
            *val = analyze_image_node(url, log, db, image_stats).await;
            return;
        }

        match val {
            Value::Object(map) => {
                for v in map.values_mut() {
                    process_and_replace_images(v, log, db, image_stats).await;
                }
            }
            Value::Array(arr) => {
                for v in arr {
                    process_and_replace_images(v, log, db, image_stats).await;
                }
            }
            _ => {}
        }
    }
    .boxed()
}

async fn analyze_image_node(
    url: String,
    log: &Arc<Mutex<std::fs::File>>,
    db: Option<&Mutex<rusqlite::Connection>>,
    image_stats: &mut ImageProcessStats,
) -> Value {
    let hash_str = image_hash(&url);
    if let Some(description) = get_cached_description(&hash_str) {
        image_stats.cache_hits += 1;
        log_image_cache_hit(log, db, &hash_str);
        return image_report_value(&description);
    }

    log_image_analysis_start(log, db, &hash_str, url.len());
    match crate::agent::analyze_image_via_vision_agent(&url, None).await {
        Ok(description) => {
            insert_cached_description(&hash_str, &description);
            image_stats.analyzed += 1;
            log_image_analysis_success(log, db, description.len());
            image_report_value(&description)
        }
        Err(e) => {
            image_stats.failed += 1;
            log_image_analysis_failure(log, db, &e.to_string());
            image_failure_value(&e.to_string())
        }
    }
}

fn image_hash(url: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::Hasher;
    hasher.write(url.as_bytes());
    format!("{:016x}", hasher.finish())
}

fn image_report_value(description: &str) -> Value {
    json!({
        "type": "text",
        "text": format!("\n[图像分析报告:\n{}\n]\n[说明: 原始图片不会持久保存；如果用户明确要求重新检查图片细节，请调用 analyze_image。若当前上下文已无原图，请让用户重新上传。]\n", description)
    })
}

fn image_failure_value(error: &str) -> Value {
    json!({
        "type": "text",
        "text": format!("\n[图像分析失败: 配置的视觉子代理无法解析图片。不要改用 shell/rg/Get-Content 读取图片内容；如果用户要求重试，请调用 analyze_image。若当前上下文已无原图，请让用户重新上传。错误: {}]\n", error)
    })
}

fn log_image_cache_hit(
    log: &Arc<Mutex<std::fs::File>>,
    db: Option<&Mutex<rusqlite::Connection>>,
    hash: &str,
) {
    crate::ai_proxy::log_write(
        &**log,
        db,
        Some("VISION_CACHE_HIT"),
        Some("proxy"),
        &format!(
            ">> Image hash {} found in in-memory cache. Using cached description.",
            hash
        ),
    );
}

fn log_image_analysis_start(
    log: &Arc<Mutex<std::fs::File>>,
    db: Option<&Mutex<rusqlite::Connection>>,
    hash: &str,
    url_len: usize,
) {
    crate::ai_proxy::log_write(
        &**log,
        db,
        Some("VISION_AGENT"),
        Some("proxy"),
        &format!(
            ">> Image detected. Spawning vision agent to analyze... Hash: {}, URL length: {}",
            hash, url_len
        ),
    );
}

fn log_image_analysis_success(
    log: &Arc<Mutex<std::fs::File>>,
    db: Option<&Mutex<rusqlite::Connection>>,
    description_len: usize,
) {
    crate::ai_proxy::log_write(
        &**log,
        db,
        Some("VISION_AGENT_SUCCESS"),
        Some("proxy"),
        &format!(
            ">> Vision agent analysis complete. Description len: {}",
            description_len
        ),
    );
}

fn log_image_analysis_failure(
    log: &Arc<Mutex<std::fs::File>>,
    db: Option<&Mutex<rusqlite::Connection>>,
    error: &str,
) {
    crate::ai_proxy::log_write(
        &**log,
        db,
        Some("ERROR"),
        Some("proxy"),
        &format!("!! Vision agent analysis failed: {}", error),
    );
}

pub fn process_latest_user_images<'a>(
    body: &'a mut Value,
    log: &'a Arc<Mutex<std::fs::File>>,
    db: Option<&'a Mutex<rusqlite::Connection>>,
    image_stats: &'a mut ImageProcessStats,
) -> futures::future::BoxFuture<'a, ()> {
    async move {
        if let Some(input_arr) = body.get_mut("input").and_then(|v| v.as_array_mut()) {
            if let Some(item) = input_arr
                .iter_mut()
                .rev()
                .find(|item| item.get("role").and_then(|v| v.as_str()) == Some("user"))
            {
                process_and_replace_images(item, log, db, image_stats).await;
            }
            return;
        }

        if let Some(messages_arr) = body.get_mut("messages").and_then(|v| v.as_array_mut()) {
            if let Some(item) = messages_arr
                .iter_mut()
                .rev()
                .find(|item| item.get("role").and_then(|v| v.as_str()) == Some("user"))
            {
                process_and_replace_images(item, log, db, image_stats).await;
            }
            return;
        }

        process_and_replace_images(body, log, db, image_stats).await;
    }
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn latest_user_image_detection_ignores_old_responses_history() {
        let body = json!({
            "input": [
                {
                    "role": "user",
                    "content": [
                        { "type": "input_text", "text": "old image" },
                        { "type": "input_image", "image_url": "data:image/png;base64,old" }
                    ]
                },
                {
                    "role": "assistant",
                    "content": "[图像分析报告: old]\n[图片 Key: img_old]"
                },
                {
                    "role": "user",
                    "content": "这次只问代码，不问图片"
                }
            ]
        });

        assert!(!has_latest_user_image_input(&body));
    }

    #[test]
    fn latest_user_image_detection_checks_chat_messages() {
        let body = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "old image" },
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,old" } }
                    ]
                },
                {
                    "role": "assistant",
                    "content": "[图像分析报告: old]\n[图片 Key: img_old]"
                },
                {
                    "role": "user",
                    "content": "这次只问日志，不问图片"
                }
            ]
        });

        assert!(!has_latest_user_image_input(&body));
    }

    #[test]
    fn latest_user_image_detection_accepts_current_chat_image() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": "ready"
                },
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "看这张图" },
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,current" } }
                    ]
                }
            ]
        });

        assert!(has_latest_user_image_input(&body));
    }

    #[test]
    fn visible_image_refs_are_request_scoped() {
        let body = json!({
            "input": [
                {
                    "role": "user",
                    "content": [
                        { "type": "input_image", "image_url": "data:image/png;base64,one" },
                        { "type": "input_image", "image_url": "data:image/png;base64,two" }
                    ]
                }
            ]
        });

        assert_eq!(set_visible_images_from_body(&body), 2);
        assert_eq!(
            resolve_visible_image_ref(None).as_deref(),
            Some("data:image/png;base64,two")
        );
        assert_eq!(
            resolve_visible_image_ref(Some("latest")).as_deref(),
            Some("data:image/png;base64,two")
        );
        assert_eq!(
            resolve_visible_image_ref(Some("1")).as_deref(),
            Some("data:image/png;base64,one")
        );
        assert_eq!(
            resolve_visible_image_ref(Some("img_2")).as_deref(),
            Some("data:image/png;base64,two")
        );

        set_visible_images_from_body(&json!({"input": [{"role": "user", "content": "no image"}]}));
        assert!(resolve_visible_image_ref(None).is_none());
    }
}
