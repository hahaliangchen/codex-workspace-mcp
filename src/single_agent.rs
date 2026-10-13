//! One durable task conversation and a bounded, read-only context subagent.
//! Both use the same request/tool loop. Only Main can launch a context subagent.
use crate::{
    agent_service::{self, AgentServiceState},
    context_window::estimate,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const INPUT_BUDGET: usize = crate::context_window::MAX_TOKENS - 24_576;
const SUMMARY_BUDGET: usize = 24_000;
const PAGE_CHARS: usize = 24_000;
const CONTEXT_ROUNDS: usize = 64;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Main,
    Context,
}
impl Mode {
    fn actor(self) -> &'static str {
        if self == Self::Main {
            "agent"
        } else {
            "context_agent"
        }
    }
    fn prompt(self) -> &'static str {
        if self == Self::Main {
            include_str!("../prompts/single_agent.md")
        } else {
            include_str!("../prompts/context_agent.md")
        }
    }
}
#[derive(Clone, Default, Deserialize, Serialize)]
struct Window {
    messages: Vec<Value>,
    previous_node: Option<String>,
    // Delivery ranges survive checkpoints. They describe supplied text, not comprehension.
    read_ranges: BTreeMap<i64, Vec<(usize, usize)>>,
    viewed_images: BTreeSet<String>,
    #[serde(default)]
    material_ranges: Vec<(usize, usize)>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Material {
    id: String,
    through: i64,
    requirements: Vec<i64>,
    messages: Vec<Value>,
}
struct Session<'a> {
    state: &'a AgentServiceState,
    task: &'a str,
    model: &'a str,
    turn: usize,
    cancel: &'a CancellationToken,
    ban_execution: bool,
    mode: Mode,
    scope: String,
    parent: Option<String>,
    material: Option<Material>,
    input_budget: usize,
}
enum LoopResult {
    Answer,
    Context(Value),
    Limit,
}

async fn db<T: Send + 'static>(
    root: &Path,
    f: impl FnOnce(&mut rusqlite::Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut conn = agent_service::open_db(&root)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_runtime_windows(
            task_id TEXT NOT NULL, scope TEXT NOT NULL, state TEXT NOT NULL,
            PRIMARY KEY(task_id,scope));
            CREATE TABLE IF NOT EXISTS agent_context_materials(
            task_id TEXT NOT NULL, id TEXT NOT NULL, data TEXT NOT NULL,
            PRIMARY KEY(task_id,id));",
        )?;
        f(&mut conn)
    })
    .await?
}
async fn save(s: &Session<'_>, w: &Window) -> Result<()> {
    let (task, scope, text) = (
        s.task.to_owned(),
        s.scope.clone(),
        serde_json::to_string(w)?,
    );
    db(s.state.workspace.root(), move |conn| {
        conn.execute("INSERT INTO agent_runtime_windows VALUES (?1,?2,?3) ON CONFLICT(task_id,scope) DO UPDATE SET state=excluded.state", params![task,scope,text])?;
        Ok(())
    }).await
}
async fn append(s: &Session<'_>, kind: &str, data: Value, surface: bool) -> Result<i64> {
    let (task, kind) = (s.task.to_owned(), kind.to_owned());
    db(s.state.workspace.root(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let seq =
            agent_service::append_event_tx(&tx, &task, &kind, &data, surface.then_some("append"))?;
        tx.commit()?;
        Ok(seq)
    })
    .await
}
fn sourced(mut message: Value, task: &str, seq: i64) -> Value {
    message["source"] = json!({"seq":seq,"history_reference":{"task_id":task,"event_seq":seq}});
    message
}
fn function(name: &str, description: &str, properties: Value, required: Value) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,
        "parameters":{"type":"object","properties":properties,"required":required}}})
}
fn tools(s: &Session<'_>) -> Vec<Value> {
    let mut tools = if s.mode == Mode::Main {
        agent_service::local_tools(
            &s.state.workspace,
            s.state.tool_catalog.as_deref(),
            s.ban_execution,
        )
        .into_iter()
        .filter(|t| {
            t["function"]["name"]
                .as_str()
                .is_some_and(|name| s.state.permission_mode.allows(name))
        })
        .collect()
    } else {
        agent_service::local_tools(&s.state.workspace, None, true)
            .into_iter()
            .filter(|t| t["function"]["name"] == "view_image")
            .collect::<Vec<_>>()
    };
    tools.push(crate::session_history::tool());
    tools.push(function("read_asset", "Read a stable asset_id without searching or repeating the original operation. Default restores the saved result in full; use char_offset/max_chars for explicit pages. Images select the saved pixels for the next AI request, without taking another screenshot. Read only if the needed content is no longer in your current context.",
        json!({"asset_id":{"type":"string"},"char_offset":{"type":"integer","minimum":0},"max_chars":{"type":"integer","minimum":1,"maximum":PAGE_CHARS}}),json!(["asset_id"])));
    tools.push(function("release_assets", "Explicitly replace selected asset bodies in this working context by their durable references. Original files/history stay saved. Must be called alone, after receiving the material in a model request. The host never releases an asset automatically.",
        json!({"asset_ids":{"type":"array","minItems":1,"items":{"type":"string"}},"reason":{"type":"string"}}),json!(["asset_ids","reason"])));
    tools.push(function("list_context_assets", "Inspect the asset references and loaded/reference-only states in your current context. Does not read asset files or alter context.",json!({}),json!([])));
    if s.mode == Mode::Main {
        tools.push(function("refresh_asset", "Explicitly re-read the current local file/range behind an existing file asset. Returns its current version as an asset; keeps the old snapshot and reference. Never replays commands or captures screenshots.",json!({"asset_id":{"type":"string"}}),json!(["asset_id"])));
        tools.push(function("compress_context", "Run a temporary read-only context subagent to review original user requirements and changes, read current context and missing history, and submit a compact context and next-step plan. Original history is preserved. It can use multiple AI rounds but cannot spawn another subagent.",
            json!({"reason":{"type":"string"}}), json!(["reason"])));
    } else {
        let asset_plan = json!({"type":"array","description":"Explicit asset decisions. Omitted assets keep their currently loaded contents and references. Use reference only when you decide the body is no longer needed.","items":{"type":"object","properties":{"asset_id":{"type":"string"},"mode":{"type":"string","enum":["keep","reference","excerpt"]},"event_seqs":{"type":"array","items":{"type":"integer"},"description":"For excerpt mode, retain only these already-loaded page deliveries; obtain event_seq from read_asset delivery_reference."}},"required":["asset_id","mode"]}});
        let paging = json!({"char_offset":{"type":"integer","minimum":0},"max_chars":{"type":"integer","minimum":1,"maximum":PAGE_CHARS}});
        tools.push(function("read_context_material", "Read the complete frozen working context in consecutive text pages. Continue at next_char_offset; omitted text has not been read.", paging.clone(), json!([])));
        let mut requirement = paging.clone();
        requirement["event_seq"] = json!({"type":"integer","minimum":0});
        tools.push(function("read_task_requirement", "Read one original user requirement from the supplied chronological requirement_event_seqs. Read every requirement, including all pages, before submitting context.", requirement, json!(["event_seq"])));
        tools.push(function("checkpoint_context", "Save your own partial analysis, evidence references, unread ranges and next reading plan. Replace this subagent's working context with that exact checkpoint and continue reading. Never spawns another agent.",
            json!({"checkpoint":{"type":"string","minLength":1},"asset_context":asset_plan}), json!(["checkpoint"])));
        tools.push(function("submit_context", "Finish context organization. Include all original requirement references. Tool summaries are your own descriptions; visual conclusions require your own delivered view_image evidence. Rejections leave the main context unchanged.",
            json!({"original_purpose":{"type":"string","minLength":1},"current_goal":{"type":"string","minLength":1},
                "requirement_changes":{"type":"string"},"constraints":{"type":"string"},"progress":{"type":"string"},
                "open_questions":{"type":"string"},"next_steps":{"type":"string","minLength":1},
                "tool_history":{"type":"string"},"asset_context":asset_plan,"source_event_seqs":{"type":"array","items":{"type":"integer"}},
                "visual_conclusions":{"type":"array","items":{"type":"object","properties":{"artifact_id":{"type":"string"},"conclusion":{"type":"string"}},"required":["artifact_id","conclusion"]}}}),
            json!(["original_purpose","current_goal","requirement_changes","constraints","progress","open_questions","next_steps","tool_history","source_event_seqs","visual_conclusions"])));
    }
    tools
}
fn visual_context(s: &Session<'_>, node: &str) -> Result<crate::visual_artifacts::VisualContext> {
    Ok(crate::visual_artifacts::VisualContext {
        turn: Some(s.turn),
        identity: json!({"task_id":s.task,"request_id":s.turn,"node_id":node,"runtime":"single_agent"}),
        allowed_artifact_ids: crate::visual_artifacts::task_artifact_ids(
            s.state.workspace.root(),
            s.task,
        )?,
        ..Default::default()
    })
}
fn request(s: &Session<'_>, messages: &[Value], definitions: &[Value]) -> Value {
    let mut wire = messages.to_vec();
    for message in &mut wire {
        if let Some(object) = message.as_object_mut() {
            object.remove("source");
            object.remove("viewed_by_context_agent");
            object.remove(crate::context_assets::REFERENCE);
            object.remove("_asset_state");
        }
    }
    let occupied = estimate(&json!({"messages":wire,"tools":definitions}));
    wire.push(json!({"role":"system","content":format!("Context capacity: estimated {occupied} input tokens; input budget {}. {}", s.input_budget,
        if s.mode == Mode::Main {"Use compress_context before running out of space. Oversized tool results remain available by history reference."}
        else {"Use checkpoint_context to preserve partial analysis and unread ranges before running out of space. Recursive compression is unavailable."})}));
    let mut body = json!({"model":s.model,"stream":true,"max_tokens":8192,"messages":wire,"tools":definitions,"parallel_tool_calls":false});
    if let Some(effort) = &s.state.reasoning_effort {
        body["reasoning_effort"] = json!(effort);
    }
    if s.state.fast_mode {
        body["service_tier"] = json!("fast");
    }
    body
}
async fn model_round(
    s: &Session<'_>,
    w: &mut Window,
    step: usize,
    definitions: &[Value],
) -> Result<(String, Value)> {
    let node = format!("request_{}", agent_service::uuid_like());
    let mut body = request(s, &w.messages, definitions);
    let mut visual = visual_context(s, &node)?;
    visual.identity["preparation_parent"] = json!(w.previous_node.as_ref().or(s.parent.as_ref()));
    // Supported routes materialize the durable image references directly.
    // Other routes need explicit selections so the visual adapter can report
    // unavailability or invoke its configured visual service.
    let selected_images = if s.state.visual.capability(&s.state.provider_name, s.model)
        == crate::visual_artifacts::ImageCapability::Supported
    {
        Vec::new()
    } else {
        w.messages
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter_map(|b| {
                (b["type"] == "image_ref")
                    .then(|| b["artifact_id"].as_str())
                    .flatten()
            })
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let dispatch = crate::visual_artifacts::prepare_request(
        s.state,
        s.mode.actor(),
        s.model,
        &visual,
        &selected_images,
        "Continue the current task",
        &mut body,
    )
    .await?;
    // An unavailable image may leave an empty transport-only user message.
    // Its reference remains durable and the adapter's delivery notice is kept.
    body["messages"]
        .as_array_mut()
        .unwrap()
        .retain(|m| !(m["role"] == "user" && m["content"].as_array().is_some_and(Vec::is_empty)));
    ensure!(
        estimate(&body) <= s.input_budget,
        "INPUT_OVER_CAPACITY: context remains saved; no request sent. Use smaller material pages or a context checkpoint before reaching capacity."
    );
    let trace = crate::request_context::record(s.state.workspace.root(), s.task,
        json!({"actor":s.mode.actor(),"stage":if s.mode==Mode::Main{"execution"}else{"context_organization"},
            "turn":s.turn,"step":step,"nodeId":node,"request_id":s.turn,"request_flow":true,
            "parent_node_id":s.parent,"previous_node_id":w.previous_node,"context_scope":s.scope,"visual_dispatch":dispatch}), &body).await
        .context("Cannot persist model input and flow node; request was not sent")?;
    w.previous_node = Some(node.clone());
    save(s, w).await?;
    agent_service::set_active_flow_node(s.state, s.task, s.turn, &node).await;
    let response = tokio::select! {
        biased;
        _ = s.cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
        result = tokio::time::timeout(Duration::from_secs(180), async {
            let response = s.state.client.post(format!("{}/chat/completions",s.state.provider_url.trim_end_matches('/')))
                .bearer_auth(&s.state.api_key).header("x-codex-visual-dispatch","task-owned").json(&body).send().await?;
            let response = crate::visual_probe::successful_response(response).await?;
            // The same stream decoder is used for both modes; child text stays
            // in its flow record and never becomes an execution-agent answer.
            if s.mode == Mode::Main {
                let (message, stats) = agent_service::consume_model_stream(response, s.state.workspace.root(), s.task, s.turn, step, s.cancel).await?;
                ensure!(stats["terminated"] == true, "model stream ended without completion");
                ensure!(!matches!(stats["finish_reason"].as_str(), Some("length" | "content_filter")), "model response was truncated or filtered; no tool was executed");
                return Ok(message);
            }
            let mut stream = crate::model_stream::ChatStream::new(crate::model_stream::Limits::WORKER, std::time::Instant::now());
            use futures::StreamExt;
            let mut bytes = response.bytes_stream();
            while !stream.received_done() {
                let Some(chunk) = bytes.next().await else { break };
                stream.push(&chunk?)?;
            }
            let completed = stream.finish()?;
            ensure!(completed.terminated, "model stream ended without completion");
            ensure!(!matches!(completed.stats["finish_reason"].as_str(), Some("length" | "content_filter")), "model response was truncated or filtered; no tool was executed");
            Ok::<_,anyhow::Error>(completed.message)
        }) => result.context("model request timed out").and_then(|result| result),
    };
    let status = if s.cancel.is_cancelled() {
        "cancelled"
    } else if response.is_ok() {
        "completed"
    } else {
        "failed"
    };
    trace.finish_checked(status, json!({"response":response.as_ref().ok(),"error":response.as_ref().err().map(|e|format!("{e:#}"))})).await?;
    let message = response?;
    // Only images originating in this subagent's view_image calls are candidates.
    let delivered: BTreeSet<String> = dispatch["images"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|image| image["artifact_id"].as_str().map(str::to_owned))
        .collect();
    if matches!(
        dispatch["status"].as_str(),
        Some("direct" | "unknown_capability")
    ) {
        for m in &w.messages {
            if m["viewed_by_context_agent"] == true {
                for b in m["content"].as_array().into_iter().flatten() {
                    if let Some(id) = b["artifact_id"]
                        .as_str()
                        .filter(|id| delivered.contains(*id))
                    {
                        w.viewed_images.insert(id.to_owned());
                    }
                }
            }
        }
    }
    let seq = append(
        s,
        if s.mode == Mode::Main {
            "assistant/message"
        } else {
            "context/assistant"
        },
        json!({"actor":s.mode.actor(),"turn":s.turn,"step":step,"nodeId":node,"message":message}),
        s.mode == Mode::Main,
    )
    .await?;
    w.messages.push(sourced(message.clone(), s.task, seq));
    save(s, w).await?;
    Ok((node, message))
}
fn page(text: &str, args: &Value) -> Value {
    let start = args["char_offset"].as_u64().unwrap_or(0) as usize;
    let limit = args["max_chars"]
        .as_u64()
        .unwrap_or(PAGE_CHARS as u64)
        .clamp(1, PAGE_CHARS as u64) as usize;
    let total = text.chars().count();
    let content: String = text.chars().skip(start).take(limit).collect();
    let end = start.saturating_add(content.chars().count());
    json!({"content":content,"char_offset":start,"total_chars":total,"returned_chars":end.saturating_sub(start),
        "next_char_offset":if end<total{Some(end)}else{None}})
}
fn fully_read(ranges: &[(usize, usize)], total: usize) -> bool {
    let mut ranges = ranges.to_vec();
    ranges.sort_unstable();
    let mut end = 0;
    for (start, finish) in ranges {
        if start > end {
            return false;
        }
        end = end.max(finish);
    }
    end >= total
}
async fn validate_summary(s: &Session<'_>, w: &Window, result: &Value) -> Result<()> {
    for field in [
        "original_purpose",
        "current_goal",
        "requirement_changes",
        "constraints",
        "progress",
        "open_questions",
        "next_steps",
        "tool_history",
    ] {
        let value = result[field]
            .as_str()
            .with_context(|| format!("{field} must be a string"))?;
        if matches!(field, "original_purpose" | "current_goal" | "next_steps") {
            ensure!(!value.trim().is_empty(), "{field} must not be empty");
        }
    }
    ensure!(
        estimate(result) <= SUMMARY_BUDGET.min(s.input_budget / 3),
        "SUMMARY_OVER_CAPACITY: refine the summary without losing goals and next steps"
    );
    let refs = result["source_event_seqs"]
        .as_array()
        .context("source_event_seqs must be an array")?
        .iter()
        .map(|v| {
            v.as_i64()
                .context("source reference must be an event sequence")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let material = s.material.as_ref().context("context material missing")?;
    ensure!(
        fully_read(
            &w.material_ranges,
            serde_json::to_string(&material.messages)?.chars().count()
        ),
        "CURRENT_CONTEXT_NOT_READ: read all pages of the frozen current context before submitting"
    );
    let (task, through, required, ranges) = (
        s.task.to_owned(),
        material.through,
        material.requirements.clone(),
        w.read_ranges.clone(),
    );
    // A subagent may cite a page it just read, using the delivery_reference
    // returned to it. Such a read is newer than the frozen main context.
    let own_deliveries: BTreeSet<i64> = w
        .messages
        .iter()
        .filter(|m| m["role"] == "tool" || m[crate::context_assets::REFERENCE].is_object())
        .filter_map(|m| m["source"]["seq"].as_i64())
        .collect();
    db(s.state.workspace.root(),move |conn| {
        for seq in &refs {
            ensure!((*seq<=through || own_deliveries.contains(seq)) && conn.query_row("SELECT EXISTS(SELECT 1 FROM agent_task_events WHERE task_id=?1 AND seq=?2)",params![task,seq],|r|r.get::<_,bool>(0))?, "source reference {seq} is neither frozen history nor a delivered result in this subagent context");
        }
        for seq in required {
            ensure!(refs.contains(&seq),"missing original user requirement reference {seq}");
            let raw:String=conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND seq=?2 AND kind='user/message'",params![task,seq],|r|r.get(0))?;
            ensure!(fully_read(ranges.get(&seq).map(Vec::as_slice).unwrap_or(&[]), raw.chars().count()), "USER_REQUIREMENT_NOT_READ: read all pages of event {seq}");
        }
        Ok(())
    }).await?;
    let mut asset_messages = material.messages.clone();
    asset_messages.extend(w.messages.iter().cloned());
    let retained = crate::context_assets::retained(&asset_messages, &result["asset_context"])?;
    ensure!(
        estimate(&json!({"summary":result,"assets":retained})) + 16_384 < s.input_budget,
        "ASSET_CONTEXT_OVER_CAPACITY: explicitly choose reference mode for assets you no longer need, or shorten your summary; no asset was automatically evicted"
    );
    for conclusion in result["visual_conclusions"]
        .as_array()
        .context("visual_conclusions must be an array")?
    {
        let id = conclusion["artifact_id"]
            .as_str()
            .context("visual conclusion needs artifact_id")?;
        ensure!(
            w.viewed_images.contains(id),
            "VISUAL_EVIDENCE_NOT_VIEWED: {id}"
        );
        ensure!(
            conclusion["conclusion"]
                .as_str()
                .is_some_and(|v| !v.trim().is_empty()),
            "visual conclusion is empty"
        );
    }
    Ok(())
}
async fn freeze(s: &Session<'_>, messages: &[Value]) -> Result<Material> {
    let (task, messages) = (s.task.to_owned(), messages.to_vec());
    db(s.state.workspace.root(),move |conn| {
        let through=conn.query_row("SELECT COALESCE(MAX(seq),-1) FROM agent_task_events WHERE task_id=?1",[&task],|r|r.get(0))?;
        let requirements = conn.prepare("SELECT seq FROM agent_task_events WHERE task_id=?1 AND kind='user/message' AND seq<=?2 ORDER BY seq")?
            .query_map(params![task,through],|r|r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        let material=Material{id:agent_service::uuid_like(),through,requirements,messages};
        conn.execute("INSERT INTO agent_context_materials VALUES (?1,?2,?3)",params![task,material.id,serde_json::to_string(&material)?])?;
        Ok(material)
    }).await
}
fn child_start(task: &str, material: &Material) -> Value {
    json!({"role":"user","content":json!({"task_id":task,"material_id":material.id,"history_through_seq":material.through,
        "requirement_event_seqs":material.requirements,"instruction":"Read every user requirement in order, organize the current material, retrieve missing history, then submit_context. Use checkpoint_context for your own multi-round reading."}).to_string()})
}
async fn business_tool(
    s: &Session<'_>,
    node: &str,
    name: &str,
    args: Value,
    call_id: &str,
) -> Result<Value> {
    if name == "read_session_history" {
        let mut args = args;
        if s.mode == Mode::Context {
            ensure!(
                args["task_id"].as_str().is_none_or(|task| task == s.task),
                "context subagent may only read this conversation"
            );
            ensure!(
                args["scope"]
                    .as_str()
                    .is_none_or(|scope| scope == "conversation"),
                "context subagent cannot search other tasks"
            );
            args["task_id"] = json!(s.task);
            let boundary = s.material.as_ref().unwrap().through;
            ensure!(
                args["event_seq"].as_i64().is_none_or(|seq| seq <= boundary),
                "history reference is beyond the frozen snapshot"
            );
            args["before_seq"] = json!(
                args["before_seq"]
                    .as_i64()
                    .unwrap_or(boundary + 1)
                    .min(boundary + 1)
            );
        }
        return crate::session_history::read(s.state.workspace.root(), s.task, &args).await;
    }
    ensure!(
        s.mode == Mode::Main || name == "view_image",
        "context subagent tool is unavailable: {name}"
    );
    ensure!(
        s.state.permission_mode.allows(name),
        "permission_denied: {name}"
    );
    if crate::workspace_changes::is_mutation(name) {
        crate::workspace_changes::execute_tracked(
            s.state.clone(),
            s.task.to_owned(),
            s.turn,
            name.to_owned(),
            args,
            s.cancel.clone(),
        )
        .await
    } else if crate::browser_control::is_tool(name) {
        let mut visual = visual_context(s, node)?;
        visual.source_tool_call_id = call_id.to_owned();
        visual.source_event_id = format!("{}:tool:{call_id}", s.task);
        crate::browser_control::execute_scoped(&s.state.workspace, &visual, name, &args).await
    } else {
        agent_service::execute_tool(
            s.state.workspace.clone(),
            s.state.tool_catalog.as_deref(),
            name,
            args,
        )
        .await
    }
}
async fn run_loop(s: &Session<'_>, w: &mut Window, max_rounds: usize) -> Result<LoopResult> {
    let definitions = tools(s);
    for step in 1..=max_rounds {
        ensure!(!s.cancel.is_cancelled(), "cancelled");
        let prefix_len = w.messages.len();
        let (node, message) = model_round(s, w, step, &definitions).await?;
        let calls = crate::format_translate::collect_all_tool_calls_from_openai_chat(&message);
        if calls.is_empty() {
            if s.mode == Mode::Main {
                ensure!(
                    message["content"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty()),
                    "model returned an empty answer"
                );
                return Ok(LoopResult::Answer);
            }
            w.messages.push(json!({"role":"user","content":"Context organization is not complete until submit_context succeeds. Continue reading, checkpoint, or submit the structured result."}));
            save(s, w).await?;
            continue;
        }
        let mut ids = BTreeSet::new();
        ensure!(
            calls
                .iter()
                .all(|c| !c.call_id.is_empty() && ids.insert(c.call_id.clone())),
            "duplicate or missing tool call id"
        );
        let call_count = calls.len();
        let mut replacement = None;
        let mut checkpoint = None;
        let mut submitted = None;
        let mut image_messages = Vec::new();
        let mut released_assets = None;
        for call in calls {
            let kind = if s.mode == Mode::Main {
                "tool/call"
            } else {
                "context/tool_call"
            };
            append(s,kind,json!({"actor":s.mode.actor(),"turn":s.turn,"step":step,"flowNodeId":node,"callId":call.call_id,"name":call.name,"arguments":call.arguments}),false).await?;
            let mut material_source: Option<(String, Value)> = None;
            let mut refreshed_from: Option<Value> = None;
            let output=async {
                let args:Value=serde_json::from_str(&call.arguments).context("invalid tool arguments JSON")?;
                // Mode enforcement is independent of the model's advertised schema.
                ensure!(definitions.iter().any(|tool|tool["function"]["name"]==call.name),"TOOL_UNAVAILABLE: {} (context subagents cannot recurse)",call.name);
                if matches!(call.name.as_str(),"compress_context"|"checkpoint_context"|"submit_context"|"release_assets") {
                    ensure!(call_count==1,"context-changing tools must be called alone so every read result reaches a subsequent AI request");
                }
                match call.name.as_str() {
                    "compress_context" => {
                        ensure!(s.mode==Mode::Main,"RECURSIVE_COMPRESSION_FORBIDDEN");
                        let material=freeze(s,&w.messages[..prefix_len]).await?;
                        let child=Session{state:s.state,task:s.task,model:s.model,turn:s.turn,cancel:s.cancel,ban_execution:true,
                            mode:Mode::Context,scope:format!("context:{}",material.id),parent:Some(node.clone()),material:Some(material.clone()),input_budget:s.input_budget};
                        let mut child_window=Window{messages:vec![json!({"role":"system","content":Mode::Context.prompt()}),child_start(s.task,&material)],..Default::default()};
                        save(&child,&child_window).await?;
                        let result=Box::pin(run_loop(&child,&mut child_window,CONTEXT_ROUNDS)).await?;
                        if let LoopResult::Context(summary)=result {
                            let mut originals=material.messages.clone();
                            originals.extend(child_window.messages.iter().cloned());
                            let retained=crate::context_assets::retained(&originals,&summary["asset_context"])?;
                            let mut proposed=vec![json!({"role":"system","content":Mode::Main.prompt()}),json!({"role":"user","name":"context_summary","content":summary.to_string()})];
                            proposed.extend(retained.iter().cloned());
                            proposed.extend_from_slice(&w.messages[prefix_len..]);
                            ensure!(estimate(&request(s,&proposed,&definitions))+4096 < s.input_budget,"COMPACTED_CONTEXT_OVER_CAPACITY: original context retained; explicitly release unneeded bodies or choose smaller excerpts");
                            replacement=Some((material.id.clone(),summary,retained));
                            Ok(json!({"organized":true,"material_id":material.id,"context_scope":child.scope}))
                        } else { anyhow::bail!("CONTEXT_ROUND_LIMIT: partial analysis is saved in {}; main context was not replaced",child.scope) }
                    },
                    "list_context_assets" => Ok(crate::context_assets::catalog(&w.messages)),
                    "release_assets" => {
                        let ids=args["asset_ids"].as_array().context("asset_ids must be an array")?.iter()
                            .map(|id|id.as_str().map(str::to_owned).context("asset id must be a string")).collect::<Result<Vec<_>>>()?;
                        ensure!(!ids.is_empty(),"asset_ids must not be empty");
                        let mut candidate=w.messages.clone();
                        crate::context_assets::release(&mut candidate,&ids)?;
                        released_assets=Some(ids.clone());
                        Ok(json!({"released_assets":ids,"history_and_files_preserved":true}))
                    },
                    "read_asset" => {
                        let id=args["asset_id"].as_str().context("asset_id is required")?;
                        let held=w.messages.iter().any(|m|m[crate::context_assets::REFERENCE]["asset_id"]==id);
                        let through=if held {None}else{s.material.as_ref().map(|m|m.through)};
                        let root=s.state.workspace.root().to_path_buf(); let task=s.task.to_owned(); let read_args=args.clone();
                        let mut result=tokio::task::spawn_blocking(move||crate::context_assets::read(&root,&task,&read_args,through)).await??;
                        if let Some(image_id)=result["asset_reference"]["artifact_id"].as_str() {
                            let viewed=business_tool(s,&node,"view_image",json!({"artifact_id":image_id,"original":true}),&call.call_id).await?;
                            result["artifact_id"]=viewed["artifact_id"].clone();
                            result["image_delivery"] = json!("selected_for_next_model_request");
                        }
                        Ok(result)
                    },
                    "refresh_asset" => {
                        let id=args["asset_id"].as_str().context("asset_id is required")?.to_owned();
                        let root=s.state.workspace.root().to_path_buf(); let task=s.task.to_owned();
                        let (previous,(name,read_args))=tokio::task::spawn_blocking(move||->Result<_> {
                            Ok((crate::context_assets::reference(&root,&task,&id,None)?,crate::context_assets::refresh_request(&root,&task,&id)?))
                        }).await??;
                        refreshed_from=Some(previous);
                        material_source=Some((name.clone(),read_args.clone()));
                        business_tool(s,&node,&name,read_args,&call.call_id).await
                    },
                    "read_context_material" => Ok(page(&serde_json::to_string(&s.material.as_ref().context("not a context subagent")?.messages)?,&args)),
                    "read_task_requirement" => {
                        let seq=args["event_seq"].as_i64().context("event_seq is required")?;
                        ensure!(s.material.as_ref().unwrap().requirements.contains(&seq),"not a user requirement in this snapshot");
                        let task=s.task.to_owned();
                        let raw=db(s.state.workspace.root(),move|conn|Ok(conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND seq=?2",params![task,seq],|r|r.get::<_,String>(0))?)).await?;
                        let mut result=page(&raw,&args); result["event_seq"]=json!(seq); Ok(result)
                    },
                    "checkpoint_context" => {
                        let text=args["checkpoint"].as_str().filter(|v|!v.trim().is_empty()).context("checkpoint must not be empty")?;
                        ensure!(estimate(&args)<SUMMARY_BUDGET.min(s.input_budget/3),"CHECKPOINT_OVER_CAPACITY");
                        let retained=crate::context_assets::retained(&w.messages,&args["asset_context"])?;
                        let mut proposed=vec![json!({"role":"system","content":Mode::Context.prompt()}),child_start(s.task,s.material.as_ref().unwrap()),json!({"role":"user","name":"context_checkpoint","content":text})];
                        proposed.extend(retained.iter().cloned());
                        ensure!(estimate(&request(s,&proposed,&definitions))+4096 < s.input_budget,"CHECKPOINT_CONTEXT_OVER_CAPACITY: explicitly choose reference mode for assets you no longer need; current context is unchanged");
                        checkpoint=Some((text.to_owned(),retained)); Ok(json!({"checkpoint_saved":true}))
                    },
                    "submit_context" => { validate_summary(s,w,&args).await?; submitted=Some(args); Ok(json!({"accepted":true})) },
                    name => {
                        if crate::context_assets::is_material_tool(name) {material_source=Some((name.to_owned(),args.clone()));}
                        business_tool(s,&node,name,args,&call.call_id).await
                    },
                }
            }.await;
            let is_error = output.is_err();
            let result = output.unwrap_or_else(|error| json!({"error":format!("{error:#}")}));
            let seq=append(s,if s.mode==Mode::Main{"tool/result"}else{"context/tool_result"},json!({
                "actor":s.mode.actor(),"turn":s.turn,"step":step,"flowNodeId":node,
                "message":{"id":agent_service::uuid_like(),"role":"tool","toolCallId":call.call_id,"isError":is_error,
                    "content":[{"type":"text","text":result.to_string()}]},"meta":{"result":result}}),s.mode==Mode::Main).await?;
            let asset_reference = if !is_error {
                if let Some((name, args)) = material_source {
                    let root = s.state.workspace.root().to_path_buf();
                    let task = s.task.to_owned();
                    let original = result.clone();
                    Some(
                        tokio::task::spawn_blocking(move || {
                            crate::context_assets::register(
                                &root, &task, seq, &name, &args, &original,
                            )
                        })
                        .await??,
                    )
                } else if call.name == "read_asset" {
                    Some(result["asset_reference"].clone())
                } else {
                    None
                }
            } else {
                None
            };
            let mut delivery = if call.name == "read_asset" {
                result.clone()
            } else {
                asset_reference
                    .as_ref()
                    .map(|reference| crate::context_assets::delivered(&result, reference))
                    .unwrap_or_else(|| result.clone())
            };
            if let (Some(previous), Some(current)) = (&refreshed_from, &asset_reference) {
                delivery["refresh"] = json!({"previous_asset_id":previous["asset_id"],"current_asset_id":current["asset_id"],
                    "source_version_changed":if previous["source_version"].is_null() || current["source_version"].is_null(){Value::Null}else{json!(previous["source_version"]!=current["source_version"])}});
            }
            if asset_reference.is_some() {
                delivery["delivery_reference"] = json!({"task_id":s.task,"event_seq":seq});
            }
            let mut candidate = w.messages.clone();
            candidate.push(
                json!({"role":"tool","tool_call_id":call.call_id,"content":delivery.to_string()}),
            );
            candidate.extend(image_messages.iter().cloned());
            if !is_error
                && matches!(call.name.as_str(), "view_image" | "read_asset")
                && result["artifact_id"].is_string()
            {
                candidate.push(json!({"role":"user","content":[{"type":"image_ref","artifact_id":result["artifact_id"]}]}));
            }
            let fits = estimate(&request(s, &candidate, &definitions)) + 4096 < s.input_budget;
            if !fits {
                delivery = json!({"error_code":"MATERIAL_OVER_CAPACITY","event_seq":seq,"task_id":s.task,"original_saved":true,
                    "asset_reference":asset_reference,
                    "instruction":"The complete result is saved but was not delivered. Read smaller asset/history pages, or explicitly release assets/checkpoint/compress before retrieving it. Existing loaded assets were not removed."});
            } else if s.mode == Mode::Context && call.name == "read_context_material" && !is_error {
                let start = result["char_offset"].as_u64().unwrap() as usize;
                w.material_ranges.push((
                    start,
                    start + result["returned_chars"].as_u64().unwrap() as usize,
                ));
            } else if s.mode == Mode::Context && call.name == "read_task_requirement" && !is_error {
                let seq = result["event_seq"].as_i64().unwrap();
                let start = result["char_offset"].as_u64().unwrap() as usize;
                let end = start + result["returned_chars"].as_u64().unwrap() as usize;
                w.read_ranges.entry(seq).or_default().push((start, end));
            }
            let mut delivered_message = sourced(
                json!({"role":"tool","tool_call_id":call.call_id,"content":delivery.to_string()}),
                s.task,
                seq,
            );
            if let Some(reference) = &asset_reference {
                delivered_message = crate::context_assets::tagged(delivered_message, reference);
                if !fits {
                    delivered_message["_asset_state"] = json!("not_delivered");
                }
            }
            w.messages.push(delivered_message);
            if fits
                && !is_error
                && matches!(call.name.as_str(), "view_image" | "read_asset")
                && result["artifact_id"].is_string()
            {
                let mut image = json!({"role":"user","viewed_by_context_agent":s.mode==Mode::Context,
                    "content":[{"type":"image_ref","artifact_id":result["artifact_id"],"view_original":true}]});
                if let Some(reference) = &asset_reference {
                    image = crate::context_assets::tagged(image, reference);
                    image["_asset_state"] = json!("image_selected");
                }
                image_messages.push(image);
            }
            save(s, w).await?;
        }
        // Keep all native results adjacent to their assistant call message.
        // Image inputs follow the complete exchange, even if a provider returns
        // multiple calls despite parallel_tool_calls=false.
        w.messages.extend(image_messages);
        save(s, w).await?;
        if let Some(ids) = released_assets {
            crate::context_assets::release(&mut w.messages, &ids)?;
            save(s, w).await?;
        }
        if let Some((material, summary, retained_assets)) = replacement {
            let mut messages = vec![
                json!({"role":"system","content":Mode::Main.prompt()}),
                json!({"role":"user","name":"context_summary","content":summary.to_string()}),
            ];
            messages.extend(retained_assets);
            messages.extend_from_slice(&w.messages[prefix_len..]);
            ensure!(
                estimate(&request(s, &messages, &definitions)) <= s.input_budget,
                "COMPACTED_CONTEXT_OVER_CAPACITY; original working context retained"
            );
            // Summary and the new window become visible together. Interrupted
            // writes can never leave a window referring to an unsaved plan.
            let (task, scope, mut next) = (s.task.to_owned(), s.scope.clone(), w.clone());
            next.messages = messages;
            let saved = next.clone();
            db(s.state.workspace.root(),move|conn| {
                let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                agent_service::append_event_tx(&tx,&task,"agent/context_summary",&json!({"actor":"context_agent","material_id":material,"message":{"role":"user","name":"context_summary","content":summary.to_string()},"result":summary}),None)?;
                tx.execute("INSERT INTO agent_runtime_windows VALUES (?1,?2,?3) ON CONFLICT(task_id,scope) DO UPDATE SET state=excluded.state",params![task,scope,serde_json::to_string(&saved)?])?;
                tx.commit()?; Ok(())
            }).await?;
            *w = next;
        }
        if let Some((text, retained_assets)) = checkpoint {
            append(s,"context/checkpoint",json!({"actor":"context_agent","turn":s.turn,"nodeId":node,"context_scope":s.scope,"checkpoint":text}),false).await?;
            w.messages = vec![
                json!({"role":"system","content":Mode::Context.prompt()}),
                child_start(s.task, s.material.as_ref().unwrap()),
                json!({"role":"user","name":"context_checkpoint","content":text}),
            ];
            w.messages.extend(retained_assets);
            ensure!(
                estimate(&request(s, &w.messages, &definitions)) <= s.input_budget,
                "CHECKPOINT_CONTEXT_OVER_CAPACITY"
            );
            save(s, w).await?;
        }
        if let Some(summary) = submitted {
            return Ok(LoopResult::Context(summary));
        }
    }
    Ok(LoopResult::Limit)
}

pub(crate) async fn run_task(
    state: AgentServiceState,
    task: String,
    model: String,
    prompt: String,
    max_rounds: usize,
    cancel: CancellationToken,
    turn: usize,
    ban_execution: bool,
) -> Result<()> {
    let s = Session {
        state: &state,
        task: &task,
        model: &model,
        turn,
        cancel: &cancel,
        ban_execution,
        mode: Mode::Main,
        scope: "main".into(),
        parent: None,
        material: None,
        input_budget: INPUT_BUDGET,
    };
    let task_copy = task.clone();
    let saved=db(state.workspace.root(),move|conn| {
        let saved:Option<String>=conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id=?1 AND scope='main'",[&task_copy],|r|r.get(0)).optional()?;
        if let Some(saved)=saved { return Ok(serde_json::from_str::<Window>(&saved)?); }
        // Old sessions are readable as attributed records; native exchanges
        // from different legacy actors are not misrepresented as this agent's.
        let mut messages=vec![json!({"role":"system","content":Mode::Main.prompt()})];
        let mut stmt=conn.prepare("SELECT seq,kind,data FROM agent_task_events WHERE task_id=?1 AND kind IN ('user/message','assistant/message','tool/call','tool/result','organizer/method_message','organizer/context_note','organizer/continuation_plan') ORDER BY seq")?;
        for row in stmt.query_map([&task_copy],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))? {
            let (seq,kind,data)=row?;
            messages.push(json!({"role":"user","name":"historical_record","content":json!({"seq":seq,"kind":kind,"record":serde_json::from_str::<Value>(&data)?}).to_string()}));
        }
        Ok(Window{messages,..Default::default()})
    }).await?;
    let mut w = saved;
    // Flow display groups requests by human turn. Do not create an edge whose
    // source is incorrectly interpreted as a node in the new turn.
    w.previous_node = None;
    // Recover a completed result that was committed before the window update.
    // When no result exists, record uncertainty instead of repeating the action.
    let mut pending = BTreeMap::new();
    for m in &w.messages {
        for c in m["tool_calls"].as_array().into_iter().flatten() {
            if let Some(id) = c["id"].as_str() {
                pending.insert(id.to_owned(), m["source"]["seq"].as_i64().unwrap_or(-1));
            }
        }
        if let Some(id) = m["tool_call_id"].as_str() {
            pending.remove(id);
        }
    }
    for (id, after) in pending {
        let (lookup_task, call) = (task.clone(), id.clone());
        let existing = db(state.workspace.root(), move |conn| {
            Ok(conn.query_row("SELECT seq,data FROM agent_task_events WHERE task_id=?1 AND kind='tool/result' AND seq>?2 AND json_extract(data,'$.message.toolCallId')=?3 ORDER BY seq DESC LIMIT 1",
                params![lookup_task,after,call], |r| Ok((r.get::<_,i64>(0)?, r.get::<_,String>(1)?))).optional()?)
        }).await?;
        let (seq, result) = if let Some((seq, raw)) = existing {
            (
                seq,
                serde_json::from_str::<Value>(&raw)?["meta"]["result"].clone(),
            )
        } else {
            let result = json!({"status":"interrupted","completion":"unknown","error":"Previous execution was interrupted. Inspect saved history and resource state before repeating an external action."});
            let seq = append(&s,"tool/result",json!({"actor":"host","turn":turn,"message":{"role":"tool","toolCallId":id,"isError":true,"content":[{"type":"text","text":result.to_string()}]},"meta":{"result":result}}),true).await?;
            (seq, result)
        };
        w.messages.push(sourced(
            json!({"role":"tool","tool_call_id":id,"content":result.to_string()}),
            &task,
            seq,
        ));
    }
    let root = state.workspace.root().to_path_buf();
    let hydrate_task = task.clone();
    let mut restored = w.messages;
    w.messages = tokio::task::spawn_blocking(move || -> Result<Vec<Value>> {
        crate::context_assets::hydrate(&root, &hydrate_task, &mut restored)?;
        Ok(restored)
    })
    .await??;
    append(&s,"turn/start",json!({"turn":turn,"runtime":"single_agent","max_steps":max_rounds,"permission_mode":state.permission_mode}),false).await?;
    let expanded = crate::composer_catalog::expand_prompt(state.workspace.root(), &prompt);
    let message = json!({"role":"user","content":expanded});
    let seq=append(&s,"user/message",json!({"turn":turn,"model_content":expanded,"message":{"id":agent_service::uuid_like(),"role":"user","content":[{"type":"text","text":prompt}]}}),true).await?;
    w.messages.push(sourced(message, &task, seq));
    save(&s, &w).await?;
    let outcome = run_loop(&s, &mut w, max_rounds).await;
    let status = if cancel.is_cancelled() {
        "cancelled"
    } else {
        match &outcome {
            Ok(LoopResult::Answer) => "completed",
            Ok(_) => "max_steps",
            Err(_) => "failed",
        }
    };
    let reason = if status == "completed" {
        json!({"kind":"completed"})
    } else {
        json!({"kind":"error","error":{"code":status,"message":outcome.as_ref().err().map(|e|format!("{e:#}"))}})
    };
    append(&s, "turn/end", json!({"turn":turn,"reason":reason}), false).await?;
    agent_service::finish(&state, &task, status).await?;
    if status == "cancelled" {
        return Ok(());
    }
    // The outcome and terminal status are already durable. The supervisor only
    // handles errors before this terminal commit (or a task panic).
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::{Arc, Mutex};

    struct TestRoot(std::path::PathBuf);
    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("single-agent-test-{}", agent_service::uuid_like()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct Script {
        requests: Vec<Value>,
        phase: usize,
        offset: usize,
        requirement: i64,
        main_round: usize,
        checkpointed: bool,
    }
    fn summary(requirement: i64) -> Value {
        json!({"original_purpose":"检查 PPT 前两页", "current_goal":"继续第二页视觉检查",
            "requirement_changes":"保留窗口和服务", "constraints":"不改业务源码",
            "progress":"文件已加载 8 页", "open_questions":"第二页尚未查看",
            "next_steps":"读取第二页截图；仅在亲自看图后作出视觉结论",
            "tool_history":"原记录表明文件已加载，未完成视觉检查",
            "source_event_seqs":[requirement], "visual_conclusions":[]})
    }
    fn call(name: &str, args: Value, n: usize) -> Value {
        json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("c{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
    }
    async fn scripted(
        State(script): State<Arc<Mutex<Script>>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let mut s = script.lock().unwrap();
        let messages = body["messages"].as_array().unwrap();
        let child = messages[0]["content"] == Mode::Context.prompt();
        let n = s.requests.len();
        s.requests.push(body.clone());
        let result = if !child {
            s.main_round += 1;
            if s.main_round == 1 {
                call("compress_context", json!({"reason":"上下文过长"}), n)
            } else {
                assert!(messages.iter().any(|m| {
                    m["name"] == "context_summary"
                        && m["content"]
                            .as_str()
                            .unwrap()
                            .contains("继续第二页视觉检查")
                }));
                assert!(!body.to_string().contains("LONG_RAW_FILLER"));
                json!({"role":"assistant","content":"继续检查第二页，保留窗口和服务，不改源码。"})
            }
        } else {
            assert!(
                !body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["function"]["name"] == "compress_context")
            );
            let initial: Value =
                serde_json::from_str(messages[1]["content"].as_str().unwrap()).unwrap();
            s.requirement = initial["requirement_event_seqs"][0].as_i64().unwrap();
            let last = messages
                .iter()
                .rev()
                .find(|m| m["role"] == "tool")
                .map(|m| serde_json::from_str::<Value>(m["content"].as_str().unwrap()).unwrap());
            match s.phase {
                0 => {
                    s.phase = 1;
                    call("compress_context", json!({"reason":"must be forbidden"}), n)
                }
                1 => {
                    assert!(
                        last.unwrap()["error"]
                            .as_str()
                            .unwrap()
                            .contains("TOOL_UNAVAILABLE")
                    );
                    s.phase = 2;
                    call("submit_context", summary(s.requirement), n)
                }
                2 => {
                    assert!(
                        last.unwrap()["error"]
                            .as_str()
                            .unwrap()
                            .contains("CURRENT_CONTEXT_NOT_READ")
                    );
                    s.phase = 3;
                    call(
                        "read_task_requirement",
                        json!({"event_seq":s.requirement,"max_chars":24000}),
                        n,
                    )
                }
                3 => {
                    let previous = last.unwrap();
                    if let Some(next) = previous["next_char_offset"].as_u64() {
                        call(
                            "read_task_requirement",
                            json!({"event_seq":s.requirement,"char_offset":next}),
                            n,
                        )
                    } else {
                        s.phase = 4;
                        call("read_context_material", json!({"max_chars":24000}), n)
                    }
                }
                4 => {
                    let previous = last.unwrap();
                    if let Some(next) = previous["next_char_offset"].as_u64() {
                        s.offset = next as usize;
                        if !s.checkpointed {
                            s.checkpointed = true;
                            s.phase = 5;
                            call(
                                "checkpoint_context",
                                json!({"checkpoint":format!("检查 PPT 前两页，保留窗口和服务，不改源码。用户要求已全部读取。当前材料读到 {}，继续下一页。",s.offset)}),
                                n,
                            )
                        } else {
                            call("read_context_material", json!({"char_offset":s.offset}), n)
                        }
                    } else {
                        s.phase = 6;
                        let mut invalid = summary(s.requirement);
                        invalid["visual_conclusions"] =
                            json!([{"artifact_id":"not-viewed","conclusion":"页面正常"}]);
                        call("submit_context", invalid, n)
                    }
                }
                5 => {
                    assert_eq!(messages.len(), 4); // system, task refs, checkpoint, capacity
                    assert!(messages[2]["name"] == "context_checkpoint");
                    assert!(!body.to_string().contains("LONG_RAW_FILLER"));
                    s.phase = 4;
                    call("read_context_material", json!({"char_offset":s.offset}), n)
                }
                6 => {
                    assert!(
                        last.unwrap()["error"]
                            .as_str()
                            .unwrap()
                            .contains("VISUAL_EVIDENCE_NOT_VIEWED")
                    );
                    s.phase = 7;
                    call("submit_context", summary(s.requirement), n)
                }
                _ => panic!("unexpected child request"),
            }
        };
        Json(
            json!({"choices":[{"message":result,"finish_reason":if result["tool_calls"].is_array(){"tool_calls"}else{"stop"}}]}),
        )
    }
    async fn setup() -> (
        TestRoot,
        AgentServiceState,
        Arc<Mutex<Script>>,
        tokio::task::JoinHandle<()>,
    ) {
        let root = TestRoot::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let script = Arc::new(Mutex::new(Script::default()));
        let app = Router::new()
            .route("/v1/chat/completions", post(scripted))
            .with_state(script.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut state = agent_service::tests::flow_test_state(root.path(), address);
        // Legacy observer configuration must not start an extra role.
        state.observer_enabled = true;
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        (root, state, script, server)
    }
    #[tokio::test]
    async fn multi_round_context_subagent_preserves_plan_and_records_every_request() {
        let (root, state, script, server) = setup().await;
        let prompt = format!(
            "检查 PPT 前两页，保留窗口和服务，不改源码。{}中段关键要求：第二页仍需检查。",
            "LONG_RAW_FILLER".repeat(2400)
        );
        run_task(
            state.clone(),
            "task".into(),
            "fake".into(),
            prompt.clone(),
            3,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        let conn = agent_service::open_db(root.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "completed"
        );
        let original: String = conn
            .query_row(
                "SELECT data FROM agent_task_events WHERE kind='user/message' AND task_id='task'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&original).unwrap()["message"]["content"][0]["text"],
            prompt
        );
        // A second process-style load must reuse the compact context and plan.
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "继续，并说明约束".into(),
            2,
            CancellationToken::new(),
            2,
            false,
        )
        .await
        .unwrap();
        let request_count = script.lock().unwrap().requests.len();
        let rows: Vec<(String, String, String)> = conn
            .prepare("SELECT actor,metadata,request_json FROM agent_request_contexts ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), request_count);
        assert!(request_count > 8);
        for (index, (actor, metadata, body)) in rows.iter().enumerate() {
            assert!(actor == "agent" || actor == "context_agent");
            let meta: Value = serde_json::from_str(metadata).unwrap();
            assert_eq!(meta["status"], "completed");
            assert!(meta["outcome"]["response"].is_object());
            if actor == "context_agent" {
                assert!(meta["parent_node_id"].is_string());
            }
            assert_eq!(
                serde_json::from_str::<Value>(body).unwrap(),
                script.lock().unwrap().requests[index]
            );
        }
        for kind in ["flow/node_add", "flow/node_state"] {
            let count: usize = conn
                .query_row(
                    "SELECT count(*) FROM agent_task_events WHERE kind=?1",
                    [kind],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, request_count);
        }
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM agent_task_events WHERE kind='agent/context_summary'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            1
        );
        let still_original:String=conn.query_row("SELECT data FROM agent_task_events WHERE kind='user/message' AND json_extract(data,'$.turn')=1",[],|r|r.get(0)).unwrap();
        assert_eq!(original, still_original);
        let summary_record: String = conn
            .query_row(
                "SELECT data FROM agent_task_events WHERE kind='agent/context_summary'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&summary_record).unwrap()["result"],
            summary(script.lock().unwrap().requirement)
        );
        server.abort();
    }
    #[tokio::test]
    async fn unavailable_request_log_prevents_network_call() {
        let (root, state, script, server) = setup().await;
        // Create the request table using its actual recorder, then deny writes.
        crate::request_context::record(
            root.path(),
            "task",
            json!({"actor":"test"}),
            &json!({"messages":[]}),
        )
        .await
        .unwrap()
        .finish("completed", json!({}))
        .await;
        agent_service::open_db(root.path()).unwrap().execute_batch("CREATE TRIGGER deny_request BEFORE INSERT ON agent_request_contexts BEGIN SELECT RAISE(ABORT,'disk unavailable'); END;").unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "test".into(),
            2,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        assert!(script.lock().unwrap().requests.is_empty());
        let conn = agent_service::open_db(root.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT status FROM agent_tasks", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "failed"
        );
        server.abort();
    }
    #[test]
    fn page_coverage_detects_missing_middle_and_unicode_offsets() {
        assert!(!fully_read(&[(0, 3), (4, 10)], 10));
        assert!(fully_read(&[(4, 10), (0, 5)], 10));
        let p = page("甲乙丙丁", &json!({"char_offset":1,"max_chars":2}));
        assert_eq!(p["content"], "乙丙");
        assert_eq!(p["next_char_offset"], 3);
    }
    #[tokio::test]
    async fn oversized_summary_is_rejected_and_main_window_is_unchanged() {
        let (root, state, _script, server) = setup().await;
        let cancel = CancellationToken::new();
        let session = Session {
            state: &state,
            task: "task",
            model: "fake",
            turn: 1,
            cancel: &cancel,
            ban_execution: true,
            mode: Mode::Context,
            scope: "context:test".into(),
            parent: None,
            material: Some(Material {
                id: "test".into(),
                through: 0,
                requirements: vec![],
                messages: vec![],
            }),
            input_budget: INPUT_BUDGET,
        };
        let original = Window {
            messages: vec![json!({"role":"user","content":"original goal"})],
            ..Default::default()
        };
        let mut main = Session {
            scope: "main".into(),
            mode: Mode::Main,
            ..session
        };
        save(&main, &original).await.unwrap();
        main.mode = Mode::Context;
        let mut result = summary(0);
        result["progress"] = json!("x".repeat(SUMMARY_BUDGET * 4));
        let error = validate_summary(&main, &Window::default(), &result)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("SUMMARY_OVER_CAPACITY"));
        let raw: String = agent_service::open_db(root.path())
            .unwrap()
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["messages"],
            json!(original.messages)
        );
        server.abort();
    }

    #[tokio::test]
    async fn restart_recovers_saved_tool_result_without_reexecuting() {
        let (root, state, script, server) = setup().await;
        script.lock().unwrap().main_round = 1;
        let cancel = CancellationToken::new();
        let session = Session {
            state: &state,
            task: "task",
            model: "fake",
            turn: 1,
            cancel: &cancel,
            ban_execution: true,
            mode: Mode::Main,
            scope: "main".into(),
            parent: None,
            material: None,
            input_budget: INPUT_BUDGET,
        };
        let assistant = call(
            "write_file",
            json!({"path":"never-repeat.txt","content":"test"}),
            0,
        );
        let seq = append(
            &session,
            "assistant/message",
            json!({"actor":"agent","message":assistant}),
            false,
        )
        .await
        .unwrap();
        let saved = Window {
            messages: vec![
                json!({"role":"system","content":Mode::Main.prompt()}),
                json!({"role":"user","name":"context_summary","content":summary(0).to_string()}),
                sourced(assistant, "task", seq),
            ],
            ..Default::default()
        };
        save(&session, &saved).await.unwrap();
        append(&session,"tool/result",json!({"message":{"toolCallId":"c0"},"meta":{"result":{"written":true,"receipt":"original-result"}}}),false).await.unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "继续".into(),
            2,
            CancellationToken::new(),
            2,
            false,
        )
        .await
        .unwrap();
        let requests = &script.lock().unwrap().requests;
        let restored = requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["tool_call_id"] == "c0")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(restored["content"].as_str().unwrap()).unwrap(),
            json!({"written":true,"receipt":"original-result"})
        );
        assert!(!root.path().join("never-repeat.txt").exists());
        server.abort();
    }
    #[tokio::test]
    async fn restart_marks_pending_request_interrupted_without_erasing_input() {
        let (root, _state, _script, server) = setup().await;
        let body = json!({"messages":[{"role":"user","content":"original task"}]});
        let trace = crate::request_context::record(
            root.path(),
            "task",
            json!({"actor":"context_agent","turn":1,"nodeId":"pending-node","request_flow":true}),
            &body,
        )
        .await
        .unwrap();
        // Simulate abrupt process death: no Drop cleanup or received response.
        std::mem::forget(trace);
        agent_service::recover_orphaned_tasks(root.path())
            .await
            .unwrap();
        let conn = agent_service::open_db(root.path()).unwrap();
        let (raw, meta): (String, String) = conn
            .query_row(
                "SELECT request_json,metadata FROM agent_request_contexts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), body);
        assert_eq!(
            serde_json::from_str::<Value>(&meta).unwrap()["status"],
            "interrupted"
        );
        let status:String=conn.query_row("SELECT json_extract(data,'$.status') FROM agent_task_events WHERE kind='flow/node_state'",[],|r|r.get(0)).unwrap();
        assert_eq!(status, "interrupted");
        server.abort();
    }

    #[tokio::test]
    async fn context_view_delivers_pixels_from_prior_turn_but_excludes_other_tasks() {
        let root = TestRoot::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app=Router::new().route("/v1/chat/completions",post(|Json(body):Json<Value>| async move {
            assert!(body["messages"].as_array().unwrap().iter().any(|m|m["content"].as_array().is_some_and(|blocks|blocks.iter().any(|b|b["type"]=="image_url"))));
            Json(json!({"choices":[{"message":{"role":"assistant","content":"image received"},"finish_reason":"stop"}]}))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut state = agent_service::tests::flow_test_state(root.path(), address);
        state
            .visual
            .capabilities
            .entry(String::new())
            .or_default()
            .insert(
                "fake".into(),
                crate::visual_artifacts::ImageCapability::Supported,
            );
        agent_service::open_db(root.path()).unwrap().execute("INSERT INTO agent_tasks VALUES ('task','test','fake','running',0,0,NULL,NULL,NULL,0)",[]).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let previous = crate::visual_artifacts::VisualContext {
            identity: json!({"task_id":"task","request_id":1,"node_id":"old","runtime":"single_agent"}),
            ..Default::default()
        };
        let artifact =
            crate::visual_artifacts::save(root.path(), &previous, json!({}), png.get_ref())
                .unwrap();
        let id = artifact["artifact_id"].as_str().unwrap();
        let cancel = CancellationToken::new();
        let session = Session {
            state: &state,
            task: "task",
            model: "fake",
            turn: 2,
            cancel: &cancel,
            ban_execution: true,
            mode: Mode::Context,
            scope: "context:visual".into(),
            parent: None,
            material: None,
            input_budget: INPUT_BUDGET,
        };
        business_tool(
            &session,
            "view-node",
            "view_image",
            json!({"artifact_id":id}),
            "view-call",
        )
        .await
        .unwrap();
        let mut window = Window {
            messages: vec![
                json!({"role":"system","content":Mode::Context.prompt()}),
                json!({"role":"user","viewed_by_context_agent":true,"content":[{"type":"image_ref","artifact_id":id,"view_original":true}]}),
            ],
            ..Default::default()
        };
        assert!(window.viewed_images.is_empty());
        model_round(&session, &mut window, 1, &tools(&session))
            .await
            .unwrap();
        assert!(window.viewed_images.contains(id));
        let mut wrong_task = visual_context(&session, "other").unwrap();
        wrong_task.identity["task_id"] = json!("different-task");
        assert!(crate::visual_artifacts::authorize(&artifact, &wrong_task).is_err());
        server.abort();
    }
}
