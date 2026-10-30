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
    captured: &BTreeMap<String, Value>,
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
            "captured":captured,"role":role,"leaf_tools":function.leaf_tools,"async":function.is_async}),
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
pub(super) const ASK: &str = "ask_user";
/// The error a function ends with when its AI asks the user. It rises to the
/// main program without being caught by code or rewritten by an author.
pub(super) const WAITING: &str = "WAITING_FOR_USER";
/// A program the user's message paused; its outcome says where.
pub(super) const PAUSED: &str = "PAUSED";
/// The window is a frame of a running program: the main program or a function it called.
pub(super) fn in_program(w: &Window) -> bool {
    w.program.is_some() || w.program_task.is_some()
}
pub(super) fn ask_tool() -> Value {
    function(ASK, "Ask the user for a decision or information this function cannot get otherwise. The program stops here and your question is shown to the user; their answer, whatever it is, starts a new turn in which the work is planned again. Use it only when the work truly depends on the user.",
        json!({"question":{"type":"string","minLength":1}}), json!(["question"]))
}
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
        "role":task["role"],"description":task["description"],"bindings":bindings,"closure":task["captured"],
        "binding_rule":"bindings lists actual local parameter values. Use each parameter directly as in the signature; bindings is display metadata, never another property of a parameter. Arguments bind by position only, never by name or automatic object packing. closure lists the outer variables the description names with @uses, read when this function was called; in code they are variables in scope, as in a JavaScript closure."}),
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
    if failed {
        locate_question(action, &mut value);
    }
    w.program.as_mut().unwrap().resume(value, failed)
}
/// A question rising through a call gains the caller's step, so its outcome
/// says where the program stopped, as a pause's does.
fn locate_question(action: &Action, error: &mut Value) {
    if error["code"] != WAITING {
        return;
    }
    let step = super::paused_programs::step_label(action);
    error["stopped_at"] = json!(match error["stopped_at"].as_str().filter(|at| !at.is_empty()) {
        Some(inner) => format!("{step} → {inner}"),
        None => step,
    });
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
        if failed {
            locate_question(&action, &mut value);
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
/// How many times a turn's main program may be rewritten after an error.
const MAIN_REWRITES: i64 = 3;
fn main_rewrites(s: &Session<'_>) -> Result<i64> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    Ok(conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id=?1 AND kind='program/runtime_error'
        AND json_extract(data,'$.main')=1 AND json_extract(data,'$.turn')=?2", params![s.task, s.turn as i64], |r| r.get(0))?)
}
/// Files written this turn by any of its programs or AIs, for its outcome.
fn turn_writes(s: &Session<'_>) -> Result<Vec<String>> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let paths = conn
        .prepare("SELECT DISTINCT json_extract(json_extract(c.data,'$.arguments'),'$.path') FROM agent_task_events c
            WHERE c.task_id=?1 AND c.kind='tool/call' AND json_extract(c.data,'$.turn')=?2
            AND json_extract(c.data,'$.name') IN ('write_file','edit_file','replace_range')
            AND EXISTS (SELECT 1 FROM agent_task_events r WHERE r.task_id=c.task_id AND r.kind='tool/result'
                AND json_extract(r.data,'$.message.toolCallId')=json_extract(c.data,'$.callId')
                AND json_extract(r.data,'$.message.isError')=0)
            ORDER BY c.seq")?
        .query_map(params![s.task, s.turn as i64], |r| r.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(paths.into_iter().flatten().collect())
}
/// The program's latest console lines, short enough for an outcome.
fn printed(w: &Window) -> Vec<String> {
    const SHOWN: usize = 20;
    let logs = w.program.as_ref().map(|program| program.logs()).unwrap_or_default();
    let mut shown = logs.iter().rev().take(SHOWN).rev().cloned().collect::<Vec<_>>();
    if logs.len() > SHOWN {
        shown.insert(0, format!("（前面还有 {} 行）", logs.len() - SHOWN));
    }
    shown
}
/// How a turn's main program ended, in a few lines: what it returned or where
/// it stopped, and the files it wrote. It is the turn's answer and what later
/// turns see of it; the code itself stays in the conversation record.
fn outcome(s: &Session<'_>, printed: &[String], ended: std::result::Result<&Value, &Value>) -> Result<String> {
    let clip = |text: &str, limit: usize| {
        if text.chars().count() > limit { format!("{}…", text.chars().take(limit).collect::<String>()) } else { text.to_owned() }
    };
    let mut lines = match ended {
        Ok(value) if value.is_null() => vec!["程序已完成。".to_owned()],
        Ok(value) => {
            let text = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
            vec!["程序已完成。".to_owned(), format!("返回：{}", clip(&text, 300))]
        }
        Err(error) if error["code"] == WAITING => {
            let question = error["question"].as_str().unwrap_or("");
            let at = error["stopped_at"].as_str().filter(|at| !at.is_empty()).map(str::to_owned)
                .unwrap_or_else(|| error["function"].as_str().unwrap_or("程序").to_owned());
            vec![
                format!("程序停在 {at}，等你回答：{question}"),
                "之后的步骤没有执行。".to_owned(),
            ]
        }
        Err(error) if error["code"] == PAUSED => {
            let at = error["stopped_at"].as_str().filter(|at| !at.is_empty())
                .map(|at| format!("停在 {at}，这一步还没完成")).unwrap_or_else(|| "停在两步之间".into());
            vec![
                format!("程序被你的新消息暂停，{at}。"),
                "它的状态都保留着：要继续，在新程序里 import * as previous from \"host:previous/promises\"，previous.resume() 会从停下的地方接着跑，返回它的结果（Promise）；不调用，它会在这一轮结束时丢弃。".to_owned(),
            ]
        }
        Err(error) => {
            let message = error["message"].as_str().map(str::to_owned).unwrap_or_else(|| error.to_string());
            let at = error["location"]["line"].as_u64().map(|line| format!("第 {line} 行")).unwrap_or_default();
            vec![format!("程序在{at}出错停止：{}", clip(&message, 300)), "之后的步骤没有执行。".to_owned()]
        }
    };
    // What the program printed reaches the AI that wrote it.
    if !printed.is_empty() {
        lines.push(format!("输出：\n{}", printed.join("\n")));
    }
    let writes = turn_writes(s)?;
    if !writes.is_empty() {
        let shown = writes.iter().take(20).cloned().collect::<Vec<_>>().join("、");
        let more = if writes.len() > 20 { format!(" 等 {} 个文件", writes.len()) } else { String::new() };
        lines.push(format!("写入：{shown}{more}"));
    }
    Ok(lines.join("\n"))
}
/// Pause the main program for the user's new message; its outcome is the
/// reply the message then follows, and the main AI decides what runs next.
pub(super) async fn pause_main(s: &Session<'_>, w: &mut Window, step: usize) -> Result<()> {
    let console = printed(w);
    let stopped_at = super::paused_programs::pause_main(s, w).await?;
    let text = outcome(s, &console, Err(&json!({"code":PAUSED,"stopped_at":stopped_at})))?;
    end_program(s, w, step, Value::Null, text).await?;
    // The turn goes on: the main AI may answer the message with a new program.
    w.program_value = None;
    save(s, w).await
}
async fn return_value(s: &Session<'_>, w: &mut Window, step: usize, value: Value) -> Result<Round> {
    let text = if w.program_task.is_some() {
        serde_json::to_string(&value)?
    } else {
        outcome(s, &printed(w), Ok(&value))?
    };
    end_program(s, w, step, value, text).await
}
/// End the window's program with this reply: a function returns its value,
/// a main program its outcome. The frame then finishes like any reply.
async fn end_program(s: &Session<'_>, w: &mut Window, step: usize, value: Value, text: String) -> Result<Round> {
    w.program_value = Some(value);
    w.program = None;
    let message = json!({"role":"assistant","content":text});
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
            // A question for the user is not a bug to fix: it rises unchanged.
            let waiting = error["code"] == WAITING;
            // Read before the program is dropped below.
            let console = printed(w);
            if !waiting && role(w) == Role::Implement && w.program_task.as_ref().is_some_and(|task| task["role"] == "implement") {
                // AI-written code is not right every time: its author gets the
                // error and what already ran, and rewrites it or gives up.
                w.program = None;
                let diagnostic = json!({"code":"FUNCTION_RUNTIME_ERROR","executed":true,"error":error,
                    "completed_writes":completed_writes(s, w)?,"console":console,
                    "instruction":"Your implementation stopped with this error. Reply with a corrected complete ```js block; it runs from its start, so do not redo the listed writes unless the contract needs them redone. If the contract cannot be met, reply with {\"$task_error\":{\"message\":\"concrete cause\"}} instead."});
                let seq = append(s, "program/runtime_error", json!({"diagnostic":diagnostic}), false).await?;
                w.messages.push(sourced(json!({"role":"user","name":"program_diagnostic","content":diagnostic.to_string()}), s.task, seq));
                save(s, w).await?;
                Ok(Round::Rewrite)
            } else if w.program_task.is_some() {
                // A runtime failure throws to the caller, as in JavaScript.
                return_value(s, w, step, json!({"$task_error":error})).await
            } else if !waiting && main_rewrites(s)? < MAIN_REWRITES {
                // The main program heals the same way, a few times a turn: the
                // main AI gets the error and the files already written.
                w.program = None;
                let diagnostic = json!({"code":"PROGRAM_RUNTIME_ERROR","executed":true,"error":error,
                    "completed_writes":turn_writes(s)?,"console":console,
                    "instruction":"Your program stopped with this error. Reply with a corrected complete ```js block; it runs from its start, so do not redo the listed writes unless the task needs them redone. If the task cannot be done, tell the user why in plain text instead."});
                let seq = append(s, "program/runtime_error", json!({"diagnostic":diagnostic,"turn":s.turn,"main":true}), false).await?;
                w.messages.push(sourced(json!({"role":"user","name":"program_diagnostic","content":diagnostic.to_string()}), s.task, seq));
                save(s, w).await?;
                Ok(Round::Rewrite)
            } else {
                // A question, or a program that keeps failing: its outcome ends
                // the turn, so the next message starts fresh instead of
                // resuming the failure.
                let text = outcome(s, &console, Err(&error))?;
                end_program(s, w, step, Value::Null, text).await
            }
        }
        State::Returned(value) => return_value(s, w, step, value).await,
        State::Action(action) => {
            let at = json!(action.at);
            let (name, args) = match &action.kind {
                ActionKind::Call { name, args, captured } => {
                    let program = &w.program.as_ref().unwrap().program;
                    let function = &program.functions[name];
                    let mut task = function_task(program, name, args, captured)?;
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
                    // A nested virtual's captured variables ride in its input.
                    let captured = program.functions[name].uses.iter()
                        .map(|used| Ok((used.clone(), input.get(used).cloned().context("captured variable missing")?)))
                        .collect::<Result<BTreeMap<_, _>>>()?;
                    let task = function_task(program, name, &arguments, &captured)?;
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
            // Resuming runs the paused program first; when the user speaks
            // again it pauses once more and this step stays pending.
            if call.name == super::paused_programs::RESUME && w.resumed.is_none() {
                let Some(resumed) = super::paused_programs::resume(s, w).await? else {
                    return Ok(Round::Continue);
                };
                w.resumed = Some(resumed);
            }
            let definitions = super::tools(s, w);
            let done = super::execute_call(s, w, &definitions, &node, step, &call, 1, Some(&at)).await?;
            super::stop_on_conflict(s, &done.result).await?;
            // A resumed program's error rises as it was, so a question keeps its code.
            let (result, is_error) = match w.resumed.take() {
                Some((error, true)) => (error, true),
                _ => (done.result, done.is_error),
            };
            resolve(s, w, &action, result, is_error, done.seq)?;
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
        assert_eq!(leaf.iter().filter(|name| *name != FIND && *name != ASK).cloned().collect::<BTreeSet<_>>(), io,
            "leaf tools are exactly the implementation's io import, plus finding methods and asking the user");
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
        assert_eq!(leaf_tools[0], [FIND, ASK], "nothing imported: only finding methods and asking the user");
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

    fn memory_of(body: &Value) -> Option<Value> {
        body["messages"].as_array().unwrap().iter().find(|m| m["name"] == "conversation_memory")
            .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
    }

    #[tokio::test]
    async fn a_program_outcome_is_the_turns_answer_and_what_the_next_turn_sees() {
        let root = TestRoot::new();
        let memory = Arc::new(std::sync::Mutex::new(None::<Value>));
        let log = memory.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let answer = if body["messages"].to_string().contains("And now?") {
                *log.lock().unwrap() = memory_of(&body);
                reply("Noted.".into())
            } else {
                reply("```js\nimport * as io from \"host:io\";\nio.write({path: \"one.txt\", content: \"1\"});\nio.write({path: \"two.txt\", content: \"2\"});\n```".into())
            };
            async move { Json(answer) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state.clone(), "task".into(), "fake".into(), "Write two files".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        run_task(state, "task".into(), "fake".into(), "And now?".into(), 10, CancellationToken::new(), 2, false).await.unwrap();
        server.abort();
        let memory = memory.lock().unwrap().clone().expect("the next turn has the conversation memory");
        let entry = memory["entries"].as_array().unwrap().iter().find(|e| e["kind"] == "task").unwrap().clone();
        assert_eq!(entry["answer"], "程序已完成。\n写入：one.txt、two.txt", "{memory}");
    }

    #[tokio::test]
    async fn a_main_program_that_keeps_failing_ends_its_turn_and_the_conversation_goes_on() {
        let root = TestRoot::new();
        let main = Arc::new(AtomicUsize::new(0));
        let rounds = main.clone();
        let memory = Arc::new(std::sync::Mutex::new(None::<Value>));
        let log = memory.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let answer = if body["messages"].to_string().contains("What happened?") {
                *log.lock().unwrap() = memory_of(&body);
                reply("It stopped with an error.".into())
            } else {
                rounds.fetch_add(1, Ordering::SeqCst);
                reply("```js\nimport * as io from \"host:io\";\nio.write({path: \"done.txt\", content: \"1\"});\nthrow new Error(\"boom\");\n```".into())
            };
            async move { Json(answer) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state.clone(), "task".into(), "fake".into(), "Do it".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        run_task(state, "task".into(), "fake".into(), "What happened?".into(), 10, CancellationToken::new(), 2, false).await.unwrap();
        server.abort();
        assert_eq!(main.load(Ordering::SeqCst), 4, "the first program and three rewrites");
        let memory = memory.lock().unwrap().clone().expect("the next turn reached the model instead of re-running the failure");
        let entry = memory["entries"].as_array().unwrap().iter().find(|e| e["kind"] == "task").unwrap().clone();
        assert_eq!(entry["answer"], "程序在第 3 行出错停止：boom\n之后的步骤没有执行。\n写入：done.txt", "{memory}");
        assert_eq!(conn.query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r| r.get::<_, String>(0)).unwrap(), "completed");
    }

    #[tokio::test]
    async fn a_leaf_question_stops_the_program_and_the_answer_starts_a_new_turn() {
        let root = TestRoot::new();
        let implement = Arc::new(AtomicUsize::new(0));
        let rounds = implement.clone();
        let memory = Arc::new(std::sync::Mutex::new(None::<Value>));
        let log = memory.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let answer = match role_of(&body).as_str() {
                "main" if body["messages"].to_string().contains("Yes, keep both") => {
                    *log.lock().unwrap() = memory_of(&body);
                    reply("Keeping both.".into())
                }
                "main" => reply(TWO_LEVEL_MAIN.into()),
                "implement" => {
                    assert_eq!(rounds.fetch_add(1, Ordering::SeqCst), 0, "a question is not handed back to the author as a bug");
                    reply("```js\nasync function summarize(paths) {\n  return judge(paths);\n}\n/** @virtual */\nfunction judge(paths) {\n  // Decide which files to keep. Returns {summary:string}.\n}\n```".into())
                }
                _ => {
                    assert!(tool_names(&body).contains(&ASK.to_owned()), "a leaf can ask the user");
                    json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
                        "tool_calls":[{"id":"ask","type":"function","function":{"name":ASK,"arguments":json!({"question":"Keep both files?"}).to_string()}}]}}]})
                }
            };
            async move { Json(answer) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state.clone(), "task".into(), "fake".into(), "Summarize a and b".into(), 20, CancellationToken::new(), 1, false).await.unwrap();
        let shown: String = conn.query_row("SELECT json_extract(data,'$.message.content') FROM agent_task_events WHERE kind='assistant/message' AND json_extract(data,'$.actor')='program' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(shown, "程序停在 第 2 行 summarize([\"a.txt\",\"b.txt\"]) → 第 2 行 judge([\"a.txt\",\"b.txt\"])，等你回答：Keep both files?\n之后的步骤没有执行。");
        assert!(!root.path().join("out.txt").exists(), "nothing after the question ran");
        run_task(state, "task".into(), "fake".into(), "Yes, keep both".into(), 10, CancellationToken::new(), 2, false).await.unwrap();
        server.abort();
        let memory = memory.lock().unwrap().clone().expect("the answer reached the main AI in a new turn");
        assert!(memory.to_string().contains("等你回答：Keep both files?"), "the new turn knows what was asked: {memory}");
    }

    #[tokio::test]
    async fn code_that_catches_errors_can_tell_a_question_apart_and_rethrow_it() {
        let root = TestRoot::new();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" => reply(TWO_LEVEL_MAIN.into()),
            "implement" => reply("```js\nasync function summarize(paths) {\n  try {\n    return judge(paths);\n  } catch (error) {\n    if (error.name === \"WaitingForUser\") throw error;\n    return {summary: \"fallback\"};\n  }\n}\n/** @virtual */\nfunction judge(paths) {\n  // Decide which files to keep. Returns {summary:string}.\n}\n```".into()),
            _ => json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
                "tool_calls":[{"id":"ask","type":"function","function":{"name":ASK,"arguments":json!({"question":"Keep both files?"}).to_string()}}]}}]}),
        }).await;
        let conn = agent_service::open_db(root.path()).unwrap();
        let shown: String = conn.query_row("SELECT json_extract(data,'$.message.content') FROM agent_task_events WHERE kind='assistant/message' AND json_extract(data,'$.actor')='program' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(shown, "程序停在 第 2 行 summarize([\"a.txt\",\"b.txt\"]) → 第 3 行 judge([\"a.txt\",\"b.txt\"])，等你回答：Keep both files?\n之后的步骤没有执行。", "the catch-all let the question through");
        assert!(!root.path().join("out.txt").exists(), "the fallback did not run");
    }

    const PAUSED_AT: &str = "程序被你的新消息暂停，停在 第 2 行 summarize([\"a.txt\",\"b.txt\"]) → 第 3 行 judge([\"a.txt\",\"b.txt\"])（AI 处理中），这一步还没完成。";
    const RESUMING_MAIN: &str = "```js\nimport * as io from \"host:io\";\nimport * as previous from \"host:previous/promises\";\nconst old = previous.resume();\nio.write({path: \"extra.txt\", content: \"extra\"});\nawait old;\n```";
    const CATCHING_IMPLEMENT: &str = "```js\nasync function summarize(paths) {\n  try {\n    return judge(paths);\n  } catch (error) {\n    return {summary: \"fallback\"};\n  }\n}\n/** @virtual */\nfunction judge(paths) {\n  // Decide which files to keep. Returns {summary:string}.\n}\n```";
    fn has_tool_result(body: &Value) -> bool {
        body["messages"].as_array().is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"))
    }
    fn find_call() -> Value {
        json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"find","type":"function","function":{"name":FIND,"arguments":json!({"query":"read"}).to_string()}}]}}]})
    }
    fn program_lines(conn: &rusqlite::Connection) -> Vec<String> {
        conn.prepare("SELECT json_extract(data,'$.message.content') FROM agent_task_events WHERE kind='assistant/message' AND json_extract(data,'$.actor')='program' ORDER BY seq").unwrap()
            .query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    }
    fn paused_nodes(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM agent_task_nodes WHERE task_id='task' AND status IN ('paused','running','suspended') AND json_extract(data,'$.metadata.parallel')=1", [], |r| r.get(0)).unwrap()
    }

    #[tokio::test]
    async fn a_user_message_pauses_the_program_and_the_new_one_resumes_it() {
        let root = TestRoot::new();
        let path = root.path().to_owned();
        let leaf = Arc::new(AtomicUsize::new(0));
        let leaf_rounds = leaf.clone();
        let heard = Arc::new(std::sync::Mutex::new(None::<String>));
        let log = heard.clone();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" if body["messages"].to_string().contains("程序被你的新消息暂停") => {
                *log.lock().unwrap() = Some(body["messages"].to_string());
                reply(RESUMING_MAIN.into())
            }
            "main" => reply(TWO_LEVEL_MAIN.into()),
            "implement" => reply(CATCHING_IMPLEMENT.into()),
            _ if has_tool_result(body) => {
                leaf_rounds.fetch_add(1, Ordering::SeqCst);
                assert!(!body["messages"].to_string().contains("Also write extra"), "the leaf sees only its own arguments");
                reply(json!({"summary":"kept both"}).to_string())
            }
            _ => {
                leaf_rounds.fetch_add(1, Ordering::SeqCst);
                crate::interjections::submit(&path, "task", "Also write extra").unwrap();
                find_call()
            }
        }).await;
        let heard = heard.lock().unwrap().clone().expect("the main AI heard the message after the outcome");
        assert!(heard.contains("Also write extra"), "{heard}");
        assert_eq!(leaf.load(Ordering::SeqCst), 2, "the leaf went on from its next step");
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "kept both", "the paused program finished as planned");
        assert!(root.path().join("extra.txt").exists(), "the new work ran too");
        let conn = agent_service::open_db(root.path()).unwrap();
        let lines = program_lines(&conn);
        assert!(lines.iter().any(|line| line.starts_with(PAUSED_AT)), "{lines:?}");
        assert!(lines.last().unwrap().starts_with("程序已完成。"), "{lines:?}");
        assert_eq!(paused_nodes(&conn), 0, "nothing is left paused");
    }

    #[tokio::test]
    async fn a_paused_program_not_resumed_is_discarded_with_the_turn() {
        let root = TestRoot::new();
        let path = root.path().to_owned();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" if body["messages"].to_string().contains("程序被你的新消息暂停") => reply("Stopped as you asked.".into()),
            "main" => reply(TWO_LEVEL_MAIN.into()),
            "implement" => reply(CATCHING_IMPLEMENT.into()),
            _ => {
                assert!(!has_tool_result(body), "a discarded leaf does not run on");
                crate::interjections::submit(&path, "task", "Stop").unwrap();
                find_call()
            }
        }).await;
        assert!(!root.path().join("out.txt").exists(), "neither the catch-all nor the rest of the program ran");
        let conn = agent_service::open_db(root.path()).unwrap();
        let answer: String = conn.query_row("SELECT json_extract(data,'$.message.content') FROM agent_task_events WHERE kind='assistant/message' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(answer, "Stopped as you asked.");
        assert!(program_lines(&conn).iter().any(|line| line.starts_with(PAUSED_AT)));
        assert_eq!(paused_nodes(&conn), 0, "the paused frames were discarded");
        let discarded: i64 = conn.query_row("SELECT count(*) FROM agent_task_nodes WHERE task_id='task' AND status='interrupted'", [], |r| r.get(0)).unwrap();
        assert!(discarded >= 3, "the paused program, its function and its leaf: {discarded}");
        assert_eq!(conn.query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r| r.get::<_, String>(0)).unwrap(), "completed");
    }

    #[tokio::test]
    async fn paused_concurrent_calls_resume_where_they_stopped() {
        let root = TestRoot::new();
        let path = root.path().to_owned();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" if body["messages"].to_string().contains("程序被你的新消息暂停") =>
                reply("```js\nimport * as previous from \"host:previous/promises\";\nawait previous.resume();\n```".into()),
            "main" => reply("```js\nimport * as io from \"host:io\";\nconst kept = await Promise.all([judge(\"a.txt\"), judge(\"b.txt\")]);\nio.write({path: \"out.txt\", content: JSON.stringify(kept)});\n\n/** @virtual */\nasync function judge(path) {\n  // Decide whether to keep the file at path. Returns {keep:boolean}.\n}\n```".into()),
            "implement" => reply("```js\nasync function judge(path) {\n  return decide(path);\n}\n/** @virtual */\nfunction decide(path) {\n  // Decide whether to keep the file. Returns {keep:boolean}.\n}\n```".into()),
            _ if has_tool_result(body) => reply(json!({"keep":true}).to_string()),
            _ => {
                crate::interjections::submit(&path, "task", "Carry on").unwrap();
                find_call()
            }
        }).await;
        let conn = agent_service::open_db(root.path()).unwrap();
        let lines = program_lines(&conn);
        assert!(lines.iter().any(|line| line.starts_with("程序被你的新消息暂停，停在 第 2 行 judge(\"a.txt\") → 第 2 行 decide(\"a.txt\")（AI 处理中）、第 2 行 judge(\"b.txt\") → 第 2 行 decide(\"b.txt\")（AI 处理中），这一步还没完成。")), "{lines:?}");
        let concurrent: i64 = conn.query_row("SELECT count(*) FROM agent_task_events WHERE data LIKE '%parallel_call_id%'", [], |r| r.get(0)).unwrap();
        assert!(concurrent > 0, "the calls ran concurrently");
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), r#"[{"keep":true},{"keep":true}]"#, "{lines:?}");
        assert_eq!(paused_nodes(&conn), 0);
    }

    #[tokio::test]
    async fn a_resumed_program_pauses_again_and_resumes_inside_the_next_one() {
        let root = TestRoot::new();
        let path = root.path().to_owned();
        let leaf = Arc::new(AtomicUsize::new(0));
        let leaf_rounds = leaf.clone();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" if body["messages"].to_string().matches("程序被你的新消息暂停").count() >= 2 =>
                reply("```js\nimport * as previous from \"host:previous/promises\";\nawait previous.resume();\n```".into()),
            "main" if body["messages"].to_string().contains("程序被你的新消息暂停") => reply(RESUMING_MAIN.into()),
            "main" => reply(TWO_LEVEL_MAIN.into()),
            "implement" => reply(CATCHING_IMPLEMENT.into()),
            _ => match leaf_rounds.fetch_add(1, Ordering::SeqCst) {
                0 => { crate::interjections::submit(&path, "task", "Also write extra").unwrap(); find_call() }
                1 => { crate::interjections::submit(&path, "task", "Keep going").unwrap(); find_call() }
                _ => reply(json!({"summary":"kept both"}).to_string()),
            },
        }).await;
        assert_eq!(leaf.load(Ordering::SeqCst), 3, "the leaf went on after each pause");
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "kept both");
        assert!(root.path().join("extra.txt").exists());
        let conn = agent_service::open_db(root.path()).unwrap();
        let lines = program_lines(&conn);
        assert_eq!(lines.iter().filter(|line| line.starts_with("程序被你的新消息暂停")).count(), 2, "{lines:?}");
        assert!(lines.iter().any(|line| line.starts_with("程序被你的新消息暂停，停在 第 3 行 previous.resume() → 第 2 行 summarize([\"a.txt\",\"b.txt\"]) → 第 3 行 judge([\"a.txt\",\"b.txt\"])（AI 处理中），这一步还没完成。")), "the second pause is in the new program: {lines:?}");
        assert_eq!(paused_nodes(&conn), 0);
    }

    #[tokio::test]
    async fn a_question_asked_after_resuming_reaches_the_user() {
        let root = TestRoot::new();
        let path = root.path().to_owned();
        let leaf = Arc::new(AtomicUsize::new(0));
        let leaf_rounds = leaf.clone();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" if body["messages"].to_string().contains("程序被你的新消息暂停") =>
                reply("```js\nimport * as previous from \"host:previous/promises\";\nawait previous.resume();\n```".into()),
            "main" => reply(TWO_LEVEL_MAIN.into()),
            "implement" => reply("```js\nasync function summarize(paths) {\n  return judge(paths);\n}\n/** @virtual */\nfunction judge(paths) {\n  // Decide which files to keep. Returns {summary:string}.\n}\n```".into()),
            _ => match leaf_rounds.fetch_add(1, Ordering::SeqCst) {
                0 => { crate::interjections::submit(&path, "task", "Carry on").unwrap(); find_call() }
                _ => json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
                    "tool_calls":[{"id":"ask","type":"function","function":{"name":ASK,"arguments":json!({"question":"Keep both files?"}).to_string()}}]}}]}),
            },
        }).await;
        let conn = agent_service::open_db(root.path()).unwrap();
        let lines = program_lines(&conn);
        assert_eq!(lines.last().unwrap(), "程序停在 第 2 行 previous.resume() → 第 2 行 summarize([\"a.txt\",\"b.txt\"]) → 第 2 行 judge([\"a.txt\",\"b.txt\"])，等你回答：Keep both files?\n之后的步骤没有执行。", "{lines:?}");
        assert!(!root.path().join("out.txt").exists());
        assert_eq!(paused_nodes(&conn), 0);
    }

    #[tokio::test]
    async fn calls_cut_off_by_a_crash_are_not_run_again() {
        let root = TestRoot::new();
        let leaf = Arc::new(AtomicUsize::new(0));
        let leaf_requests = leaf.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let leaf_requests = leaf_requests.clone();
            async move {
                Json(match role_of(&body).as_str() {
                    "main" if body["messages"].to_string().contains("PROGRAM_RUNTIME_ERROR") => reply("Noted.".into()),
                    "main" => reply("```js\nconst kept = await Promise.all([judge(\"a.txt\"), judge(\"b.txt\")]);\n\n/** @virtual */\nasync function judge(path) {\n  // Decide whether to keep the file at path. Returns {keep:boolean}.\n}\n```".into()),
                    "implement" => reply("```js\nasync function judge(path) {\n  return decide(path);\n}\n/** @virtual */\nfunction decide(path) {\n  // Decide whether to keep the file. Returns {keep:boolean}.\n}\n```".into()),
                    _ => {
                        // The process dies while the leaves wait for the model.
                        leaf_requests.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        reply(json!({"keep":true}).to_string())
                    }
                })
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        // Cut the first turn off once both leaves wait for the model.
        let waiting = leaf.clone();
        tokio::select! {
            _ = run_task(state.clone(), "task".into(), "fake".into(), "Judge a and b".into(), 20, CancellationToken::new(), 1, false) => panic!("the first turn was not cut off"),
            _ = async { while waiting.load(Ordering::SeqCst) < 2 { tokio::time::sleep(Duration::from_millis(20)).await; } } => {}
        }
        assert_eq!(leaf.load(Ordering::SeqCst), 2);
        run_task(state, "task".into(), "fake".into(), "Go on".into(), 20, CancellationToken::new(), 2, false).await.unwrap();
        server.abort();
        assert_eq!(leaf.load(Ordering::SeqCst), 2, "a call cut off mid-step is not run again");
        let error: String = conn.query_row("SELECT data FROM agent_task_events WHERE kind='program/runtime_error' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert!(error.contains("interrupted"), "the cut-off calls failed instead: {error}");
    }

    #[tokio::test]
    async fn a_question_from_a_concurrent_call_says_where_it_stopped() {
        let root = TestRoot::new();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" => reply("```js\nimport * as io from \"host:io\";\nconst kept = await Promise.all([judge(\"a.txt\"), judge(\"b.txt\")]);\nio.write({path: \"out.txt\", content: JSON.stringify(kept)});\n\n/** @virtual */\nasync function judge(path) {\n  // Decide whether to keep the file at path. Returns {keep:boolean}.\n}\n```".into()),
            "implement" => reply("```js\nasync function judge(path) {\n  return decide(path);\n}\n/** @virtual */\nfunction decide(path) {\n  // Decide whether to keep the file. Returns {keep:boolean}.\n}\n```".into()),
            _ if body["messages"].to_string().contains("a.txt") => json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
                "tool_calls":[{"id":"ask","type":"function","function":{"name":ASK,"arguments":json!({"question":"Keep a.txt?"}).to_string()}}]}}]}),
            _ => reply(json!({"keep":true}).to_string()),
        }).await;
        let conn = agent_service::open_db(root.path()).unwrap();
        let lines = program_lines(&conn);
        assert_eq!(lines.last().unwrap(), "程序停在 第 2 行 judge(\"a.txt\") → 第 2 行 decide(\"a.txt\")，等你回答：Keep a.txt?\n之后的步骤没有执行。", "{lines:?}");
        assert!(!root.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn a_failed_main_program_heals_in_the_same_turn() {
        let root = TestRoot::new();
        run_scripted(root.path(), move |body| {
            let messages = body["messages"].to_string();
            if messages.contains("PROGRAM_RUNTIME_ERROR") {
                assert!(messages.contains("draft.txt"), "the main AI sees what was already written: {messages}");
                reply("```js\nimport * as io from \"host:io\";\nio.write({path: \"done.txt\", content: \"fixed\"});\n```".into())
            } else {
                reply("```js\nimport * as io from \"host:io\";\nio.write({path: \"draft.txt\", content: \"1\"});\nthrow new Error(\"boom\");\n```".into())
            }
        }).await;
        assert_eq!(std::fs::read_to_string(root.path().join("done.txt")).unwrap(), "fixed");
        let conn = agent_service::open_db(root.path()).unwrap();
        let answer: String = conn.query_row("SELECT json_extract(data,'$.message.content') FROM agent_task_events WHERE kind='assistant/message' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(answer, "程序已完成。\n写入：draft.txt、done.txt");
    }

    #[tokio::test]
    async fn a_virtual_sees_and_its_code_uses_what_its_contract_names_with_uses() {
        let root = TestRoot::new();
        let seen = Arc::new(std::sync::Mutex::new(None::<String>));
        let log = seen.clone();
        run_scripted(root.path(), move |body| match role_of(body).as_str() {
            "main" => reply("```js\nimport * as io from \"host:io\";\nconst brief = \"Only summarize a\";\nconst result = await summarize([\"a.txt\", \"b.txt\"]);\nio.write({path: \"out.txt\", content: result.summary});\n\n/**\n * @virtual\n * Summarize the files at paths as the brief asks.\n * @uses brief the user's request, verbatim\n * Returns {summary:string}.\n */\nasync function summarize(paths) {}\n```".into()),
            _ => {
                *log.lock().unwrap() = Some(body["messages"].to_string());
                // The implementation reads the captured variable like any closure.
                reply("```js\nasync function summarize(paths) {\n  return {summary: brief + \": \" + paths[0]};\n}\n```".into())
            }
        }).await;
        let seen = seen.lock().unwrap().clone().expect("the implementation was asked for");
        assert!(seen.contains("closure") && seen.contains("Only summarize a"), "the AI sees the captured value: {seen}");
        assert_eq!(std::fs::read_to_string(root.path().join("out.txt")).unwrap(), "Only summarize a: a.txt");
    }

    #[tokio::test]
    async fn console_output_reaches_the_ai_that_wrote_the_program() {
        let root = TestRoot::new();
        let healed = Arc::new(std::sync::Mutex::new(None::<String>));
        let log = healed.clone();
        run_scripted(root.path(), move |body| {
            let messages = body["messages"].to_string();
            if messages.contains("PROGRAM_RUNTIME_ERROR") {
                *log.lock().unwrap() = Some(messages);
                reply("```js\nconsole.log(\"checked\", 2, \"files\");\nconsole.warn(\"b.txt is empty\");\n```".into())
            } else {
                reply("```js\nconsole.log(\"found\", {files: 2});\nthrow new Error(\"boom\");\n```".into())
            }
        }).await;
        let healed = healed.lock().unwrap().clone().expect("the error came back to heal");
        assert!(healed.contains("found {") && healed.contains("files"), "the diagnostic carries what it printed: {healed}");
        let conn = agent_service::open_db(root.path()).unwrap();
        let answer: String = conn.query_row("SELECT json_extract(data,'$.message.content') FROM agent_task_events WHERE kind='assistant/message' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(answer, "程序已完成。\n输出：\nchecked 2 files\n[warn] b.txt is empty");
    }
}
