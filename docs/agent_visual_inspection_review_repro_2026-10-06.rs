// Review reproductions: append these tests to src/visual_tests.rs to use its existing helpers.
// Expected on the reviewed implementation: all three tests fail because invalid passes are accepted.

#[tokio::test]
async fn review_stale_picture_cannot_be_revalidated_as_current() {
    let root=TestRoot::new(); task(&root.0,"task"); let mut ctx=context("task");
    let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap()); supported(&mut state);
    crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_open",&json!({"url":fixture(&root.0)})).await.unwrap();
    let old=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await.unwrap();
    let blank=root.0.join("blank.html"); std::fs::write(&blank,"<html><body>EMPTY CURRENT PAGE</body></html>").unwrap();
    let blank_url=format!("file:///{}",blank.to_string_lossy().replace('\\',"/"));
    crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_open",&json!({"url":blank_url})).await.unwrap();
    ctx.execution_epoch=1;
    let current=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await.unwrap();
    assert_ne!(old["visual_artifact"]["content_hash"],current["visual_artifact"]["content_hash"]);
    let old_id=old["artifact_id"].as_str().unwrap().to_owned();
    visual::view(&root.0,&ctx,&old_id).unwrap();
    let mut body=json!({"messages":[]});
    let sent=visual::prepare_request(&state,"worker","fake-model",&ctx,&[old_id],"verify current slide rendering",&mut body).await.unwrap();
    let accepted=visual::validate_check(&root.0,&ctx,&check(&sent),&[sent],"worker");
    crate::browser_control::cleanup(&root.0,"task").await;
    assert!(accepted.is_err(),"old slide pixels were accepted as current even after navigation to an empty page");
}

#[tokio::test]
async fn review_uncertain_visual_service_cannot_be_promoted_to_pass() {
    let root=TestRoot::new(); task(&root.0,"task"); let ctx=context("task");
    let id=visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(); let addr=listener.local_addr().unwrap();
    let app=axum::Router::new().route("/v1/chat/completions",post(||async {Json(json!({"choices":[{"message":{"content":json!({"assessment":"uncertain","observed_facts":[],"limitations":["cannot determine the rendered result"]}).to_string()}}]}))}));
    let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
    let mut state=agent::tests::flow_test_state(&root.0,addr);
    state.visual.capabilities.entry(String::new()).or_default().insert("fake-model".into(),visual::ImageCapability::Unsupported);
    state.visual.fallback=Some(visual::VisualRoute{provider:"vision".into(),model:"vision-model".into(),url:format!("http://{addr}/v1"),api_key:String::new()});
    let mut body=json!({"messages":[]});
    let sent=visual::prepare_request(&state,"worker","fake-model",&ctx,&[id],"verify current slide rendering",&mut body).await.unwrap();
    assert_eq!(sent["visual_service_result"]["assessment"],"uncertain"); assert!(images(&body).is_empty());
    let accepted=visual::validate_check(&root.0,&ctx,&check(&sent),&[sent],"worker"); server.abort();
    assert!(accepted.is_err(),"text-only Worker can invent a pass despite a visual service reporting uncertain without observations");
}
#[tokio::test]
async fn review_saved_visual_pass_expires_when_source_versions_change() {
    let root=TestRoot::new(); task(&root.0,"task");
    let mut scheduler=crate::work_scheduler::WorkScheduler::default(); scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"current slide visible","completion":"output","visual_goal":"check slide"}]}),false,false).unwrap();
    let ctx=visual::VisualContext{identity:crate::observer_service::identity("task",&scheduler,"w"),execution_epoch:scheduler.frame().unwrap().epoch,..Default::default()};
    let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap()); supported(&mut state);
    let id=visual::save(&root.0,&ctx,json!({}),&png(80,60)).unwrap()["artifact_id"].as_str().unwrap().to_owned();
    let mut body=json!({"messages":[]}); let sent=visual::prepare_request(&state,"worker","fake-model",&ctx,&[id],"check slide",&mut body).await.unwrap();
    scheduler.frame_mut().unwrap().visual_check_result=visual::validate_check(&root.0,&ctx,&check(&sent),&[sent],"worker").unwrap();
    let previous_epoch=scheduler.frame().unwrap().epoch;
    let mut versions=scheduler.versions(); versions.insert("renderer.ts".into(),"changed-source-hash".into()); scheduler.update_versions(&versions);
    assert!(scheduler.frame().unwrap().epoch>previous_epoch);
    let result=scheduler.return_work(&json!({"summary":"rendering verified"}));
    assert!(result.is_err(),"saved visual pass still completes the node after detected source version changes");
}