//! One recorded moment, as Telescope keeps and renders it.
//!
//! An entry is deliberately *not* an enum over the kinds the framework happens
//! to emit today. The event bus carries an open field map so a package can
//! report something core has never heard of, and the roadmap already promises
//! `ai.call`, `queue.job` and `mail.sent`. So an entry stores the kind as a
//! string and the fields as they arrived, and every renderer here degrades
//! gracefully: a kind Telescope has never seen still gets a readable one-line
//! summary, a duration, and a full field table on the detail page.

use crate::redact;
use rustlavel_core::{Event, Json};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A single captured event, with the identity and grouping Telescope adds.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Monotonic within a process, and never reused — the dashboard polls with
    /// `?after=<id>`, so a recycled id would silently hide entries.
    pub id: u64,
    pub kind: String,
    pub at: SystemTime,
    pub duration_ms: Option<f64>,
    pub fields: BTreeMap<String, Json>,
    /// The id of the `http.request` entry this belongs to, once one claims it.
    pub group: Option<u64>,
    /// An explicit correlation id, when the emitter supplied one.
    pub request_key: Option<String>,
}

/// Field names an emitter may use to correlate its events with a request.
/// Nothing in the framework sets one yet; the moment something does, grouping
/// stops being a heuristic (see [`crate::store`]).
const REQUEST_KEYS: &[&str] = &["request_id", "req_id", "trace_id", "correlation_id"];

/// Fields worth putting in a one-line summary, best first. Consulted only for
/// kinds this module does not special-case, which is how an unknown kind still
/// reads like a sentence instead of a debug dump.
const SUMMARY_KEYS: &[&str] =
    &["message", "summary", "title", "name", "subject", "job", "url", "path", "sql", "model", "to"];

impl Entry {
    /// Build an entry from a dispatched event, scrubbing credentials on the way
    /// in so no later consumer can leak them.
    pub fn from_event(id: u64, event: &Event) -> Self {
        let fields = redact::fields(event.fields.clone());
        let request_key = REQUEST_KEYS
            .iter()
            .find_map(|key| fields.get(*key))
            .and_then(Json::as_str)
            .map(str::to_string);

        Entry {
            id,
            kind: event.kind.to_string(),
            at: event.at,
            duration_ms: event.duration_ms(),
            fields,
            group: None,
            request_key,
        }
    }

    /// Milliseconds since the Unix epoch, which is what the page formats.
    pub fn at_millis(&self) -> f64 {
        self.at.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0
    }

    fn text(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Json::as_str)
    }

    /// The one line shown in the list.
    pub fn summary(&self) -> String {
        let line = match self.kind.as_str() {
            "http.request" => {
                let method = self.text("method").unwrap_or("GET");
                let path = self.text("path").unwrap_or("/");
                format!("{method} {path}")
            }
            "db.query" => self.text("sql").unwrap_or("query").to_string(),
            "log" => self.text("message").unwrap_or_default().to_string(),
            _ => self.generic_summary(),
        };
        truncate(collapse_whitespace(&line), 180)
    }

    /// The fallback for a kind Telescope has never seen: the most descriptive
    /// field it can find, or a compact `key=value` rendering of the first few.
    fn generic_summary(&self) -> String {
        if let Some(value) = SUMMARY_KEYS.iter().find_map(|key| self.text(key)) {
            return value.to_string();
        }
        if self.fields.is_empty() {
            return self.kind.clone();
        }
        self.fields
            .iter()
            .take(4)
            .map(|(key, value)| match value {
                Json::String(s) => format!("{key}={s}"),
                other => format!("{key}={other}"),
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// The small status pill: an HTTP status, a log level, a query's outcome.
    /// `None` for kinds with nothing meaningful to say, which is the honest
    /// answer for an unknown one.
    pub fn badge(&self) -> Option<String> {
        match self.kind.as_str() {
            "http.request" => self.fields.get("status").and_then(Json::as_i64).map(|s| s.to_string()),
            "log" => self.text("level").map(str::to_uppercase),
            "db.query" => Some(match self.fields.get("ok").and_then(Json::as_bool) {
                Some(false) => "failed".to_string(),
                _ => match self.fields.get("rows").and_then(Json::as_i64) {
                    Some(rows) => format!("{rows} rows"),
                    None => "ok".to_string(),
                },
            }),
            _ => self.fields.get("status").map(|status| match status {
                Json::String(s) => s.clone(),
                other => other.to_string(),
            }),
        }
    }

    /// How the badge should read: a colour role, not a colour.
    pub fn tone(&self) -> &'static str {
        match self.kind.as_str() {
            "http.request" => match self.fields.get("status").and_then(Json::as_i64).unwrap_or(200) {
                code if code >= 500 => "error",
                code if code >= 400 => "warn",
                code if code >= 300 => "muted",
                _ => "ok",
            },
            "log" => match self.text("level") {
                Some("error") => "error",
                Some("warn") => "warn",
                Some("debug") => "muted",
                _ => "ok",
            },
            "db.query" => match self.fields.get("ok").and_then(Json::as_bool) {
                Some(false) => "error",
                _ => "ok",
            },
            _ => "muted",
        }
    }

    /// The shape the dashboard and the JSON API both consume.
    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id)),
            ("kind", Json::from(self.kind.as_str())),
            ("at", Json::from(self.at_millis())),
            ("duration_ms", Json::from(self.duration_ms)),
            ("summary", Json::from(self.summary())),
            ("badge", Json::from(self.badge())),
            ("tone", Json::from(self.tone())),
            ("group", Json::from(self.group)),
            ("fields", Json::Object(self.fields.clone())),
        ])
    }

    /// Rebuild an entry written to the journal by an earlier run.
    ///
    /// Returns `None` for a line that is not an entry, so one corrupt line at
    /// the end of a file (a process killed mid-write) costs that line and not
    /// the whole history.
    pub fn from_json(value: &Json) -> Option<Entry> {
        let id = value.get("id")?.as_f64()? as u64;
        let kind = value.get("kind")?.as_str()?.to_string();
        let at_millis = value.get("at").and_then(Json::as_f64).unwrap_or_default();
        let fields = match value.get("fields") {
            Some(Json::Object(map)) => map.clone(),
            _ => BTreeMap::new(),
        };
        let request_key = REQUEST_KEYS
            .iter()
            .find_map(|key| fields.get(*key))
            .and_then(Json::as_str)
            .map(str::to_string);

        Some(Entry {
            id,
            kind,
            at: UNIX_EPOCH + Duration::from_secs_f64((at_millis / 1000.0).max(0.0)),
            duration_ms: value.get("duration_ms").and_then(Json::as_f64),
            fields,
            group: value.get("group").and_then(Json::as_f64).map(|g| g as u64),
            request_key,
        })
    }
}

/// Newlines and runs of spaces turn a formatted SQL statement into an unusable
/// list row, so summaries are flattened.
fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(mut value: String, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value;
    }
    let cut = value.char_indices().nth(limit).map_or(value.len(), |(index, _)| index);
    value.truncate(cut);
    value.push('…');
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn entry(kind: &'static str, fields: &[(&str, Json)]) -> Entry {
        let mut event = Event::new(kind).took(Duration::from_millis(12));
        for (key, value) in fields {
            event = event.with(key, value.clone());
        }
        Entry::from_event(1, &event)
    }

    #[test]
    fn an_http_request_summarises_as_method_and_path() {
        let entry = entry(
            "http.request",
            &[("method", "GET".into()), ("path", "/users".into()), ("status", 200.into())],
        );

        assert_eq!(entry.summary(), "GET /users");
        assert_eq!(entry.badge().as_deref(), Some("200"));
        assert_eq!(entry.tone(), "ok");
    }

    #[test]
    fn a_server_error_reads_as_an_error_tone() {
        let entry = entry("http.request", &[("status", 503.into())]);
        assert_eq!(entry.tone(), "error");
    }

    #[test]
    fn a_query_summarises_as_flattened_sql() {
        let entry = entry(
            "db.query",
            &[("sql", "select *\n  from users".into()), ("rows", 3.into()), ("ok", true.into())],
        );

        assert_eq!(entry.summary(), "select * from users");
        assert_eq!(entry.badge().as_deref(), Some("3 rows"));
    }

    #[test]
    fn a_failed_query_is_marked_failed() {
        let entry = entry("db.query", &[("sql", "select 1".into()), ("ok", false.into())]);

        assert_eq!(entry.badge().as_deref(), Some("failed"));
        assert_eq!(entry.tone(), "error");
    }

    #[test]
    fn a_kind_telescope_has_never_seen_still_renders_sensibly() {
        // What `rustlavel-ai` will emit, long before this crate knows about it.
        let entry = entry("ai.call", &[("model", "claude".into()), ("tokens", 1200.into())]);
        assert_eq!(entry.summary(), "claude");

        // And the worst case: no recognisable field at all.
        let opaque = entry_with_only(&[("shard", 3.into()), ("region", "eu".into())]);
        assert_eq!(opaque.summary(), "region=eu · shard=3");
        assert_eq!(opaque.tone(), "muted");
    }

    fn entry_with_only(fields: &[(&str, Json)]) -> Entry {
        entry("queue.job", fields)
    }

    #[test]
    fn a_long_summary_is_truncated_with_an_ellipsis() {
        let long = "x".repeat(400);
        let entry = entry("log", &[("message", long.as_str().into())]);

        assert!(entry.summary().chars().count() <= 181);
        assert!(entry.summary().ends_with('…'));
    }

    #[test]
    fn credentials_never_reach_an_entry() {
        let entry = entry("log", &[("message", "login".into()), ("password", "hunter2".into())]);

        assert!(!entry.to_json().to_string().contains("hunter2"));
    }

    #[test]
    fn an_entry_round_trips_through_json() {
        let original = entry("http.request", &[("method", "POST".into()), ("path", "/x".into())]);
        let restored = Entry::from_json(&original.to_json()).expect("entry parses back");

        assert_eq!(restored.id, original.id);
        assert_eq!(restored.kind, original.kind);
        assert_eq!(restored.summary(), original.summary());
        assert_eq!(restored.duration_ms, original.duration_ms);
    }

    #[test]
    fn a_corrupt_line_parses_to_nothing_instead_of_panicking() {
        assert!(Entry::from_json(&Json::parse(r#"{"half":"writ"#).unwrap_or(Json::Null)).is_none());
        assert!(Entry::from_json(&Json::object([("id", 1.into())])).is_none());
    }

    #[test]
    fn an_explicit_request_id_is_picked_up_when_an_emitter_supplies_one() {
        let entry = entry("db.query", &[("request_id", "abc-123".into())]);
        assert_eq!(entry.request_key.as_deref(), Some("abc-123"));
    }
}
