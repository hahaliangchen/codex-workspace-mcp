use super::*;
use crate::task_tree::{self as tree, Node};

fn node_id<'a>(s: &'a Session<'_>) -> &'a str {
    s.scope.strip_prefix("node:").unwrap_or(tree::ROOT)
}

pub(super) async fn notices(s: &Session<'_>, w: &mut Window) -> Result<()> {
    if s.mode != Mode::Main {
        return Ok(());
    }
    let conn = agent_service::open_db(s.state.workspace.root())?;
    if tree::get(&conn, s.task, tree::ROOT).is_err() {
        return Ok(());
    }
    let mut changed = false;
    let id = w.task_node_id.as_deref().unwrap_or_else(|| node_id(s));
    // Child material is prepared before the start transaction commits.
    // Refresh its snapshot before delivery, including frames saved by older builds.
    if let Some(message) = w.messages.iter_mut().find(|m| m["name"] == "task_node") {
        let content = json!(serde_json::to_string(&tree::get(&conn, s.task, id)?)?);
        if message["content"] != content {
            message["content"] = content;
            changed = true;
        }
    }
    for instruction in tree::instructions(&conn, s.task, id)? {
        let id = instruction["id"].as_str().unwrap().to_owned();
        let version = instruction["scope_node"].to_string();
        if w.instruction_versions.get(&id) != Some(&version) {
            w.messages.push(
                json!({"role":"user","name":"task_instruction","content":instruction.to_string()}),
            );
            w.instruction_versions.insert(id, version);
            changed = true;
        }
    }
    drop(conn);
    if changed {
        save(s, w).await?;
    }
    Ok(())
}

pub(super) fn delegate_start(s: &Session<'_>, name: &str, args: &Value) -> Result<bool> {
    if name != "start_task" {
        return Ok(false);
    }
    let conn = agent_service::open_db(s.state.workspace.root())?;
    Ok(tree::get(&conn, s.task, args["node_id"].as_str().unwrap_or(""))?.executor == "delegate")
}

pub(super) async fn delegate_context(
    s: &Session<'_>,
    w: &Window,
    args: &Value,
    planned: bool,
) -> Result<(Value, Value)> {
    let mut conn = agent_service::open_db(s.state.workspace.root())?;
    let Some(parent) = tree::active(&conn, s.task)? else {
        return Ok((
            args.clone(),
            prepare_delegate_context(s, w, &tools(s, w)).await?,
        ));
    };
    let node = if planned {
        let mut node = tree::get(
            &conn,
            s.task,
            args["node_id"].as_str().context("node_id required")?,
        )?;
        ensure!(
            node.status == "pending"
                && node.parent_id.as_deref() == Some(&parent.node_id)
                && node.executor == "delegate",
            "only pending direct delegate children may start"
        );
        node.briefing = args["briefing"]
            .as_str()
            .context("briefing required")?
            .to_owned();
        tree::validate_plan_version(&conn, s.task, &node)?;
        if let Some(inputs) = args.get("inputs") {
            node.inputs = tree::validate_inputs(inputs)?;
        }
        node
    } else {
        let mut inputs = args["inputs"]
            .as_object()
            .cloned()
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        if args["asset_ids"].is_array() {
            inputs["assets"] = args["asset_ids"].clone();
        }
        let node = tree::planned(
            &parent,
            &json!({"title":args["name"],"goal":args["task"],"done_when":args.get("done_when").unwrap_or(&args["task"]),"executor":"delegate","inputs":inputs}),
            tree::nodes(&conn, s.task)?.len(),
            "planned",
        )?;
        let tx = conn.transaction()?;
        let mut recorded = node.clone();
        tree::emit(&tx, s.task, "task/node_planned", &mut recorded, s.turn)?;
        tx.commit()?;
        recorded
    };
    drop(conn);
    let frame = child_material(s, w, &node).await?;
    let mut shared = serde_json::to_value(frame)?;
    shared["task_node"] = serde_json::to_value(&node)?;
    let mut assignment = args.clone();
    assignment["name"] = json!(node.title);
    assignment["task"] = json!(node.goal);
    assignment["task_node_id"] = json!(node.node_id);
    assignment["context"] = json!(node.briefing);
    Ok((assignment, shared))
}

pub(super) async fn delegate_return(
    s: &Session<'_>,
    w: &mut Window,
    call: &crate::model_stream::ToolCall,
    request_id: &str,
    step: usize,
) -> Result<bool> {
    let (task, turn, agent, scope, mut next, call, request_id) = (
        s.task.to_owned(),
        s.turn,
        s.delegate_id().context("not a delegate")?.to_owned(),
        s.scope.clone(),
        w.clone(),
        call.clone(),
        request_id.to_owned(),
    );
    let (window,returned)=db(s.state.workspace.root(),move|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let event=|kind:&str,data:Value|->Result<i64> {
            let local:i64=tx.query_row("UPDATE agent_delegates SET event_count=event_count+1 WHERE task_id=?1 AND id=?2 RETURNING event_count-1",params![task,agent],|r|r.get(0))?;
            agent_service::append_event_tx(&tx,&task,"subagent/event",&json!({"agent_id":agent,"agent_seq":local,"turn":turn,"event_type":kind,"event":data}),Some("append"))
        };
        event("tool/call",json!({"actor":"worker","turn":turn,"step":step,"flowNodeId":request_id,
            "callId":call.call_id,"name":call.name,"arguments":call.arguments}))?;
        let output=(||->Result<Value> {
            let args:Value=serde_json::from_str(&call.arguments)?;
            let id=next.task_node_id.as_deref().context("delegate has no task node")?;
            let mut node=tree::get(&tx,&task,id)?;
            ensure!(node.agent_id.as_deref()==Some(&agent) && node.status=="running","delegate task is not running");
            tree::validate_return(&tx,&task,&node,&args)?;
            node.status=args["status"].as_str().unwrap().into();node.result=Some(args.clone());
            let seq=tree::emit(&tx,&task,"task/node_returned",&mut node,turn)?;
            Ok(json!({"returned":id,"status":args["status"],"return_event_seq":seq}))
        })();
        let returned=output.is_ok();
        let result=output.unwrap_or_else(|error|json!({"error":format!("{error:#}")}));
        let seq=event("tool/result",json!({"actor":"worker","turn":turn,"step":step,"flowNodeId":request_id,
            "message":{"role":"tool","toolCallId":call.call_id,"isError":!returned,"content":[{"type":"text","text":result.to_string()}]},"meta":{"result":result}}))?;
        next.messages.push(sourced(json!({"role":"tool","tool_call_id":call.call_id,"content":json!({"event_seq":seq,"result":result}).to_string()}),&task,seq));
        write_window(&tx,&task,&scope,&next)?;
        tx.commit()?;
        Ok((next,returned))
    }).await?;
    *w = window;
    Ok(returned)
}

async fn child_material(s: &Session<'_>, parent: &Window, node: &Node) -> Result<Window> {
    let conn = agent_service::open_db(s.state.workspace.root())?;
    let chain = tree::path(
        &conn,
        s.task,
        node.parent_id.as_deref().context("child parent missing")?,
    )?;
    ensure!(chain.len() <= tree::MAX_DEPTH, "TASK_DEPTH_LIMIT");
    let mut pinned = BTreeSet::new();
    for ancestor in &chain {
        for id in ancestor.inputs["pinned"].as_array().into_iter().flatten() {
            pinned.insert(id.as_str().unwrap().to_owned());
        }
    }
    for id in node.inputs["pinned"].as_array().into_iter().flatten() {
        pinned.insert(id.as_str().unwrap().to_owned());
    }
    let mut ids = pinned.clone();
    for id in node.inputs["assets"].as_array().into_iter().flatten() {
        ids.insert(id.as_str().unwrap().to_owned());
    }
    let mut messages = vec![
        json!({"role":"system","content":s.prompt()}),
        json!({"role":"system","name":"task_path","content":json!(chain.iter().map(|n|json!({"node_id":n.node_id,"title":n.title,"goal":n.goal,"reason":n.spawned_from})).collect::<Vec<_>>()).to_string()}),
        json!({"role":"system","name":"task_node","content":serde_json::to_string(node)?}),
    ];
    for id in node.inputs["nodes"].as_array().into_iter().flatten() {
        let other = tree::get(&conn, s.task, id.as_str().unwrap())?;
        ensure!(other.result.is_some(), "declared input node has no return");
        ensure!(
            other.executor != "delegate" || other.review.as_deref() == Some("accepted"),
            "declared delegate input has not been accepted"
        );
        messages.push(json!({"role":"user","name":"task_input_node","content":json!({"node_id":other.node_id,"return_event_seq":other.returned_seq.or(other.last_seq),"result":other.result}).to_string(),"_frame_input":true}));
    }
    for seq in node.inputs["history"].as_array().into_iter().flatten() {
        let event = crate::session_history::load_events(
            &conn,
            s.task,
            &crate::session_history::EventQuery {
                exact: seq.as_i64(),
                ..Default::default()
            },
        )?
        .pop()
        .context("declared history event not found")?;
        messages.push(json!({"role":"user","name":"task_input_history","content":json!({"event_seq":event.seq,"kind":event.kind,"record":event.data()?}).to_string(),"_frame_input":true}));
    }
    drop(conn);
    for id in ids {
        let loaded = parent.messages.iter().find(|m| {
            m[crate::context_assets::REFERENCE]["asset_id"] == id
                && m["_asset_state"] != "reference_only"
                && m["_asset_state"] != "not_delivered"
        });
        let mut message = if let Some(message) = loaded {
            let mut message = message.clone();
            message["role"] = json!("user");
            message["name"] = json!("saved_asset");
            message.as_object_mut().unwrap().remove("tool_call_id");
            message
        } else {
            let result = load_saved_asset(s, &json!({"asset_id":id}), None).await?;
            crate::context_assets::tagged(
                json!({"role":"user","name":"saved_asset","content":result.to_string()}),
                &result["asset_reference"],
            )
        };
        message["_frame_input"] = json!(true);
        message["_pinned"] = json!(pinned.contains(&id));
        messages.push(message);
    }
    for message in &parent.messages {
        if message["_pinned"] == true
            || message[crate::context_assets::REFERENCE]["source_tool"] == "read_skill"
        {
            let mut message = message.clone();
            message["role"] = json!("user");
            message["name"] = json!("saved_asset");
            message.as_object_mut().unwrap().remove("tool_call_id");
            message["_pinned"] = json!(true);
            if !messages.iter().any(|m| m == &message) {
                messages.push(message);
            }
        }
    }
    let mut child = Window {
        messages,
        task_node_id: Some(node.node_id.clone()),
        loaded_tool_groups: tool_groups::from_inputs(s, &node.inputs)?,
        active_skills: parent.active_skills.clone(),
        active_skill_scopes: parent.active_skill_scopes.clone(),
        skill_turn: parent.skill_turn,
        token_calibration: parent.token_calibration.clone(),
        ..Default::default()
    };
    reconcile_context_policy(&mut child, s.turn, s.input_budget);
    let mut session = s.clone();
    session.scope = tree::scope(&node.node_id);
    ensure!(
        child.input_tokens(
            &session,
            &request(&session, &child.messages, &tools(&session, &child))
        ) + 4096
            < s.input_budget,
        "TASK_INPUTS_OVER_CAPACITY: choose smaller declared inputs; parent frame preserved"
    );
    Ok(child)
}

fn write_window(tx: &rusqlite::Transaction<'_>, task: &str, scope: &str, w: &Window) -> Result<()> {
    tx.execute("INSERT INTO agent_runtime_windows VALUES (?1,?2,?3) ON CONFLICT(task_id,scope) DO UPDATE SET state=excluded.state",params![task,scope,serde_json::to_string(w)?])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Harness {
        root: std::path::PathBuf,
        state: AgentServiceState,
        cancel: CancellationToken,
        w: Window,
        scope: String,
    }
    impl Harness {
        async fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("task-frames-{}", agent_service::uuid_like()));
            std::fs::create_dir_all(&root).unwrap();
            let state = agent_service::tests::test_state(&root, "127.0.0.1:1".parse().unwrap());
            let mut conn = agent_service::open_db(&root).unwrap();
            conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','','fake','running',0,0)",[]).unwrap();
            let tx = conn.transaction().unwrap();
            tree::ensure_root(&tx, "t", 1).unwrap();
            tx.commit().unwrap();
            let mut harness = Self {
                root,
                state,
                cancel: CancellationToken::new(),
                w: Window {
                    messages: vec![json!({"role":"system","content":Mode::Main.prompt()})],
                    ..Default::default()
                },
                scope: "main".into(),
            };
            harness.persist().await;
            harness
        }
        fn session(&self) -> Session<'_> {
            Session {
                plugin: None,
                state: &self.state,
                task: "t",
                model: "fake",
                turn: 1,
                cancel: &self.cancel,
                ban_execution: false,
                mode: Mode::Main,
                scope: self.scope.clone(),
                parent: None,
                material: None,
                input_budget: INPUT_BUDGET,
            }
        }
        async fn persist(&mut self) {
            save(&self.session(), &self.w).await.unwrap();
        }
        async fn call(&mut self, name: &str, args: Value) -> Value {
            let call = crate::model_stream::ToolCall {
                call_id: agent_service::uuid_like(),
                name: name.into(),
                arguments: args.to_string(),
            };
            self.w.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}));
            self.persist().await;
            let mut w = self.w.clone();
            if tool_groups::is_change(name) {
                tool_groups::execute(&self.session(), &mut w, &call, &tools(&self.session(), &self.w), "request_test", 1).await.unwrap();
            } else {
                let (scope, _) = execute(&self.session(), &mut w, &call, 1, "request_test", 1)
                    .await
                    .unwrap();
                self.scope = scope;
            }
            self.w = w;
            let conn = agent_service::open_db(&self.root).unwrap();
            let row = crate::session_history::load_events(
                &conn,
                "t",
                &crate::session_history::EventQuery {
                    kinds: &["tool/result"],
                    call_id: Some(&call.call_id),
                    newest: Some(1),
                    ..Default::default()
                },
            )
            .unwrap()
            .pop()
            .unwrap();
            row.data().unwrap()["meta"]["result"].clone()
        }
        async fn user(&mut self, text: &str) -> i64 {
            let seq = append(
                &self.session(),
                "user/message",
                json!({"turn":1,"message":{"role":"user","content":text}}),
                true,
            )
            .await
            .unwrap();
            self.w
                .messages
                .push(sourced(json!({"role":"user","content":text}), "t", seq));
            self.persist().await;
            seq
        }
        async fn evidence(&mut self) -> i64 {
            append(&self.session(),"tool/result",json!({"turn":1,"message":{"role":"tool","toolCallId":"check","isError":false},"meta":{"result":{"passed":true}}}),true).await.unwrap()
        }
        fn active(&self) -> Node {
            tree::active(&agent_service::open_db(&self.root).unwrap(), "t")
                .unwrap()
                .unwrap()
        }
        async fn plan(&mut self) -> String {
            self.call("plan_tasks",json!({"tasks":[{"title":"Page","goal":"Write a page","done_when":"Checker passes"}]})).await["tasks"][0]["node_id"].as_str().unwrap().to_owned()
        }
    }

    #[tokio::test]
    async fn started_frame_refreshes_persisted_pending_node_before_model_delivery() {
        let mut h = Harness::new().await;
        let child = h.plan().await;
        h.call("start_task", json!({"node_id":child,"briefing":"Write the assigned page"})).await;
        let mut w = h.w.clone();
        let snapshot = w.messages.iter_mut().find(|m| m["name"] == "task_node").unwrap();
        let mut node: Value = serde_json::from_str(snapshot["content"].as_str().unwrap()).unwrap();
        node["status"] = json!("pending");
        snapshot["content"] = json!(node.to_string());
        save(&h.session(), &w).await.unwrap();
        let raw: String = agent_service::open_db(&h.root).unwrap().query_row(
            "SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope=?1",
            [&h.scope], |r| r.get(0),
        ).unwrap();
        let mut restored: Window = serde_json::from_str(&raw).unwrap();
        notices(&h.session(), &mut restored).await.unwrap();
        let body = request(&h.session(), &restored.messages, &tools(&h.session(), &restored));
        let snapshot = body["messages"].as_array().unwrap().iter().find(|m| m["name"] == "task_node").unwrap();
        let node: Value = serde_json::from_str(snapshot["content"].as_str().unwrap()).unwrap();
        assert_eq!(node["node_id"], child);
        assert_eq!(node["status"], "running");
        assert_eq!(node["briefing"], "Write the assigned page");
        assert_eq!(h.active().node_id, child);
    }

    #[test]
    fn frame_compaction_preserves_originals_and_recent_pairs_without_orphaned_pinned_tools() {
        let original = "original user text\n".repeat(20000);
        let mut messages = vec![
            json!({"role":"system","content":"prompt"}),
            json!({"role":"user","content":original}),
            json!({"role":"user","name":"task_instruction","content":"scoped original"}),
        ];
        for i in 0..3 {
            messages.push(json!({"role":"assistant","tool_calls":[{"id":format!("call-{i}"),"type":"function","function":{"name":"read_file","arguments":"{}"}}]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("call-{i}"),"content":format!("material-{i}"),"_pinned":i==0}));
        }
        let retained = preserved_frame_messages(&messages);
        assert!(retained.iter().any(|m| m["content"] == original));
        assert!(retained.iter().any(|m| m["name"] == "task_instruction"));
        assert!(retained.iter().any(|m| m["name"] == "saved_asset"
            && m["content"] == "material-0"
            && m["role"] == "user"));
        assert_eq!(
            &retained[retained.len() - 4..],
            &messages[messages.len() - 4..]
        );
        for (index, message) in retained
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "tool")
        {
            assert!(
                retained[index - 1]["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|call| call["id"] == message["tool_call_id"])
            );
        }
    }

    #[tokio::test]
    async fn soft_pressure_warns_once_without_starting_hard_compression() {
        let mut h = Harness::new().await;
        h.w.messages.push(json!({"role":"assistant","content":""}));
        let session = h.session();
        let mut w = h.w.clone();
        let definitions = tools(&session, &w);
        while w.input_tokens(&session, &request(&session, &w.messages, &definitions))
            < session.input_budget * 65 / 100
        {
            let mut content = w.messages.last_mut().unwrap()["content"]
                .as_str()
                .unwrap()
                .to_owned();
            content.push_str(&"work ".repeat(1024));
            w.messages.last_mut().unwrap()["content"] = json!(content);
        }
        auto_compact(&session, &mut w, 1, &definitions)
            .await
            .unwrap();
        auto_compact(&session, &mut w, 2, &definitions)
            .await
            .unwrap();
        assert_eq!(
            w.messages
                .iter()
                .filter(|m| m["name"] == "task_context_pressure")
                .count(),
            1
        );
        let conn = agent_service::open_db(&h.root).unwrap();
        let warnings:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='context/split_reminder'",[],|r|r.get(0)).unwrap();
        assert_eq!(warnings, 1);
        let compressed:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='context/compression_start'",[],|r|r.get(0)).unwrap();
        assert_eq!(compressed, 0);
    }

    #[tokio::test]
    async fn protected_material_above_hard_threshold_does_not_generate_empty_summaries() {
        let h=Harness::new().await;let session=h.session();let mut w=h.w.clone();
        w.messages.push(json!({"role":"user","name":"saved_asset","_frame_input":true,"_pinned":true,"content":""}));
        let definitions=tools(&session,&w);
        while w.input_tokens(&session,&request(&session,&w.messages,&definitions))<session.input_budget*88/100 {
            let mut content=w.messages.last().unwrap()["content"].as_str().unwrap().to_owned();content.push_str(&"fixed ".repeat(1024));
            w.messages.last_mut().unwrap()["content"]=json!(content);
        }
        let original=w.messages.clone();
        auto_compact(&session,&mut w,1,&definitions).await.unwrap();
        auto_compact(&session,&mut w,2,&definitions).await.unwrap();
        assert_eq!(&w.messages[..original.len()],original.as_slice());
        let conn=agent_service::open_db(&h.root).unwrap();
        let summaries:i64=conn.query_row("SELECT count(*) FROM agent_task_events WHERE task_id='t' AND kind='context/compression_start'",[],|r|r.get(0)).unwrap();assert_eq!(summaries,0);
    }

    #[tokio::test]
    async fn branch_returns_to_exact_parent_and_evidence_has_node_ownership() {
        let mut h = Harness::new().await;
        h.call("load_tool_group",json!({"group":"files"})).await;
        let id = h.plan().await;
        h.call(
            "start_task",
            json!({"node_id":id,"briefing":"Use the outline"}),
        )
        .await;
        assert_eq!(h.active().node_id, id);
        assert!(h.w.loaded_tool_groups.is_empty(), "unselected parent groups must not enter a new child");
        h.call("load_tool_group",json!({"group":"code_rust"})).await;
        h.call("branch_task",json!({"title":"Repair","goal":"Repair shared CSS","done_when":"Check passes","reason":"Missing class","briefing":"Fix only the missing class","inputs":{"tool_groups":["execution"]}})).await;
        assert_eq!(h.w.loaded_tool_groups,BTreeSet::from(["execution".into()]));
        let branch = h.active().node_id;
        assert_ne!(branch, id);
        let parent: Window = serde_json::from_str(
            &agent_service::open_db(&h.root)
                .unwrap()
                .query_row(
                    "SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope=?1",
                    [tree::scope(&id)],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        let proof = h.evidence().await;
        let answer = h
            .call(
                "return_task",
                json!({"status":"done","result":"CSS repaired","evidence":[proof]}),
            )
            .await;
        assert!(answer.get("error").is_none(), "{answer}");
        assert_eq!(h.active().node_id, id);
        assert_eq!(
            &h.w.messages[..parent.messages.len()],
            parent.messages.as_slice()
        );
        assert_eq!(h.w.messages.last().unwrap()["name"], "task_child_return");
        assert_eq!(h.w.loaded_tool_groups,BTreeSet::from(["code_rust".into()]));
        let conn = agent_service::open_db(&h.root).unwrap();
        let owner: String = conn
            .query_row(
                "SELECT node_id FROM agent_task_events WHERE task_id='t' AND seq=?1",
                [proof],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, branch);
        let return_seq = tree::get(&conn, "t", &branch).unwrap().last_seq.unwrap();
        drop(conn);
        assert!(
            h.call(
                "return_task",
                json!({"status":"done","result":"Page verified","evidence":[proof]})
            )
            .await["error"]
                .as_str()
                .unwrap()
                .contains("UNRELATED")
        );
        assert!(
            h.call(
                "return_task",
                json!({"status":"done","result":"Page verified","evidence":[return_seq]})
            )
            .await
            .get("error")
            .is_none()
        );
        assert_eq!(h.active().node_id, tree::ROOT);
        assert_eq!(h.w.loaded_tool_groups,BTreeSet::from(["files".into()]));
    }

    #[tokio::test]
    async fn pending_originals_block_return_and_scoped_instruction_reaches_new_siblings() {
        let mut h = Harness::new().await;
        let id = h.plan().await;
        h.call("start_task", json!({"node_id":id,"briefing":"Page ten"}))
            .await;
        let original = "  从第 11 页开始加页脚\n".repeat(20000);
        let seq = h.user(&original).await;
        let proof = h.evidence().await;
        assert!(
            h.call(
                "return_task",
                json!({"status":"done","result":"ten done","evidence":[proof]})
            )
            .await["error"]
                .as_str()
                .unwrap()
                .contains("UNSCOPED")
        );
        assert!(
            h.call(
                "split_task",
                json!({"title":"part","completed":{"result":"done","evidence":[proof]},"tasks":[]})
            )
            .await["error"]
                .as_str()
                .unwrap()
                .contains("UNSCOPED")
        );
        h.call(
            "scope_message",
            json!({"instruction_id":format!("u{seq}"),"node_id":"n0"}),
        )
        .await;
        let mut notified = h.w.clone();
        notices(&h.session(), &mut notified).await.unwrap();
        h.w = notified;
        h.persist().await;
        h.call(
            "return_task",
            json!({"status":"done","result":"ten done","evidence":[proof]}),
        )
        .await;
        let next = h.plan().await;
        h.call(
            "start_task",
            json!({"node_id":next,"briefing":"Page eleven"}),
        )
        .await;
        let conn = agent_service::open_db(&h.root).unwrap();
        let instructions = tree::instructions(&conn, "t", &next).unwrap();
        assert_eq!(instructions[0]["original"], original);
        drop(conn);
        let mut w = h.w.clone();
        notices(&h.session(), &mut w).await.unwrap();
        let prefix = w.messages.clone();
        notices(&h.session(), &mut w).await.unwrap();
        assert_eq!(w.messages, prefix);
    }

    #[tokio::test]
    async fn split_preserves_originals_inputs_returns_and_native_pair_with_disjoint_sources() {
        let mut h = Harness::new().await;
        h.call("load_tool_group",json!({"group":"files"})).await;
        let seq = h.user("all original user text").await;
        h.call(
            "scope_message",
            json!({"instruction_id":format!("u{seq}"),"node_id":"n0"}),
        )
        .await;
        let mut notified = h.w.clone();
        notices(&h.session(), &mut notified).await.unwrap();
        h.w = notified;
        h.persist().await;
        h.w.messages.push(json!({"role":"user","name":"saved_asset","content":"design rules","_frame_input":true,"_pinned":true}));
        h.persist().await;
        let proof = h.evidence().await;
        let one=h.call("split_task",json!({"title":"first","completed":{"result":"first done","evidence":[proof]},"tasks":[]})).await;
        assert!(one.get("error").is_none(), "{one}");
        let old = one["completed_node_id"].as_str().unwrap();
        let proof2 = h.evidence().await;
        let two=h.call("split_task",json!({"title":"second","completed":{"result":"second done","evidence":[proof2]},"tasks":[]})).await;
        assert!(two.get("error").is_none(), "{two}");
        assert_eq!(h.w.loaded_tool_groups,BTreeSet::from(["files".into()]));
        let conn = agent_service::open_db(&h.root).unwrap();
        let first = tree::get(&conn, "t", old).unwrap();
        let second = tree::get(&conn, "t", two["completed_node_id"].as_str().unwrap()).unwrap();
        assert!(
            first.source.unwrap()["through_seq"].as_i64().unwrap()
                < second.source.unwrap()["after_seq"].as_i64().unwrap()
        );
        assert!(
            h.w.messages
                .iter()
                .any(|m| m["content"] == "all original user text")
        );
        assert!(h.w.messages.iter().any(|m| m["content"] == "design rules"));
        assert_eq!(
            h.w.messages
                .iter()
                .filter(|m| m["name"] == "task_child_return")
                .count(),
            2
        );
        assert!(h.w.messages.iter().any(|m| m["name"] == "task_instruction"));
        let last = h.w.messages.last().unwrap();
        let call_id = last["tool_call_id"].as_str().unwrap();
        assert!(
            h.w.messages[h.w.messages.len() - 2]["tool_calls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["id"] == call_id)
        );
        let history = crate::session_history::load_events(
            &conn,
            "t",
            &crate::session_history::EventQuery {
                node_id: Some(old),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(history.iter().any(|e| e.seq == proof));
        assert!(!history.iter().any(|e| e.seq == proof2));
    }

    #[tokio::test]
    async fn needs_user_returns_to_parent_and_rewind_restores_the_whole_branch_stack() {
        let mut h = Harness::new().await;
        h.call("load_tool_group",json!({"group":"files"})).await;
        let id = h.plan().await;
        h.call("start_task", json!({"node_id":id,"briefing":"work"}))
            .await;
        h.call("load_tool_group",json!({"group":"browser"})).await;
        let conn = agent_service::open_db(&h.root).unwrap();
        let raw: String = conn
            .query_row(
                "SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        crate::rewind::snapshot(&conn, "t", 2, Some(&raw)).unwrap();
        drop(conn);
        // Events in this simulated turn must be after the snapshot boundary.
        let mut conn = agent_service::open_db(&h.root).unwrap();
        let tx = conn.transaction().unwrap();
        agent_service::append_event_tx(
            &tx,
            "t",
            "user/message",
            &json!({"turn":2,"message":{"content":"follow up"}}),
            None,
        )
        .unwrap();
        tx.commit().unwrap();
        drop(conn);
        let seq = h.user("clarify").await;
        h.call(
            "scope_message",
            json!({"instruction_id":format!("u{seq}"),"node_id":id}),
        )
        .await;
        // Scope the simulated second-turn message too.
        let conn = agent_service::open_db(&h.root).unwrap();
        let pending = tree::instructions(&conn, "t", &id).unwrap();
        drop(conn);
        for instruction in pending {
            if instruction["scope_pending"] == true {
                h.call(
                    "scope_message",
                    json!({"instruction_id":instruction["id"],"node_id":id}),
                )
                .await;
            }
        }
        let result=h.call("return_task",json!({"status":"needs_user","result":"requires a plan change","question":"Replace the dependency?"})).await;
        assert!(result.get("error").is_none(), "{result}");
        assert_eq!(h.active().node_id, tree::ROOT);
        agent_service::open_db(&h.root)
            .unwrap()
            .execute("UPDATE agent_tasks SET status='completed' WHERE id='t'", [])
            .unwrap();
        crate::rewind::rewind(&h.root, "t", 2).unwrap();
        assert_eq!(h.active().node_id, id);
        let conn = agent_service::open_db(&h.root).unwrap();
        let windows: i64 = conn
            .query_row(
                "SELECT count(*) FROM agent_runtime_windows WHERE task_id='t'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(windows, 2);
        for (scope,expected) in [("main".to_owned(),"files"),(tree::scope(&id),"browser")] {
            let raw:String=conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope=?1",[scope],|r|r.get(0)).unwrap();
            let restored:Window=serde_json::from_str(&raw).unwrap();
            assert_eq!(restored.loaded_tool_groups,BTreeSet::from([expected.to_owned()]));
        }
    }

    #[tokio::test]
    async fn depth_limit_and_corrupt_active_stack_are_rejected() {
        let mut h = Harness::new().await;
        for _ in 0..tree::MAX_DEPTH {
            assert!(h.call("branch_task",json!({"title":"nested","goal":"repair","done_when":"check","reason":"dependency failed","briefing":"repair"})).await.get("error").is_none());
        }
        let deepest = h.active();
        let rejected=h.call("branch_task",json!({"title":"too deep","goal":"repair","done_when":"check","reason":"failed","briefing":"repair"})).await;
        assert!(rejected["error"].as_str().unwrap().contains("DEPTH"));
        assert_eq!(h.active().node_id, deepest.node_id);
        let mut conn = agent_service::open_db(&h.root).unwrap();
        let tx = conn.transaction().unwrap();
        let mut root = tree::get(&tx, "t", tree::ROOT).unwrap();
        root.status = "running".into();
        tree::emit(&tx, "t", "task/node_resumed", &mut root, 1).unwrap();
        tx.commit().unwrap();
        assert!(
            tree::active(&conn, "t")
                .err()
                .unwrap()
                .to_string()
                .contains("MULTIPLE_RUNNING")
        );
    }

    #[tokio::test]
    async fn changed_root_definition_rejects_old_completion_and_allows_replanning() {
        let mut h = Harness::new().await;
        let source = h.user("Create the first deliverable").await;
        let definition = json!({"objective":"First deliverable","constraints":[],"completion_conditions":["Checker passes"],"source_event_seqs":[source]});
        let proposal = crate::main_task_anchor::update(&h.root, "t", &definition).unwrap();
        let decision = crate::main_task_anchor::decide_definition(
            &h.root,
            "t",
            proposal["version"].as_i64().unwrap(),
            true,
            "user",
        )
        .unwrap();
        h.call(
            "scope_message",
            json!({"instruction_id":format!("u{source}"),"node_id":"n0"}),
        )
        .await;
        let id = h.plan().await;
        h.call(
            "start_task",
            json!({"node_id":id,"briefing":"first deliverable"}),
        )
        .await;
        let proof = h.evidence().await;
        let source = h.user("Change the deliverable").await;
        let mut changed = definition;
        changed["objective"] = json!("Different deliverable");
        changed["source_event_seqs"] = json!([source]);
        changed["expected_version"] = decision["definition_version"].clone();
        let proposal = crate::main_task_anchor::update(&h.root, "t", &changed).unwrap();
        crate::main_task_anchor::decide_definition(
            &h.root,
            "t",
            proposal["version"].as_i64().unwrap(),
            true,
            "user",
        )
        .unwrap();
        h.call(
            "scope_message",
            json!({"instruction_id":format!("u{source}"),"node_id":"n0"}),
        )
        .await;
        let rejected = h
            .call(
                "return_task",
                json!({"status":"done","result":"old work verified","evidence":[proof]}),
            )
            .await;
        assert!(
            rejected["error"]
                .as_str()
                .unwrap()
                .contains("TASK_DEFINITION_SUPERSEDED")
        );
        assert!(h.call("return_task",json!({"status":"blocked","result":"scope changed","open_issues":"Replan for the new definition"})).await.get("error").is_none());
        let next = h.plan().await;
        assert!(
            h.call(
                "start_task",
                json!({"node_id":next,"briefing":"new definition"})
            )
            .await
            .get("error")
            .is_none()
        );
    }

    #[tokio::test]
    async fn delegate_return_commits_native_pair_and_requires_parent_acceptance() {
        let mut h = Harness::new().await;
        let mut conn = crate::collaboration_jobs::db(&h.root).unwrap();
        conn.execute("INSERT INTO agent_delegates(id,task_id,turn,call_id,name,specification,status,created_at) VALUES ('worker','t',1,'spawn','work','{}','running',0)",[]).unwrap();
        let tx = conn.transaction().unwrap();
        let root = tree::get(&tx, "t", tree::ROOT).unwrap();
        let mut node = tree::planned(
            &root,
            &json!({"title":"worker","goal":"work","done_when":"check","executor":"delegate"}),
            0,
            "planned",
        )
        .unwrap();
        node.agent_id = Some("worker".into());
        node.status = "running".into();
        tree::emit(&tx, "t", "task/node_started", &mut node, 1).unwrap();
        tx.commit().unwrap();
        drop(conn);
        let mut worker = h.w.clone();
        worker.task_node_id = Some(node.node_id.clone());
        let mut session = h.session();
        session.scope = "delegate:worker".into();
        let proof = append(
            &session,
            "tool/result",
            json!({"message":{"isError":false},"meta":{"result":{"passed":true}}}),
            true,
        )
        .await
        .unwrap();
        let args = json!({"status":"done","result":"verified worker output","evidence":[proof]});
        let call = crate::model_stream::ToolCall {
            call_id: "worker-return".into(),
            name: "return_task".into(),
            arguments: args.to_string(),
        };
        worker.messages.push(json!({"role":"assistant","tool_calls":[{"id":call.call_id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}));
        save(&session, &worker).await.unwrap();
        assert!(
            delegate_return(&session, &mut worker, &call, "request", 1)
                .await
                .unwrap()
        );
        let conn = agent_service::open_db(&h.root).unwrap();
        let returned = tree::get(&conn, "t", &node.node_id).unwrap();
        assert_eq!(returned.status, "done");
        let stored:Window=serde_json::from_str(&conn.query_row("SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope='delegate:worker'",[],|r|r.get::<_,String>(0)).unwrap()).unwrap();
        assert_eq!(
            stored.messages.last().unwrap()["tool_call_id"],
            call.call_id
        );
        let owned: String = conn
            .query_row(
                "SELECT node_id FROM agent_task_events WHERE task_id='t' AND seq=?1",
                [proof],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owned, node.node_id);
        drop(conn);
        drop(session);
        let result = h
            .call(
                "return_task",
                json!({"status":"done","result":"parent complete","evidence":[returned.last_seq]}),
            )
            .await;
        assert!(result["error"].as_str().unwrap().contains("UNACCEPTED"));
        let mut conn = agent_service::open_db(&h.root).unwrap();
        let tx = conn.transaction().unwrap();
        let mut accepted = returned.clone();
        accepted.review = Some("accepted".into());
        tree::emit(&tx, "t", "task/node_reviewed", &mut accepted, 1).unwrap();
        tx.commit().unwrap();
        drop(conn);
        assert!(
            h.call(
                "return_task",
                json!({"status":"done","result":"parent complete","evidence":[returned.last_seq]})
            )
            .await
            .get("error")
            .is_none()
        );
    }

    #[tokio::test]
    async fn restart_recovers_active_child_and_pairs_interrupted_native_call() {
        let mut h = Harness::new().await;
        let id = h.plan().await;
        h.call(
            "start_task",
            json!({"node_id":id,"briefing":"resume in this page"}),
        )
        .await;
        h.call("load_tool_group",json!({"group":"files"})).await;
        h.w.messages.push(json!({"role":"assistant","tool_calls":[{"id":"interrupted-write","type":"function","function":{"name":"write_file","arguments":"{}"}}]}));
        h.persist().await;
        run_task(
            h.state.clone(),
            "t".into(),
            "fake".into(),
            "continue".into(),
            0,
            CancellationToken::new(),
            2,
            false,
        )
        .await
        .unwrap();
        assert_eq!(h.active().node_id, id);
        let conn = agent_service::open_db(&h.root).unwrap();
        let stored: Window = serde_json::from_str(
            &conn
                .query_row(
                    "SELECT state FROM agent_runtime_windows WHERE task_id='t' AND scope=?1",
                    [tree::scope(&id)],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        let recovered = stored
            .messages
            .iter()
            .find(|m| m["tool_call_id"] == "interrupted-write")
            .unwrap();
        assert_eq!(stored.loaded_tool_groups,BTreeSet::from(["files".into()]));
        assert!(
            recovered["content"]
                .as_str()
                .unwrap()
                .contains("interrupted")
        );
        let end_owner:String=conn.query_row("SELECT node_id FROM agent_task_events WHERE task_id='t' AND kind='turn/end' ORDER BY seq DESC LIMIT 1",[],|r|r.get(0)).unwrap();
        assert_eq!(end_owner, id);
    }
}

pub(super) async fn execute(
    s: &Session<'_>,
    w: &mut Window,
    call: &crate::model_stream::ToolCall,
    count: usize,
    request_id: &str,
    step: usize,
) -> Result<(String, bool)> {
    let parsed =
        serde_json::from_str::<Value>(&call.arguments).context("invalid tool arguments JSON");
    let prepare = async {
        ensure!(count == 1, "task-changing tools must be called alone");
        ensure!(
            s.is_root(),
            "only main execution may manage the inline task tree"
        );
        let args = parsed?;
        let conn = agent_service::open_db(s.state.workspace.root())?;
        let parent = tree::active(&conn, s.task)?.context("no running task")?;
        ensure!(parent.node_id == node_id(s), "TASK_STACK_FRAME_MISMATCH");
        if matches!(call.name.as_str(), "start_task" | "branch_task") {
            tree::validate_plan_version(&conn, s.task, &parent)?;
        }
        let child = match call.name.as_str() {
            "start_task" => {
                let mut child = tree::get(
                    &conn,
                    s.task,
                    args["node_id"].as_str().context("node_id required")?,
                )?;
                ensure!(
                    child.parent_id.as_deref() == Some(&parent.node_id)
                        && child.status == "pending",
                    "only pending direct children may start"
                );
                ensure!(
                    child.executor == "inline",
                    "use spawn_agent with task_node_id for delegate nodes"
                );
                tree::validate_plan_version(&conn, s.task, &child)?;
                if let Some(inputs) = args.get("inputs") {
                    child.inputs = tree::validate_inputs(inputs)?;
                }
                child.briefing = args["briefing"]
                    .as_str()
                    .context("briefing required")?
                    .to_owned();
                Some(child)
            }
            "branch_task" => {
                ensure!(
                    args["reason"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty()),
                    "branch reason required"
                );
                let mut child =
                    tree::planned(&parent, &args, tree::nodes(&conn, s.task)?.len(), "branch")?;
                child.briefing = args["briefing"]
                    .as_str()
                    .context("briefing required")?
                    .to_owned();
                Some(child)
            }
            _ => None,
        };
        drop(conn);
        let child = if let Some(node) = child {
            let frame = child_material(s, w, &node).await?;
            Some((node, frame))
        } else {
            None
        };
        Ok::<_, anyhow::Error>((args, child))
    }
    .await;
    let (task, old_scope, turn, current, mut next, name, arguments, call_id, request_id) = (
        s.task.to_owned(),
        s.scope.clone(),
        s.turn,
        node_id(s).to_owned(),
        w.clone(),
        call.name.clone(),
        call.arguments.clone(),
        call.call_id.clone(),
        request_id.to_owned(),
    );
    let (scope,window,end)=db(s.state.workspace.root(),move|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        agent_service::append_event_tx(&tx,&task,"tool/call",&json!({"actor":"agent","turn":turn,"step":step,"task_node_id":current,"flowNodeId":request_id,"callId":call_id,"name":name,"arguments":arguments}),None)?;
        tx.execute_batch("SAVEPOINT task_operation")?;
        let original_next=next.clone();
        let mut switch=None;
        let mut returned=None;
        let mut end=false;
        let mut split=false;
        let output=(|| -> Result<Value> {
            let (args,child)=prepare?;
            let mut parent=tree::active(&tx,&task)?.context("no running task")?;
            ensure!(parent.node_id==current,"TASK_STACK_CHANGED");
            match name.as_str() {
                "plan_tasks"=>tree::plan(&tx,&task,&parent,&args,turn),
                "scope_message"=>tree::scope_message(&tx,&task,&parent,&args,turn),
                "start_task"|"branch_task"=>{
                    let (mut child,frame)=child.context("child preparation missing")?;
                    if name=="branch_task" {tree::emit(&tx,&task,"task/node_planned",&mut child,turn)?;}
                    parent.status="suspended".into();tree::emit(&tx,&task,"task/node_suspended",&mut parent,turn)?;
                    child.status="running".into();tree::emit(&tx,&task,"task/node_started",&mut child,turn)?;
                    let result=json!({"started":child.node_id,"parent_node_id":parent.node_id});
                    switch=Some((tree::scope(&child.node_id),frame));
                    Ok(result)
                },
                "return_task"=>{
                    tree::validate_return(&tx,&task,&parent,&args)?;
                    parent.status=args["status"].as_str().unwrap().into(); parent.result=Some(args.clone());
                    let seq=tree::emit(&tx,&task,"task/node_returned",&mut parent,turn)?;
                    let child_return=json!({"role":"user","name":"task_child_return","content":json!({"node_id":parent.node_id,"return_event_seq":seq,"return":args}).to_string(),"source":{"seq":seq}});
                    if let Some(id)=parent.parent_id.as_deref() {
                        let mut ancestor=tree::get(&tx,&task,id)?;
                        let scope=tree::scope(id);
                        let raw:String=tx.query_row("SELECT state FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2",params![task,scope],|r|r.get(0)).context("suspended parent frame missing")?;
                        let mut frame:Window=serde_json::from_str(&raw)?;
                        frame.messages.push(child_return);
                        ancestor.status="running".into();tree::emit(&tx,&task,"task/node_resumed",&mut ancestor,turn)?;
                        switch=Some((scope,frame));
                    } else if parent.status=="needs_user" {
                        parent.status="running".into();tree::emit(&tx,&task,"task/node_resumed",&mut parent,turn)?;
                    }
                    end=parent.parent_id.is_none()||parent.status=="needs_user";
                    returned=Some(args.clone());
                    Ok(json!({"returned":parent.node_id,"status":args["status"],"parent_node_id":parent.parent_id}))
                },
                "split_task"=>{
                    tree::no_unscoped(&tx,&task)?;
                    ensure!(tree::path(&tx,&task,&current)?.len()<=tree::MAX_DEPTH,"TASK_DEPTH_LIMIT");
                    let through=crate::agent_service::next_event_seq(&tx,&task)?-2;
                    let after=parent.split_through.map(|v|v+1).unwrap_or(parent.first_seq.unwrap_or(0));
                    let mut done=tree::planned(&parent,&json!({"title":args["title"],"goal":args["title"],"done_when":"Completed and verified before this split"}),0,"planned")?;
                    done.source=Some(json!({"node_id":current,"after_seq":after,"through_seq":through}));
                    let mut result=args["completed"].clone();result["status"]=json!("done");
                    tree::validate_return(&tx,&task,&done,&result)?;
                    done.status="done".into();done.result=Some(result.clone());
                    let seq=tree::emit(&tx,&task,"task/node_returned",&mut done,turn)?;
                    parent.split_through=Some(through);tree::emit(&tx,&task,"task/node_revised",&mut parent,turn)?;
                    let planned=tree::plan(&tx,&task,&parent,&args,turn)?;
                    next.messages.retain(|m| m["role"]=="system" && m["name"]!="context_summary" || m["_frame_input"]==true || m["_pinned"]==true || matches!(m["name"].as_str(),Some("task_child_return"|"task_instruction")) || m["role"]=="user" && m["source"]["seq"].as_i64().is_some_and(|seq| {
                        crate::session_history::load_events(&tx,&task,&crate::session_history::EventQuery{exact:Some(seq),..Default::default()}).ok().and_then(|events|events.into_iter().next()).is_some_and(|e|matches!(e.kind.as_str(),"user/message"|"user/interjection"))
                    }));
                    for message in &mut next.messages {
                        if message["role"]=="tool" {
                            message["role"]=json!("user");message["name"]=json!("saved_asset");
                            message.as_object_mut().unwrap().remove("tool_call_id");
                        }
                    }
                    next.messages.push(json!({"role":"user","name":"task_child_return","content":json!({"node_id":done.node_id,"return_event_seq":seq,"return":result,"source":done.source}).to_string(),"source":{"seq":seq}}));
                    // Keep the exact native split call after the retained user/return records.
                    split=true;
                    next.pressure_warned=false;
                    Ok(json!({"completed_node_id":done.node_id,"planned":planned}))
                },
                _=>anyhow::bail!("unknown task tool")
            }
        })();
        let is_error=output.is_err();
        if is_error {
            tx.execute_batch("ROLLBACK TO task_operation")?;
            next=original_next;
            switch=None;returned=None;end=false;split=false;
        }
        tx.execute_batch("RELEASE task_operation")?;
        let result=output.unwrap_or_else(|error|json!({"error":format!("{error:#}")}));
        let seq=agent_service::append_event_tx(&tx,&task,"tool/result",&json!({"actor":"agent","turn":turn,"step":step,"task_node_id":current,"flowNodeId":request_id,
            "message":{"role":"tool","toolCallId":call_id,"isError":is_error,"content":[{"type":"text","text":result.to_string()}]},"meta":{"result":result}}),Some("append"))?;
        if split {
            // The assistant message was saved by model_round before this transaction.
            let raw:String=tx.query_row("SELECT state FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2",params![task,old_scope],|r|r.get(0))?;
            let original:Window=serde_json::from_str(&raw)?;
            let message=original.messages.iter().rev().find(|m|m["tool_calls"].as_array().is_some_and(|calls|calls.iter().any(|c|c["id"]==call_id))).context("native split call missing")?;
            next.messages.push(message.clone());
        }
        next.messages.push(sourced(json!({"role":"tool","tool_call_id":call_id,"content":json!({"event_seq":seq,"result":result}).to_string()}),&task,seq));
        if let Some(args)=&returned && end {
            let text=args["question"].as_str().unwrap_or_else(||args["result"].as_str().unwrap_or(""));
            let message=json!({"role":"assistant","content":text});
            let seq=agent_service::append_event_tx(&tx,&task,"assistant/message",&json!({"actor":"agent","turn":turn,"task_node_id":current,"message":message}),Some("append"))?;
            if let Some((_,frame))=switch.as_mut() {frame.messages.push(sourced(message,&task,seq));}
            else {next.messages.push(sourced(message,&task,seq));}
        }
        write_window(&tx,&task,&old_scope,&next)?;
        let (scope,frame)=if let Some((scope,frame))=switch {
            write_window(&tx,&task,&scope,&frame)?;
            if returned.is_some() {tx.execute("DELETE FROM agent_runtime_windows WHERE task_id=?1 AND scope=?2",params![task,old_scope])?;}
            (scope,frame)
        }else{(old_scope,next)};
        tree::active(&tx,&task)?;
        tx.commit()?;
        Ok((scope,frame,end))
    }).await?;
    *w = window;
    Ok((scope, end))
}
