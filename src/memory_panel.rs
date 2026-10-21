//! What the agent remembers about one project, made visible: list, correct
//! or forget the work and architecture memories it recalls by search. Each
//! project keeps its memories in its own .codex-workspace-mcp store.
use crate::agent_service::AgentServiceState;
use anyhow::{Result, bail, ensure};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};

const LIMIT: i64 = 200;

fn table(kind: &str) -> Result<&'static str> {
    Ok(match kind {
        "work" => "memories",
        "architecture" => "architecture_memories",
        _ => bail!("未知的记忆类型"),
    })
}

fn list(conn: &Connection, project: &str) -> Result<Value> {
    let work = conn
        .prepare("SELECT id,time_unix,kind,summary,implementation,tests,risks,files_changed,applies_to FROM memories WHERE workspace_root=?2 ORDER BY time_unix DESC LIMIT ?1")?
        .query_map(params![LIMIT, project], |r| {
            let files: String = r.get(7)?;
            Ok(json!({"id":r.get::<_,i64>(0)?,"time":r.get::<_,i64>(1)?,"kind":r.get::<_,String>(2)?,"summary":r.get::<_,String>(3)?,
                "implementation":r.get::<_,String>(4)?,"tests":r.get::<_,String>(5)?,"risks":r.get::<_,String>(6)?,
                "files":serde_json::from_str::<Value>(&files).unwrap_or(json!([])),"applies_to":r.get::<_,String>(8)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let architecture = conn
        .prepare("SELECT id,updated_at_unix,area,summary,boundaries,risks,key_files FROM architecture_memories WHERE workspace_root=?2 ORDER BY updated_at_unix DESC LIMIT ?1")?
        .query_map(params![LIMIT, project], |r| {
            let files: String = r.get(6)?;
            Ok(json!({"id":r.get::<_,i64>(0)?,"time":r.get::<_,i64>(1)?,"area":r.get::<_,String>(2)?,"summary":r.get::<_,String>(3)?,
                "boundaries":r.get::<_,String>(4)?,"risks":r.get::<_,String>(5)?,"files":serde_json::from_str::<Value>(&files).unwrap_or(json!([]))}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"work":work,"architecture":architecture}))
}

fn update(conn: &Connection, project: &str, kind: &str, id: i64, summary: &str) -> Result<()> {
    let summary = summary.trim();
    ensure!(!summary.is_empty(), "内容不能为空");
    let changed = conn.execute(&format!("UPDATE {} SET summary=?2 WHERE id=?1 AND workspace_root=?3", table(kind)?), params![id, summary, project])?;
    ensure!(changed == 1, "这条记忆已不存在");
    Ok(())
}

fn forget(conn: &Connection, project: &str, kind: &str, id: i64) -> Result<()> {
    conn.execute(&format!("DELETE FROM {} WHERE id=?1 AND workspace_root=?2", table(kind)?), params![id, project])?;
    Ok(())
}

fn response(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(json!({"error":error.to_string()}))).into_response(),
    }
}

/// The project's memory store, after checking it is a registered project.
fn open_project(state: &AgentServiceState, workspace: &str) -> Result<(Connection, String)> {
    let project = crate::project_notes::registered(state, workspace)?;
    let root = project.display().to_string();
    Ok((crate::database::init_db(&project)?, root))
}

#[derive(Deserialize)]
pub struct ProjectQuery {
    workspace: String,
}
pub async fn get(State(state): State<AgentServiceState>, Query(query): Query<ProjectQuery>) -> Response {
    response(open_project(&state, &query.workspace).and_then(|(conn, project)| list(&conn, &project)))
}

#[derive(Deserialize)]
pub struct UpdateInput {
    workspace: String,
    summary: String,
}
pub async fn patch(
    State(state): State<AgentServiceState>,
    Path((kind, id)): Path<(String, i64)>,
    Json(input): Json<UpdateInput>,
) -> Response {
    response((|| {
        let (conn, project) = open_project(&state, &input.workspace)?;
        update(&conn, &project, &kind, id, &input.summary)?;
        list(&conn, &project)
    })())
}
pub async fn delete(
    State(state): State<AgentServiceState>,
    Path((kind, id)): Path<(String, i64)>,
    Query(query): Query<ProjectQuery>,
) -> Response {
    response((|| {
        let (conn, project) = open_project(&state, &query.workspace)?;
        forget(&conn, &project, &kind, id)?;
        list(&conn, &project)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memories_can_be_listed_corrected_and_forgotten() {
        let root = std::env::temp_dir().join(format!("memory-panel-{}", crate::agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let conn = crate::database::init_db(&root).unwrap();
        conn.execute("INSERT INTO memories(time_unix,workspace_root,summary,implementation,tests,risks,files_changed) VALUES (1,'r','打包用 musl','','','','[\"build.sh\"]'),(2,'other','别的项目','','','','[]')", []).unwrap();
        let listed = list(&conn, "r").unwrap();
        assert_eq!(listed["work"].as_array().unwrap().len(), 1, "another project's memory stays out");
        let id = listed["work"][0]["id"].as_i64().unwrap();
        assert_eq!(listed["work"][0]["files"][0], "build.sh");
        update(&conn, "r", "work", id, "打包用 musl 静态链接").unwrap();
        assert_eq!(list(&conn, "r").unwrap()["work"][0]["summary"], "打包用 musl 静态链接");
        assert!(update(&conn, "r", "work", id, " ").is_err());
        assert!(update(&conn, "r", "nope", id, "x").is_err());
        assert!(update(&conn, "other", "work", id, "x").is_err(), "ids are scoped to their project");
        forget(&conn, "other", "work", id).unwrap();
        assert_eq!(list(&conn, "r").unwrap()["work"].as_array().unwrap().len(), 1);
        forget(&conn, "r", "work", id).unwrap();
        assert!(list(&conn, "r").unwrap()["work"].as_array().unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
