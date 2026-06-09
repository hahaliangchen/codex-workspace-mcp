use super::responses_events::{build_completed_tool_output_item, tool_item_id};
use crate::tool_display;
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Default, Clone)]
pub(super) struct ToolCallState {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
    pub(super) added_emitted: bool,
    pub(super) done_emitted: bool,
}

pub(super) fn tool_call_index(tc: &Value) -> usize {
    tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize
}

fn empty_accumulated_tool_call() -> Value {
    json!({
        "id": "",
        "type": "function",
        "function": {
            "name": "",
            "arguments": ""
        }
    })
}

pub(super) fn ensure_accumulated_tool_call(calls: &mut Vec<Value>, idx: usize) -> &mut Value {
    while calls.len() <= idx {
        calls.push(empty_accumulated_tool_call());
    }
    &mut calls[idx]
}

pub(super) fn apply_tool_delta_to_state(
    tc: &Value,
    target: &mut Value,
    state: &mut ToolCallState,
) -> Option<String> {
    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
        target["id"] = json!(id);
        state.id = id.to_owned();
    }

    let func = tc.get("function")?;

    if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
        target["function"]["name"] = json!(name);
        state.name = name.to_owned();
    }

    let args = func.get("arguments").and_then(|v| v.as_str())?;
    let cur = target["function"]["arguments"].as_str().unwrap_or("");
    target["function"]["arguments"] = json!(format!("{}{}", cur, args));
    state.arguments.push_str(args);
    Some(args.to_owned())
}

pub(super) fn collect_completed_tool_outputs(
    response_id: &str,
    accumulated_tool_calls: &[Value],
    tool_status: &HashMap<usize, ToolCallState>,
    tool_route_map: &HashMap<String, (String, String)>,
) -> Vec<Value> {
    let mut output_items = Vec::new();

    for (idx, tc) in accumulated_tool_calls.iter().enumerate() {
        let raw_name = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        if raw_name.is_empty() {
            continue;
        }

        let call_id = tc
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if let Some(st) = tool_status.get(&idx) {
            let (name, arguments) = tool_display::completed_output_tool(
                &call_id,
                &st.name,
                &st.arguments,
                tool_route_map,
            );
            output_items.push(build_completed_tool_output_item(
                &tool_item_id(response_id, idx),
                &call_id,
                &name,
                &arguments,
            ));
        }
    }

    output_items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_tool_delta_accumulates_arguments_chunks() {
        let mut calls = Vec::new();
        let target = ensure_accumulated_tool_call(&mut calls, 0);
        let mut state = ToolCallState::default();

        let first = json!({
            "index": 0,
            "id": "call_1",
            "function": {
                "name": "read_file",
                "arguments": "{\"path\":"
            }
        });
        let second = json!({
            "index": 0,
            "function": {
                "arguments": "\"a.rs\"}"
            }
        });

        assert_eq!(
            apply_tool_delta_to_state(&first, target, &mut state).as_deref(),
            Some("{\"path\":")
        );
        let target = ensure_accumulated_tool_call(&mut calls, 0);
        assert_eq!(
            apply_tool_delta_to_state(&second, target, &mut state).as_deref(),
            Some("\"a.rs\"}")
        );

        assert_eq!(state.id, "call_1");
        assert_eq!(state.name, "read_file");
        assert_eq!(state.arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn collect_completed_tool_outputs_skips_empty_tool_names() {
        let mut calls = Vec::new();
        ensure_accumulated_tool_call(&mut calls, 0);
        let target = ensure_accumulated_tool_call(&mut calls, 1);
        let mut state = ToolCallState::default();
        let delta = json!({
            "index": 1,
            "id": "call_2",
            "function": {
                "name": "read_file",
                "arguments": "{\"path\":\"a.rs\"}"
            }
        });
        apply_tool_delta_to_state(&delta, target, &mut state);

        let mut statuses = HashMap::new();
        statuses.insert(1, state);
        let outputs = collect_completed_tool_outputs("resp_1", &calls, &statuses, &HashMap::new());

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0]["id"], "item_tool_resp_1_1");
        assert_eq!(outputs[0]["call_id"], "call_2");
        assert_eq!(outputs[0]["name"], "read_file");
    }
}
