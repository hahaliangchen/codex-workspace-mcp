import re
import sys

with open('src/go_index.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# Add Sqlite error
content = content.replace('    Json(#[from] serde_json::Error),\n}', '    Json(#[from] serde_json::Error),\n    #[error("sqlite error: {0}")]\n    Sqlite(#[from] rusqlite::Error),\n}')

load_symbols_code = """
fn load_all_symbols(root: &std::path::Path) -> Result<Vec<GoSymbol>> {
    let conn = crate::database::init_db(root).map_err(|e| GoIndexError::SymbolNotFound(e.to_string()))?;
    let mut stmt = conn.prepare("SELECT id, name, kind, package_name, file_path, start_line, end_line, signature, docstring, receiver, receiver_name, receiver_type, calls_json FROM go_symbols WHERE workspace_root = ?").map_err(|e| GoIndexError::SymbolNotFound(e.to_string()))?;
    let symbol_iter = stmt.query_map(rusqlite::params![root.to_string_lossy()], |row| {
        Ok(GoSymbol {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: serde_json::from_str(&format!("\\"{}\\"", row.get::<_, String>(2)?)).unwrap_or(GoSymbolKind::Function),
            package: row.get(3)?,
            file_path: row.get(4)?,
            start_line: row.get(5)?,
            end_line: row.get(6)?,
            signature: row.get(7)?,
            docstring: row.get(8)?,
            receiver: row.get(9)?,
            receiver_name: row.get(10)?,
            receiver_type: row.get(11)?,
            calls: serde_json::from_str(&row.get::<_, String>(12)?).unwrap_or_default(),
        })
    }).map_err(|e| GoIndexError::SymbolNotFound(e.to_string()))?;

    let mut symbols = Vec::new();
    for sym in symbol_iter {
        if let Ok(s) = sym {
            symbols.push(s);
        }
    }
    Ok(symbols)
}

fn load_or_build_or_create(root: &std::path::Path) -> Result<Vec<GoSymbol>> {
    let mut symbols = load_all_symbols(root)?;
    if symbols.is_empty() {
        index_workspace(root)?;
        symbols = load_all_symbols(root)?;
    }
    Ok(symbols)
}
"""

content = re.sub(r'fn load_or_build\(root: &Path\) -> Result<GoIndex> \{.*?\n\}\n\nfn load_or_build_or_create\(root: &Path\) -> Result<GoIndex> \{.*?\n\}', load_symbols_code.strip(), content, flags=re.DOTALL)

content = content.replace('let index = load_or_build_or_create(root)?;\n    let symbols = index\n        .symbols\n        .iter()', 'let index_symbols = load_or_build_or_create(root)?;\n    let symbols = index_symbols\n        .iter()')
content = content.replace('let mut matches: Vec<_> = index\n        .symbols\n        .iter()', 'let index_symbols = load_or_build_or_create(root)?;\n    let mut matches: Vec<_> = index_symbols\n        .iter()')

content = content.replace('let index = load_or_build(root)?;\n    let symbol = index\n        .symbols\n        .iter()', 'let index_symbols = load_or_build_or_create(root)?;\n    let symbol = index_symbols\n        .iter()')

content = content.replace('build_context(&index, &symbol)', 'build_context(&index_symbols, &symbol)')

content = content.replace('fn build_context(\n    index: &GoIndex,', 'fn build_context(\n    index_symbols: &[GoSymbol],')
content = content.replace('for item in &index.symbols {', 'for item in index_symbols {')
content = content.replace('resolve_call(index, item, call)', 'resolve_call(index_symbols, item, call)')

content = content.replace('fn resolve_call<\'a>(\n    index: &\'a GoIndex,', 'fn resolve_call<\'a>(\n    index_symbols: &\'a [GoSymbol],')
content = content.replace('matches.extend(index.symbols.iter()', 'matches.extend(index_symbols.iter()')
content = content.replace('matches.extend(\n                index\n                    .symbols\n                    .iter()', 'matches.extend(\n                index_symbols\n                    .iter()')

index_ws_code = """
pub fn index_workspace(root: &Path) -> Result<IndexGoWorkspaceResponse> {
    let (files_indexed, symbols_indexed) = build_index(root)?;
    Ok(IndexGoWorkspaceResponse {
        index_path: "SQLite".to_string(),
        files_indexed,
        symbols_indexed,
        generated_at_unix: crate::rust_index::now_unix(),
    })
}

pub fn status(root: &Path) -> GoIndexStatus {
    let conn = crate::database::init_db(root).unwrap();
    let symbols_indexed: i64 = conn.query_row(
        "SELECT count(*) FROM go_symbols WHERE workspace_root = ?",
        rusqlite::params![root.to_string_lossy()],
        |row| row.get(0)
    ).unwrap_or(0);
    
    if symbols_indexed > 0 {
        let files_indexed: i64 = conn.query_row(
            "SELECT count(DISTINCT file_path) FROM go_symbols WHERE workspace_root = ?",
            rusqlite::params![root.to_string_lossy()],
            |row| row.get(0)
        ).unwrap_or(0);
        return GoIndexStatus {
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: Some(crate::rust_index::now_unix()),
            files_indexed: Some(files_indexed as usize),
            symbols_indexed: Some(symbols_indexed as usize),
        };
    }
    GoIndexStatus {
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
) -> Result<Option<IndexGoWorkspaceResponse>> {
    if changed_path.extension().and_then(|value| value.to_str()) != Some("go") {
        return Ok(None);
    }
    let conn = crate::database::init_db(root).unwrap();
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM go_symbols WHERE workspace_root = ?",
        rusqlite::params![root.to_string_lossy()],
        |row| row.get(0)
    ).unwrap_or(0);
    if count == 0 {
        return Ok(None);
    }
    index_workspace(root).map(Some)
}
"""
content = re.sub(r'pub fn index_workspace\(root: &Path\) -> Result<IndexGoWorkspaceResponse> \{.*?pub fn list_symbols', index_ws_code.strip() + '\n\npub fn list_symbols', content, flags=re.DOTALL)

build_idx_code = """
fn build_index(root: &Path) -> Result<(usize, usize)> {
    let mut files_indexed = 0;
    let mut symbols_indexed = 0;
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

    let mut conn = crate::database::init_db(root).unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute("DELETE FROM go_symbols WHERE workspace_root = ?", rusqlite::params![root.to_string_lossy()]).unwrap();

    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("go") {
            continue;
        }
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > MAX_GO_FILE_BYTES {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let parsed = match parse_go_file(root, path, &content) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        files_indexed += 1;
        for sym in parsed.symbols {
            let calls_json = serde_json::to_string(&sym.calls).unwrap_or_default();
            let kind = serde_json::to_string(&sym.kind).unwrap_or_default().trim_matches('"').to_string();
            tx.execute(
                "INSERT INTO go_symbols (
                    id, workspace_root, name, kind, package_name, file_path, start_line, end_line,
                    signature, docstring, receiver, receiver_name, receiver_type, calls_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    sym.id, root.to_string_lossy(), sym.name, kind, sym.package, sym.file_path,
                    sym.start_line, sym.end_line, sym.signature, sym.docstring,
                    sym.receiver, sym.receiver_name, sym.receiver_type, calls_json
                ]
            ).unwrap();
            symbols_indexed += 1;
        }
    }
    tx.commit().unwrap();
    Ok((files_indexed, symbols_indexed))
}
"""
content = re.sub(r'fn build_index\(root: &Path\) -> Result<GoIndex> \{.*?Ok\(GoIndex \{.*?\}\)\n\}', build_idx_code.strip(), content, flags=re.DOTALL)

with open('src/go_index.rs', 'w', encoding='utf-8') as f:
    f.write(content)
