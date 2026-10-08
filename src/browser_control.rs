//! Local Chrome/Edge control for the agent. A shell that opens a URL is not this tool.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpStream, process::Command, time::timeout};

pub const TOOLS: &[&str] = &["browser_open", "browser_read", "browser_wait", "browser_diagnostics", "browser_click", "browser_press_key", "browser_upload", "browser_screenshot", "browser_close", "view_image"];
pub fn is_tool(name: &str) -> bool { TOOLS.contains(&name) }

struct Session { ws: String, child: tokio::process::Child, profile:std::path::PathBuf, session_id:String, page_id:String, owner:Value, epoch:u64, capture_seq:u64, signature:Value, visible:bool }
struct CdpConnection { stream: TcpStream, next_id: u64 }
type Page=std::sync::Arc<tokio::sync::Mutex<Option<Session>>>;
static SESSIONS:std::sync::OnceLock<tokio::sync::Mutex<std::collections::HashMap<String,Page>>>=std::sync::OnceLock::new();
fn sessions()->&'static tokio::sync::Mutex<std::collections::HashMap<String,Page>> {SESSIONS.get_or_init(Default::default)}
fn session_key(root:&Path,task:&str)->String {
    let root=root.canonicalize().unwrap_or_else(|_|root.to_path_buf());
    format!("{}:{task}",root.to_string_lossy())
}
pub async fn cleanup(root:&Path,task:&str) {
    let _=close_session(root,task).await;
}

pub async fn cleanup_after_finish(root:&Path,task:&str,status:&str) {
    if status=="completed" {
        let page=sessions().lock().await.get(&session_key(root,task)).cloned();
        if let Some(page)=page {
            if page.lock().await.as_ref().is_some_and(|session|session.visible) {return;}
        }
    }
    let _=close_session(root,task).await;
}

/// Verify that a historical upload receipt still names the live controlled
/// page and the exact upload attempt kept in that page's JavaScript state.
/// A completed work frame remains historical evidence even when this returns
/// unavailable; callers only revoke reuse of the receipt.
pub async fn validate_upload_receipt(root:&Path,task:&str,receipt:&Value)->Value {
    let receipt_page=&receipt["page"];
    let session_id=receipt_page["browser_session_id"].as_str().unwrap_or("");
    let page_id=receipt_page["page_id"].as_str().unwrap_or("");
    let attempt_id=receipt["upload_attempt_id"].as_str().unwrap_or("");
    let invalid=|reason:&str|json!({"available":false,"reason":reason,"browser_session_id":session_id,
        "page_id":page_id,"upload_attempt_id":attempt_id});
    if session_id.is_empty()||page_id.is_empty()||attempt_id.is_empty() {return invalid("receipt_identity_incomplete");}
    let page=sessions().lock().await.get(&session_key(root,task)).cloned();
    let Some(page)=page else {return invalid("browser_session_missing");};
    let mut guard=page.lock().await;
    let Some(session)=guard.as_mut() else {return invalid("browser_session_closed");};
    if session.session_id!=session_id {return invalid("browser_session_changed");}
    if session.page_id!=page_id {return invalid("browser_page_changed");}
    if !receipt_page["request_id"].is_null()&&session.owner["request_id"]!=receipt_page["request_id"] {return invalid("browser_request_identity_changed");}
    if receipt_page["page_epoch"].as_u64().is_some_and(|epoch|session.epoch<epoch) {return invalid("browser_page_epoch_changed");}
    match session.child.try_wait() {
        Ok(Some(_))=>return invalid("browser_process_exited"),
        Err(_)=>return invalid("browser_process_status_unavailable"),
        Ok(None)=>{}
    }
    let token=serde_json::to_string(attempt_id).unwrap_or_else(|_|"\"\"".to_owned());
    let expression=format!(r#"(()=>{{
      const attempt=window.__codexDocumentUploadAttempts?.[{token}];
      return {{url:location.href,attempt_id:attempt?.id||null,change_event_received:attempt?.change_event_received===true,
        latest_attempt_id:window.__codexLatestDocumentUploadAttempt||null,file_name:attempt?.file_name||null,status:attempt?.status||null}};
    }})()"#);
    let current=match eval(&session.ws,&expression).await {
        Ok(value)=>value,
        Err(_)=>return invalid("browser_page_unreachable"),
    };
    if current["url"]!=receipt_page["url"] {return invalid("browser_page_url_changed");}
    if current["attempt_id"]!=attempt_id||current["change_event_received"]!=true {return invalid("upload_attempt_missing_or_changed");}
    if current["latest_attempt_id"]!=attempt_id {return invalid("upload_attempt_superseded");}
    if !receipt["file"]["name"].is_null()&&current["file_name"]!=receipt["file"]["name"] {return invalid("upload_file_identity_changed");}
    json!({"available":true,"reason":"active_session_page_and_upload_attempt_match","browser_session_id":session_id,
        "page_id":page_id,"page_epoch":session.epoch,"upload_attempt_id":attempt_id,
        "file_name":current["file_name"],"load_status":current["status"]})
}

async fn close_session(root:&Path,task:&str)->Value {
    let page=sessions().lock().await.remove(&session_key(root,task));
    if let Some(page)=page {if let Some(mut session)=page.lock().await.take() {
        let identity=json!({"browser_session_id":session.session_id,"page_id":session.page_id,"display_mode":if session.visible{"visible_window"}else{"headless"}});
        let _=session.child.kill().await;let _=timeout(Duration::from_secs(2),session.child.wait()).await;
        let temp=std::env::temp_dir();
        if session.profile.parent()==Some(temp.as_path()) && session.profile.file_name().is_some_and(|name|name.to_string_lossy().starts_with("codex-agent-browser-")) {
            for _ in 0..3 {if std::fs::remove_dir_all(&session.profile).is_ok(){break;}tokio::time::sleep(Duration::from_millis(50)).await;}
        }
        return json!({"ok":true,"closed":true,"status":"closed","closed_page":identity});
    }}
    json!({"ok":true,"closed":false,"status":"no_browser_session"})
}

pub fn definitions() -> Vec<Value> {
    let url = json!({"type":"string","description":"http(s) or file URL to open."});
    vec![
        json!({"name":"browser_open","description":"Open a URL in the task-owned browser and return its identity, display mode, location, title and visible text. Set visible=true to show the controlled Chrome/Edge window to the user. This does not by itself prove a document or slide loaded.","inputSchema":{"type":"object","required":["url"],"properties":{"url":url,"visible":{"type":"boolean","description":"Show the task-owned browser window. Choose this on the first browser call when the user should see the page; it cannot be changed after the session starts."}}}}),
        json!({"name":"browser_read","description":"Read the current page URL, title, visible text, controls, file inputs, slide count, page indicator and document load evidence. After a file change, generic DOM evidence without a load cycle tied to that upload is reported as unconfirmed. Set expect_text to a phrase that proves the requested page is visible.","inputSchema":{"type":"object","properties":{"expect_text":{"type":"string"}}}}),
        json!({"name":"browser_wait","description":"Wait up to timeout_ms for a selector, visible text, font loading state or an application-confirmed presentation load after the latest file change. A timeout is an unmet condition and is reported as a tool failure.","inputSchema":{"type":"object","properties":{"selector":{"type":"string"},"text":{"type":"string"},"state":{"type":"string","enum":["visible","exists","hidden"]},"font_status":{"type":"string","enum":["loading","loaded"]},"document_loaded":{"type":"boolean","description":"Set true to wait for a confirmed upload change, a completed application loading cycle, a valid current/total page indicator, slides, and no explicit load error. A generic DOM snapshot without a correlated load cycle stays unconfirmed."},"timeout_ms":{"type":"integer","minimum":100,"maximum":30000}}}}),
        json!({"name":"browser_diagnostics","description":"Read bounded console errors, page exceptions, and the page's recorded requests since navigation: every fetch/XHR call (method, URL, status, time, duration, failure) plus font/resource timing entries. Filter with url_contains (e.g. /api/fonts) and category. request_coverage states what the record cannot see; an empty result means nothing matched in that scope, not that no request happened. This tool does not execute worker-provided JavaScript.","inputSchema":{"type":"object","properties":{"url_contains":{"type":"string"},"category":{"type":"string","enum":["all","fetch","xhr","font","resource"]}}}}),
        json!({"name":"browser_click","description":"Click a visible, enabled control. text matches the same label browser_read lists under controls (exact label preferred, then containing); selector is an exact CSS selector and takes precedence. times (1-5) repeats the same click, e.g. to step through slides, before you read the result.","inputSchema":{"type":"object","properties":{"text":{"type":"string"},"selector":{"type":"string"},"times":{"type":"integer","minimum":1,"maximum":5}}}}),
        json!({"name":"browser_press_key","description":"Press a supported key in this task's focused browser page, e.g. Delete after selecting a slide element. This is an interaction, not proof of its visible result.","inputSchema":{"type":"object","required":["key"],"properties":{"key":{"type":"string","enum":["Delete","Backspace","Enter","Escape","Tab","ArrowLeft","ArrowRight","ArrowUp","ArrowDown"]}}}}),
        json!({"name":"browser_upload","description":"Assign a workspace file to an input[type=file]. path is relative to the workspace.","inputSchema":{"type":"object","required":["path"],"properties":{"path":{"type":"string"},"selector":{"type":"string"}}}}),
        json!({"name":"browser_screenshot","description":"Capture an immutable visual artifact of this task's page and select it for the next normal Worker image request. Capture is not a visual pass. Default viewport; full_page is explicit.","inputSchema":{"type":"object","properties":{"full_page":{"type":"boolean"}}}}),
        json!({"name":"browser_close","description":"Explicitly close this task's controlled browser session. Completed visible sessions remain open until this is called or a replacement page is opened.","inputSchema":{"type":"object","properties":{}}}),
        json!({"name":"view_image","description":"Select a host-issued visual artifact_id for the next image request; only this instance or explicitly exported upstream images are permitted. No arbitrary filesystem paths.","inputSchema":{"type":"object","required":["artifact_id"],"properties":{"artifact_id":{"type":"string"},"original":{"type":"boolean"},"checked_goal":{"type":"string"}}}}),
    ]
}

pub async fn execute(workspace: &crate::tools::Workspace, name: &str, args: &Value) -> Result<Value> {
    execute_scoped(workspace,&crate::visual_artifacts::VisualContext::mcp(workspace.root()),name,args).await
}

pub async fn execute_scoped(workspace:&crate::tools::Workspace,context:&crate::visual_artifacts::VisualContext,name:&str,args:&Value)->Result<Value> {
    if name=="view_image" {let mut result=crate::visual_artifacts::view(workspace.root(),context,args["artifact_id"].as_str().context("artifact_id is required")?)?;result["view_original"]=json!(args["original"]==true);return Ok(result);}
    ensure!(!context.task_id().is_empty(),"browser requires a host-bound task");
    if name=="browser_close" {return Ok(close_session(workspace.root(),context.task_id()).await);}
    let page=sessions().lock().await.entry(session_key(workspace.root(),context.task_id())).or_default().clone();
    let mut guard=page.lock().await;
    if guard.is_none() {*guard=Some(launch(name=="browser_open" && args["visible"]==true).await?);}
    let session=guard.as_mut().unwrap();
    if name=="browser_open" && args.get("visible").is_some_and(|value|value.as_bool().is_some_and(|visible|visible!=session.visible)) {
        return Ok(json!({"ok":false,"error_code":"browser_visibility_locked","stage":"session_start","needed":"Open the page in a new task browser session with visible set before the first browser call.","display_mode":if session.visible{"visible_window"}else{"headless"},"browser_session_id":session.session_id}));
    }
    ensure!(name=="browser_open" || session.owner.is_null() || session.owner["request_id"]==context.identity["request_id"],"page belongs to an earlier request; browser_open must bind the new request's page");
    session.owner=context.identity.clone();
    if matches!(name,"browser_open"|"browser_click"|"browser_press_key"|"browser_upload") {session.epoch+=1;}
    let mut result=match name {
        "browser_open" => open(&session.ws,args["url"].as_str().context("url is required")?).await?,
        "browser_read" => read(&session.ws,args["expect_text"].as_str().unwrap_or("")).await?,
        "browser_wait" => wait_for(&session.ws,args).await?,
        "browser_diagnostics" => diagnostics(&session.ws,args).await?,
        "browser_click" => click(&session.ws,args["text"].as_str().unwrap_or(""),args["selector"].as_str().unwrap_or(""),args["times"].as_u64().unwrap_or(1)).await?,
        "browser_press_key" => press_key(&session.ws,args["key"].as_str().context("key is required")?).await?,
        "browser_upload" => upload(&session.ws,workspace.root(),args).await?,
        "browser_screenshot" => return screenshot(workspace.root(),context,session,args["full_page"]==true).await,
        _ => anyhow::bail!("unknown browser tool"),
    };
    let state=page_state(&session.ws).await?;
    if !session.signature.is_null() && session.signature!=state {session.epoch+=1;}
    session.signature=state.clone();result["page"]=json!({"browser_session_id":session.session_id,"page_id":session.page_id,"page_epoch":session.epoch,"url":state["url"],"viewport":state["viewport"],"identity":context.identity});
    result["display_mode"]=json!(if session.visible {"visible_window"} else {"headless"});
    Ok(result)
}

/// Observer has no navigation/mutation API. The captured instance and page must
/// still be the ones authorized by the Worker before and throughout capture.
pub async fn observe_page_snapshot(root:&Path,context:&crate::visual_artifacts::VisualContext,expected_page:&Value)->Result<Value> {
    let page=sessions().lock().await.get(&session_key(root,context.task_id())).cloned().context("no authorized task page is available")?;
    let mut guard=page.lock().await;let session=guard.as_mut().context("task browser is closed")?;
    ensure!(session.owner==context.identity,"Observer page belongs to another work instance");
    if !expected_page["page_id"].is_null() {ensure!(expected_page["page_id"]==session.page_id,"Observer page identity changed");}
    screenshot(root,context,session,false).await
}

async fn page_state(ws:&str)->Result<Value> {
    eval(ws,r#"({url:location.href,viewport:{width:innerWidth,height:innerHeight,device_scale_factor:devicePixelRatio},document_epoch:performance.timeOrigin,dom:document.documentElement.outerHTML.slice(0,200000),ready:document.readyState})"#).await
}

#[cfg(test)]
pub(crate) async fn inspect_test_page(root:&Path,context:&crate::visual_artifacts::VisualContext,expression:&str)->Result<Value> {
    let page=sessions().lock().await.get(&session_key(root,context.task_id())).cloned().context("no test page")?;
    let guard=page.lock().await;let session=guard.as_ref().context("closed test page")?;
    ensure!(session.owner==context.identity,"test inspection cannot cross instance scope");eval(&session.ws,expression).await
}

/* All functions below receive the already locked task page socket. */
async fn open(ws:&str,url: &str) -> Result<Value> {
    ensure!(url.len() <= 2000 && (url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file:")), "url must be http(s) or file");
    let mut cdp=connect_cdp(ws).await?;
    arm_diagnostics(&mut cdp).await?;
    cdp.call("Page.navigate",json!({"url":url})).await?;
    timeout(Duration::from_secs(20), async {
        let mut stable=0;
        for _ in 0..40 {
            let state=eval(ws,"({href:location.href,ready:document.readyState,loaded:(performance.getEntriesByType('navigation')[0]||{}).loadEventEnd>0})").await?;
            let ready=state["ready"]=="complete" && state["loaded"]==true && page_url_matches(state["href"].as_str().unwrap_or(""),url);
            stable=if ready {stable+1}else{0};if stable>=2 {return Ok::<(),anyhow::Error>(());}
            tokio::time::sleep(Duration::from_millis(250)).await;
        }anyhow::bail!("page load did not become ready")
    }).await.context("page load timed out")??;
    read(ws,"").await
}

fn page_url_matches(href: &str, url: &str) -> bool {
    let href = href.trim_end_matches('/');
    let url = url.trim_end_matches('/');
    href == url || href.starts_with(url)
}

async fn read(ws:&str,expect: &str) -> Result<Value> {
    let value = eval(ws, r#"(()=>{
      const visible=el=>{const s=getComputedStyle(el);return !!(el.getClientRects().length&&s.display!=='none'&&s.visibility!=='hidden'&&Number(s.opacity||1)>0)};
      const controls=[...document.querySelectorAll('button,a,[role=button],input,select,textarea')].slice(0,40).map(el=>({tag:el.tagName.toLowerCase(),type:el.type||'',text:(el.getAttribute('aria-label')||el.innerText||el.value||'').trim().slice(0,120),id:el.id||'',name:el.name||'',selector:el.id?'#'+CSS.escape(el.id):'',disabled:!!el.disabled,visible:visible(el)}));
      const file_inputs=[...document.querySelectorAll('input[type=file]')].slice(0,12).map(el=>({id:el.id||'',name:el.name||'',accept:el.accept||'',multiple:!!el.multiple,disabled:!!el.disabled,visible:visible(el),files_length:el.files?.length||0,selector:el.id?'#'+CSS.escape(el.id):'input[type=file]'}));
      const slide_count=document.querySelectorAll('#slide-list .slide-item,.slide-item,[data-slide-index]').length;
      const page_indicator=[...document.querySelectorAll('#page-indicator,.page-number,.slide-number,[aria-current="page"]')].filter(visible).map(el=>(el.innerText||el.textContent||'').trim().slice(0,80)).filter(Boolean).slice(0,4);
      const loading_indicators=[...document.querySelectorAll('[aria-busy="true"],[data-loading="true"],[role="progressbar"],.loading,.loading-overlay,.spinner')].filter(visible).slice(0,8).map(el=>({selector:el.id?'#'+CSS.escape(el.id):el.tagName.toLowerCase(),text:(el.innerText||el.getAttribute('aria-label')||'').trim().slice(0,120)}));
      const parsePage=value=>{const normalized=String(value||'').trim().replace(/^slide\s+/i,'');const m=normalized.match(/^(\d+)\s*(?:\/|of)\s*(\d+)$/i);if(!m)return null;const current=Number(m[1]),total=Number(m[2]);return Number.isSafeInteger(current)&&Number.isSafeInteger(total)&&current>=1&&total>=1&&current<=total?{current,total}:null};
      const page_numbers=page_indicator.map(parsePage).filter(Boolean);const valid_page_indicator=page_numbers.length>0;
      const error_indicators=[...document.querySelectorAll('#page-indicator,[role="alert"],.error-message,[data-error]')].filter(visible).map(el=>({selector:el.id?'#'+CSS.escape(el.id):el.getAttribute('role')==='alert'?'[role="alert"]':el.className?.toString().slice(0,100)||el.tagName.toLowerCase(),text:(el.innerText||el.textContent||el.getAttribute('aria-label')||'').trim().slice(0,160)})).filter(x=>/打开失败|上传失败|解析失败|导入失败|failed to (open|load|parse|import)|invalid (pptx|presentation)|cannot (read|open)|unable to (read|open)/i.test(x.text)).slice(0,8);
      const upload_attempt_id=window.__codexLatestDocumentUploadAttempt||null;const upload_attempt=upload_attempt_id?(window.__codexDocumentUploadAttempts||{})[upload_attempt_id]||null:null;
      const explicit_error=error_indicators.length>0||upload_attempt?.status==='failed';
      const inferred_loaded=slide_count>0&&valid_page_indicator&&!loading_indicators.length&&!explicit_error;
      const correlated_loaded=!!(upload_attempt&&upload_attempt.change_event_received&&upload_attempt.status==='loaded'&&slide_count>0&&valid_page_indicator&&!loading_indicators.length&&!explicit_error);
      const document_status=explicit_error?'failed':loading_indicators.length||upload_attempt?.status==='loading'?'loading':upload_attempt?(correlated_loaded?'loaded':'unconfirmed'):inferred_loaded?'loaded':slide_count>0?'unconfirmed':'not_loaded';
      return{url:location.href,title:document.title,text:(document.body&&document.body.innerText||'').slice(0,4000),controls,file_inputs,slide_count,page_indicator,page_numbers,valid_page_indicator,loading_indicators,error_indicators,document_loaded:{status:document_status,confirmation:correlated_loaded?'application_load_cycle':inferred_loaded?'dom_inference':'unconfirmed',upload_attempt_id:upload_attempt?.id||null,upload_file_name:upload_attempt?.file_name||null,upload_change_observed:upload_attempt?.change_event_received===true,upload_load_status:upload_attempt?.status||null,slides_detected:slide_count,page_indicator,page_numbers,valid_page_indicator,loading_indicators,error_indicators,ready_state:document.readyState},font_state:document.fonts?document.fonts.status:'unsupported'}
    })()"#).await?;
    let text = value["text"].as_str().unwrap_or("");
    let matched = expect.is_empty() || text.contains(expect);
    Ok(json!({"ok":value["document_loaded"]["status"]!="failed","status":value["document_loaded"]["status"],"url":value["url"],"title":value["title"],"text":text,"controls":value["controls"],"file_inputs":value["file_inputs"],"slide_count":value["slide_count"],"page_indicator":value["page_indicator"],"loading_indicators":value["loading_indicators"],"error_indicators":value["error_indicators"],"document_loaded":value["document_loaded"],"font_state":value["font_state"],"expect_text":expect,"matched":matched,
        "page_state":if matched {"visible"} else {"expect_text missing"}}))
}

async fn wait_for(ws:&str,args:&Value)->Result<Value> {
    let selector=args["selector"].as_str().unwrap_or("");let text=args["text"].as_str().unwrap_or("");
    let font_status=args["font_status"].as_str().unwrap_or("");let require_document_loaded=args["document_loaded"]==true;
    if selector.trim().is_empty()&&text.is_empty()&&font_status.is_empty()&&!require_document_loaded {return Ok(json!({"ok":false,"status":"invalid_condition","error_code":"wait_condition_required","needed":"Set selector, text, font_status or document_loaded=true."}));}
    let state=match args["state"].as_str().unwrap_or("visible") {"visible"=>"visible","exists"=>"exists","hidden"=>"hidden",_=>"visible"};
    let timeout_ms=args["timeout_ms"].as_u64().unwrap_or(10000).clamp(100,30000);
    let started=tokio::time::Instant::now();let deadline=started+Duration::from_millis(timeout_ms);
    loop {
        let expression=format!(r#"(()=>{{const selector={};const text={};const state={};const font_status={};const require_document_loaded={};let el=null;let selector_error=null;if(selector){{try{{el=document.querySelector(selector)}}catch(e){{selector_error=String(e)}}}}const style=el?getComputedStyle(el):null;const visible=!!(el&&el.getClientRects().length&&style.display!=='none'&&style.visibility!=='hidden'&&Number(style.opacity||1)>0);const text_match=!text||(document.body&&document.body.innerText||'').includes(text);const selector_match=!selector||(state==='hidden'?!visible:(state==='exists'?!!el:visible));const actual_font_status=document.fonts?document.fonts.status:'unsupported';const font_match=!font_status||actual_font_status===font_status;const slide_count=document.querySelectorAll('#slide-list .slide-item,.slide-item,[data-slide-index]').length;const page_indicator=[...document.querySelectorAll('#page-indicator,.page-number,.slide-number,[aria-current="page"]')].filter(n=>{{const s=getComputedStyle(n);return n.getClientRects().length&&s.display!=='none'&&s.visibility!=='hidden'&&Number(s.opacity||1)>0}}).map(n=>(n.innerText||n.textContent||'').trim()).filter(Boolean);const parsePage=value=>{{const normalized=String(value||'').trim().replace(/^slide\s+/i,'');const m=normalized.match(/^(\d+)\s*(?:\/|of)\s*(\d+)$/i);if(!m)return null;const current=Number(m[1]),total=Number(m[2]);return Number.isSafeInteger(current)&&Number.isSafeInteger(total)&&current>=1&&total>=current?{{current,total}}:null}};const page_numbers=page_indicator.map(parsePage).filter(Boolean);const valid_page_indicator=page_numbers.length>0;const loading=[...document.querySelectorAll('[aria-busy="true"],[data-loading="true"],[role="progressbar"],.loading,.loading-overlay,.spinner')].filter(n=>{{const s=getComputedStyle(n);return n.getClientRects().length&&s.display!=='none'&&s.visibility!=='hidden'&&Number(s.opacity||1)>0}});const error_indicators=[...document.querySelectorAll('#page-indicator,[role="alert"],.error-message,[data-error]')].filter(n=>{{const s=getComputedStyle(n);return n.getClientRects().length&&s.display!=='none'&&s.visibility!=='hidden'&&Number(s.opacity||1)>0}}).map(n=>(n.innerText||n.textContent||'').trim()).filter(t=>/打开失败|上传失败|解析失败|导入失败|failed to (open|load|parse|import)|invalid (pptx|presentation)|cannot (read|open)|unable to (read|open)/i.test(t));const attempt_id=window.__codexLatestDocumentUploadAttempt||null;const attempt=attempt_id?(window.__codexDocumentUploadAttempts||{{}})[attempt_id]||null:null;const failed=error_indicators.length>0||attempt?.status==='failed';const inferred=slide_count>0&&valid_page_indicator&&!loading.length&&!failed;const loaded=!failed&&!loading.length&&(attempt?(attempt.change_event_received&&attempt.status==='loaded'&&slide_count>0&&valid_page_indicator):inferred);const document_status=failed?'failed':loading.length||attempt?.status==='loading'?'loading':loaded?'loaded':attempt?'unconfirmed':slide_count>0?'unconfirmed':'not_loaded';return{{selector_error,selector_found:!!el,visible,text_match,font_status:actual_font_status,font_match,document_loaded:loaded,document_status,upload_attempt_id:attempt?.id||null,upload_file_name:attempt?.file_name||null,upload_change_observed:attempt?.change_event_received===true,upload_load_status:attempt?.status||null,slide_count,page_indicator,page_numbers,valid_page_indicator,loading_count:loading.length,error_indicators,matched:selector_match&&text_match&&font_match&&(!require_document_loaded||loaded),title:document.title,url:location.href}}}})()"#,
            serde_json::to_string(selector)?,serde_json::to_string(text)?,serde_json::to_string(state)?,serde_json::to_string(font_status)?,require_document_loaded);
        let value=eval(ws,&expression).await?;
        if let Some(error)=value["selector_error"].as_str() {return Ok(json!({"ok":false,"status":"invalid_selector","error_code":"invalid_selector","selector":selector,"error":error,"elapsed_ms":started.elapsed().as_millis()}));}
        if value["document_status"]=="failed"&&require_document_loaded {return Ok(json!({"ok":false,"status":"failed","matched":false,"error_code":"document_load_failed","needed":"The application reported a document load error; the current slides cannot confirm the uploaded file.","page_state":value,"elapsed_ms":started.elapsed().as_millis()}));}
        if value["matched"]==true {return Ok(json!({"ok":true,"status":"matched","matched":true,"selector":selector,"text":text,"font_status":font_status,"document_loaded":require_document_loaded,"state":state,"elapsed_ms":started.elapsed().as_millis(),"page":value}));}
        if tokio::time::Instant::now()>=deadline {return Ok(json!({"ok":false,"status":"timeout","matched":false,"error_code":"wait_timeout","needed":"The requested page condition did not become true; inspect the returned page state and diagnostics.","selector":selector,"text":text,"font_status":font_status,"document_loaded":require_document_loaded,"state":state,"timeout_ms":timeout_ms,"elapsed_ms":started.elapsed().as_millis(),"page_state":value}));}
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const REQUEST_LIMIT: usize = 60;

/// Coverage of `requests`; repeating the read never widens it.
fn request_coverage(captured: bool) -> Value {
    json!({"captured_since_navigation":captured,
        "recorded":"in-page fetch and XMLHttpRequest calls (every status) since the last navigation, plus Resource Timing entries for fetch/xhr/font/css/link",
        "not_visible":["requests made before the last browser_open or by an earlier document","Web Worker and Service Worker requests",
            "status of cross-origin Resource Timing entries without Timing-Allow-Origin (reported as null)","responses served from memory cache without a timing entry"],
        "fact_kind":"browser-observed requests; a server-side http_probe result is a separate fact"})
}

async fn diagnostics(ws:&str,args:&Value)->Result<Value> {
    let value=eval(ws,r#"(()=>{const d=window.__codexBrowserDiagnostics||{events:[],requests:[],startedAt:null};const clean=x=>{try{const u=new URL(x,location.href);u.search='';u.hash='';return u.href.slice(0,500)}catch{return String(x||'').slice(0,500)}};const entries=performance.getEntriesByType('resource');const resources=entries.filter(r=>r.initiatorType==='font'||/\.(woff2?|ttf|otf)(\?|$)/i.test(r.name)||Number(r.responseStatus)>=400).slice(-40).map(r=>({url:clean(r.name),initiator:r.initiatorType||'',status:Number(r.responseStatus)||null,duration_ms:Math.round(r.duration||0),transfer_bytes:Number(r.transferSize)||0}));const timing=entries.filter(r=>['fetch','xmlhttprequest','font','css','link'].includes(r.initiatorType)||/\.(woff2?|ttf|otf)(\?|$)/i.test(r.name)).slice(-150).map(r=>({source:'resource_timing',url:clean(r.name),initiator:r.initiatorType||'',method:null,status:Number(r.responseStatus)||null,time:Math.round(performance.timeOrigin+r.startTime),duration_ms:Math.round(r.duration||0),transfer_bytes:Number(r.transferSize)||0,cache:r.transferSize===0&&r.decodedBodySize>0?'cache_or_unexposed':null}));const fonts=document.fonts?[...document.fonts].slice(0,40).map(f=>({family:String(f.family||'').slice(0,120),status:f.status,weight:f.weight,style:f.style})):[];return{url:location.href,started_at:d.startedAt||null,events:(d.events||[]).slice(-40),requests:d.requests||[],requests_dropped:d.requestsDropped||0,timing,resources,font_state:document.fonts?document.fonts.status:'unsupported',font_faces:fonts}})()"#).await?;
    let events=value["events"].as_array().cloned().unwrap_or_default();let failures=events.iter().filter(|event|matches!(event["type"].as_str(),Some("page_error"|"unhandled_rejection"|"console_error"|"network_error"|"http_error"|"resource_error"|"font_error"))).cloned().collect::<Vec<_>>();
    let needle=args["url_contains"].as_str().unwrap_or("").trim().to_owned();
    let category=args["category"].as_str().unwrap_or("all");
    let matches=|item:&Value|{
        let url=item["url"].as_str().unwrap_or("");
        let kind=match (item["source"].as_str(),item["initiator"].as_str()) {
            (Some("fetch"),_)|(_,Some("fetch"))=>"fetch",(Some("xhr"),_)|(_,Some("xmlhttprequest"))=>"xhr",
            (_,Some("font"))=>"font",_ if url.contains(".woff")||url.ends_with(".ttf")||url.ends_with(".otf")=>"font",_=>"resource"};
        (needle.is_empty()||url.contains(&needle))&&(category=="all"||category==kind)
    };
    let mut requests=value["requests"].as_array().into_iter().flatten().chain(value["timing"].as_array().into_iter().flatten())
        .filter(|item|matches(item)).cloned().collect::<Vec<_>>();
    requests.sort_by_key(|item|item["time"].as_u64().unwrap_or(0));
    let matched=requests.len();
    let requests=requests.split_off(matched.saturating_sub(REQUEST_LIMIT));
    let captured=value["started_at"].is_number();
    let empty_reason=requests.is_empty().then(||format!("No matching request was recorded in the observed scope{}. This does not prove that no request was made or that fonts loaded; see request_coverage.",
        if needle.is_empty(){String::new()}else{format!(" for URLs containing {needle:?}")}));
    Ok(json!({"url":value["url"],"captured_since_navigation":captured,"error_count":failures.len(),"errors":failures,
        "requests":requests,"request_filter":{"url_contains":needle,"category":category},"matched_requests":matched,
        "requests_dropped_before_read":value["requests_dropped"],"empty_reason":empty_reason,"request_coverage":request_coverage(captured),
        "font_and_failed_resources":value["resources"],"font_state":value["font_state"],"font_faces":value["font_faces"],
        "limits":{"errors":40,"requests":REQUEST_LIMIT,"recorded_requests":150,"resources":40,"font_faces":40,"message_chars":900}}))
}

async fn arm_diagnostics(cdp:&mut CdpConnection)->Result<()> {
    cdp.call("Page.enable",json!({})).await?;
    let source=r#"(()=>{
      if(window.__codexBrowserDiagnostics)return;
      const d=window.__codexBrowserDiagnostics={events:[],requests:[],requestsDropped:0,startedAt:Date.now()};
      const cleanUrl=x=>{try{const u=new URL(x,location.href);u.search='';u.hash='';return u.href.slice(0,500)}catch{return String(x||'').slice(0,500)}};
      const add=(type,data={})=>{d.events.push({type,time:Date.now(),...data});if(d.events.length>80)d.events.shift()};
      const request=data=>{d.requests.push({time:Date.now(),...data});if(d.requests.length>150){d.requests.shift();d.requestsDropped++}};
      const msg=x=>{try{return (x&&x.stack)||String(x)}catch{return '[unprintable]'}};
      add('navigation',{url:cleanUrl(location.href)});
      window.addEventListener('error',e=>{
        const target=e.target;
        if(target&&target!==window&&(target.src||target.href))add('resource_error',{stage:'resource_load',url:cleanUrl(target.src||target.href),tag:String(target.tagName||'').toLowerCase(),message:'resource failed to load'});
        else add('page_error',{stage:'script_execution',message:msg(e.error||e.message).slice(0,900),source:cleanUrl(e.filename||''),line:e.lineno||0,column:e.colno||0});
      },true);
      window.addEventListener('unhandledrejection',e=>add('unhandled_rejection',{stage:'promise',message:msg(e.reason).slice(0,900)}));
      if(document.fonts)document.fonts.addEventListener('loadingerror',e=>add('font_error',{stage:'font_load',families:[...(e.fontfaces||[])].slice(0,12).map(face=>String(face.family||'').slice(0,120))}));
      try{new PerformanceObserver(list=>{for(const r of list.getEntries()){const status=Number(r.responseStatus)||0;if(status>=400)add('http_error',{stage:'resource_timing',method:'GET',url:cleanUrl(r.name),status,initiator:String(r.initiatorType||'').slice(0,40)})}}).observe({type:'resource',buffered:true})}catch{}
      const oldError=console.error;
      console.error=function(...args){add('console_error',{stage:'console',message:args.map(msg).join(' ').slice(0,900)});return oldError.apply(this,args)};
      const oldFetch=window.fetch;
      if(oldFetch)window.fetch=function(input,init){const url=cleanUrl(typeof input==='string'?input:(input&&input.url)||'');const method=String((init&&init.method)||(input&&input.method)||'GET').toUpperCase().slice(0,16);const started=performance.now();return oldFetch.apply(this,arguments).then(response=>{request({source:'fetch',method,url,status:response.status,ok:response.ok,response_type:response.type,duration_ms:Math.round(performance.now()-started),failure:null});if(!response.ok)add('http_error',{stage:'fetch_response',method,url,status:response.status});return response},error=>{request({source:'fetch',method,url,status:null,ok:false,duration_ms:Math.round(performance.now()-started),failure:msg(error).slice(0,300)});add('network_error',{stage:'fetch',method,url,message:msg(error).slice(0,500)});throw error})};
      const XHR=window.XMLHttpRequest;
      if(XHR){const oldOpen=XHR.prototype.open;XHR.prototype.open=function(method,url){this.__codexRequest={method:String(method||'GET').toUpperCase().slice(0,16),url:cleanUrl(url)};return oldOpen.apply(this,arguments)};const oldSend=XHR.prototype.send;XHR.prototype.send=function(){const x=this;const req=x.__codexRequest||{method:'GET',url:''};const started=performance.now();x.addEventListener('loadend',()=>{request({source:'xhr',...req,status:x.status||null,ok:x.status>=200&&x.status<400,duration_ms:Math.round(performance.now()-started),failure:x.status===0?'request ended with status 0':null});if(x.status>=400)add('http_error',{...req,stage:'xhr_response',status:x.status});else if(x.status===0)add('network_error',{...req,stage:'xhr',message:'request ended with status 0'})},{once:true});x.addEventListener('error',()=>add('network_error',{...req,stage:'xhr',message:'XMLHttpRequest error'}),{once:true});return oldSend.apply(this,arguments)}}
    })()"#;
    cdp.call("Page.addScriptToEvaluateOnNewDocument",json!({"source":source})).await?;
    Ok(())
}

const MAX_REPEATED_CLICKS: u64 = 5;

/// Candidates and labels match `browser_read` controls, so any label it lists
/// can be clicked by that text. An exact selector takes precedence.
async fn click(ws:&str,text: &str, selector: &str, times: u64) -> Result<Value> {
    ensure!(!text.is_empty() || !selector.is_empty(), "click needs text or selector");
    let expression = format!(r#"(()=>{{const text={};const selector={};
      const label=n=>(n.getAttribute('aria-label')||n.innerText||n.value||'').trim();const norm=s=>String(s).replace(/\s+/g,' ').trim().toLowerCase();
      const visible=el=>{{const s=getComputedStyle(el);return !!(el.getClientRects().length&&s.display!=='none'&&s.visibility!=='hidden'&&Number(s.opacity||1)>0)}};
      let nodes;try{{nodes=[...document.querySelectorAll(selector||'button,a,[role=button],input,select,textarea')]}}catch(e){{return{{ok:false,error:'invalid selector: '+String(e).slice(0,200)}}}}
      const wanted=norm(text);const usable=nodes.filter(n=>visible(n)&&!n.disabled);
      const exact=wanted?usable.filter(n=>norm(label(n))===wanted):usable;const partial=wanted?usable.filter(n=>norm(label(n)).includes(wanted)):[];
      const pool=exact.length?exact:partial;const el=pool[0];
      if(!el)return{{ok:false,available:usable.slice(0,12).map(n=>label(n).slice(0,80)).filter(Boolean),hidden_or_disabled:nodes.length-usable.length}};
      el.scrollIntoView({{block:'center',inline:'center'}});const r=el.getBoundingClientRect();
      return{{ok:r.width>0&&r.height>0,x:r.x+r.width/2,y:r.y+r.height/2,label:(label(el)||el.tagName).slice(0,160),match:selector&&!text?'selector':exact.length?'exact_label':'label_contains',candidates:pool.length}}}})()"#,
        serde_json::to_string(text)?, serde_json::to_string(selector)?);
    let value = eval(&ws, &expression).await?;
    if let Some(error)=value["error"].as_str() {anyhow::bail!("{error}");}
    if value["ok"] != true {
        anyhow::bail!("no visible enabled control matches {}; visible control labels: {}",
            if selector.is_empty(){format!("text {text:?}")}else{format!("selector {selector:?}")},
            serde_json::to_string(&value["available"]).unwrap_or_default());
    }
    let times=times.clamp(1,MAX_REPEATED_CLICKS);
    for index in 0..times {
        if index>0 {tokio::time::sleep(Duration::from_millis(120)).await;}
        for kind in ["mousePressed","mouseReleased"] {call(ws,"Input.dispatchMouseEvent",json!({"type":kind,"x":value["x"],"y":value["y"],"button":"left","clickCount":1})).await?;}
    }
    Ok(json!({"clicked":value["label"],"matched":true,"match":value["match"],"candidates":value["candidates"],"times":times}))
}

async fn press_key(ws:&str,key:&str)->Result<Value> {
    let code=match key {"Delete"=>46,"Backspace"=>8,"Enter"=>13,"Escape"=>27,"Tab"=>9,"ArrowLeft"=>37,"ArrowRight"=>39,"ArrowUp"=>38,"ArrowDown"=>40,_=>anyhow::bail!("unsupported browser key")};
    for kind in ["rawKeyDown","keyUp"] {call(ws,"Input.dispatchKeyEvent",json!({"type":kind,"key":key,"code":key,"windowsVirtualKeyCode":code,"nativeVirtualKeyCode":code})).await?;}
    Ok(json!({"pressed":key}))
}

async fn upload(ws:&str,root: &Path, args: &Value) -> Result<Value> {
    let Some(relative)=args["path"].as_str() else {return Ok(upload_failure("file_unavailable","resolve_file","path is required",json!({})));};
    let file=match crate::file_edit::workspace_path(root,relative) {
        Ok(file)=>file,
        Err(error)=>return Ok(upload_failure("file_unavailable","resolve_file","Provide an existing regular file inside the workspace.",json!({"path":relative,"error":error.to_string()}))),
    };
    if !file.is_file() {return Ok(upload_failure("file_unavailable","resolve_file","Provide an existing regular file inside the workspace.",json!({"path":relative})));}
    let selector=args["selector"].as_str().unwrap_or("input[type=file]");
    let absolute=match cdp_file_path(&file) {
        Ok(path)=>path,
        Err(error)=>return Ok(upload_failure("file_unavailable","resolve_file","The workspace file path cannot be represented safely for the browser.",json!({"path":relative,"error":error.to_string()}))),
    };
    let name=file.file_name().map(|name|name.to_string_lossy().into_owned()).unwrap_or_default();
    let size=std::fs::metadata(&file).map(|m|m.len()).unwrap_or(0);
    let mut cdp=connect_cdp(ws).await?;
    let before=upload_page_identity(&mut cdp).await?;
    for attempt in 0..2 {
        let document=match cdp.call("DOM.getDocument",json!({"depth":0})).await {
            Ok(document)=>document,
            Err(error)=>return Ok(upload_failure("upload_assignment_failed","locate_input","Reload the current page and retry the upload.",json!({"error":error.to_string()}))),
        };
        let Some(root_id)=document.pointer("/result/root/nodeId").and_then(Value::as_u64) else {
            return Ok(upload_failure("page_changed","locate_input","Wait for the page document to finish loading, then retry.",json!({"path":relative})));
        };
        let query=match cdp.call("DOM.querySelectorAll",json!({"nodeId":root_id,"selector":selector})).await {
            Ok(value)=>value,
            Err(error)=>{
                let current=upload_page_identity(&mut cdp).await.unwrap_or(Value::Null);
                if current!=before {return Ok(upload_failure("page_changed","locate_input","Re-open the intended page and retry the upload.",json!({"before":before,"after":current})));}
                if is_stale_node_error(&error)&&attempt==0 {continue;}
                return Ok(upload_failure(if is_stale_node_error(&error){"stale_dom_node"}else{"selector_not_found"},"locate_input","Use a selector for one current input[type=file] element.",json!({"selector":selector,"error":error.to_string()})));
            },
        };
        let after_query=upload_page_identity(&mut cdp).await.unwrap_or(Value::Null);
        if after_query!=before {return Ok(upload_failure("page_changed","locate_input","Re-open the intended page and retry the upload.",json!({"before":before,"after":after_query})));}
        let node_ids=query.pointer("/result/nodeIds").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|value|value.as_u64()).filter(|id|*id>0).collect::<Vec<_>>();
        if node_ids.is_empty() {
            let fallback:Vec<Value>=if selector=="input[type=file]" {Vec::new()} else {
                match cdp.call("DOM.querySelectorAll",json!({"nodeId":root_id,"selector":"input[type=file]"})).await {
                    Ok(value)=>describe_file_inputs(&mut cdp,value.pointer("/result/nodeIds").and_then(Value::as_array).cloned().unwrap_or_default()).await.unwrap_or_default(),
                    Err(_)=>Vec::new(),
                }
            };
            return Ok(upload_failure("selector_not_found","locate_input","Inspect the listed file input candidates and pass the matching selector.",json!({"selector":selector,"candidates":fallback})));
        }
        let mut candidates=Vec::new();let mut valid=Vec::new();let mut stale_seen=false;
        for id in &node_ids {
            match cdp.call("DOM.describeNode",json!({"nodeId":id})).await {
                Ok(description)=>{
                    let node=&description["result"]["node"];
                    let tag=node["nodeName"].as_str().unwrap_or("").to_ascii_lowercase();
                    let attrs=node["attributes"].as_array().cloned().unwrap_or_default();
                    let attributes=attrs.chunks_exact(2).filter_map(|pair|Some((pair[0].as_str()?,pair[1].as_str()?))).collect::<std::collections::HashMap<_,_>>();
                    let is_file=tag=="input"&&attributes.get("type").is_some_and(|value|value.eq_ignore_ascii_case("file"));
                    candidates.push(json!({"tag":tag,"id":attributes.get("id"),"name":attributes.get("name"),"accept":attributes.get("accept"),"multiple":attributes.contains_key("multiple"),"disabled":attributes.contains_key("disabled"),"is_file_input":is_file}));
                    if is_file {valid.push(*id);}
                },
                Err(error) if is_stale_node_error(&error)=>{stale_seen=true;break;},
                Err(error)=>return Ok(upload_failure("upload_assignment_failed","inspect_input","Reload the current page and retry the upload.",json!({"error":error.to_string()}))),
            }
        }
        if valid.is_empty() {
            let current=upload_page_identity(&mut cdp).await.unwrap_or(Value::Null);
            if current!=before {return Ok(upload_failure("page_changed","validate_input","Re-open the intended page and retry the upload.",json!({"before":before,"after":current})));}
            if stale_seen&&attempt==0 {continue;}
            if stale_seen {return Ok(upload_failure("stale_dom_node","validate_input","Read the current page once, then retry upload.",json!({"selector":selector,"candidates":candidates})));}
            if candidates.is_empty()&&attempt==0 {continue;}
            return Ok(upload_failure("selector_not_found","validate_input","Target exactly one input[type=file] element.",json!({"selector":selector,"candidates":candidates})));
        }
        if valid.len()>1 {return Ok(upload_failure("multiple_file_inputs","select_input","Pass selector for exactly one of the file input candidates.",json!({"selector":selector,"candidates":candidates})));}
        let input=valid[0];
        let resolved=match cdp.call("DOM.resolveNode",json!({"nodeId":input})).await {
            Ok(value)=>value,
            Err(error) if is_stale_node_error(&error)&&attempt==0=>{
                let current=upload_page_identity(&mut cdp).await.unwrap_or(Value::Null);
                if current!=before {return Ok(upload_failure("page_changed","resolve_input","Re-open the intended page and retry the upload.",json!({"before":before,"after":current})));}
                continue;
            },
            Err(error)=>return Ok(upload_failure("stale_dom_node","resolve_input","Read the current page once, then retry upload.",json!({"error":error.to_string()}))),
        };
        let Some(object_id)=resolved.pointer("/result/object/objectId").and_then(Value::as_str) else {
            if attempt==0 {continue;}
            return Ok(upload_failure("stale_dom_node","resolve_input","Read the current page once, then retry upload.",json!({})));
        };
        let marker=format!("upload_{}",crate::visual_artifacts::new_id());
        let upload_observer=r#"function(token,fileName){
          const el=this;
          window.__codexUploadEvents=window.__codexUploadEvents||{};
          window.__codexUploadEvents[token]={seen:false};
          window.__codexDocumentUploadAttempts=window.__codexDocumentUploadAttempts||{};
          const attempt={id:token,file_name:fileName,change_event_received:false,busy_seen:false,status:'waiting_for_change',error:null};
          window.__codexDocumentUploadAttempts[token]=attempt;
          window.__codexLatestDocumentUploadAttempt=token;
          const visible=n=>{const s=getComputedStyle(n);return !!(n.getClientRects().length&&s.display!=='none'&&s.visibility!=='hidden'&&Number(s.opacity||1)>0)};
          const parsePage=value=>{
            const normalized=String(value||'').trim().replace(/^slide\s+/i,'');
            const match=normalized.match(/^(\d+)\s*(?:\/|of)\s*(\d+)$/i);
            if(!match)return null;
            const current=Number(match[1]),total=Number(match[2]);
            return Number.isSafeInteger(current)&&Number.isSafeInteger(total)&&current>0&&total>=current?{current,total}:null;
          };
          const inspect=()=>{
            if(!attempt.change_event_received)return;
            const errors=[...document.querySelectorAll('#page-indicator,[role=alert],.error-message,[data-error]')].filter(visible).map(n=>(n.innerText||n.textContent||'').trim()).filter(t=>/打开失败|上传失败|解析失败|导入失败|failed to (open|load|parse|import)|invalid (pptx|presentation)|cannot (read|open)|unable to (read|open)/i.test(t));
            if(errors.length){
              attempt.status='failed';
              attempt.error=errors[0].slice(0,160);
              attempt.completed_at=Date.now();
              observer.disconnect();
              clearTimeout(attempt.timer);
              return;
            }
            const busy=[...document.querySelectorAll('[aria-busy=true],[data-loading=true],[role=progressbar],.loading,.loading-overlay,.spinner')].some(visible);
            if(busy){
              attempt.busy_seen=true;
              attempt.status='loading';
              return;
            }
            if(attempt.busy_seen){
              const page_indicator=document.querySelector('#page-indicator')?.innerText||'';
              const valid=!!parsePage(page_indicator);
              const slides=document.querySelectorAll('#slide-list .slide-item,.slide-item,[data-slide-index]').length;
              attempt.status=slides>0&&valid?'loaded':'unconfirmed';
              attempt.completed_at=Date.now();
              observer.disconnect();
              clearTimeout(attempt.timer);
            }
          };
          const observer=new MutationObserver(inspect);
          observer.observe(document.documentElement,{subtree:true,childList:true,attributes:true,characterData:true,attributeFilter:['aria-busy','data-loading','class','hidden']});
          el.addEventListener('change',()=>{
            window.__codexUploadEvents[token]={seen:true,name:el.files&&el.files[0]?el.files[0].name:null,count:el.files?el.files.length:0};
            attempt.change_event_received=true;
            attempt.file_name=el.files&&el.files[0]?el.files[0].name:fileName;
            inspect();
          },{once:true});
          attempt.timer=setTimeout(()=>{
            if(attempt.status==='loading'||attempt.status==='waiting_for_change'){
              attempt.status='unconfirmed';
              attempt.completed_at=Date.now();
            }
            observer.disconnect();
          },60000);
          return true;
        }"#;
        let listen=cdp.call("Runtime.callFunctionOn",json!({"objectId":object_id,"functionDeclaration":upload_observer,"arguments":[{"value":marker},{"value":name}],"returnByValue":true})).await;
        if let Err(error)=listen {
            if is_stale_node_error(&error)&&attempt==0 {continue;}
            return Ok(upload_failure(if is_stale_node_error(&error){"stale_dom_node"}else{"upload_assignment_failed"},"watch_change","Read the current page once, then retry upload.",json!({"error":error.to_string()})));
        }
        let assigned=cdp.call("DOM.setFileInputFiles",json!({"nodeId":input,"files":[absolute]})).await;
        if let Err(error)=assigned {
            let current=upload_page_identity(&mut cdp).await.unwrap_or(Value::Null);
            if current!=before {return Ok(upload_failure("page_changed","assign_file","Re-open the intended page and retry the upload.",json!({"before":before,"after":current})));}
            if is_stale_node_error(&error)&&attempt==0 {continue;}
            return Ok(upload_failure(if is_stale_node_error(&error){"stale_dom_node"}else{"upload_assignment_failed"},"assign_file","Read the current page once, then retry upload.",json!({"error":error.to_string()})));
        }
        let mut state=Value::Null;let mut verification_stale=false;
        for _ in 0..10 {
            state=match cdp.call("Runtime.callFunctionOn",json!({"objectId":object_id,"functionDeclaration":"function(token){const m=(window.__codexUploadEvents||{})[token]||{};const f=this.files;return{tag:this.tagName,type:this.type,files_length:f?f.length:0,file_names:f?[...f].map(x=>x.name):[],multiple:!!this.multiple,change_event_received:m.seen===true,change_count:m.count||0,change_file_name:m.name||null}}","arguments":[{"value":marker}],"returnByValue":true})).await {
                Ok(value)=>value.pointer("/result/result/value").cloned().unwrap_or(Value::Null),
                Err(error) if is_stale_node_error(&error)=>{verification_stale=true;break;},
                Err(error)=>return Ok(upload_failure("upload_assignment_failed","verify_file","The file was assigned but its input could not be verified; inspect the current page.",json!({"error":error.to_string()}))),
            };
            if state["change_event_received"]==true {break;}
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let after=upload_page_identity(&mut cdp).await.unwrap_or(Value::Null);
        if after!=before {return Ok(upload_failure("page_changed","verify_file","Re-open the intended page and retry the upload.",json!({"before":before,"after":after,"file_state":state})));}
        if verification_stale {return Ok(upload_failure("stale_dom_node","verify_file","Read the current page once, then retry upload.",json!({"file_state":state})));}
        if state["files_length"]!=1||state["file_names"][0]!=name {
            return Ok(upload_failure("upload_assignment_failed","verify_file","Select a stable visible or hidden input[type=file] and retry.",json!({"expected_file":name,"file_state":state,"selector":selector})));
        }
        let file_info=json!({"name":name,"size_bytes":size,"files_length":1,"absolute_path":absolute});
        let input_info=json!({"selector":selector,"multiple":state["multiple"],"change_event_received":state["change_event_received"]});
        if state["change_event_received"]!=true {
            return Ok(json!({"ok":false,"status":"file_assigned_unconfirmed","error_code":"upload_change_event_not_received","stage":"verify_change","needed":"The file is assigned but the page did not receive its change event. Do not upload it again; wait for the page state or inspect browser_diagnostics.","uploaded":relative,"upload_attempt_id":marker,"matched":false,"file":file_info,"input":input_info,"document_loaded":{"status":"not_verified","slides_detected":null},"next_step":"Do not repeat upload. Wait for a document loading state or inspect browser diagnostics."}));
        }
        return Ok(json!({"ok":true,"status":"file_assigned","uploaded":relative,"upload_attempt_id":marker,"matched":true,"file":file_info,"input":input_info,"document_loaded":{"status":"not_verified","slides_detected":null},"next_step":"Wait until browser_read reports a load cycle tied to this upload attempt, then verify page state, font requests and a screenshot."}));
    }
    Ok(upload_failure("stale_dom_node","locate_input","Read the current page once, then retry upload.",json!({"selector":selector})))
}

fn upload_failure(code:&str,stage:&str,needed:&str,details:Value)->Value {
    json!({"ok":false,"status":"failed","error_code":code,"stage":stage,"needed":needed,"details":details})
}
fn cdp_file_path(path:&Path)->Result<String> {
    let raw=path.to_str().context("path is not valid Unicode")?;
    #[cfg(windows)] {
        let ordinary=if let Some(unc)=raw.strip_prefix(r"\\?\UNC\"){format!(r"\\{unc}")}
            else {raw.strip_prefix(r"\\?\").unwrap_or(raw).to_owned()};
        Ok(ordinary.replace('/',"\\"))
    }
    #[cfg(not(windows))] { Ok(raw.to_owned()) }
}
fn is_stale_node_error(error:&anyhow::Error)->bool {
    let message=format!("{error:#}").to_ascii_lowercase();
    message.contains("could not find node")||message.contains("no node with given id")||message.contains("node with given id")||message.contains("not a valid node")
}
async fn upload_page_identity(cdp:&mut CdpConnection)->Result<Value> {
    let value=cdp.call("Runtime.evaluate",json!({"expression":"({url:location.href,time_origin:performance.timeOrigin,ready:document.readyState})","returnByValue":true})).await?;
    value.pointer("/result/result/value").cloned().context("page identity was unavailable")
}
async fn describe_file_inputs(cdp:&mut CdpConnection,node_ids:Vec<Value>)->Result<Vec<Value>> {
    let mut candidates=Vec::new();
    for id in node_ids.into_iter().filter_map(|value|value.as_u64()) {
        let description=cdp.call("DOM.describeNode",json!({"nodeId":id})).await?;let node=&description["result"]["node"];
        let attrs=node["attributes"].as_array().cloned().unwrap_or_default();let attributes=attrs.chunks_exact(2).filter_map(|pair|Some((pair[0].as_str()?,pair[1].as_str()?))).collect::<std::collections::HashMap<_,_>>();
        candidates.push(json!({"tag":node["nodeName"],"id":attributes.get("id"),"name":attributes.get("name"),"accept":attributes.get("accept"),"multiple":attributes.contains_key("multiple"),"disabled":attributes.contains_key("disabled"),"is_file_input":node["nodeName"]=="INPUT"&&attributes.get("type").is_some_and(|value|value.eq_ignore_ascii_case("file"))}));
    }
    Ok(candidates)
}

fn ensure_capture_stable(before:&Value,after:&Value)->Result<()> {
    ensure!(before==after,"page changed during capture; visual ownership/state is uncertain, retry a focused capture");
    Ok(())
}

async fn screenshot(root:&Path,context:&crate::visual_artifacts::VisualContext,session:&mut Session,full_page:bool)->Result<Value> {
    let before=page_state(&session.ws).await?;
    if !session.signature.is_null() && session.signature!=before {session.epoch+=1;}
    session.signature=before.clone();
    let mut options=json!({"format":"png","captureBeyondViewport":full_page});
    if full_page {
        let layout=call(&session.ws,"Page.getLayoutMetrics",json!({})).await?;let size=&layout["result"]["cssContentSize"];
        options["clip"]=json!({"x":0,"y":0,"width":size["width"],"height":size["height"],"scale":1});
    }
    let shot = call(&session.ws, "Page.captureScreenshot", options).await?;
    let data = shot.pointer("/result/data").and_then(Value::as_str).context("screenshot missing")?;
    let bytes = base64_decode(data)?;
    let after=page_state(&session.ws).await?;
    ensure_capture_stable(&before,&after)?;
    session.capture_seq+=1;
    let display_mode=if session.visible{"visible_window"}else{"headless"};
    let artifact=crate::visual_artifacts::save(root,context,json!({"browser_session_id":session.session_id,"page_id":session.page_id,"page_epoch":session.epoch,
        "capture_seq":session.capture_seq,"url":before["url"],"viewport":before["viewport"],"full_page":full_page,"display_mode":display_mode}),&bytes)?;
    Ok(json!({"artifact_id":artifact["artifact_id"],"visual_artifact":artifact,"captured":true,"display_mode":display_mode,"page":{"browser_session_id":session.session_id,"page_id":session.page_id,"page_epoch":session.epoch,"url":before["url"]},"visual_assessment":"not_evaluated","guidance":"Capture succeeded; this does not prove the rendered result passed a visual check."}))
}

async fn launch(visible:bool) -> Result<Session> {
    let browser = browser_path().context("no executable Chrome, Edge or Chromium was found; set AGENT_BROWSER_PATH to its executable path")?;
    let session_id=crate::visual_artifacts::new_id();
    let dir = std::env::temp_dir().join(format!("codex-agent-browser-{session_id}"));
    std::fs::create_dir_all(&dir)?;
    let mut child = Command::new(&browser);
    if !visible {child.arg("--headless=new").arg("--disable-gpu");}
    child.args(["--disable-extensions","--no-first-run","--remote-debugging-port=0"])
        .arg(format!("--user-data-dir={}", dir.display()))
        .args(["--window-size=1280,800","about:blank"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    let child = child.spawn().context("could not start the browser")?;
    let port = timeout(Duration::from_secs(8), async {
        let marker = dir.join("DevToolsActivePort");
        for _ in 0..40 {
            if let Ok(text) = tokio::fs::read_to_string(&marker).await {
                if let Some(port) = text.lines().next().and_then(|line| line.trim().parse::<u16>().ok()) { return Ok(port); }
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        anyhow::bail!("browser did not publish a debugging port")
    }).await??;
    let ws = page_socket(port).await?;
    call(&ws,"Emulation.setDeviceMetricsOverride",json!({"width":1280,"height":800,"deviceScaleFactor":1,"mobile":false})).await?;
    let page_id=ws.rsplit('/').next().unwrap_or("").to_owned();
    Ok(Session { ws, child,profile:dir,session_id,page_id,owner:Value::Null,epoch:0,capture_seq:0,signature:Value::Null,visible })
}

fn page_target(page: &Value) -> bool {
    page["type"] == "page" && page["url"].as_str().is_some_and(|url| !url.starts_with("chrome-extension://") && !url.starts_with("devtools://"))
}

async fn page_socket(port: u16) -> Result<String> {
    let list: Value = reqwest::get(format!("http://127.0.0.1:{port}/json/list")).await?.json().await?;
    if let Some(ws) = list.as_array().and_then(|pages| pages.iter().find(|page| page_target(page) && page["url"].as_str().unwrap_or("").starts_with("about:")).and_then(|page| page["webSocketDebuggerUrl"].as_str())) {
        return Ok(ws.to_owned());
    }
    let created: Value = reqwest::Client::new().put(format!("http://127.0.0.1:{port}/json/new?about:blank")).send().await?.error_for_status()?.json().await?;
    created["webSocketDebuggerUrl"].as_str().context("browser did not open a page").map(str::to_owned)
}

fn browser_path() -> Option<std::path::PathBuf> {
    find_browser(std::env::var_os("AGENT_BROWSER_PATH").as_deref(), &browser_candidates())
}

fn find_browser(explicit: Option<&std::ffi::OsStr>, candidates: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    // An explicit but invalid path must not silently select a different browser.
    if let Some(path) = explicit {
        let path = std::path::PathBuf::from(path);
        return browser_executable(&path).then_some(path);
    }
    candidates.iter().find(|path| browser_executable(path)).cloned()
}

fn browser_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else { return false };
    if !metadata.is_file() { return false; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 { return false; }
    }
    true
}

fn browser_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(Path::new(&local).join(r"Google\Chrome\Application\chrome.exe"));
            candidates.push(Path::new(&local).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
        if let Some(program) = std::env::var_os("PROGRAMFILES") {
            candidates.push(Path::new(&program).join(r"Google\Chrome\Application\chrome.exe"));
            candidates.push(Path::new(&program).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
        if let Some(program) = std::env::var_os("PROGRAMFILES(X86)") {
            candidates.push(Path::new(&program).join(r"Google\Chrome\Application\chrome.exe"));
            candidates.push(Path::new(&program).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut roots = vec![std::path::PathBuf::from("/Applications")];
        if let Some(home) = std::env::var_os("HOME") { roots.push(Path::new(&home).join("Applications")); }
        for root in roots {
            for app in ["Google Chrome.app/Contents/MacOS/Google Chrome", "Microsoft Edge.app/Contents/MacOS/Microsoft Edge", "Chromium.app/Contents/MacOS/Chromium"] {
                candidates.push(root.join(app));
            }
        }
    }
    let names: &[&str] = if cfg!(windows) { &["chrome.exe", "msedge.exe", "chromium.exe"] }
        else { &["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge", "microsoft-edge-stable"] };
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths).filter(|path| path.is_absolute()) {
            for name in names { candidates.push(directory.join(name)); }
        }
    }
    #[cfg(target_os = "linux")]
    {
        for directory in ["/usr/bin", "/usr/local/bin", "/snap/bin"] {
            for name in names { candidates.push(Path::new(directory).join(name)); }
        }
        candidates.extend(["/opt/google/chrome/chrome", "/opt/microsoft/msedge/msedge"].map(std::path::PathBuf::from));
    }
    candidates
}

async fn eval(ws: &str, expression: &str) -> Result<Value> {
    let response = call(ws, "Runtime.evaluate", json!({"expression":expression,"returnByValue":true})).await?;
    ensure!(response.pointer("/result/exceptionDetails").is_none(), "page script failed: {}", response.pointer("/result/exceptionDetails/text").and_then(Value::as_str).unwrap_or("exception"));
    response.pointer("/result/result/value").cloned().context("page script returned no value")
}

async fn call(ws: &str, method: &str, params: Value) -> Result<Value> {
    let mut connection=connect_cdp(ws).await?;
    connection.call(method,params).await
}

async fn connect_cdp(ws:&str)->Result<CdpConnection> {
    Ok(CdpConnection{stream:connect(ws).await?,next_id:1})
}

impl CdpConnection {
    async fn call(&mut self,method:&str,params:Value)->Result<Value> {
        let id=self.next_id;
        self.next_id=self.next_id.checked_add(1).context("browser command id space exhausted")?;
        send(&mut self.stream,&json!({"id":id,"method":method,"params":params}).to_string()).await?;
        let deadline=tokio::time::Instant::now()+Duration::from_secs(20);
        loop {
            let message=timeout(deadline.saturating_duration_since(tokio::time::Instant::now()),recv(&mut self.stream)).await.context("browser command timed out")??;
            let value:Value=serde_json::from_str(&message)?;
            if value["id"].as_u64()==Some(id) {
                if let Some(error)=value.get("error") {anyhow::bail!("{method} failed: {error}");}
                return Ok(value);
            }
        }
    }
}

async fn connect(ws_url: &str) -> Result<TcpStream> {
    let url = reqwest::Url::parse(ws_url)?;
    let host = url.host_str().context("browser socket has no host")?;
    let port = url.port_or_known_default().context("browser socket has no port")?;
    let mut stream = TcpStream::connect((host, port)).await?;
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let request = format!("GET {} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n", url.path());
    stream.write_all(request.as_bytes()).await?;
    let mut header = Vec::new();
    while !header.windows(4).any(|mark| mark == b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        header.push(byte[0]);
        ensure!(header.len() < 8192, "browser upgrade header is too large");
    }
    let text = String::from_utf8_lossy(&header);
    ensure!(text.contains("101"), "browser did not accept the websocket: {text}");
    Ok(stream)
}

async fn send(stream: &mut TcpStream, text: &str) -> Result<()> {
    let payload = text.as_bytes();
    ensure!(payload.len() < 1_000_000, "browser command is too large");
    let mut frame = vec![0x81];
    if payload.len() < 126 { frame.push(0x80 | payload.len() as u8); }
    else if payload.len()<=u16::MAX as usize {frame.push(0x80|126);frame.extend((payload.len() as u16).to_be_bytes());}
    else {frame.push(0x80|127);frame.extend((payload.len() as u64).to_be_bytes());}
    let mask = [1u8, 2, 3, 4];
    frame.extend(mask);
    frame.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ mask[index % 4]));
    stream.write_all(&frame).await?;
    Ok(())
}

async fn recv(stream: &mut TcpStream) -> Result<String> {
    let mut message=Vec::new();
    loop {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await?;
    let opcode = header[0] & 0x0f;
    let mut length = (header[1] & 0x7f) as usize;
    if length == 126 {
        let mut extended = [0u8; 2];
        stream.read_exact(&mut extended).await?;
        length = u16::from_be_bytes(extended) as usize;
    } else if length == 127 {
        let mut extended=[0u8;8];stream.read_exact(&mut extended).await?;
        length=usize::try_from(u64::from_be_bytes(extended))?;
    }
    ensure!(length<=64*1024*1024 && message.len()+length<=64*1024*1024,"browser message exceeds capture budget");
    let masked=header[1]&0x80!=0;let mut mask=[0u8;4];
    if masked {
        stream.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await?;
    if masked {for (index,byte) in payload.iter_mut().enumerate(){*byte^=mask[index%4];}}
    ensure!(opcode==1 || opcode==0,"browser returned a non-text/closed frame");
    message.extend(payload);
    if header[0]&0x80!=0 {return Ok(String::from_utf8(message)?);}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_discovery_honors_explicit_paths_and_skips_non_executables() {
        let root = std::env::temp_dir().join(format!("browser-discovery-{}", crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let absent = root.join("missing");
        let directory = root.join("directory");
        std::fs::create_dir(&directory).unwrap();
        let browser = root.join("Chrome with spaces");
        std::fs::write(&browser, "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(find_browser(None, std::slice::from_ref(&browser)).is_none());
            std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let candidates = vec![absent.clone(), directory, browser.clone()];
        assert_eq!(find_browser(None, &candidates), Some(browser.clone()));
        assert_eq!(find_browser(Some(browser.as_os_str()), &[]), Some(browser));
        assert!(find_browser(Some(absent.as_os_str()), &candidates).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn page_change_during_capture_is_uncertain() {
        let before=json!({"url":"http://localhost/","dom":"old"});
        let after=json!({"url":"http://localhost/","dom":"new"});
        assert!(ensure_capture_stable(&before,&after).unwrap_err().to_string().contains("uncertain"));
        assert!(ensure_capture_stable(&before,&before).is_ok());
    }

    #[test]
    fn recognizes_stale_cdp_dom_node_errors() {
        for message in ["Could not find node with given id", "No node with given id found", "Node with given id is not valid"] {
            assert!(is_stale_node_error(&anyhow::anyhow!(message)),"{message}");
        }
        assert!(!is_stale_node_error(&anyhow::anyhow!("Invalid selector: input[type=file")));
    }

    #[test]
    fn converts_extended_windows_file_paths_to_regular_cdp_paths() {
        if cfg!(windows) {
            assert_eq!(cdp_file_path(Path::new(r"\\?\D:\企业 项目\演示.pptx")).unwrap(),r"D:\企业 项目\演示.pptx");
            assert_eq!(cdp_file_path(Path::new(r"\\?\UNC\server\share\演示.pptx")).unwrap(),r"\\server\share\演示.pptx");
        }
    }

    #[tokio::test]
    async fn opens_a_local_page_and_matches_visible_text() {
        if browser_path().is_none() { return; }
        let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("browser-slide-{}",crate::visual_artifacts::new_id()));std::fs::create_dir_all(&root).unwrap();
        let file = root.join("codex-browser-slide.html");
        std::fs::write(&file, "<html><body><h1>Slide 1</h1><div id='slide-list'><div class='slide-item'>Slide 1</div></div><div id='page-indicator' class='page-info'>1 / 1</div><div class='loading' hidden>Loading</div></body></html>").unwrap();
        let url = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let workspace=crate::tools::Workspace::new(file.parent().unwrap()).unwrap();
        let opened = execute(&workspace,"browser_open",&json!({"url":url})).await.unwrap();
        assert_eq!(opened["title"].as_str().unwrap_or(""), "");
        assert_eq!(opened["display_mode"],"headless");
        let page = execute(&workspace,"browser_read",&json!({"expect_text":"Slide 1"})).await.unwrap();
        assert_eq!(page["matched"], true, "{page}");
        assert_eq!(page["slide_count"],1);
        assert_eq!(page["page_indicator"][0],"1 / 1");
        assert_eq!(page["document_loaded"]["status"],"loaded");
        let waited=execute(&workspace,"browser_wait",&json!({"document_loaded":true,"timeout_ms":3000})).await.unwrap();
        assert_eq!(waited["status"],"matched","{waited}");
        let timeout_result=execute(&workspace,"browser_wait",&json!({"text":"never appears","timeout_ms":100})).await.unwrap();
        assert_eq!(timeout_result["status"],"timeout");assert_eq!(timeout_result["ok"],false);assert_eq!(timeout_result["matched"],false);
        let screenshot=execute(&workspace,"browser_screenshot",&json!({})).await.unwrap();
        assert_eq!(screenshot["display_mode"],"headless");
        assert_eq!(screenshot["page"]["page_id"],opened["page"]["page_id"]);
        assert_eq!(screenshot["visual_artifact"]["display_mode"],"headless");
        let task_id=crate::visual_artifacts::VisualContext::mcp(workspace.root()).task_id().to_owned();
        cleanup_after_finish(workspace.root(),&task_id,"completed").await;
        assert!(!sessions().lock().await.contains_key(&session_key(workspace.root(),&task_id)));
        let _=std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn completed_visible_browser_session_is_retained_until_explicit_close() {
        if browser_path().is_none() { return; }
        let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("browser-lifecycle-{}",crate::visual_artifacts::new_id()));std::fs::create_dir_all(&root).unwrap();
        let workspace=crate::tools::Workspace::new(&root).unwrap();let context=crate::visual_artifacts::VisualContext::mcp(workspace.root());
        let html=root.join("page.html");std::fs::write(&html,"<html><body>browser lifecycle</body></html>").unwrap();
        let url=reqwest::Url::from_file_path(&html).unwrap().to_string();
        execute_scoped(&workspace,&context,"browser_open",&json!({"url":url})).await.unwrap();
        let key=session_key(workspace.root(),context.task_id());let page=sessions().lock().await.get(&key).cloned().unwrap();
        {let mut guard=page.lock().await;guard.as_mut().unwrap().visible=true;}
        let before=page.lock().await.as_ref().unwrap().owner.clone();
        cleanup_after_finish(workspace.root(),context.task_id(),"completed").await;
        assert!(sessions().lock().await.contains_key(&key));
        assert_eq!(page.lock().await.as_ref().unwrap().owner,before);
        let closed=execute_scoped(&workspace,&context,"browser_close",&json!({})).await.unwrap();
        assert_eq!(closed["closed"],true);assert_eq!(closed["status"],"closed");
        assert!(!sessions().lock().await.contains_key(&key));
        let _=std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn upload_keeps_dom_node_on_one_connection_and_checks_change_with_unicode_path() {
        if browser_path().is_none() { return; }
        let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("browser-upload-{}",crate::visual_artifacts::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let html=root.join("upload.html");let sample=root.join("演示 文件.pptx");
        std::fs::write(&html,"<html><body><input id='pptx' type='file' style='display:none' accept='.pptx'><p id='event'></p><script>document.querySelector('#pptx').addEventListener('change',e=>document.querySelector('#event').textContent='changed:'+e.target.files[0].name)</script></body></html>").unwrap();
        std::fs::write(&sample,b"temporary pptx fixture").unwrap();
        let workspace=crate::tools::Workspace::new(&root).unwrap();let context=crate::visual_artifacts::VisualContext::mcp(workspace.root());
        let url=reqwest::Url::from_file_path(&html).unwrap().to_string();
        execute_scoped(&workspace,&context,"browser_open",&json!({"url":url})).await.unwrap();
        let assigned=execute_scoped(&workspace,&context,"browser_upload",&json!({"path":"演示 文件.pptx","selector":"#pptx"})).await.unwrap();
        assert_eq!(assigned["status"],"file_assigned","{assigned}");
        assert_eq!(assigned["file"]["name"],"演示 文件.pptx");
        assert_eq!(assigned["file"]["files_length"],1);
        assert_eq!(assigned["input"]["change_event_received"],true,"{assigned}");
        assert_eq!(assigned["document_loaded"]["status"],"not_verified");
        let observed=execute_scoped(&workspace,&context,"browser_wait",&json!({"text":"changed:演示 文件.pptx","timeout_ms":3000})).await.unwrap();
        assert_eq!(observed["status"],"matched","{observed}");
        let outside=execute_scoped(&workspace,&context,"browser_upload",&json!({"path":"../outside.pptx"})).await.unwrap();
        assert_eq!(outside["error_code"],"file_unavailable","{outside}");
        cleanup(workspace.root(),context.task_id()).await;
        let _=std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn upload_reports_multiple_inputs_and_diagnostics_capture_console_and_http_errors() {
        if browser_path().is_none() { return; }
        let listener=tokio::net::TcpListener::bind(("127.0.0.1",0)).await.unwrap();let address=listener.local_addr().unwrap();
        let server=tokio::spawn(async move {
            loop {
                let Ok((mut stream,_))=listener.accept().await else {break};
                tokio::spawn(async move {
                    let mut request=vec![0u8;8192];let n=stream.read(&mut request).await.unwrap_or(0);let request=String::from_utf8_lossy(&request[..n]);
                    let path=request.split_whitespace().nth(1).unwrap_or("/");
                    let (status,content_type,body)=if path=="/" {("200 OK","text/html; charset=utf-8",r#"<html><head><style>@font-face{font-family:missing;src:url('/missing-font.woff2')}body{font-family:missing}</style></head><body><input id='a' type='file'><input id='b' type='file'><p id='event'></p><button aria-label='Next slide' onclick="document.querySelector('#count').textContent=Number(document.querySelector('#count').textContent)+1">&gt;</button><span id='count'>0</span><script>document.querySelectorAll('input[type=file]').forEach(x=>x.addEventListener('change',e=>document.querySelector('#event').textContent='changed:'+e.target.files[0].name));console.error('fixture-console-error');fetch('/api-failure');fetch('/api/fonts?family=demo')</script></body></html>"#)}else if path=="/api-failure" {("503 Service Unavailable","application/json","{}" )}else if path.starts_with("/api/fonts") {("200 OK","application/json",r#"{"fonts":["demo"]}"#)}else if path=="/missing-font.woff2" {("404 Not Found","text/plain","missing")}else {("404 Not Found","text/plain","missing")};
                    let response=format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{body}",body.len());let _=stream.write_all(response.as_bytes()).await;
                });
            }
        });
        let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("browser-diagnostics-{}",crate::visual_artifacts::new_id()));std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("sample.pptx"),b"temporary fixture").unwrap();let workspace=crate::tools::Workspace::new(&root).unwrap();let context=crate::visual_artifacts::VisualContext::mcp(workspace.root());
        let opened=execute_scoped(&workspace,&context,"browser_open",&json!({"url":format!("http://{address}/")})).await.unwrap();assert_eq!(opened["display_mode"],"headless");
        let probe=inspect_test_page(&root,&context,"({has:!!window.__codexBrowserDiagnostics})").await.unwrap();assert_eq!(probe["has"],true,"navigation diagnostics were not installed");
        let ambiguous=execute_scoped(&workspace,&context,"browser_upload",&json!({"path":"sample.pptx"})).await.unwrap();assert_eq!(ambiguous["error_code"],"multiple_file_inputs","{ambiguous}");assert_eq!(ambiguous["details"]["candidates"].as_array().unwrap().len(),2);
        let assigned=execute_scoped(&workspace,&context,"browser_upload",&json!({"path":"sample.pptx","selector":"#a"})).await.unwrap();assert_eq!(assigned["status"],"file_assigned","{assigned}");
        let fonts=execute_scoped(&workspace,&context,"browser_wait",&json!({"font_status":"loaded","timeout_ms":5000})).await.unwrap();assert_eq!(fonts["status"],"matched","{fonts}");
        let diagnostics=execute_scoped(&workspace,&context,"browser_diagnostics",&json!({})).await.unwrap();
        assert!(diagnostics["errors"].as_array().unwrap().iter().any(|item|item["type"]=="console_error"),"{diagnostics}");
        assert!(diagnostics["errors"].as_array().unwrap().iter().any(|item|item["type"]=="http_error"&&item["status"]==503),"{diagnostics}");
        assert!(diagnostics["font_and_failed_resources"].as_array().unwrap().iter().any(|item|item["url"].as_str().unwrap_or("").contains("missing-font.woff2")),"{diagnostics}");
        assert!(diagnostics["font_faces"].as_array().unwrap().iter().any(|face|face["family"]=="missing"&&face["status"]=="error"),"{diagnostics}");
        let fonts_api=execute_scoped(&workspace,&context,"browser_diagnostics",&json!({"url_contains":"/api/fonts","category":"fetch"})).await.unwrap();
        let request=fonts_api["requests"].as_array().unwrap().iter().find(|item|item["source"]=="fetch").unwrap_or_else(||panic!("{fonts_api}"));
        assert_eq!(request["status"],200);assert_eq!(request["method"],"GET");assert!(request["time"].is_u64());
        assert!(!request["url"].as_str().unwrap().contains("family="),"query strings stay out of the record");
        assert!(fonts_api["empty_reason"].is_null());assert!(fonts_api["request_coverage"]["not_visible"].is_array());
        let none=execute_scoped(&workspace,&context,"browser_diagnostics",&json!({"url_contains":"/never-requested"})).await.unwrap();
        assert_eq!(none["requests"],json!([]));assert!(none["empty_reason"].as_str().unwrap().contains("does not prove"),"{none}");
        let read=execute_scoped(&workspace,&context,"browser_read",&json!({})).await.unwrap();
        let label=read["controls"].as_array().unwrap().iter().find(|control|control["tag"]=="button").unwrap()["text"].as_str().unwrap().to_owned();
        assert_eq!(label,"Next slide","browser_read lists the accessible label");
        let clicked=execute_scoped(&workspace,&context,"browser_click",&json!({"text":"next slide","times":3})).await.unwrap();
        assert_eq!(clicked["match"],"exact_label");assert_eq!(clicked["times"],3);
        let count=inspect_test_page(&root,&context,"({n:document.querySelector('#count').textContent})").await.unwrap();assert_eq!(count["n"],"3");
        let missing=execute_scoped(&workspace,&context,"browser_click",&json!({"text":"Export"})).await.unwrap_err().to_string();
        assert!(missing.contains("Next slide"),"the failure lists the visible labels: {missing}");
        cleanup(workspace.root(),context.task_id()).await;server.abort();let _=std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn live_editor_demo_shows_a_slide() {
        let Ok(url) = std::env::var("PPTX_URL") else { return; };
        if browser_path().is_none() { return; }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace=crate::tools::Workspace::new(root).unwrap();
        let opened = execute(&workspace,"browser_open",&json!({"url":url})).await.unwrap();
        assert!(opened["text"].as_str().unwrap_or("").contains("载入示例"), "{opened}");
        execute(&workspace,"browser_click",&json!({"selector":"#btn-welcome-demo"})).await.unwrap();
        let mut page = json!({});
        for _ in 0..40 {
            page = execute(&workspace,"browser_read",&json!({"expect_text":"Slide 1"})).await.unwrap();
            if page["matched"] == true { break; }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let shot = execute(&workspace,"browser_screenshot",&json!({})).await.unwrap();
        assert_eq!(page["matched"], true, "page={page} screenshot={shot}");
        cleanup(workspace.root(),crate::visual_artifacts::VisualContext::mcp(root).task_id()).await;
    }
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    const TABLE: [u8; 128] = {
        let mut table = [255u8; 128];
        let mut i = 0;
        while i < 26 { table[(b'A' + i) as usize] = i; table[(b'a' + i) as usize] = i + 26; i += 1; }
        i = 0;
        while i < 10 { table[(b'0' + i) as usize] = i + 52; i += 1; }
        table[b'+' as usize] = 62; table[b'/' as usize] = 63; table
    };
    let bytes = text.bytes().filter(|byte| *byte != b'=' && !byte.is_ascii_whitespace()).collect::<Vec<_>>();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 { break; }
        let values = chunk.iter().map(|byte| TABLE.get(*byte as usize).copied().unwrap_or(255)).collect::<Vec<_>>();
        ensure!(values.iter().all(|value| *value < 64), "screenshot is not valid base64");
        out.push((values[0] << 2) | (values[1] >> 4));
        if chunk.len() > 2 { out.push((values[1] << 4) | (values[2] >> 2)); }
        if chunk.len() > 3 { out.push((values[2] << 6) | values[3]); }
    }
    Ok(out)
}
