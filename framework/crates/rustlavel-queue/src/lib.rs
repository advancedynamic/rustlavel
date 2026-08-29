//! rustlavel-queue: background jobs and scheduling.
//!
//! Work that should not happen while someone is waiting for a response —
//! sending mail, resizing an image, calling a slow API — goes on a queue and is
//! run by a worker. Work that should happen at a particular time goes on a
//! schedule.
//!
//! ```ignore
//! // Define a job.
//! struct SendWelcomeEmail { user_id: i64 }
//!
//! impl Job for SendWelcomeEmail {
//!     const NAME: &'static str = "send-welcome-email";
//!
//!     fn payload(&self) -> Json {
//!         Json::object([("user_id", Json::from(self.user_id))])
//!     }
//!
//!     fn from_payload(payload: &Json) -> Result<Self> {
//!         Ok(SendWelcomeEmail {
//!             user_id: payload.get("user_id").and_then(Json::as_i64).unwrap_or_default(),
//!         })
//!     }
//!
//!     fn handle(&self) -> impl Future<Output = Result<()>> + Send {
//!         async move { Ok(()) }
//!     }
//! }
//!
//! // Dispatch it from a handler.
//! queue.dispatch(&SendWelcomeEmail { user_id }).await?;
//!
//! // Run it, in `queue:work`.
//! let mut jobs = JobRegistry::new();
//! jobs.register::<SendWelcomeEmail>();
//!
//! let shutdown = Shutdown::new();
//! shutdown.on_ctrl_c();
//! run_pool(Worker::new(queue, Arc::new(jobs)), 4, shutdown).await?;
//! ```
//!
//! ## The one thing that is not like Laravel
//!
//! Laravel serialises the job object and asks PHP to bring the class back. A
//! compiled language cannot resurrect a type from a string, so the way back
//! from a stored name to running code is stated once, at boot, in a
//! [`JobRegistry`]. That is the only line of ceremony the design adds, and it
//! buys a `queue:work` that fails to compile against a renamed job rather than
//! failing at three in the morning against a class that is no longer there.
//!
//! ## What is in here
//!
//! * [`Job`] and [`JobRegistry`] — defining work and finding it again.
//! * [`Queue`] and [`QueueExt`] — the driver contract and its conveniences.
//! * [`MemoryQueue`], [`DatabaseQueue`], [`FakeQueue`] — the three drivers.
//! * [`Worker`], [`run_pool`], [`Shutdown`] — running jobs, and stopping well.
//! * [`Scheduler`] and [`Cron`] — running work at a time rather than on demand.
//! * [`QueueDashboard`] — the plugin that exposes it all over HTTP.
//!
//! Everything the queue does is reported on `rustlavel_core::events` as
//! `queue.pushed`, `queue.processed` and `queue.failed`, so Telescope shows
//! jobs without this crate knowing Telescope exists.

pub mod cron;
pub mod database;
pub mod fake;
pub mod job;
pub mod memory;
pub mod plugin;
pub mod queue;
pub mod schedule;
pub mod time;
pub mod worker;

#[cfg(test)]
mod tests_support;

pub use cron::{Cron, Weekday};
pub use database::{CreateQueueTables, DatabaseQueue};
pub use fake::{FakeQueue, fake};
pub use job::{
    BoxFuture, DEFAULT_QUEUE, DEFAULT_RETRY_AFTER, DEFAULT_TRIES, FailedJob, Job, JobRegistry,
    QueuedJob, ReservedJob,
};
pub use memory::MemoryQueue;
pub use plugin::QueueDashboard;
pub use queue::{Queue, QueueExt};
pub use schedule::{ScheduledEvent, Scheduler, TickReport, schedule};
pub use worker::{Outcome, Shutdown, Worker, WorkerOptions, WorkerStats, run_pool};

pub use rustlavel_core::{Error, Result};

/// What a job file imports.
pub mod prelude {
    pub use crate::{
        Cron, DatabaseQueue, FailedJob, FakeQueue, Job, JobRegistry, MemoryQueue, Queue, QueueExt,
        QueuedJob, ReservedJob, Scheduler, Shutdown, Weekday, Worker, run_pool,
    };
    pub use rustlavel_core::{Error, Json, Result};
    pub use std::future::Future;
    pub use std::sync::Arc;
    pub use std::time::Duration;
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use crate::tests_support::{CountingJob, FailingJob, counter, registry};
    use crate::worker::Outcome;

    /// The whole lifecycle, in the order an application meets it: define,
    /// dispatch, work, retry, bury.
    #[tokio::test]
    async fn a_job_travels_from_dispatch_through_a_worker_to_completion() {
        let queue: Arc<dyn Queue> = Arc::new(MemoryQueue::new());
        let worker = Worker::new(Arc::clone(&queue), Arc::new(registry()))
            .poll_interval(Duration::from_millis(5));

        let tally = counter();
        queue.dispatch(&CountingJob::new(tally, 10)).await.unwrap();
        queue.dispatch(&CountingJob::new(tally, 5)).await.unwrap();
        queue.dispatch(&FailingJob::new(counter(), 1)).await.unwrap();

        let shutdown = Shutdown::new();
        let stats = worker.max_jobs(3).run(shutdown).await.unwrap();

        assert_eq!(stats.processed, 2);
        assert_eq!(stats.failed, 1);
        assert_eq!(tally.total(), 15);
        assert_eq!(queue.size("default").await.unwrap(), 0);
        assert_eq!(queue.failed_jobs().await.unwrap().len(), 1);
    }

    /// The same job, the same worker, against a fake: nothing runs, everything
    /// is recorded.
    #[tokio::test]
    async fn the_fake_stands_in_for_the_real_thing_in_an_application_test() {
        let queue = crate::fake();
        let tally = counter();

        queue.dispatch(&CountingJob::new(tally, 10)).await.unwrap();

        queue.assert_pushed("counting").assert_pushed_times("counting", 1);
        assert_eq!(tally.runs(), 0);
    }

    #[tokio::test]
    async fn a_delayed_job_waits_and_a_worker_finds_nothing_meanwhile() {
        let queue: Arc<dyn Queue> = Arc::new(MemoryQueue::new());
        let worker = Worker::new(Arc::clone(&queue), Arc::new(registry()));

        queue
            .dispatch_later(Duration::from_secs(3600), &CountingJob::new(counter(), 1))
            .await
            .unwrap();

        assert_eq!(worker.run_once().await.unwrap(), None);
        assert_eq!(queue.size("default").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_scheduler_and_the_worker_meet_in_the_middle() {
        let memory = Arc::new(MemoryQueue::new());
        let queue: Arc<dyn Queue> = memory.clone();
        let tally = counter();

        let mut plan = Scheduler::new(Arc::clone(&queue));
        plan.job(CountingJob::new(tally, 3)).daily().at("13:00");

        plan.tick(crate::time::to_unix(2026, 8, 29, 13, 0, 0)).await.unwrap();

        let worker = Worker::new(Arc::clone(&queue), Arc::new(registry()))
            .poll_interval(Duration::from_millis(5))
            .max_jobs(1);

        // The scheduler spawns its dispatch, so the worker may look before the
        // job lands; it is running a loop for exactly this reason.
        let shutdown = Shutdown::new();
        let signal = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            signal.signal();
        });

        let stats = worker.run(shutdown).await.unwrap();

        assert_eq!(stats.processed, 1);
        assert_eq!(tally.total(), 3);
    }

    #[tokio::test]
    async fn a_worker_survives_whatever_a_job_does_to_it() {
        use crate::tests_support::PanickingJob;

        let queue: Arc<dyn Queue> = Arc::new(MemoryQueue::new());
        let worker = Worker::new(Arc::clone(&queue), Arc::new(registry()));

        queue.dispatch(&PanickingJob::new(counter(), 1)).await.unwrap();
        let healthy = counter();
        queue.dispatch(&CountingJob::new(healthy, 1)).await.unwrap();

        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Failed));
        assert_eq!(worker.run_once().await.unwrap(), Some(Outcome::Processed));
        assert_eq!(healthy.runs(), 1);
    }
}
