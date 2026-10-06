//! Reusable code responsibilities attached to live definitions and source versions.
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, OnceLock},
};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RecordOptions {
    pub qualified_name: String,
    pub keywords: Vec<String>,
    pub scope: String,
    pub source: String,
    pub expected_code_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Description {
    pub qualified_name: String,
    pub text: String,
    pub status: &'static str,
    pub source: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub area: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub read_when: String,
    pub code_hash: String,
    pub definition_hash: String,
    #[serde(skip)]
    pub searchable: String,
}

static SCHEMA_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
pub fn schema(conn: &Connection) -> rusqlite::Result<()> {
    let _guard = SCHEMA_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (table, columns) in [
        (
            "symbol_business_contexts",
            vec![
                ("qualified_name", "TEXT NOT NULL DEFAULT ''"),
                ("keywords", "TEXT NOT NULL DEFAULT '[]'"),
                ("scope", "TEXT NOT NULL DEFAULT 'symbol'"),
                ("source", "TEXT NOT NULL DEFAULT 'legacy'"),
                ("code_hash", "TEXT NOT NULL DEFAULT ''"),
                ("definition_hash", "TEXT NOT NULL DEFAULT ''"),
                ("stale", "INTEGER NOT NULL DEFAULT 1"),
            ],
        ),
        (
            "symbol_index_files",
            vec![("content_hash", "TEXT NOT NULL DEFAULT ''")],
        ),
    ] {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (name, declaration) in columns {
            if !names.iter().any(|existing| existing == name) {
                conn.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {name} {declaration}"
                ))?;
            }
        }
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_symbol_context_location ON symbol_business_contexts(workspace_root,language,file_path,qualified_name);
        CREATE INDEX IF NOT EXISTS idx_symbol_context_file ON symbol_business_contexts(workspace_root,file_path);")
}

pub fn content_hash(bytes: &[u8]) -> String {
    // A deterministic change detector, not a security digest.
    let value = bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("fnv1a64:{value:016x}")
}

pub fn language(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some("rust"),
        "ts" | "typescript" | "javascript" | "js" | "tsx" | "jsx" | "mjs" | "cjs" => Some("ts"),
        "python" | "py" => Some("python"),
        "go" | "golang" => Some("go"),
        _ => None,
    }
}

pub fn definition_sql(language: &str) -> (&'static str, &'static str) {
    match language {
        "rust" => (
            "rust_symbols",
            "CASE WHEN module_path='' THEN coalesce(impl_type,'') WHEN coalesce(impl_type,'')='' THEN module_path ELSE module_path||'::'||impl_type END",
        ),
        "go" => (
            "go_symbols",
            "CASE WHEN coalesce(receiver_type,'')='' THEN package_name ELSE package_name||'.'||receiver_type END",
        ),
        "python" => ("python_symbols", "coalesce(class_name,'')"),
        _ => ("ts_symbols", "scope_path"),
    }
}

pub fn qualified_name(language: &str, scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!(
            "{scope}{}{name}",
            if language == "rust" { "::" } else { "." }
        )
    }
}

/// SQL expression referring to the outer symbol table. Used by every language search.
pub fn search_predicate(table: &str, parameter: usize) -> String {
    let lang = table.trim_end_matches("_symbols");
    let (_, scope) = definition_sql(lang);
    let scope = match lang {
        "rust" => scope
            .replace("module_path", "rust_symbols.module_path")
            .replace("impl_type", "rust_symbols.impl_type"),
        "go" => scope
            .replace("package_name", "go_symbols.package_name")
            .replace("receiver_type", "go_symbols.receiver_type"),
        "python" => scope.replace("class_name", "python_symbols.class_name"),
        _ => "ts_symbols.scope_path".to_owned(),
    };
    let separator = if lang == "rust" { "::" } else { "." };
    let qualified = format!(
        "CASE WHEN ({scope})='' THEN {table}.name ELSE ({scope})||'{separator}'||{table}.name END"
    );
    let fields = [
        "business_role",
        "keywords",
        "belongs_to_area",
        "common_tasks",
        "read_when",
    ];
    format!(
        "EXISTS (SELECT 1 FROM symbol_business_contexts ctx WHERE ctx.workspace_root=?1 AND ctx.language='{lang}' AND ctx.file_path={table}.file_path AND (ctx.symbol_id={table}.id OR (ctx.qualified_name<>'' AND ctx.qualified_name=({qualified})) OR ctx.scope='file') AND ({}))",
        fields
            .iter()
            .map(|field| format!("instr(lower(ctx.{field}),?{parameter})>0"))
            .collect::<Vec<_>>()
            .join(" OR ")
    )
}

#[derive(Debug, Clone)]
pub struct Located {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub language: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
}

pub fn locate(
    conn: &Connection,
    root: &Path,
    lang: &str,
    id: &str,
    file: &str,
    name: &str,
) -> rusqlite::Result<Vec<Located>> {
    let (table, scope) = definition_sql(lang);
    let mut sql = format!(
        "SELECT id,name,file_path,{scope},start_line,end_line FROM {table} WHERE workspace_root=?1"
    );
    let mut values = vec![rusqlite::types::Value::Text(
        root.to_string_lossy().into_owned(),
    )];
    if !id.is_empty() {
        values.push(id.to_owned().into());
        sql.push_str(" AND id=?2");
    } else {
        values.push(file.to_owned().into());
        sql.push_str(" AND file_path=?2");
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            let name: String = row.get(1)?;
            Ok(Located {
                id: row.get(0)?,
                qualified_name: qualified_name(lang, &row.get::<_, String>(3)?, &name),
                name,
                language: lang.to_owned(),
                file_path: row.get(2)?,
                start_line: row.get(4)?,
                end_line: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|symbol| {
            !id.is_empty()
                || symbol.name == name
                || symbol.qualified_name == name
                || symbol.qualified_name.ends_with(&format!(
                    "{}{}",
                    if lang == "rust" { "::" } else { "." },
                    name
                ))
        })
        .collect())
}

/// Populate comments without replacing a responsibility learned by a Worker/Observer.
pub fn synchronize_file(
    conn: &Connection,
    root: &Path,
    lang: &str,
    file: &str,
) -> rusqlite::Result<()> {
    conn.execute("UPDATE symbol_business_contexts SET stale=1 WHERE workspace_root=?1 AND language=?2 AND file_path=?3",params![root.to_string_lossy(),lang,file])?;
    let tracked=conn.query_row("SELECT content_hash,status FROM symbol_index_files WHERE workspace_root=?1 AND language=?2 AND file_path=?3",params![root.to_string_lossy(),lang,file],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional()?;
    let Some((hash, status)) =
        tracked.filter(|(hash, status)| !hash.is_empty() && status == "indexed")
    else {
        return Ok(());
    };
    let _ = status;
    let snapshot = crate::source_read::read(&root.join(file), 2 * 1024 * 1024)
        .ok()
        .filter(|snapshot| snapshot.hash == hash);
    let Some(snapshot) = snapshot else {
        return Ok(());
    };
    conn.execute("UPDATE symbol_business_contexts SET stale=0 WHERE workspace_root=?1 AND language=?2 AND file_path=?3 AND scope='file' AND code_hash=?4",params![root.to_string_lossy(),lang,file,hash])?;
    let (table, scope) = definition_sql(lang);
    let mut stmt=conn.prepare(&format!("SELECT id,name,{scope},docstring,start_line,end_line FROM {table} WHERE workspace_root=?1 AND file_path=?2 ORDER BY start_line,id"))?;
    let symbols = stmt
        .query_map(params![root.to_string_lossy(), file], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, usize>(4)?,
                row.get::<_, usize>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut multiplicity = HashMap::<String, usize>::new();
    for (_, name, scope, _, _, _) in &symbols {
        *multiplicity
            .entry(qualified_name(lang, scope, name))
            .or_default() += 1;
    }
    for (id, name, scope, doc, start, end) in symbols {
        let definition_hash = snapshot
            .range(start, end)
            .map(|source| content_hash(source.as_bytes()))
            .unwrap_or_default();
        let qualified = qualified_name(lang, &scope, &name);
        // A responsibility survives unrelated edits and moved line numbers only
        // when its exact definition bytes still match. File hashes remain the
        // edit precondition exposed to callers.
        if !definition_hash.is_empty() {
            let identity = if multiplicity[&qualified] == 1 {
                "(qualified_name=?5 OR (qualified_name='' AND symbol_name=?4))"
            } else {
                "symbol_id=?8"
            };
            conn.execute(&format!("UPDATE symbol_business_contexts SET symbol_name=?4,qualified_name=?5,stale=CASE WHEN definition_hash=?7 OR (definition_hash='' AND code_hash=?6) THEN 0 ELSE 1 END,code_hash=CASE WHEN definition_hash=?7 OR (definition_hash='' AND code_hash=?6) THEN ?6 ELSE code_hash END,definition_hash=CASE WHEN definition_hash='' AND code_hash=?6 THEN ?7 ELSE definition_hash END WHERE workspace_root=?1 AND language=?2 AND file_path=?3 AND scope='symbol' AND {identity} AND ?8<>''"),params![root.to_string_lossy(),lang,file,name,qualified,hash,definition_hash,id])?;
        }
        let description_identity = if multiplicity[&qualified] == 1 {
            "(symbol_id=?4 OR qualified_name=?5)"
        } else {
            "symbol_id=?4 AND ?5<>''"
        };
        let existing=conn.query_row( &format!("SELECT id,source FROM symbol_business_contexts WHERE workspace_root=?1 AND language=?2 AND file_path=?3 AND scope='symbol' AND {description_identity} ORDER BY CASE WHEN source='comment' THEN 1 ELSE 0 END,updated_at_unix DESC LIMIT 1"),params![root.to_string_lossy(),lang,file,id,qualified],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?))).optional()?;
        let text = comment_summary(&doc);
        match existing {
            Some((row_id, source)) if source == "comment" => {
                if text.is_empty() {
                    conn.execute("DELETE FROM symbol_business_contexts WHERE id=?1", [row_id])?;
                } else {
                    conn.execute("UPDATE symbol_business_contexts SET business_role=?2,code_hash=?3,stale=0,qualified_name=?4,symbol_name=?5,definition_hash=?6 WHERE id=?1",params![row_id,text,hash,qualified,name,definition_hash])?;
                }
            }
            Some(_) => {}
            None if !text.is_empty() => {
                conn.execute("INSERT INTO symbol_business_contexts(workspace_root,symbol_id,symbol_name,language,file_path,belongs_to_area,business_role,common_tasks,read_when,avoid_when,risks,confidence,updated_at_unix,qualified_name,keywords,scope,source,code_hash,stale) VALUES (?1,?2,?3,?4,?5,'',?6,'[]','','','',0.5,?7,?8,'[]','symbol','comment',?9,0)",params![root.to_string_lossy(),id,name,lang,file,text,crate::rust_index::now_unix() as i64,qualified,hash])?;
            }
            _ => {}
        }
        conn.execute("UPDATE symbol_business_contexts SET definition_hash=?4 WHERE workspace_root=?1 AND language=?2 AND symbol_id=?3 AND source='comment' AND stale=0",params![root.to_string_lossy(),lang,id,definition_hash])?;
    }
    Ok(())
}

fn comment_summary(doc: &str) -> String {
    let mut lines = Vec::new();
    for line in doc.lines() {
        let line = line.trim().trim_start_matches('*').trim();
        if line.starts_with('@') || line.starts_with("# Examples") || line.starts_with("```") {
            break;
        }
        if line.is_empty() && !lines.is_empty() {
            break;
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    crate::symbol_query::preview(&lines.join(" "), 240)
}

pub struct PreparedRecord {
    pub symbol: Located,
    pub scope: String,
    pub source: String,
    pub hash: String,
    pub definition_hash: String,
    pub stale: bool,
    pub unambiguous: bool,
}

pub fn prepare_record(
    root: &Path,
    request: &crate::memory::RecordSymbolBusinessContextRequest,
) -> crate::memory::Result<PreparedRecord> {
    let options = &request.description;
    let scope = if options.scope.is_empty() {
        "symbol"
    } else {
        options.scope.as_str()
    };
    if !matches!(scope, "symbol" | "file") {
        return Err(std::io::Error::other("scope must be symbol or file").into());
    }
    let source = if options.source.is_empty() {
        "worker"
    } else {
        options.source.as_str()
    };
    if !matches!(source, "worker" | "observer" | "architecture") {
        return Err(std::io::Error::other("source must be worker, observer or architecture; comments are extracted by the indexer").into());
    }
    if request.business_role.trim().is_empty() {
        return Err(
            std::io::Error::other("business_role must describe the actual responsibility").into(),
        );
    }
    let file = crate::symbol_query::normalize_path(&request.file_path);
    if !file.is_empty()
        && (Path::new(&file).is_absolute()
            || file.contains(':')
            || file.split('/').any(|part| part == ".."))
    {
        return Err(std::io::Error::other("file_path must stay within the workspace").into());
    }
    let lang = language(&request.language)
        .or_else(|| {
            Path::new(&file)
                .extension()
                .and_then(|ext| ext.to_str())
                .and_then(language)
        })
        .or_else(|| request.symbol_id.split(':').next().and_then(language))
        .ok_or_else(|| {
            std::io::Error::other("provide a supported language or source-file extension")
        })?;
    // Annotation reuses the known source location, not a whole-project scan.
    let annotation_file = if !file.is_empty() {
        Some(file.clone())
    } else {
        let conn = crate::database::init_db(root)?;
        let (table, _) = definition_sql(lang);
        conn.query_row(
            &format!("SELECT file_path FROM {table} WHERE workspace_root=?1 AND id=?2"),
            params![root.to_string_lossy(), request.symbol_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    };
    ensure_language_index(root, lang, annotation_file.as_deref())?;
    let conn = crate::database::init_db(root)?;
    let requested = if !options.qualified_name.is_empty() {
        options.qualified_name.as_str()
    } else {
        request.symbol_name.as_str()
    };
    let mut candidates = if scope == "symbol" && !request.symbol_id.is_empty() {
        locate(&conn, root, lang, &request.symbol_id, "", "")?
    } else {
        Vec::new()
    };
    if scope == "symbol" && candidates.is_empty() && !file.is_empty() && !requested.is_empty() {
        candidates = locate(&conn, root, lang, "", &file, requested)?;
    }
    if candidates.len() > 1 {
        return Err(std::io::Error::other(format!(
            "ambiguous definition; provide one of these symbol IDs: {}",
            candidates
                .iter()
                .take(10)
                .map(|symbol| symbol.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .into());
    }
    let bound = candidates.len() == 1 || scope == "file";
    let symbol = candidates.pop().unwrap_or_else(|| Located {
        id: if scope == "file" {
            format!("file:{lang}:{file}")
        } else {
            request.symbol_id.clone()
        },
        name: request.symbol_name.clone(),
        qualified_name: if scope == "file" {
            String::new()
        } else {
            requested.to_owned()
        },
        language: lang.to_owned(),
        file_path: file.clone(),
        start_line: 0,
        end_line: 0,
    });
    if symbol.id.is_empty() || symbol.file_path.is_empty() {
        return Err(std::io::Error::other("provide symbol_id or file_path plus qualified_name/symbol_name; file scope requires file_path").into());
    }
    if !file.is_empty() && file != symbol.file_path {
        return Err(std::io::Error::other(
            "symbol_id and file_path identify different definitions",
        )
        .into());
    }
    let path = root.join(&symbol.file_path);
    let hash = if let Ok(canonical) = path.canonicalize() {
        let canonical_root = root.canonicalize()?;
        if !canonical.starts_with(&canonical_root) {
            return Err(std::io::Error::other("source resolves outside the workspace").into());
        }
        if std::fs::metadata(&canonical)?.len() > 2 * 1024 * 1024 {
            return Err(std::io::Error::other("source exceeds the 2 MiB indexing limit").into());
        }
        content_hash(&std::fs::read(canonical)?)
    } else {
        String::new()
    };
    if let Some(expected) = options.expected_code_hash.as_deref() {
        if expected.is_empty() || expected != hash {
            return Err(std::io::Error::other("source changed since it was read; read the current implementation before recording its responsibility").into());
        }
    }
    if scope == "file" && hash.is_empty() {
        return Err(
            std::io::Error::other("file description requires an existing source file").into(),
        );
    }
    let indexed_hash=conn.query_row("SELECT content_hash FROM symbol_index_files WHERE workspace_root=?1 AND language=?2 AND file_path=?3 AND status='indexed'",params![root.to_string_lossy(),lang,symbol.file_path],|row|row.get::<_,String>(0)).optional()?;
    let stale = !bound
        || hash.is_empty()
        || (scope == "symbol" && indexed_hash.as_deref() != Some(hash.as_str()));
    let unambiguous = scope == "file"
        || (bound
            && locate(
                &conn,
                root,
                lang,
                "",
                &symbol.file_path,
                &symbol.qualified_name,
            )?
            .len()
                == 1);
    let definition_hash = if scope == "symbol" && !stale {
        crate::source_read::read(&root.join(&symbol.file_path), 2 * 1024 * 1024)
            .ok()
            .filter(|snapshot| snapshot.hash == hash)
            .and_then(|snapshot| {
                snapshot
                    .range(symbol.start_line, symbol.end_line)
                    .ok()
                    .map(|source| content_hash(source.as_bytes()))
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(PreparedRecord {
        definition_hash,
        symbol,
        scope: scope.to_owned(),
        source: source.to_owned(),
        hash,
        stale,
        unambiguous,
    })
}

/// Standalone memory tools also flag external edits, before the next index query.
pub fn refresh_recorded_sources(root: &Path) -> rusqlite::Result<()> {
    let conn = crate::database::init_db(root)?;
    let mut stmt=conn.prepare("SELECT DISTINCT file_path FROM symbol_business_contexts WHERE workspace_root=?1 AND stale=0")?;
    let files = stmt
        .query_map([root.to_string_lossy()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for file in files {
        let relative = Path::new(&file);
        let safe = !relative.is_absolute()
            && !file.contains(':')
            && !file.split(['/', '\\']).any(|part| part == "..");
        let path = root.join(relative);
        let inside = path
            .canonicalize()
            .ok()
            .zip(root.canonicalize().ok())
            .is_some_and(|(path, root)| path.starts_with(root));
        let hash = if safe
            && inside
            && std::fs::metadata(&path).is_ok_and(|meta| meta.len() <= 2 * 1024 * 1024)
        {
            std::fs::read(&path)
                .ok()
                .map(|bytes| content_hash(&bytes))
                .unwrap_or_default()
        } else {
            String::new()
        };
        if safe && inside {
            if let Some(lang) = path
                .extension()
                .and_then(|ext| ext.to_str())
                .and_then(language)
            {
                let _ = ensure_language_index(root, lang, Some(&file));
            }
        }
        conn.execute("UPDATE symbol_business_contexts SET stale=1 WHERE workspace_root=?1 AND file_path=?2 AND (code_hash='' OR code_hash<>?3)",params![root.to_string_lossy(),file,hash])?;
    }
    Ok(())
}

pub struct Catalog {
    records: HashMap<(String, String), Vec<crate::memory::SymbolBusinessContext>>,
    files: HashMap<String, String>,
}
impl Catalog {
    pub fn load_for_files(root: &Path, lang: &str, files: &[&str]) -> rusqlite::Result<Self> {
        let files = files
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        Self::load_selected(
            root,
            lang,
            Some(&files.into_iter().map(str::to_owned).collect::<Vec<_>>()),
        )
    }
    fn load_selected(root: &Path, lang: &str, files: Option<&[String]>) -> rusqlite::Result<Self> {
        let conn = crate::database::init_db(root)?;
        let mut records = HashMap::<(String, String), Vec<_>>::new();
        let mut hashes = HashMap::new();
        let batches = match files {
            Some(files) => files.chunks(128).map(Some).collect::<Vec<_>>(),
            None => vec![None],
        };
        for batch in batches {
            let mut values = vec![
                rusqlite::types::Value::Text(root.to_string_lossy().into_owned()),
                rusqlite::types::Value::Text(lang.to_owned()),
            ];
            let filter = if let Some(files) = batch {
                values.extend(files.iter().cloned().map(rusqlite::types::Value::Text));
                format!(
                    " AND file_path IN ({})",
                    (3..=values.len())
                        .map(|index| format!("?{index}"))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            } else {
                String::new()
            };
            let mut stmt = conn.prepare(&format!(
                "{} WHERE workspace_root=?1 AND language=?2{filter} ORDER BY updated_at_unix DESC",
                crate::memory::SYMBOL_CONTEXT_SELECT
            ))?;
            for row in stmt.query_map(
                rusqlite::params_from_iter(values.iter()),
                crate::memory::symbol_business_context_from_row,
            )? {
                let record = row?;
                records
                    .entry((record.file_path.clone(), record.qualified_name.clone()))
                    .or_default()
                    .push(record);
            }
            let mut stmt=conn.prepare(&format!("SELECT file_path,content_hash FROM symbol_index_files WHERE workspace_root=?1 AND language=?2{filter}"))?;
            for row in stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (file, hash) = row?;
                hashes.insert(file, hash);
            }
        }
        let files = hashes;
        Ok(Self { records, files })
    }
    pub fn describe(&self, id: &str, file: &str, qualified: &str) -> Description {
        let symbol = self
            .records
            .get(&(file.to_owned(), qualified.to_owned()))
            .and_then(|rows| {
                let eligible = rows
                    .iter()
                    .filter(|row| row.scope == "symbol")
                    .collect::<Vec<_>>();
                eligible
                    .iter()
                    .copied()
                    .find(|row| row.symbol_id == id)
                    .or_else(|| (eligible.len() == 1).then(|| eligible[0]))
            });
        let module = self
            .records
            .get(&(file.to_owned(), String::new()))
            .and_then(|rows| rows.iter().find(|row| row.scope == "file"));
        let record = symbol.or(module);
        let hash = self.files.get(file).cloned().unwrap_or_default();
        let Some(record) = record else {
            return Description {
                qualified_name: qualified.to_owned(),
                text: String::new(),
                status: "missing",
                source: String::new(),
                scope: "symbol".to_owned(),
                keywords: Vec::new(),
                area: String::new(),
                read_when: String::new(),
                code_hash: hash,
                definition_hash: String::new(),
                searchable: String::new(),
            };
        };
        let current = !record.stale && !hash.is_empty() && record.code_hash == hash;
        let mut searchable = format!(
            "{} {} {} {} {}",
            record.business_role,
            record.keywords.join(" "),
            record.common_tasks.join(" "),
            record.belongs_to_area,
            record.read_when
        );
        if let Some(module) = module
            .filter(|_| symbol.is_some())
            .filter(|module| !module.stale && module.code_hash == hash)
        {
            searchable.push_str(&format!(
                " {} {} {}",
                module.business_role,
                module.keywords.join(" "),
                module.common_tasks.join(" ")
            ));
        }
        Description {
            qualified_name: qualified.to_owned(),
            text: crate::symbol_query::preview(&record.business_role, 240),
            status: if current { "current" } else { "stale" },
            source: record.source.clone(),
            scope: record.scope.clone(),
            keywords: record
                .keywords
                .iter()
                .take(12)
                .map(|word| crate::symbol_query::preview(word, 60))
                .collect(),
            area: crate::symbol_query::preview(&record.belongs_to_area, 80),
            read_when: crate::symbol_query::preview(&record.read_when, 160),
            code_hash: hash,
            definition_hash: record.definition_hash.clone(),
            searchable,
        }
    }
}

pub(crate) fn ensure_language_index(
    root: &Path,
    lang: &str,
    file: Option<&str>,
) -> std::io::Result<()> {
    let result = match lang {
        "rust" => {
            crate::rust_index::ensure_query_index(root, file, None).map_err(|e| e.to_string())
        }
        "go" => crate::go_index::ensure_query_index(root, file, None).map_err(|e| e.to_string()),
        "python" => {
            crate::python_index::ensure_query_index(root, file, None).map_err(|e| e.to_string())
        }
        _ => crate::ts_index::ensure_query_index(root, file, None).map_err(|e| e.to_string()),
    };
    result.map_err(std::io::Error::other)
}
