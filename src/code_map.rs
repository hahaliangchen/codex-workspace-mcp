//! Functional navigation: a definition, source module, or saved business area.
//! This map does not infer responsibilities from call chains or identifier names.
use rusqlite::types::Value as SqlValue;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

#[derive(Deserialize)]
pub struct Request {
    pub query: String,
    pub language: Option<String>,
    pub file_path: Option<String>,
    pub directory: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub group_offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_entries")]
    pub entry_limit: usize,
}
fn default_limit() -> usize {
    8
}
fn default_entries() -> usize {
    6
}

pub fn search(root: &Path, request: Request) -> anyhow::Result<Value> {
    let terms = crate::symbol_query::terms(&request.query, crate::symbol_query::MatchMode::Any);
    anyhow::ensure!(
        !terms.is_empty(),
        "query must contain a functionality or definition name"
    );
    let selected_language = request
        .language
        .as_deref()
        .map(|value| {
            crate::symbol_description::language(value)
                .ok_or_else(|| anyhow::anyhow!("supported languages: rust, ts, python, go"))
        })
        .transpose()?;
    let languages = if let Some(language) = selected_language {
        vec![language]
    } else if let Some(language) = request
        .file_path
        .as_deref()
        .and_then(|file| Path::new(file).extension())
        .and_then(|ext| ext.to_str())
        .and_then(crate::symbol_description::language)
    {
        vec![language]
    } else {
        discover_languages(root)
    };
    let mut groups = BTreeMap::<String, Value>::new();
    let mut saved_membership = BTreeMap::<String, Vec<String>>::new();
    let mut pages = Vec::new();
    let mut issues = Vec::new();
    let entry_limit = request.entry_limit.clamp(1, 20);
    let conn = crate::database::init_db(root)?;
    // A saved area may span languages and files. It remains a remembered guide,
    // not a claim that every current implementation has been verified.
    let mut params = vec![SqlValue::Text(root.to_string_lossy().into_owned())];
    let conditions = terms
        .iter()
        .map(|term| {
            params.push(SqlValue::Text(term.clone()));
            format!(
                "instr(lower(area||' '||summary||' '||common_tasks),?{})>0",
                params.len()
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut stmt=conn.prepare(&format!("SELECT area,summary,key_files,key_symbols,updated_at_unix FROM architecture_memories WHERE workspace_root=?1 AND ({conditions}) ORDER BY updated_at_unix DESC LIMIT 100"))?;
    let areas = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, u64>(4)?,
        ))
    })?;
    for area in areas {
        let (area, summary, files, symbols, updated) = area?;
        let files = serde_json::from_str::<Vec<String>>(&files).unwrap_or_default();
        let files = files
            .into_iter()
            .filter(|file| {
                in_scope(file, &request)
                    && selected_language.is_none_or(|lang| {
                        Path::new(file)
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .and_then(crate::symbol_description::language)
                            == Some(lang)
                    })
            })
            .collect::<Vec<_>>();
        if files.is_empty()
            && (request.file_path.is_some()
                || request.directory.is_some()
                || selected_language.is_some())
        {
            continue;
        }
        for file in &files {
            saved_membership
                .entry(crate::symbol_query::normalize_path(file))
                .or_default()
                .push(area.clone());
        }
        groups.insert(format!("area:{area}"),json!({"group_kind":"area","name":area,"responsibility":crate::symbol_query::preview(&summary,800),"description_status":"saved_requires_confirmation","description_source":"architecture_memory","key_files":files.into_iter().take(30).collect::<Vec<_>>(),"key_symbols":serde_json::from_str::<Vec<String>>(&symbols).unwrap_or_default().into_iter().take(30).collect::<Vec<_>>(),"updated_at_unix":updated,"entries":[],"matching_entries":0,"score":180}));
    }
    for language in languages {
        let args = json!({"workspace_root":root.to_string_lossy(),"query":request.query,"file_path":request.file_path,"directory":request.directory,"offset":request.offset,"limit":100,"detailed":true});
        let result: anyhow::Result<Value> = (|| {
            Ok(match language {
                "rust" => serde_json::to_value(crate::rust_index::search_symbols(
                    root,
                    serde_json::from_value(args)?,
                )?)?,
                "go" => serde_json::to_value(crate::go_index::search_symbols(
                    root,
                    serde_json::from_value(args)?,
                )?)?,
                "python" => serde_json::to_value(crate::python_index::search_symbols(
                    root,
                    serde_json::from_value(args)?,
                )?)?,
                _ => serde_json::to_value(crate::ts_index::search_symbols(
                    root,
                    serde_json::from_value(args)?,
                )?)?,
            })
        })();
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                issues.push(json!({"language":language,"error":error.to_string()}));
                continue;
            }
        };
        pages.push(json!({"language":language,"page":result["page"],"index":result["index"]}));
        let entries = result["matches"].as_array().cloned().unwrap_or_default();
        let files = entries
            .iter()
            .filter_map(|entry| entry["file_path"].as_str())
            .collect::<Vec<_>>();
        let catalog = crate::symbol_description::Catalog::load_for_files(root, language, &files)?;
        for mut entry in entries {
            let file = entry["file_path"].as_str().unwrap_or_default().to_owned();
            let explicit_area = entry
                .pointer("/description/area")
                .and_then(Value::as_str)
                .filter(|area| !area.is_empty())
                .map(str::to_owned);
            let remembered_area = saved_membership
                .get(&crate::symbol_query::normalize_path(&file))
                .filter(|areas| areas.len() == 1)
                .and_then(|areas| areas.first())
                .cloned();
            let selected_area = explicit_area.or(remembered_area);
            let area = selected_area.as_deref();
            let key = area
                .map(|area| format!("area:{area}"))
                .unwrap_or_else(|| format!("file:{language}:{file}"));
            let module = catalog.describe("", &file, "");
            let group=groups.entry(key).or_insert_with(||json!({"group_kind":if area.is_some(){"area"}else{"file"},"name":area.unwrap_or(&file),"responsibility":if area.is_some(){""}else{module.text.as_str()},"description_status":if area.is_some(){"missing"}else{module.status},"description_source":if area.is_some(){""}else{module.source.as_str()},"key_files":[],"key_symbols":[],"entries":[],"matching_entries":0,"score":0}));
            if let Some(files) = group["key_files"].as_array_mut() {
                if !files.iter().any(|item| item.as_str() == Some(&file)) && files.len() < 30 {
                    files.push(json!(file));
                }
            }
            let score = entry["score"].as_u64().unwrap_or(0);
            group["score"] = json!(score.max(group["score"].as_u64().unwrap_or(0)));
            group["matching_entries"] = json!(group["matching_entries"].as_u64().unwrap_or(0) + 1);
            entry["language"] = json!(language);
            entry["read_tool"] = json!(format!("read_{language}_symbol"));
            entry["read_arguments"] = json!({"symbol_id":entry["id"]});
            if language == "ts" {
                entry["read_arguments"]["include_outline"] = json!(true);
            }
            if let Some(entries) = group["entries"].as_array_mut() {
                entries.push(entry);
            }
        }
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    for group in &mut groups {
        if let Some(entries) = group["entries"].as_array_mut() {
            entries.sort_by(|a, b| {
                b["score"]
                    .as_u64()
                    .cmp(&a["score"].as_u64())
                    .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
            });
            entries.truncate(entry_limit);
        }
        group["entries_truncated"] =
            json!(group["matching_entries"].as_u64().unwrap_or(0) > entry_limit as u64);
    }
    groups.sort_by(|a, b| {
        b["score"]
            .as_u64()
            .cmp(&a["score"].as_u64())
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    let page = crate::symbol_query::page(
        groups.len(),
        request.group_offset,
        request.limit.clamp(1, 20),
    );
    let groups = groups
        .into_iter()
        .skip(page.offset)
        .take(page.limit)
        .collect::<Vec<_>>();
    Ok(
        json!({"query":request.query,"terms":terms,"groups":groups,"group_page":page,"language_pages":pages,"issues":issues,"candidate_window_per_language":100,"guidance":"Groups show saved responsibilities, not inferred call chains. Read a listed definition directly when implementation details are needed. Save a definition or file role with record_symbol_business_context; save a related cross-file area with record_architecture_memory. Missing/stale descriptions and zero matches do not prove missing functionality. offset pages each language's symbol candidates; group_offset pages groups within that candidate window."}),
    )
}
fn in_scope(file: &str, request: &Request) -> bool {
    let file = crate::symbol_query::normalize_path(file);
    if request
        .file_path
        .as_deref()
        .is_some_and(|path| file != crate::symbol_query::normalize_path(path))
    {
        return false;
    }
    !request.directory.as_deref().is_some_and(|directory| {
        let directory = crate::symbol_query::normalize_path(directory);
        !directory.is_empty() && directory != "." && !file.starts_with(&format!("{directory}/"))
    })
}
fn discover_languages(root: &Path) -> Vec<&'static str> {
    use std::{
        sync::{Mutex, OnceLock},
        time::{Duration, Instant},
    };
    static CACHE: OnceLock<Mutex<BTreeMap<std::path::PathBuf, (Instant, Vec<&'static str>)>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some((_, languages)) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(root)
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(2))
    {
        return languages.clone();
    }
    let mut languages = std::collections::BTreeSet::new();
    for path in crate::index_refresh::scan_source_mtimes(root).keys() {
        if let Some(language) = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(crate::symbol_description::language)
        {
            languages.insert(language);
        }
    }
    if let Ok(conn) = crate::database::init_db(root) {
        for language in ["rust", "ts", "python", "go"] {
            if crate::database::get_index_generated_at(&conn, &root.to_string_lossy(), language)
                .is_some()
            {
                languages.insert(language);
            }
        }
    }
    let languages = languages.into_iter().collect::<Vec<_>>();
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(2));
    cache.insert(root.to_owned(), (Instant::now(), languages.clone()));
    languages
}
