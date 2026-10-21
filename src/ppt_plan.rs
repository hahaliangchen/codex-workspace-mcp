//! PPT adapter for the generic task board. No document JSON is stored here.
use anyhow::{Context,Result,ensure};
use serde_json::{Value,json};
use std::path::Path;

pub fn proposal(root:&Path,task:&str,project:u64,args:&Value)->Result<Value>{
    let board=crate::main_task_anchor::board(root,task,None)?.context("task missing")?;
    ensure!(board["definition"].is_object(),"TASK_DEFINITION_REQUIRED: define the main task before proposing its outline");
    let pages=args["pages"].as_array().context("pages required: one ordered item per target page, with id, title and requirements")?;
    ensure!(args["total_pages"].as_u64()==Some(pages.len() as u64),"outline count must equal target page count");
    for key in ["title","outline","design_spec"] {ensure!(args[key].as_str().is_some_and(|s|!s.trim().is_empty()),"{key} required");}
    Ok(json!({"domain":"ppt","project_id":project,"title":args["title"],"items":pages,
        "target_page_count":pages.len(),"outline":args["outline"],"design":args["design_spec"],
        "template":crate::ppt_sessions::template_pin(root,task)?,"definition_version":board["definition"]["version"]}))
}
pub fn require_generation(root:&Path,task:&str)->Result<Value>{
    let plan=crate::main_task_anchor::require_confirmed(root,task)?;
    ensure!(plan["project_id"].as_u64()==crate::ppt_sessions::project(root,task)?,"plan project mismatch");
    Ok(plan)
}
pub fn guard(root:&Path,task:&str,name:&str)->Result<()> {
    if matches!(name.trim_start_matches("ppt_master__"),"apply_html"|"manage_design_pages"|"apply_edits"|"build_deck") {
        require_generation(root,task)?;
    }
    Ok(())
}
pub fn saved_matches(plan:&Value,project:&Value)->bool {
    plan["project_id"]==project["project_id"] && plan["outline"]==project["outline"] && plan["design"]==project["design"]
}

/// Record actual editor identities and successful authoring receipts. Template
/// duplication only creates pages; it never marks their content authored.
pub fn observe_page_result(root:&Path,task:&str,name:&str,result:&Value)->Result<()> {
    if result["status"]!="succeeded" {return Ok(())}
    let Ok(plan)=require_generation(root,task) else{return Ok(())};
    let board=crate::main_task_anchor::board(root,task,None)?.unwrap();
    let version=board["plan"]["version"].as_i64().unwrap();
    let conn=crate::agent_service::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_ppt_page_map(task_id TEXT NOT NULL,plan_version INTEGER NOT NULL,page_index INTEGER NOT NULL,plan_page_id TEXT NOT NULL,slide_id TEXT NOT NULL,authored INTEGER NOT NULL DEFAULT 0,revision INTEGER,PRIMARY KEY(task_id,plan_version,page_index),FOREIGN KEY(task_id) REFERENCES agent_tasks(id) ON DELETE CASCADE)")?;
    let parts=result["data"].as_array().cloned().unwrap_or_else(||vec![result["data"].clone()]);
    for part in parts {
        let pages=part["pageList"].as_array().cloned().unwrap_or_else(||{
            if part["page"].is_number() && part["pageId"].is_string(){vec![json!({"page":part["page"],"id":part["pageId"]})]}else{vec![]}
        });
        for page in pages {
            let Some(index)=page["page"].as_u64().filter(|i|*i>0) else{continue};
            let Some(id)=page["id"].as_str() else{continue};
            let Some(item)=plan["items"].get(index as usize-1) else{continue};
            let authored=name.ends_with("__apply_html") && part["changes"].as_array().is_some_and(|c|c.iter().any(|c|c["pageId"]==id));
            conn.execute("INSERT INTO agent_ppt_page_map(task_id,plan_version,page_index,plan_page_id,slide_id,authored,revision) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(task_id,plan_version,page_index) DO UPDATE SET authored=CASE WHEN slide_id=excluded.slide_id THEN MAX(authored,excluded.authored) ELSE excluded.authored END,slide_id=excluded.slide_id,revision=excluded.revision",rusqlite::params![task,version,index,item["id"].as_str(),id,authored,result["document"]["revision"].as_i64()])?;
        }
    }
    Ok(())
}
