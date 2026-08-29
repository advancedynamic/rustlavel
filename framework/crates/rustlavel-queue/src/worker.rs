//! The worker loop: reserve, run, and then delete, retry, or bury.
//!
//! What `queue:work` runs. A worker is deliberately dull — it owns no state
//! beyond its options, so N of them against one queue is the same code run N
//! times, and a worker that dies mid-job leaves the job recoverable rather than
//! lost.

use crate::job::{JobRegistry, ReservedJob};
use crate::queue::Queue;
use rustlavel_core::events::{self, Event};
use rustlavel_core::{Json, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// How long a worker waits before asking an empty queue again.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The ceiling on exponential backoff. Without one, a job on its fifteenth
/// attempt would be scheduled a year out and never seen again.
pub const DEFAULT_BACKOFF_CAP: Duration = Duration::from_secs(3600);

/// What happened to one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It succeeded and was deleted.
    Processed,
    /// It failed and went back on the queue for another attempt.
    Released,
    /// It failed for the last time and went to the dead-letter store.
    Failed,
}

/// A tally of what a worker did, returned when it stops.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerStats {
    pub processed: u64,
    pub released: u64,
    pub failed: u64,
}

impl WorkerStats {
    /// Every job attempt this worker made.
    pub fn attempts(&self) -> u64 {
        self.processed + self.released + self.failed
    }

    fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Processed => self.processed += 1,
            Outcome::Released => self.released += 1,
            Outcome::Failed => self.failed += 1,
        }
    }

    fn merge(&mut self, other: WorkerStats) {
        self.processed += other.processed;
        self.released += other.released;
        self.failed += other.failed;
    }
}

/// A shutdown signal shared by every worker in a pool.
///
/// Cloning shares one signal, so `Ctrl+C` handled once stops the whole pool.
#[derive(Clone)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
    sender: Arc<watch::Sender<bool>>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Shutdown::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Shutdown { flag: Arc::new(AtomicBool::new(false)), sender: Arc::new(sender) }
    }

    /// Ask every worker to stop after the job it currently holds.
    pub fn signal(&self) {
        // The flag is set *before* the notification, so a worker that
        // subscribes and then reads the flag can never miss both.
        self.flag.store(true, Ordering::SeqCst);
        let _ = self.sender.send(true);
    }

    pub fn is_signalled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Stop the pool when the process is interrupted — what `queue:work` wants.
    pub fn on_ctrl_c(&self) {
        let signal = self.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                rustlavel_core::info!("shutting down: finishing the job in hand");
                signal.signal();
            }
        });
    }

    /// Wait up to `duration`. Returns true when shutdown arrived first, so a
    /// worker on an idle queue stops promptly instead of sitting out its poll.
    pub async fn wait_for(&self, duration: Duration) -> bool {
        let mut receiver = self.sender.subscribe();
        if self.is_signalled() {
            return true;
        }

        tokio::select! {
            _ = receiver.changed() => true,
            _ = tokio::time::sleep(duration) => self.is_signalled(),
        }
    }
}

/// How a worker behaves.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// The queue to work. One worker works one queue.
    pub queue: String,
    /// How long to wait after finding nothing.
    pub poll_interval: Duration,
    /// Stop after this many attempts. `None` means run until shutdown; `Some(1)`
    /// is `queue:work --once`.
    pub max_jobs: Option<u64>,
    /// The ceiling on retry backoff.
    pub backoff_cap: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        WorkerOptions {
            queue: crate::job::DEFAULT_QUEUE.to_string(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_jobs: None,
            backoff_cap: DEFAULT_BACKOFF_CAP,
        }
    }
}

/// One worker. Cloning is cheap and gives a second worker on the same queue.
#[derive(Clone)]
pub struct Worker {
    queue: Arc<dyn Queue>,
    registry: Arc<JobRegistry>,
    options: WorkerOptions,
}

impl Worker {
    pub fn new(queue: Arc<dyn Queue>, registry: Arc<JobRegistry>) -> Self {
        // The same hook rustlavel-http installs for handlers: it records where a
        // panic happened and keeps the default backtrace off stderr for panics
        // we are about to report ourselves.
        rustlavel_http::panic::install_hook();

        Worker { queue, registry, options: WorkerOptions::default() }
    }

    pub fn on_queue(mut self, queue: impl Into<String>) -> Self {
        self.options.queue = queue.into();
        self
    }

    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.options.poll_interval = interval;
        self
    }

    /// Stop after `count` attempts, whatever the shutdown signal says.
    pub fn max_jobs(mut self, count: u64) -> Self {
        self.options.max_jobs = Some(count);
        self
    }

    pub fn backoff_cap(mut self, cap: Duration) -> Self {
        self.options.backoff_cap = cap;
        self
    }

    pub fn options(&self) -> &WorkerOptions {
        &self.options
    }

    /// Work until shutdown is signalled or `max_jobs` is reached.
    ///
    /// The shutdown check is at the top of the loop and nowhere else. That is
    /// the entire graceful-shutdown guarantee: a job already reserved is always
    /// run to completion, because there is no point at which the loop can
    /// abandon one.
    pub async fn run(&self, shutdown: Shutdown) -> Result<WorkerStats> {
        let mut stats = WorkerStats::default();

        loop {
            if shutdown.is_signalled() {
                break;
            }
            if self.options.max_jobs.is_some_and(|max| stats.attempts() >= max) {
                break;
            }

            if let Some(outcome) = self.run_once().await? {
                stats.record(outcome);
            } else if shutdown.wait_for(self.options.poll_interval).await {
                break;
            }
        }

        Ok(stats)
    }

    /// Reserve and run at most one job. `None` when the queue had nothing ready.
    pub async fn run_once(&self) -> Result<Option<Outcome>> {
        let Some(reserved) = self.queue.pop(&self.options.queue).await? else {
            return Ok(None);
        };
        Ok(Some(self.process(reserved).await?))
    }

    /// Run one reserved job and settle its fate.
    pub async fn process(&self, reserved: ReservedJob) -> Result<Outcome> {
        let started = Instant::now();
        let result = self.run_handler(&reserved).await;
        let elapsed = started.elapsed();

        match result {
            Ok(()) => {
                self.queue.delete(&reserved).await?;
                record(
                    "queue.processed",
                    self.queue.driver(),
                    &reserved,
                    elapsed,
                    |event| event,
                );
                Ok(Outcome::Processed)
            }
            Err(error) => {
                let dead = reserved.is_last_attempt();

                if dead {
                    self.queue.fail(&reserved, &error).await?;
                    rustlavel_core::error!(
                        "job `{}` failed after {} attempt(s) and was moved to failed_jobs: {error}",
                        reserved.job.name,
                        reserved.attempts
                    );
                } else {
                    let delay = backoff(
                        reserved.job.retry_after,
                        reserved.attempts,
                        self.options.backoff_cap,
                    );
                    self.queue.release(&reserved, delay).await?;
                    rustlavel_core::warn!(
                        "job `{}` failed on attempt {} of {} and will retry in {}s: {error}",
                        reserved.job.name,
                        reserved.attempts,
                        reserved.job.max_tries,
                        delay.as_secs()
                    );
                }

                // Reported on every failed attempt, not only the last, so
                // Telescope can show a job that is thrashing before it dies.
                // `dead_lettered` is the flag that separates the two.
                record("queue.failed", self.queue.driver(), &reserved, elapsed, |event| {
                    event.with("exception", error.as_str()).with("dead_lettered", dead)
                });

                Ok(if dead { Outcome::Failed } else { Outcome::Released })
            }
        }
    }

    /// Run the handler, turning every way it can go wrong into one `Err`.
    ///
    /// A panicking job must not take the worker with it: one bad payload would
    /// otherwise stop every other job on the queue. Caught with the same
    /// technique rustlavel-http uses for a panicking handler, so a panic across
    /// an await point is caught too, and treated as an ordinary failure — it
    /// retries and it dead-letters, exactly like a returned error.
    async fn run_handler(&self, reserved: &ReservedJob) -> std::result::Result<(), String> {
        let future = self.registry.run(&reserved.job.name, reserved.job.payload.clone());

        match rustlavel_http::panic::catch(future).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.to_string()),
            Err(payload) => {
                let location = rustlavel_http::panic::take_location()
                    .map(|at| format!(" at {}:{}:{}", at.file, at.line, at.column))
                    .unwrap_or_default();
                Err(format!("the job panicked{location}: {payload}"))
            }
        }
    }
}

/// Run `count` copies of a worker concurrently, all watching one shutdown
/// signal, and return their combined tally once every one has stopped.
///
/// This is `queue:work --workers=N`. They share a queue, not any state: the
/// only thing keeping two of them off the same job is the driver's reservation.
pub async fn run_pool(worker: Worker, count: usize, shutdown: Shutdown) -> Result<WorkerStats> {
    let mut handles = Vec::with_capacity(count.max(1));

    for _ in 0..count.max(1) {
        let worker = worker.clone();
        let shutdown = shutdown.clone();
        handles.push(tokio::spawn(async move { worker.run(shutdown).await }));
    }

    let mut total = WorkerStats::default();
    for handle in handles {
        let stats = handle
            .await
            .map_err(|e| rustlavel_core::Error::msg(format!("a worker task did not finish: {e}")))??;
        total.merge(stats);
    }

    Ok(total)
}

/// How long to wait before the next attempt.
///
/// Exponential: the job's own `retry_after`, doubled once per attempt already
/// made, capped. A dependency that is down stays down for a while, and hammering
/// it every minute helps nobody.
pub fn backoff(retry_after: Duration, attempts: u32, cap: Duration) -> Duration {
    // Shifting by 32 or more is undefined, and by then the cap has won anyway.
    let doublings = attempts.saturating_sub(1).min(20);
    let factor = 1u32 << doublings;

    retry_after.saturating_mul(factor).min(cap)
}

/// Dispatch a worker event, if anyone is listening.
fn record(
    kind: &'static str,
    driver: &'static str,
    reserved: &ReservedJob,
    elapsed: Duration,
    extend: impl FnOnce(Event) -> Event,
) {
    if !events::has_subscribers() {
        return;
    }

    let event = Event::new(kind)
        .took(elapsed)
        .with("job", reserved.job.name.as_str())
        .with("queue", reserved.job.queue.as_str())
        .with("id", reserved.id.as_str())
        .with("driver", driver)
        .with("attempt", reserved.attempts)
        .with("max_tries", reserved.job.max_tries);

    extend(event).dispatch();
}

/// A convenience for a dashboard: the worker events as JSON.
pub fn outcome_to_json(outcome: Outcome) -> Json {
    Json::from(match outcome {
        Outcome::Processed => "processed",
        Outcome::Released => "released",
        Outcome::Failed => "failed",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, QueuedJob};
    use crate::memory::MemoryQueue;
    use crate::queue::QueueExt;
    use crate::tests_support::{
        CountingJob, FailingJob, FlakyJob, PanickingJob, SlowJob, counter, registry,
    };

    /// A worker over a fresh in-memory queue, with every fixture registered.
    fn worker() -> (Arc<MemoryQueue>, Worker) {
        let queue = Arc::new(MemoryQueue::new());
        let worker = Worker::new(queue.clone(), Arc::new(registry()))
            .poll_interval(Duration::from_millis(5));
        (queue, worker)
    }

    #[tokio::test]
    async fn a_worker_runs_a_job_and_removes_it() {
        let (queue, worker) = worker();
        let tally = counter();
        queue.dispatch(&CountingJob::new(tally, 7)).await.unwrap();

        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Processed));

        assert_eq!(tally.runs(), 1);
        assert_eq!(tally.total(), 7);
        assert_eq!(queue.size("default").await.unwrap(), 0);
        assert!(queue.failed_jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_queue_gives_the_worker_nothing_to_do() {
        let (_queue, worker) = worker();
        assert_eq!(worker.run_once().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_worker_only_takes_from_the_queue_it_was_pointed_at() {
        let (queue, worker) = worker();
        let tally = counter();
        queue
            .push_on("emails", CountingJob::new(tally, 1).to_queued())
            .await
            .unwrap();

        assert_eq!(worker.run_once().await.unwrap(), None, "nothing on `default`");

        let emails = worker.clone().on_queue("emails");
        assert_eq!(emails.run_once().await.unwrap(), Some(Outcome::Processed));
        assert_eq!(tally.runs(), 1);
    }

    #[tokio::test]
    async fn a_failing_job_is_retried_and_then_dead_lettered() {
        let (queue, worker) = worker();
        let tally = counter();
        queue.dispatch(&FailingJob::new(tally, 3)).await.unwrap();

        // Three attempts: two releases back onto the queue, then the grave.
        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Released));
        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Released));
        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Failed));
        assert_eq!(worker.run_once().await.unwrap(), None, "nothing is left to run");

        assert_eq!(tally.runs(), 3, "it ran exactly as many times as it had tries");
        assert_eq!(queue.size("default").await.unwrap(), 0);

        let failed = queue.failed_jobs().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, "failing");
        assert_eq!(failed[0].attempts, 3);
        assert!(failed[0].error.contains("attempt 3 went wrong"), "{}", failed[0].error);
    }

    #[tokio::test]
    async fn a_job_that_recovers_never_reaches_the_dead_letter_store() {
        let (queue, worker) = worker();
        let tally = counter();
        queue.dispatch(&FlakyJob::new(tally, 3, 5)).await.unwrap();

        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Released));
        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Released));
        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Processed));

        assert_eq!(tally.runs(), 3);
        assert!(queue.failed_jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_job_with_one_try_is_buried_on_its_first_failure() {
        let (queue, worker) = worker();
        let tally = counter();
        queue.dispatch(&FailingJob::new(tally, 1)).await.unwrap();

        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Failed));
        assert_eq!(queue.failed_jobs().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_panicking_job_fails_cleanly_instead_of_killing_the_worker() {
        let (queue, worker) = worker();
        let tally = counter();
        let healthy = counter();

        queue.dispatch(&PanickingJob::new(tally, 1)).await.unwrap();
        queue.dispatch(&CountingJob::new(healthy, 1)).await.unwrap();

        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Failed));

        let failed = queue.failed_jobs().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert!(
            failed[0].error.contains("this job panicked on purpose"),
            "the panic message should survive: {}",
            failed[0].error
        );

        // The worker is still alive and the next job is unaffected.
        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Processed));
        assert_eq!(healthy.runs(), 1);
    }

    #[tokio::test]
    async fn a_job_with_no_registered_handler_fails_rather_than_vanishing() {
        let (queue, worker) = worker();
        queue.push(QueuedJob::new("never-registered", Json::Null).with_tries(1)).await.unwrap();

        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Failed));

        let failed = queue.failed_jobs().await.unwrap();
        assert!(failed[0].error.contains("no handler is registered"), "{}", failed[0].error);
    }

    #[tokio::test]
    async fn max_jobs_stops_the_loop_without_a_shutdown_signal() {
        let (queue, worker) = worker();
        let tally = counter();
        for _ in 0..5 {
            queue.dispatch(&CountingJob::new(tally, 1)).await.unwrap();
        }

        let stats = worker.max_jobs(2).run(Shutdown::new()).await.unwrap();

        assert_eq!(stats.processed, 2);
        assert_eq!(tally.runs(), 2);
        assert_eq!(queue.size("default").await.unwrap(), 3, "the rest are still waiting");
    }

    #[tokio::test]
    async fn an_already_signalled_shutdown_starts_no_work_at_all() {
        let (queue, worker) = worker();
        let tally = counter();
        queue.dispatch(&CountingJob::new(tally, 1)).await.unwrap();

        let shutdown = Shutdown::new();
        shutdown.signal();

        let stats = worker.run(shutdown).await.unwrap();

        assert_eq!(stats, WorkerStats::default());
        assert_eq!(tally.runs(), 0);
        assert_eq!(queue.size("default").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn shutdown_lets_the_job_in_hand_finish_first() {
        let (queue, worker) = worker();
        let tally = counter();
        queue.dispatch(&SlowJob::new(tally, 150)).await.unwrap();
        queue.dispatch(&CountingJob::new(tally, 1)).await.unwrap();

        let shutdown = Shutdown::new();
        let signal = shutdown.clone();
        tokio::spawn(async move {
            // Land in the middle of the slow job, not before it starts.
            tokio::time::sleep(Duration::from_millis(40)).await;
            signal.signal();
        });

        let stats = worker.run(shutdown).await.unwrap();

        assert_eq!(stats.processed, 1, "the slow job ran to completion");
        assert_eq!(tally.runs(), 1);
        assert_eq!(queue.size("default").await.unwrap(), 1, "the second job was left alone");
    }

    #[tokio::test]
    async fn an_idle_worker_wakes_up_for_shutdown_instead_of_waiting_out_its_poll() {
        let (_queue, worker) = worker();
        let worker = worker.poll_interval(Duration::from_secs(30));

        let shutdown = Shutdown::new();
        let signal = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            signal.signal();
        });

        let started = Instant::now();
        worker.run(shutdown).await.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an idle worker should stop as soon as it is asked, not after its poll interval"
        );
    }

    #[tokio::test]
    async fn several_workers_share_one_queue_and_run_each_job_once() {
        let (queue, worker) = worker();
        let tally = counter();
        for _ in 0..40 {
            queue.dispatch(&CountingJob::new(tally, 1)).await.unwrap();
        }

        let shutdown = Shutdown::new();
        let signal = shutdown.clone();
        let drain = queue.clone();
        tokio::spawn(async move {
            while drain.size("default").await.unwrap_or(0) > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            signal.signal();
        });

        let stats = run_pool(worker, 4, shutdown).await.unwrap();

        assert_eq!(stats.processed, 40);
        assert_eq!(tally.runs(), 40, "no job ran twice and none was skipped");
        assert_eq!(tally.total(), 40);
    }

    #[tokio::test]
    async fn the_worker_reports_what_it_did_on_the_event_bus() {
        // The event bus is process-wide, so this test cannot share it with
        // another that also subscribes; every assertion here filters by the job
        // name it dispatched, which nothing else uses.
        let (queue, worker) = worker();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);

        events::subscribe(move |event: &Event| {
            if event.field("job").and_then(Json::as_str) == Some("failing") {
                sink.lock()
                    .unwrap()
                    .push((event.kind, event.field("attempt").and_then(Json::as_i64)));
            }
        });

        queue.dispatch(&FailingJob::new(counter(), 2)).await.unwrap();
        worker.run_once().await.unwrap();
        worker.run_once().await.unwrap();

        let seen = seen.lock().unwrap().clone();
        assert!(seen.contains(&("queue.pushed", None)), "{seen:?}");
        assert!(seen.contains(&("queue.failed", Some(1))), "{seen:?}");
        assert!(seen.contains(&("queue.failed", Some(2))), "{seen:?}");

        events::clear_subscribers();
    }

    #[test]
    fn backoff_doubles_per_attempt_and_then_stops_at_the_cap() {
        let base = Duration::from_secs(10);
        let cap = Duration::from_secs(300);

        assert_eq!(backoff(base, 1, cap), Duration::from_secs(10));
        assert_eq!(backoff(base, 2, cap), Duration::from_secs(20));
        assert_eq!(backoff(base, 3, cap), Duration::from_secs(40));
        assert_eq!(backoff(base, 4, cap), Duration::from_secs(80));
        assert_eq!(backoff(base, 10, cap), cap, "the cap wins eventually");
        assert_eq!(backoff(base, 5000, cap), cap, "and never overflows");
    }

    #[test]
    fn backoff_of_zero_stays_zero_so_a_test_can_retry_immediately() {
        assert_eq!(backoff(Duration::ZERO, 9, DEFAULT_BACKOFF_CAP), Duration::ZERO);
    }

    #[test]
    fn stats_add_up_across_a_pool() {
        let mut stats = WorkerStats { processed: 1, released: 2, failed: 3 };
        stats.merge(WorkerStats { processed: 10, released: 20, failed: 30 });

        assert_eq!(stats, WorkerStats { processed: 11, released: 22, failed: 33 });
        assert_eq!(stats.attempts(), 66);
    }

    #[test]
    fn outcomes_render_for_a_dashboard() {
        assert_eq!(outcome_to_json(Outcome::Processed).as_str(), Some("processed"));
        assert_eq!(outcome_to_json(Outcome::Released).as_str(), Some("released"));
        assert_eq!(outcome_to_json(Outcome::Failed).as_str(), Some("failed"));
    }
}
