//! Protocol and ownership regressions use actual PNGs, HTTP and the task runtime.
use crate::{agent_service as agent, visual_artifacts as visual};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
    time::Duration,
};

struct TestRoot(PathBuf);
impl TestRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("codex-visual-test-{}", visual::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn context(task: &str) -> visual::VisualContext {
    visual::VisualContext {
        identity: json!({"task_id":task,"request_id":1,"work_id":"w","node_id":"n","revision":1,"plan_revision":1}),
        ..Default::default()
    }
}
fn png(width: u32, height: u32) -> Vec<u8> {
    let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        width,
        height,
        image::Rgba([210, 40, 90, 255]),
    ));
    let mut bytes = Vec::new();
    img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    bytes
}
fn task(root: &Path, id: &str) {
    agent::open_db(root).unwrap().execute("INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES (?1,'visual test','fake-model','running',0,0)",[id]).unwrap();
}
fn supported(state: &mut agent::AgentServiceState) {
    state
        .visual
        .capabilities
        .entry(String::new())
        .or_default()
        .insert("fake-model".into(), visual::ImageCapability::Supported);
}
fn images(body: &Value) -> Vec<Vec<u8>> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter_map(|block| block["image_url"]["url"].as_str())
        .map(|url| {
            STANDARD
                .decode(url.strip_prefix("data:image/png;base64,").unwrap())
                .unwrap()
        })
        .collect()
}
fn check(dispatch: &Value) -> Value {
    let selected = dispatch["images"]
        .as_array()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let current = selected.last();
    let observed=current.map(|image|vec![json!({"artifact_id":image["artifact_id"],"region":"viewport","fact":"fixture image was supplied"})]).unwrap_or_default();
    json!({"artifact_ids":selected.iter().map(|image|image["artifact_id"].clone()).collect::<Vec<_>>(),
    "checked_goal":dispatch["checked_goal"],"expected_visible_result":"a visible slide fixture","request_trace_id":dispatch["request_trace_id"],"assessment":"pass",
    "observed_facts":observed,"limitations":["scripted model validates transport, not visual understanding"]})
}
#[tokio::test]
#[ignore = "real configured Observer replay; requires OBSERVER_REPLAY_AUDIT and OBSERVER_REPLAY_ROOT"]
async fn real_observer_original_multimodal_history_recheck() -> anyhow::Result<()> {
    let audit = PathBuf::from(std::env::var("OBSERVER_REPLAY_AUDIT")?);
    let original_root = PathBuf::from(std::env::var("OBSERVER_REPLAY_ROOT")?);
    let out = audit.parent().unwrap().join(format!(
        "shared-image-retrospective-recheck-{}",
        visual::new_id()
    ));
    std::fs::create_dir_all(&out)?;
    let source: Value =
        serde_json::from_slice(&std::fs::read(audit.join("request-contexts/607.json"))?)?;
    let mut input = crate::session_history::parse_plain_context(
        source["body"]["messages"][1]["content"].as_str().unwrap(),
    );
    let task_id = input["identity"]["task_id"].as_str().unwrap().to_owned();
    let mut history = crate::session_history::task_process(
        &original_root,
        &task_id,
        input["identity"]["request_id"].as_u64().unwrap() as usize,
    )
    .await?;
    let before = input["task_history"]["history_reference"]["before_seq"]
        .as_i64()
        .unwrap();
    history["records"]
        .as_array_mut()
        .unwrap()
        .retain(|record| record["seq"].as_i64().unwrap() < before);
    history["total_records"] = json!(history["records"].as_array().unwrap().len());
    history["history_reference"] = input["task_history"]["history_reference"].clone();
    input["task_history"] = history.clone();
    // Isolate test event writes while preserving original IDs, metadata and bytes.
    task(&out, &task_id);
    let original = agent::open_db(&original_root)?;
    let target = agent::open_db(&out)?;
    target.execute_batch("CREATE TABLE IF NOT EXISTS agent_visual_artifacts(artifact_id TEXT PRIMARY KEY,task_id TEXT NOT NULL,metadata TEXT NOT NULL)")?;
    let mut stmt = original
        .prepare("SELECT artifact_id,metadata FROM agent_visual_artifacts WHERE task_id=?1")?;
    let artifacts = stmt
        .query_map([&task_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, raw) in &artifacts {
        let item: Value = serde_json::from_str(raw)?;
        let path = item["workspace_relative_path"].as_str().unwrap();
        let destination = out.join(path);
        std::fs::create_dir_all(destination.parent().unwrap())?;
        std::fs::copy(original_root.join(path), destination)?;
        target.execute(
            "INSERT INTO agent_visual_artifacts(artifact_id,task_id,metadata) VALUES (?1,?2,?3)",
            rusqlite::params![id, task_id, raw],
        )?;
    }
    // Retain exact history reads if the model asks for an original event.
    let mut events=original.prepare("SELECT seq,timestamp,kind,data FROM agent_task_events WHERE task_id=?1 AND seq<?2 ORDER BY seq")?;
    let rows = events
        .query_map(rusqlite::params![task_id, before], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (seq, time, kind, data) in rows {
        target.execute("INSERT INTO agent_task_events(task_id,seq,timestamp,kind,data) VALUES (?1,?2,?3,?4,?5)",rusqlite::params![task_id,seq,time,kind,data])?;
    }
    target.execute_batch("CREATE TABLE IF NOT EXISTS agent_context_debug(task_id TEXT PRIMARY KEY,enabled INTEGER NOT NULL DEFAULT 0)")?;
    target.execute(
        "INSERT OR REPLACE INTO agent_context_debug(task_id,enabled) VALUES (?1,1)",
        [&task_id],
    )?;
    let mut state = agent::tests::test_state(&out, "127.0.0.1:1".parse()?);
    let config = crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state, &config);
    let model = source["body"]["model"].as_str().unwrap();
    let started = std::time::Instant::now();
    let result = agent::observer_json_response(
        &state,
        model,
        include_str!("../prompts/observer_retrospective.md"),
        input,
        3072,
        &task_id,
        json!({"stage":"retrospective_recheck"}),
    )
    .await;
    let mut requests = target.prepare(
        "SELECT metadata,request_json FROM agent_request_contexts WHERE task_id=?1 ORDER BY id",
    )?;
    let requests = requests
        .query_map([&task_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (index, (_, body)) in requests.iter().enumerate() {
        std::fs::write(out.join(format!("request-{index}.json")), body)?;
    }
    let raw = result
        .as_ref()
        .cloned()
        .unwrap_or_else(|error| format!("{error:#}"));
    std::fs::write(out.join("observer-response.txt"), &raw)?;
    let pixel_inputs = requests
        .iter()
        .map(|(_, body)| {
            images(&serde_json::from_str::<Value>(body).unwrap())
                .iter()
                .map(|bytes| json!({"bytes":bytes.len(),"hash":visual::hash(bytes)}))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    std::fs::write(
        out.join("report.json"),
        serde_json::to_vec_pretty(
            &json!({"model":model,"provider":state.observer_provider,"original_task":task_id,"through_seq_exclusive":before,"process_records":history["total_records"],"elapsed_ms":started.elapsed().as_millis(),"request_image_inputs":pixel_inputs,"succeeded":result.is_ok(),"response":raw,"original_task_modified":false,"new_captures":0}),
        )?,
    )?;
    println!("Observer original multimodal recheck: {}", out.display());
    result?;
    anyhow::ensure!(
        pixel_inputs.first().is_some_and(|images| images.len() == 2),
        "the actual Observer request must receive both original images"
    );
    Ok(())
}

#[test]
fn visual_artifacts_are_immutable_and_require_task_request_and_explicit_upstream_scope() {
    let root = TestRoot::new();
    let owner = context("a");
    let bytes = png(120, 90);
    let first = visual::save(
        &root.0,
        &owner,
        json!({"page_id":"a","page_epoch":1}),
        &bytes,
    )
    .unwrap();
    let second = visual::save(
        &root.0,
        &owner,
        json!({"page_id":"a","page_epoch":2}),
        &bytes,
    )
    .unwrap();
    assert_ne!(first["artifact_id"], second["artifact_id"]);
    assert_ne!(
        first["workspace_relative_path"],
        second["workspace_relative_path"]
    );
    let id = first["artifact_id"].as_str().unwrap();
    assert_eq!(visual::read(&root.0, "a", id).unwrap().1, bytes);
    assert!(visual::read(&root.0, "b", id).is_err());
    assert!(visual::metadata(&root.0, "a", "../../fake.png").is_err());
    let mut other = owner.clone();
    other.identity["revision"] = json!(2);
    assert!(visual::view(&root.0, &other, id).is_err());
    other.allowed_artifact_ids.push(id.into());
    assert!(visual::view(&root.0, &other, id).is_ok());
    other.identity["request_id"] = json!(2);
    assert!(visual::view(&root.0, &other, id).is_err());
    std::fs::write(
        root.0
            .join(first["workspace_relative_path"].as_str().unwrap()),
        png(121, 90),
    )
    .unwrap();
    let error = visual::read(&root.0, "a", id).unwrap_err().to_string();
    assert!(
        error.contains("content_hash"),
        "tampered bytes must fail the content hash check: {error}"
    );
}

/// Rerun only acceptance against a completed real task. Browser/project
/// operations are not repeated; writes are acceptance events and request logs.

#[test]
fn visual_budget_preserves_the_original_and_records_the_scaled_input_hash() {
    let root = TestRoot::new();
    let context = context("a");
    let bytes = png(2400, 2100);
    let artifact = visual::save(&root.0, &context, json!({}), &bytes).unwrap();
    let id = artifact["artifact_id"].as_str().unwrap();
    assert!(visual::image_block(&root.0, &context, id, true).is_err());
    let (block, sent) = visual::image_block(&root.0, &context, id, false).unwrap();
    assert_eq!(sent["mode"], "scaled");
    let data = STANDARD
        .decode(
            block["image_url"]["url"]
                .as_str()
                .unwrap()
                .split_once(',')
                .unwrap()
                .1,
        )
        .unwrap();
    assert_eq!(sent["input_hash"], visual::hash(&data));
    assert!(
        sent["input_width"].as_u64().unwrap() * sent["input_height"].as_u64().unwrap()
            <= visual::MAX_INPUT_PIXELS
    );
    assert_eq!(visual::read(&root.0, "a", id).unwrap().1, bytes);
    assert!(visual::save(&root.0, &context, json!({}), b"not an image").is_err());
}

#[tokio::test]
async fn visual_dispatch_capability_budget_restore_and_result_binding_are_explicit() {
    let root = TestRoot::new();
    let ctx = context("task");
    task(&root.0, "task");
    let ids = (0..3)
        .map(|_| {
            visual::save(&root.0, &ctx, json!({}), &png(80, 60)).unwrap()["artifact_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    let mut state = agent::tests::test_state(&root.0, "127.0.0.1:1".parse().unwrap());
    let mut body = json!({"model":"fake-model","messages":[]});
    let unknown = visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &ctx,
        &ids,
        "check slide",
        &mut body,
    )
    .await
    .unwrap();
    assert_eq!(unknown["status"], "unknown_capability");
    assert!(images(&body).is_empty());
    assert!(unknown["images"].as_array().unwrap().is_empty());
    assert_eq!(
        body["messages"],
        json!([]),
        "unavailability stays in the dispatch record; the host adds no message"
    );
    let mut unavailable = json!({"request_trace_id":unknown["request_trace_id"],"checked_goal":"check slide","artifact_ids":[ids[0]],"assessment":"unavailable",
        "observed_facts":[],"issues":[],"limitations":["image input unavailable"]});
    assert!(
        visual::validate_check(&root.0, &ctx, &unavailable, &[unknown.clone()], "worker").is_ok(),
        "an unavailable result may cite the undelivered screenshot"
    );
    unavailable["assessment"] = json!("pass");
    assert!(
        visual::validate_check(&root.0, &ctx, &unavailable, &[unknown.clone()], "worker").is_err(),
        "a pass still needs a delivered image"
    );
    supported(&mut state);
    let mut body = json!({"model":"fake-model","messages":[]});
    let sent = visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &ctx,
        &ids,
        "check slide",
        &mut body,
    )
    .await
    .unwrap();
    assert_eq!(images(&body).len(), 2);
    assert_eq!(sent["omitted_by_image_budget"], 1);
    for (bytes, meta) in images(&body).iter().zip(sent["images"].as_array().unwrap()) {
        assert_eq!(meta["input_hash"], visual::hash(bytes));
        assert_eq!(image::load_from_memory(bytes).unwrap().width(), 80);
    }
    let valid = check(&sent);
    assert!(visual::validate_check(&root.0, &ctx, &valid, &[sent.clone()], "worker").is_ok());
    let mut invented = valid.clone();
    invented["observed_facts"][0]["artifact_id"] = json!(ids[2]);
    assert!(visual::validate_check(&root.0, &ctx, &invented, &[sent.clone()], "worker").is_err());
    assert!(visual::validate_check(&root.0, &ctx, &valid, &[], "worker").is_err());
    let mut advanced = ctx.clone();
    advanced.execution_epoch = 1;
    let historical =
        visual::validate_check(&root.0, &advanced, &valid, &[sent.clone()], "worker").unwrap();
    assert_eq!(
        historical["source_binding"]["execution_epoch"], ctx.execution_epoch,
        "later execution must not rewrite the state associated with the image request"
    );
    assert_eq!(
        historical["source_binding"]["related_source_versions"],
        sent["related_source_versions"]
    );
    assert_eq!(
        historical["source_binding"]["current_page"],
        sent["current_page"]
    );
    assert_eq!(
        historical["visual_request"], sent,
        "preserve the original request for the AI to judge applicability"
    );
    assert_eq!(
        historical["artifacts"][0]["execution_epoch"],
        ctx.execution_epoch
    );
    let mut wrong = valid.clone();
    wrong["artifact_ids"] = json!([ids[2]]);
    assert!(visual::validate_check(&root.0, &ctx, &wrong, &[sent.clone()], "worker").is_err());
    let saved = serde_json::to_string(&ids).unwrap();
    assert!(!saved.contains("base64"));
    let restored: Vec<String> = serde_json::from_str(&saved).unwrap();
    let mut recovered = json!({"messages":[]});
    visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &ctx,
        &restored,
        "check slide",
        &mut recovered,
    )
    .await
    .unwrap();
    assert_eq!(images(&recovered), images(&body));
    let mcp =
        visual::mcp_result(&root.0, &ctx, visual::view(&root.0, &ctx, &ids[0]).unwrap()).unwrap();
    assert_eq!(mcp["content"][1]["type"], "image");
    assert_eq!(
        STANDARD
            .decode(mcp["content"][1]["data"].as_str().unwrap())
            .unwrap(),
        png(80, 60)
    );
}

#[tokio::test]
async fn visual_reports_keep_capture_facts_when_page_or_source_changes() {
    let root = TestRoot::new();
    task(&root.0, "task");
    let mut before = context("task");
    before.execution_epoch = 0;
    before.related_source_versions = json!({"src/app.ts":"v1"});
    let old = visual::save(
        &root.0,
        &before,
        json!({"browser_session_id":"session","page_id":"page","page_epoch":1}),
        &png(80, 60),
    )
    .unwrap()["artifact_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut current = before.clone();
    current.execution_epoch = 1;
    current.current_page = json!({"browser_session_id":"session","page_id":"page","page_epoch":2});
    let mut state = agent::tests::test_state(&root.0, "127.0.0.1:1".parse().unwrap());
    supported(&mut state);

    let mut body = json!({"messages":[]});
    let old_only = visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &current,
        &[old.clone()],
        "check slide",
        &mut body,
    )
    .await
    .unwrap();
    assert_eq!(old_only["images"][0]["page_epoch"], 1);
    assert_eq!(old_only["current_page"]["page_epoch"], 2);
    assert!(
        visual::validate_check(&root.0, &current, &check(&old_only), &[old_only], "worker").is_ok(),
        "the model decides how to use the earlier capture"
    );

    let latest = visual::save(
        &root.0,
        &current,
        json!({"browser_session_id":"session","page_id":"page","page_epoch":2}),
        &png(80, 60),
    )
    .unwrap()["artifact_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut body = json!({"messages":[]});
    let comparison = visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &current,
        &[old.clone(), latest.clone()],
        "check slide",
        &mut body,
    )
    .await
    .unwrap();
    assert_eq!(comparison["images"][0]["page_epoch"], 1);
    assert_eq!(comparison["images"][1]["page_epoch"], 2);
    let mut valid = check(&comparison);
    valid["observed_facts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"artifact_id":old,"region":"before","fact":"historical comparison image"}));
    assert!(
        visual::validate_check(&root.0, &current, &valid, &[comparison.clone()], "worker").is_ok(),
        "before/after observations are preserved"
    );
    valid["observed_facts"] =
        json!([{"artifact_id":old,"region":"before","fact":"historical comparison image"}]);
    assert!(
        visual::validate_check(&root.0, &current, &valid, &[comparison], "worker").is_ok(),
        "the host does not judge the sufficiency of historical facts"
    );

    let mut changed = current.clone();
    changed.execution_epoch = 2;
    changed.related_source_versions = json!({"src/app.ts":"v2"});
    let mut body = json!({"messages":[]});
    let restored = visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &changed,
        &[latest],
        "check slide",
        &mut body,
    )
    .await
    .unwrap();
    assert_eq!(
        restored["images"][0]["related_source_versions"]["src/app.ts"],
        "v1"
    );
    assert_eq!(restored["related_source_versions"]["src/app.ts"], "v2");
    assert!(
        visual::validate_check(&root.0, &changed, &check(&restored), &[restored], "worker").is_ok()
    );
}

fn fixture(root: &Path) -> String {
    let path = root.join("slide.html");
    std::fs::write(&path,"<html><body><canvas id='slide' width='800' height='450'></canvas><script>const c=document.querySelector('canvas').getContext('2d');c.fillStyle='#bb2255';c.fillRect(0,0,800,450);c.fillStyle='white';c.font='40px sans-serif';c.fillText('Canvas slide',40,90)</script></body></html>").unwrap();
    reqwest::Url::from_file_path(&path).unwrap().to_string()
}

#[tokio::test]
async fn unavailable_report_can_reference_original_captures_from_separate_requests() {
    let root = TestRoot::new();
    task(&root.0, "task");
    let ctx = context("task");
    let first = visual::save(&root.0, &ctx, json!({}), &png(80, 60)).unwrap()["artifact_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = visual::save(&root.0, &ctx, json!({}), &png(90, 60)).unwrap()["artifact_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let state = agent::tests::test_state(&root.0, "127.0.0.1:1".parse().unwrap());
    let mut body = json!({"messages":[]});
    let first_request = visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &ctx,
        &[first.clone()],
        "check slide",
        &mut body,
    )
    .await
    .unwrap();
    let second_request = visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &ctx,
        &[second.clone()],
        "check slide",
        &mut body,
    )
    .await
    .unwrap();
    let report = json!({"assessment":"unavailable","checked_goal":"check slide",
        "artifact_ids":[first,second],"request_trace_id":second_request["request_trace_id"],
        "limitations":["Screenshots captured; image input unavailable"]});
    let checked = visual::validate_check(
        &root.0,
        &ctx,
        &report,
        &[first_request, second_request],
        "worker",
    )
    .unwrap();
    assert_eq!(checked["assessment"], "unavailable");
    assert_eq!(checked["artifact_ids"], report["artifact_ids"]);
    assert!(images(&body).is_empty());
}

#[tokio::test]
async fn restoring_a_reference_reports_changed_bytes_without_sending_them() {
    let root = TestRoot::new();
    task(&root.0, "task");
    let ctx = context("task");
    let artifact = visual::save(&root.0, &ctx, json!({}), &png(80, 60)).unwrap();
    let id = artifact["artifact_id"].as_str().unwrap().to_owned();
    let mut state = agent::tests::test_state(&root.0, "127.0.0.1:1".parse().unwrap());
    supported(&mut state);
    let dispatch = visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &ctx,
        &[id],
        "check slide",
        &mut json!({"messages":[]}),
    )
    .await
    .unwrap();
    let reference = visual::conversation_message(&dispatch);
    std::fs::write(
        root.0
            .join(artifact["workspace_relative_path"].as_str().unwrap()),
        png(90, 60),
    )
    .unwrap();
    let mut body = json!({"messages":[reference]});
    let restored = visual::prepare_request(
        &state,
        "worker",
        "fake-model",
        &ctx,
        &[],
        "check slide",
        &mut body,
    )
    .await
    .unwrap();
    assert!(images(&body).is_empty());
    assert!(
        restored["unavailable"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("content_hash")
    );
}

#[tokio::test]
async fn visual_browser_pages_are_task_owned_and_observer_cannot_follow_replaced_instances() {
    let root = TestRoot::new();
    let workspace = crate::tools::Workspace::new(&root.0).unwrap();
    let a = context("a");
    let b = context("b");
    let a_url = fixture(&root.0);
    let other = root.0.join("other.html");
    std::fs::write(&other, "<html><body>Task B</body></html>").unwrap();
    let b_url = reqwest::Url::from_file_path(&other).unwrap().to_string();
    let a_args = json!({"url":a_url});
    let b_args = json!({"url":b_url});
    let (first, second) = tokio::join!(
        crate::browser_control::execute_scoped(&workspace, &a, "browser_open", &a_args),
        crate::browser_control::execute_scoped(&workspace, &b, "browser_open", &b_args)
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_ne!(first["page"]["page_id"], second["page"]["page_id"]);
    let shot =
        crate::browser_control::execute_scoped(&workspace, &a, "browser_screenshot", &json!({}))
            .await
            .unwrap();
    assert_eq!(shot["visual_artifact"]["url"], a_url);
    assert!(
        crate::browser_control::observe_page_snapshot(&root.0, &a, &first["page"])
            .await
            .is_ok(),
        "normal and canonical workspace paths refer to the same owned session"
    );
    assert!(
        crate::browser_control::observe_page_snapshot(&root.0, &b, &first["page"])
            .await
            .is_err()
    );
    let mut replacement = a.clone();
    replacement.identity["request_id"] = json!(2);
    replacement.identity["revision"] = json!(2);
    let continued = crate::browser_control::execute_scoped(
        &workspace,
        &replacement,
        "browser_read",
        &json!({}),
    )
    .await
    .unwrap();
    assert_eq!(
        continued["page"]["page_id"], first["page"]["page_id"],
        "a new request in the same conversation resumes the existing page"
    );
    assert_eq!(
        continued["page"]["url"], a_url,
        "continuing must not navigate the loaded page"
    );
    assert_eq!(continued["page"]["identity"], replacement.identity);
    assert!(
        crate::browser_control::observe_page_snapshot(&root.0, &a, &first["page"])
            .await
            .is_err(),
        "an old Observer instance cannot act as the new caller"
    );
    crate::browser_control::execute_scoped(
        &workspace,
        &replacement,
        "browser_open",
        &json!({"url":b_url}),
    )
    .await
    .unwrap();
    assert!(
        crate::browser_control::observe_page_snapshot(&root.0, &a, &first["page"])
            .await
            .is_err()
    );
    assert_eq!(
        visual::read(&root.0, "a", shot["artifact_id"].as_str().unwrap())
            .unwrap()
            .0["identity"],
        a.identity
    );
    crate::browser_control::cleanup(&root.0, "a").await;
    crate::browser_control::cleanup(&root.0, "b").await;
}
#[tokio::test]
#[ignore = "real provider acceptance; requires the locally built PPT demo and editor harness"]
async fn real_ppt_visual_acceptance() -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    let root = workspace
        .join(".codex-workspace-mcp")
        .join(format!("visual-acceptance-{}", visual::new_id()));
    std::fs::create_dir_all(&root)?;
    let demo = std::env::var_os("PPT_DEMO_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("../pptx-editor-engine/dist"));
    anyhow::ensure!(
        demo.join("index.html").is_file(),
        "build the existing PPT demo first"
    );
    let harness = workspace.join(".codex-workspace-mcp/visual-acceptance-editor");
    anyhow::ensure!(
        harness.join("index.html").is_file(),
        "build the existing SlideEditor harness first"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = axum::Router::new()
        .nest_service("/editor", tower_http::services::ServeDir::new(harness))
        .fallback_service(tower_http::services::ServeDir::new(demo));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut state = agent::tests::test_state(&root, addr);
    let config = crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state, &config);
    let model = state
        .model_map
        .get(&state.default_model)
        .cloned()
        .unwrap_or(state.default_model.clone());
    let declared = state.visual.capability(&state.provider_name, &model);
    task(&root, "real-ppt");
    let mut ctx = context("real-ppt");
    ctx.identity["node_id"] = json!("load_demo");
    // An opt-in bounded capability experiment, cached only for this acceptance run.
    state
        .visual
        .capabilities
        .entry(state.provider_name.clone())
        .or_default()
        .insert(model.clone(), visual::ImageCapability::Supported);
    let probe = std::env::var_os("VISUAL_ACCEPTANCE_CAPABILITY_PROBED").is_none();
    let mut report = json!({"route":{"provider":state.provider_name,"model":model},"declared_image_capability":declared,"capability_probe":probe,"global_config_changed":false,
        "driver":"host browser scenario with real role model image judgments; autonomous Worker transport is covered separately","worker_calls":0,"observer_calls":0,"visual_service_calls":0,"screenshots":0,"image_reuse":0,"extra_worker_receipt_rounds":0,"cases":[]});
    crate::browser_control::execute_scoped(
        &state.workspace,
        &ctx,
        "browser_open",
        &json!({"url":format!("http://{addr}/")}),
    )
    .await?;
    crate::browser_control::execute_scoped(
        &state.workspace,
        &ctx,
        "browser_click",
        &json!({"selector":"#btn-welcome-demo"}),
    )
    .await?;
    for _ in 0..30 {
        let ready = crate::browser_control::execute_scoped(
            &state.workspace,
            &ctx,
            "browser_read",
            &json!({"expect_text":"Slide 1"}),
        )
        .await?;
        if ready["matched"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let read =
        crate::browser_control::execute_scoped(&state.workspace, &ctx, "browser_read", &json!({}))
            .await?;
    let page_debug=crate::browser_control::inspect_test_page(&root,&ctx,"({resources:performance.getEntriesByType('resource').map(r=>({name:r.name,bytes:r.transferSize})),slide_count:document.querySelectorAll('#slide-list .slide-item').length})").await?;
    let shot = crate::browser_control::execute_scoped(
        &state.workspace,
        &ctx,
        "browser_screenshot",
        &json!({}),
    )
    .await?;
    report["screenshots"] = json!(1);
    let id = shot["artifact_id"].as_str().unwrap().to_owned();
    let load_goal = "Inspect the loaded PPT demo screenshot. Describe the actual slide content, shapes, colors and any empty or clipped regions; assess whether a slide is rendered. DOM text alone cannot establish this.";
    let first = real_visual_judgment(&state, &model, &ctx, &[id], load_goal, probe).await;
    report["worker_calls"] = json!(1);
    let probe_ok = first["request_succeeded"] == true;
    report["probe_image_request_accepted"] = json!(probe_ok);
    report["cases"].as_array_mut().unwrap().push(json!({"case":"load_demo","dom":read,"page_debug":page_debug,"artifact":shot["visual_artifact"],"worker":first}));
    if probe_ok {
        ctx.identity["node_id"] = json!("delete_node");
        ctx.identity["work_id"] = json!("delete_node");
        crate::browser_control::execute_scoped(
            &state.workspace,
            &ctx,
            "browser_open",
            &json!({"url":format!("http://{addr}/editor/")}),
        )
        .await?;
        let before = crate::browser_control::execute_scoped(
            &state.workspace,
            &ctx,
            "browser_screenshot",
            &json!({}),
        )
        .await?;
        let before_state = crate::browser_control::execute_scoped(
            &state.workspace,
            &ctx,
            "browser_read",
            &json!({"expect_text":"target_count=1"}),
        )
        .await?;
        crate::browser_control::execute_scoped(
            &state.workspace,
            &ctx,
            "browser_click",
            &json!({"selector":"#delete-target"}),
        )
        .await?;
        crate::browser_control::execute_scoped(
            &state.workspace,
            &ctx,
            "browser_press_key",
            &json!({"key":"Delete"}),
        )
        .await?;
        let after_state = crate::browser_control::execute_scoped(
            &state.workspace,
            &ctx,
            "browser_read",
            &json!({"expect_text":"target_count=0; edits=1"}),
        )
        .await?;
        let node_state=crate::browser_control::inspect_test_page(&root,&ctx,"({target_count:document.querySelectorAll('#delete-target').length,blue_circle_count:document.querySelectorAll('svg circle').length,heading_present:[...document.querySelectorAll('svg text')].some(t=>t.textContent==='Quarterly product review')})").await?;
        let after = crate::browser_control::execute_scoped(
            &state.workspace,
            &ctx,
            "browser_screenshot",
            &json!({}),
        )
        .await?;
        report["screenshots"] = json!(3);
        let ids = vec![
            before["artifact_id"].as_str().unwrap().to_owned(),
            after["artifact_id"].as_str().unwrap().to_owned(),
        ];
        let goal = "Compare the before and after PPT editor images, in that order. Check whether the pink rectangular visual node was removed while the blue circle and heading remain. Describe what you actually see in each image. The host separately records target_count 1 to 0 and an onChange callback; pixels alone cannot prove editing persistence.";
        let worker = real_visual_judgment(&state, &model, &ctx, &ids, goal, false).await;
        report["worker_calls"] = json!(2);
        // Use the same explicitly probed main route for a separate Observer context.
        state.observer_provider = state.provider_name.clone();
        state.observer_provider_url = state.provider_url.clone();
        state.observer_api_key = state.api_key.clone();
        let context = json!({"identity":ctx.identity,"request":{"goal":goal},"visual_artifacts":[before["visual_artifact"],after["visual_artifact"]],"delivery":{"before_state":before_state,"after_state":after_state}});
        let start = std::time::Instant::now();
        let result=agent::observer_json_response(&state,&model,
            "Review this visual delivery independently and concisely. Output JSON with assessment on_track|adjust|uncertain, summary and visual_check_result. In visual_check_result cite the Host visual dispatch request_trace_id, exact checked_goal and artifact_ids; expected_visible_result; assessment pass|issue|uncertain|unavailable; record observed_facts with artifact_id, region, fact and limitations. Only assess visible facts.",context,1400,"real-ppt",json!({"acceptance":true})).await;
        let observer = match result {
            Ok(raw) => serde_json::from_str::<Value>(&raw)
                .unwrap_or(json!({"assessment":"uncertain","raw":raw})),
            Err(error) => json!({"assessment":"unavailable","error":error.to_string()}),
        };
        report["observer_calls"] = json!(1);
        report["image_reuse"] = json!(2);
        report["cases"].as_array_mut().unwrap().push(json!({"case":"delete_visual_node","before":before["visual_artifact"],"after":after["visual_artifact"],"before_state":before_state,"after_state":after_state,"node_state":node_state,"worker":worker,"observer":observer,"observer_elapsed_ms":start.elapsed().as_millis(),"new_observer_screenshots":0}));
    }
    std::fs::write(
        root.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    crate::browser_control::cleanup(&root, "real-ppt").await;
    server.abort();
    println!(
        "Real visual acceptance report: {}",
        root.join("report.json").display()
    );
    Ok(())
}

async fn real_visual_judgment(
    state: &agent::AgentServiceState,
    model: &str,
    ctx: &visual::VisualContext,
    ids: &[String],
    goal: &str,
    probe: bool,
) -> Value {
    let mut body = json!({"model":model,"stream":false,"max_tokens":1800,"reasoning_effort":"low","messages":[{"role":"system","content":"Inspect the supplied images for this goal. Output only JSON with artifact_ids, checked_goal, expected_visible_result, request_trace_id from Host visual dispatch, assessment pass|issue|uncertain|unavailable, observed_facts [{artifact_id,region,fact}], issues and limitations. Facts must come from actual visible pixels; report uncertain if no images or insufficient detail. Do not invent parser causes or interaction success."}]});
    let start = std::time::Instant::now();
    let dispatch =
        match visual::prepare_request(state, "worker", model, ctx, ids, goal, &mut body).await {
            Ok(d) => d,
            Err(e) => return json!({"assessment":"unavailable","error":e.to_string()}),
        };
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let response = state
            .client
            .post(format!(
                "{}/chat/completions",
                state.provider_url.trim_end_matches('/')
            ))
            .bearer_auth(&state.api_key)
            .header("x-codex-visual-dispatch", "task-owned")
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let response: Value = response.json().await?;
        let raw = response
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("model returned no text"))?;
        let check: Value = serde_json::from_str(
            raw.trim()
                .trim_start_matches("```json")
                .trim_end_matches("```")
                .trim(),
        )?;
        Ok::<_, anyhow::Error>((check, response["usage"].clone()))
    })
    .await
    .map_err(|_| anyhow::anyhow!("45s visual acceptance budget exceeded"))
    .and_then(|r| r);
    match result {
        Ok((check, usage)) => {
            let validated = visual::validate_check(
                state.workspace.root(),
                ctx,
                &check,
                &[dispatch.clone()],
                "worker",
            );
            let validation = match validated {
                Ok(check) => json!({"valid":true,"check":check}),
                Err(e) => json!({"valid":false,"error":e.to_string()}),
            };
            json!({"request_succeeded":true,"capability_probe":probe,"manifest":dispatch,"check":check,"host_validation":validation,"usage":usage,"elapsed_ms":start.elapsed().as_millis()})
        }
        Err(error) => {
            json!({"request_succeeded":false,"assessment":"unavailable","capability_probe":probe,"manifest":dispatch,"error":error.to_string(),"elapsed_ms":start.elapsed().as_millis()})
        }
    }
}

#[tokio::test]
#[ignore = "real provider recheck of preserved task-owned images; requires VISUAL_ACCEPTANCE_REPORT"]
async fn real_visual_observer_reuse_recheck() -> anyhow::Result<()> {
    let path = PathBuf::from(std::env::var("VISUAL_ACCEPTANCE_REPORT")?);
    let root = path.parent().unwrap();
    let report: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let case = &report["cases"][1];
    let mut state = agent::tests::test_state(root, "127.0.0.1:1".parse()?);
    let config = crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state, &config);
    let model = report["route"]["model"].as_str().unwrap();
    anyhow::ensure!(
        report["probe_image_request_accepted"] == true,
        "requires an already successful explicit capability probe"
    );
    state
        .visual
        .capabilities
        .entry(state.provider_name.clone())
        .or_default()
        .insert(model.into(), visual::ImageCapability::Supported);
    state.observer_provider = state.provider_name.clone();
    state.observer_provider_url = state.provider_url.clone();
    state.observer_api_key = state.api_key.clone();
    let input = json!({"identity":case["after"]["identity"],"request":{"goal":case["worker"]["manifest"]["checked_goal"]},"visual_artifacts":[case["before"],case["after"]],"delivery":{"node_state":case["node_state"]}});
    let start = std::time::Instant::now();
    let response=agent::observer_json_response(&state,model,"Review the before/after slide images independently. Be concise. Return JSON with assessment on_track|adjust|uncertain, summary and visual_check_result using the complete host-provided structure. Preserve every binding field verbatim and replace placeholders with visible facts.",input,1400,"real-ppt",json!({"acceptance_recheck":true})).await;
    let response = match response {
        Ok(raw) => serde_json::from_str::<Value>(&raw)?,
        Err(error) => json!({"assessment":"unavailable","error":error.to_string()}),
    };
    let result = json!({"observer_calls":1,"worker_calls":0,"new_screenshots":0,"reused_images":2,"input_bytes":case["before"]["byte_size"].as_u64().unwrap()+case["after"]["byte_size"].as_u64().unwrap(),"elapsed_ms":start.elapsed().as_millis(),"global_config_changed":false,"response":response});
    std::fs::write(
        root.join("observer_recheck.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    println!(
        "Real Observer reuse report: {}",
        root.join("observer_recheck.json").display()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "real Observer inspection of a completed six-stage report; requires VISUAL_ACCEPTANCE_REPORT"]
async fn real_visual_observer_six_stage_reuse_recheck() -> anyhow::Result<()> {
    let path = PathBuf::from(std::env::var("VISUAL_ACCEPTANCE_REPORT")?);
    let root = path.parent().unwrap();
    let report: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let check = report["visual_checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| {
            check["actor"] == "worker"
                && check["artifact_ids"]
                    .as_array()
                    .is_some_and(|ids| ids.len() == 2)
        })
        .ok_or_else(|| anyhow::anyhow!("real Worker pair is required"))?;
    let dispatch = &check["visual_request"];
    let task = check["identity"]["task_id"].as_str().unwrap();
    let mut state = agent::tests::test_state(root, "127.0.0.1:1".parse()?);
    let config = crate::ai_proxy::load_config(&crate::ai_proxy::dsh_config_path())?;
    crate::ai_proxy::apply_agent_config(&mut state, &config);
    let model = report["route"]["model"].as_str().unwrap();
    state.observer_provider = state.provider_name.clone();
    state.observer_provider_url = state.provider_url.clone();
    state.observer_api_key = state.api_key.clone();
    anyhow::ensure!(
        state.visual.capability(&state.observer_provider, model)
            == visual::ImageCapability::Supported,
        "requires verified configured image support"
    );
    let conn = agent::open_db(root)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_context_debug(task_id TEXT PRIMARY KEY,enabled INTEGER NOT NULL DEFAULT 0)")?;
    conn.execute(
        "INSERT OR REPLACE INTO agent_context_debug(task_id,enabled) VALUES (?1,1)",
        [task],
    )?;
    let before: i64 = conn.query_row("SELECT COUNT(*) FROM agent_visual_artifacts", [], |row| {
        row.get(0)
    })?;
    let input = json!({"identity":check["identity"],"request":{"goal":check["checked_goal"]},"visual_artifacts":check["artifacts"],
        "execution_epoch":dispatch["execution_epoch"],"related_source_versions":dispatch["related_source_versions"],
        "task_page":dispatch["current_page"]});
    let started = std::time::Instant::now();
    let raw=agent::observer_json_response(&state,model,include_str!("../prompts/observer_system.md"),input,2400,task,
        json!({"stage":"saved_image_recheck","review_id":"real-reuse-recheck","identity":check["identity"]})).await?;
    let result: Value = serde_json::from_str(
        raw.trim()
            .trim_start_matches("```json")
            .trim_end_matches("```")
            .trim(),
    )?;
    let actual: String = conn.query_row(
        "SELECT request_json FROM agent_request_contexts WHERE task_id=?1 ORDER BY id DESC LIMIT 1",
        [task],
        |row| row.get(0),
    )?;
    let actual: Value = serde_json::from_str(&actual)?;
    let pixels = images(&actual);
    let after: i64 = conn.query_row("SELECT COUNT(*) FROM agent_visual_artifacts", [], |row| {
        row.get(0)
    })?;
    std::fs::write(
        root.join("observer_recheck.json"),
        serde_json::to_vec_pretty(&json!({"raw_response":raw,"result":result,
        "elapsed_ms":started.elapsed().as_millis() as u64,"new_captures":after-before,
        "actual_http_images":pixels.iter().map(|bytes|json!({"hash":visual::hash(bytes),"bytes":bytes.len()})).collect::<Vec<_>>()}))?,
    )?;
    anyhow::ensure!(
        pixels.len() == 2 && before == after,
        "Observer must reuse both saved pictures without recapturing"
    );
    anyhow::ensure!(
        result["visual_check_result"]["check_id"].is_string()
            && result["visual_check_result"]["observed_facts"]
                .as_array()
                .is_some_and(|facts| facts.len() >= 2),
        "Observer must return valid image-bound facts"
    );
    anyhow::ensure!(
        pixels
            .iter()
            .zip(result["visual_request"]["images"].as_array().unwrap())
            .all(|(bytes, image)| image["input_hash"] == visual::hash(bytes)),
        "actual Observer request pixels must match the recorded image inputs"
    );
    Ok(())
}
