//! Model-facing declarations, generated from the same schemas used by tools.
use super::{
    libraries::{self, Libraries},
    *,
};
use std::collections::BTreeSet;

fn text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("*/", "* /")
}
fn key(value: &str) -> String {
    if libraries::identifier(value) {
        value.into()
    } else {
        json!(value).to_string()
    }
}
fn note(schema: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(description) = schema["description"].as_str() {
        parts.push(text(description));
    }
    if schema["type"] == "integer"
        || schema["type"]
            .as_array()
            .is_some_and(|types| types.iter().any(|ty| ty == "integer"))
    {
        parts.push("integer".into());
    }
    // Constraints that TS cannot express remain explicit annotations, never dropped.
    for (name, value) in schema.as_object().into_iter().flatten() {
        if !matches!(
            name.as_str(),
            "type"
                | "description"
                | "properties"
                | "required"
                | "items"
                | "prefixItems"
                | "additionalProperties"
                | "enum"
                | "const"
                | "anyOf"
                | "oneOf"
                | "allOf"
                | "$defs"
                | "definitions"
                | "$ref"
        ) {
            parts.push(text(&format!("{name}={value}")));
        }
    }
    if schema["additionalProperties"] == false {
        parts.push("no extra properties".into());
    }
    if schema.get("oneOf").is_some() {
        parts.push("exactly one schema must match".into());
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("/** {} */ ", parts.join("; "))
    }
}
fn ty(schema: &Value, root: &Value, seen: &mut BTreeSet<String>, depth: usize) -> String {
    if depth > 32 {
        return format!("{}unknown", note(&json!({"schema":schema})));
    }
    if schema == &Value::Bool(false) {
        return "never".into();
    }
    if schema == &Value::Bool(true) || schema.is_null() {
        return "unknown".into();
    }
    if let Some(reference) = schema["$ref"].as_str() {
        let value = reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer));
        if !seen.insert(reference.into()) || value.is_none() {
            return format!("/** $ref={} */ unknown", text(reference));
        }
        let resolved = ty(value.unwrap(), root, seen, depth + 1);
        seen.remove(reference);
        let mut siblings = schema.clone();
        siblings.as_object_mut().unwrap().remove("$ref");
        return if siblings.as_object().unwrap().is_empty() {
            resolved
        } else {
            format!("({resolved}) & ({})", ty(&siblings, root, seen, depth + 1))
        };
    }
    let mut shapes = Vec::new();
    if let Some(value) = schema.get("const") {
        shapes.push(value.to_string());
    }
    if let Some(values) = schema["enum"].as_array() {
        shapes.push(if values.is_empty() {
            "never".into()
        } else {
            values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(" | ")
        });
    }
    for (name, separator) in [("anyOf", " | "), ("oneOf", " | "), ("allOf", " & ")] {
        if let Some(items) = schema[name].as_array() {
            if items.is_empty() {
                shapes.push(if name == "allOf" {
                    "unknown".into()
                } else {
                    "never".into()
                });
                continue;
            }
            shapes.push(format!(
                "({})",
                items
                    .iter()
                    .map(|v| format!("({})", ty(v, root, seen, depth + 1)))
                    .collect::<Vec<_>>()
                    .join(separator)
            ));
        }
    }
    let type_names = schema["type"]
        .as_array()
        .map(|v| v.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_else(|| schema["type"].as_str().into_iter().collect());
    if !type_names.is_empty() || schema.get("properties").is_some() {
        let names = if type_names.is_empty() {
            vec!["object"]
        } else {
            type_names
        };
        let types = names
            .into_iter()
            .map(|name| match name {
                "null" => "null".into(),
                "boolean" => "boolean".into(),
                "number" | "integer" => "number".into(),
                "string" => "string".into(),
                "array" => {
                    if let Some(prefix) = schema["prefixItems"]
                        .as_array()
                        .or_else(|| schema["items"].as_array())
                    {
                        let mut types = prefix
                            .iter()
                            .map(|v| ty(v, root, seen, depth + 1))
                            .collect::<Vec<_>>();
                        let rest = if schema.get("prefixItems").is_some() {
                            &schema["items"]
                        } else {
                            &schema["additionalItems"]
                        };
                        if rest != &Value::Bool(false) {
                            types.push(format!("...({})[]", ty(rest, root, seen, depth + 1)));
                        }
                        // JSON Schema permits shorter tuples unless minItems requires them.
                        let minimum = schema["minItems"].as_u64().unwrap_or(0) as usize;
                        for (index, item) in types.iter_mut().take(prefix.len()).enumerate() {
                            if index >= minimum {
                                *item = format!("({item})?");
                            }
                        }
                        format!("[{}]", types.join(", "))
                    } else {
                        format!("({})[]", ty(&schema["items"], root, seen, depth + 1))
                    }
                }
                "object" => {
                    let mut fields = schema["properties"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(name, value)| {
                            let required = schema["required"]
                                .as_array()
                                .is_some_and(|list| list.iter().any(|v| v == name));
                            format!(
                                "{}{}: {}",
                                key(name),
                                if required { "" } else { "?" },
                                ty(value, root, seen, depth + 1)
                            )
                        })
                        .collect::<Vec<_>>();
                    match schema.get("additionalProperties") {
                        Some(Value::Bool(false)) => {}
                        Some(Value::Object(_)) => {
                            // Intersection keeps the additional-key constraint without
                            // incorrectly imposing it on named properties in TypeScript.
                            let extra = ty(&schema["additionalProperties"], root, seen, depth + 1);
                            fields.push(format!(
                                "/** additional keys: {} */ [key: string]: unknown",
                                text(&extra)
                            ));
                        }
                        _ => fields.push("[key: string]: unknown".into()),
                    }
                    if let Some(required) = schema["required"].as_array() {
                        for name in required.iter().filter_map(Value::as_str) {
                            if schema["properties"].get(name).is_none() {
                                fields.push(format!("{}: unknown", key(name)));
                            }
                        }
                    }
                    format!("{{ {} }}", fields.join("; "))
                }
                _ => "unknown".into(),
            })
            .collect::<Vec<String>>();
        shapes.push(types.join(" | "));
    }
    let shape = if shapes.is_empty() {
        "unknown".into()
    } else {
        shapes
            .into_iter()
            .map(|s| format!("({s})"))
            .collect::<Vec<_>>()
            .join(" & ")
    };
    format!("{}{shape}", note(schema))
}
pub fn schema_type(schema: &Value) -> String {
    ty(schema, schema, &mut BTreeSet::new(), 0)
}
pub fn headers(libraries: &Libraries, definitions: &[Value]) -> String {
    let definitions = definitions
        .iter()
        .filter_map(|d| {
            d["function"]["name"]
                .as_str()
                .map(|name| (name, &d["function"]))
        })
        .collect::<BTreeMap<_, _>>();
    let mut result = String::from(
        "// Declarations only: call these fixed names in plain JS flow source; do not copy TS types into source.\n",
    );
    for (library, methods) in libraries {
        let object = methods.keys().any(|name| libraries::reserved(name));
        result.push_str(&if object {
            format!("declare const {library}: {{\n")
        } else {
            format!("declare namespace {library} {{\n")
        });
        for (method, target) in methods {
            let Some(definition) = definitions.get(target.as_str()) else {
                continue;
            };
            // Planning and native execution must receive the complete same
            // tool contract, including requirements after its first sentence.
            let description = text(definition["description"].as_str().unwrap_or(""));
            let returns = definition
                .get("outputSchema")
                .filter(|v| !v.is_null())
                .map(schema_type)
                .unwrap_or_else(|| "unknown".into());
            let prefix = if object { "" } else { "function " };
            result.push_str(&format!(
                "  /** {description} */\n  {prefix}{method}(args: {}): {returns};\n",
                schema_type(&definition["parameters"])
            ));
        }
        result.push_str(if object { "};\n" } else { "}\n" });
    }
    result
}

/// Compact, discoverable names; signatures are fetched only when needed.
pub fn index(libraries: &Libraries, definitions: &[Value]) -> String {
    let descriptions = definitions
        .iter()
        .filter_map(|d| {
            Some((
                d["function"]["name"].as_str()?,
                d["function"]["description"].as_str().unwrap_or(""),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut result = String::from(
        "// Tool index only, not signatures. Use read_tool_contracts for exact inputs and returns.\n",
    );
    for (library, methods) in libraries {
        let object = methods.keys().any(|name| libraries::reserved(name));
        result.push_str(&if object {
            format!("declare const {library}: {{\n")
        } else {
            format!("declare namespace {library} {{\n")
        });
        for (method, target) in methods {
            let description = descriptions.get(target.as_str()).copied().unwrap_or("");
            let summary = description.split(['.', '。', '\n']).next().unwrap_or("");
            let summary = text(&summary.chars().take(140).collect::<String>());
            if !summary.is_empty() {
                result.push_str(&format!("  /** {summary} */\n"));
            }
            let prefix = if object { "" } else { "function " };
            result.push_str(&format!(
                "  {prefix}{method}(...args: unknown[]): unknown;\n"
            ));
        }
        result.push_str(if object { "};\n" } else { "}\n" });
    }
    result
}

pub fn selected(libraries: &Libraries, args: &Value) -> anyhow::Result<Libraries> {
    use anyhow::{Context, ensure};
    let mut result = Libraries::new();
    ensure!(
        args.as_object()
            .is_some_and(|o| o.keys().all(|k| k == "libraries" || k == "methods")),
        "expected libraries and/or methods"
    );
    ensure!(
        args.get("libraries").is_none_or(Value::is_array)
            && args.get("methods").is_none_or(Value::is_array),
        "libraries and methods must be arrays"
    );
    let groups = args.get("libraries").and_then(Value::as_array);
    let methods = args.get("methods").and_then(Value::as_array);
    ensure!(
        groups.is_some_and(|x| !x.is_empty()) || methods.is_some_and(|x| !x.is_empty()),
        "select at least one library or qualified method"
    );
    for group in groups.into_iter().flatten() {
        let name = group.as_str().context("library name must be a string")?;
        let items = libraries
            .get(name)
            .with_context(|| format!("unknown or unavailable library {name}"))?;
        result
            .entry(name.into())
            .or_insert_with(BTreeMap::new)
            .extend(items.clone());
    }
    for method in methods.into_iter().flatten() {
        let name = method.as_str().context("method name must be a string")?;
        let (group, item) = name
            .split_once('.')
            .context("use a qualified method such as io.read")?;
        let target = libraries::target(libraries, name)
            .with_context(|| format!("unknown or unavailable method {name}"))?;
        result
            .entry(group.into())
            .or_insert_with(BTreeMap::new)
            .insert(item.into(), target.into());
    }
    Ok(result)
}

pub fn contract_tool() -> Value {
    json!({"type":"function","function":{"name":"read_tool_contracts","description":"Read exact TypeScript-like tool declarations for selected available libraries or qualified methods before coding or invoking them. Does not execute tools or expand permissions.","parameters":{"type":"object","properties":{"libraries":{"type":"array","items":{"type":"string"}},"methods":{"type":"array","items":{"type":"string"}}},"additionalProperties":false}}})
}

/// Provider-safe transport name. The only method names in this schema are the
/// same qualified names used in source; original dispatcher names stay private.
pub const CALL: &str = "invoke_library";
pub fn native_tool(libraries: &Libraries, _definitions: &[Value]) -> Option<Value> {
    if libraries.is_empty() {
        return None;
    }
    Some(
        json!({"type":"function","function":{"name":CALL,"description":"Execute an available qualified library method in the current virtual task. Read its contract with read_tool_contracts when needed. The host validates arguments and permissions before dispatch.","parameters":{"type":"object","properties":{"method":{"type":"string"},"args":{"type":"object"}},"required":["method","args"],"additionalProperties":false}}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(source: &str) {
        use swc_common::{FileName, SourceMap, sync::Lrc};
        use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
        let map: Lrc<SourceMap> = Default::default();
        let file = map.new_source_file(
            FileName::Custom("tools.d.ts".into()).into(),
            source.to_owned(),
        );
        let lexer = Lexer::new(
            Syntax::Typescript(TsSyntax {
                dts: true,
                ..Default::default()
            }),
            Default::default(),
            StringInput::from(&*file),
            None,
        );
        let mut parser = Parser::new_from(lexer);
        parser
            .parse_typescript_module()
            .unwrap_or_else(|e| panic!("invalid declaration {e:?}: {source}"));
        assert!(parser.take_errors().is_empty(), "{source}");
    }
    #[test]
    fn signatures_preserve_json_constraints_refs_and_actual_returns() {
        let definition = json!({"function":{"name":"read_file","description":"Read a file */ safely.\nReturn actual data.",
            "parameters":{"type":"object","required":["path"],"additionalProperties":false,"$defs":{"Mode":{"enum":["text","binary"]}},"properties":{
                "path":{"type":"string","minLength":1,"pattern":"^[a-z]"},"limit":{"type":"integer","minimum":1,"maximum":50},
                "mode":{"$ref":"#/$defs/Mode"},"value":{"anyOf":[{"type":"null"},{"type":"boolean"}]}}},
            "outputSchema":{"type":"object","required":["content"],"properties":{"content":{"type":"string"}}}}});
        let libs = libraries::catalog(&[definition.clone()], &json!([]));
        let declarations = headers(&libs, &[definition]);
        for expected in [
            "declare namespace io",
            "function read(args:",
            "path:",
            "limit?:",
            "integer",
            "minimum=1",
            "maximum=50",
            "minLength=1",
            "pattern=",
            "no extra properties",
            "\"text\" | \"binary\"",
            "content:",
            "* /",
            "Return actual data.",
        ] {
            assert!(
                declarations.contains(expected),
                "{expected}: {declarations}"
            );
        }
        assert!(!declarations.contains("read_file"));
        parse(&declarations);
    }
    #[test]
    fn missing_return_contract_is_unknown_and_headers_are_stable() {
        let defs = [
            json!({"function":{"name":"read_file","parameters":{"type":"object","properties":{"odd-key":{"type":["string","null"]}}}}}),
        ];
        let libs = libraries::catalog(&defs, &json!([]));
        let declarations = headers(&libs, &defs);
        assert!(declarations.contains("): unknown;"));
        assert!(declarations.contains("\"odd-key\"?:"));
        assert_eq!(declarations, headers(&libs, &defs));
        parse(&declarations);
    }
    #[test]
    fn recursive_unresolved_and_composite_schemas_remain_explicit() {
        let schema = json!({"type":"object","properties":{"next":{"$ref":"#"},"external":{"$ref":"https://example.invalid/schema"},"tuple":{"type":"array","prefixItems":[{"type":"integer"},{"type":"boolean"}],"minItems":1,"items":false},"dictionary":{"type":"object","additionalProperties":{"type":["integer","null"],"minimum":0}},"union":{"oneOf":[{"const":"x"},{"type":"number"}]}}});
        let value = schema_type(&schema);
        assert!(value.contains("$ref=#"));
        assert!(value.contains("$ref=https://example.invalid/schema"));
        assert!(value.contains("exactly one schema must match"));
        assert!(value.contains("additional keys"));
        parse(&format!(
            "declare namespace sample {{ function run(args:{value}):unknown; }}"
        ));
    }
    #[test]
    fn compact_index_and_selected_contracts_expose_only_available_tools() {
        let defs = [
            json!({"function":{"name":"read_file","description":"Read a file","parameters":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}}}),
            json!({"function":{"name":"write_file","description":"Write a file","parameters":{"type":"object","required":["content"],"properties":{"content":{"type":"string"}}}}}),
        ];
        let libs = libraries::catalog(&defs, &json!([]));
        let directory = index(&libs, &defs);
        parse(&directory);
        assert!(directory.contains("function read("));
        assert!(directory.contains("function write("));
        assert!(!directory.contains("path:"));
        let chosen = selected(&libs, &json!({"methods":["io.read"]})).unwrap();
        let signature = headers(&chosen, &defs);
        assert!(signature.contains("path:"));
        assert!(!signature.contains("function write("));
        assert!(selected(&libs, &json!({"methods":["io.missing"]})).is_err());
        assert!(selected(&libs, &json!({"libraries":["missing"]})).is_err());
        assert!(selected(&libs, &json!({})).is_err());
        let native = native_tool(&libs, &defs).unwrap();
        assert_eq!(native["function"]["name"], CALL);
        assert!(!native.to_string().contains("write_file"));
        assert!(!native.to_string().contains("oneOf"));
    }
    #[test]
    fn original_contract_validator_preserves_local_references_and_constraints() {
        let schema = json!({"type":"object","$defs":{"Text":{"type":"string","minLength":1}},"properties":{"path":{"$ref":"#/$defs/Text"}},"required":["path"],"additionalProperties":false});
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&json!({"path":"ok"})));
        assert!(!validator.is_valid(&json!({"path":""})));
        assert!(!validator.is_valid(&json!({"path":"ok","extra":1})));
        assert!(!validator.is_valid(&json!({})));
    }
    #[test]
    fn all_available_host_headers_are_valid_declarations_including_keyword_methods() {
        let root = crate::tools::Workspace::new(std::env::current_dir().unwrap()).unwrap();
        // Use the actual host definitions, not a hand-written signature sample.
        let definitions = crate::agent_service::local_tools(&root, None, false);
        let mut libs = libraries::catalog(&definitions, &json!([]));
        let mut definitions = definitions;
        definitions
            .push(json!({"function":{"name":"plugin__delete","parameters":{"type":"object"}}}));
        libs.insert(
            "plugin".into(),
            BTreeMap::from([("delete".into(), "plugin__delete".into())]),
        );
        let declarations = headers(&libs, &definitions);
        let compact = index(&libs, &definitions);
        let native = native_tool(&libs, &definitions).unwrap().to_string();
        eprintln!(
            "host catalog sizes: full_declarations={} compact_index={} native_wrapper={} bytes",
            declarations.len(),
            compact.len(),
            native.len()
        );
        assert!(compact.len() < declarations.len());
        parse(&compact);
        parse(&declarations);
    }
}
