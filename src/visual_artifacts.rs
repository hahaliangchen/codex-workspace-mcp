//! Immutable, task-owned visual materials. Durable messages carry IDs, never base64.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{GenericImageView, ImageReader};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Cursor, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_IMAGES: usize = 2;
pub const MAX_ORIGINAL_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_INPUT_PIXELS: u64 = 4_000_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageCapability {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default)]
pub struct VisualSettings {
    pub capabilities:
        std::collections::HashMap<String, std::collections::HashMap<String, ImageCapability>>,
}
#[derive(Clone, Debug)]
pub struct VisualRoute {
    pub provider: String,
    pub model: String,
    pub url: String,
    pub api_key: String,
}
impl VisualSettings {
    pub fn capability(&self, provider: &str, model: &str) -> ImageCapability {
        self.capabilities
            .get(provider)
            .and_then(|models| models.get(model))
            .copied()
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct VisualContext {
    pub identity: Value,
    /// Human chat turn of this invocation, distinct from the continuing goal ID.
    pub turn: Option<usize>,
    pub source_tool_call_id: String,
    pub source_event_id: String,
    pub allowed_artifact_ids: Vec<String>,
    pub execution_epoch: usize,
    pub related_source_versions: Value,
    /// The page snapshot most recently recorded by the host for this work item.
    pub current_page: Value,
    /// Legacy latest-capture references, retained for saved-session compatibility.
    /// They do not determine whether a model may use a screenshot.
    pub original_artifact_ids: Vec<String>,
}
impl VisualContext {
    pub fn task_id(&self) -> &str {
        self.identity["task_id"].as_str().unwrap_or("")
    }
    pub fn mcp(root: &Path) -> Self {
        let hash = hash(root.to_string_lossy().as_bytes());
        Self {
            identity: json!({"task_id":format!("mcp:{}",&hash[..16]),"request_id":0,"work_id":"mcp","node_id":"mcp","revision":0,"plan_revision":0}),
            ..Self::default()
        }
    }
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn new_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "visual-{}-{}-{}",
        std::process::id(),
        now(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
fn connection(root: &Path) -> Result<rusqlite::Connection> {
    let conn = crate::agent_service::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_visual_artifacts(artifact_id TEXT PRIMARY KEY,task_id TEXT NOT NULL,metadata TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_visual_task ON agent_visual_artifacts(task_id);
        CREATE TABLE IF NOT EXISTS agent_visual_checks(id TEXT PRIMARY KEY,task_id TEXT NOT NULL,metadata TEXT NOT NULL);")?;
    Ok(conn)
}
fn decode(bytes: &[u8]) -> Result<image::DynamicImage> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_ORIGINAL_BYTES,
        "visual material exceeds the 32 MiB original image budget"
    );
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    ensure!(
        matches!(
            reader.format(),
            Some(image::ImageFormat::Png | image::ImageFormat::Jpeg)
        ),
        "visual material must be PNG or JPEG"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(32_768);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader
        .decode()
        .context("visual material is corrupt or exceeds the decoded pixel budget")?)
}
pub fn save(
    root: &Path,
    context: &VisualContext,
    mut metadata: Value,
    bytes: &[u8],
) -> Result<Value> {
    ensure!(
        !context.task_id().is_empty(),
        "visual capture requires a host task identity"
    );
    let img = decode(bytes)?;
    let (width, height) = img.dimensions();
    let id = new_id();
    let relative = format!(".codex-workspace-mcp/visual/{id}.png");
    let path = root.join(&relative);
    std::fs::create_dir_all(path.parent().unwrap())?;
    // Browser captures are PNG; do not label arbitrary encoded data as PNG.
    ensure!(
        image::guess_format(bytes)? == image::ImageFormat::Png,
        "capture must be PNG"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    metadata["artifact_id"] = json!(id);
    metadata["identity"] = context.identity.clone();
    for field in [
        "task_id",
        "request_id",
        "work_id",
        "node_id",
        "revision",
        "plan_revision",
    ] {
        metadata[field] = context.identity[field].clone();
    }
    metadata["source_tool_call_id"] = json!(context.source_tool_call_id);
    metadata["source_event_id"] = json!(context.source_event_id);
    metadata["execution_epoch"] = json!(context.execution_epoch);
    metadata["related_source_versions"] = context.related_source_versions.clone();
    metadata["content_hash"] = json!(hash(bytes));
    metadata["mime_type"] = json!("image/png");
    metadata["width"] = json!(width);
    metadata["height"] = json!(height);
    metadata["byte_size"] = json!(bytes.len());
    if metadata["captured_at"].as_u64().is_none() {
        metadata["captured_at"] = json!(now());
    }
    metadata["workspace_relative_path"] = json!(relative);
    metadata["image_url"] = json!(format!(
        "/agent/tasks/{}/visual-artifacts/{id}/image",
        context.task_id()
    ));
    connection(root)?.execute(
        "INSERT INTO agent_visual_artifacts(artifact_id,task_id,metadata) VALUES (?1,?2,?3)",
        params![id, context.task_id(), metadata.to_string()],
    )?;
    Ok(metadata)
}
pub fn metadata(root: &Path, task: &str, id: &str) -> Result<Value> {
    ensure!(
        id.len() < 160
            && id.starts_with("visual-")
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid artifact_id; use a host-issued visual ID"
    );
    let text: String = connection(root)?
        .query_row(
            "SELECT metadata FROM agent_visual_artifacts WHERE task_id=?1 AND artifact_id=?2",
            params![task, id],
            |r| r.get(0),
        )
        .optional()?
        .context("visual artifact not found in this task")?;
    Ok(serde_json::from_str(&text)?)
}
pub fn read(root: &Path, task: &str, id: &str) -> Result<(Value, Vec<u8>)> {
    let item = metadata(root, task, id)?;
    let expected = format!(".codex-workspace-mcp/visual/{id}.png");
    ensure!(
        item["workspace_relative_path"] == expected,
        "visual path does not match its immutable identity"
    );
    let path = crate::file_edit::workspace_path(root, &expected)?;
    ensure!(
        std::fs::metadata(&path)?.len() <= MAX_ORIGINAL_BYTES as u64,
        "visual file exceeds original budget"
    );
    let bytes = std::fs::read(path)?;
    ensure!(
        item["content_hash"] == hash(&bytes),
        "stored visual bytes do not match the recorded content_hash"
    );
    let img = decode(&bytes)?;
    ensure!(
        item["width"] == img.width() && item["height"] == img.height(),
        "visual dimensions changed"
    );
    Ok((item, bytes))
}
pub fn authorize(item: &Value, context: &VisualContext) -> Result<()> {
    let retained_task_image = context.identity["runtime"] == "single_agent"
        && context
            .allowed_artifact_ids
            .iter()
            .any(|id| item["artifact_id"] == id.as_str());
    ensure!(
        item["task_id"] == context.task_id()
            && (item["request_id"] == context.identity["request_id"] || retained_task_image),
        "visual artifact belongs to a different task/request"
    );
    ensure!(
        item["identity"] == context.identity
            || context
                .allowed_artifact_ids
                .iter()
                .any(|id| item["artifact_id"] == id.as_str()),
        "visual artifact is from another work instance; require an explicit valid upstream export"
    );
    Ok(())
}
pub fn view(root: &Path, context: &VisualContext, id: &str) -> Result<Value> {
    let (artifact, _) = read(root, context.task_id(), id)?;
    authorize(&artifact, context)?;
    Ok(
        json!({"visual_artifact":artifact,"artifact_id":id,"captured":true,"visual_assessment":"not_evaluated","guidance":"This image is selected for the next normal model request if the route explicitly supports image input. Capture/view alone is not visual verification."}),
    )
}
/// Observer can inspect images belonging to the accepted human goal, across
/// its child work instances. Other tasks and replaced goals remain excluded.
/// One durable single-agent conversation may explicitly read its older images.
pub fn task_artifact_ids(root: &Path, task: &str) -> Result<Vec<String>> {
    let conn = connection(root)?;
    let mut stmt = conn.prepare(
        "SELECT artifact_id FROM agent_visual_artifacts WHERE task_id=?1 ORDER BY rowid",
    )?;
    Ok(stmt
        .query_map([task], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
/// Encode at dispatch, keeping persistent state and SSE free of base64 copies.
pub fn image_block(
    root: &Path,
    context: &VisualContext,
    id: &str,
    original: bool,
) -> Result<(Value, Value)> {
    let (artifact, mut bytes) = read(root, context.task_id(), id)?;
    authorize(&artifact, context)?;
    let img = decode(&bytes)?;
    let (mut width, mut height) = img.dimensions();
    let mut mode = "original";
    if u64::from(width) * u64::from(height) > MAX_INPUT_PIXELS || bytes.len() > MAX_INPUT_BYTES {
        ensure!(
            !original,
            "original image exceeds the model input budget; inspect a scaled copy and report detail limitations"
        );
        let scaled = img.thumbnail(1600, 1600);
        width = scaled.width();
        height = scaled.height();
        bytes.clear();
        scaled.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)?;
        mode = "scaled";
    }
    ensure!(
        bytes.len() <= MAX_INPUT_BYTES,
        "encoded visual image exceeds the 8 MiB per-image budget"
    );
    let block = json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}",STANDARD.encode(&bytes)),"detail":"auto"}});
    let mut sent = artifact;
    sent["input_width"] = json!(width);
    sent["input_height"] = json!(height);
    sent["input_bytes"] = json!(bytes.len());
    sent["input_hash"] = json!(hash(&bytes));
    sent["mode"] = json!(mode);
    Ok((block, sent))
}
pub fn mcp_result(root: &Path, context: &VisualContext, result: Value) -> Result<Value> {
    let mut content = vec![json!({"type":"text","text":serde_json::to_string_pretty(&result)?})];
    if let Some(id) = result["artifact_id"].as_str() {
        let (block, sent) = image_block(root, context, id, false)?;
        let data = block["image_url"]["url"]
            .as_str()
            .unwrap()
            .split_once(',')
            .unwrap()
            .1;
        content.push(json!({"type":"image","mimeType":"image/png","data":data,"_meta":{"visual_artifact":sent}}));
    }
    Ok(json!({"content":content,"structuredContent":result}))
}

/// Conversation stores file references; only the outgoing request holds bytes.
/// Resolve exactly the references present in the current conversation window.
fn materialize_message(
    root: &Path,
    context: &VisualContext,
    message: &Value,
    capability: ImageCapability,
) -> Result<(Value, Vec<Value>, Vec<Value>)> {
    let mut wire = message.clone();
    let mut images = Vec::new();
    let mut unavailable = Vec::new();
    let source = wire
        .as_object_mut()
        .and_then(|fields| fields.remove("source_identity"));
    let mut source_context = context.clone();
    // Routing a saved original to another role does not change its ownership.
    // The history/router already read this original in the authorized workspace.
    // Use its original identity, including an explicitly retrieved older task;
    // do not mistake historical evidence for a new capture by the receiving role.
    // Direct selections still use their own context and immutable-byte checks.
    if let Some(source) =
        source.filter(|source| source["task_id"].is_string() && !source["request_id"].is_null())
    {
        source_context.identity = source;
    }
    if let Some(content) = wire["content"].as_array_mut() {
        let mut materialized = Vec::with_capacity(content.len());
        for block in std::mem::take(content) {
            if block["type"] != "image_ref" {
                materialized.push(block);
                continue;
            }
            let id = block["artifact_id"]
                .as_str()
                .context("image reference requires artifact_id")?
                .to_owned();
            let original = block["view_original"].as_bool().unwrap_or(false);
            let result = if capability == ImageCapability::Supported {
                image_block(root, &source_context, &id, original)
            } else {
                Err(anyhow::anyhow!(
                    "current model image capability is {capability:?}; referenced pixels were not delivered"
                ))
            };
            match result {
                Ok((pixels, sent)) => {
                    materialized.push(pixels);
                    images.push(sent);
                }
                Err(error) => {
                    let reason = error.to_string();
                    unavailable.push(json!({"artifact_id":id,"reason":reason}));
                }
            }
        }
        *content = materialized;
    }
    Ok((wire, images, unavailable))
}

/// The same original message is saved and handed to subsequent role requests.
/// No data URLs are stored in the conversation or event stream.
pub fn conversation_message(manifest: &Value) -> Value {
    let mut content = Vec::new();
    for id in manifest["selected_artifact_ids"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let Some(image) = manifest["images"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|image| image["artifact_id"] == *id)
        else {
            continue;
        };
        let mode = image["mode"].as_str().unwrap_or("processed");
        content.push(json!({"type":"image_ref","artifact_id":id,"view_original":mode=="original"}));
    }
    json!({"role":"user","content":content})
}

/// Multimodal data stays a real content block. It never lives inside JSON text.
pub async fn prepare_request(
    state: &crate::agent_service::AgentServiceState,
    actor: &str,
    model: &str,
    context: &VisualContext,
    ids: &[String],
    goal: &str,
    body: &mut Value,
) -> Result<Value> {
    let provider = &state.provider_name;
    let capability = state.visual.capability(provider, model);
    let request_id = new_id();
    let mut manifest = json!({"turn":context.turn,"request_trace_id":request_id,"identity":context.identity,"actor":actor,"model_route":{"provider":provider,"model":model},
        "capability":capability,"images":[],"status":"no_images","checked_goal":goal,"execution_epoch":context.execution_epoch,
        "related_source_versions":context.related_source_versions,"current_page":context.current_page,"selected_artifact_ids":ids.iter().take(MAX_IMAGES).collect::<Vec<_>>()});
    let mut retained = Vec::new();
    let mut retained_unavailable = Vec::new();
    for message in body["messages"]
        .as_array_mut()
        .context("image request requires messages")?
    {
        let (wire, images, unavailable) =
            materialize_message(state.workspace.root(), context, message, capability)?;
        *message = wire;
        retained.extend(images);
        retained_unavailable.extend(unavailable);
    }
    if ids.is_empty() && retained.is_empty() && retained_unavailable.is_empty() {
        return Ok(manifest);
    }
    manifest["retained_artifact_ids"] = json!(
        retained
            .iter()
            .map(|image| image["artifact_id"].clone())
            .collect::<Vec<_>>()
    );
    let mut images = retained;
    let mut unavailable = retained_unavailable;
    for id in ids.iter().take(MAX_IMAGES) {
        match image_block(
            state.workspace.root(),
            context,
            id,
            context.original_artifact_ids.contains(id),
        ) {
            Ok((_, sent)) => {
                if !images
                    .iter()
                    .any(|image| image["artifact_id"] == sent["artifact_id"])
                {
                    images.push(sent);
                }
            }
            Err(error) => unavailable.push(json!({"artifact_id":id,"reason":error.to_string()})),
        }
    }
    manifest["images"] = json!(images);
    manifest["unavailable"] = json!(unavailable);
    manifest["omitted_by_image_budget"] = json!(ids.len().saturating_sub(MAX_IMAGES));
    let messages = body["messages"]
        .as_array_mut()
        .context("image request requires messages")?;
    if images.is_empty() {
        manifest["status"] = json!("unavailable");
    } else if capability == ImageCapability::Supported {
        manifest["status"] = json!("direct");
    } else {
        manifest["status"] = json!(if capability == ImageCapability::Unknown {
            "unknown_capability"
        } else {
            "unavailable"
        });
        manifest["selected_images"] = manifest["images"].take();
        manifest["images"] = json!([]);
        let captured = manifest["selected_images"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|image| image["artifact_id"].clone())
            .collect::<Vec<_>>();
        manifest["image_input_unavailable"] = json!({"reason":if capability==ImageCapability::Unknown {
            "image capability of this model is unknown"
        }else{"this model does not accept images"},
            "captured_artifact_ids":captured});
    }
    if !ids.is_empty() {
        let message = conversation_message(&manifest);
        if message["content"].as_array().is_none_or(Vec::is_empty) {
            return Ok(manifest);
        }
        // Keep only the image references in the conversation record. Dispatch
        // metadata stays in the visual manifest and request trace.
        crate::agent_service::emit(state.workspace.root(),context.task_id(),"visual/input_result",json!({
            "turn":context.turn.map(|turn|json!(turn)).unwrap_or_else(||context.identity["request_id"].clone()),"identity":context.identity,"actor":actor,
            "request_trace_id":manifest["request_trace_id"],"dispatch_metadata":manifest,"message":message})).await?;
        let (wire, _, _) =
            materialize_message(state.workspace.root(), context, &message, capability)?;
        messages.push(wire);
    }
    Ok(manifest)
}

pub async fn image_route(
    axum::extract::State(state): axum::extract::State<crate::agent_service::AgentServiceState>,
    axum::extract::Path((task, id)): axum::extract::Path<(String, String)>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let root = state.workspace.root().to_path_buf();
    match tokio::task::spawn_blocking(move || read(&root, &task, &id)).await {
        Ok(Ok((_, bytes))) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/png"),
                (axum::http::header::CACHE_CONTROL, "private, no-cache"),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(error)) => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"error":error.to_string()})),
        )
            .into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "visual read failed",
        )
            .into_response(),
    }
}
