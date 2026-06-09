use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

static TOOL_CALL_REGISTRY: OnceLock<Mutex<HashMap<String, (String, String)>>> = OnceLock::new();

pub fn insert(id: &str, name: &str, arguments: &str) {
    registry()
        .lock()
        .unwrap()
        .insert(id.to_string(), (name.to_string(), arguments.to_string()));
}

pub fn get(id: &str) -> Option<(String, String)> {
    registry().lock().unwrap().get(id).cloned()
}

pub fn take(id: &str) -> Option<(String, String)> {
    registry().lock().unwrap().remove(id)
}

fn registry() -> &'static Mutex<HashMap<String, (String, String)>> {
    TOOL_CALL_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}
