//! Project skills distilled from a selected stretch of the task flow. They live
//! in the project at .agents/skills/<name>/SKILL.md, travel with git like
//! AGENTS.md, and are advertised to every code session of that project.
use crate::agent_service::AgentServiceState;
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path as FsPath, PathBuf};
use tokio_util::sync::CancellationToken;

pub const DIR: &str = ".agents/skills";
const MAX_SKILL_CHARS: usize = 64_000;
const MAX_TOOL_CHARS: usize = 1_500;
const MAX_STEPS_CHARS: usize = 60_000;

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectSkill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// The `name:` and `description:` lines of a SKILL.md front matter.
fn front_matter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    for line in lines.take_while(|line| line.trim() != "---") {
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(value.trim().trim_matches('"').to_owned());
        } else if let Some(value) = line.strip_prefix("description:") {
            description = Some(value.trim().trim_matches('"').to_owned());
        }
    }
    (name, description)
}

pub fn list(project: &FsPath) -> Vec<ProjectSkill> {
    let Ok(entries) = std::fs::read_dir(project.join(DIR)) else {
        return Vec::new();
    };
    let mut skills = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path().join("SKILL.md");
            let text = std::fs::read_to_string(&path).ok()?;
            let (name, description) = front_matter(&text);
            let name = name.filter(|name| *name == dir && valid_name(name))?;
            Some(ProjectSkill {
                name,
                description: description.unwrap_or_default(),
                path,
            })
        })
        .collect::<Vec<_>>();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// read_skill's result for a project skill. Project skills carry instructions only, no tools.
pub fn read(project: &FsPath, name: &str) -> Option<Result<Value>> {
    let skill = list(project).into_iter().find(|skill| skill.name == name)?;
    Some((|| {
        let content = std::fs::read_to_string(&skill.path)?;
        ensure!(content.len() <= MAX_SKILL_CHARS, "skill body too large");
        Ok(
            json!({"skill":name,"content":content,"source":skill.path,"scope":"project","tools":[],
            "note":"Project skill recorded by the user from an earlier task flow. Follow its steps for matching work, adapting parameters to the current request."}),
        )
    })())
}

/// Advertises the project's skills on every request so matching work loads them.
pub fn system_message(project: &FsPath) -> Option<Value> {
    let skills = list(project);
    if skills.is_empty() {
        return None;
    }
    let lines = skills
        .iter()
        .map(|skill| format!("- {}: {}", skill.name, skill.description))
        .collect::<Vec<_>>()
        .join("\n");
    Some(
        json!({"role":"system","name":"project_skills","content":format!(
            "Project skills saved by the user from earlier work. Before starting work that matches one, call read_skill with its name and follow its procedure.\n{lines}"
        )}),
    )
}

fn render(name: &str, description: &str, body: &str) -> String {
    let description = description.replace(['\n', '\r'], " ");
    format!(
        "---\nname: {name}\ndescription: {}\n---\n\n{}\n",
        description.trim(),
        body.trim()
    )
}

pub fn save(
    project: &FsPath,
    name: &str,
    description: &str,
    body: &str,
    overwrite: bool,
) -> Result<PathBuf> {
    ensure!(valid_name(name), "技能名称只能用小写英文字母、数字和连字符");
    ensure!(!description.trim().is_empty(), "请填写适用场景");
    ensure!(!body.trim().is_empty(), "技能步骤不能为空");
    let dir = project.join(DIR).join(name);
    let path = dir.join("SKILL.md");
    ensure!(
        overwrite || !path.exists(),
        "SKILL_EXISTS: 已有同名技能 {name}"
    );
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, render(name, description, body))
        .with_context(|| format!("无法写入 {}", path.display()))?;
    Ok(path)
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…(已截断)")
}

#[derive(Deserialize)]
pub struct DraftTool {
    name: String,
    #[serde(default)]
    input: String,
    #[serde(default)]
    output: String,
    #[serde(default)]
    failed: bool,
}
#[derive(Deserialize)]
pub struct DraftStep {
    title: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tools: Vec<DraftTool>,
}
#[derive(Deserialize)]
pub struct DraftInput {
    #[serde(default)]
    goal: String,
    steps: Vec<DraftStep>,
}

/// The selected steps as compact text for the distilling model.
fn transcript(input: &DraftInput) -> Result<String> {
    ensure!(!input.steps.is_empty(), "请先选择流程步骤");
    let mut out = String::new();
    if !input.goal.trim().is_empty() {
        out.push_str(&format!(
            "用户当时的请求：\n{}\n\n",
            clip(input.goal.trim(), 4_000)
        ));
    }
    for (index, step) in input.steps.iter().enumerate() {
        out.push_str(&format!(
            "## 步骤 {}：{}（{}）\n",
            index + 1,
            step.title,
            step.status
        ));
        if !step.description.trim().is_empty() {
            out.push_str(&format!(
                "{}\n",
                clip(step.description.trim(), MAX_TOOL_CHARS)
            ));
        }
        for tool in &step.tools {
            out.push_str(&format!(
                "- 工具 {}{}\n  输入：{}\n  输出：{}\n",
                tool.name,
                if tool.failed { "（失败）" } else { "" },
                clip(tool.input.trim(), MAX_TOOL_CHARS),
                clip(tool.output.trim(), MAX_TOOL_CHARS)
            ));
        }
        out.push('\n');
    }
    Ok(clip(&out, MAX_STEPS_CHARS))
}

const DISTILL_PROMPT: &str = r#"你要把一段已经执行过的 agent 工作流程，提炼成以后可复用的技能（SKILL.md）。
要求：
1. 写成通用流程，不是这一次的回放。每次会变的值（服务器地址、版本号、路径、分支名等）抽成参数，在正文开头的「参数」里列出，并说明从哪里获得。
2. 步骤按顺序编号，每步写清楚做什么、用什么命令或工具、怎样确认这步成功。
3. 流程中失败、返工或绕路的地方，写进「注意事项」：不要做什么、为什么、应该怎么做。只记录从材料中能确认的教训。
4. 不要编造材料里没有的命令或事实；不确定的地方写成需要确认的问题。
5. 正文用用户使用的语言，Markdown 格式，简洁。
只输出一个 JSON 对象，不要输出其他文字：
{"name":"小写英文加连字符的技能名，如 deploy-server","description":"一句话：什么时候使用这个技能（用用户的语言，包含触发关键词）","body":"SKILL.md 正文 Markdown，不含 front matter"}"#;

fn parse_draft(text: &str) -> Result<Value> {
    let start = text.find('{').context("模型没有返回技能草稿")?;
    let end = text.rfind('}').context("模型没有返回技能草稿")?;
    let value: Value = serde_json::from_str(&text[start..=end]).context("技能草稿格式不正确")?;
    let field = |key: &str| {
        value[key]
            .as_str()
            .map(str::trim)
            .unwrap_or_default()
            .to_owned()
    };
    let mut name = field("name").to_ascii_lowercase().replace([' ', '_'], "-");
    name.retain(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let name = name.trim_matches('-').to_owned();
    let body = field("body");
    if body.is_empty() {
        bail!("模型返回的技能正文为空");
    }
    Ok(
        json!({"name": if valid_name(&name) { name } else { "saved-flow".into() },
        "description": field("description"), "body": body}),
    )
}

async fn distill(state: &AgentServiceState, task: &str, steps: String) -> Result<Value> {
    ensure!(!state.provider_url.is_empty(), "模型服务未配置");
    let model = &state.default_model;
    let body = json!({"model":model,"stream":true,"max_tokens":8192,"messages":[
        {"role":"system","content":DISTILL_PROMPT},
        {"role":"user","content":steps}]});
    let request_id = crate::agent_service::uuid_like();
    let log = crate::model_transport::ModelRequestLog::new(
        state.workspace.root(),
        json!({"actor":"skill_distiller","request_id":request_id,"task_id":task,
            "provider":state.provider_name,"model":model}),
        &state.provider_url,
        &body,
    );
    log.sending();
    let pending = async {
        let response = state
            .model_client
            .post(format!(
                "{}/chat/completions",
                state.provider_url.trim_end_matches('/')
            ))
            .bearer_auth(&state.api_key)
            .header("x-client-request-id", request_id.as_str())
            .json(&body)
            .send()
            .await?;
        log.headers(&response);
        let response = crate::visual_probe::successful_response(response).await?;
        let completed = crate::model_transport::consume_response(response, &log).await?;
        let text = completed.message["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        parse_draft(&text)
    };
    let result = log
        .watch(
            pending,
            &CancellationToken::new(),
            crate::model_transport::TimeoutPolicy::default(),
        )
        .await;
    log.finish(
        if result.is_ok() {
            "completed"
        } else {
            "failed"
        },
        result.as_ref().err(),
    );
    result
}

fn response(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}

/// Distill the selected flow steps into an editable draft; nothing is saved.
pub async fn draft(
    State(state): State<AgentServiceState>,
    Path(task): Path<String>,
    Json(input): Json<DraftInput>,
) -> Response {
    let state = state.with_latest_config().await;
    let result = async {
        let steps = transcript(&input)?;
        distill(&state, &task, steps).await
    }
    .await;
    response(result)
}

#[derive(Deserialize)]
pub struct SaveInput {
    name: String,
    description: String,
    body: String,
    #[serde(default)]
    overwrite: bool,
}
/// Save a reviewed draft into the session's project.
pub async fn save_route(
    State(state): State<AgentServiceState>,
    Path(task): Path<String>,
    Json(input): Json<SaveInput>,
) -> Response {
    response((|| {
        let project = crate::project_notes::task_project(state.workspace.root(), &task)?
            .context("只有代码会话可以保存项目技能")?;
        let path = save(
            &project,
            input.name.trim(),
            &input.description,
            &input.body,
            input.overwrite,
        )?;
        Ok(json!({"name":input.name.trim(),"path":path}))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_skills_are_listed_read_and_advertised() {
        let dir =
            std::env::temp_dir().join(format!("flow-skills-{}", crate::agent_service::uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(system_message(&dir).is_none());
        assert!(save(&dir, "Deploy Server", "部署时", "步骤", false).is_err());
        save(
            &dir,
            "deploy-server",
            "部署到服务器时使用\n第二行",
            "1. 打包\n2. 上传",
            false,
        )
        .unwrap();
        assert!(
            save(&dir, "deploy-server", "x", "y", false)
                .unwrap_err()
                .to_string()
                .contains("SKILL_EXISTS")
        );
        save(
            &dir,
            "deploy-server",
            "部署到服务器时使用",
            "1. 打完整包\n2. 上传",
            true,
        )
        .unwrap();
        let skills = list(&dir);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "部署到服务器时使用");
        let content = read(&dir, "deploy-server").unwrap().unwrap();
        assert!(content["content"].as_str().unwrap().contains("打完整包"));
        assert!(read(&dir, "missing").is_none());
        let hint = system_message(&dir).unwrap();
        assert!(
            hint["content"]
                .as_str()
                .unwrap()
                .contains("- deploy-server: 部署到服务器时使用")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn drafts_are_parsed_and_names_normalized() {
        let draft = parse_draft("```json\n{\"name\":\"Deploy_Server\",\"description\":\"部署时\",\"body\":\"## 步骤\"}\n```").unwrap();
        assert_eq!(draft["name"], "deploy-server");
        let draft =
            parse_draft("{\"name\":\"部署\",\"description\":\"d\",\"body\":\"b\"}").unwrap();
        assert_eq!(draft["name"], "saved-flow");
        assert!(parse_draft("{\"name\":\"x\",\"body\":\"\"}").is_err());
        let input = DraftInput {
            goal: "部署".into(),
            steps: vec![DraftStep {
                title: "上传".into(),
                status: "failed".into(),
                description: String::new(),
                tools: vec![DraftTool {
                    name: "run_program".into(),
                    input: "scp".into(),
                    output: "x".repeat(5000),
                    failed: true,
                }],
            }],
        };
        let text = transcript(&input).unwrap();
        assert!(text.contains("工具 run_program（失败）") && text.contains("已截断"));
    }
}
