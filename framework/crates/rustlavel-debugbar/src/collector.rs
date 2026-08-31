//! Gathering what one request did, and only that request.
//!
//! The instrumentation bus is global: every query in the process dispatches to
//! the same subscribers. A debug bar needs the opposite — the queries *this*
//! request made, so the number on the page belongs to the page.
//!
//! A task-local does that. Every request is a task, everything the handler
//! awaits stays on it, and so a collector installed for the task sees that
//! request's events and nobody else's. Work moved off the task with
//! `tokio::spawn` is not collected, which is the honest limit of the approach
//! and is stated on the bar rather than hidden.

use rustlavel_core::events::Event;
use rustlavel_core::Json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

tokio::task_local! {
    /// The collector for the request running on this task, if the bar is on.
    static CURRENT: Arc<Mutex<Collected>>;
}

/// One thing that happened, in the order it happened.
#[derive(Debug, Clone)]
pub struct Timing {
    pub kind: String,
    /// The line shown on the bar — SQL, a cache key, a model name.
    pub label: String,
    pub duration: Option<Duration>,
    /// Everything else the event carried, already redacted.
    pub fields: Vec<(String, String)>,
}

/// Everything collected for one request.
#[derive(Debug, Default, Clone)]
pub struct Collected {
    pub timings: Vec<Timing>,
    pub started: Option<Instant>,
}

impl Collected {
    /// Events of one kind, in order.
    pub fn of(&self, kind: &str) -> Vec<&Timing> {
        self.timings.iter().filter(|timing| timing.kind == kind).collect()
    }

    pub fn total(&self, kind: &str) -> Duration {
        self.of(kind).iter().filter_map(|timing| timing.duration).sum()
    }

    /// Queries whose text repeats, and how many times.
    ///
    /// This is the N+1 detector, and it is the reason a bar is worth more than
    /// a total. "31 queries, 12ms" looks fine; "the same query 30 times" is a
    /// missing eager load, and only the second one tells you what to do.
    pub fn repeated_queries(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();

        for query in self.of("db.query") {
            match counts.iter_mut().find(|(sql, _)| *sql == query.label) {
                Some((_, count)) => *count += 1,
                None => counts.push((query.label.clone(), 1)),
            }
        }

        counts.retain(|(_, count)| *count > 1);
        // Most repeated first: that is the one worth fixing.
        counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        counts
    }
}

/// Installs itself on the bus and routes events to whichever request is running.
pub struct Collector;

impl Collector {
    /// Subscribe once, for the life of the process.
    ///
    /// Every dispatched event reaches this, and it drops the ones arriving on a
    /// task with no bar — a background job, a queue worker, anything that is not
    /// a request being rendered.
    ///
    /// Idempotent, and it has to be. Subscribing twice does not collect twice
    /// as carefully; it records every query twice, and the bar then reports a
    /// page making double the queries it makes. A plugin registered in two
    /// places, or a test suite calling this per test, would both do it.
    pub fn install() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
        rustlavel_core::events::subscribe(|event: &Event| {
            let _ = CURRENT.try_with(|collected| {
                // A poisoned lock means a panic happened mid-push. Losing one
                // row of a debug bar is not worth a second panic on top.
                if let Ok(mut collected) = collected.lock() {
                    collected.timings.push(timing_from(event));
                }
            });
        });
        });
    }

    /// Run `work` with a collector installed, and return what it gathered.
    pub async fn collect<F, T>(work: F) -> (T, Collected)
    where
        F: std::future::Future<Output = T>,
    {
        let collected = Arc::new(Mutex::new(Collected {
            timings: Vec::new(),
            started: Some(Instant::now()),
        }));
        let handle = Arc::clone(&collected);

        let result = CURRENT.scope(collected, work).await;
        let gathered = handle
            .lock()
            .map(|inner| inner.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone());
        (result, gathered)
    }
}

/// The fields worth showing for a kind of event, in the order to show them.
fn label_for(event: &Event) -> String {
    let field = |name: &str| event.field(name).and_then(Json::as_str).unwrap_or("").to_string();

    match event.kind {
        "db.query" => field("sql"),
        "cache.hit" | "cache.miss" => field("key"),
        "http.client" => format!("{} {}", field("method"), field("url")),
        "mail.sent" => field("to"),
        "ai.call" => format!("{} {}", field("provider"), field("model")),
        "mcp.call" => field("tool"),
        "queue.pushed" | "queue.processed" | "queue.failed" => field("job"),
        _ => event
            .fields
            .iter()
            .next()
            .map(|(key, value)| format!("{key}={}", brief(value)))
            .unwrap_or_default(),
    }
}

fn timing_from(event: &Event) -> Timing {
    Timing {
        kind: event.kind.to_string(),
        label: label_for(event),
        duration: event.duration,
        fields: event
            .fields
            .iter()
            .filter(|(key, _)| !is_sensitive(key))
            .map(|(key, value)| (key.clone(), brief(value)))
            .collect(),
    }
}

/// A field name that must never reach the page.
///
/// The bar is written into HTML, which lands in a browser, a screenshot and a
/// bug report. Query bindings are already withheld in production by the
/// database package; this is the second line, and it is deliberately blunt
/// rather than clever.
fn is_sensitive(key: &str) -> bool {
    const MARKERS: &[&str] = &[
        "password", "passwd", "secret", "token", "authorization", "api_key", "apikey",
        "api-key", "credential", "private_key", "session", "cookie",
    ];
    let lowered = key.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lowered.contains(marker))
}

/// A value short enough for one row of a bar.
fn brief(value: &Json) -> String {
    let text = match value {
        Json::String(text) => text.clone(),
        other => other.to_string(),
    };
    if text.chars().count() > 200 {
        format!("{}…", text.chars().take(200).collect::<String>())
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(sql: &str, ms: u64) -> Timing {
        Timing {
            kind: "db.query".into(),
            label: sql.into(),
            duration: Some(Duration::from_millis(ms)),
            fields: Vec::new(),
        }
    }

    #[test]
    fn totals_only_the_kind_asked_for() {
        let collected = Collected {
            timings: vec![
                query("select 1", 3),
                query("select 2", 4),
                Timing {
                    kind: "cache.hit".into(),
                    label: "users:1".into(),
                    duration: Some(Duration::from_millis(50)),
                    fields: Vec::new(),
                },
            ],
            started: None,
        };

        assert_eq!(collected.of("db.query").len(), 2);
        assert_eq!(collected.total("db.query"), Duration::from_millis(7));
        assert_eq!(collected.total("cache.hit"), Duration::from_millis(50));
    }

    #[test]
    fn finds_the_query_that_repeats() {
        // The whole reason a bar beats a total. Thirty of these is a missing
        // eager load; the count alone would just say "31 queries".
        let mut timings = vec![query("select * from posts", 2)];
        for _ in 0..30 {
            timings.push(query("select * from users where id = ?", 1));
        }
        let collected = Collected { timings, started: None };

        let repeated = collected.repeated_queries();
        assert_eq!(repeated.len(), 1, "only the repeated one is reported");
        assert_eq!(repeated[0], ("select * from users where id = ?".to_string(), 30));
    }

    #[test]
    fn a_query_run_once_is_not_reported_as_repeated() {
        let collected =
            Collected { timings: vec![query("select 1", 1), query("select 2", 1)], started: None };

        assert!(collected.repeated_queries().is_empty());
    }

    #[test]
    fn sensitive_field_names_never_reach_the_page() {
        // The bar is written into HTML that ends up in screenshots and bug
        // reports, so this is bluntly a list of names rather than anything
        // clever about values.
        for key in [
            "password", "PASSWORD", "api_key", "authorization", "x-api-key",
            "session_id", "cookie", "db_credential", "private_key",
        ] {
            assert!(is_sensitive(key), "{key} should be withheld");
        }
        for key in ["sql", "rows", "duration_ms", "route", "status", "key"] {
            assert!(!is_sensitive(key), "{key} is not sensitive and is worth showing");
        }
    }

    #[test]
    fn a_long_value_is_cut_rather_than_filling_the_page() {
        let long = Json::String("x".repeat(5_000));
        let shown = brief(&long);

        assert!(shown.chars().count() <= 201, "got {} characters", shown.chars().count());
        assert!(shown.ends_with('…'));
    }

    #[tokio::test]
    async fn collects_what_happened_inside_the_scope_and_nothing_outside_it() {
        Collector::install();

        // Dispatched before any collector exists: belongs to nobody.
        Event::new("db.query").with("sql", "select 'before'").dispatch();

        let ((), collected) = Collector::collect(async {
            Event::new("db.query").with("sql", "select 'inside'").dispatch();
            Event::new("cache.hit").with("key", "users:1").dispatch();
        })
        .await;

        Event::new("db.query").with("sql", "select 'after'").dispatch();

        let queries = collected.of("db.query");
        assert_eq!(queries.len(), 1, "only the query inside the scope");
        assert_eq!(queries[0].label, "select 'inside'");
        assert_eq!(collected.of("cache.hit").len(), 1);
    }

    #[tokio::test]
    async fn two_requests_do_not_see_each_other() {
        // The property the whole design rests on: a number on one page must
        // belong to that page. The bus is global; the collector is not.
        Collector::install();

        let first = Collector::collect(async {
            Event::new("db.query").with("sql", "first").dispatch();
            tokio::task::yield_now().await;
            Event::new("db.query").with("sql", "first again").dispatch();
        });

        let second = Collector::collect(async {
            Event::new("db.query").with("sql", "second").dispatch();
            tokio::task::yield_now().await;
        });

        let (((), one), ((), two)) = tokio::join!(first, second);

        assert_eq!(one.of("db.query").len(), 2);
        assert_eq!(two.of("db.query").len(), 1);
        assert_eq!(two.of("db.query")[0].label, "second");
    }
}
