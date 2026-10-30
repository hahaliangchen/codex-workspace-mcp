
#[tokio::test]
async fn review_fresh_capture_after_async_page_change_is_current() {
    let root=TestRoot::new(); task(&root.0,"task");
    let mut scheduler=crate::work_scheduler::WorkScheduler::default(); scheduler.request_started_turn=1;
    scheduler.apply(&json!({"action":"work","orders":[{"id":"w","node_id":"n","goal":"check slide","done_when":"current pixels","completion":"output","visual_goal":"check slide"}]}),false,false).unwrap();
    let mut state=agent::tests::flow_test_state(&root.0,"127.0.0.1:1".parse().unwrap()); supported(&mut state);
    let ctx=scheduler_context("task",&scheduler,"w");
    let opened=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_open",&json!({"url":fixture(&root.0)})).await.unwrap(); scheduler.observe("browser_open",&json!({}),&opened,false);
    let ctx=scheduler_context("task",&scheduler,"w");
    crate::browser_control::inspect_test_page(&root.0,&ctx,"(()=>{document.body.insertAdjacentHTML('beforeend','<h1>ASYNC RENDER FINISHED</h1>');return true})()").await.unwrap();
    let shot=crate::browser_control::execute_scoped(&state.workspace,&ctx,"browser_screenshot",&json!({})).await.unwrap(); scheduler.observe("browser_screenshot",&json!({}),&shot,false);
    let next_ctx=scheduler_context("task",&scheduler,"w");
    let id=shot["artifact_id"].as_str().unwrap().to_owned(); let mut body=json!({"messages":[]});
    let sent=visual::prepare_request(&state,"worker","fake-model",&next_ctx,&[id],"check slide",&mut body).await.unwrap();
    let judged_current=sent["images"][0]["current_result"]==true;
    crate::browser_control::cleanup(&root.0,"task").await;
    assert!(judged_current,"fresh screenshot rejected as stale: captured_epoch={}, frame_epoch={}, page_epoch={}",shot["visual_artifact"]["execution_epoch"],next_ctx.execution_epoch,shot["visual_artifact"]["page_epoch"]);
}