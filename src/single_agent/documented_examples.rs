//! Execute the published source, not a rewritten approximation of its main.
use super::*;
use axum::{Json, Router, routing::post};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

// Opt-in fault injection exists only in test builds and is scoped to one
// isolated test workspace. It runs immediately before the program reads an
// existing output, after the agent itself has produced it.
static FAULT: std::sync::LazyLock<
    std::sync::Mutex<Option<(std::path::PathBuf, std::path::PathBuf)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

fn latest_run_marker(name: &str) -> std::path::PathBuf {
    if cfg!(windows) {
        std::env::temp_dir().join(name)
    } else {
        Path::new("/tmp").join(name)
    }
}

pub(super) fn inject_before_read(root: &Path, name: &str, args: &Value) -> Result<()> {
    if name != "read_file" {
        return Ok(());
    }
    let mut fault = FAULT.lock().unwrap();
    let Some((target_root, evidence)) = fault.as_ref() else {
        return Ok(());
    };
    if root != target_root {
        return Ok(());
    }
    let Some(path) = args["path"].as_str().filter(|p| p.ends_with(".html")) else {
        return Ok(());
    };
    let file = root.join(path);
    if !file.is_file() {
        return Ok(());
    }
    let original = std::fs::read_to_string(&file)?;
    if !original.contains("shared.css") {
        return Ok(());
    }
    let changed = original.replacen("shared.css", "missing-local-style.css", 1);
    std::fs::write(evidence.join("before-fault.html"), original)?;
    std::fs::write(evidence.join("injected-fault.html"), &changed)?;
    std::fs::write(
        evidence.join("fault.json"),
        serde_json::to_vec_pretty(
            &json!({"path":path,"change":"shared.css -> missing-local-style.css"}),
        )?,
    )?;
    std::fs::write(file, changed)?;
    *fault = None;
    Ok(())
}

struct Example {
    name: String,
    source: String,
    input: Value,
    fragment: Option<String>,
}

fn examples() -> Vec<Example> {
    let mut cases = Vec::new();
    for (name, text) in [
        ("manual", include_str!("../../docs/agent-program.md")),
        ("skill", crate::agent_program::SKILL),
    ] {
        for (n, block) in text.split("```js\n").skip(1).enumerate() {
            let original = block.split("```").next().unwrap().to_owned();
            let (source, input, fragment) = if original.contains("function compose(") {
                (
                    original,
                    serde_json::from_str(include_str!(
                        "../../skills/task-program/examples/serial.input.json"
                    ))
                    .unwrap(),
                    None,
                )
            } else if original.contains("function makePages(") {
                (
                    original,
                    serde_json::from_str(include_str!(
                        "../../skills/task-program/examples/parallel-five-pages.input.json"
                    ))
                    .unwrap(),
                    None,
                )
            } else if original.contains("function process(") {
                ("function main(request){return process(request.shared,request.part);}\n/** @virtual */\nfunction process(shared,part){/* Implement the assigned transformation. */}".into(),
                 json!({"shared":{"rule":"转换"},"part":{"source":"source.txt","output":"transformed.txt"}}), Some(original))
            } else if original.contains("function produce(") {
                (
                    original,
                    json!({"output":"artifact.txt","requirements":"制作真实产物并验收"}),
                    None,
                )
            } else if original.contains("function prepare(") {
                (original, json!({"requirements":"两路并行制作与验收"}), None)
            } else {
                assert!(
                    original.contains("const actual = io.read"),
                    "unclassified published example: {original}"
                );
                (original, json!({"source":"source.txt"}), None)
            };
            cases.push(Example {
                name: format!("{name}-{}", n + 1),
                source,
                input,
                fragment,
            });
        }
    }
    // The copies embedded in the Skill must be exactly the shipped files.
    for file in [
        include_str!("../../skills/task-program/examples/serial.js"),
        include_str!("../../skills/task-program/examples/parallel-five-pages.js"),
    ] {
        assert!(crate::agent_program::SKILL.contains(file.trim()));
    }
    assert_eq!(
        cases.len(),
        7,
        "classify and execute every published JS block"
    );
    cases
}

fn tool(id: &str, name: &str, args: Value) -> Value {
    json!({"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
}
fn answer(value: Value) -> Value {
    json!({"role":"assistant","content":value.to_string()})
}

fn scripted(
    body: &Value,
    root: &Path,
    fragment: Option<&str>,
    rejections: usize,
    inspections: &AtomicUsize,
) -> Value {
    let messages = body["messages"].as_array().unwrap();
    let task: Value = serde_json::from_str(
        messages
            .iter()
            .find(|m| m["name"] == "program_task_input")
            .expect("submitted programs must not ask the model to replan main")["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let name = task["name"].as_str().unwrap();
    let input = &task["bindings"];
    match name {
        "compose" | "designPage" => {
            let title = if name == "compose" {
                &input["request"]["topic"]
            } else {
                &input["page"]["title"]
            };
            if name == "designPage" {
                assert!(
                    input["shared"]["source_content"]
                        .as_str()
                        .unwrap()
                        .contains("SOURCE")
                );
            }
            answer(
                json!({"content":format!("<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><title>{0}</title></head><body><h1>{0}</h1><p>实际测试产物</p></body></html>",title.as_str().unwrap())}),
            )
        }
        "transform" => {
            answer(json!({"content":format!("{} 转换",input["content"].as_str().unwrap())}))
        }
        "prepare" => answer(
            json!({"shared":{"requirements":"SHARED"},"left":{"path":"left.txt"},"right":{"path":"right.txt"}}),
        ),
        "process" => tool(
            "refine",
            "define_task_body",
            json!({"source":fragment.unwrap()}),
        ),
        "produce" | "repair" | "work" => {
            let (params, path) = match name {
                "produce" => ("request", "request.output"),
                "work" => ("shared, assignment", "assignment.path"),
                _ if input.get("artifact").is_some() => {
                    ("artifact, check, request", "artifact.path")
                }
                _ => ("output, check, request", "output.path"),
            };
            let write = if name == "repair" {
                format!(
                    "const old=io.read({{path:{path}}});io.write({{path:{path},content:'ACTUAL_ARTIFACT',expected_code_hash:old.code_hash}});"
                )
            } else {
                format!("io.write({{path:{path},content:'ACTUAL_ARTIFACT'}});")
            };
            tool(
                "implement",
                "define_task_body",
                json!({"source":format!("function {name}({params}){{{write}return {{path:{path}}};}} ")}),
            )
        }
        "inspect" | "inspectPage" | "deliver" => {
            let path = input["page"]["path"]
                .as_str()
                .or_else(|| input["request"]["output"].as_str())
                .or_else(|| input["artifact"]["path"].as_str())
                .or_else(|| input["output"]["path"].as_str())
                .unwrap();
            let content = if let Some(content) = input["content"].as_str() {
                assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), content);
                content.to_owned()
            } else if let Some(result) = messages
                .iter()
                .rev()
                .find(|m| m["tool_call_id"] == "inspect-file")
            {
                let result: Value =
                    serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
                let content = result["content"].as_str().unwrap();
                assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), content);
                content.to_owned()
            } else {
                return tool(
                    "inspect-file",
                    "invoke_library",
                    json!({"method":"io.read","args":{"path":path}}),
                );
            };
            assert!(!content.is_empty());
            let passed =
                name == "deliver" || inspections.fetch_add(1, Ordering::SeqCst) >= rejections;
            answer(
                json!({"passed":passed,"path":path,"issues":if !passed{vec!["fixture rejection"]}else{vec![]},"evidence":[content]}),
            )
        }
        _ => panic!("unexpected published function {name}"),
    }
}

async fn seed(state: &AgentServiceState, case: &Example, model: &str) -> Result<()> {
    let mut conn = crate::collaboration_jobs::db(state.workspace.root())?;
    conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('example','Run the published example',?1,'running',0,0)",[model])?;
    let tx = conn.transaction()?;
    crate::task_tree::ensure_root(&tx, "example", 1)?;
    tx.commit()?;
    let cancel = CancellationToken::new();
    let session = Session {
        agent_id: None,
        task_node_id: None,
        step_node_id: None,
        plugin: None,
        state,
        task: "example",
        model,
        turn: 1,
        cancel: &cancel,
        ban_execution: false,
        scope: "main".into(),
        parent: None,
        input_budget: crate::model_settings::input_budget(state, model),
    };
    let mut window = Window {
        messages: vec![
            json!({"role":"system","content":MAIN_PROMPT}),
            json!({"role":"user","content":"Run the exact published program, including its checks. Two Agents are authorized where the example uses fork."}),
        ],
        ..Default::default()
    };
    let call = crate::model_stream::ToolCall {
        call_id: "example-start".into(),
        name: "create_task".into(),
        arguments: json!({"goal":"Execute and verify the published example"}).to_string(),
    };
    window.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}));
    task_frames::create_task(&session, &mut window, &call, "start", 1).await?;
    program_flow::install_scoped(
        &session,
        &mut window,
        &json!({"source":case.source,"input":case.input}),
        false,
    )?;
    save(&session, &window).await
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().into())
}
fn init_git(root: &Path) -> Result<()> {
    git(root, &["init", "-b", "main"])?;
    git(root, &["config", "core.autocrlf", "false"])?;
    std::fs::write(root.join(".gitignore"), "/.codex-workspace-mcp/\n/logs/\n")?;
    git(root, &["add", "-A"])?;
    git(
        root,
        &[
            "-c",
            "user.name=Example Test",
            "-c",
            "user.email=test@local",
            "commit",
            "-m",
            "Example baseline",
        ],
    )?;
    Ok(())
}
async fn run_scripted(case: Example, rejections: usize, empty: bool) -> Result<()> {
    run_scripted_workspace(case, rejections, empty, false).await
}
async fn run_scripted_workspace(
    case: Example,
    rejections: usize,
    empty: bool,
    git_workspace: bool,
) -> Result<()> {
    let root = std::env::temp_dir().join(format!(
        "documented-{}-{}",
        case.name,
        agent_service::uuid_like()
    ));
    std::fs::create_dir_all(root.join("docs"))?;
    std::fs::write(root.join("source.txt"), if empty { "" } else { "SOURCE" })?;
    std::fs::write(root.join("docs/generic-agent-design.md"), "SOURCE")?;
    if git_workspace {
        init_git(&root)?;
    }
    let path = root.clone();
    let fragment = case.fragment.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let inspections = Arc::new(AtomicUsize::new(0));
    let checks = inspections.clone();
    let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>|{
        count.fetch_add(1,Ordering::SeqCst);
        let message=scripted(&body,&path,fragment.as_deref(),rejections,&checks);
        async move {Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))}
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let state = agent_service::tests::test_state(&root, listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    seed(&state, &case, "fake").await?;
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        run_task(
            state,
            "example".into(),
            "fake".into(),
            "Execute the submitted example. Two Agents are authorized where fork is used.".into(),
            40,
            CancellationToken::new(),
            1,
            false,
        ),
    )
    .await?;
    server.abort();
    let conn = agent_service::open_db(&root)?;
    let status: String = conn.query_row(
        "SELECT status FROM agent_tasks WHERE id='example'",
        [],
        |r| r.get(0),
    )?;
    if rejections == usize::MAX {
        ensure!(
            status == "failed",
            "{} delivered after a rejected review: {status}",
            case.name
        );
        let error:String=conn.query_row("SELECT data FROM agent_task_events WHERE kind='program/error' ORDER BY seq DESC LIMIT 1",[],|r|r.get(0))?;
        ensure!(
            error.contains("验收") || error.contains("检查未通过"),
            "{} must stop for its actual failed review: {error}",
            case.name
        );
        let delivered:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='task/node_started' AND json_extract(data,'$.goal')='deliver'",[],|r|r.get(0))?;
        ensure!(delivered == 0, "delivery must not run after rejection");
    } else {
        outcome?;
        let failure:Option<String>=conn.query_row("SELECT data FROM agent_task_events WHERE kind='program/error' ORDER BY seq DESC LIMIT 1",[],|r|r.get(0)).optional()?;
        ensure!(
            status == "completed",
            "{}: {status}: {}",
            case.name,
            failure.as_deref().unwrap_or("no program error")
        );
        let raw: String = conn.query_row(
            "SELECT state FROM agent_runtime_windows WHERE task_id='example' AND scope='main'",
            [],
            |r| r.get(0),
        )?;
        let output: Value = serde_json::from_str::<Value>(&raw)?["program_value"].clone();
        if case.source.contains("function makePages(") {
            let pages = case.input["pages"].as_array().unwrap();
            ensure!(output.is_null() && pages.len() == 5, "five pages required");
            for file in pages {
                ensure!(
                    std::fs::read_to_string(root.join(file["path"].as_str().unwrap()))?
                        .contains(file["title"].as_str().unwrap())
                );
            }
        } else if case.source.contains("function compose(") {
            ensure!(
                output.is_null() && root.join(case.input["output"].as_str().unwrap()).is_file()
            );
        } else if case.source.contains("function prepare(") {
            ensure!(
                output["outputs"].as_array().unwrap().len() == 2
                    && output["checks"].as_array().unwrap().len() == 2
            );
        } else if case.fragment.is_some() {
            ensure!(std::fs::read_to_string(root.join("transformed.txt"))? == "SOURCE 转换");
        } else if case.source.contains("function produce(") {
            ensure!((output.is_null() || output["passed"] == true) && root.join("artifact.txt").is_file());
            let deliveries:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='task/node_started' AND json_extract(data,'$.goal')='deliver'",[],|r|r.get(0))?;
            ensure!(deliveries == 1, "a passed review must deliver exactly once");
        } else {
            ensure!(output["present"] == !empty);
        }
    }
    let count: i64 = conn.query_row("SELECT count(*) FROM agent_delegates", [], |r| r.get(0))?;
    ensure!(
        count == if case.source.contains("fork(") { 2 } else { 0 },
        "{}: unexpected delegate count {count}",
        case.name
    );
    if git_workspace {
        let mut statement = conn.prepare("SELECT id,specification,result FROM agent_delegates")?;
        let children = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, spec, result) in children {
            let spec: Value = serde_json::from_str(&spec)?;
            let result: Value = serde_json::from_str(&result)?;
            ensure!(
                spec["git"]["worktree"]
                    .as_str()
                    .is_some_and(|p| Path::new(p) != root)
            );
            ensure!(
                crate::delegate_git::is_merged(
                    &root,
                    &spec,
                    result["git"]["head"].as_str().unwrap()
                )?,
                "child {id} must be merged"
            );
        }
        for name in ["inspect_agent_changes", "merge_agent_result"] {
            let n:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')=?1",[name],|r|r.get(0))?;
            ensure!(n == count, "one {name} per isolated branch required");
        }
    }
    if rejections == 1 {
        ensure!(
            inspections.load(Ordering::SeqCst) == 2,
            "the repaired artifact must be inspected again"
        );
        let writes:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='write_file'",[],|r|r.get(0))?;
        ensure!(
            writes == 2,
            "one initial write and one actual repair required"
        );
    }
    ensure!(std::fs::read_to_string(root.join("source.txt"))? == if empty { "" } else { "SOURCE" });
    eprintln!(
        "DOCUMENTED_EXAMPLE {} rejections={rejections} empty={empty} status={status} requests={} workspace={}",
        case.name,
        requests.load(Ordering::SeqCst),
        root.display()
    );
    Ok(())
}

#[tokio::test]
async fn all_published_javascript_blocks_run_through_actual_host_tools() -> Result<()> {
    for case in examples() {
        run_scripted(case, 0, false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn published_examples_stop_on_failed_reviews_and_cover_the_empty_read() -> Result<()> {
    for case in examples().into_iter().filter(|c| c.fragment.is_none()) {
        if case.source.contains("const actual = io.read")
            && !case.source.contains("function inspect(")
            && !case.source.contains("function inspectPage(")
        {
            run_scripted(case, 0, true).await?;
        } else {
            run_scripted(case, usize::MAX, false).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn both_published_repair_examples_reinspect_before_delivery() -> Result<()> {
    for case in examples()
        .into_iter()
        .filter(|c| c.source.contains("function produce("))
    {
        run_scripted(case, 1, false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn published_parallel_examples_integrate_real_git_worktrees_before_readback() -> Result<()> {
    for case in examples()
        .into_iter()
        .filter(|c| c.source.contains("fork("))
    {
        run_scripted_workspace(case, 0, false, true).await?;
    }
    Ok(())
}

#[tokio::test]
async fn skipping_git_integration_reproduces_the_missing_parent_file() -> Result<()> {
    let mut case = examples()
        .into_iter()
        .find(|c| c.source.contains("function makePages("))
        .unwrap();
    case.source = case
        .source
        .replace("integrate(a);\n", "")
        .replace("integrate(b);\n", "");
    let error = run_scripted_workspace(case, 0, false, true)
        .await
        .unwrap_err();
    ensure!(
        format!("{error:#}").contains("path does not exist"),
        "must reproduce the actual missing-file failure: {error:#}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "real configured model; isolated workspaces; consumes provider tokens"]
async fn documented_file_examples_with_configured_model() -> Result<()> {
    // Runtime workspaces must be outside this Git checkout: fork correctly
    // refuses to use an arbitrary descendant of a repository as its root.
    let batch = std::env::temp_dir().join(format!(
        "documented-examples-real-{}",
        agent_service::uuid_like()
    ));
    std::fs::create_dir_all(&batch)?;
    std::fs::write(
        latest_run_marker("documented-examples-latest"),
        batch.to_string_lossy().as_bytes(),
    )?;
    let mut failures = Vec::new();
    for case in examples().into_iter().filter(|c| {
        c.source.contains("function compose(") || c.source.contains("function makePages(")
    }) {
        let root = batch.join(&case.name);
        std::fs::create_dir_all(root.join("docs"))?;
        let original = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/generic-agent-design.md"),
        )?;
        std::fs::write(root.join("docs/generic-agent-design.md"), &original)?;
        std::fs::write(root.join("example.js"), &case.source)?;
        std::fs::write(
            root.join("input.json"),
            serde_json::to_vec_pretty(&case.input)?,
        )?;
        let mut state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse()?);
        let config = crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
        crate::ai_proxy::apply_agent_config(&mut state, &config);
        let model = state
            .model_map
            .get(&state.default_model)
            .cloned()
            .unwrap_or_else(|| state.default_model.clone());
        ensure!(
            !state.provider_url.is_empty() && !state.api_key.is_empty(),
            "configured model unavailable"
        );
        seed(&state, &case, &model).await?;
        eprintln!(
            "REAL_DOCUMENTED_START {} model={model} root={}",
            case.name,
            root.display()
        );
        let cancel = CancellationToken::new();
        let start = std::time::Instant::now();
        let mut run = Box::pin(run_task(
            state,
            "example".into(),
            model.clone(),
            "执行已经提交的示例程序及其检查。两 Agent 并发仅在该示例中已写 fork 时获授权。".into(),
            80,
            cancel.clone(),
            1,
            false,
        ));
        let conn = agent_service::open_db(&root)?;
        let mut last = 0i64;
        let mut stop = None;
        let mut signatures = BTreeMap::<String, usize>::new();
        let mut compile_errors = BTreeMap::<String, usize>::new();
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        let outcome = loop {
            tokio::select! {
                result=&mut run=>break result,
                _=interval.tick()=>{
                    if start.elapsed().as_secs()>=300 {stop=Some("five minute deadline".into());}
                    let mut query=conn.prepare("SELECT seq,kind,data FROM agent_task_events WHERE task_id='example' AND seq>?1 ORDER BY seq")?;
                    let events=query.query_map([last],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                    for (seq,mut kind,raw) in events {
                        last=seq;let raw:Value=serde_json::from_str(&raw)?;
                        let actor=raw["agent_id"].as_str().unwrap_or("main");
                        let event=if kind=="subagent/event" {kind=raw["event_type"].as_str().unwrap_or("").into();&raw["event"]}else{&raw};
                        if kind=="program/error" || kind=="turn/error" || kind=="tool/result" && event["message"]["isError"]==true {
                            if event["meta"]["result"]["code"]=="PROGRAM_SOURCE_INVALID" {
                                let count=compile_errors.entry(actor.into()).or_default();*count+=1;
                                if *count>3 {stop=Some(format!("source repair limit exceeded for {actor}"));}
                            } else {stop=Some(format!("first execution error at {seq}: {}",event.to_string().chars().take(1600).collect::<String>()));}
                        }
                        if kind=="tool/call" {
                            let name=event["name"].as_str().unwrap_or("");
                            let signature=format!("{actor}:{name}:{}",event["arguments"]);
                            if !matches!(name,"join_agents"|"wait_agents") {
                                let count=signatures.entry(signature).or_default();*count+=1;
                                if *count>=3 {stop=Some(format!("repeated operation: {actor} {name}"));}
                            }
                            eprintln!("REAL_DOCUMENTED_CALL {} {actor} {name}",case.name);
                        }
                    }
                    if stop.is_some() {
                        cancel.cancel();
                        break match tokio::time::timeout(Duration::from_secs(8),&mut run).await {Ok(result)=>result,Err(_)=>Err(anyhow::anyhow!("stopped by watcher"))};
                    }
                }
            }
        };
        drop(run);
        let status: String = conn.query_row(
            "SELECT status FROM agent_tasks WHERE id='example'",
            [],
            |r| r.get(0),
        )?;
        let has_requests:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_request_contexts')",[],|r|r.get(0))?;
        let requests: i64 = if has_requests {
            conn.query_row("SELECT count(*) FROM agent_request_contexts", [], |r| {
                r.get(0)
            })?
        } else {
            0
        };
        let delegates = conn
            .prepare("SELECT status FROM agent_delegates")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let paths = if let Some(pages) = case.input["pages"].as_array() {
            pages
                .iter()
                .map(|p| p["path"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        } else {
            vec![case.input["output"].as_str().unwrap().to_owned()]
        };
        let files = paths
            .iter()
            .filter(|p| root.join(p).is_file())
            .cloned()
            .collect::<Vec<_>>();
        let raw: String = conn.query_row(
            "SELECT state FROM agent_runtime_windows WHERE task_id='example' AND scope='main'",
            [],
            |r| r.get(0),
        )?;
        let result = serde_json::from_str::<Value>(&raw)?["program_value"].clone();
        let checks = if paths.len() == 1 {
            result["review"]["passed"] == true
        } else {
            result["reviews"]
                .as_array()
                .is_some_and(|r| r.len() == 5 && r.iter().all(|r| r["passed"] == true))
        };
        let unchanged = std::fs::read(root.join("docs/generic-agent-design.md"))? == original
            && std::fs::read_to_string(root.join("example.js"))? == case.source;
        let passed = outcome.is_ok()
            && stop.is_none()
            && status == "completed"
            && files.len() == paths.len()
            && checks
            && unchanged
            && delegates.len() == if paths.len() == 1 { 0 } else { 2 }
            && delegates.iter().all(|s| s == "completed");
        let report = json!({"case":case.name,"model":model,"passed":passed,"status":status,"seconds":start.elapsed().as_secs_f64(),"model_requests":requests,"files":files,"expected_paths":paths,"delegates":delegates,"checks_passed":checks,"source_unchanged":unchanged,"stop_reason":stop,"outcome":outcome.map(|_|"ok".to_owned()).unwrap_or_else(|e|format!("{e:#}")),"compile_errors":compile_errors,"result":result});
        std::fs::write(
            root.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        eprintln!(
            "REAL_DOCUMENTED_REPORT {}",
            json!({"case":case.name,"passed":passed,"seconds":report["seconds"],"model_requests":requests,"files":report["files"],"stop_reason":report["stop_reason"]})
        );
        if !passed {
            failures.push(case.name);
        }
    }
    ensure!(
        failures.is_empty(),
        "documented examples failed: {failures:?}; evidence {}",
        batch.display()
    );
    Ok(())
}

fn generated_outputs(root: &Path) -> Result<(Vec<String>, Vec<String>)> {
    fn walk(root: &Path, dir: &Path, html: &mut Vec<String>, css: &mut Vec<String>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if [".git", ".codex-workspace-mcp", "logs"]
                .iter()
                .any(|skip| name == *skip)
            {
                continue;
            }
            if entry.file_type()?.is_dir() {
                walk(root, &path, html, css)?;
            } else if path.extension().is_some_and(|x| x == "html") {
                html.push(path.strip_prefix(root)?.to_string_lossy().into_owned());
            } else if name == "shared.css" {
                css.push(path.strip_prefix(root)?.to_string_lossy().into_owned());
            }
        }
        Ok(())
    }
    let (mut html, mut css) = (Vec::new(), Vec::new());
    walk(root, root, &mut html, &mut css)?;
    html.sort();
    css.sort();
    Ok((html, css))
}

fn missing_local_links(root: &Path, files: &[String]) -> Result<Vec<Value>> {
    let href = regex::Regex::new(r#"(?i)\bhref\s*=\s*[\"']([^\"']+)[\"']"#)?;
    let mut missing = Vec::new();
    for file in files {
        let full = root.join(file);
        let content = std::fs::read_to_string(&full)?;
        for found in href.captures_iter(&content) {
            let link = &found[1];
            let path = link.split(['#', '?']).next().unwrap_or("");
            if path.is_empty() || path.contains(':') || path.starts_with('/') {
                continue;
            }
            if !full.parent().unwrap_or(root).join(path).exists() {
                missing.push(json!({"page":file,"href":link}));
            }
        }
    }
    Ok(missing)
}

#[test]
fn local_navigation_check_reports_missing_files() {
    let root =
        std::env::temp_dir().join(format!("navigation-check-{}", agent_service::uuid_like()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("present.html"), "ok").unwrap();
    std::fs::write(
        root.join("index.html"),
        "<a href='./present.html#part'>ok</a><a href='missing.html'>broken</a>",
    )
    .unwrap();
    let missing = missing_local_links(&root, &["index.html".into()]).unwrap();
    assert_eq!(
        missing,
        vec![json!({"page":"index.html","href":"missing.html"})]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "real Luna; clean isolated Git repository; consumes provider tokens"]
async fn real_luna_five_pages_repairs_injected_fault() -> Result<()> {
    run_luna_five_pages(true, false).await
}

#[tokio::test]
#[ignore = "real Luna; clean isolated Git repository; consumes provider tokens"]
async fn real_luna_five_pages_inherits_scope_snapshot() -> Result<()> {
    run_luna_five_pages(false, false).await
}

#[tokio::test]
#[ignore = "real Luna; natural five-page task; consumes provider tokens"]
async fn real_luna_five_pages_simple_review() -> Result<()> {
    run_luna_five_pages(false, true).await
}

async fn run_luna_five_pages(inject_fault: bool, simple: bool) -> Result<()> {
    let evidence = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("runs")
        .join(format!(
            "luna-five-{}-{}",
            if simple {
                "simple"
            } else if inject_fault {
                "repair"
            } else {
                "scope"
            },
            agent_service::uuid_like()
        ));
    let root = evidence.join("project");
    std::fs::create_dir_all(root.join("docs"))?;
    std::fs::create_dir_all(evidence.join("flows"))?;
    std::fs::write(
        latest_run_marker("luna-five-repair-latest"),
        evidence.to_string_lossy().as_bytes(),
    )?;
    let mut materials = BTreeMap::new();
    for file in [
        "README.md",
        "docs/agent-program.md",
        "docs/generic-agent-design.md",
    ] {
        let bytes = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(file))?;
        std::fs::write(root.join(file), &bytes)?;
        materials.insert(file, bytes);
    }
    init_git(&root)?;
    if inject_fault {
        *FAULT.lock().unwrap() = Some((root.clone(), evidence.clone()));
    }
    let base = git(&root, &["rev-parse", "HEAD"])?;
    let mut state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse()?);
    let config = crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state, &config);
    let model = state
        .model_map
        .get("gpt-6-luna")
        .cloned()
        .context("configured provider must retain Luna")?;
    state.default_model = model.clone();
    state.delegate_agent_settings = crate::model_settings::ModelSettings {
        provider: Some(state.provider_name.clone()),
        model: Some(model.clone()),
        ..Default::default()
    };
    let prompt = if simple {
        "请根据本工作区 README.md、docs/agent-program.md 和 docs/generic-agent-design.md，制作5个中文HTML页面，介绍本项目通用Agent的设计。使用一个主Agent和一个子Agent并行完成，主Agent整合产物。验收时请让同一个 AI 虚函数在自己的上下文里逐步检查实际产物，发现问题自行处理并复查，再返回结果。不要修改源文档；未经浏览器渲染不能声称视觉验收通过。"
    } else {
        "请根据本工作区 README.md、docs/agent-program.md 和 docs/generic-agent-design.md，制作5个独立中文HTML页面，介绍本项目通用Agent的设计。每页一个HTML文件，使用同一个shared.css，页面之间可以导航，无远程资源，兼顾桌面与手机阅读。请由主Agent和一个子Agent并行完成，最后由主Agent整合全部文件并逐页读回验收。不要修改源文档；未经浏览器渲染不能声称视觉验收通过。"
    };
    let conn = crate::collaboration_jobs::db(&root)?;
    conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('ten-pages',?1,?2,'running',0,0)",params![prompt,model])?;
    std::fs::write(
        evidence.join("manifest.json"),
        serde_json::to_vec_pretty(
            &json!({"model":model,"delegate_model":model,"prompt":prompt,"base":base,"intermediate_user_messages":0,"fault_injection":inject_fault}),
        )?,
    )?;
    eprintln!("LUNA_TEN_START {} model={model}", evidence.display());
    let cancel = CancellationToken::new();
    let start = std::time::Instant::now();
    let mut run = Box::pin(run_task(
        state,
        "ten-pages".into(),
        model.clone(),
        prompt.into(),
        80,
        cancel.clone(),
        1,
        false,
    ));
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    let mut last = 0i64;
    let mut stop: Option<String> = None;
    let mut compile_errors = BTreeMap::<String, usize>::new();
    let outcome = loop {
        tokio::select! {
            result=&mut run=>break result,
            _=interval.tick()=>{
                if start.elapsed().as_secs()>=600 {stop=Some("ten minute deadline".into());}
                let mut query=conn.prepare("SELECT seq,kind,data FROM agent_task_events WHERE task_id='ten-pages' AND seq>?1 ORDER BY seq")?;
                let events=query.query_map([last],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for (seq,mut kind,raw) in events {
                    last=seq;let raw:Value=serde_json::from_str(&raw)?;
                    let actor=raw["agent_id"].as_str().unwrap_or("main");
                    let event=if kind=="subagent/event" {kind=raw["event_type"].as_str().unwrap_or("").into();&raw["event"]}else{&raw};
                    if kind=="program/error" || kind=="turn/error" || kind=="tool/result" && event["message"]["isError"]==true {
                        if event["meta"]["result"]["code"]=="PROGRAM_SOURCE_INVALID" {
                            let count=compile_errors.entry(actor.into()).or_default();*count+=1;
                        }
                    }
                    if kind=="tool/call" {
                        let name=event["name"].as_str().unwrap_or("");
                        let args=event["arguments"].as_str().and_then(|s|serde_json::from_str::<Value>(s).ok()).unwrap_or_else(||event["arguments"].clone());
                        if matches!(name,"execute_program"|"define_task_body") {
                            if let Some(source)=args["source"].as_str(){std::fs::write(evidence.join("flows").join(format!("{seq}-{name}.js")),source)?;}
                        }
                        eprintln!("LUNA_TEN_CALL {actor} {name}");
                    }
                }
                if stop.is_some() {
                    cancel.cancel();
                    break match tokio::time::timeout(Duration::from_secs(8),&mut run).await{Ok(result)=>result,Err(_)=>Err(anyhow::anyhow!("stopped by watcher"))};
                }
            }
        }
    };
    drop(run);
    let status: String = conn.query_row(
        "SELECT status FROM agent_tasks WHERE id='ten-pages'",
        [],
        |r| r.get(0),
    )?;
    let requests: i64 = conn.query_row("SELECT count(*) FROM agent_request_contexts", [], |r| {
        r.get(0)
    })?;
    let (files, styles) = generated_outputs(&root)?;
    let missing_links = missing_local_links(&root, &files)?;
    let mut delegates = Vec::new();
    for row in conn
        .prepare("SELECT id,status,specification,result FROM agent_delegates")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
    {
        let (id, status, spec, result) = row?;
        let spec: Value = serde_json::from_str(&spec)?;
        let result: Value = result
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or(Value::Null);
        let merged = if let Some(head) = result["git"]["head"].as_str() {
            crate::delegate_git::is_merged(&root, &spec, head)?
        } else {
            false
        };
        delegates.push(json!({"id":id,"status":status,"git":result["git"],"merged":merged}));
    }
    let mut integrations = Vec::new();
    for name in ["inspect_agent_changes", "merge_agent_result"] {
        let n:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')=?1",[name],|r|r.get(0))?;
        integrations.push(json!({"name":name,"calls":n}));
    }
    let unchanged = materials
        .iter()
        .all(|(path, bytes)| std::fs::read(root.join(path)).ok().as_ref() == Some(bytes));
    let fault: Value = std::fs::read(evidence.join("fault.json"))
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or(Value::Null);
    let final_content = fault["path"]
        .as_str()
        .and_then(|p| std::fs::read_to_string(root.join(p)).ok());
    if let Some(content) = &final_content {
        std::fs::write(evidence.join("after-run.html"), content)?;
    }
    let repaired = final_content
        .as_ref()
        .is_some_and(|s| s.contains("shared.css") && !s.contains("missing-local-style.css"));
    let passed = outcome.is_ok()
        && stop.is_none()
        && status == "completed"
        && files.len() == 5
        && (!inject_fault || repaired)
        && missing_links.is_empty()
        && (simple || styles.len() == 1)
        && unchanged
        && (simple || delegates.len() == 1)
        && delegates
            .iter()
            .all(|d| d["status"] == "completed" && d["merged"] == true)
        && (simple
            || integrations
                .iter()
                .all(|r| r["calls"].as_i64().unwrap_or(0) >= 1));
    let report = json!({"passed":passed,"fault_injection":inject_fault,"fault":fault,"fault_repaired_in_file":repaired,"missing_local_links":missing_links,"model":model,"status":status,"seconds":start.elapsed().as_secs_f64(),"model_requests":requests,"files":files,"shared_css":styles,"delegates":delegates,"integrations":integrations,"source_unchanged":unchanged,"stop_reason":stop,"compile_errors":compile_errors,"outcome":outcome.map(|_|"ok".to_owned()).unwrap_or_else(|e|format!("{e:#}"))});
    std::fs::write(
        evidence.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    eprintln!("LUNA_TEN_REPORT {report}");
    ensure!(
        passed,
        "Luna ten-page Git acceptance failed; evidence {}",
        evidence.display()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "real Luna acceptance of existing five pages; consumes provider tokens"]
async fn real_luna_existing_five_page_acceptance() -> Result<()> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    let evidence = base.join("runs").join(format!("luna-global-five-acceptance-{}", agent_service::uuid_like()));
    let root = evidence.join("project");
    std::fs::create_dir_all(root.join("runs/acceptance-five-ai-review"))?;
    std::fs::create_dir_all(root.join("docs"))?;
    let pages = ["01-overview.html", "02-program.html", "03-tools.html", "04-agents.html", "05-quality.html"];
    for name in pages {
        std::fs::copy(base.join("runs/acceptance-five-ai-review").join(name), root.join("runs/acceptance-five-ai-review").join(name))?;
    }
    for name in ["README.md", "docs/agent-program.md", "docs/generic-agent-design.md"] {
        std::fs::copy(base.join(name), root.join(name))?;
    }
    std::fs::write(latest_run_marker("luna-global-five-acceptance-latest"), evidence.to_string_lossy().as_bytes())?;
    let mut state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse()?);
    let config = crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state, &config);
    let model = state.model_map.get("gpt-6-luna").cloned().context("configured provider must retain Luna")?;
    state.default_model = model.clone();
    let prompt = "runs/acceptance-five-ai-review 里已有的五个中文 HTML 页面是根据工作区项目文档制作的项目介绍。请先弄清这个项目要表达什么，再从整个五页作品是否达到介绍项目的目标出发，结合实际页面验收，检查整体和各部分是否存在问题。请把结论、依据、发现的问题和未检查的事项写入 acceptance-report.md；不要修改原有页面。";
    let conn = agent_service::open_db(&root)?;
    conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('acceptance',?1,?2,'running',0,0)",params![prompt,model])?;
    std::fs::write(evidence.join("manifest.json"),serde_json::to_vec_pretty(&json!({"model":model,"prompt":prompt,"original_pages":"runs/acceptance-five-ai-review","injected_source":false,"intermediate_user_messages":0}))?)?;
    eprintln!("REAL_ACCEPTANCE_START {} model={model}", evidence.display());
    let cancel = CancellationToken::new();
    let start = std::time::Instant::now();
    let mut run = Box::pin(run_task(state,"acceptance".into(),model.clone(),prompt.into(),80,cancel.clone(),1,false));
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut last = 0i64;
    let outcome = loop {
        tokio::select! {
            result=&mut run=>break result,
            _=interval.tick()=>{
                let mut query=conn.prepare("SELECT seq,kind,data FROM agent_task_events WHERE task_id='acceptance' AND seq>?1 ORDER BY seq")?;
                let events=query.query_map([last],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for (seq,kind,raw) in events {
                    last=seq;
                    let event:Value=serde_json::from_str(&raw)?;
                    if kind=="tool/call" {
                        let name=event["name"].as_str().unwrap_or("");
                        if matches!(name,"execute_program"|"define_task_body") {
                            let args=event["arguments"].as_str().and_then(|v|serde_json::from_str::<Value>(v).ok()).unwrap_or_else(||event["arguments"].clone());
                            if let Some(source)=args["source"].as_str(){std::fs::write(evidence.join(format!("{seq}-{name}.js")),source)?;}
                        }
                        eprintln!("REAL_ACCEPTANCE_CALL {seq} {name}");
                    }
                    if kind=="program/error" || kind=="turn/error" { eprintln!("REAL_ACCEPTANCE_ERROR {seq} {}",event.to_string().chars().take(1200).collect::<String>()); }
                }
                if start.elapsed().as_secs()>=600 {cancel.cancel();break Err(anyhow::anyhow!("acceptance deadline"));}
            }
        }
    };
    drop(run);
    let status:String=conn.query_row("SELECT status FROM agent_tasks WHERE id='acceptance'",[],|r|r.get(0))?;
    let requests:i64=conn.query_row("SELECT count(*) FROM agent_request_contexts WHERE task_id='acceptance'",[],|r|r.get(0))?;
    let files = pages.iter().map(|x|format!("runs/acceptance-five-ai-review/{x}")).collect::<Vec<_>>();
    let missing_links=missing_local_links(&root,&files)?;
    let unchanged=pages.iter().all(|name|std::fs::read(root.join("runs/acceptance-five-ai-review").join(name)).ok()==std::fs::read(base.join("runs/acceptance-five-ai-review").join(name)).ok());
    let result=json!({"model":model,"status":status,"seconds":start.elapsed().as_secs_f64(),"model_requests":requests,"outcome":outcome.as_ref().map(|_|"ok".to_owned()).unwrap_or_else(|e|format!("{e:#}")),"report_exists":root.join("acceptance-report.md").is_file(),"pages_unchanged":unchanged,"independent_missing_links":missing_links});
    std::fs::write(evidence.join("run-report.json"),serde_json::to_vec_pretty(&result)?)?;
    eprintln!("REAL_ACCEPTANCE_REPORT {result}");
    ensure!(outcome.is_ok() && status=="completed" && unchanged && root.join("acceptance-report.md").is_file(),"acceptance did not complete; evidence {}",evidence.display());
    Ok(())
}

#[tokio::test]
#[ignore = "real Luna direct script generation and execution; consumes provider tokens"]
async fn real_luna_direct_javascript_five_page_acceptance() -> Result<()> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    let evidence = base.join("runs").join(format!("luna-direct-js-five-{}", agent_service::uuid_like()));
    let root = evidence.join("project");
    std::fs::create_dir_all(root.join("runs/acceptance-five-ai-review"))?;
    std::fs::create_dir_all(root.join("docs"))?;
    let pages = ["01-overview.html", "02-program.html", "03-tools.html", "04-agents.html", "05-quality.html"];
    for name in pages { std::fs::copy(base.join("runs/acceptance-five-ai-review").join(name),root.join("runs/acceptance-five-ai-review").join(name))?; }
    for name in ["README.md", "docs/agent-program.md", "docs/generic-agent-design.md"] { std::fs::copy(base.join(name),root.join(name))?; }
    std::fs::write(latest_run_marker("luna-direct-js-five-latest"), evidence.to_string_lossy().as_bytes())?;
    let mut state=agent_service::tests::test_state(&root,"127.0.0.1:1".parse()?);
    let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=state.model_map.get("gpt-6-luna").cloned().context("configured provider must retain Luna")?;
    let definitions=agent_service::local_tools(&state.workspace,state.tool_catalog.as_deref(),false);
    let io=crate::agent_program::libraries::builtin();
    let headers=crate::agent_program::declarations::headers(&io,&definitions);
    let prompt="请写一个 JS 脚本完成以下任务：runs/acceptance-five-ai-review 里的五个中文 HTML 页面是根据工作区项目文档制作的项目介绍。先弄清这个项目要表达什么，再从整个五页作品是否达到介绍项目的目标出发，结合实际页面验收，检查整体和各部分是否存在问题。把结论、依据、发现的问题和未检查的事项写入 acceptance-report.md；不要修改原有页面。";
    let instructions="只输出完整的 JS 源码，不要解释或代码围栏。脚本在宿主的 JS 子集运行，顶层语句顺序执行；无需 main 或顶层 return。使用 import 从 host:io 引入文件工具，参数是一个 JSON 对象。需要 AI 处理的能力声明为空的 @virtual 函数，注释说明职责与返回契约；调用它即可执行。普通代码负责流程、读取、写入和可确定的检查。每个虚函数尽量职责清晰。程序需完成用户目标。语法不支持时会收到编译错误并可以修正。";
    let mut messages=vec![json!({"role":"system","content":format!("{instructions}\n\n可用文件工具声明（只用于了解参数，不要复制类型）：\n{headers}")}),json!({"role":"user","content":prompt})];
    std::fs::write(evidence.join("manifest.json"),serde_json::to_vec_pretty(&json!({"model":model,"prompt":prompt,"instructions":instructions,"generation_uses_execute_program":false,"original_pages":"runs/acceptance-five-ai-review"}))?)?;
    eprintln!("DIRECT_JS_START {} model={model}",evidence.display());
    let start=std::time::Instant::now();
    let mut attempts=Vec::new();
    let mut compiled=None;
    for attempt in 1..=3 {
        let response=state.model_client.post(format!("{}/chat/completions",state.provider_url.trim_end_matches('/')))
            .bearer_auth(&state.api_key).json(&json!({"model":model,"messages":messages,"stream":false})).send().await?.error_for_status()?;
        let result:Value=response.json().await?;
        let raw=result["choices"][0]["message"]["content"].as_str().context("model returned no script")?.trim();
        let source=if raw.starts_with("```") { raw.split_once('\n').map(|(_,s)|s).unwrap_or(raw).trim_end_matches('`').trim() } else { raw };
        std::fs::write(evidence.join(format!("attempt-{attempt}.js")),source)?;
        let result_compile=crate::agent_program::compile_with_libraries(source,io.clone());
        let issue=result_compile.as_ref().err().map(|e|format!("{e:#}"));
        let record=json!({"attempt":attempt,"compiled":issue.is_none(),"error":issue,"usage":result["usage"]});
        eprintln!("DIRECT_JS_ATTEMPT {record}");
        attempts.push(record);
        if result_compile.is_ok() {compiled=Some(source.to_owned());break;}
        messages.push(json!({"role":"assistant","content":raw}));
        messages.push(json!({"role":"user","content":format!("编译未执行任何操作。错误：{}。请修正并重新输出完整 JS 源码。",attempts.last().unwrap()["error"].as_str().unwrap_or("unknown"))}));
    }
    std::fs::write(evidence.join("generation.json"),serde_json::to_vec_pretty(&attempts)?)?;
    let source=compiled.context("script failed compilation after three attempts")?;
    let case=Example{name:"direct-js-five".into(),source,input:Value::Null,fragment:None};
    seed(&state,&case,&model).await?;
    eprintln!("DIRECT_JS_EXECUTE");
    let outcome=tokio::time::timeout(Duration::from_secs(600),run_task(state,"example".into(),model.clone(),prompt.into(),80,CancellationToken::new(),1,false)).await;
    let conn=agent_service::open_db(&root)?;
    let status:String=conn.query_row("SELECT status FROM agent_tasks WHERE id='example'",[],|r|r.get(0))?;
    let requests:i64=conn.query_row("SELECT count(*) FROM agent_request_contexts WHERE task_id='example'",[],|r|r.get(0))?;
    let paths=pages.iter().map(|p|format!("runs/acceptance-five-ai-review/{p}")).collect::<Vec<_>>();
    let missing=missing_local_links(&root,&paths)?;
    let unchanged=pages.iter().all(|name|std::fs::read(root.join("runs/acceptance-five-ai-review").join(name)).ok()==std::fs::read(base.join("runs/acceptance-five-ai-review").join(name)).ok());
    let succeeded=matches!(&outcome,Ok(Ok(()))) && status=="completed" && root.join("acceptance-report.md").is_file() && unchanged;
    let report=json!({"model":model,"seconds":start.elapsed().as_secs_f64(),"generation_attempts":attempts,"execution_model_requests":requests,"status":status,"completed":succeeded,"report_exists":root.join("acceptance-report.md").is_file(),"pages_unchanged":unchanged,"independent_missing_links":missing,"outcome":format!("{outcome:?}")});
    std::fs::write(evidence.join("run-report.json"),serde_json::to_vec_pretty(&report)?)?;
    eprintln!("DIRECT_JS_REPORT {report}");
    ensure!(succeeded,"direct script test did not complete; evidence {}",evidence.display());
    Ok(())
}

#[tokio::test]
#[ignore = "real Luna TypeScript object-oriented design experiment; consumes provider tokens"]
async fn real_luna_typescript_oop_acceptance_design() -> Result<()> {
    let base=Path::new(env!("CARGO_MANIFEST_DIR"));
    let evidence=base.join("runs").join(format!("luna-ts-oop-five-{}",agent_service::uuid_like()));
    std::fs::create_dir_all(&evidence)?;
    std::fs::write(latest_run_marker("luna-ts-oop-five-latest"),evidence.to_string_lossy().as_bytes())?;
    let mut state=agent_service::tests::test_state(&evidence,"127.0.0.1:1".parse()?);
    let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=state.model_map.get("gpt-6-luna").cloned().context("configured provider must retain Luna")?;
    let prompt="请为一个已有的五页中文 HTML 项目写 TypeScript 验收脚本：先理解工作区 README.md、docs/agent-program.md、docs/generic-agent-design.md 的项目设计，再从整组页面是否有效介绍项目的角度，结合 runs/acceptance-five-ai-review 下五个 HTML 文件，检查整体和各页问题，并把结论、依据、问题和未检查事项写入 acceptance-report.md。不要修改页面。";
    let instruction="使用传统面向对象方式编写 TypeScript：用类组织职责、状态和流程。能用普通代码与现有工具可靠实现的步骤应写出具体实现；需要理解、判断或其他尚无法直接编程实现的能力，声明为职责内聚的抽象方法，清楚定义参数和返回类型，不必编写这些抽象方法的实现类。宿主可提供满足抽象类接口的实例。文件系统能力也可以作为接口声明。适度设计类，避免将整个流程变成一个抽象方法；检查基于真实内容和证据，准确记录无法确认的内容。请只返回完整 TypeScript 源码，不要解释或 Markdown 围栏。无需为当前特定虚拟机限制代码。";
    std::fs::write(evidence.join("request.json"),serde_json::to_vec_pretty(&json!({"model":model,"system":instruction,"user":prompt}))?)?;
    eprintln!("TS_OOP_START {} model={model}",evidence.display());
    let started=std::time::Instant::now();
    let response=state.model_client.post(format!("{}/chat/completions",state.provider_url.trim_end_matches('/'))).bearer_auth(&state.api_key).json(&json!({"model":model,"messages":[{"role":"system","content":instruction},{"role":"user","content":prompt}],"stream":false})).send().await?.error_for_status()?;
    let result:Value=response.json().await?;
    let raw=result["choices"][0]["message"]["content"].as_str().context("model returned no source")?.trim();
    let source=if raw.starts_with("```"){raw.split_once('\n').map(|(_,s)|s).unwrap_or(raw).trim_end_matches('`').trim()}else{raw};
    std::fs::write(evidence.join("acceptance.ts"),source)?;
    let map:swc_common::sync::Lrc<swc_common::SourceMap>=Default::default();
    let file=map.new_source_file(swc_common::FileName::Custom("acceptance.ts".into()).into(),source.to_owned());
    let lexer=swc_ecma_parser::lexer::Lexer::new(swc_ecma_parser::Syntax::Typescript(swc_ecma_parser::TsSyntax::default()),Default::default(),swc_ecma_parser::StringInput::from(&*file),None);
    let mut parser=swc_ecma_parser::Parser::new_from(lexer);
    let parsed=parser.parse_typescript_module();
    let errors=parser.take_errors();
    let syntax_ok=parsed.is_ok() && errors.is_empty();
    let report=json!({"model":model,"seconds":started.elapsed().as_secs_f64(),"usage":result["usage"],"typescript_syntax_ok":syntax_ok,"syntax_error":parsed.err().map(|e|format!("{:?}",e.kind())),"parser_errors":errors.iter().map(|e|format!("{:?}",e.kind())).collect::<Vec<_>>(),"executed":false});
    std::fs::write(evidence.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
    eprintln!("TS_OOP_REPORT {report}");
    Ok(())
}

#[tokio::test]
#[ignore = "real Luna minimal user-only TypeScript acceptance experiment"]
async fn real_luna_typescript_minimal_prompt() -> Result<()> {
    let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("runs").join(format!("luna-html-js-single-responsibility-{}",agent_service::uuid_like()));
    std::fs::create_dir_all(&root)?;
    let mut state=agent_service::tests::test_state(&root,"127.0.0.1:1".parse()?);
    let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=state.model_map.get("gpt-6-luna").cloned().context("configured provider must retain Luna")?;
    let prompt="请用 JavaScript 写一份过程式代码，验收我已有的五页 HTML 项目。项目根目录是 /home/goodtime/Documents/project/codex-workspace-mcp，五个页面都在 runs/acceptance-five-ai-review 目录下，分别是 01-overview.html、02-program.html、03-tools.html、04-agents.html、05-quality.html。项目设计资料是根目录下的 README.md、docs/agent-program.md 和 docs/generic-agent-design.md。

请先根据设计资料理解项目目标，再检查五页作为一组是否完整、内容是否与设计一致、各页之间是否协调；同时检查页面本地链接与资源是否有效。根据实际检查结果写一份验收报告，包含整体和逐页结论、具体问题、依据以及没有检查或无法确认的事项。不要修改已有页面。

用普通代码组织完整流程，并实现可以用代码和工具可靠完成的步骤。需要 AI 的能力用带 @virtual 注释的空函数声明，由宿主提供实现；写清职责、参数和返回类型。

按单一职责原则设计函数，保持高内聚、低耦合。每个虚函数尽量只做一件事，不要让一个虚函数承担过多不同职责；普通函数负责组合调用和具体执行。按实际需要划分，不为形式机械拆分。";
    std::fs::write(root.join("prompt.txt"),prompt)?;
    eprintln!("TS_MINIMAL_START {} model={model}",root.display());
    let started=std::time::Instant::now();
    let response=state.model_client.post(format!("{}/chat/completions",state.provider_url.trim_end_matches('/'))).bearer_auth(&state.api_key).json(&json!({"model":model,"messages":[{"role":"user","content":prompt}],"stream":false})).send().await?.error_for_status()?;
    let result:Value=response.json().await?;
    let raw=result["choices"][0]["message"]["content"].as_str().context("model returned no output")?;
    std::fs::write(root.join("response.md"),raw)?;
    let source=raw.split("```javascript").nth(1).or_else(||raw.split("```js").nth(1)).or_else(||raw.split("```typescript").nth(1)).or_else(||raw.split("```ts").nth(1)).map(|s|s.split("```").next().unwrap_or(s));
    let mut syntax_ok=None;
    if let Some(source)=source {
        std::fs::write(root.join("acceptance.js"),source.trim())?;
        let map:swc_common::sync::Lrc<swc_common::SourceMap>=Default::default();
        let file=map.new_source_file(swc_common::FileName::Custom("acceptance.ts".into()).into(),source.to_owned());
        let lexer=swc_ecma_parser::lexer::Lexer::new(swc_ecma_parser::Syntax::Typescript(swc_ecma_parser::TsSyntax::default()),Default::default(),swc_ecma_parser::StringInput::from(&*file),None);
        let mut parser=swc_ecma_parser::Parser::new_from(lexer);
        syntax_ok=Some(parser.parse_typescript_module().is_ok() && parser.take_errors().is_empty());
    }
    let report=json!({"model":model,"seconds":started.elapsed().as_secs_f64(),"usage":result["usage"],"user_messages":1,"system_messages":0,"tool_declarations":0,"typescript_syntax_ok":syntax_ok,"executed":false});
    std::fs::write(root.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
    eprintln!("TS_MINIMAL_REPORT {report}");
    Ok(())
}
