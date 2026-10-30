//! Adapter from flow pseudocode to the existing tool/Agent boundary.
//! It synthesizes normal tool exchanges; it never calls filesystem/plugin APIs directly.
use super::*;
use crate::agent_program::{self, Action, Runtime, State, runtime::ActionKind};

pub(super) enum Round {
    Exchange(String, Value),
    /// A program step ran; continue the program.
    Continue,
    /// A function call opened the frame with this scope.
    Entered(String),
    /// The current AI rewrites its failed implementation.
    Rewrite,
    Concurrent(Vec<Action>),
    Waiting,
}
pub(super) fn library_catalog(
    s: &Session<'_>,
    definitions: &[Value],
) -> agent_program::libraries::Libraries {
    let packages = s
        .state
        .tool_catalog
        .as_ref()
        .map(|c| c.packages.catalog())
        .unwrap_or_else(|| json!([]));
    agent_program::libraries::catalog(definitions, &packages)
}
/// One call representation for ordinary, virtual and forked functions.
pub(super) fn function_task(
    program: &agent_program::Program,
    name: &str,
    arguments: &[Value],
) -> Result<Value> {
    let function = program
        .functions
        .get(name)
        .context("called function missing")?;
    ensure!(
        function.params.len() == arguments.len(),
        "function argument count mismatch"
    );
    let description = if function.description.is_empty() {
        format!("Execute {name} using its supplied arguments and concrete function body")
    } else {
        function.description.clone()
    };
    let role = match (&function.leaf_tools, function.virtual_task) {
        (_, false) => "body",
        (None, true) => "implement",
        (Some(_), true) => "leaf",
    };
    // A nested function's id carries its location; the AI sees its own name.
    let shown = if function.virtual_task {
        name.rsplit('/').next().and_then(|n| n.split('@').next()).unwrap_or(name)
    } else {
        name
    };
    Ok(
        json!({"name":shown,"description":description,"parameters":function.params,"arguments":arguments,
            "role":role,"leaf_tools":function.leaf_tools,"async":function.is_async}),
    )
}
/// What the AI does in a function window. Only virtual functions call AI: a
/// virtual declared in the main program is implemented by AI-written code; one
/// declared inside such an implementation is a leaf tool loop.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Role {
    Main,
    Implement,
    Leaf(BTreeSet<String>),
}
pub(super) fn role(w: &Window) -> Role {
    let Some(task) = &w.program_task else { return Role::Main };
    if task["role"] != "leaf" {
        return Role::Implement;
    }
    let mut tools = task["leaf_tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    tools.extend(found_tools(&w.messages));
    Role::Leaf(tools)
}
pub(super) const IMPLEMENT_PROMPT: &str = include_str!("../../prompts/virtual_implement.md");
pub(super) const LEAF_PROMPT: &str = include_str!("../../prompts/virtual_leaf.md");
/// A request's role, from the function input its messages carry.
pub(super) fn request_role(messages: &[Value]) -> &'static str {
    let Some(input) = messages.iter().find(|m| m["name"] == "program_task_input") else {
        return "main";
    };
    let view = serde_json::from_str::<Value>(input["content"].as_str().unwrap_or("")).unwrap_or_default();
    if view["role"] == "leaf" { "leaf" } else { "implement" }
}
pub(super) fn prompt(role: &str) -> &'static str {
    match role {
        "main" => MAIN_PROMPT,
        "leaf" => LEAF_PROMPT,
        _ => IMPLEMENT_PROMPT,
    }
}

pub(super) const FIND: &str = "find_library_methods";
pub(super) fn find_tool() -> Value {
    function(FIND, "Search the available library methods by keywords when this function needs one it was not given. Matching methods become callable in this function only; permissions are unchanged.",
        json!({"query":{"type":"string","minLength":1}}), json!(["query"]))
}
/// Methods a leaf found earlier in its own loop. They live in its messages and
/// disappear with them when the function returns.
fn found_tools(messages: &[Value]) -> Vec<String> {
    let calls = messages
        .iter()
        .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
        .filter(|c| c["function"]["name"] == FIND)
        .filter_map(|c| c["id"].as_str())
        .collect::<BTreeSet<_>>();
    messages
        .iter()
        .filter(|m| m["role"] == "tool" && m["tool_call_id"].as_str().is_some_and(|id| calls.contains(id)))
        .filter_map(|m| serde_json::from_str::<Value>(m["content"].as_str()?).ok())
        .flat_map(|result| {
            result["methods"].as_array().into_iter().flatten()
                .filter_map(|m| m["tool"].as_str().map(str::to_owned)).collect::<Vec<_>>()
        })
        .collect()
}
pub(super) fn find(s: &Session<'_>, w: &Window, args: &Value) -> Result<Value> {
    let query = args["query"].as_str().context("query is required")?.to_lowercase();
    let terms = query.split_whitespace().collect::<Vec<_>>();
    ensure!(!terms.is_empty(), "query is required");
    let definitions = available_tools(s, w);
    let catalog = library_catalog(s, &definitions);
    let methods = definitions
        .iter()
        .filter_map(|d| {
            let tool = d["function"]["name"].as_str()?;
            let method = agent_program::libraries::public_name(&catalog, tool)?;
            let description = d["function"]["description"].as_str().unwrap_or("");
            let text = format!("{method} {tool} {description}").to_lowercase();
            terms.iter().any(|term| text.contains(term)).then(|| {
                json!({"tool":tool,"method":method,"description":description,"parameters":d["function"]["parameters"]})
            })
        })
        .take(8)
        .collect::<Vec<_>>();
    Ok(json!({"methods":methods}))
}
/// Files a function wrote successfully: write calls recorded under its frame,
/// whether its AI or its program made them, whose results were not errors.
pub(super) fn completed_writes(s: &Session<'_>, w: &Window) -> Result<Vec<Value>> {
    let Some(frame) = w.task_node_id.as_deref() else { return Ok(Vec::new()) };
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let rows = conn
        .prepare("SELECT json_extract(c.data,'$.name'),json_extract(c.data,'$.arguments') FROM agent_task_events c
            WHERE c.task_id=?1 AND c.kind='tool/call' AND json_extract(c.data,'$.task_node_id')=?2
            AND json_extract(c.data,'$.name') IN ('write_file','edit_file','replace_range')
            AND EXISTS (SELECT 1 FROM agent_task_events r WHERE r.task_id=c.task_id AND r.kind='tool/result'
                AND json_extract(r.data,'$.message.toolCallId')=json_extract(c.data,'$.callId')
                AND json_extract(r.data,'$.message.isError')=0)
            ORDER BY c.seq")?
        .query_map(params![s.task, frame], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(tool, arguments)| {
            let path = serde_json::from_str::<Value>(&arguments).map(|a| a["path"].clone()).unwrap_or(Value::Null);
            json!({"tool":tool,"path":path})
        })
        .collect())
}
fn call_arguments(task: &Value) -> Result<(&Vec<Value>, &Vec<Value>)> {
    let parameters = task["parameters"].as_array().context(
        "function parameters missing; legacy call contracts cannot resume under this version",
    )?;
    let arguments = task["arguments"]
        .as_array()
        .context("ordered function arguments missing")?;
    ensure!(
        parameters.len() == arguments.len() && parameters.iter().all(Value::is_string),
        "invalid function argument contract"
    );
    Ok((parameters, arguments))
}
/// Model-facing view: bindings are actual local variable values, not an input envelope.
pub(super) fn call_view(task: &Value) -> Result<Value> {
    let (parameters, arguments) = call_arguments(task)?;
    let names = parameters
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect::<Vec<_>>();
    let bindings = names
        .iter()
        .zip(arguments)
        .map(|(p, v)| ((*p).to_owned(), v.clone()))
        .collect::<serde_json::Map<_, _>>();
    Ok(
        json!({"name":task["name"],"signature":format!("{}function {}({})",if task["async"] == true {"async "} else {""},task["name"].as_str().context("function name missing")?,names.join(", ")),
        "role":task["role"],"description":task["description"],"bindings":bindings,
        "binding_rule":"bindings lists actual local parameter values. Use each parameter directly as in the signature; bindings is display metadata, never another property of a parameter. Arguments bind by position only, never by name or automatic object packing."}),
    )
}
pub(super) fn task_runtime(task: &Value) -> Result<Option<Runtime>> {
    if !task["body"].is_object() {
        return Ok(None);
    }
    let (_, arguments) = call_arguments(task)?;
    compiled_runtime(&json!({"program":{"compiled":task["body"]["compiled"],"entry":task["name"],"args":arguments}})).map(Some)
}
pub(super) fn install_scoped(
    s: &Session<'_>,
    w: &mut Window,
    args: &Value,
    body: bool,
) -> Result<Value> {
    ensure!(args.is_object(), "program arguments must be a JSON object");
    ensure!(
        args.get("libraries").is_none(),
        "library names are fixed by the host/plugin declarations; libraries aliases are unsupported"
    );
    let libraries = library_catalog(s, &tools(s, w));
    if body {
        install_body_with(w, args, libraries)
    } else {
        install_with(w, args, libraries)
    }
}
/// The program in an AI reply: its one fenced block tagged js, javascript or
/// mjs. Other fences, such as ```text, stay ordinary answer text.
pub(super) fn reply_source(message: &Value) -> std::result::Result<Option<String>, String> {
    let text = message["content"].as_str().unwrap_or("");
    let mut sources = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(tag) = line.trim_start().strip_prefix("```") else { continue };
        let program = matches!(tag.trim().to_ascii_lowercase().as_str(), "js" | "javascript" | "mjs");
        let mut body = Vec::new();
        for line in lines.by_ref() {
            if line.trim_start().starts_with("```") { break; }
            // A closing fence may follow the last code on the same line.
            if let Some(code) = line.trim_end().strip_suffix("```") {
                body.push(code);
                break;
            }
            body.push(line);
        }
        if program { sources.push(body.join("\n")); }
    }
    match sources.len() {
        0 => Ok(None),
        1 => Ok(sources.pop()),
        n => Err(format!("the reply contains {n} JavaScript blocks; a reply runs exactly one complete program, so merge them into one ```js block")),
    }
}
/// Repairable source errors stay ordinary tool results for the current AI.
/// No source is auto-edited and no failed install replaces the saved program.
pub(super) fn source_error(operation: &str, args: &Value, error: &anyhow::Error) -> Value {
    let message = format!("{error:#}");
    let at = message.strip_prefix("TS_PROGRAM ").and_then(|rest| {
        let mut parts = rest.splitn(3, ':');
        Some((
            parts.next()?.parse::<usize>().ok()?,
            parts.next()?.parse::<usize>().ok()?,
        ))
    });
    let diagnostic = if let Some((line, column)) = at {
        let lines = args["source"]
            .as_str()
            .unwrap_or("")
            .lines()
            .collect::<Vec<_>>();
        let start = line.saturating_sub(2);
        let excerpt = lines.iter().enumerate().skip(start).take(3)
            .map(|(index, text)|json!({"line":index+1,"text":text.chars().take(400).collect::<String>()}))
            .collect::<Vec<_>>();
        json!({"message":message,"line":line,"column":column,"source_excerpt":excerpt})
    } else {
        json!({"message":message})
    };
    json!({"error":message,"code":"PROGRAM_SOURCE_INVALID","phase":"source_validation",
        "retryable":true,"executed":false,"operation":operation,"diagnostics":[diagnostic],
        "recovery":"Nothing ran. Rewrite the complete source using these diagnostics and the published tool contracts, and reply with it again as one ```js block. Keep the goal, the current function name and parameters, and prior results. Do not repeat completed operations. Never report a rejected program as completed."})
}

#[cfg(test)]
pub(super) fn install(w: &mut Window, args: &Value) -> Result<Value> {
    install_with(w, args, agent_program::libraries::builtin())
}
fn install_with(
    w: &mut Window,
    args: &Value,
    libraries: agent_program::libraries::Libraries,
) -> Result<Value> {
    ensure!(
        args.get("libraries").is_none(),
        "model-defined libraries are unsupported"
    );
    let source = args["source"].as_str().context("source is required")?;
    let program = agent_program::compile_with_libraries(source, libraries)?;
    ensure!(
        args.as_object().is_some_and(|args| args.keys().all(|key| key == "source")),
        "a program is its source alone; put its data in the code or read it with the imported tools"
    );
    let entry = agent_program::ENTRY;
    let runtime = Runtime::task_program(program, entry, vec![])?;
    w.program = Some(runtime);
    w.program_value = None;
    Ok(
        json!({"status":"started","entry":entry,"version":agent_program::VERSION,"source_hash":crate::visual_artifacts::hash(source.as_bytes())}),
    )
}
#[cfg(test)]
pub(super) fn install_body(w: &mut Window, args: &Value) -> Result<Value> {
    install_body_with(w, args, agent_program::libraries::builtin())
}
fn install_body_with(
    w: &mut Window,
    args: &Value,
    libraries: agent_program::libraries::Libraries,
) -> Result<Value> {
    ensure!(
        args.get("libraries").is_none(),
        "model-defined libraries are unsupported"
    );
    ensure!(
        w.program.is_none(),
        "the current task body is already executing"
    );
    let task = w
        .program_task
        .as_ref()
        .context("define_task_body requires a current task function")?;
    let name = task["name"]
        .as_str()
        .context("current function name missing")?;
    let source = args["source"].as_str().context("source is required")?;
    let scope = super::function_scope::values(w);
    let program = agent_program::compile_body_with_scope(
        source,
        name,
        libraries,
        scope.keys().cloned().collect(),
    )?;
    let params = &program.functions[name].params;
    let (_, arguments) = call_arguments(task)?;
    ensure!(
        params.len() == arguments.len(),
        "keep the current function's parameter count ({}); arguments bind by position, with no automatic object packing",
        arguments.len()
    );
    let values = arguments.clone();
    let entry = name.to_owned();
    w.program = Some(Runtime::task_program_with_scope(
        program, &entry, values, scope,
    )?);
    w.program_value = None;
    Ok(
        json!({"status":"started","function":entry,"version":agent_program::VERSION,"source_hash":crate::visual_artifacts::hash(source.as_bytes())}),
    )
}
pub(super) fn compiled_runtime(args: &Value) -> Result<Runtime> {
    compiled_runtime_with_scope(args, BTreeMap::new())
}
pub(super) fn compiled_runtime_with_scope(
    args: &Value,
    scope: BTreeMap<String, Value>,
) -> Result<Runtime> {
    let p = args
        .get("program")
        .filter(|v| v.is_object())
        .context("compiled flow function required")?;
    let program: agent_program::Program =
        serde_json::from_value(p["compiled"].clone()).context("compiled host program required")?;
    let entry = p["entry"].as_str().context("program.entry is required")?;
    let arguments = p["args"]
        .as_array()
        .context("program.args must be an array")?
        .clone();
    Runtime::task_program_with_scope(program, entry, arguments, scope)
}
/// Private scheduling effects have no model tool schema. Only a pending
/// compiled instruction may cross this dispatch boundary.
pub(super) fn internal_call(w: &Window, call: &crate::model_stream::ToolCall) -> bool {
    w.program
        .as_ref()
        .and_then(|p| p.pending.as_ref())
        .is_some_and(|action| {
            action.id == call.call_id
                && matches!(
                    (&action.kind, call.name.as_str()),
                    (ActionKind::ParallelReads { .. }, "parallel_read_batch")
                )
        })
}

fn recorded(s: &Session<'_>, id: &str) -> Result<Option<(i64, Value)>> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let sql = "SELECT seq,data FROM agent_task_events WHERE task_id=?1 AND ((kind='tool/result' AND json_extract(data,'$.message.toolCallId')=?2) OR (kind='subagent/event' AND json_extract(data,'$.event_type')='tool/result' AND json_extract(data,'$.event.message.toolCallId')=?2)) ORDER BY seq DESC LIMIT 1";
    let row: Option<(i64, String)> = conn
        .query_row(sql, params![s.task, id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    row.map(|(seq, raw)| {
        let v: Value = serde_json::from_str(&raw)?;
        Ok((
            seq,
            if v["event"].is_object() {
                v["event"].clone()
            } else {
                v
            },
        ))
    })
    .transpose()
}
fn resolve(
    _s: &Session<'_>,
    w: &mut Window,
    action: &Action,
    mut value: Value,
    mut failed: bool,
    _seq: i64,
) -> Result<()> {
    if !failed {
        match &action.kind {
            ActionKind::Fork { .. } | ActionKind::Join { .. } => anyhow::bail!("saved fork/join calls are retired"),
            ActionKind::Tool { .. } | ActionKind::ParallelReads { .. } => {}
            ActionKind::Task { .. } | ActionKind::Call { .. } => {
                match value["result"]
                    .as_str()
                    .context("comment task returned no value")
                    .and_then(|s| {
                        serde_json::from_str::<Value>(s)
                            .context("comment task must return a JSON value")
                    }) {
                    Ok(v) if v.get("$task_error").is_some() => {
                        failed = true;
                        value = v["$task_error"].clone();
                    }
                    Ok(v) => value = v,
                    Err(e) => {
                        failed = true;
                        value = json!({"message":format!("{e:#}"),"receipt":value});
                    }
                }
            }
        }
    }
    w.program.as_mut().unwrap().resume(value, failed)
}
pub(super) async fn recover(s: &Session<'_>, w: &mut Window) -> Result<()> {
    // Version rejection must precede receipt consumption or dispatch.
    if w.program
        .as_ref()
        .is_some_and(|p| p.program.version != agent_program::VERSION)
    {
        return Ok(());
    }
    let inflight = w.program.as_ref().map(|p| p.inflight_actions()).unwrap_or_default();
    let mut completed = Vec::new();
    for action in inflight {
        if let Some((seq, event)) = recorded(s, &action.id)? { completed.push((seq, action, event)); }
    }
    completed.sort_by_key(|(seq, _, _)| *seq);
    for (_, action, event) in completed {
        let mut value = event["meta"]["result"].clone();
        let mut failed = event["message"]["isError"] == true;
        if !failed {
            match value["result"].as_str().and_then(|text| serde_json::from_str::<Value>(text).ok()) {
                Some(result) if result.get("$task_error").is_some() => { failed = true; value = result["$task_error"].clone(); }
                Some(result) => value = result,
                None => { failed = true; value = json!({"message":"async function returned no valid JSON value"}); }
            }
        }
        w.program.as_mut().unwrap().resume_action(&action.id, value, failed)?;
        save(s, w).await?;
    }
    let Some(action) = w.program.as_ref().and_then(|p| p.pending.clone()) else {
        return Ok(());
    };
    if let Some((seq, event)) = recorded(s, &action.id)? {
        resolve(
            s,
            w,
            &action,
            event["meta"]["result"].clone(),
            event["message"]["isError"] == true,
            seq,
        )?;
        save(s, w).await?;
    }
    Ok(())
}
pub(super) async fn interrupt_unfinished(s: &Session<'_>, w: &mut Window) -> Result<()> {
    let actions = w.program.as_ref().map(|runtime| runtime.inflight_actions()).unwrap_or_default();
    for action in actions {
        task_frames::interrupt_parallel_function(s, &action.id).await?;
        w.program.as_mut().unwrap().resume_action(&action.id, json!({"message":"async function was interrupted; its external effects were not replayed"}), true)?;
    }
    save(s, w).await
}
async fn return_value(s: &Session<'_>, w: &mut Window, step: usize, value: Value) -> Result<Round> {
    w.program_value = Some(value.clone());
    w.program = None;
    let text = if w.program_task.is_none() && value.is_null() {
        "程序执行完成。".to_owned()
    } else if w.program_task.is_some() {
        serde_json::to_string(&value)?
    } else {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| serde_json::to_string_pretty(&value).unwrap())
    };
    let message =
        json!({"role":"assistant","content":if text.is_empty(){"程序执行完成。".into()}else{text}});
    let seq = append(
        s,
        "assistant/message",
        json!({"actor":"program","turn":s.turn,"step":step,"message":message}),
        s.is_primary(),
    )
    .await?;
    w.messages.push(sourced(message.clone(), s.task, seq));
    save(s, w).await?;
    Ok(Round::Exchange(
        format!("program_return_{}", agent_service::uuid_like()),
        message,
    ))
}
pub(super) async fn next(s: &Session<'_>, w: &mut Window, step: usize) -> Result<Round> {
    recover(s, w).await?;
    let state = w.program.as_mut().unwrap().next_concurrent();
    save(s, w).await?;
    let state = match state {
        agent_program::runtime::ConcurrentState::Actions(actions) if actions.first().is_some_and(|action| w.program.as_ref().unwrap().async_scope(&action.id).is_some()) => return Ok(Round::Concurrent(actions)),
        agent_program::runtime::ConcurrentState::Actions(mut actions) => State::Action(actions.remove(0)),
        agent_program::runtime::ConcurrentState::Waiting => return Ok(Round::Waiting),
        agent_program::runtime::ConcurrentState::Returned(value) => State::Returned(value),
        agent_program::runtime::ConcurrentState::Failed(error) => State::Failed(error),
    };
    match state {
        State::Waiting => unreachable!(),
        State::Failed(error) => {
            append(s, "program/error", json!({"error":error}), true).await?;
            if role(w) == Role::Implement && w.program_task.as_ref().is_some_and(|task| task["role"] == "implement") {
                // AI-written code is not right every time: its author gets the
                // error and what already ran, and rewrites it or gives up.
                w.program = None;
                let diagnostic = json!({"code":"FUNCTION_RUNTIME_ERROR","executed":true,"error":error,
                    "completed_writes":completed_writes(s, w)?,
                    "instruction":"Your implementation stopped with this error. Reply with a corrected complete ```js block; it runs from its start, so do not redo the listed writes unless the contract needs them redone. If the contract cannot be met, reply with {\"$task_error\":{\"message\":\"concrete cause\"}} instead."});
                let seq = append(s, "program/runtime_error", json!({"diagnostic":diagnostic}), false).await?;
                w.messages.push(sourced(json!({"role":"user","name":"program_diagnostic","content":diagnostic.to_string()}), s.task, seq));
                save(s, w).await?;
                Ok(Round::Rewrite)
            } else if w.program_task.is_some() {
                // A runtime failure throws to the caller, as in JavaScript.
                return_value(s, w, step, json!({"$task_error":error})).await
            } else {
                anyhow::bail!("TS_PROGRAM_RUNTIME: {error}")
            }
        }
        State::Returned(value) => return_value(s, w, step, value).await,
        State::Action(action) => {
            let at = json!(action.at);
            let (name, args) = match &action.kind {
                ActionKind::Call { name, args } => {
                    let program = &w.program.as_ref().unwrap().program;
                    let function = &program.functions[name];
                    let mut task = function_task(program, name, args)?;
                    let description = task["description"].clone();
                    if !function.virtual_task {
                        task["body"] = json!({"compiled":program});
                    }
                    (
                        "create_task".into(),
                        json!({"goal":name,"briefing":description,"program_task":task,"call_site":at}),
                    )
                }
                ActionKind::Task {
                    name,
                    description,
                    input,
                } => {
                    let program = &w.program.as_ref().unwrap().program;
                    let arguments = program.functions[name]
                        .params
                        .iter()
                        .map(|parameter| {
                            input
                                .get(parameter)
                                .cloned()
                                .context("virtual argument missing")
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let task = function_task(program, name, &arguments)?;
                    (
                        "create_task".into(),
                        json!({"goal":name,"briefing":description,"program_task":task,"call_site":at}),
                    )
                }
                ActionKind::Tool { name, args } => (name.clone(), args.clone()),
                ActionKind::ParallelReads { operations } => (
                    "parallel_read_batch".into(),
                    json!({"operations":operations.iter().map(|(name,args)|json!({"name":name,"args":args})).collect::<Vec<_>>(), "settled": w.program.as_ref().unwrap().is_pending_async_batch()}),
                ),
                ActionKind::Fork { .. } | ActionKind::Join { .. } => anyhow::bail!("saved fork/join calls are retired"),
            };
            let node = format!("program_{}", agent_service::uuid_like());
            let call = crate::model_stream::ToolCall { call_id: action.id.clone(), name, arguments: args.to_string() };
            // A function call opens its frame; a tool step runs and its result is
            // the value of the program's expression. Neither enters an AI window.
            if call.name == "create_task" {
                let scope = task_frames::create_task(s, w, &call, &node, step).await?;
                return Ok(Round::Entered(scope));
            }
            let definitions = super::tools(s, w);
            let done = super::execute_call(s, w, &definitions, &node, step, &call, 1, Some(&at)).await?;
            super::stop_on_conflict(s, &done.result).await?;
            resolve(s, w, &action, done.result, done.is_error, done.seq)?;
            save(s, w).await?;
            Ok(Round::Continue)
        }
    }
}

/// The output pages a published example declares inline.
#[cfg(test)]
pub(super) fn example_pages(source: &str) -> Vec<&str> {
    source.split("path: \"").skip(1).filter_map(|rest| rest.split('"').next()).filter(|path| path.ends_with(".html")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct TestRoot(std::path::PathBuf);
    impl TestRoot {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("ts-program-test-{}", agent_service::uuid_like()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn path(&self) -> &std::path::Path { &self.0 }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[tokio::test]
    async fn unavailable_write_fails_compilation_before_any_tool_effect() {
        let root = TestRoot::new();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let app=Router::new().route("/v1/chat/completions",post(move |Json(_):Json<Value>|{
            let n=requests.fetch_add(1,Ordering::SeqCst);
            async move {
            if n==2 {return Json(json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"The write is unavailable in read-only mode; no file operation ran."}}]}));}
            let (name,args)=match n {
                0=>("create_task",json!({"goal":"Attempt denied write"})),
                1=>("execute_program",json!({"source":"function main(){io.write({path:'denied.txt',content:'x'});io.read({path:'next.txt'});}"})),
                _=>panic!("runtime error must stop, not ask model to reschedule"),
            };
            Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state =
            agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        state.permission_mode = agent_service::PermissionMode::ReadOnly;
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Attempt a write".into(),
            8,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(
            conn.query_row("SELECT status FROM agent_tasks", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "completed"
        );
        assert!(!root.path().join("denied.txt").exists());
        assert_eq!(count.load(Ordering::SeqCst), 3);
        let calls:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='read_file'",[],|r|r.get(0)).unwrap();
        assert_eq!(calls, 0, "operation after denied write must not execute");
    }



    #[test]
    fn source_adapters_reject_extensions_without_installing_a_program() {
        for source in [
            "function* work(){return 1;}",
            "function work(input: string){return input;}",
            "function work(){return ai({instruction:'value'});}",
            "function work(){return io.read({});} export function unused(){return 1;}",
        ] {
            let mut main = Window::default();
            assert!(install(&mut main, &json!({"source":source.replace("work", "main")})).is_err());
            assert!(main.program.is_none());
            let mut child = Window {
                program_task: Some(json!({"name":"work","parameters":[],"arguments":[]})),
                ..Default::default()
            };
            assert!(install_body(&mut child, &json!({"source":source})).is_err());
            assert!(child.program.is_none());
            assert!(
                compiled_runtime(&json!({"program":{"source":source,"entry":"work","args":[]}}))
                    .is_err()
            );
        }
    }


    #[test]
    fn refinement_uses_only_positional_arguments_and_never_packs_them() {
        let shared = json!({"rule":"R"});
        let item = json!({"path":"a"});
        for source in [
            "function work(shared,item){return {shared,item};}",
            "function work(rules,part){return {shared:rules,item:part};}",
        ] {
            let mut w = Window {
                program_task: Some(
                    json!({"name":"work","parameters":["shared","item"],"arguments":[shared,item]}),
                ),
                ..Default::default()
            };
            install_body(&mut w, &json!({"source":source})).unwrap();
            assert!(
                matches!(w.program.as_mut().unwrap().next(),State::Returned(value) if value==json!({"shared":shared,"item":item}))
            );
        }
        let mut w = Window {
            program_task: Some(
                json!({"name":"work","parameters":["shared","item"],"arguments":[shared,item]}),
            ),
            ..Default::default()
        };
        assert!(
            install_body(
                &mut w,
                &json!({"source":"function work(input){return input;}"})
            )
            .is_err()
        );
        assert!(w.program.is_none());
    }

    #[test]
    fn forked_functions_use_the_host_compiled_program() {
        let program=agent_program::compile("function work(input){return io.read({path:input.path});} function main(){return null;}").unwrap();
        let mut fork = compiled_runtime(
            &json!({"program":{"compiled":program,"entry":"work","args":[{"path":"source.txt"}]}}),
        )
        .unwrap();
        assert!(
            matches!(fork.next(),State::Action(Action{kind:ActionKind::Tool{name,args},..}) if name=="read_file" && args["path"]=="source.txt")
        );
    }


    #[test]
    fn fixed_libraries_respect_frame_permissions_and_reject_model_aliases() {
        let root = TestRoot::new();
        let mut state =
            agent_service::tests::test_state(root.path(), "127.0.0.1:9".parse().unwrap());
        state.permission_mode = agent_service::PermissionMode::ReadOnly;
        let cancel = CancellationToken::new();
        let s = Session {
            agent_id: None,
            task_node_id: None,
            plugin: None,
            state: &state,
            task: "task",
            model: "fake",
            turn: 1,
            cancel: &cancel,
            ban_execution: false,
            scope: "main".into(),
            parent: None,
            input_budget: INPUT_BUDGET,
        };
        let mut w = Window::default();
        let source = "function main(){io.read({path:'source'});io.write({path:'must-not-write'});}";
        assert!(install_scoped(&s, &mut w, &json!([]), false).is_err());
        assert!(install_scoped(&s, &mut w, &json!({"source":source}), false).is_err());
        assert!(w.program.is_none());
        assert!(install_scoped(&s,&mut w,&json!({"source":"function main(){return null;}","libraries":{"io2":{"write":"write_file"}}}),false).is_err());
    }




    #[test]
    fn source_diagnostics_preserve_the_rejected_program_and_existing_window() {
        let broken = include_str!("../agent_program/fixtures/virtual-body-missing-brace.js");
        let mut window = Window {
            program_task: Some(
                json!({"name":"author_pages","parameters":["args"],"arguments":[{}]}),
            ),
            ..Default::default()
        };
        let before = serde_json::to_value(&window).unwrap();
        let args = json!({"source":broken});
        let error = install_body(&mut window, &args).unwrap_err();
        let diagnostic = source_error("define_task_body", &args, &error);
        assert_eq!(diagnostic["code"], "PROGRAM_SOURCE_INVALID");
        assert_eq!(diagnostic["executed"], false);
        assert_eq!(diagnostic["retryable"], true);
        assert_eq!(diagnostic["diagnostics"][0]["line"], 17);
        assert!(diagnostic["diagnostics"][0]["column"].as_u64().unwrap() > 0);
        assert!(
            !diagnostic["diagnostics"][0]["source_excerpt"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(serde_json::to_value(&window).unwrap(), before);
        let fixed = format!("{broken}\n}}");
        install_body(&mut window, &json!({"source":fixed})).unwrap();
        assert!(window.program.is_some());

        let mut root = Window::default();
        let original = serde_json::to_value(&root).unwrap();
        assert!(
            install(
                &mut root,
                &json!({"source":"function main(){io.write({path:'never.txt'});"})
            )
            .is_err()
        );
        assert_eq!(serde_json::to_value(&root).unwrap(), original);
    }


    #[test]
    fn call_bindings_are_identical_for_every_parameter_name_and_original_order() {
        let value = json!({"pages":[{"path":"one.html"}],"input":{"legitimate":true}});
        for parameter in ["input", "args", "context", "bindings"] {
            let task = json!({"name":"work","description":"Use the supplied value","parameters":[parameter],"arguments":[value]});
            let view = call_view(&task).unwrap();
            assert_eq!(view["signature"], format!("function work({parameter})"));
            assert!(view.get("input").is_none());
            assert!(
                view.get("arguments").is_none(),
                "the view must not duplicate whole input bodies"
            );
            assert_eq!(view["bindings"][parameter], value);
            let mut window = Window {
                program_task: Some(task),
                ..Default::default()
            };
            install_body(
                &mut window,
                &json!({"source":format!("function work({parameter}){{return {parameter};}}") }),
            )
            .unwrap();
            assert!(
                matches!(window.program.as_mut().unwrap().next(),State::Returned(actual) if actual==value)
            );
        }
        let task =
            json!({"name":"work","parameters":["zeta","alpha"],"arguments":["first","second"]});
        let mut window = Window {
            program_task: Some(task),
            ..Default::default()
        };
        install_body(
            &mut window,
            &json!({"source":"function work(a,z){return [a,z];}"}),
        )
        .unwrap();
        assert!(
            matches!(window.program.as_mut().unwrap().next(),State::Returned(actual) if actual==json!(["first","second"]))
        );
        assert!(
            call_view(&json!({"name":"work","input":{"input":value}})).is_err(),
            "never guess the old envelope"
        );
    }

    #[test]
    fn refined_function_can_read_caller_variables_without_adding_parameters() {
        let mut window = Window {
            scope_values: BTreeMap::from([
                ("rules".into(), json!({"prefix":"from parent"})),
                ("unused".into(), json!("NOT_NEEDED")),
            ]),
            program_task: Some(json!({"name":"work","parameters":["item"],"arguments":["value"]})),
            ..Default::default()
        };
        install_body(
            &mut window,
            &json!({"source":"function work(item){return rules.prefix + ':' + item;}"}),
        )
        .unwrap();
        // Inherited names form the body's outermost scope.
        let runtime = window.program.as_mut().unwrap();
        assert!(matches!(runtime.next(), State::Returned(value) if value == "from parent:value"));
    }





    fn role_of(body: &Value) -> String {
        body["messages"].as_array().unwrap().iter().find(|m| m["name"] == "program_task_input")
            .map(|m| serde_json::from_str::<Value>(m["content"].as_str().unwrap()).unwrap()["role"].as_str().unwrap().to_owned())
            .unwrap_or_else(|| "main".into())
    }
    fn tool_names(body: &Value) -> Vec<String> {
        body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_owned()).collect()
    }
    fn reply(content: String) -> Value {
        json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":content}}]})
    }
    const TWO_LEVEL_MAIN: &str = "```js\nimport * as io from \"host:io\";\nconst result = await summarize([\"a.txt\", \"b.txt\"]);\nio.write({path: \"out.txt\", content: result.summary});\n\n/** @virtual */\nasync function summarize(paths) {\n  // Summarize the files at paths. Returns {summary:string}.\n}\n```";

    #[tokio::test]
    async fn a_main_virtual_is_implemented_as_code_and_its_leaf_loops_with_imported_tools() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(root.path().join("b.txt"), "beta").unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, Vec<String>, Value)>::new()));
        let log = seen.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let role = role_of(&body);
            let tools = tool_names(&body);
            let prompt = body["messages"][0]["content"].as_str().unwrap_or("").to_owned();
            let text = body.to_string();
            assert!(!text.contains("\"task_status\"") && !text.contains("read_scope"), "no task-tree status or scope reads reach the model");
            if role == "implement" {
                let headers = body["messages"].as_array().unwrap().iter().find(|m| m["name"] == "program_tool_headers").unwrap()["content"].as_str().unwrap().to_owned();
                for main_only in ["read_asset(", "read_task_board(", "list_context_assets(", "read_session_history("] {
                    assert!(!headers.contains(main_only), "an implementation's libraries hold no {main_only}");
                }
                let input = body["messages"].as_array().unwrap().iter().find(|m| m["name"] == "program_task_input").unwrap();
                let input: Value = serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                assert_eq!(input["signature"], "async function summarize(paths)", "the declared async is shown");
            }
            if role != "main" {
                for message in body["messages"].as_array().unwrap() {
                    assert!(message["role"] != "user" || message["name"] == "program_task_input",
                        "a virtual function sees only its own input: {message}");
                }
            }
            log.lock().unwrap().push((role.clone(), tools, json!(prompt)));
            let answer = match role.as_str() {
                "main" => reply(TWO_LEVEL_MAIN.into()),
                "implement" => reply("```js\nimport * as io from \"host:io\";\nasync function summarize(paths) {\n  const texts = [];\n  for (const path of paths) { texts.push(io.read({path: path}).content); }\n  return judge(texts);\n}\n/** @virtual */\nfunction judge(texts) {\n  // Join the texts into one summary. Returns {summary:string}.\n}\n```".into()),
                "leaf" => {
                    let input = body["messages"].as_array().unwrap().iter().find(|m| m["name"] == "program_task_input").unwrap();
                    let input: Value = serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    assert_eq!(input["bindings"]["texts"], json!(["alpha", "beta"]));
                    reply(json!({"summary":"alpha+beta"}).to_string())
                }
                other => panic!("unexpected role {other}"),
            };
            async move { Json(answer) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Summarize a and b".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "alpha+beta");
        // The program's reads and writes ran as its own steps: no AI window holds them.
        let conn = agent_service::open_db(root.path()).unwrap();
        let windows = conn.prepare("SELECT state FROM agent_runtime_windows WHERE task_id='task'").unwrap()
            .query_map([], |r| r.get::<_, String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        for window in windows {
            let window: Value = serde_json::from_str(&window).unwrap();
            assert!(window["messages"].as_array().unwrap().iter().all(|m| m["tool_calls"].is_null() && m["role"] != "tool"),
                "no synthesized program exchange in {window}");
        }
        let program_steps: i64 = conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.program_location') IS NOT NULL", [], |r| r.get(0)).unwrap();
        assert!(program_steps >= 3, "the program's steps are still recorded, with their source location");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().map(|(role, _, _)| role.as_str()).collect::<Vec<_>>(), ["main", "implement", "leaf"]);
        let (_, implement, prompt) = &seen[1];
        assert_eq!(implement, &["read_tool_contracts", "read_program_guide"], "an implementation is code: it only reads contracts and the guide");
        assert!(prompt.as_str().unwrap().starts_with("You implement one @virtual function"));
        let (_, leaf, prompt) = &seen[2];
        assert!(prompt.as_str().unwrap().starts_with("You perform one @virtual function"));
        assert!(leaf.contains(&"read_file".to_owned()) && leaf.contains(&FIND.to_owned()));
        for retired in ["read_tool_contracts", "define_task_body", "invoke_library", "create_task", "read_scope", "read_asset"] {
            assert!(!leaf.contains(&retired.to_owned()), "leaf must not see {retired}");
        }
        let definitions = crate::agent_service::local_tools(&crate::tools::Workspace::new(root.path().to_path_buf()).unwrap(), None, false);
        let io = agent_program::libraries::catalog(&definitions, &json!([]))["io"].values().cloned().collect::<BTreeSet<_>>();
        assert_eq!(leaf.iter().filter(|name| *name != FIND).cloned().collect::<BTreeSet<_>>(), io,
            "leaf tools are exactly the implementation's io import");
    }

    #[tokio::test]
    async fn a_leaf_that_outgrows_its_context_throws_to_its_coded_caller() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("a.txt"), "alpha ".repeat(4000)).unwrap();
        std::fs::write(root.path().join("b.txt"), "beta").unwrap();
        let leaf_rounds = Arc::new(AtomicUsize::new(0));
        let rounds = leaf_rounds.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let answer = match role_of(&body).as_str() {
                "main" => reply(TWO_LEVEL_MAIN.into()),
                "implement" => reply("```js\nimport * as io from \"host:io\";\nasync function summarize(paths) {\n  try {\n    return judge(paths);\n  } catch (error) {\n    return {summary: error.code + \":\" + error.completed_writes.length};\n  }\n}\n/** @virtual */\nfunction judge(paths) {\n  // Read every file at paths and summarize them. Returns {summary:string}.\n}\n```".into()),
                "leaf" => {
                    let n = rounds.fetch_add(1, Ordering::SeqCst);
                    assert!(n < 40, "the host must end the leaf instead of folding its context");
                    let call = json!({"id":format!("read-{n}"),"type":"function","function":{"name":"read_file","arguments":json!({"path":"a.txt"}).to_string()}});
                    json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[call]}}]})
                }
                other => panic!("unexpected role {other}"),
            };
            async move { Json(answer) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        state.provider_routes.insert(state.provider_name.clone(), agent_service::AgentProviderRoute {
            prefix_system_messages: false, url: state.provider_url.clone(), api_key: state.api_key.clone(),
            models: Default::default(), max_output_tokens: Default::default(),
            context_windows: [("fake".to_owned(), 64_000)].into_iter().collect(),
        });
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Summarize a and b".into(), 60, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert!(leaf_rounds.load(Ordering::SeqCst) > 1);
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "CONTEXT_OVERFLOW:0");
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='context/step_fold' OR kind LIKE 'context/fold%'",[],|r|r.get::<_,i64>(0)).unwrap(), 0);
    }

    async fn run_scripted(root: &std::path::Path, answer: impl Fn(&Value) -> Value + Clone + Send + Sync + 'static) {
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let answer = answer(&body);
            async move { Json(answer) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root, listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Summarize a and b".into(), 20, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn a_leaf_finds_a_missing_method_for_its_own_loop_only() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("a.txt"), "alpha").unwrap();
        let leaf_tools = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let log = leaf_tools.clone();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" => reply(TWO_LEVEL_MAIN.into()),
            "implement" => reply("```js\nasync function summarize(paths) {\n  return judge(paths[0]);\n}\n/** @virtual */\nfunction judge(path) {\n  // Read the file at path. Returns {summary:string} with its text.\n}\n```".into()),
            _ => {
                let tools = tool_names(body);
                let mut log = log.lock().unwrap();
                log.push(tools);
                let call = |id: &str, name: &str, args: Value| json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
                    "tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]});
                match log.len() {
                    1 => call("find", FIND, json!({"query":"read file"})),
                    2 => call("read", "read_file", json!({"path":"a.txt"})),
                    _ => reply(json!({"summary":"alpha"}).to_string()),
                }
            }
        }).await;
        let leaf_tools = leaf_tools.lock().unwrap();
        assert_eq!(leaf_tools[0], [FIND], "nothing imported, nothing given");
        assert!(leaf_tools[1].contains(&"read_file".to_owned()), "a found method becomes callable");
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "alpha");
    }

    #[tokio::test]
    async fn an_implementation_must_be_code_and_its_runtime_error_goes_back_to_its_author() {
        let root = TestRoot::new();
        let implement = Arc::new(AtomicUsize::new(0));
        let rounds = implement.clone();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" => reply("```js\nimport * as io from \"host:io\";\nlet text;\ntry {\n  text = (await summarize([\"a.txt\"])).summary;\n} catch (error) {\n  text = \"caught:\" + error.message;\n}\nio.write({path: \"out.txt\", content: text});\n\n/** @virtual */\nasync function summarize(paths) {\n  // Summarize the files at paths. Returns {summary:string}.\n}\n```".into()),
            _ => {
                let messages = body["messages"].as_array().unwrap();
                let diagnostic = |code: &str| messages.iter().find(|m| m["name"] == "program_diagnostic" && m["content"].as_str().unwrap().contains(code)).cloned();
                match rounds.fetch_add(1, Ordering::SeqCst) {
                    0 => reply("{\"summary\":\"answered without code\"}".into()),
                    1 => {
                        assert!(diagnostic("FUNCTION_IMPLEMENTATION_REQUIRED").is_some());
                        reply("```js\nimport * as io from \"host:io\";\nasync function summarize(paths) {\n  io.write({path: \"partial.txt\", content: \"done\"});\n  const missing = null;\n  return {summary: missing.text};\n}\n```".into())
                    }
                    2 => {
                        let error = diagnostic("FUNCTION_RUNTIME_ERROR").expect("the author sees its runtime error");
                        let error: Value = serde_json::from_str(error["content"].as_str().unwrap()).unwrap();
                        assert_eq!(error["completed_writes"], json!([{"tool":"write_file","path":"partial.txt"}]));
                        assert!(messages.iter().all(|m| m["tool_calls"].is_null() && m["role"] != "tool"), "the program's own steps are not replayed to the author");
                        reply("```js\nasync function summarize(paths) {\n  return {summary: \"fixed\"};\n}\n```".into())
                    }
                    n => panic!("a corrected implementation needs no more requests: {n}"),
                }
            }
        }).await;
        assert_eq!(implement.load(Ordering::SeqCst), 3);
        assert_eq!(std::fs::read_to_string(root.path().join("partial.txt")).unwrap(), "done");
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "fixed");
    }

    #[tokio::test]
    async fn an_implementation_that_cannot_meet_its_contract_gives_up_to_the_caller() {
        let root = TestRoot::new();
        let implement = Arc::new(AtomicUsize::new(0));
        let rounds = implement.clone();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" => reply("```js\nimport * as io from \"host:io\";\nlet text;\ntry {\n  text = (await summarize([\"a.txt\"])).summary;\n} catch (error) {\n  text = \"caught:\" + error.message;\n}\nio.write({path: \"out.txt\", content: text});\n\n/** @virtual */\nasync function summarize(paths) {\n  // Summarize the files at paths. Returns {summary:string}.\n}\n```".into()),
            _ => match rounds.fetch_add(1, Ordering::SeqCst) {
                0 => reply("```js\nasync function summarize(paths) {\n  throw new Error(\"boom\");\n}\n```".into()),
                1 => reply(json!({"$task_error":{"message":"no source files"}}).to_string()),
                n => panic!("giving up ends the function: {n}"),
            },
        }).await;
        assert_eq!(implement.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "caught:no source files");
        // The function's frame shows where it was called and that it failed.
        let conn = agent_service::open_db(root.path()).unwrap();
        let frame: Value = serde_json::from_str(&conn.query_row(
            "SELECT data FROM agent_task_events WHERE kind='task/node_returned' AND json_extract(data,'$.inputs.program_task.name')='summarize'",
            [], |r| r.get::<_, String>(0)).unwrap()).unwrap();
        assert_eq!(frame["status"], "failed");
        assert_eq!(frame["metadata"]["call_site"]["line"], 4, "the await summarize(...) line");
        assert!(frame["metadata"]["caller"]["task"].is_string(), "the frame knows its caller");
    }

    #[tokio::test]
    async fn a_nested_virtual_helper_runs_as_a_leaf_under_its_own_name() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(root.path().join("b.txt"), "beta").unwrap();
        let leaf_names = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = leaf_names.clone();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" => reply(TWO_LEVEL_MAIN.into()),
            "implement" => reply("```js\nimport * as io from \"host:io\";\nasync function summarize(paths) {\n  /** @virtual */\n  function join(texts) {\n    // Join the texts. Returns {summary:string}.\n  }\n  const texts = [];\n  for (const path of paths) { texts.push(io.read({path: path}).content); }\n  return join(texts);\n}\n```".into()),
            _ => {
                let input = body["messages"].as_array().unwrap().iter().find(|m| m["name"] == "program_task_input").unwrap();
                let input: Value = serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                log.lock().unwrap().push(input["name"].as_str().unwrap().to_owned());
                assert!(tool_names(body).contains(&"read_file".to_owned()), "the implementation's io import");
                reply(json!({"summary":input["bindings"]["texts"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect::<Vec<_>>().join("+")}).to_string())
            }
        }).await;
        assert_eq!(*leaf_names.lock().unwrap(), ["join"]);
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "alpha+beta");
    }

    #[test]
    fn a_closing_fence_on_the_last_code_line_ends_the_program() {
        let message = json!({"content":"```js\nfunction f() {\n  return 1;\n}```\nDone."});
        assert_eq!(reply_source(&message).unwrap().unwrap(), "function f() {\n  return 1;\n}");
    }

    #[tokio::test]
    async fn a_later_turn_reads_earlier_programs_from_the_conversation_history() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(root.path().join("b.txt"), "beta").unwrap();
        let read_back = Arc::new(std::sync::Mutex::new(None::<Value>));
        let log = read_back.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let turn2 = body["messages"].to_string().contains("What did the last program do");
            let answer = match role_of(&body).as_str() {
                "main" if !turn2 => reply(TWO_LEVEL_MAIN.into()),
                "main" => {
                    let messages = body["messages"].as_array().unwrap();
                    match messages.iter().rev().find(|m| m["role"] == "tool") {
                        None => {
                            assert!(!body.to_string().contains("async function summarize"), "no earlier code sits in the new turn's context");
                            assert!(tool_names(&body).contains(&"read_session_history".to_owned()), "the main AI reads history itself");
                            json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
                                "tool_calls":[{"id":"history","type":"function","function":{"name":"read_session_history","arguments":json!({"programs":true,"turn":1}).to_string()}}]}}]})
                        }
                        Some(result) => {
                            *log.lock().unwrap() = Some(serde_json::from_str(result["content"].as_str().unwrap()).unwrap());
                            reply("It summarized a.txt and b.txt into out.txt.".into())
                        }
                    }
                }
                "implement" => reply("```js\nimport * as io from \"host:io\";\nasync function summarize(paths) {\n  const texts = [];\n  for (const path of paths) { texts.push(io.read({path: path}).content); }\n  return judge(texts);\n}\n/** @virtual */\nfunction judge(texts) {\n  // Join the texts into one summary. Returns {summary:string}.\n}\n```".into()),
                _ => reply(json!({"summary":"alpha+beta"}).to_string()),
            };
            async move { Json(answer) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state.clone(), "task".into(), "fake".into(), "Summarize a and b".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        run_task(state, "task".into(), "fake".into(), "What did the last program do?".into(), 10, CancellationToken::new(), 2, false).await.unwrap();
        server.abort();
        let read_back = read_back.lock().unwrap().clone().expect("the second turn read the history");
        let programs = read_back["programs"].as_array().unwrap();
        assert_eq!(programs.len(), 2, "the main program and the implementation it led to: {read_back}");
        assert!(programs[0]["function"].is_null() && programs[0]["source"].as_str().unwrap().contains("await summarize("));
        assert_eq!(programs[1]["function"], "summarize");
        assert!(programs[1]["source"].as_str().unwrap().contains("function judge("));
    }
}
