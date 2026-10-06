use rusqlite::types::Value;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    #[default]
    Any,
    All,
    Phrase,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ListOptions {
    pub directory: Option<String>,
    pub offset: usize,
    #[serde(default = "list_limit")]
    pub limit: usize,
    pub include_locals: bool,
    pub detailed: bool,
}
impl Default for ListOptions {
    fn default() -> Self {
        Self {
            directory: None,
            offset: 0,
            limit: 40,
            include_locals: false,
            detailed: false,
        }
    }
}
pub fn list_limit() -> usize {
    40
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct SearchOptions {
    pub file_path: Option<String>,
    pub directory: Option<String>,
    pub kind: Option<String>,
    pub offset: usize,
    pub match_mode: MatchMode,
    pub include_locals: bool,
    pub detailed: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ReadOptions {
    pub file_path: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PageInfo {
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    pub truncated: bool,
}
pub fn page(total: usize, offset: usize, limit: usize) -> PageInfo {
    let limit = limit.clamp(1, 100);
    let end = offset.saturating_add(limit).min(total);
    PageInfo {
        total,
        offset,
        limit,
        next_offset: (end < total).then_some(end),
        truncated: end < total || offset > 0,
    }
}
pub fn preview(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut value: String = line.chars().take(limit).collect();
    if line.chars().count() > limit {
        value.push('…');
    }
    value
}
pub fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    path.trim_start_matches("./")
        .trim_end_matches('/')
        .to_owned()
}
pub fn terms(query: &str, mode: MatchMode) -> Vec<String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    if matches!(mode, MatchMode::Phrase) {
        return vec![query];
    }
    let mut terms = Vec::new();
    for word in query.split(|ch: char| ch.is_whitespace() || matches!(ch, '|' | ',' | ';')) {
        if !word.is_empty() && !terms.iter().any(|term| term == word) {
            terms.push(word.to_owned());
        }
    }
    // Keep the literal phrase, then extract task vocabulary rather than splitting
    // arbitrary Chinese characters (which produces noisy one-character hits).
    const TASK_WORDS: &[(&str, &[&str])] = &[
        ("拖拽", &["拖动", "drag", "pointermove"]),
        ("删除", &["delete", "remove"]),
        ("渲染", &["render", "绘制", "draw"]),
        ("导出", &["export"]),
        ("导入", &["import"]),
        ("序列化", &["serialize"]),
        ("反序列化", &["deserialize"]),
        ("图片", &["image"]),
        ("文本", &["text"]),
        ("形状", &["shape"]),
        ("节点", &["node"]),
        ("缩放", &["scale", "zoom"]),
        ("旋转", &["rotate", "rotation"]),
        ("选择", &["select", "selection"]),
        ("撤销", &["undo"]),
        ("重做", &["redo"]),
        ("保存", &["save"]),
    ];
    // All means all user terms, not all synonyms. Expansion is for discovery only.
    if matches!(mode, MatchMode::Any) {
        for (word, aliases) in TASK_WORDS {
            if *word == "序列化" && query.contains("反序列化") {
                continue;
            }
            if query.contains(word)
                || aliases.iter().any(|alias| {
                    query
                        .split(|c: char| !c.is_alphanumeric())
                        .any(|part| part == *alias)
                })
            {
                for value in std::iter::once(*word).chain(aliases.iter().copied()) {
                    if !terms.iter().any(|term| term == value) {
                        terms.push(value.to_owned());
                    }
                }
            }
        }
        for word in query.split(|c: char| !c.is_alphanumeric()) {
            if word.len() >= 2 && !terms.iter().any(|term| term == word) {
                terms.push(word.to_owned());
            }
        }
    }
    terms.truncate(24);
    terms
}
pub fn score(
    name: &str,
    scope: &str,
    signature: &str,
    doc: &str,
    path: &str,
    terms: &[String],
    mode: MatchMode,
    description: &crate::symbol_description::Description,
) -> Option<(u32, Vec<String>)> {
    let name = name.to_lowercase();
    let scope = scope.to_lowercase();
    let signature = signature.to_lowercase();
    let doc = doc.to_lowercase();
    let path = path.to_lowercase();
    let semantic = description.searchable.to_lowercase();
    let mut matched = Vec::new();
    let mut score = 0;
    for term in terms {
        let weight = if name == *term {
            1000
        } else if name.starts_with(term) {
            400
        } else if name.contains(term) {
            200
        } else if scope.contains(term) {
            120
        } else if semantic.contains(term) {
            if description.status == "stale" {
                10
            } else if description.scope == "file" {
                90
            } else {
                180
            }
        } else if signature.contains(term) {
            60
        } else if doc.contains(term) {
            30
        } else if path.contains(term) {
            20
        } else {
            0
        };
        if weight > 0 {
            matched.push(term.clone());
            score += weight;
        }
    }
    if matched.is_empty() || (matches!(mode, MatchMode::All) && matched.len() != terms.len()) {
        return None;
    }
    score += matched.len() as u32 * 50;
    // Prefer implementation definitions over test helpers at the same relevance.
    if path.contains("/tests/")
        || path.ends_with(".test.ts")
        || scope == "tests"
        || scope.starts_with("tests::")
    {
        score = score * 3 / 4;
    }
    Some((score, matched))
}

#[derive(Default)]
pub struct Selection<'a> {
    pub file_path: Option<&'a str>,
    pub directory: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub symbol_id: Option<&'a str>,
    pub name: Option<&'a str>,
    pub terms: &'a [String],
    pub mode: MatchMode,
    pub include_locals: bool,
    pub include_relationships: bool,
    pub include_details: bool,
    pub pagination: Option<(usize, usize)>,
}
impl Selection<'_> {
    pub fn count(&self, root: &Path, table: &str) -> rusqlite::Result<usize> {
        // Outline pages have no search terms; search ranking counts its filtered candidates.
        debug_assert!(self.terms.is_empty());
        let conn = crate::database::init_db(root)?;
        let (where_sql, params) = self.sql(root, table, &[]);
        let where_sql = where_sql
            .strip_suffix(" ORDER BY file_path,start_line,id")
            .unwrap_or(&where_sql);
        conn.query_row(
            &format!("SELECT count(*) FROM {table}{where_sql}"),
            rusqlite::params_from_iter(params.iter()),
            |row| row.get::<_, usize>(0),
        )
    }
    pub fn sql(&self, root: &Path, table: &str, columns: &[&str]) -> (String, Vec<Value>) {
        let mut sql = " WHERE workspace_root = ?1".to_owned();
        let mut params = vec![Value::Text(root.to_string_lossy().into_owned())];
        let mut add = |column: &str, value: String| {
            params.push(Value::Text(value));
            sql.push_str(&format!(" AND {column} = ?{}", params.len()));
        };
        if let Some(path) = self.file_path {
            add("file_path", normalize_path(path));
        }
        if let Some(kind) = self.kind {
            add("kind", kind.to_owned());
        }
        if let Some(id) = self.symbol_id {
            add("id", id.to_owned());
        }
        // Exact unqualified names are filtered in SQLite. Qualified names are
        // resolved against the small file-scoped candidate set by the reader.
        if let Some(name) = self
            .name
            .filter(|name| !name.contains('.') && !name.contains("::"))
        {
            add("name", name.to_owned());
        }
        if let Some(directory) = self.directory {
            let directory = normalize_path(directory);
            if !directory.is_empty() && directory != "." {
                params.push(Value::Text(format!("{directory}/")));
                sql.push_str(&format!(
                    " AND substr(file_path,1,length(?{0})) = ?{0}",
                    params.len()
                ));
            }
        }
        if table == "ts_symbols" && !self.include_locals {
            sql.push_str(" AND (parent_id IS NULL OR parent_id NOT IN (WITH RECURSIVE local_scopes(id) AS (SELECT id FROM ts_symbols WHERE workspace_root=?1 AND kind IN ('function','arrow_function','method') UNION SELECT child.id FROM ts_symbols child JOIN local_scopes parent ON child.parent_id=parent.id WHERE child.workspace_root=?1) SELECT id FROM local_scopes))" );
        }
        if !self.terms.is_empty() {
            let mut groups = Vec::new();
            for term in self.terms {
                params.push(Value::Text(term.clone()));
                let index = params.len();
                let mut conditions = columns
                    .iter()
                    .map(|column| format!("instr(lower(coalesce({column},'')),?{index})>0"))
                    .collect::<Vec<_>>();
                conditions.push(crate::symbol_description::search_predicate(table, index));
                groups.push(format!("({})", conditions.join(" OR ")));
            }
            let join = if matches!(self.mode, MatchMode::All) {
                " AND "
            } else {
                " OR "
            };
            sql.push_str(&format!(" AND ({})", groups.join(join)));
        }
        sql.push_str(" ORDER BY file_path,start_line,id");
        (sql, params)
    }
}

pub fn name_matches(name: &str, scope: &str, requested: &str) -> bool {
    if name == requested {
        return true;
    }
    let dotted = format!("{scope}.{name}");
    let rust = format!("{scope}::{name}");
    dotted == requested
        || rust == requested
        || dotted.ends_with(&format!(".{requested}"))
        || rust.ends_with(&format!("::{requested}"))
}
pub fn source_range(content: &str, start: usize, end: usize) -> std::io::Result<String> {
    let offsets = crate::source_read::line_offsets(content);
    if start == 0 || end < start || end >= offsets.len() {
        return Err(std::io::Error::other(
            "indexed source range is no longer valid; refresh the index",
        ));
    }
    Ok(content[offsets[start - 1]..offsets[end]].to_owned())
}

/// Rank only the prefix needed for this page; preserve the exact candidate count.
pub fn rank_page<T>(
    values: &mut Vec<T>,
    offset: usize,
    limit: usize,
    compare: impl Fn(&T, &T) -> std::cmp::Ordering,
) -> PageInfo {
    let page = page(values.len(), offset, limit);
    let end = page.offset.saturating_add(page.limit).min(values.len());
    if end == 0 || page.offset >= values.len() {
        values.clear();
        return page;
    }
    if end < values.len() {
        values.select_nth_unstable_by(end - 1, &compare);
        values.truncate(end);
    }
    values.sort_by(compare);
    page
}
