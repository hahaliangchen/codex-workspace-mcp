use serde_json::Value;

pub fn event_to_narrative(event: &crate::expert_surgery::SurgeryEvent) -> String {
    match event {
        crate::expert_surgery::SurgeryEvent::ProModelInvoked => {
            "\n[agent:expert] Pro model invoked for code surgery...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::ProModelGraphDone { elapsed_ms } => {
            format!("\n[agent:expert] Pro model execution completed in {}ms.\n", elapsed_ms)
        }
        crate::expert_surgery::SurgeryEvent::PreWriteVerificationStarted => {
            "\n[agent:verify] Pre-write consistency verification started...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::FileConsistentApproved => {
            "\n[agent:verify] Disk file is consistent with pre-await snapshot. Approved for writing.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::OffsetDriftDetected => {
            "\n[agent:drift] Disk change/offset drift detected. Initializing 3-way relocation...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::ThreeWayRelocationSuccess { byte_range } => {
            format!("\n[agent:drift] 3-way relocation succeeded. Offset adjusted to range {}..{}.\n", byte_range.0, byte_range.1)
        }
        crate::expert_surgery::SurgeryEvent::HardConflictEncountered => {
            "\n[agent:drift] Hard conflict encountered: could not align patch block on disk.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::FlashResolverStarted => {
            "\n[agent:expert] Flash model resolver started for semantic merge...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::FlashResolverSuccess => {
            "\n[agent:expert] Flash resolver successfully merged conflict blocks.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::LocalLintStarted => {
            "\n[agent:verify] Local verification (AST check & cargo check) started...\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::SyntaxTreeVerified => {
            "\n[agent:verify] AST syntax tree verified successfully.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::CargoCheckPassed => {
            "\n[agent:verify] Local compilation check passed.\n".to_string()
        }
        crate::expert_surgery::SurgeryEvent::TransactionRolledBack { reason } => {
            format!("\n[agent:verify] Transaction rolled back: {}. Restoring original file contents.\n", reason)
        }
    }
}

pub fn format_response_created(response_id: &str, model: &str) -> String {
    format!(
        "event: response.created\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "in_progress",
                "model": model
            }
        })).unwrap()
    )
}

pub fn format_output_item_added(response_id: &str, item_id: &str) -> String {
    format!(
        "event: response.output_item.added\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.output_item.added",
            "response_id": response_id,
            "output_index": 0,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        })).unwrap()
    )
}

pub fn format_content_part_added(response_id: &str) -> String {
    format!(
        "event: response.content_part.added\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.content_part.added",
            "response_id": response_id,
            "output_index": 0,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": ""
            }
        })).unwrap()
    )
}

pub fn format_text_delta(response_id: &str, item_id: &str, delta: &str) -> String {
    format!(
        "event: response.output_text.delta\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.output_text.delta",
            "response_id": response_id,
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "delta": delta
        })).unwrap()
    )
}

pub fn format_text_done(response_id: &str, item_id: &str, text: &str) -> String {
    format!(
        "event: response.output_text.done\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.output_text.done",
            "response_id": response_id,
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "text": text
        })).unwrap()
    )
}

pub fn format_content_part_done(response_id: &str, text: &str) -> String {
    format!(
        "event: response.content_part.done\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.content_part.done",
            "response_id": response_id,
            "output_index": 0,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": text
            }
        })).unwrap()
    )
}

pub fn format_output_item_done(response_id: &str, message_item: &Value) -> String {
    format!(
        "event: response.output_item.done\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.output_item.done",
            "response_id": response_id,
            "output_index": 0,
            "item": message_item
        })).unwrap()
    )
}

pub fn format_response_completed(response_id: &str, model: &str, output: &[Value]) -> String {
    format!(
        "event: response.completed\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "completed",
                "model": model,
                "output": output,
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "total_tokens": 0
                }
            }
        })).unwrap()
    )
}

pub fn format_delegated_tool_call(response_id: &str, output_index: usize, tool_call: &Value) -> String {
    let added = format!(
        "event: response.output_item.added\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.output_item.added",
            "response_id": response_id,
            "output_index": output_index,
            "item": tool_call
        })).unwrap()
    );
    let done = format!(
        "event: response.output_item.done\ndata: {}\n\n",
        serde_json::to_string(&serde_json::json!({
            "type": "response.output_item.done",
            "response_id": response_id,
            "output_index": output_index,
            "item": tool_call
        })).unwrap()
    );
    format!("{}{}", added, done)
}
