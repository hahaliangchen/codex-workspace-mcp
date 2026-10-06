//! Versioned, bounded source snapshots. Display pages never redefine the saved source.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

pub const FILE_LIMIT: usize = 16 * 1024 * 1024;
const CACHE_LIMIT: usize = 64 * 1024 * 1024;
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}
fn stamp(meta: &fs::Metadata) -> Stamp {
    Stamp {
        size: meta.len(),
        modified: meta.modified().ok(),
        created: meta.created().ok(),
    }
}
pub struct Snapshot {
    pub content: String,
    pub hash: String,
    pub offsets: Vec<usize>,
    stamp: Stamp,
}
impl Snapshot {
    pub fn line_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }
    pub fn range(&self, start: usize, end: usize) -> io::Result<&str> {
        if start == 0 || end < start || end > self.line_count() {
            return Err(io::Error::other(format!(
                "source_range_out_of_bounds: requested {start}-{end}, total_lines={}",
                self.line_count()
            )));
        }
        Ok(&self.content[self.offsets[start - 1]..self.offsets[end]])
    }
}
pub fn line_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    if offsets.last().copied() != Some(content.len()) {
        offsets.push(content.len());
    }
    offsets
}
struct Cache {
    entries: BTreeMap<PathBuf, (Arc<Snapshot>, u64)>,
    bytes: usize,
    tick: u64,
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            entries: BTreeMap::new(),
            bytes: 0,
            tick: 0,
        })
    })
}
fn cost(snapshot: &Snapshot) -> usize {
    snapshot.content.len() + snapshot.offsets.len() * std::mem::size_of::<usize>()
}
pub fn invalidate(path: &Path) {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut cache = cache().lock().unwrap_or_else(|error| error.into_inner());
    if let Some((old, _)) = cache.entries.remove(&path) {
        cache.bytes = cache.bytes.saturating_sub(cost(&old));
    }
}
pub fn clear() {
    let mut cache = cache().lock().unwrap_or_else(|error| error.into_inner());
    cache.entries.clear();
    cache.bytes = 0;
}
pub fn read(path: &Path, limit: usize) -> io::Result<Arc<Snapshot>> {
    let path = path.canonicalize()?;
    let limit = limit.min(FILE_LIMIT);
    let before = fs::metadata(&path)?;
    if !before.is_file() {
        return Err(io::Error::other("source path is not a regular file"));
    }
    if before.len() > limit as u64 {
        return Err(io::Error::other(format!(
            "source_file_too_large: {} bytes exceeds {limit}; use a bounded external reader for larger files",
            before.len()
        )));
    }
    {
        let mut cache = cache().lock().unwrap_or_else(|error| error.into_inner());
        cache.tick = cache.tick.wrapping_add(1);
        let tick = cache.tick;
        if let Some((snapshot, used)) = cache
            .entries
            .get_mut(&path)
            .filter(|(snapshot, _)| before.modified().is_ok() && snapshot.stamp == stamp(&before))
        {
            *used = tick;
            return Ok(snapshot.clone());
        }
    }
    for _ in 0..2 {
        let file = fs::File::open(&path)?;
        let before = file.metadata()?;
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(io::Error::other(format!(
                "source_file_too_large: exceeds {limit} bytes during read"
            )));
        }
        let after = fs::metadata(&path)?;
        if stamp(&before) != stamp(&after) {
            continue;
        }
        let content = String::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let snapshot = Arc::new(Snapshot {
            hash: crate::symbol_description::content_hash(content.as_bytes()),
            offsets: line_offsets(&content),
            content,
            stamp: stamp(&after),
        });
        let mut cache = cache().lock().unwrap_or_else(|error| error.into_inner());
        if let Some((old, _)) = cache.entries.remove(&path) {
            cache.bytes = cache.bytes.saturating_sub(cost(&old));
        }
        if cost(&snapshot) <= CACHE_LIMIT {
            while cache.bytes + cost(&snapshot) > CACHE_LIMIT || cache.entries.len() >= 128 {
                let Some(key) = cache
                    .entries
                    .iter()
                    .min_by_key(|(_, (_, used))| *used)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                if let Some((old, _)) = cache.entries.remove(&key) {
                    cache.bytes = cache.bytes.saturating_sub(cost(&old));
                }
            }
            cache.tick = cache.tick.wrapping_add(1);
            let tick = cache.tick;
            cache.bytes += cost(&snapshot);
            cache.entries.insert(path.clone(), (snapshot.clone(), tick));
        }
        return Ok(snapshot);
    }
    Err(io::Error::other(
        "source_version_conflict: file changed while reading; retry the focused read",
    ))
}

pub fn indexed_hash(root: &Path, language: &str, file: &str) -> Option<String> {
    crate::database::init_db(root).ok()?.query_row("SELECT content_hash FROM symbol_index_files WHERE workspace_root=?1 AND language=?2 AND file_path=?3",rusqlite::params![root.to_string_lossy(),language,file],|row|row.get(0)).ok()
}
pub fn needs_index_refresh(root: &Path, language: &str, file: &str) -> bool {
    read(&root.join(file), FILE_LIMIT).is_ok_and(|snapshot| {
        indexed_hash(root, language, file).as_deref() != Some(snapshot.hash.as_str())
    })
}

/// Full-line display page, preserving CRLF and the final delimiter. The
/// caller keeps original requested bounds for notebook material capture.
pub fn page(result: &mut Value, args: &Value) -> io::Result<()> {
    let Some(content) = result["content"].as_str().map(str::to_owned) else {
        return Ok(());
    };
    let origin = result
        .get("start_line")
        .or_else(|| result.pointer("/symbol/start_line"))
        .and_then(Value::as_u64)
        .unwrap_or(1) as usize;
    let offsets = line_offsets(&content);
    let count = offsets.len().saturating_sub(1);
    let material_end = origin + count.saturating_sub(1);
    let from = if result.get("lines").is_none() {
        args["start_line"].as_u64().unwrap_or(origin as u64) as usize
    } else {
        origin
    };
    let until = if result.get("lines").is_none() {
        args["end_line"].as_u64().unwrap_or(material_end as u64) as usize
    } else {
        material_end
    };
    if count > 0 && (from < origin || from > material_end || until < from) {
        return Err(io::Error::other(format!(
            "source_range_out_of_bounds: material covers {origin}-{material_end}"
        )));
    }
    let until = until.min(material_end);
    let budget = args["max_chars"]
        .as_u64()
        .unwrap_or(24_000)
        .clamp(1000, 60_000) as usize;
    if args["start_line"].as_u64() == Some(0) || args["end_line"].as_u64() == Some(0) {
        return Err(io::Error::other("line numbers start at 1"));
    }
    let column = args["start_column"].as_u64().unwrap_or(1) as usize;
    if column == 0 {
        return Err(io::Error::other("start_column starts at 1"));
    }
    if count > 0 {
        let line = &content[offsets[from - origin]..offsets[from - origin + 1]];
        let line_chars = line.chars().count();
        if column > line_chars + 1 {
            return Err(io::Error::other("start_column is outside this line"));
        }
        if column > 1 || line_chars > budget {
            let fragment = line
                .chars()
                .skip(column - 1)
                .take(budget)
                .collect::<String>();
            let next_column = column + fragment.chars().count();
            let line_complete = next_column > line_chars;
            result["material_start_line"] = json!(origin);
            result["material_end_line"] = json!(material_end);
            result["content"] = json!(fragment);
            result["start_line"] = json!(from);
            result["end_line"] = json!(from);
            result["start_column"] = json!(column);
            result["partial_line"] = json!(true);
            result["returned_chars"] = json!(next_column - column);
            result["complete"] = json!(line_complete && from == until);
            result["next_start_line"] = if !line_complete {
                json!(from)
            } else if from < until {
                json!(from + 1)
            } else {
                Value::Null
            };
            result["next_column"] = if line_complete {
                Value::Null
            } else {
                json!(next_column)
            };
            result["source_format"] = json!({"byte_exact":true,"fragment":true,"column_unit":"Unicode characters, including line delimiters"});
            if result.get("lines").is_some() {
                result["lines"] = json!([]);
            }
            return Ok(());
        }
    }
    let mut end = from.saturating_sub(1);
    let mut chars = 0;
    if count > 0 {
        for line in from..=until {
            let part = &content[offsets[line - origin]..offsets[line - origin + 1]];
            let size = part.chars().count();
            if chars + size > budget {
                break;
            }
            chars += size;
            end = line;
        }
    }
    if count > 0 && end < from {
        return Err(io::Error::other(
            "source_line_exceeds_page_budget: raise max_chars up to 60000 or use another focused range",
        ));
    }
    let page = if count == 0 {
        String::new()
    } else {
        content[offsets[from - origin]..offsets[end - origin + 1]].to_owned()
    };
    result["material_start_line"] = json!(origin);
    result["material_end_line"] = json!(if count == 0 { 0 } else { material_end });
    result["start_line"] = json!(from);
    result["end_line"] = json!(end);
    result["complete"] = json!(count == 0 || end >= until);
    result["next_start_line"] = if end >= until || count == 0 {
        Value::Null
    } else {
        json!(end + 1)
    };
    result["returned_chars"] = json!(chars);
    result["partial_line"] = json!(false);
    result["start_column"] = json!(1);
    result["next_column"] = Value::Null;
    result["content"] = json!(page);
    if result.get("lines").is_some() {
        result["lines"] = json!(
            result["content"]
                .as_str()
                .unwrap_or("")
                .lines()
                .enumerate()
                .map(|(index, text)| json!({"line":from+index,"text":text}))
                .collect::<Vec<_>>()
        );
    }
    result["source_format"] = json!({"byte_exact":true,"newline":if content.contains("\r\n"){"CRLF"}else{"LF"},"ends_with_newline":content.ends_with('\n')});
    Ok(())
}

/// Keep a valid JSON envelope even if an unusually large tool result needs
/// abbreviation. The original is retained in the task's tool/result event.
pub fn bounded(result: &Value, limit: usize, call_id: &str) -> String {
    let raw = result.to_string();
    if raw.chars().count() <= limit {
        return raw;
    }
    json!({"truncated":true,"original_chars":raw.chars().count(),"tool_call_id":call_id,"notebook_material":result["notebook_material"],
        "path":result.get("path").or_else(||result.pointer("/symbol/file_path")),"code_hash":result.get("code_hash").or_else(||result.pointer("/description/code_hash")),"start_line":result["start_line"],"end_line":result["end_line"],
        "complete":false,"next_start_line":result["next_start_line"],"error":result["error"],"excerpt":raw.chars().take(limit/4).collect::<String>(),
        "retrieve":"Use recall_work with tool_call_ids to recover the original tool result, or include_material=true with the notebook material id for exact source. Excerpt is abbreviated JSON text, not complete source."}).to_string()
}
pub fn query_fingerprint(result: &Value) -> String {
    let mut value = result.clone();
    if let Some(index) = value.get_mut("index").and_then(Value::as_object_mut) {
        index.remove("checked_at_unix");
        index.remove("changed_files_last_check");
        index.remove("removed_files_last_check");
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("generated_at_unix");
    }
    crate::symbol_description::content_hash(value.to_string().as_bytes())
}
pub fn result_page(value: &Value, args: &Value) -> io::Result<Value> {
    let budget = args["max_chars"]
        .as_u64()
        .unwrap_or(24_000)
        .clamp(1000, 60_000) as usize;
    let offset = args["result_offset"].as_u64().unwrap_or(0) as usize;
    let explicit = args["result_field"].as_str();
    if explicit.is_none() && value.to_string().chars().count() <= budget {
        return Ok(json!({"result":value,"complete":true,"next_offset":null}));
    }
    let automatic = value.as_object().and_then(|object| {
        object
            .iter()
            .filter(|(_, value)| value.is_array())
            .max_by_key(|(_, value)| value.to_string().len())
            .map(|(key, _)| key.as_str())
    });
    let Some(field) = explicit.or(automatic) else {
        return Ok(
            json!({"complete":false,"available_fields":value.as_object().map(|object|object.keys().collect::<Vec<_>>()),"guidance":"Choose result_field (field name or JSON pointer) to retrieve a bounded part of this historical result."}),
        );
    };
    let selected = if field.starts_with('/') {
        value.pointer(field)
    } else {
        value.get(field)
    }
    .ok_or_else(|| io::Error::other("result_field not found"))?;
    if let Some(items) = selected.as_array() {
        if offset > items.len() {
            return Err(io::Error::other("result_offset is outside the array"));
        }
        let mut page = Vec::new();
        let mut chars = 0;
        for item in &items[offset..] {
            let size = item.to_string().chars().count() + 1;
            if chars + size > budget {
                break;
            }
            chars += size;
            page.push(item.clone());
        }
        if page.is_empty() && offset < items.len() {
            return Ok(
                json!({"result_field":field,"complete":false,"guidance":"The item exceeds this page budget. Select a narrower JSON pointer, such as /matches/0/text."}),
            );
        }
        let next = offset + page.len();
        return Ok(
            json!({"result_field":field,"result":page,"offset":offset,"total_items":items.len(),"complete":next==items.len(),"next_offset":if next<items.len(){Some(next)}else{None},"omitted_fields":value.as_object().map(|object|object.keys().filter(|key|key.as_str()!=field).collect::<Vec<_>>())}),
        );
    }
    if let Some(text) = selected.as_str() {
        let total = text.chars().count();
        if offset > total {
            return Err(io::Error::other("result_offset is outside the string"));
        }
        let piece = text.chars().skip(offset).take(budget).collect::<String>();
        let next = offset + piece.chars().count();
        return Ok(
            json!({"result_field":field,"result":piece,"offset":offset,"offset_unit":"Unicode characters","total_chars":total,"complete":next==total,"next_offset":if next<total{Some(next)}else{None}}),
        );
    }
    if selected.to_string().chars().count() > budget {
        return Ok(
            json!({"result_field":field,"complete":false,"guidance":"Select a narrower JSON pointer for this object."}),
        );
    }
    Ok(json!({"result_field":field,"result":selected,"complete":true,"next_offset":null}))
}
