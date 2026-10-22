//! Domain-neutral task board. Definitions and confirmed deliverables are durable
//! events, independent of compressible conversation windows. Raw chat stays in history.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

fn owner(conn: &Connection, task: &str) -> Result<Option<String>> {
    let mut id=task.to_owned(); let mut seen=BTreeSet::new();
    for _ in 0..32 {
        ensure!(seen.insert(id.clone()),"cyclic task lineage");
        match conn.query_row("SELECT parent_task_id FROM agent_tasks WHERE id=?1",[&id],|r|r.get::<_,Option<String>>(0)).optional()? {
            Some(Some(parent))=>id=parent, Some(None)=>return Ok(Some(id)), None=>return Ok(None),
        }
    }
    anyhow::bail!("task lineage too deep")
}
fn latest(conn:&Connection,task:&str,kind:&str,through:Option<i64>)->Result<Option<Value>> {
    let row:Option<(i64,String)>=conn.query_row("SELECT seq,data FROM agent_task_events WHERE task_id=?1 AND kind=?2 AND (?3 IS NULL OR seq<=?3) ORDER BY seq DESC LIMIT 1",params![task,kind,through],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    row.map(|(seq,raw)|{let mut data:Value=serde_json::from_str(&raw)?;data["version"]=json!(seq);Ok(data)}).transpose()
}
fn record(root:&Path,task:&str,kind:&str,expected:Option<i64>,data:&Value)->Result<Value> {
    let mut conn=crate::agent_service::open_db(root)?;
    let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(owner(&tx,task)?.as_deref()==Some(task),"only the main task may change its definition");
    let previous=latest(&tx,task,kind,None)?;
    ensure!(expected==previous.as_ref().and_then(|v|v["version"].as_i64()),"TASK_VERSION_CONFLICT: read the current task board before updating");
    let seq=crate::agent_service::append_event_tx(&tx,task,kind,data,Some("append"))?;
    tx.commit()?;
    let mut result=data.clone();result["version"]=json!(seq);Ok(result)
}
fn strings(value:&Value,field:&str,required:bool)->Result<()> {
    let items=value[field].as_array().with_context(||format!("{field} must be an array"))?;
    ensure!(!required || !items.is_empty(),"{field} cannot be empty");
    ensure!(items.len()<=64 && items.iter().all(|v|v.as_str().is_some_and(|s|!s.trim().is_empty()&&s.len()<=2048)),"invalid {field}");Ok(())
}
pub fn update(root:&Path,task:&str,args:&Value)->Result<Value> {
    let goal=args["objective"].as_str().filter(|s|!s.trim().is_empty()&&s.len()<=4096).context("concise objective required (max 4096 bytes)")?;
    strings(args,"constraints",false)?;strings(args,"completion_conditions",true)?;
    let refs=args["source_event_seqs"].as_array().filter(|v|!v.is_empty()&&v.len()<=32).context("cite user events establishing this task or explicit changes")?;
    let conn=crate::agent_service::open_db(root)?;
    for seq in refs {
        let seq=seq.as_i64().context("invalid source event")?;
        let kind:Option<String>=conn.query_row("SELECT kind FROM agent_task_events WHERE task_id=?1 AND seq=?2",params![task,seq],|r|r.get(0)).optional()?;
        ensure!(matches!(kind.as_deref(),Some("user/message"|"user/interjection")),"task changes require real user sources, not tool/model events");
    }
    let data=json!({"objective":goal,"constraints":args["constraints"],"completion_conditions":args["completion_conditions"],"source_event_seqs":refs});
    ensure!(data.to_string().len()<=16384,"TASK_DEFINITION_TOO_LARGE: keep the task definition concise; store working materials by reference");
    // A model can propose, never activate, a task definition. Compare-and-swap
    // against the active version inside the same transaction as proposal creation.
    drop(conn);
    let mut conn=crate::agent_service::open_db(root)?;
    let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(owner(&tx,task)?.as_deref()==Some(task),"only the main task may propose its definition");
    let current=latest(&tx,task,"task/definition",None)?;
    let base=current.as_ref().and_then(|v|v["version"].as_i64());
    ensure!(args["expected_version"].as_i64()==base,"TASK_VERSION_CONFLICT: read the current task board before proposing changes");
    if let Some(current)=&current { if same_definition(current,&data) {return Ok(json!({"status":"unchanged","definition":current}));} }
    if let Some(pending)=pending_definition(&tx,task,None)? {
        if same_definition(&pending["definition"],&data) {return Ok(pending);}
    }
    let mut proposal=json!({"definition":data,"base_version":base,"status":"awaiting_user_confirmation"});
    let seq=crate::agent_service::append_event_tx(&tx,task,"task/definition_proposed",&proposal,Some("append"))?;
    tx.commit()?;proposal["version"]=json!(seq);Ok(proposal)
}
fn same_definition(a:&Value,b:&Value)->bool {
    ["objective","constraints","completion_conditions"].iter().all(|k|a[*k]==b[*k])
}
pub(crate) fn definition_matches(conn:&Connection,task:&str,version:Option<i64>)->Result<bool> {
    let current=latest(conn,task,"task/definition",None)?;
    match (current,version) {
        (None,None)=>Ok(true),
        (Some(current),Some(version))=>{
            if current["version"]==version {return Ok(true);}
            Ok(latest(conn,task,"task/definition",Some(version))?.is_some_and(|old|old["version"]==version && same_definition(&old,&current)))
        },
        _=>Ok(false)
    }
}
fn pending_definition(conn:&Connection,task:&str,through:Option<i64>)->Result<Option<Value>> {
    let proposal=latest(conn,task,"task/definition_proposed",through)?;
    let decision=latest(conn,task,"task/definition_decided",through)?;
    let active=latest(conn,task,"task/definition",through)?;
    Ok(proposal.filter(|p|decision.as_ref().is_none_or(|d|d["proposal_version"]!=p["version"])
        && p["base_version"]==active.as_ref().map(|d|d["version"].clone()).unwrap_or(Value::Null)))
}
/// Host/UI action only. Deliberately absent from the model's tool catalog.
/// The client sends an exact proposal version, never replacement definition text.
pub fn decide_definition(root:&Path,task:&str,version:i64,approve:bool,identity:&str)->Result<Value> {
    ensure!(!identity.is_empty(),"confirmation identity required");
    let mut conn=crate::agent_service::open_db(root)?;
    let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(owner(&tx,task)?.as_deref()==Some(task),"only main task definitions may be confirmed");
    let proposal=latest(&tx,task,"task/definition_proposed",None)?.context("no task proposal")?;
    ensure!(proposal["version"]==version,"TASK_PROPOSAL_CONFLICT: review the latest task proposal");
    if let Some(decision)=latest(&tx,task,"task/definition_decided",None)? {
        if decision["proposal_version"]==version {
            ensure!(decision["approved"]==approve,"task proposal already decided");
            return Ok(decision);
        }
    }
    let active=latest(&tx,task,"task/definition",None)?;
    ensure!(proposal["base_version"]==active.as_ref().map(|v|v["version"].clone()).unwrap_or(Value::Null),"TASK_VERSION_CONFLICT: proposal is based on an older task");
    let mut decision=json!({"proposal_version":version,"approved":approve,"identity":identity});
    if approve {
        let mut definition=proposal["definition"].clone();
        definition["approved_proposal_version"]=json!(version);
        definition["confirmed_by"]=json!(identity);
        let seq=crate::agent_service::append_event_tx(&tx,task,"task/definition",&definition,Some("append"))?;
        crate::task_tree::definition_changed(&tx,task,seq)?;
        decision["definition_version"]=json!(seq);
    }
    crate::agent_service::append_event_tx(&tx,task,"task/definition_decided",&decision,Some("append"))?;
    tx.commit()?;Ok(decision)
}
fn plan_matches_definition(conn:&Connection,task:&str,plan:&Value,definition:Option<&Value>)->Result<bool> {
    let Some(version)=plan["definition_version"].as_i64() else {return Ok(true)};
    let Some(current)=definition else {return Ok(false)};
    if current["version"]==version {return Ok(true)}
    let baseline=latest(conn,task,"task/definition",Some(version))?;
    Ok(baseline.as_ref().is_some_and(|b|b["version"]==version && same_definition(b,current)))
}
/// A domain adapter proposes ordered deliverables. Approval is an authenticated
/// human action, never an agent tool or an inferred tool success.
pub fn propose_plan(root:&Path,task:&str,plan:Value)->Result<Value> {
    ensure!(plan["items"].as_array().is_some_and(|a|!a.is_empty()&&a.len()<=500),"plan needs ordered deliverables");
    let items=plan["items"].as_array().unwrap();let mut ids=BTreeSet::new();
    for item in items {
        let id=item["id"].as_str().filter(|s|!s.is_empty()).context("plan item id required")?;
        ensure!(ids.insert(id),"duplicate plan item id");
        ensure!(item["title"].as_str().is_some_and(|s|!s.trim().is_empty()),"each plan item needs a title");
        ensure!(item["requirements"].as_str().is_some_and(|s|!s.trim().is_empty()),"each plan item needs concrete content/acceptance requirements");
    }
    ensure!(plan.to_string().len()<=65536,"plan exceeds 64 KiB; use a concise complete deliverable list");
    let conn=crate::agent_service::open_db(root)?;
    let previous=latest(&conn,task,"task/plan",None)?;
    if let Some(previous)=&previous { if previous["plan"]==plan { return Ok(previous.clone()); } }
    record(root,task,"task/plan",previous.as_ref().and_then(|v|v["version"].as_i64()),&json!({"plan":plan}))
}
pub fn confirm_plan(root:&Path,task:&str,version:i64,identity:&str)->Result<Value> {
    let mut conn=crate::agent_service::open_db(root)?;
    let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let plan=latest(&tx,task,"task/plan",None)?.context("no proposed plan")?;
    ensure!(plan["version"]==version,"PLAN_VERSION_CONFLICT: review the latest outline before confirming");
    ensure!(owner(&tx,task)?.as_deref()==Some(task),"only main task plans may be confirmed");
    ensure!(plan_matches_definition(&tx,task,&plan["plan"],latest(&tx,task,"task/definition",None)?.as_ref())?,"TASK_CHANGED: propose an outline for the active task before confirming");
    let approved=json!({"plan_version":version,"identity":identity});
    let existing=latest(&tx,task,"task/plan_confirmed",None)?;
    if !existing.as_ref().is_some_and(|p|p["plan_version"]==version) {
        crate::agent_service::append_event_tx(&tx,task,"task/plan_confirmed",&approved,Some("append"))?;
    }
    crate::task_tree::confirmed_ppt_pages(&tx,task,version,&plan["plan"])?;
    tx.commit()?;Ok(approved)
}
pub fn board(root:&Path,task:&str,through:Option<i64>)->Result<Option<Value>> {
    let conn=crate::agent_service::open_db(root)?;
    let Some(owner)=owner(&conn,task)? else{return Ok(None)};
    let through=if owner==task {through}else{None};
    let definition=latest(&conn,&owner,"task/definition",through)?;
    let plan=latest(&conn,&owner,"task/plan",through)?;
    let confirmation=latest(&conn,&owner,"task/plan_confirmed",through)?;
    let matches=match &plan {Some(p)=>plan_matches_definition(&conn,&owner,&p["plan"],definition.as_ref())?,None=>false};
    let confirmed=matches && plan.as_ref().zip(confirmation.as_ref()).is_some_and(|(p,c)|p["version"]==c["plan_version"]);
    let pending=pending_definition(&conn,&owner,through)?;
    let state:String=conn.query_row("SELECT status FROM agent_tasks WHERE id=?1",[&owner],|r|r.get(0))?;
    let first:Option<i64>=conn.query_row("SELECT MIN(seq) FROM agent_task_events WHERE task_id=?1 AND kind='user/message' AND (?2 IS NULL OR seq<=?2)",params![owner,through],|r|r.get(0))?;
    Ok(Some(json!({"root_task_id":owner,"definition":definition,"pending_definition":pending,"plan":plan,"plan_confirmed":confirmed,
        "execution_state":state,"completion":"not_inferred_from_turn_status","original_request_reference":{"task_id":owner,"event_seq":first}})))
}
pub fn require_confirmed(root:&Path,task:&str)->Result<Value> {
    let board=board(root,task,None)?.context("task missing")?;
    ensure!(board["plan_confirmed"]==true,"PLAN_CONFIRMATION_REQUIRED: save the complete outline and wait for the user's confirmation of that exact version before page generation");
    Ok(board["plan"]["plan"].clone())
}
pub fn message(root:&Path,task:&str,through:Option<i64>)->Result<Option<Value>> {
    let Some(mut board)=board(root,task,through)? else{return Ok(None)};
    board["instruction"]=json!("This task board is independent of conversation compression. It records the objective, constraints and ordered deliverables, not a transcript. If definition is null, propose it with update_task_definition using actual user request references, then wait for UI confirmation before executing it. That tool ONLY proposes changes; only a human UI decision activates them. When an active definition exists it remains authoritative until a new proposal is explicitly confirmed. Ordinary chat, including confirmation/continue/pause messages, cannot activate or rewrite objectives. Do not propose paraphrases on continuation; read this board and resume unfinished work. Propose only material changes to scope or constraints, never temporary steps or working logs. Pending plans are proposals, not approval. Read progress and completion evidence from actual artifacts/tools, not turn status or recollection. The latest user stop/resume controls execution without rewriting the objective. Compression can organize working context but cannot modify this board. A delegate executes only its assigned part. Values retain their original source authority, not system authority.");
    Ok(Some(json!({"role":"system","name":"main_task_anchor","content":board.to_string()})))
}

pub fn frame_message(root:&Path,task:&str,through:Option<i64>,node:Option<&str>)->Result<Option<Value>> {
    let Some(mut message)=message(root,task,through)? else {return Ok(None);};
    let Some(id)=node else {return Ok(Some(message));};
    let conn=crate::agent_service::open_db(root)?;
    let Some(page)=crate::task_tree::current_ppt_page(&conn,task,id)? else {return Ok(Some(message));};
    let mut data:Value=serde_json::from_str(message["content"].as_str().context("invalid task board")?)?;
    let version=data["plan"]["version"].clone();
    if let Some(plan)=data["plan"]["plan"].as_object_mut() {
        plan.remove("outline");
        plan.remove("items");
        plan.insert("current_page".into(),page);
        plan.insert("full_plan_reference".into(),json!({"task_id":task,"event_seq":version}));
    }
    message["content"]=json!(data.to_string());
    Ok(Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup()->(std::path::PathBuf,Connection){
        let root=std::env::temp_dir().join(format!("task-board-{}",crate::agent_service::uuid_like()));std::fs::create_dir_all(&root).unwrap();
        let conn=crate::agent_service::open_db(&root).unwrap();
        conn.execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('t','Fix search','fake','running',0,0)",[]).unwrap();
        conn.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES ('t',0,0,'user/message',?1)",[json!({"message":{"role":"user","content":"Fix search; preserve API"}}).to_string()]).unwrap();(root,conn)
    }
    fn definition()->Value{json!({"objective":"Fix search results","constraints":["Preserve API"],"completion_conditions":["Relevant regression passes"],"source_event_seqs":[0]})}
    fn active(root:&Path)->Value {
        let p=update(root,"t",&definition()).unwrap();
        decide_definition(root,"t",p["version"].as_i64().unwrap(),true,"test-user").unwrap();
        board(root,"t",None).unwrap().unwrap()["definition"].clone()
    }
    #[test]fn model_and_chat_cannot_activate_or_rewrite_task() {
        let (root,conn)=setup();let p=update(&root,"t",&definition()).unwrap();
        assert!(board(&root,"t",None).unwrap().unwrap()["definition"].is_null());
        assert_eq!(update(&root,"t",&definition()).unwrap(),p);
        decide_definition(&root,"t",p["version"].as_i64().unwrap(),true,"user").unwrap();
        let first=board(&root,"t",None).unwrap().unwrap()["definition"].clone();
        for (i,text) in ["继续","确认","好的","停一下"].iter().enumerate(){
            conn.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES ('t',?1,0,'user/interjection',?2)",params![100+i,json!({"text":text}).to_string()]).unwrap();
        }
        let mut change=definition();change["expected_version"]=first["version"].clone();change["objective"]=json!("Accidentally paraphrased goal");change["source_event_seqs"]=json!([100]);
        let proposal=update(&root,"t",&change).unwrap();
        let b=board(&root,"t",None).unwrap().unwrap();assert_eq!(b["definition"],first);assert_eq!(b["pending_definition"],proposal);
        conn.execute("UPDATE agent_tasks SET status='cancelled' WHERE id='t'",[]).unwrap();assert_eq!(board(&root,"t",None).unwrap().unwrap()["definition"],first);
        decide_definition(&root,"t",proposal["version"].as_i64().unwrap(),false,"user").unwrap();
        assert!(board(&root,"t",None).unwrap().unwrap()["pending_definition"].is_null());
        assert_eq!(board(&root,"t",None).unwrap().unwrap()["definition"],first);
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]fn exact_proposal_confirmation_is_atomic_and_idempotent() {
        let (root,_)=setup();let first=active(&root);
        let mut change=definition();change["expected_version"]=first["version"].clone();change["objective"]=json!("Fix sorting");
        let old=update(&root,"t",&change).unwrap();change["objective"]=json!("Fix sorting with stable ties");let new=update(&root,"t",&change).unwrap();
        assert!(decide_definition(&root,"t",old["version"].as_i64().unwrap(),true,"user").is_err());
        let version=new["version"].as_i64().unwrap();let decision=decide_definition(&root,"t",version,true,"user").unwrap();
        assert_eq!(decide_definition(&root,"t",version,true,"user").unwrap()["definition_version"],decision["definition_version"]);
        assert!(decide_definition(&root,"t",version,false,"user").is_err());
        assert_eq!(board(&root,"t",None).unwrap().unwrap()["definition"]["objective"],change["objective"]);
        assert_eq!(board(&root,"t",Some(first["version"].as_i64().unwrap())).unwrap().unwrap()["definition"],first);
        assert!(update(&root,"t",&change).is_err()); // old active version cannot overwrite
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]fn confirmed_plan_uses_only_active_definition_and_exact_outline() {
        let (root,_)=setup();let first=active(&root);
        let plan=json!({"definition_version":first["version"],"items":[{"id":"a","title":"Search","requirements":"Stable ordering"}]});
        let p=propose_plan(&root,"t",plan.clone()).unwrap();let version=p["version"].as_i64().unwrap();
        assert!(require_confirmed(&root,"t").is_err());confirm_plan(&root,"t",version,"user").unwrap();
        assert_eq!(board(&root,"t",None).unwrap().unwrap()["definition"],first); // outline confirmation cannot rewrite goal
        let mut change=definition();change["expected_version"]=first["version"].clone();change["objective"]=json!("Different scope");
        let pending=update(&root,"t",&change).unwrap();assert!(require_confirmed(&root,"t").is_ok());
        decide_definition(&root,"t",pending["version"].as_i64().unwrap(),true,"user").unwrap();
        assert!(require_confirmed(&root,"t").is_err());assert!(confirm_plan(&root,"t",version,"user").is_err());
        let current=board(&root,"t",None).unwrap().unwrap()["definition"].clone();
        let mut restored=definition();restored["expected_version"]=current["version"].clone();let restore=update(&root,"t",&restored).unwrap();
        decide_definition(&root,"t",restore["version"].as_i64().unwrap(),true,"user").unwrap();
        assert!(require_confirmed(&root,"t").is_ok()); // exact restored requirements retain original approval
        let mut changed=plan;changed["items"][0]["requirements"]=json!("New page content");propose_plan(&root,"t",changed).unwrap();
        assert!(require_confirmed(&root,"t").is_err());assert!(confirm_plan(&root,"t",version,"user").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]fn ppt_confirmation_creates_versioned_pages_once_and_projects_page_context() {
        let (root,conn)=setup();let first=active(&root);
        let mut plan=json!({"domain":"ppt","project_id":820,"definition_version":first["version"],"design":"Shared design",
            "outline":"Full outline for two pages","items":[{"id":"p1","title":"One","requirements":"First page content"},{"id":"p2","title":"Two","requirements":"Second page content"}]});
        let version=propose_plan(&root,"t",plan.clone()).unwrap()["version"].as_i64().unwrap();
        confirm_plan(&root,"t",version,"user").unwrap();confirm_plan(&root,"t",version,"user").unwrap();
        let nodes=crate::task_tree::nodes(&conn,"t").unwrap();assert_eq!(nodes.len(),3);
        let page=nodes.iter().find(|n|n.metadata["page_index"]==2).unwrap();
        let projected=frame_message(&root,"t",None,Some(&page.node_id)).unwrap().unwrap();
        let data:Value=serde_json::from_str(projected["content"].as_str().unwrap()).unwrap();
        assert_eq!(data["definition"],first);
        assert_eq!(data["plan"]["plan"]["current_page"]["requirements"],"Second page content");
        assert_eq!(data["plan"]["plan"]["design"],"Shared design");
        assert!(data["plan"]["plan"]["items"].is_null());assert!(data["plan"]["plan"]["outline"].is_null());
        plan["items"][1]["requirements"]=json!("Revised second page");
        let next=propose_plan(&root,"t",plan).unwrap()["version"].as_i64().unwrap();
        confirm_plan(&root,"t",next,"user").unwrap();
        let nodes=crate::task_tree::nodes(&conn,"t").unwrap();
        assert_eq!(nodes.iter().filter(|n|n.status=="cancelled").count(),2);
        assert_eq!(nodes.iter().filter(|n|n.status=="pending").count(),2);
        assert!(crate::task_tree::validate_plan_version(&conn,"t",page).is_err());
        drop(conn);std::fs::remove_dir_all(root).unwrap();
    }
}
