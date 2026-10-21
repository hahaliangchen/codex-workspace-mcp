//! User-owned project notes kept in the project's AGENTS.md. Unlike recalled
//! memory, they are standing instructions: every execution request carries
//! them, so compression or search misses can never drop them.
use crate::agent_service::{self, AgentServiceState};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const FILE: &str = "AGENTS.md";
const MAX_CHARS: usize = 16_000;
const HEADER: &str = "# 项目备注\n\n写给 AI 的长期说明，每次对话都会带上。\n\n";

/// The project directory of a code session; chat and plugin sessions have none.
pub fn task_project(root: &Path, task: &str) -> Result<Option<PathBuf>> {
    let conn = agent_service::open_db(root)?;
    let plugin: Option<String> = conn
        .query_row("SELECT plugin_id FROM plugin_sessions WHERE task_id=?1", [task], |r| r.get(0))
        .optional()
        .unwrap_or(None);
    let meta: Option<(Option<String>, String)> = conn
        .query_row(
            "SELECT workspace,kind FROM agent_session_meta WHERE task_id=?1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if plugin.is_some() || meta.as_ref().is_some_and(|(_, kind)| kind == "chat") {
        return Ok(None);
    }
    Ok(Some(meta.and_then(|(path, _)| path).map_or_else(|| root.to_path_buf(), PathBuf::from)))
}

pub fn read(project: &Path) -> Option<String> {
    let text = std::fs::read_to_string(project.join(FILE)).ok()?;
    (!text.trim().is_empty()).then_some(text)
}

/// The system message carrying the notes, or None when the project has none.
pub fn system_message(project: &Path) -> Option<Value> {
    let text = read(project)?;
    let mut body: String = text.chars().take(MAX_CHARS).collect();
    if body.len() < text.len() {
        body.push_str("\n…(truncated; read AGENTS.md for the rest)");
    }
    Some(json!({"role":"system","name":"project_notes","content":format!(
        "Project notes from {FILE}, written or approved by the user. Treat them as standing instructions for all work in this project unless the user overrides them in this conversation.\n\n{body}"
    )}))
}

/// Append one note as a list item, creating the file with a short header.
pub fn append(project: &Path, note: &str) -> Result<String> {
    let note = note.trim();
    ensure!(!note.is_empty(), "备注不能为空");
    let path = project.join(FILE);
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.trim().is_empty() {
        text = HEADER.to_owned();
    } else if !text.ends_with('\n') {
        text.push('\n');
    }
    let mut lines = note.lines();
    text.push_str(&format!("- {}\n", lines.next().unwrap_or_default()));
    for line in lines {
        text.push_str(&format!("  {line}\n"));
    }
    std::fs::write(&path, &text).with_context(|| format!("无法写入 {}", path.display()))?;
    Ok(text)
}

/// Only registered projects may be read or written through HTTP.
pub(crate) fn registered(state: &AgentServiceState, workspace: &str) -> Result<PathBuf> {
    let conn = agent_service::open_db(state.workspace.root())?;
    let known: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_projects WHERE path=?1)",
        [workspace],
        |r| r.get(0),
    )?;
    ensure!(known || Path::new(workspace) == state.workspace.root(), "项目不存在");
    Ok(PathBuf::from(workspace))
}

fn response(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(json!({"error":error.to_string()}))).into_response(),
    }
}

fn notes(project: &Path) -> Value {
    json!({"path":project.join(FILE),"content":std::fs::read_to_string(project.join(FILE)).unwrap_or_default()})
}

#[derive(Deserialize)]
pub struct NotesQuery {
    workspace: String,
}
pub async fn get(State(state): State<AgentServiceState>, Query(query): Query<NotesQuery>) -> Response {
    response(registered(&state, &query.workspace).map(|project| notes(&project)))
}

#[derive(Deserialize)]
pub struct NotesInput {
    workspace: String,
    content: String,
}
/// Replace the whole file with what the user edited.
pub async fn put(State(state): State<AgentServiceState>, Json(input): Json<NotesInput>) -> Response {
    response((|| {
        let project = registered(&state, &input.workspace)?;
        let path = project.join(FILE);
        std::fs::write(&path, &input.content).with_context(|| format!("无法写入 {}", path.display()))?;
        Ok(notes(&project))
    })())
}

#[derive(Deserialize)]
pub struct AppendInput {
    workspace: String,
    note: String,
}
/// Accept a note the agent proposed (possibly edited by the user).
pub async fn post(State(state): State<AgentServiceState>, Json(input): Json<AppendInput>) -> Response {
    response((|| {
        let project = registered(&state, &input.workspace)?;
        append(&project, &input.note)?;
        Ok(notes(&project))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_creates_header_and_indents_continuation_lines() {
        let dir = std::env::temp_dir().join(format!("notes-{}", agent_service::uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(system_message(&dir).is_none());
        append(&dir, "打包要打完整独立包\n依赖全部打进去").unwrap();
        let text = append(&dir, "部署前先跑测试").unwrap();
        assert!(text.starts_with("# 项目备注"));
        assert!(text.contains("- 打包要打完整独立包\n  依赖全部打进去\n- 部署前先跑测试\n"));
        let message = system_message(&dir).unwrap();
        assert_eq!(message["name"], "project_notes");
        assert!(message["content"].as_str().unwrap().contains("部署前先跑测试"));
        assert!(append(&dir, "  ").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
