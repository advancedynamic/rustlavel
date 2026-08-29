//! The subscriber that turns framework events into entries.
//!
//! Everything here runs inline on the thread that dispatched the event — which
//! is the request's own thread — so the rule is simple: do no work that can
//! block. Capturing is a redaction pass, a short mutex and a push; persistence
//! is a send into a queue that a writer thread drains. There is no `await`, no
//! socket and no `fsync` anywhere on this path.

use crate::journal::Journal;
use crate::store::Store;
use rustlavel_core::events::{Event, Subscriber};
use rustlavel_core::Json;

/// Records events into a [`Store`], optionally mirroring them to a journal.
pub struct Recorder {
    store: Store,
    journal: Option<Journal>,
    /// Kinds an application asked to be left out — a chatty package, or `log`
    /// when someone only wants queries.
    ignored: Vec<String>,
    /// The dashboard's own mount point. Telescope watching itself would fill
    /// the buffer with its own polling within seconds.
    mount: Option<String>,
}

impl Recorder {
    pub fn new(store: Store) -> Self {
        Recorder { store, journal: None, ignored: Vec::new(), mount: None }
    }

    pub fn with_journal(mut self, journal: Option<Journal>) -> Self {
        self.journal = journal;
        self
    }

    pub fn ignoring(mut self, kinds: Vec<String>) -> Self {
        self.ignored = kinds;
        self
    }

    /// Drop `http.request` events for the dashboard's own routes.
    pub fn skipping_path(mut self, mount: impl Into<String>) -> Self {
        self.mount = Some(mount.into());
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Whether this event is Telescope looking at itself.
    fn is_own_traffic(&self, event: &Event) -> bool {
        let Some(mount) = &self.mount else { return false };
        if event.kind != "http.request" {
            return false;
        }
        event.field("path").and_then(Json::as_str).is_some_and(|path| path.starts_with(mount.as_str()))
    }
}

impl Subscriber for Recorder {
    /// Rejecting by kind here means the event is never even inspected, which is
    /// what the bus offers this hook for.
    fn interested_in(&self, kind: &str) -> bool {
        !self.ignored.iter().any(|ignored| ignored == kind)
    }

    fn handle(&self, event: &Event) {
        if self.is_own_traffic(event) {
            return;
        }
        let entry = self.store.record(event);
        let Some(journal) = &self.journal else { return };

        journal.append(&entry);

        // A request only learns which queries and log lines belong to it when
        // it finishes, by which time those entries have already been written
        // ungrouped. Writing them again with their grouping filled in is what
        // keeps the "recorded during this request" panel working across a
        // restart; the loader keeps the last line for an id.
        if entry.kind == "http.request" {
            for claimed in self.store.related(entry.id) {
                journal.append(&claimed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Filter;
    use rustlavel_core::events;

    #[test]
    fn a_dispatched_event_is_captured() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let store = Store::new();
        events::subscribe(Recorder::new(store.clone()));
        Event::new("http.request").with("method", "GET").with("path", "/users").dispatch();

        let entries = store.entries(&Filter::default());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].summary(), "GET /users");

        events::clear_subscribers();
    }

    #[test]
    fn the_oldest_entry_is_dropped_once_the_buffer_is_full() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let store = Store::with_capacity(2);
        events::subscribe(Recorder::new(store.clone()));
        for n in 1..=4 {
            Event::new("log").with("message", format!("line {n}")).dispatch();
        }

        let entries = store.entries(&Filter::default());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].summary(), "line 4");
        assert_eq!(entries[1].summary(), "line 3");

        events::clear_subscribers();
    }

    #[test]
    fn credentials_in_a_dispatched_event_are_redacted_before_storage() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let store = Store::new();
        events::subscribe(Recorder::new(store.clone()));
        Event::new("http.request")
            .with("path", "/login")
            .with("password", "hunter2")
            .with("authorization", "Bearer sk-live-abc")
            .dispatch();

        let rendered = store.to_json(&Filter::default()).to_string();
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains("sk-live-abc"));
        assert!(rendered.contains("[redacted]"));
        assert!(rendered.contains("/login"));

        events::clear_subscribers();
    }

    #[test]
    fn an_ignored_kind_is_never_inspected() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let store = Store::new();
        events::subscribe(Recorder::new(store.clone()).ignoring(vec!["log".to_string()]));
        Event::new("log").with("message", "noise").dispatch();
        Event::new("db.query").with("sql", "select 1").dispatch();

        assert_eq!(store.len(), 1);
        assert_eq!(store.entries(&Filter::default())[0].kind, "db.query");

        events::clear_subscribers();
    }

    #[test]
    fn telescope_does_not_record_its_own_dashboard_traffic() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let store = Store::new();
        events::subscribe(Recorder::new(store.clone()).skipping_path("/telescope"));
        Event::new("http.request").with("path", "/telescope/api/entries").dispatch();
        Event::new("http.request").with("path", "/users").dispatch();

        assert_eq!(store.len(), 1);
        assert_eq!(store.entries(&Filter::default())[0].summary(), "GET /users");

        events::clear_subscribers();
    }

    #[test]
    fn a_log_line_is_captured_through_the_logging_facade() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let store = Store::new();
        events::subscribe(Recorder::new(store.clone()));
        rustlavel_core::log::log(rustlavel_core::log::Level::Warn, "disk is filling up");

        let entries = store.entries(&Filter::default());
        assert_eq!(entries[0].kind, "log");
        assert_eq!(entries[0].badge().as_deref(), Some("WARN"));
        assert_eq!(entries[0].summary(), "disk is filling up");

        events::clear_subscribers();
    }
}
