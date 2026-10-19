//! Plugin tools that run in the plugin's own page. A plugin marks a tool with
//! `"executor": "page"`; the host then hands each call to the page that owns
//! the task (the one that sent the latest prompt) and waits for its answer.
//! Nothing here knows any plugin: the page takes calls and replies through the
//! generic plugin session methods `tool_calls` and `tool_result`.
//!
//! A task without a live page cannot run page tools. A page counts as gone when
//! it has not asked for calls for `OFFLINE`; the call then fails with
//! `PAGE_CLOSED` and the caller abandons the task.
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

/// A page that has not polled for this long is treated as closed.
pub const OFFLINE: Duration = Duration::from_secs(45);
/// Longest a page may take to answer a call it has taken.
pub const EXECUTION_LIMIT: Duration = Duration::from_secs(180);
/// Longest one `tool_calls` request is held open.
pub const MAX_WAIT: Duration = Duration::from_secs(25);
pub const PAGE_CLOSED: &str = "PAGE_CLOSED";

struct Call {
    id: String,
    name: String,
    arguments: Value,
}
#[derive(Default)]
struct TaskPages {
    owner: Option<String>,
    seen: BTreeMap<String, Instant>,
    queue: VecDeque<Call>,
    /// Calls waiting for an answer, with the time a page took them (None: still queued).
    waiting: BTreeMap<String, (oneshot::Sender<Value>, Option<Instant>)>,
}
#[derive(Default)]
struct Broker {
    tasks: Mutex<BTreeMap<String, TaskPages>>,
    notify: Notify,
}
fn broker() -> &'static Broker {
    static BROKER: OnceLock<Broker> = OnceLock::new();
    BROKER.get_or_init(Broker::default)
}
fn valid_page(page: &str) -> Result<()> {
    ensure!(
        !page.is_empty()
            && page.len() <= 64
            && page
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid page_id"
    );
    Ok(())
}

/// The page that sent a prompt runs that task's page tools from now on.
pub fn claim(task: &str, page: &str) -> Result<()> {
    valid_page(page)?;
    let mut tasks = broker().tasks.lock().unwrap();
    let entry = tasks.entry(task.to_owned()).or_default();
    entry.owner = Some(page.to_owned());
    entry.seen.insert(page.to_owned(), Instant::now());
    Ok(())
}

/// Calls for `page`, waiting up to `wait` for one to arrive. Pages that do not
/// own the task get none; polling still marks them alive. A task nobody has
/// claimed yet (host restarted, page opened before any prompt) goes to the
/// first page that asks.
pub async fn poll(task: &str, page: &str, wait: Duration) -> Result<Value> {
    valid_page(page)?;
    let deadline = Instant::now() + wait.min(MAX_WAIT);
    loop {
        let notified = broker().notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let mut tasks = broker().tasks.lock().unwrap();
            let entry = tasks.entry(task.to_owned()).or_default();
            entry.seen.insert(page.to_owned(), Instant::now());
            if entry.owner.is_none() {
                entry.owner = Some(page.to_owned());
            }
            let owner = entry.owner.as_deref() == Some(page);
            if owner && !entry.queue.is_empty() {
                let now = Instant::now();
                let calls: Vec<Value> = entry
                    .queue
                    .drain(..)
                    .map(|call| {
                        if let Some((_, taken)) = entry.waiting.get_mut(&call.id) {
                            *taken = Some(now);
                        }
                        json!({"call_id":call.id,"name":call.name,"arguments":call.arguments})
                    })
                    .collect();
                return Ok(json!({"calls":calls,"owner":true}));
            }
            if Instant::now() >= deadline {
                return Ok(json!({"calls":[],"owner":owner}));
            }
        }
        let _ = tokio::time::timeout_at(deadline.into(), notified).await;
    }
}

/// The owning page's answer to a call it took.
pub fn reply(task: &str, page: &str, call: &str, result: Value) -> Result<()> {
    valid_page(page)?;
    let mut tasks = broker().tasks.lock().unwrap();
    let entry = tasks.get_mut(task).ok_or_else(|| anyhow::anyhow!("no page tool calls for this task"))?;
    entry.seen.insert(page.to_owned(), Instant::now());
    ensure!(entry.owner.as_deref() == Some(page), "another page runs this task's tools");
    let (sender, _) = entry
        .waiting
        .remove(call)
        .ok_or_else(|| anyhow::anyhow!("unknown or expired call_id"))?;
    let _ = sender.send(result);
    Ok(())
}

fn alive(entry: &TaskPages) -> bool {
    entry
        .owner
        .as_ref()
        .and_then(|owner| entry.seen.get(owner))
        .is_some_and(|seen| seen.elapsed() < OFFLINE)
}

/// Run `name` in the task's page and return what the page answered.
pub async fn call(task: &str, name: &str, arguments: Value, cancel: &CancellationToken) -> Result<Value> {
    let id = crate::agent_service::uuid_like();
    let (sender, mut receiver) = oneshot::channel();
    {
        let mut tasks = broker().tasks.lock().unwrap();
        let entry = tasks.entry(task.to_owned()).or_default();
        if !alive(entry) {
            bail!("{PAGE_CLOSED}: 执行这个工具的页面已经关闭，任务已放弃");
        }
        entry.waiting.insert(id.clone(), (sender, None));
        entry.queue.push_back(Call { id: id.clone(), name: name.to_owned(), arguments });
    }
    broker().notify.notify_waiters();
    let forget = |reason: &str| -> anyhow::Error {
        let mut tasks = broker().tasks.lock().unwrap();
        if let Some(entry) = tasks.get_mut(task) {
            entry.waiting.remove(&id);
            entry.queue.retain(|call| call.id != id);
        }
        anyhow::anyhow!("{reason}")
    };
    loop {
        tokio::select! {
            answer = &mut receiver => {
                return answer.map_err(|_| forget("page tool call was dropped"));
            }
            _ = cancel.cancelled() => return Err(forget("task cancelled during page tool execution")),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        let gone = {
            let tasks = broker().tasks.lock().unwrap();
            match tasks.get(task) {
                None => true,
                Some(entry) => match entry.waiting.get(&id).map(|(_, taken)| *taken) {
                    // Taken: the page is working on it; give it the execution limit.
                    Some(Some(taken)) => taken.elapsed() > EXECUTION_LIMIT,
                    // Still queued: the owner must keep polling.
                    Some(None) => !alive(entry),
                    None => false,
                },
            }
        };
        if gone {
            return Err(forget(&format!("{PAGE_CLOSED}: 执行这个工具的页面没有响应（已关闭或断开），任务已放弃")));
        }
    }
}

/// Drop the page state of a finished or deleted task.
pub fn forget_task(task: &str) {
    broker().tasks.lock().unwrap().remove(task);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn owner_page_takes_calls_and_answers_them() {
        let task = format!("task-{}", crate::agent_service::uuid_like());
        claim(&task, "page-a").unwrap();
        let cancel = CancellationToken::new();
        let caller = {
            let task = task.clone();
            tokio::spawn(async move { call(&task, "render_slide", json!({"slide":1}), &cancel).await })
        };
        // Another tab polls: it is alive but gets nothing.
        let other = poll(&task, "page-b", Duration::from_millis(50)).await.unwrap();
        assert_eq!(other["owner"], false);
        let taken = poll(&task, "page-a", Duration::from_secs(2)).await.unwrap();
        let calls = taken["calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "render_slide");
        assert!(reply(&task, "page-b", calls[0]["call_id"].as_str().unwrap(), json!({})).is_err());
        reply(&task, "page-a", calls[0]["call_id"].as_str().unwrap(), json!({"ok":true})).unwrap();
        assert_eq!(caller.await.unwrap().unwrap(), json!({"ok":true}));
        forget_task(&task);
    }

    #[tokio::test]
    async fn a_task_without_a_live_page_fails_at_once() {
        let task = format!("task-{}", crate::agent_service::uuid_like());
        let error = call(&task, "build_deck", json!({}), &CancellationToken::new()).await.unwrap_err();
        assert!(error.to_string().starts_with(PAGE_CLOSED));
        forget_task(&task);
    }

    #[tokio::test]
    async fn the_latest_prompting_page_takes_over() {
        let task = format!("task-{}", crate::agent_service::uuid_like());
        claim(&task, "page-a").unwrap();
        claim(&task, "page-b").unwrap();
        let cancel = CancellationToken::new();
        let caller = {
            let task = task.clone();
            tokio::spawn(async move { call(&task, "apply_edits", json!({}), &cancel).await })
        };
        let a = poll(&task, "page-a", Duration::from_millis(50)).await.unwrap();
        assert_eq!(a["calls"].as_array().unwrap().len(), 0);
        let b = poll(&task, "page-b", Duration::from_secs(2)).await.unwrap();
        let id = b["calls"][0]["call_id"].as_str().unwrap().to_owned();
        reply(&task, "page-b", &id, json!({"ok":true})).unwrap();
        caller.await.unwrap().unwrap();
        forget_task(&task);
    }
}
