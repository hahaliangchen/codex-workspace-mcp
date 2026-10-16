//! Task-directed rolling views of the shared history. Selection is performed by
//! the receiving role; the host only resolves references, orders originals and
//! checks capacity. The durable window contains references, not another history.
use crate::{
    agent_service::AgentServiceState,
    context_window::{HistoryView, MAX_TOKENS, estimate},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct Window {
    through: i64,
    selected: BTreeSet<i64>,
    message_slices: Vec<MessageSlice>,
    continuation_plan_ref: Option<i64>,
}
type MessageSlice = (i64, usize, usize, usize);
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
        row.map(|(through,refs)| -> Result<Window> {
            let refs: Value = serde_json::from_str(&refs)?;
            // Older saved windows stored only a JSON array of event references.
            let (mut selected, message_slices, continuation_plan_ref): (
                BTreeSet<i64>,
                Vec<MessageSlice>,
                Option<i64>,
            ) = if refs.is_array() {
                (serde_json::from_value(refs)?, Vec::new(), None)
            } else {
                (
                    serde_json::from_value(refs["event_seqs"].clone())?,
                    serde_json::from_value(refs["message_slices"].clone()).unwrap_or_default(),
                    refs["continuation_plan_ref"].as_i64(),
                )
            };
            if let Some(plan_ref) = continuation_plan_ref {
                selected.insert(plan_ref);
            }
            Ok(Window { through, selected, message_slices, continuation_plan_ref })
        }).transpose()
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
        let selection = json!({"event_seqs":window.selected,"message_slices":window.message_slices,"continuation_plan_ref":window.continuation_plan_ref});
        conn.execute("INSERT INTO agent_context_windows(task_id,actor,scope,through_seq,selected_refs) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(task_id,actor,scope) DO UPDATE SET through_seq=excluded.through_seq,selected_refs=excluded.selected_refs WHERE excluded.through_seq>=agent_context_windows.through_seq",params![task,actor,scope,window.through,selection.to_string()])?;
        Ok(())
    }).await?
}

fn references(view: &HistoryView) -> BTreeSet<i64> {
    view.sources.iter().flatten().copied().collect()
}

fn original_event_seq(message: &Value, task: &str) -> Option<i64> {
    let source = &message["source"];
    let owner = source["history_reference"]["task_id"]
        .as_str()
        .or_else(|| source["identity"]["task_id"].as_str());
    if owner.is_some_and(|owner| owner != task) {
        return None;
    }
    source["seq"].as_i64()
}

/// Public references are original event IDs. Internally a window still stores
/// the existing exchange anchor so old windows remain readable. Resolving an
/// original result only adds its protocol partners; it never renumbers history.
#[derive(Default)]
struct SelectionReferences {
    anchors: BTreeMap<i64, i64>,
    message_indexes: BTreeMap<i64, Vec<(i64, usize)>>,
}
impl SelectionReferences {
    fn resolve(&self, requested: &BTreeSet<i64>) -> Option<BTreeSet<i64>> {
        requested
            .iter()
            .map(|seq| self.anchors.get(seq).copied())
            .collect()
    }
}

async fn selection_references(
    state: &AgentServiceState,
    task: &str,
    view: &HistoryView,
    eligible: &BTreeSet<i64>,
    through: i64,
) -> Result<SelectionReferences> {
    // A result beyond an explicit observation boundary cannot be pulled in by
    // selecting its earlier call. Require the whole exchange to be in scope.
    let mut eligible = eligible.clone();
    for (message, anchor) in view.messages.iter().zip(&view.sources) {
        if original_event_seq(message, task).is_some_and(|seq| seq > through) {
            if let Some(anchor) = anchor {
                eligible.remove(anchor);
            }
        }
    }
    let mut refs = SelectionReferences::default();
    refs.anchors.extend(eligible.iter().map(|seq| (*seq, *seq)));
    let mut indexes = BTreeMap::<i64, usize>::new();
    let mut pending = BTreeMap::<String, (i64, i64)>::new();
    let mut exchanges = Vec::new();
    for (message, anchor) in view.messages.iter().zip(&view.sources) {
        let Some(anchor) = anchor.filter(|anchor| eligible.contains(anchor)) else {
            continue;
        };
        let index = indexes.entry(anchor).or_default();
        let original = original_event_seq(message, task);
        let native = message["role"] == "tool"
            || (message["role"] == "assistant"
                && message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty()));
        if let Some(seq) = original.filter(|seq| native || *seq == anchor) {
            if let Some(previous) = refs.anchors.insert(seq, anchor) {
                ensure!(
                    previous == anchor,
                    "history reference {seq} belongs to conflicting exchanges"
                );
            }
            refs.message_indexes
                .entry(seq)
                .or_default()
                .push((anchor, *index));
        }
        *index += 1;
        if message["role"] == "assistant" {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                if let Some(id) = call["id"].as_str() {
                    pending.insert(id.to_owned(), (anchor, original.unwrap_or(anchor)));
                }
            }
        } else if message["role"] == "tool" {
            if let Some(id) = message["tool_call_id"].as_str() {
                if let Some((call_anchor, start)) = pending.remove(id) {
                    ensure!(
                        call_anchor == anchor,
                        "tool result lost its exchange anchor"
                    );
                    exchanges.push((id.to_owned(), anchor, start, original.unwrap_or(anchor)));
                }
            }
        }
    }
    ensure!(
        pending.is_empty(),
        "selectable history has an incomplete tool exchange"
    );
    // tool/call audit events have their own IDs but no separate wire message.
    // Accept those IDs too, using call ID plus event interval (call IDs can be
    // reused later in the same conversation).
    if !exchanges.is_empty() {
        let root = state.workspace.root().to_path_buf();
        let task = task.to_owned();
        let aliases = tokio::task::spawn_blocking(move || -> Result<Vec<(i64, i64)>> {
            let conn = crate::agent_service::open_db(&root)?;
            let rows = crate::session_history::load_events(
                &conn,
                &task,
                &crate::session_history::EventQuery {
                    through: Some(through),
                    kinds: &["tool/call"],
                    ..Default::default()
                },
            )?;
            let mut aliases = Vec::new();
            for event in rows {
                let data = event.data()?;
                let seq = event.seq;
                for (id, anchor, start, end) in &exchanges {
                    if data["callId"] == id.as_str() && *start <= seq && seq <= *end {
                        aliases.push((seq, *anchor));
                    }
                }
            }
            Ok(aliases)
        })
        .await??;
        for (seq, anchor) in aliases {
            if let Some(previous) = refs.anchors.insert(seq, anchor) {
                ensure!(previous == anchor, "tool call reference {seq} is ambiguous");
            }
        }
    }
    Ok(refs)
}
fn assemble(
    view: &HistoryView,
    pool: &HistoryView,
    selected: &BTreeSet<i64>,
    slices: &[MessageSlice],
) -> Result<Vec<Value>> {
    let mut messages = view
        .messages
        .iter()
        .zip(&view.sources)
        .take(2)
        .map(|(m, _)| m.clone())
        .collect::<Vec<_>>();
    // Existing source references determine order; no number decorates a body.
    let mut indexes = std::collections::HashMap::<i64, usize>::new();
    let slices = slices
        .iter()
        .map(|&(seq, index, start, end)| ((seq, index), (start, end)))
        .collect::<std::collections::HashMap<_, _>>();
    let mut records = Vec::new();
    for (message, source) in pool.messages.iter().zip(&pool.sources) {
        let Some(seq) = source.filter(|seq| selected.contains(seq)) else {
            continue;
        };
        let index = indexes.entry(seq).or_default();
        let mut restored = message.clone();
        if let Some(&(start, end)) = slices.get(&(seq, *index)) {
            restored = slice_message(&restored, start, end)?;
        }
        *index += 1;
        records.push((seq, restored));
    }
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
    Ok(messages)
}
fn selection_tool() -> Value {
    json!({"type":"function","function":{"name":"select_context","description":"Finish rebuilding this task's context by choosing existing history event references. You may select the original ID of a tool call or its result; the host includes the complete native exchange once, in original order. Source message IDs and bodies remain unchanged.","parameters":{"type":"object","properties":{"event_seqs":{"type":"array","maxItems":2048,"items":{"type":"integer","minimum":0}}},"required":["event_seqs"]}}})
}
fn slice_message(message: &Value, start: usize, end: usize) -> Result<Value> {
    ensure!(start < end, "message slice must be a non-empty range");
    ensure!(
        message["tool_calls"]
            .as_array()
            .is_none_or(|calls| calls.is_empty()),
        "assistant tool calls must remain complete"
    );
    let content = message["content"]
        .as_str()
        .context("only string message content can be sliced")?;
    let chars = content.chars().collect::<Vec<_>>();
    ensure!(
        end <= chars.len(),
        "message slice exceeds the complete original content"
    );
    let mut restored = message.clone();
    restored["content"] = json!(chars[start..end].iter().collect::<String>());
    restored["source"]["selected_excerpt"] = json!({
        "start_char":start,
        "end_char":end,
        "original_char_count":chars.len()
    });
    Ok(restored)
}
async fn pool(
    state: &AgentServiceState,
    task: &str,
    _actor: &str,
    _input: &Value,
    through: i64,
    view: &HistoryView,
) -> Result<HistoryView> {
    let mut pool = crate::session_history::process_view(
        &crate::session_history::task_process(state.workspace.root(), task, 0).await?,
    )?;
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
        actor == "observer",
        "Only Observer uses retrospective history selection"
    );
    ensure!(
        view.messages.len() == view.sources.len(),
        "context provenance array mismatch"
    );
    let before = estimate(body);
    let through = metadata["history_through_seq"].as_i64().unwrap_or_else(|| {
        references(&view)
            .into_iter()
            .chain(
                view.messages
                    .iter()
                    .filter_map(|message| original_event_seq(message, task)),
            )
            .max()
            .unwrap_or(0)
    });
    let previous = load(state, task, actor, scope)
        .await?
        .filter(|window| window.through <= through);
    let mut selected = references(&view);
    let mut source_pool = None;
    if let Some(previous) = &previous {
        selected = previous.selected.clone();
        let originals = pool(state, task, actor, input, through, &view).await?;
        selected.extend(
            originals
                .sources
                .iter()
                .flatten()
                .filter(|seq| **seq > previous.through)
                .copied(),
        );
        body["messages"] = json!(assemble(
            &view,
            &originals,
            &selected,
            &previous.message_slices
        )?);
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
                    message_slices: previous
                        .as_ref()
                        .map(|window| window.message_slices.clone())
                        .unwrap_or_default(),
                    continuation_plan_ref: previous
                        .as_ref()
                        .and_then(|window| window.continuation_plan_ref),
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
    let mut empty = body.clone();
    empty["messages"] = json!(assemble(&view, &originals, &BTreeSet::new(), &[])?);
    ensure!(
        estimate(&empty) <= limit,
        "Current task instructions exceed context capacity; originals were not truncated"
    );
    let selection_refs = selection_references(state, task, &originals, &available, through).await?;
    let selection_through = through;
    let seed = view.messages.iter().take(2).cloned().collect::<Vec<_>>();
    let mut selection_body = json!({"model":model,"stream":false,"max_tokens":4096,"messages":seed,"tools":[crate::session_history::tool(),selection_tool()],"tool_choice":{"type":"function","function":{"name":"read_session_history"}}});
    if let Some(effort) = state.reasoning_effort.as_deref() {
        selection_body["reasoning_effort"] = json!(effort);
    }
    let select = async {
        loop {
            if let Some(Value::Array(messages)) = selection_body.get_mut("messages") {
                crate::session_history::order_context_messages(messages);
            }
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
            let (url, key) = (&state.observer_provider_url, &state.observer_api_key);
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
                let mut result = match name {
                    "read_session_history" => {
                        // Exact role inputs belong to this conversation too.
                        // Reading them helps choose evidence; it neither runs
                        // the old invocation again nor selects its entire body.
                        ensure!(
                            args["scope"].as_str().is_none_or(|s| s == "conversation")
                                && args["task_id"].as_str().is_none_or(|s| s == task),
                            "Context selection reads only this shared conversation"
                        );
                        let mut bounded = args.clone();
                        bounded["before_seq"] = json!(
                            bounded["before_seq"]
                                .as_i64()
                                .unwrap_or(selection_through + 1)
                                .min(selection_through + 1)
                        );
                        ensure!(
                            bounded["event_seq"]
                                .as_i64()
                                .is_none_or(|s| s <= selection_through),
                            "History reference is beyond this observation's boundary"
                        );
                        let result =
                            crate::session_history::read(state.workspace.root(), task, &bounded)
                                .await?;
                        crate::agent_service::emit(state.workspace.root(),task,"context/selection_read",json!({"actor":actor,"scope":scope,"arguments":bounded,"result":result})).await?;
                        result
                    }
                    "select_context" => {
                        let values = args["event_seqs"]
                            .as_array()
                            .context("select_context.event_seqs must be an array")?;
                        let requested = values
                            .iter()
                            .map(|v| {
                                v.as_i64()
                                    .context("context reference must be an existing event sequence")
                            })
                            .collect::<Result<BTreeSet<_>>>()?;
                        let resolved = selection_refs.resolve(&requested);
                        if resolved.is_none() {
                            json!({"error_code":"REFERENCE_NOT_AVAILABLE","requested_refs":requested})
                        } else {
                            let chosen = resolved.unwrap();
                            let candidate = assemble(&view, &originals, &chosen, &[])?;
                            let mut candidate_body = body.clone();
                            candidate_body["messages"] = json!(candidate);
                            if estimate(&candidate_body) > limit {
                                json!({"error_code":"SELECTION_OVER_CAPACITY","estimated_input_tokens":estimate(&candidate_body),"input_budget":limit})
                            } else {
                                return Ok::<_, anyhow::Error>((candidate_body, chosen, requested));
                            }
                        }
                    }
                    _ => {
                        json!({"error_code":"TOOL_NOT_AVAILABLE_IN_SELECTION_PHASE","requested_tool":name})
                    }
                };
                if name == "read_session_history" {
                    let tool_message = json!({
                        "role":"tool",
                        "tool_call_id":call["id"],
                        "content":result.to_string()
                    });
                    let mut projected = selection_body.clone();
                    projected["messages"]
                        .as_array_mut()
                        .unwrap()
                        .push(tool_message);
                    let projected_tokens = estimate(&projected);
                    let selection_budget = MAX_TOKENS.saturating_sub(8192);
                    if projected_tokens > selection_budget {
                        result = json!({
                            "error_code":"OBSERVER_CONTEXT_CAPACITY_EXCEEDED",
                            "event_seq":args["event_seq"],
                            "char_offset":args["char_offset"],
                            "requested_max_chars":args["max_chars"],
                            "estimated_input_tokens":projected_tokens,
                            "input_budget":selection_budget,
                            "content_returned":false,
                            "instruction":"This history page was not supplied because it would exceed your current context capacity. The original remains available at this event and offset. Select context to carry forward and write a multi-round plan; you may retry with a smaller page only if it fits. Do not claim to have read this page."
                        });
                    }
                }
                selection_body["messages"].as_array_mut().unwrap().push(
                    json!({"role":"tool","tool_call_id":call["id"],"content":result.to_string()}),
                );
            }
            selection_body["tool_choice"] = json!("auto");
        }
    };
    let (rebuilt, chosen, requested) = tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("context selection cancelled"),result=tokio::time::timeout(timeout,select)=>result.context("context selection timed out")??};
    *body = rebuilt;
    save(
        state,
        task,
        actor,
        scope,
        Window {
            through,
            selected: chosen.clone(),
            message_slices: Vec::new(),
            continuation_plan_ref: None,
        },
    )
    .await?;
    let metadata = json!({"mode":"task_selected_history","max_tokens":MAX_TOKENS,"estimated_before_tokens":before,"estimated_input_tokens":estimate(body),"requested_event_refs":requested,"selected_event_refs":chosen,"through_seq":through,"omitted_old_messages":references(&view).difference(&chosen).count()});
    crate::agent_service::emit(
        state.workspace.root(),
        task,
        "context/rebuilt",
        json!({"actor":actor,"scope":scope,"context_window":metadata}),
    )
    .await?;
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, routing::post};
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
            "worker/yield",
            json!({"turn":1,"output":{"original_return":"irrelevant ".repeat(10000)}}),
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
        let mut state = crate::agent_service::tests::test_state(&root, address);
        state.observer_provider_url = state.provider_url.clone();
        let process = crate::session_history::task_process(&root, "task", 0)
            .await
            .unwrap();
        let mut view = crate::session_history::process_view(&process).unwrap();
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
}
