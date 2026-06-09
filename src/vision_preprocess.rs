use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use futures::FutureExt;
use serde_json::{json, Value};

static IMAGE_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
static IMAGE_REGISTRY: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

pub fn get_cached_description(hash: &str) -> Option<String> {
    IMAGE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(hash)
        .cloned()
}

pub fn insert_cached_description(hash: &str, description: &str) {
    IMAGE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(hash.to_string(), description.to_string());
}

pub fn get_registered_image(key: &str) -> Option<String> {
    IMAGE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(key)
        .cloned()
}

pub fn insert_registered_image(key: &str, raw_data: &str) {
    IMAGE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(key.to_string(), raw_data.to_string());
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

impl ImageProcessStats {
    fn has_activity(&self) -> bool {
        self.seen > 0 || self.analyzed > 0 || self.cache_hits > 0 || self.failed > 0
    }

    pub fn codex_prefix(&self) -> Option<String> {
        if !self.has_activity() {
            return None;
        }

        let mut parts = Vec::new();
        parts.push(format!("检测到 {} 张图片", self.seen));
        if self.analyzed > 0 {
            parts.push(format!("新分析 {} 张", self.analyzed));
        }
        if self.cache_hits > 0 {
            parts.push(format!("缓存复用 {} 张", self.cache_hits));
        }
        if self.failed > 0 {
            parts.push(format!("失败 {} 张", self.failed));
        }

        Some(format!(
            "🤖 **[AI Proxy: 已调用 mimo-v2.5 视觉子代理处理图片；{}。图片分析文本已合并至上下文]**\n\n",
            parts.join("，")
        ))
    }
}

pub fn process_and_replace_images<'a>(
    val: &'a mut Value,
    log: &'a Arc<Mutex<std::fs::File>>,
    db: Option<&'a Mutex<rusqlite::Connection>>,
    image_stats: &'a mut ImageProcessStats,
) -> futures::future::BoxFuture<'a, ()> {
    async move {
        match val {
            Value::Object(map) => {
                let mut found_image_url = None;
                if let Some(t) = map.get("type").and_then(|v| v.as_str()) {
                    if t == "image_url" || t == "input_image" {
                        if let Some(img_url) = map.get("image_url") {
                            if let Some(url_str) = img_url.as_str() {
                                found_image_url = Some(url_str.to_string());
                            } else if let Some(url_str) = img_url.get("url").and_then(|v| v.as_str()) {
                                found_image_url = Some(url_str.to_string());
                            }
                        } else if let Some(url) = map.get("url").and_then(|v| v.as_str()) {
                            found_image_url = Some(url.to_string());
                        }
                    } else if t == "image" {
                        if let Some(source) = map.get("source") {
                            if let (Some(media_type), Some(data)) = (
                                source.get("media_type").and_then(|v| v.as_str()),
                                source.get("data").and_then(|v| v.as_str()),
                            ) {
                                found_image_url = Some(format!("data:{};base64,{}", media_type, data));
                            }
                        }
                    }
                } else if map.contains_key("image_url") {
                    if let Some(img_url) = map.get("image_url") {
                        if let Some(url_str) = img_url.as_str() {
                            found_image_url = Some(url_str.to_string());
                        } else if let Some(url_str) = img_url.get("url").and_then(|v| v.as_str()) {
                            found_image_url = Some(url_str.to_string());
                        }
                    }
                }

                if let Some(url) = found_image_url {
                    image_stats.seen += 1;

                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    use std::hash::Hasher;
                    hasher.write(url.as_bytes());
                    let hash_val = hasher.finish();
                    let hash_str = format!("{:016x}", hash_val);
                    let image_key = format!("img_{}", &hash_str[..12]);

                    insert_registered_image(&image_key, &url);

                    let cached_desc = get_cached_description(&hash_str);

                    if let Some(description) = cached_desc {
                        image_stats.cache_hits += 1;
                        crate::ai_proxy::log_write(&**log, db, Some("VISION_CACHE_HIT"), Some("proxy"), &format!(">> Image hash {} found in in-memory cache. Using cached description.", hash_str));
                        *val = json!({
                            "type": "text",
                            "text": format!("\n[图像分析报告:\n{}\n]\n[图片 Key: {}]\n", description, image_key)
                        });
                        return;
                    }

                    crate::ai_proxy::log_write(&**log, db, Some("VISION_AGENT"), Some("proxy"), &format!(">> Image detected. Spawning vision agent to analyze... Hash: {}, Key: {}, URL length: {}", hash_str, image_key, url.len()));
                    match crate::agent::analyze_image_via_vision_agent(&url, None).await {
                        Ok(description) => {
                            crate::ai_proxy::log_write(&**log, db, Some("VISION_AGENT_SUCCESS"), Some("proxy"), &format!(">> Vision agent analysis complete. Description len: {}", description.len()));

                            insert_cached_description(&hash_str, &description);

                            image_stats.analyzed += 1;
                            *val = json!({
                                "type": "text",
                                "text": format!("\n[图像分析报告:\n{}\n]\n[图片 Key: {}]\n", description, image_key)
                            });
                            return;
                        }
                        Err(e) => {
                            image_stats.failed += 1;
                            crate::ai_proxy::log_write(&**log, db, Some("ERROR"), Some("proxy"), &format!("!! Vision agent analysis failed: {}", e));
                            *val = json!({
                                "type": "text",
                                "text": format!("\n[图像分析失败: 无法解析图片。错误: {}]\n[图片 Key: {}]\n", e, image_key)
                            });
                            return;
                        }
                    }
                }

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
