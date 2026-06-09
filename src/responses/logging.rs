use serde_json::Value;
use std::io::Write;
use std::sync::Mutex;

pub fn log_request_body(
    log: &Mutex<std::fs::File>,
    db: &Mutex<rusqlite::Connection>,
    sys_log: &Mutex<std::fs::File>,
    body: &Value,
    client_model: &str,
) {
    crate::ai_proxy::log_write(
        log,
        Some(db),
        Some("REQ_IN"),
        Some("user"),
        &format!(
            "=== /v1/responses received from Codex  model={}",
            client_model
        ),
    );

    let mut body_brief = body.clone();
    if let Some(obj) = body_brief.as_object_mut() {
        if let Some(input_arr) = obj.get_mut("input") {
            if let Some(arr) = input_arr.as_array() {
                let count = arr.len();
                write_system_prompt_log(sys_log, arr, client_model);
                *input_arr = Value::String(format!(
                    "[{} items -> see system_prompt.log]",
                    count
                ));
            }
        }
    }

    crate::ai_proxy::log_write(
        log,
        Some(db),
        Some("REQ_IN"),
        Some("user"),
        &format!(
            "   Codex Responses body: {}",
            crate::ai_proxy::fmt_body(
                serde_json::to_string(&body_brief)
                    .unwrap_or_default()
                    .as_bytes()
            )
        ),
    );
}

fn write_system_prompt_log(
    sys_log: &Mutex<std::fs::File>,
    input_items: &[Value],
    client_model: &str,
) {
    let ts = crate::ai_proxy::now_china();
    let sep = format!("\n{} ===== /v1/responses model={} =====\n", ts, client_model);
    if let Ok(mut sf) = sys_log.lock() {
        let _ = sf.write_all(sep.as_bytes());
        for (idx, item) in input_items.iter().enumerate() {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("-");
            let item_str = serde_json::to_string_pretty(item).unwrap_or_default();
            let line = format!("--- input[{}] role={} ---\n{}\n", idx, role, item_str);
            let _ = sf.write_all(line.as_bytes());
        }
        let _ = sf.flush();
    }
}

pub fn log_diagnostics(
    log: &Mutex<std::fs::File>,
    db: &Mutex<rusqlite::Connection>,
    body: &Value,
) {
    let prev_id = body
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .unwrap_or("<none>");
    crate::ai_proxy::log_write(
        log,
        Some(db),
        Some("DIAG"),
        Some("proxy"),
        &format!("   [DIAG] previous_response_id={}", prev_id),
    );

    if let Some(input_arr) = body.get("input").and_then(|v| v.as_array()) {
        log_input_counts(log, db, input_arr);
        log_tool_related_items(log, db, input_arr);
    }

    let msgs_exist = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    crate::ai_proxy::log_write(
        log,
        Some(db),
        Some("DIAG"),
        Some("proxy"),
        &format!("   [DIAG] messages field count={}", msgs_exist),
    );
}

fn log_input_counts(
    log: &Mutex<std::fs::File>,
    db: &Mutex<rusqlite::Connection>,
    input_arr: &[Value],
) {
    let mut type_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut role_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for item in input_arr {
        let t = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let r = item
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
            .to_string();
        *type_counts.entry(t).or_insert(0) += 1;
        *role_counts.entry(r).or_insert(0) += 1;
    }

    crate::ai_proxy::log_write(
        log,
        Some(db),
        Some("DIAG"),
        Some("proxy"),
        &format!(
            "   [DIAG] input total={} type_counts={:?} role_counts={:?}",
            input_arr.len(),
            type_counts,
            role_counts
        ),
    );
}

fn log_tool_related_items(
    log: &Mutex<std::fs::File>,
    db: &Mutex<rusqlite::Connection>,
    input_arr: &[Value],
) {
    for (idx, item) in input_arr.iter().enumerate() {
        let t = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let r = item.get("role").and_then(|v| v.as_str()).unwrap_or("-");
        if t == "function_call" || t == "function_call_output" || r == "assistant" {
            let brief = serde_json::to_string(item).unwrap_or_default();
            let brief_safe = brief.chars().take(300).collect::<String>();
            crate::ai_proxy::log_write(
                log,
                Some(db),
                Some("TOOL_MATCH"),
                Some(r),
                &format!(
                    "   [DIAG] input[{}] type={} role={} : {}",
                    idx, t, r, brief_safe
                ),
            );
        }
    }
}
