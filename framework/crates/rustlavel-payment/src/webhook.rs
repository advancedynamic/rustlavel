//! What a gateway says happened, and the record that stops it being heard twice.

use crate::charge::Charge;
use crate::transfer::Transfer;
use rustlavel_core::{Json, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    ChargePaid,
    ChargeExpired,
    ChargeFailed,
    TransferCompleted,
    TransferFailed,
    /// A kind this crate does not model. Stored and handed on unchanged, so a
    /// gateway adding an event type does not make the receiver drop it on the
    /// floor — and does not make it something it is not.
    Other(String),
}

impl EventKind {
    pub fn as_str(&self) -> &str {
        match self {
            EventKind::ChargePaid => "charge.paid",
            EventKind::ChargeExpired => "charge.expired",
            EventKind::ChargeFailed => "charge.failed",
            EventKind::TransferCompleted => "transfer.completed",
            EventKind::TransferFailed => "transfer.failed",
            EventKind::Other(name) => name,
        }
    }
}

/// A verified event.
///
/// Built by a driver's `verify_webhook`, which is the only constructor that
/// makes sense: an event that has not had its signature checked is a string
/// somebody posted.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookEvent {
    /// The gateway's id for *the event*, or for the charge when the gateway
    /// gives events no id of their own. Deduplication keys on this together
    /// with the kind, so "charge X paid" and "charge X expired" are two events.
    pub id: String,
    pub kind: EventKind,
    pub charge: Option<Charge>,
    pub transfer: Option<Transfer>,
    /// The body exactly as received, for storage and for replaying to a
    /// handler that was broken at the time.
    pub raw: Json,
}

impl WebhookEvent {
    /// The key a duplicate is recognised by.
    pub fn dedup_key(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }
}

pub type LogFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Where received events are recorded.
///
/// **`record` must be atomic**: two deliveries of one event racing each other
/// must see exactly one `true`. A read followed by a write leaves a window in
/// which both do, and both credit the customer. The database implementation
/// does it with a unique index and an insert; the memory one with a mutex.
pub trait WebhookLog: Send + Sync + 'static {
    /// Record the event, returning whether this call was the first to. `false`
    /// means it has been seen: hand it back as handled and do nothing else.
    fn record<'a>(&'a self, event: &'a WebhookEvent) -> LogFuture<'a, bool>;

    /// The raw body of a recorded event, for replay.
    fn raw<'a>(&'a self, dedup_key: &'a str) -> LogFuture<'a, Option<Json>>;

    /// Forget an event whose handler failed, so the gateway's retry is
    /// processed rather than dropped as a duplicate of something nothing
    /// handled. Without this, a handler that crashed once would have crashed
    /// forever as far as that payment is concerned.
    fn withdraw<'a>(&'a self, dedup_key: &'a str) -> LogFuture<'a, ()>;
}

/// Events held in this process's memory. Right for tests; wrong for a
/// deployment, where a duplicate that arrives after a restart is still a
/// duplicate.
#[derive(Default)]
pub struct MemoryWebhookLog {
    seen: Mutex<HashMap<String, Json>>,
}

impl MemoryWebhookLog {
    pub fn new() -> MemoryWebhookLog {
        MemoryWebhookLog::default()
    }

    pub fn len(&self) -> usize {
        self.seen.lock().expect("the webhook log is poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl WebhookLog for MemoryWebhookLog {
    fn record<'a>(&'a self, event: &'a WebhookEvent) -> LogFuture<'a, bool> {
        Box::pin(async move {
            let mut seen = self.seen.lock().expect("the webhook log is poisoned");
            // One critical section for the check and the insert — the whole
            // of what "atomic" means here.
            if seen.contains_key(&event.dedup_key()) {
                return Ok(false);
            }
            seen.insert(event.dedup_key(), event.raw.clone());
            Ok(true)
        })
    }

    fn raw<'a>(&'a self, dedup_key: &'a str) -> LogFuture<'a, Option<Json>> {
        Box::pin(async move {
            Ok(self.seen.lock().expect("the webhook log is poisoned").get(dedup_key).cloned())
        })
    }

    fn withdraw<'a>(&'a self, dedup_key: &'a str) -> LogFuture<'a, ()> {
        Box::pin(async move {
            self.seen.lock().expect("the webhook log is poisoned").remove(dedup_key);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, kind: EventKind) -> WebhookEvent {
        WebhookEvent { id: id.into(), kind, charge: None, transfer: None, raw: Json::from(id) }
    }

    #[tokio::test]
    async fn the_same_event_is_recorded_once() {
        let log = MemoryWebhookLog::new();
        assert!(log.record(&event("ch_1", EventKind::ChargePaid)).await.unwrap());
        assert!(!log.record(&event("ch_1", EventKind::ChargePaid)).await.unwrap(), "a duplicate was accepted");
        assert_eq!(log.len(), 1);
    }

    /// "charge X paid" and "charge X expired" are two events, and a gateway
    /// that sends both must not have the second dropped as a duplicate.
    #[tokio::test]
    async fn the_same_id_under_another_kind_is_another_event() {
        let log = MemoryWebhookLog::new();
        assert!(log.record(&event("ch_1", EventKind::ChargePaid)).await.unwrap());
        assert!(log.record(&event("ch_1", EventKind::ChargeExpired)).await.unwrap());
    }

    /// A handler that crashed once must not have crashed forever as far as
    /// that payment is concerned.
    #[tokio::test]
    async fn a_withdrawn_event_can_be_recorded_again() {
        let log = MemoryWebhookLog::new();
        let e = event("ch_1", EventKind::ChargePaid);
        assert!(log.record(&e).await.unwrap());
        log.withdraw(&e.dedup_key()).await.unwrap();
        assert!(log.record(&e).await.unwrap(), "the retry was dropped as a duplicate");
    }

    #[tokio::test]
    async fn the_raw_body_is_kept_for_replay() {
        let log = MemoryWebhookLog::new();
        let e = event("ch_9", EventKind::ChargePaid);
        log.record(&e).await.unwrap();
        assert_eq!(log.raw(&e.dedup_key()).await.unwrap(), Some(Json::from("ch_9")));
        assert_eq!(log.raw("charge.paid:nothing").await.unwrap(), None);
    }
}
