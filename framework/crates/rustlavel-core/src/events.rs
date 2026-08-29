//! The internal instrumentation bus.
//!
//! Every part of the framework reports what it does here; packages listen.
//! Telescope, structured logging, and tracing exporters are all just
//! subscribers, which is why nothing in core needs to know they exist.
//!
//! Events carry an open field map rather than a fixed enum so a package can
//! record something core has never heard of.

use crate::json::Json;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

/// One recorded moment: a request, a query, a dispatched job, an exception.
#[derive(Debug, Clone)]
pub struct Event {
    /// A dotted, stable identifier: `http.request`, `db.query`, `ai.call`.
    pub kind: &'static str,
    pub at: SystemTime,
    /// How long the recorded work took, when it was a span rather than a point.
    pub duration: Option<Duration>,
    pub fields: BTreeMap<String, Json>,
}

impl Event {
    pub fn new(kind: &'static str) -> Self {
        Event { kind, at: SystemTime::now(), duration: None, fields: BTreeMap::new() }
    }

    pub fn with(mut self, key: &str, value: impl Into<Json>) -> Self {
        self.fields.insert(key.to_string(), value.into());
        self
    }

    pub fn took(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    pub fn field(&self, key: &str) -> Option<&Json> {
        self.fields.get(key)
    }

    /// Milliseconds elapsed, for renderers that show a duration column.
    pub fn duration_ms(&self) -> Option<f64> {
        self.duration.map(|d| d.as_secs_f64() * 1000.0)
    }

    /// Publish this event to every subscriber.
    pub fn dispatch(self) {
        dispatch(self);
    }
}

/// Anything that wants to observe framework activity.
pub trait Subscriber: Send + Sync + 'static {
    fn handle(&self, event: &Event);

    /// Return false to skip events a subscriber does not care about, before
    /// the event is cloned or formatted.
    fn interested_in(&self, _kind: &str) -> bool {
        true
    }
}

impl<F> Subscriber for F
where
    F: Fn(&Event) + Send + Sync + 'static,
{
    fn handle(&self, event: &Event) {
        self(event)
    }
}

type Subscribers = RwLock<Vec<Arc<dyn Subscriber>>>;

fn registry() -> &'static Subscribers {
    static REGISTRY: OnceLock<Subscribers> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a subscriber for the lifetime of the process.
pub fn subscribe(subscriber: impl Subscriber) {
    registry().write().expect("event registry poisoned").push(Arc::new(subscriber));
}

/// Publish an event to every interested subscriber.
///
/// Dispatch is synchronous and best-effort: a subscriber that needs to do real
/// work (writing to a database, shipping over the network) is expected to queue
/// it, so instrumentation never slows down a request.
pub fn dispatch(event: Event) {
    let subscribers = registry().read().expect("event registry poisoned");
    for subscriber in subscribers.iter() {
        if subscriber.interested_in(event.kind) {
            subscriber.handle(&event);
        }
    }
}

/// True when at least one subscriber is listening, so callers can skip building
/// an expensive event payload nobody will read.
pub fn has_subscribers() -> bool {
    !registry().read().expect("event registry poisoned").is_empty()
}

/// Remove every subscriber. Intended for tests.
pub fn clear_subscribers() {
    registry().write().expect("event registry poisoned").clear();
}

/// Time a block and dispatch an event with its duration.
pub fn timed<T>(kind: &'static str, fields: impl FnOnce() -> Vec<(String, Json)>, work: impl FnOnce() -> T) -> T {
    if !has_subscribers() {
        return work();
    }
    let started = std::time::Instant::now();
    let result = work();
    let elapsed = started.elapsed();

    let mut event = Event::new(kind).took(elapsed);
    for (key, value) in fields() {
        event.fields.insert(key, value);
    }
    dispatch(event);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn subscribers_receive_dispatched_events() {
        clear_subscribers();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);

        subscribe(move |event: &Event| {
            assert_eq!(event.kind, "http.request");
            assert_eq!(event.field("path").and_then(Json::as_str), Some("/users"));
            counter.fetch_add(1, Ordering::SeqCst);
        });

        Event::new("http.request").with("path", "/users").dispatch();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        clear_subscribers();
    }

    #[test]
    fn timed_records_a_duration() {
        clear_subscribers();
        let millis = Arc::new(RwLock::new(None));
        let sink = Arc::clone(&millis);

        subscribe(move |event: &Event| {
            *sink.write().unwrap() = event.duration_ms();
        });

        let result = timed("db.query", || vec![("sql".to_string(), Json::from("select 1"))], || 21 * 2);

        assert_eq!(result, 42);
        assert!(millis.read().unwrap().is_some());
        clear_subscribers();
    }
}
