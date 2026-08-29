//! The [`Queue`] contract every driver implements.
//!
//! Dyn-compatible on purpose: an application picks its driver at boot and the
//! rest of the framework holds an `Arc<dyn Queue>` without knowing which one it
//! got. That is why every method returns a [`BoxFuture`] rather than being an
//! `async fn` — `async fn` in a trait is not dyn-compatible — and why the trait
//! speaks in [`QueuedJob`] rather than in a generic `J: Job`.
//!
//! The generic conveniences that cannot be dyn-compatible (`dispatch`, which
//! takes a concrete job type) live in [`QueueExt`], blanket-implemented for
//! every `Queue` including `dyn Queue`, so callers never notice the split.

use crate::job::{BoxFuture, FailedJob, Job, QueuedJob, ReservedJob};
use rustlavel_core::events::{self, Event};
use rustlavel_core::{Json, Result};
use std::time::Duration;

/// A queue backend.
pub trait Queue: Send + Sync + 'static {
    /// The driver's name. It appears in `queue.*` events and error messages,
    /// which is how Telescope shows which backend handled a job.
    fn driver(&self) -> &'static str;

    /// Store a job. Returns the driver's identifier for it.
    ///
    /// The envelope carries its own queue and delay, so this one primitive is
    /// all a driver has to implement.
    fn push(&self, job: QueuedJob) -> BoxFuture<'_, Result<String>>;

    /// Store a job on a named queue, whatever the job itself asked for.
    fn push_on<'a>(&'a self, queue: &'a str, job: QueuedJob) -> BoxFuture<'a, Result<String>> {
        self.push(job.on_queue(queue))
    }

    /// Store a job that only becomes visible to a worker after `delay`.
    fn later<'a>(&'a self, delay: Duration, job: QueuedJob) -> BoxFuture<'a, Result<String>> {
        self.push(job.with_delay(delay))
    }

    /// Reserve the next available job, or `None` when there is nothing ready.
    ///
    /// A delayed job whose time has not come is not "available"; neither is one
    /// another worker already holds. Reserving must be atomic across workers —
    /// see the database driver for how that is done and why.
    fn pop<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<Option<ReservedJob>>>;

    /// How many jobs are waiting on a queue, including delayed ones.
    fn size<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>>;

    /// Finish with a reserved job: it succeeded and is gone.
    fn delete<'a>(&'a self, job: &'a ReservedJob) -> BoxFuture<'a, Result<()>>;

    /// Put a reserved job back, available again after `delay`.
    ///
    /// The attempt count is kept, which is what makes a retry a retry rather
    /// than a fresh job that will fail forever.
    fn release<'a>(&'a self, job: &'a ReservedJob, delay: Duration) -> BoxFuture<'a, Result<()>>;

    /// Move a reserved job to the dead-letter store with the error that killed
    /// it. It must no longer be on the queue afterwards.
    fn fail<'a>(&'a self, job: &'a ReservedJob, error: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Everything in the dead-letter store, oldest first.
    fn failed_jobs(&self) -> BoxFuture<'_, Result<Vec<FailedJob>>>;

    /// Throw away every job waiting on a queue. Returns how many went.
    fn clear<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>>;
}

/// The conveniences that take a concrete job type, and so cannot live on a
/// dyn-compatible trait. Blanket-implemented, including for `dyn Queue`.
pub trait QueueExt: Queue {
    /// `dispatch(SendWelcomeEmail { user_id })` — the everyday call.
    fn dispatch<'a, J: Job>(&'a self, job: &'a J) -> BoxFuture<'a, Result<String>> {
        self.push(job.to_queued())
    }

    /// Dispatch onto a named queue instead of the job's own.
    fn dispatch_on<'a, J: Job>(
        &'a self,
        queue: &'a str,
        job: &'a J,
    ) -> BoxFuture<'a, Result<String>> {
        self.push_on(queue, job.to_queued())
    }

    /// Dispatch, but not before `delay` has passed.
    fn dispatch_later<'a, J: Job>(
        &'a self,
        delay: Duration,
        job: &'a J,
    ) -> BoxFuture<'a, Result<String>> {
        self.later(delay, job.to_queued())
    }
}

impl<T: Queue + ?Sized> QueueExt for T {}

/// Report a push on the instrumentation bus.
///
/// Guarded by `has_subscribers` so an application with no Telescope never pays
/// for building an event nobody reads. Drivers call this from `push`; the
/// worker reports `queue.processed` and `queue.failed` itself.
pub(crate) fn record_pushed(driver: &'static str, job: &QueuedJob, id: &str) {
    if !events::has_subscribers() {
        return;
    }

    Event::new("queue.pushed")
        .with("job", job.name.as_str())
        .with("queue", job.queue.as_str())
        .with("id", id)
        .with("driver", driver)
        .with("delay_seconds", job.delay.as_secs())
        .dispatch();
}

/// The JSON body a dashboard or `queue:status` shows.
pub async fn stats(queue: &dyn Queue, names: &[String]) -> Result<Json> {
    let mut sizes = Vec::new();
    for name in names {
        sizes.push((name.clone(), Json::from(queue.size(name).await?)));
    }

    let failed = queue.failed_jobs().await?;

    Ok(Json::object([
        ("driver", Json::from(queue.driver())),
        ("queues", Json::Object(sizes.into_iter().collect())),
        ("failed", Json::Array(failed.iter().map(FailedJob::to_json).collect())),
    ]))
}
