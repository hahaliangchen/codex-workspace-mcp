//! One writer per database: a thread that owns the only connection that writes, and runs the
//! write jobs sent to it one after the other, first come first served. Callers wait for their
//! job's result (a sequence number, the rows it read before writing, or the error), so a write is
//! visible to every reader as soon as the call returns. Readers keep their own connections; in
//! WAL mode they never wait for the writer.
//!
//! A job sent from inside another job (the writer thread calling back into a helper that writes)
//! runs on a connection of its own, as before this module existed, instead of queueing behind
//! itself. A writer that has been idle for a while ends its thread; the next write starts a new one.

use anyhow::{Context, Result, anyhow};
use rusqlite::Connection;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

struct Writer {
    sender: Sender<Job>,
    generation: u64,
}

/// Writers by workspace root (one database each).
fn writers() -> &'static Mutex<(u64, HashMap<PathBuf, Writer>)> {
    static WRITERS: OnceLock<Mutex<(u64, HashMap<PathBuf, Writer>)>> = OnceLock::new();
    WRITERS.get_or_init(|| Mutex::new((0, HashMap::new())))
}

/// How long a writer waits for work before its thread ends.
const IDLE: Duration = Duration::from_secs(60);

thread_local! {
    /// Set on a writer thread: a write requested from inside a job must not queue behind that job.
    static ON_WRITER: Cell<bool> = const { Cell::new(false) };
}

fn run(root: PathBuf, generation: u64, jobs: Receiver<Job>) {
    ON_WRITER.with(|flag| flag.set(true));
    let mut conn = match crate::agent_service::open_db(&root) {
        Ok(conn) => conn,
        Err(error) => {
            // Without a connection the queued jobs are dropped, so their callers get an error;
            // the next write starts a new writer and tries again.
            tracing::warn!(error = %format!("{error:#}"), root = %root.display(), "database writer could not open the database");
            forget(&root, generation);
            return;
        }
    };
    loop {
        let job = match jobs.recv_timeout(IDLE) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                // Idle: end, unless a job arrived while we decided (senders hold the registry lock).
                let mut registry = writers().lock().unwrap();
                match jobs.try_recv() {
                    Ok(job) => {
                        drop(registry);
                        job
                    }
                    Err(_) => {
                        if registry.1.get(&root).is_some_and(|writer| writer.generation == generation) {
                            registry.1.remove(&root);
                        }
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut conn)));
        // A job that failed or panicked half-way must not leave its transaction open for the next one.
        if !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        if outcome.is_err() {
            tracing::warn!(root = %root.display(), "a database write job panicked");
        }
    }
}

fn forget(root: &Path, generation: u64) {
    let mut registry = writers().lock().unwrap();
    if registry.1.get(root).is_some_and(|writer| writer.generation == generation) {
        registry.1.remove(root);
    }
}

/// Queue a job on the writer of `root`'s database, starting the writer when there is none.
fn submit(root: &Path, job: Job) -> Result<()> {
    let root = root.to_path_buf();
    let mut registry = writers().lock().unwrap();
    let job = match registry.1.get(&root) {
        Some(writer) => match writer.sender.send(job) {
            Ok(()) => return Ok(()),
            // The thread ended (it could not open the database): start a new one below.
            Err(mpsc::SendError(job)) => job,
        },
        None => job,
    };
    registry.0 += 1;
    let generation = registry.0;
    let (sender, receiver) = mpsc::channel();
    sender.send(job).map_err(|_| anyhow!("database writer channel closed"))?;
    let thread_root = root.clone();
    std::thread::Builder::new()
        .name("db-writer".into())
        .spawn(move || run(thread_root, generation, receiver))
        .context("could not start the database writer")?;
    registry.1.insert(root, Writer { sender, generation });
    Ok(())
}

/// Run `job` on the database writer of `root` and wait for its result. The job gets the writer's
/// connection; it opens its own transaction when several statements must commit together.
pub fn write<T: Send + 'static>(
    root: &Path,
    job: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    if ON_WRITER.with(|flag| flag.get()) {
        let mut conn = crate::agent_service::open_db(root)?;
        return job(&mut conn);
    }
    let (reply, result) = mpsc::sync_channel(1);
    submit(
        root,
        Box::new(move |conn| {
            let _ = reply.send(job(conn));
        }),
    )?;
    result
        .recv()
        .map_err(|_| anyhow!("database writer stopped before finishing the write"))?
}

/// `write` for async code: the caller's task waits without holding a runtime thread.
pub async fn write_async<T: Send + 'static>(
    root: &Path,
    job: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    if ON_WRITER.with(|flag| flag.get()) {
        return write(root, job);
    }
    let (reply, result) = tokio::sync::oneshot::channel();
    submit(
        root,
        Box::new(move |conn| {
            let _ = reply.send(job(conn));
        }),
    )?;
    result
        .await
        .map_err(|_| anyhow!("database writer stopped before finishing the write"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("db_writer_{}", crate::agent_service::uuid_like()));
        std::fs::create_dir_all(&root).unwrap();
        let conn = crate::agent_service::open_db(&root).unwrap();
        conn.execute(
            "INSERT INTO agent_tasks(id,prompt,model,status,created_at,updated_at) VALUES ('task','p','m','running',0,0)",
            [],
        )
        .unwrap();
        root
    }

    fn append(root: &Path, step: usize) -> Result<i64> {
        write(root, move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let seq = crate::agent_service::append_event_tx(&tx, "task", "test/event", &json!({ "step": step }), None)?;
            tx.commit()?;
            Ok(seq)
        })
    }

    #[test]
    fn writes_from_many_threads_run_one_after_another_and_return_their_results() {
        let root = root();
        let threads: Vec<_> = (0..8)
            .map(|writer| {
                let root = root.clone();
                std::thread::spawn(move || (0..50).map(|step| append(&root, writer * 100 + step).unwrap()).collect::<Vec<_>>())
            })
            .collect();
        let mut seqs: Vec<i64> = threads.into_iter().flat_map(|thread| thread.join().unwrap()).collect();
        seqs.sort();
        assert_eq!(seqs, (0..400).collect::<Vec<_>>(), "every write got its own sequence number");
        let conn = crate::agent_service::open_db(&root).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM agent_task_events", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 400);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_or_panicking_job_rolls_back_and_the_writer_goes_on() {
        let root = root();
        let failed = write(&root, |conn| -> Result<()> {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            conn.execute("UPDATE agent_tasks SET status='half' WHERE id='task'", [])?;
            Err(anyhow!("stopped half-way"))
        });
        assert!(failed.is_err());
        let panicked = std::thread::spawn({
            let root = root.clone();
            move || write(&root, |_conn| -> Result<()> { panic!("boom") })
        })
        .join();
        assert!(panicked.is_err() || panicked.unwrap().is_err());
        // The open transaction was rolled back, and the writer still takes work.
        assert_eq!(append(&root, 1).unwrap(), 0);
        let conn = crate::agent_service::open_db(&root).unwrap();
        let status: String = conn.query_row("SELECT status FROM agent_tasks WHERE id='task'", [], |r| r.get(0)).unwrap();
        assert_eq!(status, "running");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_write_requested_from_inside_a_write_does_not_wait_for_itself() {
        let root = root();
        let inner_root = root.clone();
        let seq = write(&root, move |_conn| append(&inner_root, 7)).unwrap();
        assert_eq!(seq, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn async_callers_queue_with_the_others() {
        let root = root();
        let tasks: Vec<_> = (0..20)
            .map(|step| {
                let root = root.clone();
                tokio::spawn(async move {
                    write_async(&root, move |conn| {
                        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                        let seq = crate::agent_service::append_event_tx(&tx, "task", "test/async", &json!({ "step": step }), None)?;
                        tx.commit()?;
                        Ok(seq)
                    })
                    .await
                })
            })
            .collect();
        let mut seqs = Vec::new();
        for task in tasks {
            seqs.push(task.await.unwrap().unwrap());
        }
        seqs.sort();
        assert_eq!(seqs, (0..20).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(root);
    }
}
