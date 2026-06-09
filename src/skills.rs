use anyhow::Result;
use std::sync::{OnceLock, RwLock};

/// 技能的元数据（轻量索引，不含 SKILL.md 全文）
#[derive(Clone, Debug, serde::Serialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub file_path: String,
}

/// 全局技能注册表，由 ai_proxy 在解析 System Prompt 时填充
static SKILLS_REGISTRY: OnceLock<RwLock<Vec<SkillMeta>>> = OnceLock::new();

fn registry() -> &'static RwLock<Vec<SkillMeta>> {
    SKILLS_REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// 从 Codex 原始的 <skills_instructions> 块中解析并注册所有技能
/// 在代理层处理每次请求时调用（首次调用后即固定不变）
pub fn register_skills_from_prompt(skills_text: &str) {
    // 如果已经注册过了，跳过（技能列表在同一次启动中是固定的）
    {
        let r = registry().read().unwrap();
        if !r.is_empty() {
            return;
        }
    }

    let mut skills = Vec::new();

    // 解析格式：`- skill_name: description (file: /path/to/SKILL.md)`
    for line in skills_text.lines() {
        let line = line.trim();
        if !line.starts_with("- ") {
            continue;
        }
        let content = &line[2..]; // 去掉 "- "

        // 分割 name: description (file: path)
        if let Some(colon_pos) = content.find(": ") {
            let name = content[..colon_pos].trim().to_string();
            let rest = &content[colon_pos + 2..];

            // 提取 file 路径
            let (description, file_path) = if let Some(file_idx) = rest.rfind(" (file: ") {
                let desc = rest[..file_idx].trim().to_string();
                let file_end = rest.rfind(')').unwrap_or(rest.len());
                let path = rest["(file: ".len() + file_idx..file_end]
                    .trim()
                    .to_string();
                (desc, path)
            } else {
                (rest.to_string(), String::new())
            };

            if !name.is_empty() {
                skills.push(SkillMeta {
                    name,
                    description,
                    file_path,
                });
            }
        }
    }

    if !skills.is_empty() {
        let mut w = registry().write().unwrap();
        *w = skills;
    }
}

/// 返回所有已注册技能的元数据列表（给 list_skills 工具调用）
pub fn list_skills() -> Vec<SkillMeta> {
    registry().read().unwrap().clone()
}

/// 根据技能名读取完整 SKILL.md 内容（给 read_skill 工具调用）
pub fn read_skill(name: &str) -> Result<String> {
    let registry = registry().read().unwrap();

    // 支持精确匹配和去前缀匹配（如 "documents:documents" -> "documents"）
    let meta = registry.iter().find(|s| {
        s.name == name
            || s.name.split(':').last() == Some(name)
            || s.name.to_lowercase() == name.to_lowercase()
    });

    let meta = meta.ok_or_else(|| {
        anyhow::anyhow!(
            "Skill '{}' not found. Call list_skills to see available skills.",
            name
        )
    })?;

    if meta.file_path.is_empty() {
        anyhow::bail!("Skill '{}' has no file path recorded.", name);
    }

    let content = std::fs::read_to_string(&meta.file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read SKILL.md at '{}': {}", meta.file_path, e))?;

    Ok(content)
}
