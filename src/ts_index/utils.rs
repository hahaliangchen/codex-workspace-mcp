use std::path::Path;

use swc_common::{SourceMap, Span, sync::Lrc};
use swc_ecma_ast::*;

use crate::ts_index::*;

pub(crate) fn exported_names(symbol: &TsSymbol) -> Vec<String> {
    if !symbol.export_names.is_empty() {
        symbol.export_names.clone()
    } else if symbol.export {
        export_names(&symbol.name, true)
    } else {
        Vec::new()
    }
}

pub(crate) fn export_names(name: &str, export: bool) -> Vec<String> {
    if !export {
        return Vec::new();
    }
    if name == "default" || name == "defaultExport" {
        vec!["default".to_string()]
    } else {
        vec![name.to_string()]
    }
}

pub(crate) fn default_export_names() -> Vec<String> {
    vec!["default".to_string()]
}

pub(crate) fn module_export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::Ident(ident) => ident.sym.to_string(),
        ModuleExportName::Str(value) => value.value.to_string_lossy().to_string(),
    }
}

pub(crate) fn normalize_workspace_relative_path(value: &str) -> String {
    let mut normalized = normalize_slashes(value);
    while normalized.starts_with("./") {
        normalized = normalized.trim_start_matches("./").to_string();
    }
    while normalized.starts_with(".\\") {
        normalized = normalized.trim_start_matches(".\\").to_string();
    }
    normalized
}

pub(crate) fn load_all_symbols(root: &std::path::Path) -> Result<Vec<TsSymbol>> {
    load_selected_symbols(
        root,
        &crate::symbol_query::Selection {
            include_locals: true,
            include_relationships: true,
            include_details: true,
            ..Default::default()
        },
    )
}

pub(crate) fn load_selected_symbols(
    root: &std::path::Path,
    selection: &crate::symbol_query::Selection<'_>,
) -> Result<Vec<TsSymbol>> {
    let conn =
        crate::database::init_db(root).map_err(|e| TsIndexError::SymbolNotFound(e.to_string()))?;
    let (mut where_sql, mut params) = selection.sql(
        root,
        "ts_symbols",
        &["name", "scope_path", "signature", "docstring", "file_path"],
    );
    let mut select = "SELECT id, name, kind, file_path, scope_path, parent_id, start_line, end_line, signature, docstring, export, export_names_json, calls_json, import_bindings_json, imports_json, re_exports_json FROM ts_symbols".to_owned();
    if !selection.include_relationships {
        for column in &[
            "export_names_json",
            "calls_json",
            "import_bindings_json",
            "imports_json",
            "re_exports_json",
        ] {
            select = select.replace(column, &format!("'[]' AS {column}"));
        }
    }
    if !selection.include_details {
        select = select
            .replace(", signature,", ", '' AS signature,")
            .replace(", docstring,", ", '' AS docstring,");
    }
    if let Some((offset, limit)) = selection.pagination {
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(
            i64::try_from(offset).unwrap_or(i64::MAX),
        ));
        where_sql.push_str(&format!(
            " LIMIT ?{} OFFSET ?{}",
            params.len() - 1,
            params.len()
        ));
    }
    let mut stmt = conn
        .prepare(&(select + where_sql.as_str()))
        .map_err(|e| TsIndexError::SymbolNotFound(e.to_string()))?;
    let symbol_iter = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(TsSymbol {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: serde_json::from_str(&format!("\"{}\"", row.get::<_, String>(2)?))
                    .unwrap_or(TsSymbolKind::Function),
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
                import_bindings: serde_json::from_str(&row.get::<_, String>(13)?)
                    .unwrap_or_default(),
                imports: serde_json::from_str(&row.get::<_, String>(14)?).unwrap_or_default(),
                re_exports: serde_json::from_str(&row.get::<_, String>(15)?).unwrap_or_default(),
            })
        })
        .map_err(|e| TsIndexError::SymbolNotFound(e.to_string()))?;

    Ok(symbol_iter.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub(crate) fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(crate) fn normalize_slashes(value: &str) -> String {
    value.replace('\\', "/")
}

pub(crate) fn ts_symbol_id(file_path: &str, scope_path: &str, name: &str, line: usize) -> String {
    if scope_path.is_empty() {
        format!("ts:{file_path}:{name}:{line}")
    } else {
        format!("ts:{file_path}:{scope_path}.{name}:{line}")
    }
}

pub(crate) fn snippet(content: &str, cm: &Lrc<SourceMap>, span: Span, max_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = line_of(cm, span.lo).saturating_sub(1);
    let end = line_of(cm, span.hi).min(lines.len());
    lines[start..end.min(start + max_lines)].join("\n")
}

pub(crate) fn line_of(cm: &Lrc<SourceMap>, pos: swc_common::BytePos) -> usize {
    cm.lookup_char_pos(pos).line
}

pub(crate) fn looks_like_component(name: &str) -> bool {
    name.chars()
        .next()
        .map(|ch| ch.is_uppercase())
        .unwrap_or(false)
}
