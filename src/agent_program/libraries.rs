//! Static source libraries bind to real tools, never to guessed operations.
use super::*;
use anyhow::{Context, Result, ensure};
use std::collections::BTreeSet;

pub type Libraries = BTreeMap<String, BTreeMap<String, String>>;

pub fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || matches!(c, '_' | '$'))
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
}
fn reserved(name: &str) -> bool {
    matches!(
        name,
        "tools"
            | "JSON"
            | "call"
            | "fork"
            | "join"
            | "ai"
            | "assert"
            | "main"
            | "function"
            | "var"
            | "let"
            | "const"
            | "if"
            | "else"
            | "while"
            | "for"
            | "do"
            | "return"
            | "throw"
            | "try"
            | "catch"
            | "finally"
            | "class"
            | "new"
            | "this"
            | "true"
            | "false"
            | "null"
            | "switch"
            | "break"
            | "continue"
            | "import"
            | "export"
    ) || name.starts_with("$temp")
}
fn namespace(id: &str) -> String {
    let mut name = id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '$') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if name.chars().next().is_some_and(|c| c.is_numeric()) {
        name.insert(0, '_');
    }
    name
}
/// These are file-tool API aliases, not domain workflow rules.
pub fn builtin() -> Libraries {
    BTreeMap::from([(
        "io".into(),
        [
            ("read", "read_file"),
            ("read_lines", "read_file_lines"),
            ("write", "write_file"),
            ("list", "list_dir"),
            ("search", "search_text"),
            ("edit", "edit_file"),
            ("replace", "replace_range"),
        ]
        .into_iter()
        .map(|(method, tool)| (method.into(), tool.into()))
        .collect(),
    )])
}
/// Only tools present in this frame's catalog are published. Colliding plugin
/// namespaces are omitted rather than silently routing to the wrong owner.
pub fn catalog(definitions: &[Value], packages: &Value) -> Libraries {
    let allowed = definitions
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect::<BTreeSet<_>>();
    let mut result = builtin();
    for methods in result.values_mut() {
        methods.retain(|_, tool| allowed.contains(tool.as_str()));
    }
    result.retain(|_, methods| !methods.is_empty());
    let mut candidates: BTreeMap<String, Vec<BTreeMap<String, String>>> = BTreeMap::new();
    for package in packages.as_array().into_iter().flatten() {
        if package["enabled"] != true {
            continue;
        }
        let Some(id) = package["id"].as_str() else {
            continue;
        };
        let library = namespace(id);
        if !identifier(&library) || reserved(&library) || library == "io" {
            continue;
        }
        let prefix = format!("{}__", id.replace('-', "_"));
        let methods = package["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|name| allowed.contains(name))
            .filter_map(|name| {
                let method = name.strip_prefix(&prefix).unwrap_or(name);
                identifier(method).then(|| (method.to_owned(), name.to_owned()))
            })
            .collect::<BTreeMap<_, _>>();
        if !methods.is_empty() {
            candidates.entry(library).or_default().push(methods);
        }
    }
    for (name, mut methods) in candidates {
        if methods.len() == 1 {
            result.insert(name, methods.remove(0));
        }
    }
    result
}
pub fn merge(
    mut base: Libraries,
    additions: &Value,
    allowed: Option<&BTreeSet<String>>,
) -> Result<Libraries> {
    if additions.is_null() {
        return Ok(base);
    }
    let additions = additions
        .as_object()
        .context("libraries must map namespace names to method/tool mappings")?;
    ensure!(additions.len() <= 64, "at most 64 library aliases");
    for (name, methods) in additions {
        ensure!(
            identifier(name) && !reserved(name),
            "invalid or reserved library namespace {name}"
        );
        let methods = methods
            .as_object()
            .context("library methods must map names to exact tool names")?;
        ensure!(methods.len() <= 256, "too many library methods");
        for (method, tool) in methods {
            ensure!(identifier(method), "invalid library method {method}");
            let tool = tool
                .as_str()
                .filter(|n| !n.is_empty() && n.len() <= 256)
                .context("library target must be an exact tool name")?;
            ensure!(
                !matches!(
                    tool,
                    "execute_program"
                        | "define_task_body"
                        | "create_task"
                        | "spawn_agent"
                        | "join_agents"
                        | "return_task"
                ),
                "library methods cannot control the task scheduler"
            );
            ensure!(
                allowed.is_none_or(|names| names.contains(tool)),
                "LIBRARY_TOOL_UNAVAILABLE: {tool}"
            );
            let entry = base.entry(name.clone()).or_default();
            ensure!(
                entry.get(method).is_none_or(|old| old == tool),
                "cannot rebind published library method {name}.{method}"
            );
            entry.insert(method.clone(), tool.to_owned());
        }
    }
    Ok(base)
}
