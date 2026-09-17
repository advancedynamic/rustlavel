//! Where a job is, as far as somebody outside the worker can tell.
//!
//! A job that takes a minute is a spinner for a minute unless it says how far
//! it has got. The job calls [`JobContext::progress`]; a request handler in
//! the web process reads [`ProgressStore::get`]; the two share a store and
//! nothing else — which is the point, because the worker is usually another
//! process on another machine.
//!
//! Progress is a *hint*. It is written with no transaction and read with no
//! lock, and a job that crashes between two reports leaves the last one
//! standing. The worker closes that gap: it marks the job finished on success
//! and failed on the final attempt, whatever the job last said.

use rustlavel_core::{Json, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub type ProgressFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// One job's position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// 0 to 100. Clamped on the way in, because a job that reports 130% has
    /// a bug and a bar that overflows its box does not help find it.
    pub percent: u8,
    /// What the job is doing now, in words a person can read: "transcribing",
    /// "rendering clip 3 of 7".
    pub stage: String,
    /// Unix seconds.
    pub updated_at: u64,
    /// Set by the worker, not the job, when the job's `handle` returned.
    pub finished: bool,
    /// Set by the worker on the last failed attempt, with the error.
    pub failed: Option<String>,
}

impl Progress {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("percent", Json::from(self.percent as i64)),
            ("stage", Json::from(self.stage.as_str())),
            ("updated_at", Json::from(self.updated_at as i64)),
            ("finished", Json::from(self.finished)),
            ("failed", self.failed.as_deref().map_or(Json::Null, Json::from)),
        ])
    }
}

/// Where progress lives. Keyed by job id, and by chain id for a job that is a
/// step in one.
pub trait ProgressStore: Send + Sync + 'static {
    fn set<'a>(&'a self, key: &'a str, progress: Progress) -> ProgressFuture<'a, ()>;
    fn get<'a>(&'a self, key: &'a str) -> ProgressFuture<'a, Option<Progress>>;
    /// Drop records older than `before` (unix seconds). Housekeeping.
    fn purge<'a>(&'a self, before: u64) -> ProgressFuture<'a, usize>;
}

/// Progress kept in this process. Right for tests and for a worker that runs
/// inside the web process; wrong once the worker is a separate process, which
/// is the arrangement this exists for.
#[derive(Default)]
pub struct MemoryProgress {
    inner: Mutex<HashMap<String, Progress>>,
}

impl MemoryProgress {
    pub fn new() -> MemoryProgress {
        MemoryProgress::default()
    }
}

impl ProgressStore for MemoryProgress {
    fn set<'a>(&'a self, key: &'a str, progress: Progress) -> ProgressFuture<'a, ()> {
        Box::pin(async move {
            self.inner.lock().expect("the progress store is poisoned").insert(key.to_string(), progress);
            Ok(())
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> ProgressFuture<'a, Option<Progress>> {
        Box::pin(async move { Ok(self.inner.lock().expect("the progress store is poisoned").get(key).cloned()) })
    }

    fn purge<'a>(&'a self, before: u64) -> ProgressFuture<'a, usize> {
        Box::pin(async move {
            let mut held = self.inner.lock().expect("the progress store is poisoned");
            let before_len = held.len();
            held.retain(|_, progress| progress.updated_at >= before);
            Ok(before_len - held.len())
        })
    }
}

/// What a running job is handed: who it is, and where to say how far it has
/// got.
#[derive(Clone)]
pub struct JobContext {
    /// The driver's id for this job.
    pub id: String,
    /// The chain this job is a step of, when it is one. Progress reported
    /// here is recorded under the chain as well, so a UI following a pipeline
    /// has one key to watch.
    pub chain: Option<String>,
    store: Option<Arc<dyn ProgressStore>>,
}

impl JobContext {
    pub fn new(id: impl Into<String>, chain: Option<String>, store: Option<Arc<dyn ProgressStore>>) -> JobContext {
        JobContext { id: id.into(), chain, store }
    }

    /// A context for a job run outside the worker — a test calling `handle`
    /// directly. Reports go nowhere.
    pub fn detached() -> JobContext {
        JobContext { id: "detached".into(), chain: None, store: None }
    }

    /// Say how far along the job is.
    ///
    /// Never fails the job. A progress store that is down should cost the
    /// user a stale bar, not a failed render — so the error is logged and
    /// swallowed here rather than returned.
    pub async fn progress(&self, percent: u8, stage: impl Into<String>) {
        let Some(store) = &self.store else { return };
        let progress = Progress {
            percent: percent.min(100),
            stage: stage.into(),
            updated_at: crate::time::unix_now().max(0) as u64,
            finished: false,
            failed: None,
        };
        for key in self.keys() {
            if let Err(error) = store.set(&key, progress.clone()).await {
                rustlavel_core::warn!("queue: could not record progress for {key}: {error}");
            }
        }
    }

    /// The job's own key, and the chain's when there is one.
    pub fn keys(&self) -> Vec<String> {
        let mut keys = vec![self.id.clone()];
        if let Some(chain) = &self.chain {
            keys.push(format!("chain:{chain}"));
        }
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn progress_is_readable_by_job_id_and_by_chain() {
        let store: Arc<dyn ProgressStore> = Arc::new(MemoryProgress::new());
        let ctx = JobContext::new("job-1", Some("chain-9".into()), Some(Arc::clone(&store)));

        ctx.progress(40, "transcribing").await;

        let by_job = store.get("job-1").await.unwrap().expect("recorded");
        assert_eq!(by_job.percent, 40);
        assert_eq!(by_job.stage, "transcribing");
        assert!(!by_job.finished);

        let by_chain = store.get("chain:chain-9").await.unwrap().expect("recorded under the chain too");
        assert_eq!(by_chain.percent, 40);
    }

    /// 130% is a bug in the job, and a bar that overflows its box does not
    /// help anybody find it.
    #[tokio::test]
    async fn percent_is_clamped() {
        let store: Arc<dyn ProgressStore> = Arc::new(MemoryProgress::new());
        let ctx = JobContext::new("j", None, Some(Arc::clone(&store)));
        ctx.progress(130, "x").await;
        assert_eq!(store.get("j").await.unwrap().unwrap().percent, 100);
    }

    /// A job run by hand in a test has nowhere to report and must not care.
    #[tokio::test]
    async fn a_detached_context_swallows_reports() {
        JobContext::detached().progress(50, "fine").await;
    }
}
