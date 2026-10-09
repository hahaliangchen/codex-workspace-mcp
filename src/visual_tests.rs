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
    let current=selected.last();
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
    std::fs::write(root.0.join(first["workspace_relative_path"].as_str().unwrap()),png(121,90)).unwrap();
    let error=visual::read(&root.0,"a",id).unwrap_err().to_string();
    assert!(error.contains("content_hash"),"tampered bytes must fail the content hash check: {error}");
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
    assert!(body["messages"].to_string().contains("Image input is unavailable"),"the Worker is told it did not see the screenshot");
    let mut unavailable=json!({"request_trace_id":unknown["request_trace_id"],"checked_goal":"check slide","artifact_ids":[ids[0]],"assessment":"unavailable",
        "observed_facts":[],"issues":[],"limitations":["image input unavailable"]});
    assert!(visual::validate_check(&root.0,&ctx,&unavailable,&[unknown.clone()],"worker").is_ok(),"an unavailable result may cite the undelivered screenshot");
    unavailable["assessment"]=json!("pass");
    assert!(visual::validate_check(&root.0,&ctx,&unavailable,&[unknown.clone()],"worker").is_err(),"a pass still needs a delivered image");
    supported(&mut state);let mut body=json!({"model":"fake-model","messages":[]});
    let sent=visual::prepare_request(&state,"worker","fake-model",&ctx,&ids,"check slide",&mut body).await.unwrap();
    assert_eq!(images(&body).len(),2);assert_eq!(sent["omitted_by_image_budget"],1);
    for (bytes,meta) in images(&body).iter().zip(sent["images"].as_array().unwrap()) {assert_eq!(meta["input_hash"],visual::hash(bytes));assert_eq!(image::load_from_memory(bytes).unwrap().width(),80);}
    let valid=check(&sent);assert!(visual::validate_check(&root.0,&ctx,&valid,&[sent.clone()],"worker").is_ok());
    let mut invented=valid.clone();invented["observed_facts"][0]["artifact_id"]=json!(ids[2]);assert!(visual::validate_check(&root.0,&ctx,&invented,&[sent.clone()],"worker").is_err());
    assert!(visual::validate_check(&root.0,&ctx,&valid,&[],"worker").is_err());
    let mut advanced=ctx.clone();advanced.execution_epoch=1;
    let historical=visual::validate_check(&root.0,&advanced,&valid,&[sent.clone()],"worker").unwrap();
    assert_eq!(historical["source_binding"]["execution_epoch"],ctx.execution_epoch,
        "later execution must not rewrite the state associated with the image request");
    assert_eq!(historical["source_binding"]["related_source_versions"],sent["related_source_versions"]);
    assert_eq!(historical["source_binding"]["current_page"],sent["current_page"]);
    assert_eq!(historical["visual_request"],sent,"preserve the original request for the AI to judge applicability");
    assert_eq!(historical["artifacts"][0]["execution_epoch"],ctx.execution_epoch);
    let mut wrong=valid.clone();wrong["artifact_ids"]=json!([ids[2]]);assert!(visual::validate_check(&root.0,&ctx,&wrong,&[sent.clone()],"worker").is_err());
    let saved=serde_json::to_string(&ids).unwrap();assert!(!saved.contains("base64"));let restored:Vec<String>=serde_json::from_str(&saved).unwrap();
    let mut recovered=json!({"messages":[]});visual::prepare_request(&state,"worker","fake-model",&ctx,&restored,"check slide",&mut recovered).await.unwrap();assert_eq!(images(&recovered),images(&body));
    let mcp=visual::mcp_result(&root.0,&ctx,visual::view(&root.0,&ctx,&ids[0]).unwrap()).unwrap();assert_eq!(mcp["content"][1]["type"],"image");assert_eq!(STANDARD.decode(mcp["content"][1]["data"].as_str().unwrap()).unwrap(),png(80,60));
}

#[tokio::test]
async fn unavailable_visual_message_survives_worker_restore_and_observer_http() {
    let root=TestRoot::new();task(&root.0,"task");
    let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"report rendering","completion":"output","visual_goal":"check slide"}]}),false,false).unwrap();
    let ctx=scheduler_context("task",&scheduler,"w");
    let artifact=visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap();let id=artifact["artifact_id"].as_str().unwrap().to_owned();
    async fn reply(Json(body):Json<Value>)->Json<Value> {
        let dispatch=manifest(&body);
        Json(json!({"choices":[{"message":{"content":json!({"assessment":"uncertain","summary":"Image input unavailable",
            "diagnostic":{"original_reason":"image capability unknown; no fallback"},"recommendations":[],
            "visual_check_result":{"assessment":"unavailable","request_trace_id":dispatch["request_trace_id"],"checked_goal":"check slide",
                "artifact_ids":dispatch["selected_artifact_ids"],"limitations":["No image input was sent"]}}).to_string()}}]}))
    }
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let server=tokio::spawn(async move {axum::serve(listener,axum::Router::new().route("/v1/chat/completions",post(reply))).await.unwrap();});
    let mut state=agent::tests::flow_test_state(&root.0,address);state.observer_provider_url=state.provider_url.clone();
    let mut body=json!({"messages":[]});
    let dispatch=visual::prepare_request(&state,"worker","fake-model",&ctx,&[id.clone()],"check slide",&mut body).await.unwrap();
    assert_eq!(dispatch["status"],"unknown_capability");assert!(images(&body).is_empty());
    assert!(!body.to_string().contains("Return now with yield_work"),"a capability diagnostic does not decide when Worker must stop unrelated work");
    scheduler.visual_response_received(&dispatch);
    let mut restored:crate::work_scheduler::WorkScheduler=serde_json::from_value(scheduler.snapshot()).unwrap();
    assert!(restored.worker_input("check slide").get("visual_requests").is_none(),"do not repeat original delivery messages in the work packet");
    assert_eq!(restored.frame().unwrap().visual_requests[0],dispatch,"retain the original binding for result references");
    let process=crate::session_history::task_process(&root.0,"task",1).await.unwrap();
    assert_eq!(process["records"][0]["payload"],body["messages"][0],"history retains exactly the text sent to Worker");
    let worker_check=json!({"assessment":"unavailable","request_trace_id":dispatch["request_trace_id"],"checked_goal":"check slide","artifact_ids":[id],"limitations":["No image input was sent"]});
    let checked=visual::validate_check(&root.0,&ctx,&worker_check,&restored.frame().unwrap().visual_requests,"worker").unwrap();
    assert_eq!(checked["visual_request"],dispatch);assert_eq!(checked["input_mode"],"unknown_capability");assert_eq!(checked["model_route"],dispatch["model_route"]);
    restored.frame_mut().unwrap().visual_check_result=checked.clone();
    let worker_return=json!({"summary":"Capture completed; image capability unknown", "limitations":["No fallback"],"visual_check_result":worker_check,
        "diagnostic":{"original_reason":"image capability unknown; no fallback"}});
    restored.return_work(&worker_return).unwrap();let organizer=restored.organizer_input();
    assert_eq!(organizer["current_result"]["worker_return"],worker_return);
    assert_eq!(organizer["current_result"]["visual_check_result"],checked);
    let observer_context=json!({"identity":ctx.identity,"execution_epoch":ctx.execution_epoch,"related_source_versions":ctx.related_source_versions,
        "task_page":ctx.current_page,"visual_artifacts":[artifact],"request":{"goal":"check slide"}});
    let raw=agent::observer_json_response(&state,"fake-model","Review current result",observer_context,2000,"task",json!({})).await.unwrap();
    let result:Value=serde_json::from_str(&raw).unwrap();
    assert!(result.get("visual_request").is_none(),"Observer did not request image input");
    assert_eq!(result["observer_return"]["diagnostic"]["original_reason"],"image capability unknown; no fallback");
    server.abort();
}

#[tokio::test]
async fn worker_keeps_visual_unavailability_in_original_messages_across_work_rounds() {
    let root=TestRoot::new();task(&root.0,"task");
    let requests=Arc::new(Mutex::new(Vec::<Value>::new()));let saved=requests.clone();
    let calls=Arc::new(AtomicUsize::new(0));let worker_calls=calls.clone();let path=root.0.clone();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let app=axum::Router::new().route("/v1/chat/completions",post(move|Json(body):Json<Value>| {
        let saved=saved.clone();let worker_calls=worker_calls.clone();let path=path.clone();async move {
            let organizer=body["tools"].as_array().into_iter().flatten().any(|schema|schema["function"]["name"]=="schedule_task");
            let worker=body["tools"].as_array().into_iter().flatten().any(|schema|schema["function"]["name"]=="yield_work");
            saved.lock().unwrap().push(body.clone());
            let message=if organizer {
                let first=worker_calls.load(Ordering::Relaxed)==0;
                json!({"role":"assistant","content":null,"tool_calls":[if first {
                    tool("assign","schedule_task",json!({"goal":"check slide","return_when":"report actual image input","reason":"inspect","completion":"output"}))
                }else{tool("finish","finish_request",json!({"summary":"No image input; other work completed","achieved":false,"unresolved":["Image capability unknown; no fallback"]}))}]})
            }else if worker {
                let stage=worker_calls.fetch_add(1,Ordering::Relaxed);
                let operation=match stage {
                    0=>{
                        let packet=body["messages"].as_array().unwrap().iter().filter_map(|message|message["content"].as_str())
                            .find_map(|text|text.strip_prefix("Current work packet:\n")).map(crate::session_history::parse_plain_context).unwrap();
                        let order=&packet["current_work"];
                        let ctx=visual::VisualContext {identity:json!({"task_id":"task","request_id":packet["request_id"],"work_id":order["id"],
                            "node_id":order["node_id"],"revision":order["revision"],"plan_revision":order["plan_revision"]}),..Default::default()};
                        let artifact=visual::save(&path,&ctx,json!({}),&png(80,60)).unwrap();
                        tool("view","view_image",json!({"artifact_id":artifact["artifact_id"]}))
                    },
                    1=>tool("info","workspace_info",json!({})),
                    _=>tool("return","yield_work",json!({"summary":"Screenshot exists; no image input. Workspace checked.","limitations":["Image capability unknown; no fallback"],"visual_check_result":null})),
                };
                json!({"role":"assistant","content":if stage==1 {json!("能力限制已收到，继续检查工作区。") }else{Value::Null},"tool_calls":[operation]})
            }else{json!({"role":"assistant","content":json!({"assessment":"uncertain","summary":"No image input; preserve limitation"}).to_string()})};
            Json(json!({"choices":[{"message":message}]}))
        }
    }));
    let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap();});
    let mut state=agent::tests::flow_test_state(&root.0,address);
    state.observer_enabled=true;state.observer_provider_url=state.provider_url.clone();
    tokio::time::timeout(Duration::from_secs(20),agent::run_task(state,"task".into(),"fake-model".into(),"Check saved screenshot and workspace".into(),7,CancellationToken::new(),false,1,vec![],false,false)).await.unwrap().unwrap();
    let requests=requests.lock().unwrap();
    let workers=requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|schema|schema["function"]["name"]=="yield_work")).collect::<Vec<_>>();
    assert_eq!(workers.len(),3);
    let original=workers[1]["messages"].as_array().unwrap().iter().find(|message|message["content"].as_str().is_some_and(|text|text.starts_with("Host visual dispatch:"))).unwrap();
    for request in [&workers[1],&workers[2]] {
        assert_eq!(request["messages"].as_array().unwrap().iter().filter(|message|*message==original).count(),1);
        assert!(images(request).is_empty());
    }
    let text=original["content"].as_str().unwrap();
    assert!(text.contains("image capability of this model is unknown and no fallback visual service is configured"));
    assert!(requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().any(|schema|schema["function"]["name"]=="schedule_task")).last().unwrap()["messages"].as_array().unwrap().iter().any(|message|message["content"].as_str().is_some_and(|content|content.contains(text))));
    assert!(requests.iter().filter(|body|body["tools"].as_array().into_iter().flatten().all(|schema|schema["function"]["name"]!="yield_work" && schema["function"]["name"]!="schedule_task")).any(|body|body["messages"].as_array().unwrap().iter().any(|message|message["content"].as_str().is_some_and(|content|content.contains(text)))),"Observer receives the same complete original limitation");
    let process=crate::session_history::task_process(&root.0,"task",1).await.unwrap();
    assert_eq!(process["records"].as_array().unwrap().iter().filter(|record|record["kind"]=="visual/input_result").count(),1);
    assert!(crate::session_history::process_messages(&process).iter().any(|message|message["content"].as_str().unwrap().contains(text)));
    let history=crate::session_history::read(&root.0,"task",&json!({"event_seq":process["records"].as_array().unwrap().iter().find(|record|record["kind"]=="visual/input_result").unwrap()["seq"],"max_chars":12000})).await.unwrap();
    assert_eq!(history["records"][0]["content"],text);
    assert_eq!(calls.load(Ordering::Relaxed),3);server.abort();
}

#[tokio::test]
async fn visual_reports_keep_capture_facts_when_page_or_source_changes() {
    let root=TestRoot::new();task(&root.0,"task");
    let mut before=context("task");before.execution_epoch=0;before.related_source_versions=json!({"src/app.ts":"v1"});
    let old=visual::save(&root.0,&before,json!({"browser_session_id":"session","page_id":"page","page_epoch":1}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let mut current=before.clone();current.execution_epoch=1;current.current_page=json!({"browser_session_id":"session","page_id":"page","page_epoch":2});
    let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap());supported(&mut state);

    let mut body=json!({"messages":[]});let old_only=visual::prepare_request(&state,"worker","fake-model",&current,&[old.clone()],"check slide",&mut body).await.unwrap();
    assert_eq!(old_only["images"][0]["page_epoch"],1);
    assert_eq!(old_only["current_page"]["page_epoch"],2);
    assert!(visual::validate_check(&root.0,&current,&check(&old_only),&[old_only],"worker").is_ok(),"the model decides how to use the earlier capture");

    let latest=visual::save(&root.0,&current,json!({"browser_session_id":"session","page_id":"page","page_epoch":2}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let mut body=json!({"messages":[]});let comparison=visual::prepare_request(&state,"worker","fake-model",&current,&[old.clone(),latest.clone()],"check slide",&mut body).await.unwrap();
    assert_eq!(comparison["images"][0]["page_epoch"],1);assert_eq!(comparison["images"][1]["page_epoch"],2);
    let mut valid=check(&comparison);valid["observed_facts"].as_array_mut().unwrap().push(json!({"artifact_id":old,"region":"before","fact":"historical comparison image"}));
    assert!(visual::validate_check(&root.0,&current,&valid,&[comparison.clone()],"worker").is_ok(),"before/after observations are preserved");
    valid["observed_facts"]=json!([{"artifact_id":old,"region":"before","fact":"historical comparison image"}]);
    assert!(visual::validate_check(&root.0,&current,&valid,&[comparison],"worker").is_ok(),"the host does not judge the sufficiency of historical facts");

    let mut changed=current.clone();changed.execution_epoch=2;changed.related_source_versions=json!({"src/app.ts":"v2"});
    let mut body=json!({"messages":[]});let restored=visual::prepare_request(&state,"worker","fake-model",&changed,&[latest],"check slide",&mut body).await.unwrap();
    assert_eq!(restored["images"][0]["related_source_versions"]["src/app.ts"],"v1");
    assert_eq!(restored["related_source_versions"]["src/app.ts"],"v2");
    assert!(visual::validate_check(&root.0,&changed,&check(&restored),&[restored],"worker").is_ok());
}

#[tokio::test]
async fn captures_preserve_page_and_source_observations_without_expiry() {
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
    assert!(dispatch["images"][0]["page_epoch"].as_u64().unwrap()<dispatch["images"][1]["page_epoch"].as_u64().unwrap());
    assert!(dispatch["images"][1]["page_observations"].is_object());
    let artifact=visual::metadata(&root.0,"task",&fresh_id).unwrap();
    assert_eq!(artifact["execution_epoch"],json!(fresh["visual_artifact"]["execution_epoch"]),"screenshot metadata remains immutable");
    assert!(fresh_ctx.execution_epoch>artifact["execution_epoch"].as_u64().unwrap() as usize,"scheduler advanced after receiving the screenshot");

    let mut versions=scheduler.versions();versions.insert("renderer.ts".into(),"changed-source-hash".into());scheduler.update_versions(&versions);
    let changed_ctx=scheduler_context("task",&scheduler,"w");let mut body=json!({"messages":[]});
    let restored=visual::prepare_request(&state,"worker","fake-model",&changed_ctx,&[fresh_id.clone()],"check slide",&mut body).await.unwrap();
    assert_ne!(restored["images"][0]["related_source_versions"],restored["related_source_versions"]);
    crate::browser_control::cleanup(&root.0,"task").await;
}

#[tokio::test]
async fn failed_capture_preserves_previous_images() {
    let root=TestRoot::new();task(&root.0,"task");
    let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"current slide visible","completion":"output","visual_goal":"check slide"}]}),false,false).unwrap();
    let frame=scheduler.frame_mut().unwrap();frame.browser_page=json!({"browser_session_id":"session","page_id":"page","page_epoch":1});
    let ctx=scheduler_context("task",&scheduler,"w");let old_id=visual::save(&root.0,&ctx,json!({"browser_session_id":"session","page_id":"page","page_epoch":1}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    scheduler.frame_mut().unwrap().current_visual_artifact_ids=vec![old_id.clone()];
    scheduler.observe("browser_screenshot",&json!({}),&json!({"error":"page changed during capture; visual ownership/state is uncertain"}),true);
    let after=scheduler_context("task",&scheduler,"w");let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap());supported(&mut state);
    let mut body=json!({"messages":[]});let dispatch=visual::prepare_request(&state,"worker","fake-model",&after,&[old_id],"check slide",&mut body).await.unwrap();
    assert_eq!(dispatch["images"][0]["page_epoch"],1);
    assert_eq!(dispatch["current_page"]["page_epoch"],1,"a failed tool call does not erase the last observed page");
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
async fn saved_visual_observation_survives_source_versions_change() {
    let root=TestRoot::new();task(&root.0,"task");
    let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"current slide visible","completion":"output","visual_goal":"check slide"}]}),false,false).unwrap();
    let mut ctx=visual::VisualContext {identity:crate::observer_service::identity("task",&scheduler,"w"),..Default::default()};ctx.related_source_versions=json!({});
    let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap());supported(&mut state);
    let id=visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let mut body=json!({"messages":[]});let sent=visual::prepare_request(&state,"worker","fake-model",&ctx,&[id],"check slide",&mut body).await.unwrap();
    let saved=visual::validate_check(&root.0,&ctx,&check(&sent),&[sent],"worker").unwrap();scheduler.frame_mut().unwrap().visual_check_result=saved.clone();
    let mut versions=scheduler.versions();versions.insert("renderer.ts".into(),"changed-source-hash".into());scheduler.update_versions(&versions);
    assert_eq!(scheduler.frame().unwrap().visual_check_result,saved,"source changes do not erase the model's previous observation");
    let returned=scheduler.return_work(&json!({"summary":"A visual result exists, but it predates the current renderer source version",
        "limitations":["The current renderer version has not been visually verified"]})).unwrap();
    assert_eq!(scheduler.frame().unwrap().status,crate::work_scheduler::WorkStatus::Done);
    assert!(returned.get("expectation_met").is_none(),"host freshness facts do not become a user-goal conclusion");
    assert_eq!(returned["visual_check_result"]["assessment"],"pass","the previous result remains historical evidence");
}

#[derive(Default)]
struct Script {requests:Mutex<Vec<Value>>,worker_calls:AtomicUsize,organizer_calls:AtomicUsize,url:String}
fn tool(id:&str,name:&str,args:Value)->Value {json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})}
async fn chat(State(script):State<Arc<Script>>,Json(body):Json<Value>)->Json<Value> {
    let is_organizer=body["tools"].as_array().into_iter().flatten().any(|tool|tool["function"]["name"]=="schedule_task"||tool["function"]["name"]=="finish_request");
    let worker=body["tools"].as_array().into_iter().flatten().any(|tool|tool["function"]["name"]=="yield_work");
    let dispatch=manifest(&body);script.requests.lock().unwrap().push(body.clone());
    let message=if is_organizer {
        let first=script.organizer_calls.fetch_add(1,Ordering::Relaxed)==0;
        json!({"role":"assistant","content":null,"tool_calls":[if first {tool("schedule","schedule_task",json!({"goal":"check slide","return_when":"bound visual result","reason":"verify pixels","completion":"output","execution_scope":{"visual_goal":"check slide"}}))}else{tool("finish","finish_request",json!({"summary":"visual transport complete","achieved":true}))}]})
    }else if worker {
        let stage=script.worker_calls.fetch_add(1,Ordering::Relaxed);
        let calls=match stage {
            0=>vec![tool("open","browser_open",json!({"url":script.url})),tool("shot","browser_screenshot",json!({}))],
            1=>{
                let artifact=body["messages"].as_array().unwrap().iter().rev().filter(|message|message["role"]=="tool")
                    .filter_map(|message|serde_json::from_str::<Value>(message["content"].as_str()?).ok())
                    .find_map(|result|result["artifact_id"].as_str().map(str::to_owned)).unwrap();
                vec![tool("view","view_image",json!({"artifact_id":artifact}))]
            },
            _=>vec![tool("yield","yield_work",json!({"summary":"visual transport checked","visual_check_result":check(&dispatch)}))]
        };
        json!({"role":"assistant","content":null,"tool_calls":calls})
    }else if body["stream"]==true {json!({"role":"assistant","content":"Visual transport complete"})}
    else {json!({"role":"assistant","content":json!({"assessment":"on_track","summary":"review","visual_check_result":if dispatch["status"]=="direct" {check(&dispatch)}else{Value::Null}}).to_string()})};
    Json(json!({"choices":[{"message":message}]}))
}
async fn server(script:Arc<Script>)->(std::net::SocketAddr,tokio::task::JoinHandle<()>) {
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let app=axum::Router::new().route("/v1/chat/completions",post(chat)).with_state(script);
    (address,tokio::spawn(async move{axum::serve(listener,app).await.unwrap();}))
}
fn fixture(root:&Path)->String {let path=root.join("slide.html");std::fs::write(&path,"<html><body><canvas id='slide' width='800' height='450'></canvas><script>const c=document.querySelector('canvas').getContext('2d');c.fillStyle='#bb2255';c.fillRect(0,0,800,450);c.fillStyle='white';c.font='40px sans-serif';c.fillText('Canvas slide',40,90)</script></body></html>").unwrap();reqwest::Url::from_file_path(&path).unwrap().to_string()}

#[tokio::test]
async fn unavailable_report_can_reference_original_captures_from_separate_requests() {
    let root=TestRoot::new();task(&root.0,"task");let ctx=context("task");
    let first=visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let second=visual::save(&root.0,&ctx,json!({}),&png(90,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap());
    let mut body=json!({"messages":[]});
    let first_request=visual::prepare_request(&state,"worker","fake-model",&ctx,&[first.clone()],"check slide",&mut body).await.unwrap();
    let second_request=visual::prepare_request(&state,"worker","fake-model",&ctx,&[second.clone()],"check slide",&mut body).await.unwrap();
    let report=json!({"assessment":"unavailable","checked_goal":"check slide",
        "artifact_ids":[first,second],"request_trace_id":second_request["request_trace_id"],
        "limitations":["Screenshots captured; image input unavailable"]});
    let checked=visual::validate_check(&root.0,&ctx,&report,&[first_request,second_request],"worker").unwrap();
    assert_eq!(checked["assessment"],"unavailable");assert_eq!(checked["artifact_ids"],report["artifact_ids"]);
    assert!(images(&body).is_empty());
}

#[tokio::test]
async fn visual_worker_runtime_sends_real_images_without_an_observer_or_extra_receipt_round() {
    let root=TestRoot::new();task(&root.0,"task");let script=Arc::new(Script {url:fixture(&root.0),..Default::default()});let (address,server)=server(script.clone()).await;
    let mut state=agent::tests::flow_test_state(&root.0,address);supported(&mut state);
    tokio::time::timeout(Duration::from_secs(20),agent::run_task(state,"task".into(),"fake-model".into(),"Inspect canvas rendering".into(),6,CancellationToken::new(),false,1,vec![],false,false)).await.unwrap().unwrap();
    let requests=script.requests.lock().unwrap();let visual_requests=requests.iter().filter(|body|!images(body).is_empty()).collect::<Vec<_>>();assert_eq!(visual_requests.len(),1);
    let dispatch=manifest(visual_requests[0]);let id=dispatch["images"][0]["artifact_id"].as_str().unwrap();assert_eq!(visual::read(&root.0,"task",id).unwrap().1,images(visual_requests[0])[0]);
    assert_eq!(script.worker_calls.load(Ordering::Relaxed),3,"capture, explicit view_image, then visual return");
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
    let a_url=fixture(&root.0);let other=root.0.join("other.html");std::fs::write(&other,"<html><body>Task B</body></html>").unwrap();let b_url=reqwest::Url::from_file_path(&other).unwrap().to_string();
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
async fn observer_does_not_attach_or_capture_images_without_a_model_method_call() {
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
            tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
        assert_eq!(reviews.len(),1,"an Observer result without recommendations is still delivered");
        assert!(reviews[0]["suggestions"].as_array().unwrap().is_empty());
        assert_eq!(reviews[0]["observer_return"]["summary"],"review");observer.finish("completed").await.unwrap();
        let requests=script.requests.lock().unwrap();assert_eq!(requests.len(),1);assert!(images(&requests[0]).is_empty());
        let conn=agent::open_db(&root.0).unwrap();
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='agent_visual_artifacts')",[],|row|row.get(0)).unwrap();
        let count=if exists{conn.query_row("SELECT COUNT(*) FROM agent_visual_artifacts",[],|row|row.get::<_,i64>(0)).unwrap()}else{0};
        assert_eq!(count,if reuse {2}else{0},"Observer never captures on its own");
        let result:String=conn.query_row("SELECT result FROM agent_observations",[],|row|row.get(0)).unwrap();let result:Value=serde_json::from_str(&result).unwrap();
        assert!(result["visual_capture"].is_null());
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
        let start=std::time::Instant::now();let result=agent::observer_json_response(&state,&model,
            "Review this visual delivery independently and concisely. Output JSON with assessment on_track|adjust|uncertain, summary and visual_check_result. In visual_check_result cite the Host visual dispatch request_trace_id, exact checked_goal and artifact_ids; expected_visible_result; assessment pass|issue|uncertain|unavailable; record observed_facts with artifact_id, region, fact and limitations. Only assess visible facts.",context,1400,"real-ppt",json!({"acceptance":true})).await;
        let observer=match result {Ok(raw)=>serde_json::from_str::<Value>(&raw).unwrap_or(json!({"assessment":"uncertain","raw":raw})),Err(error)=>json!({"assessment":"unavailable","error":error.to_string()})};
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
    let start=std::time::Instant::now();let response=agent::observer_json_response(&state,model,"Review the before/after slide images independently. Be concise. Return JSON with assessment on_track|adjust|uncertain, summary and visual_check_result using the complete host-provided structure. Preserve every binding field verbatim and replace placeholders with visible facts.",input,1400,"real-ppt",json!({"acceptance_recheck":true})).await;
    let response=match response {Ok(raw)=>serde_json::from_str::<Value>(&raw)?,Err(error)=>json!({"assessment":"unavailable","error":error.to_string()})};
    let result=json!({"observer_calls":1,"worker_calls":0,"new_screenshots":0,"reused_images":2,"input_bytes":case["before"]["byte_size"].as_u64().unwrap()+case["after"]["byte_size"].as_u64().unwrap(),"elapsed_ms":start.elapsed().as_millis(),"global_config_changed":false,"response":response});
    std::fs::write(root.join("observer_recheck.json"),serde_json::to_vec_pretty(&result)?)?;println!("Real Observer reuse report: {}",root.join("observer_recheck.json").display());Ok(())
}

#[test]
fn only_explicit_view_calls_select_images_and_do_not_automatically_add_comparisons() {
    let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"compare","done_when":"report","completion":"output",
        "checks":["http-probe: http://127.0.0.1:3000/#ignored"]}]}),false,true).unwrap();
    assert_eq!(scheduler.order().unwrap().checks,vec!["http-probe:http://127.0.0.1:3000/"]);
    scheduler.frame_mut().unwrap().visual_artifact_ids=vec!["before".into()];
    assert!(scheduler.visual_input_ids().is_empty(),"capture is not an image-input request");
    scheduler.observe("view_image",&json!({"artifact_id":"before"}),&json!({"artifact_id":"before"}),false);
    assert_eq!(scheduler.visual_input_ids(),vec!["before"]);
    scheduler.visual_response_received(&json!({"status":"direct","selected_artifact_ids":["before"]}));
    assert!(scheduler.visual_input_ids().is_empty(),"no new image means no repeated input");
    scheduler.frame_mut().unwrap().visual_artifact_ids.push("after".into());
    assert!(scheduler.visual_input_ids().is_empty());
    scheduler.observe("view_image",&json!({"artifact_id":"after"}),&json!({"artifact_id":"after"}),false);
    assert_eq!(scheduler.visual_input_ids(),vec!["after"]);
    scheduler.visual_response_failed(&json!({"status":"direct","selected_artifact_ids":["before","after"]}),"HTTP refusal");
    assert!(scheduler.visual_input_ids().is_empty());
    scheduler.frame_mut().unwrap().visual_artifact_ids.push("fresh".into());
    assert!(scheduler.visual_input_ids().is_empty());
    scheduler.observe("view_image",&json!({"artifact_id":"fresh"}),&json!({"artifact_id":"fresh"}),false);
    assert_eq!(scheduler.visual_input_ids(),vec!["fresh"]);
    let restored:crate::work_scheduler::WorkScheduler=serde_json::from_value(scheduler.snapshot()).unwrap();
    assert_eq!(restored.visual_input_ids(),scheduler.visual_input_ids());
}

#[tokio::test]
async fn visual_http_rejection_preserves_original_reason_in_every_role_context() {
    let root=TestRoot::new();task(&root.0,"task");let ctx=context("task");
    let id=visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    const REASON:&str="image input unsupported on model m; provider request_id=raw-rejection-42";
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
    let server=tokio::spawn(async move {axum::serve(listener,axum::Router::new().route("/v1/chat/completions",post(||async{(axum::http::StatusCode::BAD_REQUEST,REASON)}))).await.unwrap()});
    let mut state=agent::tests::flow_test_state(&root.0,addr);state.observer_provider_url=state.provider_url.clone();supported(&mut state);
    let input=json!({"identity":ctx.identity,"visual_artifacts":[visual::metadata(&root.0,"task",&id).unwrap()],"request":{"goal":"check slide"}});
    let error=agent::observer_json_response(&state,"fake-model","review",input,1000,"task",json!({})).await.unwrap_err();
    assert!(error.downcast_ref::<crate::visual_probe::VisualModelFailure>().is_none(),"a plain request failure is not a visual attempt");
    assert!(format!("{error:#}").contains(REASON));
    let mut body=json!({"messages":[]});
    let dispatch=visual::prepare_request(&state,"worker","fake-model",&ctx,&[id.clone()],"check slide",&mut body).await.unwrap();
    let mut scheduler=crate::work_scheduler::WorkScheduler::default();scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"report rendering","completion":"output"}]}),false,false).unwrap();
    scheduler.visual_response_failed(&dispatch,REASON);
    let restored:crate::work_scheduler::WorkScheduler=serde_json::from_value(scheduler.snapshot()).unwrap();
    assert_eq!(restored.frame().unwrap().visual_requests[0]["error"],REASON);
    assert_eq!(restored.frame().unwrap().visual_check_result["limitations"][0],REASON);
    let mut restored=restored;restored.return_work(&json!({"summary":"Screenshot exists but visual request failed","limitations":[REASON]})).unwrap();
    assert_eq!(restored.organizer_input()["current_result"]["visual_requests"][0]["error"],REASON);
    assert!(!agent::load_observer_work_trace(&root.0,"task").unwrap().iter().any(|item|item["type"]=="visual/model_failed"),"Observer made no image request to mislabel as a visual failure");
    state.visual.capabilities.entry(String::new()).or_default().insert("fake-model".into(),visual::ImageCapability::Unsupported);
    state.visual.fallback=Some(visual::VisualRoute{provider:"vision".into(),model:"m".into(),url:state.provider_url.clone(),api_key:String::new()});
    let failed=visual::prepare_request(&state,"worker","fake-model",&ctx,&[id],"check slide",&mut json!({"messages":[]})).await.unwrap();
    assert_eq!(failed["status"],"unavailable");assert!(failed["visual_service_result"]["limitations"].to_string().contains(REASON));server.abort();
}

#[tokio::test]
async fn visual_worker_image_rejection_returns_to_organizer_without_automatic_retry() {
    #[derive(Default)]
    struct Rejection { url:String, organizer:AtomicUsize, worker:AtomicUsize, requests:Mutex<Vec<Value>> }
    const REASON:&str="this model does not support image input; upstream-id=raw-17";
    async fn reply(State(script):State<Arc<Rejection>>,Json(body):Json<Value>)->axum::response::Response {
        use axum::response::IntoResponse;
        script.requests.lock().unwrap().push(body.clone());
        if !images(&body).is_empty() {return (axum::http::StatusCode::BAD_REQUEST,REASON).into_response();}
        let organizer=body["tools"].as_array().into_iter().flatten().any(|tool|tool["function"]["name"]=="schedule_task");
        let calls=if organizer {
            if script.organizer.fetch_add(1,Ordering::Relaxed)==0 {
                vec![tool("schedule","schedule_task",json!({"goal":"check slide","return_when":"report visible rendering or its actual limitation","reason":"check pixels","execution_scope":{"visual_goal":"check slide"}}))]
            }else{
                assert!(body["messages"].to_string().contains(REASON),"Organizer must receive the original provider rejection");
                vec![tool("finish","finish_request",json!({"summary":"Captured the screenshot; visual inspection is unavailable.","achieved":false,"unresolved":[REASON]}))]
            }
        }else if script.worker.fetch_add(1,Ordering::Relaxed)==0 {vec![tool("open","browser_open",json!({"url":script.url})),tool("shot","browser_screenshot",json!({}))]}
        else {
            let artifact=body["messages"].as_array().unwrap().iter().rev().filter(|message|message["role"]=="tool")
                .filter_map(|message|serde_json::from_str::<Value>(message["content"].as_str()?).ok())
                .find_map(|result|result["artifact_id"].as_str().map(str::to_owned)).unwrap();
            vec![tool("view","view_image",json!({"artifact_id":artifact}))]
        };
        Json(json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":calls}}]})).into_response()
    }
    let root=TestRoot::new();task(&root.0,"task");let script=Arc::new(Rejection{url:fixture(&root.0),..Default::default()});
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();let server_script=script.clone();
    let server=tokio::spawn(async move{axum::serve(listener,axum::Router::new().route("/v1/chat/completions",post(reply)).with_state(server_script)).await.unwrap()});
    let mut state=agent::tests::flow_test_state(&root.0,addr);supported(&mut state);
    agent::run_task(state,"task".into(),"fake-model".into(),"Inspect canvas rendering and preserve any limitations".into(),8,CancellationToken::new(),false,1,vec![],false,false).await.unwrap();
    assert_eq!(script.requests.lock().unwrap().iter().filter(|body|!images(body).is_empty()).count(),1,"image failures are not silently retried");
    let conn=agent::open_db(&root.0).unwrap();
    assert_eq!(conn.query_row("SELECT status FROM agent_tasks WHERE id='task'",[],|row|row.get::<_,String>(0)).unwrap(),"completed");
    let final_text:String=conn.query_row("SELECT data FROM agent_task_events WHERE kind='assistant/message' ORDER BY seq DESC LIMIT 1",[],|row|row.get(0)).unwrap();
    assert!(final_text.contains(REASON),"final delivery must include the original limitation");
    let failure:String=conn.query_row("SELECT data FROM agent_task_events WHERE kind='visual/model_failed' ORDER BY seq DESC LIMIT 1",[],|row|row.get(0)).unwrap();
    assert!(failure.contains(REASON));assert!(!failure.contains("base64"));server.abort();
}

/// Opt-in actual Organizer/Worker/Observer task, using the existing PPT build and sample.
/// The external PPT project is read only; reports, database and the sample copy stay here.
#[tokio::test]
#[ignore = "real configured provider and browser acceptance; requires PPT demo dist and sample"]
async fn real_ppt_visual_input_six_stage_acceptance() -> anyhow::Result<()> {
    let workspace=std::env::current_dir()?;
    let root=workspace.join(".codex-workspace-mcp").join(format!("visual-input-e2e-{}",visual::new_id()));
    std::fs::create_dir_all(&root)?;
    let demo=std::env::var_os("PPT_DEMO_DIST").map(PathBuf::from).unwrap_or_else(||workspace.join("../pptx-editor-engine/dist"));
    anyhow::ensure!(demo.join("index.html").is_file(),"existing PPT demo dist is required");
    let fonts=reqwest::Client::new().get("http://127.0.0.1:8080/api/fonts").timeout(Duration::from_secs(5)).send().await?;
    anyhow::ensure!(fonts.status().is_success(),"start the existing PPT font backend before real acceptance");
    let sample=std::env::var_os("PPT_SAMPLE_PATH").map(PathBuf::from).or_else(||
        std::fs::read_dir(demo.parent()?).ok()?.filter_map(Result::ok).map(|entry|entry.path())
            .find(|path|path.extension().is_some_and(|ext|ext=="pptx"))).ok_or_else(||anyhow::anyhow!("PPT sample is required"))?;
    std::fs::copy(sample,root.join("sample.pptx"))?;
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;let addr=listener.local_addr()?;
    let server=tokio::spawn(async move{axum::serve(listener,axum::Router::new().fallback_service(tower_http::services::ServeDir::new(demo))).await.unwrap()});
    let mut state=agent::tests::flow_test_state(&root,addr);
    let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=state.model_map.get(&state.default_model).cloned().unwrap_or(state.default_model.clone());
    anyhow::ensure!(state.visual.capability(&state.provider_name,&model)==visual::ImageCapability::Supported,"run --probe-image-input first; this acceptance never guesses image support");
    // Keep the verified route; lower reasoning for this bounded transport/rendering acceptance.
    state.reasoning_effort=Some("low".into());state.fast_mode=false;state.enable_subagent=false;
    let task_id="real-visual-input";task(&root,task_id);
    let conn=agent::open_db(&root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_context_debug(task_id TEXT PRIMARY KEY,enabled INTEGER NOT NULL DEFAULT 0)")?;
    conn.execute("INSERT INTO agent_context_debug(task_id,enabled) VALUES (?1,1)",[task_id])?;drop(conn);
    let stop=CancellationToken::new();let watch_stop=stop.clone();let watch_root=root.clone();
    let watcher=tokio::spawn(async move {
        let mut sent=std::collections::BTreeMap::new();
        loop {
            if let Ok(conn)=agent::open_db(&watch_root) {
                if let Ok(mut stmt)=conn.prepare("SELECT id,metadata,request_json FROM agent_request_contexts WHERE task_id='real-visual-input'") {
                    if let Ok(rows)=stmt.query_map([],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?))) {
                        for row in rows.flatten() {
                            if let (Ok(meta),Ok(body))=(serde_json::from_str::<Value>(&row.1),serde_json::from_str::<Value>(&row.2)) {
                                let pixels=images(&body);
                                if !pixels.is_empty() {sent.insert(row.0,json!({"id":row.0,"metadata":meta,
                                    "actual_http_images":pixels.iter().map(|bytes|json!({"hash":visual::hash(bytes),"bytes":bytes.len()})).collect::<Vec<_>>()}));}
                            }
                        }
                    }
                }
            }
            tokio::select! {_=watch_stop.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(100))=>{}}
        }
        sent.into_values().collect::<Vec<_>>()
    });
    let prompt=format!("验收当前已构建的 PPT 浏览器页面 {addr_url}，工作区 sample.pptx 是真实 8 页示例。由 Organizer 分派六个独立阶段，每阶段有自己的目标和返回条件：1 服务探测（只检查给定 URL，复用探测结果）；2 打开可见浏览器（仅一次）；3 上传 sample.pptx 并等待该上传加载完成（仅一次，后续阶段显式复用上传回执和同一浏览器页面）；4 读取初始页码及总页数；5 截图并实际看图、点击 #btn-next 翻页后再次截图看图，分别描述第 1 页和第 2 页可见内容、页码及空白/裁切/缺失/排版问题，提交绑定真实图片的 visual_check_result；6 browser_diagnostics 记录实际资源/字体和浏览器错误。使用浏览器工具，不读取源码、不启动或修改其他服务、不执行编辑交互。DOM 页码和截图成功各自只是事实。允许如实交付存在限制的结果，最终正文列出所有未确认事项。Observer 按增量上下文观察并在任务完成后复盘，记录有依据的可复用经验。",addr_url=format!("http://{addr}/"));
    println!("Real six-stage acceptance root: {}",root.display());
    let start=std::time::Instant::now();
    let run=agent::run_task(state.clone(),task_id.into(),model.clone(),prompt,48,CancellationToken::new(),false,1,vec![],false,true).await;
    stop.cancel();let requests=watcher.await?;
    let conn=agent::open_db(&root)?;
    let status:String=conn.query_row("SELECT status FROM agent_tasks WHERE id=?1",[task_id],|row|row.get(0))?;
    let mut stmt=conn.prepare("SELECT seq,kind,data FROM agent_task_events WHERE task_id=?1 ORDER BY seq")?;
    let events=stmt.query_map([task_id],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(|(seq,kind,data)|json!({"seq":seq,"type":kind,"data":serde_json::from_str::<Value>(&data).unwrap()})).collect::<Vec<_>>();
    let tools=events.iter().filter(|event|event["type"]=="tool/call").map(|event|event["data"]["name"].clone()).collect::<Vec<_>>();
    let returned=events.iter().filter(|event|event["type"]=="worker/yield").map(|event|event["data"]["output"].clone()).collect::<Vec<_>>();
    let checks=events.iter().filter(|event|event["type"]=="visual/check_result").map(|event|event["data"]["result"].clone()).collect::<Vec<_>>();
    let final_text=events.iter().rev().find(|event|event["type"]=="assistant/message").map(|event|event["data"]["message"]["content"].clone());
    let report=json!({"route":{"provider":state.provider_name,"model":model},"status":status,"error":run.as_ref().err().map(|e|format!("{e:#}")),
        "elapsed_ms":start.elapsed().as_millis() as u64,"reasoning_effort":"low","tools":tools,"task_returns":returned,"visual_checks":checks,
        "actual_image_requests":requests,"final_content":final_text,"events":events});
    std::fs::write(root.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
    drop(stmt);drop(conn);
    // Recreate state and HTTP handlers after normal startup cleanup, as a page refresh does.
    agent::recover_orphaned_tasks(&root).await?;
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;let restore_addr=listener.local_addr()?;
    let restore=tokio::spawn(async move {axum::serve(listener,axum::Router::new()
        .route("/agent/tasks/{task}/events",axum::routing::get(agent::get_events))
        .route("/agent/tasks/{task}/visual-artifacts/{id}/image",axum::routing::get(visual::image_route)).with_state(state)).await.unwrap()});
    let restored:Value=reqwest::get(format!("http://{restore_addr}/agent/tasks/{task_id}/events")).await?.json().await?;
    let mut restored_images=0;
    for event in report["events"].as_array().unwrap().iter().filter(|event|event["type"]=="tool/result") {
        if let Some(id)=event["data"]["meta"]["result"]["visual_artifact"]["artifact_id"].as_str() {
            let response=reqwest::get(format!("http://{restore_addr}/agent/tasks/{task_id}/visual-artifacts/{id}/image")).await?;
            anyhow::ensure!(response.status().is_success(),"saved image could not be restored");
            let bytes=response.bytes().await?;
            anyhow::ensure!(visual::hash(&bytes)==event["data"]["meta"]["result"]["visual_artifact"]["content_hash"],"restored image changed");restored_images+=1;
        }
    }
    // Startup removes transient execution/debug snapshots by design. Verify
    // each durable tool, Flow, role, visual and memory event instead.
    let durable=report["events"].as_array().unwrap().iter().filter(|event| {
        let kind=event["type"].as_str().unwrap();
        !kind.starts_with("execution/") && !matches!(kind,"scheduler/state"|"worker/work_state"|"worker/source_working_set"|"assistant/delta"|"organizer/request_metrics"|"debug/context_request"|"debug/context_end")
    }).collect::<Vec<_>>();
    let history_preserved=durable.iter().all(|event|restored["events"].as_array().into_iter().flatten()
        .any(|saved|saved["seq"]==event["seq"] && saved["type"]==event["type"] && saved["data"]==event["data"]));
    std::fs::write(root.join("restoration.json"),serde_json::to_vec_pretty(&json!({"event_count":restored["events"].as_array().map(Vec::len),"durable_event_count":durable.len(),"history_preserved":history_preserved,"restored_images":restored_images}))?)?;
    restore.abort();server.abort();crate::browser_control::cleanup(&root,task_id).await;
    println!("Real six-stage report: {}",root.join("report.json").display());
    run?;
    anyhow::ensure!(status=="completed","real task did not complete: {status}");
    anyhow::ensure!(tools.iter().filter(|tool|**tool=="browser_open").count()==1 && tools.iter().filter(|tool|**tool=="browser_upload").count()==1,"page/upload was not reused");
    anyhow::ensure!(returned.len()>=6,"six independent stages were not returned");
    anyhow::ensure!(tools.iter().any(|tool|*tool=="browser_diagnostics") && restored_images>=2,"diagnostics and before/after images are required");
    anyhow::ensure!(!checks.is_empty() && !requests.is_empty(),"real image requests and image-bound model judgments are required");
    anyhow::ensure!(checks.iter().any(|check|check["actor"]=="worker" && check["input_mode"]=="direct"
        && check["artifact_ids"].as_array().is_some_and(|ids|ids.len()>=2)
        && check["observed_facts"].as_array().is_some_and(|facts|facts.len()>=2)),"Worker must return an actual two-image comparison, including an honest issue or limitation");
    anyhow::ensure!(requests.iter().any(|request|request["metadata"]["actor"]=="worker"
        && request["actual_http_images"].as_array().is_some_and(|images|images.len()==2 && images[0]["hash"]!=images[1]["hash"])),"before/after Worker request must contain distinct actual pixels");
    anyhow::ensure!(report["events"].as_array().unwrap().iter().any(|event|event["type"]=="observer/retrospective"
        && event["data"]["status"]=="completed" && event["data"]["memoryRecorded"]==true),"real full retrospective and grounded memory are required");
    anyhow::ensure!(history_preserved,"durable history was lost after cleanup");
    Ok(())
}

#[tokio::test]
#[ignore = "real Observer inspection of a completed six-stage report; requires VISUAL_ACCEPTANCE_REPORT"]
async fn real_visual_observer_six_stage_reuse_recheck() -> anyhow::Result<()> {
    let path=PathBuf::from(std::env::var("VISUAL_ACCEPTANCE_REPORT")?);let root=path.parent().unwrap();
    let report:Value=serde_json::from_slice(&std::fs::read(&path)?)?;
    let check=report["visual_checks"].as_array().unwrap().iter().find(|check|check["actor"]=="worker"
        && check["artifact_ids"].as_array().is_some_and(|ids|ids.len()==2)).ok_or_else(||anyhow::anyhow!("real Worker pair is required"))?;
    let dispatch=&check["visual_request"];let task=check["identity"]["task_id"].as_str().unwrap();
    let mut state=agent::tests::flow_test_state(root,"127.0.0.1:1".parse()?);
    let config=crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;crate::ai_proxy::apply_agent_config(&mut state,&config);
    let model=report["route"]["model"].as_str().unwrap();
    state.observer_provider=state.provider_name.clone();state.observer_provider_url=state.provider_url.clone();state.observer_api_key=state.api_key.clone();
    anyhow::ensure!(state.visual.capability(&state.observer_provider,model)==visual::ImageCapability::Supported,"requires verified configured image support");
    let conn=agent::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_context_debug(task_id TEXT PRIMARY KEY,enabled INTEGER NOT NULL DEFAULT 0)")?;
    conn.execute("INSERT OR REPLACE INTO agent_context_debug(task_id,enabled) VALUES (?1,1)",[task])?;
    let before:i64=conn.query_row("SELECT COUNT(*) FROM agent_visual_artifacts",[],|row|row.get(0))?;
    let input=json!({"identity":check["identity"],"request":{"goal":check["checked_goal"]},"visual_artifacts":check["artifacts"],
        "execution_epoch":dispatch["execution_epoch"],"related_source_versions":dispatch["related_source_versions"],
        "task_page":dispatch["current_page"]});
    let started=std::time::Instant::now();
    let raw=agent::observer_json_response(&state,model,include_str!("../prompts/observer_system.md"),input,2400,task,
        json!({"stage":"saved_image_recheck","review_id":"real-reuse-recheck","identity":check["identity"]})).await?;
    let result:Value=serde_json::from_str(raw.trim().trim_start_matches("```json").trim_end_matches("```").trim())?;
    let actual:String=conn.query_row("SELECT request_json FROM agent_request_contexts WHERE task_id=?1 ORDER BY id DESC LIMIT 1",[task],|row|row.get(0))?;
    let actual:Value=serde_json::from_str(&actual)?;let pixels=images(&actual);
    let after:i64=conn.query_row("SELECT COUNT(*) FROM agent_visual_artifacts",[],|row|row.get(0))?;
    std::fs::write(root.join("observer_recheck.json"),serde_json::to_vec_pretty(&json!({"raw_response":raw,"result":result,
        "elapsed_ms":started.elapsed().as_millis() as u64,"new_captures":after-before,
        "actual_http_images":pixels.iter().map(|bytes|json!({"hash":visual::hash(bytes),"bytes":bytes.len()})).collect::<Vec<_>>()}))?)?;
    anyhow::ensure!(pixels.len()==2 && before==after,"Observer must reuse both saved pictures without recapturing");
    anyhow::ensure!(result["visual_check_result"]["check_id"].is_string()
        && result["visual_check_result"]["observed_facts"].as_array().is_some_and(|facts|facts.len()>=2),"Observer must return valid image-bound facts");
    anyhow::ensure!(pixels.iter().zip(result["visual_request"]["images"].as_array().unwrap()).all(|(bytes,image)|
        image["input_hash"]==visual::hash(bytes)),"actual Observer request pixels must match the recorded image inputs");
    Ok(())
}
