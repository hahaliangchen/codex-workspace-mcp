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

/// Persist Organizer-authored notes and the continuation plan as shared-history
/// events, then save their window references in the same transaction. Bodies
/// are stored unchanged and replayed from history on subsequent requests.
async fn save_with_organizer_material(
    state: &AgentServiceState,
    task: &str,
    actor: &str,
    scope: &str,
    mut window: Window,
    notes: Vec<Value>,
    plan: String,
) -> Result<(Vec<i64>, i64)> {
    let root = state.workspace.root().to_path_buf();
    let task = task.to_owned();
    let actor = actor.to_owned();
    let scope = scope.to_owned();
    tokio::task::spawn_blocking(move || -> Result<(Vec<i64>, i64)> {
        let mut conn = crate::agent_service::open_db(&root)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS agent_context_windows(task_id TEXT NOT NULL,actor TEXT NOT NULL,scope TEXT NOT NULL,through_seq INTEGER NOT NULL,selected_refs TEXT NOT NULL,PRIMARY KEY(task_id,actor,scope))")?;
        let mut note_refs = Vec::with_capacity(notes.len());
        for note in notes {
            let content = note["content"]
                .as_str()
                .context("organizer context note content must be a string")?;
            let sources = note["source_event_seqs"]
                .as_array()
                .context("organizer context note source_event_seqs must be an array")?;
            let mut source_event_seqs = BTreeSet::new();
            for source in sources {
                let source = source
                    .as_i64()
                    .context("context note source references must be event sequence numbers")?;
                ensure!(source >= 0, "context note source references must be non-negative");
                let exists=!crate::session_history::load_events(&tx,&task,&crate::session_history::EventQuery{exact:Some(source),..Default::default()})?.is_empty();
                ensure!(exists, "context note references missing event {source}");
                source_event_seqs.insert(source);
            }
            ensure!(!source_event_seqs.is_empty(), "context notes must cite at least one source event");
            let message = json!({"role":"user","name":"organizer_context_note","content":content});
            let note_ref = crate::agent_service::append_event_tx(
                &tx,
                &task,
                "organizer/context_note",
                &json!({"actor":"organizer","message":message,"source_event_seqs":source_event_seqs}),
                None,
            )?;
            window.selected.insert(note_ref);
            note_refs.push(note_ref);
        }
        let message = json!({
            "role":"user",
            "name":"organizer_continuation_plan",
            "content":plan
        });
        let plan_ref = crate::agent_service::append_event_tx(
            &tx,
            &task,
            "organizer/continuation_plan",
            &json!({"actor":"organizer","message":message}),
            None,
        )?;
        window.selected.insert(plan_ref);
        let continuation_plan_ref = Some(plan_ref);
        window.through = window.through.max(plan_ref);
        let selection = json!({
            "event_seqs":window.selected,
            "message_slices":window.message_slices,
            "continuation_plan_ref":continuation_plan_ref
        });
        tx.execute(
            "INSERT INTO agent_context_windows(task_id,actor,scope,through_seq,selected_refs) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(task_id,actor,scope) DO UPDATE SET through_seq=excluded.through_seq,selected_refs=excluded.selected_refs WHERE excluded.through_seq>=agent_context_windows.through_seq",
            params![task,actor,scope,window.through,selection.to_string()],
        )?;
        tx.commit()?;
        Ok((note_refs, plan_ref))
    })
    .await?
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

    fn slices(&self, slices: Vec<MessageSlice>) -> Result<Vec<MessageSlice>> {
        slices
            .into_iter()
            .map(|(seq, index, start, end)| {
                let anchor = self
                    .anchors
                    .get(&seq)
                    .context("message slice references an unavailable event")?;
                // Existing anchor/index pairs retain their saved-window semantics.
                let (anchor, index) = if *anchor == seq {
                    (seq, index)
                } else {
                    *self
                        .message_indexes
                        .get(&seq)
                        .and_then(|messages| messages.get(index))
                        .context("message slice points to a missing original event message")?
                };
                Ok((anchor, index, start, end))
            })
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
    if view
        .messages
        .get(1)
        .is_some_and(|m| m["name"] == "organizer")
    {
        return Ok(view
            .messages
            .iter()
            .zip(&view.sources)
            .filter(|(_, s)| s.is_none_or(|s| selected.contains(&s)))
            .map(|(m, _)| m.clone())
            .collect());
    }
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
fn select_held_context_tool() -> Value {
    json!({"type":"function","function":{"name":"select_held_context","description":"After inspecting the saved raw context through read_session_history, choose original event references and any exact text ranges needed for the active task. You may create concise Organizer-authored context notes when several completed tool records or long sources can be usefully organized; each note must cite its original source event sequence numbers. Notes are new immutable history messages, not replacements for raw history. Do not infer visual content from a screenshot reference, metadata or another actor's description: a visual conclusion requires your own successful view_image call with image content actually supplied. State the next Organizer round's task and missing facts in continuation_plan. You may select the original event ID of a tool call or result; the host includes the whole native exchange once before checking capacity. Event sequence numbers remain unchanged. The host restores choices and enforces capacity; it does not choose or rewrite context for you.","parameters":{"type":"object","properties":{"event_seqs":{"type":"array","maxItems":2048,"items":{"type":"integer","minimum":0}},"message_slices":{"type":"array","description":"Optional exact excerpts for selected event messages. Messages not listed here remain whole. Do not slice assistant messages containing tool_calls; their exchanges stay intact.","items":{"type":"object","properties":{"event_seq":{"type":"integer","minimum":0},"message_index":{"type":"integer","minimum":0,"description":"Zero-based message index within the referenced original event (normally 0 for a tool result). Existing exchange-anchor indexes remain supported."},"start_char":{"type":"integer","minimum":0},"end_char":{"type":"integer","minimum":0,"description":"Exclusive Unicode-character offset within that message's content."}},"required":["event_seq","message_index","start_char","end_char"]}},"context_notes":{"type":"array","maxItems":32,"description":"Optional concise notes you authored to organize important information or completed tool exchanges. Preserve raw history unchanged. Cite the original event sequence(s) used; these provenance references do not replace event_seqs.","items":{"type":"object","properties":{"content":{"type":"string","minLength":1,"description":"Your exact note text. Attribute claims and uncertainty; only report visual conclusions you personally made after successfully calling view_image and receiving image content."},"source_event_seqs":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"integer","minimum":0}}},"required":["content","source_event_seqs"]}},"continuation_plan":{"type":"string","description":"Your next-round task plan: what to continue, what information is missing, and what you will retrieve or do next."}},"required":["event_seqs","continuation_plan"]}}})
}
fn held_context_refs(view: &HistoryView, selected: &BTreeSet<i64>) -> BTreeSet<i64> {
    view.sources
        .iter()
        .flatten()
        .filter(|source| selected.contains(source))
        .copied()
        .collect()
}
fn assemble_held_context(
    view: &HistoryView,
    selected: &BTreeSet<i64>,
    slices: &[MessageSlice],
) -> Result<Vec<Value>> {
    let mut by_message = std::collections::BTreeMap::new();
    for &(event_seq, message_index, start, end) in slices {
        ensure!(
            selected.contains(&event_seq),
            "message slice references an unselected event"
        );
        ensure!(start < end, "message slice must be a non-empty range");
        ensure!(
            by_message
                .insert((event_seq, message_index), (start, end))
                .is_none(),
            "duplicate message slice"
        );
    }
    let mut indexes = std::collections::HashMap::<i64, usize>::new();
    let mut messages = Vec::new();
    for (message, source) in view.messages.iter().zip(&view.sources) {
        let Some(event_seq) = source else {
            messages.push(message.clone());
            continue;
        };
        let index = indexes.entry(*event_seq).or_default();
        let message_index = *index;
        *index += 1;
        if !selected.contains(event_seq) {
            continue;
        }
        let mut restored = message.clone();
        if let Some(&(start, end)) = by_message.get(&(*event_seq, message_index)) {
            ensure!(
                message["tool_calls"]
                    .as_array()
                    .is_none_or(|calls| calls.is_empty()),
                "assistant tool calls must remain complete"
            );
            restored = slice_message(&restored, start, end)?;
        }
        messages.push(restored);
    }
    ensure!(
        by_message
            .keys()
            .all(|(seq, index)| indexes.get(seq).is_some_and(|count| index < count)),
        "message slice points to a missing catalog message"
    );
    Ok(messages)
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
fn organizer_compaction_body(
    model: &str,
    context_reference: &Value,
    estimated_input_tokens: usize,
    input_budget: usize,
) -> Value {
    let system = "The host reports that your held context exceeded its input budget and saved the exact raw request in conversation history. The host does not send you that material here or choose what to keep. First inspect and organize the context already held by reading the supplied snapshot with read_session_history and include_context=true; page or search only as needed. Then choose original event references and exact text ranges to carry forward. You may write concise context_notes in your own words to organize important facts or completed tool exchanges; each note is a new immutable history message and must cite original source event sequence numbers. Raw tool calls and results remain unchanged and available for later lookup. Make a multi-round continuation_plan that preserves the active human goal and says what to do or retrieve next. For long text, ranges use zero-based Unicode-character offsets, start inclusive and end exclusive. Do not state visual findings based on metadata, image existence, or another actor's description: a visual conclusion requires your own successful view_image call with image content actually supplied. The host restores your choices in original event order, saves your notes and plan as history, and only enforces capacity. Reply with select_held_context when ready.";
    let input_content = json!({
        "host_capacity_notice":{"status":"over_capacity","estimated_input_tokens":estimated_input_tokens,"input_budget":input_budget},
        "context_snapshot_reference":context_reference
    });
    let body = json!({
        "model":model,
        "stream":false,
        "max_tokens":4096,
        "messages":[
            {"role":"system","content":system},
            {"role":"user","name":"held_context_catalog","content":input_content.to_string()}
        ],
        "tools":[crate::session_history::tool(),select_held_context_tool()],
        "tool_choice":"auto"
    });
    body
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
        )?
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
    let organizer_compaction = actor == "organizer" && input["target_role"] != "worker";
    let held_refs = if organizer_compaction {
        held_context_refs(&view, &selected)
    } else {
        selected.clone()
    };
    let selection_refs = selection_references(
        state,
        task,
        if organizer_compaction {
            &view
        } else {
            &originals
        },
        if organizer_compaction {
            &held_refs
        } else {
            &available
        },
        through,
    )
    .await?;
    let context_reference = if organizer_compaction {
        let mut snapshot_metadata = metadata.clone();
        snapshot_metadata["actor"] = json!(actor);
        snapshot_metadata["stage"] = json!("context_overflow_source");
        snapshot_metadata["context_scope"] = json!(scope);
        snapshot_metadata["context_window"] = json!({
            "status":"over_capacity",
            "estimated_input_tokens":before,
            "input_budget":limit
        });
        let trace =
            crate::request_context::record(state.workspace.root(), task, snapshot_metadata, body)
                .await;
        let reference = trace
            .as_ref()
            .map(crate::request_context::Trace::history_reference)
            .filter(|reference| reference["event_seq"].is_i64())
            .context("could not save over-capacity Organizer context for history retrieval")?;
        crate::request_context::finish(
            trace,
            "available_for_history",
            json!({"note":"Organizer must retrieve raw context through read_session_history"}),
        )
        .await;
        reference
    } else {
        Value::Null
    };
    let selection_through = context_reference["event_seq"].as_i64().unwrap_or(through);
    let mut selection_body = if organizer_compaction {
        organizer_compaction_body(model, &context_reference, before, limit)
    } else {
        let seed = view.messages.iter().take(2).cloned().collect::<Vec<_>>();
        json!({"model":model,"stream":false,"max_tokens":4096,"messages":seed,"tools":[crate::session_history::tool(),selection_tool()],"tool_choice":{"type":"function","function":{"name":"read_session_history"}}})
    };
    if let Some(effort) = state.reasoning_effort.as_deref() {
        selection_body["reasoning_effort"] = json!(effort);
    }
    let mut context_snapshot_read = !organizer_compaction;
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
                .context(if organizer_compaction {
                    "Organizer current-context compaction requires select_held_context"
                } else {
                    "Context selection requires read_session_history or select_context"
                })?;
            selection_body["messages"]
                .as_array_mut()
                .unwrap()
                .push(message.clone());
            for call in calls {
                let name = call["function"]["name"].as_str().unwrap_or("");
                let snapshot_read_before_call = context_snapshot_read;
                let args = serde_json::from_str::<Value>(
                    call["function"]["arguments"].as_str().unwrap_or("{}"),
                )?;
                let mut result = match name {
                    "select_held_context" if organizer_compaction => {
                        let values = args["event_seqs"]
                            .as_array()
                            .context("select_held_context.event_seqs must be an array")?;
                        let requested = values
                            .iter()
                            .map(|value| {
                                value
                                    .as_i64()
                                    .context("context reference must be an existing event sequence")
                            })
                            .collect::<Result<BTreeSet<_>>>()?;
                        let continuation_plan = args["continuation_plan"]
                            .as_str()
                            .filter(|plan| !plan.trim().is_empty())
                            .map(str::to_owned);
                        let context_notes = args["context_notes"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        let resolved = selection_refs.resolve(&requested);
                        if resolved.is_none() {
                            json!({"error_code":"REFERENCE_NOT_HELD","requested_refs":requested,"held_refs":selection_refs.anchors.keys().collect::<Vec<_>>()})
                        } else if !context_snapshot_read {
                            json!({"error_code":"RAW_CONTEXT_NOT_READ","instruction":"Read the saved raw request with read_session_history using context_snapshot_reference and include_context=true before selecting what to retain."})
                        } else if organizer_compaction && continuation_plan.is_none() {
                            json!({"error_code":"CONTINUATION_PLAN_REQUIRED","instruction":"State the next Organizer round's task and missing information to retrieve."})
                        } else {
                            let chosen = resolved.unwrap();
                            let requested_slices = args["message_slices"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            let slices_result = requested_slices.iter().map(|slice| {
                                Ok::<_, anyhow::Error>((
                                    slice["event_seq"].as_i64().context("message_slices.event_seq must be an integer")?,
                                    usize::try_from(slice["message_index"].as_u64().context("message_slices.message_index must be a non-negative integer")?)?,
                                    usize::try_from(slice["start_char"].as_u64().context("message_slices.start_char must be a non-negative integer")?)?,
                                    usize::try_from(slice["end_char"].as_u64().context("message_slices.end_char must be a non-negative integer")?)?,
                                ))
                            }).collect::<Result<Vec<_>>>();
                            let slices = match slices_result
                                .and_then(|slices| selection_refs.slices(slices))
                            {
                                Ok(slices) => slices,
                                Err(error) => {
                                    selection_body["messages"].as_array_mut().unwrap().push(
                                        json!({"role":"tool","tool_call_id":call["id"],"content":json!({"error_code":"INVALID_MESSAGE_SLICE","error":error.to_string()}).to_string()}),
                                    );
                                    selection_body["tool_choice"] = json!("auto");
                                    continue;
                                }
                            };
                            let candidate_result = assemble_held_context(&view, &chosen, &slices);
                            let candidate = match candidate_result {
                                Ok(candidate) => candidate,
                                Err(error) => {
                                    selection_body["messages"].as_array_mut().unwrap().push(
                                        json!({"role":"tool","tool_call_id":call["id"],"content":json!({"error_code":"INVALID_MESSAGE_SLICE","error":error.to_string()}).to_string()}),
                                    );
                                    selection_body["tool_choice"] = json!("auto");
                                    continue;
                                }
                            };
                            let mut candidate_body = body.clone();
                            let mut candidate = candidate;
                            for note in &context_notes {
                                let content = note["content"]
                                    .as_str()
                                    .filter(|content| !content.trim().is_empty())
                                    .context("context note content must be a non-empty string")?;
                                let sources = note["source_event_seqs"]
                                    .as_array()
                                    .context("context note source_event_seqs must be an array")?;
                                ensure!(
                                    !sources.is_empty(),
                                    "context notes must cite at least one source event"
                                );
                                for source in sources {
                                    source.as_i64().context("context note source references must be event sequence numbers")?;
                                }
                                candidate.push(json!({"role":"user","name":"organizer_context_note","content":content}));
                            }
                            if let Some(plan) = continuation_plan.as_ref() {
                                candidate.push(json!({"role":"user","name":"organizer_continuation_plan","content":plan}));
                            }
                            candidate_body["messages"] = json!(candidate);
                            if estimate(&candidate_body) > limit {
                                json!({"error_code":"SELECTION_OVER_CAPACITY","estimated_input_tokens":estimate(&candidate_body),"input_budget":limit,"instruction":"Refine your own current-context selection and keep the continuation plan concise. The host only enforces capacity."})
                            } else {
                                return Ok::<_, anyhow::Error>((
                                    candidate_body,
                                    chosen,
                                    slices,
                                    context_notes,
                                    continuation_plan,
                                    requested,
                                ));
                            }
                        }
                    }
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
                        if input["target_role"] == "worker" {
                            bounded["node_id"] = metadata["nodeId"].clone();
                            if !matches!(bounded["role"].as_str(), Some("worker" | "tool")) {
                                bounded["role"] = json!("tool");
                            }
                        }
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
                        if organizer_compaction
                            && bounded["event_seq"] == context_reference["event_seq"]
                            && bounded["include_context"] == true
                        {
                            context_snapshot_read = true;
                        }
                        crate::agent_service::emit(state.workspace.root(),task,"context/selection_read",json!({"actor":actor,"scope":scope,"arguments":bounded,"result":result})).await?;
                        result
                    }
                    "select_context" if !organizer_compaction => {
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
                                return Ok::<_, anyhow::Error>((
                                    candidate_body,
                                    chosen,
                                    Vec::new(),
                                    Vec::new(),
                                    None,
                                    requested,
                                ));
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
                        if organizer_compaction
                            && args["event_seq"] == context_reference["event_seq"]
                            && args["include_context"] == true
                        {
                            // The read was attempted, but its page is not in the
                            // Organizer's context. Keep the full source available
                            // and give Organizer a small, actionable capacity signal.
                            context_snapshot_read = snapshot_read_before_call;
                        }
                        result = json!({
                            "error_code":"ORGANIZER_CONTEXT_CAPACITY_EXCEEDED",
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
    let (rebuilt, mut chosen, chosen_slices, context_notes, continuation_plan, requested) = tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("context selection cancelled"),result=tokio::time::timeout(timeout,select)=>result.context("context selection timed out")??};
    *body = rebuilt;
    let mut through = through;
    let (context_note_refs, continuation_plan_ref) = if let Some(plan) = continuation_plan {
        let (note_refs, plan_ref) = save_with_organizer_material(
            state,
            task,
            actor,
            scope,
            Window {
                through,
                selected: chosen.clone(),
                message_slices: chosen_slices.clone(),
                continuation_plan_ref: None,
            },
            context_notes,
            plan,
        )
        .await?;
        chosen.extend(note_refs.iter().copied());
        chosen.insert(plan_ref);
        through = through.max(plan_ref);
        (note_refs, Some(plan_ref))
    } else {
        save(
            state,
            task,
            actor,
            scope,
            Window {
                through,
                selected: chosen.clone(),
                message_slices: chosen_slices,
                continuation_plan_ref: None,
            },
        )
        .await?;
        (Vec::new(), None)
    };
    let metadata = json!({"mode":if organizer_compaction{"organizer_current_context_compaction"}else{"task_selected_history"},"max_tokens":MAX_TOKENS,"estimated_before_tokens":before,"estimated_input_tokens":estimate(body),"requested_event_refs":requested,"selected_event_refs":chosen,"context_note_refs":context_note_refs,"continuation_plan_ref":continuation_plan_ref,"through_seq":through,"omitted_old_messages":references(&view).difference(&chosen).count()});
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
    let (calls, texts, through) = tokio::task::spawn_blocking(move || -> Result<_> {
        let conn = crate::agent_service::open_db(&root)?;
        let through = crate::session_history::load_events(
            &conn,
            &saved_task,
            &crate::session_history::EventQuery {
                newest: Some(1),
                ..Default::default()
            },
        )?
        .pop()
        .map(|event| event.seq)
        .unwrap_or(0);
        let rows = crate::session_history::load_events(
            &conn,
            &saved_task,
            &crate::session_history::EventQuery {
                kinds: &["tool/call", "assistant/message"],
                ..Default::default()
            },
        )?;
        let mut calls = std::collections::HashMap::new();
        let mut texts = std::collections::HashMap::new();
        for event in rows {
            let data = event.data()?;
            let (seq, kind) = (event.seq, event.kind);
            if kind == "tool/call" {
                if let Some(id) = data["callId"].as_str() {
                    calls.insert(id.to_owned(), seq);
                }
            } else {
                let text = data["message"]["content"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        data["message"]["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    });
                if !text.is_empty() {
                    texts.insert(text, seq);
                }
            }
        }
        Ok((calls, texts, through))
    })
    .await??;
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
    async fn organizer_view(root: &std::path::Path, input: &Value) -> HistoryView {
        let mut view = crate::session_history::organizer_view(root, "task", input, false)
            .await
            .unwrap();
        view.prepend(json!({"role":"system","content":"Organizer"}));
        view
    }

    fn tool_exchange(root: &std::path::Path, seq: i64, result: &str) {
        insert(
            root,
            seq,
            "assistant/message",
            json!({"actor":"worker","message":{
            "role":"assistant","content":null,"tool_calls":[
                {"id":"read","type":"function","function":{"name":"browser_read","arguments":"{}"}},
                {"id":"diagnostics","type":"function","function":{"name":"browser_diagnostics","arguments":"{}"}}
            ]}}),
        );
        insert(
            root,
            seq + 1,
            "tool/call",
            json!({"callId":"read","name":"browser_read","arguments":"{}"}),
        );
        insert(
            root,
            seq + 2,
            "tool/result",
            json!({"callId":"read","meta":{"result":result}}),
        );
        insert(
            root,
            seq + 3,
            "tool/call",
            json!({"callId":"diagnostics","name":"browser_diagnostics","arguments":"{}"}),
        );
        insert(
            root,
            seq + 4,
            "tool/result",
            json!({"callId":"diagnostics","meta":{"result":{"error":"original diagnostic failure"}}}),
        );
    }

    #[tokio::test]
    async fn original_call_and_result_ids_resolve_to_complete_exchanges_within_boundary() {
        let root = root();
        task(&root);
        tool_exchange(&root, 559, "loaded 8 pages");
        // The same call IDs may occur in a later invocation.
        tool_exchange(&root, 600, "later result");
        let state =
            crate::agent_service::tests::flow_test_state(&root, "127.0.0.1:1".parse().unwrap());
        let input = json!({"process":{"request_id":1}});
        let organizer = organizer_view(&root, &input).await;
        let process = crate::session_history::task_process(&root, "task", 0)
            .await
            .unwrap();
        let observer = crate::session_history::process_view(&process).unwrap();
        for view in [&organizer, &observer] {
            let refs = selection_references(&state, "task", view, &references(view), 604)
                .await
                .unwrap();
            for seq in 559..=563 {
                assert_eq!(
                    refs.resolve(&BTreeSet::from([seq])),
                    Some(BTreeSet::from([559]))
                );
            }
            assert_eq!(
                refs.resolve(&BTreeSet::from([559, 560, 561, 562, 563])),
                Some(BTreeSet::from([559]))
            );
            assert_eq!(
                refs.resolve(&BTreeSet::from([602, 561])),
                Some(BTreeSet::from([559, 600]))
            );
            assert!(refs.resolve(&BTreeSet::from([999])).is_none());
            let selected =
                assemble_held_context(view, &refs.resolve(&BTreeSet::from([561])).unwrap(), &[])
                    .unwrap();
            let native = selected
                .iter()
                .filter(|m| m["role"] == "assistant" || m["role"] == "tool")
                .collect::<Vec<_>>();
            assert_eq!(native.len(), 3);
            assert_eq!(
                native
                    .iter()
                    .map(|m| m["source"]["seq"].as_i64().unwrap())
                    .collect::<Vec<_>>(),
                vec![559, 561, 563]
            );
            assert_eq!(native[0]["tool_calls"].as_array().unwrap().len(), 2);
            assert_eq!(native[1]["content"], json!("loaded 8 pages").to_string());
            assert_eq!(
                native[2]["content"],
                json!({"error":"original diagnostic failure"}).to_string()
            );
            let bounded = selection_references(&state, "task", view, &references(view), 563)
                .await
                .unwrap();
            assert!(bounded.resolve(&BTreeSet::from([602])).is_none());
            assert_eq!(
                bounded.resolve(&BTreeSet::from([560])),
                Some(BTreeSet::from([559]))
            );
            let incomplete = selection_references(&state, "task", view, &references(view), 560)
                .await
                .unwrap();
            assert!(
                incomplete.resolve(&BTreeSet::from([559])).is_none(),
                "an earlier call cannot expose future results"
            );
            let not_held = selection_references(&state, "task", view, &BTreeSet::from([600]), 604)
                .await
                .unwrap();
            assert!(not_held.resolve(&BTreeSet::from([561])).is_none());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn raw_result_selection_checks_expanded_capacity_and_restores_excerpt_after_reload() {
        let root = root();
        task(&root);
        let result = format!(
            "{}已加载8页，仍需检查前两页。{}",
            "背景。".repeat(2000),
            "细节。".repeat(2000)
        );
        tool_exchange(&root, 559, &result);
        let wire_result = json!(result).to_string();
        let phrase = "已加载8页，仍需检查前两页。";
        let start = wire_result[..wire_result.find(phrase).unwrap()]
            .chars()
            .count();
        let end = start + phrase.chars().count();
        let originals = || {
            let conn = crate::agent_service::open_db(&root).unwrap();
            let mut stmt = conn.prepare("SELECT seq,timestamp,kind,data FROM agent_task_events WHERE task_id='task' AND seq<=563 ORDER BY seq").unwrap();
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        let before = originals();
        let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
                let captured = captured.clone();
                async move {
                    let mut captured = captured.lock().unwrap();
                    captured.push(body.clone());
                    let notice: Value = serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
                    let plan = "检查前两页；保留窗口和服务；不改源码。";
                    let message = match captured.len() {
                        1 => call("read_session_history", json!({"event_seq":notice["context_snapshot_reference"]["event_seq"],"include_context":true})),
                        2 => call("select_held_context", json!({"event_seqs":[561],"continuation_plan":plan})),
                        3 => {
                            assert!(body.to_string().contains("SELECTION_OVER_CAPACITY"), "capacity includes the completed exchange");
                            call("select_held_context", json!({"event_seqs":[notice["context_snapshot_reference"]["event_seq"]],"continuation_plan":plan}))
                        },
                        _ => {
                            assert!(body.to_string().contains("REFERENCE_NOT_HELD"), "a readable snapshot is still not a selectable message");
                            call("select_held_context", json!({"event_seqs":[561],"message_slices":[{"event_seq":561,"message_index":0,"start_char":start,"end_char":end}],"continuation_plan":plan}))
                        }
                    };
                    Json(json!({"choices":[{"message":message}]}))
                }
            }))).await.unwrap();
        });
        let input = json!({"process":{"request_id":1}});
        let state = crate::agent_service::tests::flow_test_state(&root, address);
        let view = organizer_view(&root, &input).await;
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
            json!({}),
            &CancellationToken::new(),
            Duration::from_secs(5),
            3000,
        )
        .await
        .unwrap();
        assert_eq!(metadata["requested_event_refs"], json!([561]));
        let plan_ref = metadata["continuation_plan_ref"].as_i64().unwrap();
        assert_eq!(metadata["selected_event_refs"], json!([559, plan_ref]));
        for _ in 0..2 {
            let state = crate::agent_service::tests::flow_test_state(&root, address);
            let view = organizer_view(&root, &input).await;
            request = body(&view);
            let restored = prepare_at_limit(
                &state,
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
                3000,
            )
            .await
            .unwrap();
            assert_eq!(restored["mode"], "rolling_window");
            let native = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "assistant" || m["role"] == "tool")
                .collect::<Vec<_>>();
            assert_eq!(native.len(), 3);
            assert_eq!(native[1]["source"]["seq"], 561);
            assert_eq!(native[1]["content"], phrase);
            assert_eq!(native[2]["source"]["seq"], 563);
            assert_eq!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|m| m["name"] == "organizer_continuation_plan")
                    .count(),
                1
            );
            assert!(estimate(&request) <= 3000);
        }
        assert_eq!(calls.lock().unwrap().len(), 4);
        assert_eq!(
            before,
            originals(),
            "the raw calls and returns remain byte-for-byte unchanged"
        );
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn organizer_compacts_held_context_and_keeps_active_goal_for_history_followup() {
        let root = root();
        task(&root);
        insert(
            &root,
            1,
            "user/message",
            json!({"turn":1,"message":{"content":"original goal"}}),
        );
        let original = format!(
            " \n前置背景 {} 必要的旧信息：只上传一次。后续细节 {}\n ",
            "旧段。".repeat(600),
            "更多。".repeat(600)
        );
        let kept = "必要的旧信息：只上传一次。";
        let excerpt_start = original.find(kept).unwrap();
        let excerpt_start = original[..excerpt_start].chars().count();
        let excerpt_end = excerpt_start + kept.chars().count();
        insert(
            &root,
            3,
            "worker/yield",
            json!({"turn":1,"output":{"id":"early","original_return":original.clone()}}),
        );
        insert(
            &root,
            5,
            "worker/yield",
            json!({"turn":1,"output":{"id":"noise","original_return":{"irrelevant":"noise ".repeat(10000)}}}),
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
        let model_excerpt = (excerpt_start, excerpt_end);
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
                                let notice: Value = serde_json::from_str(
                                    body["messages"][1]["content"].as_str().unwrap(),
                                ).unwrap();
                                assert!(!body.to_string().contains("必要的旧信息"), "host does not send the raw held context");
                                call("read_session_history", json!({
                                    "scope":"conversation",
                                    "task_id":"task",
                                    "event_seq":notice["context_snapshot_reference"]["event_seq"],
                                    "include_context":true,
                                    "char_offset":0,
                                    "max_chars":12000
                                }))
                            } else {
                                call("select_held_context", json!({
                                    "event_seqs":[11,9,3],
                                    "message_slices":[{"event_seq":3,"message_index":0,"start_char":model_excerpt.0,"end_char":model_excerpt.1}],
                                    "continuation_plan":"Continue checking the current file; retrieve missing upload evidence from conversation history if needed."
                                }))
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
        let input = json!({"human_request":"original goal","process":{"request_id":1}});
        let view = organizer_view(&root, &input).await;
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
            5000,
        )
        .await
        .unwrap();
        assert_eq!(metadata["mode"], "organizer_current_context_compaction");
        assert!(request.to_string().contains("original goal"));
        assert!(
            request
                .to_string()
                .contains("retrieve missing upload evidence")
        );
        assert_eq!(
            crate::session_history::original_text(
                request["messages"][2]["content"].as_str().unwrap()
            ),
            kept,
            "Organizer-selected original substring survives exactly"
        );
        assert!(request.to_string().contains("keep this correction"));
        assert!(!request.to_string().contains("noise noise"));
        let plan_ref = metadata["continuation_plan_ref"].as_i64().unwrap();
        assert!(plan_ref > 11);
        assert_eq!(metadata["selected_event_refs"], json!([3, 9, 11, plan_ref]));
        let saved_plan = load(&state, "task", "organizer", "request:1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved_plan.continuation_plan_ref, Some(plan_ref));
        let selection = requests.lock().unwrap();
        assert_eq!(
            selection.len(),
            2,
            "Organizer reads its saved raw input before selecting context"
        );
        assert_eq!(selection[0]["tools"].as_array().unwrap().len(), 2);
        assert_eq!(
            selection[0]["tools"][0]["function"]["name"],
            "read_session_history"
        );
        assert_eq!(
            selection[0]["tools"][1]["function"]["name"],
            "select_held_context"
        );
        assert_eq!(
            selection[0]["tool_choice"], "auto",
            "Organizer chooses whether and how to compact"
        );
        let selection_text = selection[0]["messages"][1]["content"].as_str().unwrap();
        let notice: Value = serde_json::from_str(selection_text).unwrap();
        assert_eq!(notice["host_capacity_notice"]["status"], "over_capacity");
        assert!(notice["context_snapshot_reference"]["event_seq"].is_i64());
        assert!(!selection[0].to_string().contains("original goal"));
        assert!(!selection[0].to_string().contains(&original));
        assert!(selection[1].to_string().contains("前置背景"));
        assert!(selection[1].to_string().contains("后续细节"));
        assert!(selection[1].to_string().contains("original goal"));
        drop(selection);
        insert(
            &root,
            100,
            "worker/yield",
            json!({"turn":2,"output":{"id":"new","original_return":"new actual result"}}),
        );
        let reloaded = crate::agent_service::tests::flow_test_state(&root, address);
        assert_eq!(
            load(&reloaded, "task", "organizer", "request:1")
                .await
                .unwrap()
                .unwrap()
                .through,
            plan_ref
        );
        let view = organizer_view(&root, &input).await;
        assert!(
            view.sources.contains(&Some(100)),
            "new history source not present: {:?}",
            view.sources
        );
        let original_pool = pool(&reloaded, "task", "organizer", &input, 100, &view)
            .await
            .unwrap();
        assert!(
            original_pool.sources.contains(&Some(100)),
            "new source missing from pool: {:?}",
            original_pool.sources
        );
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
            12000,
        )
        .await
        .unwrap();
        assert_eq!(metadata["mode"], "rolling_window");
        assert!(
            request.to_string().contains("new actual result"),
            "metadata={metadata}"
        );
        assert!(request.to_string().contains("必要的旧信息"));
        assert!(!request.to_string().contains("noise noise"));
        assert!(!request.to_string().contains("前置背景"));
        assert!(!request.to_string().contains("后续细节"));
        let plans = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["name"] == "organizer_continuation_plan")
            .collect::<Vec<_>>();
        assert_eq!(
            plans.len(),
            1,
            "the persisted plan is restored exactly once"
        );
        assert_eq!(
            plans[0]["content"],
            "Continue checking the current file; retrieve missing upload evidence from conversation history if needed."
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "new messages append without re-running context compaction"
        );
        save(
            &reloaded,
            "task",
            "organizer",
            "request:1",
            Window {
                through: 11,
                selected: BTreeSet::from([3]),
                message_slices: Vec::new(),
                continuation_plan_ref: None,
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
        assert_eq!(
            records, 3,
            "overflow source plus Organizer read and selection requests are auditable"
        );
        drop(conn);
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn oversized_history_read_and_selection_report_capacity_to_organizer() {
        let root = root();
        task(&root);
        insert(
            &root,
            1,
            "user/message",
            json!({"turn":1,"message":{"content":"检查已有上传结果，不能重复上传"}}),
        );
        let original = format!(
            " \n前置背景 {} 必要的旧信息：只上传一次。后续细节 {}\n ",
            "旧段。".repeat(600),
            "更多。".repeat(600)
        );
        insert(
            &root,
            3,
            "worker/yield",
            json!({"turn":1,"output":{"original_return":original}}),
        );
        // One message that cannot fit in the Organizer selector's whole window.
        // Its page reads are real session_history calls; only the model is faked.
        let synthetic = "合成超长历史材料。".repeat(150_000);
        insert(
            &root,
            5,
            "worker/yield",
            json!({"turn":1,"output":{"original_return":synthetic}}),
        );
        let pages = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = pages.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/v1/chat/completions",
                    post(move |Json(body): Json<Value>| {
                        let captured = captured.clone();
                        async move {
                            let mut captured = captured.lock().unwrap();
                            captured.push(body.clone());
                            let notice: Value = serde_json::from_str(
                                body["messages"][1]["content"].as_str().unwrap(),
                            )
                            .unwrap();
                            let reference = &notice["context_snapshot_reference"];
                            if captured.len() == 1 {
                                assert!(!body.to_string().contains("只上传一次"));
                                assert!(!body.to_string().contains("合成超长历史材料"));
                            }
                            let prior = body["messages"].to_string();
                            let message = if prior.contains("SELECTION_OVER_CAPACITY") {
                                call("select_held_context", json!({
                                    "event_seqs":[1,3,5],
                                    "message_slices":[
                                        {"event_seq":3,"message_index":0,"start_char":" \n前置背景 ".chars().count()+"旧段。".repeat(600).chars().count()+1,"end_char":" \n前置背景 ".chars().count()+"旧段。".repeat(600).chars().count()+1+"必要的旧信息：只上传一次。".chars().count()},
                                        {"event_seq":5,"message_index":0,"start_char":0,"end_char":"合成超长历史材料。".chars().count()}
                                    ],
                                    "continuation_plan":"保留只上传一次的回执和当前目标；下一轮按需要从 seq 5 的后续偏移继续读取材料。"
                                }))
                            } else if prior.contains("ORGANIZER_CONTEXT_CAPACITY_EXCEEDED") {
                                call("select_held_context", json!({
                                    "event_seqs":[1,3,5],
                                    "continuation_plan":"先保留当前目标和上传回执；下一轮继续按页读取超长历史材料。"
                                }))
                            } else {
                                let read_pages = body["messages"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter(|message| message["tool_calls"].as_array().is_some_and(|calls|
                                        calls.iter().any(|call| call["function"]["name"] == "read_session_history")))
                                    .count();
                                call("read_session_history", json!({
                                    "scope":"conversation",
                                    "task_id":"task",
                                    "event_seq":reference["event_seq"],
                                    "include_context":true,
                                    "char_offset":(read_pages * 12_000) as u64,
                                    "max_chars":12_000
                                }))
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
        let input =
            json!({"human_request":"检查已有上传结果，不能重复上传","process":{"request_id":1}});
        let view = organizer_view(&root, &input).await;
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
            json!({}),
            &CancellationToken::new(),
            Duration::from_secs(20),
            12_000,
        )
        .await
        .unwrap();
        assert_eq!(metadata["mode"], "organizer_current_context_compaction");
        assert!(request.to_string().contains("只上传一次"));
        assert!(request.to_string().contains("合成超长历史材料。"));
        assert!(request.to_string().contains("下一轮"));
        assert!(!request.to_string().contains("前置背景"));
        assert!(!request.to_string().contains("后续细节"));
        assert!(!request.to_string().contains(&synthetic));
        let plan_ref = metadata["continuation_plan_ref"].as_i64().unwrap();
        assert!(plan_ref > 5);
        assert_eq!(metadata["selected_event_refs"], json!([1, 3, 5, plan_ref]));
        assert_eq!(
            load(&state, "task", "organizer", "request:1")
                .await
                .unwrap()
                .unwrap()
                .continuation_plan_ref,
            Some(plan_ref)
        );
        let captured = pages.lock().unwrap();
        assert!(
            captured.len() >= 8,
            "read enough pages to approach the hard window: {}",
            captured.len()
        );
        assert!(
            captured.iter().any(|body| body
                .to_string()
                .contains("ORGANIZER_CONTEXT_CAPACITY_EXCEEDED")),
            "Organizer receives an explicit notice when a history page cannot fit"
        );
        assert!(
            captured
                .iter()
                .any(|body| body.to_string().contains("SELECTION_OVER_CAPACITY")),
            "Organizer receives capacity feedback when its selected context still cannot fit"
        );
        drop(captured);
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
        let mut state = crate::agent_service::tests::flow_test_state(&root, address);
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
        assert_eq!(
            assemble(&view, &view, &BTreeSet::from([7]), &[]).unwrap(),
            messages
        );
        assert_eq!(
            assemble(&view, &view, &BTreeSet::new(), &[]).unwrap(),
            messages[..2]
        );
    }

    #[test]
    fn organizer_can_keep_an_exact_unicode_excerpt_without_renumbering_source() {
        let full = format!(
            "前文{}中段关键事实：只上传一次。{}后文",
            "背景。".repeat(700),
            "细节。".repeat(700)
        );
        let view = HistoryView {
            messages: vec![
                json!({"role":"system","content":"Organizer"}),
                json!({"role":"user","content":full,"source":{"seq":8}}),
            ],
            sources: vec![None, Some(8)],
        };
        let phrase = "中段关键事实：只上传一次。";
        let start = full.find(phrase).unwrap();
        let start = full[..start].chars().count();
        let end = start + phrase.chars().count();
        let selected =
            assemble_held_context(&view, &BTreeSet::from([8]), &[(8, 0, start, end)]).unwrap();
        assert_eq!(selected[1]["content"], phrase);
        assert_eq!(selected[1]["source"]["seq"], 8);
        assert_eq!(
            selected[1]["source"]["selected_excerpt"]["start_char"],
            start
        );
        assert_eq!(selected[1]["source"]["selected_excerpt"]["end_char"], end);
    }

    #[tokio::test]
    async fn over_context_capacity_preserves_original_returns_with_role_specific_history_routing() {
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
                "worker/yield",
                json!({"turn":1,"output":{"original_return":"UNRELATED_FILLER ".repeat(42000)}}),
            );
            let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
            let seen = captured.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener,axum::Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>| {
                let seen=seen.clone();async move {
                    let mut seen=seen.lock().unwrap();seen.push(body.clone());
                    if body["tools"].as_array().unwrap().iter().any(|t|t["function"]["name"]=="select_held_context") {
                        let message=if seen.len()==1 {
                            let notice:Value=serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
                            assert!(!body.to_string().contains("OLD_NEEDED_RECEIPT"));
                            call("read_session_history",json!({"scope":"conversation","task_id":"task","event_seq":notice["context_snapshot_reference"]["event_seq"],"include_context":true,"query":"OLD_NEEDED_RECEIPT","max_chars":12000}))
                        } else {
                            assert!(body.to_string().contains("OLD_NEEDED_RECEIPT"),"Organizer reads the saved raw context");
                            call("select_held_context",json!({"event_seqs":[1,3],"continuation_plan":"Continue the current review; read missing facts from history."}))
                        };
                        return Json(json!({"choices":[{"message":message}]})).into_response();
                    }
                    if body["tools"].as_array().unwrap().iter().any(|t|t["function"]["name"]=="select_context") {
                        let message=if seen.len()==1 {call("read_session_history",json!({"query":"OLD_NEEDED_RECEIPT"}))}else{call("select_context",json!({"event_seqs":[1,3]}))};
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
                assert_eq!(decision.decision["action"], "finish");
            } else {
                let history = crate::session_history::task_process(&root, "task", 1)
                    .await
                    .unwrap();
                let raw=crate::agent_service::observer_json_response(&state,"fake","review",json!({"stage":"retrospective","identity":{"request_id":1},"request":{"goal":"check existing upload"},"task_history":history}),1000,"task",json!({"turn":1})).await.unwrap();
                assert_eq!(raw, "reviewed original receipt");
            }
            let expected_requests = 3;
            let expected_selections = 2;
            let seen = captured.lock().unwrap();
            assert_eq!(seen.len(), expected_requests);
            if actor == "organizer" {
                let input_content = seen[0]["messages"][1]["content"].as_str().unwrap();
                let notice: Value = serde_json::from_str(input_content).unwrap();
                assert_eq!(notice["host_capacity_notice"]["status"], "over_capacity");
                assert!(!seen[0].to_string().contains("OLD_NEEDED_RECEIPT"));
                assert!(
                    seen[1].to_string().contains("OLD_NEEDED_RECEIPT"),
                    "Organizer retrieved the raw context itself"
                );
            }
            if actor == "observer" {
                let header = crate::session_history::parse_plain_context(
                    seen[0]["messages"][1]["content"].as_str().unwrap(),
                );
                let history = &header["task_history"];
                assert_eq!(history["coverage"], "complete");
                assert!(history.get("omitted_event_refs").is_none());
                assert_eq!(history["history_reference"]["task_id"], "task");
                let omitted = crate::session_history::read(
                    &root,
                    "task",
                    &json!({"scope":"conversation","task_id":"task","event_seq":5}),
                )
                .await
                .unwrap();
                assert!(
                    omitted.to_string().contains("UNRELATED_FILLER"),
                    "omitted originals remain readable rather than being discarded"
                );
            }
            drop(seen);
            let conn = crate::agent_service::open_db(&root).unwrap();
            let requests: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_request_contexts WHERE actor=?1",
                    [actor],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                requests,
                expected_requests as i64 + if actor == "organizer" { 1 } else { 0 }
            );
            let selections:i64=conn.query_row("SELECT COUNT(*) FROM agent_request_contexts WHERE json_extract(metadata,'$.stage')='context_selection'",[],|r|r.get(0)).unwrap();
            assert_eq!(selections, expected_selections);
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
                            assert!(
                                !body
                                    .to_string()
                                    .contains("OBSERVER_ADVICE_MUST_NOT_REACH_WORKER")
                            );
                            let mut n = seen.lock().unwrap();
                            *n += 1;
                            let message = if *n == 1 {
                                call(
                                    "read_session_history",
                                    json!({"query":"needed own original","role":"observer"}),
                                )
                            } else {
                                call("select_context", json!({"event_seqs":[4]}))
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
        request["messages"][5] = crate::session_history::sourced_tool_result(
            &root,
            "task",
            request["messages"][5].clone(),
            "needed",
        )
        .await
        .unwrap();
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
        assert_eq!(metadata["requested_event_refs"], json!([4]));
        assert_eq!(metadata["selected_event_refs"], json!([3]));
        assert_eq!(request["messages"][3]["source"]["seq"], 4);
        assert_eq!(request["messages"].as_array().unwrap().len(), 4);
        assert_eq!(request["messages"][1]["content"], "original command");
        assert_eq!(request["messages"][2]["tool_calls"][0]["id"], "needed");
        assert!(
            !request
                .to_string()
                .contains("OBSERVER_ADVICE_MUST_NOT_REACH_WORKER")
        );
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
