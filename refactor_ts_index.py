import re

with open('src/ts_index.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# Add Sqlite error
content = content.replace('    Json(#[from] serde_json::Error),\n}', '    Json(#[from] serde_json::Error),\n    #[error("sqlite error: {0}")]\n    Sqlite(#[from] rusqlite::Error),\n}')

load_symbols_code = """
fn load_all_symbols(root: &std::path::Path) -> Result<Vec<TsSymbol>> {
    let conn = crate::database::init_db(root).map_err(|e| TsIndexError::SymbolNotFound(e.to_string()))?;
    let mut stmt = conn.prepare("SELECT id, name, kind, file_path, scope_path, parent_id, start_line, end_line, signature, docstring, export, export_names_json, calls_json, import_bindings_json, imports_json FROM ts_symbols WHERE workspace_root = ?").map_err(|e| TsIndexError::SymbolNotFound(e.to_string()))?;
    let symbol_iter = stmt.query_map(rusqlite::params![root.to_string_lossy()], |row| {
        Ok(TsSymbol {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: serde_json::from_str(&format!("\\"{}\\"", row.get::<_, String>(2)?)).unwrap_or(TsSymbolKind::Function),
            file_path: row.get(3)?,
            scope_path: row.get(4)?,
            parent_id: row.get(5)?,
            start_line: row.get(6)?,
            end_line: row.get(7)?,
            signature: row.get(8)?,
            docstring: row.get(9)?,
            export: row.get::<_, i64>(10)? != 0,
            export_names: serde_json::from_str(&row.get::<_, String>(11)?).unwrap_or_default(),
            calls: serde_json::from_str(&row.get::<_, String>(12)?).unwrap_or_default(),
            import_bindings: serde_json::from_str(&row.get::<_, String>(13)?).unwrap_or_default(),
            imports: serde_json::from_str(&row.get::<_, String>(14)?).unwrap_or_default(),
        })
    }).map_err(|e| TsIndexError::SymbolNotFound(e.to_string()))?;

    let mut symbols = Vec::new();
    for sym in symbol_iter {
        if let Ok(s) = sym {
            symbols.push(s);
        }
    }
    Ok(symbols)
}

fn load_or_build_or_create(root: &std::path::Path) -> Result<Vec<TsSymbol>> {
    let mut symbols = load_all_symbols(root)?;
    if symbols.is_empty() {
        index_workspace(root)?;
        symbols = load_all_symbols(root)?;
    }
    Ok(symbols)
}

fn load_or_build(root: &std::path::Path) -> Result<Vec<TsSymbol>> {
    let symbols = load_all_symbols(root)?;
    if symbols.is_empty() {
        return Err(TsIndexError::MissingIndex);
    }
    Ok(symbols)
}
"""

content = re.sub(r'fn load_or_build\(root: &Path\) -> Result<TsIndex> \{.*?\n\}\n\nfn load_or_build_or_create\(root: &Path\) -> Result<TsIndex> \{.*?\n\}', load_symbols_code.strip(), content, flags=re.DOTALL)

content = content.replace('let index = load_or_build_or_create(root)?;\n    let symbols = index\n        .symbols\n        .iter()', 'let index_symbols = load_or_build_or_create(root)?;\n    let symbols = index_symbols\n        .iter()')
content = content.replace('let mut matches: Vec<_> = index\n        .symbols\n        .iter()', 'let index_symbols = load_or_build_or_create(root)?;\n    let mut matches: Vec<_> = index_symbols\n        .iter()')

content = content.replace('let index = load_or_build(root)?;\n    let symbol = index\n        .symbols\n        .iter()', 'let index_symbols = load_or_build_or_create(root)?;\n    let symbol = index_symbols\n        .iter()')

content = content.replace('build_context(&index, &symbol)', 'build_context(&index_symbols, &symbol)')

build_context_code = """
fn build_context(
    index_symbols: &[TsSymbol],
    symbol: &TsSymbol,
) -> (Vec<TsCallee>, Vec<TsCaller>, Vec<TsResolvedImport>, Vec<TsSuggestedRead>) {
    let id_to_symbol: std::collections::BTreeMap<String, &TsSymbol> =
        index_symbols.iter().map(|s| (s.id.clone(), s)).collect();

    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| TsCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: resolve_call(index_symbols, symbol, call)
                .into_iter()
                .map(|s| s.id.clone())
                .collect(),
        })
        .collect();

    let resolved_imports = resolve_imports(index_symbols, symbol);

    let mut suggested_reads = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for callee in &callees {
        for matched_id in &callee.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched_symbol) = id_to_symbol.get(matched_id) {
                suggested_reads.push(TsSuggestedRead {
                    reason: suggestion_reason_callee(symbol, matched_symbol).to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: TsSymbolSummary::from(*matched_symbol),
                });
            }
        }
    }

    for ri in &resolved_imports {
        for matched_id in &ri.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched_symbol) = id_to_symbol.get(matched_id) {
                suggested_reads.push(TsSuggestedRead {
                    reason: suggestion_reason_import(symbol, matched_symbol).to_string(),
                    trigger_call: ri.imported_name.clone(),
                    trigger_line: symbol.start_line,
                    trigger_snippet: format!("import {} from '{}'", ri.local_name, ri.source),
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
            if resolve_call(index_symbols, item, call)
                .into_iter()
                .any(|m| m.id == symbol.id)
            {
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
"""
content = re.sub(r'fn build_context\(\n    index: &TsIndex,\n    symbol: &TsSymbol,\n\) -> \(Vec<TsCallee>, Vec<TsCaller>, Vec<TsResolvedImport>, Vec<TsSuggestedRead>\) \{.*?\n\}\n\nfn resolve_call', build_context_code.strip() + '\n\nfn resolve_call', content, flags=re.DOTALL)

content = content.replace('fn resolve_call<' + "'a" + '>(\n    index: &' + "'a" + ' TsIndex,', 'fn resolve_call<' + "'a" + '>(\n    index_symbols: &' + "'a" + ' [TsSymbol],')
content = content.replace('matches.extend(index.symbols.iter()', 'matches.extend(index_symbols.iter()')
content = content.replace('index.symbols.iter().filter', 'index_symbols.iter().filter')

content = content.replace('fn resolve_imports(\n    index: &TsIndex,\n    symbol: &TsSymbol,\n) -> Vec<TsResolvedImport>', 'fn resolve_imports(\n    index_symbols: &[TsSymbol],\n    symbol: &TsSymbol,\n) -> Vec<TsResolvedImport>')
content = content.replace('resolve_import(index, symbol, imp)', 'resolve_import(index_symbols, symbol, imp)')

content = content.replace('fn resolve_import(\n    index: &TsIndex,\n    symbol: &TsSymbol,\n    imp: &TsImport,\n) -> Option<TsResolvedImport>', 'fn resolve_import(\n    index_symbols: &[TsSymbol],\n    symbol: &TsSymbol,\n    imp: &TsImport,\n) -> Option<TsResolvedImport>')
content = content.replace('resolve_reexports(index, symbol, imp.source.clone(), imp.imported_name.clone(), imp.kind.clone())', 'resolve_reexports(index_symbols, symbol, imp.source.clone(), imp.imported_name.clone(), imp.kind.clone())')

content = content.replace('fn resolve_reexports(\n    index: &TsIndex,\n    symbol: &TsSymbol,\n    mut source: String,\n    mut imported_name: String,\n    mut kind: TsImportKind,\n) -> (String, Option<String>, String, TsImportKind, Vec<TsExportChainStep>)', 'fn resolve_reexports(\n    index_symbols: &[TsSymbol],\n    symbol: &TsSymbol,\n    mut source: String,\n    mut imported_name: String,\n    mut kind: TsImportKind,\n) -> (String, Option<String>, String, TsImportKind, Vec<TsExportChainStep>)')
content = content.replace('for re_export in &index.re_exports {', 'for re_export in index_symbols.iter().flat_map(|s| &s.re_exports) {')  # Wait! TsReExport is in TsIndex!

index_ws_code = """
pub fn index_workspace(root: &Path) -> Result<IndexTsWorkspaceResponse> {
    let (files_indexed, symbols_indexed) = build_index(root)?;
    Ok(IndexTsWorkspaceResponse {
        index_path: "SQLite".to_string(),
        files_indexed,
        symbols_indexed,
        generated_at_unix: crate::rust_index::now_unix(),
    })
}

pub fn status(root: &Path) -> TsIndexStatus {
    let conn = crate::database::init_db(root).unwrap();
    let symbols_indexed: i64 = conn.query_row(
        "SELECT count(*) FROM ts_symbols WHERE workspace_root = ?",
        rusqlite::params![root.to_string_lossy()],
        |row| row.get(0)
    ).unwrap_or(0);
    
    if symbols_indexed > 0 {
        let files_indexed: i64 = conn.query_row(
            "SELECT count(DISTINCT file_path) FROM ts_symbols WHERE workspace_root = ?",
            rusqlite::params![root.to_string_lossy()],
            |row| row.get(0)
        ).unwrap_or(0);
        return TsIndexStatus {
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: Some(crate::rust_index::now_unix()),
            files_indexed: Some(files_indexed as usize),
            symbols_indexed: Some(symbols_indexed as usize),
        };
    }
    TsIndexStatus {
        index_path: "SQLite".to_string(),
        exists: false,
        workspace_root: root.display().to_string(),
        generated_at_unix: None,
        files_indexed: None,
        symbols_indexed: None,
    }
}

pub fn maybe_reindex_after_write(
    root: &Path,
    changed_path: &Path,
) -> Result<Option<IndexTsWorkspaceResponse>> {
    if !matches!(
        changed_path.extension().and_then(|value| value.to_str()),
        Some("ts" | "tsx" | "js" | "jsx")
    ) {
        return Ok(None);
    }
    let conn = crate::database::init_db(root).unwrap();
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM ts_symbols WHERE workspace_root = ?",
        rusqlite::params![root.to_string_lossy()],
        |row| row.get(0)
    ).unwrap_or(0);
    if count == 0 {
        return Ok(None);
    }
    index_workspace(root).map(Some)
}
"""
content = re.sub(r'pub fn index_workspace\(root: &Path\) -> Result<IndexTsWorkspaceResponse> \{.*?pub fn list_symbols', index_ws_code.strip() + '\n\npub fn list_symbols', content, flags=re.DOTALL)

build_idx_code = """
fn build_index(root: &Path) -> Result<(usize, usize)> {
    let mut files_indexed = 0;
    let mut symbols_indexed = 0;
    
    let mut conn = crate::database::init_db(root).unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute("DELETE FROM ts_symbols WHERE workspace_root = ?", rusqlite::params![root.to_string_lossy()]).unwrap();

    for entry in walk_source_files(root) {
        let path = match entry {
            Ok(p) => p,
            Err(_) => continue,
        };
        if path.extension().and_then(|value| value.to_str()) == Some("js")
            || path.extension().and_then(|value| value.to_str()) == Some("jsx")
            || path.extension().and_then(|value| value.to_str()) == Some("ts")
            || path.extension().and_then(|value| value.to_str()) == Some("tsx")
        {
            let metadata = std::fs::metadata(&path)?;
            if metadata.len() > MAX_TS_FILE_BYTES {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            files_indexed += 1;
            let parsed = parse_ts_file(root, &path, &content);
            for mut sym in parsed.symbols {
                sym.re_exports = parsed.re_exports.clone(); // Attach re_exports to symbol
                let export_names_json = serde_json::to_string(&sym.export_names).unwrap_or_default();
                let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();
                let import_bindings_json = serde_json::to_string(&sym.import_bindings).unwrap_or_default();
                let imports_json = serde_json::to_string(&sym.imports).unwrap_or_default();
                let kind = serde_json::to_string(&sym.kind).unwrap_or_default().trim_matches('"').to_string();
                let re_exports_json = serde_json::to_string(&sym.re_exports).unwrap_or_default();
                
                tx.execute(
                    "INSERT INTO ts_symbols (
                        id, workspace_root, name, kind, file_path, scope_path, parent_id, start_line, end_line,
                        signature, docstring, export, export_names_json, calls_json, import_bindings_json, imports_json, re_exports_json
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    rusqlite::params![
                        sym.id, root.to_string_lossy(), sym.name, kind, sym.file_path, sym.scope_path, sym.parent_id,
                        sym.start_line, sym.end_line, sym.signature, sym.docstring, if sym.export { 1 } else { 0 },
                        export_names_json, calls_json, import_bindings_json, imports_json, re_exports_json
                    ]
                ).unwrap();
                symbols_indexed += 1;
            }
        }
    }
    tx.commit().unwrap();
    Ok((files_indexed, symbols_indexed))
}
"""
content = re.sub(r'fn build_index\(root: &Path\) -> Result<TsIndex> \{.*?Ok\(TsIndex \{.*?\}\)\n\}', build_idx_code.strip(), content, flags=re.DOTALL)

with open('src/ts_index.rs', 'w', encoding='utf-8') as f:
    f.write(content)
