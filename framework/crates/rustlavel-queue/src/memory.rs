//! The in-process driver: the default, and the one tests use.
//!
//! Everything lives behind one `Mutex`. A queue is not a cache — it is a
//! sequencing device, and every operation on it has to agree with every other
//! about what the head of the queue is — so sharding would buy nothing but a
//! way to hand the same job to two workers.
//!
//! No lock is ever held across an `await`, which is why a plain `std` mutex is
//! the right one here rather than Tokio's.

use crate::job::{BoxFuture, FailedJob, QueuedJob, ReservedJob};
use crate::queue::{Queue, record_pushed};
use crate::time::unix_now;
use rustlavel_core::{Error, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A job waiting on a queue.
#[derive(Debug, Clone)]
struct Entry {
    id: String,
    job: QueuedJob,
    attempts: u32,
    /// Epoch seconds before which no worker may see this job.
    available_at: i64,
}

#[derive(Default)]
struct State {
    /// Keyed by queue name; each list is kept in insertion order, so a queue
    /// with no delays is strictly first-in-first-out.
    ready: HashMap<String, Vec<Entry>>,
    failed: Vec<FailedJob>,
}

struct Inner {
    state: Mutex<State>,
    next_id: AtomicU64,
}

/// A process-local queue. Cloning shares one store, so it can be registered as
/// application state and handed to every handler and worker.
#[derive(Clone)]
pub struct MemoryQueue {
    inner: Arc<Inner>,
}

impl Default for MemoryQueue {
    fn default() -> Self {
        MemoryQueue::new()
    }
}

impl MemoryQueue {
    pub fn new() -> Self {
        MemoryQueue {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.inner
            .state
            .lock()
            .map_err(|_| Error::msg("the in-memory queue was poisoned by a panic while locked"))
    }

    /// How many jobs are ready to run right now, as opposed to [`Queue::size`],
    /// which counts delayed ones too. Useful for asserting in a test that a
    /// delay is actually holding a job back.
    pub fn ready_size(&self, queue: &str) -> Result<u64> {
        let now = unix_now();
        let state = self.state()?;
        Ok(state
            .ready
            .get(queue)
            .map(|entries| entries.iter().filter(|e| e.available_at <= now).count() as u64)
            .unwrap_or(0))
    }

    /// Empty the dead-letter store. `queue:flush` in Laravel.
    pub fn flush_failed(&self) -> Result<u64> {
        let mut state = self.state()?;
        let count = state.failed.len() as u64;
        state.failed.clear();
        Ok(count)
    }
}

impl Queue for MemoryQueue {
    fn driver(&self) -> &'static str {
        "memory"
    }

    fn push(&self, job: QueuedJob) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst).to_string();
            let entry = Entry {
                id: id.clone(),
                available_at: unix_now() + job.delay.as_secs() as i64,
                job: job.clone(),
                attempts: 0,
            };

            self.state()?.ready.entry(job.queue.clone()).or_default().push(entry);

            record_pushed(self.driver(), &job, &id);
            Ok(id)
        })
    }

    fn pop<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<Option<ReservedJob>>> {
        Box::pin(async move {
            let now = unix_now();
            let mut state = self.state()?;
            let Some(entries) = state.ready.get_mut(queue) else { return Ok(None) };

            // The first *available* job, not the first job: a delayed one at the
            // head must not block the ready one behind it.
            let Some(index) = entries.iter().position(|entry| entry.available_at <= now) else {
                return Ok(None);
            };

            // Removing rather than flagging is safe here because the store dies
            // with the process: there is no crashed-worker case to recover, and
            // a job in a worker's hand is a job no other worker can see.
            let entry = entries.remove(index);

            Ok(Some(ReservedJob {
                id: entry.id,
                job: entry.job,
                attempts: entry.attempts + 1,
            }))
        })
    }

    fn size<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            Ok(self.state()?.ready.get(queue).map(|entries| entries.len() as u64).unwrap_or(0))
        })
    }

    fn delete<'a>(&'a self, _job: &'a ReservedJob) -> BoxFuture<'a, Result<()>> {
        // Reserving already removed it. Kept as a no-op so the worker's flow is
        // the same against every driver.
        Box::pin(async { Ok(()) })
    }

    fn release<'a>(&'a self, job: &'a ReservedJob, delay: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let entry = Entry {
                id: job.id.clone(),
                job: job.job.clone(),
                attempts: job.attempts,
                available_at: unix_now() + delay.as_secs() as i64,
            };

            self.state()?.ready.entry(job.job.queue.clone()).or_default().push(entry);
            Ok(())
        })
    }

    fn fail<'a>(&'a self, job: &'a ReservedJob, error: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.state()?.failed.push(FailedJob {
                id: job.id.clone(),
                name: job.job.name.clone(),
                queue: job.job.queue.clone(),
                payload: job.job.payload.clone(),
                attempts: job.attempts,
                error: error.to_string(),
                failed_at: unix_now(),
            });
            Ok(())
        })
    }

    fn failed_jobs(&self) -> BoxFuture<'_, Result<Vec<FailedJob>>> {
        Box::pin(async move { Ok(self.state()?.failed.clone()) })
    }

    fn clear<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            Ok(self
                .state()?
                .ready
                .remove(queue)
                .map(|entries| entries.len() as u64)
                .unwrap_or(0))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Job;
    use crate::queue::QueueExt;
    use crate::tests_support::{CountingJob, counter};
    use rustlavel_core::Json;

    #[tokio::test]
    async fn a_pushed_job_comes_back_out_intact() {
        let queue = MemoryQueue::new();
        let id = queue.dispatch(&CountingJob::new(counter(), 3)).await.unwrap();

        assert_eq!(queue.size("default").await.unwrap(), 1);

        let reserved = queue.pop("default").await.unwrap().expect("a job is waiting");
        assert_eq!(reserved.id, id);
        assert_eq!(reserved.job.name, CountingJob::NAME);
        assert_eq!(reserved.attempts, 1);
        assert_eq!(reserved.job.payload.get("step").and_then(Json::as_i64), Some(3));

        queue.delete(&reserved).await.unwrap();
        assert_eq!(queue.size("default").await.unwrap(), 0);
        assert!(queue.pop("default").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_empty_queue_pops_nothing_rather_than_failing() {
        let queue = MemoryQueue::new();
        assert!(queue.pop("default").await.unwrap().is_none());
        assert_eq!(queue.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn jobs_come_back_in_the_order_they_went_in() {
        let queue = MemoryQueue::new();
        for step in 1..=3 {
            queue.dispatch(&CountingJob::new(counter(), step)).await.unwrap();
        }

        let mut seen = Vec::new();
        while let Some(job) = queue.pop("default").await.unwrap() {
            seen.push(job.job.payload.get("step").and_then(Json::as_i64).unwrap());
        }

        assert_eq!(seen, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn queues_are_separate_from_each_other() {
        let queue = MemoryQueue::new();
        queue.push(QueuedJob::new("a", Json::Null)).await.unwrap();
        queue.push(QueuedJob::new("b", Json::Null).on_queue("emails")).await.unwrap();

        assert_eq!(queue.size("default").await.unwrap(), 1);
        assert_eq!(queue.size("emails").await.unwrap(), 1);
        assert_eq!(queue.pop("emails").await.unwrap().unwrap().job.name, "b");
        assert!(queue.pop("emails").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn push_on_overrides_the_queue_the_job_asked_for() {
        let queue = MemoryQueue::new();
        queue.push_on("urgent", QueuedJob::new("a", Json::Null)).await.unwrap();

        assert_eq!(queue.size("default").await.unwrap(), 0);
        assert_eq!(queue.size("urgent").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_delayed_job_is_counted_but_not_yet_visible() {
        let queue = MemoryQueue::new();
        queue
            .later(Duration::from_secs(3600), QueuedJob::new("later", Json::Null))
            .await
            .unwrap();

        assert_eq!(queue.size("default").await.unwrap(), 1, "it is on the queue");
        assert_eq!(queue.ready_size("default").unwrap(), 0, "but not ready");
        assert!(queue.pop("default").await.unwrap().is_none(), "and not poppable");
    }

    #[tokio::test]
    async fn a_delay_that_has_already_passed_is_visible_immediately() {
        let queue = MemoryQueue::new();
        queue.later(Duration::ZERO, QueuedJob::new("now", Json::Null)).await.unwrap();

        assert!(queue.pop("default").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_delayed_job_does_not_block_a_ready_one_behind_it() {
        let queue = MemoryQueue::new();
        queue
            .later(Duration::from_secs(3600), QueuedJob::new("later", Json::Null))
            .await
            .unwrap();
        queue.push(QueuedJob::new("now", Json::Null)).await.unwrap();

        assert_eq!(queue.pop("default").await.unwrap().unwrap().job.name, "now");
        assert!(queue.pop("default").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn releasing_keeps_the_attempt_count() {
        let queue = MemoryQueue::new();
        queue.push(QueuedJob::new("retry-me", Json::Null)).await.unwrap();

        let first = queue.pop("default").await.unwrap().unwrap();
        assert_eq!(first.attempts, 1);
        queue.release(&first, Duration::ZERO).await.unwrap();

        let second = queue.pop("default").await.unwrap().unwrap();
        assert_eq!(second.attempts, 2, "a retry continues counting");
        assert_eq!(second.id, first.id, "and keeps its identity");
    }

    #[tokio::test]
    async fn a_release_with_a_delay_is_not_immediately_visible() {
        let queue = MemoryQueue::new();
        queue.push(QueuedJob::new("retry-me", Json::Null)).await.unwrap();

        let reserved = queue.pop("default").await.unwrap().unwrap();
        queue.release(&reserved, Duration::from_secs(3600)).await.unwrap();

        assert_eq!(queue.size("default").await.unwrap(), 1);
        assert!(queue.pop("default").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn failing_moves_a_job_to_the_dead_letter_store() {
        let queue = MemoryQueue::new();
        queue.push(QueuedJob::new("doomed", Json::object([("x", Json::from(1))]))).await.unwrap();

        let reserved = queue.pop("default").await.unwrap().unwrap();
        queue.fail(&reserved, "it exploded").await.unwrap();

        assert_eq!(queue.size("default").await.unwrap(), 0, "it is off the queue");

        let failed = queue.failed_jobs().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, "doomed");
        assert_eq!(failed[0].error, "it exploded");
        assert_eq!(failed[0].attempts, 1);
        assert_eq!(failed[0].payload.get("x").and_then(Json::as_i64), Some(1));

        assert_eq!(queue.flush_failed().unwrap(), 1);
        assert!(queue.failed_jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn clearing_removes_only_the_named_queue() {
        let queue = MemoryQueue::new();
        queue.push(QueuedJob::new("a", Json::Null)).await.unwrap();
        queue.push(QueuedJob::new("b", Json::Null)).await.unwrap();
        queue.push(QueuedJob::new("c", Json::Null).on_queue("other")).await.unwrap();

        assert_eq!(queue.clear("default").await.unwrap(), 2);
        assert_eq!(queue.size("default").await.unwrap(), 0);
        assert_eq!(queue.size("other").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn clones_share_one_store() {
        let queue = MemoryQueue::new();
        let clone = queue.clone();

        queue.push(QueuedJob::new("shared", Json::Null)).await.unwrap();
        assert_eq!(clone.size("default").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_driver_is_usable_behind_a_trait_object() {
        let queue: Arc<dyn Queue> = Arc::new(MemoryQueue::new());

        queue.dispatch(&CountingJob::new(counter(), 1)).await.unwrap();
        assert_eq!(queue.size("default").await.unwrap(), 1);
        assert_eq!(queue.driver(), "memory");
    }
}
