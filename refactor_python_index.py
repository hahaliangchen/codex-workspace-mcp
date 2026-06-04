import re

with open('src/python_index.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# Add Sqlite error
content = content.replace('    Json(#[from] serde_json::Error),\n}', '    Json(#[from] serde_json::Error),\n    #[error("sqlite error: {0}")]\n    Sqlite(#[from] rusqlite::Error),\n}')

# 1. Add file_imports to PythonSymbol
content = content.replace('pub struct PythonSymbol {\n    pub id: String,', 'pub struct PythonSymbol {\n    pub id: String,\n    #[serde(default)]\n    pub file_imports: Vec<PythonImport>,')

# 2. Add file_imports initialization in parse_symbol (Wait, I will handle it via replace in python_index.rs)
content = content.replace('            decorators,\n            calls,\n        })', '            decorators,\n            calls,\n            file_imports: Vec::new(),\n        })')

# 3. New database load methods
load_symbols_code = """
fn load_all_symbols(root: &std::path::Path) -> Result<Vec<PythonSymbol>> {
    let conn = crate::database::init_db(root).map_err(|e| PythonIndexError::SymbolNotFound(e.to_string()))?;
    let mut stmt = conn.prepare("SELECT id, name, kind, file_path, class_name, start_line, end_line, signature, docstring, decorators_json, calls_json, file_imports_json FROM python_symbols WHERE workspace_root = ?").map_err(|e| PythonIndexError::SymbolNotFound(e.to_string()))?;
    let symbol_iter = stmt.query_map(rusqlite::params![root.to_string_lossy()], |row| {
        Ok(PythonSymbol {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: serde_json::from_str(&format!("\\"{}\\"", row.get::<_, String>(2)?)).unwrap_or(PythonSymbolKind::Function),
            file_path: row.get(3)?,
            class_name: row.get(4)?,
            start_line: row.get(5)?,
            end_line: row.get(6)?,
            signature: row.get(7)?,
            docstring: row.get(8)?,
            decorators: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default(),
            calls: serde_json::from_str(&row.get::<_, String>(10)?).unwrap_or_default(),
            file_imports: serde_json::from_str(&row.get::<_, String>(11)?).unwrap_or_default(),
        })
    }).map_err(|e| PythonIndexError::SymbolNotFound(e.to_string()))?;

    let mut symbols = Vec::new();
    for sym in symbol_iter {
        if let Ok(s) = sym {
            symbols.push(s);
        }
    }
    Ok(symbols)
}

fn load_or_build_or_create(root: &std::path::Path) -> Result<Vec<PythonSymbol>> {
    let mut symbols = load_all_symbols(root)?;
    if symbols.is_empty() {
        index_workspace(root)?;
        symbols = load_all_symbols(root)?;
    }
    Ok(symbols)
}

fn load_or_build(root: &std::path::Path) -> Result<Vec<PythonSymbol>> {
    let symbols = load_all_symbols(root)?;
    if symbols.is_empty() {
        return Err(PythonIndexError::MissingIndex);
    }
    Ok(symbols)
}
"""

content = re.sub(r'fn load_or_build\(root: &Path\) -> Result<PythonIndex> \{.*?\n\}\n\nfn load_or_build_or_create\(root: &Path\) -> Result<PythonIndex> \{.*?\n\}', load_symbols_code.strip(), content, flags=re.DOTALL)

# 4. Fix search_symbols, read_symbol
content = content.replace('let index = load_or_build_or_create(root)?;\n    let symbols = index\n        .symbols\n        .iter()', 'let index_symbols = load_or_build_or_create(root)?;\n    let symbols = index_symbols\n        .iter()')
content = content.replace('let mut matches: Vec<_> = index\n        .symbols\n        .iter()', 'let index_symbols = load_or_build_or_create(root)?;\n    let mut matches: Vec<_> = index_symbols\n        .iter()')
content = content.replace('let index = load_or_build(root)?;\n    let symbol = index\n        .symbols\n        .iter()', 'let index_symbols = load_or_build_or_create(root)?;\n    let symbol = index_symbols\n        .iter()')
content = content.replace('build_context(&index, &symbol)', 'build_context(&index_symbols, &symbol)')

# 5. Fix build_context
build_context_code = """
fn build_context(
    index_symbols: &[PythonSymbol],
    symbol: &PythonSymbol,
) -> (Vec<PythonCaller>, Vec<PythonCallee>, Vec<PythonSuggestedRead>) {
    let mut id_to_symbol = std::collections::BTreeMap::new();
    for item in index_symbols {
        id_to_symbol.insert(item.id.clone(), item);
    }

    let mut file_infos = std::collections::BTreeMap::new();
    for sym in index_symbols {
        file_infos.entry(sym.file_path.clone()).or_insert_with(|| PythonFileInfo {
            file_path: sym.file_path.clone(),
            imports: sym.file_imports.clone(),
        });
    }

    let callees: Vec<_> = symbol
        .calls
        .iter()
        .map(|call| PythonCallee {
            target_text: call.target_text.clone(),
            line: call.line,
            snippet: call.snippet.clone(),
            matched_symbol_ids: resolve_call(index_symbols, &file_infos, symbol, call)
                .into_iter()
                .map(|s| s.id.clone())
                .collect(),
        })
        .collect();

    let mut suggested_reads = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for callee in &callees {
        for matched_id in &callee.matched_symbol_ids {
            if matched_id == &symbol.id || !seen.insert(matched_id.clone()) {
                continue;
            }
            if let Some(matched_symbol) = id_to_symbol.get(matched_id) {
                suggested_reads.push(PythonSuggestedRead {
                    reason: suggestion_reason(symbol, matched_symbol).to_string(),
                    trigger_call: callee.target_text.clone(),
                    trigger_line: callee.line,
                    trigger_snippet: callee.snippet.clone(),
                    symbol: PythonSymbolSummary::from(*matched_symbol),
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
            let matched = resolve_call(index_symbols, &file_infos, item, call)
                .into_iter()
                .any(|m| m.id == symbol.id);
            if matched {
                callers.push(PythonCaller {
                    symbol_id: item.id.clone(),
                    name: item.name.clone(),
                    file_path: item.file_path.clone(),
                    line: call.line,
                    snippet: call.snippet.clone(),
                });
            }
        }
    }

    (callers, callees, suggested_reads)
}
"""

content = re.sub(r'fn build_context\(\n    index: &PythonIndex,\n    symbol: &PythonSymbol,\n\) -> \(Vec<PythonCaller>, Vec<PythonCallee>, Vec<PythonSuggestedRead>\) \{.*?\n\}\n\nfn resolve_call', build_context_code.strip() + '\n\nfn resolve_call', content, flags=re.DOTALL)

content = content.replace('fn resolve_call<' + "'a" + '>(\n    index: &' + "'a" + ' PythonIndex,', 'fn resolve_call<' + "'a" + '>(\n    index_symbols: &' + "'a" + ' [PythonSymbol],')
content = content.replace('index.symbols.iter()', 'index_symbols.iter()')
content = content.replace('resolve_call(index, &file_infos, symbol, call)', 'resolve_call(index_symbols, &file_infos, symbol, call)')
content = content.replace('resolve_call(index, &file_infos, item, call)', 'resolve_call(index_symbols, &file_infos, item, call)')

# 6. index_workspace / status / build_index
index_ws_code = """
pub fn index_workspace(root: &Path) -> Result<IndexPythonWorkspaceResponse> {
    let (files_indexed, symbols_indexed) = build_index(root)?;
    Ok(IndexPythonWorkspaceResponse {
        index_path: "SQLite".to_string(),
        files_indexed,
        symbols_indexed,
        generated_at_unix: crate::rust_index::now_unix(),
    })
}

pub fn status(root: &Path) -> PythonIndexStatus {
    let conn = crate::database::init_db(root).unwrap();
    let symbols_indexed: i64 = conn.query_row(
        "SELECT count(*) FROM python_symbols WHERE workspace_root = ?",
        rusqlite::params![root.to_string_lossy()],
        |row| row.get(0)
    ).unwrap_or(0);
    
    if symbols_indexed > 0 {
        let files_indexed: i64 = conn.query_row(
            "SELECT count(DISTINCT file_path) FROM python_symbols WHERE workspace_root = ?",
            rusqlite::params![root.to_string_lossy()],
            |row| row.get(0)
        ).unwrap_or(0);
        return PythonIndexStatus {
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: Some(crate::rust_index::now_unix()),
            files_indexed: Some(files_indexed as usize),
            symbols_indexed: Some(symbols_indexed as usize),
        };
    }
    PythonIndexStatus {
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
) -> Result<Option<IndexPythonWorkspaceResponse>> {
    if changed_path.extension().and_then(|value| value.to_str()) != Some("py") {
        return Ok(None);
    }
    let conn = crate::database::init_db(root).unwrap();
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM python_symbols WHERE workspace_root = ?",
        rusqlite::params![root.to_string_lossy()],
        |row| row.get(0)
    ).unwrap_or(0);
    if count == 0 {
        return Ok(None);
    }
    index_workspace(root).map(Some)
}
"""

content = re.sub(r'pub fn index_workspace\(root: &Path\) -> Result<IndexPythonWorkspaceResponse> \{.*?pub fn list_symbols', index_ws_code.strip() + '\n\npub fn list_symbols', content, flags=re.DOTALL)

build_idx_code = """
fn build_index(root: &Path) -> Result<(usize, usize)> {
    let mut files_indexed = 0;
    let mut symbols_indexed = 0;
    
    let mut conn = crate::database::init_db(root).unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute("DELETE FROM python_symbols WHERE workspace_root = ?", rusqlite::params![root.to_string_lossy()]).unwrap();

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| !NOISE_DIRS.contains(&name))
                .unwrap_or(true)
        });

    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("py") {
            continue;
        }
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > MAX_PYTHON_FILE_BYTES {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        files_indexed += 1;
        let parsed = match parse_python_file(root, path, &content) {
            Ok(p) => p,
            Err(_) => continue,
        };
        
        for sym in parsed.symbols {
            let kind = serde_json::to_string(&sym.kind).unwrap_or_default().trim_matches('"').to_string();
            let decorators_json = serde_json::to_string(&sym.decorators).unwrap_or_default();
            let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();
            let file_imports_json = serde_json::to_string(&parsed.file.imports).unwrap_or_default();
            
            tx.execute(
                "INSERT INTO python_symbols (
                    id, workspace_root, name, kind, file_path, class_name, start_line, end_line,
                    signature, docstring, decorators_json, calls_json, file_imports_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    sym.id, root.to_string_lossy(), sym.name, kind, sym.file_path, sym.class_name,
                    sym.start_line, sym.end_line, sym.signature, sym.docstring, decorators_json, calls_json, file_imports_json
                ]
            ).unwrap();
            symbols_indexed += 1;
        }
    }
    tx.commit().unwrap();
    Ok((files_indexed, symbols_indexed))
}
"""

content = re.sub(r'fn build_index\(root: &Path\) -> Result<PythonIndex> \{.*?Ok\(PythonIndex \{.*?\}\)\n\}', build_idx_code.strip(), content, flags=re.DOTALL)

with open('src/python_index.rs', 'w', encoding='utf-8') as f:
    f.write(content)
