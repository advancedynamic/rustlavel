//! The ring buffer everything Telescope knows lives in.
//!
//! Recording happens on the request's own thread, inside the event dispatch, so
//! the cost of an entry is a short mutex, a push, and possibly a pop — no
//! allocation of a database handle, no I/O, no `await`. The buffer is bounded,
//! so a process that runs for a week under load uses exactly as much memory as
//! one that just booted.

use crate::entry::Entry;
use rustlavel_core::{Event, Json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

/// How many entries a store keeps when nothing says otherwise.
pub const DEFAULT_CAPACITY: usize = 500;

/// A handle onto the recorded entries. Cloning shares one buffer, so the
/// subscriber, the dashboard handlers and the application all see the same
/// history.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    entries: VecDeque<Entry>,
    capacity: usize,
    next_id: u64,
    /// Entries recorded since the last request finished, waiting to be claimed
    /// by whichever `http.request` reports next.
    pending: Vec<u64>,
}

/// What the API asks the store for.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Only this kind, when set.
    pub kind: Option<String>,
    /// Only entries newer than this id, so the page can poll incrementally.
    pub after: Option<u64>,
    /// At most this many, newest first.
    pub limit: Option<usize>,
}

impl Store {
    pub fn new() -> Self {
        Store::with_capacity(DEFAULT_CAPACITY)
    }

    /// A capacity of zero would make every recording a no-op and every page
    /// empty, which reads as a bug rather than a setting, so it is clamped.
    pub fn with_capacity(capacity: usize) -> Self {
        Store {
            inner: Arc::new(Mutex::new(Inner {
                entries: VecDeque::new(),
                capacity: capacity.max(1),
                next_id: 1,
                pending: Vec::new(),
            })),
        }
    }

    /// A poisoned lock means some other thread panicked while recording. That
    /// is not a reason to take the application down with it: a debugging tool
    /// recovers and keeps going.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Capture an event. Returns the stored entry so a caller can persist it
    /// without taking the lock a second time.
    pub fn record(&self, event: &Event) -> Entry {
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;

        let entry = Entry::from_event(id, event);
        inner.push(entry);

        if event.kind == "http.request" {
            let window = event.duration.unwrap_or(Duration::ZERO);
            inner.claim(id, event.at.checked_sub(window).unwrap_or(event.at));
        } else {
            inner.pending.push(id);
            // Pending can never outgrow the buffer that holds its entries.
            let overflow = inner.pending.len().saturating_sub(inner.capacity);
            inner.pending.drain(..overflow);
        }

        inner.entries.back().cloned().expect("the entry just pushed is present")
    }

    /// Insert entries recovered from the journal, keeping ids monotonic so a
    /// restart cannot hand out an id the page has already seen.
    pub fn restore(&self, entries: Vec<Entry>) {
        let mut inner = self.lock();
        for entry in entries {
            inner.next_id = inner.next_id.max(entry.id + 1);
            inner.push(entry);
        }
    }

    /// Entries matching a filter, newest first.
    pub fn entries(&self, filter: &Filter) -> Vec<Entry> {
        let inner = self.lock();
        let limit = filter.limit.unwrap_or(usize::MAX);
        inner
            .entries
            .iter()
            .rev()
            .filter(|entry| filter.kind.as_ref().is_none_or(|kind| &entry.kind == kind))
            .filter(|entry| filter.after.is_none_or(|after| entry.id > after))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn get(&self, id: u64) -> Option<Entry> {
        self.lock().entries.iter().find(|entry| entry.id == id).cloned()
    }

    /// The other entries recorded during the same request, oldest first — the
    /// queries and log lines that belong with it.
    pub fn related(&self, id: u64) -> Vec<Entry> {
        let inner = self.lock();
        let Some(group) = inner.entries.iter().find(|entry| entry.id == id).and_then(|e| e.group)
        else {
            return Vec::new();
        };
        inner
            .entries
            .iter()
            .filter(|entry| entry.group == Some(group) && entry.id != id)
            .cloned()
            .collect()
    }

    /// Every kind currently held, with counts, for the filter bar.
    pub fn kinds(&self) -> Vec<(String, usize)> {
        let inner = self.lock();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &inner.entries {
            *counts.entry(entry.kind.clone()).or_default() += 1;
        }
        counts.into_iter().collect()
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.lock().capacity
    }

    /// Forget everything. Ids keep counting up, so a page polling with
    /// `?after=` does not start seeing recycled entries.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.entries.clear();
        inner.pending.clear();
    }

    /// The payload the listing endpoint returns: the entries plus the little
    /// bit of context the page needs to render its chrome.
    pub fn to_json(&self, filter: &Filter) -> Json {
        let entries: Vec<Json> = self.entries(filter).iter().map(Entry::to_json).collect();
        let kinds: Vec<Json> = self
            .kinds()
            .into_iter()
            .map(|(kind, count)| Json::object([("kind", Json::from(kind)), ("count", Json::from(count))]))
            .collect();

        Json::object([
            ("entries", Json::Array(entries)),
            ("kinds", Json::Array(kinds)),
            ("total", Json::from(self.len())),
            ("capacity", Json::from(self.capacity())),
        ])
    }
}

impl Default for Store {
    fn default() -> Self {
        Store::new()
    }
}

impl Inner {
    fn push(&mut self, entry: Entry) {
        self.entries.push_back(entry);
        while self.entries.len() > self.capacity {
            // The oldest entry leaves; anything still waiting to be grouped
            // with it leaves the pending list too.
            if let Some(evicted) = self.entries.pop_front() {
                self.pending.retain(|id| *id != evicted.id);
            }
        }
    }

    /// Attach the entries a finished request is responsible for.
    ///
    /// When the emitter supplied a correlation id this is exact. It usually has
    /// not: the event bus carries no request context, and `http.request` is
    /// dispatched *after* the handler returns, so the queries and logs of a
    /// request arrive before the request itself does. The fallback is therefore
    /// a heuristic — every ungrouped entry recorded inside the request's own
    /// duration window is assumed to belong to it. That is exact for a dev
    /// server answering one request at a time, which is what Telescope is for,
    /// and approximate under concurrency: two overlapping requests can trade an
    /// entry. Entries older than the window are left pending rather than
    /// discarded, so a slower request still in flight can claim its own.
    fn claim(&mut self, request_id: u64, window_start: SystemTime) {
        let key = self
            .entries
            .iter()
            .find(|entry| entry.id == request_id)
            .and_then(|entry| entry.request_key.clone());

        let pending = std::mem::take(&mut self.pending);
        let mut unclaimed = Vec::new();

        for entry in self.entries.iter_mut() {
            if entry.id == request_id {
                entry.group = Some(request_id);
                continue;
            }
            if entry.group.is_some() || !pending.contains(&entry.id) {
                continue;
            }

            let mine = match key.as_deref() {
                Some(key) => entry.request_key.as_deref() == Some(key),
                None => entry.at >= window_start,
            };
            if mine {
                entry.group = Some(request_id);
            } else {
                unclaimed.push(entry.id);
            }
        }

        self.pending = unclaimed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn event(kind: &'static str) -> Event {
        Event::new(kind)
    }

    #[test]
    fn recording_assigns_monotonic_ids() {
        let store = Store::new();

        assert_eq!(store.record(&event("log")).id, 1);
        assert_eq!(store.record(&event("log")).id, 2);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn the_oldest_entry_is_evicted_past_capacity() {
        let store = Store::with_capacity(3);
        for _ in 0..5 {
            store.record(&event("log"));
        }

        let ids: Vec<u64> = store.entries(&Filter::default()).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![5, 4, 3]);
        assert_eq!(store.len(), 3);
        assert!(store.get(1).is_none());
    }

    #[test]
    fn entries_come_back_newest_first() {
        let store = Store::new();
        store.record(&event("log"));
        store.record(&event("db.query"));

        let ids: Vec<u64> = store.entries(&Filter::default()).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![2, 1]);
    }

    #[test]
    fn filtering_by_kind_keeps_only_that_kind() {
        let store = Store::new();
        store.record(&event("log"));
        store.record(&event("db.query"));
        store.record(&event("log"));

        let filter = Filter { kind: Some("log".into()), ..Filter::default() };
        let found = store.entries(&filter);

        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|entry| entry.kind == "log"));
    }

    #[test]
    fn filtering_after_an_id_returns_only_newer_entries() {
        let store = Store::new();
        store.record(&event("log"));
        store.record(&event("log"));
        store.record(&event("log"));

        let filter = Filter { after: Some(2), ..Filter::default() };
        let ids: Vec<u64> = store.entries(&filter).iter().map(|e| e.id).collect();

        assert_eq!(ids, vec![3]);
    }

    #[test]
    fn a_limit_caps_the_result() {
        let store = Store::new();
        for _ in 0..10 {
            store.record(&event("log"));
        }

        let filter = Filter { limit: Some(4), ..Filter::default() };
        assert_eq!(store.entries(&filter).len(), 4);
    }

    #[test]
    fn a_request_claims_the_queries_recorded_during_it() {
        let store = Store::new();
        store.record(&event("db.query"));
        store.record(&event("log"));
        // The request event reports last, covering a window that includes both.
        let request = store.record(&event("http.request").took(Duration::from_secs(5)));

        let related: Vec<String> = store.related(request.id).iter().map(|e| e.kind.clone()).collect();
        assert_eq!(related, vec!["db.query".to_string(), "log".to_string()]);
        assert_eq!(store.get(1).unwrap().group, Some(request.id));
    }

    #[test]
    fn an_entry_older_than_the_window_is_left_for_a_later_request() {
        let store = Store::new();
        let mut old = Event::new("log");
        old.at = SystemTime::now() - Duration::from_secs(60);
        store.record(&old);

        // A request that took a millisecond cannot have caused a minute-old log.
        let request = store.record(&event("http.request").took(Duration::from_millis(1)));

        assert!(store.related(request.id).is_empty());
        assert_eq!(store.get(1).unwrap().group, None);
    }

    #[test]
    fn an_explicit_request_id_groups_exactly() {
        let store = Store::new();
        store.record(&event("db.query").with("request_id", "a"));
        store.record(&event("db.query").with("request_id", "b"));
        let request =
            store.record(&event("http.request").with("request_id", "a").took(Duration::from_secs(5)));

        let related = store.related(request.id);
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].fields["request_id"].as_str(), Some("a"));
    }

    #[test]
    fn kinds_are_counted_for_the_filter_bar() {
        let store = Store::new();
        store.record(&event("log"));
        store.record(&event("log"));
        store.record(&event("db.query"));

        assert_eq!(store.kinds(), vec![("db.query".into(), 1), ("log".into(), 2)]);
    }

    #[test]
    fn clearing_empties_the_buffer_without_reusing_ids() {
        let store = Store::new();
        store.record(&event("log"));
        store.clear();

        assert!(store.is_empty());
        assert_eq!(store.record(&event("log")).id, 2);
    }

    #[test]
    fn restoring_keeps_ids_ahead_of_what_was_loaded() {
        let store = Store::new();
        store.restore(vec![Entry::from_event(41, &event("log"))]);

        assert_eq!(store.record(&event("log")).id, 42);
    }

    #[test]
    fn a_zero_capacity_still_keeps_one_entry() {
        let store = Store::with_capacity(0);
        store.record(&event("log"));

        assert_eq!(store.capacity(), 1);
        assert_eq!(store.len(), 1);
    }
}
