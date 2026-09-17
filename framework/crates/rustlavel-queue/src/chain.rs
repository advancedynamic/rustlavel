//! Jobs that run one after another.
//!
//! A pipeline — transcribe, then detect the moments, then render each — is a
//! chain: the next step is dispatched when the one before it succeeds, and a
//! step that fails for good ends the chain there. The steps are ordinary jobs;
//! nothing about a job knows it is in a chain except the context it is handed,
//! which carries the chain's id so progress can be reported against the whole.
//!
//! ```ignore
//! queue.dispatch_chain(
//!     Chain::new(Transcribe { video })
//!         .then(DetectMoments { video })
//!         .then(RenderClips { video }),
//! ).await?;
//! ```
//!
//! **A failed step stops the chain**, and the remaining steps go to the
//! dead-letter table with it, so what did not run is visible rather than
//! silently gone. Retries happen within the step, with the step's own policy;
//! the chain only advances on success.

use crate::job::{Job, QueuedJob};

/// A sequence of jobs.
#[derive(Debug, Clone, PartialEq)]
pub struct Chain {
    /// A name for the whole, so progress for the pipeline has one key.
    /// Generated when not given.
    pub id: String,
    steps: Vec<QueuedJob>,
}

impl Chain {
    /// Start a chain with its first step.
    pub fn new<J: Job>(first: J) -> Chain {
        Chain { id: chain_id(), steps: vec![first.to_queued()] }
    }

    /// Name the chain yourself — an order id, a video id — so the key a UI
    /// watches is one it already knows.
    pub fn named(mut self, id: impl Into<String>) -> Chain {
        self.id = id.into();
        self
    }

    pub fn then<J: Job>(mut self, next: J) -> Chain {
        self.steps.push(next.to_queued());
        self
    }

    /// Add a step that is already an envelope, for a job with no Rust type.
    pub fn then_queued(mut self, next: QueuedJob) -> Chain {
        self.steps.push(next);
        self
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The first step, carrying the rest and the chain's id. This is what is
    /// pushed; the worker peels one step off each time one succeeds.
    pub fn into_queued(mut self) -> QueuedJob {
        let mut first = self.steps.remove(0);
        first.chain = Some(self.id);
        first.then = self.steps;
        first
    }
}

fn chain_id() -> String {
    // Time and a counter: unique within a process and very likely across
    // them, and readable in a log. Not a secret, so not from the CSPRNG.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:x}-{count:x}", crate::time::unix_now())
}

impl QueuedJob {
    /// The job to push once this one has succeeded, if this one is a step in
    /// a chain with more to come. The returned job carries the remainder.
    pub fn next_in_chain(&self) -> Option<QueuedJob> {
        let mut rest = self.then.clone();
        if rest.is_empty() {
            return None;
        }
        let mut next = rest.remove(0);
        next.chain = self.chain.clone();
        next.then = rest;
        Some(next)
    }
}
