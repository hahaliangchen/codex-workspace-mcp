//! An ordinary read-only tool. Its model input consists only of caller-supplied
//! question and saved pixels; it never edits or inherits a conversation window.
use crate::{
    agent_service::AgentServiceState,
    visual_artifacts::{self, VisualContext},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use tokio_util::sync::CancellationToken;

pub fn tool() -> Value {
    json!({"type":"function","function":{
        "name":"analyze_images",
        "description":"Analyze saved images with a dedicated vision model. Use for image interpretation and visual verification. Supply artifact_ids from screenshot/image assets and your question, including necessary task background. Returns the model's original analysis as tool evidence. Reads existing original pixels; does not browse, recapture, select images for your next request, or change context.",
        "parameters":{"type":"object","properties":{
            "artifact_ids":{"type":"array","minItems":1,"maxItems":visual_artifacts::MAX_IMAGES,"uniqueItems":true,"items":{"type":"string","minLength":1}},
            "question":{"type":"string","minLength":1}
        },"required":["artifact_ids","question"],"additionalProperties":false}
    }})
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
    let mut body = json!({"model":model,"stream":false,"max_tokens":8192,
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
    let trace = crate::request_context::record(state.workspace.root(), context.task_id(),
        json!({"actor":"tool","stage":"tool_execution","tool_name":"analyze_images",
            "turn":context.turn,"nodeId":visual_artifacts::new_id(),"parent_node_id":context.identity["node_id"],
            "tool_call_id":call_id,"request_flow":false,"images":sent,
            "model_route":{"provider":state.provider_name,"model":model}}), &body).await
        .context("Cannot persist image analysis input; request was not sent")?;
    let reference = trace.history_reference();
    let mut raw_response = String::new();
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
        result = tokio::time::timeout(Duration::from_secs(180), async {
            let response = state.client.post(format!("{}/chat/completions",state.provider_url.trim_end_matches('/')))
                .bearer_auth(&state.api_key).header("x-codex-visual-dispatch","task-owned").json(&body).send().await?;
            let status = response.status();
            raw_response = response.text().await?;
            ensure!(status.is_success(), "image analysis provider returned HTTP {status}: {raw_response}");
            let response:Value = serde_json::from_str(&raw_response).context("invalid image analysis response JSON")?;
            let choice = &response["choices"][0];
            ensure!(choice["finish_reason"]=="stop", "image analysis did not finish normally: {}",choice["finish_reason"]);
            ensure!(choice["message"]["tool_calls"].as_array().is_none_or(Vec::is_empty), "image analysis returned unexpected tool calls");
            let analysis = choice["message"]["content"].as_str().filter(|s|!s.trim().is_empty()).context("image analysis returned no text")?;
            Ok::<_,anyhow::Error>(json!({"analysis":analysis,"artifact_ids":ids,"model":model,
                "reasoning_effort":state.reasoning_effort,"request_reference":reference}))
        }) => result.map_err(|_|anyhow::anyhow!("image analysis timed out")).and_then(|r|r),
    };
    let status = if result.is_ok() {
        "completed"
    } else if cancel.is_cancelled() {
        "cancelled"
    } else {
        "failed"
    };
    let response =
        serde_json::from_str::<Value>(&raw_response).unwrap_or_else(|_| json!(raw_response));
    trace
        .finish_checked(
            status,
            json!({"response":response,"error":result.as_ref().err().map(|e|format!("{e:#}"))}),
        )
        .await?;
    result
}
