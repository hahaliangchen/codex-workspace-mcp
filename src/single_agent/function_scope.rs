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
    window.previous_node = None;
}
