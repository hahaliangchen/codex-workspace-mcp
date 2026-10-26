use super::*;

const GROUPS: &[(&str, &str)] = &[
    (
        "files",
        "Inspect directories; read, search, write and edit local files; refresh file assets",
    ),
    ("code_index", "Find code through the workspace code map"),
    ("code_go", "Index and inspect Go symbols"),
    ("code_rust", "Index and inspect Rust symbols"),
    ("code_ts", "Index and inspect TypeScript/JavaScript symbols"),
    ("code_python", "Index and inspect Python symbols"),
    (
        "execution",
        "Run programs and project scripts, install dependencies, manage services and probe HTTP",
    ),
    ("browser", "Open, read, operate and capture browser pages"),
    (
        "memory",
        "Read and record project knowledge, architecture and business context; propose project notes",
    ),
    ("attachments", "Read uploaded document and file attachments"),
    (
        "vision",
        "View saved images and request or collect visual analysis",
    ),
    (
        "collaboration",
        "Fork child functions onto Agents; join their returns, inspect, merge, review or cancel",
    ),
];

pub(super) fn group(name: &str) -> Option<&'static str> {
    Some(match name {
        "list_dir" | "read_file" | "read_file_lines" | "search_text" | "write_file"
        | "replace_range" | "edit_file" | "refresh_asset" => "files",
        "search_code_map" => "code_index",
        "index_go_workspace" | "go_index_status" | "list_go_symbols" | "search_go_symbols"
        | "read_go_symbol" => "code_go",
        "index_rust_workspace"
        | "rust_index_status"
        | "list_rust_symbols"
        | "search_rust_symbols"
        | "read_rust_symbol" => "code_rust",
        "index_ts_workspace" | "ts_index_status" | "list_ts_symbols" | "search_ts_symbols"
        | "read_ts_symbol" => "code_ts",
        "index_python_workspace"
        | "python_index_status"
        | "list_python_symbols"
        | "search_python_symbols"
        | "read_python_symbol" => "code_python",
        "install_dependencies"
        | "run_project_script"
        | "get_project_process"
        | "stop_project_process"
        | "http_probe"
        | "run_program" => "execution",
        "browser_open"
        | "browser_read"
        | "browser_wait"
        | "browser_diagnostics"
        | "browser_click"
        | "browser_press_key"
        | "browser_upload"
        | "browser_screenshot"
        | "browser_close" => "browser",
        "record_work_memory"
        | "list_work_memory"
        | "search_work_memory"
        | "record_architecture_memory"
        | "list_architecture_memory"
        | "search_architecture_memory"
        | "record_symbol_business_context"
        | "list_symbol_business_context"
        | "search_symbol_business_context"
        | "propose_project_note" => "memory",
        "read_attachment" => "attachments",
        "view_image" | "analyze_images" | "image_analysis_results" => "vision",
        "spawn_agent"
        | "join_agents"
        | "wait_agents"
        | "review_agent_result"
        | "inspect_agent_changes"
        | "merge_agent_result"
        | "cancel_agent" => "collaboration",
        _ => return None,
    })
}

pub(super) fn tools() -> Vec<Value> {
    let selector =
        json!({"group":{"type":"string","enum":GROUPS.iter().map(|(id,_)|id).collect::<Vec<_>>()}});
    vec![
        function(
            "list_tool_groups",
            "List available built-in tool groups, their tools and this frame's loaded state. Definitions are sent only for loaded groups. Skill tools are loaded separately with read_skill.",
            json!({}),
            json!([]),
        ),
        function(
            "load_tool_group",
            "Load one built-in tool group into this frame. Call alone; use the newly advertised tools in the next request. Loading preserves existing permissions and plugin boundaries. Use list_tool_groups to discover available groups.",
            selector.clone(),
            json!(["group"]),
        ),
        function(
            "unload_tool_group",
            "Hide one built-in tool group's definitions from this frame. Call alone. Existing messages, results and permissions are unchanged; load it again when needed.",
            selector,
            json!(["group"]),
        ),
    ]
}

pub(super) fn is_change(name: &str) -> bool {
    matches!(name, "load_tool_group" | "unload_tool_group")
}

pub(crate) fn requested(value: &Value) -> Result<BTreeSet<String>> {
    if value.is_null() {
        return Ok(BTreeSet::new());
    }
    let items = value.as_array().context("tool_groups must be an array")?;
    ensure!(items.len() <= GROUPS.len(), "too many tool groups");
    items
        .iter()
        .map(|item| {
            let id = item.as_str().context("tool group IDs must be strings")?;
            ensure!(
                GROUPS.iter().any(|(known, _)| *known == id),
                "UNKNOWN_TOOL_GROUP: {id}"
            );
            Ok(id.to_owned())
        })
        .collect()
}

fn names(definitions: &[Value], id: &str) -> Vec<String> {
    definitions
        .iter()
        .filter_map(|tool| {
            let name = tool["function"]["name"].as_str()?;
            (group(name) == Some(id)).then(|| name.to_owned())
        })
        .collect()
}

pub(super) fn list(s: &Session<'_>, w: &Window) -> Value {
    let definitions = available_tools(s, w);
    let groups = GROUPS
        .iter()
        .filter_map(|(id, description)| {
            let names = names(&definitions, id);
            (!names.is_empty()).then(|| {
                json!({"group":id,"description":description,
            "loaded":w.loaded_tool_groups.contains(*id),"tools":names})
            })
        })
        .collect::<Vec<_>>();
    json!({"groups":groups,"scope":"current_task_frame","skill_tools":"read_skill"})
}

pub(super) fn from_inputs(s: &Session<'_>, inputs: &Value) -> Result<BTreeSet<String>> {
    let groups = requested(&inputs["tool_groups"])?;
    let definitions = available_tools(s, &Window::default());
    for id in &groups {
        ensure!(
            !names(&definitions, id).is_empty(),
            "TOOL_GROUP_UNAVAILABLE: {id}"
        );
    }
    Ok(groups)
}

pub(super) async fn execute(
    s: &Session<'_>,
    w: &mut Window,
    call: &crate::model_stream::ToolCall,
    definitions: &[Value],
    request_id: &str,
    step: usize,
) -> Result<()> {
    let prepared = (|| -> Result<(Window, Value)> {
        validate_tool(s, w, definitions, &call.name)?;
        let args: Value =
            serde_json::from_str(&call.arguments).context("invalid tool arguments JSON")?;
        let id = args["group"].as_str().context("group is required")?;
        requested(&json!([id]))?;
        let names = names(&available_tools(s, w), id);
        let enable = call.name == "load_tool_group";
        ensure!(!enable || !names.is_empty(), "TOOL_GROUP_UNAVAILABLE: {id}");
        let mut next = w.clone();
        if enable {
            next.loaded_tool_groups.insert(id.to_owned());
        } else {
            next.loaded_tool_groups.remove(id);
        }
        let result = json!({"group":id,"loaded":enable,"tools":if enable {names}else{vec![]},
            "loaded_groups":next.loaded_tool_groups,"scope":"current_task_frame"});
        let mut messages = next.messages.clone();
        messages
            .push(json!({"role":"tool","tool_call_id":call.call_id,"content":result.to_string()}));
        ensure!(
            !enable
                || next.input_tokens(s, &request(s, &messages, &super::tools(s, &next))) + 4096
                    < s.input_budget,
            "TOOL_GROUP_OVER_CAPACITY: release unneeded material or unload another group before loading {id}; current groups preserved"
        );
        Ok((next, result))
    })();
    let (mut next, result, is_error) = match prepared {
        Ok((next, result)) => (next, result, false),
        Err(error) => (w.clone(), json!({"error":format!("{error:#}")}), true),
    };
    let description = task_frames::tool_description(definitions, &call.name, &call.arguments);
    let (task, scope, agent, turn, call, request_id) = (
        s.task.to_owned(),
        s.scope.clone(),
        s.delegate_id().map(str::to_owned),
        s.turn,
        call.clone(),
        request_id.to_owned(),
    );
    next = db(s.state.workspace.root(), move |conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Persist the native exchange and capability state together for restart/rewind.
        let event = |kind: &str, mut data: Value| -> Result<i64> {
            data["turn"]=json!(turn);
            data["task_node_id"]=json!(next.current_step_node_id.as_ref().or(next.task_node_id.as_ref()));
            if let Some(agent)=&agent {
                let local:i64=tx.query_row("UPDATE agent_delegates SET event_count=event_count+1 WHERE task_id=?1 AND id=?2 RETURNING event_count-1",params![task,agent],|r|r.get(0))?;
                agent_service::append_event_tx(&tx,&task,"subagent/event",&json!({"agent_id":agent,"agent_seq":local,"turn":turn,"task_node_id":data["task_node_id"],"event_type":kind,"event":data}),Some("append"))
            } else {agent_service::append_event_tx(&tx,&task,kind,&data,Some("append"))}
        };
        let actor=if agent.is_some(){"worker"}else{"agent"};
        event("tool/call",json!({"actor":actor,"step":step,"flowNodeId":request_id,
            "callId":call.call_id,"name":call.name,"arguments":call.arguments,"description":description}))?;
        let seq=event("tool/result",json!({"actor":actor,"step":step,"flowNodeId":request_id,
            "message":{"role":"tool","toolCallId":call.call_id,"isError":is_error,"content":[{"type":"text","text":result.to_string()}]},"meta":{"result":result}}))?;
        next.messages.push(sourced(json!({"role":"tool","tool_call_id":call.call_id,
            "content":json!({"event_seq":seq,"result":result}).to_string()}),&task,seq));
        tx.execute("INSERT INTO agent_runtime_windows VALUES (?1,?2,?3) ON CONFLICT(task_id,scope) DO UPDATE SET state=excluded.state",params![task,scope,serde_json::to_string(&next)?])?;
        tx.commit()?;
        Ok(next)
    }).await?;
    *w = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};

    struct Harness {
        root: std::path::PathBuf,
        state: AgentServiceState,
        cancel: CancellationToken,
    }
    impl Harness {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("tool-groups-{}", agent_service::uuid_like()));
            std::fs::create_dir_all(&root).unwrap();
            let state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse().unwrap());
            agent_service::open_db(&root).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','','fake','running',0,0)",[]).unwrap();
            Self {
                root,
                state,
                cancel: CancellationToken::new(),
            }
        }
        fn session(&self) -> Session<'_> {
            Session {agent_id:None,task_node_id:None,step_node_id:None,
                plugin: None,
                state: &self.state,
                task: "t",
                model: "fake",
                turn: 1,
                cancel: &self.cancel,
                ban_execution: false,
                mode: Mode::Main,
                scope: "main".into(),
                parent: None,
                input_budget: INPUT_BUDGET,
            }
        }
    }
    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn has(definitions: &[Value], name: &str) -> bool {
        definitions.iter().any(|t| t["function"]["name"] == name)
    }

    #[test]
    fn fresh_frames_only_advertise_core_and_every_local_tool_is_classified() {
        let h = Harness::new();
        let s = h.session();
        let mut w = Window::default();
        let full = available_tools(&s, &w);
        let core = super::super::tools(&s, &w);
        for name in [
            "read_asset",
            "read_session_history",
            "read_skill",
            "load_tool_group",
            "list_tool_groups",
        ] {
            assert!(has(&core, name), "missing core tool {name}");
        }
        for name in [
            "plan_tasks",
            "branch_task",
            "next_step",
            "read_file",
            "write_file",
            "run_program",
            "browser_open",
            "read_rust_symbol",
            "analyze_images",
            "spawn_agent",
        ] {
            assert!(!has(&core, name), "unloaded definition leaked: {name}");
        }
        for tool in agent_service::local_tools(&h.state.workspace, None, false) {
            let name = tool["function"]["name"].as_str().unwrap();
            assert!(
                name == "workspace_info" || group(name).is_some(),
                "unclassified local tool: {name}"
            );
        }
        assert!(estimate(&json!(core)) * 3 < estimate(&json!(full)));
        w.loaded_tool_groups = requested(&json!(["files", "execution"])).unwrap();
        let selected = super::super::tools(&s, &w);
        assert!(has(&selected, "write_file") && has(&selected, "run_program"));
        assert!(!has(&selected, "browser_open") && !has(&selected, "read_go_symbol"));
        assert_eq!(
            selected,
            super::super::tools(&s, &w),
            "unchanged frames keep a stable tool list"
        );
        eprintln!(
            "tool schema estimates: all={} tools/{} tokens; core={} tools/{} tokens; files+execution={} tools/{} tokens",
            full.len(),
            estimate(&json!(full)),
            core.len(),
            estimate(&json!(core)),
            selected.len(),
            estimate(&json!(selected))
        );
        let mut legacy = serde_json::to_value(&w).unwrap();
        legacy.as_object_mut().unwrap().remove("loaded_tool_groups");
        assert!(
            serde_json::from_value::<Window>(legacy)
                .unwrap()
                .loaded_tool_groups
                .is_empty()
        );
        assert!(requested(&json!(["nonexistent"])).is_err());
        assert!(crate::task_tree::validate_inputs(&json!({"tool_groups":"files"})).is_err());
    }

    #[tokio::test]
    async fn loading_preserves_permissions_plugin_scope_and_capacity() {
        let mut h = Harness::new();
        h.state.permission_mode = agent_service::PermissionMode::ReadOnly;
        let mut s = h.session();
        s.ban_execution = true;
        let mut w = Window {
            loaded_tool_groups: requested(&json!(["files", "execution", "collaboration"])).unwrap(),
            ..Default::default()
        };
        let definitions = super::super::tools(&s, &w);
        assert!(has(&definitions, "read_file"));
        assert!(!has(&definitions, "write_file") && !has(&definitions, "run_program"));
        assert!(validate_tool(&s, &w, &definitions, "write_file").is_err());
        s.plugin = Some("ppt-master".into());
        assert!(from_inputs(&s, &json!({"tool_groups":["files"]})).is_err());
        let catalog = list(&s, &w);
        assert!(
            !catalog["groups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|g| g["group"] == "files")
        );
        assert!(
            !super::super::tools(&s, &w)
                .iter()
                .any(|t| t["function"]["name"] == "read_file")
        );
        s.plugin = None;
        s.input_budget = 1000;
        w.loaded_tool_groups.clear();
        let call = crate::model_stream::ToolCall {
            call_id: "load".into(),
            name: "load_tool_group".into(),
            arguments: json!({"group":"files"}).to_string(),
        };
        w.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}));
        let definitions = super::super::tools(&s, &w);
        execute(&s, &mut w, &call, &definitions, "request", 1)
            .await
            .unwrap();
        assert!(w.loaded_tool_groups.is_empty());
        assert!(
            w.messages.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("TOOL_GROUP_OVER_CAPACITY")
        );
        let stored: String = agent_service::open_db(&h.root)
            .unwrap()
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            serde_json::from_str::<Window>(&stored)
                .unwrap()
                .loaded_tool_groups
                .is_empty()
        );
    }

    #[tokio::test]
    async fn real_loop_requires_loading_and_unloading_hides_definitions_and_blocks_calls() {
        let mut h = Harness::new();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = captured.clone();
        let app=Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>| {
            let seen=seen.clone();async move {
                let mut requests=seen.lock().unwrap();let n=requests.len();
                let definitions=body["tools"].as_array().unwrap();
                let call=|name:&str,args:Value|json!({"role":"assistant","tool_calls":[{"id":format!("c{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]});
                let message=match n {
                    0=>call("create_task",json!({"goal":"Verify lazy tools","first_step":"Try a file tool"})),
                    1=>{assert!(!has(definitions,"write_file"));call("write_file",json!({"path":"blocked.txt","content":"must not run"}))},
                    2=>{assert!(body.to_string().contains("TOOL_GROUP_NOT_LOADED"));call("load_tool_group",json!({"group":"files"}))},
                    3=>{assert!(has(definitions,"write_file") && !has(definitions,"run_program"));
                        let mut response=call("load_tool_group",json!({"group":"execution"}));
                        response["tool_calls"].as_array_mut().unwrap().push(json!({"id":"parallel-program","type":"function","function":{"name":"run_program","arguments":json!({"program":"does-not-exist"}).to_string()}}));response},
                    4=>{assert!(!has(definitions,"run_program"));call("write_file",json!({"path":"saved.txt","content":"loaded tool worked"}))},
                    5=>call("unload_tool_group",json!({"group":"files"})),
                    6=>{assert!(!has(definitions,"write_file"));call("write_file",json!({"path":"blocked-again.txt","content":"must not run"}))},
                    _=>json!({"role":"assistant","content":"Verified tool loading and unloading."}),
                };
                requests.push(body);
                Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        h.state.provider_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        run_task(
            h.state.clone(),
            "t".into(),
            "fake".into(),
            "Verify lazy tools".into(),
            9,
            h.cancel.clone(),
            1,
            false,
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(captured.lock().unwrap().len(), 8);
        assert_eq!(
            std::fs::read_to_string(h.root.join("saved.txt")).unwrap(),
            "loaded tool worked"
        );
        assert!(!h.root.join("blocked.txt").exists() && !h.root.join("blocked-again.txt").exists());
        let raw: String = agent_service::open_db(&h.root)
            .unwrap()
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let saved: Window = serde_json::from_str(&raw).unwrap();
        assert!(saved.loaded_tool_groups.is_empty());
        for (i, m) in saved
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "tool")
        {
            assert!(saved.messages[..i].iter().any(|a| {
                a["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| calls.iter().any(|c| c["id"] == m["tool_call_id"]))
            }));
        }
    }
}
