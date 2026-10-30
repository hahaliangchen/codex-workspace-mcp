//! Authenticated remote-plugin sessions. Business identifiers, selections and
//! lifecycle rules live in the remote plugin; the host only owns chat identity.
use crate::{
    agent_service::AgentServiceState,
    plugin_runtime::PluginRuntime,
    skill_packages::remote::{self, Connection},
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

type Credentials = BTreeMap<String, (String, String, Arc<Connection>)>;
static CREDENTIALS: OnceLock<RwLock<Credentials>> = OnceLock::new();
static ACTIVE: OnceLock<RwLock<BTreeMap<String, Arc<Connection>>>> = OnceLock::new();
fn credentials() -> &'static RwLock<Credentials> {
    CREDENTIALS.get_or_init(Default::default)
}
fn active() -> &'static RwLock<BTreeMap<String, Arc<Connection>>> {
    ACTIVE.get_or_init(Default::default)
}
fn schema(root: &std::path::Path) -> Result<()> {
    crate::agent_service::open_db(root)?.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_plugin_sessions (
        task_id TEXT PRIMARY KEY REFERENCES agent_tasks(id) ON DELETE CASCADE,
        plugin_id TEXT NOT NULL, subject TEXT NOT NULL, service_url TEXT NOT NULL);",
    )?;
    Ok(())
}
// Resolve the authenticated conversation for a child execution frame too.
fn binding(root: &std::path::Path, task: &str) -> Result<Option<(String, String, String, String)>> {
    schema(root)?;
    Ok(crate::agent_service::open_db(root)?.query_row("WITH RECURSIVE lineage(id,depth) AS (
        SELECT ?1,0 UNION ALL SELECT t.parent_task_id,l.depth+1 FROM agent_tasks t JOIN lineage l ON t.id=l.id
        WHERE t.parent_task_id IS NOT NULL AND l.depth<32)
        SELECT b.task_id,b.plugin_id,b.subject,b.service_url FROM lineage l JOIN remote_plugin_sessions b ON b.task_id=l.id
        ORDER BY l.depth LIMIT 1", [task], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?)
}
fn bound(root: &std::path::Path, task: &str) -> Result<Option<(String, String, Arc<Connection>)>> {
    let Some((conversation, plugin, subject, url)) = binding(root, task)? else {
        return Ok(None);
    };
    let connection = active()
        .read()
        .unwrap()
        .get(&conversation)
        .cloned()
        .context("Plugin login expired; reconnect from its page")?;
    ensure!(
        connection.subject == subject && connection.url() == url,
        "Plugin session identity changed"
    );
    Ok(Some((conversation, plugin, connection)))
}
fn context(task: &str, conversation: &str) -> Value {
    json!({"conversation_id":conversation,"execution_id":task})
}
/// Vision analyses a call names by `analysis_id` / `analysis_ids` travel with it
/// as host evidence, so the plugin reads what the vision model actually said.
fn analysis_ids(args: &Value) -> Vec<String> {
    let mut ids = Vec::<String>::new();
    for id in args["analysis_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(args.get("analysis_id"))
        .filter_map(Value::as_str)
    {
        if !ids.iter().any(|known| known == id) {
            ids.push(id.to_owned());
        }
    }
    ids
}
fn save_binding(
    root: &std::path::Path,
    task: &str,
    plugin: &str,
    c: &Arc<Connection>,
) -> Result<()> {
    schema(root)?;
    crate::agent_service::open_db(root)?.execute(
        "INSERT INTO remote_plugin_sessions VALUES (?1,?2,?3,?4)",
        params![task, plugin, c.subject, c.url()],
    )?;
    active().write().unwrap().insert(task.into(), c.clone());
    Ok(())
}
fn check_binding(
    root: &std::path::Path,
    task: &str,
    plugin: &str,
    c: &Arc<Connection>,
) -> Result<String> {
    let (conversation, owner, subject, url) =
        binding(root, task)?.context("Remote plugin conversation not found")?;
    ensure!(
        owner == plugin && subject == c.subject && url == c.url(),
        "Conversation belongs to a different plugin or user"
    );
    crate::plugin_sessions::check(root, &conversation, plugin)?;
    active()
        .write()
        .unwrap()
        .insert(conversation.clone(), c.clone());
    Ok(conversation)
}
/// Generic capability response. The plugin can propose a plan using the host's
/// existing versioned confirmation UI; it cannot confirm it or change a goal.
fn tool_value(mut response: Value) -> Result<Value> {
    ensure!(
        response.is_object() && response.get("value").is_some(),
        "Plugin tool response requires value"
    );
    response
        .get_mut("value")
        .map(Value::take)
        .context("Plugin tool response requires value")
}
struct Transport;
impl crate::plugin_io::SessionIo for Transport {
    fn execute<'a>(
        &'a self,
        state: &'a AgentServiceState,
        task: &'a str,
        workspace: Arc<crate::tools::Workspace>,
        name: &'a str,
        args: Value,
    ) -> futures::future::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let catalog = state
                .tool_catalog
                .as_ref()
                .context("Tool registry unavailable")?;
            if let Some((conversation, plugin, c)) = bound(state.workspace.root(), task)? {
                if catalog.packages.owns_tool(&plugin, name) {
                    let prefix = format!("{}__", plugin.replace('-', "_"));
                    let raw = name.strip_prefix(&prefix).context("Tool owner mismatch")?;
                    let mut call_context = context(task, &conversation);
                    let ids = analysis_ids(&args);
                    if !ids.is_empty() {
                        call_context["analyses"] = json!(crate::image_analysis::analyses(
                            state.workspace.root(),
                            &[task, conversation.as_str()],
                            &ids,
                        )?);
                    }
                    let result = c.call_in(raw, args, call_context).await?;
                    sync(state, &conversation, &c).await?;
                    return tool_value(result);
                }
            }
            catalog.execute(workspace, name, args).await
        })
    }
    fn sync_attachments<'a>(
        &'a self,
        state: &'a AgentServiceState,
        task: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some((conversation, _, c)) = bound(state.workspace.root(), task)? {
                sync(state, &conversation, &c).await?
            }
            Ok(())
        })
    }
    fn read_asset<'a>(
        &'a self,
        state: &'a AgentServiceState,
        task: &'a str,
        reference: &'a Value,
        args: &'a Value,
    ) -> futures::future::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            use base64::Engine;
            let (conversation, _, c) =
                bound(state.workspace.root(), task)?.context("Remote plugin login required")?;
            let result = c
                .rpc(
                    "plugin/resource",
                    json!({"context":context(task,&conversation),"reference":reference}),
                )
                .await?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(result["data"].as_str().context("Resource data missing")?)?;
            crate::context_assets::remote_delivery(reference, &bytes, args)
        })
    }
    fn import_picture<'a>(
        &'a self,
        state: &'a AgentServiceState,
        task: &'a str,
        name: &'a str,
        media: &'a str,
        bytes: &'a [u8],
    ) -> futures::future::BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            use base64::Engine;
            let Some((conversation, _, c)) = bound(state.workspace.root(), task)? else {
                return Ok(None);
            };
            if !c.supports("attachments") {
                return Ok(None);
            };
            let result=c.rpc("plugin/importAttachment",json!({"context":context(task,&conversation),"name":name,"mediaType":media,"data":base64::engine::general_purpose::STANDARD.encode(bytes)})).await?;
            Ok(result["path"].is_string().then_some(result))
        })
    }
    fn review_brief<'a>(
        &'a self,
        state: &'a AgentServiceState,
        task: &'a str,
        tool: &'a str,
        args: &'a Value,
        document: &'a Value,
        images: &'a Value,
    ) -> futures::future::BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            let Some((conversation, plugin, c)) = bound(state.workspace.root(), task)? else {
                return Ok(None);
            };
            if !c.supports("visual_review") {
                return Ok(None);
            }
            let prefix = format!("{}__", plugin.replace('-', "_"));
            let raw = tool.strip_prefix(&prefix).unwrap_or(tool);
            let brief = c
                .rpc(
                    "plugin/reviewBrief",
                    json!({"context":context(task,&conversation),"tool":raw,"arguments":args,"document":document,"images":images}),
                )
                .await?;
            let Some(instructions) = brief["instructions"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
            else {
                return Ok(None);
            };
            // One subject per image; a single `subject` describes every image.
            let count = images.as_array().map_or(1, Vec::len);
            let subjects = (0..count)
                .map(|index| match brief["subjects"].as_array() {
                    Some(subjects) => subjects.get(index).cloned().unwrap_or(Value::Null),
                    None => brief["subject"].clone(),
                })
                .collect::<Vec<_>>();
            Ok(Some(
                json!({"instructions":instructions,"subjects":subjects}),
            ))
        })
    }
}
async fn sync(state: &AgentServiceState, task: &str, c: &Connection) -> Result<()> {
    use base64::Engine;
    if !c.supports("attachments") {
        return Ok(());
    }
    for (receipt, reference, media, bytes) in
        crate::attachments::pending_imports(state, task, task)?
    {
        let result=c.rpc("plugin/importAttachment",json!({"context":context(task,task),"attachmentId":reference["attachmentId"],"name":reference["name"],"mediaType":media,"data":base64::engine::general_purpose::STANDARD.encode(bytes)})).await?;
        if let Some(path) = result["path"].as_str() {
            crate::attachments::imported(state, &receipt, task, path)?;
        }
    }
    Ok(())
}
pub async fn request(
    runtime: &PluginRuntime,
    state: AgentServiceState,
    plugin: &str,
    method: &str,
    headers: HeaderMap,
    args: Value,
) -> Response {
    match handle(runtime, state, plugin, method, headers, args).await {
        Ok(value) => value,
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
    plugin: &str,
    method: &str,
    headers: HeaderMap,
    mut args: Value,
) -> Result<Response> {
    ensure!(args.is_object(), "Object request required");
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty())
        .context("Plugin login required")?;
    let hash = crate::visual_artifacts::hash(token.as_bytes());
    let registry = &state
        .tool_catalog
        .as_ref()
        .context("Tool registry unavailable")?
        .packages;
    if method == "connect" {
        // URL is administrator-configured, never selected by an untrusted page.
        let url = remote::configured_url(registry, plugin)?;
        let package = remote::connect_user(
            remote::Config {
                url,
                token: token.into(),
            },
            true,
        )
        .await?;
        ensure!(package.manifest.id == plugin, "Plugin identity mismatch");
        let c = remote::mount_user_package(runtime, registry, package)?;
        crate::plugin_io::register(plugin, Arc::new(Transport));
        let handle = crate::agent_service::uuid_like();
        let mut cache = credentials().write().unwrap();
        ensure!(
            cache.len() < 4096,
            "Too many plugin login connections; restart or disconnect unused sessions"
        );
        cache.insert(handle.clone(), (hash, plugin.into(), c.clone()));
        return Ok(Json(json!({"connection_handle":handle,"subject":c.subject})).into_response());
    }
    let handle = args["connection_handle"]
        .as_str()
        .context("Plugin connection required")?;
    let (saved, owner, c) = credentials()
        .read()
        .unwrap()
        .get(handle)
        .cloned()
        .context("Plugin connection expired; reconnect")?;
    ensure!(
        saved == hash && owner == plugin,
        "Plugin login changed; reconnect"
    );
    let identity = c.rpc("plugin/identity", json!({})).await?;
    let subject = identity["subject"]
        .as_str()
        .context("Plugin identity missing")?;
    ensure!(
        c.subject == crate::visual_artifacts::hash(format!("{}:{subject}", c.url()).as_bytes()),
        "Plugin identity changed"
    );
    if let Some(object) = args.as_object_mut() {
        object.remove("connection_handle");
    }
    let scope = registry
        .scopes(&std::collections::BTreeSet::from([plugin.into()]))
        .get(plugin)
        .copied()
        .context("Plugin disabled")?;
    args["package_scope"] = json!(scope);
    if method == "session" && args["method"] == "models" {
        return crate::plugin_sessions::handle(state, plugin, args).await;
    }
    if method == "session" && matches!(args["method"].as_str(), Some("open" | "create")) {
        // The plugin resolves its own document/history IDs to a conversation.
        let lookup = c
            .rpc(
                "plugin/session",
                json!({"method":"resolve","arguments":args,"context":{}}),
            )
            .await?;
        if let Some(task) = lookup["conversation_id"]
            .as_str()
            .or_else(|| args["task_id"].as_str())
        {
            let conversation = if binding(state.workspace.root(), task)?.is_none() {
                // An existing plugin-owned chat may predate remote bindings.
                // Only the authenticated plugin can authorize its adoption.
                crate::plugin_sessions::check(state.workspace.root(), task, plugin)?;
                c.rpc(
                    "plugin/session",
                    json!({"method":"open","arguments":args,"context":context(task,task)}),
                )
                .await?;
                save_binding(state.workspace.root(), task, plugin, &c)?;
                task.to_owned()
            } else {
                check_binding(state.workspace.root(), task, plugin, &c)?
            };
            let extra = c
                .rpc(
                    "plugin/session",
                    json!({"method":"open","arguments":args,"context":context(task,&conversation)}),
                )
                .await?;
            return Ok(Json(json!({"task_id":conversation,"plugin_state":extra})).into_response());
        }
        let response = crate::plugin_sessions::handle(
            state.clone(),
            plugin,
            json!({"method":"create","package_scope":scope}),
        )
        .await?;
        let created: Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 65536).await?)?;
        let task = created["task_id"]
            .as_str()
            .context("Session creation failed")?;
        let extra = c
            .rpc(
                "plugin/session",
                json!({"method":"bind","arguments":args,"context":context(task,task)}),
            )
            .await?;
        save_binding(state.workspace.root(), task, plugin, &c)?;
        return Ok(Json(json!({"task_id":task,"plugin_state":extra})).into_response());
    }
    let task = args["task_id"]
        .as_str()
        .context("task_id required")?
        .to_owned();
    let conversation = check_binding(state.workspace.root(), &task, plugin, &c)?;
    if method == "upload" {
        return Ok(Json(crate::attachments::encoded(&state, &task, &args)?).into_response());
    }
    if method == "attachment" {
        return Ok(Json(crate::attachments::preview(
            &state,
            &task,
            args["receiptId"].as_str().context("receiptId required")?,
        )?)
        .into_response());
    }
    if method == "session" {
        let operation = args["method"]
            .as_str()
            .context("Session method required")?
            .to_owned();
        let extra = if matches!(operation.as_str(), "prompt" | "status") {
            c.rpc(
                "plugin/session",
                json!({"method":operation,"arguments":args,"context":context(&task,&conversation)}),
            )
            .await?
        } else {
            Value::Null
        };
        let response = crate::plugin_sessions::handle(state, plugin, args).await?;
        if operation == "status" {
            let status = response.status();
            let mut value: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 1024 * 1024).await?,
            )?;
            value["plugin_state"] = extra;
            return Ok((status, Json(value)).into_response());
        }
        return Ok(response);
    }
    ensure!(method == "service", "Unknown remote plugin operation");
    Ok(Json(c.rpc("plugin/service",json!({"method":args["method"],"arguments":args["arguments"],"context":context(&task,&conversation)})).await?).into_response())
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
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }
    fn headers(user: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {user}").parse().unwrap());
        h
    }
    #[tokio::test]
    async fn authenticated_generic_sessions_preserve_context_and_isolate_users() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let received = calls.clone();
        let app=Router::new().route("/api/plugin/{method}",post(move|Path(method):Path<String>,h:HeaderMap,Json(body):Json<Value>| {
            let received=received.clone();async move {
                let user=h["authorization"].to_str().unwrap().strip_prefix("Bearer ").unwrap();
                let args=&body["payload"]["args"];
                let result=match method.as_str() {
                    "handshake"=>json!({"version":1,"subject":user,"state":"awaiting_confirmation","connection_id":format!("c-{user}"),"plugin":{"id":"notes-test","title":"Notes","skill":"skill:SKILL.md","pages":[],"tools":[{"name":"save","description":"Save note","inputSchema":{"type":"object","properties":{}}}]}}),
                    "confirm"=>json!({"state":"ready"}),"identity"=>json!({"subject":user}),
                    "session"=>{received.lock().unwrap().push(args.clone());json!({"document_key":"plugin-owned","project_id":"opaque-state"})},
                    "call"=>{received.lock().unwrap().push(args.clone());json!({"value":{"saved":true}})},
                    "service"=>json!({"echo":args}),_=>panic!("unexpected {method}")
                };
                Json(json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":true,"value":result}}))
            }
        })).route("/resource",get(||async{"---\nname: notes-test\ndescription: Write notes\n---\nUse save."}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = std::env::temp_dir().join(format!(
            "remote-session-{}",
            crate::agent_service::uuid_like()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let (runtime, catalog) = crate::skill_packages::test_runtime();
        let package = remote::connect_user(
            remote::Config {
                url: format!("http://{address}"),
                token: "admin".into(),
            },
            true,
        )
        .await
        .unwrap();
        remote::mount_user_package(&runtime, &catalog.packages, package).unwrap();
        let mut state = crate::agent_service::tests::test_state(&root, address);
        state.tool_catalog = Some(catalog.clone());
        let alice = value(
            handle(
                &runtime,
                state.clone(),
                "notes-test",
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
                "notes-test",
                "connect",
                headers("bob"),
                json!({}),
            )
            .await
            .unwrap(),
        )
        .await;
        let created=value(handle(&runtime,state.clone(),"notes-test","session",headers("alice"),json!({"connection_handle":alice["connection_handle"],"method":"create","document":"D1"})).await.unwrap()).await;
        let task = created["task_id"].as_str().unwrap();
        assert_eq!(created["plugin_state"]["project_id"], "opaque-state");
        assert!(handle(&runtime,state.clone(),"notes-test","session",headers("bob"),json!({"connection_handle":bob["connection_handle"],"task_id":task,"method":"events"})).await.is_err());
        assert!(handle(&runtime,state.clone(),"other","session",headers("alice"),json!({"connection_handle":alice["connection_handle"],"task_id":task,"method":"events"})).await.is_err());
        let db = crate::agent_service::open_db(&root).unwrap();
        db.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at,parent_task_id) VALUES ('child','','fake','draft',0,0,?1)",[task]).unwrap();
        let args = json!({"project_id":"business-string","text":"你好"});
        let result = crate::plugin_io::execute(
            &state,
            "child",
            state.workspace.clone(),
            "notes_test__save",
            args.clone(),
        )
        .await
        .unwrap();
        assert_eq!(result["saved"], true);
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call["tool"] == "save"
                    && call["arguments"] == args
                    && call["context"] == context("child", task))
        );
        // A plugin that does not declare visual_review is never asked for review instructions.
        assert!(
            crate::plugin_io::review_brief(
                &state,
                "child",
                "notes_test__save",
                &json!({}),
                &json!({}),
                &json!([])
            )
            .await
            .unwrap()
            .is_none()
        );
        // Re-authentication restores an existing binding without accepting a different subject.
        active().write().unwrap().remove(task);
        let opened=value(handle(&runtime,state.clone(),"notes-test","session",headers("alice"),json!({"connection_handle":alice["connection_handle"],"task_id":task,"method":"open"})).await.unwrap()).await;
        assert_eq!(opened["task_id"], task);
        runtime.shutdown().unwrap();
        server.abort();
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
    /// The real loop: a page tool renders an image, the plugin attaches its review
    /// instructions, analyze_images sends them as the vision system message, and
    /// the plugin's record tool receives the stored analysis as host evidence.
    #[tokio::test]
    async fn rendered_images_carry_plugin_review_instructions_into_vision_and_back_as_evidence() {
        use base64::Engine;
        use std::sync::atomic::{AtomicUsize, Ordering};
        const INSTRUCTIONS: &str = "Transcribe every visible text, then list defects, grade them against the outline, then conclude.";
        const VISION: &str = "Transcript: Cover title; Contents.\n```json\n{\"images\":[{\"image\":1,\"conclusion\":\"pass\"},{\"image\":2,\"conclusion\":\"pass\"}]}\n```";
        fn subject(page: &Value) -> String {
            format!("Page {page}: outline entry {page}.")
        }
        let plugin_calls = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
        let vision_bodies = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let rounds = Arc::new(AtomicUsize::new(0));
        let ids_in = |body: &Value, prefix: &str| -> Vec<String> {
            let pattern = regex::Regex::new(&format!(r"{prefix}\d+-\d+-\d+")).unwrap();
            let mut ids = Vec::<String>::new();
            for found in pattern.find_iter(&body.to_string()) {
                if !ids.iter().any(|id| id == found.as_str()) {
                    ids.push(found.as_str().to_owned());
                }
            }
            ids
        };
        let call = |name: &str, args: Value, n: usize| {
            json!({"role":"assistant","content":null,
            "tool_calls":[{"id":format!("c{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
        };
        let app = Router::new()
            .route("/api/plugin/{method}", post({
                let plugin_calls = plugin_calls.clone();
                move |Path(method): Path<String>, Json(body): Json<Value>| {
                    let plugin_calls = plugin_calls.clone();
                    async move {
                        let args = body["payload"]["args"].clone();
                        plugin_calls.lock().unwrap().push((method.clone(), args.clone()));
                        let result = match method.as_str() {
                            "handshake" => json!({"version":1,"subject":"alice","state":"awaiting_confirmation","connection_id":"c-alice",
                                "plugin":{"id":"review-lab","title":"Visual check","pages":[],"capabilities":["visual_review"],"tools":[
                                    {"name":"render","description":"Render pages","executor":"page","inputSchema":{"type":"object","properties":{"slides":{"type":"array","items":{"type":"integer"}}}}},
                                    {"name":"record","description":"Record a review","inputSchema":{"type":"object","properties":{"analysis_ids":{"type":"array"}}}}]}}),
                            "confirm" => json!({"state":"ready"}),
                            "identity" => json!({"subject":"alice"}),
                            "session" => json!({}),
                            "reviewBrief" => json!({"instructions":INSTRUCTIONS,
                                "subjects":args["images"].as_array().unwrap().iter().map(|meta| subject(&meta["slide"])).collect::<Vec<_>>()}),
                            "call" => json!({"value":{"recorded":true}}),
                            _ => panic!("unexpected {method}"),
                        };
                        Json(json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":true,"value":result}}))
                    }
                }
            }))
            .route("/v1/chat/completions", post({
                let vision_bodies = vision_bodies.clone();
                let rounds = rounds.clone();
                move |Json(body): Json<Value>| {
                    let vision_bodies = vision_bodies.clone();
                    let rounds = rounds.clone();
                    async move {
                        if body["model"] == "vision-only" {
                            vision_bodies.lock().unwrap().push(body);
                            return Json(json!({"choices":[{"message":{"role":"assistant","content":VISION},"finish_reason":"stop"}]}));
                        }
                        let n = rounds.fetch_add(1, Ordering::SeqCst);
                        let message = match n {
                            0 => call("create_task", json!({"goal":"Render and review page 1"}), n),
                            1 => call("review_lab__render", json!({"slides":[1,2]}), n),
                            2 => call("analyze_images", json!({"artifact_ids":ids_in(&body, "visual-"),"question":"I just changed the subtitle."}), n),
                            3 => call("review_lab__record", json!({"analysis_ids":ids_in(&body, "analysis-")}), n),
                            _ => json!({"role":"assistant","content":"Page 1 reviewed."}),
                        };
                        Json(json!({"choices":[{"finish_reason":if message["tool_calls"].is_array(){"tool_calls"}else{"stop"},"message":message}]}))
                    }
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = std::env::temp_dir().join(format!(
            "remote-review-{}",
            crate::agent_service::uuid_like()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let (runtime, catalog) = crate::skill_packages::test_runtime();
        let package = remote::connect_user(
            remote::Config {
                url: format!("http://{address}"),
                token: "admin".into(),
            },
            true,
        )
        .await
        .unwrap();
        remote::mount_user_package(&runtime, &catalog.packages, package).unwrap();
        let mut state = crate::agent_service::tests::test_state(&root, address);
        state.tool_catalog = Some(catalog.clone());
        state.visual_tool_settings.model = Some("vision-only".into());
        let alice = value(
            handle(
                &runtime,
                state.clone(),
                "review-lab",
                "connect",
                headers("alice"),
                json!({}),
            )
            .await
            .unwrap(),
        )
        .await;
        let created = value(handle(&runtime, state.clone(), "review-lab", "session", headers("alice"),
            json!({"connection_handle":alice["connection_handle"],"method":"create","document":"D1"})).await.unwrap()).await;
        let task = created["task_id"].as_str().unwrap().to_owned();
        crate::agent_service::open_db(&root)
            .unwrap()
            .execute(
                "UPDATE agent_tasks SET status='running',model='fake' WHERE id=?1",
                [&task],
            )
            .unwrap();
        crate::page_tools::claim(&task, "page-1").unwrap();
        let page = {
            let task = task.clone();
            tokio::spawn(async move {
                let mut png = Vec::new();
                image::DynamicImage::new_rgb8(8, 8)
                    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                    .unwrap();
                let data = base64::engine::general_purpose::STANDARD.encode(png);
                loop {
                    let taken =
                        crate::page_tools::poll(&task, "page-1", std::time::Duration::from_secs(2))
                            .await
                            .unwrap();
                    for call in taken["calls"].as_array().into_iter().flatten() {
                        let mut content = call["arguments"]["slides"].as_array().unwrap().iter()
                            .map(|slide| json!({"type":"image","mimeType":"image/png","data":data,"meta":{"slide":slide}}))
                            .collect::<Vec<_>>();
                        content.push(json!({"type":"text","text":"rendered"}));
                        crate::page_tools::reply(&task, "page-1", call["call_id"].as_str().unwrap(),
                            json!({"ok":true,"content":content,"document":{"revision":2,"stable":true}})).unwrap();
                    }
                }
            })
        };
        crate::single_agent::run_task(
            state.clone(),
            task.clone(),
            "fake".into(),
            "Review page 1".into(),
            12,
            tokio_util::sync::CancellationToken::new(),
            1,
            false,
        )
        .await
        .unwrap();
        page.abort();

        let calls = plugin_calls.lock().unwrap().clone();
        let (_, brief_request) = calls
            .iter()
            .find(|(method, _)| method == "reviewBrief")
            .expect("plugin was asked for review instructions");
        assert_eq!(brief_request["tool"], "render");
        assert_eq!(brief_request["arguments"], json!({"slides":[1,2]}));
        assert_eq!(
            brief_request["images"],
            json!([{"slide":1},{"slide":2}]),
            "each image says which page it shows"
        );
        assert_eq!(
            brief_request["document"],
            json!({"revision":2,"stable":true})
        );
        assert_eq!(brief_request["context"], context(&task, &task));
        assert!(brief_request.get("plan").is_none(), "there is no task-board plan to send");

        let vision = vision_bodies.lock().unwrap().clone();
        assert_eq!(vision.len(), 1);
        let messages = vision[0]["messages"].as_array().unwrap();
        assert_eq!(
            messages[0],
            json!({"role":"system","content":format!(
            "{INSTRUCTIONS}\n\nImage 1:\n{}\n\nImage 2:\n{}", subject(&json!(1)), subject(&json!(2)))}),
            "each image is reviewed against its own outline entry"
        );
        assert_eq!(
            messages[1]["content"][0],
            json!({"type":"text","text":"I just changed the subtitle."})
        );
        assert_eq!(
            messages[1]["content"][1],
            json!({"type":"text","text":"Image 1"})
        );
        assert_eq!(
            messages[1]["content"][3],
            json!({"type":"text","text":"Image 2"})
        );

        let (_, record) = calls
            .iter()
            .find(|(method, args)| method == "call" && args["tool"] == "record")
            .expect("record call");
        let evidence = &record["context"]["analyses"][0];
        assert_eq!(
            evidence["analysis"], VISION,
            "the plugin reads the vision model's own text"
        );
        assert_eq!(
            evidence["analysis_id"],
            record["arguments"]["analysis_ids"][0]
        );
        assert_eq!(evidence["images"][0]["source"]["meta"], json!({"slide":1}));
        assert_eq!(evidence["images"][1]["source"]["meta"], json!({"slide":2}));
        assert_eq!(
            evidence["images"][1]["source"]["arguments"],
            json!({"slides":[1,2]})
        );
        assert_eq!(evidence["images"][0]["source"]["document"]["revision"], 2);
        assert_eq!(
            evidence["images"][0]["source"]["tool"],
            "review_lab__render"
        );

        let forged = crate::plugin_io::execute(
            &state,
            &task,
            state.workspace.clone(),
            "review_lab__record",
            json!({"analysis_ids":["analysis-forged"]}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(forged.contains("Unknown analysis_id"), "{forged}");
        crate::page_tools::forget_task(&task);
        runtime.shutdown().unwrap();
        server.abort();
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn host_transport_contains_no_business_adapter() {
        for source in [
            include_str!("skill_packages/remote.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap(),
            include_str!("remote_sessions.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap(),
        ] {
            for forbidden in [
                "ppt/",
                "ppt-master",
                "PPT_PLUGIN_URL",
                "project_id",
                "template_id",
                "commit_design_spec",
                "init_project",
                "X-Ppt-",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "business coupling: {forbidden}"
                );
            }
        }
    }
}
