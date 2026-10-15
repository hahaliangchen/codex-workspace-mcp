//! Mechanical access to the original conversation records. No task selection,
//! summarization, role decisions, or message rewriting belongs in this layer.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params, params_from_iter, types::Value as SqlValue};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Default)]
pub struct EventQuery<'a> {
    pub after: Option<i64>,
    pub through: Option<i64>,
    pub exact: Option<i64>,
    pub turn: Option<usize>,
    pub kinds: &'a [&'a str],
    pub call_id: Option<&'a str>,
    pub newest: Option<usize>,
}

pub struct StoredEvent {
    pub seq: i64,
    pub time: i64,
    pub kind: String,
    pub raw: String,
    pub surface_op: Option<String>,
}
impl StoredEvent {
    pub fn data(&self) -> Result<Value> {
        Ok(serde_json::from_str(&self.raw)?)
    }
}

/// All selections are explicit caller parameters. Results are always ascending
/// by the persisted sequence, including a request for the newest N records.
pub fn load_events(
    conn: &Connection,
    task: &str,
    query: &EventQuery<'_>,
) -> Result<Vec<StoredEvent>> {
    let mut sql =
        "SELECT seq,timestamp,kind,data,surface_op FROM agent_task_events WHERE task_id=?1"
            .to_owned();
    let mut args = vec![SqlValue::Text(task.to_owned())];
    for (value, expression) in [
        (query.after, "seq>"),
        (query.through, "seq<="),
        (query.exact, "seq="),
    ] {
        if let Some(value) = value {
            args.push(SqlValue::Integer(value));
            sql.push_str(&format!(" AND {expression}?{}", args.len()));
        }
    }
    if !query.kinds.is_empty() {
        let placeholders = query
            .kinds
            .iter()
            .map(|kind| {
                args.push(SqlValue::Text((*kind).to_owned()));
                format!("?{}", args.len())
            })
            .collect::<Vec<_>>();
        sql.push_str(&format!(" AND kind IN ({})", placeholders.join(",")));
    }
    if let Some(id) = query.call_id {
        args.push(SqlValue::Text(id.to_owned()));
        sql.push_str(&format!(" AND COALESCE(json_extract(data,'$.callId'),json_extract(data,'$.message.toolCallId'),json_extract(data,'$.message.source.callId'))=?{}", args.len()));
    }
    if let Some(turn) = query.turn {
        args.push(SqlValue::Integer(i64::try_from(turn)?));
        sql.push_str(&format!(" AND json_extract(data,'$.turn')=?{}", args.len()));
    }
    sql.push_str(if query.newest.is_some() {
        " ORDER BY seq DESC"
    } else {
        " ORDER BY seq ASC"
    });
    if let Some(limit) = query.newest {
        args.push(SqlValue::Integer(i64::try_from(limit)?));
        sql.push_str(&format!(" LIMIT ?{}", args.len()));
    }
    let mut events = query_events(conn, &sql, args, |row| {
        Ok(StoredEvent {
            seq: row.get(0)?,
            time: row.get(1)?,
            kind: row.get(2)?,
            raw: row.get(3)?,
            surface_op: row.get(4)?,
        })
    })?;
    if query.newest.is_some() {
        events.reverse();
    }
    Ok(events)
}

/// Freeze a numeric history boundary and the original user record references.
pub fn requirement_snapshot(conn: &Connection, task: &str) -> Result<(i64, Vec<i64>)> {
    let through = load_events(
        conn,
        task,
        &EventQuery {
            newest: Some(1),
            ..Default::default()
        },
    )?
    .last()
    .map(|event| event.seq)
    .unwrap_or(-1);
    let requirements = load_events(
        conn,
        task,
        &EventQuery {
            through: Some(through),
            kinds: &["user/message"],
            ..Default::default()
        },
    )?
    .into_iter()
    .map(|event| event.seq)
    .collect();
    Ok((through, requirements))
}

/// Page exact stored JSON bytes as Unicode characters, without reserializing it.
pub async fn read_event(
    root: &Path,
    task: &str,
    seq: i64,
    through: Option<i64>,
    args: &Value,
) -> Result<Value> {
    ensure!(
        through.is_none_or(|boundary| seq <= boundary),
        "history reference is beyond the frozen snapshot"
    );
    let (root, task, args) = (root.to_owned(), task.to_owned(), args.clone());
    tokio::task::spawn_blocking(move || {
        let conn = crate::agent_service::open_db(&root)?;
        let event = load_events(&conn, &task, &EventQuery {exact: Some(seq), ..Default::default()})?
            .pop().context("history event not found in this conversation")?;
        let start = args["char_offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["max_chars"].as_u64().unwrap_or(24000).clamp(1, 24000) as usize;
        let total = event.raw.chars().count();
        ensure!(start <= total, "history char_offset exceeds total_chars={total}");
        let content: String = event.raw.chars().skip(start).take(limit).collect();
        let end = start + content.chars().count();
        Ok(json!({"event_seq":seq,"kind":event.kind,"content":content,"char_offset":start,
            "total_chars":total,"returned_chars":end-start,"next_char_offset":if end<total{Some(end)}else{None},
            "history_reference":{"task_id":task,"event_seq":seq,"include_event":true}}))
    }).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compression_has_no_private_history_query() {
        let source = include_str!("../single_agent.rs");
        let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
        assert!(
            !production.contains("FROM agent_task_events"),
            "compression and the main loop must access original records through the history module"
        );
    }
    #[tokio::test]
    async fn raw_pages_and_queries_preserve_history() {
        let root = std::env::temp_dir().join(format!(
            "history-store-{}",
            crate::agent_service::uuid_like()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let conn = crate::agent_service::open_db(&root).unwrap();
        for task in ["a", "b"] {
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'test','test','completed',1,1)",[task]).unwrap();
        }
        let raw = "{ \"message\": {\"role\":\"user\", \"content\":\"你好🌊\\n保留原文\"} }";
        for (task, seq, kind, body) in [
            ("a", 9, "user/message", raw),
            ("a", 3, "assistant/message", "{}"),
            ("b", 9, "user/message", "{}"),
            ("a", 12, "tool/result", r#"{"message":{"toolCallId":"c"}}"#),
        ] {
            conn.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES (?1,?2,1,?3,?4)", rusqlite::params![task,seq,kind,body]).unwrap();
        }
        let newest = load_events(
            &conn,
            "a",
            &EventQuery {
                newest: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            newest.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![9, 12]
        );
        assert_eq!(requirement_snapshot(&conn, "a").unwrap(), (12, vec![9]));
        assert_eq!(
            load_events(
                &conn,
                "a",
                &EventQuery {
                    call_id: Some("c"),
                    ..Default::default()
                }
            )
            .unwrap()[0]
                .seq,
            12
        );
        let mut offset = 0;
        let mut restored = String::new();
        loop {
            let page = read_event(
                &root,
                "a",
                9,
                Some(9),
                &json!({"char_offset":offset,"max_chars":7}),
            )
            .await
            .unwrap();
            restored.push_str(page["content"].as_str().unwrap());
            if let Some(next) = page["next_char_offset"].as_u64() {
                offset = next;
            } else {
                break;
            }
        }
        assert_eq!(restored, raw);
        assert!(
            read_event(&root, "a", 12, Some(9), &json!({}))
                .await
                .is_err()
        );
        assert!(
            read_event(&root, "a", 9, None, &json!({"char_offset":999}))
                .await
                .is_err()
        );
        assert!(
            read_event(&root, "missing", 9, None, &json!({}))
                .await
                .is_err()
        );
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
}

// Search projection metadata; each row still contains the unchanged stored JSON.
pub(super) type SearchRow = (
    i64,
    i64,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
);
pub(super) struct SearchSelection {
    pub rows: Vec<SearchRow>,
    pub project: bool,
    pub target: String,
    pub role: String,
    pub query: String,
    pub turn: Option<i64>,
    pub limit: usize,
    pub max_chars: usize,
    pub offset: usize,
    pub project_offset: i64,
}

pub(super) fn search_events(
    conn: &Connection,
    task: &str,
    args: &Value,
) -> Result<SearchSelection> {
    let scope = args["scope"].as_str().unwrap_or("conversation");
    ensure!(
        matches!(scope, "conversation" | "project"),
        "invalid history scope"
    );
    let project = scope == "project";
    let target = args["task_id"].as_str().unwrap_or(task);
    let role = args["role"].as_str().unwrap_or("all");
    ensure!(
        matches!(
            role,
            "all"
                | "user"
                | "agent"
                | "context_agent"
                | "worker"
                | "organizer"
                | "observer"
                | "tool"
        ),
        "invalid history role"
    );
    let query = args["query"].as_str().unwrap_or("").trim().to_lowercase();
    let node = args["node_id"].as_str().unwrap_or("");
    let turn = args["turn"].as_i64();
    let exact = args["event_seq"].as_i64();
    let before = args["before_seq"].as_i64();
    let after = args["after_seq"].as_i64();
    let request = args["request_id"].as_i64();
    let limit = if exact.is_some() {
        1
    } else {
        args["limit"].as_u64().unwrap_or(4).clamp(1, 12) as usize
    };
    let max_chars = args["max_chars"].as_u64().unwrap_or(6000).clamp(256, 12000) as usize;
    let offset = args["char_offset"].as_u64().unwrap_or(0) as usize;
    ensure!(
        offset == 0 || exact.is_some(),
        "char_offset requires event_seq"
    );
    ensure!(
        !project
            || (exact.is_none()
                && before.is_none()
                && after.is_none()
                && args["task_id"].is_null()),
        "Project search returns conversation references; use scope=conversation with task_id for exact events or before_seq."
    );
    let has_contexts:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_request_contexts')",[],|row|row.get(0))?;
    let context_search = if has_contexts {
        "OR (kind='debug/context_request' AND instr(lower(COALESCE((SELECT request_json FROM agent_request_contexts c WHERE c.task_id=history.task_id AND c.id=json_extract(history.data,'$.id')),'')),?3)>0)"
    } else {
        ""
    };
    if !project {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_tasks WHERE id=?1)",
            params![target],
            |row| row.get(0),
        )?;
        ensure!(exists, "Conversation not found in this workspace: {target}");
    }
    let project_offset = args["project_offset"].as_i64().unwrap_or(0).max(0);
    let base=format!("WITH history AS (
        SELECT e.task_id,e.seq,e.timestamp,e.kind,e.data,
            CASE WHEN e.kind LIKE 'tool/%' THEN 'tool'
                 WHEN json_extract(e.data,'$.actor') IS NOT NULL THEN json_extract(e.data,'$.actor')
                 WHEN e.kind='debug/context_request' THEN 'unknown'
                 WHEN e.kind='user/message' THEN 'user'
                 WHEN e.kind='visual/input_result' THEN COALESCE(json_extract(e.data,'$.actor'),'worker')
                 WHEN e.kind LIKE 'observer/%' THEN 'observer'
                 WHEN e.kind LIKE 'organizer/%' THEN 'organizer'
                 WHEN e.kind LIKE 'tool/%' THEN 'tool' ELSE 'worker' END AS role,
            COALESCE(json_extract(e.data,'$.nodeId'),json_extract(e.data,'$.flowNodeId'),
                json_extract(e.data,'$.identity.node_id'),
                (SELECT json_extract(c.data,'$.flowNodeId') FROM agent_task_events c
                 WHERE c.task_id=e.task_id AND c.kind='tool/call'
                   AND json_extract(c.data,'$.callId')=json_extract(e.data,'$.message.toolCallId') ORDER BY c.seq DESC LIMIT 1)) AS node_id
        FROM agent_task_events e WHERE (?1 IS NULL OR e.task_id=?1)
          AND (e.kind!='debug/context_request' OR ?6 IS NOT NULL OR ?12)
    ), filtered AS (SELECT * FROM history
      WHERE (?2='all' OR role=?2) AND (?3='' OR instr(lower(data),?3)>0 {context_search})
        AND (?4='' OR node_id=?4) AND (?5 IS NULL OR json_extract(data,'$.turn')=?5)
        AND (?6 IS NULL OR seq=?6) AND (?7 IS NULL OR seq<?7) AND (?10 IS NULL OR seq>?10)
        AND (?11 IS NULL OR json_extract(data,'$.identity.request_id') IS NULL OR json_extract(data,'$.identity.request_id')=?11)
    )");
    let select = if project {
        ", ranked AS (SELECT *, ROW_NUMBER() OVER(PARTITION BY task_id ORDER BY seq DESC) AS rank,
           COUNT(*) OVER(PARTITION BY task_id) AS matched_events FROM filtered)
         SELECT r.seq,r.timestamp,r.kind,r.data,r.role,r.node_id,r.task_id,t.prompt,t.status,r.matched_events
         FROM ranked r JOIN agent_tasks t ON t.id=r.task_id WHERE r.rank=1
         ORDER BY t.updated_at DESC,t.id DESC LIMIT ?8 OFFSET ?9"
    } else {
        " SELECT seq,timestamp,kind,data,role,node_id,task_id,NULL,NULL,NULL FROM filtered
          ORDER BY seq DESC LIMIT ?8 OFFSET ?9"
    };
    let number = |value: Option<i64>| value.map(SqlValue::Integer).unwrap_or(SqlValue::Null);
    let bindings = vec![
        if project {
            SqlValue::Null
        } else {
            SqlValue::Text(target.to_owned())
        },
        SqlValue::Text(role.to_owned()),
        SqlValue::Text(query.clone()),
        SqlValue::Text(node.to_owned()),
        number(turn),
        number(exact),
        number(before),
        SqlValue::Integer((limit + 1) as i64),
        SqlValue::Integer(if project { project_offset } else { 0 }),
        number(after),
        number(request),
        SqlValue::Integer(i64::from(args["include_context"] == true)),
    ];
    let rows = query_events(conn, &format!("{base}{select}"), bindings, |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
        ))
    })?;
    Ok(SearchSelection {
        rows,
        project,
        target: target.to_owned(),
        role: role.to_owned(),
        query,
        turn,
        limit,
        max_chars,
        offset,
        project_offset,
    })
}

/// Shared query executor for both exact event access and history searches.
/// Callers inside this repository provide bindings and row shapes; original
/// message data is returned verbatim and no semantic selection occurs here.
fn query_events<T, F>(
    conn: &Connection,
    sql: &str,
    bindings: Vec<SqlValue>,
    map: F,
) -> Result<Vec<T>>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mut statement = conn.prepare(sql)?;
    Ok(statement
        .query_map(params_from_iter(bindings), map)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
