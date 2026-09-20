//! The queue against SQLite.
//!
//! This file exists because of a bug it would have caught: every statement in
//! `DatabaseQueue` used to write `$1` and `for update skip locked` literally,
//! which is PostgreSQL's spelling and nobody else's. The queue was the one
//! package in the workspace that could not run on the other databases the
//! framework claims to support, and nothing said so — the only integration
//! test was against PostgreSQL, so the assumption tested itself.
//!
//! SQLite needs no container, so this runs on every machine, every time.

#![cfg(feature = "sqlite")]

use rustlavel_db::Database;
use rustlavel_queue::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Every test tallies under its own run id. These tests run concurrently in
/// one binary, so a single shared counter would have each test zeroing the
/// others' work — which it did, and which is why this is a map.
fn tallies() -> &'static Mutex<HashMap<String, u32>> {
    static TALLIES: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    TALLIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn run_id(name: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{name}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn ran(run: &str) -> u32 {
    tallies().lock().expect("tallies poisoned").get(run).copied().unwrap_or(0)
}

struct Count {
    run: String,
}

impl Job for Count {
    const NAME: &'static str = "count";

    fn payload(&self) -> Json {
        Json::object([("run", Json::from(self.run.as_str()))])
    }

    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(Count {
            run: payload
                .get("run")
                .and_then(Json::as_str)
                .ok_or_else(|| Error::msg("a count job needs a `run`"))?
                .to_string(),
        })
    }

    fn handle(&self) -> impl Future<Output = Result<()>> + Send {
        let run = self.run.clone();
        async move {
            *tallies().lock().expect("tallies poisoned").entry(run).or_insert(0) += 1;
            Ok(())
        }
    }
}

async fn queue() -> DatabaseQueue {
    let db = Database::connect("sqlite://:memory:").await.expect("sqlite opens");
    let queue = DatabaseQueue::new(db);
    queue.migrate().await.expect("the queue tables are created on SQLite");
    queue
}

fn registry() -> Arc<JobRegistry> {
    let mut registry = JobRegistry::new();
    registry.register::<Count>();
    Arc::new(registry)
}

/// The whole round trip on a database whose placeholders are `?`, whose
/// `limit` is a `limit`, and which has no `for update skip locked` at all.
#[tokio::test]
async fn a_job_is_enqueued_reserved_and_completed() {
    let queue = queue().await;
    let run = run_id("roundtrip");

    queue.push(Count { run: run.clone() }.to_queued()).await.expect("pushed");
    assert_eq!(queue.size("default").await.unwrap(), 1);

    let worker = Worker::new(Arc::new(queue.clone()), registry());
    worker.run_once().await.expect("the worker reserved and ran the job");

    assert_eq!(ran(&run), 1);
    assert_eq!(queue.size("default").await.unwrap(), 0, "the job was not removed");
}

/// **The claim the whole design rests on**, now on a second database: a job is
/// handed to exactly one worker. On PostgreSQL that is `for update skip
/// locked`; on SQLite there are no row locks at all, and the guarantee comes
/// from `begin immediate` taking the write lock up front so the second worker
/// waits rather than reading the same row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_workers_never_run_one_job_twice() {
    let queue = queue().await;
    let run = run_id("race");

    for _ in 0..12 {
        queue.push(Count { run: run.clone() }.to_queued()).await.unwrap();
    }

    let mut racing = Vec::new();
    for _ in 0..4 {
        let queue = queue.clone();
        racing.push(tokio::spawn(async move {
            let worker = Worker::new(Arc::new(queue), registry());
            for _ in 0..12 {
                let _ = worker.run_once().await;
            }
        }));
    }
    for task in racing {
        task.await.unwrap();
    }

    assert_eq!(ran(&run), 12, "a job ran twice, or not at all");
    assert_eq!(queue.size("default").await.unwrap(), 0);
}

#[tokio::test]
async fn a_delayed_job_is_not_reserved_before_its_time() {
    let queue = queue().await;
    let run = run_id("delayed");

    queue
        .push(Count { run: run.clone() }.to_queued().with_delay(Duration::from_secs(3600)))
        .await
        .expect("pushed");

    let worker = Worker::new(Arc::new(queue.clone()), registry());
    let _ = worker.run_once().await;

    assert_eq!(ran(&run), 0, "a job due in an hour ran now");
    assert_eq!(queue.size("default").await.unwrap(), 1);
}
