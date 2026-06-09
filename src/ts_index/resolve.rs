use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use crate::ts_index::*;

pub(crate) fn build_context(
    index_symbols: &[TsSymbol],
    symbol: &TsSymbol,
) -> (
    Vec<TsCallee>,
    Vec<TsCaller>,
    Vec<TsResolvedImport>,
    Vec<TsSuggestedRead>,
) {
    let mut name_to_ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut id_to_symbol: BTreeMap<String, &TsSymbol> = BTreeMap::new();
    let file_export_to_ids = build_file_export_map(index_symbols);
    for item in index_symbols {
        name_to_ids
            .entry(item.name.clone())
            .or_default()
            .push(item.id.clone());
        id_to_symbol.insert(item.id.clone(), item);
    }

    let resolved_imports: Vec<_> = symbol
        .import_bindings
        .iter()
        .map(|import| {
            let target_file_path = resolve_import_source(&symbol.file_path, &import.source, index_symbols);
            let re_export_chain = target_file_path
                .as_ref()
                .map(|target_file| {
                    build_re_export_chain(
                        index_symbols,
                        target_file,
                        &import.imported_name,
                        &file_export_to_ids,
                    )
                })
                .unwrap_or_default();
            let matched_symbol_ids = target_file_path
                .as_ref()
                .map(|target_file| match import.kind {
                    TsImportKind::Namespace | TsImportKind::SideEffect => {
                        exported_ids_for_file(target_file, &file_export_to_ids)
                    }
                    TsImportKind::Default | TsImportKind::Named => file_export_to_ids
                        .get(&(target_file.clone(), import.imported_name.clone()))
                        .cloned()
                        .unwrap_or_default(),
                })
                .unwrap_or_default();
            TsResolvedImport {
                source: import.source.clone(),
                local_name: import.local_name.clone(),
                imported_name: import.imported_name.clone(),
                kind: import.kind.clone(),
                target_file_path,
                matched_symbol_ids,
                re_export_chain,
            }
        })
        .collect();

    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| TsCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: resolve_call_targets(
                call,
                &name_to_ids,
                &resolved_imports,
                &file_export_to_ids,
            ),
        })
        .collect();
    let mut suggested_reads = Vec::new();
    let mut seen = BTreeSet::new();
    for callee in &callees {
        for matched_id in &callee.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched_symbol) = id_to_symbol.get(matched_id) {
                let reason = if resolved_imports.iter().any(|import| {
                    import.local_name == callee.target_text
                        || callee.snippet.contains(&format!("{}.", import.local_name))
                }) {
                    "resolved_import"
                } else {
                    "direct_callee"
                };
                suggested_reads.push(TsSuggestedRead {
                    reason: reason.to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: TsSymbolSummary::from(*matched_symbol),
                });
            }
        }
    }
    for import in &resolved_imports {
        for matched_id in &import.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched_symbol) = id_to_symbol.get(matched_id) {
                suggested_reads.push(TsSuggestedRead {
                    reason: "resolved_import".to_string(),
                    trigger_call: import.local_name.clone(),
                    trigger_line: symbol.start_line,
                    trigger_snippet: format!("import {} from {}", import.local_name, import.source),
                    symbol: TsSymbolSummary::from(*matched_symbol),
                });
            }
        }
    }
    let mut callers = Vec::new();
    for item in index_symbols {
        if item.id == symbol.id {
            continue;
        }
        for call in &item.calls {
            if call.target_text == symbol.name || imported_call_matches(index_symbols, item, call, symbol) {
                callers.push(TsCaller {
                    symbol_id: item.id.clone(),
                    name: item.name.clone(),
                    file_path: item.file_path.clone(),
                    line: call.line,
                    snippet: call.snippet.clone(),
                });
            }
        }
    }
    (callees, callers, resolved_imports, suggested_reads)
}

pub(crate) fn resolve_call_targets(
    call: &TsCall,
    name_to_ids: &BTreeMap<String, Vec<String>>,
    resolved_imports: &[TsResolvedImport],
    file_export_to_ids: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<String> {
    if let Some(namespace) = call.namespace.as_deref()
        && let Some(import) = resolved_imports
            .iter()
            .find(|import| import.local_name == namespace && import.kind == TsImportKind::Namespace)
        && let Some(target_file) = import.target_file_path.as_ref()
        && let Some(ids) = file_export_to_ids.get(&(target_file.clone(), call.target_text.clone()))
    {
        return ids.clone();
    }

    if let Some(import) = resolved_imports
        .iter()
        .find(|import| import.local_name == call.target_text)
    {
        if !import.matched_symbol_ids.is_empty() {
            return import.matched_symbol_ids.clone();
        }
    }
    name_to_ids
        .get(&call.target_text)
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn imported_call_matches(
    index_symbols: &[TsSymbol],
    caller: &TsSymbol,
    call: &TsCall,
    target: &TsSymbol,
) -> bool {
    let file_export_to_ids = build_file_export_map(index_symbols);
    caller.import_bindings.iter().any(|import| {
        if call.namespace.as_deref() == Some(import.local_name.as_str())
            && import.kind == TsImportKind::Namespace
            && let Some(target_file) =
                resolve_import_source(&caller.file_path, &import.source, index_symbols)
            && let Some(ids) = file_export_to_ids.get(&(target_file, call.target_text.clone()))
        {
            return ids.iter().any(|id| id == &target.id);
        }

        import.local_name == call.target_text
            && resolve_import_source(&caller.file_path, &import.source, index_symbols).as_deref()
                == Some(target.file_path.as_str())
            && exported_names(target)
                .iter()
                .any(|export_name| export_name == &import.imported_name)
    })
}

pub(crate) fn build_file_export_map(index_symbols: &[TsSymbol]) -> BTreeMap<(String, String), Vec<String>> {
    let mut file_export_to_ids: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for item in index_symbols {
        if item.export {
            for export_name in exported_names(item) {
                file_export_to_ids
                    .entry((item.file_path.clone(), export_name))
                    .or_default()
                    .push(item.id.clone());
            }
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for re_export in index_symbols.iter().flat_map(|s| &s.re_exports) {
            let Some(source_file) =
                resolve_import_source(&re_export.file_path, &re_export.source, index_symbols)
            else {
                continue;
            };
            let source_exports =
                if re_export.kind == TsImportKind::Namespace && re_export.exported_name == "*" {
                    exported_names_for_file(&source_file, &file_export_to_ids)
                } else {
                    vec![re_export.local_name.clone()]
                };
            for source_export in source_exports {
                let exported_name = if re_export.kind == TsImportKind::Namespace {
                    source_export.clone()
                } else {
                    re_export.exported_name.clone()
                };
                let Some(ids) = file_export_to_ids
                    .get(&(source_file.clone(), source_export))
                    .cloned()
                else {
                    continue;
                };
                let entry = file_export_to_ids
                    .entry((re_export.file_path.clone(), exported_name))
                    .or_default();
                for id in ids {
                    if !entry.contains(&id) {
                        entry.push(id);
                        changed = true;
                    }
                }
            }
        }
    }
    file_export_to_ids
}

pub(crate) fn build_re_export_chain(
    index_symbols: &[TsSymbol],
    file_path: &str,
    imported_name: &str,
    file_export_to_ids: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<TsExportChainStep> {
    let mut chain = Vec::new();
    let mut current_file = file_path.to_string();
    let mut current_name = imported_name.to_string();
    let mut seen = BTreeSet::new();

    loop {
        if !seen.insert((current_file.clone(), current_name.clone())) {
            break;
        }
        let Some(re_export) = find_re_export_for_name(index_symbols, &current_file, &current_name) else {
            break;
        };
        let Some(target_file) =
            resolve_import_source(&re_export.file_path, &re_export.source, index_symbols)
        else {
            break;
        };
        let next_name = if re_export.kind == TsImportKind::Namespace {
            current_name.clone()
        } else {
            re_export.local_name.clone()
        };
        if !file_export_to_ids.contains_key(&(target_file.clone(), next_name.clone())) {
            break;
        }
        chain.push(TsExportChainStep {
            file_path: re_export.file_path.clone(),
            source: re_export.source.clone(),
            imported_name: next_name.clone(),
            local_name: re_export.exported_name.clone(),
            kind: re_export.kind.clone(),
            target_file_path: Some(target_file.clone()),
        });
        current_file = target_file;
        current_name = next_name;
    }

    chain
}

pub(crate) fn find_re_export_for_name<'a>(
    index_symbols: &'a [TsSymbol],
    file_path: &str,
    export_name: &str,
) -> Option<&'a TsReExport> {
    index_symbols.iter().flat_map(|s| &s.re_exports).find(|re_export| {
        re_export.file_path == file_path
            && (re_export.exported_name == export_name
                || (re_export.kind == TsImportKind::Namespace && re_export.exported_name == "*"))
    })
}

pub(crate) fn exported_names_for_file(
    file_path: &str,
    file_export_to_ids: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<String> {
    let mut names: Vec<_> = file_export_to_ids
        .keys()
        .filter_map(|(file, name)| (file == file_path).then_some(name.clone()))
        .collect();
    names.sort();
    names.dedup();
    names
}

pub(crate) fn exported_ids_for_file(
    file_path: &str,
    file_export_to_ids: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<String> {
    let mut ids = Vec::new();
    for (file, _name) in file_export_to_ids.keys() {
        if file != file_path {
            continue;
        }
        if let Some(values) = file_export_to_ids.get(&(file.clone(), _name.clone())) {
            for value in values {
                if !ids.contains(value) {
                    ids.push(value.clone());
                }
            }
        }
    }
    ids
}

pub(crate) fn resolve_import_source(from_file: &str, source: &str, index_symbols: &[TsSymbol]) -> Option<String> {
    if !source.starts_with('.') {
        return None;
    }
    let from_dir = Path::new(from_file)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let candidate = normalize_workspace_relative_path(&from_dir.join(source).to_string_lossy());
    let candidates = import_path_candidates(&candidate);
    candidates.into_iter().find(|import_source| {
        index_symbols
            .iter()
            .find(|symbol| symbol.file_path == *import_source)
            .is_some()
            || index_symbols
                .iter()
                .flat_map(|s| &s.re_exports)
                .any(|re_export| re_export.file_path == *import_source)
    })
}

pub(crate) fn import_path_candidates(base: &str) -> Vec<String> {
    let base = normalize_workspace_relative_path(base);
    let path = Path::new(&base);
    if path.extension().is_some() {
        return vec![base];
    }
    ["ts", "tsx", "js", "jsx"]
        .into_iter()
        .map(|ext| format!("{base}.{ext}"))
        .chain(
            ["ts", "tsx", "js", "jsx"]
                .into_iter()
                .map(|ext| format!("{base}/index.{ext}")),
        )
        .collect()
}

