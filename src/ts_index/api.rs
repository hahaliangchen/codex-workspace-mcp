use std::{fs, path::Path};

use crate::ts_index::*;

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
    // Bug3: 读取元数据中记录的真实索引创建时间
    let generated_at = crate::database::get_index_generated_at(
        &conn,
        &root.to_string_lossy(),
        "ts",
    );
    if generated_at.is_some() {
        let symbols_indexed: i64 = conn.query_row(
            "SELECT count(*) FROM ts_symbols WHERE workspace_root = ?",
            rusqlite::params![root.to_string_lossy()],
            |row| row.get(0)
        ).unwrap_or(0);
        let files_indexed: i64 = conn.query_row(
            "SELECT count(DISTINCT file_path) FROM ts_symbols WHERE workspace_root = ?",
            rusqlite::params![root.to_string_lossy()],
            |row| row.get(0)
        ).unwrap_or(0);
        return TsIndexStatus {
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: generated_at,
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

pub fn list_symbols(root: &Path, request: ListTsSymbolsRequest) -> Result<ListTsSymbolsResponse> {
    let index_symbols = load_or_build_or_create(root)?;
    let symbols = index_symbols
        .iter()
        .filter(|symbol| {
            request
                .file_path
                .as_deref()
                .map(|file| symbol.file_path == normalize_slashes(file))
                .unwrap_or(true)
        })
        .filter(|symbol| {
            request
                .kind
                .as_ref()
                .map(|kind| &symbol.kind == kind)
                .unwrap_or(true)
        })
        .map(TsSymbolSummary::from)
        .collect();
    Ok(ListTsSymbolsResponse { symbols })
}

pub fn search_symbols(
    root: &Path,
    request: SearchTsSymbolsRequest,
) -> Result<SearchTsSymbolsResponse> {
    let needle = request.query.to_lowercase();
    let index_symbols = load_or_build_or_create(root)?;
    let mut matches: Vec<_> = index_symbols
        .iter()
        .filter(|symbol| {
            [
                symbol.name.as_str(),
                symbol.signature.as_str(),
                symbol.docstring.as_str(),
                symbol.file_path.as_str(),
                symbol.scope_path.as_str(),
            ]
            .join("\n")
            .to_lowercase()
            .contains(&needle)
        })
        .take(request.limit.max(1))
        .map(TsSymbolSummary::from)
        .collect();
    matches.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
    });
    Ok(SearchTsSymbolsResponse {
        query: request.query,
        matches,
    })
}

pub fn read_symbol(root: &Path, request: ReadTsSymbolRequest) -> Result<ReadTsSymbolResponse> {
    let index_symbols = load_or_build_or_create(root)?;
    let symbol = index_symbols
        .iter()
        .find(|symbol| symbol.id == request.symbol_id)
        .cloned()
        .ok_or_else(|| TsIndexError::SymbolNotFound(request.symbol_id.clone()))?;
    let path = root.join(&symbol.file_path);
    let content = fs::read_to_string(path)?;
    let lines: Vec<_> = content.lines().collect();
    let content = lines[(symbol.start_line - 1)..symbol.end_line.min(lines.len())].join("\n");
    let (callees, callers, resolved_imports, suggested_reads) = if request.include_context {
        build_context(&index_symbols, &symbol)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), Vec::new())
    };
    Ok(ReadTsSymbolResponse {
        symbol,
        content,
        callees,
        callers,
        resolved_imports,
        suggested_reads,
    })
}

