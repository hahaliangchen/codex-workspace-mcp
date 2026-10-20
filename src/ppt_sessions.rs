//! Authenticated standalone PPT pages. JWTs stay in memory; task ownership and
//! the PPT project binding survive restarts. Never replace a global user token.
use crate::{
    agent_service::AgentServiceState,
    plugin_runtime::PluginRuntime,
    skill_packages::{
        Registry,
        remote::{self, Connection},
    },
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock, RwLock},
};

type Credentials = BTreeMap<String, (String, Arc<Connection>)>;
static CREDENTIALS: OnceLock<RwLock<Credentials>> = OnceLock::new();
static ACTIVE: OnceLock<RwLock<BTreeMap<String, Arc<Connection>>>> = OnceLock::new();
fn credentials() -> &'static RwLock<Credentials> {
    CREDENTIALS.get_or_init(Default::default)
}
fn active() -> &'static RwLock<BTreeMap<String, Arc<Connection>>> {
    ACTIVE.get_or_init(Default::default)
}
fn binding(root: &std::path::Path, task: &str) -> Result<Option<(String, u64)>> {
    Ok(crate::agent_service::open_db(root)?
        .query_row(
            "SELECT subject,project_id FROM ppt_session_bindings WHERE task_id=?1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}
/// Tell the PPT service which conversation built a project, so its history
/// list can point at the Agent. Best effort: the host binding stays the
/// source of truth for tool calls.
async fn bind_chat(connection: &Connection, task: &str, project: u64) {
    if project == 0 {
        return;
    }
    if let Err(error) = connection
        .rpc("ppt/bindChat", json!({"project_id":project,"task_id":task}))
        .await
    {
        tracing::warn!(%error, task, project, "could not record PPT chat binding");
    }
}
pub fn project(root: &std::path::Path, task: &str) -> Result<Option<u64>> {
    Ok(binding(root, task)?.map(|(_, id)| id))
}
pub fn connection(root: &std::path::Path, task: &str) -> Result<Option<Arc<Connection>>> {
    if binding(root, task)?.is_none() {
        return Ok(None);
    }
    Ok(Some(active().read().unwrap().get(task).cloned().context(
        "PPT login connection expired; reconnect from the PPT page",
    )?))
}
pub async fn execute(
    state: &AgentServiceState,
    task: &str,
    name: &str,
    mut args: Value,
) -> Result<Option<Value>> {
    let Some((_, project)) = binding(state.workspace.root(), task)? else {
        return Ok(None);
    };
    let connection = connection(state.workspace.root(), task)?.unwrap();
    let raw = name
        .strip_prefix("ppt_master__")
        .context("tool is not owned by PPT")?;
    ensure!(
        state
            .tool_catalog
            .as_ref()
            .is_some_and(|c| c.packages.owns_tool("ppt-master", name)),
        "PPT tool unavailable"
    );
    if raw == "init_project" {
        ensure!(
            project == 0,
            "this conversation is already bound to a PPT project"
        );
    } else if matches!(raw, "task" | "cancel") {
        let found = connection
            .rpc("ppt/task", json!({"task_id":args["task_id"]}))
            .await?;
        let known = crate::agent_service::open_db(state.workspace.root())?
            .query_row(
                "SELECT 1 FROM ppt_session_jobs WHERE host_task_id=?1 AND remote_task_id=?2",
                params![task, args["task_id"].as_str().unwrap_or("")],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        ensure!(
            (project > 0 && found["project_id"].as_u64() == Some(project)) || known,
            "task belongs to a different PPT project"
        );
    } else if independent_skill_read(raw, &args) {
        ensure!(
            args["project_id"].as_u64().is_none_or(|id| id == project),
            "tool targets a different PPT project"
        );
        if project > 0 {
            args["project_id"] = json!(project);
        }
    } else {
        ensure!(project > 0, "initialize the PPT project first");
        ensure!(
            args["project_id"].as_u64().is_none_or(|id| id == project),
            "tool targets a different PPT project"
        );
        args["project_id"] = json!(project);
    }
    if project > 0 && !independent_skill_read(raw, &args) {
        sync_attachments(state, task, project, &connection).await?;
    }
    let result = connection.call(raw, args).await?;
    // Failed submissions can still have committed a remote task before the
    // database connection broke. Keep ownership so recovery can query it even
    // while init_project has not bound this conversation to a project yet.
    if let Some(job) = result["task_id"].as_str().filter(|id| !id.is_empty()) {
        crate::agent_service::open_db(state.workspace.root())?.execute(
            "INSERT OR IGNORE INTO ppt_session_jobs VALUES (?1,?2,?3)",
            params![task, job, raw],
        )?;
    }
    let initialized = if raw == "init_project" {
        Some(&result)
    } else if raw == "task" && result["tool"] == "init_project" {
        Some(&result["result"])
    } else {
        None
    };
    if let Some(initialized) = initialized
        && initialized["status"] == "succeeded"
        && project == 0
    {
        let project = initialized["data"]["project_id"]
            .as_u64()
            .or(initialized["project_id"].as_u64())
            .filter(|id| *id > 0)
            .context("PPT initialization returned no project ID")?;
        crate::agent_service::open_db(state.workspace.root())?.execute(
            "UPDATE ppt_session_bindings SET project_id=?2 WHERE task_id=?1 AND project_id=0",
            params![task, project],
        )?;
        bind_chat(&connection, task, project).await;
        sync_attachments(state, task, project, &connection).await?;
    }
    Ok(Some(result))
}

fn independent_skill_read(tool: &str, args: &Value) -> bool {
    match tool {
        "read_file" | "list_dir" => args["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("skill:")),
        "read_files" => args["paths"].as_array().is_some_and(|paths| {
            !paths.is_empty()
                && paths.len() <= 8
                && paths
                    .iter()
                    .all(|path| path.as_str().is_some_and(|s| s.starts_with("skill:")))
        }),
        _ => false,
    }
}
pub async fn request(
    runtime: &PluginRuntime,
    state: AgentServiceState,
    method: &str,
    headers: HeaderMap,
    args: Value,
) -> Response {
    match handle(runtime, state, method, headers, args).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}
async fn handle(
    runtime: &PluginRuntime,
    state: AgentServiceState,
    method: &str,
    headers: HeaderMap,
    mut args: Value,
) -> Result<Response> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty())
        .context("PPT login required")?;
    let token_hash = crate::visual_artifacts::hash(token.as_bytes());
    let registry: &Registry = &state
        .tool_catalog
        .as_ref()
        .context("tool registry unavailable")?
        .packages;
    if method == "connect" {
        let url = std::env::var("PPT_PLUGIN_URL")
            .ok()
            .or_else(|| {
                registry
                    .remote("ppt-master")
                    .ok()
                    .map(|r| r.url().to_owned())
            })
            .context(
                "Configure PPT_PLUGIN_URL on the Agent host (the PPT backend .../aiPpt/plugin URL)",
            )?;
        let package = remote::connect_user(
            remote::Config {
                url,
                token: token.to_owned(),
            },
            true,
        )
        .await?;
        let connection = remote::mount_user_template(runtime, registry, package)?;
        let handle = crate::agent_service::uuid_like();
        let mut cache = credentials().write().unwrap();
        if cache.len() >= 128 {
            if let Some(key) = cache.keys().next().cloned() {
                cache.remove(&key);
            }
        }
        cache.insert(handle.clone(), (token_hash, connection));
        return Ok(Json(
            json!({"connection_handle":handle,"subject":cache.get(&handle).unwrap().1.subject}),
        )
        .into_response());
    }
    let handle = args["connection_handle"]
        .as_str()
        .context("PPT connection required")?;
    let (saved_hash, connection) = credentials()
        .read()
        .unwrap()
        .get(handle)
        .cloned()
        .context("PPT connection expired; reconnect")?;
    ensure!(saved_hash == token_hash, "PPT login changed; reconnect");
    // The PPT service checks JWT expiry, revocation and user permissions on every request.
    let identity = connection.rpc("ppt/identity", json!({})).await?;
    let user = identity["user_id"]
        .as_u64()
        .filter(|id| *id > 0)
        .context("invalid PPT identity")?;
    ensure!(
        connection.subject
            == crate::visual_artifacts::hash(format!("{}:{user}", connection.url()).as_bytes()),
        "PPT identity changed"
    );
    let scope = registry
        .scopes(&std::collections::BTreeSet::from(["ppt-master".into()]))
        .get("ppt-master")
        .copied()
        .context("PPT plugin disabled")?;
    if method == "session" && args["method"] == "models" {
        args["package_scope"] = json!(scope);
        return crate::plugin_sessions::handle(state, "ppt-master", args).await;
    }
    if method == "session" && matches!(args["method"].as_str(), Some("open" | "create")) {
        let project = args["project_id"].as_u64().unwrap_or(0);
        if project > 0 {
            connection
                .rpc("ppt/project", json!({"project_id":project}))
                .await?;
        }
        let mut task = args["task_id"].as_str().map(str::to_owned);
        if task.is_none() && args["method"] == "open" && project > 0 {
            task=crate::agent_service::open_db(state.workspace.root())?.query_row("SELECT task_id FROM ppt_session_bindings WHERE subject=?1 AND project_id=?2 ORDER BY rowid DESC LIMIT 1",params![connection.subject,project],|r|r.get(0)).optional()?;
        }
        if let Some(task) = task {
            let (subject, bound) =
                binding(state.workspace.root(), &task)?.context("PPT conversation not found")?;
            ensure!(
                subject == connection.subject && (project == 0 || project == bound),
                "conversation belongs to a different user or PPT project"
            );
            active().write().unwrap().insert(task.clone(), connection);
            return Ok(Json(json!({"task_id":task,"project_id":bound})).into_response());
        }
        let response = crate::plugin_sessions::handle(
            state.clone(),
            "ppt-master",
            json!({"method":"create","package_scope":scope}),
        )
        .await?;
        let response: Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 65536).await?)?;
        let task = response["task_id"]
            .as_str()
            .context("session creation failed")?;
        crate::agent_service::open_db(state.workspace.root())?.execute(
            "INSERT INTO ppt_session_bindings VALUES (?1,?2,?3)",
            params![task, connection.subject, project],
        )?;
        bind_chat(&connection, task, project).await;
        active()
            .write()
            .unwrap()
            .insert(task.to_owned(), connection);
        return Ok(Json(json!({"task_id":task,"project_id":project})).into_response());
    }
    let task = args["task_id"]
        .as_str()
        .context("task_id required")?
        .to_owned();
    let (subject, project) =
        binding(state.workspace.root(), &task)?.context("PPT conversation not found")?;
    ensure!(
        subject == connection.subject,
        "conversation belongs to a different user"
    );
    crate::plugin_sessions::check(state.workspace.root(), &task, "ppt-master")?;
    active()
        .write()
        .unwrap()
        .insert(task.clone(), connection.clone());
    if method == "upload" {
        return Ok(Json(crate::attachments::encoded(&state, &task, &args)?).into_response());
    }
    if method == "attachment" {
        let receipt = args["receiptId"].as_str().context("receiptId required")?;
        return Ok(Json(crate::attachments::preview(&state, &task, receipt)?).into_response());
    }
    if method == "session" {
        args["package_scope"] = json!(scope);
        if args["method"] == "status" {
            let response = crate::plugin_sessions::handle(state, "ppt-master", args).await?;
            let status = response.status();
            let mut body: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 1024 * 1024).await?,
            )?;
            body["project_id"] = json!(project);
            return Ok((status, Json(body)).into_response());
        }
        if project > 0 && args["method"] == "prompt" {
            args["selection"]["project_id"] = json!(project);
        }
        return crate::plugin_sessions::handle(state, "ppt-master", args).await;
    }
    if method == "tool" {
        ensure!(
            args["name"] == "export_pptx",
            "only AI may modify slide sources"
        );
        let result = execute(
            &state,
            &task,
            "ppt_master__export_pptx",
            json!({"project_id":project}),
        )
        .await?
        .context("PPT binding missing")?;
        return Ok(Json(result).into_response());
    }
    ensure!(
        method == "service" && project > 0,
        "PPT project is not initialized"
    );
    let operation = args["method"].as_str().context("service method required")?;
    let mut input = args["arguments"].clone();
    ensure!(input.is_object(), "object arguments required");
    ensure!(
        input["project_id"].as_u64().is_none_or(|id| id == project),
        "service targets a different PPT project"
    );
    input["project_id"] = json!(project);
    if operation == "resource" {
        use base64::Engine;
        let path = input["path"].as_str().context("resource path required")?;
        let (mime, bytes) = connection
            .asset(
                "resource",
                &[("project_id", project.to_string()), ("path", path.into())],
            )
            .await?;
        return Ok(Json(json!({"mime_type":mime,"data_url":format!("data:{mime};base64,{}",base64::engine::general_purpose::STANDARD.encode(bytes))})).into_response());
    }
    ensure!(
        matches!(operation, "project" | "events"),
        "service method unavailable"
    );
    Ok(Json(connection.rpc(&format!("ppt/{operation}"), input).await?).into_response())
}

pub(crate) async fn sync_attachments(
    state: &AgentServiceState,
    task: &str,
    project: u64,
    connection: &Connection,
) -> Result<()> {
    use base64::Engine;
    for (receipt, reference, media, bytes) in
        crate::attachments::pending_imports(state, task, project)?
    {
        let result=connection.rpc("ppt/importAttachment",json!({"project_id":project,"attachmentId":reference["attachmentId"],"name":reference["name"],"mediaType":media,"data":base64::engine::general_purpose::STANDARD.encode(bytes)})).await?;
        let path = result["path"]
            .as_str()
            .context("PPT attachment import returned no path")?;
        crate::attachments::imported(state, &receipt, project, path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::Path,
        routing::{get, post},
    };
    async fn value(response: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap()
    }
    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {token}").parse().unwrap());
        h
    }
    #[tokio::test]
    async fn ppt_login_isolates_users_tasks_and_projects_without_global_token_replacement() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let seen = calls.clone();
        let imports = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let imported = imports.clone();
        let init_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app=Router::new().route("/api/ppt/{method}",post(move|Path(method):Path<String>,h:HeaderMap,Json(body):Json<Value>| {
            let seen=seen.clone();let imports=imported.clone(); let init_calls=init_calls.clone(); async move {
                let user=if h["authorization"]=="Bearer alice"{1}else{2};
                let args=&body["payload"]["args"];
                let result=match method.as_str() {
                    "handshake"=>json!({"version":1,"user_id":user,"connection_id":format!("c{user}"),"state":"awaiting_confirmation","plugin":{"id":"ppt-master","title":"PPT","attachment_extensions":["png","jpg","jpeg","webp","pdf","docx","txt","md"],"skill":"skill:SKILL.md","pages":[],"tools":[{"name":"init_project","description":"Initialize PPT","inputSchema":{"type":"object","properties":{}}},{"name":"write_file","description":"Write SVG","inputSchema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},{"name":"read_files","description":"Batch read","inputSchema":{"type":"object","properties":{"paths":{"type":"array","items":{"type":"string"}}},"required":["paths"]}}]}}),
                    "confirm"=>json!({"state":"ready"}),
                    "identity"=>json!({"user_id":user}),
                    "importAttachment"=>{assert_eq!(user,1);assert_eq!(args["project_id"],7);imports.lock().unwrap().push(args.clone());json!({"path":"sources/brief.md"})},
                    "project"=>json!({"project_id":args["project_id"],"slides":[]}),
                    "call"=>{seen.lock().unwrap().push(json!({"user":user,"args":args})); if args["tool"]=="init_project" {if init_calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst)==0 {json!({"status":"retryable_error","retryable":true,"task_id":"lost-ack-job"})} else {json!({"status":"accepted","task_id":"init-job"})}} else {json!({"status":"succeeded","project_id":args["project_id"]})}},
                    "task"=>if args["task_id"]=="lost-ack-job" {json!({"tool":"init_project","status":"failed","project_id":0,"result":{"status":"retryable_error","retryable":true}})} else {json!({"tool":"init_project","status":"succeeded","project_id":11,"result":{"status":"succeeded","data":{"project_id":11}}})},
                    _=>panic!("unexpected method {method}"),
                };
                Json(json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":true,"value":result}}))
            }
        })).route("/resource",get(||async{"---\nname: ppt-master\ndescription: PPT only\n---\nOperate SVG."}))
        .route("/v1/chat/completions",post({
            let phase=Arc::new(std::sync::atomic::AtomicUsize::new(0));
            move |Json(body):Json<Value>| {let phase=phase.clone();async move {
                let n=phase.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                assert!(body["messages"].to_string().contains("Authenticated PPT project binding: 7"));
                let (name,args)=if n==0 {("read_skill",json!({"name":"ppt-master"}))} else {("ppt_master__write_file",json!({"project_id":7,"path":"svg_output/01.svg"}))};
                let message=if n<2 {json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})} else {json!({"role":"assistant","content":"PPT updated"})};
                Json(json!({"choices":[{"message":message,"finish_reason":if n<2{"tool_calls"}else{"stop"}}]}))
            }}
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = std::env::temp_dir().join(format!(
            "ppt-auth-test-{}",
            crate::agent_service::uuid_like()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let (runtime, catalog) = crate::skill_packages::test_runtime();
        let package = remote::connect_user(
            remote::Config {
                url: format!("http://{address}"),
                token: "alice".into(),
            },
            true,
        )
        .await
        .unwrap();
        remote::mount_user_template(&runtime, &catalog.packages, package).unwrap();
        let mut state = crate::agent_service::tests::test_state(&root, address);
        state.tool_catalog = Some(catalog.clone());
        state.config = Some(Arc::new(tokio::sync::RwLock::new(serde_json::from_value(json!({
            "providers":{"test":{"url":"http://private/v1","api_key":"secret","model_map":{"m":"m"}}}
        })).unwrap())));
        let alice = value(
            handle(
                &runtime,
                state.clone(),
                "connect",
                headers("alice"),
                json!({}),
            )
            .await
            .unwrap(),
        )
        .await;
        let bob = value(
            handle(
                &runtime,
                state.clone(),
                "connect",
                headers("bob"),
                json!({}),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_ne!(alice["subject"], bob["subject"]);
        let models = value(
            handle(
                &runtime,
                state.clone(),
                "session",
                headers("alice"),
                json!({"connection_handle":alice["connection_handle"],"method":"models"}),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(models["models"][0]["id"], "m");
        assert!(
            handle(
                &runtime,
                state.clone(),
                "session",
                HeaderMap::new(),
                json!({"connection_handle":alice["connection_handle"],"method":"models"})
            )
            .await
            .is_err()
        );
        let a=value(handle(&runtime,state.clone(),"session",headers("alice"),json!({"connection_handle":alice["connection_handle"],"method":"open","project_id":7})).await.unwrap()).await;
        let b=value(handle(&runtime,state.clone(),"session",headers("bob"),json!({"connection_handle":bob["connection_handle"],"method":"open","project_id":9})).await.unwrap()).await;
        let task = a["task_id"].as_str().unwrap();
        assert_ne!(a["task_id"], b["task_id"]);
        assert!(handle(&runtime,state.clone(),"session",headers("bob"),json!({"connection_handle":bob["connection_handle"],"method":"events","task_id":task})).await.is_err());
        assert!(handle(&runtime,state.clone(),"session",headers("bob"),json!({"connection_handle":alice["connection_handle"],"method":"events","task_id":task})).await.is_err());
        let reopened=value(handle(&runtime,state.clone(),"session",headers("alice"),json!({"connection_handle":alice["connection_handle"],"method":"open","project_id":7})).await.unwrap()).await;
        assert_eq!(a["task_id"], reopened["task_id"]);
        use base64::Engine;
        let source = base64::engine::general_purpose::STANDARD.encode("# 演讲资料");
        let up=value(handle(&runtime,state.clone(),"upload",headers("alice"),json!({"connection_handle":alice["connection_handle"],"task_id":task,"name":"brief.md","data":source})).await.unwrap()).await;
        assert!(handle(&runtime,state.clone(),"upload",headers("bob"),json!({"connection_handle":bob["connection_handle"],"task_id":task,"name":"brief.md","data":source})).await.is_err());
        assert!(handle(&runtime,state.clone(),"upload",headers("alice"),json!({"connection_handle":alice["connection_handle"],"task_id":task,"name":"deck.pptx","data":source})).await.is_err());
        let mut db = crate::agent_service::open_db(&root).unwrap();
        let tx = db.transaction().unwrap();
        crate::attachments::admit(
            &tx,
            &root,
            task,
            1,
            "Update this PPT",
            &[json!({"type":"file","receiptId":up["receiptId"]})],
            Some("ppt-upload"),
            Some(
                crate::attachments::PPT_EXTENSIONS
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .as_slice(),
            ),
        )
        .unwrap();
        tx.commit().unwrap();
        let preview=value(handle(&runtime,state.clone(),"attachment",headers("alice"),json!({"connection_handle":alice["connection_handle"],"task_id":task,"receiptId":up["file"]["attachmentId"]})).await.unwrap()).await;
        assert!(preview["data_url"].as_str().unwrap().ends_with(&source));
        assert!(handle(&runtime,state.clone(),"attachment",headers("bob"),json!({"connection_handle":bob["connection_handle"],"task_id":task,"receiptId":up["file"]["attachmentId"]})).await.is_err());
        let output = execute(
            &state,
            task,
            "ppt_master__write_file",
            json!({"project_id":7,"path":"svg_output/01.svg"}),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output["status"], "succeeded");
        assert!(
            execute(
                &state,
                task,
                "ppt_master__write_file",
                json!({"project_id":9,"path":"svg_output/01.svg"})
            )
            .await
            .is_err()
        );
        crate::single_agent::run_task(
            state.clone(),
            task.to_owned(),
            "fake".into(),
            "Update this PPT".into(),
            4,
            tokio_util::sync::CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            imports.lock().unwrap().len(),
            1,
            "Each attachment imports once into its owning project"
        );
        assert!(
            crate::attachments::pending_imports(&state, task, 7)
                .unwrap()
                .is_empty()
        );
        let read = crate::attachments::read(
            &state,
            task,
            &json!({"attachment_id":up["file"]["attachmentId"]}),
        )
        .await
        .unwrap();
        assert_eq!(read["remote_path"], "sources/brief.md");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1]["user"], 1);
        assert_eq!(calls[0]["user"], 1);
        drop(calls);
        // Registering Bob never makes generic tool calls execute with Bob's JWT.
        assert!(
            catalog
                .execute(
                    state.workspace.clone(),
                    "ppt_master__write_file",
                    json!({"project_id":9,"path":"test"})
                )
                .await
                .is_err()
        );
        let fresh = value(
            handle(
                &runtime,
                state.clone(),
                "session",
                headers("alice"),
                json!({"connection_handle":alice["connection_handle"],"method":"create"}),
            )
            .await
            .unwrap(),
        )
        .await;
        let fresh_task = fresh["task_id"].as_str().unwrap();
        let definition = catalog
            .packages
            .tools(&std::collections::BTreeSet::from(["ppt-master".to_owned()]))
            .into_iter()
            .find(|d| d["function"]["name"] == "ppt_master__read_files")
            .unwrap();
        assert!(
            !definition["function"]["parameters"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("project_id"))
        );
        assert_eq!(
            execute(
                &state,
                fresh_task,
                "ppt_master__read_files",
                json!({"paths":["skill:references/strategist.md"]})
            )
            .await
            .unwrap()
            .unwrap()["status"],
            "succeeded"
        );
        assert!(
            execute(
                &state,
                fresh_task,
                "ppt_master__read_files",
                json!({"paths":["skill:references/strategist.md","templates/01.svg"]})
            )
            .await
            .is_err()
        );
        assert!(
            execute(
                &state,
                fresh_task,
                "ppt_master__read_files",
                json!({"project_id":7,"paths":["skill:references/strategist.md"]})
            )
            .await
            .is_err()
        );
        // A lost database acknowledgement can return a task ID before there is
        // a project binding. This conversation must still be able to query it.
        let lost_ack = execute(&state, fresh_task, "ppt_master__init_project", json!({}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lost_ack["status"], "retryable_error");
        let recovered = execute(
            &state,
            fresh_task,
            "ppt_master__task",
            json!({"task_id":"lost-ack-job"}),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(recovered["status"], "failed");
        assert_eq!(project(&root, fresh_task).unwrap(), Some(0));
        assert!(
            execute(
                &state,
                fresh_task,
                "ppt_master__task",
                json!({"task_id":"unknown-job"})
            )
            .await
            .is_err()
        );

        assert_eq!(
            execute(&state, fresh_task, "ppt_master__init_project", json!({}))
                .await
                .unwrap()
                .unwrap()["status"],
            "accepted"
        );
        execute(
            &state,
            fresh_task,
            "ppt_master__task",
            json!({"task_id":"init-job"}),
        )
        .await
        .unwrap();
        assert_eq!(project(&root, fresh_task).unwrap(), Some(11));
        active().write().unwrap().remove(task);
        assert!(
            connection(&root, task).is_err(),
            "restart requires re-authentication, never fallback to a shared credential"
        );
        runtime.shutdown().unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}
