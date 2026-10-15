//! Feature membership and responsibilities are explicit, human/AI-authored
//! declarations. The host only resolves source locations and call evidence.
use crate::rust_index::{RustCall, RustSymbol, RustSymbolKind};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const MANIFEST: &str = "code-flow.json";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    version: u32,
    features: Vec<Feature>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Feature {
    id: String,
    #[serde(default)]
    parent: Option<String>,
    title: String,
    summary: String,
    #[serde(default)]
    nodes: Vec<Binding>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    id: String,
    label: String,
    file: String,
    name: String,
    #[serde(default)]
    module: String,
    #[serde(default)]
    impl_type: Option<String>,
    role: String,
    responsibility: String,
    inputs: String,
    outputs: String,
    #[serde(default)]
    tests: Vec<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    pub workspace_root: Option<String>,
    pub feature_id: Option<String>,
    pub node_id: Option<String>,
}

fn manifest(root: &Path) -> Result<Manifest> {
    let path = crate::file_edit::workspace_path(root, MANIFEST)?;
    let raw = std::fs::read(&path).context("This workspace has no code-flow.json. Define feature membership explicitly before viewing its flow.")?;
    ensure!(raw.len() <= 1024 * 1024, "code-flow.json exceeds 1 MiB");
    let data: Manifest = serde_json::from_slice(&raw)?;
    ensure!(data.version == 1, "unsupported code flow version");
    ensure!(data.features.len() <= 100, "too many features");
    let ids = data
        .features
        .iter()
        .map(|feature| feature.id.as_str())
        .collect::<BTreeSet<_>>();
    ensure!(ids.len() == data.features.len(), "duplicate feature IDs");
    for feature in &data.features {
        ensure!(
            !feature.id.is_empty() && !feature.title.is_empty(),
            "feature ID and title are required"
        );
        let mut chain = BTreeSet::from([feature.id.as_str()]);
        let mut parent = feature.parent.as_deref();
        while let Some(id) = parent {
            ensure!(chain.insert(id), "cyclic feature hierarchy");
            parent = data
                .features
                .iter()
                .find(|item| item.id == id)
                .context("unknown parent feature")?
                .parent
                .as_deref();
        }
        ensure!(
            feature.nodes.len() <= 100,
            "a feature may contain at most 100 nodes"
        );
        let mut nodes = BTreeSet::new();
        for node in &feature.nodes {
            ensure!(
                !node.id.is_empty() && nodes.insert(&node.id),
                "empty or duplicate node ID"
            );
            ensure!(
                matches!(node.role.as_str(), "entry" | "unit" | "shared"),
                "invalid declared node role"
            );
            crate::file_edit::workspace_path(root, &node.file)?;
        }
    }
    Ok(data)
}

fn file_module(file: &str) -> String {
    let relative = file.strip_prefix("src/").unwrap_or(file);
    let mut parts = relative
        .trim_end_matches(".rs")
        .split('/')
        .collect::<Vec<_>>();
    if matches!(parts.last(), Some(&"mod" | &"lib" | &"main")) {
        parts.pop();
    }
    format!(
        "crate{}",
        if parts.is_empty() {
            String::new()
        } else {
            format!("::{}", parts.join("::"))
        }
    )
}
fn module(symbol: &RustSymbol) -> String {
    let base = file_module(&symbol.file_path);
    if symbol.module_path.is_empty() {
        base
    } else {
        format!("{base}::{}", symbol.module_path)
    }
}
fn qualified(symbol: &RustSymbol) -> String {
    format!(
        "{}::{}{}",
        module(symbol),
        symbol
            .impl_type
            .as_ref()
            .map(|name| format!("{name}::"))
            .unwrap_or_default(),
        symbol.name
    )
}
fn absolute(path: &str, current: &str) -> String {
    if path == "self" {
        return current.to_owned();
    }
    if let Some(rest) = path.strip_prefix("self::") {
        return format!("{current}::{rest}");
    }
    if let Some(rest) = path.strip_prefix("super::") {
        return absolute(
            rest,
            current
                .rsplit_once("::")
                .map(|(prefix, _)| prefix)
                .unwrap_or("crate"),
        );
    }
    if path == "super" {
        return current
            .rsplit_once("::")
            .map(|(prefix, _)| prefix)
            .unwrap_or("crate")
            .to_owned();
    }
    path.to_owned()
}
fn use_paths(tree: &syn::UseTree, prefix: &str, result: &mut Vec<(String, String)>) {
    match tree {
        syn::UseTree::Path(path) => {
            use_paths(&path.tree, &format!("{prefix}{}::", path.ident), result)
        }
        syn::UseTree::Name(name) => {
            let target = format!("{prefix}{}", name.ident);
            let alias = if name.ident == "self" {
                prefix
                    .trim_end_matches("::")
                    .rsplit("::")
                    .next()
                    .unwrap_or("")
                    .to_owned()
            } else {
                name.ident.to_string()
            };
            result.push((alias, target.trim_end_matches("::self").to_owned()));
        }
        syn::UseTree::Rename(rename) => result.push((
            rename.rename.to_string(),
            format!("{prefix}{}", rename.ident),
        )),
        syn::UseTree::Group(group) => {
            for item in &group.items {
                use_paths(item, prefix, result);
            }
        }
        syn::UseTree::Glob(_) => {} // Wildcard/type resolution needs the compiler.
    }
}
#[derive(Default)]
struct Resolver {
    symbols: BTreeMap<String, Vec<usize>>,
    imports: BTreeMap<(String, String), BTreeMap<String, String>>,
    exports: BTreeMap<String, String>,
    modules: BTreeSet<String>,
}
impl Resolver {
    fn items(&mut self, file: &str, current: &str, items: &[syn::Item]) {
        for item in items {
            match item {
                syn::Item::Use(item) => {
                    let mut paths = Vec::new();
                    use_paths(&item.tree, "", &mut paths);
                    for (alias, path) in paths {
                        let first = path.split("::").next().unwrap_or("");
                        let target = if items.iter().any(
                            |item| matches!(item,syn::Item::Mod(module) if module.ident==first),
                        ) {
                            format!("{current}::{path}")
                        } else {
                            absolute(&path, current)
                        };
                        self.imports
                            .entry((file.to_owned(), current.to_owned()))
                            .or_default()
                            .insert(alias.clone(), target.clone());
                        if matches!(item.vis, syn::Visibility::Public(_)) {
                            self.exports.insert(format!("{current}::{alias}"), target);
                        }
                    }
                }
                syn::Item::Mod(item) => {
                    self.modules.insert(format!("{current}::{}", item.ident));
                    if let Some((_, items)) = &item.content {
                        self.items(file, &format!("{current}::{}", item.ident), items);
                    }
                }
                _ => {}
            }
        }
    }
    fn expand(&self, path: &str) -> String {
        let mut result = path.to_owned();
        let mut seen = BTreeSet::new();
        while seen.insert(result.clone()) {
            let replacement = self
                .exports
                .iter()
                .filter(|(prefix, _)| {
                    result == **prefix || result.starts_with(&format!("{prefix}::"))
                })
                .max_by_key(|(prefix, _)| prefix.len());
            if let Some((prefix, target)) = replacement {
                result = format!("{target}{}", &result[prefix.len()..]);
            } else {
                break;
            }
        }
        result
    }
    fn resolve(&self, caller: &RustSymbol, call: &RustCall) -> Vec<usize> {
        let current = module(caller);
        let imports = self
            .imports
            .get(&(caller.file_path.clone(), current.clone()));
        let path = match call.qualifier.as_deref() {
            None => imports
                .and_then(|items| items.get(&call.target_text))
                .cloned()
                .unwrap_or_else(|| format!("{current}::{}", call.target_text)),
            Some("self" | "Self" | "&self" | "& self") => match &caller.impl_type {
                Some(name) => format!("{current}::{name}::{}", call.target_text),
                None => return Vec::new(),
            },
            Some(qualifier) => {
                let (first, rest) = qualifier
                    .split_once("::")
                    .map(|(first, rest)| (first, format!("::{rest}")))
                    .unwrap_or((qualifier, String::new()));
                let prefix = if let Some(target) = imports.and_then(|items| items.get(first)) {
                    format!("{target}{rest}")
                } else if matches!(first, "crate" | "self" | "super") {
                    absolute(qualifier, &current)
                } else if self.modules.contains(&format!("{current}::{first}"))
                    || qualifier.chars().next().is_some_and(char::is_uppercase)
                {
                    format!("{current}::{qualifier}")
                } else {
                    return Vec::new();
                };
                format!("{prefix}::{}", call.target_text)
            }
        };
        self.symbols
            .get(&self.expand(&path))
            .cloned()
            .unwrap_or_default()
    }
}

/// One view for both AI tools and the human UI. No second graph assembler.
pub fn read(root: &Path, request: Request) -> Result<Value> {
    ensure!(
        request.node_id.is_none() || request.feature_id.is_some(),
        "node_id requires feature_id"
    );
    let root = root.canonicalize()?;
    let data = manifest(&root)?;
    let manifest_hash =
        crate::symbol_description::content_hash(&std::fs::read(root.join(MANIFEST))?);
    let catalog = data.features.iter().map(|feature|json!({"id":feature.id,"parent":feature.parent,"title":feature.title,"summary":feature.summary,"node_count":feature.nodes.len()})).collect::<Vec<_>>();
    let Some(id) = request.feature_id else {
        return Ok(
            json!({"workspace_root":root,"manifest":MANIFEST,"manifest_hash":manifest_hash,"features":catalog,"declaration_source":"human_or_ai_authored"}),
        );
    };
    let feature = data
        .features
        .iter()
        .find(|feature| feature.id == id)
        .context("unknown feature ID")?;
    let index = crate::rust_index::index_workspace(&root)?;
    let symbols = crate::rust_index::load_all_symbols(&root)?;
    let mut resolver = Resolver::default();
    let mut hashes = BTreeMap::new();
    for (position, symbol) in symbols.iter().enumerate() {
        resolver
            .symbols
            .entry(qualified(symbol))
            .or_default()
            .push(position);
    }
    for file in symbols
        .iter()
        .map(|symbol| symbol.file_path.as_str())
        .collect::<BTreeSet<_>>()
    {
        let path = crate::file_edit::workspace_path(&root, file)?;
        let source = std::fs::read_to_string(&path)?;
        let hash = crate::symbol_description::content_hash(source.as_bytes());
        ensure!(
            crate::source_read::indexed_hash(&root, "rust", file).as_deref() == Some(hash.as_str()),
            "source_version_conflict: refresh code flow after source changes"
        );
        resolver.items(file, &file_module(file), &syn::parse_file(&source)?.items);
        hashes.insert(file.to_owned(), hash);
    }
    let mut selected = BTreeMap::<usize, String>::new();
    let mut nodes = Vec::new();
    for binding in &feature.nodes {
        let candidates = symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| {
                symbol.file_path == binding.file
                    && symbol.name == binding.name
                    && symbol.module_path == binding.module
                    && symbol.impl_type == binding.impl_type
            })
            .collect::<Vec<_>>();
        let mut node = serde_json::to_value(binding)?;
        node["declaration_source"] = json!("human_or_ai_authored");
        if candidates.len() == 1 {
            let (position, symbol) = candidates[0];
            ensure!(
                selected.insert(position, binding.id.clone()).is_none(),
                "same symbol declared as multiple nodes in this feature"
            );
            node["status"] = json!("current");
            node["symbol_id"] = json!(symbol.id);
            node["source"] = json!({"file":symbol.file_path,"line":symbol.start_line,"end_line":symbol.end_line,"code_hash":hashes[&symbol.file_path]});
            node["signature"] = json!(symbol.signature);
            node["test_entries"] = json!(binding.tests.iter().map(|test| {
                let matching=symbols.iter().filter(|symbol|qualified(symbol).strip_prefix("crate::")==Some(test.as_str())).map(|symbol|json!({"symbol_id":symbol.id,"file":symbol.file_path,"line":symbol.start_line,"code_hash":hashes[&symbol.file_path]})).collect::<Vec<_>>();
                json!({"filter":test,"argv":["cargo","test",test,"--","--exact"],"source_matches":matching,"status":if matching.is_empty(){"missing"}else{"declared_not_run"}})
            }).collect::<Vec<_>>());
        } else {
            node["status"] = json!(if candidates.is_empty() {
                "missing"
            } else {
                "ambiguous"
            });
        }
        nodes.push(node);
    }
    let mut edges = Vec::new();
    let mut boundary_calls = Vec::new();
    let mut external_callers = Vec::new();
    for (position, caller) in symbols.iter().enumerate() {
        if !matches!(
            caller.kind,
            RustSymbolKind::Function | RustSymbolKind::Method
        ) {
            continue;
        }
        for call in &caller.calls {
            let targets = resolver.resolve(caller, call);
            let evidence = json!({"file":caller.file_path,"line":call.line,"snippet":call.snippet,"code_hash":hashes[&caller.file_path]});
            if let Some(source) = selected.get(&position) {
                if targets.len() == 1
                    && let Some(target) = selected.get(&targets[0])
                {
                    edges.push(json!({"id":format!("{source}:{target}:{}",call.line),"source":source,"target":target,"kind":"call","resolution":"syntax_not_type_checked","evidence":evidence}));
                } else {
                    boundary_calls.push(json!({"source":source,"target_text":call.target_text,"qualifier":call.qualifier,"resolution":if targets.len()==1{"outside_feature"}else if targets.is_empty(){"unresolved_or_external"}else{"ambiguous"},"candidates":targets.iter().map(|i|json!({"symbol_id":symbols[*i].id,"file":symbols[*i].file_path,"line":symbols[*i].start_line})).collect::<Vec<_>>(),"evidence":evidence}));
                }
            } else if targets.len() == 1
                && let Some(target) = selected.get(&targets[0])
            {
                external_callers.push(json!({"target":target,"symbol_id":caller.id,"name":caller.name,"file":caller.file_path,"line":call.line,"evidence":evidence}));
            }
        }
    }
    let mut result = json!({"workspace_root":root,"manifest":MANIFEST,"manifest_hash":manifest_hash,"features":catalog,"feature":{"id":feature.id,"title":feature.title,"summary":feature.summary,"parent":feature.parent},"nodes":nodes,"edges":edges,"boundary_calls":boundary_calls,"external_callers":external_callers,"index":index.index,
        "coverage":{"language":"rust","call_resolution":"syntax_not_type_checked","macros_and_dynamic_dispatch":"not_resolved","membership":"explicit_manifest","tests":"entry_points_only_not_execution_results"}});
    if let Some(node_id) = request.node_id {
        let node = result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == node_id)
            .context("unknown node ID")?
            .clone();
        let symbol_id = node["symbol_id"]
            .as_str()
            .context("node source is missing or ambiguous")?;
        let source = crate::rust_index::read_symbol(
            &root,
            serde_json::from_value(json!({"workspace_root":root,"symbol_id":symbol_id}))?,
        )?;
        ensure!(
            node["source"]["code_hash"] == source.description.code_hash,
            "source_version_conflict: source changed while opening the node; refresh the graph"
        );
        result["selected_node"] = node;
        result["source_content"] = json!(source.content);
        result["scope"] = json!("node_neighborhood");
        let incident = result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|edge| edge["source"] == node_id || edge["target"] == node_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut neighbors = BTreeSet::from([node_id.as_str()]);
        for edge in &incident {
            neighbors.insert(edge["source"].as_str().unwrap());
            neighbors.insert(edge["target"].as_str().unwrap());
        }
        result["nodes"] = json!(
            result["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|node| neighbors.contains(node["id"].as_str().unwrap()))
                .collect::<Vec<_>>()
        );
        result["edges"] = json!(incident);
        result["boundary_calls"] = json!(
            result["boundary_calls"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|call| call["source"] == node_id)
                .collect::<Vec<_>>()
        );
        result["external_callers"] = json!(
            result["external_callers"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|call| call["target"] == node_id)
                .collect::<Vec<_>>()
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn feature_graph_keeps_shared_nodes_and_unresolved_calls_honest() {
        let root =
            std::env::temp_dir().join(format!("code-flow-{}", crate::agent_service::uuid_like()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='graph-test'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"),"mod history; pub use history::shared; pub fn a(){crate::shared(); unknown.read();} pub fn b(){history::shared();}").unwrap();
        std::fs::write(root.join("src/history.rs"), "pub fn shared() {}\n").unwrap();
        let binding = |id: &str, file: &str, name: &str, role: &str| json!({"id":id,"file":file,"name":name,"label":id,"role":role,"responsibility":"declared","inputs":"none","outputs":"none"});
        let manifest = json!({"version":1,"features":[{"id":"history","title":"History","summary":"Explicit grouping","nodes":[binding("a","src/lib.rs","a","entry"),binding("b","src/lib.rs","b","entry"),binding("shared","src/history.rs","shared","shared")]}]});
        std::fs::write(root.join(MANIFEST), manifest.to_string()).unwrap();
        let graph = read(
            &root,
            Request {
                feature_id: Some("history".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(graph["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(graph["edges"].as_array().unwrap().len(), 2);
        assert!(
            graph["edges"]
                .as_array()
                .unwrap()
                .iter()
                .all(|edge| edge["target"] == "shared")
        );
        assert!(
            graph["boundary_calls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|call| call["target_text"] == "read"
                    && call["resolution"] == "unresolved_or_external")
        );
        let detail = read(
            &root,
            Request {
                feature_id: Some("history".into()),
                node_id: Some("shared".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(detail["source_content"], "pub fn shared() {}\n");
        std::fs::write(root.join("src/history.rs"), "pub fn renamed() {}\n").unwrap();
        let changed = read(
            &root,
            Request {
                feature_id: Some("history".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(changed["nodes"][2]["status"], "missing");
        assert!(changed["edges"].as_array().unwrap().is_empty());
        let mut invalid = manifest;
        invalid["features"][0]["parent"] = json!("history");
        std::fs::write(root.join(MANIFEST), invalid.to_string()).unwrap();
        assert!(read(&root, Request::default()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Tool schema belongs to this package alongside its implementation and page.
pub fn tool_definition() -> Value {
    serde_json::from_str(include_str!("tool.json")).expect("compiled code-flow tool schema")
}
