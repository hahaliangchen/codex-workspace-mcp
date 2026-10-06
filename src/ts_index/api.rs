use std::path::Path;

use crate::ts_index::*;

pub fn index_workspace(root: &Path) -> Result<IndexTsWorkspaceResponse> {
    let (files_indexed, symbols_indexed) = build_index(root)?;
    Ok(IndexTsWorkspaceResponse {
        index: crate::symbol_index_state::health(root, "ts", "ts_symbols")?,
        index_path: "SQLite".to_string(),
        files_indexed,
        symbols_indexed,
        generated_at_unix: status(root)
            .generated_at_unix
            .unwrap_or_else(crate::rust_index::now_unix),
    })
}

pub fn status(root: &Path) -> TsIndexStatus {
    let Some(conn) = crate::database::init_db(root).ok() else {
        return TsIndexStatus {
            index: None,
            index_path: "SQLite".to_string(),
            exists: false,
            workspace_root: root.display().to_string(),
            generated_at_unix: None,
            files_indexed: None,
            symbols_indexed: None,
        };
    };
    // Bug3: 读取元数据中记录的真实索引创建时间
    let generated_at =
        crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "ts");
    if generated_at.is_some() {
        let symbols_indexed: i64 = conn
            .query_row(
                "SELECT count(*) FROM ts_symbols WHERE workspace_root = ?",
                rusqlite::params![root.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let files_indexed: i64 = conn
            .query_row(
                "SELECT count(*) FROM symbol_index_files WHERE workspace_root = ? AND language = 'ts' AND status = 'indexed'",
                rusqlite::params![root.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        return TsIndexStatus {
            index: crate::symbol_index_state::health(root, "ts", "ts_symbols").ok(),
            index_path: "SQLite".to_string(),
            exists: true,
            workspace_root: root.display().to_string(),
            generated_at_unix: generated_at,
            files_indexed: Some(files_indexed as usize),
            symbols_indexed: Some(symbols_indexed as usize),
        };
    }
    TsIndexStatus {
        index: crate::symbol_index_state::health(root, "ts", "ts_symbols").ok(),
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
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")
    ) {
        return Ok(None);
    }
    let conn = crate::database::init_db(root)?;
    if crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), "ts").is_none() {
        return Ok(None);
    }
    let (files_indexed,symbols_indexed)=build_index_scope(root,Some(changed_path))?;
    Ok(Some(IndexTsWorkspaceResponse {index:crate::symbol_index_state::health(root,"ts","ts_symbols")?,index_path:"SQLite".into(),files_indexed,symbols_indexed,generated_at_unix:crate::rust_index::now_unix()}))
}

pub fn list_symbols(root: &Path, request: ListTsSymbolsRequest) -> Result<ListTsSymbolsResponse> {
    ensure_query_index(root,request.file_path.as_deref(),request.options.directory.as_deref())?;
    let kind = request
        .kind
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?
        .and_then(|value| value.as_str().map(str::to_owned));
    let mut selection = crate::symbol_query::Selection {
        file_path: request.file_path.as_deref(),
        directory: request.options.directory.as_deref(),
        kind: kind.as_deref(),
        include_locals: request.options.include_locals,
        include_details: request.options.detailed,
        ..Default::default()
    };
    let total = selection.count(root, "ts_symbols")?;
    let page = crate::symbol_query::page(total, request.options.offset, request.options.limit);
    selection.pagination = Some((page.offset, page.limit));
    let symbols = load_selected_symbols(root, &selection)?;
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "ts", &symbols.iter().map(|symbol|symbol.file_path.as_str()).collect::<Vec<_>>())?;
    let symbols = symbols
        .iter()
        .map(|symbol| {
            let mut summary = compact_summary(symbol, request.options.detailed);
            summary.description = Some(describe_symbol(&catalog, symbol));
            summary
        })
        .collect();
    Ok(ListTsSymbolsResponse {
        symbols,
        page,
        index: crate::symbol_index_state::health(root, "ts", "ts_symbols")?,
    })
}

fn describe_symbol(
    catalog: &crate::symbol_description::Catalog,
    symbol: &TsSymbol,
) -> crate::symbol_description::Description {
    let scope = symbol.scope_path.clone();
    let qualified = crate::symbol_description::qualified_name("ts", &scope, &symbol.name);
    catalog.describe(&symbol.id, &symbol.file_path, &qualified)
}

fn compact_summary(symbol: &TsSymbol, detailed: bool) -> TsSymbolSummary {
    let mut summary = TsSymbolSummary::from(symbol);
    if detailed {
        summary.signature = crate::symbol_query::preview(&symbol.signature, 240);
        summary.docstring = crate::symbol_query::preview(&symbol.docstring, 480);
    } else {
        summary.signature.clear();
        summary.docstring.clear();
    }
    summary
}

pub fn search_symbols(
    root: &Path,
    request: SearchTsSymbolsRequest,
) -> Result<SearchTsSymbolsResponse> {
    let terms = crate::symbol_query::terms(&request.query, request.options.match_mode);
    if terms.is_empty() {
        return Err(std::io::Error::other("query must contain at least one search term").into());
    }
    ensure_query_index(root,request.options.file_path.as_deref(),request.options.directory.as_deref())?;
    let selection = crate::symbol_query::Selection {
        file_path: request.options.file_path.as_deref(),
        directory: request.options.directory.as_deref(),
        kind: request.options.kind.as_deref(),
        include_details: true,
        terms: &terms,
        mode: request.options.match_mode,
        include_locals: request.options.include_locals,
        ..Default::default()
    };
    let symbols = load_selected_symbols(root, &selection)?;
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "ts", &symbols.iter().map(|symbol|symbol.file_path.as_str()).collect::<Vec<_>>())?;
    let mut ranked: Vec<_> = symbols
        .iter()
        .filter_map(|symbol| {
            let scope = symbol.scope_path.clone();
            let description = describe_symbol(&catalog, symbol);
            crate::symbol_query::score(
                &symbol.name,
                &scope,
                &symbol.signature,
                &symbol.docstring,
                &symbol.file_path,
                &terms,
                request.options.match_mode,
                &description,
            )
            .map(|(score, matched)| (score, symbol, matched, description))
        })
        .collect();
    let page = crate::symbol_query::rank_page(&mut ranked, request.options.offset, request.limit, |(a_score, a, _, _), (b_score, b, _, _)| {
        b_score
            .cmp(a_score)
            .then(a.file_path.cmp(&b.file_path))
            .then(a.start_line.cmp(&b.start_line))
            .then(a.id.cmp(&b.id))
    });
    let matches = ranked
        .iter()
        .skip(page.offset)
        .take(page.limit)
        .map(|(score, symbol, matched, description)| {
            let mut summary = compact_summary(symbol, request.options.detailed);
            summary.description = Some(description.clone());
            summary.score = Some(*score);
            summary.matched_terms = matched.clone();
            summary
        })
        .collect();
    Ok(SearchTsSymbolsResponse {
        terms,
        match_mode: request.options.match_mode,
        query: request.query,
        matches,
        page,
        index: crate::symbol_index_state::health(root, "ts", "ts_symbols")?,
    })
}

pub fn read_symbol(root: &Path, request: ReadTsSymbolRequest) -> Result<ReadTsSymbolResponse> {
    read_symbol_snapshot(root,&request,true)
}

fn read_symbol_snapshot(root: &Path, request: &ReadTsSymbolRequest, retry:bool) -> Result<ReadTsSymbolResponse> {
    if request.symbol_id.is_empty()
        && (request
            .options
            .file_path
            .as_deref()
            .is_none_or(str::is_empty)
            || request.options.name.as_deref().is_none_or(str::is_empty))
    {
        return Err(std::io::Error::other(
            "provide symbol_id or both file_path and name (qualified name allowed)",
        )
        .into());
    }
    let initialized=crate::database::init_db(root).ok().is_some_and(|conn|crate::database::get_index_generated_at(&conn,&root.to_string_lossy(),"ts").is_some());
    if !initialized {build_index(root)?;}
    else if request.symbol_id.is_empty() {
        if let Some(file)=request.options.file_path.as_deref() {
            if crate::source_read::needs_index_refresh(root,"ts",file) {maybe_reindex_after_write(root,&root.join(file))?;}
        }
    }
    let by_id = !request.symbol_id.is_empty();
    let selection = crate::symbol_query::Selection {
        symbol_id: by_id.then_some(request.symbol_id.as_str()),
        file_path: if by_id {
            None
        } else {
            request.options.file_path.as_deref()
        },
        name: if by_id {
            None
        } else {
            request.options.name.as_deref()
        },
        include_locals: true,
        include_relationships: true,
        include_details: true,
        ..Default::default()
    };
    let mut candidates = load_selected_symbols(root, &selection)?;
    if !by_id {
        candidates.retain(|symbol| {
            let scope = symbol.scope_path.clone();
            crate::symbol_query::name_matches(
                &symbol.name,
                &scope,
                request.options.name.as_deref().unwrap_or(""),
            )
        });
    }
    if candidates.len() > 1 {
        let choices = candidates
            .iter()
            .take(10)
            .map(|symbol| {
                format!(
                    "{} ({}:{}-{})",
                    symbol.id, symbol.file_path, symbol.start_line, symbol.end_line
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(std::io::Error::other(format!(
            "ambiguous symbol name; use a qualified name or symbol_id: {choices}"
        ))
        .into());
    }
    let symbol = candidates.pop().ok_or_else(|| {
        TsIndexError::SymbolNotFound(if by_id {
            request.symbol_id.clone()
        } else {
            format!(
                "{}:{}",
                request.options.file_path.as_deref().unwrap_or(""),
                request.options.name.as_deref().unwrap_or("")
            )
        })
    })?;
    let snapshot=crate::source_read::read(&root.join(&symbol.file_path),crate::source_read::FILE_LIMIT)?;
    if crate::source_read::indexed_hash(root,"ts",&symbol.file_path).as_deref()!=Some(snapshot.hash.as_str()) {
        if !retry {return Err(std::io::Error::other("source_version_conflict: indexed symbol and source versions differ; no source returned").into());}
        maybe_reindex_after_write(root,&root.join(&symbol.file_path))?;
        return read_symbol_snapshot(root,request,false);
    }
    let source_code_hash=snapshot.hash.clone();
    let content = crate::symbol_query::source_range(&snapshot.content, symbol.start_line, symbol.end_line)?;
    let (callees, callers, resolved_imports, suggested_reads) = if request.include_context {
        let index_symbols = load_all_symbols(root)?;
        build_context(&index_symbols, &symbol)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), Vec::new())
    };
    let catalog = crate::symbol_description::Catalog::load_for_files(root, "ts", &[symbol.file_path.as_str()])?;
    let mut description = describe_symbol(&catalog, &symbol);
    if description.code_hash != source_code_hash && description.status == "current" {
        description.status = "stale";
    }
    description.code_hash = source_code_hash;
    let (related_types, related_type_issues, edit_context) = super::read_details::collect(
        root, &symbol, &content, request.include_related_types, request.include_outline,
    );
    Ok(ReadTsSymbolResponse {
        description,
        symbol,
        content,
        callees,
        callers,
        resolved_imports,
        suggested_reads,
        related_types,
        related_type_issues,
        edit_context,
        index: crate::symbol_index_state::health(root, "ts", "ts_symbols")?,
        relationship_accuracy: "heuristic_not_type_checked",
    })
}
