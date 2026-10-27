//! Protocol and ownership regressions use actual PNGs, HTTP and the task runtime.
use crate::{agent_service as agent, visual_artifacts as visual};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
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
    assert_eq!(sent["execution_epoch"], ctx.execution_epoch);
    assert_eq!(sent["related_source_versions"], ctx.related_source_versions);
    assert_eq!(sent["current_page"], ctx.current_page);
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

fn fixture(root: &Path) -> String {
    let path = root.join("slide.html");
    std::fs::write(&path,"<html><body><canvas id='slide' width='800' height='450'></canvas><script>const c=document.querySelector('canvas').getContext('2d');c.fillStyle='#bb2255';c.fillRect(0,0,800,450);c.fillStyle='white';c.font='40px sans-serif';c.fillText('Canvas slide',40,90)</script></body></html>").unwrap();
    reqwest::Url::from_file_path(&path).unwrap().to_string()
}

#[tokio::test]
async fn visual_browser_pages_are_task_owned_across_continuations() {
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
    crate::browser_control::execute_scoped(
        &workspace,
        &replacement,
        "browser_open",
        &json!({"url":b_url}),
    )
    .await
    .unwrap();
    assert_eq!(
        visual::read(&root.0, "a", shot["artifact_id"].as_str().unwrap())
            .unwrap()
            .0["identity"],
        a.identity
    );
    crate::browser_control::cleanup(&root.0, "a").await;
    crate::browser_control::cleanup(&root.0, "b").await;
}
