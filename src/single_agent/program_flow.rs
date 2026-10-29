//! Adapter from flow pseudocode to the existing tool/Agent boundary.
//! It synthesizes normal tool exchanges; it never calls filesystem/plugin APIs directly.
use super::*;
use crate::agent_program::{self, Action, Runtime, State, runtime::ActionKind};

pub(super) enum Round {
    Exchange(String, Value),
    Recover,
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
    Ok(
        json!({"name":name,"description":description,"parameters":function.params,"arguments":arguments}),
    )
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
        json!({"name":task["name"],"signature":format!("function {}({})",task["name"].as_str().context("function name missing")?,names.join(", ")),
        "description":task["description"],"bindings":bindings,
        "binding_rule":"bindings lists actual local parameter values. Use each parameter directly as in the signature; bindings is display metadata, never another property of a parameter. define_task_body keeps the same parameter count; arguments bind by position only, never by name or automatic object packing."}),
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
/// Compile against this exact frame on a disposable copy; never run next().
pub(super) fn check_scoped(s: &Session<'_>, w: &Window, args: &Value) -> Result<Value> {
    let mut candidate = w.clone();
    let receipt = install_scoped(s, &mut candidate, args, w.program_task.is_some())?;
    let program = &candidate
        .program
        .as_ref()
        .context("checked program missing")?
        .program;
    Ok(
        json!({"status":"valid","executed":false,"version":agent_program::VERSION,
        "source_hash":receipt["source_hash"],"function_count":program.functions.len(),
        "submit_tool":if w.program_task.is_some(){"define_task_body"}else{"execute_program"}}),
    )
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
        "recovery":"Rewrite the complete source using these diagnostics and the published tool contracts, then resubmit in this same task scope. Keep the goal, current function name, input and prior results; no new main for a current function. Do not repeat completed operations. Never report a rejected program as completed. check_program is optional; execute_program/define_task_body always check before effects."})
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
    let entry = args["entry"].as_str().unwrap_or("main");
    ensure!(
        entry == "main",
        "execute_program enters the task through its single main; helpers run through ordinary or async calls"
    );
    let count = program
        .functions
        .get(entry)
        .context("entry function not found")?
        .params
        .len();
    ensure!(count <= 1, "entry takes zero or one input parameter");
    let runtime = Runtime::task_program(
        program,
        entry,
        if count == 0 {
            vec![]
        } else {
            vec![args["input"].clone()]
        },
    )?;
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
async fn exchange(
    s: &Session<'_>,
    w: &mut Window,
    step: usize,
    id: &str,
    name: &str,
    args: Value,
    at: &Value,
) -> Result<(String, Value)> {
    let node = format!("program_{}", agent_service::uuid_like());
    task_frames::begin_round(s, w, &node).await?;
    let mut execution = s.clone();
    execution.step_node_id = w.current_step_node_id.clone();
    let message = json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
    let seq=append(&execution,"assistant/message",json!({"actor":"program","turn":s.turn,"step":step,"nodeId":node,"program_location":at,"message":message}),false).await?;
    w.messages.push(sourced(message.clone(), s.task, seq));
    w.previous_node = Some(node.clone());
    save(s, w).await?;
    Ok((node, message))
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
            if w.program_task.is_some() {
                let failed = w.program.take().context("failed program missing")?;
                let bindings = failed.named_bindings();
                let names = bindings.keys().cloned().collect::<Vec<_>>();
                w.scope_values.extend(bindings);
                w.failed_programs.push(failed);
                w.program_value = None;
                let diagnostic = json!({"code":"FUNCTION_BODY_FAILED","error":error,
                    "saved_variables":names,
                    "instruction":"The current function body stopped. Continue this same function using the actual error, prior tool receipts and saved variables (read with host.read_scope). Inspect current state before acting; do not automatically replay completed or uncertain operations. You may use tools directly or define_task_body for the remaining work. A replacement body starts at its first instruction, so implement only the remaining work and explicitly read saved variables if needed. Return the original function's JSON contract, or $task_error with a concrete cause if recovery is not possible."});
                let seq = append(
                    s,
                    "program/recovery",
                    json!({"diagnostic":diagnostic}),
                    false,
                )
                .await?;
                w.messages.push(sourced(json!({"role":"user","name":"function_recovery","content":diagnostic.to_string()}), s.task, seq));
                save(s, w).await?;
                Ok(Round::Recover)
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
                        json!({"goal":name,"briefing":description,"program_task":task}),
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
                        json!({"goal":name,"briefing":description,"program_task":task}),
                    )
                }
                ActionKind::Tool { name, args } => (name.clone(), args.clone()),
                ActionKind::ParallelReads { operations } => (
                    "parallel_read_batch".into(),
                    json!({"operations":operations.iter().map(|(name,args)|json!({"name":name,"args":args})).collect::<Vec<_>>(), "settled": w.program.as_ref().unwrap().is_pending_async_batch()}),
                ),
                ActionKind::Fork { .. } | ActionKind::Join { .. } => anyhow::bail!("saved fork/join calls are retired"),
            };
            #[cfg(test)]
            super::documented_examples::inject_before_read(s.state.workspace.root(), &name, &args)?;
            let (node, message) = exchange(s, w, step, &action.id, &name, args, &at).await?;
            Ok(Round::Exchange(node, message))
        }
    }
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
    async fn interrupted_parallel_functions_are_not_replayed_after_resume() {
        let root=TestRoot::new();
        let count=Arc::new(AtomicUsize::new(0)); let counter=count.clone();
        let source=r#"async function main(){try{return await Promise.all([work('a'),work('b')]);}catch(e){return e.message;}}
/** @virtual Complete independent work and return a value. */ async function work(item){}"#;
        let app=Router::new().route("/v1/chat/completions",post(move |Json(_):Json<Value>| {
            let n=counter.fetch_add(1,Ordering::SeqCst);
            async move {
                let message=if n==0 {json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Do work"}).to_string()}}]})}
                else if n==1 {json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]})}
                else {tokio::time::sleep(Duration::from_secs(5)).await;json!({"role":"assistant","content":"\"late\""})};
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state=agent_service::tests::test_state(root.path(),listener.local_addr().unwrap());
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        let conn=agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        let cancel=CancellationToken::new(); let token=cancel.clone(); let first_state=state.clone();
        let running=tokio::spawn(async move{run_task(first_state,"task".into(),"fake".into(),"Do work".into(),10,token,1,false).await});
        tokio::time::timeout(Duration::from_secs(30),async {while count.load(Ordering::SeqCst)<4 {tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
        assert!(crate::task_tree::active(&conn,"task").unwrap().is_some());
        cancel.cancel();let _=tokio::time::timeout(Duration::from_secs(4),running).await.unwrap().unwrap();
        let started=count.load(Ordering::SeqCst);
        run_task(state,"task".into(),"fake".into(),"Resume".into(),10,CancellationToken::new(),2,false).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst),started,"uncertain model requests must not be repeated");
        let raw:String=conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'",[],|r|r.get(0)).unwrap();
        let message = serde_json::from_str::<Value>(&raw).unwrap()["program_value"].as_str().unwrap().to_owned();
        assert!(message.contains("interrupted") || message.contains("cancelled"), "{message}");
        server.abort();
    }
    #[tokio::test]
    async fn async_virtual_refinement_can_call_another_async_virtual() {
        let root=TestRoot::new();let count=Arc::new(AtomicUsize::new(0));let counter=count.clone();
        let source=r#"async function main(){return await outer('x');}
/** @virtual Execute the outer function and return its result. */ async function outer(item){}"#;
        let body_source=r#"async function outer(item){let value=await inner(item);return value;}
/** @virtual Perform inner work and return a string. */ async function inner(data){}"#;
        let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>|{
            let n=counter.fetch_add(1,Ordering::SeqCst);
            async move {
                let message=if n==0 {json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Nested async"}).to_string()}}]})}
                else if n==1 {json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]})}
                else {
                    let input=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let input:Value=serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    if input["name"]=="outer" {json!({"role":"assistant","tool_calls":[{"id":"body","type":"function","function":{"name":"define_task_body","arguments":json!({"source":body_source}).to_string()}}]})}
                    else {assert_eq!(input["bindings"]["data"],"x");json!({"role":"assistant","content":"\"ok\""})}
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let state=agent_service::tests::test_state(root.path(),listener.local_addr().unwrap());
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});let conn=agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(state,"task".into(),"fake".into(),"Nested async".into(),15,CancellationToken::new(),1,false).await.unwrap();server.abort();
        let raw:String=conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'",[],|r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["program_value"],"ok");
        assert_eq!(count.load(Ordering::SeqCst),4);
    }
    #[tokio::test]
    async fn async_virtual_functions_run_in_independent_concurrent_windows() {
        let root = TestRoot::new();
        let requests = Arc::new(AtomicUsize::new(0)); let counter = requests.clone();
        let active = Arc::new(AtomicUsize::new(0)); let current = active.clone();
        let peak = Arc::new(AtomicUsize::new(0)); let highest = peak.clone();
        let source = r#"async function main(){let shared='first';let a=inspect('a');shared='second';let b=inspect('b');return await Promise.all([a,b]);}
/** @virtual Inspect a value and return {item,shared}. */ async function inspect(item){}"#;
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let current = current.clone(); let highest = highest.clone();
            async move {
                let message = if n == 0 {
                    json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Inspect two items"}).to_string()}}]})
                } else if n == 1 {
                    json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]})
                } else {
                    let input = body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let input: Value = serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    let item = input["bindings"]["item"].as_str().unwrap().to_owned();
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1; highest.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(if item == "a" { 140 } else { 30 })).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                    json!({"role":"assistant","content":json!({"item":item}).to_string()})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Inspect items".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(peak.load(Ordering::SeqCst),2,"independent AI requests must overlap");
        let raw: String = conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'", [], |r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["program_value"],json!([{"item":"a"},{"item":"b"}]));
    }
    #[tokio::test]
    async fn async_virtual_failure_rejects_only_its_own_promise() {
        let root = TestRoot::new();
        let requests = Arc::new(AtomicUsize::new(0)); let counter = requests.clone();
        let active = Arc::new(AtomicUsize::new(0)); let current = active.clone();
        let peak = Arc::new(AtomicUsize::new(0)); let highest = peak.clone();
        let source = r#"async function main(){let shared='first';let a=inspect('a');shared='second';let b=inspect('b');try{await b;}catch(e){return [await a,e.message];}}
/** @virtual Inspect a value and return {item,shared}. */ async function inspect(item){}"#;
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let current = current.clone(); let highest = highest.clone();
            async move {
                let message = if n == 0 {
                    json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Inspect two items"}).to_string()}}]})
                } else if n == 1 {
                    json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]})
                } else {
                    let input = body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let input: Value = serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    let item = input["bindings"]["item"].as_str().unwrap().to_owned();
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1; highest.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(if item == "a" { 140 } else { 30 })).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                    json!({"role":"assistant","content":if item == "b" {json!({"$task_error":{"message":"review failed"}}).to_string()} else {json!({"item":item}).to_string()}})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Inspect items".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(peak.load(Ordering::SeqCst),2,"independent AI requests must overlap");
        let raw: String = conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'", [], |r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["program_value"],json!([{"item":"a"},"review failed"]));
    }
    #[tokio::test]
    async fn five_async_virtual_functions_share_model_concurrency_limit() {
        let root = TestRoot::new();
        let requests = Arc::new(AtomicUsize::new(0)); let counter = requests.clone();
        let active = Arc::new(AtomicUsize::new(0)); let current = active.clone();
        let peak = Arc::new(AtomicUsize::new(0)); let highest = peak.clone();
        let source = r#"async function main(){return await Promise.all(['a','b','c','d','e'].map(x=>inspect(x)));}
/** @virtual Inspect a value and return {item,shared}. */ async function inspect(item){}"#;
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let current = current.clone(); let highest = highest.clone();
            async move {
                let message = if n == 0 {
                    json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Inspect two items"}).to_string()}}]})
                } else if n == 1 {
                    json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]})
                } else {
                    let input = body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let input: Value = serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    let item = input["bindings"]["item"].as_str().unwrap().to_owned();
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1; highest.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                    json!({"role":"assistant","content":json!({"item":item}).to_string()})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Inspect items".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(peak.load(Ordering::SeqCst),3,"five functions must use the default three model slots");
        assert_eq!(requests.load(Ordering::SeqCst),7);
        let raw: String = conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'", [], |r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["program_value"],json!([{"item":"a"},{"item":"b"},{"item":"c"},{"item":"d"},{"item":"e"}]));
    }
    #[tokio::test]
    async fn parallel_virtual_writes_to_same_file_preserve_the_conflicting_candidate() {
        let root=TestRoot::new(); let count=Arc::new(AtomicUsize::new(0));let counter=count.clone();
        let source=r#"async function main(){try{return await Promise.all([work('a'),work('b')]);}catch(e){return e.message;}}
/** @virtual Write the requested value and return a string. */ async function work(item){}"#;
        let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>| {
            let n=counter.fetch_add(1,Ordering::SeqCst);
            async move {
                let message=if n==0 {json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Write a file"}).to_string()}}]})}
                else if n==1 {json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]})}
                else {
                    let messages=body["messages"].as_array().unwrap();
                    let input=messages.iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let input:Value=serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    let item=input["bindings"]["item"].as_str().unwrap(); let id=format!("write-{item}");
                    if messages.iter().any(|m|m["role"]=="tool" && m["tool_call_id"]==id) {
                        json!({"role":"assistant","content":json!(item).to_string()})
                    } else {
                        json!({"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":"write_file","arguments":json!({"path":"same.txt","content":item}).to_string()}}]})
                    }
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state=agent_service::tests::test_state(root.path(),listener.local_addr().unwrap());
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        let conn=agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(state,"task".into(),"fake".into(),"Write a file".into(),15,CancellationToken::new(),1,false).await.unwrap();server.abort();
        let actual=std::fs::read_to_string(root.path().join("same.txt")).unwrap();
        let conflicts=std::fs::read_dir(root.path().join(".codex-workspace-mcp/conflicts")).unwrap().collect::<Vec<_>>();
        assert_eq!(conflicts.len(),1);
        let candidate=std::fs::read_to_string(conflicts[0].as_ref().unwrap().path()).unwrap();
        assert!(matches!((actual.as_str(),candidate.as_str()),("a","b")|("b","a")));
    }
    #[tokio::test]
    async fn published_async_twenty_page_example_runs_unchanged() {
        let root=TestRoot::new();std::fs::create_dir_all(root.path().join("docs")).unwrap();
        for path in ["README.md","docs/agent-program.md","docs/generic-agent-design.md","docs/skill_packages.md","docs/task_delegation.md"] {std::fs::write(root.path().join(path),"Fixture source").unwrap();}
        let count=Arc::new(AtomicUsize::new(0));let counter=count.clone();
        let source=include_str!("../../skills/task-program/examples/parallel-twenty-html.js");
        let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>|{
            let n=counter.fetch_add(1,Ordering::SeqCst);
            async move {
                let message=if n==0 {json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Make twenty pages"}).to_string()}}]})}
                else if n==1 {json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]})}
                else {
                    let input=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();let input:Value=serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    let result=match input["name"].as_str().unwrap() {
                        "designStyles" => json!({"content":"body { color: black; }"}),
                        "generatePageBody" => json!({"path":input["bindings"]["page"]["path"],"sections":[{"heading":"概述","blocks":[{"kind":"paragraph","text":"材料描述的内容"}]},{"heading":"说明","blocks":[{"kind":"paragraph","text":"实际任务范围"}]}]}),
                        "reviewDocumentation" => {assert_eq!(input["bindings"]["actualPages"].as_array().unwrap().len(),20);json!({"passed":true,"evidence":["checked"],"issues":[]})},
                        name => panic!("unexpected virtual {name}"),
                    };
                    json!({"role":"assistant","content":result.to_string()})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let state=agent_service::tests::test_state(root.path(),listener.local_addr().unwrap());
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});let conn=agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(state,"task".into(),"fake".into(),"Make twenty pages".into(),50,CancellationToken::new(),1,false).await.unwrap();server.abort();
        assert_eq!(count.load(Ordering::SeqCst),24);
        for i in 1..=20 {let page=root.path().join(format!("page{i:02}.html"));assert!(std::fs::read_to_string(page).unwrap().contains("shared.css"));}
        assert!(root.path().join("shared.css").is_file());
    }
    #[tokio::test]
    async fn five_page_async_design_write_read_and_review_uses_concurrent_windows() {
        let root = TestRoot::new();
        let count = Arc::new(AtomicUsize::new(0)); let counter = count.clone();
        let active = Arc::new(AtomicUsize::new(0)); let current = active.clone();
        let peak = Arc::new(AtomicUsize::new(0)); let highest = peak.clone();
        let source = r#"import * as io from 'host:io';
const pages=input.pages;
const designs=await Promise.all(pages.map(page=>design(page)));
for(let i=0;i<pages.length;i++){io.write({path:pages[i],content:designs[i].content});}
const reviews=await Promise.all(pages.map(page=>review(page,io.read({path:page}).content)));
for(const review of reviews){assert(review.passed,'review failed');}
/** @virtual Design this page and return its HTML as {content}. */ async function design(page){}
/** @virtual Inspect the actual page and return {passed}. */ async function review(page,content){}"#;
        let pages = (1..=5).map(|i|format!("page{i}.html")).collect::<Vec<_>>();
        let inputs = pages.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n=counter.fetch_add(1,Ordering::SeqCst); let current=current.clone(); let highest=highest.clone(); let inputs=inputs.clone();
            async move {
                let message=if n==0 {json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Make five pages"}).to_string()}}]})}
                else if n==1 {json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source,"input":{"pages":inputs}}).to_string()}}]})}
                else {
                    let input=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let input:Value=serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    let name=input["name"].as_str().unwrap(); let page=input["bindings"]["page"].as_str().unwrap();
                    let now=current.fetch_add(1,Ordering::SeqCst)+1;highest.fetch_max(now,Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(60)).await;current.fetch_sub(1,Ordering::SeqCst);
                    let output=if name=="design" {json!({"content":format!("<html><body>{page}</body></html>")})} else {
                        json!({"passed":input["bindings"]["content"].as_str().unwrap().contains(page)})
                    };
                    json!({"role":"assistant","content":output.to_string()})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state=agent_service::tests::test_state(root.path(),listener.local_addr().unwrap());
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
        let conn=agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(state,"task".into(),"fake".into(),"Make five pages".into(),20,CancellationToken::new(),1,false).await.unwrap();server.abort();
        assert_eq!(count.load(Ordering::SeqCst),12); assert!(peak.load(Ordering::SeqCst)>=2);
        assert_eq!(conn.query_row("SELECT status FROM agent_tasks WHERE id='task'",[],|r|r.get::<_,String>(0)).unwrap(),"completed");
        for page in pages {assert!(std::fs::read_to_string(root.path().join(&page)).unwrap().contains(&page));}
    }
    #[tokio::test]
    async fn published_async_five_page_example_runs_unchanged() {
        let root = TestRoot::new();
        let count = Arc::new(AtomicUsize::new(0)); let counter = count.clone();
        let active = Arc::new(AtomicUsize::new(0)); let current = active.clone();
        let peak = Arc::new(AtomicUsize::new(0)); let highest = peak.clone();
        let source=include_str!("../../skills/task-program/examples/parallel-five-pages.js");
        let inputs:Value=serde_json::from_str(include_str!("../../skills/task-program/examples/parallel-five-pages.input.json")).unwrap();
        std::fs::create_dir_all(root.path().join("docs")).unwrap();
        std::fs::write(root.path().join("docs/generic-agent-design.md"),"Source material").unwrap();
        let pages=inputs["pages"].as_array().unwrap().iter().map(|page|page["path"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n=counter.fetch_add(1,Ordering::SeqCst); let current=current.clone(); let highest=highest.clone(); let inputs=inputs.clone();
            async move {
                let message=if n==0 {json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Make five pages"}).to_string()}}]})}
                else if n==1 {json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source,"input":inputs}).to_string()}}]})}
                else {
                    let input=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let input:Value=serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    let name=input["name"].as_str().unwrap(); let page=input["bindings"]["page"]["path"].as_str().unwrap();
                    let now=current.fetch_add(1,Ordering::SeqCst)+1;highest.fetch_max(now,Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(60)).await;current.fetch_sub(1,Ordering::SeqCst);
                    let output=if name=="designPage" {json!({"content":format!("<html><body>{page}</body></html>")})} else {
                        json!({"passed":input["bindings"]["content"].as_str().unwrap().contains(page),"path":page,"issues":[],"evidence":["checked"]})
                    };
                    json!({"role":"assistant","content":output.to_string()})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state=agent_service::tests::test_state(root.path(),listener.local_addr().unwrap());
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
        let conn=agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(state,"task".into(),"fake".into(),"Make five pages".into(),20,CancellationToken::new(),1,false).await.unwrap();server.abort();
        assert_eq!(count.load(Ordering::SeqCst),12); assert!(peak.load(Ordering::SeqCst)>=2);
        assert_eq!(conn.query_row("SELECT status FROM agent_tasks WHERE id='task'",[],|r|r.get::<_,String>(0)).unwrap(),"completed");
        for page in pages {assert!(std::fs::read_to_string(root.path().join(&page)).unwrap().contains(&page));}
    }
    #[tokio::test]
    async fn async_parent_resumes_on_first_completion_while_sibling_runs() {
        let root = TestRoot::new();
        let requests = Arc::new(AtomicUsize::new(0)); let counter = requests.clone();
        let active = Arc::new(AtomicUsize::new(0)); let current = active.clone();
        let peak = Arc::new(AtomicUsize::new(0)); let highest = peak.clone();
        let source = r#"async function main(){let shared='first';let a=inspect('a');shared='second';let b=inspect('b');let second=await b;let c=inspect(second.item);return await Promise.all([a,c]);}
/** @virtual Inspect a value and return {item,shared}. */ async function inspect(item){}"#;
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let current = current.clone(); let highest = highest.clone();
            async move {
                let message = if n == 0 {
                    json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Inspect two items"}).to_string()}}]})
                } else if n == 1 {
                    json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]})
                } else {
                    let input = body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let input: Value = serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    let item = input["bindings"]["item"].as_str().unwrap().to_owned();
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1; highest.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(if item == "a" { 350 } else { 30 })).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                    json!({"role":"assistant","content":json!({"item":item}).to_string()})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Inspect items".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(requests.load(Ordering::SeqCst),5,"a third AI call must start after b returns");
        assert_eq!(peak.load(Ordering::SeqCst),2,"the third call must overlap the slow first call");
        let raw: String = conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'", [], |r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["program_value"],json!([{"item":"a"},{"item":"b"}]));
    }
    #[tokio::test]
    async fn parallel_reads_overlap_and_feed_one_sequential_ai_task() {
        let root = TestRoot::new();
        for path in ["first", "second", "third"] {
            std::fs::write(root.path().join(path), path).unwrap();
        }
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let catalog = Arc::new(crate::builtin_plugins::ToolCatalog::default());
        let active_reads = active.clone();
        let peak_reads = peak.clone();
        catalog.register(&["read_file", "search_text"], Arc::new(move |_, _, args| {
            let active = active_reads.clone();
            let peak = peak_reads.clone();
            Box::pin(async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let path = args["path"].as_str().unwrap().to_owned();
                tokio::time::sleep(Duration::from_millis(if path == "first" { 120 } else { 30 })).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(json!({"path":path,"content":path,"code_hash":crate::symbol_description::content_hash(path.as_bytes())}))
            })
        })).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let source = r#"
import * as io from "host:io/promises";
async function main() {
  const pages = await Promise.all([
    io.read({path:"first"}),
    io.search({path:"second", query:"second"}),
    io.read({path:"third"})
  ]);
  return inspect(pages);
}
/** @virtual */ function inspect(pages) { /* Inspect the supplied pages in one context. */ }
"#;
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let message = if n == 0 {
                json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Inspect pages"}).to_string()}}]})
            } else if n == 1 {
                json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]})
            } else if n == 2 {
                let input = body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();
                let input: Value = serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                assert_eq!(input["bindings"]["pages"].as_array().unwrap().iter().map(|v|v["path"].as_str().unwrap()).collect::<Vec<_>>(), vec!["first","second","third"]);
                json!({"role":"assistant","content":"{\"reviewed\":3}"})
            } else { panic!("unexpected model request {n}") };
            async move { Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        state.tool_catalog = Some(catalog);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Inspect pages".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(peak.load(Ordering::SeqCst), 3, "reads must overlap");
        assert_eq!(requests.load(Ordering::SeqCst), 3, "one AI inspection, no agents");
        assert_eq!(conn.query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r|r.get::<_,String>(0)).unwrap(), "completed");
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='spawn_agent'", [], |r|r.get::<_,i64>(0)).unwrap(), 0);
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_file_baselines WHERE task_id='task'", [], |r|r.get::<_,i64>(0)).unwrap(), 2, "parallel file reads must retain each file baseline");
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='parallel_read/item'", [], |r|r.get::<_,i64>(0)).unwrap(), 3);
    }
    #[tokio::test]
    async fn parallel_read_failure_is_catchable_without_replaying_reads() {
        let root = TestRoot::new();
        let finished = Arc::new(AtomicUsize::new(0));
        let done = finished.clone();
        let catalog = Arc::new(crate::builtin_plugins::ToolCatalog::default());
        catalog.register(&["read_file"], Arc::new(move |_, _, args| {
            let done = done.clone();
            Box::pin(async move {
                let path = args["path"].as_str().unwrap();
                if path == "good" { tokio::time::sleep(Duration::from_millis(50)).await; }
                done.fetch_add(1, Ordering::SeqCst);
                if path == "bad" { anyhow::bail!("read failed") }
                Ok(json!({"content":path}))
            })
        })).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let source = r#"import * as io from 'host:io/promises'; async function main(){try{return await Promise.all([io.read({path:"good"}),io.read({path:"bad"})]);}catch(error){return error.message;}}"#;
        let app = Router::new().route("/v1/chat/completions", post(move |Json(_): Json<Value>| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (name,args) = match n { 0 => ("create_task",json!({"goal":"Read files"})), 1 => ("execute_program",json!({"source":source})), _ => panic!("unexpected request") };
            let message = json!({"role":"assistant","tool_calls":[{"id":format!("call-{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
            async move { Json(json!({"choices":[{"finish_reason":"tool_calls","message":message}]})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        state.tool_catalog = Some(catalog);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Read files".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(finished.load(Ordering::SeqCst), 2, "all started reads must settle");
        assert_eq!(requests.load(Ordering::SeqCst), 2, "the host must not replay or replan the batch");
        let raw: String = conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'", [], |r|r.get(0)).unwrap();
        assert!(serde_json::from_str::<Value>(&raw).unwrap()["program_value"].as_str().unwrap().contains("read failed"));
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='parallel_read_batch'", [], |r|r.get::<_,i64>(0)).unwrap(), 1);
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='parallel_read/item' AND json_extract(data,'$.is_error')=1", [], |r|r.get::<_,i64>(0)).unwrap(), 1);
    }
    #[tokio::test]
    async fn async_read_batch_records_independent_outcomes_without_replay() {
        let root = TestRoot::new();
        let finished = Arc::new(AtomicUsize::new(0));
        let done = finished.clone();
        let catalog = Arc::new(crate::builtin_plugins::ToolCatalog::default());
        catalog.register(&["read_file"], Arc::new(move |_, _, args| {
            let done = done.clone();
            Box::pin(async move {
                let path = args["path"].as_str().unwrap();
                if path == "good" { tokio::time::sleep(Duration::from_millis(50)).await; }
                done.fetch_add(1, Ordering::SeqCst);
                if path == "bad" { anyhow::bail!("read failed") }
                Ok(json!({"content":path}))
            })
        })).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let source = r#"import {read} from 'host:io/promises'; async function main(){let result=await Promise.allSettled([read({path:'good'}),read({path:'bad'})]);return [result[0].status,result[0].value.content,result[1].status,result[1].reason.message];}"#;
        let app = Router::new().route("/v1/chat/completions", post(move |Json(_): Json<Value>| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (name,args) = match n { 0 => ("create_task",json!({"goal":"Read files"})), 1 => ("execute_program",json!({"source":source})), _ => panic!("unexpected request") };
            let message = json!({"role":"assistant","tool_calls":[{"id":format!("call-{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
            async move { Json(json!({"choices":[{"finish_reason":"tool_calls","message":message}]})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        state.tool_catalog = Some(catalog);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Read files".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(finished.load(Ordering::SeqCst), 2, "all started reads must settle");
        assert_eq!(requests.load(Ordering::SeqCst), 2, "the host must not replay or replan the batch");
        let raw: String = conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'", [], |r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["program_value"], json!(["fulfilled","good","rejected","read failed"]));
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='parallel_read_batch'", [], |r|r.get::<_,i64>(0)).unwrap(), 1);
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='parallel_read/item' AND json_extract(data,'$.is_error')=1", [], |r|r.get::<_,i64>(0)).unwrap(), 1);
    }
    #[tokio::test]
    async fn ordinary_file_functions_write_and_read_five_real_files_without_ai_work() {
        let root = TestRoot::new();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let files = (1..=5).map(|n| json!({"path":format!("page-{n}.html"),"content":format!("<p>page {n}</p>")})).collect::<Vec<_>>();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(_): Json<Value>| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (name, args) = match n {
                0 => ("create_task", json!({"goal":"Write and read the supplied files"})),
                1 => ("execute_program", json!({"source":include_str!("../../skills/task-program/examples/file-functions.js"),"input":{"files":files}})),
                _ => panic!("file helpers must not request AI: {n}"),
            };
            let message = json!({"role":"assistant","tool_calls":[{"id":format!("call-{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
            async move { Json(json!({"choices":[{"finish_reason":"tool_calls","message":message}]})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Write and read files".into(), 8, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(conn.query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r|r.get::<_,String>(0)).unwrap(), "completed");
        for n in 1..=5 {
            assert_eq!(std::fs::read_to_string(root.path().join(format!("page-{n}.html"))).unwrap(), format!("<p>page {n}</p>"));
        }
        let raw: String = conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'", [], |r|r.get(0)).unwrap();
        let window: Value = serde_json::from_str(&raw).unwrap();
        let result = window["program_value"].as_array().unwrap();
        assert_eq!(result.len(), 5);
        for (index, item) in result.iter().enumerate() {
            assert_eq!(item["actual"]["content"], format!("<p>page {}</p>", index + 1));
        }
        for (name, expected) in [("write_file",5), ("read_file",5), ("spawn_agent",0)] {
            assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')=?1", [name], |r|r.get::<_,i64>(0)).unwrap(), expected);
        }
    }
    #[tokio::test]
    async fn async_example_reads_five_real_files_in_one_ai_scope() {
        let root = TestRoot::new();
        for i in 1..=5 { std::fs::write(root.path().join(format!("{i}.html")), format!("page-{i}")).unwrap(); }
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let source = include_str!("../../skills/task-program/examples/async-page-review.js");
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n = requests.fetch_add(1, Ordering::SeqCst);
            let message = match n {
                0 => json!({"role":"assistant","tool_calls":[{"id":"create","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Review five pages"}).to_string()}}]}),
                1 => json!({"role":"assistant","tool_calls":[{"id":"program","type":"function","function":{"name":"execute_program","arguments":json!({"source":source,"input":{"paths":["1.html","2.html","3.html","4.html","5.html"],"requirements":"check"}}).to_string()}}]}),
                2 => {
                    let input = body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let input: Value = serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    assert_eq!(input["bindings"]["pages"].as_array().unwrap().iter().map(|v|v["initial"]["content"].as_str().unwrap()).collect::<Vec<_>>(), vec!["page-1","page-2","page-3","page-4","page-5"]);
                    json!({"role":"assistant","content":"{\"reviewed\":5}"})
                }
                _ => panic!("unexpected model request {n}"),
            };
            async move { Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Review five pages".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert_eq!(conn.query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r|r.get::<_,String>(0)).unwrap(), "completed");
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_file_baselines WHERE task_id='task'", [], |r|r.get::<_,i64>(0)).unwrap(), 5);
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='spawn_agent'", [], |r|r.get::<_,i64>(0)).unwrap(), 0);
    }
    #[tokio::test]
    async fn current_virtual_function_can_become_an_async_concrete_body() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("one.txt"), "one").unwrap();
        std::fs::write(root.path().join("two.txt"), "two").unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(_): Json<Value>| {
            let n = requests.fetch_add(1, Ordering::SeqCst);
            let (name, args) = match n {
                0 => ("create_task",json!({"goal":"Read the pages"})),
                1 => ("execute_program",json!({"source":"function main(){return review(['one.txt','two.txt']);} /** @virtual */ function review(paths){ /* Read both files. */ }"})),
                2 => ("define_task_body",json!({"source":"import * as io from 'host:io/promises'; async function review(paths){return await Promise.all(paths.map(path => io.read({path:path})));}"})),
                _ => panic!("async body should not need another model request"),
            };
            let message = json!({"role":"assistant","tool_calls":[{"id":format!("call-{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
            async move { Json(json!({"choices":[{"finish_reason":"tool_calls","message":message}]})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(state, "task".into(), "fake".into(), "Read the pages".into(), 10, CancellationToken::new(), 1, false).await.unwrap();
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert_eq!(conn.query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r|r.get::<_,String>(0)).unwrap(), "completed");
        let raw:String = conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='task' AND scope='main'", [], |r|r.get(0)).unwrap();
        let result = &serde_json::from_str::<Value>(&raw).unwrap()["program_value"];
        assert_eq!(result[0]["content"], "one");
        assert_eq!(result[1]["content"], "two");
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

    #[tokio::test]
    async fn an_empty_task_can_refine_its_body_with_coded_and_comment_subtasks() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("source.txt"), "SOURCE").unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>|{
            let n=seen.fetch_add(1,Ordering::SeqCst);
            async move {
                let messages=body["messages"].as_array().unwrap();
                let current=messages.iter().find(|m|m["name"]=="program_task_input").map(|m|serde_json::from_str::<Value>(m["content"].as_str().unwrap()).unwrap());
                let (name,args)=if let Some(task)=current {
                    match task["name"].as_str().unwrap() {
                        "phase" => {
                            assert!(body["tools"].as_array().unwrap().iter().any(|t|t["function"]["name"]=="define_task_body"));
                            if !messages.iter().any(|m|m["role"]=="assistant" && m["tool_calls"].as_array().is_some_and(|calls|calls.iter().any(|c|c["function"]["name"]=="read_program_guide"))) {
                                return Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":"guide","type":"function","function":{"name":"read_program_guide","arguments":"{}"}}]}}]}));
                            }
                            assert!(messages.iter().any(|m|m["role"]=="tool" && m.to_string().contains("Flow runtime capabilities")));
                            ("define_task_body",json!({"source":r#"
function phase(request) {
  // Refine this same task, not another main.
  const result=stage(request);
  io.write({path:request.output,content:result});
  return {path:request.output,status:result};
}
function stage(request) {
  // Ordinary helpers use Runtime frames; tools remain steps.
  const file=io.read({path:request.source});
  return analyze(request,file.content);
}
function analyze(request,content) { return evaluate(request,content); }
/** @virtual */
function evaluate(request,content) {
  // Evaluate the current content and return the JSON string READY.
}
"#}))
                        },
                        "evaluate"=>{
                            assert_eq!(task["bindings"]["content"],"SOURCE");
                            return Json(json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"\"READY\""}}]}));
                        },
                        other=>panic!("coded helper {other} should execute without a model scheduling round"),
                    }
                } else {
                    match n {
                        0=>("create_task",json!({"goal":"Nested task functions preserve the caller and its inputs"})),
                        1=>("read_task_board",json!({})),
                        2=>("execute_program",json!({"source":"function main(request){const result=phase(request);const file=io.read({path:result.path});return {result,actual:file.content};} function phase(request){/* @virtual */ // Read and process the source, save output, and return its path and status.\n}","input":{"source":"source.txt","output":"output.txt"}})),
                        _=>panic!("unexpected planner request {n}"),
                    }
                };
                Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        tokio::time::timeout(
            Duration::from_secs(30),
            run_task(
                state,
                "task".into(),
                "fake".into(),
                "Process the source and save its result".into(),
                12,
                CancellationToken::new(),
                1,
                false,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 6);
        let status: String = conn
            .query_row("SELECT status FROM agent_tasks", [], |r| r.get(0))
            .unwrap();
        let terminal = conn
            .prepare("SELECT data FROM agent_task_events WHERE kind='turn/end'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(status, "completed", "{terminal:?}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("output.txt")).unwrap(),
            "READY"
        );
        let nodes = crate::task_tree::nodes(&conn, "task").unwrap();
        let tasks = nodes
            .iter()
            .filter(|n| n.inputs["program_task"].is_object())
            .collect::<Vec<_>>();
        assert_eq!(
            tasks.len(),
            2,
            "only virtual functions create AI tasks; refinement reuses phase"
        );
        for name in ["phase", "evaluate"] {
            let node = tasks
                .iter()
                .find(|n| n.inputs["program_task"]["name"] == name)
                .unwrap();
            assert_eq!(node.status, "done");
            let remaining: i64 = conn
                .query_row(
                    "SELECT count(*) FROM agent_runtime_windows WHERE scope=?1",
                    [crate::task_tree::frame_scope(node)],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(remaining, 0);
        }
        let raw: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let w: Window = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            w.program_value,
            Some(json!({"result":{"path":"output.txt","status":"READY"},"actual":"READY"}))
        );
    }

    #[tokio::test]
    async fn coded_child_failure_returns_to_the_parent_catch_without_replanning() {
        let root = TestRoot::new();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let app=Router::new().route("/v1/chat/completions",post(move |Json(_):Json<Value>|{
            let n=seen.fetch_add(1,Ordering::SeqCst);
            async move {
                let (name,args)=match n {
                    0=>("create_task",json!({"goal":"Catch a child function error"})),
                    1=>("execute_program",json!({"source":"function main(){try{const value=child();io.write({path:'never.txt',content:value});return 'unexpected';}catch(error){return {caught:true,message:error.message};}} function child(){return io.read({path:'missing.txt'});}"})),
                    _=>panic!("function returns and catch must not ask the model to reschedule"),
                };
                Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Handle a missing input".into(),
            6,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(
            conn.query_row("SELECT status FROM agent_tasks", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "completed"
        );
        assert!(!root.path().join("never.txt").exists());
        let raw: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let w: Window = serde_json::from_str(&raw).unwrap();
        assert_eq!(w.program_value.as_ref().unwrap()["caught"], true);
        assert!(
            w.program_value.as_ref().unwrap()["message"]
                .as_str()
                .unwrap()
                .contains("missing.txt")
        );
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

    #[tokio::test]
    async fn common_flow_runs_with_no_model_requests_after_program_submission() {
        let root = TestRoot::new();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let source = r#"function main(){var rows=[];var sum=0;for(var i=0;i<4;i++){if(i===1)continue;const path=`part-${i}.txt`;io.write({path,content:`${i+1}`});const actual=io.read({path});rows.push(actual.content);sum+=i;}assert(rows.length===3);if(false&&io.read({path:'never'})){throw 'bad';}return {rows,sum,selection:sum>0?'passed':'failed'};}"#;
        let app=Router::new().route("/v1/chat/completions",post(move |Json(_):Json<Value>|{
            let n=requests.fetch_add(1,Ordering::SeqCst);
            let (name,args)=match n {0=>("create_task",json!({"goal":"Execute deterministic common flow"})),1=>("execute_program",json!({"source":source})),_=>panic!("ordinary computation must never request a model: {n}")};
            async move {Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}))}
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Execute common flow".into(),
            8,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let status: String = conn
            .query_row("SELECT status FROM agent_tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "completed");
        for (i, text) in [(0, "1"), (2, "3"), (3, "4")] {
            assert_eq!(
                std::fs::read_to_string(root.path().join(format!("part-{i}.txt"))).unwrap(),
                text
            );
        }
        let calls:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='read_file'",[],|r|r.get(0)).unwrap();
        assert_eq!(calls, 3, "the short-circuited read must not run");
        let raw: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let window: Window = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            window.program_value,
            Some(json!({"rows":["1","3","4"],"sum":5,"selection":"passed"}))
        );
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

    #[tokio::test]
    async fn io_library_and_c_conditions_work_after_real_writes_without_extra_models() {
        let root = TestRoot::new();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let source = r#"function main(){io.write({path:'one.txt',content:'one'});io.write({path:'two.txt',content:'two'});const actual=io.read({path:'two.txt'});const items=[actual];const result={};const obj={};if(items.length && !result.error && obj){return {content:actual.content,checked:true};}throw 'incorrect truth conversion';}"#;
        let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>|{
            let n=requests.fetch_add(1,Ordering::SeqCst);
            let messages=body["messages"].as_array().unwrap();
            if n==1 {
                // The unloaded file group is published in the same declarations
                // that the source compiler consumes.
                assert!(messages.iter().any(|m|m["name"]=="program_tool_headers"));
                let headers=messages.iter().find(|m|m["name"]=="program_tool_headers").unwrap()["content"].as_str().unwrap();
                assert!(headers.contains("function read("));assert!(headers.contains("function write("));
                assert!(!body["tools"].as_array().unwrap().iter().any(|t|matches!(t["function"]["name"].as_str(),Some("read_file"|"write_file"|"invoke_library"))));
            }
            if n==2 {
                assert!(messages.iter().any(|m|m["role"]=="tool" && m["content"].as_str().is_some_and(|c| c.contains("function read(args:") && c.contains("path:"))), "exact selected contract must reach the planner");
            }
            if n==3 { assert!(messages.iter().any(|m|m["role"]=="tool" && m.to_string().contains("Flow runtime capabilities"))); }
            let (name,args)=match n {0=>("create_task",json!({"goal":"Use file library and C truth values"})),1=>("read_tool_contracts",json!({"methods":["io.read","io.write"]})),2=>("read_program_guide",json!({})),3=>("execute_program",json!({"source":source})),_=>panic!("no condition should request a model: {n}")};
            async move {Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}))}
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Use the library".into(),
            8,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        let status: String = conn
            .query_row("SELECT status FROM agent_tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "completed");
        assert_eq!(count.load(Ordering::SeqCst), 4);
        assert_eq!(
            std::fs::read_to_string(root.path().join("one.txt")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("two.txt")).unwrap(),
            "two"
        );
        let names: Vec<String> = conn
            .prepare(
                "SELECT json_extract(data,'$.name') FROM agent_task_events WHERE kind='tool/call'",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "write_file").count(),
            2
        );
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "read_file").count(),
            1
        );
        assert!(!names.iter().any(|n| n.starts_with("io.")));
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
            step_node_id: None,
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

    #[tokio::test]
    async fn plugin_declared_library_uses_the_registered_owner_tool() {
        let root = TestRoot::new();
        std::fs::write(
            root.path().join("code-flow.json"),
            r#"{"version":1,"features":[]}"#,
        )
        .unwrap();
        let (_runtime, catalog) = crate::skill_packages::test_runtime();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let project_root = root.path().to_string_lossy().into_owned();
        let app=Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>|{
            let n=requests.fetch_add(1,Ordering::SeqCst);
            let message=body["messages"].as_array().unwrap().iter().find(|m|m["name"]=="program_tool_headers").unwrap();
            let headers=message["content"].as_str().unwrap();
            assert!(headers.contains("declare namespace flow"));
            assert!(headers.contains("function read_code_flow("));
            assert!(!headers.contains("declare namespace io"),"file tools are outside this plugin's ownership");
            let (name,args)=match n {
                0=>("create_task",json!({"goal":"Read this plugin through its library"})),
                1=>("execute_program",json!({"source":"function main(request){return flow.read_code_flow({workspace_root:request.root});}","input":{"root":project_root}})),
                _=>panic!("no extra model is needed for library dispatch"),
            };
            async move{Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}))}
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state =
            agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        state.tool_catalog = Some(catalog);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        conn.execute(
            "INSERT INTO plugin_sessions VALUES ('task','code-flow')",
            [],
        )
        .unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Read the plugin library".into(),
            6,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let diagnostics=conn.prepare("SELECT data FROM agent_task_events WHERE kind IN ('turn/end','program/error','tool/result')").unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(
            conn.query_row("SELECT status FROM agent_tasks", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "completed",
            "{}",
            diagnostics.join("\n")
        );
        let calls:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='read_code_flow'",[],|r|r.get(0)).unwrap();
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn recorded_comment_returns_are_typed_and_invalid_values_raise_to_caller() {
        for (reply, expected) in [
            (
                r#"{"passed":true,"files":["a.txt"]}"#,
                json!({"passed":true,"files":["a.txt"]}),
            ),
            (
                r#"{"$task_error":{"message":"actual inspection failed"}}"#,
                json!({"message":"actual inspection failed"}),
            ),
            (
                "I finished",
                json!({"message":"comment task must return a JSON value"}),
            ),
        ] {
            let root = TestRoot::new();
            let state =
                agent_service::tests::test_state(root.path(), "127.0.0.1:9".parse().unwrap());
            agent_service::open_db(root.path()).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
            let cancel = CancellationToken::new();
            let session = Session {
                agent_id: None,
                task_node_id: None,
                step_node_id: None,
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
            install(&mut w,&json!({"source":"function inspect(){/* @virtual */ // Inspect actual files and return the check result.\n} function main(){try{return inspect();}catch(error){return {message:error.message};}}"})).unwrap();
            let State::Action(action) = w.program.as_mut().unwrap().next() else {
                panic!()
            };
            assert!(matches!(
                action.kind,
                ActionKind::Task { .. } | ActionKind::Call { .. }
            ));
            append(&session,"tool/result",json!({"message":{"toolCallId":action.id,"isError":false},"meta":{"result":{"result":reply}}}),false).await.unwrap();
            save(&session, &w).await.unwrap();
            let raw: String = agent_service::open_db(root.path())
                .unwrap()
                .query_row(
                    "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let mut restored: Window = serde_json::from_str(&raw).unwrap();
            recover(&session, &mut restored).await.unwrap();
            let State::Returned(value) = restored.program.as_mut().unwrap().next() else {
                panic!()
            };
            if serde_json::from_str::<Value>(reply).is_err() {
                assert!(
                    value["message"]
                        .as_str()
                        .unwrap()
                        .starts_with(expected["message"].as_str().unwrap())
                );
            } else {
                assert_eq!(value, expected);
            }
        }
    }

    #[tokio::test]
    async fn explicit_inspection_reads_artifacts_and_host_asserts_without_implicit_ai() {
        let root = TestRoot::new();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let source = r#"
function main(request) {
  let 产物 = 制作(request);
  const check = 检查(产物, request);
  if (!check.passed) { 产物 = 修正(产物, request); }
  const actual = io.read({path:产物.path});
  assert(actual.content === request.expected, 'artifact still fails inspection');
  return {path:产物.path, content:actual.content};
}
function 制作(request) {
  io.write({path:request.path, content:'DRAFT'});
  return {path:request.path};
}
/** @virtual */
function 检查(产物, request) {
  // Read the actual artifact and compare its quality with request.
  // Return {passed:boolean,evidence:[actual tool observations]}.
}
function 修正(产物, request) {
  const original = io.read({path:产物.path});
  io.write({path:产物.path, content:request.expected, expected_code_hash:original.code_hash});
  return 产物;
}
"#;
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                let messages = body["messages"].as_array().unwrap();
                let message = if let Some(task) = messages.iter().find(|m| m["name"] == "program_task_input") {
                    let task: Value = serde_json::from_str(task["content"].as_str().unwrap()).unwrap();
                    assert_eq!(task["name"], "检查");
                    let input = &task["bindings"];
                    assert!(body["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "invoke_library"));
                    assert!(!messages.iter().any(|m| m["name"] == "task_program_skill"), "virtual work should not receive the full programming guide");
                    assert!(body["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "read_program_guide"));
                    let has_call = |id: &str| messages.iter().any(|m| m["tool_calls"].as_array().is_some_and(|calls| calls.iter().any(|c| c["id"] == id)));
                    if !has_call("contract") {
                        json!({"role":"assistant","tool_calls":[{"id":"contract","type":"function","function":{"name":"read_tool_contracts","arguments":json!({"methods":["io.read"]}).to_string()}}]})
                    } else if !has_call("invalid-read") {
                        assert!(messages.iter().any(|m| m["role"] == "tool" && m.to_string().contains("function read(args:")));
                        json!({"role":"assistant","tool_calls":[{"id":"invalid-read","type":"function","function":{"name":"invoke_library","arguments":json!({"method":"io.read","args":{}}).to_string()}}]})
                    } else if !has_call("inspect-actual") {
                        assert!(messages.iter().any(|m| m["role"] == "tool" && m.to_string().contains("invalid arguments")), "invalid input must be rejected before dispatch");
                        json!({"role":"assistant","tool_calls":[{"id":"inspect-actual","type":"function","function":{"name":"invoke_library","arguments":json!({"method":"io.read","args":{"path":input["产物"]["path"]}}).to_string()}}]})
                    } else {
                        assert!(messages.iter().any(|m| m["role"] == "tool" && m.to_string().contains("DRAFT")));
                        json!({"role":"assistant","content":json!({"passed":false,"evidence":[{"path":"artifact.txt","observed":"DRAFT","source":"read_file"}]}).to_string()})
                    }
                } else {
                    let (name,args) = match n {
                        0 => ("create_task", json!({"goal":"Produce, inspect and repair an artifact"})),
                        1 => ("read_task_board", json!({})),
                        2 => ("execute_program", json!({"source":source,"input":{"path":"artifact.txt","expected":"APPROVED"}})),
                        _ => panic!("the host must resume the caller without another planning round: {n}"),
                    };
                    json!({"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        tokio::time::timeout(
            Duration::from_secs(30),
            run_task(
                state,
                "task".into(),
                "fake".into(),
                "Produce, inspect and repair an artifact".into(),
                8,
                CancellationToken::new(),
                1,
                false,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        let status: String = conn
            .query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let terminal = conn
            .prepare(
                "SELECT data FROM agent_task_events WHERE kind IN ('turn/end','program/error')",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(status, "completed", "{terminal:?}");
        assert_eq!(requests.load(Ordering::SeqCst), 7);
        assert_eq!(
            std::fs::read_to_string(root.path().join("artifact.txt")).unwrap(),
            "APPROVED"
        );
        let raw: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let w: Window = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            w.program_value,
            Some(json!({"path":"artifact.txt","content":"APPROVED"}))
        );
        let nodes = crate::task_tree::nodes(&conn, "task").unwrap();
        let inspections = nodes
            .iter()
            .filter(|n| n.inputs["program_task"]["name"] == "检查")
            .collect::<Vec<_>>();
        assert_eq!(inspections.len(), 1);
        for node in inspections {
            assert_eq!(node.status, "done");
            let remaining: i64 = conn
                .query_row(
                    "SELECT count(*) FROM agent_runtime_windows WHERE scope=?1",
                    [crate::task_tree::frame_scope(node)],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                remaining, 0,
                "explicit inspection releases its local context"
            );
        }
        let evidence: i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/result' AND json_extract(data,'$.meta.result.result') LIKE '%evidence%'",[],|r|r.get(0)).unwrap();
        assert_eq!(
            evidence, 1,
            "explicit inspection evidence remains in durable receipts"
        );
    }

    #[tokio::test]
    async fn recorded_receipt_recovers_program_without_replaying_the_write() {
        let root = TestRoot::new();
        let state = agent_service::tests::test_state(root.path(), "127.0.0.1:9".parse().unwrap());
        agent_service::open_db(root.path()).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        let cancel = CancellationToken::new();
        let session = Session {
            agent_id: None,
            task_node_id: None,
            step_node_id: None,
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
        install(&mut w,&json!({"source":"function main(){const result=io.write({path:'never-repeat.txt',content:'x'});return result.receipt;}"})).unwrap();
        let State::Action(action) = w.program.as_mut().unwrap().next() else {
            panic!()
        };
        save(&session, &w).await.unwrap();
        append(&session,"tool/result",json!({"message":{"toolCallId":action.id,"isError":false},"meta":{"result":{"receipt":"saved"}}}),false).await.unwrap();
        let raw: String = agent_service::open_db(root.path())
            .unwrap()
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mut recovered: Window = serde_json::from_str(&raw).unwrap();
        recover(&session, &mut recovered).await.unwrap();
        assert!(
            matches!(recovered.program.as_mut().unwrap().next(),State::Returned(v) if v=="saved")
        );
        assert!(!root.path().join("never-repeat.txt").exists());

        // A persisted explicit inspection return is consumed once; ordinary
        // boolean branching does not ask a model for another verdict.
        let mut w = Window::default();
        install(&mut w, &json!({"source":"function main(){const check=inspect();if(!check.passed){return 'repair';}return 'deliver';} function inspect(){/* @virtual */ // Return {passed:boolean,evidence:[...]}.\n}"})).unwrap();
        let State::Action(action) = w.program.as_mut().unwrap().next() else {
            panic!()
        };
        save(&session, &w).await.unwrap();
        append(&session,"tool/result",json!({"message":{"toolCallId":action.id,"isError":false},"meta":{"result":{"result":json!({"passed":false,"evidence":[{"source":"saved inspection"}]}).to_string()}}}),false).await.unwrap();
        let raw: String = agent_service::open_db(root.path())
            .unwrap()
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mut restored: Window = serde_json::from_str(&raw).unwrap();
        recover(&session, &mut restored).await.unwrap();
        recover(&session, &mut restored).await.unwrap();
        assert!(
            matches!(restored.program.as_mut().unwrap().next(),State::Returned(v) if v=="repair")
        );
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

    #[tokio::test]
    async fn compile_error_returns_to_ai_and_checked_rewrite_resumes_without_replaying_work() {
        let root = TestRoot::new();
        let directory = root.path().to_path_buf();
        let seen = Arc::new(AtomicUsize::new(0));
        let requests = seen.clone();
        let source = "function main(){const saved=author_pages({path:'actual.txt'});return io.read({path:saved.path}).content;} /** @virtual Write actual content to args.path; return {path}. */ function author_pages(args){}";
        let fixed = "function author_pages(args){io.write({path:args.path,content:'ACTUAL'});return {path:args.path};}";
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body):Json<Value>| {
            let n = requests.fetch_add(1, Ordering::SeqCst);
            let messages = body["messages"].as_array().unwrap();
            let (name,args) = match n {
                0 => ("create_task",json!({"goal":"Create actual output and recover from a rejected function source"})),
                1 => ("execute_program",json!({"source":source})),
                2 => {
                    assert!(messages.iter().any(|m|m["name"]=="program_task_input"));
                    ("define_task_body",json!({"source":include_str!("../agent_program/fixtures/virtual-body-missing-brace.js")}))
                },
                3 => {
                    let failed=messages.iter().find(|m|m["tool_call_id"]=="fix-2").unwrap();
                    let error:Value=serde_json::from_str(failed["content"].as_str().unwrap()).unwrap();
                    assert_eq!(error["code"],"PROGRAM_SOURCE_INVALID");
                    assert_eq!(error["diagnostics"][0]["line"],17);
                    assert!(!directory.join("actual.txt").exists());
                    ("check_program",json!({"source":fixed}))
                },
                4 => {
                    let checked=messages.iter().find(|m|m["tool_call_id"]=="fix-3").unwrap();
                    let receipt:Value=serde_json::from_str(checked["content"].as_str().unwrap()).unwrap();
                    assert_eq!(receipt["status"],"valid");
                    assert_eq!(receipt["executed"],false);
                    assert_eq!(receipt["submit_tool"],"define_task_body");
                    assert!(!directory.join("actual.txt").exists(),"preflight must not execute writes");
                    ("define_task_body",json!({"source":fixed}))
                },
                other=>panic!("unexpected re-planning request {other}"),
            };
            async move {Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":format!("fix-{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}))}
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Create actual output".into(),
            12,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(seen.load(Ordering::SeqCst), 5);
        assert_eq!(
            std::fs::read_to_string(root.path().join("actual.txt")).unwrap(),
            "ACTUAL"
        );
        assert_eq!(
            conn.query_row("SELECT status FROM agent_tasks", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "completed"
        );
        for (name, expected) in [
            ("write_file", 1),
            ("read_file", 1),
            ("create_task", 2),
            ("check_program", 1),
        ] {
            let count:i64=conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')=?1",[name],|r|r.get(0)).unwrap();
            assert_eq!(
                count, expected,
                "{name} must not be replayed or create duplicate tasks"
            );
        }
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

    #[tokio::test]
    async fn virtual_function_reuses_input_and_checks_work_in_one_scope() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("source.txt"), "ORIGINAL_MATERIAL").unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let source = "function main(){const material=io.read({path:'source.txt'});const result=verify(material);assert(result.target===material.path,'result contract mismatch');return result;} /** @virtual Inspect the supplied material, process it, verify the result and return {target,verified}. */ function verify(material){}";
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            let messages = body["messages"].as_array().unwrap();
            let invoke = |name: &str, args: Value| json!({"role":"assistant","tool_calls":[{"id":format!("contract-{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
            let message = match n {
                0 => invoke("create_task", json!({"goal":"Verify supplied material"})),
                1 => invoke("execute_program", json!({"source":source})),
                2 => {
                    let task: Value = serde_json::from_str(messages.iter().find(|m|m["name"]=="program_task_input").unwrap()["content"].as_str().unwrap()).unwrap();
                    assert_eq!(task["bindings"]["material"]["content"], "ORIGINAL_MATERIAL");
                    invoke("invoke_library", json!({"method":"io.write","args":{"path":"result.txt","content":"processed"}}))
                },
                3 => {
                    assert!(messages.iter().any(|m|m["tool_call_id"]=="contract-2"));
                    invoke("invoke_library", json!({"method":"io.read","args":{"path":"result.txt"}}))
                },
                4 => {
                    assert!(messages.iter().any(|m|m["tool_call_id"]=="contract-2"));
                    assert!(messages.iter().any(|m|m["tool_call_id"]=="contract-3" && m["content"].as_str().unwrap_or("").contains("processed")));
                    json!({"role":"assistant","content":json!({"target":"source.txt","verified":true}).to_string()})
                },
                _ => panic!("unexpected request {n}"),
            };
            async move { Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Verify supplied material".into(),
            12,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 5);
        let source_reads: i64 = conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='read_file' AND data LIKE '%source.txt%'", [], |r|r.get(0)).unwrap();
        assert_eq!(source_reads, 1);
        let task_count: i64 = conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE kind='task/node_started' AND json_extract(data,'$.inputs.program_task.name')='verify'", [], |r|r.get(0)).unwrap();
        assert_eq!(task_count, 1);
        let raw: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["program_value"],
            json!({"target":"source.txt","verified":true})
        );
    }

    #[tokio::test]
    async fn failed_scoped_body_returns_to_same_ai_context_and_invalid_json_can_be_corrected() {
        let root = TestRoot::new();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let source = "function main(){const result=work();return result;} /** @virtual Complete the work and return a JSON object. */ function work(){}";
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let n = requests.fetch_add(1, Ordering::SeqCst);
            let messages = body["messages"].as_array().unwrap();
            let invoke = |name: &str, args: Value| json!({"role":"assistant","tool_calls":[{"id":format!("recovery-{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
            let message = match n {
                0 => invoke("create_task", json!({"goal":"Complete the work"})),
                1 => invoke("execute_program", json!({"source":source})),
                2 => invoke("define_task_body", json!({"source":"function work(){const saved=io.write({path:'once.txt',content:'written once'});const marker='local evidence';assert(false,'needs repair');return saved;}"})),
                3 => {
                    let recovery = messages.iter().find(|m| m["name"] == "function_recovery").unwrap();
                    assert!(recovery["content"].as_str().unwrap().contains("needs repair"));
                    invoke("invoke_library", json!({"method":"host.read_scope","args":{"name":"marker"}}))
                },
                4 => {
                    assert!(messages.iter().any(|m| m["tool_call_id"] == "recovery-3" && m["content"].as_str().unwrap_or("").contains("local evidence")));
                    json!({"role":"assistant","content":"not json"})
                },
                5 => {
                    assert!(messages.iter().any(|m| m["name"] == "function_result_diagnostic"));
                    assert!(messages.iter().any(|m| m["name"] == "function_recovery"));
                    json!({"role":"assistant","content":"{\"recovered\":true}"})
                },
                _ => panic!("unexpected model request {n}"),
            };
            async move { Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)", []).unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Complete the work".into(),
            12,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 6);
        assert_eq!(
            std::fs::read_to_string(root.path().join("once.txt")).unwrap(),
            "written once"
        );
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='write_file'", [], |r| r.get::<_,i64>(0)).unwrap(), 1);
        let raw: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["program_value"],
            json!({"recovered":true})
        );
    }

    #[tokio::test]
    async fn library_values_and_fixed_names_match_between_source_and_native_task_calls() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("source.txt"), "RAW_TOOL_VALUE").unwrap();
        std::fs::write(root.path().join("native.txt"), "RAW_TOOL_VALUE").unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let source = "function main(){const actual=io.read({path:'source.txt'});const observed=inspect(actual);assert(actual.content===observed.content && actual.code_hash===observed.code_hash && observed.path==='native.txt','library return shape changed');return observed.content;} /** @virtual Read native.txt with the actual library and return its original JSON. */ function inspect(actual){}";
        let app=Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>|{
            let n=requests.fetch_add(1,Ordering::SeqCst);let messages=body["messages"].as_array().unwrap();
            let names=body["tools"].as_array().unwrap().iter().filter_map(|t|t["function"]["name"].as_str()).collect::<Vec<_>>();
            for old in ["spawn_agent","join_agents","list_tool_groups","load_tool_group","unload_tool_group","read_file"] {assert!(!names.contains(&old),"private/legacy tool leaked: {old}");}
            let message=match n {
                0=>json!({"role":"assistant","tool_calls":[{"id":"start","type":"function","function":{"name":"create_task","arguments":json!({"goal":"Compare source and native library calls"}).to_string()}}]}),
                1=>json!({"role":"assistant","tool_calls":[{"id":"source","type":"function","function":{"name":"execute_program","arguments":json!({"source":source}).to_string()}}]}),
                2=>{
                    let descriptor=messages.iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let view:Value=serde_json::from_str(descriptor["content"].as_str().unwrap()).unwrap();
                    assert_eq!(view["signature"],"function inspect(actual)");
                    assert_eq!(view["bindings"]["actual"]["content"],"RAW_TOOL_VALUE");
                    assert!(view["bindings"]["actual"].get("event_seq").is_none());
                    assert!(names.contains(&"invoke_library"));
                    json!({"role":"assistant","tool_calls":[{"id":"native-read","type":"function","function":{"name":"invoke_library","arguments":json!({"method":"io.read","args":{"path":"native.txt"}}).to_string()}}]})
                },
                3=>{
                    let message=messages.iter().find(|m|m["tool_call_id"]=="native-read").unwrap();
                    let actual:Value=serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
                    assert_eq!(actual["content"],"RAW_TOOL_VALUE", "native result: {actual}");
                    let descriptor=messages.iter().find(|m|m["name"]=="program_task_input").unwrap();
                    let view:Value=serde_json::from_str(descriptor["content"].as_str().unwrap()).unwrap();
                    let mut source_value=view["bindings"]["actual"].clone();
                    source_value["path"]=json!("native.txt");
                    assert_eq!(actual,source_value,"only the explicitly different file path may differ");
                    for wrapper in ["original_result","asset_reference","event_seq"] {assert!(actual.get(wrapper).is_none(),"{wrapper} is transport metadata, not the tool value");}
                    json!({"role":"assistant","content":actual.to_string()})
                },other=>panic!("unexpected model request {other}")
            };
            async move {Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))}
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Check library contracts".into(),
            10,
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
        assert_eq!(count.load(Ordering::SeqCst), 4);
        let events:i64=conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE kind='tool/result' AND json_extract(data,'$.meta.result.content')='RAW_TOOL_VALUE'",[],|r|r.get(0)).unwrap();
        assert_eq!(events, 2, "audit history keeps both original tool results");
    }

    #[tokio::test]
    async fn visible_parent_values_are_inherited_and_finished_locals_are_released() {
        let root = TestRoot::new();
        let payload = "SOURCE_FOR_PREPARATION_ONLY_".repeat(500);
        std::fs::write(root.path().join("source.txt"), &payload).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let source = "function main(){const doc=io.read({path:'source.txt'});const shared=prepare(doc.content);const result=produce(shared);return {result,shared};} /** @virtual Summarize the source into {brief}. */ function prepare(source){} /** @virtual Save output.txt using shared.brief and return its result. */ function produce(shared){}";
        let app=Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>|{
            let n=seen.fetch_add(1,Ordering::SeqCst);let messages=body["messages"].as_array().unwrap();
            let invoke=|name:&str,args:Value|json!({"role":"assistant","tool_calls":[{"id":format!("scope-{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
            let message=match n {
                0=>invoke("create_task",json!({"goal":"Prepare once and produce actual output"})),
                1=>invoke("execute_program",json!({"source":source})),
                2=>{
                    let text=body.to_string();
                    assert_eq!(text.matches("SOURCE_FOR_PREPARATION_ONLY_").count(),500,"only the referenced argument is delivered, not the unused parent value");
                    assert!(!messages.iter().any(|m|matches!(m["name"].as_str(),Some("scope_value"|"caller_activity"))));
                    json!({"role":"assistant","content":json!({"brief":"Prepared content"}).to_string()})
                },
                3=>{
                    assert!(!body.to_string().contains("SOURCE_FOR_PREPARATION_ONLY_"),"unused parent bodies are indexed but not sent to the next function");
                    let input:Value=serde_json::from_str(messages.iter().find(|m|m["name"]=="program_task_input").unwrap()["content"].as_str().unwrap()).unwrap();
                    assert_eq!(input["bindings"]["shared"]["brief"],"Prepared content");
                    invoke("invoke_library",json!({"method":"io.write","args":{"path":"output.txt","content":"Prepared content"}}))
                },
                4=>json!({"role":"assistant","content":json!({"path":"output.txt","saved":true}).to_string()}),
                n=>panic!("unexpected extra material or scheduling request {n}"),
            };
            async move{Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))}
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(
            state,
            "task".into(),
            "fake".into(),
            "Prepare and save output".into(),
            12,
            CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(
            std::fs::read_to_string(root.path().join("output.txt")).unwrap(),
            "Prepared content"
        );
        let main: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !main.contains("SOURCE_FOR_PREPARATION_ONLY_"),
            "finished task locals must leave the live conversation frame"
        );
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='read_file'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/result' AND json_extract(data,'$.meta.result.content') LIKE 'SOURCE_FOR_PREPARATION_ONLY_%'",[],|r|r.get::<_,i64>(0)).unwrap(),1,"the original result remains in history");
    }
}
