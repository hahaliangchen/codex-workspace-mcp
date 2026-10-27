//! Isolated acceptance runs for the compiled flow runtime.
use crate::agent_service::{self, AgentServiceState};
use anyhow::{Context, Result, ensure};
use axum::{Json, Router, routing::post};
use rusqlite::params;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

const TASK: &str = "html-20-pages";
const UPDATE: &str = "从第 11 页开始，页脚加上页码和'内部资料'字样";
const FILES: &[&str] = &[
    "outline.json",
    "design_spec.md",
    "shared.css",
    "check_pages.py",
];
const PROMPT: &str = "Generate page01.html through page20.html from outline.json and design_spec.md. Read the shared materials, follow each page's exact title, points and layout, and use shared.css. Run python check_pages.py --page N after writing each page. Repair ordinary failures and continue until all 20 pages pass a final check. Preserve subsequent user updates. Do not modify the checker, outline or design specification. The task definition is already confirmed by this acceptance harness.";

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/html_20_pages")
}

fn workspace(label: &str) -> Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/context-acceptance")
        .join(format!("{label}-{}", agent_service::uuid_like()));
    std::fs::create_dir_all(&root)?;
    for name in FILES {
        std::fs::copy(fixture_root().join(name), root.join(name))?;
    }
    Ok(root)
}

fn initialize(state: &mut AgentServiceState) -> Result<()> {
    let mut conn = agent_service::open_db(state.workspace.root())?;
    conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,?2,?3,'running',0,0)", params![TASK,PROMPT,state.default_model])?;
    let tx = conn.transaction()?;
    let seq = agent_service::append_event_tx(
        &tx,
        TASK,
        "user/message",
        &json!({"turn":0,"message":{"role":"user","content":PROMPT}}),
        None,
    )?;
    tx.commit()?;
    let proposal = crate::main_task_anchor::update(
        state.workspace.root(),
        TASK,
        &json!({"objective":"Generate and verify all twenty HTML pages","constraints":["Preserve the outline, checker and design specification","Use shared.css","Apply user updates to their stated scope"],
            "completion_conditions":["All 20 saved pages pass the final checker"],"source_event_seqs":[seq]}),
    )?;
    crate::main_task_anchor::decide_definition(
        state.workspace.root(),
        TASK,
        proposal["version"].as_i64().unwrap(),
        true,
        "acceptance-harness",
    )?;
    let route = state
        .provider_routes
        .entry(state.provider_name.clone())
        .or_insert_with(|| agent_service::AgentProviderRoute {
            prefix_system_messages: false,
            url: state.provider_url.clone(),
            api_key: state.api_key.clone(),
            models: [(state.default_model.clone(), state.default_model.clone())]
                .into_iter()
                .collect(),
            max_output_tokens: Default::default(),
            context_windows: Default::default(),
        });
    route
        .context_windows
        .insert(state.default_model.clone(), 65_536);
    state.model_map = route.models.clone();
    let hashes = FILES
        .iter()
        .map(|name| {
            Ok((
                name.to_string(),
                json!(crate::visual_artifacts::hash(&std::fs::read(
                    fixture_root().join(name)
                )?)),
            ))
        })
        .collect::<Result<serde_json::Map<String, Value>>>()?;
    let revision = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()?;
    std::fs::write(
        state.workspace.root().join("run-manifest.json"),
        serde_json::to_vec_pretty(&json!({
        "task_id":TASK,"provider":state.provider_name,"model":state.default_model,"context_window":65_536,
        "revision":String::from_utf8_lossy(&revision.stdout).trim(),"fixture_hashes":hashes,
        "runtime":"task_tree_frames","user_message_policy":"preserve_full_original","original_prompt":PROMPT}))?,
    )?;
    Ok(())
}

/// Windows installs `python`; most Linux distributions only `python3`.
fn python() -> &'static str {
    let found = |name: &str| {
        std::process::Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if found("python") { "python" } else { "python3" }
}

fn program(root: &Path, args: &[&str]) -> Result<Value> {
    let output = std::process::Command::new(python())
        .args(args)
        .current_dir(root)
        .output()
        .context("Python is required for the HTML acceptance checker")?;
    let result: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| String::from_utf8_lossy(&output.stderr).into_owned())?;
    Ok(result)
}

fn save_reports(root: &Path) -> Result<Value> {
    for name in ["outline.json", "design_spec.md", "check_pages.py"] {
        ensure!(
            std::fs::read(root.join(name))? == std::fs::read(fixture_root().join(name))?,
            "fixture input modified: {name}"
        );
    }
    let check = program(root, &["check_pages.py", "--footer-from", "11"])?;
    std::fs::write(
        root.join("final-check.json"),
        serde_json::to_vec_pretty(&check)?,
    )?;
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/context_acceptance_stats.py");
    let stats = program(
        root,
        &[
            script.to_str().context("stats path must be UTF-8")?,
            ".codex-workspace-mcp/codex_state.db",
            TASK,
        ],
    )?;
    std::fs::write(
        root.join("metrics.json"),
        serde_json::to_vec_pretty(&stats)?,
    )?;
    println!("Acceptance results: {}", root.display());
    Ok(check)
}

fn tool(id: usize, name: &str, args: Value) -> Value {
    json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("acceptance-{id}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
}

/// Twenty pages as steps of one main task. The broken shared stylesheet found
/// at page 7 is repaired by a temporary task under that step; the user's update
/// at page 10 reaches every later page; finished page steps leave brief records.
#[tokio::test]
async fn scripted_twenty_page_task_graph_repairs_and_applies_the_interjection() -> Result<()> {
    let root = workspace("scripted-program")?;
    let outline: Value = serde_json::from_slice(&std::fs::read(root.join("outline.json"))?)?;
    let mut html = std::collections::BTreeMap::new();
    for page in outline["pages"].as_array().unwrap() {
        let n = page["id"].as_u64().unwrap();
        let title = page["title"].as_str().unwrap();
        let points = page["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|text| format!("<p class=\"point\">{}</p>", text.as_str().unwrap()))
            .collect::<String>();
        let footer = if n >= 11 {
            format!("<footer class=\"footer\">{n} 内部资料</footer>")
        } else {
            String::new()
        };
        html.insert(n, format!("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{title}</title><link rel=\"stylesheet\" href=\"shared.css\"></head><body><main class=\"page\"><h1>{title}</h1><section class=\"{}\">{points}</section>{footer}</main><!-- {} --></body></html>",page["layout"].as_str().unwrap(),"context pressure fixture ".repeat(1200)));
    }
    let source = r#"
function main() {
  const outline = JSON.parse(io.read({path:"outline.json"}).content);
  const spec = io.read({path:"design_spec.md"});
  const shared = {spec:spec.content};
  const files = [];
  for (const page of outline.pages) {
    const draft = designPage(shared,page);
    const path = "page" + (page.id < 10 ? "0" : "") + page.id + ".html";
    io.write({path,content:draft.content});
    let check = host.run_program({program:"python",args:["check_pages.py","--page",""+page.id,"--footer-from","11"],timeout_seconds:30});
    if (check.process_exit_code !== 0) {
      repair(shared,page,check);
      check = host.run_program({program:"python",args:["check_pages.py","--page",""+page.id,"--footer-from","11"],timeout_seconds:30});
    }
    assert(check.process_exit_code === 0,"page failed");
    files.push(path);
  }
  const final = host.run_program({program:"python",args:["check_pages.py","--footer-from","11"],timeout_seconds:30});
  assert(final.process_exit_code === 0,"final checker failed");
  return {files};
}
/** @virtual */ function designPage(shared,page) {
  // Use shared design rules and this page's outline. Return {content:string}.
}
/** @virtual */ function repair(shared,page,check) {
  // Inspect the failed checker result; repair the shared prerequisite, return null.
}
"#;
    let progress = Arc::new(Mutex::new((0usize, 0usize, false, 0usize)));
    let captured = progress.clone();
    let working = root.clone();
    let mut repaired_css = std::fs::read_to_string(root.join("shared.css"))?;
    let expected_hash = crate::symbol_description::content_hash(repaired_css.as_bytes());
    repaired_css
        .push_str("\n.repair-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 24px; }\n");
    let app=Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>| {
        let captured=captured.clone();let working=working.clone();let html=html.clone();
        let repaired_css=repaired_css.clone();let expected_hash=expected_hash.clone();
        async move {
            let mut progress=captured.lock().unwrap();let round=progress.0;progress.0+=1;
            let messages=body["messages"].as_array().unwrap();
            let task=messages.iter().find(|m|m["name"]=="program_task_input")
                .map(|m|serde_json::from_str::<Value>(m["content"].as_str().unwrap()).unwrap());
            let message=if let Some(task)=task {
                match task["name"].as_str().unwrap() {
                    "designPage"=>{
                        let n=task["bindings"]["page"]["id"].as_u64().unwrap();
                        if n==10 && !progress.2 {crate::interjections::submit(&working,TASK,UPDATE).unwrap();progress.2=true;}
                        if n==11 {assert!(body.to_string().contains("内部资料"),"user update must reach the next function");}
                        progress.1=progress.1.max(n as usize);
                        json!({"role":"assistant","content":json!({"content":html[&n]}).to_string()})
                    }
                    "repair"=>{
                        let stage=progress.3;progress.3+=1;
                        match stage {
                            0=>tool(round,"invoke_library",json!({"method":"io.read","args":{"path":"shared.css"}})),
                            1=>tool(round,"invoke_library",json!({"method":"io.write","args":{"path":"shared.css","content":repaired_css,"expected_code_hash":expected_hash}})),
                            2=>json!({"role":"assistant","content":"null"}),
                            _=>panic!("repair must execute once"),
                        }
                    }
                    other=>panic!("unexpected function {other}"),
                }
            } else {
                match round {
                    0=>tool(round,"create_task",json!({"goal":"Generate and verify all twenty HTML pages"})),
                    1=>tool(round,"execute_program",json!({"source":source})),
                    _=>panic!("the compiled main must continue without model rescheduling"),
                }
            };
            Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut state = agent_service::tests::test_state(&root, listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    initialize(&mut state)?;
    state
        .provider_routes
        .get_mut(&state.provider_name)
        .unwrap()
        .context_windows
        .insert(state.default_model.clone(), 131_072);
    let outcome = crate::single_agent::run_task(
        state.clone(),
        TASK.into(),
        state.default_model.clone(),
        PROMPT.into(),
        200,
        CancellationToken::new(),
        1,
        false,
    )
    .await;
    server.abort();
    outcome?;
    let check = save_reports(&root)?;
    ensure!(check["passed"] == true, "final checker failed: {check}");
    let progress = progress.lock().unwrap();
    ensure!(
        progress.1 == 20 && progress.2 && progress.3 == 3,
        "all functions and user update must complete once"
    );
    let conn = agent_service::open_db(&root)?;
    let nodes = crate::task_tree::nodes(&conn, TASK)?;
    let main = nodes
        .iter()
        .find(|n| n.metadata["role"] == "main")
        .context("main task missing")?;
    ensure!(main.status == "done", "main must return");
    let repair = nodes
        .iter()
        .find(|n| n.goal == "repair")
        .context("repair function missing")?;
    let owner = nodes
        .iter()
        .find(|n| Some(n.node_id.as_str()) == repair.parent_id.as_deref())
        .context("repair caller step missing")?;
    ensure!(
        repair.status == "done" && owner.kind == "step",
        "repair must return to its suspended program"
    );
    ensure!(
        nodes.iter().filter(|n| n.goal == "designPage").count() == 20,
        "each page has one function scope"
    );
    let failures:i64=conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='tool/result' AND json_extract(data,'$.meta.result.process_exit_code')=1",[TASK],|r|r.get(0))?;
    ensure!(
        failures == 1,
        "only the intentional page 7 check may fail, got {failures}"
    );
    let over_budget:i64=conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='context/usage' AND json_extract(data,'$.estimated_input_tokens')>json_extract(data,'$.input_budget')",[TASK],|r|r.get(0))?;
    ensure!(over_budget == 0, "requests must fit the budget");
    // Per-function scopes may avoid folding entirely; pressure folding is
    // exercised by step_context's growing-window tests, not forced here.
    let obsolete:i64=conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='tool/call' AND json_extract(data,'$.name') IN ('load_tool_group','create_step','return_task')",[TASK],|r|r.get(0))?;
    ensure!(
        obsolete == 0,
        "the acceptance run must use the current flow path"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "real configured model run; writes an isolated target/context-acceptance workspace and consumes provider tokens"]
async fn real_model_twenty_page_task_tree_acceptance() -> Result<()> {
    let root = workspace("real-tree")?;
    let mut state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse()?);
    let config = crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state, &config);
    state.default_model = state
        .model_map
        .get(&state.default_model)
        .cloned()
        .unwrap_or(state.default_model);
    initialize(&mut state)?;
    let watcher_state = state.clone();
    let watcher = tokio::spawn(async move {
        loop {
            if let Ok(result) = program(
                watcher_state.workspace.root(),
                &["check_pages.py", "--page", "10"],
            ) {
                if result["passed"] == true {
                    return crate::interjections::submit(
                        watcher_state.workspace.root(),
                        TASK,
                        UPDATE,
                    );
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
    let result = crate::single_agent::run_task(
        state.clone(),
        TASK.into(),
        state.default_model.clone(),
        PROMPT.into(),
        100,
        CancellationToken::new(),
        1,
        false,
    )
    .await;
    watcher.abort();
    let check = save_reports(&root)?;
    result?;
    ensure!(
        check["passed"] == true,
        "real-model run incomplete; inspect the saved metrics and event database"
    );
    Ok(())
}

const BLOG_TASK: &str = "blog-build";
const BLOG_PROMPT: &str = "Build the static blog described in SPEC.md: write build.py, generate site/, and finish when python check_site.py reports passed. Do not modify posts/, templates/, SPEC.md or check_site.py. The task definition is already confirmed by this acceptance harness.";

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// A small code project with a defect the plan does not know about: the shared
/// `lib/slug.py` drops Chinese characters, so some posts collide or get empty
/// slugs. Watch whether the Agent yields a repair task from the step that hits
/// it, resumes that step, and keeps each step's context to earlier results.
#[tokio::test]
#[ignore = "real configured model run; writes an isolated target/context-acceptance workspace and consumes provider tokens"]
async fn real_model_blog_build_repairs_an_unexpected_dependency() -> Result<()> {
    real_model_blog("blog_build").await
}

/// The shared Markdown helper memoizes on body length, so two posts of equal
/// length share one rendering. Nothing in the code or checker names the helper;
/// the defect appears only as missing body text after a build.
#[tokio::test]
#[ignore = "real configured model run; writes an isolated target/context-acceptance workspace and consumes provider tokens"]
async fn real_model_blog_build_meets_a_runtime_only_defect() -> Result<()> {
    real_model_blog("blog_runtime").await
}

async fn real_model_blog(fixture_name: &str) -> Result<()> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture_name);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/context-acceptance")
        .join(format!(
            "{fixture_name}-real-{}",
            agent_service::uuid_like()
        ));
    copy_tree(&fixture, &root)?;
    let mut state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse()?);
    let config = crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state, &config);
    state.default_model = state
        .model_map
        .get(&state.default_model)
        .cloned()
        .unwrap_or(state.default_model);
    let mut conn = agent_service::open_db(&root)?;
    conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,?2,?3,'running',0,0)", params![BLOG_TASK,BLOG_PROMPT,state.default_model])?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let seq = agent_service::append_event_tx(
        &tx,
        BLOG_TASK,
        "user/message",
        &json!({"turn":0,"message":{"role":"user","content":BLOG_PROMPT}}),
        None,
    )?;
    tx.commit()?;
    let proposal = crate::main_task_anchor::update(
        &root,
        BLOG_TASK,
        &json!({"objective":"Build the static blog in SPEC.md with build.py","constraints":["Do not modify posts/, templates/, SPEC.md or check_site.py","Standard library only"],
        "completion_conditions":["python check_site.py reports passed"],"source_event_seqs":[seq]}),
    )?;
    crate::main_task_anchor::decide_definition(
        &root,
        BLOG_TASK,
        proposal["version"].as_i64().unwrap(),
        true,
        "acceptance-harness",
    )?;
    let outcome = crate::single_agent::run_task(
        state.clone(),
        BLOG_TASK.into(),
        state.default_model.clone(),
        BLOG_PROMPT.into(),
        120,
        CancellationToken::new(),
        1,
        false,
    )
    .await;
    let check = program(&root, &["check_site.py"])
        .unwrap_or_else(|error| json!({"passed":false,"error":format!("{error:#}")}));
    let nodes = crate::task_tree::nodes(&conn, BLOG_TASK)?;
    let usage = |node: &str| -> Result<(i64, i64, i64)> {
        Ok(conn.query_row("SELECT COUNT(*), COALESCE(SUM(json_extract(data,'$.estimated_input_tokens')),0), COALESCE(MAX(json_extract(data,'$.estimated_input_tokens')),0) FROM agent_task_events WHERE task_id=?1 AND node_id=?2 AND kind='context/usage'",
            params![BLOG_TASK,node],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?)
    };
    let tree = nodes.iter().map(|n| {
        let (requests, total, peak) = usage(&n.node_id)?;
        Ok(json!({"node_id":n.node_id,"parent_id":n.parent_id,"kind":n.kind,"title":n.title,"status":n.status,"reason":n.spawned_from,
            "result":n.result.as_ref().map(|r|r["result"].clone()),"requests":requests,"estimated_input_total":total,"estimated_input_peak":peak}))
    }).collect::<Result<Vec<_>>>()?;
    let count = |kind: &str| -> Result<i64> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind=?2",
            params![BLOG_TASK, kind],
            |r| r.get(0),
        )?)
    };
    let tool_errors: Vec<String> = conn.prepare("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='tool/result' AND json_extract(data,'$.message.isError')=1")?
        .query_map([BLOG_TASK], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter().map(|raw| serde_json::from_str::<Value>(&raw).map(|v| v["meta"]["result"]["error"].to_string().chars().take(200).collect()).unwrap_or(raw)).collect();
    let report = json!({"outcome":outcome.as_ref().map(|_|"ok".to_owned()).unwrap_or_else(|e|format!("{e:#}")),
        "task_status":conn.query_row("SELECT status FROM agent_tasks WHERE id=?1",[BLOG_TASK],|r|r.get::<_,String>(0))?,
        "check":check,"nodes":tree,"compressions":count("context/compression_start")?,"tool_calls":count("tool/call")?,"tool_errors":tool_errors,
        "changed_shared":std::fs::read_dir(fixture.join("lib"))?.filter_map(|e|e.ok()).filter_map(|e| {
            let name=e.file_name();(std::fs::read(e.path()).ok()!=std::fs::read(root.join("lib").join(&name)).ok()).then(||name.to_string_lossy().into_owned())
        }).collect::<Vec<_>>()});
    std::fs::write(
        root.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    eprintln!(
        "Blog acceptance report: {}",
        root.join("report.json").display()
    );
    outcome?;
    ensure!(
        check["passed"] == true,
        "blog build incomplete; inspect report.json and the event database"
    );
    Ok(())
}
