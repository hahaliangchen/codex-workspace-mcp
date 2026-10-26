//! Adapter from flow pseudocode to the existing tool/Agent boundary.
//! It synthesizes normal tool exchanges; it never calls filesystem/plugin APIs directly.
use super::*;
use crate::agent_program::{self, Action, Runtime, State, runtime::ActionKind};

pub(super) enum Round {
    Exchange(String, Value),
}
pub(super) fn library_catalog(s: &Session<'_>, definitions: &[Value]) -> agent_program::libraries::Libraries {
    let packages=s.state.tool_catalog.as_ref().map(|c|c.packages.catalog()).unwrap_or_else(||json!([]));
    agent_program::libraries::catalog(definitions,&packages)
}
pub(super) fn install_scoped(s: &Session<'_>, w: &mut Window, args: &Value, body: bool) -> Result<Value> {
    ensure!(args.is_object(), "program arguments must be a JSON object");
    ensure!(args.get("libraries").is_none(), "library names are fixed by the host/plugin declarations; libraries aliases are unsupported");
    let libraries = library_catalog(s, &tools(s,w));
    if body { install_body_with(w,args,libraries) } else { install_with(w,args,libraries) }
}
#[cfg(test)]
pub(super) fn install(w: &mut Window, args: &Value) -> Result<Value> {
    install_with(w,args,agent_program::libraries::builtin())
}
fn install_with(w: &mut Window, args: &Value, libraries: agent_program::libraries::Libraries) -> Result<Value> {
    ensure!(args.get("libraries").is_none(), "model-defined libraries are unsupported");
    let source = args["source"].as_str().context("source is required")?;
    let program = agent_program::compile_with_libraries(source,libraries)?;
    let entry = args["entry"].as_str().unwrap_or("main");
    ensure!(
        entry == "main",
        "execute_program enters the task through its single main; helpers run through ordinary calls/fork"
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
    install_body_with(w,args,agent_program::libraries::builtin())
}
fn install_body_with(w: &mut Window, args: &Value, libraries: agent_program::libraries::Libraries) -> Result<Value> {
    ensure!(args.get("libraries").is_none(), "model-defined libraries are unsupported");
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
    let program = agent_program::compile_body_with_libraries(source, name, libraries)?;
    let params = &program.functions[name].params;
    let input = task["input"]
        .as_object()
        .context("named function arguments missing")?;
    let values = if params.iter().all(|p| input.contains_key(p)) {
        params.iter().map(|p| input[p].clone()).collect()
    } else if params.len() == 1 {
        // A single context parameter receives the complete named input object.
        vec![Value::Object(input.clone())]
    } else {
        anyhow::bail!(
            "bind parameters by their current input names, or use one parameter for the complete named input object"
        )
    };
    let entry = name.to_owned();
    w.program = Some(Runtime::task_program(program, &entry, values)?);
    w.program_value = None;
    Ok(
        json!({"status":"started","function":entry,"version":agent_program::VERSION,"source_hash":crate::visual_artifacts::hash(source.as_bytes())}),
    )
}
pub(super) fn fork_runtime(args: &Value) -> Result<Option<Runtime>> {
    let Some(p) = args.get("program") else {
        return Ok(None);
    };
    let program: agent_program::Program = serde_json::from_value(p["compiled"].clone()).context("compiled host program required")?;
    let entry = p["entry"].as_str().context("program.entry is required")?;
    let arguments = p["args"]
        .as_array()
        .context("program.args must be an array")?
        .clone();
    Ok(Some(Runtime::task_program(
        program,
        entry,
        arguments,
    )?))
}
fn recorded(s: &Session<'_>, id: &str, kind: &str) -> Result<Option<(i64, Value)>> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let (normal, wrapped) = if kind == "tool/result" {
        ("$.message.toolCallId", "$.event.message.toolCallId")
    } else {
        ("$.action_id", "$.event.action_id")
    };
    let sql = format!(
        "SELECT seq,data FROM agent_task_events WHERE task_id=?1 AND ((kind=?2 AND json_extract(data,'{normal}')=?3) OR (kind='subagent/event' AND json_extract(data,'$.event_type')=?2 AND json_extract(data,'{wrapped}')=?3)) ORDER BY seq DESC LIMIT 1"
    );
    let row: Option<(i64, String)> = conn
        .query_row(&sql, params![s.task, kind, id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
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
    s: &Session<'_>,
    w: &mut Window,
    action: &Action,
    mut value: Value,
    mut failed: bool,
    seq: i64,
) -> Result<()> {
    if !failed {
        match &action.kind {
            ActionKind::Fork { .. } => {
                if value["agent_id"].is_string() {
                    value = json!({"agent_id":value["agent_id"]});
                } else {
                    failed = true;
                    value = json!({"message":"fork did not return an Agent handle","result":value});
                }
            }
            ActionKind::Join { ids } => {
                let conn = crate::collaboration_jobs::db(s.state.workspace.root())?;
                let mut results = Vec::new();
                for id in ids {
                    let raw: String = conn.query_row(
                        "SELECT result FROM agent_delegates WHERE task_id=?1 AND id=?2",
                        params![s.task, id],
                        |r| r.get(0),
                    )?;
                    let result: Value = serde_json::from_str(&raw)?;
                    if result["status"] != "completed" {
                        failed = true;
                        results.push(json!({"agent_id":id,"status":result["status"],"error":result["error"]}));
                    } else {
                        results.push(result["program_value"].clone());
                    }
                }
                value = if failed {
                    json!({"message":"a forked function failed","children":results})
                } else {
                    json!(results)
                };
            }
            ActionKind::Tool { .. } => {
                if value.is_object() {
                    value["event_seq"] = json!(seq);
                }
            }
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
            ActionKind::Ai { .. } => {}
        }
    }
    w.program.as_mut().unwrap().resume(value, failed)
}
async fn recover(s: &Session<'_>, w: &mut Window) -> Result<()> {
    // Version rejection must precede receipt consumption or dispatch.
    if w.program.as_ref().is_some_and(|p|p.program.version!=agent_program::VERSION) {return Ok(());}
    let Some(action) = w.program.as_ref().and_then(|p| p.pending.clone()) else {
        return Ok(());
    };
    let kind = if matches!(action.kind, ActionKind::Ai { .. }) {
        "program/ai_result"
    } else {
        "tool/result"
    };
    if let Some((seq, event)) = recorded(s, &action.id, kind)? {
        if kind == "program/ai_result" {
            resolve(
                s,
                w,
                &action,
                event["value"].clone(),
                event["failed"] == true,
                seq,
            )?;
        } else {
            resolve(
                s,
                w,
                &action,
                event["meta"]["result"].clone(),
                event["message"]["isError"] == true,
                seq,
            )?;
        }
        save(s, w).await?;
    }
    Ok(())
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
    let text = if w.program_task.is_some() {
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
        s.is_root(),
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
    let state = w.program.as_mut().unwrap().next();
    save(s, w).await?;
    match state {
        State::Failed(error) => {
            append(s, "program/error", json!({"error":error}), true).await?;
            if w.program_task.is_some() {
                return_value(s, w, step, json!({"$task_error":error})).await
            } else {
                anyhow::bail!("TS_PROGRAM_RUNTIME: {error}")
            }
        }
        State::Returned(value) => return_value(s, w, step, value).await,
        State::Action(action) => {
            let at = json!(action.at);
            let (name, args) = match &action.kind {
                ActionKind::Ai { .. } => anyhow::bail!("legacy AI expression actions are unsupported; use an explicit task"),
                ActionKind::Call { name, args } => {
                    let program = &w.program.as_ref().unwrap().program;
                    let function = &program.functions[name];
                    let input: serde_json::Map<String, Value> =
                        function.params.iter().cloned().zip(args.clone()).collect();
                    let description = if function.description.is_empty() {
                        format!(
                            "Execute the {name} task with its supplied arguments and compiled function body"
                        )
                    } else {
                        function.description.clone()
                    };
                    let mut task = json!({"name":name,"description":description,"input":input});
                    if !function.comment_only {
                        task["body"] = json!({"compiled":program,"entry":name,"args":args});
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
                } => (
                    "create_task".into(),
                    json!({"goal":name,"briefing":description,"program_task":{"name":name,"description":description,"input":input}}),
                ),
                ActionKind::Tool { name, args } => (name.clone(), args.clone()),
                ActionKind::Fork { entry, args } => (
                    "spawn_agent".into(),
                    json!({"name":format!("function {entry}"),"task":format!("Execute the compiled {entry} function with its provided arguments and return its value"),"program":{"compiled":w.program.as_ref().unwrap().program,"entry":entry,"args":args}}),
                ),
                ActionKind::Join { ids } => ("join_agents".into(), json!({"agent_ids":ids})),
            };
            let definitions = tools(s, w);
            if !definitions.iter().any(|t| t["function"]["name"] == name)
                && available_tools(s, w)
                    .iter()
                    .any(|t| t["function"]["name"] == name)
                && let Some(group) = tool_groups::group(&name)
            {
                let id = format!("program-load-{}", agent_service::uuid_like());
                let (node, message) = exchange(
                    s,
                    w,
                    step,
                    &id,
                    "load_tool_group",
                    json!({"group":group}),
                    &at,
                )
                .await?;
                return Ok(Round::Exchange(node, message));
            }
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
            let root = std::env::temp_dir()
                .join(format!("ts-program-test-{}", agent_service::uuid_like()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[tokio::test]
    async fn ts_program_drives_two_agents_tools_tasks_and_join_without_model_scheduler_rounds() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("spec.txt"), "SHARED_SOURCE").unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let source = r#"
function worker(input) {
  const value = make_text(input.text);
  io.write({path:input.path,content:value});
  return {path:input.path, text:value};
}
function make_text(text) {
  // Return the supplied text as a JSON string.
}
function main() {
  const spec=io.read({path:"spec.txt"});
  const a=fork(worker,{path:"a.txt",text:"OUTPUT_A"});
  const b=fork(worker,{path:"b.txt",text:"OUTPUT_B"});
  const [x,y]=join(a,b);
  const checkA=io.read({path:x.path});
  const checkB=io.read({path:y.path});
  return {outputs:[x.path,y.path],source:spec,checkA,checkB};
}"#;
        let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>|{
            let requests=requests.clone();async move {
                let n=requests.fetch_add(1,Ordering::SeqCst);
                let messages=body["messages"].as_array().unwrap();
                let message=if let Some(input)=messages.iter().find(|m|m["name"]=="program_task_input") {
                    let task:Value=serde_json::from_str(input["content"].as_str().unwrap()).unwrap();
                    assert_eq!(task["name"], "make_text");
                    json!({"role":"assistant","content":task["input"]["text"].to_string()})
                } else {
                    let (name,args)=match n {0=>("create_task",json!({"goal":"Use two Agents with typed function returns"})),1=>("execute_program",json!({"source":source})),_=>panic!("unexpected scheduler model request {n}")};
                    json!({"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        tokio::time::timeout(
            Duration::from_secs(40),
            run_task(
                state,
                "task".into(),
                "fake".into(),
                "Use two Agents for two files".into(),
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
        let events =
            crate::session_history::load_events(&conn, "task", &Default::default()).unwrap();
        assert_eq!(
            status,
            "completed",
            "{}",
            events
                .iter()
                .filter(|e| e.kind == "turn/end")
                .map(|e| e.data().unwrap().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            4,
            "only setup and two comment tasks call a model"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "OUTPUT_A"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
            "OUTPUT_B"
        );
        assert!(!events.iter().any(|e|e.kind=="tool/result" && e.data().unwrap()["message"]["isError"]==true));
        let completed = crate::collaboration_jobs::statuses(root.path(), "task").unwrap();
        assert_eq!(completed.as_array().unwrap().len(), 2);
        assert!(
            completed
                .as_array()
                .unwrap()
                .iter()
                .all(|j| j["status"] == "completed" && j["program_function"] == true)
        );
        assert!(!crate::collaboration_jobs::outstanding(root.path(), "task").unwrap());
    }
    #[tokio::test]
    async fn comment_tasks_use_tools_return_typed_values_and_resume_two_forked_callers() {
        let root = TestRoot::new();
        std::fs::write(root.path().join("spec.txt"), "SHARED_SOURCE").unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let source = r#"
function main() {
  const spec=io.read({path:"spec.txt"});
  const a=fork(worker,{path:"a.txt",text:"A",source:spec.content});
  const b=fork(worker,{path:"b.txt",text:"B",source:spec.content});
  const [x,y]=join(a,b);
  return {x,y};
}
function worker(part) {
  const saved=produce(part);
  const checked=io.read({path:saved.path});
  return {path:saved.path,content:checked.content};
}
function produce(part) {
  // Write part.text to part.path using the actual file tool.
  // Reuse part.source as shared background; return {path:string} after saving.
}
"#;
        let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>|{
            let n=requests.fetch_add(1,Ordering::SeqCst);
            async move {
                let messages=body["messages"].as_array().unwrap();
                let message=if let Some(task)=messages.iter().find(|m|m["name"]=="program_task_input") {
                    assert!(messages.iter().any(|m|m["name"]=="task_program_skill"),"the executing AI needs the generic representation skill to refine this function");
                    assert!(messages.iter().any(|m|m["name"]=="program_task_execution"));
                    let names=body["tools"].as_array().unwrap().iter().map(|t|t["function"]["name"].as_str().unwrap()).collect::<Vec<_>>();
                    assert!(names.contains(&"invoke_library"));
                    assert!(!names.contains(&"write_file"));
                    for name in ["create_task","execute_program","spawn_agent","join_agents"] {assert!(!names.contains(&name),"{name} must not be advertised to a task body");}
                    let task:Value=serde_json::from_str(task["content"].as_str().unwrap()).unwrap();
                    assert_eq!(task["name"],"produce");
                    assert!(names.contains(&"define_task_body"));
                    let part=&task["input"]["part"];
                    assert_eq!(part["source"],"SHARED_SOURCE");
                    if messages.iter().any(|m|m["role"]=="assistant" && m["tool_calls"].as_array().is_some_and(|a|a.iter().any(|c|c["function"]["name"]=="invoke_library"))) {
                        json!({"role":"assistant","content":json!({"path":part["path"]}).to_string()})
                    } else {
                        json!({"role":"assistant","tool_calls":[{"id":format!("write{n}"),"type":"function","function":{"name":"invoke_library","arguments":json!({"method":"io.write","args":{"path":part["path"],"content":part["text"]}}).to_string()}}]})
                    }
                } else {
                    assert!(messages.iter().any(|m|m["name"]=="task_program_skill"));
                    let (name,args)=match n {0=>("create_task",json!({"goal":"Two forked helpers with comment tasks"})),1=>("execute_program",json!({"source":source})),_=>panic!("unexpected planner request {n}")};
                    json!({"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
                };
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = agent_service::tests::test_state(root.path(), listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        tokio::time::timeout(
            Duration::from_secs(40),
            run_task(
                state,
                "task".into(),
                "fake".into(),
                "Use two Agents for two distinct files".into(),
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
        let events =
            crate::session_history::load_events(&conn, "task", &Default::default()).unwrap();
        let status: String = conn
            .query_row("SELECT status FROM agent_tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            status,
            "completed",
            "{}",
            events
                .iter()
                .filter(|e| e.kind == "turn/end")
                .map(|e| e.data().unwrap().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            6,
            "setup plus two tool-using task bodies; no scheduler or completion model calls"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "A"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
            "B"
        );
        let jobs = crate::collaboration_jobs::statuses(root.path(), "task").unwrap();
        assert!(
            jobs.as_array()
                .unwrap()
                .iter()
                .all(|j| j["status"] == "completed")
        );
        let nodes = crate::task_tree::nodes(&conn, "task").unwrap();
        let bodies = nodes
            .iter()
            .filter(|n| n.inputs["program_task"].is_object())
            .collect::<Vec<_>>();
        assert_eq!(bodies.len(), 2);
        for node in bodies {
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
                "local working window must be released after return"
            );
        }
        let raw: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let final_window: Window = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            final_window.program_value,
            json!({"x":{"path":"a.txt","content":"A"},"y":{"path":"b.txt","content":"B"}}).into()
        );
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
                            assert!(messages.iter().any(|m|m["name"]=="task_program_skill"));
                            ("define_task_body",json!({"source":r#"
function phase(request) {
  // Refine this same task, not another main.
  const result=stage(request);
  io.write({path:request.output,content:result});
  return {path:request.output,status:result};
}
function stage(request) {
  // Coded helpers are tasks too; tools remain steps.
  const file=io.read({path:request.source});
  return analyze(request,file.content);
}
function analyze(request,content) { return evaluate(request,content); }
function evaluate(request,content) {
  // Evaluate the current content and return the JSON string READY.
}
"#}))
                        },
                        "evaluate"=>{
                            assert_eq!(task["input"]["content"],"SOURCE");
                            return Json(json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"\"READY\""}}]}));
                        },
                        other=>panic!("coded helper {other} should execute without a model scheduling round"),
                    }
                } else {
                    match n {
                        0=>("create_task",json!({"goal":"Nested task functions preserve the caller and its inputs"})),
                        1=>("load_tool_group",json!({"group":"files"})),
                        2=>("execute_program",json!({"source":"function main(request){const result=phase(request);const file=io.read({path:result.path});return {result,actual:file.content};} function phase(request){// Read and process the source, save output, and return its path and status.\n}","input":{"source":"source.txt","output":"output.txt"}})),
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
        assert_eq!(count.load(Ordering::SeqCst), 5);
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
            4,
            "one task per AI-defined function call; refinement reuses phase"
        );
        for name in ["phase", "stage", "analyze", "evaluate"] {
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
            "async function work(){return 1;}",
            "function work(input: string){return input;}",
            "function work(){return ai({instruction:'value'});}",
            "function work(){return io.read({});} export function unused(){return 1;}",
        ] {
            let mut main = Window::default();
            assert!(install(&mut main, &json!({"source":source.replace("work", "main")})).is_err());
            assert!(main.program.is_none());
            let mut child = Window { program_task: Some(json!({"name":"work","input":{}})), ..Default::default() };
            assert!(install_body(&mut child, &json!({"source":source})).is_err());
            assert!(child.program.is_none());
            assert!(fork_runtime(&json!({"program":{"source":source,"entry":"work","args":[]}})).is_err());
        }
    }

    #[tokio::test]
    async fn common_flow_runs_with_no_model_requests_after_program_submission() {
        let root=TestRoot::new();
        let count=Arc::new(AtomicUsize::new(0));
        let requests=count.clone();
        let source=r#"function main(){var rows=[];var sum=0;for(var i=0;i<4;i++){if(i===1)continue;const path=`part-${i}.txt`;io.write({path,content:`${i+1}`});const actual=io.read({path});rows.push(actual.content);sum+=i;}assert(rows.length===3);if(false&&io.read({path:'never'})){throw 'bad';}return {rows,sum,selection:sum>0?'passed':'failed'};}"#;
        let app=Router::new().route("/v1/chat/completions",post(move |Json(_):Json<Value>|{
            let n=requests.fetch_add(1,Ordering::SeqCst);
            let (name,args)=match n {0=>("create_task",json!({"goal":"Execute deterministic common flow"})),1=>("execute_program",json!({"source":source})),_=>panic!("ordinary computation must never request a model: {n}")};
            async move {Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}))}
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state=agent_service::tests::test_state(root.path(),listener.local_addr().unwrap());
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
        let conn=agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(state,"task".into(),"fake".into(),"Execute common flow".into(),8,CancellationToken::new(),1,false).await.unwrap();
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst),2);
        let status:String=conn.query_row("SELECT status FROM agent_tasks",[],|r|r.get(0)).unwrap();
        assert_eq!(status,"completed");
        for (i,text) in [(0,"1"),(2,"3"),(3,"4")] {assert_eq!(std::fs::read_to_string(root.path().join(format!("part-{i}.txt"))).unwrap(),text);}
        let calls:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='read_file'",[],|r|r.get(0)).unwrap();
        assert_eq!(calls,3,"the short-circuited read must not run");
        let raw:String=conn.query_row("SELECT state FROM agent_runtime_windows WHERE scope='main'",[],|r|r.get(0)).unwrap();
        let window:Window=serde_json::from_str(&raw).unwrap();
        assert_eq!(window.program_value,Some(json!({"rows":["1","3","4"],"sum":5,"selection":"passed"})));
    }

    #[test]
    fn task_body_accepts_named_or_aggregate_inputs_without_changing_the_caller() {
        for source in [
            "function work(shared,item){return {shared,item};}",
            "function work(input){return input;}",
        ] {
            let input = json!({"shared":{"rule":"R"},"item":{"path":"a"}});
            let mut w = Window {
                program_task: Some(json!({"name":"work","input":input})),
                ..Default::default()
            };
            install_body(&mut w, &json!({"source":source})).unwrap();
            assert!(
                matches!(w.program.as_mut().unwrap().next(),State::Returned(value) if value==input)
            );
        }
    }

    #[test]
    fn forked_functions_use_the_host_compiled_program() {
        let program=agent_program::compile("function work(input){return io.read({path:input.path});} function main(){return null;}").unwrap();
        let mut fork=fork_runtime(&json!({"program":{"compiled":program,"entry":"work","args":[{"path":"source.txt"}]}})).unwrap().unwrap();
        assert!(matches!(fork.next(),State::Action(Action{kind:ActionKind::Tool{name,args},..}) if name=="read_file" && args["path"]=="source.txt"));
    }

    #[tokio::test]
    async fn io_library_and_c_conditions_work_after_real_writes_without_extra_models() {
        let root=TestRoot::new();
        let count=Arc::new(AtomicUsize::new(0));
        let requests=count.clone();
        let source=r#"function main(){io.write({path:'one.txt',content:'one'});io.write({path:'two.txt',content:'two'});const actual=io.read({path:'two.txt'});const items=[actual];const result={};const obj={};if(items.length && !result.error && obj){return {content:actual.content,checked:true};}throw 'incorrect truth conversion';}"#;
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
            let (name,args)=match n {0=>("create_task",json!({"goal":"Use file library and C truth values"})),1=>("execute_program",json!({"source":source})),_=>panic!("no condition should request a model: {n}")};
            async move {Json(json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":format!("m{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}))}
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state=agent_service::tests::test_state(root.path(),listener.local_addr().unwrap());
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        let conn=agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        run_task(state,"task".into(),"fake".into(),"Use the library".into(),8,CancellationToken::new(),1,false).await.unwrap();
        server.abort();
        let status:String=conn.query_row("SELECT status FROM agent_tasks",[],|r|r.get(0)).unwrap();
        assert_eq!(status,"completed");
        assert_eq!(count.load(Ordering::SeqCst),2);
        assert_eq!(std::fs::read_to_string(root.path().join("one.txt")).unwrap(),"one");
        assert_eq!(std::fs::read_to_string(root.path().join("two.txt")).unwrap(),"two");
        let names:Vec<String>=conn.prepare("SELECT json_extract(data,'$.name') FROM agent_task_events WHERE kind='tool/call'").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(names.iter().filter(|n|n.as_str()=="write_file").count(),2);
        assert_eq!(names.iter().filter(|n|n.as_str()=="read_file").count(),1);
        assert!(!names.iter().any(|n|n.starts_with("io.")));
    }

    #[test]
    fn fixed_libraries_respect_frame_permissions_and_reject_model_aliases() {
        let root=TestRoot::new();
        let mut state=agent_service::tests::test_state(root.path(),"127.0.0.1:9".parse().unwrap());
        state.permission_mode=agent_service::PermissionMode::ReadOnly;
        let cancel=CancellationToken::new();
        let s=Session{agent_id:None,task_node_id:None,step_node_id:None,plugin:None,state:&state,task:"task",model:"fake",turn:1,cancel:&cancel,ban_execution:false,mode:Mode::Main,scope:"main".into(),parent:None,input_budget:INPUT_BUDGET};
        let mut w=Window::default();
        let source="function main(){io.read({path:'source'});io.write({path:'must-not-write'});}";
        assert!(install_scoped(&s,&mut w,&json!([]),false).is_err());
        assert!(install_scoped(&s,&mut w,&json!({"source":source}),false).is_err());
        assert!(w.program.is_none());
        assert!(install_scoped(&s,&mut w,&json!({"source":"function main(){return null;}","libraries":{"io2":{"write":"write_file"}}}),false).is_err());
    }

    #[tokio::test]
    async fn plugin_declared_library_uses_the_registered_owner_tool() {
        let root=TestRoot::new();
        std::fs::write(root.path().join("code-flow.json"),r#"{"version":1,"features":[]}"#).unwrap();
        let (_runtime,catalog)=crate::skill_packages::test_runtime();
        let count=Arc::new(AtomicUsize::new(0));let requests=count.clone();
        let project_root=root.path().to_string_lossy().into_owned();
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
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state=agent_service::tests::test_state(root.path(),listener.local_addr().unwrap());state.tool_catalog=Some(catalog);
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        let conn=agent_service::open_db(root.path()).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','','fake','running',0,0)",[]).unwrap();
        conn.execute("INSERT INTO plugin_sessions VALUES ('task','code-flow')",[]).unwrap();
        run_task(state,"task".into(),"fake".into(),"Read the plugin library".into(),6,CancellationToken::new(),1,false).await.unwrap();server.abort();
        assert_eq!(count.load(Ordering::SeqCst),2);
        let diagnostics=conn.prepare("SELECT data FROM agent_task_events WHERE kind IN ('turn/end','program/error','tool/result')").unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(conn.query_row("SELECT status FROM agent_tasks",[],|r|r.get::<_,String>(0)).unwrap(),"completed","{}",diagnostics.join("\n"));
        let calls:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='read_code_flow'",[],|r|r.get(0)).unwrap();
        assert_eq!(calls,1);
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
                mode: Mode::Main,
                scope: "main".into(),
                parent: None,
                input_budget: INPUT_BUDGET,
            };
            let mut w = Window::default();
            install(&mut w,&json!({"source":"function inspect(){// Inspect actual files and return the check result.\n} function main(){try{return inspect();}catch(error){return {message:error.message};}}"})).unwrap();
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
  产物 = 制作(request);
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
                    let input = &task["input"];
                    assert!(body["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "invoke_library"));
                    let read = messages.iter().any(|m| m["role"] == "assistant" && m["tool_calls"].as_array().is_some_and(|calls| calls.iter().any(|c| c["function"]["name"] == "invoke_library")));
                    if !read {
                        json!({"role":"assistant","tool_calls":[{"id":"inspect-actual","type":"function","function":{"name":"invoke_library","arguments":json!({"method":"io.read","args":{"path":input["产物"]["path"]}}).to_string()}}]})
                    } else {
                        assert!(messages.iter().any(|m| m["role"] == "tool" && m.to_string().contains("DRAFT")));
                        json!({"role":"assistant","content":json!({"passed":false,"evidence":[{"path":"artifact.txt","observed":"DRAFT","source":"read_file"}]}).to_string()})
                    }
                } else {
                    let (name,args) = match n {
                        0 => ("create_task", json!({"goal":"Produce, inspect and repair an artifact"})),
                        1 => ("load_tool_group", json!({"group":"files"})),
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
        assert_eq!(requests.load(Ordering::SeqCst), 5);
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
            mode: Mode::Main,
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
        install(&mut w, &json!({"source":"function main(){const check=inspect();if(!check.passed){return 'repair';}return 'deliver';} function inspect(){// Return {passed:boolean,evidence:[...]}.\n}"})).unwrap();
        let State::Action(action) = w.program.as_mut().unwrap().next() else {
            panic!()
        };
        assert_eq!(action.semantic_boolean, None);
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
}
