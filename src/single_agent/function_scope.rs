//! Function variables live in the saved frame, not in replayed tool history.
use super::*;

pub(super) fn values(window: &Window) -> BTreeMap<String, Value> {
    let mut values = window.scope_values.clone();
    if let Some(runtime) = &window.program {
        values.extend(runtime.named_bindings());
    } else if let Some(task) = &window.program_task {
        for (name, value) in task["parameters"]
            .as_array()
            .into_iter()
            .flatten()
            .zip(task["arguments"].as_array().into_iter().flatten())
        {
            if let Some(name) = name.as_str() {
                values.insert(name.to_owned(), value.clone());
            }
        }
        // A failed body's live locals are closer than the original argument
        // bindings, including when that body reassigned a parameter.
        if let Some(failed) = window.failed_programs.last() {
            values.extend(failed.named_bindings());
        }
    }
    values
}

/// Advertise visible names without sending unused values to the model. The
/// full entry snapshot remains available through read_scope.
pub(super) fn index(window: &Window) -> Option<Value> {
    let parameters = window
        .program_task
        .as_ref()
        .and_then(|task| task["parameters"].as_array());
    let entries = window
        .scope_values
        .iter()
        .filter(|(name, _)| {
            !parameters.is_some_and(|p| p.iter().any(|p| p.as_str() == Some(name.as_str())))
        })
        .map(|(name, value)| json!({"name":name,"type":match value { Value::Null=>"null",Value::Bool(_)=>"boolean",Value::Number(_)=>"number",Value::String(_)=>"string",Value::Array(_)=>"array",Value::Object(_)=>"object" },"bytes":value.to_string().len()}))
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return None;
    }
    Some(
        json!({"role":"user","name":"scope_variables","_frame_input":true,
        "content":json!({"host_status":true,"not_a_user_request":true,"variables":entries,
        "instruction":"Visible caller-variable names in the read-only entry snapshot. Current parameters are in program_task_input.bindings. Use host.read_scope({name,path}) when another value is needed, without replaying the original action. Later parent changes are not included."}).to_string()}),
    )
}

pub(super) fn tool() -> Value {
    let mut definition = function(
        "read_scope",
        "Read a current or visible parent function variable from the saved scope, without rereading files or replaying tools. Use current arguments directly; consult scope_variables for other names. Optional path selects nested object keys or array indices (as strings). Returns {name, path, value}; values are read-only snapshots and never modify the caller.",
        json!({"name":{"type":"string","minLength":1},"path":{"type":"array","items":{"type":"string"}}}),
        json!(["name"]),
    );
    definition["function"]["outputSchema"] = json!({"type":"object","properties":{
        "name":{"type":"string"},"path":{"type":"array","items":{"type":"string"}},"value":{}},"required":["name","path","value"]});
    definition
}

pub(super) fn read(window: &Window, args: &Value) -> Result<Value> {
    let name = args["name"]
        .as_str()
        .context("scope variable name required")?;
    let mut value = values(window).remove(name).context(
        "scope variable not found; use the current bindings or names in scope_variables",
    )?;
    let path = args
        .get("path")
        .map(|p| p.as_array().context("path must be an array"))
        .transpose()?
        .cloned()
        .unwrap_or_default();
    for key in &path {
        let key = key.as_str().context("scope path keys must be strings")?;
        value = match &value {
            Value::Object(fields) => fields.get(key),
            Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)),
            _ => None,
        }
        .context("scope path not found")?
        .clone();
    }
    Ok(json!({"name":name,"path":path,"value":value}))
}

/// A completed program returns its value; its working transcript is no longer
/// a live scope. The immutable event log remains the source for history reads.
pub(super) fn release_finished(window: &mut Window) {
    if window.program_value.is_none() || window.program_task.is_some() {
        return;
    }
    let answer = window
        .messages
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant" && m["tool_calls"].is_null())
        .cloned();
    window.messages.retain(|m| {
        (m["role"] == "system" && m.get("name").is_none())
            || task_frames::is_user_original(m)
            || m["_pinned"] == true
    });
    if let Some(answer) = answer {
        window.messages.push(answer);
    }
    window.scope_values.clear();
    window.current_step_node_id = None;
    window.previous_node = None;
}
