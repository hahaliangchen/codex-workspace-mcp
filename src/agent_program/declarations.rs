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
            let full = text(definition["description"].as_str().unwrap_or(""));
            let end = full
                .find(". ")
                .map(|index| index + 1)
                .or_else(|| full.find('。').map(|index| index + '。'.len_utf8()))
                .unwrap_or(full.len());
            let description = &full[..end];
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

/// Provider-safe transport name. The only method names in this schema are the
/// same qualified names used in source; original dispatcher names stay private.
pub const CALL: &str = "invoke_library";
fn rebase_refs(schema: &mut Value, prefix: &str) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    // An explicit resource ID establishes its own reference scope.
    if object.contains_key("$id") {
        return;
    }
    if let Some(Value::String(reference)) = object.get_mut("$ref") {
        if reference == "#" || reference.starts_with("#/") {
            *reference = format!("{prefix}{}", &reference[1..]);
        }
    }
    for name in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(Value::Object(map)) = object.get_mut(name) {
            for value in map.values_mut() {
                rebase_refs(value, prefix);
            }
        }
    }
    for name in [
        "items",
        "additionalItems",
        "additionalProperties",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
    ] {
        if let Some(value) = object.get_mut(name) {
            if let Some(items) = value.as_array_mut() {
                for item in items {
                    rebase_refs(item, prefix);
                }
            } else {
                rebase_refs(value, prefix);
            }
        }
    }
    for name in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(Value::Array(items)) = object.get_mut(name) {
            for item in items {
                rebase_refs(item, prefix);
            }
        }
    }
}
pub fn native_tool(libraries: &Libraries, definitions: &[Value]) -> Option<Value> {
    let options = definitions.iter().filter_map(|definition| {
        let function=&definition["function"];
        let name=libraries::public_name(libraries,function["name"].as_str()?)?;
        Some(json!({"type":"object","description":function["description"],"properties":{"method":{"type":"string","const":name},"args":function["parameters"]},"required":["method","args"],"additionalProperties":false}))
    }).enumerate().map(|(index,mut option)| {
        rebase_refs(&mut option["properties"]["args"],&format!("#/oneOf/{index}/properties/args"));
        option
    }).collect::<Vec<_>>();
    (!options.is_empty()).then(||json!({"type":"function","function":{"name":CALL,"description":"Execute a declared library method as a native tool step. Use the exact qualified method name in program_tool_headers and its JSON arguments. This transport wrapper does not rename methods or expand permissions.","parameters":{"type":"object","oneOf":options}}}))
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
    fn native_transport_exposes_only_the_same_qualified_names_and_schemas() {
        let defs = [
            json!({"function":{"name":"write_file","parameters":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}}}),
        ];
        let libs = libraries::catalog(&defs, &json!([]));
        let native = native_tool(&libs, &defs).unwrap();
        assert_eq!(native["function"]["name"], CALL);
        assert_eq!(
            native["function"]["parameters"]["oneOf"][0]["properties"]["method"]["const"],
            "io.write"
        );
        assert_eq!(
            native["function"]["parameters"]["oneOf"][0]["properties"]["args"],
            defs[0]["function"]["parameters"]
        );
        assert!(!native.to_string().contains("write_file"));
    }
    #[test]
    fn native_wrapper_preserves_local_reference_scope_and_literal_data() {
        let schema = json!({"type":"object","$defs":{"Text":{"type":"string"}},"properties":{"path":{"$ref":"#/$defs/Text"},"data":{"const":{"$ref":"literal data"}}}});
        let defs = [json!({"function":{"name":"read_file","parameters":schema}})];
        let libs = libraries::catalog(&defs, &json!([]));
        let parameters = &native_tool(&libs, &defs).unwrap()["function"]["parameters"];
        let args = &parameters["oneOf"][0]["properties"]["args"];
        let reference = args["properties"]["path"]["$ref"].as_str().unwrap();
        assert_eq!(
            parameters
                .pointer(reference.strip_prefix('#').unwrap())
                .unwrap(),
            &json!({"type":"string"})
        );
        assert_eq!(args["properties"]["data"]["const"]["$ref"], "literal data");
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
        eprintln!(
            "host catalog sizes: declarations={} bytes, native schemas={} bytes",
            declarations.len(),
            serde_json::to_string(&definitions).unwrap().len()
        );
        parse(&declarations);
    }
}
