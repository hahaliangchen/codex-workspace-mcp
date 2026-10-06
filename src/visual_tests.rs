//! Protocol and ownership regressions use actual PNGs, HTTP and the task runtime.
use crate::{agent_service as agent,visual_artifacts as visual};
use axum::{Json,extract::State,routing::post};
use base64::{Engine,engine::general_purpose::STANDARD};
use serde_json::{Value,json};
use std::{io::Cursor,path::{Path,PathBuf},sync::{Arc,Mutex,atomic::{AtomicUsize,Ordering}},time::Duration};
use tokio_util::sync::CancellationToken;

struct TestRoot(PathBuf);
impl TestRoot {fn new()->Self {let root=std::env::temp_dir().join(format!("codex-visual-test-{}",visual::new_id()));std::fs::create_dir_all(&root).unwrap();Self(root)}}
impl Drop for TestRoot {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
fn context(task:&str)->visual::VisualContext {visual::VisualContext {identity:json!({"task_id":task,"request_id":1,"work_id":"w","node_id":"n","revision":1,"plan_revision":1}),..Default::default()}}
fn png(width:u32,height:u32)->Vec<u8> {let img=image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(width,height,image::Rgba([210,40,90,255])));let mut bytes=Vec::new();img.write_to(&mut Cursor::new(&mut bytes),image::ImageFormat::Png).unwrap();bytes}
fn task(root:&Path,id:&str) {agent::open_db(root).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'visual test','fake-model','running',0,0)",[id]).unwrap();}
fn supported(state:&mut agent::AgentServiceState) {state.visual.capabilities.entry(String::new()).or_default().insert("fake-model".into(),visual::ImageCapability::Supported);}
fn scheduler_context(task:&str,scheduler:&crate::work_scheduler::WorkScheduler,work:&str)->visual::VisualContext {
    let frame=scheduler.frames.get(work).unwrap();visual::VisualContext {identity:crate::observer_service::identity(task,scheduler,work),execution_epoch:frame.epoch,
        related_source_versions:json!(&frame.versions),current_page:frame.browser_page.clone(),host_current_artifact_ids:frame.current_visual_artifact_ids.clone(),..Default::default()}
}
fn images(body:&Value)->Vec<Vec<u8>> {body["messages"].as_array().into_iter().flatten().flat_map(|message|message["content"].as_array().into_iter().flatten()).filter_map(|block|block["image_url"]["url"].as_str()).map(|url|STANDARD.decode(url.strip_prefix("data:image/png;base64,").unwrap()).unwrap()).collect()}
fn manifest(body:&Value)->Value {body["messages"].as_array().into_iter().flatten().filter_map(|message|message["content"].as_str()).find_map(|text|text.strip_prefix("Host visual dispatch: ")).map(|text|serde_json::Deserializer::from_str(text).into_iter::<Value>().next().unwrap().unwrap()).unwrap_or(Value::Null)}
fn check(dispatch:&Value)->Value {
    let selected=dispatch["images"].as_array().into_iter().flatten().collect::<Vec<_>>();
    let current=selected.iter().rev().find(|image|image["current_result"]==true).or_else(||selected.last());
    let observed=current.map(|image|vec![json!({"artifact_id":image["artifact_id"],"region":"viewport","fact":"fixture image was supplied"})]).unwrap_or_default();
    json!({"artifact_ids":selected.iter().map(|image|image["artifact_id"].clone()).collect::<Vec<_>>(),
    "checked_goal":dispatch["checked_goal"],"expected_visible_result":"a visible slide fixture","request_trace_id":dispatch["request_trace_id"],"assessment":"pass",
    "observed_facts":observed,"limitations":["scripted model validates transport, not visual understanding"]})
}
fn fallback_check(dispatch:&Value)->Value {
    let service=&dispatch["visual_service_result"];
    json!({"artifact_ids":service["artifact_ids"],"checked_goal":service["checked_goal"],"expected_visible_result":service["expected_visible_result"],
        "request_trace_id":dispatch["request_trace_id"],"assessment":service["assessment"],"observed_facts":service["observed_facts"],"issues":service["issues"],
        "limitations":service["limitations"],"visual_service_result_id":dispatch["visual_service_result_id"]})
}

#[test]
fn visual_artifacts_are_immutable_and_require_task_request_and_explicit_upstream_scope() {
    let root=TestRoot::new();let owner=context("a");let bytes=png(120,90);
    let first=visual::save(&root.0,&owner,json!({"page_id":"a","page_epoch":1}),&bytes).unwrap();
    let second=visual::save(&root.0,&owner,json!({"page_id":"a","page_epoch":2}),&bytes).unwrap();
    assert_ne!(first["artifact_id"],second["artifact_id"]);assert_ne!(first["workspace_relative_path"],second["workspace_relative_path"]);
    let id=first["artifact_id"].as_str().unwrap();assert_eq!(visual::read(&root.0,"a",id).unwrap().1,bytes);
    assert!(visual::read(&root.0,"b",id).is_err());assert!(visual::metadata(&root.0,"a","../../fake.png").is_err());
    let mut other=owner.clone();other.identity["revision"]=json!(2);assert!(visual::view(&root.0,&other,id).is_err());
    other.allowed_artifact_ids.push(id.into());assert!(visual::view(&root.0,&other,id).is_ok());
    other.identity["request_id"]=json!(2);assert!(visual::view(&root.0,&other,id).is_err());
    std::fs::write(root.0.join(first["workspace_relative_path"].as_str().unwrap()),png(121,90)).unwrap();assert!(visual::read(&root.0,"a",id).unwrap_err().to_string().contains("hash changed"));
}

#[test]
fn visual_budget_preserves_the_original_and_records_the_scaled_input_hash() {
    let root=TestRoot::new();let context=context("a");let bytes=png(2400,2100);
    let artifact=visual::save(&root.0,&context,json!({}),&bytes).unwrap();let id=artifact["artifact_id"].as_str().unwrap();
    assert!(visual::image_block(&root.0,&context,id,true).is_err());
    let (block,sent)=visual::image_block(&root.0,&context,id,false).unwrap();assert_eq!(sent["mode"],"scaled");
    let data=STANDARD.decode(block["image_url"]["url"].as_str().unwrap().split_once(',').unwrap().1).unwrap();
    assert_eq!(sent["input_hash"],visual::hash(&data));assert!(sent["input_width"].as_u64().unwrap()*sent["input_height"].as_u64().unwrap()<=visual::MAX_INPUT_PIXELS);
    assert_eq!(visual::read(&root.0,"a",id).unwrap().1,bytes);
    assert!(visual::save(&root.0,&context,json!({}),b"not an image").is_err());
}

#[tokio::test]
async fn visual_dispatch_capability_budget_restore_and_result_binding_are_explicit() {
    let root=TestRoot::new();let ctx=context("task");task(&root.0,"task");
    let ids=(0..3).map(|_|visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap());
    let mut body=json!({"model":"fake-model","messages":[]});
    let unknown=visual::prepare_request(&state,"worker","fake-model",&ctx,&ids,"check slide",&mut body).await.unwrap();
    assert_eq!(unknown["status"],"unknown_capability");assert!(images(&body).is_empty());assert!(unknown["images"].as_array().unwrap().is_empty());
    supported(&mut state);let mut body=json!({"model":"fake-model","messages":[]});
    let sent=visual::prepare_request(&state,"worker","fake-model",&ctx,&ids,"check slide",&mut body).await.unwrap();
    assert_eq!(images(&body).len(),2);assert_eq!(sent["omitted_by_image_budget"],1);
    for (bytes,meta) in images(&body).iter().zip(sent["images"].as_array().unwrap()) {assert_eq!(meta["input_hash"],visual::hash(bytes));assert_eq!(image::load_from_memory(bytes).unwrap().width(),80);}
    let valid=check(&sent);assert!(visual::validate_check(&root.0,&ctx,&valid,&[sent.clone()],"worker").is_ok());
    let mut invented=valid.clone();invented["observed_facts"][0]["artifact_id"]=json!(ids[2]);assert!(visual::validate_check(&root.0,&ctx,&invented,&[sent.clone()],"worker").is_err());
    assert!(visual::validate_check(&root.0,&ctx,&valid,&[],"worker").is_err());
    let mut stale=ctx.clone();stale.execution_epoch=1;assert!(visual::validate_check(&root.0,&stale,&valid,&[sent.clone()],"worker").is_err());
    let mut wrong=valid.clone();wrong["artifact_ids"]=json!([ids[2]]);assert!(visual::validate_check(&root.0,&ctx,&wrong,&[sent.clone()],"worker").is_err());
    let saved=serde_json::to_string(&ids).unwrap();assert!(!saved.contains("base64"));let restored:Vec<String>=serde_json::from_str(&saved).unwrap();
    let mut recovered=json!({"messages":[]});visual::prepare_request(&state,"worker","fake-model",&ctx,&restored,"check slide",&mut recovered).await.unwrap();assert_eq!(images(&recovered),images(&body));
    let mcp=visual::mcp_result(&root.0,&ctx,visual::view(&root.0,&ctx,&ids[0]).unwrap()).unwrap();assert_eq!(mcp["content"][1]["type"],"image");assert_eq!(STANDARD.decode(mcp["content"][1]["data"].as_str().unwrap()).unwrap(),png(80,60));
}

#[tokio::test]
async fn visual_pass_requires_a_current_image_and_a_fact_from_it() {
    let root=TestRoot::new();task(&root.0,"task");
    let mut before=context("task");before.execution_epoch=0;before.related_source_versions=json!({"src/app.ts":"v1"});
    let old=visual::save(&root.0,&before,json!({"browser_session_id":"session","page_id":"page","page_epoch":1}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let mut current=before.clone();current.execution_epoch=1;current.current_page=json!({"browser_session_id":"session","page_id":"page","page_epoch":2});
    let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap());supported(&mut state);

    let mut body=json!({"messages":[]});let old_only=visual::prepare_request(&state,"worker","fake-model",&current,&[old.clone()],"check slide",&mut body).await.unwrap();
    assert_eq!(old_only["images"][0]["current_result"],false,"restoring the pre-navigation screenshot does not refresh its host binding: {old_only}");
    assert!(visual::validate_check(&root.0,&current,&check(&old_only),&[old_only],"worker").is_err(),"an old image alone cannot prove the current page");

    let latest=visual::save(&root.0,&current,json!({"browser_session_id":"session","page_id":"page","page_epoch":2}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let mut body=json!({"messages":[]});let comparison=visual::prepare_request(&state,"worker","fake-model",&current,&[old.clone(),latest.clone()],"check slide",&mut body).await.unwrap();
    assert_eq!(comparison["images"][0]["current_result"],false);assert_eq!(comparison["images"][1]["current_result"],true);
    let mut valid=check(&comparison);valid["observed_facts"].as_array_mut().unwrap().push(json!({"artifact_id":old,"region":"before","fact":"historical comparison image"}));
    assert!(visual::validate_check(&root.0,&current,&valid,&[comparison.clone()],"worker").is_ok(),"before/after comparison remains valid when a fact cites the current result");
    valid["observed_facts"]=json!([{"artifact_id":old,"region":"before","fact":"historical comparison image"}]);
    assert!(visual::validate_check(&root.0,&current,&valid,&[comparison],"worker").is_err(),"historical facts alone cannot establish the current result");

    let mut changed=current.clone();changed.execution_epoch=2;changed.related_source_versions=json!({"src/app.ts":"v2"});
    let mut body=json!({"messages":[]});let restored=visual::prepare_request(&state,"worker","fake-model",&changed,&[latest],"check slide",&mut body).await.unwrap();
    assert_eq!(restored["images"][0]["current_result"],false,"restoring a screenshot after a source version change does not refresh its host binding");
    assert!(visual::validate_check(&root.0,&changed,&check(&restored),&[restored],"worker").is_err());
}

#[tokio::test]
async fn fresh_capture_after_async_page_change_is_current_and_old_or_source_stale_images_are_rejected() {
    let root=TestRoot::new();task(&root.0,"task");
    let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap());supported(&mut state);
    let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"current slide visible","completion":"output","visual_goal":"check slide"}]}),false,false).unwrap();
    let mut ctx=scheduler_context("task",&scheduler,"w");
    let opened=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_open",&json!({"url":fixture(&root.0)})).await.unwrap();scheduler.observe("browser_open",&json!({}),&opened,false);
    ctx=scheduler_context("task",&scheduler,"w");
    let old=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await.unwrap();scheduler.observe("browser_screenshot",&json!({}),&old,false);
    let old_id=old["artifact_id"].as_str().unwrap().to_owned();

    ctx=scheduler_context("task",&scheduler,"w");
    crate::browser_control::inspect_test_page(&root.0,&ctx,"(()=>{document.body.insertAdjacentHTML('beforeend','<h1>ASYNC RENDER FINISHED</h1>');return true})()").await.unwrap();
    let fresh=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await.unwrap();scheduler.observe("browser_screenshot",&json!({}),&fresh,false);
    let fresh_id=fresh["artifact_id"].as_str().unwrap().to_owned();
    let fresh_ctx=scheduler_context("task",&scheduler,"w");
    let mut body=json!({"messages":[]});let dispatch=visual::prepare_request(&state,"worker","fake-model",&fresh_ctx,&[old_id.clone(),fresh_id.clone()],"check slide",&mut body).await.unwrap();
    assert_eq!(dispatch["images"][0]["current_result"],false,"the pre-update screenshot remains historical");
    assert_eq!(dispatch["images"][1]["current_result"],true,"the screenshot that discovered the async DOM update is host-accepted for the received page epoch");
    let artifact=visual::metadata(&root.0,"task",&fresh_id).unwrap();
    assert_eq!(artifact["execution_epoch"],json!(fresh["visual_artifact"]["execution_epoch"]),"acceptance is a separate host reference; screenshot metadata remains immutable");
    assert!(fresh_ctx.execution_epoch>artifact["execution_epoch"].as_u64().unwrap() as usize,"scheduler advanced after receiving the screenshot");

    let mut versions=scheduler.versions();versions.insert("renderer.ts".into(),"changed-source-hash".into());scheduler.update_versions(&versions);
    let changed_ctx=scheduler_context("task",&scheduler,"w");let mut body=json!({"messages":[]});
    let restored=visual::prepare_request(&state,"worker","fake-model",&changed_ctx,&[fresh_id.clone()],"check slide",&mut body).await.unwrap();
    assert_eq!(restored["images"][0]["current_result"],false,"a host-current screenshot loses current status after source changes");
    crate::browser_control::cleanup(&root.0,"task").await;
}

#[tokio::test]
async fn failed_capture_revokes_the_previous_images_current_result_status() {
    let root=TestRoot::new();task(&root.0,"task");
    let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"current slide visible","completion":"output","visual_goal":"check slide"}]}),false,false).unwrap();
    let frame=scheduler.frame_mut().unwrap();frame.browser_page=json!({"browser_session_id":"session","page_id":"page","page_epoch":1});
    let ctx=scheduler_context("task",&scheduler,"w");let old_id=visual::save(&root.0,&ctx,json!({"browser_session_id":"session","page_id":"page","page_epoch":1}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    scheduler.frame_mut().unwrap().current_visual_artifact_ids=vec![old_id.clone()];
    scheduler.observe("browser_screenshot",&json!({}),&json!({"error":"page changed during capture; visual ownership/state is uncertain"}),true);
    let after=scheduler_context("task",&scheduler,"w");let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap());supported(&mut state);
    let mut body=json!({"messages":[]});let dispatch=visual::prepare_request(&state,"worker","fake-model",&after,&[old_id],"check slide",&mut body).await.unwrap();
    assert_eq!(dispatch["images"][0]["current_result"],false,"an uncertain capture invalidates prior visual currentness");
}

#[tokio::test]
async fn visual_fallback_uncertain_or_incomplete_results_cannot_be_promoted_to_pass() {
    for incomplete in [false,true] {
        let root=TestRoot::new();task(&root.0,"task");let ctx=context("task");
        let id=visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
        let service_id=id.clone();
        let app=axum::Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>|{let service_id=service_id.clone();async move {
            let system=body.pointer("/messages/0/content").and_then(Value::as_str).unwrap();
            let trace=system.split("Host request_trace_id=").nth(1).unwrap().split(" and artifact_ids=").next().unwrap();
            let response=if incomplete {json!({"assessment":"pass","observed_facts":[]})} else {json!({"request_trace_id":trace,"checked_goal":"check slide","artifact_ids":[service_id],
                "expected_visible_result":"a visible slide fixture","assessment":"uncertain","observed_facts":[],"issues":[],"limitations":["service could not determine the result"]})};
            Json(json!({"choices":[{"message":{"content":response.to_string()}}]}))
        }}));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        let mut state=agent::tests::flow_test_state(&root.0,addr);state.visual.capabilities.entry(String::new()).or_default().insert("fake-model".into(),visual::ImageCapability::Unsupported);
        state.visual.fallback=Some(visual::VisualRoute{provider:"explicit-vision".into(),model:"image-model".into(),url:format!("http://{addr}/v1"),api_key:String::new()});
        let mut body=json!({"messages":[]});let dispatch=visual::prepare_request(&state,"worker","fake-model",&ctx,&[id],"check slide",&mut body).await.unwrap();
        assert!(images(&body).is_empty());
        let mut forged=check(&dispatch);forged["visual_service_result_id"]=dispatch["visual_service_result_id"].clone();
        assert!(visual::validate_check(&root.0,&ctx,&forged,&[dispatch.clone()],"worker").is_err(),"uncertain or incomplete service output cannot support a Worker pass");
        if incomplete {assert_eq!(dispatch["status"],"unavailable");} else {assert_eq!(dispatch["visual_service_result"]["assessment"],"uncertain");}
        server.abort();
    }
}

#[tokio::test]
async fn saved_visual_pass_expires_when_source_versions_change_before_delivery() {
    let root=TestRoot::new();task(&root.0,"task");
    let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"current slide visible","completion":"output","visual_goal":"check slide"}]}),false,false).unwrap();
    let mut ctx=visual::VisualContext {identity:crate::observer_service::identity("task",&scheduler,"w"),..Default::default()};ctx.related_source_versions=json!({});
    let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap());supported(&mut state);
    let id=visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let mut body=json!({"messages":[]});let sent=visual::prepare_request(&state,"worker","fake-model",&ctx,&[id],"check slide",&mut body).await.unwrap();
    let saved=visual::validate_check(&root.0,&ctx,&check(&sent),&[sent],"worker").unwrap();scheduler.frame_mut().unwrap().visual_check_result=saved.clone();
    let mut versions=scheduler.versions();versions.insert("renderer.ts".into(),"changed-source-hash".into());scheduler.update_versions(&versions);
    assert!(scheduler.frame().unwrap().visual_check_result.is_null(),"source version update clears the cached visual result");
    scheduler.frame_mut().unwrap().visual_check_result=saved;
    assert!(scheduler.return_work(&json!({"summary":"rendering verified"})).is_err(),"delivery rejects a result bound to the previous source version");
}

#[derive(Default)]
struct Script {requests:Mutex<Vec<Value>>,worker_calls:AtomicUsize,organizer_calls:AtomicUsize,url:String}
fn tool(id:&str,name:&str,args:Value)->Value {json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})}
async fn chat(State(script):State<Arc<Script>>,Json(body):Json<Value>)->Json<Value> {
    let is_organizer=body["tools"].as_array().into_iter().flatten().any(|tool|tool["function"]["name"]=="organize_work");
    let worker=body["tools"].as_array().into_iter().flatten().any(|tool|tool["function"]["name"]=="yield_work");
    let dispatch=manifest(&body);script.requests.lock().unwrap().push(body.clone());
    let message=if is_organizer {
        let first=script.organizer_calls.fetch_add(1,Ordering::Relaxed)==0;
        json!({"role":"assistant","content":null,"tool_calls":[tool("organize","organize_work",if first {json!({"action":"work","reason":"verify pixels","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"bound visual result","completion":"output","visual_goal":"check slide"}]})}else{json!({"action":"finish","reason":"delivered verified transport","summary":"visual transport complete"})})]})
    }else if worker {
        let first=script.worker_calls.fetch_add(1,Ordering::Relaxed)==0;
        json!({"role":"assistant","content":null,"tool_calls":if first {vec![tool("open","browser_open",json!({"url":script.url})),tool("shot","browser_screenshot",json!({}))]}else{vec![tool("yield","yield_work",json!({"summary":"visual transport checked","visual_check_result":check(&dispatch)}))]}})
    }else if body["stream"]==true {json!({"role":"assistant","content":"Visual transport complete"})}
    else {json!({"role":"assistant","content":json!({"assessment":"on_track","summary":"review","visual_check_result":if dispatch["status"]=="direct" {check(&dispatch)}else{Value::Null}}).to_string()})};
    Json(json!({"choices":[{"message":message}]}))
}
async fn server(script:Arc<Script>)->(std::net::SocketAddr,tokio::task::JoinHandle<()>) {
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let app=axum::Router::new().route("/v1/chat/completions",post(chat)).with_state(script);
    (address,tokio::spawn(async move{axum::serve(listener,app).await.unwrap();}))
}
fn fixture(root:&Path)->String {let path=root.join("slide.html");std::fs::write(&path,"<html><body><canvas id='slide' width='800' height='450'></canvas><script>const c=document.querySelector('canvas').getContext('2d');c.fillStyle='#bb2255';c.fillRect(0,0,800,450);c.fillStyle='white';c.font='40px sans-serif';c.fillText('Canvas slide',40,90)</script></body></html>").unwrap();format!("file:///{}",path.to_string_lossy().replace('\\',"/"))}

#[tokio::test]
async fn visual_worker_runtime_sends_real_images_without_an_observer_or_extra_receipt_round() {
    let root=TestRoot::new();task(&root.0,"task");let script=Arc::new(Script {url:fixture(&root.0),..Default::default()});let (address,server)=server(script.clone()).await;
    let mut state=agent::tests::flow_test_state(&root.0,address);supported(&mut state);
    tokio::time::timeout(Duration::from_secs(20),agent::run_task(state,"task".into(),"fake-model".into(),"Inspect canvas rendering".into(),6,CancellationToken::new(),false,1,vec![],false,false)).await.unwrap().unwrap();
    let requests=script.requests.lock().unwrap();let visual_requests=requests.iter().filter(|body|!images(body).is_empty()).collect::<Vec<_>>();assert_eq!(visual_requests.len(),1);
    let dispatch=manifest(visual_requests[0]);let id=dispatch["images"][0]["artifact_id"].as_str().unwrap();assert_eq!(visual::read(&root.0,"task",id).unwrap().1,images(visual_requests[0])[0]);
    assert_eq!(script.worker_calls.load(Ordering::Relaxed),2,"capture then normal visual/yield request; no receipt-only Worker round");
    assert_eq!(agent::open_db(&root.0).unwrap().query_row("SELECT COUNT(*) FROM agent_task_events WHERE kind='tool/call' AND json_extract(data,'$.name')='respond_observer'",[],|row|row.get::<_,i64>(0)).unwrap(),0);server.abort();
}

#[test]
fn visual_protocol_adapters_preserve_image_content_blocks() {
    let data=STANDARD.encode(png(2,2));let url=format!("data:image/png;base64,{data}");
    let response=json!({"messages":crate::format_translate::responses_body_to_openai_chat_messages(&json!({"model":"m","input":[{"role":"user","content":[{"type":"input_text","text":"inspect"},{"type":"input_image","image_url":url}]}]}))});
    assert_eq!(images(&response),vec![png(2,2)]);
    let anthropic=crate::format_translate::anthropic_to_openai(&json!({"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"inspect"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}]}]}));
    assert_eq!(images(&anthropic),vec![png(2,2)]);
}

#[tokio::test]
async fn visual_fallback_is_one_explicit_service_call_and_never_claims_role_image_input() {
    let root=TestRoot::new();task(&root.0,"task");let ctx=context("task");
    let id=visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let calls=Arc::new(AtomicUsize::new(0));let count=calls.clone();
    let service_id=id.clone();
    let app=axum::Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>|{let count=count.clone();let service_id=service_id.clone();async move {
        count.fetch_add(1,Ordering::Relaxed);assert_eq!(images(&body).len(),1);
        let system=body.pointer("/messages/0/content").and_then(Value::as_str).unwrap();
        let trace=system.split("Host request_trace_id=").nth(1).unwrap().split(" and artifact_ids=").next().unwrap();
        Json(json!({"choices":[{"message":{"content":json!({"request_trace_id":trace,"checked_goal":"check slide","artifact_ids":[service_id],
            "expected_visible_result":"a visible slide fixture","assessment":"pass","observed_facts":[{"artifact_id":service_id,"region":"viewport","fact":"visible pink fixture"}],"issues":[],"limitations":["service evidence only"]}).to_string()}}]}))
    }}));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
    let mut state=agent::tests::flow_test_state(&root.0,addr);
    state.visual.capabilities.entry(String::new()).or_default().insert("fake-model".into(),visual::ImageCapability::Unsupported);
    state.visual.fallback=Some(visual::VisualRoute{provider:"explicit-vision".into(),model:"image-model".into(),url:format!("http://{addr}/v1"),api_key:String::new()});
    let mut body=json!({"messages":[]});let dispatch=visual::prepare_request(&state,"worker","fake-model",&ctx,&[id.clone()],"check slide",&mut body).await.unwrap();
    assert_eq!(dispatch["status"],"fallback");assert!(images(&body).is_empty());assert_eq!(calls.load(Ordering::Relaxed),1);
    let result=visual::validate_check(&root.0,&ctx,&fallback_check(&dispatch),&[dispatch.clone()],"worker").unwrap();assert_eq!(result["input_mode"],"fallback");assert_eq!(result["visual_service_route"]["provider"],"explicit-vision");
    server.abort();state.visual.fallback.as_mut().unwrap().url="http://127.0.0.1:1/v1".into();
    let mut body=json!({"messages":[]});let failed=visual::prepare_request(&state,"observer","fake-model",&ctx,&[id],"check slide",&mut body).await.unwrap();
    assert_eq!(failed["status"],"unavailable");assert!(images(&body).is_empty());assert!(visual::validate_check(&root.0,&ctx,&check(&failed),&[failed],"observer").is_err());
}

#[tokio::test]
async fn visual_browser_pages_are_task_owned_and_observer_cannot_follow_replaced_instances() {
    let root=TestRoot::new();let workspace=crate::tools::Workspace::new(&root.0).unwrap();let a=context("a");let b=context("b");
    let a_url=fixture(&root.0);let other=root.0.join("other.html");std::fs::write(&other,"<html><body>Task B</body></html>").unwrap();let b_url=format!("file:///{}",other.to_string_lossy().replace('\\',"/"));
    let a_args=json!({"url":a_url});let b_args=json!({"url":b_url});
    let (first,second)=tokio::join!(crate::browser_control::execute_scoped(&workspace,&a,"browser_open",&a_args),crate::browser_control::execute_scoped(&workspace,&b,"browser_open",&b_args));
    let first=first.unwrap();let second=second.unwrap();assert_ne!(first["page"]["page_id"],second["page"]["page_id"]);
    let shot=crate::browser_control::execute_scoped(&workspace,&a,"browser_screenshot",&json!({})).await.unwrap();assert_eq!(shot["visual_artifact"]["url"],a_url);
    assert!(crate::browser_control::observe_page_snapshot(&root.0,&a,&first["page"]).await.is_ok(),"normal and canonical workspace paths refer to the same owned session");
    assert!(crate::browser_control::observe_page_snapshot(&root.0,&b,&first["page"]).await.is_err());
    let mut replacement=a.clone();replacement.identity["request_id"]=json!(2);replacement.identity["revision"]=json!(2);
    assert!(crate::browser_control::execute_scoped(&workspace,&replacement,"browser_read",&json!({})).await.is_err());
    crate::browser_control::execute_scoped(&workspace,&replacement,"browser_open",&json!({"url":b_url})).await.unwrap();
    assert!(crate::browser_control::observe_page_snapshot(&root.0,&a,&first["page"]).await.is_err());
    assert_eq!(visual::read(&root.0,"a",shot["artifact_id"].as_str().unwrap()).unwrap().0["identity"],a.identity);
    crate::browser_control::cleanup(&root.0,"a").await;crate::browser_control::cleanup(&root.0,"b").await;
}

#[tokio::test]
async fn visual_observer_reuses_worker_images_or_captures_once_without_touching_other_pages() {
    for reuse in [true,false] {
        let root=TestRoot::new();task(&root.0,"task");let script=Arc::new(Script::default());let (address,server)=server(script.clone()).await;
        let mut state=agent::tests::flow_test_state(&root.0,address);supported(&mut state);state.observer_enabled=true;state.observer_provider_url=state.provider_url.clone();
        let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
        scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"visible canvas","completion":"output","visual_goal":"check slide"}]}),false,false).unwrap();
        let mut ctx=scheduler_context("task",&scheduler,"w");
        let opened=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_open",&json!({"url":fixture(&root.0)})).await.unwrap();scheduler.observe("browser_open",&json!({}),&opened,false);
        if reuse {
            ctx=scheduler_context("task",&scheduler,"w");
            let shot=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await.unwrap();scheduler.observe("browser_screenshot",&json!({}),&shot,false);
            ctx=scheduler_context("task",&scheduler,"w");
            let interaction=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_click",&json!({"selector":"canvas"})).await.unwrap();scheduler.observe("browser_click",&json!({}),&interaction,false);
            ctx=scheduler_context("task",&scheduler,"w");
            let shot=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await.unwrap();scheduler.observe("browser_screenshot",&json!({}),&shot,false);
        }
        let input=crate::observer_service::observation("task",&scheduler,"w",1,1,"handoff","check slide",&json!({}),Value::Null,json!({"summary":"canvas ready"}),Value::Null);
        crate::observer_service::commit(&root.0,"task",json!({"commit_id":"visual_handoff"}),vec![input]).await.unwrap();
        let mut observer=crate::observer_service::ObserverSession::start(state.clone(),"fake-model".into(),"task".into(),&CancellationToken::new());observer.set_scope(&scheduler,"check slide");
        let reviews=tokio::time::timeout(Duration::from_secs(8),async {loop {let reviews=observer.reviews().await.unwrap();if !reviews.is_empty(){return reviews;}
            let count=agent::open_db(&root.0).unwrap().query_row("SELECT COUNT(*) FROM agent_observations WHERE status='completed'",[],|row|row.get::<_,i64>(0)).unwrap();if count==1{return vec![];}tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
        assert!(reviews.is_empty());observer.finish("completed").await.unwrap();
        let requests=script.requests.lock().unwrap();assert_eq!(requests.len(),1);assert_eq!(images(&requests[0]).len(),if reuse {2}else{1});
        if reuse {let dispatch=manifest(&requests[0]);assert!(dispatch["images"][0]["page_epoch"].as_u64().unwrap()<dispatch["images"][1]["page_epoch"].as_u64().unwrap());}
        let conn=agent::open_db(&root.0).unwrap();assert_eq!(conn.query_row("SELECT COUNT(*) FROM agent_visual_artifacts",[],|row|row.get::<_,i64>(0)).unwrap(),if reuse {2}else{1},"before/after reuse causes no recapture; missing input captures once");
        let result:String=conn.query_row("SELECT result FROM agent_observations",[],|row|row.get(0)).unwrap();let result:Value=serde_json::from_str(&result).unwrap();assert_eq!(result["visual_check_result"]["assessment"],"pass");
        assert_eq!(result["visual_capture"]["calls"],if reuse {0}else{1});
        crate::browser_control::cleanup(&root.0,"task").await;server.abort();
    }
}

// Explicit opt-in: network calls to the configured main provider. No global setting changes.
// Run `node scripts/visual_acceptance_harness.mjs`, then this ignored test.
#[tokio::test]
#[ignore = "real provider acceptance; requires the locally built PPT demo and editor harness"]
async fn real_ppt_visual_acceptance() -> anyhow::Result<()> {
    let workspace=std::env::current_dir()?;let root=workspace.join(".codex-workspace-mcp").join(format!("visual-acceptance-{}",visual::new_id()));std::fs::create_dir_all(&root)?;
    let demo=std::env::var_os("PPT_DEMO_DIST").map(PathBuf::from).unwrap_or_else(||workspace.join("../pptx-editor-engine/dist"));
    anyhow::ensure!(demo.join("index.html").is_file(),"build the existing PPT demo first");
    let harness=workspace.join(".codex-workspace-mcp/visual-acceptance-editor");anyhow::ensure!(harness.join("index.html").is_file(),"build the existing SlideEditor harness first");
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;let addr=listener.local_addr()?;
    let app=axum::Router::new().nest_service("/editor",tower_http::services::ServeDir::new(harness)).fallback_service(tower_http::services::ServeDir::new(demo));
    let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
    let mut state=agent::tests::flow_test_state(&root,addr);let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=state.model_map.get(&state.default_model).cloned().unwrap_or(state.default_model.clone());let declared=state.visual.capability(&state.provider_name,&model);
    task(&root,"real-ppt");let mut ctx=context("real-ppt");ctx.identity["node_id"]=json!("load_demo");
    // An opt-in bounded capability experiment, cached only for this acceptance run.
    state.visual.capabilities.entry(state.provider_name.clone()).or_default().insert(model.clone(),visual::ImageCapability::Supported);
    let probe=std::env::var_os("VISUAL_ACCEPTANCE_CAPABILITY_PROBED").is_none();
    let mut report=json!({"route":{"provider":state.provider_name,"model":model},"declared_image_capability":declared,"capability_probe":probe,"global_config_changed":false,
        "driver":"host browser scenario with real role model image judgments; autonomous Worker transport is covered separately","worker_calls":0,"observer_calls":0,"visual_service_calls":0,"screenshots":0,"image_reuse":0,"extra_worker_receipt_rounds":0,"cases":[]});
    crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_open",&json!({"url":format!("http://{addr}/")})).await?;
    crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_click",&json!({"selector":"#btn-welcome-demo"})).await?;
    for _ in 0..30 {
        let ready=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_read",&json!({"expect_text":"Slide 1"})).await?;
        if ready["matched"]==true {break;}tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let read=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_read",&json!({})).await?;
    let page_debug=crate::browser_control::inspect_test_page(&root,&ctx,"({resources:performance.getEntriesByType('resource').map(r=>({name:r.name,bytes:r.transferSize})),slide_count:document.querySelectorAll('#slide-list .slide-item').length})").await?;
    let shot=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await?;report["screenshots"]=json!(1);
    let id=shot["artifact_id"].as_str().unwrap().to_owned();
    let load_goal="Inspect the loaded PPT demo screenshot. Describe the actual slide content, shapes, colors and any empty or clipped regions; assess whether a slide is rendered. DOM text alone cannot establish this.";
    let first=real_visual_judgment(&state,&model,&ctx,&[id],load_goal,probe).await;report["worker_calls"]=json!(1);
    let probe_ok=first["request_succeeded"]==true;report["probe_image_request_accepted"]=json!(probe_ok);report["cases"].as_array_mut().unwrap().push(json!({"case":"load_demo","dom":read,"page_debug":page_debug,"artifact":shot["visual_artifact"],"worker":first}));
    if probe_ok {
        ctx.identity["node_id"]=json!("delete_node");ctx.identity["work_id"]=json!("delete_node");
        crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_open",&json!({"url":format!("http://{addr}/editor/")})).await?;
        let before=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await?;
        let before_state=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_read",&json!({"expect_text":"target_count=1"})).await?;
        crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_click",&json!({"selector":"#delete-target"})).await?;
        crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_press_key",&json!({"key":"Delete"})).await?;
        let after_state=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_read",&json!({"expect_text":"target_count=0; edits=1"})).await?;
        let node_state=crate::browser_control::inspect_test_page(&root,&ctx,"({target_count:document.querySelectorAll('#delete-target').length,blue_circle_count:document.querySelectorAll('svg circle').length,heading_present:[...document.querySelectorAll('svg text')].some(t=>t.textContent==='Quarterly product review')})").await?;
        let after=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await?;report["screenshots"]=json!(3);
        let ids=vec![before["artifact_id"].as_str().unwrap().to_owned(),after["artifact_id"].as_str().unwrap().to_owned()];
        let goal="Compare the before and after PPT editor images, in that order. Check whether the pink rectangular visual node was removed while the blue circle and heading remain. Describe what you actually see in each image. The host separately records target_count 1 to 0 and an onChange callback; pixels alone cannot prove editing persistence.";
        let worker=real_visual_judgment(&state,&model,&ctx,&ids,goal,false).await;report["worker_calls"]=json!(2);
        // Use the same explicitly probed main route for a separate Observer context.
        state.observer_provider=state.provider_name.clone();state.observer_provider_url=state.provider_url.clone();state.observer_api_key=state.api_key.clone();
        let context=json!({"identity":ctx.identity,"request":{"goal":goal},"visual_artifacts":[before["visual_artifact"],after["visual_artifact"]],"delivery":{"before_state":before_state,"after_state":after_state}});
        let start=std::time::Instant::now();let result=tokio::time::timeout(Duration::from_secs(12),agent::observer_json_response(&state,&model,
            "Review this visual delivery independently and concisely. Output JSON with assessment on_track|adjust|uncertain, summary and visual_check_result. In visual_check_result cite the Host visual dispatch request_trace_id, exact checked_goal and artifact_ids; expected_visible_result; assessment pass|issue|uncertain|unavailable; record observed_facts with artifact_id, region, fact and limitations. Only assess visible facts.",context,1400,"real-ppt",json!({"acceptance":true}))).await;
        let observer=match result {Ok(Ok(raw))=>serde_json::from_str::<Value>(&raw).unwrap_or(json!({"assessment":"uncertain","raw":raw})),Ok(Err(error))=>json!({"assessment":"unavailable","error":error.to_string()}),Err(_)=>json!({"assessment":"uncertain","error":"Observer 12s acceptance budget exceeded"})};
        report["observer_calls"]=json!(1);report["image_reuse"]=json!(2);
        report["cases"].as_array_mut().unwrap().push(json!({"case":"delete_visual_node","before":before["visual_artifact"],"after":after["visual_artifact"],"before_state":before_state,"after_state":after_state,"node_state":node_state,"worker":worker,"observer":observer,"observer_elapsed_ms":start.elapsed().as_millis(),"new_observer_screenshots":0}));
    }
    std::fs::write(root.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
    crate::browser_control::cleanup(&root,"real-ppt").await;server.abort();println!("Real visual acceptance report: {}",root.join("report.json").display());Ok(())
}

async fn real_visual_judgment(state:&agent::AgentServiceState,model:&str,ctx:&visual::VisualContext,ids:&[String],goal:&str,probe:bool)->Value {
    let mut body=json!({"model":model,"stream":false,"max_tokens":1800,"reasoning_effort":"low","messages":[{"role":"system","content":"Inspect the supplied images for this goal. Output only JSON with artifact_ids, checked_goal, expected_visible_result, request_trace_id from Host visual dispatch, assessment pass|issue|uncertain|unavailable, observed_facts [{artifact_id,region,fact}], issues and limitations. Facts must come from actual visible pixels; report uncertain if no images or insufficient detail. Do not invent parser causes or interaction success."}]});
    let start=std::time::Instant::now();let dispatch=match visual::prepare_request(state,"worker",model,ctx,ids,goal,&mut body).await {Ok(d)=>d,Err(e)=>return json!({"assessment":"unavailable","error":e.to_string()})};
    let result=tokio::time::timeout(Duration::from_secs(45),async {
        let response=state.client.post(format!("{}/chat/completions",state.provider_url.trim_end_matches('/'))).bearer_auth(&state.api_key).header("x-codex-visual-dispatch","task-owned").json(&body).send().await?.error_for_status()?;
        let response:Value=response.json().await?;let raw=response.pointer("/choices/0/message/content").and_then(Value::as_str).ok_or_else(||anyhow::anyhow!("model returned no text"))?;
        let check:Value=serde_json::from_str(raw.trim().trim_start_matches("```json").trim_end_matches("```").trim())?;Ok::<_,anyhow::Error>((check,response["usage"].clone()))
    }).await.map_err(|_|anyhow::anyhow!("45s visual acceptance budget exceeded")).and_then(|r|r);
    match result {Ok((check,usage))=>{
        let validated=visual::validate_check(state.workspace.root(),ctx,&check,&[dispatch.clone()],"worker");
        let validation=match validated {Ok(check)=>json!({"valid":true,"check":check}),Err(e)=>json!({"valid":false,"error":e.to_string()})};
        json!({"request_succeeded":true,"capability_probe":probe,"manifest":dispatch,"check":check,"host_validation":validation,"usage":usage,"elapsed_ms":start.elapsed().as_millis()})
    },Err(error)=>json!({"request_succeeded":false,"assessment":"unavailable","capability_probe":probe,"manifest":dispatch,"error":error.to_string(),"elapsed_ms":start.elapsed().as_millis()})}
}

#[tokio::test]
#[ignore = "real provider recheck of preserved task-owned images; requires VISUAL_ACCEPTANCE_REPORT"]
async fn real_visual_observer_reuse_recheck() -> anyhow::Result<()> {
    let path=PathBuf::from(std::env::var("VISUAL_ACCEPTANCE_REPORT")?);let root=path.parent().unwrap();
    let report:Value=serde_json::from_slice(&std::fs::read(&path)?)?;let case=&report["cases"][1];
    let mut state=agent::tests::flow_test_state(root,"127.0.0.1:1".parse()?);let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=report["route"]["model"].as_str().unwrap();anyhow::ensure!(report["probe_image_request_accepted"]==true,"requires an already successful explicit capability probe");
    state.visual.capabilities.entry(state.provider_name.clone()).or_default().insert(model.into(),visual::ImageCapability::Supported);
    state.observer_provider=state.provider_name.clone();state.observer_provider_url=state.provider_url.clone();state.observer_api_key=state.api_key.clone();
    let input=json!({"identity":case["after"]["identity"],"request":{"goal":case["worker"]["manifest"]["checked_goal"]},"visual_artifacts":[case["before"],case["after"]],"delivery":{"node_state":case["node_state"]}});
    let start=std::time::Instant::now();let response=tokio::time::timeout(Duration::from_secs(12),agent::observer_json_response(&state,model,"Review the before/after slide images independently. Be concise. Return JSON with assessment on_track|adjust|uncertain, summary and visual_check_result using the complete host-provided structure. Preserve every binding field verbatim and replace placeholders with visible facts.",input,1400,"real-ppt",json!({"acceptance_recheck":true}))).await;
    let response=match response {Ok(Ok(raw))=>serde_json::from_str::<Value>(&raw)?,Ok(Err(error))=>json!({"assessment":"unavailable","error":error.to_string()}),Err(_)=>json!({"assessment":"uncertain","error":"12s Observer budget exceeded"})};
    let result=json!({"observer_calls":1,"worker_calls":0,"new_screenshots":0,"reused_images":2,"input_bytes":case["before"]["byte_size"].as_u64().unwrap()+case["after"]["byte_size"].as_u64().unwrap(),"elapsed_ms":start.elapsed().as_millis(),"global_config_changed":false,"response":response});
    std::fs::write(root.join("observer_recheck.json"),serde_json::to_vec_pretty(&result)?)?;println!("Real Observer reuse report: {}",root.join("observer_recheck.json").display());Ok(())
}
