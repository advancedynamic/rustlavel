//! What a job is, and how a worker finds one again after it has been stored.
//!
//! ```ignore
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
//!         Ok(SendWelcomeEmail { user_id: payload.get("user_id").and_then(Json::as_i64).unwrap_or(0) })
//!     }
//!
//!     fn handle(&self) -> impl Future<Output = Result<()>> + Send {
//!         async move { Ok(()) }
//!     }
//! }
//! ```

use crate::progress::JobContext;
use rustlavel_core::{Error, Json, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// The queue a job goes on when it does not ask for another.
pub const DEFAULT_QUEUE: &str = "default";

/// How many times a job is attempted before it is considered dead.
///
/// Three matches Laravel, and is the number that turns "the API blipped" into a
/// non-event without turning "this job is broken" into an infinite loop.
pub const DEFAULT_TRIES: u32 = 3;

/// The base delay before a failed job is retried. Doubled on each attempt by
/// the worker's backoff.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);

/// A boxed future, the shape everything dyn-compatible in this crate returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A unit of work that can be stored and run later.
///
/// Deliberately *not* dyn-compatible: `NAME` and `from_payload` have no
/// receiver, and `handle` returns an opaque future. Nothing needs a `dyn Job`,
/// because the moment a job is queued it becomes a [`QueuedJob`] — plain data —
/// and the way back to a type is the [`JobRegistry`].
pub trait Job: Send + Sync + 'static {
    /// The stable name this job is stored under.
    ///
    /// It travels into the database and back out again, possibly across a
    /// deploy, so renaming it strands every job already queued. Choose it the
    /// way you would choose a table name.
    const NAME: &'static str;

    /// Everything `from_payload` needs to rebuild this job.
    fn payload(&self) -> Json;

    /// Rebuild the job from a payload the queue handed back.
    ///
    /// Returning an error here fails the job rather than panicking the worker,
    /// which is what you want when an old payload meets new code.
    fn from_payload(payload: &Json) -> Result<Self>
    where
        Self: Sized;

    /// Do the work.
    ///
    /// Declared as `-> impl Future + Send` rather than `async fn` because the
    /// registry boxes the future and sends it to another task, and an `async
    /// fn` *in a trait* promises nothing about `Send`. An implementation is
    /// free to write `async fn handle(&self) -> Result<()>` instead: the
    /// compiler can see the body, so it can prove the `Send` this demands.
    fn handle(&self) -> impl Future<Output = Result<()>> + Send;

    /// Do the work, with somewhere to say how far along it is.
    ///
    /// The default calls [`Job::handle`] and reports nothing, so a job that
    /// takes a second need not care. A job that takes a minute overrides this
    /// and calls `ctx.progress(percent, "stage")` as it goes; a request
    /// handler in the web process reads it back from the same store.
    fn handle_with(&self, ctx: &JobContext) -> impl Future<Output = Result<()>> + Send {
        let _ = ctx;
        self.handle()
    }

    /// How many attempts this job gets before it is dead-lettered.
    fn tries(&self) -> u32 {
        DEFAULT_TRIES
    }

    /// How long to wait before the first retry. The worker doubles it per
    /// attempt.
    fn retry_after(&self) -> Duration {
        DEFAULT_RETRY_AFTER
    }

    /// Which queue this job belongs on.
    fn queue(&self) -> &'static str {
        DEFAULT_QUEUE
    }

    /// Package the job for storage. Rarely called directly — `push` does it.
    fn to_queued(&self) -> QueuedJob {
        QueuedJob {
            name: Self::NAME.to_string(),
            payload: self.payload(),
            queue: self.queue().to_string(),
            max_tries: self.tries().max(1),
            retry_after: self.retry_after(),
            delay: Duration::ZERO,
            chain: None,
            then: Vec::new(),
        }
    }
}

/// A job as it is stored: a name, a payload, and the policy for running it.
///
/// Everything past the dispatch call deals in this rather than in a concrete
/// type, which is what keeps the [`Queue`](crate::Queue) trait dyn-compatible.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedJob {
    pub name: String,
    pub payload: Json,
    pub queue: String,
    pub max_tries: u32,
    pub retry_after: Duration,
    /// How long after being pushed the job becomes visible to a worker.
    pub delay: Duration,
    /// The chain this job is a step of, when it is one. See [`crate::Chain`].
    pub chain: Option<String>,
    /// The steps to run after this one succeeds, in order. Empty for a job
    /// that stands alone.
    pub then: Vec<QueuedJob>,
}

impl QueuedJob {
    /// An envelope built by hand, for a caller that has a name and a payload
    /// but no Rust type — a job pushed by another service, say.
    pub fn new(name: impl Into<String>, payload: Json) -> Self {
        QueuedJob {
            name: name.into(),
            payload,
            queue: DEFAULT_QUEUE.to_string(),
            max_tries: DEFAULT_TRIES,
            retry_after: DEFAULT_RETRY_AFTER,
            delay: Duration::ZERO,
            chain: None,
            then: Vec::new(),
        }
    }

    /// The payload as it is stored, with the chain folded in.
    ///
    /// A chain rides inside the payload column rather than in columns of its
    /// own, so a `jobs` table created by 0.7 needs no migration to carry one.
    /// A job with no chain is stored exactly as before, so every row already in
    /// a table reads back unchanged. The keys start with `$` so they cannot
    /// collide with a payload's own.
    pub fn stored_payload(&self) -> Json {
        if self.chain.is_none() && self.then.is_empty() {
            return self.payload.clone();
        }
        Json::object([
            ("$payload", self.payload.clone()),
            ("$chain", self.chain.as_deref().map_or(Json::Null, Json::from)),
            ("$then", Json::Array(self.then.iter().map(QueuedJob::to_json).collect())),
        ])
    }

    /// The inverse of [`stored_payload`](Self::stored_payload): a job read back
    /// from storage, with the chain unfolded and the payload restored.
    pub fn with_stored_payload(mut self, stored: Json) -> QueuedJob {
        let is_envelope = stored.get("$payload").is_some() && stored.get("$then").is_some();
        if !is_envelope {
            self.payload = stored;
            return self;
        }
        self.chain = stored.get("$chain").and_then(Json::as_str).map(str::to_string);
        self.then = stored
            .get("$then")
            .and_then(Json::as_array)
            .map(|steps| steps.iter().filter_map(QueuedJob::from_json).collect())
            .unwrap_or_default();
        self.payload = stored.get("$payload").cloned().unwrap_or(Json::Null);
        self
    }

    /// The whole envelope as JSON, for a chain's remaining steps.
    pub fn to_json(&self) -> Json {
        Json::object([
            ("name", Json::from(self.name.as_str())),
            ("payload", self.payload.clone()),
            ("queue", Json::from(self.queue.as_str())),
            ("max_tries", Json::from(self.max_tries as i64)),
            ("retry_after", Json::from(self.retry_after.as_secs() as i64)),
            ("delay", Json::from(self.delay.as_secs() as i64)),
            ("then", Json::Array(self.then.iter().map(QueuedJob::to_json).collect())),
        ])
    }

    pub fn from_json(json: &Json) -> Option<QueuedJob> {
        Some(QueuedJob {
            name: json.get("name")?.as_str()?.to_string(),
            payload: json.get("payload").cloned().unwrap_or(Json::Null),
            queue: json.get("queue").and_then(Json::as_str).unwrap_or(DEFAULT_QUEUE).to_string(),
            max_tries: json.get("max_tries").and_then(Json::as_i64).unwrap_or(DEFAULT_TRIES as i64).max(1) as u32,
            retry_after: Duration::from_secs(json.get("retry_after").and_then(Json::as_i64).unwrap_or(60).max(0) as u64),
            delay: Duration::from_secs(json.get("delay").and_then(Json::as_i64).unwrap_or(0).max(0) as u64),
            chain: None,
            then: json
                .get("then")
                .and_then(Json::as_array)
                .map(|steps| steps.iter().filter_map(QueuedJob::from_json).collect())
                .unwrap_or_default(),
        })
    }

    pub fn on_queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = queue.into();
        self
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn with_tries(mut self, tries: u32) -> Self {
        self.max_tries = tries.max(1);
        self
    }

    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = retry_after;
        self
    }
}

/// A job a worker has taken off the queue and is responsible for.
///
/// It is not gone: until the worker deletes, releases or fails it, the job is
/// still the queue's business, which is what makes a crashed worker recoverable.
#[derive(Debug, Clone, PartialEq)]
pub struct ReservedJob {
    /// The driver's identifier for this job. Opaque to everything else.
    pub id: String,
    pub job: QueuedJob,
    /// How many times this job has now been handed to a worker, counting this
    /// time. Always at least 1.
    pub attempts: u32,
}

impl ReservedJob {
    /// Whether this attempt is the last one the job is allowed.
    pub fn is_last_attempt(&self) -> bool {
        self.attempts >= self.job.max_tries
    }
}

/// A job that used up every attempt, kept so a human can look at it.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedJob {
    pub id: String,
    pub name: String,
    pub queue: String,
    pub payload: Json,
    pub attempts: u32,
    /// What the last attempt said on its way out.
    pub error: String,
    /// Epoch seconds.
    pub failed_at: i64,
}

impl FailedJob {
    /// The shape a dashboard or `queue:failed` wants.
    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id.as_str())),
            ("name", Json::from(self.name.as_str())),
            ("queue", Json::from(self.queue.as_str())),
            ("payload", self.payload.clone()),
            ("attempts", Json::from(self.attempts)),
            ("error", Json::from(self.error.as_str())),
            ("failed_at", Json::from(self.failed_at)),
        ])
    }
}

/// What the registry stores: a payload in, a running job out.
type Handler = Arc<dyn Fn(Json, JobContext) -> BoxFuture<'static, Result<()>> + Send + Sync>;

/// The map from a stored job name back to code that can run it.
///
/// Laravel needs nothing like this: it stores the serialised PHP object and
/// asks the runtime to bring the class back to life. A compiled language cannot
/// resurrect a type from a string — the type may not even be linked into the
/// binary that reads the row — so the application states the connection once,
/// at boot:
///
/// ```ignore
/// let mut jobs = JobRegistry::new();
/// jobs.register::<SendWelcomeEmail>();
/// jobs.register::<GenerateInvoice>();
/// ```
///
/// The cost is one line per job. What is bought is that `queue:work` fails to
/// *compile* against a renamed job type, instead of failing at three in the
/// morning against a class that is no longer there.
#[derive(Default)]
pub struct JobRegistry {
    handlers: HashMap<String, Handler>,
}

impl JobRegistry {
    pub fn new() -> Self {
        JobRegistry::default()
    }

    /// Teach the registry about one job type.
    ///
    /// Registering the same name twice replaces the handler, so a test can
    /// substitute a job without rebuilding the registry.
    pub fn register<J: Job>(&mut self) -> &mut Self {
        self.register_fn_with(J::NAME, |payload, ctx| {
            Box::pin(async move {
                // Rebuilding happens inside the future so a bad payload fails
                // the job the same way a bad `handle` does, with the same
                // retries and the same dead-letter entry.
                let job = J::from_payload(&payload)?;
                job.handle_with(&ctx).await
            })
        })
    }

    /// Register a handler directly, for a name with no Rust type behind it —
    /// a job pushed by another service, or a stub in a test.
    pub fn register_fn<F>(&mut self, name: &str, handler: F) -> &mut Self
    where
        F: Fn(Json) -> BoxFuture<'static, Result<()>> + Send + Sync + 'static,
    {
        let handler = Arc::new(handler);
        self.register_fn_with(name, move |payload, _ctx| handler(payload))
    }

    /// [`register_fn`](Self::register_fn), for a handler that reports progress.
    pub fn register_fn_with<F>(&mut self, name: &str, handler: F) -> &mut Self
    where
        F: Fn(Json, JobContext) -> BoxFuture<'static, Result<()>> + Send + Sync + 'static,
    {
        self.handlers.insert(name.to_string(), Arc::new(handler));
        self
    }

    /// The handler for a name, if one was registered.
    pub fn handler(&self, name: &str) -> Option<Handler> {
        self.handlers.get(name).map(Arc::clone)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Every registered name, sorted — for `queue:work`'s boot log and for the
    /// error message below.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort();
        names
    }

    /// Run a stored job, or explain why it cannot be run.
    ///
    /// The error names what *is* registered: "no handler for `x`" on its own
    /// leaves the reader guessing whether they misspelled the job or forgot
    /// the registration line.
    pub fn run(&self, name: &str, payload: Json) -> BoxFuture<'static, Result<()>> {
        self.run_with(name, payload, JobContext::detached())
    }

    /// [`run`](Self::run), with the context the worker hands a real job.
    pub fn run_with(&self, name: &str, payload: Json, ctx: JobContext) -> BoxFuture<'static, Result<()>> {
        match self.handler(name) {
            Some(handler) => handler(payload, ctx),
            None => {
                let message = format!(
                    "no handler is registered for the job `{name}`. Add \
                     `registry.register::<{name}>()` at boot. Registered: {}",
                    if self.handlers.is_empty() {
                        "nothing yet".to_string()
                    } else {
                        self.names().join(", ")
                    }
                );
                Box::pin(async move { Err(Error::msg(message)) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    static ADDED: AtomicI64 = AtomicI64::new(0);

    struct AddToTotal {
        amount: i64,
    }

    impl Job for AddToTotal {
        const NAME: &'static str = "add-to-total";

        fn payload(&self) -> Json {
            Json::object([("amount", Json::from(self.amount))])
        }

        fn from_payload(payload: &Json) -> Result<Self> {
            let amount = payload
                .get("amount")
                .and_then(Json::as_i64)
                .ok_or_else(|| Error::msg("add-to-total needs an `amount`"))?;
            Ok(AddToTotal { amount })
        }

        fn handle(&self) -> impl Future<Output = Result<()>> + Send {
            let amount = self.amount;
            async move {
                ADDED.fetch_add(amount, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    struct Fussy;

    impl Job for Fussy {
        const NAME: &'static str = "fussy";

        fn payload(&self) -> Json {
            Json::Null
        }

        fn from_payload(_payload: &Json) -> Result<Self> {
            Ok(Fussy)
        }

        /// Spelled as an `async fn`, which the trait also accepts.
        async fn handle(&self) -> Result<()> {
            Ok(())
        }

        fn tries(&self) -> u32 {
            7
        }

        fn retry_after(&self) -> Duration {
            Duration::from_secs(5)
        }

        fn queue(&self) -> &'static str {
            "slow"
        }
    }

    #[test]
    fn a_job_packages_itself_with_its_own_policy() {
        let queued = Fussy.to_queued();

        assert_eq!(queued.name, "fussy");
        assert_eq!(queued.queue, "slow");
        assert_eq!(queued.max_tries, 7);
        assert_eq!(queued.retry_after, Duration::from_secs(5));
        assert_eq!(queued.delay, Duration::ZERO);
    }

    #[test]
    fn the_defaults_apply_to_a_job_that_says_nothing() {
        let queued = AddToTotal { amount: 1 }.to_queued();

        assert_eq!(queued.queue, DEFAULT_QUEUE);
        assert_eq!(queued.max_tries, DEFAULT_TRIES);
        assert_eq!(queued.retry_after, DEFAULT_RETRY_AFTER);
    }

    #[test]
    fn a_job_that_asks_for_zero_tries_still_runs_once() {
        assert_eq!(QueuedJob::new("x", Json::Null).with_tries(0).max_tries, 1);
    }

    #[tokio::test]
    async fn the_registry_runs_a_job_rebuilt_from_its_payload() {
        ADDED.store(0, Ordering::SeqCst);

        let mut registry = JobRegistry::new();
        registry.register::<AddToTotal>();

        let queued = AddToTotal { amount: 41 }.to_queued();
        registry.run(&queued.name, queued.payload).await.unwrap();

        assert_eq!(ADDED.load(Ordering::SeqCst), 41);
    }

    #[tokio::test]
    async fn an_unknown_job_name_names_what_is_registered() {
        let mut registry = JobRegistry::new();
        registry.register::<AddToTotal>();

        let error = registry.run("not-a-job", Json::Null).await.unwrap_err().to_string();

        assert!(error.contains("no handler is registered for the job `not-a-job`"), "{error}");
        assert!(error.contains("add-to-total"), "{error}");
    }

    #[tokio::test]
    async fn an_unreadable_payload_fails_the_job_rather_than_the_worker() {
        let mut registry = JobRegistry::new();
        registry.register::<AddToTotal>();

        let error = registry.run("add-to-total", Json::Null).await.unwrap_err().to_string();
        assert!(error.contains("needs an `amount`"), "{error}");
    }

    #[tokio::test]
    async fn a_handler_can_be_registered_without_a_type_behind_it() {
        let mut registry = JobRegistry::new();
        registry.register_fn("external", |payload| {
            Box::pin(async move {
                assert_eq!(payload.get("from").and_then(Json::as_str), Some("php"));
                Ok(())
            })
        });

        assert!(registry.contains("external"));
        registry
            .run("external", Json::object([("from", Json::from("php"))]))
            .await
            .unwrap();
    }

    #[test]
    fn registering_twice_replaces_rather_than_duplicates() {
        let mut registry = JobRegistry::new();
        registry.register::<AddToTotal>();
        registry.register::<AddToTotal>();

        assert_eq!(registry.len(), 1);
        assert_eq!(registry.names(), vec!["add-to-total"]);
    }

    #[test]
    fn a_reserved_job_knows_when_it_is_out_of_attempts() {
        let job = QueuedJob::new("x", Json::Null).with_tries(3);
        let reserve = |attempts| ReservedJob { id: "1".into(), job: job.clone(), attempts };

        assert!(!reserve(2).is_last_attempt());
        assert!(reserve(3).is_last_attempt());
        assert!(reserve(4).is_last_attempt());
    }

    #[test]
    fn a_failed_job_renders_for_a_dashboard() {
        let failed = FailedJob {
            id: "9".into(),
            name: "add-to-total".into(),
            queue: "default".into(),
            payload: Json::object([("amount", Json::from(2))]),
            attempts: 3,
            error: "boom".into(),
            failed_at: 1_700_000_000,
        };

        assert_eq!(
            failed.to_json().to_string(),
            r#"{"attempts":3,"error":"boom","failed_at":1700000000,"id":"9","name":"add-to-total","payload":{"amount":2},"queue":"default"}"#
        );
    }
}
