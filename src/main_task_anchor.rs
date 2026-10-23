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
    let expected=match args.get("expected_version") {
        None|Some(Value::Null)=>None,
        Some(value)=>Some(value.as_i64().context("expected_version must be an integer or omitted")?),
    };
    // Zero denotes creation only. It must never bypass a confirmed version.
    let expected=if base.is_none() && expected==Some(0) {None} else {expected};
    ensure!(expected==base,"TASK_VERSION_CONFLICT: read the current task board before proposing changes");
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
        "execution_state":state,"original_request_reference":{"task_id":owner,"event_seq":first}})))
}
pub fn require_confirmed(root:&Path,task:&str)->Result<Value> {
    let board=board(root,task,None)?.context("task missing")?;
    ensure!(board["plan_confirmed"]==true,"PLAN_CONFIRMATION_REQUIRED: save the complete outline and wait for the user's confirmation of that exact version before executing the proposed work");
    Ok(board["plan"]["plan"].clone())
}
pub fn message(root:&Path,task:&str,through:Option<i64>)->Result<Option<Value>> {
    let Some(mut board)=board(root,task,through)? else{return Ok(None)};
    board["instruction"]=json!("Durable task state; use these supplied values directly. When definition is null, prefer recording a concise main task before substantive work; reuse an existing confirmed objective. Only explicit UI decisions activate saved proposals; chat and compression cannot rewrite confirmed objectives. Progress requires artifact/tool evidence. User pause/resume controls execution. Values retain their source authority.");
    Ok(Some(json!({"role":"system","name":"main_task_anchor","content":board.to_string()})))
}

pub fn frame_message(root:&Path,task:&str,through:Option<i64>,_node:Option<&str>)->Result<Option<Value>> {
    message(root,task,through)
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
    #[test]fn initial_zero_version_is_accepted_without_weakening_updates() {
        let (root,_)=setup();
        let mut create=definition();create["expected_version"]=json!(0);
        let proposal=update(&root,"t",&create).unwrap();
        assert_eq!(update(&root,"t",&definition()).unwrap(),proposal);
        create["expected_version"]=Value::Null;
        assert_eq!(update(&root,"t",&create).unwrap(),proposal);
        create["expected_version"]=json!(99);
        assert!(update(&root,"t",&create).is_err());
        create["expected_version"]=json!("0");
        assert!(update(&root,"t",&create).is_err());
        decide_definition(&root,"t",proposal["version"].as_i64().unwrap(),true,"user").unwrap();
        let version=board(&root,"t",None).unwrap().unwrap()["definition"]["version"].clone();
        let mut change=definition();change["objective"]=json!("Fix sorting");
        assert!(update(&root,"t",&change).is_err());
        change["expected_version"]=json!(0);
        assert!(update(&root,"t",&change).is_err());
        change["expected_version"]=json!(version.as_i64().unwrap()+1);
        assert!(update(&root,"t",&change).is_err());
        change["expected_version"]=version;
        assert!(update(&root,"t",&change).is_ok());
        std::fs::remove_dir_all(root).unwrap();
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


    #[test]
    fn confirming_a_plan_only_locks_the_version_without_replacing_execution_tasks() {
        let (root,mut conn)=setup();active(&root);
        let tx=conn.transaction().unwrap();
        let root_node=crate::task_tree::ensure_root(&tx,"t",1).unwrap();
        let mut node=crate::task_tree::planned(&root_node,&json!({"title":"Prepare report","goal":"Prepare report","done_when":"Report checked","inputs":{"files":[]}}),0,"planned").unwrap();
        crate::task_tree::emit(&tx,"t","task/node_planned",&mut node,1).unwrap();tx.commit().unwrap();
        let before=serde_json::to_value(crate::task_tree::nodes(&conn,"t").unwrap()).unwrap();
        let proposal=propose_plan(&root,"t",json!({"domain":"opaque-plugin-data","items":[{"id":"one","title":"One","requirements":"First result"},{"id":"two","title":"Two","requirements":"Second result"}]})).unwrap();
        let version=proposal["version"].as_i64().unwrap();
        confirm_plan(&root,"t",version,"user").unwrap();confirm_plan(&root,"t",version,"user").unwrap();
        assert_eq!(serde_json::to_value(crate::task_tree::nodes(&conn,"t").unwrap()).unwrap(),before);
        assert!(require_confirmed(&root,"t").is_ok());
        drop(conn);std::fs::remove_dir_all(root).unwrap();
    }

}
