//! Integration tests for the database driver, against a real PostgreSQL server.
//!
//! They run only when `DATABASE_URL` is set, so `cargo test` stays green on a
//! machine with no database. Start one with:
//!
//! ```text
//! docker run -d --name rustlavel-pg -e POSTGRES_PASSWORD=secret \
//!   -e POSTGRES_USER=rustlavel -e POSTGRES_DB=rustlavel_test \
//!   -p 55432:5432 postgres:16
//! export DATABASE_URL=postgres://rustlavel:secret@127.0.0.1:55432/rustlavel_test
//! ```
//!
//! The one that matters most is
//! `many_workers_racing_for_the_same_jobs_each_run_exactly_once`: everything
//! else in this crate can be checked in memory, but "two workers never get the
//! same row" is a claim about PostgreSQL's locking, and only PostgreSQL can
//! settle it.

use rustlavel_db::Database;
use rustlavel_queue::prelude::*;
use rustlavel_queue::worker::Outcome;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Skip the test when no database is configured, saying so out loud.
macro_rules! database {
    () => {
        match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => match Database::connect(&url).await {
                Ok(db) => db,
                Err(e) => panic!("DATABASE_URL is set but connecting failed: {e}"),
            },
            _ => {
                eprintln!("skipped: set DATABASE_URL to run the PostgreSQL integration tests");
                return;
            }
        }
    };
}

/// Each test owns uniquely named tables, so they can run concurrently against
/// one database without stepping on each other.
async fn fresh_queue(db: &Database, name: &str) -> DatabaseQueue {
    let queue =
        DatabaseQueue::with_tables(db.clone(), &format!("q_{name}"), &format!("q_{name}_failed"))
            .expect("the test table names are valid identifiers");

    queue.drop_tables().await.expect("dropping any leftovers");
    queue.migrate().await.expect("creating the queue tables");
    queue
}

// --- Fixture jobs -----------------------------------------------------------
//
// Every run is addressed by a fresh id, so two tests running at once never see
// each other's tallies.

type Tally = HashMap<(String, i64), u32>;

fn tallies() -> &'static Mutex<Tally> {
    static TALLIES: OnceLock<Mutex<Tally>> = OnceLock::new();
    TALLIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn run_id(name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{name}-{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

/// How many times each index of a run was handled.
fn counts_for(run: &str) -> HashMap<i64, u32> {
    tallies()
        .lock()
        .expect("tallies poisoned")
        .iter()
        .filter(|((r, _), _)| r == run)
        .map(|((_, index), count)| (*index, *count))
        .collect()
}

/// Records that it ran, and yields so several workers really do interleave.
struct RecordJob {
    run: String,
    index: i64,
}

impl Job for RecordJob {
    const NAME: &'static str = "record";

    fn payload(&self) -> Json {
        Json::object([
            ("run", Json::from(self.run.as_str())),
            ("index", Json::from(self.index)),
        ])
    }

    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(RecordJob {
            run: payload
                .get("run")
                .and_then(Json::as_str)
                .ok_or_else(|| Error::msg("a record job needs a `run`"))?
                .to_string(),
            index: payload.get("index").and_then(Json::as_i64).unwrap_or(-1),
        })
    }

    fn handle(&self) -> impl Future<Output = Result<()>> + Send {
        let (run, index) = (self.run.clone(), self.index);
        async move {
            // Long enough that a worker is holding this job while others are
            // reserving, which is the situation the test exists to create.
            tokio::time::sleep(Duration::from_millis(2)).await;
            *tallies().lock().expect("tallies poisoned").entry((run, index)).or_insert(0) += 1;
            Ok(())
        }
    }
}

/// Always fails, so retries and the dead-letter table can be observed.
struct BoomJob {
    run: String,
    tries: u32,
}

impl Job for BoomJob {
    const NAME: &'static str = "boom";

    fn payload(&self) -> Json {
        Json::object([
            ("run", Json::from(self.run.as_str())),
            ("tries", Json::from(self.tries)),
        ])
    }

    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(BoomJob {
            run: payload.get("run").and_then(Json::as_str).unwrap_or_default().to_string(),
            tries: payload.get("tries").and_then(Json::as_i64).unwrap_or(3) as u32,
        })
    }

    fn handle(&self) -> impl Future<Output = Result<()>> + Send {
        let run = self.run.clone();
        async move {
            let mut tallies = tallies().lock().expect("tallies poisoned");
            let attempt = tallies.entry((run, 0)).or_insert(0);
            *attempt += 1;
            Err(Error::msg(format!("attempt {attempt} exploded")))
        }
    }

    fn tries(&self) -> u32 {
        self.tries
    }

    fn retry_after(&self) -> Duration {
        Duration::ZERO
    }
}

/// Panics, to prove a worker survives one against a real database too.
struct PanicJob;

impl Job for PanicJob {
    const NAME: &'static str = "panic";

    fn payload(&self) -> Json {
        Json::Null
    }

    fn from_payload(_payload: &Json) -> Result<Self> {
        Ok(PanicJob)
    }

    async fn handle(&self) -> Result<()> {
        tokio::task::yield_now().await;
        panic!("this job panicked against a real database");
    }

    fn tries(&self) -> u32 {
        1
    }
}

fn registry() -> Arc<JobRegistry> {
    let mut registry = JobRegistry::new();
    registry.register::<RecordJob>();
    registry.register::<BoomJob>();
    registry.register::<PanicJob>();
    Arc::new(registry)
}

// --- Tests ------------------------------------------------------------------

#[tokio::test]
async fn the_migration_creates_tables_a_job_can_round_trip_through() {
    let db = database!();
    let queue = fresh_queue(&db, "roundtrip").await;
    let run = run_id("roundtrip");

    let id = queue.dispatch(&RecordJob { run: run.clone(), index: 7 }).await.unwrap();
    assert!(id.parse::<i64>().is_ok(), "the database driver hands out row ids");
    assert_eq!(queue.size("default").await.unwrap(), 1);

    let reserved = queue.pop("default").await.unwrap().expect("a job is waiting");
    assert_eq!(reserved.id, id);
    assert_eq!(reserved.job.name, "record");
    assert_eq!(reserved.attempts, 1);
    assert_eq!(reserved.job.payload.get("index").and_then(Json::as_i64), Some(7));
    assert_eq!(reserved.job.max_tries, 3);

    // Reserved, so no second worker may have it.
    assert!(queue.pop("default").await.unwrap().is_none());

    queue.delete(&reserved).await.unwrap();
    assert_eq!(queue.size("default").await.unwrap(), 0);

    queue.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_delayed_job_is_stored_but_stays_invisible_until_it_is_due() {
    let db = database!();
    let queue = fresh_queue(&db, "delayed").await;
    let run = run_id("delayed");

    queue
        .dispatch_later(Duration::from_secs(3600), &RecordJob { run: run.clone(), index: 1 })
        .await
        .unwrap();
    queue.dispatch(&RecordJob { run: run.clone(), index: 2 }).await.unwrap();

    assert_eq!(queue.size("default").await.unwrap(), 2, "both are stored");

    // Only the second one is available, and the delayed one does not block it.
    let reserved = queue.pop("default").await.unwrap().expect("the ready job");
    assert_eq!(reserved.job.payload.get("index").and_then(Json::as_i64), Some(2));
    queue.delete(&reserved).await.unwrap();

    assert!(queue.pop("default").await.unwrap().is_none(), "the delayed job is not yet due");
    assert_eq!(queue.size("default").await.unwrap(), 1);

    queue.drop_tables().await.unwrap();
}

#[tokio::test]
async fn queues_in_one_table_do_not_see_each_other() {
    let db = database!();
    let queue = fresh_queue(&db, "queues").await;
    let run = run_id("queues");

    queue.dispatch(&RecordJob { run: run.clone(), index: 1 }).await.unwrap();
    queue
        .dispatch_on("emails", &RecordJob { run: run.clone(), index: 2 })
        .await
        .unwrap();

    assert_eq!(queue.size("default").await.unwrap(), 1);
    assert_eq!(queue.size("emails").await.unwrap(), 1);

    let reserved = queue.pop("emails").await.unwrap().expect("the emails job");
    assert_eq!(reserved.job.queue, "emails");
    assert_eq!(reserved.job.payload.get("index").and_then(Json::as_i64), Some(2));

    queue.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_failing_job_is_retried_with_backoff_and_then_moved_to_failed_jobs() {
    let db = database!();
    let queue: Arc<dyn Queue> = Arc::new(fresh_queue(&db, "failing").await);
    let run = run_id("failing");

    queue.dispatch(&BoomJob { run: run.clone(), tries: 3 }).await.unwrap();

    let worker = Worker::new(Arc::clone(&queue), registry());

    assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Released));
    assert_eq!(queue.size("default").await.unwrap(), 1, "a released job is back on the queue");

    assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Released));
    assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Failed));

    assert_eq!(queue.size("default").await.unwrap(), 0, "and gone once it is buried");
    assert_eq!(counts_for(&run).get(&0), Some(&3), "it ran once per try");

    let failed = queue.failed_jobs().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].name, "boom");
    assert_eq!(failed[0].attempts, 3);
    assert!(failed[0].error.contains("attempt 3 exploded"), "{}", failed[0].error);
    assert_eq!(failed[0].payload.get("run").and_then(Json::as_str), Some(run.as_str()));
    assert!(failed[0].failed_at > 0, "the time of death is recorded");

    // The dead-letter row survives a fresh handle on the same tables.
    let reopened = DatabaseQueue::with_tables(db.clone(), "q_failing", "q_failing_failed").unwrap();
    assert_eq!(reopened.failed_jobs().await.unwrap().len(), 1);

    reopened.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_panicking_job_is_buried_rather_than_killing_the_worker() {
    let db = database!();
    let queue: Arc<dyn Queue> = Arc::new(fresh_queue(&db, "panicking").await);
    let run = run_id("panicking");

    queue.dispatch(&PanicJob).await.unwrap();
    queue.dispatch(&RecordJob { run: run.clone(), index: 1 }).await.unwrap();

    let worker = Worker::new(Arc::clone(&queue), registry());

    assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Failed));
    assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Processed));

    let failed = queue.failed_jobs().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0].error.contains("this job panicked against a real database"),
        "{}",
        failed[0].error
    );
    assert_eq!(counts_for(&run).get(&1), Some(&1), "the job behind it still ran");

    DatabaseQueue::with_tables(db, "q_panicking", "q_panicking_failed")
        .unwrap()
        .drop_tables()
        .await
        .unwrap();
}

/// Only one of many simultaneous reservations may win.
///
/// This is the narrowest possible test of `for update skip locked`: one row,
/// twelve workers reaching for it at the same instant. Without the lock two of
/// them read the same row and the job runs twice; with `for update` but no
/// `skip locked` they would all still be correct but queued behind each other.
#[tokio::test]
async fn only_one_reservation_can_win_a_single_row() {
    let db = database!();
    let queue: Arc<DatabaseQueue> = Arc::new(fresh_queue(&db, "single").await);
    let run = run_id("single");

    queue.dispatch(&RecordJob { run, index: 1 }).await.unwrap();

    let attempts: Vec<_> = (0..12)
        .map(|_| {
            let queue = Arc::clone(&queue);
            tokio::spawn(async move { queue.pop("default").await })
        })
        .collect();

    let mut winners = 0;
    for attempt in attempts {
        if attempt.await.unwrap().unwrap().is_some() {
            winners += 1;
        }
    }

    assert_eq!(winners, 1, "exactly one worker may reserve a given row");

    queue.drop_tables().await.unwrap();
}

/// The test this driver exists to pass.
///
/// Five workers, sixty jobs, one table. Every job must be handled exactly once:
/// not twice because two workers reserved the same row, and not zero times
/// because a worker skipped past one.
#[tokio::test]
async fn many_workers_racing_for_the_same_jobs_each_run_exactly_once() {
    let db = database!();
    let queue: Arc<dyn Queue> = Arc::new(fresh_queue(&db, "concurrent").await);
    let run = run_id("concurrent");

    const JOBS: i64 = 60;
    const WORKERS: usize = 5;

    for index in 0..JOBS {
        queue.dispatch(&RecordJob { run: run.clone(), index }).await.unwrap();
    }
    assert_eq!(queue.size("default").await.unwrap(), JOBS as u64);

    let worker = Worker::new(Arc::clone(&queue), registry())
        .poll_interval(Duration::from_millis(10));

    // Stop the pool once the table is empty — which, because a reserved row is
    // still a row, cannot happen until every job has been deleted or buried.
    let shutdown = Shutdown::new();
    let signal = shutdown.clone();
    let watcher = Arc::clone(&queue);
    tokio::spawn(async move {
        for _ in 0..600 {
            if watcher.size("default").await.unwrap_or(1) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        signal.signal();
    });

    let stats = run_pool(worker, WORKERS, shutdown).await.unwrap();

    assert_eq!(stats.processed, JOBS as u64, "every job succeeded once");
    assert_eq!(stats.released, 0, "and none had to be retried");
    assert_eq!(stats.failed, 0);

    let counts = counts_for(&run);
    assert_eq!(counts.len(), JOBS as usize, "every job ran");
    for index in 0..JOBS {
        assert_eq!(
            counts.get(&index),
            Some(&1),
            "job {index} ran {:?} time(s), not exactly once",
            counts.get(&index)
        );
    }

    assert_eq!(queue.size("default").await.unwrap(), 0);
    assert!(queue.failed_jobs().await.unwrap().is_empty());

    DatabaseQueue::with_tables(db, "q_concurrent", "q_concurrent_failed")
        .unwrap()
        .drop_tables()
        .await
        .unwrap();
}

/// A worker killed mid-job leaves `reserved_at` set forever. Once the job's own
/// `retry_after` has passed with nothing to show for it, the job goes back.
#[tokio::test]
async fn a_job_orphaned_by_a_dead_worker_is_reclaimed() {
    let db = database!();
    let queue = fresh_queue(&db, "orphan").await;
    let run = run_id("orphan");

    queue
        .push(
            RecordJob { run: run.clone(), index: 1 }
                .to_queued()
                .with_retry_after(Duration::from_secs(60)),
        )
        .await
        .unwrap();

    let reserved = queue.pop("default").await.unwrap().expect("a job is waiting");
    assert_eq!(reserved.attempts, 1);

    // Nothing deletes or releases it, and nothing will: the worker is gone.
    // Its reservation is not stale yet, so a sweep now must leave it alone —
    // reclaiming a job someone is still working on would run it twice.
    assert_eq!(queue.reclaim_expired("default").await.unwrap(), 0);
    assert!(queue.pop("default").await.unwrap().is_none());

    // Age the reservation past the job's `retry_after`, which is what waiting
    // an hour would do.
    queue
        .database()
        .execute(
            "update q_orphan set reserved_at = reserved_at - 3600 where id = $1",
            &[rustlavel_db::Value::from(reserved.id.parse::<i64>().unwrap())],
        )
        .await
        .unwrap();

    assert_eq!(queue.reclaim_expired("default").await.unwrap(), 1);

    let reclaimed = queue.pop("default").await.unwrap().expect("the orphan came back");
    assert_eq!(reclaimed.id, reserved.id, "the same job, not a copy");
    assert_eq!(reclaimed.attempts, 2, "and it remembers how many chances it has had");

    // `pop` runs the same sweep by itself once the queue looks empty, so a
    // worker needs no separate reaper.
    queue
        .database()
        .execute(
            "update q_orphan set reserved_at = reserved_at - 3600 where id = $1",
            &[rustlavel_db::Value::from(reclaimed.id.parse::<i64>().unwrap())],
        )
        .await
        .unwrap();

    let third = queue.pop("default").await.unwrap().expect("pop reclaimed it on its own");
    assert_eq!(third.attempts, 3);

    queue.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_released_job_waits_out_its_delay_before_a_worker_sees_it_again() {
    let db = database!();
    let queue = fresh_queue(&db, "released").await;
    let run = run_id("released");

    queue.dispatch(&RecordJob { run, index: 1 }).await.unwrap();

    let reserved = queue.pop("default").await.unwrap().unwrap();
    queue.release(&reserved, Duration::from_secs(3600)).await.unwrap();

    assert_eq!(queue.size("default").await.unwrap(), 1, "it is back on the queue");
    assert!(queue.pop("default").await.unwrap().is_none(), "but not for another hour");

    queue.release(&reserved, Duration::ZERO).await.unwrap();
    let again = queue.pop("default").await.unwrap().expect("released with no delay");
    assert_eq!(again.attempts, 2);

    queue.drop_tables().await.unwrap();
}

#[tokio::test]
async fn clearing_a_queue_leaves_the_others_alone() {
    let db = database!();
    let queue = fresh_queue(&db, "clearing").await;
    let run = run_id("clearing");

    for index in 0..3 {
        queue.dispatch(&RecordJob { run: run.clone(), index }).await.unwrap();
    }
    queue.dispatch_on("keep", &RecordJob { run: run.clone(), index: 9 }).await.unwrap();

    assert_eq!(queue.clear("default").await.unwrap(), 3);
    assert_eq!(queue.size("default").await.unwrap(), 0);
    assert_eq!(queue.size("keep").await.unwrap(), 1);

    queue.drop_tables().await.unwrap();
}

#[tokio::test]
async fn a_payload_can_never_become_sql() {
    let db = database!();
    let queue = fresh_queue(&db, "injection").await;

    let hostile = "'; drop table q_injection; --";
    queue
        .push(QueuedJob::new(hostile, Json::object([("note", Json::from(hostile))])))
        .await
        .unwrap();

    let reserved = queue.pop("default").await.unwrap().expect("the table still exists");
    assert_eq!(reserved.job.name, hostile);
    assert_eq!(reserved.job.payload.get("note").and_then(Json::as_str), Some(hostile));

    queue.fail(&reserved, hostile).await.unwrap();
    assert_eq!(queue.failed_jobs().await.unwrap()[0].error, hostile);

    queue.drop_tables().await.unwrap();
}
