//! Isolated acceptance runs shared by the linear baseline and task-tree rollout.
use crate::agent_service::{self, AgentServiceState};
use anyhow::{Context, Result, ensure};
use axum::{Json, Router, routing::post};
use rusqlite::params;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
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
            context_request_options: Default::default(),
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

fn program(root: &Path, args: &[&str]) -> Result<Value> {
    let output = std::process::Command::new("python")
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

fn check_call(number: Option<u64>, footer: bool) -> (String, Value) {
    let mut args = vec![json!("check_pages.py")];
    if let Some(number) = number {
        args.extend([json!("--page"), json!(number.to_string())]);
    }
    if footer {
        args.extend([json!("--footer-from"), json!("11")]);
    }
    (
        "run_program".into(),
        json!({"program":"python","args":args,"timeout_seconds":30}),
    )
}

#[tokio::test]
async fn scripted_twenty_page_task_tree_repairs_a_branch_and_scopes_the_interjection() -> Result<()>
{
    let root = workspace("scripted-tree")?;
    let outline: Value = serde_json::from_slice(&std::fs::read(root.join("outline.json"))?)?;
    let mut queue = VecDeque::new();
    for group in ["files", "execution"] {
        queue.push_back(("load_tool_group".into(), json!({"group":group})));
    }
    for name in ["outline.json", "design_spec.md"] {
        queue.push_back(("read_file".into(), json!({"path":name})));
    }
    queue.push_back(("plan_pages".into(), json!({})));
    for page in outline["pages"].as_array().unwrap() {
        let n = page["id"].as_u64().unwrap();
        queue.push_back(("start_page".into(), json!({"page":n})));
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
        let html = format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{title}</title><link rel=\"stylesheet\" href=\"shared.css\"></head><body><main class=\"page\"><h1>{title}</h1><section class=\"{}\">{points}</section>{footer}</main><!-- {} --></body></html>",
            page["layout"].as_str().unwrap(),
            "context pressure fixture ".repeat(1200)
        );
        queue.push_back((
            "write_file".into(),
            json!({"path":format!("page{n:02}.html"),"content":html}),
        ));
        queue.push_back(check_call(Some(n), n >= 11));
        if n == 7 {
            queue.push_back(("branch_task".into(),json!({"title":"Repair shared stylesheet","goal":"Add the missing repair-grid class","done_when":"Page 7 check passes","reason":"Page 7 check failed because the shared class is missing","briefing":"Read and repair only shared.css, then recheck page 7","inputs":{"tool_groups":["files","execution"]}})));
            let mut css = std::fs::read_to_string(root.join("shared.css"))?;
            let expected = crate::symbol_description::content_hash(css.as_bytes());
            css.push_str(
                "\n.repair-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 24px; }\n",
            );
            queue.push_back(("read_file".into(), json!({"path":"shared.css"})));
            queue.push_back((
                "write_file".into(),
                json!({"path":"shared.css","content":css,"expected_code_hash":expected}),
            ));
            queue.push_back(check_call(Some(n), false));
            queue.push_back(("return_current".into(), json!({})));
            queue.push_back(check_call(Some(n), false));
        }
        if n == 10 {
            queue.push_back(("inject_user_update".into(), json!({})));
        }
        queue.push_back(("return_current".into(), json!({})));
    }
    queue.push_back(check_call(None, true));
    queue.push_back(("return_current".into(), json!({})));
    let progress = Arc::new(Mutex::new((queue, 0usize, 0usize, false)));
    let captured = progress.clone();
    let working = root.clone();
    let app = Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>| {
        let captured = captured.clone();
        let working = working.clone();
        async move {
            let mut progress = captured.lock().unwrap();
            progress.1 += 1;
            let round = progress.1;
            let messages = body["messages"].as_array().unwrap();
            let conn=agent_service::open_db(&working).unwrap();
            let active=crate::task_tree::active(&conn,TASK).unwrap();
            let pending=active.as_ref().and_then(|node|crate::task_tree::instructions(&conn,TASK,&node.node_id).unwrap().into_iter().find(|instruction|instruction["scope_pending"]==true));
            let message = if let Some(review) = messages.iter().find(|m|m["name"]=="context_review_input") {
                progress.2 += 1;
                let supplied: Value = serde_json::from_str(review["content"].as_str().unwrap()).unwrap();
                let refs = supplied["original_user_requirements"].as_array().unwrap().iter().map(|r|r["event_seq"].clone()).collect::<Vec<_>>();
                tool(round,"submit_context",json!({"original_purpose":"Generate and verify all twenty HTML pages","current_goal":"Continue the remaining page writes and verification",
                    "requirement_changes":if progress.3 {UPDATE} else {"none"},"constraints":"Follow the outline and shared design specification. Preserve all successful saved pages.",
                    "progress":"Completed pages and the shared repair remain on disk; use check_pages.py for evidence.","open_questions":"none",
                    "next_steps":"Continue the scripted remaining work, apply the footer only from page 11, and verify all 20 pages.","tool_history":"Do not replay completed writes; inspect saved files when needed.",
                    "source_event_seqs":refs,"visual_conclusions":[],"restore_assets":[]}))
            } else if let Some(instruction)=pending {
                tool(round,"scope_message",json!({"instruction_id":instruction["id"],"node_id":"n0"}))
            } else if let Some((name,args)) = progress.0.pop_front() {
                if name=="inject_user_update" {
                    crate::interjections::submit(&working,TASK,UPDATE).unwrap();
                    progress.3 = true;
                    tool(round,"read_task_board",json!({}))
                } else if name=="plan_pages" {
                    let materials=conn.prepare("SELECT asset_id FROM agent_context_assets WHERE task_id=?1 AND source_tool='read_file'").unwrap().query_map([TASK],|r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
                    tool(round,"plan_tasks",json!({"tasks":(1..=20).map(|page|json!({"title":format!("Page {page}"),"goal":format!("Generate page {page} from its exact outline entry"),"done_when":"The saved HTML passes its page checker","inputs":{"assets":materials,"pinned":materials,"tool_groups":["files","execution"]}})).collect::<Vec<_>>()}))
                } else if name=="start_page" {
                    let page=args["page"].as_u64().unwrap();
                    let node=crate::task_tree::nodes(&conn,TASK).unwrap().into_iter().find(|n|n.title==format!("Page {page}")).unwrap();
                    tool(round,"start_task",json!({"node_id":node.node_id,"briefing":format!("Follow the shared outline and specification for page {page}; verify its actual saved file.")}))
                } else if name=="return_current" {
                    let active=active.as_ref().unwrap();
                    let proof:i64=conn.query_row("SELECT seq FROM agent_task_events WHERE task_id=?1 AND node_id=?2 AND kind='tool/result' AND json_extract(data,'$.meta.result.process_exit_code')=0 ORDER BY seq DESC LIMIT 1",params![TASK,active.node_id],|r|r.get(0)).unwrap();
                    tool(round,"return_task",json!({"status":"done","result":format!("{} verified against the saved files",active.title),"evidence":[proof]}))
                } else {
                    if name=="write_file" && args["path"]=="page11.html" {
                        assert!(body.to_string().contains("内部资料"),"the mid-turn update must reach the next execution request");
                    }
                    tool(round,&name,args)
                }
            } else {json!({"role":"assistant","content":"All twenty fixture pages were checked."})};
            Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut state = agent_service::tests::test_state(&root, listener.local_addr()?);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    initialize(&mut state)?;
    let outcome = crate::single_agent::run_task(
        state.clone(),
        TASK.into(),
        state.default_model.clone(),
        PROMPT.into(),
        160,
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
        progress.0.is_empty() && progress.3,
        "the script must finish and propagate the user update"
    );
    let conn = agent_service::open_db(&root)?;
    let nodes = crate::task_tree::nodes(&conn, TASK)?;
    ensure!(
        nodes.len() == 22 && nodes.iter().all(|n| n.status == "done"),
        "all 20 page nodes, the repair branch and root must finish"
    );
    let page7 = nodes.iter().find(|n| n.title == "Page 7").unwrap();
    ensure!(
        nodes
            .iter()
            .any(|n| n.kind == "branch" && n.parent_id.as_deref() == Some(&page7.node_id)),
        "the stylesheet repair must belong to page 7"
    );
    let failures:i64 = conn.query_row("SELECT COUNT(*) FROM agent_task_events WHERE task_id=?1 AND kind='tool/result' AND json_extract(data,'$.meta.result.process_exit_code')=1",[TASK],|r|r.get(0))?;
    ensure!(
        failures == 1,
        "only the intentional page 7 check may fail, got {failures}"
    );
    for name in ["outline.json", "design_spec.md", "check_pages.py"] {
        ensure!(
            std::fs::read(root.join(name))? == std::fs::read(fixture_root().join(name))?,
            "fixture input modified: {name}"
        );
    }
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
