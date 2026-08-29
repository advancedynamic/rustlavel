//! `Queue::fake()` — a queue that records instead of running.
//!
//! Application tests need this far more than they need a real queue. What a
//! controller test wants to know is "did registering a user dispatch the
//! welcome email?", not "does the welcome email work?" — that is the job's own
//! test. So the fake accepts pushes, remembers them, and never hands anything
//! to a worker.
//!
//! ```ignore
//! let queue = fake();
//! client.post("/register", &[("email", "ada@example.com")]).await.assert_ok();
//! queue.assert_pushed("send-welcome-email");
//! ```
//!
//! The assertions panic with a message that lists what *was* pushed, because
//! "assertion failed" on its own leaves you re-running the test with print
//! statements.

use crate::job::{BoxFuture, FailedJob, QueuedJob, ReservedJob};
use crate::queue::{Queue, record_pushed};
use rustlavel_core::{Error, Result};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The Rust spelling of Laravel's `Queue::fake()`.
pub fn fake() -> FakeQueue {
    FakeQueue::new()
}

/// A queue that records dispatches and runs nothing.
#[derive(Default)]
pub struct FakeQueue {
    pushed: Mutex<Vec<QueuedJob>>,
    next_id: AtomicU64,
}

impl FakeQueue {
    pub fn new() -> Self {
        FakeQueue { pushed: Mutex::new(Vec::new()), next_id: AtomicU64::new(1) }
    }

    /// Every job pushed, in order.
    pub fn all(&self) -> Vec<QueuedJob> {
        self.pushed.lock().expect("the fake queue was poisoned").clone()
    }

    /// The jobs pushed under one name — for asserting on a payload.
    pub fn pushed(&self, name: &str) -> Vec<QueuedJob> {
        self.all().into_iter().filter(|job| job.name == name).collect()
    }

    /// How many jobs were pushed in total.
    pub fn count(&self) -> usize {
        self.pushed.lock().expect("the fake queue was poisoned").len()
    }

    /// Forget everything, so one test can reuse a fake across phases.
    pub fn reset(&self) {
        self.pushed.lock().expect("the fake queue was poisoned").clear();
    }

    /// Assert a job with this name was dispatched at least once.
    #[track_caller]
    pub fn assert_pushed(&self, name: &str) -> &Self {
        let found = self.pushed(name);
        assert!(
            !found.is_empty(),
            "expected `{name}` to have been pushed, but {}",
            self.summary()
        );
        self
    }

    /// Assert a job with this name was dispatched exactly `times` times.
    #[track_caller]
    pub fn assert_pushed_times(&self, name: &str, times: usize) -> &Self {
        let found = self.pushed(name).len();
        assert_eq!(
            found, times,
            "expected `{name}` to have been pushed {times} time(s), but it was pushed {found} \
             time(s); {}",
            self.summary()
        );
        self
    }

    /// Assert a job with this name was never dispatched.
    #[track_caller]
    pub fn assert_not_pushed(&self, name: &str) -> &Self {
        self.assert_pushed_times(name, 0)
    }

    /// Assert nothing at all was dispatched.
    #[track_caller]
    pub fn assert_nothing_pushed(&self) -> &Self {
        assert!(self.count() == 0, "expected nothing to have been pushed, but {}", self.summary());
        self
    }

    /// Assert a job was dispatched onto a particular queue.
    #[track_caller]
    pub fn assert_pushed_on(&self, queue: &str, name: &str) -> &Self {
        let found = self.pushed(name);
        assert!(
            found.iter().any(|job| job.queue == queue),
            "expected `{name}` to have been pushed on `{queue}`, but it went to {:?}; {}",
            found.iter().map(|job| job.queue.as_str()).collect::<Vec<_>>(),
            self.summary()
        );
        self
    }

    /// Assert a job was dispatched with a delay of at least `delay`.
    #[track_caller]
    pub fn assert_pushed_later(&self, name: &str, delay: Duration) -> &Self {
        let found = self.pushed(name);
        assert!(
            found.iter().any(|job| job.delay >= delay),
            "expected `{name}` to have been delayed by at least {delay:?}, but the delays were \
             {:?}; {}",
            found.iter().map(|job| job.delay).collect::<Vec<_>>(),
            self.summary()
        );
        self
    }

    /// What actually happened, for a failure message.
    fn summary(&self) -> String {
        let all = self.all();
        if all.is_empty() {
            return "nothing was pushed at all".to_string();
        }

        let names: Vec<String> =
            all.iter().map(|job| format!("{} on `{}`", job.name, job.queue)).collect();
        format!("what was pushed: {}", names.join(", "))
    }
}

impl Queue for FakeQueue {
    fn driver(&self) -> &'static str {
        "fake"
    }

    fn push(&self, job: QueuedJob) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst).to_string();
            self.pushed.lock().expect("the fake queue was poisoned").push(job.clone());

            // Still reported on the bus: a Telescope test should see the same
            // events an application would produce.
            record_pushed(self.driver(), &job, &id);
            Ok(id)
        })
    }

    /// Always empty. A fake that handed jobs back would run them, which is the
    /// one thing it exists not to do.
    fn pop<'a>(&'a self, _queue: &'a str) -> BoxFuture<'a, Result<Option<ReservedJob>>> {
        Box::pin(async { Ok(None) })
    }

    fn size<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            Ok(self.all().iter().filter(|job| job.queue == queue).count() as u64)
        })
    }

    fn delete<'a>(&'a self, _job: &'a ReservedJob) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn release<'a>(&'a self, _job: &'a ReservedJob, _delay: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(Error::msg("a faked queue never reserves a job, so it cannot release one"))
        })
    }

    fn fail<'a>(&'a self, _job: &'a ReservedJob, _error: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(Error::msg("a faked queue never reserves a job, so it cannot fail one"))
        })
    }

    fn failed_jobs(&self) -> BoxFuture<'_, Result<Vec<FailedJob>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn clear<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let mut pushed = self.pushed.lock().expect("the fake queue was poisoned");
            let before = pushed.len();
            pushed.retain(|job| job.queue != queue);
            Ok((before - pushed.len()) as u64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::QueueExt;
    use crate::tests_support::{CountingJob, counter};
    use rustlavel_core::Json;

    #[tokio::test]
    async fn a_faked_queue_records_a_dispatch_without_running_it() {
        let queue = fake();
        let tally = counter();

        queue.dispatch(&CountingJob::new(tally, 5)).await.unwrap();

        queue.assert_pushed("counting");
        assert_eq!(tally.runs(), 0, "a fake must never run the job");
        assert_eq!(queue.count(), 1);
    }

    #[tokio::test]
    async fn a_faked_queue_hands_nothing_to_a_worker() {
        let queue = fake();
        queue.push(QueuedJob::new("anything", Json::Null)).await.unwrap();

        assert!(queue.pop("default").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn counting_assertions_distinguish_between_job_names() {
        let queue = fake();
        for _ in 0..3 {
            queue.push(QueuedJob::new("a", Json::Null)).await.unwrap();
        }
        queue.push(QueuedJob::new("b", Json::Null)).await.unwrap();

        queue.assert_pushed_times("a", 3).assert_pushed_times("b", 1).assert_not_pushed("c");
    }

    #[tokio::test]
    async fn nothing_pushed_holds_on_a_fresh_fake_and_after_a_reset() {
        let queue = fake();
        queue.assert_nothing_pushed();

        queue.push(QueuedJob::new("a", Json::Null)).await.unwrap();
        queue.reset();
        queue.assert_nothing_pushed();
    }

    #[tokio::test]
    async fn the_queue_and_delay_a_job_was_given_are_both_recorded() {
        let queue = fake();
        queue.push_on("emails", QueuedJob::new("welcome", Json::Null)).await.unwrap();
        queue
            .later(Duration::from_secs(600), QueuedJob::new("digest", Json::Null))
            .await
            .unwrap();

        queue.assert_pushed_on("emails", "welcome");
        queue.assert_pushed_later("digest", Duration::from_secs(300));
        assert_eq!(queue.size("emails").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_recorded_payload_is_available_for_inspection() {
        let queue = fake();
        queue
            .push(QueuedJob::new("invoice", Json::object([("order", Json::from(42))])))
            .await
            .unwrap();

        let pushed = queue.pushed("invoice");
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].payload.get("order").and_then(Json::as_i64), Some(42));
    }

    #[tokio::test]
    async fn a_failed_assertion_says_what_was_pushed_instead() {
        let queue = fake();
        queue.push(QueuedJob::new("actual", Json::Null)).await.unwrap();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            queue.assert_pushed("expected");
        }))
        .unwrap_err();

        let message = rustlavel_http::panic::message_of(panic.as_ref());
        assert!(message.contains("expected `expected` to have been pushed"), "{message}");
        assert!(message.contains("actual on `default`"), "{message}");
    }

    #[tokio::test]
    async fn a_fake_is_usable_wherever_a_real_queue_is() {
        let queue: std::sync::Arc<dyn Queue> = std::sync::Arc::new(fake());

        queue.dispatch(&CountingJob::new(counter(), 1)).await.unwrap();

        assert_eq!(queue.driver(), "fake");
        assert_eq!(queue.size("default").await.unwrap(), 1);
        assert!(queue.failed_jobs().await.unwrap().is_empty());
    }
}
