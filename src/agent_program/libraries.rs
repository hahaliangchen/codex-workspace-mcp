//! Static source libraries bind to real tools, never to guessed operations.
use super::*;
use std::collections::BTreeSet;

pub type Libraries = BTreeMap<String, BTreeMap<String, String>>;

pub fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || matches!(c, '_' | '$'))
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
}
pub fn reserved(name: &str) -> bool {
    matches!(
        name,
        "await"
            | "yield"
            | "delete"
            | "void"
            | "typeof"
            | "instanceof"
            | "in"
            | "super"
            | "with"
            | "debugger"
            | "extends"
            | "enum"
            | "implements"
            | "interface"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "default"
            | "case"
            | "tools"
            | "JSON"
            | "Promise"
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
pub fn namespace(id: &str) -> String {
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
    let owned = packages
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["tools"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let mut result = builtin();
    for methods in result.values_mut() {
        methods.retain(|_, tool| allowed.contains(tool.as_str()) && !owned.contains(tool.as_str()));
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
        let library = package["library"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| namespace(id));
        if !valid_namespace(&library) {
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
    let bound = result
        .values()
        .flat_map(|methods| methods.values().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let rest = allowed
        .iter()
        .filter(|name| !bound.contains(**name) && !owned.contains(**name) && !control(name))
        .map(|name| ((*name).to_owned(), (*name).to_owned()))
        .collect::<BTreeMap<_, _>>();
    if !rest.is_empty() {
        result.insert("host".into(), rest);
    }
    result
}

/// Host-owned namespaces cannot be claimed by plugins.
pub fn valid_namespace(name: &str) -> bool {
    identifier(name) && !reserved(name) && !matches!(name, "io" | "host")
}
pub fn control(name: &str) -> bool {
    matches!(
        name,
        "execute_program"
            | "check_program"
            | "define_task_body"
            | "create_task"
            | "spawn_agent"
            | "join_agents"
            | "list_skills"
            | "read_skill"
            | "unload_skill"
            | "release_assets"
    )
}
pub fn public_name(libraries: &Libraries, target: &str) -> Option<String> {
    libraries.iter().find_map(|(library, methods)| {
        methods
            .iter()
            .find_map(|(method, tool)| (tool == target).then(|| format!("{library}.{method}")))
    })
}
pub fn target<'a>(libraries: &'a Libraries, name: &str) -> Option<&'a str> {
    let (library, method) = name.split_once('.')?;
    libraries.get(library)?.get(method).map(String::as_str)
}
