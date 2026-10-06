//! Task-local source materials. Full reads are stored once; model context carries an index.
use std::{path::{Path, PathBuf}, collections::BTreeMap};
use anyhow::{Result, Context, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use axum::{Json, extract::{Path as RoutePath, Query, State}, response::{IntoResponse,Response}, http::StatusCode};
use serde::Deserialize;

pub struct Notebook { root: PathBuf, task_id: String, entries: BTreeMap<i64, Value> }

#[cfg(test)]
mod request_scope_tests {
    use super::*;

    #[test]
    fn reused_node_does_not_implicitly_import_cancelled_request_materials() {
        let notebook=Notebook { root:PathBuf::new(),task_id:String::new(),entries:BTreeMap::from([
            (1,json!({"id":1,"node_id":"work_a","turn":1,"path":"old.rs"})),
            (2,json!({"id":2,"node_id":"work_a","turn":2,"path":"new.rs"})),
        ]) };
        assert_eq!(notebook.node_context("work_a",&[],2)["index"],json!([{"id":2,"node_id":"work_a","turn":2,"path":"new.rs"}]));
        assert_eq!(notebook.node_context("work_a",&["old.rs".into()],2)["index"].as_array().unwrap().len(),2);
        assert_eq!(notebook.node_context("work_a",&[],1)["index"].as_array().unwrap().len(),2);
    }
}

pub const SOURCE_REQUEST_CHARS: usize = 100_000;
// Keep room for new source to be seen once before the Worker chooses what to retain.
pub const SOURCE_LEARNING_RESERVE: usize = 20_000;
pub const SOURCE_CONTEXT_CHARS: usize = SOURCE_REQUEST_CHARS - SOURCE_LEARNING_RESERVE;
pub const MATERIAL_PAGE_CHARS: usize = 60_000;

/// Byte-preserving pages needed by the current node/action, independent of reports.
/// Task-tree frames save selections; the notebook remains the durable backing store.
#[derive(Default)]
pub struct SourceWorkingSet { materials: Vec<Value>, evicted: Vec<Value>, action_id: String }

impl SourceWorkingSet {
    /// The durable notebook is unbounded by this context budget. This collection
    /// contains only pages selected for one action, not every material discovered.
    pub fn select_action(&mut self, args: &Value) {
        if let Some(action)=args["action_id"].as_str().filter(|id|!id.trim().is_empty()) {
            // Renaming an action is not a material-selection decision. Only an
            // explicit replacement/subset releases already selected source.
            self.action_id=action.chars().take(160).collect();
        }
        if args["replace_context"]==true {self.materials.clear();self.evicted.clear();}
        if let Some(ids)=args["source_material_ids"].as_array() {
            self.materials.retain(|page|ids.contains(&page["id"]));
        }
    }
    pub fn materials(&self) -> &[Value] { &self.materials }
    pub fn used_chars(&self) -> usize {
        self.materials.iter().map(|item|item.to_string().chars().count()).sum()
    }
    pub fn fresh_source_budget(&self) -> usize {
        SOURCE_REQUEST_CHARS.saturating_sub(self.used_chars())
    }
    pub fn snapshot(&self) -> Value {
        json!({"action_id":self.action_id,"materials":self.materials.iter().map(|material|json!({
            "id":material["id"],"relative_start":material["relative_start"],
            "path":material["path"],"start_line":material["start_line"],"end_line":material["end_line"],"code_hash":material["code_hash"],
            "relative_end":material["relative_end"],"start_column":material["start_column"],
            "returned_chars":material["returned_chars"]})).collect::<Vec<_>>()})
    }
    /// Restore this node's previously selected pages, checking their current versions.
    /// Every other node's full material remains in the durable notebook.
    pub async fn restore(&mut self, snapshot: &Value, notebook: &mut Notebook, targets: &[String]) -> Result<()> {
        self.materials.clear();self.evicted.clear();
        self.action_id=snapshot["action_id"].as_str().unwrap_or("").to_owned();
        for page in snapshot["materials"].as_array().into_iter().flatten() {
            let mut args=page.clone();args["material_ids"]=json!([page["id"]]);args["include_material"]=json!(true);
            args["max_chars"]=json!(page["returned_chars"].as_u64().unwrap_or(24_000).clamp(1000,MATERIAL_PAGE_CHARS as u64));
            match notebook.recall(&args).await {
                Ok(result)=>{
                    for item in result["materials"].as_array().into_iter().flatten().filter(|item|item["available"]!=true) {
                        self.evicted.push(json!({"id":item["id"],"reason":item["status"]}));
                    }
                    self.add(&result["materials"],targets);
                },
                Err(error)=>self.evicted.push(json!({"id":page["id"],"reason":error.to_string()})),
            }
        }
        Ok(())
    }
    pub fn metadata(&self) -> Value {
        json!({"action_id":self.action_id,"scope":"current_action","capacity_kind":"model_request_source_budget",
            "full_materials_remain_in_notebook":true,"budget_chars":SOURCE_CONTEXT_CHARS,
            "total_source_budget_chars":SOURCE_REQUEST_CHARS,"fresh_source_budget_chars":self.fresh_source_budget(),
            "used_chars":self.used_chars(),
            "materials":self.materials.iter().map(|item|json!({"id":item["id"],"path":item["path"],
                "start_line":item["start_line"],"end_line":item["end_line"],"code_hash":item["code_hash"]})).collect::<Vec<_>>(),
            "evicted":self.evicted})
    }
    pub async fn refresh(&mut self, notebook: &mut Notebook, targets: &[String]) -> Result<()> {
        let saved=self.snapshot();
        self.materials.clear(); self.evicted.clear();
        for page in saved["materials"].as_array().into_iter().flatten() {
            let mut args=page.clone(); args["material_ids"]=json!([page["id"]]);
            args["include_material"]=json!(true);
            args["max_chars"]=json!(page["returned_chars"].as_u64().unwrap_or(24_000).clamp(1000,MATERIAL_PAGE_CHARS as u64));
            match notebook.recall(&args).await {
                Ok(result)=>{
                    for item in result["materials"].as_array().into_iter().flatten().filter(|item|item["available"]!=true) {
                        self.evicted.push(json!({"id":item["id"],"reason":item["status"]}));
                    }
                    self.add(&result["materials"],targets);
                },
                Err(error)=>self.evicted.push(json!({"id":page["id"],"reason":error.to_string()})),
            }
        }
        Ok(())
    }
    pub fn add(&mut self, incoming: &Value, _targets: &[String]) {
        for item in incoming.as_array().into_iter().flatten().filter(|item|item["available"]==true) {
            let covers=|left:&Value,right:&Value| left["path"]==right["path"] && left["code_hash"]==right["code_hash"]
                && left["partial_line"]!=true && right["partial_line"]!=true
                && left["start_line"].as_u64()<=right["start_line"].as_u64()
                && left["end_line"].as_u64()>=right["end_line"].as_u64();
            if let Some(index)=self.materials.iter().position(|old|covers(old,item)) {
                let existing=self.materials.remove(index); self.materials.push(existing); continue;
            }
            let replaced=|old:&Value| covers(item,old) || (old["id"]==item["id"]
                && old["relative_start"]==item["relative_start"] && old["start_column"]==item["start_column"]);
            let used=self.materials.iter().filter(|old|!replaced(old)).map(|old|old.to_string().chars().count()).sum::<usize>();
            if used+item.to_string().chars().count()>SOURCE_CONTEXT_CHARS {
                self.evicted.push(json!({"id":item["id"],"path":item["path"],"reason":"not activated: current action context budget; full material remains in notebook","retrieve":"Select only needed source_material_ids or use replace_context=true with focused line ranges."}));
                continue;
            }
            self.materials.retain(|old|!replaced(old));
            self.materials.push(item.clone());
        }
    }
    pub fn contains_page(&self, material: &Value, result: &Value) -> bool {
        self.materials.iter().any(|page| page["id"]==material["id"] && Some(&page["code_hash"])==result.get("code_hash").or_else(||result.pointer("/description/code_hash"))
            && page["partial_line"]!=true && result["partial_line"]!=true
            && page["start_line"].as_u64()<=result["start_line"].as_u64()
            && page["end_line"].as_u64()>=result["end_line"].as_u64())
    }
}

fn connection(root: &Path) -> Result<Connection> {
    let conn = crate::agent_service::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_notebook_materials(
        id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL, path TEXT NOT NULL,
        file_hash TEXT NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
        source_hash TEXT NOT NULL, descriptor TEXT NOT NULL, result_json TEXT NOT NULL,
        UNIQUE(task_id,path,file_hash,start_line,end_line,source_hash));
        CREATE INDEX IF NOT EXISTS idx_notebook_task ON agent_notebook_materials(task_id,id);")?;
    Ok(conn)
}

fn safe_file(root: &Path, raw: &str) -> Result<PathBuf> {
    let root = std::fs::canonicalize(root)?;
    let path = std::fs::canonicalize(root.join(raw))?;
    ensure!(path.starts_with(&root) && path.is_file(), "material path is outside this workspace or not a file");
    Ok(path)
}

fn source_lines(result: &Value) -> Vec<String> {
    if let Some(content) = result["content"].as_str() { return content.lines().map(str::to_owned).collect(); }
    result["lines"].as_array().into_iter().flatten().filter_map(|line|line["text"].as_str().map(str::to_owned)).collect()
}

impl Notebook {
    pub async fn history(&self, args: &Value) -> Result<Value> {
        let (root,task,query,before,limit)=(self.root.clone(),self.task_id.clone(),
            args["query"].as_str().unwrap_or("").to_lowercase(),args["history_before"].as_i64().unwrap_or(i64::MAX),
            args["history_limit"].as_u64().unwrap_or(50).clamp(1,100) as usize);
        tokio::task::spawn_blocking(move || -> Result<Value> {
            let conn=connection(&root)?;
            ensure!(conn.query_row("SELECT 1 FROM agent_tasks WHERE id=?1",[&task],|_|Ok(())).optional()?.is_some(),"task not found");
            let terms=query.split_whitespace().take(12).collect::<Vec<_>>();
            let mut statement=conn.prepare("SELECT seq,timestamp,data FROM agent_task_events WHERE task_id=?1 AND kind='worker/finding_revision' AND seq<?2 ORDER BY seq DESC")?;
            let rows=statement.query_map(params![task,before],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?)))?;
            let mut items=Vec::new();
            for row in rows {
                let (seq,time,raw)=row?;
                if !terms.is_empty() && !terms.iter().any(|term|raw.to_lowercase().contains(term)) {continue;}
                items.push(json!({"seq":seq,"time":time,"record":serde_json::from_str::<Value>(&raw)?}));
                if items.len()>limit {break;}
            }
            let more=items.len()>limit;items.truncate(limit);
            Ok(json!({"next_before":items.last().map(|item|item["seq"].clone()),"items":items,"has_more":more}))
        }).await?
    }
    pub async fn recall_results(&self,args:&Value)->Result<Value> {
        let ids=args["tool_call_ids"].as_array().context("tool_call_ids must be an array")?.iter().map(|id|id.as_str().map(str::to_owned).context("tool_call_ids must contain strings")).collect::<Result<Vec<_>>>()?;
        ensure!(!ids.is_empty() && ids.len()<=3,"request between 1 and 3 tool call ids");
        let (root,task,request)=(self.root.clone(),self.task_id.clone(),args.clone());
        tokio::task::spawn_blocking(move || -> Result<Value> {
            let conn=connection(&root)?;let mut pages=Vec::new();
            for id in ids {
                let raw=conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='tool/result' AND json_extract(data,'$.message.toolCallId')=?2 ORDER BY seq DESC LIMIT 1",params![task,id],|row|row.get::<_,String>(0)).optional()?.context("tool result not found in this task")?;
                let event:Value=serde_json::from_str(&raw)?;
                let result=event.pointer("/meta/result").context("original tool result unavailable")?;
                let mut page=crate::source_read::result_page(result,&request)?;
                page["tool_call_id"]=json!(id);page["historical"]=json!(true);pages.push(page);
            }
            Ok(json!({"tool_results":pages,"guidance":"These are historical tool results, not proof of current filesystem state. Reuse their conclusions; run a fresh focused query only when current state matters."}))
        }).await?
    }
    pub async fn load(root: &Path, task_id: &str) -> Result<Self> {
        let root = root.to_path_buf(); let task_id = task_id.to_owned();
        let db_root = root.clone(); let db_id = task_id.clone();
        let entries = tokio::task::spawn_blocking(move || -> Result<BTreeMap<i64,Value>> {
            let conn = connection(&db_root)?;
            let mut stmt = conn.prepare("SELECT id,descriptor FROM agent_notebook_materials WHERE task_id=?1 ORDER BY id")?;
            let rows = stmt.query_map([db_id], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?;
            let mut entries = BTreeMap::new();
            for row in rows { let (id,raw) = row?; let mut item: Value = serde_json::from_str(&raw)?; item["id"] = json!(id); entries.insert(id,item); }
            Ok(entries)
        }).await??;
        Ok(Self { root,task_id,entries })
    }

    /// Capture before coverage filtering or message-size limits, including complete method bodies.
    pub async fn save(&mut self, turn: usize, step: usize, node: &str, tool: &str, result: &Value) -> Result<Option<Value>> {
        let Some(path) = result.get("path").or_else(||result.pointer("/symbol/file_path")).and_then(Value::as_str) else { return Ok(None); };
        let Some(file_hash) = result.get("code_hash").or_else(||result.pointer("/description/code_hash")).and_then(Value::as_str) else { return Ok(None); };
        let start=result.get("material_start_line").or_else(||result.get("start_line")).or_else(||result.pointer("/symbol/start_line")).and_then(Value::as_u64).unwrap_or(1);
        let path=path.replace('\\',"/");
        let symbol=result.pointer("/description/qualified_name").or_else(||result.pointer("/symbol/name")).and_then(Value::as_str).unwrap_or("");
        let initial=json!({"path":path,"file_hash":file_hash,"symbol":symbol,"tool":tool,"turn":turn,"last_step":step,"node_id":node,"whole_file":tool=="read_file","status":"captured","byte_exact":true});
        let (root,task_id,stored,desc)=(self.root.clone(),self.task_id.clone(),result.clone(),initial);
        let (id,descriptor)=tokio::task::spawn_blocking(move || -> Result<(i64,Value)> {
            let snapshot=crate::source_read::read(&safe_file(&root,desc["path"].as_str().unwrap())?,crate::source_read::FILE_LIMIT)?;
            ensure!(snapshot.hash==desc["file_hash"].as_str().unwrap(),"source changed before notebook capture");
            let end=if desc["whole_file"]==true {snapshot.line_count() as u64} else {
                stored.get("material_end_line").or_else(||stored.pointer("/symbol/end_line")).or_else(||stored.get("end_line")).and_then(Value::as_u64).unwrap_or(snapshot.line_count() as u64)
            };
            let body=if end==0 && desc["whole_file"]==true {""} else {snapshot.range(start as usize,end as usize)?};
            let source_hash=crate::symbol_description::content_hash(body.as_bytes());
            let mut desc=desc;desc["start_line"]=json!(start);desc["end_line"]=json!(end);desc["source_hash"]=json!(source_hash);
            let mut stored=stored;
            if let Some(object)=stored.as_object_mut() {object.remove("lines");object.remove("related_types");}
            stored["content"]=json!(body);stored["start_line"]=json!(start);stored["end_line"]=json!(end);stored["complete"]=json!(true);stored["next_start_line"]=Value::Null;
            stored["partial_line"]=json!(false);stored["start_column"]=json!(1);stored["next_column"]=Value::Null;stored["returned_chars"]=json!(body.chars().count());
            stored["source_format"]=json!({"byte_exact":true,"ends_with_newline":body.ends_with('\n')});
            let conn=connection(&root)?;
            conn.execute("INSERT INTO agent_notebook_materials(task_id,path,file_hash,start_line,end_line,source_hash,descriptor,result_json)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(task_id,path,file_hash,start_line,end_line,source_hash) DO UPDATE SET descriptor=excluded.descriptor,result_json=excluded.result_json",params![task_id,desc["path"].as_str(),desc["file_hash"].as_str(),start,end,source_hash,desc.to_string(),stored.to_string()])?;
            let id=conn.query_row("SELECT id FROM agent_notebook_materials WHERE task_id=?1 AND path=?2 AND file_hash=?3 AND start_line=?4 AND end_line=?5 AND source_hash=?6",params![task_id,desc["path"].as_str(),desc["file_hash"].as_str(),start,end,source_hash],|row|row.get(0))?;
            Ok((id,desc))
        }).await??;
        let mut descriptor = descriptor; descriptor["id"] = json!(id);
        self.entries.insert(id,descriptor.clone());
        Ok(Some(descriptor))
    }

    pub fn index(&self, node: &str, query: &str, limit: usize) -> Vec<Value> {
        let terms: Vec<_> = query.to_lowercase().replace('\\', "/").split_whitespace().map(str::to_owned).collect();
        let mut ranked = self.entries.values().filter_map(|entry| {
            let text = format!("{} {}",entry["path"].as_str().unwrap_or(""),entry["symbol"].as_str().unwrap_or("")).to_lowercase();
            let hits = terms.iter().filter(|term|text.contains(term.as_str())).count();
            if !terms.is_empty() && hits == 0 { return None; }
            Some((hits,entry["node_id"]==node,entry["id"].as_i64().unwrap_or(0),entry))
        }).collect::<Vec<_>>();
        ranked.sort_by_key(|(hits,same_node,id,_)| std::cmp::Reverse((*hits,*same_node,*id)));
        ranked.into_iter().take(limit).map(|(_,_,_,entry)| entry.clone()).collect()
    }

    pub fn context(&self, node: &str, query: &str) -> Value {
        json!({"material_count":self.entries.len(),"index":self.index(node,query,12),
            "retrieve":"recall_work with material_ids and include_material=true returns version-checked source. Query without include_material searches conclusions and this index. Stored materials are not approval requirements."})
    }
    pub fn node_context(&self, node: &str, files: &[String], request_started_turn: usize) -> Value {
        let index=self.entries.values().rev().filter(|entry|
            entry["node_id"].as_str()==Some(node) && entry["turn"].as_u64().unwrap_or(0) >= request_started_turn as u64
            || files.iter().any(|file|entry["path"].as_str().is_some_and(|path|path==file.replace('\\',"/"))))
            .take(12).cloned().collect::<Vec<_>>();
        json!({"material_count":self.entries.len(),"index":index,"scope":"current node and selected files",
            "visual_materials":crate::visual_artifacts::index(&self.root,&self.task_id,node,request_started_turn).unwrap_or_default(),
            "retrieve":"Other nodes' full materials remain stored. Parent material pointers and child results identify useful shared source; retrieve only needed material IDs/ranges."})
    }

    pub fn link_findings(&self, mut work: Value) -> Value {
        let link=|finding:&mut Value| {
            let files=finding["files"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(" ");
            let exact=finding["material_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64)
                .filter(|id|self.entries.contains_key(id)).collect::<Vec<_>>();
            finding["available_material_ids"]=json!(exact);
            // File-level recency is a navigation hint, never an exact evidence link.
            if !files.is_empty() {finding["related_material_ids"]=json!(self.index("",&files,8).iter().map(|material|material["id"].clone()).collect::<Vec<_>>());}
        };
        if let Some(findings)=work.get_mut("findings") {
            if let Some(items)=findings.as_array_mut() {for item in items{link(item);}}
            else if let Some(items)=findings.as_object_mut() {for item in items.values_mut(){link(item);}}
        }
        work
    }

    pub async fn recall(&mut self, args: &Value) -> Result<Value> {
        let query = args["query"].as_str().unwrap_or("");
        let explicit = args["material_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect::<Vec<_>>();
        let mut candidates = if explicit.is_empty() { self.index("",query,12) } else {
            let mut out = Vec::new();
            for id in explicit { out.push(self.entries.get(&id).context("unknown material id for this task")?.clone()); }
            out
        };
        if candidates.is_empty() && !query.trim().is_empty() {
            let (root,task_id,query)=(self.root.clone(),self.task_id.clone(),query.to_owned());
            candidates=tokio::task::spawn_blocking(move || -> Result<Vec<Value>> {
                let conn=connection(&root)?; let mut found=BTreeMap::new();
                for term in query.split_whitespace().take(8) {
                    let mut stmt=conn.prepare("SELECT id,descriptor FROM agent_notebook_materials WHERE task_id=?1 AND instr(lower(result_json),lower(?2))>0 ORDER BY length(result_json),id DESC LIMIT 12")?;
                    let rows=stmt.query_map(params![task_id,term],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?;
                    for row in rows {let (id,raw)=row?; let mut desc:Value=serde_json::from_str(&raw)?;desc["id"]=json!(id);found.insert(id,desc);}
                }
                Ok(found.into_values().rev().take(12).collect())
            }).await??;
        }
        if candidates.is_empty() {
            let related=args["related_files"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(" ");
            if !related.is_empty() {candidates=self.index("",&related,12);}
        }
        if args["include_material"] != true { return Ok(json!({"index":candidates})); }
        ensure!(!query.trim().is_empty() || !candidates.is_empty(), "provide material_ids or a focused query");
        let mut budget = args["max_chars"].as_u64().unwrap_or(MATERIAL_PAGE_CHARS as u64).clamp(1000,MATERIAL_PAGE_CHARS as u64) as usize;
        let mut materials = Vec::new();
        let mut deferred = Vec::new();
        for entry in candidates {
            if budget < 1000 { deferred.push(entry); continue; }
            let (root,task_id,id,request,allowed) = (self.root.clone(),self.task_id.clone(),entry["id"].as_i64().unwrap(),args.clone(),budget);
            let material = tokio::task::spawn_blocking(move || retrieve(&root,&task_id,id,&request,allowed)).await??;
            budget = budget.saturating_sub(material["returned_chars"].as_u64().unwrap_or(0) as usize);
            if let Some(descriptor) = material.get("descriptor") { self.entries.insert(id,descriptor.clone()); }
            materials.push(material);
        }
        Ok(json!({"materials":materials,"deferred":deferred,"guidance":"Current materials may be used for the next edit without another source read. changed/missing/ambiguous materials require only a targeted refresh. Truncated materials explicitly name the next line; disjoint pieces are not a complete definition."}))
    }
}

/// Verify the actual file even when the old snapshot is still in RAM.
pub fn retrieve(root: &Path, task_id: &str, id: i64, args: &Value, budget: usize) -> Result<Value> {
    let conn = connection(root)?;
    let raw:String=conn.query_row("SELECT descriptor FROM agent_notebook_materials WHERE task_id=?1 AND id=?2",params![task_id,id],|row|row.get(0)).optional()?.context("material not found")?;
    let mut desc: Value = serde_json::from_str(&raw)?; desc["id"] = json!(id);
    let path = match safe_file(root,desc["path"].as_str().unwrap_or("")) {
        Ok(path) => path,
        Err(_) => return Ok(json!({"id":id,"status":"missing","descriptor":desc,"available":false})),
    };
    if args["force_read"]==true {crate::source_read::invalidate(&path);}
    let snapshot=crate::source_read::read(&path,crate::source_read::FILE_LIMIT)?;
    let current_hash=snapshot.hash.clone();
    let mut start = desc["start_line"].as_u64().unwrap_or(1) as usize;
    let captured_end=desc["end_line"].as_u64().unwrap_or(0) as usize;
    let mut line_count=if captured_end>=start {captured_end-start+1}else{0};
    if snapshot.line_count()==0 && desc["whole_file"]==true {line_count=0;}
    let status = if desc["file_hash"] == current_hash { "current" } else {
        // A small edit elsewhere must not discard an unchanged method. Relocate
        // only a unique exact region; never pretend changed source is current.
        if desc["whole_file"]==true {return Ok(json!({"id":id,"status":"changed","descriptor":desc,"current_file_hash":current_hash,"available":false}));}
        let body:String=conn.query_row("SELECT result_json FROM agent_notebook_materials WHERE task_id=?1 AND id=?2",params![task_id,id],|row|row.get(0))?;
        let old_lines=source_lines(&serde_json::from_str::<Value>(&body)?);
        line_count=old_lines.len();
        let current_lines:Vec<_>=snapshot.content.lines().collect();
        let matches = if old_lines.is_empty() { Vec::new() } else {
            current_lines.windows(old_lines.len()).enumerate().filter(|(_,region)| region.iter().zip(&old_lines).all(|(left,right)|*left==right)).map(|(offset,_)|offset+1).take(2).collect::<Vec<_>>()
        };
        if matches.len() != 1 { return Ok(json!({"id":id,"status":if matches.len()>1{"ambiguous"}else{"changed"},"descriptor":desc,"current_file_hash":current_hash,"available":false})); }
        start = matches[0]; "unchanged_region"
    };
    let end = start + line_count.saturating_sub(1);
    if line_count==0 && desc["whole_file"]==true {
        return Ok(json!({"id":id,"descriptor":desc,"available":true,"status":status,"path":desc["path"],"code_hash":current_hash,
            "start_line":1,"end_line":0,"content":"","returned_chars":0,"complete":true,"next_start_line":null,"byte_exact":true}));
    }
    let requested_start = args["relative_start"].as_u64().map(|offset|(start as u64).saturating_add(offset))
        .or_else(||args["start_line"].as_u64()).unwrap_or(start as u64).max(start as u64) as usize;
    let requested_end = args["relative_end"].as_u64().map(|offset|(start as u64).saturating_add(offset))
        .or_else(||args["end_line"].as_u64()).unwrap_or(end as u64).min(end as u64) as usize;
    ensure!(requested_start <= requested_end, "requested range is outside this material");
    let old_descriptor_hash = desc["file_hash"].clone();
    desc["file_hash"]=json!(current_hash);desc["start_line"]=json!(start);desc["end_line"]=json!(end);desc["status"]=json!(status);
    if old_descriptor_hash!=desc["file_hash"] {conn.execute("UPDATE agent_notebook_materials SET descriptor=?1 WHERE id=?2 AND task_id=?3",params![desc.to_string(),id,task_id])?;}
    let mut output=json!({"id":id,"descriptor":desc,"available":true,"status":status,"path":desc["path"],"code_hash":current_hash,
        "start_line":requested_start,"end_line":requested_end,"content":snapshot.range(requested_start,requested_end)?,"byte_exact":true});
    let page_args=json!({"max_chars":budget,"start_line":requested_start,"end_line":requested_end,"start_column":args["start_column"]});
    crate::source_read::page(&mut output,&page_args)?;
    output["relative_start"]=json!(requested_start-start);output["relative_end"]=json!(output["end_line"].as_u64().unwrap_or(requested_start as u64) as usize-start);
    Ok(output)
}

pub async fn list(State(state): State<crate::agent_service::AgentServiceState>,RoutePath(task_id):RoutePath<String>) -> Response {
    let root=state.workspace.root().to_path_buf();
    let result=async {
        let notebook=Notebook::load(&root,&task_id).await?;
        let mut work=tokio::task::spawn_blocking(move || -> Result<Value> {
            let conn=connection(&root)?;
            ensure!(conn.query_row("SELECT 1 FROM agent_tasks WHERE id=?1",[&task_id],|_|Ok(())).optional()?.is_some(),"task not found");
            let raw=conn.query_row("SELECT data FROM agent_task_events WHERE task_id=?1 AND kind='worker/work_state' ORDER BY seq DESC LIMIT 1",[task_id],|row|row.get::<_,String>(0)).optional()?;
            Ok(raw.and_then(|raw|serde_json::from_str::<Value>(&raw).ok()).map(|event|event["state"].clone()).unwrap_or(json!({})))
        }).await??;
        work["finding_timeline"]=notebook.history(&json!({})).await?;
        Ok::<Value,anyhow::Error>(json!({"materials":notebook.index("","",usize::MAX),"work":notebook.link_findings(work)}))
    }.await;
    api_response(result)
}

#[derive(Deserialize)]
pub struct HistoryQuery { before: Option<i64>, query: Option<String>, limit: Option<usize> }
pub async fn history(State(state): State<crate::agent_service::AgentServiceState>,RoutePath(task_id):RoutePath<String>,Query(query):Query<HistoryQuery>) -> Response {
    let root=state.workspace.root().to_path_buf();
    api_response(async {
        let notebook=Notebook::load(&root,&task_id).await?;
        notebook.history(&json!({"history_before":query.before,"query":query.query,"history_limit":query.limit})).await
    }.await)
}

#[derive(Deserialize)]
pub struct MaterialRange { start_line: Option<usize>, end_line: Option<usize>, start_column:Option<usize> }
pub async fn get(State(state): State<crate::agent_service::AgentServiceState>,RoutePath((task_id,id)):RoutePath<(String,i64)>,Query(range):Query<MaterialRange>) -> Response {
    let root=state.workspace.root().to_path_buf();
    api_response(tokio::task::spawn_blocking(move ||retrieve(&root,&task_id,id,&json!({"start_line":range.start_line,"end_line":range.end_line,"start_column":range.start_column}),24_000)).await.unwrap_or_else(|error|Err(error.into())))
}
fn api_response(result: Result<Value>) -> Response {
    match result {
        Ok(value)=>Json(value).into_response(),
        Err(error)=>{let status=if error.to_string().ends_with("not found"){StatusCode::NOT_FOUND}else{StatusCode::BAD_REQUEST};(status,Json(json!({"error":error.to_string()}))).into_response()},
    }
}
