//! Task-directed rolling views of the shared history. Selection is performed by
//! the receiving role; the host only resolves references, orders originals and
//! checks capacity. The durable window contains references, not another history.
use crate::{
    agent_service::AgentServiceState,
    context_window::{estimate, HistoryView, MAX_TOKENS},
};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{collections::BTreeSet, time::Duration};
use tokio_util::sync::CancellationToken;

const SELECTION_SYSTEM:&str="You are rebuilding your own context window for the current task. This is a context-selection phase, not task execution. Search the shared conversation using read_session_history: inspect the latest changes, then retrieve earlier instructions, results, corrections or advice necessary for your current task. Select existing event references with select_context. Keep necessary dependencies and superseding instructions; do not select old receipts as new executions. The host will recover whole original messages and put them in original history order. Do not summarize or rewrite them. Select only what is necessary, leave space for continued execution, and do not try to fill the window. No project operations, Worker dispatch or messages to other roles are allowed in this phase.";

#[derive(Clone, Default)]
struct Window {
    through: i64,
    selected: BTreeSet<i64>,
}
async fn load(
    state: &AgentServiceState,
    task: &str,
    actor: &str,
    scope: &str,
) -> Result<Option<Window>> {
    let root = state.workspace.root().to_path_buf();
    let task = task.to_owned();
    let actor = actor.to_owned();
    let scope = scope.to_owned();
    tokio::task::spawn_blocking(move ||->Result<Option<Window>> {
        let conn=crate::agent_service::open_db(&root)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_context_windows(task_id TEXT NOT NULL,actor TEXT NOT NULL,scope TEXT NOT NULL,through_seq INTEGER NOT NULL,selected_refs TEXT NOT NULL,PRIMARY KEY(task_id,actor,scope))")?;
        let row:Option<(i64,String)>=conn.query_row("SELECT through_seq,selected_refs FROM agent_context_windows WHERE task_id=?1 AND actor=?2 AND scope=?3",params![task,actor,scope],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        row.map(|(through,refs)|Ok(Window {through,selected:serde_json::from_str(&refs)?})).transpose()
    }).await?
}
async fn save(
    state: &AgentServiceState,
    task: &str,
    actor: &str,
    scope: &str,
    window: Window,
) -> Result<()> {
    let root = state.workspace.root().to_path_buf();
    let task = task.to_owned();
    let actor = actor.to_owned();
    let scope = scope.to_owned();
    tokio::task::spawn_blocking(move ||->Result<()> {
        let conn=crate::agent_service::open_db(&root)?;
        conn.execute("INSERT INTO agent_context_windows(task_id,actor,scope,through_seq,selected_refs) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(task_id,actor,scope) DO UPDATE SET through_seq=excluded.through_seq,selected_refs=excluded.selected_refs WHERE excluded.through_seq>=agent_context_windows.through_seq",params![task,actor,scope,window.through,serde_json::to_string(&window.selected)?])?;
        Ok(())
    }).await?
}
fn references(view: &HistoryView) -> BTreeSet<i64> {
    view.sources.iter().flatten().copied().collect()
}
fn assemble(view: &HistoryView, pool: &HistoryView, selected: &BTreeSet<i64>) -> Vec<Value> {
    if view
        .messages
        .get(1)
        .is_some_and(|m| m["name"] == "organizer")
    {
        return view
            .messages
            .iter()
            .zip(&view.sources)
            .filter(|(_, s)| s.is_none_or(|s| selected.contains(&s)))
            .map(|(m, _)| m.clone())
            .collect();
    }
    let mut messages = view
        .messages
        .iter()
        .zip(&view.sources)
        .take(2)
        .map(|(m, _)| m.clone())
        .collect::<Vec<_>>();
    // Existing source references determine order; no number decorates a body.
    let mut records = pool
        .messages
        .iter()
        .zip(&pool.sources)
        .filter_map(|(m, seq)| {
            seq.filter(|seq| selected.contains(seq))
                .map(|seq| (seq, m.clone()))
        })
        .collect::<Vec<_>>();
    records.sort_by_key(|(seq, _)| *seq);
    messages.extend(records.into_iter().map(|(_, m)| m));
    messages.extend(
        view.messages
            .iter()
            .zip(&view.sources)
            .skip(2)
            .filter(|(_, s)| s.is_none())
            .map(|(m, _)| m.clone()),
    );
    if !references(pool).is_subset(selected) {
        messages.push(json!({"role":"system","content":"This rolling context contains selected original history, not every event in the shared conversation. A history header's complete coverage describes the saved pool, not this selected view. Use read_session_history for any missing evidence and state unread scope before claiming a complete process review. Retained receipts remain historical operations, not new executions."}));
    }
    messages
}
fn selection_tool() -> Value {
    json!({"type":"function","function":{"name":"select_context","description":"Finish rebuilding this task's context by choosing existing history event references. Whole originals are restored in chronological order; this does not execute a task or change source messages.","parameters":{"type":"object","properties":{"event_seqs":{"type":"array","maxItems":2048,"items":{"type":"integer","minimum":0}}},"required":["event_seqs"]}}})
}
async fn pool(
    state: &AgentServiceState,
    task: &str,
    actor: &str,
    input: &Value,
    through: i64,
    view: &HistoryView,
) -> Result<HistoryView> {
    if input["target_role"] == "worker" {
        return Ok(view.clone());
    }
    let mut pool = if actor == "organizer" {
        crate::session_history::organizer_view(state.workspace.root(), task, input, true).await?
    } else {
        crate::session_history::process_view(
            &crate::session_history::task_process(state.workspace.root(), task, 0).await?,
        )
    };
    // Freeze asynchronous Observer visibility at its execution boundary.
    let mut messages = Vec::new();
    let mut sources = Vec::new();
    for (m, s) in pool.messages.into_iter().zip(pool.sources) {
        if s.is_some_and(|s| s <= through) {
            messages.push(m);
            sources.push(s);
        }
    }
    let have = sources.iter().flatten().copied().collect::<BTreeSet<_>>();
    for (m, s) in view.messages.iter().zip(&view.sources) {
        if s.is_some_and(|s| s <= through && !have.contains(&s)) {
            messages.push(m.clone());
            sources.push(*s);
        }
    }
    pool = HistoryView { messages, sources };
    Ok(pool)
}

#[allow(clippy::too_many_arguments)]
pub async fn prepare(
    state: &AgentServiceState,
    model: &str,
    actor: &str,
    task: &str,
    scope: &str,
    input: &Value,
    body: &mut Value,
    view: HistoryView,
    metadata: Value,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<Value> {
    let limit =
        MAX_TOKENS.saturating_sub(body["max_tokens"].as_u64().unwrap_or(8192) as usize + 8192);
    prepare_at_limit(
        state, model, actor, task, scope, input, body, view, metadata, cancel, timeout, limit,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn prepare_at_limit(
    state: &AgentServiceState,
    model: &str,
    actor: &str,
    task: &str,
    scope: &str,
    input: &Value,
    body: &mut Value,
    view: HistoryView,
    metadata: Value,
    cancel: &CancellationToken,
    timeout: Duration,
    limit: usize,
) -> Result<Value> {
    ensure!(
        matches!(actor, "organizer" | "observer"),
        "Worker cannot select from shared history"
    );
    ensure!(
        view.messages.len() == view.sources.len(),
        "context provenance array mismatch"
    );
    let before = estimate(body);
    let through = metadata["history_through_seq"]
        .as_i64()
        .unwrap_or_else(|| references(&view).last().copied().unwrap_or(0));
    let previous = load(state, task, actor, scope)
        .await?
        .filter(|window| window.through <= through);
    let mut selected = references(&view);
    let mut source_pool = None;
    if let Some(previous) = &previous {
        selected = previous.selected.clone();
        selected.extend(
            view.sources
                .iter()
                .flatten()
                .filter(|seq| **seq > previous.through)
                .copied(),
        );
        let originals = pool(state, task, actor, input, through, &view).await?;
        body["messages"] = json!(assemble(&view, &originals, &selected));
        source_pool = Some(originals);
    }
    if estimate(body) <= limit {
        if previous.is_some() {
            save(
                state,
                task,
                actor,
                scope,
                Window {
                    through,
                    selected: selected.clone(),
                },
            )
            .await?;
        }
        return Ok(
            json!({"mode":if previous.is_some(){"rolling_window"}else{"within_window"},"max_tokens":MAX_TOKENS,"estimated_before_tokens":before,"estimated_input_tokens":estimate(body),"selected_event_refs":selected,"through_seq":through}),
        );
    }
    let originals = match source_pool {
        Some(pool) => pool,
        None => pool(state, task, actor, input, through, &view).await?,
    };
    let available = references(&originals);
    // A genuine human instruction remains original and mandatory. Headers have
    // current role/task information; the selection phase retrieves process facts.
    let human = view
        .messages
        .iter()
        .zip(&view.sources)
        .rev()
        .find(|(m, s)| {
            s.is_some()
                && m["role"] == "user"
                && (m["name"].is_null()
                    || matches!(m["name"].as_str(), Some("user_message" | "user")))
        })
        .and_then(|(_, s)| *s);
    let mandatory = human.into_iter().collect::<BTreeSet<_>>();
    let mut empty = body.clone();
    empty["messages"] = json!(assemble(&view, &originals, &mandatory));
    ensure!(
        estimate(&empty) <= limit,
        "Current task instructions exceed context capacity; originals were not truncated"
    );
    let mut seed = view.messages.iter().take(2).cloned().collect::<Vec<_>>();
    if input["target_role"] == "worker" {
        seed[0] = json!({"role":"system","content":include_str!("../prompts/organizer_system.md")});
    }
    if let Some(human) = human {
        seed.extend(
            originals
                .messages
                .iter()
                .zip(&originals.sources)
                .filter(|(_, s)| **s == Some(human))
                .map(|(m, _)| m.clone()),
        );
    }
    seed.push(json!({"role":"system","content":format!("{SELECTION_SYSTEM}\nReceiving role: {actor}. Target context: {}. Existing conversation: {task}. Read only events before {}. Rebuilt task input budget: {limit} tokens. Select a smaller working set with room for new messages.",input["target_role"].as_str().unwrap_or(actor),through+1)}));
    if input["target_role"] == "worker" {
        seed.push(json!({"role":"system","content":"Select only this Worker's supplied invocation history. Its original Organizer commands and inputs remain mandatory. Select saved tool/call references to retain their whole assistant/tool-result exchange atomically. You can read this node's Worker or tool records; Observer advice is outside this view."}));
    }
    let mut selection_body = json!({"model":model,"stream":false,"max_tokens":4096,"messages":seed,"tools":[crate::session_history::tool(),selection_tool()],"tool_choice":{"type":"function","function":{"name":"read_session_history"}}});
    if let Some(effort) = state.reasoning_effort.as_deref() {
        selection_body["reasoning_effort"] = json!(effort);
    }
    let select = async {
        let mut looked_up = false;
        loop {
            ensure!(
                estimate(&selection_body) <= MAX_TOKENS.saturating_sub(8192),
                "Context-selection search exceeded its capacity; no original was silently dropped"
            );
            let mut trace_metadata = metadata.clone();
            trace_metadata["actor"] = json!(actor);
            trace_metadata["stage"] = json!("context_selection");
            trace_metadata["context_scope"] = json!(scope);
            let trace = crate::request_context::record(
                state.workspace.root(),
                task,
                trace_metadata,
                &selection_body,
            )
            .await;
            let (url, key) = if actor == "observer" {
                (&state.observer_provider_url, &state.observer_api_key)
            } else {
                (&state.provider_url, &state.api_key)
            };
            let response = async {
                let response = state
                    .client
                    .post(format!("{}/chat/completions", url.trim_end_matches('/')))
                    .bearer_auth(key)
                    .json(&selection_body)
                    .send()
                    .await?;
                crate::visual_probe::successful_response(response)
                    .await?
                    .json::<Value>()
                    .await
                    .map_err(anyhow::Error::from)
            }
            .await;
            crate::request_context::finish(trace,if response.is_ok(){"completed"}else{"failed"},json!({"response":response.as_ref().ok(),"error":response.as_ref().err().map(ToString::to_string)})).await;
            let response = response?;
            let message = &response["choices"][0]["message"];
            let calls = message["tool_calls"]
                .as_array()
                .filter(|calls| !calls.is_empty())
                .context("Context selection requires read_session_history or select_context")?;
            selection_body["messages"]
                .as_array_mut()
                .unwrap()
                .push(message.clone());
            for call in calls {
                let name = call["function"]["name"].as_str().unwrap_or("");
                let args = serde_json::from_str::<Value>(
                    call["function"]["arguments"].as_str().unwrap_or("{}"),
                )?;
                let result = match name {
                    "read_session_history" => {
                        ensure!(args["scope"].as_str().is_none_or(|s|s=="conversation") && args["task_id"].as_str().is_none_or(|s|s==task) && args["include_context"]!=true,"Context selection reads only this shared conversation, not model-request copies");
                        let mut bounded = args.clone();
                        if input["target_role"] == "worker" {
                            bounded["node_id"] = metadata["nodeId"].clone();
                            if !matches!(bounded["role"].as_str(), Some("worker" | "tool")) {
                                bounded["role"] = json!("tool");
                            }
                        }
                        bounded["before_seq"] = json!(bounded["before_seq"]
                            .as_i64()
                            .unwrap_or(through + 1)
                            .min(through + 1));
                        ensure!(
                            bounded["event_seq"].as_i64().is_none_or(|s| s <= through),
                            "History reference is beyond this observation's boundary"
                        );
                        let result =
                            crate::session_history::read(state.workspace.root(), task, &bounded)
                                .await?;
                        looked_up = true;
                        crate::agent_service::emit(state.workspace.root(),task,"context/selection_read",json!({"actor":actor,"scope":scope,"arguments":bounded,"result":result})).await?;
                        result
                    }
                    "select_context" => {
                        ensure!(looked_up, "Search history before selecting the working set");
                        let values = args["event_seqs"]
                            .as_array()
                            .context("select_context.event_seqs must be an array")?;
                        let chosen = values
                            .iter()
                            .map(|v| {
                                v.as_i64()
                                    .context("context reference must be an existing event sequence")
                            })
                            .collect::<Result<BTreeSet<_>>>()?;
                        if !chosen.is_subset(&available) {
                            json!({"error":"One or more references are not original messages visible to this role at this boundary. Search for appropriate originals."})
                        } else {
                            let mut chosen = chosen;
                            chosen.extend(&mandatory);
                            let candidate = assemble(&view, &originals, &chosen);
                            let mut candidate_body = body.clone();
                            candidate_body["messages"] = json!(candidate);
                            if estimate(&candidate_body) > limit {
                                json!({"error":"The selected whole originals exceed the input budget. Select fewer necessary events; do not rewrite or truncate them.","estimated_input_tokens":estimate(&candidate_body),"input_budget":limit})
                            } else {
                                return Ok::<_, anyhow::Error>((candidate_body, chosen));
                            }
                        }
                    }
                    _ => {
                        json!({"error":"Only history reading and context selection are available; no task operation was executed"})
                    }
                };
                selection_body["messages"].as_array_mut().unwrap().push(
                    json!({"role":"tool","tool_call_id":call["id"],"content":result.to_string()}),
                );
            }
            selection_body["tool_choice"] = json!("auto");
        }
    };
    let (rebuilt, chosen) = tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("context selection cancelled"),result=tokio::time::timeout(timeout,select)=>result.context("context selection timed out")??};
    *body = rebuilt;
    save(
        state,
        task,
        actor,
        scope,
        Window {
            through,
            selected: chosen.clone(),
        },
    )
    .await?;
    let metadata = json!({"mode":"task_selected_history","max_tokens":MAX_TOKENS,"estimated_before_tokens":before,"estimated_input_tokens":estimate(body),"selected_event_refs":chosen,"through_seq":through,"omitted_old_messages":references(&view).difference(&chosen).count()});
    crate::agent_service::emit(
        state.workspace.root(),
        task,
        "context/rebuilt",
        json!({"actor":actor,"scope":scope,"context_window":metadata}),
    )
    .await?;
    Ok(metadata)
}

/// Organizer selects only the execution messages already available to this
/// Worker invocation. Native assistant/tool exchanges share an existing call
/// event reference so selecting a result cannot break the provider protocol.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_worker(
    state: &AgentServiceState,
    model: &str,
    task: &str,
    scope: &str,
    body: &mut Value,
    metadata: Value,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<Value> {
    let root = state.workspace.root().to_path_buf();
    let saved_task = task.to_owned();
    let (calls,texts,through)=tokio::task::spawn_blocking(move ||->Result<_> {
        let conn=crate::agent_service::open_db(&root)?;
        let through=conn.query_row("SELECT COALESCE(MAX(seq),0) FROM agent_task_events WHERE task_id=?1",[&saved_task],|r|r.get::<_,i64>(0))?;
        let mut stmt=conn.prepare("SELECT seq,kind,data FROM agent_task_events WHERE task_id=?1 AND kind IN ('tool/call','assistant/message') ORDER BY seq")?;
        let rows=stmt.query_map([saved_task],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut calls=std::collections::HashMap::new();let mut texts=std::collections::HashMap::new();
        for (seq,kind,data) in rows {
            let data:Value=serde_json::from_str(&data)?;
            if kind=="tool/call" {if let Some(id)=data["callId"].as_str(){calls.insert(id.to_owned(),seq);}}
            else {let text=data["message"]["content"].as_str().map(str::to_owned).unwrap_or_else(||data["message"]["content"].as_array().into_iter().flatten().filter_map(|b|b["text"].as_str()).collect::<Vec<_>>().join("\n"));if !text.is_empty(){texts.insert(text,seq);}}
        }
        Ok((calls,texts,through))
    }).await??;
    let messages = body["messages"]
        .as_array()
        .context("Worker messages are missing")?
        .clone();
    let mut view = HistoryView {
        sources: vec![None; messages.len()],
        messages,
    };
    let mut groups = std::collections::HashMap::new();
    for (index, message) in view.messages.iter().enumerate().skip(2) {
        if let Some(ids) = message["tool_calls"].as_array() {
            let source = ids
                .iter()
                .filter_map(|call| call["id"].as_str().and_then(|id| calls.get(id)))
                .min()
                .copied()
                .context("Worker exchange lacks its saved call reference")?;
            view.sources[index] = Some(source);
            for call in ids {
                if let Some(id) = call["id"].as_str() {
                    groups.insert(id.to_owned(), source);
                }
            }
        } else if message["role"] == "assistant" {
            view.sources[index] = message["content"]
                .as_str()
                .and_then(|text| texts.get(text))
                .copied();
        }
    }
    for (index, message) in view.messages.iter().enumerate().skip(2) {
        if message["role"] == "tool" {
            view.sources[index] = Some(
                message["tool_call_id"]
                    .as_str()
                    .and_then(|id| groups.get(id))
                    .copied()
                    .context("Worker result lacks its matching saved exchange")?,
            );
        }
    }
    let mut metadata = metadata;
    metadata["history_through_seq"] = json!(through);
    metadata["target_role"] = json!("worker");
    prepare(
        state,
        model,
        "organizer",
        task,
        &format!("worker:{scope}"),
        &json!({"target_role":"worker"}),
        body,
        view,
        metadata,
        cancel,
        timeout,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json};
    use std::sync::{Arc, Mutex};
    fn call(name: &str, args: Value) -> Value {
        json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call_{name}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
    }
    fn root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "context-selection-{}",
            crate::visual_artifacts::new_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn insert(root: &std::path::Path, seq: i64, kind: &str, data: Value) {
        crate::agent_service::open_db(root).unwrap().execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES ('task',?1,?1,?2,?3)",params![seq,kind,data.to_string()]).unwrap();
    }
    fn task(root: &std::path::Path) {
        crate::agent_service::open_db(root).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','current goal','fake','running',0,0)",[]).unwrap();
    }
    fn body(view: &HistoryView) -> Value {
        json!({"model":"fake","max_tokens":32,"messages":view.messages})
    }
    async fn organizer_view(root: &std::path::Path) -> HistoryView {
        let mut view = crate::session_history::organizer_view(
            root,
            "task",
            &json!({"process":{"request_id":1}}),
            false,
        )
        .await
        .unwrap();
        view.prepend(json!({"role":"system","content":"Organizer"}));
        view
    }
    #[tokio::test]
    async fn role_search_restores_old_needed_original_and_window_survives_reload() {
        let root = root();
        task(&root);
        insert(
            &root,
            1,
            "user/message",
            json!({"turn":1,"message":{"content":"original goal"}}),
        );
        let original = " \n必要的旧信息：只上传一次。\n ";
        insert(
            &root,
            3,
            "worker/yield",
            json!({"turn":1,"output":{"id":"early","original_return":original}}),
        );
        insert(
            &root,
            5,
            "tool/result",
            json!({"turn":1,"meta":{"result":{"irrelevant":"noise ".repeat(10000)}}}),
        );
        insert(
            &root,
            9,
            "user/message",
            json!({"turn":2,"message":{"content":"继续检查当前文件"}}),
        );
        insert(
            &root,
            11,
            "organizer/error",
            json!({"turn":2,"feedback":{"error":"keep this correction"}}),
        );
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/v1/chat/completions",
                    post(move |Json(body): Json<Value>| {
                        let seen = seen.clone();
                        async move {
                            let mut seen = seen.lock().unwrap();
                            seen.push(body.clone());
                            let message = if seen.len() == 1 {
                                call(
                                    "read_session_history",
                                    json!({"query":"必要的旧信息","limit":2}),
                                )
                            } else {
                                call("select_context", json!({"event_seqs":[11,3]}))
                            };
                            Json(json!({"choices":[{"message":message}]}))
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let state = crate::agent_service::tests::flow_test_state(&root, address);
        let input = json!({"process":{"request_id":1}});
        let view = organizer_view(&root).await;
        let mut request = body(&view);
        let metadata = prepare_at_limit(
            &state,
            "fake",
            "organizer",
            "task",
            "request:1",
            &input,
            &mut request,
            view,
            json!({"turn":2}),
            &CancellationToken::new(),
            Duration::from_secs(5),
            1200,
        )
        .await
        .unwrap();
        assert_eq!(metadata["mode"], "task_selected_history");
        assert_eq!(
            crate::session_history::original_text(request["messages"][2]["content"].as_str().unwrap()), original,
            "whitespace and original wording survive"
        );
        assert!(request.to_string().contains("keep this correction"));
        assert!(!request.to_string().contains("noise noise"));
        assert_eq!(
            metadata["selected_event_refs"],
            json!([3, 9, 11]),
            "selection is sorted by original history, not the model's returned order"
        );
        assert_eq!(requests.lock().unwrap().len(), 2);
        insert(
            &root,
            100,
            "worker/yield",
            json!({"turn":2,"output":{"id":"new","original_return":"new actual result"}}),
        );
        let reloaded = crate::agent_service::tests::flow_test_state(&root, address);
        let view = organizer_view(&root).await;
        let mut request = body(&view);
        let metadata = prepare_at_limit(
            &reloaded,
            "fake",
            "organizer",
            "task",
            "request:1",
            &input,
            &mut request,
            view,
            json!({}),
            &CancellationToken::new(),
            Duration::from_secs(5),
            1200,
        )
        .await
        .unwrap();
        assert_eq!(metadata["mode"], "rolling_window");
        assert!(request.to_string().contains("new actual result"));
        assert!(request.to_string().contains("必要的旧信息"));
        assert!(!request.to_string().contains("noise noise"));
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "new messages append without re-searching every request"
        );
        save(
            &reloaded,
            "task",
            "organizer",
            "request:1",
            Window {
                through: 11,
                selected: BTreeSet::from([3]),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            load(&reloaded, "task", "organizer", "request:1")
                .await
                .unwrap()
                .unwrap()
                .through,
            100,
            "a delayed older selection cannot roll back the saved window"
        );
        let conn = crate::agent_service::open_db(&root).unwrap();
        let records: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_request_contexts WHERE actor='organizer'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(records, 2);
        drop(conn);
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn observer_rebuild_rejects_future_reference_and_cannot_execute_business_tools() {
        let root = root();
        task(&root);
        insert(
            &root,
            3,
            "worker/yield",
            json!({"turn":1,"output":{"original_return":"needed past"}}),
        );
        insert(
            &root,
            5,
            "tool/result",
            json!({"turn":1,"meta":{"result":"irrelevant ".repeat(10000)}}),
        );
        insert(
            &root,
            13,
            "worker/yield",
            json!({"turn":1,"output":{"original_return":"FUTURE_RESULT_MUST_NOT_LEAK"}}),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let step = Arc::new(Mutex::new(0));
        let calls = step.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/v1/chat/completions",
                    post(move |Json(body): Json<Value>| {
                        let calls = calls.clone();
                        async move {
                            let mut n = calls.lock().unwrap();
                            *n += 1;
                            assert!(!body.to_string().contains("FUTURE_RESULT_MUST_NOT_LEAK"));
                            let message = match *n {
                                1 => call("read_session_history", json!({"before_seq":99})),
                                2 => call("run_program", json!({"command":"touch forbidden"})),
                                3 => call("select_context", json!({"event_seqs":[13]})),
                                _ => call("select_context", json!({"event_seqs":[3]})),
                            };
                            Json(json!({"choices":[{"message":message}]}))
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let mut state = crate::agent_service::tests::flow_test_state(&root, address);
        state.observer_provider_url = state.provider_url.clone();
        let process = crate::session_history::task_process(&root, "task", 0)
            .await
            .unwrap();
        let mut view = crate::session_history::process_view(&process);
        let keep = view
            .sources
            .iter()
            .map(|s| s.is_some_and(|s| s <= 5))
            .collect::<Vec<_>>();
        view.messages = view
            .messages
            .into_iter()
            .zip(&keep)
            .filter(|(_, keep)| **keep)
            .map(|(m, _)| m)
            .collect();
        view.sources = view
            .sources
            .into_iter()
            .zip(keep)
            .filter(|(_, keep)| *keep)
            .map(|(s, _)| s)
            .collect();
        view.prepend(json!({"role":"user","content":"review current goal"}));
        view.prepend(json!({"role":"system","content":"Observer"}));
        let mut request = body(&view);
        prepare_at_limit(
            &state,
            "fake",
            "observer",
            "task",
            "review:5",
            &json!({}),
            &mut request,
            view,
            json!({"turn":1}),
            &CancellationToken::new(),
            Duration::from_secs(5),
            1200,
        )
        .await
        .unwrap();
        assert!(request.to_string().contains("needed past"));
        assert!(!request.to_string().contains("FUTURE_RESULT"));
        assert!(!root.join("forbidden").exists());
        let conn = crate::agent_service::open_db(&root).unwrap();
        let calls: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_task_events WHERE kind='tool/call'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(calls, 0);
        drop(conn);
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn organizer_selected_worker_view_keeps_native_exchange_atomic_and_array_order() {
        let messages = vec![
            json!({"role":"system","content":"Worker"}),
            json!({"role":"user","name":"organizer","content":"original assignment"}),
            call("read_file", json!({"path":"needed"})),
            json!({"role":"tool","tool_call_id":"call_read_file","content":"original failure"}),
        ];
        let view = HistoryView {
            messages: messages.clone(),
            sources: vec![None, None, Some(7), Some(7)],
        };
        assert_eq!(assemble(&view, &view, &BTreeSet::from([7])), messages);
        assert_eq!(assemble(&view, &view, &BTreeSet::new()), messages[..2]);
    }

    #[tokio::test]
    async fn over_256k_rebuilds_before_actual_organizer_and_observer_requests() {
        use axum::response::IntoResponse;
        for actor in ["organizer", "observer"] {
            let root = root();
            task(&root);
            insert(
                &root,
                1,
                "user/message",
                json!({"turn":1,"message":{"content":"检查已有上传结果，不能重复上传"}}),
            );
            let original = "  OLD_NEEDED_RECEIPT: uploaded once\n";
            insert(
                &root,
                3,
                "worker/yield",
                json!({"turn":1,"output":{"original_return":original}}),
            );
            insert(
                &root,
                5,
                "tool/result",
                json!({"turn":1,"meta":{"result":"UNRELATED_FILLER ".repeat(60000)}}),
            );
            let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
            let seen = captured.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener,axum::Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>| {
                let seen=seen.clone();async move {
                    let mut seen=seen.lock().unwrap();seen.push(body.clone());
                    if body["tools"].as_array().unwrap().iter().any(|t|t["function"]["name"]=="select_context") {
                        let message=if seen.len()==1 {call("read_session_history",json!({"query":"OLD_NEEDED_RECEIPT"}))}else{call("select_context",json!({"event_seqs":[3]}))};
                        return Json(json!({"choices":[{"message":message}]})).into_response();
                    }
                    assert!(body["messages"].as_array().unwrap().iter().any(|m|m["content"].as_str().is_some_and(|text|crate::session_history::original_text(text)==original)));
                    assert!(!body["messages"].to_string().contains("UNRELATED_FILLER"));
                    assert!(body["messages"].to_string().contains("检查已有上传结果，不能重复上传"),"current original human instruction survives in both roles");
                    if body["stream"]==true {
                        let args=json!({"summary":"verified once","achieved":true}).to_string();
                        let stream=format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"finish","type":"function","function":{"name":"finish_request","arguments":args}}]}}]}),json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}));
                        ([("content-type","text/event-stream")],stream).into_response()
                    }else {Json(json!({"choices":[{"message":{"role":"assistant","content":"reviewed original receipt"}}]})).into_response()}
                }
            }))).await.unwrap();
            });
            let mut state = crate::agent_service::tests::flow_test_state(&root, address);
            state.observer_provider_url = state.provider_url.clone();
            if actor == "organizer" {
                let decision = crate::work_organizer::decide(
                    &state,
                    "fake",
                    "task",
                    1,
                    1,
                    "node",
                    json!({"process":{"request_id":1}}),
                    &CancellationToken::new(),
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
                assert_eq!(decision["action"], "finish");
            } else {
                let history = crate::session_history::task_process(&root, "task", 1)
                    .await
                    .unwrap();
                let raw=crate::agent_service::observer_json_response(&state,"fake","review",json!({"stage":"retrospective","identity":{"request_id":1},"request":{"goal":"check existing upload"},"task_history":history}),1000,"task",json!({"turn":1})).await.unwrap();
                assert_eq!(raw, "reviewed original receipt");
            }
            assert_eq!(
                captured.lock().unwrap().len(),
                3,
                "two selection requests then exactly one normal role request"
            );
            let conn = crate::agent_service::open_db(&root).unwrap();
            let requests: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_request_contexts WHERE actor=?1",
                    [actor],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(requests, 3);
            let selections:i64=conn.query_row("SELECT COUNT(*) FROM agent_request_contexts WHERE json_extract(metadata,'$.stage')='context_selection'",[],|r|r.get(0)).unwrap();
            assert_eq!(selections, 2);
            drop(conn);
            server.abort();
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn worker_overflow_is_selected_by_organizer_without_observer_advice() {
        let root = root();
        task(&root);
        insert(
            &root,
            1,
            "tool/call",
            json!({"nodeId":"node","callId":"noise","name":"read_file"}),
        );
        insert(
            &root,
            2,
            "tool/result",
            json!({"nodeId":"node","message":{"toolCallId":"noise"},"meta":{"result":"FILLER ".repeat(150000)}}),
        );
        insert(
            &root,
            3,
            "tool/call",
            json!({"nodeId":"node","callId":"needed","name":"read_file"}),
        );
        insert(
            &root,
            4,
            "tool/result",
            json!({"nodeId":"node","message":{"toolCallId":"needed"},"meta":{"result":"needed own original"}}),
        );
        insert(
            &root,
            5,
            "observer/advice_delivered",
            json!({"advice":{"raw_response":"OBSERVER_ADVICE_MUST_NOT_REACH_WORKER"}}),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let n = Arc::new(Mutex::new(0));
        let seen = n.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/v1/chat/completions",
                    post(move |Json(body): Json<Value>| {
                        let seen = seen.clone();
                        async move {
                            assert!(!body
                                .to_string()
                                .contains("OBSERVER_ADVICE_MUST_NOT_REACH_WORKER"));
                            let mut n = seen.lock().unwrap();
                            *n += 1;
                            let message = if *n == 1 {
                                call(
                                    "read_session_history",
                                    json!({"query":"needed own original","role":"observer"}),
                                )
                            } else {
                                call("select_context", json!({"event_seqs":[3]}))
                            };
                            Json(json!({"choices":[{"message":message}]}))
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let state = crate::agent_service::tests::flow_test_state(&root, address);
        let native_call = |id: &str| json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":"read_file","arguments":"{}"}}]});
        let mut request = json!({"model":"fake","messages":[{"role":"system","content":"Worker"},{"role":"user","name":"organizer","content":"original command"},native_call("noise"),{"role":"tool","tool_call_id":"noise","content":"FILLER ".repeat(150000)},native_call("needed"),{"role":"tool","tool_call_id":"needed","content":"needed own original"}]});
        let metadata = prepare_worker(
            &state,
            "fake",
            "task",
            "node",
            &mut request,
            json!({"nodeId":"node"}),
            &CancellationToken::new(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(metadata["mode"], "task_selected_history");
        assert_eq!(request["messages"].as_array().unwrap().len(), 4);
        assert_eq!(request["messages"][1]["content"], "original command");
        assert_eq!(request["messages"][2]["tool_calls"][0]["id"], "needed");
        assert!(!request
            .to_string()
            .contains("OBSERVER_ADVICE_MUST_NOT_REACH_WORKER"));
        assert!(!request.to_string().contains("FILLER"));
        let conn = crate::agent_service::open_db(&root).unwrap();
        let actors: Vec<String> = conn
            .prepare("SELECT actor FROM agent_request_contexts ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(actors, vec!["organizer", "organizer"]);
        drop(conn);
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}
