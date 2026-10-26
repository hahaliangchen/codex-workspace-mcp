//! Remote service adapter for DSH Connection envelopes. No plugin installation.
use super::*;
use crate::plugin_protocol::{NativeContributions, NativeTool, Page};
use std::{collections::BTreeSet, time::Duration};

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
    capabilities: BTreeSet<String>,
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
            capabilities: self.capabilities.clone(),
        })
    }
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }
    pub fn url(&self) -> &str {
        &self.config.url
    }
    /// Arguments are plugin-owned data and cross the transport unchanged.
    pub async fn call(&self, raw: &str, args: Value) -> Result<Value> {
        self.call_in(raw, args, json!({}))
            .await?
            .get("value")
            .cloned()
            .context("Plugin tool response requires value")
    }
    pub async fn call_in(&self, raw: &str, args: Value, context: Value) -> Result<Value> {
        self.rpc(
            "plugin/call",
            json!({"tool":raw,"arguments":args,"context":context}),
        )
        .await
    }
    pub async fn rpc(&self, method: &str, args: Value) -> Result<Value> {
        ensure!(
            !self.require_binding,
            "Plugin service requires an authenticated user session"
        );
        let rpc_id = crate::agent_service::uuid_like();
        let request = self
            .client
            .post(format!("{}/api/{method}", self.config.url));
        let request = if matches!(method, "plugin/handshake" | "plugin/confirm") {
            request.timeout(Duration::from_secs(10))
        } else {
            request
        };
        let response: Value = request
            .bearer_auth(&self.config.token).header("X-Plugin-Connection",&self.connection_id)
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
            "Plugin resources require an authenticated user session"
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
            .header("X-Plugin-Connection", &self.connection_id)
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
pub(crate) fn mount_user_package(
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
        "Plugin server must report an authenticated subject"
    );
    let id = &package.manifest.id;
    let _transition = registry.transition.lock().unwrap();
    if let Some(old) = registry.entries.read().unwrap().get(id) {
        ensure!(
            old.remote
                .as_ref()
                .is_some_and(|c| c.url() == connection.url()),
            "Plugin service URL differs from registered plugin"
        );
        ensure!(old.scope.is_some(), "Plugin disabled");
        ensure!(
            old.manifest.tools == package.manifest.tools,
            "Plugin tool manifest changed; reload plugin"
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
        capabilities: BTreeSet::new(),
    };
    let hello = remote.rpc("plugin/handshake", json!({"version":1})).await?;
    ensure!(
        hello["version"] == 1 && hello["state"] == "awaiting_confirmation",
        "unsupported remote handshake"
    );
    remote.connection_id = hello["connection_id"]
        .as_str()
        .context("missing connection ID")?
        .to_owned();
    if let Some(subject) = hello["subject"].as_str().filter(|s| !s.is_empty()) {
        remote.subject =
            crate::visual_artifacts::hash(format!("{}:{subject}", remote.config.url).as_bytes());
    }
    let plugin = &hello["plugin"];
    let id = plugin["id"]
        .as_str()
        .context("missing plugin ID")?
        .to_owned();
    ensure!(identifier(&id), "invalid remote plugin ID");
    let confirmed = remote
        .rpc(
            "plugin/confirm",
            json!({"connection_id":remote.connection_id}),
        )
        .await?;
    ensure!(confirmed["state"] == "ready", "remote plugin not ready");
    let skill_entry = plugin["skill"].as_str().unwrap_or_default();
    if !skill_entry.trim().is_empty() {
        match remote.asset("resource", &[("path", skill_entry.into())]).await {
            Ok((_, body)) => {ensure!(body.len() <= 128000, "remote skill body too large"); remote.skill = String::from_utf8(body)?;},
            Err(error) if error.downcast_ref::<reqwest::Error>().is_some_and(|e|e.status()==Some(reqwest::StatusCode::NOT_FOUND)) => {},
            Err(error) => return Err(error),
        }
    }
    let description = if !remote.skill.trim().is_empty() {
        let fields = super::frontmatter(&remote.skill)?;
        ensure!(fields["name"] == id, "remote skill ID mismatch");
        fields["description"].as_str().context("skill description required")?.to_owned()
    } else { plugin["description"].as_str().or_else(||plugin["title"].as_str()).unwrap_or("Remote plugin").to_owned() };
    remote.capabilities = plugin["capabilities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let prefix = id.replace('-', "_");
    let definitions = plugin["tools"]
        .as_array()
        .context("remote tools required")?
        .clone();
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
        let page = definition["executor"] == "page";
        ensure!(
            definition["executor"].is_null() || page || definition["executor"] == "service",
            "unknown tool executor"
        );
        // Tool input schemas belong to the plugin. Transport/session binding
        // must not inject business parameters into the model's contract.
        if !remote.skill.trim().is_empty() {
            remote.skill.push_str(&format!(
                "\nSource method: `{}.{raw}(args)`. Follow program_tool_headers for its JSON contract.\n", plugin["library"].as_str().map(str::to_owned).unwrap_or_else(||crate::agent_program::libraries::namespace(&id))
            ));
        }
        names.push(name.clone());
        mapped.push((name, raw, definition));
    }
    remote.require_binding = require_binding;
    let remote = Arc::new(remote);
    let mut native = NativeContributions::default();
    for (name, raw, definition) in mapped {
        let connection = remote.clone();
        let page_tool = definition["executor"] == "page";
        native.tools.insert(
            name,
            NativeTool {
                definition,
                permission: ToolPermission::WorkspaceWrite,
                handler: Arc::new(move |_, _, args| {
                    let connection = connection.clone();
                    let raw = raw.clone();
                    let page = page_tool;
                    Box::pin(async move {
                        // The Agent loop routes page tools to the task's page before this.
                        ensure!(!page, "this tool runs in the plugin page of an open task");
                        connection.call(&raw, args).await
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
            library: plugin.get("library").filter(|v| !v.is_null()).map(|v| {
                let name = v.as_str().context("plugin library must be a string")?;
                ensure!(crate::agent_program::libraries::valid_namespace(name), "invalid plugin library namespace");
                Ok::<_, anyhow::Error>(name.to_owned())
            }).transpose()?,
            title: plugin["title"].as_str().unwrap_or("Remote plugin").into(),
            skill: skill_entry.into(),
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
/// An administrator may register just a service address. User credentials are
/// supplied when its page connects, never persisted as shared plugin identity.
pub(crate) fn configured_url(registry: &Registry, id: &str) -> Result<String> {
    if let Ok(connection)=registry.remote(id) {return Ok(connection.url().to_owned())}
    let configured=settings()?;
    ensure!(configured.enabled.get(id)!=Some(&false) && !configured.removed.contains(id),"Plugin disabled");
    configured.remotes.get(id).map(|c|c.url.clone()).context("Plugin service address is not configured")
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
    let result = async {
        let catalog = state.tool_catalog.context("tool registry unavailable")?;
        ensure!(
            args["package_scope"].is_u64()
                && catalog
                    .packages
                    .scopes(&std::collections::BTreeSet::from([id.clone()]))
                    .get(&id)
                    .copied()
                    == args["package_scope"].as_u64(),
            "plugin page is unavailable or stale"
        );
        let connection = catalog.packages.remote(&id)?;
        let method = args["method"].as_str().context("method required")?;
        let input = &args["arguments"];
        ensure!(input.is_object(), "object arguments required");
        connection
            .rpc(
                "plugin/service",
                json!({"method":method,"arguments":input,"context":{}}),
            )
            .await
    }
    .await;
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
    async fn remote_plugins_may_omit_or_not_have_a_skill_resource() {
        for entry in [None,Some("skill:missing.md")] {
            let app=Router::new().route("/api/plugin/{method}",post(move|Path(method):Path<String>,Json(body):Json<Value>|async move {
                let value=if method=="handshake" {
                    let mut plugin=json!({"id":"plain-page","title":"No instructions","pages":[],"tools":[{"name":"ping","description":"Ping","inputSchema":{"type":"object"}}]});
                    if let Some(entry)=entry {plugin["skill"]=json!(entry)}
                    json!({"version":1,"subject":"user","state":"awaiting_confirmation","connection_id":"c","plugin":plugin})
                } else {assert_eq!(method,"confirm");json!({"state":"ready"})};
                Json(json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":true,"value":value}}))
            }));
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
            let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
            let package=connect(Config{url:format!("http://{address}"),token:"test".into()}).await.unwrap();
            assert!(package.remote.as_ref().unwrap().skill.is_empty(),"tool mapping must not fabricate missing MD");
            assert_eq!(package.manifest.tools,vec!["plain_page__ping"]);
            server.abort();
        }
    }
    #[tokio::test]
    async fn remote_contract_is_generic_and_preserves_business_arguments() {
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let received = captured.clone();
        let app=Router::new().route("/api/plugin/{method}",post(move|Path(method):Path<String>,headers:HeaderMap,Json(body):Json<Value>| {
            let received=received.clone();async move {
                assert_eq!(headers["authorization"],"Bearer test-secret");
                assert!(!headers.contains_key("x-ppt-connection"));
                assert_eq!(body["method"],format!("plugin/{method}"));
                let value=match method.as_str() {
                    "handshake"=>json!({"version":1,"subject":"user-a","state":"awaiting_confirmation","connection_id":"test-connection","plugin":{"id":"note-book","library":"notes","title":"Notes","skill":"skill:SKILL.md","pages":[],"tools":[{"name":"read_file","description":"Read a note","inputSchema":{"type":"object","properties":{"document":{"type":"string"}},"required":["document"],"additionalProperties":false},"outputSchema":{"type":"object","properties":{"content":{"type":"string"}},"required":["content"]}}]}}),
                    "confirm"=>json!({"state":"ready"}),
                    "call"=>{assert_eq!(headers["x-plugin-connection"],"test-connection");received.lock().unwrap().push(body["payload"]["args"].clone());json!({"value":{"content":"note text"}})},
                    _=>panic!("unexpected method {method}")
                };
                Json(json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":true,"value":value}}))
            }
        })).route("/resource",get(||async{"---\nname: note-book\ndescription: Notes only\n---\nUse read_file."}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let package = connect(Config {
            url: format!("http://{address}"),
            token: "test-secret".into(),
        })
        .await
        .unwrap();
        assert_eq!(package.manifest.tools, vec!["note_book__read_file"]);
        assert_eq!(package.manifest.library.as_deref(),Some("notes"));
        assert!(package.root.as_os_str().is_empty());
        let (runtime, catalog) = test_runtime();
        let parent = runtime
            .snapshot()
            .into_iter()
            .find(|p| p.id == "skill-packages")
            .unwrap()
            .scope;
        let scope = runtime.mount(parent, PackagePlugin(package)).unwrap();
        let read = catalog.packages.read("note-book").unwrap();
        assert!(
            read["content"]
                .as_str()
                .unwrap()
                .contains("`notes.read_file(args)`")
        );
        assert!(!read.to_string().contains("test-secret"));
        let tools = catalog
            .packages
            .tools(&std::collections::BTreeSet::from(["note-book".into()]));
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["outputSchema"]["properties"]["content"]["type"],"string");
        let bindings=crate::agent_program::libraries::catalog(&tools,&catalog.packages.catalog());
        let headers=crate::agent_program::declarations::headers(&bindings,&tools);
        assert!(headers.contains("declare namespace notes") && headers.contains("content:"));
        assert!(!headers.contains("note_book__read_file"));
        assert_eq!(
            tools[0]["function"]["parameters"]["additionalProperties"],
            false
        );
        let root = std::env::temp_dir().join(format!(
            "generic-remote-{}",
            crate::agent_service::uuid_like()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let args = json!({"document":"doc-a","project_id":"this-is-opaque","nested":{"task":"business-data"}});
        let result = catalog
            .execute(
                Arc::new(crate::tools::Workspace::new(&root).unwrap()),
                "note_book__read_file",
                args.clone(),
            )
            .await
            .unwrap();
        assert_eq!(result, json!({"content":"note text"}));
        assert_eq!(
            *captured.lock().unwrap(),
            vec![json!({"tool":"read_file","arguments":args,"context":{}})]
        );
        runtime.unmount(scope).unwrap();
        assert!(!catalog.contains("note_book__read_file"));
        runtime.shutdown().unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}
