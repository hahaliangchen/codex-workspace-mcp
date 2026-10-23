//! An ordinary read-only tool. Its model input consists only of caller-supplied
//! question and saved pixels; it never edits or inherits a conversation window.
use crate::{
    agent_service::AgentServiceState,
    visual_artifacts::{self, VisualContext},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tokio_util::sync::CancellationToken;

pub fn tool() -> Value {
    json!({"type":"function","function":{
        "name":"analyze_images",
        "description":"Analyze saved images with a dedicated vision model. Use for image interpretation and visual verification. Supply artifact_ids from screenshot/image assets and your question, including necessary task background. Returns the model's original analysis as tool evidence. Reads existing original pixels; does not browse, recapture, select images for your next request, or change context.",
        "parameters":{"type":"object","properties":{
            "artifact_ids":{"type":"array","minItems":1,"maxItems":visual_artifacts::MAX_IMAGES,"uniqueItems":true,"items":{"type":"string","minLength":1}},
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
        !ids.is_empty() && ids.len() <= visual_artifacts::MAX_IMAGES,
        "invalid image count"
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
        !ids.is_empty() && ids.len() <= visual_artifacts::MAX_IMAGES,
        "provide between 1 and {} images",
        visual_artifacts::MAX_IMAGES
    );
    let question = args["question"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("question must not be empty")?;
    let mut content = vec![json!({"type":"text","text":question})];
    let mut sent = Vec::new();
    let mut unique = BTreeSet::new();
    for value in ids {
        let id = value
            .as_str()
            .filter(|s| !s.is_empty())
            .context("artifact id must be a nonempty string")?;
        ensure!(unique.insert(id), "duplicate artifact id: {id}");
        // Strict original mode rejects an over-budget image instead of silently
        // resizing it. Ownership and byte hashes are checked by the shared reader.
        let (block, metadata) =
            visual_artifacts::image_block(state.workspace.root(), context, id, true)?;
        content.push(block);
        sent.push(metadata);
    }
    ensure!(
        !state.provider_url.is_empty(),
        "image analysis provider is unavailable"
    );
    let model = &state.default_model;
    let mut body = json!({"model":model,"stream":true,"max_tokens":8192,
        "messages":[{"role":"user","content":content}]});
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
    let trace = crate::request_context::record(
        state.workspace.root(),
        context.task_id(),
        json!({"actor":"tool","stage":"tool_execution","tool_name":"analyze_images",
            "turn":context.turn,"nodeId":node,"parent_node_id":context.identity["node_id"],
            "tool_call_id":call_id,"request_flow":false,"images":sent,
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
        let output = json!({"status":"completed","analysis":analysis,"artifact_ids":ids,"model":model,
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
    result
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
        std::fs::remove_dir_all(root).unwrap();
    }

}
