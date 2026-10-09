//! Immutable, task-owned visual materials. Durable messages carry IDs, never base64.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{GenericImageView, ImageReader};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::{Cursor, Write}, path::Path, time::{SystemTime, UNIX_EPOCH}};

pub const MAX_IMAGES: usize = 2;
pub const MAX_ORIGINAL_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_INPUT_PIXELS: u64 = 4_000_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all="snake_case")]
pub enum ImageCapability { Supported, Unsupported, #[default] Unknown }

#[derive(Clone, Debug, Default)]
pub struct VisualSettings {
    pub capabilities:std::collections::HashMap<String,std::collections::HashMap<String,ImageCapability>>,
    pub fallback:Option<VisualRoute>,
}
#[derive(Clone, Debug)]
pub struct VisualRoute {pub provider:String,pub model:String,pub url:String,pub api_key:String}
impl VisualSettings {
    pub fn capability(&self,provider:&str,model:&str)->ImageCapability {self.capabilities.get(provider).and_then(|models|models.get(model)).copied().unwrap_or_default()}
}

#[derive(Clone, Debug, Default)]
pub struct VisualContext {
    pub identity: Value,
    pub source_tool_call_id: String,
    pub source_event_id: String,
    pub allowed_artifact_ids: Vec<String>,
    pub execution_epoch:usize,
    pub related_source_versions:Value,
    /// The page snapshot most recently recorded by the host for this work item.
    pub current_page:Value,
    /// Legacy latest-capture references, retained for saved-session compatibility.
    /// They do not determine whether a model may use a screenshot.
    pub host_current_artifact_ids:Vec<String>,
    pub original_artifact_ids:Vec<String>,
}
impl VisualContext {
    pub fn task_id(&self)->&str {self.identity["task_id"].as_str().unwrap_or("")}
    pub fn mcp(root:&Path)->Self {
        let hash=hash(root.to_string_lossy().as_bytes());
        Self {identity:json!({"task_id":format!("mcp:{}",&hash[..16]),"request_id":0,"work_id":"mcp","node_id":"mcp","revision":0,"plan_revision":0}),..Self::default()}
    }
}
pub fn now()->u64 {SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64}
pub fn hash(bytes:&[u8])->String {format!("{:x}",Sha256::digest(bytes))}
pub fn new_id()->String {
    static NEXT:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(0);
    format!("visual-{}-{}-{}",std::process::id(),now(),NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed))
}
fn connection(root:&Path)->Result<rusqlite::Connection> {
    let conn=crate::agent_service::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_visual_artifacts(artifact_id TEXT PRIMARY KEY,task_id TEXT NOT NULL,metadata TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_visual_task ON agent_visual_artifacts(task_id);
        CREATE TABLE IF NOT EXISTS agent_visual_checks(id TEXT PRIMARY KEY,task_id TEXT NOT NULL,metadata TEXT NOT NULL);")?;
    Ok(conn)
}
fn decode(bytes:&[u8])->Result<image::DynamicImage> {
    ensure!(!bytes.is_empty() && bytes.len()<=MAX_ORIGINAL_BYTES,"visual material exceeds the 32 MiB original image budget");
    let mut reader=ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    ensure!(matches!(reader.format(),Some(image::ImageFormat::Png|image::ImageFormat::Jpeg)),"visual material must be PNG or JPEG");
    let mut limits=image::Limits::default();limits.max_image_width=Some(16_384);limits.max_image_height=Some(32_768);limits.max_alloc=Some(128*1024*1024);reader.limits(limits);
    Ok(reader.decode().context("visual material is corrupt or exceeds the decoded pixel budget")?)
}
pub fn save(root:&Path,context:&VisualContext,mut metadata:Value,bytes:&[u8])->Result<Value> {
    ensure!(!context.task_id().is_empty(),"visual capture requires a host task identity");
    let img=decode(bytes)?;let (width,height)=img.dimensions();
    let id=new_id();let relative=format!(".codex-workspace-mcp/visual/{id}.png");
    let path=root.join(&relative);std::fs::create_dir_all(path.parent().unwrap())?;
    // Browser captures are PNG; do not label arbitrary encoded data as PNG.
    ensure!(image::guess_format(bytes)?==image::ImageFormat::Png,"capture must be PNG");
    let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;file.write_all(bytes)?;file.sync_all()?;
    metadata["artifact_id"]=json!(id);metadata["identity"]=context.identity.clone();
    for field in ["task_id","request_id","work_id","node_id","revision","plan_revision"] {metadata[field]=context.identity[field].clone();}
    metadata["source_tool_call_id"]=json!(context.source_tool_call_id);metadata["source_event_id"]=json!(context.source_event_id);
    metadata["execution_epoch"]=json!(context.execution_epoch);metadata["related_source_versions"]=context.related_source_versions.clone();
    metadata["content_hash"]=json!(hash(bytes));metadata["mime_type"]=json!("image/png");metadata["width"]=json!(width);metadata["height"]=json!(height);
    metadata["byte_size"]=json!(bytes.len());
    if metadata["captured_at"].as_u64().is_none() {metadata["captured_at"]=json!(now());}
    metadata["workspace_relative_path"]=json!(relative);
    metadata["image_url"]=json!(format!("/agent/tasks/{}/visual-artifacts/{id}/image",context.task_id()));
    connection(root)?.execute("INSERT INTO agent_visual_artifacts(artifact_id,task_id,metadata) VALUES (?1,?2,?3)",params![id,context.task_id(),metadata.to_string()])?;
    Ok(metadata)
}
pub fn metadata(root:&Path,task:&str,id:&str)->Result<Value> {
    ensure!(id.len()<160 && id.starts_with("visual-") && id.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'-'),"invalid artifact_id; use a host-issued visual ID");
    let text: String=connection(root)?.query_row("SELECT metadata FROM agent_visual_artifacts WHERE task_id=?1 AND artifact_id=?2",params![task,id],|r|r.get(0)).optional()?.context("visual artifact not found in this task")?;
    Ok(serde_json::from_str(&text)?)
}
pub fn read(root:&Path,task:&str,id:&str)->Result<(Value,Vec<u8>)> {
    let item=metadata(root,task,id)?;
    let expected=format!(".codex-workspace-mcp/visual/{id}.png");
    ensure!(item["workspace_relative_path"]==expected,"visual path does not match its immutable identity");
    let path=crate::file_edit::workspace_path(root,&expected)?;
    ensure!(std::fs::metadata(&path)?.len()<=MAX_ORIGINAL_BYTES as u64,"visual file exceeds original budget");
    let bytes=std::fs::read(path)?;ensure!(item["content_hash"]==hash(&bytes),"stored visual bytes do not match the recorded content_hash");
    let img=decode(&bytes)?;ensure!(item["width"]==img.width() && item["height"]==img.height(),"visual dimensions changed");
    Ok((item,bytes))
}
pub fn authorize(item:&Value,context:&VisualContext)->Result<()> {
    ensure!(item["task_id"]==context.task_id() && item["request_id"]==context.identity["request_id"],"visual artifact belongs to a different task/request");
    ensure!(item["identity"]==context.identity || context.allowed_artifact_ids.iter().any(|id|item["artifact_id"]==id.as_str()),"visual artifact is from another work instance; require an explicit valid upstream export");Ok(())
}
fn seal_visual_service_result(mut result:Value,manifest:&Value,context:&VisualContext,goal:&str)->Result<Value> {
    ensure!(result.is_object(),"visual service response must be a JSON object");
    let assessment=result["assessment"].as_str().context("visual service assessment is required")?;
    ensure!(matches!(assessment,"pass"|"issue"|"uncertain"),"visual service returned an invalid assessment");
    ensure!(result["checked_goal"].as_str()==Some(goal),"visual service checked_goal must match the requested goal");
    ensure!(result["request_trace_id"]==manifest["request_trace_id"],"visual service result must cite the host request_trace_id");
    let expected_ids=manifest["images"].as_array().into_iter().flatten().map(|image|image["artifact_id"].clone()).collect::<Vec<_>>();
    ensure!(result["artifact_ids"]==json!(expected_ids),"visual service artifact_ids must match the supplied images in order");
    ensure!(result["expected_visible_result"].as_str().is_some_and(|value|!value.trim().is_empty()),"visual service expected_visible_result is required");
    for field in ["observed_facts","issues","limitations"] {
        ensure!(result[field].as_array().is_some(),"visual service {field} must be an array");
    }
    for fact in result["observed_facts"].as_array().unwrap() {
        ensure!(fact["artifact_id"].as_str().is_some_and(|id|expected_ids.iter().any(|expected|expected.as_str()==Some(id)))
            && fact["region"].as_str().is_some_and(|value|!value.trim().is_empty())
            && fact["fact"].as_str().is_some_and(|value|!value.trim().is_empty()),
            "each visual service observation must cite a supplied artifact_id, region, and visible fact");
    }
    if matches!(assessment,"pass"|"issue") {
        ensure!(!result["observed_facts"].as_array().unwrap().is_empty(),"visual service pass/issue requires at least one image-bound observation");
    }
    let id=new_id();
    result["visual_service_result_id"]=json!(id);
    result["identity"]=context.identity.clone();
    result["request_trace_id"]=manifest["request_trace_id"].clone();
    result["host_validation"]=json!({"validated":true,"request_trace_id":manifest["request_trace_id"],"artifact_ids":expected_ids});
    Ok(result)
}
pub fn index(root:&Path,task:&str,node:&str,request:usize)->Result<Vec<Value>> {
    if !root.is_dir() || task.is_empty() {return Ok(vec![]);}
    let conn=connection(root)?;let mut stmt=conn.prepare("SELECT metadata FROM agent_visual_artifacts WHERE task_id=?1 ORDER BY rowid DESC")?;
    let records=stmt.query_map([task],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(records.into_iter().filter_map(|s|serde_json::from_str::<Value>(&s).ok()).filter(|item|item["request_id"]==request && item["node_id"]==node).take(8)
        .map(|item|json!({"artifact_id":item["artifact_id"],"identity":item["identity"],"url":item["url"],"page_epoch":item["page_epoch"],"width":item["width"],"height":item["height"],"content_hash":item["content_hash"],"captured_at":item["captured_at"]})).collect())
}
pub fn view(root:&Path,context:&VisualContext,id:&str)->Result<Value> {
    let (artifact,_)=read(root,context.task_id(),id)?;authorize(&artifact,context)?;
    Ok(json!({"visual_artifact":artifact,"artifact_id":id,"captured":true,"visual_assessment":"not_evaluated","guidance":"This image is selected for the next normal model request if the route explicitly supports image input. Capture/view alone is not visual verification."}))
}
/// Encode at dispatch, keeping persistent state and SSE free of base64 copies.
pub fn image_block(root:&Path,context:&VisualContext,id:&str,original:bool)->Result<(Value,Value)> {
    let (artifact,mut bytes)=read(root,context.task_id(),id)?;authorize(&artifact,context)?;
    let img=decode(&bytes)?;let (mut width,mut height)=img.dimensions();let mut mode="original";
    if u64::from(width)*u64::from(height)>MAX_INPUT_PIXELS || bytes.len()>MAX_INPUT_BYTES {
        ensure!(!original,"original image exceeds the model input budget; inspect a scaled copy and report detail limitations");
        let scaled=img.thumbnail(1600,1600);width=scaled.width();height=scaled.height();bytes.clear();scaled.write_to(&mut Cursor::new(&mut bytes),image::ImageFormat::Png)?;mode="scaled";
    }
    ensure!(bytes.len()<=MAX_INPUT_BYTES,"encoded visual image exceeds the 8 MiB per-image budget");
    let block=json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}",STANDARD.encode(&bytes)),"detail":"auto"}});
    let mut sent=artifact;sent["input_width"]=json!(width);sent["input_height"]=json!(height);sent["input_bytes"]=json!(bytes.len());sent["input_hash"]=json!(hash(&bytes));sent["mode"]=json!(mode);
    Ok((block,sent))
}
pub fn mcp_result(root:&Path,context:&VisualContext,result:Value)->Result<Value> {
    let mut content=vec![json!({"type":"text","text":serde_json::to_string_pretty(&result)?})];
    if let Some(id)=result["artifact_id"].as_str() {
        let (block,sent)=image_block(root,context,id,false)?;
        let data=block["image_url"]["url"].as_str().unwrap().split_once(',').unwrap().1;
        content.push(json!({"type":"image","mimeType":"image/png","data":data,"_meta":{"visual_artifact":sent}}));
    }
    Ok(json!({"content":content,"structuredContent":result}))
}

/// Multimodal data stays a real content block. It never lives inside JSON text.
pub async fn prepare_request(state:&crate::agent_service::AgentServiceState,actor:&str,model:&str,context:&VisualContext,ids:&[String],goal:&str,body:&mut Value)->Result<Value> {
    let provider=if actor=="observer" {&state.observer_provider}else{&state.provider_name};
    let capability=state.visual.capability(provider,model);
    let request_id=new_id();let mut manifest=json!({"request_trace_id":request_id,"identity":context.identity,"actor":actor,"model_route":{"provider":provider,"model":model},
        "capability":capability,"images":[],"status":"no_images","checked_goal":goal,"execution_epoch":context.execution_epoch,
        "related_source_versions":context.related_source_versions,"current_page":context.current_page,"selected_artifact_ids":ids.iter().take(MAX_IMAGES).collect::<Vec<_>>()});
    if ids.is_empty() {return Ok(manifest);}
    let mut blocks=vec![json!({"type":"text","text":format!("Host tool materials for the existing work goal, not new human instructions. Inspect only the following task-owned images. Goal: {goal}. Report what is visible and limitations; a screenshot cannot prove interaction behavior. Scaled images cannot establish that small details are absent.")})];
    let mut images=Vec::new();let mut unavailable=Vec::new();
    for id in ids.iter().take(MAX_IMAGES) {
        match image_block(state.workspace.root(),context,id,context.original_artifact_ids.contains(id)) {
            Ok((block,sent))=>{blocks.push(json!({"type":"text","text":format!("artifact_id={id}; captured_at={}; url={}; capture identity {}; page_epoch={}; related_source_versions={}; input_mode={}",sent["captured_at"],sent["url"],sent["identity"],sent["page_epoch"],sent["related_source_versions"],sent["mode"])}));blocks.push(block);images.push(sent);},
            Err(error)=>unavailable.push(json!({"artifact_id":id,"reason":error.to_string()})),
        }
    }
    manifest["images"]=json!(images);manifest["unavailable"]=json!(unavailable);
    manifest["omitted_by_image_budget"]=json!(ids.len().saturating_sub(MAX_IMAGES));
    let messages=body["messages"].as_array_mut().context("image request requires messages")?;
    if images.is_empty() {manifest["status"]=json!("unavailable");}
    else if capability==ImageCapability::Supported {
        messages.push(json!({"role":"user","content":blocks}));manifest["status"]=json!("direct");
    } else if capability==ImageCapability::Unsupported && state.visual.fallback.is_some() {
        let route=state.visual.fallback.as_ref().unwrap();
        let service_trace=manifest["request_trace_id"].as_str().unwrap_or("");
        let service_artifact_ids=manifest["images"].as_array().into_iter().flatten().map(|image|image["artifact_id"].clone()).collect::<Vec<_>>();
        let vision_body=json!({"model":route.model,"stream":false,"max_tokens":1600,"messages":[
            {"role":"system","content":format!("You are a visual tool service. Inspect the supplied images for this goal: {goal}. Return JSON with request_trace_id, checked_goal (copy verbatim), artifact_ids (exact supplied IDs in order), expected_visible_result, assessment pass|issue|uncertain, observed_facts [{{artifact_id,region,fact}}], issues, and limitations. Cite supplied artifact IDs. Use their capture times, page information and source versions to judge whether the images answer the goal; describe relevant limitations. Do not infer source code or interaction correctness from pixels alone. Host request_trace_id={service_trace} and artifact_ids={service_artifact_ids:?}")},
            {"role":"user","content":blocks}]});
        let mut fallback_metadata=manifest.clone();fallback_metadata["actor"]=json!("visual_service");fallback_metadata["model_route"]=json!({"provider":route.provider,"model":route.model});fallback_metadata["model"]=json!(route.model);
        let trace=crate::request_context::record(state.workspace.root(),context.task_id(),fallback_metadata.clone(),&vision_body).await;
        let started=std::time::Instant::now();
        let mut raw_service_response=Value::Null;
        let result=tokio::time::timeout(std::time::Duration::from_secs(60),async {
            let response=state.client.post(format!("{}/chat/completions",route.url.trim_end_matches('/'))).bearer_auth(&route.api_key).header("x-codex-visual-dispatch","task-owned").json(&vision_body).send().await?;
            let response=crate::visual_probe::successful_response(response).await?;
            let raw=response.text().await?;raw_service_response=json!(raw);
            let response:Value=serde_json::from_str(&raw)?;let raw=response.pointer("/choices/0/message/content").and_then(Value::as_str).context("visual service returned no text")?;
            let result:Value=serde_json::from_str(raw.trim().trim_start_matches("```json").trim_end_matches("```").trim())?;Ok::<_,anyhow::Error>(result)
        }).await.map_err(|_|anyhow::anyhow!("visual service timed out")).and_then(|r|r);
        let result=result.and_then(|result|seal_visual_service_result(result,&manifest,context,goal));
        crate::request_context::finish(trace,if result.is_ok(){"completed"}else{"failed"},json!({"duration_ms":started.elapsed().as_millis(),
            "error":result.as_ref().err().map(|error|format!("{error:#}")),"response":raw_service_response})).await;
        let result=match result {
            Ok(result)=>result,
            Err(error)=>json!({"assessment":"unavailable","limitations":[format!("{error:#}")],"host_validation":{"validated":false}}),
        };
        let validated=result["host_validation"]["validated"]==true;
        manifest["status"]=json!(if validated {"fallback"}else{"unavailable"});
        manifest["visual_service_result_id"]=if validated {result["visual_service_result_id"].clone()}else{Value::Null};
        manifest["visual_service_result_validated"]=json!(validated);
        manifest["visual_service_response"]=raw_service_response;
        manifest["visual_service_result"]=result.clone();manifest["visual_service_route"]=fallback_metadata["model_route"].clone();
        crate::agent_service::emit(state.workspace.root(),context.task_id(),"visual/service_result",json!({"manifest":manifest,"result":result,"elapsed_ms":started.elapsed().as_millis()})).await?;
    } else {
        manifest["status"]=json!(if capability==ImageCapability::Unknown {"unknown_capability"}else{"unavailable"});manifest["selected_images"]=manifest["images"].take();manifest["images"]=json!([]);
        let captured=manifest["selected_images"].as_array().into_iter().flatten().map(|image|image["artifact_id"].clone()).collect::<Vec<_>>();
        manifest["image_input_unavailable"]=json!({"reason":if capability==ImageCapability::Unknown {
            if state.visual.fallback.is_some() {"image capability of this model is unknown; the configured fallback is used only after explicit image rejection"}else{"image capability of this model is unknown and no fallback visual service is configured"}
        }else{"this model does not accept images and no fallback visual service is configured"},
            "captured_artifact_ids":captured});
    }
    let contract=json!({"artifact_ids":manifest["images"].as_array().into_iter().flatten().map(|image|image["artifact_id"].clone()).collect::<Vec<_>>(),
        "checked_goal":goal,"request_trace_id":manifest["request_trace_id"],"visual_service_result_id":manifest["visual_service_result_id"],"expected_visible_result":"describe the expected visible result","assessment":"uncertain",
        "observed_facts":[{"artifact_id":manifest["images"][0]["artifact_id"],"region":"describe the inspected region","fact":"replace this with actual visible evidence"}],"issues":[],"limitations":[]});
    let delivery_note=if manifest["images"].as_array().is_some_and(Vec::is_empty) {
        format!("Image input is unavailable for this request: {}. Captured images remain available to the user, but their pixels were not delivered to this model.",manifest["image_input_unavailable"]["reason"].as_str().unwrap_or("see the complete unavailable or visual_service_result fields"))
    }else{String::new()};
    let message=json!({"role":"system","content":format!("Host visual dispatch: {}. {} For visual_check_result use this JSON structure: {}. Preserve checked_goal, request_trace_id and artifact references so the observation can be associated with its inputs. Capture metadata and this request's page/source observations are recorded facts; you decide whether the supplied screenshots are sufficient for the goal and whether another capture is needed. Describe visible facts and any relevant limitations. Only direct means this role received real image content blocks; fallback evidence comes from the named visual service and should retain that attribution. unavailable/unknown means image content was not delivered. Screenshot or DOM status alone is not a visual assessment.",manifest,delivery_note,contract)});
    // Save exactly the text delivered to the model, before its request starts.
    // Other roles and history read this same result; image blocks stay transient.
    crate::agent_service::emit(state.workspace.root(),context.task_id(),"visual/input_result",json!({
        "turn":context.identity["request_id"],"identity":context.identity,"actor":actor,
        "request_trace_id":manifest["request_trace_id"],"message":message})).await?;
    messages.push(message);
    Ok(manifest)
}

pub fn validate_check(root:&Path,context:&VisualContext,check:&Value,requests:&[Value],actor:&str)->Result<Value> {
    ensure!(check.is_object(),"visual_check_result must be an object");
    ensure!(check["identity"].is_null() || check["identity"]==context.identity,"visual judgment identity does not match the active instance");
    let assessment=check["assessment"].as_str().context("visual assessment is required")?;
    ensure!(matches!(assessment,"pass"|"issue"|"uncertain"|"unavailable"),"invalid visual assessment");
    let goal=check["checked_goal"].as_str().filter(|s|!s.trim().is_empty()).context("visual result needs its checked_goal")?;
    let ids=check["artifact_ids"].as_array().context("visual result requires artifact_ids")?;
    ensure!(ids.len()<=MAX_IMAGES,"visual result exceeds image selection budget");
    let request=requests.iter().rev().find(|request|request["request_trace_id"]==check["request_trace_id"] && request["identity"]==context.identity);
    let mut artifacts=Vec::new();
    for id in ids {
        let id=id.as_str().context("visual artifact IDs must be strings")?;
        let (artifact,_)=read(root,context.task_id(),id)?;authorize(&artifact,context)?;artifacts.push(artifact);
    }
    if let Some(request)=request {
        ensure!(request["checked_goal"]==goal,"checked_goal must match the actual visual request goal");
        // A limitation report may cite authorized historical captures across
        // requests; a positive pixel claim must refer to an actual sent image.
        let undelivered_allowed=!matches!(assessment,"pass"|"issue");
        for id in ids.iter().filter(|_|!undelivered_allowed) {
            let sent=request["images"].as_array().into_iter().flatten().any(|image|image["artifact_id"]==*id);

            ensure!(sent,"visual result cites an image never sent in the referenced request");
        }
    }
    if matches!(assessment,"pass"|"issue") {
        let request=request.context("visual result must cite a successfully received image request_trace_id")?;
        ensure!(matches!(request["status"].as_str(),Some("direct"|"fallback")) && !ids.is_empty(),"visual pass/issue requires actual image input, not DOM/capture status");
        ensure!(check["observed_facts"].as_array().is_some_and(|facts|!facts.is_empty()),"visual pass/issue must record actual observed facts");
        ensure!(check["expected_visible_result"].as_str().is_some_and(|s|!s.trim().is_empty()),"visual pass/issue requires expected_visible_result");
        for fact in check["observed_facts"].as_array().unwrap() {
            ensure!(fact["fact"].as_str().is_some_and(|s|!s.trim().is_empty()) && ids.iter().any(|id|id==&fact["artifact_id"]),"each observed fact must cite a selected artifact_id and describe visible evidence");
        }
        if request["status"]=="fallback" {
            let service=&request["visual_service_result"];
            ensure!(request["visual_service_result_validated"]==true && service["host_validation"]["validated"]==true
                && service["identity"]==context.identity && service["request_trace_id"]==request["request_trace_id"],
                "fallback pass/issue requires a host-validated visual service result");
            ensure!(check["visual_service_result_id"]==service["visual_service_result_id"]
                && request["visual_service_result_id"]==service["visual_service_result_id"],
                "fallback result must cite its validated visual_service_result_id");
            ensure!(service["assessment"]==assessment,"fallback assessment must preserve the visual service conclusion");
            ensure!(check["expected_visible_result"]==service["expected_visible_result"],"fallback expected_visible_result must come from the validated visual service result");
            for fact in check["observed_facts"].as_array().unwrap() {
                ensure!(service["observed_facts"].as_array().into_iter().flatten().any(|source|
                    source["artifact_id"]==fact["artifact_id"] && source["region"]==fact["region"] && source["fact"]==fact["fact"]),
                    "fallback Worker facts must be present in the validated visual service result");
            }
        }
    }
    let mut result=check.clone();result["identity"]=context.identity.clone();result["actor"]=json!(actor);result["checked_at"]=json!(now());result["artifacts"]=json!(artifacts);
    result["model_route"]=request.map(|r|r["model_route"].clone()).unwrap_or(Value::Null);result["input_mode"]=request.map(|r|r["status"].clone()).unwrap_or(json!("not_received"));
    // Keep the observations supplied with the image request. Later page/source
    // observations must not rewrite what the model actually received.
    result["source_binding"]=request.map(|request|json!({"execution_epoch":request["execution_epoch"],
        "related_source_versions":request["related_source_versions"],"current_page":request["current_page"]}))
        .unwrap_or_else(||json!({"execution_epoch":context.execution_epoch,"related_source_versions":context.related_source_versions,"current_page":context.current_page}));
    if let Some(request)=request {
        // Preserve the same dispatch record for successes and failures. A
        // missing model input must not erase why the host did not send it.
        result["visual_request"]=request.clone();
        result["capability"]=request["capability"].clone();
        result["image_input_unavailable"]=request["image_input_unavailable"].clone();
        result["visual_service_route"]=request["visual_service_route"].clone();
        result["visual_service_result_id"]=request["visual_service_result_id"].clone();
    }
    let id=new_id();result["check_id"]=json!(id);
    connection(root)?.execute("INSERT INTO agent_visual_checks(id,task_id,metadata) VALUES (?1,?2,?3)",params![id,context.task_id(),result.to_string()])?;
    Ok(result)
}

pub async fn image_route(axum::extract::State(state):axum::extract::State<crate::agent_service::AgentServiceState>,axum::extract::Path((task,id)):axum::extract::Path<(String,String)>)->axum::response::Response {
    use axum::response::IntoResponse;
    let root=state.workspace.root().to_path_buf();
    match tokio::task::spawn_blocking(move||read(&root,&task,&id)).await {
        Ok(Ok((_,bytes)))=>([(axum::http::header::CONTENT_TYPE,"image/png"),(axum::http::header::CACHE_CONTROL,"private, no-cache")],bytes).into_response(),
        Ok(Err(error))=>(axum::http::StatusCode::NOT_FOUND,axum::Json(json!({"error":error.to_string()}))).into_response(),
        Err(_)=>(axum::http::StatusCode::INTERNAL_SERVER_ERROR,"visual read failed").into_response(),
    }
}
