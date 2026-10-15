//! Remote service adapter for DSH Connection envelopes. No plugin installation.
use super::*;
use crate::plugin_protocol::{NativeContributions, NativeTool, Page};
use std::time::Duration;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub url: String,
    pub token: String,
}
pub struct Connection {
    client: reqwest::Client,
    config: Config,
    connection_id: String,
    pub skill: String,
    pub subject: String,
    pub require_binding: bool,
}
impl Connection {
    pub fn authenticated(&self) -> Arc<Self> {
        Arc::new(Self {
            client: self.client.clone(),
            config: self.config.clone(),
            connection_id: self.connection_id.clone(),
            skill: self.skill.clone(),
            subject: self.subject.clone(),
            require_binding: false,
        })
    }
    pub fn url(&self) -> &str {
        &self.config.url
    }
    pub async fn call(&self, raw: &str, mut args: Value) -> Result<Value> {
        if matches!(raw, "project" | "task" | "cancel" | "events") {
            return self.rpc(&format!("ppt/{raw}"), args).await;
        }
        let project = args
            .as_object_mut()
            .context("object arguments required")?
            .remove("project_id")
            .unwrap_or(json!(0));
        self.rpc(
            "ppt/call",
            json!({"project_id":project,"tool":raw,"arguments":args}),
        )
        .await
    }
    pub async fn rpc(&self, method: &str, args: Value) -> Result<Value> {
        ensure!(
            !self.require_binding,
            "PPT service requires an authenticated user session"
        );
        let rpc_id = crate::agent_service::uuid_like();
        let request = self
            .client
            .post(format!("{}/api/{method}", self.config.url));
        let request = if matches!(method, "ppt/handshake" | "ppt/confirm") {
            request.timeout(Duration::from_secs(10))
        } else {
            request
        };
        let response: Value = request
            .bearer_auth(&self.config.token).header("X-Ppt-Connection",&self.connection_id)
            .json(&json!({"type":"client-request","rpcId":rpc_id,"method":method,"payload":{"args":args}}))
            .send().await.context("remote plugin request failed")?.error_for_status().context("remote plugin HTTP error")?.json().await?;
        ensure!(
            response["type"] == "server-response" && response["rpcId"] == rpc_id,
            "invalid remote response envelope"
        );
        ensure!(
            response["result"]["ok"] == true,
            "remote plugin: {}",
            response["result"]["error"]["message"]
                .as_str()
                .unwrap_or("request failed")
        );
        Ok(response["result"]["value"].clone())
    }
    pub async fn asset(&self, entry: &str, query: &[(&str, String)]) -> Result<(String, Vec<u8>)> {
        ensure!(
            !self.require_binding || matches!(entry, "page" | "module.js"),
            "PPT resources require an authenticated user session"
        );
        ensure!(
            matches!(entry, "page" | "module.js" | "resource"),
            "undeclared remote resource"
        );
        let response = self
            .client
            .get(format!("{}/{entry}", self.config.url))
            .query(query)
            .bearer_auth(&self.config.token)
            .header("X-Ppt-Connection", &self.connection_id)
            .send()
            .await
            .context("remote resource request failed")?
            .error_for_status()
            .context("remote resource HTTP error")?;
        ensure!(
            response.content_length().unwrap_or(0) <= 16 * 1024 * 1024,
            "remote resource too large"
        );
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let mut stream = response.bytes_stream();
        use futures::StreamExt;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            ensure!(
                bytes.len() + chunk.len() <= 16 * 1024 * 1024,
                "remote resource too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok((mime, bytes))
    }
}
pub(crate) fn mount_user_template(
    runtime: &PluginRuntime,
    registry: &Registry,
    package: Package,
) -> Result<Arc<Connection>> {
    let connection = package
        .remote
        .as_ref()
        .context("not a remote plugin")?
        .authenticated();
    ensure!(
        !connection.subject.is_empty(),
        "PPT server must report authenticated user_id"
    );
    ensure!(
        package.manifest.id == "ppt-master",
        "unexpected PPT plugin identity"
    );
    let _transition = registry.transition.lock().unwrap();
    if let Some(old) = registry.entries.read().unwrap().get("ppt-master") {
        ensure!(
            old.remote
                .as_ref()
                .is_some_and(|c| c.url() == connection.url()),
            "PPT service URL differs from registered plugin"
        );
        ensure!(old.scope.is_some(), "PPT plugin disabled");
        ensure!(
            old.manifest.tools == package.manifest.tools,
            "PPT tool manifest changed; reload plugin"
        );
        return Ok(connection);
    }
    let parent = runtime
        .snapshot()
        .into_iter()
        .find(|p| p.id == "skill-packages")
        .context("package service unavailable")?
        .scope;
    runtime.mount(parent, PackagePlugin(package))?;
    Ok(connection)
}
async fn connect(config: Config) -> Result<Package> {
    connect_user(config, false).await
}
pub(crate) async fn connect_user(mut config: Config, require_binding: bool) -> Result<Package> {
    config.url = config.url.trim_end_matches('/').to_owned();
    let url = reqwest::Url::parse(&config.url)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "invalid remote service URL"
    );
    let mut remote = Connection {
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(180))
            .build()?,
        config,
        connection_id: String::new(),
        skill: String::new(),
        subject: String::new(),
        require_binding: false,
    };
    let hello = remote.rpc("ppt/handshake", json!({"version":1})).await?;
    ensure!(
        hello["version"] == 1 && hello["state"] == "awaiting_confirmation",
        "unsupported remote handshake"
    );
    remote.connection_id = hello["connection_id"]
        .as_str()
        .context("missing connection ID")?
        .to_owned();
    if let Some(user) = hello["user_id"].as_u64().filter(|id| *id > 0) {
        remote.subject =
            crate::visual_artifacts::hash(format!("{}:{user}", remote.config.url).as_bytes());
    }
    let plugin = &hello["plugin"];
    let id = plugin["id"]
        .as_str()
        .context("missing plugin ID")?
        .to_owned();
    ensure!(identifier(&id), "invalid remote plugin ID");
    let confirmed = remote
        .rpc("ppt/confirm", json!({"connection_id":remote.connection_id}))
        .await?;
    ensure!(confirmed["state"] == "ready", "remote plugin not ready");
    let (_, body) = remote
        .asset(
            "resource",
            &[
                ("project_id", "0".into()),
                (
                    "path",
                    plugin["skill"]
                        .as_str()
                        .context("missing skill resource")?
                        .into(),
                ),
            ],
        )
        .await?;
    ensure!(body.len() <= 128000, "remote skill body too large");
    remote.skill = String::from_utf8(body)?;
    let front = remote
        .skill
        .strip_prefix("---\n")
        .and_then(|s| s.split_once("\n---"))
        .context("skill frontmatter required")?
        .0;
    let fields: Value = serde_yaml::from_str(front)?;
    ensure!(fields["name"] == id, "remote skill ID mismatch");
    let description = fields["description"]
        .as_str()
        .context("skill description required")?
        .to_owned();
    let prefix = id.replace('-', "_");
    let mut definitions = plugin["tools"]
        .as_array()
        .context("remote tools required")?
        .clone();
    for (name, description, schema) in [
        (
            "project",
            "Read the PPT project and current slide revisions",
            json!({"type":"object","properties":{"project_id":{"type":"integer","minimum":1}},"required":["project_id"]}),
        ),
        (
            "task",
            "Inspect an asynchronous PPT task before proceeding",
            json!({"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"]}),
        ),
        (
            "events",
            "Read persistent PPT project events",
            json!({"type":"object","properties":{"project_id":{"type":"integer","minimum":1},"after":{"type":"integer","minimum":0}},"required":["project_id"]}),
        ),
        (
            "cancel",
            "Cancel an asynchronous PPT task",
            json!({"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"]}),
        ),
    ] {
        definitions.push(json!({"name":name,"description":description,"inputSchema":schema}));
    }
    let mut names = Vec::new();
    let mut mapped = Vec::new();
    for mut definition in definitions {
        let raw = definition["name"]
            .as_str()
            .context("tool name required")?
            .to_owned();
        ensure!(
            !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid remote tool name"
        );
        let name = format!("{prefix}__{raw}");
        ensure!(
            name.len() <= 64 && !names.contains(&name),
            "duplicate or oversized tool name"
        );
        ensure!(
            definition["inputSchema"]["type"] == "object",
            "object tool schema required"
        );
        definition["name"] = json!(name);
        if !matches!(
            raw.as_str(),
            "init_project" | "project" | "task" | "cancel" | "events"
        ) {
            definition["inputSchema"]["properties"]["project_id"] = json!({"type":"integer","minimum":1,"description":"PPT project ID, never a filesystem path"});
            let required = definition["inputSchema"]["required"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut required = required;
            if !required.contains(&json!("project_id")) {
                required.push(json!("project_id"));
            }
            definition["inputSchema"]["required"] = json!(required);
        }
        remote.skill.push_str(&format!(
            "\nHost tool mapping: `{raw}` → `{name}`. Use the host name when calling.\n"
        ));
        names.push(name.clone());
        mapped.push((name, raw, definition));
    }
    remote.require_binding = require_binding;
    let remote = Arc::new(remote);
    let mut native = NativeContributions::default();
    for (name, raw, definition) in mapped {
        let connection = remote.clone();
        native.tools.insert(
            name,
            NativeTool {
                definition,
                permission: ToolPermission::WorkspaceWrite,
                handler: Arc::new(move |_, _, mut args| {
                    let connection = connection.clone();
                    let raw = raw.clone();
                    Box::pin(async move {
                        ensure!(
                            !connection.require_binding,
                            "PPT tools require an authenticated user session"
                        );
                        if matches!(raw.as_str(), "project" | "task" | "cancel" | "events") {
                            return connection.rpc(&format!("ppt/{raw}"), args).await;
                        }
                        let project = args
                            .as_object_mut()
                            .context("object arguments required")?
                            .remove("project_id")
                            .unwrap_or(json!(0));
                        connection
                            .rpc(
                                "ppt/call",
                                json!({"project_id":project,"tool":raw,"arguments":args}),
                            )
                            .await
                    })
                }),
            },
        );
    }
    let pages: Vec<Page> = serde_json::from_value(plugin["pages"].clone())?;
    ensure!(
        pages.iter().all(|p| identifier(&p.id)
            && matches!(
                (p.renderer.as_str(), p.entry.as_str()),
                ("html", "page") | ("react", "module.js")
            )),
        "invalid remote page declaration"
    );
    Ok(Package {
        root: PathBuf::new(),
        manifest: Manifest {
            version: 1,
            id,
            title: plugin["title"].as_str().unwrap_or("Remote plugin").into(),
            skill: plugin["skill"].as_str().unwrap().into(),
            inject: vec![],
            tools: names,
            command_tools: vec![],
            pages,
            attachment_extensions: plugin["attachment_extensions"].as_array().map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
        },
        description,
        scope: None,
        origin: Origin::External,
        native: Some(Arc::new(native)),
        remote: Some(remote),
    })
}
pub async fn install(
    runtime: &PluginRuntime,
    registry: &Registry,
    config: Config,
    persist: bool,
) -> Result<Value> {
    let package = connect(config.clone()).await?;
    let _transition = registry.transition.lock().unwrap();
    let id = package.manifest.id.clone();
    ensure!(
        !registry.entries.read().unwrap().contains_key(&id),
        "plugin already connected"
    );
    let parent = runtime
        .snapshot()
        .into_iter()
        .find(|p| p.id == "skill-packages")
        .context("package service unavailable")?
        .scope;
    let mut settings = settings()?;
    let scope = runtime.mount(parent, PackagePlugin(package))?;
    if persist {
        settings.remotes.insert(id.clone(), config);
        settings.enabled.insert(id.clone(), true);
        settings.removed.remove(&id);
        if let Err(error) = save_settings(&settings) {
            runtime.unmount(scope)?;
            registry.entries.write().unwrap().remove(&id);
            return Err(error);
        }
    }
    Ok(registry.catalog())
}
pub async fn restore(runtime: &PluginRuntime, registry: &Registry) -> Result<()> {
    let config = settings()?;
    for (id, remote) in config.remotes {
        match install(runtime, registry, remote, false).await {
            Ok(_) if config.enabled.get(&id) == Some(&false) => {
                set_enabled(runtime, registry, &id, false, false)?;
            }
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(plugin=%id,"remote plugin unavailable; reconnect from plugin settings")
            }
        }
    }
    Ok(())
}
pub async fn reconnect(runtime: &PluginRuntime, registry: &Registry, id: &str) -> Result<Value> {
    let old = registry
        .entries
        .read()
        .unwrap()
        .get(id)
        .cloned()
        .context("plugin not found")?;
    let config = old
        .remote
        .as_ref()
        .context("not a remote plugin")?
        .config
        .clone();
    let current = connect_user(
        config,
        old.remote.as_ref().is_some_and(|c| c.require_binding),
    )
    .await?;
    ensure!(current.manifest.id == id, "remote plugin identity changed");
    let _transition = registry.transition.lock().unwrap();
    let latest = registry
        .entries
        .read()
        .unwrap()
        .get(id)
        .cloned()
        .context("plugin removed while reconnecting")?;
    ensure!(
        latest.scope == old.scope,
        "plugin changed while reconnecting"
    );
    let parent = runtime
        .snapshot()
        .into_iter()
        .find(|p| p.id == "skill-packages")
        .context("package service unavailable")?
        .scope;
    if let Some(scope) = old.scope {
        runtime.unmount(scope)?;
        if let Err(error) = runtime.mount(parent, PackagePlugin(current)) {
            runtime.mount(parent, PackagePlugin(old))?;
            return Err(error);
        }
    } else {
        registry
            .entries
            .write()
            .unwrap()
            .insert(id.to_owned(), current);
    }
    Ok(registry.catalog())
}
pub async fn service(
    axum::extract::State(state): axum::extract::State<crate::agent_service::AgentServiceState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(args): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let result=async {
        let catalog=state.tool_catalog.context("tool registry unavailable")?;
        ensure!(args["package_scope"].is_u64() && catalog.packages.scopes(&std::collections::BTreeSet::from([id.clone()])).get(&id).copied()==args["package_scope"].as_u64(),"plugin page is unavailable or stale");
        let connection=catalog.packages.remote(&id)?;
        let method=args["method"].as_str().context("method required")?;
        let input=&args["arguments"];
        ensure!(input.is_object(),"object arguments required");
        if method=="resource" {
            use base64::Engine;
            let project=input["project_id"].as_u64().context("project_id required")?;
            let path=input["path"].as_str().context("resource path required")?;
            let (mime,bytes)=connection.asset("resource",&[("project_id",project.to_string()),("path",path.into())]).await?;
            return Ok(json!({"data_url":format!("data:{mime};base64,{}",base64::engine::general_purpose::STANDARD.encode(bytes))}));
        }
        ensure!(matches!(method,"project"|"task"|"events"),"page service method unavailable");
        connection.rpc(&format!("ppt/{method}"),input.clone()).await
    }.await;
    match result {
        Ok::<Value, anyhow::Error>(value) => axum::Json(value).into_response(),
        Err(error) => (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::Path,
        http::HeaderMap,
        routing::{get, post},
    };
    #[tokio::test]
    async fn remote_handshake_registers_namespaced_tools_and_both_page_adapters_without_files() {
        let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = calls.clone();
        let app=Router::new().route("/api/ppt/{method}",post(move|Path(method):Path<String>, headers:HeaderMap, Json(body):Json<Value>|{
            let captured=captured.clone(); async move {
                assert_eq!(headers["authorization"],"Bearer private-token");
                assert_eq!(body["type"],"client-request"); assert_eq!(body["method"],format!("ppt/{method}"));
                let value=match method.as_str() {
                    "handshake"=>json!({"version":1,"state":"awaiting_confirmation","connection_id":"connection-test","plugin":{"id":"ppt-master","title":"PPT","skill":"skill:SKILL.md","tools":[{"name":"write_file","description":"Write a project SVG","inputSchema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}],"pages":[{"id":"preview","title":"iframe","entry":"page","renderer":"html"},{"id":"workspace","title":"Module","entry":"module.js","renderer":"react"}]}}),
                    "confirm"=>{assert_eq!(body["payload"]["args"]["connection_id"],"connection-test");json!({"state":"ready"})},
                    "call"=>{assert_eq!(headers["x-ppt-connection"],"connection-test");assert_eq!(body["payload"]["args"],json!({"project_id":7,"tool":"write_file","arguments":{"path":"svg_output/01_title.svg"}}));json!({"status":"accepted","project_id":7,"task_id":"remote-task"})},
                    _=>panic!("unexpected method"),
                };
                captured.lock().unwrap().push(body.clone());
                Json(json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":true,"value":value}}))
            }
        })).route("/resource",get(||async{"---\nname: ppt-master\ndescription: PPT creation only\n---\nCall write_file."}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let package = connect(Config {
            url: format!("http://{address}"),
            token: "private-token".into(),
        })
        .await
        .unwrap();
        assert!(package.root.as_os_str().is_empty());
        assert_eq!(package.manifest.pages.len(), 2);
        let (runtime, catalog) = test_runtime();
        let parent = runtime
            .snapshot()
            .into_iter()
            .find(|p| p.id == "skill-packages")
            .unwrap()
            .scope;
        let scope = runtime.mount(parent, PackagePlugin(package)).unwrap();
        let read = catalog.packages.read("ppt-master").unwrap();
        assert!(
            read["content"]
                .as_str()
                .unwrap()
                .contains("ppt_master__write_file")
        );
        assert!(!read.to_string().contains("private-token"));
        assert!(
            !catalog
                .packages
                .catalog()
                .to_string()
                .contains("private-token")
        );
        let root = std::env::temp_dir().join(format!(
            "ppt-remote-test-{}",
            crate::agent_service::uuid_like()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = Arc::new(crate::tools::Workspace::new(&root).unwrap());
        let result = catalog
            .execute(
                workspace,
                "ppt_master__write_file",
                json!({"project_id":7,"path":"svg_output/01_title.svg"}),
            )
            .await
            .unwrap();
        assert_eq!(result["status"], "accepted");
        assert_eq!(calls.lock().unwrap().len(), 3);
        runtime.unmount(scope).unwrap();
        assert!(catalog.packages.read("ppt-master").is_err());
        assert!(!catalog.contains("ppt_master__write_file"));
        runtime.shutdown().unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}
