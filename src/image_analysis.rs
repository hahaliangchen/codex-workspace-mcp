//! An ordinary read-only tool. Its model input consists of the caller-supplied
//! question, saved pixels and, when the plugin that produced an image attached
//! them, that plugin's review instructions. It never edits or inherits a
//! conversation window.
use crate::{
    agent_service::AgentServiceState,
    visual_artifacts::{self, VisualContext},
};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tokio_util::sync::CancellationToken;

/// Images one analysis may cover. One or two go to the vision model as they
/// are; three or more are laid out on one numbered sheet (3×3 at most), so a
/// whole deck can be reviewed in a single request.
pub const MAX_ANALYSIS_IMAGES: usize = 9;
const SHEET_FROM: usize = 3;
const SHEET_BAR: u32 = 36;
const SHEET_GAP: u32 = 12;

pub fn tool() -> Value {
    json!({"type":"function","function":{
        "name":"analyze_images",
        "description":"Analyze saved images with a dedicated vision model. Use for image interpretation and visual verification. Supply up to 9 artifact_ids from screenshot/image assets and your question; three or more images are shown to the vision model as one numbered sheet. When a plugin made an image and attached its review instructions, the vision model reads those instructions first and your question after them, so ask only about extra points. The result's analysis_id is what plugin tools that record a review take. Returns the model's original analysis as tool evidence. Reads existing original pixels; does not browse, recapture, select images for your next request, or change context.",
        "parameters":{"type":"object","properties":{
            "artifact_ids":{"type":"array","minItems":1,"maxItems":MAX_ANALYSIS_IMAGES,"uniqueItems":true,"items":{"type":"string","minLength":1}},
            "question":{"type":"string","minLength":1},
            "background":{"type":"boolean","description":"Optionally queue visual inspection while execution continues. By default the call waits synchronously. Background checks deliver completion to the same conversation."}
        },"required":["artifact_ids","question"],"additionalProperties":false}
    }})
}

pub fn validate_background(
    state: &AgentServiceState,
    context: &VisualContext,
    args: &Value,
    main_model: &str,
) -> Result<()> {
    let resolved = state.visual_tool_settings.resolve(state, main_model)?;
    ensure!(
        !resolved.provider_url.is_empty(),
        "image analysis provider is unavailable"
    );
    ensure!(
        args["question"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "question must not be empty"
    );
    let ids = args["artifact_ids"]
        .as_array()
        .context("artifact_ids must be an array")?;
    ensure!(
        !ids.is_empty() && ids.len() <= MAX_ANALYSIS_IMAGES,
        "provide between 1 and {MAX_ANALYSIS_IMAGES} images"
    );
    let mut unique = BTreeSet::new();
    for value in ids {
        let id = value.as_str().context("artifact id must be a string")?;
        ensure!(unique.insert(id), "duplicate artifact id");
        visual_artifacts::image_block(state.workspace.root(), context, id, true)?;
    }
    Ok(())
}

pub async fn execute(
    state: &AgentServiceState,
    context: &VisualContext,
    args: &Value,
    call_id: &str,
    main_model: &str,
    cancel: &CancellationToken,
) -> Result<Value> {
    ensure!(!cancel.is_cancelled(), "cancelled");
    let resolved = state.visual_tool_settings.resolve(state, main_model)?;
    let input_budget = state
        .visual_tool_settings
        .input_budget(crate::context_window::MAX_TOKENS - crate::model_settings::OUTPUT_RESERVE);
    let state = &resolved;
    let ids = args["artifact_ids"]
        .as_array()
        .context("artifact_ids must be an array")?;
    ensure!(
        !ids.is_empty() && ids.len() <= MAX_ANALYSIS_IMAGES,
        "provide between 1 and {MAX_ANALYSIS_IMAGES} images"
    );
    let question = args["question"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("question must not be empty")?;
    let mut blocks = Vec::new();
    let mut pictures = Vec::new();
    let mut sent = Vec::new();
    let mut unique = BTreeSet::new();
    let tiled = ids.len() >= SHEET_FROM;
    for value in ids {
        let id = value
            .as_str()
            .filter(|s| !s.is_empty())
            .context("artifact id must be a nonempty string")?;
        ensure!(unique.insert(id), "duplicate artifact id: {id}");
        if tiled {
            let (artifact, bytes) = visual_artifacts::read(state.workspace.root(), context.task_id(), id)?;
            visual_artifacts::authorize(&artifact, context)?;
            pictures.push(image::load_from_memory(&bytes).context("visual material is unreadable")?);
            sent.push(artifact);
            continue;
        }
        // Strict original mode rejects an over-budget image instead of silently
        // resizing it. Ownership and byte hashes are checked by the shared reader.
        let (block, metadata) =
            visual_artifacts::image_block(state.workspace.root(), context, id, true)?;
        blocks.push(block);
        sent.push(metadata);
    }
    ensure!(
        !state.provider_url.is_empty(),
        "image analysis provider is unavailable"
    );
    let brief = review_brief(&sent);
    let mut content = vec![json!({"type":"text","text":question})];
    let mut sheet_record = Value::Null;
    if tiled {
        let (png, layout) = sheet(&pictures)?;
        let saved = visual_artifacts::save(
            state.workspace.root(),
            context,
            json!({"source":"analysis_sheet","images":sent.iter().map(|image| image["artifact_id"].clone()).collect::<Vec<_>>(),"layout":layout}),
            &png,
        )?;
        sheet_record = json!({"artifact_id":saved["artifact_id"],"layout":layout});
        content.push(json!({"type":"text","text":format!(
            "Images 1-{} are tiled on this sheet, left to right and top to bottom. The white number in the black bar above each tile is its image number.",
            pictures.len())}));
        content.push(json!({"type":"image_url","image_url":{
            "url":format!("data:image/png;base64,{}",base64::engine::general_purpose::STANDARD.encode(&png)),"detail":"high"}}));
    }
    for (index, block) in blocks.into_iter().enumerate() {
        // The plugin's instructions refer to images by position.
        if brief.is_some() {
            content.push(json!({"type":"text","text":format!("Image {}", index + 1)}));
        }
        content.push(block);
    }
    let mut messages = Vec::new();
    if let Some(brief) = &brief {
        messages.push(json!({"role":"system","content":brief}));
    }
    messages.push(json!({"role":"user","content":content}));
    let model = &state.default_model;
    let mut body = json!({"model":model,"stream":true,"max_tokens":8192,"messages":messages});
    if let Some(effort) = &state.reasoning_effort {
        body["reasoning_effort"] = json!(effort);
    }
    if state.fast_mode {
        body["service_tier"] = json!("fast");
    }
    ensure!(
        crate::context_window::estimate(&body) <= input_budget,
        "image analysis input exceeds configured context window"
    );
    // A nested request log is diagnostic data, not another agent/flow window.
    let node = visual_artifacts::new_id();
    let analysis_id = format!("analysis-{}", node.trim_start_matches("visual-"));
    let trace = crate::request_context::record(
        state.workspace.root(),
        context.task_id(),
        json!({"actor":"tool","stage":"tool_execution","tool_name":"analyze_images",
            "turn":context.turn,"nodeId":node,"parent_node_id":context.identity["node_id"],
            "tool_call_id":call_id,"request_flow":false,"images":sent,"sheet":sheet_record,
            "model_route":{"provider":state.provider_name,"model":model}}),
        &body,
    )
    .await
    .context("Cannot persist image analysis input; request was not sent")?;
    let reference = trace.history_reference();
    let diagnostics = crate::model_transport::ModelRequestLog::new(
        state.workspace.root(),
        json!({"actor":"tool","request_id":node,"task_id":context.task_id(),"context_id":trace.id(),
            "turn":context.turn,"tool":"analyze_images","provider":state.provider_name,"model":model}),
        &state.provider_url,
        &body,
    );
    diagnostics.sending();
    let mut raw_response = Value::Null;
    let pending = async {
        let response = state
            .model_client
            .post(format!(
                "{}/chat/completions",
                state.provider_url.trim_end_matches('/')
            ))
            .bearer_auth(&state.api_key)
            .header("x-codex-visual-dispatch", "task-owned")
            .header("x-client-request-id", node.as_str())
            .json(&body)
            .send()
            .await?;
        diagnostics.headers(&response);
        let response = crate::visual_probe::successful_response(response).await?;
        let completed = crate::model_transport::consume_response(response, &diagnostics).await?;
        raw_response = json!({"choices":[{"message":completed.message,"finish_reason":completed.stats["finish_reason"]}],"usage":completed.stats["usage"]});
        diagnostics.phase("response_validation");
        ensure!(
            completed.terminated && completed.stats["finish_reason"] == "stop",
            "image analysis did not finish normally: {}",
            completed.stats["finish_reason"]
        );
        let message = &raw_response["choices"][0]["message"];
        ensure!(
            message["tool_calls"].as_array().is_none_or(Vec::is_empty),
            "image analysis returned unexpected tool calls"
        );
        let analysis = message["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .context("image analysis returned no text")?;
        diagnostics.phase("completed");
        let output = json!({"status":"completed","analysis":analysis,"analysis_id":analysis_id,"artifact_ids":ids,"model":model,
            "reasoning_effort":state.reasoning_effort,"request_reference":reference});
        Ok(output)
    };
    let result = diagnostics
        .watch(
            pending,
            cancel,
            crate::model_transport::TimeoutPolicy::default(),
        )
        .await;
    let status = if result.is_ok() {
        "completed"
    } else if cancel.is_cancelled() {
        "cancelled"
    } else {
        "failed"
    };
    let transport = diagnostics.finish(status, result.as_ref().err());
    trace
        .finish_checked(
            status,
            json!({"response":raw_response,"error":result.as_ref().err().map(|e|format!("{e:#}")),"transport":transport}),
        )
        .await?;
    if let Ok(output) = &result {
        // Plugins that record reviews receive this record, not the caller's summary of it.
        let images = sent
            .iter()
            .map(|image| json!({"artifact_id":image["artifact_id"],
                "source":{"tool":image["tool"],"arguments":image["arguments"],"document":image["document"],"meta":image["image_meta"]}}))
            .collect::<Vec<_>>();
        store(
            state.workspace.root(),
            &json!({"analysis_id":analysis_id,"task_id":context.task_id(),"analysis":output["analysis"],
                "model":model,"images":images,"sheet":sheet_record,"completed_at":visual_artifacts::now()}),
        )?;
    }
    result
}

/// The review instructions that the plugin which produced these images attached
/// to them: each distinct instruction text verbatim, then each image's subject.
fn review_brief(images: &[Value]) -> Option<String> {
    let mut parts = Vec::<String>::new();
    for image in images {
        if let Some(text) = image["review"]["instructions"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            && !parts.iter().any(|part| part == text)
        {
            parts.push(text.to_owned());
        }
    }
    for (index, image) in images.iter().enumerate() {
        if let Some(subject) = image["review"]["subject"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
        {
            parts.push(format!("Image {}:\n{subject}", index + 1));
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// 5×7 bitmaps for the digits that number sheet tiles.
const DIGITS: [[u8; 7]; 10] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
    [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
];

fn draw_number(canvas: &mut image::RgbImage, x: u32, y: u32, number: usize) {
    const SCALE: u32 = 4;
    for (place, digit) in number.to_string().bytes().map(|b| usize::from(b - b'0')).enumerate() {
        let left = x + place as u32 * 6 * SCALE;
        for (row, bits) in DIGITS[digit].iter().enumerate() {
            for column in 0..5u32 {
                if bits & (0b10000 >> column) == 0 {
                    continue;
                }
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        canvas.put_pixel(left + column * SCALE + dx, y + row as u32 * SCALE + dy, image::Rgb([255, 255, 255]));
                    }
                }
            }
        }
    }
}

/// One PNG holding `pictures` in reading order, each under a black bar with
/// its white number, on a grey ground. Tiles keep their pixels unless the
/// sheet would exceed the per-image input budget; then all shrink equally.
fn sheet(pictures: &[image::DynamicImage]) -> Result<(Vec<u8>, Value)> {
    use image::GenericImageView;
    ensure!(!pictures.is_empty() && pictures.len() <= MAX_ANALYSIS_IMAGES, "a sheet holds 1 to {MAX_ANALYSIS_IMAGES} images");
    let count = pictures.len() as u32;
    let columns = (f64::from(count).sqrt().ceil() as u32).min(3);
    let rows = count.div_ceil(columns);
    let cell_w = pictures.iter().map(|p| p.width()).max().unwrap_or(1);
    let cell_h = pictures.iter().map(|p| p.height()).max().unwrap_or(1);
    let mut scale = 1.0_f64;
    loop {
        let tile_w = ((f64::from(cell_w) * scale) as u32).max(1);
        let tile_h = ((f64::from(cell_h) * scale) as u32).max(1);
        let width = columns * tile_w + (columns + 1) * SHEET_GAP;
        let height = rows * (tile_h + SHEET_BAR) + (rows + 1) * SHEET_GAP;
        if u64::from(width) * u64::from(height) > visual_artifacts::MAX_INPUT_PIXELS {
            scale *= 0.95;
            continue;
        }
        let mut canvas = image::RgbImage::from_pixel(width, height, image::Rgb([128, 128, 128]));
        for (index, picture) in pictures.iter().enumerate() {
            let (column, row) = (index as u32 % columns, index as u32 / columns);
            let x = SHEET_GAP + column * (tile_w + SHEET_GAP);
            let y = SHEET_GAP + row * (tile_h + SHEET_BAR + SHEET_GAP);
            for py in y..y + SHEET_BAR {
                for px in x..x + tile_w {
                    canvas.put_pixel(px, py, image::Rgb([0, 0, 0]));
                }
            }
            draw_number(&mut canvas, x + 8, y + 4, index + 1);
            let (w, h) = picture.dimensions();
            let fitted = if scale < 1.0 || w > tile_w || h > tile_h {
                picture.resize(tile_w, tile_h, image::imageops::FilterType::Triangle)
            } else {
                picture.clone()
            };
            image::imageops::overlay(&mut canvas, &fitted.to_rgb8(), i64::from(x), i64::from(y + SHEET_BAR));
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(canvas).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        if png.len() > visual_artifacts::MAX_INPUT_BYTES {
            scale *= 0.9;
            continue;
        }
        return Ok((png, json!({"columns":columns,"rows":rows,"tile_width":tile_w,"tile_height":tile_h,"width":width,"height":height,"scale":scale})));
    }
}

fn connection(root: &std::path::Path) -> Result<rusqlite::Connection> {
    let conn = crate::agent_service::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_visual_analyses(analysis_id TEXT PRIMARY KEY,task_id TEXT NOT NULL,metadata TEXT NOT NULL);")?;
    Ok(conn)
}

fn store(root: &std::path::Path, record: &Value) -> Result<()> {
    connection(root)?.execute(
        "INSERT INTO agent_visual_analyses(analysis_id,task_id,metadata) VALUES (?1,?2,?3)",
        params![
            record["analysis_id"].as_str(),
            record["task_id"].as_str(),
            record.to_string()
        ],
    )?;
    Ok(())
}

/// Completed analyses named by a plugin call, in the order named. Each must
/// belong to one of `tasks`; an unknown ID stops the call.
pub fn analyses(root: &std::path::Path, tasks: &[&str], ids: &[String]) -> Result<Vec<Value>> {
    let conn = connection(root)?;
    ids.iter()
        .map(|id| {
            let found: Option<(String, String)> = conn
                .query_row(
                    "SELECT task_id,metadata FROM agent_visual_analyses WHERE analysis_id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (_, metadata) = found
                .filter(|(task, _)| tasks.contains(&task.as_str()))
                .with_context(|| {
                    format!("Unknown analysis_id {id}: use an analysis_id that analyze_images returned in this task")
                })?;
            Ok(serde_json::from_str(&metadata)?)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, body::Body, response::IntoResponse, routing::post};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn vision_sse_is_progress_tracked_without_execution_chat_deltas() {
        let root =
            std::env::temp_dir().join(format!("vision-stream-{}", visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = received.clone();
        let app=Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>|{
            let requests=requests.clone();
            async move {
                requests.lock().unwrap().push(body);
                let parts=[json!({"choices":[{"delta":{"reasoning_content":"检查图片"}}]}),
                    json!({"choices":[{"delta":{"content":"页面正常，"}}]}),
                    json!({"choices":[{"delta":{"content":"留白充足。"},"finish_reason":"stop"}]})];
                let mut wire=parts.iter().map(|p|format!("data: {p}\n\n")).collect::<String>();
                wire.push_str("data: [DONE]\n\n");
                let chunks=wire.as_bytes().chunks(7).map(|part|Ok::<_,std::convert::Infallible>(part.to_vec())).collect::<Vec<_>>();
                ([("content-type","text/event-stream")],Body::from_stream(futures::stream::iter(chunks))).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state =
            crate::agent_service::tests::test_state(&root, listener.local_addr().unwrap());
        state.visual_tool_settings.reasoning_effort = Some("low".into());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        let mut context = VisualContext {
            turn: Some(1),
            identity: json!({"task_id":"task","request_id":1,"node_id":"check","runtime":"single_agent"}),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgb8(12, 12)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let saved =
            visual_artifacts::save(&root, &context, json!({"source":"native_ppt","project_id":820,"page_index":5,"source_revision":22,"source_stable":true,"source_hash":"ab".repeat(32)}), &bytes).unwrap();
        context
            .allowed_artifact_ids
            .push(saved["artifact_id"].as_str().unwrap().to_owned());
        let result = execute(
            &state,
            &context,
            &json!({"artifact_ids":[saved["artifact_id"]],"question":"检查图片"}),
            "vision-call",
            "fake",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result["analysis"], "页面正常，留白充足。");
        assert_eq!(result["status"], "completed");
        assert!(result.get("page_reviews").is_none());
        let requests = received.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["messages"].as_array().unwrap().len(),1,"source metadata must not inject a business system prompt");
        assert_eq!(requests[0]["stream"], true);
        assert_eq!(requests[0]["reasoning_effort"], "low");
        assert!(requests[0].get("tools").is_none());
        assert_eq!(requests[0]["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM agent_task_events WHERE kind='assistant/delta'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        let raw: String = conn
            .query_row(
                "SELECT metadata FROM agent_request_contexts WHERE actor='tool'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let metadata: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(metadata["status"], "completed");
        assert_eq!(
            metadata["outcome"]["transport"]["decoder"]["text_chars"],
            10
        );
        assert_eq!(
            metadata["outcome"]["transport"]["decoder"]["reasoning_chars"],
            4
        );
        assert_eq!(
            metadata["outcome"]["response"]["choices"][0]["message"]["content"],
            result["analysis"]
        );
        drop(requests);
        server.abort();
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn plugin_review_instructions_are_sent_verbatim_as_the_system_message() {
        let root = std::env::temp_dir().join(format!("vision-brief-{}", visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = received.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let requests = requests.clone();
            async move {
                requests.lock().unwrap().push(body);
                Json(json!({"choices":[{"message":{"role":"assistant","content":"转录：一、二\n```json\n{\"images\":[]}\n```"},"finish_reason":"stop"}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = crate::agent_service::tests::test_state(&root, listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        let mut context = VisualContext {
            turn: Some(1),
            identity: json!({"task_id":"task","request_id":1,"node_id":"check","runtime":"single_agent"}),
            ..Default::default()
        };
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let instructions = "先逐区转录可见文字，再列缺陷、按大纲分级，最后给结论。\n调用方的问题只增加关注点。";
        let mut ids = Vec::new();
        for (slide, subject) in [(2, "第 2 页：目录"), (3, "第 3 页：机制链")] {
            let saved = visual_artifacts::save(&root, &context, json!({"source":"plugin_page","tool":"demo__render",
                "arguments":{"slide":slide},"document":{"revision":9,"stable":true},
                "review":{"instructions":instructions,"subject":subject}}), &png).unwrap();
            ids.push(saved["artifact_id"].as_str().unwrap().to_owned());
        }
        context.allowed_artifact_ids = ids.clone();
        let result = execute(&state, &context, &json!({"artifact_ids":ids,"question":"我刚改了第 2 页的副标题"}),
            "vision-call", "fake", &CancellationToken::new()).await.unwrap();

        let requests = received.lock().unwrap();
        let messages = requests[0]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], json!({"role":"system",
            "content":format!("{instructions}\n\nImage 1:\n第 2 页：目录\n\nImage 2:\n第 3 页：机制链")}),
            "shared instructions once, then each image's own subject, unchanged");
        let content = messages[1]["content"].as_array().unwrap();
        assert_eq!(content[0], json!({"type":"text","text":"我刚改了第 2 页的副标题"}));
        assert_eq!(content[1], json!({"type":"text","text":"Image 1"}));
        assert_eq!(content[2]["type"], "image_url");
        assert_eq!(content[3], json!({"type":"text","text":"Image 2"}));
        assert_eq!(content[4]["type"], "image_url");
        drop(requests);

        let analysis_id = result["analysis_id"].as_str().unwrap().to_owned();
        assert!(analysis_id.starts_with("analysis-"));
        let records = analyses(&root, &["task"], &[analysis_id.clone()]).unwrap();
        assert_eq!(records[0]["analysis"], result["analysis"]);
        assert_eq!(records[0]["images"][0]["artifact_id"], json!(context.allowed_artifact_ids[0]));
        assert_eq!(records[0]["images"][1]["source"]["arguments"], json!({"slide":3}));
        assert_eq!(records[0]["images"][1]["source"]["document"]["revision"], 9);
        let foreign = analyses(&root, &["other-task"], &[analysis_id]).unwrap_err().to_string();
        assert!(foreign.contains("Unknown analysis_id"), "{foreign}");
        server.abort();
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn three_or_more_images_go_to_the_vision_model_on_one_numbered_sheet() {
        let root = std::env::temp_dir().join(format!("vision-sheet-{}", visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = received.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let requests = requests.clone();
            async move {
                requests.lock().unwrap().push(body);
                Json(json!({"choices":[{"message":{"role":"assistant","content":"五页都看过了"},"finish_reason":"stop"}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = crate::agent_service::tests::test_state(&root, listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','test','fake','running',0,0)",[]).unwrap();
        let mut context = VisualContext {
            turn: Some(1),
            identity: json!({"task_id":"task","request_id":1,"node_id":"check","runtime":"single_agent"}),
            ..Default::default()
        };
        let colors = [[200u8, 0, 0], [0, 160, 0], [0, 0, 200], [200, 200, 0], [0, 200, 200]];
        let mut ids = Vec::new();
        for (page, color) in colors.iter().enumerate() {
            let mut png = Vec::new();
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(64, 36, image::Rgb(*color)))
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
            let saved = visual_artifacts::save(&root, &context, json!({"source":"plugin_page","image_meta":{"slide":page + 1}}), &png).unwrap();
            ids.push(saved["artifact_id"].as_str().unwrap().to_owned());
        }
        context.allowed_artifact_ids = ids.clone();
        let result = execute(&state, &context, &json!({"artifact_ids":ids,"question":"检查整套页面"}), "vision-call", "fake", &CancellationToken::new())
            .await.unwrap();

        let requests = received.lock().unwrap();
        let content = requests[0]["messages"][0]["content"].as_array().unwrap().clone();
        drop(requests);
        assert_eq!(content.len(), 3, "question, sheet explanation, one sheet");
        assert!(content[1]["text"].as_str().unwrap().contains("Images 1-5 are tiled on this sheet"));
        let url = content[2]["image_url"]["url"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(url.split_once(',').unwrap().1).unwrap();
        let sheet = image::load_from_memory(&bytes).unwrap().to_rgb8();
        // ceil(sqrt(5)) = 3 columns, 2 rows; tiles keep their pixels.
        assert_eq!((sheet.width(), sheet.height()), (3 * 64 + 4 * SHEET_GAP, 2 * (36 + SHEET_BAR) + 3 * SHEET_GAP));
        for (index, color) in colors.iter().enumerate() {
            let x = SHEET_GAP + (index as u32 % 3) * (64 + SHEET_GAP);
            let y = SHEET_GAP + (index as u32 / 3) * (36 + SHEET_BAR + SHEET_GAP);
            assert_eq!(sheet.get_pixel(x + 32, y + SHEET_BAR + 18).0, *color, "tile {} holds image {}", index + 1, index + 1);
            assert_eq!(sheet.get_pixel(x + 60, y + 2).0, [0, 0, 0], "a black number bar sits above tile {}", index + 1);
        }
        // The first bar's "1": the middle column of its top row is set.
        assert_eq!(sheet.get_pixel(SHEET_GAP + 8 + 2 * 4, SHEET_GAP + 4).0, [255, 255, 255]);

        let record = analyses(&root, &["task"], &[result["analysis_id"].as_str().unwrap().to_owned()]).unwrap().remove(0);
        assert_eq!(record["images"].as_array().unwrap().len(), 5);
        assert_eq!(record["images"][4]["source"]["meta"], json!({"slide":5}));
        let (sheet_artifact, saved_bytes) = visual_artifacts::read(&root, "task", record["sheet"]["artifact_id"].as_str().unwrap()).unwrap();
        assert_eq!(sheet_artifact["source"], "analysis_sheet");
        assert_eq!(saved_bytes, bytes, "the saved sheet is exactly what the vision model received");

        let mut ten = ids.clone();
        ten.extend((0..5).map(|n| format!("visual-extra-{n}")));
        let error = execute(&state, &context, &json!({"artifact_ids":ten,"question":"太多"}), "vision-call", "fake", &CancellationToken::new())
            .await.unwrap_err().to_string();
        assert!(error.contains("between 1 and 9 images"), "{error}");
        server.abort();
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
}
