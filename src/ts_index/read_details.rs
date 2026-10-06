//! Bounded editing context, separate from optional call-chain navigation.
use std::{collections::{BTreeMap, BTreeSet, VecDeque}, path::Path};
use serde_json::{Value, json};
use crate::ts_index::*;

const TYPE_CHARS: usize = 12_000;
const TYPE_COUNT: usize = 8;

fn identifiers(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !(ch.is_alphanumeric() || matches!(ch, '_' | '$')))
        .filter(|part| !part.is_empty()).map(str::to_owned).collect()
}

fn file_symbols(root: &Path, file: &str) -> Result<Vec<TsSymbol>> {
    if crate::source_read::needs_index_refresh(root, "ts", file) {
        maybe_reindex_after_write(root, &root.join(file))?;
    }
    load_selected_symbols(root, &crate::symbol_query::Selection {
        file_path: Some(file), include_locals: true, include_details: true,
        include_relationships: true, ..Default::default()
    })
}

fn is_type(symbol: &TsSymbol) -> bool {
    matches!(symbol.kind, TsSymbolKind::Interface | TsSymbolKind::TypeAlias | TsSymbolKind::Enum)
}

pub(super) fn collect(root: &Path, primary: &TsSymbol, content: &str, types: bool, outline: bool)
    -> (Vec<Value>, Vec<Value>, Option<Value>)
{
    if !types && !outline { return (vec![], vec![], None); }
    let mut issues = Vec::new();
    let symbols = match file_symbols(root, &primary.file_path) {
        Ok(symbols) => symbols,
        Err(error) => return (vec![], vec![json!({"path":primary.file_path,"error":error.to_string()})], None),
    };
    let edit_context = outline.then(|| {
        let parent = primary.parent_id.as_ref().and_then(|id| symbols.iter().find(|item| &item.id == id));
        let mut neighbors = symbols.iter().filter(|item| item.id != primary.id && item.parent_id == primary.parent_id)
            .collect::<Vec<_>>();
        neighbors.sort_by_key(|item| item.start_line.abs_diff(primary.start_line));
        neighbors.truncate(6);
        neighbors.sort_by_key(|item| item.start_line);
        let summary = |item: &TsSymbol| json!({"id":item.id,"name":item.name,"kind":item.kind,
            "file_path":item.file_path,"start_line":item.start_line,"end_line":item.end_line,
            "signature":crate::symbol_query::preview(&item.signature,240)});
        json!({"parent":parent.map(summary),"neighbors":neighbors.into_iter().map(summary).collect::<Vec<_>>(),
            "scope":"nearest six definitions in the same lexical scope; positions, not source bodies"})
    });
    if !types { return (vec![], issues, edit_context); }
    let canonical_root = match root.canonicalize() { Ok(root) => root, Err(_) => return (vec![], issues, edit_context) };
    let mut files = BTreeMap::from([(primary.file_path.clone(), symbols)]);
    let mut queue = VecDeque::from([(primary.clone(), content.to_owned(), 0usize)]);
    let mut seen = BTreeSet::from([primary.id.clone()]);
    let mut pages = Vec::new();
    let mut used = 0;
    while let Some((owner, body, depth)) = queue.pop_front() {
        let names = identifiers(&body);
        let mut candidates = files.get(&owner.file_path).into_iter().flatten()
            .filter(|item| is_type(item) && names.contains(&item.name) &&
                (item.scope_path.is_empty() || item.scope_path == owner.scope_path))
            .cloned().collect::<Vec<_>>();
        for import in owner.import_bindings.iter().filter(|import| names.contains(&import.local_name)
            && import.source.starts_with('.') && matches!(import.kind, TsImportKind::Named | TsImportKind::Default)).take(16) {
            let base = Path::new(&owner.file_path).parent().unwrap_or(Path::new("")).join(&import.source);
            let target = import_path_candidates(&base.to_string_lossy()).into_iter().find_map(|file| {
                let path = root.join(file).canonicalize().ok()?;
                if !path.is_file() { return None; }
                path.strip_prefix(&canonical_root).ok().map(|relative| normalize_slashes(&relative.to_string_lossy()))
            });
            let Some(target) = target else {
                issues.push(json!({"name":import.local_name,"source":import.source,"status":"local import not resolved"}));
                continue;
            };
            if !files.contains_key(&target) {
                if files.len() >= 20 {
                    issues.push(json!({"path":target,"status":"not inspected: bounded related-type file count"}));
                    continue;
                }
                match file_symbols(root, &target) {
                    Ok(symbols) => { files.insert(target.clone(), symbols); },
                    Err(error) => {issues.push(json!({"path":target,"error":error.to_string()}));continue;},
                }
            }
            let matches = files[&target].iter().filter(|item| is_type(item)
                && exported_names(item).contains(&import.imported_name)).cloned().collect::<Vec<_>>();
            if matches.is_empty() {
                issues.push(json!({"name":import.local_name,"path":target,"status":"no direct type export; re-exports and namespace imports are not expanded"}));
            }
            candidates.extend(matches);
        }
        candidates.sort_by(|a,b| (&a.file_path,a.start_line,&a.id).cmp(&(&b.file_path,b.start_line,&b.id)));
        for candidate in candidates {
            if !seen.insert(candidate.id.clone()) { continue; }
            let result = (|| -> Result<Value> {
                let snapshot = crate::source_read::read(&root.join(&candidate.file_path),crate::source_read::FILE_LIMIT)?;
                if crate::source_read::indexed_hash(root,"ts",&candidate.file_path).as_deref() != Some(snapshot.hash.as_str()) {
                    return Err(std::io::Error::other("source_version_conflict: related type changed during read").into());
                }
                let code = snapshot.range(candidate.start_line,candidate.end_line)?;
                Ok(json!({"symbol":{"id":candidate.id,"name":candidate.name,"file_path":candidate.file_path,
                    "start_line":candidate.start_line,"end_line":candidate.end_line},"path":candidate.file_path,
                    "code_hash":snapshot.hash,"start_line":candidate.start_line,"end_line":candidate.end_line,
                    "content":code,"complete":true,"returned_chars":code.chars().count(),"available":true,
                    "reason":"referenced local type identifier; heuristic, not type checked"}))
            })();
            match result {
                Ok(page) => {
                    let size = page["returned_chars"].as_u64().unwrap_or(0) as usize;
                    if pages.len() >= TYPE_COUNT || used + size > TYPE_CHARS {
                        issues.push(json!({"symbol_id":candidate.id,"path":candidate.file_path,
                            "start_line":candidate.start_line,"end_line":candidate.end_line,"status":"not included: bounded related-type budget"}));
                        continue;
                    }
                    used += size;
                    if depth < 1 {queue.push_back((candidate,page["content"].as_str().unwrap_or("").to_owned(),depth+1));}
                    pages.push(page);
                },
                Err(error) => issues.push(json!({"symbol_id":candidate.id,"error":error.to_string()})),
            }
        }
    }
    (pages, issues.into_iter().take(12).collect(), edit_context)
}
