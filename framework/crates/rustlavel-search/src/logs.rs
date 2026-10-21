//! Shipping logs to Elasticsearch or OpenSearch.
//!
//! Every `info!`, `warn!` and `error!` is already an event on the bus
//! (`rustlavel_core::events`), so shipping logs is a subscriber — the same way
//! Telescope and the OpenTelemetry exporter hear them. Nothing about logging
//! changes for the code that logs.
//!
//! ```ignore
//! // The plugin, from configuration (`search.logs.enabled = true`):
//! App::new()?.plugin(LogShipping::default()).run().await
//!
//! // Or by hand:
//! LogShipper::new(SearchClient::new("http://localhost:9200"))
//!     .service("blog")
//!     .index("logs")
//!     .install();
//! ```
//!
//! # What lands in the cluster
//!
//! One document per log line, in a **daily index** (`logs-2026.10.21`), shaped
//! after the Elastic Common Schema's most-used fields so Kibana's and
//! OpenSearch Dashboards' log views find them without a mapping:
//!
//! ```json
//! { "@timestamp": "2026-10-21T13:24:27.123Z",
//!   "message": "Rustlavel serving on http://127.0.0.1:8000",
//!   "log": { "level": "info" },
//!   "service": { "name": "blog" },
//!   "event": { "dataset": "log" } }
//! ```
//!
//! It is a subset of ECS, not a claim of conformance. Indices are created by
//! the first write; `@timestamp` as an ISO-8601 string is picked up as a date
//! by dynamic mapping, so nothing needs creating beforehand.
//!
//! # Failure
//!
//! Logging must not be able to take down what it observes, and a log pipeline
//! must not be able to make its own outage louder:
//!
//! * Recording is a mutex and a push. It never awaits and never touches the
//!   network, so a log call's latency does not depend on a cluster.
//! * The queue is bounded. When it is full the **newest** line is dropped and
//!   counted — dropping the oldest would discard the batch nearest to leaving
//!   and keep the queue permanently full under load.
//! * An unreachable cluster costs lines, never requests: a whole-request
//!   failure is retried a few times with backoff, then the batch is discarded.
//!   Individual items the cluster rejected for load (`429`) are put back and
//!   tried again, up to the same limit; items it rejected for being wrong
//!   (a mapping conflict) are counted as failed and not retried, because
//!   sending them again gives the same answer.
//! * **The shipper's own trouble never re-enters the pipeline.** Its failures
//!   go straight to stderr, and anything logged while it is talking to the
//!   cluster — including by the HTTP client underneath — is skipped. Without
//!   that, a down cluster would produce a warning that was shipped to the down
//!   cluster, which would fail, and warn.
//! * Shutdown flushes, with a bounded wait.

use crate::client::SearchClient;
use crate::document::BulkOperation;
use rustlavel_core::events::{Event, Subscriber, subscribe};
use rustlavel_core::log::Level;
use rustlavel_core::{Config, Json};
use rustlavel_http::{Plugin, Setup};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

tokio::task_local! {
    /// Set while the shipper is talking to the cluster, so what is logged
    /// meanwhile is not shipped.
    static SHIPPING: ();
}

/// How the shipper behaves. Every field has a default that is safe to run with.
#[derive(Debug, Clone)]
pub struct LogSettings {
    /// The index name before the date: `logs` gives `logs-2026.10.21`.
    pub index: String,
    /// `service.name` on every document. Empty leaves it out.
    pub service: String,
    /// Lines below this level are not shipped.
    pub min_level: Level,
    /// Event kinds to ship. Just `log` by default; add `http.request` or
    /// `db.query` to ship those too, as documents named for their kind.
    pub kinds: Vec<String>,
    /// How often the background task flushes.
    pub interval: Duration,
    /// The most documents in one `_bulk` request.
    pub max_batch: usize,
    /// The most documents held while waiting to be sent.
    pub max_queue: usize,
    /// How many times a batch, or an item rejected for load, is tried again.
    pub max_retries: u32,
}

impl Default for LogSettings {
    fn default() -> Self {
        LogSettings {
            index: "logs".into(),
            service: String::new(),
            min_level: Level::Info,
            kinds: vec!["log".into()],
            interval: Duration::from_secs(2),
            max_batch: 500,
            max_queue: 10_000,
            max_retries: 3,
        }
    }
}

/// What the shipper has done so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogStats {
    /// Waiting to be sent.
    pub pending: usize,
    /// Accepted by the cluster.
    pub shipped: u64,
    /// Rejected by the cluster for a reason retrying cannot fix.
    pub failed: u64,
    /// Lost: the queue was full, or the cluster could not be reached.
    pub dropped: u64,
}

enum Command {
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

struct Queued {
    index: String,
    document: Json,
    attempts: u32,
}

struct Inner {
    settings: LogSettings,
    client: SearchClient,
    queue: Mutex<VecDeque<Queued>>,
    shipped: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
    reported: AtomicU64,
    started: AtomicBool,
    control: mpsc::UnboundedSender<Command>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<Command>>>,
}

/// The queue, and the background task that empties it into the cluster.
#[derive(Clone)]
pub struct LogShipper {
    inner: Arc<Inner>,
}

impl LogShipper {
    pub fn new(client: SearchClient) -> LogShipper {
        LogShipper::with_settings(client, LogSettings::default())
    }

    pub fn with_settings(client: SearchClient, settings: LogSettings) -> LogShipper {
        let (control, receiver) = mpsc::unbounded_channel();
        LogShipper {
            inner: Arc::new(Inner {
                settings,
                client,
                queue: Mutex::new(VecDeque::new()),
                shipped: AtomicU64::new(0),
                failed: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                reported: AtomicU64::new(0),
                started: AtomicBool::new(false),
                control,
                receiver: Mutex::new(Some(receiver)),
            }),
        }
    }

    /// Builders. They rebuild the shipper, so call them before `install`.
    fn edit(self, change: impl FnOnce(&mut LogSettings)) -> LogShipper {
        let mut settings = self.inner.settings.clone();
        change(&mut settings);
        LogShipper::with_settings(self.inner.client.clone(), settings)
    }

    /// The index name before the date.
    pub fn index(self, prefix: impl Into<String>) -> LogShipper {
        let prefix = prefix.into();
        self.edit(|s| s.index = prefix)
    }

    /// `service.name` on every document.
    pub fn service(self, name: impl Into<String>) -> LogShipper {
        let name = name.into();
        self.edit(|s| s.service = name)
    }

    pub fn min_level(self, level: Level) -> LogShipper {
        self.edit(|s| s.min_level = level)
    }

    /// Ship these event kinds as well as, or instead of, `log`.
    pub fn kinds<I, S>(self, kinds: I) -> LogShipper
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let kinds: Vec<String> = kinds.into_iter().map(Into::into).collect();
        self.edit(|s| s.kinds = kinds)
    }

    pub fn interval(self, every: Duration) -> LogShipper {
        self.edit(|s| s.interval = every)
    }

    pub fn max_queue(self, documents: usize) -> LogShipper {
        self.edit(|s| s.max_queue = documents.max(1))
    }

    pub fn max_batch(self, documents: usize) -> LogShipper {
        self.edit(|s| s.max_batch = documents.max(1))
    }

    pub fn settings(&self) -> &LogSettings {
        &self.inner.settings
    }

    /// Subscribe to the event bus and start the background task.
    ///
    /// Returns the shipper, to hold for `flush` and `shutdown`. Subscribers
    /// live for the process, so call this once.
    pub fn install(self) -> LogShipper {
        subscribe(Collector { shipper: self.clone() });

        // `start` spawns onto the current runtime. Outside one — a test, or a
        // CLI assembling routes to list them — the shipper still queues, and
        // simply has nothing flushing until `start` is called.
        if tokio::runtime::Handle::try_current().is_ok() {
            self.start();
        }
        self
    }

    /// Start the background flush loop. False if it is already running.
    pub fn start(&self) -> bool {
        let Some(receiver) = self.inner.receiver.lock().expect("log control poisoned").take()
        else {
            return false;
        };
        self.inner.started.store(true, Ordering::Release);
        let shipper = self.clone();
        tokio::spawn(async move { run(shipper, receiver).await });
        true
    }

    /// Queue one event if it is the kind and level this shipper ships.
    ///
    /// Synchronous and non-blocking: this runs on whichever task logged.
    pub fn record(&self, event: &Event) {
        if SHIPPING.try_with(|_| ()).is_ok() {
            return;
        }
        if !self.wants(event) {
            return;
        }

        let document = log_document(event, &self.inner.settings.service);
        let index = index_for(&self.inner.settings.index, event.at);

        let mut queue = self.inner.queue.lock().expect("log queue poisoned");
        if queue.len() >= self.inner.settings.max_queue {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        queue.push_back(Queued { index, document, attempts: 0 });
    }

    fn wants(&self, event: &Event) -> bool {
        if !self.inner.settings.kinds.iter().any(|kind| kind == event.kind) {
            return false;
        }
        if event.kind == "log" {
            let level = event
                .field("level")
                .and_then(Json::as_str)
                .and_then(Level::parse)
                .unwrap_or(Level::Info);
            return level >= self.inner.settings.min_level;
        }
        true
    }

    pub fn stats(&self) -> LogStats {
        LogStats {
            pending: self.inner.queue.lock().expect("log queue poisoned").len(),
            shipped: self.inner.shipped.load(Ordering::Relaxed),
            failed: self.inner.failed.load(Ordering::Relaxed),
            dropped: self.inner.dropped.load(Ordering::Relaxed),
        }
    }

    /// Send everything queued now, and wait for it.
    pub async fn flush(&self) {
        if self.inner.started.load(Ordering::Acquire) {
            let (ack, wait) = oneshot::channel();
            if self.inner.control.send(Command::Flush(ack)).is_ok()
                && tokio::time::timeout(self.deadline(), wait).await.is_ok()
            {
                return;
            }
        }
        self.flush_now().await;
    }

    /// Stop the background task, flushing what is left. A bounded wait: a
    /// cluster that has stopped answering must not stop a deployment.
    pub async fn shutdown(&self) {
        if self.inner.started.swap(false, Ordering::AcqRel) {
            let (ack, wait) = oneshot::channel();
            if self.inner.control.send(Command::Shutdown(ack)).is_ok()
                && tokio::time::timeout(self.deadline(), wait).await.is_ok()
            {
                return;
            }
        }
        self.flush_now().await;
    }

    fn deadline(&self) -> Duration {
        Duration::from_secs(10) * (self.inner.settings.max_retries + 2)
    }

    async fn flush_now(&self) {
        SHIPPING.scope((), self.drain()).await;
        self.report_drops();
    }

    /// One line per flush about what was lost — not one per lost line, which
    /// would make an outage of the cluster an outage of stderr as well.
    fn report_drops(&self) {
        let dropped = self.inner.dropped.load(Ordering::Relaxed);
        let reported = self.inner.reported.swap(dropped, Ordering::Relaxed);
        if dropped > reported {
            eprintln!(
                "search-logs: dropped {} log line(s); the cluster is not keeping up or is \
                 unreachable (queue holds {})",
                dropped - reported,
                self.inner.settings.max_queue
            );
        }
    }

    async fn drain(&self) {
        loop {
            let batch: Vec<Queued> = {
                let mut queue = self.inner.queue.lock().expect("log queue poisoned");
                let take = queue.len().min(self.inner.settings.max_batch);
                queue.drain(..take).collect()
            };
            if batch.is_empty() {
                return;
            }

            let operations: Vec<BulkOperation> = batch
                .iter()
                .map(|queued| {
                    BulkOperation::index(queued.document.clone()).in_index(queued.index.clone())
                })
                .collect();

            match self.inner.client.bulk_across_indices(&operations).await {
                Ok(report) => {
                    let retry = report.retryable_positions();
                    let failed = report.failed();
                    let total = batch.len();
                    self.inner.shipped.fetch_add((total - failed) as u64, Ordering::Relaxed);

                    if let Some(item) = report.failures().first() {
                        eprintln!(
                            "search-logs: the cluster rejected {failed} of {total} line(s); first: {}",
                            item.error.as_ref().map_or_else(
                                || format!("status {}", item.status),
                                ToString::to_string
                            )
                        );
                    }

                    // Overloaded, not wrong: put these back, up to the limit.
                    let overloaded: std::collections::HashSet<usize> = retry.iter().copied().collect();
                    let mut requeue = Vec::new();
                    for (at, mut queued) in batch.into_iter().enumerate() {
                        if !overloaded.contains(&at) {
                            continue;
                        }
                        queued.attempts += 1;
                        if queued.attempts > self.inner.settings.max_retries {
                            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                        } else {
                            requeue.push(queued);
                        }
                    }
                    let rejected_for_good = failed - overloaded.len();
                    self.inner.failed.fetch_add(rejected_for_good as u64, Ordering::Relaxed);

                    if !requeue.is_empty() {
                        {
                            let mut queue = self.inner.queue.lock().expect("log queue poisoned");
                            for queued in requeue {
                                queue.push_front(queued);
                            }
                        }
                        // The cluster said it is busy. Hammering it is the
                        // opposite of an answer; the next flush tries again.
                        return;
                    }
                }
                Err(error) => {
                    let attempts = batch.iter().map(|q| q.attempts).max().unwrap_or(0);
                    if error.is_retryable() && attempts < self.inner.settings.max_retries {
                        {
                            let mut queue = self.inner.queue.lock().expect("log queue poisoned");
                            for mut queued in batch.into_iter().rev() {
                                queued.attempts += 1;
                                queue.push_front(queued);
                            }
                        }
                        tokio::time::sleep(backoff(attempts)).await;
                        continue;
                    }

                    eprintln!(
                        "search-logs: could not reach the cluster ({error}); discarding {} line(s)",
                        batch.len()
                    );
                    self.inner.dropped.fetch_add(batch.len() as u64, Ordering::Relaxed);
                    // Anything still queued would meet the same answer.
                    return;
                }
            }
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(250 * 2u64.pow(attempt.min(5))).min(Duration::from_secs(8))
}

async fn run(shipper: LogShipper, mut control: mpsc::UnboundedReceiver<Command>) {
    let mut ticker = tokio::time::interval(shipper.inner.settings.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick is immediate and would flush an empty queue.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => shipper.flush_now().await,
            command = control.recv() => match command {
                Some(Command::Flush(ack)) => {
                    shipper.flush_now().await;
                    let _ = ack.send(());
                }
                Some(Command::Shutdown(ack)) => {
                    shipper.flush_now().await;
                    let _ = ack.send(());
                    return;
                }
                None => return,
            },
        }
    }
}

struct Collector {
    shipper: LogShipper,
}

impl Subscriber for Collector {
    fn handle(&self, event: &Event) {
        self.shipper.record(event);
    }

    fn interested_in(&self, kind: &str) -> bool {
        self.shipper.inner.settings.kinds.iter().any(|wanted| wanted == kind)
    }
}

/// The document one event becomes.
pub fn log_document(event: &Event, service: &str) -> Json {
    let mut fields: Vec<(String, Json)> = Vec::new();
    fields.push(("@timestamp".into(), Json::from(iso8601(event.at))));

    let is_log = event.kind == "log";
    let message = if is_log {
        event.field("message").and_then(Json::as_str).unwrap_or_default().to_string()
    } else {
        event.kind.to_string()
    };
    fields.push(("message".into(), Json::from(message)));

    if is_log {
        let level = event.field("level").and_then(Json::as_str).unwrap_or("info");
        fields.push(("log".into(), Json::object([("level", Json::from(level))])));
    }
    if !service.is_empty() {
        fields.push(("service".into(), Json::object([("name", Json::from(service))])));
    }

    let mut ecs_event = vec![("dataset".to_string(), Json::from(event.kind))];
    if let Some(duration) = event.duration {
        // ECS counts duration in nanoseconds.
        ecs_event.push(("duration".into(), Json::from(duration.as_nanos() as i64)));
    }
    fields.push(("event".into(), Json::object(ecs_event)));

    // Whatever else the event carried, kept apart from the standard fields so a
    // field named `message` or `level` can never overwrite them.
    let labels: Vec<(String, Json)> = event
        .fields
        .iter()
        .filter(|(key, _)| !(is_log && (key.as_str() == "level" || key.as_str() == "message")))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if !labels.is_empty() {
        fields.push(("labels".into(), Json::object(labels)));
    }

    Json::object(fields)
}

/// `logs` and a moment, as `logs-2026.10.21`: the day, in UTC, the line was
/// written. Daily indices are what make retention a delete of one index rather
/// than a query that scans them all.
pub fn index_for(prefix: &str, at: SystemTime) -> String {
    let (year, month, day, ..) = civil(at);
    format!("{prefix}-{year:04}.{month:02}.{day:02}")
}

/// An ISO-8601 UTC timestamp to the millisecond, which is what Elasticsearch's
/// default `date` format reads.
pub fn iso8601(at: SystemTime) -> String {
    let (year, month, day, hour, minute, second, millis) = civil(at);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil(at: SystemTime) -> (i64, u32, u32, u32, u32, u32, u32) {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs() as i64;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let rest = seconds.rem_euclid(86_400) as u32;
    (year, month, day, rest / 3600, rest % 3600 / 60, rest % 60, since.subsec_millis())
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The plugin: ships logs when `search.logs.enabled` is true.
///
/// Off by default, unlike the OpenTelemetry exporter's traces, because logs can
/// carry anything an application prints and sending them off the machine is a
/// decision somebody should have made.
///
/// | Key | Meaning |
/// |---|---|
/// | `search.logs.enabled` | Ship logs at all. `false` by default. |
/// | `search.logs.index` | Index prefix. `logs` by default. |
/// | `search.logs.level` | Lowest level shipped. `info` by default. |
/// | `search.logs.service` | `service.name`. Falls back to `app.name`. |
/// | `search.logs.interval_ms` | Flush interval. `2000` by default. |
/// | `search.logs.queue` | Most lines held while waiting. `10000` by default. |
///
/// The cluster itself comes from `search.url` and its credentials, as for
/// [`SearchClient::from_config`].
#[derive(Default)]
pub struct LogShipping {
    enabled: Option<bool>,
    client: Option<SearchClient>,
}

impl LogShipping {
    /// Ship regardless of configuration.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// Use this client instead of one built from `search.*` configuration.
    pub fn client(mut self, client: SearchClient) -> Self {
        self.client = Some(client);
        self
    }

    fn settings(config: &Config) -> LogSettings {
        let mut settings = LogSettings {
            index: config.string("search.logs.index", "logs"),
            service: {
                let named = config.string("search.logs.service", "");
                if named.is_empty() { config.string("app.name", "rustlavel") } else { named }
            },
            ..LogSettings::default()
        };
        if let Some(level) = Level::parse(&config.string("search.logs.level", "info")) {
            settings.min_level = level;
        }
        let interval = config.int("search.logs.interval_ms", 0);
        if interval > 0 {
            settings.interval = Duration::from_millis(interval as u64);
        }
        let queue = config.int("search.logs.queue", 0);
        if queue > 0 {
            settings.max_queue = queue as usize;
        }
        settings
    }

    /// Build and install, returning the shipper when it is on.
    pub fn install(self, config: &Config) -> Option<LogShipper> {
        if !self.enabled.unwrap_or_else(|| config.bool("search.logs.enabled", false)) {
            return None;
        }
        let client = self.client.unwrap_or_else(|| SearchClient::from_config(config));
        let shipper = LogShipper::with_settings(client, LogShipping::settings(config)).install();
        rustlavel_core::info!(
            "search-logs: shipping to {} as `{}-*`",
            shipper.inner.client.base_url(),
            shipper.inner.settings.index
        );
        Some(shipper)
    }
}

impl Plugin for LogShipping {
    fn name(&self) -> &'static str {
        "search-logs"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        if let Some(shipper) = (*self).install(setup.config) {
            // State, so the application can read `stats()` and so the server
            // can flush it on shutdown.
            setup.state(shipper);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    fn at(secs: u64, millis: u32) -> SystemTime {
        UNIX_EPOCH + Duration::new(secs, millis * 1_000_000)
    }

    fn log_event(level: &str, message: &str) -> Event {
        Event::new("log").with("level", level).with("message", message)
    }

    fn shipper(body: &str, status: u16) -> (LogShipper, Arc<Fake>) {
        let fake = Fake::new().fallback(FakeResponse::text(body).status(status));
        let client = SearchClient::new("http://localhost:9200").faking(fake);
        let fake = client.fake().unwrap().clone();
        (LogShipper::new(client).service("blog"), fake)
    }

    const OK: &str = r#"{"took":1,"errors":false,"items":[]}"#;

    #[test]
    fn a_timestamp_is_iso_8601_utc_to_the_millisecond() {
        // 2026-10-21T13:24:27.123Z
        assert_eq!(iso8601(at(1_792_589_067, 123)), "2026-10-21T13:24:27.123Z");
        assert_eq!(iso8601(at(0, 0)), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn the_calendar_is_right_across_a_leap_day_and_a_year_end() {
        // 2024-02-29 and 2024-03-01, then the last second of 2025.
        assert_eq!(iso8601(at(1_709_164_800, 0)), "2024-02-29T00:00:00.000Z");
        assert_eq!(iso8601(at(1_709_251_200, 0)), "2024-03-01T00:00:00.000Z");
        assert_eq!(iso8601(at(1_767_225_599, 999)), "2025-12-31T23:59:59.999Z");
    }

    #[test]
    fn the_index_is_the_utc_day_of_the_line_not_the_day_it_was_sent() {
        assert_eq!(index_for("logs", at(1_767_225_599, 0)), "logs-2025.12.31");
        assert_eq!(index_for("logs", at(1_767_225_600, 0)), "logs-2026.01.01");
    }

    #[test]
    fn a_log_line_becomes_an_ecs_shaped_document() {
        let event = Event { at: at(1_792_589_067, 5), ..log_event("warn", "disk at 91%") };
        let document = log_document(&event, "blog");

        assert_eq!(document.get("@timestamp").and_then(Json::as_str), Some("2026-10-21T13:24:27.005Z"));
        assert_eq!(document.get("message").and_then(Json::as_str), Some("disk at 91%"));
        assert_eq!(document.get("log").and_then(|l| l.get("level")).and_then(Json::as_str), Some("warn"));
        assert_eq!(document.get("service").and_then(|s| s.get("name")).and_then(Json::as_str), Some("blog"));
        assert_eq!(document.get("event").and_then(|e| e.get("dataset")).and_then(Json::as_str), Some("log"));
        assert!(document.get("labels").is_none(), "level and message are not repeated as labels");
    }

    #[test]
    fn another_kind_is_named_for_itself_and_keeps_its_duration_and_fields() {
        let event = Event::new("http.request")
            .with("status", 200)
            .took(Duration::from_millis(12));
        let document = log_document(&event, "");

        assert_eq!(document.get("message").and_then(Json::as_str), Some("http.request"));
        assert!(document.get("log").is_none());
        assert!(document.get("service").is_none(), "an empty service name is left out");
        assert_eq!(
            document.get("event").and_then(|e| e.get("duration")).and_then(Json::as_i64),
            Some(12_000_000)
        );
        assert!(document.get("labels").and_then(|l| l.get("status")).is_some());
    }

    #[test]
    fn a_field_called_message_cannot_overwrite_the_message() {
        let event = Event::new("db.query").with("message", "attacker").with("level", "x");
        let document = log_document(&event, "");
        assert_eq!(document.get("message").and_then(Json::as_str), Some("db.query"));
        assert_eq!(
            document.get("labels").and_then(|l| l.get("message")).and_then(Json::as_str),
            Some("attacker")
        );
    }

    #[test]
    fn lines_below_the_level_and_kinds_not_asked_for_are_not_queued() {
        let (shipper, _) = shipper(OK, 200);
        shipper.record(&log_event("debug", "noise"));
        shipper.record(&Event::new("http.request"));
        shipper.record(&log_event("info", "kept"));
        shipper.record(&log_event("error", "kept too"));
        assert_eq!(shipper.stats().pending, 2);
    }

    #[tokio::test]
    async fn a_flush_sends_one_bulk_to_the_root_endpoint_naming_each_daily_index() {
        let (shipper, fake) = shipper(OK, 200);
        shipper.record(&Event { at: at(1_767_225_599, 0), ..log_event("info", "last of 2025") });
        shipper.record(&Event { at: at(1_767_225_600, 0), ..log_event("info", "first of 2026") });

        shipper.flush().await;

        let sent = fake.recorded();
        assert_eq!(sent.len(), 1, "two lines, one request");
        assert_eq!(sent[0].url, "http://localhost:9200/_bulk");
        let body = sent[0].body_text();
        assert!(body.contains(r#""_index":"logs-2025.12.31""#), "{body}");
        assert!(body.contains(r#""_index":"logs-2026.01.01""#), "{body}");
        assert!(body.contains("last of 2025") && body.ends_with('\n'));
        assert_eq!(shipper.stats().shipped, 2);
        assert_eq!(shipper.stats().pending, 0);
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_newest_and_counts_it() {
        let (shipper, _) = shipper(OK, 200);
        let shipper = shipper.max_queue(2);
        for n in 0..5 {
            shipper.record(&log_event("info", &format!("line {n}")));
        }
        let stats = shipper.stats();
        assert_eq!((stats.pending, stats.dropped), (2, 3));
    }

    #[tokio::test]
    async fn a_line_the_cluster_refuses_for_good_is_counted_and_not_sent_again() {
        let body = r#"{"took":1,"errors":true,"items":[{"index":{"_index":"logs-2026.10.21","_id":"x","status":400,"error":{"type":"document_parsing_exception","reason":"failed to parse"}}}]}"#;
        let (shipper, fake) = shipper(body, 200);
        shipper.record(&log_event("info", "one"));

        shipper.flush().await;

        let stats = shipper.stats();
        assert_eq!((stats.failed, stats.shipped, stats.pending), (1, 0, 0));
        assert_eq!(fake.count(), 1, "a mapping conflict answers the same way every time");
    }

    #[tokio::test]
    async fn a_line_rejected_for_load_goes_back_and_is_dropped_after_the_limit() {
        let body = r#"{"took":1,"errors":true,"items":[{"index":{"_index":"logs-2026.10.21","_id":"x","status":429,"error":{"type":"es_rejected_execution_exception","reason":"rejected"}}}]}"#;
        let (shipper, fake) = shipper(body, 200);
        shipper.record(&log_event("info", "busy"));

        // Each flush tries once and puts the line back; the fourth gives up.
        for _ in 0..4 {
            shipper.flush().await;
        }

        let stats = shipper.stats();
        assert_eq!(fake.count(), 4);
        assert_eq!((stats.dropped, stats.pending, stats.failed), (1, 0, 0));
    }

    #[tokio::test]
    async fn an_unreachable_cluster_costs_lines_and_never_a_stuck_queue() {
        let fake = Fake::new().fallback(FakeResponse::text("down").status(503));
        let client = SearchClient::new("http://localhost:9200").retries(0).faking(fake);
        let shipper = LogShipper::new(client);
        shipper.record(&log_event("info", "a"));
        shipper.record(&log_event("info", "b"));

        shipper.flush().await;

        let stats = shipper.stats();
        assert_eq!(stats.pending + stats.dropped as usize, 2, "every line is accounted for");
        assert_eq!(stats.shipped, 0);
    }

    #[tokio::test]
    async fn what_is_logged_while_shipping_is_not_shipped() {
        let (shipper, _) = shipper(OK, 200);
        SHIPPING.scope((), async { shipper.record(&log_event("warn", "from the client")) }).await;
        assert_eq!(shipper.stats().pending, 0);
    }

    #[test]
    fn the_plugin_is_off_unless_configured() {
        let config = Config::default();
        assert!(LogShipping::default().install(&config).is_none());
    }
}
