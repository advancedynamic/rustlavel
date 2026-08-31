//! Batching and shipping, over OTLP/HTTP.
//!
//! The governing rule is that telemetry may never take down the application it
//! observes. Everything here follows from that:
//!
//! * Recording a span takes a mutex and pushes onto a queue. It never awaits,
//!   never allocates a connection, and never blocks on the collector, so a
//!   handler's latency does not depend on a machine somewhere else being up.
//! * The queue is bounded, and a full queue **drops the newest span** and
//!   counts it. Dropping the oldest instead would throw away the batch nearest
//!   to leaving — work already done — and would keep the queue permanently full
//!   under load rather than letting it drain. A dropped span is reported both
//!   as a log line and as a metric, because telemetry that vanishes silently is
//!   worse than none: a flat graph reads as a quiet service.
//! * An unreachable collector produces a warning and dropped spans. It never
//!   produces a stalled request, an unbounded retry loop, or a growing queue.
//! * Shutdown flushes. Losing the last few seconds of a trace is exactly the
//!   part someone was watching when they restarted the process.

use crate::metrics::{Meter, instruments};
use crate::resource::Resource;
use crate::trace::Span;
use rustlavel_client::Client;
use rustlavel_http::Status;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Which encoding to put on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// `application/x-protobuf`. Every collector must accept it, which is why
    /// it is the default.
    #[default]
    Protobuf,
    /// `application/json`. A collector *may* accept it — the OTLP
    /// specification makes it optional — but it is readable in a proxy log,
    /// which is worth a great deal when an export is being debugged.
    Json,
}

impl Protocol {
    pub fn parse(value: &str) -> Option<Protocol> {
        match value.trim().to_ascii_lowercase().as_str() {
            "http/protobuf" | "protobuf" | "proto" => Some(Protocol::Protobuf),
            "http/json" | "json" => Some(Protocol::Json),
            _ => None,
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Protocol::Protobuf => "application/x-protobuf",
            Protocol::Json => "application/json",
        }
    }
}

/// How the exporter behaves. Every field has a default that is safe to run
/// with; the plugin fills them from configuration and environment.
#[derive(Debug, Clone)]
pub struct Settings {
    pub traces_endpoint: String,
    pub metrics_endpoint: String,
    pub protocol: Protocol,
    pub headers: Vec<(String, String)>,
    /// How long one attempt at reaching the collector may take.
    pub timeout: Duration,
    /// How often the background task flushes.
    pub interval: Duration,
    /// The largest number of spans in one request. A collector's default body
    /// limit is a few megabytes, and a batch past it fails as a whole.
    pub max_batch: usize,
    /// The largest number of spans held while waiting to be exported.
    pub max_queue: usize,
    /// How many times a retryable failure is tried again before the batch is
    /// discarded.
    pub max_retries: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            traces_endpoint: "http://localhost:4318/v1/traces".into(),
            metrics_endpoint: "http://localhost:4318/v1/metrics".into(),
            protocol: Protocol::default(),
            headers: Vec::new(),
            timeout: Duration::from_secs(10),
            interval: Duration::from_secs(5),
            max_batch: 512,
            max_queue: 2048,
            max_retries: 3,
        }
    }
}

/// Append a signal's path to a base endpoint, the way the specification says
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is used for HTTP.
pub fn signal_endpoint(base: &str, signal: &str) -> String {
    format!("{}/v1/{signal}", base.trim_end_matches('/'))
}

enum Command {
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

/// The queue, the meter, and the background task that empties them.
#[derive(Clone)]
pub struct Exporter {
    inner: Arc<Inner>,
}

struct Inner {
    settings: Settings,
    resource: Resource,
    client: Client,
    meter: Meter,
    spans: Mutex<VecDeque<Span>>,
    dropped: AtomicU64,
    /// How many drops have already been reported, so the warning is one line
    /// per flush rather than one per lost span — a collector outage must not
    /// turn into a second outage in the log pipeline.
    reported: AtomicU64,
    started: AtomicBool,
    control: mpsc::UnboundedSender<Command>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<Command>>>,
}

tokio::task_local! {
    /// Set while the exporter is talking to the collector.
    ///
    /// The exporter posts with `rustlavel-client`, which reports every outbound
    /// request on the instrumentation bus — including its own. Without this
    /// flag the subscriber would measure the export, and the next export would
    /// carry a data point about the previous one, for ever.
    static EXPORTING: ();
}

/// Whether the current task is the exporter shipping a batch.
pub fn is_exporting() -> bool {
    EXPORTING.try_with(|_| ()).is_ok()
}

impl Exporter {
    pub fn new(settings: Settings, resource: Resource) -> Exporter {
        let client = Client::new().timeout(settings.timeout);
        Exporter::with_client(settings, resource, client)
    }

    /// Build with a caller-supplied client — a faked one in tests, or one
    /// carrying a proxy or client-certificate configuration in production.
    pub fn with_client(settings: Settings, resource: Resource, client: Client) -> Exporter {
        let (control, receiver) = mpsc::unbounded_channel();
        Exporter {
            inner: Arc::new(Inner {
                settings,
                resource,
                client,
                meter: Meter::new(),
                spans: Mutex::new(VecDeque::new()),
                dropped: AtomicU64::new(0),
                reported: AtomicU64::new(0),
                started: AtomicBool::new(false),
                control,
                receiver: Mutex::new(Some(receiver)),
            }),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    pub fn resource(&self) -> &Resource {
        &self.inner.resource
    }

    /// The meter this exporter ships, so an application can record its own.
    pub fn meter(&self) -> &Meter {
        &self.inner.meter
    }

    /// Queue a finished span.
    ///
    /// Synchronous and non-blocking by construction: this runs on the request's
    /// own task, and anything that could await here would be latency charged to
    /// a user.
    pub fn record_span(&self, span: Span) {
        if !span.context.sampled {
            return;
        }

        let mut queue = self.inner.spans.lock().expect("span queue poisoned");
        if queue.len() >= self.inner.settings.max_queue {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        queue.push_back(span);
    }

    /// How many spans are waiting.
    pub fn pending(&self) -> usize {
        self.inner.spans.lock().expect("span queue poisoned").len()
    }

    /// How many spans have been dropped for want of queue space.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Take the queued spans. Tests assert on what was recorded, which a flush
    /// through a faked client cannot show without decoding the payload again.
    #[cfg(test)]
    pub(crate) fn drain_for_test(&self) -> Vec<Span> {
        self.inner.spans.lock().expect("span queue poisoned").drain(..).collect()
    }

    /// Start the background flush loop. Returns false if it is already running.
    pub fn start(&self) -> bool {
        let receiver = match self.inner.receiver.lock().expect("control lock poisoned").take() {
            Some(receiver) => receiver,
            None => return false,
        };
        self.inner.started.store(true, Ordering::Release);

        let exporter = self.clone();
        tokio::spawn(async move { run(exporter, receiver).await });
        true
    }

    /// Send everything queued now, and wait for it.
    ///
    /// When the background loop is running this asks it to flush rather than
    /// flushing here, so two flushes never race for the same batch.
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

    /// Stop the background loop, flushing what is left.
    ///
    /// The tail of a trace is the interesting part when a process is being
    /// restarted, so this is worth the wait — but it is a bounded wait, because
    /// a collector that has stopped answering must not stop a deployment.
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

    /// A whole flush may make several attempts against several batches, so it
    /// is allowed more time than one request.
    fn deadline(&self) -> Duration {
        self.inner.settings.timeout * (self.inner.settings.max_retries + 2)
    }

    /// Do the work, on whatever task is calling.
    async fn flush_now(&self) {
        self.report_drops();
        self.flush_spans().await;
        self.flush_metrics().await;
    }

    /// Turn accumulated drops into a warning and a counter, once per flush.
    fn report_drops(&self) {
        let dropped = self.inner.dropped.load(Ordering::Relaxed);
        let reported = self.inner.reported.swap(dropped, Ordering::Relaxed);
        let pending = self.pending();

        // Reported only once there is something to report. A permanently idle
        // process should send nothing at all rather than a heartbeat of zeroes
        // every interval — the exporter describing its own emptiness is the
        // one metric nobody is paying to store.
        if pending > 0 || dropped > 0 {
            self.inner.meter.set(instruments::QUEUE_DEPTH, Vec::new(), pending as f64);
        }

        if dropped > reported {
            let lost = dropped - reported;
            self.inner.meter.add(instruments::SPANS_DROPPED, Vec::new(), lost);
            rustlavel_core::warn!(
                "otel: dropped {lost} span(s); the export queue holds {} and the collector is not \
                 keeping up. Raise `otel.queue`, shorten `otel.interval_ms`, or fix the collector.",
                self.inner.settings.max_queue
            );
        }
    }

    async fn flush_spans(&self) {
        loop {
            let batch: Vec<Span> = {
                let mut queue = self.inner.spans.lock().expect("span queue poisoned");
                let take = queue.len().min(self.inner.settings.max_batch);
                queue.drain(..take).collect()
            };
            if batch.is_empty() {
                return;
            }

            let body = match self.inner.settings.protocol {
                Protocol::Protobuf => crate::trace::export_request(&self.inner.resource, &batch),
                Protocol::Json => crate::trace::export_request_json(&self.inner.resource, &batch)
                    .to_string()
                    .into_bytes(),
            };

            if !self.post(&self.inner.settings.traces_endpoint, body).await {
                // The batch is already out of the queue and is now gone. Put it
                // back and it would be retried for ever against a collector
                // that has said no; count it as a drop and the loss is visible.
                self.inner.dropped.fetch_add(batch.len() as u64, Ordering::Relaxed);
                return;
            }
        }
    }

    async fn flush_metrics(&self) {
        let body = match self.inner.settings.protocol {
            Protocol::Protobuf => self.inner.meter.export_request(&self.inner.resource),
            Protocol::Json => self
                .inner
                .meter
                .export_request_json(&self.inner.resource)
                .map(|json| json.to_string().into_bytes()),
        };

        // Cumulative metrics are a full snapshot, so a failed send costs
        // resolution and nothing else: the next flush states the same totals.
        if let Some(body) = body {
            self.post(&self.inner.settings.metrics_endpoint, body).await;
        }
    }

    /// One signal's payload, with retries. True when the collector took it.
    async fn post(&self, endpoint: &str, body: Vec<u8>) -> bool {
        let mut attempt = 0;

        loop {
            let mut request = self
                .inner
                .client
                .post(endpoint)
                .header("content-type", self.inner.settings.protocol.content_type())
                .body(body.clone());
            for (name, value) in &self.inner.settings.headers {
                request = request.header(name, value.clone());
            }

            // Scoped so the client's own `http.client` event is recognised as
            // the exporter's and skipped rather than measured.
            let result = EXPORTING.scope((), request.send()).await;

            let backoff = match result {
                Ok(response) if response.is_success() => return true,
                Ok(response) if is_retryable(response.status) => {
                    retry_after(&response).unwrap_or_else(|| backoff_for(attempt))
                }
                Ok(response) => {
                    // A 400 means the payload itself is wrong, and sending it
                    // again produces the same 400. The body carries the
                    // collector's explanation, which is the only thing that
                    // makes an encoding bug findable.
                    rustlavel_core::warn!(
                        "otel: {endpoint} rejected the payload with {}: {}",
                        response.status,
                        excerpt(&response.text())
                    );
                    return false;
                }
                Err(error) if attempt < self.inner.settings.max_retries => {
                    rustlavel_core::debug!("otel: {endpoint} is unreachable ({error}), retrying");
                    backoff_for(attempt)
                }
                Err(error) => {
                    rustlavel_core::warn!(
                        "otel: giving up on {endpoint} after {} attempt(s): {error}",
                        attempt + 1
                    );
                    return false;
                }
            };

            if attempt >= self.inner.settings.max_retries {
                rustlavel_core::warn!(
                    "otel: {endpoint} kept refusing the batch after {} attempt(s); discarding it",
                    attempt + 1
                );
                return false;
            }

            tokio::time::sleep(backoff).await;
            attempt += 1;
        }
    }
}

/// The flush loop.
async fn run(exporter: Exporter, mut control: mpsc::UnboundedReceiver<Command>) {
    let mut ticker = tokio::time::interval(exporter.inner.settings.interval);
    // A Tokio interval fires immediately; the first tick would flush an empty
    // queue before the application has served anything.
    ticker.tick().await;
    // Skipping missed ticks rather than firing them back to back: if a flush
    // ran long, what is wanted next is one flush of everything, not a burst of
    // catch-up requests at a collector that is already struggling.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => exporter.flush_now().await,
            command = control.recv() => match command {
                Some(Command::Flush(ack)) => {
                    exporter.flush_now().await;
                    let _ = ack.send(());
                }
                Some(Command::Shutdown(ack)) => {
                    exporter.flush_now().await;
                    let _ = ack.send(());
                    return;
                }
                // Every handle is gone, so nothing can record another span.
                None => {
                    exporter.flush_now().await;
                    return;
                }
            },
        }
    }
}

/// Which status codes are worth trying again.
///
/// 429 and 503 are the collector saying "later"; the rest of the 5xx range is a
/// collector that is broken now and may not be in a moment. Any other 4xx is a
/// statement about the request, and repeating it changes nothing.
fn is_retryable(status: Status) -> bool {
    status == Status::TOO_MANY_REQUESTS || status.code() >= 500
}

/// Exponential backoff, starting at a quarter second.
fn backoff_for(attempt: u32) -> Duration {
    Duration::from_millis(250 * 2u64.pow(attempt.min(6)))
}

/// Honour `Retry-After` when the collector sends one, capped so a mistaken or
/// hostile header cannot park the flush loop for an hour.
fn retry_after(response: &rustlavel_client::ClientResponse) -> Option<Duration> {
    let seconds: u64 = response.headers.get("retry-after")?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds.min(30)))
}

fn excerpt(body: &str) -> String {
    let trimmed = body.trim();
    match trimmed.char_indices().nth(300) {
        Some((at, _)) => format!("{}…", &trimmed[..at]),
        None => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{SpanContext, SpanKind};
    use rustlavel_client::fake::{Fake, FakeResponse};

    fn settings() -> Settings {
        Settings {
            traces_endpoint: "http://collector.test/v1/traces".into(),
            metrics_endpoint: "http://collector.test/v1/metrics".into(),
            timeout: Duration::from_millis(200),
            max_retries: 2,
            ..Settings::default()
        }
    }

    fn exporter_with(fake: Fake, settings: Settings) -> (Exporter, Arc<Fake>) {
        let client = Client::new().faking(fake);
        let handle = client.fake().expect("faked").clone();
        (Exporter::with_client(settings, Resource::new("test"), client), handle)
    }

    fn a_span() -> Span {
        Span::new(SpanContext::root(), "GET /orders", SpanKind::Server)
    }

    #[tokio::test]
    async fn a_flush_posts_the_queued_spans_with_the_protobuf_content_type() {
        let (exporter, fake) =
            exporter_with(Fake::new().fallback(FakeResponse::text("")), settings());
        exporter.record_span(a_span());

        exporter.flush().await;

        fake.assert_sent("/v1/traces");
        let traces = fake
            .recorded()
            .into_iter()
            .find(|r| r.url.ends_with("/v1/traces"))
            .expect("a trace request");
        assert_eq!(traces.headers.get("content-type"), Some("application/x-protobuf"));
        assert!(traces.body.windows(11).any(|w| w == b"GET /orders"), "the span name is missing");
        assert_eq!(exporter.pending(), 0);
    }

    #[tokio::test]
    async fn the_json_protocol_sends_a_readable_body() {
        let (exporter, fake) = exporter_with(
            Fake::new().fallback(FakeResponse::text("")),
            Settings { protocol: Protocol::Json, ..settings() },
        );
        exporter.record_span(a_span());

        exporter.flush().await;

        let traces = fake
            .recorded()
            .into_iter()
            .find(|r| r.url.ends_with("/v1/traces"))
            .expect("a trace request");
        assert_eq!(traces.headers.get("content-type"), Some("application/json"));
        assert!(traces.body_text().contains(r#""name":"GET /orders""#), "{}", traces.body_text());
    }

    #[tokio::test]
    async fn configured_headers_are_sent_on_every_request() {
        let mut settings = settings();
        settings.headers = vec![("authorization".into(), "Bearer t".into())];
        let (exporter, fake) = exporter_with(Fake::new().fallback(FakeResponse::text("")), settings);
        exporter.record_span(a_span());

        exporter.flush().await;

        assert!(
            fake.recorded().iter().all(|r| r.headers.get("authorization") == Some("Bearer t")),
            "a request went out without the configured header"
        );
    }

    #[tokio::test]
    async fn an_empty_meter_and_queue_send_nothing_at_all() {
        let (exporter, fake) =
            exporter_with(Fake::new().fallback(FakeResponse::text("")), settings());

        exporter.flush().await;

        fake.assert_count(0);
    }

    #[tokio::test]
    async fn metrics_go_to_their_own_endpoint() {
        let (exporter, fake) =
            exporter_with(Fake::new().fallback(FakeResponse::text("")), settings());
        exporter.meter().add(instruments::QUEUE_JOBS, Vec::new(), 1);

        exporter.flush().await;

        fake.assert_sent("/v1/metrics");
        fake.assert_not_sent("/v1/traces");
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_newest_span_and_keeps_the_ones_already_waiting() {
        let (exporter, _) = exporter_with(
            Fake::new().fallback(FakeResponse::text("")),
            Settings { max_queue: 2, ..settings() },
        );

        let first = a_span();
        exporter.record_span(first.clone());
        exporter.record_span(a_span());
        exporter.record_span(a_span());

        assert_eq!(exporter.pending(), 2);
        assert_eq!(exporter.dropped(), 1);
        // The span that was already waiting is still the one at the front.
        let queue = exporter.inner.spans.lock().expect("queue");
        assert_eq!(queue[0].context.span_id, first.context.span_id);
    }

    #[tokio::test]
    async fn an_unsampled_span_is_never_queued() {
        let (exporter, _) = exporter_with(Fake::new().fallback(FakeResponse::text("")), settings());
        let mut span = a_span();
        span.context.sampled = false;

        exporter.record_span(span);

        assert_eq!(exporter.pending(), 0);
        assert_eq!(exporter.dropped(), 0);
    }

    #[tokio::test]
    async fn drops_are_reported_as_a_metric_once_rather_than_once_each() {
        let (exporter, _) = exporter_with(
            Fake::new().fallback(FakeResponse::text("")),
            Settings { max_queue: 1, ..settings() },
        );
        exporter.record_span(a_span());
        exporter.record_span(a_span());
        exporter.record_span(a_span());

        exporter.flush().await;
        exporter.flush().await;

        assert_eq!(exporter.meter().counter(instruments::SPANS_DROPPED, &Vec::new()), 2);
    }

    #[tokio::test]
    async fn a_batch_larger_than_the_limit_is_split_across_requests() {
        let (exporter, fake) = exporter_with(
            Fake::new().fallback(FakeResponse::text("")),
            Settings { max_batch: 2, ..settings() },
        );
        for _ in 0..5 {
            exporter.record_span(a_span());
        }

        exporter.flush().await;

        // Three trace requests of at most two spans each, plus the metrics one.
        assert_eq!(fake.recorded().iter().filter(|r| r.url.ends_with("/v1/traces")).count(), 3);
        assert_eq!(exporter.pending(), 0);
    }

    #[tokio::test]
    async fn a_server_error_is_retried_and_then_the_batch_is_given_up_on() {
        let (exporter, fake) = exporter_with(
            Fake::new().fallback(FakeResponse::text("boom").status(503)),
            Settings { max_retries: 2, ..settings() },
        );
        exporter.record_span(a_span());

        exporter.flush().await;

        // The first attempt plus two retries.
        assert_eq!(fake.recorded().iter().filter(|r| r.url.ends_with("/v1/traces")).count(), 3);
        // And the loss is counted rather than hidden.
        assert_eq!(exporter.dropped(), 1);
        assert_eq!(exporter.pending(), 0);
    }

    #[tokio::test]
    async fn a_rejected_payload_is_not_retried_because_it_would_be_rejected_again() {
        let (exporter, fake) = exporter_with(
            Fake::new().fallback(FakeResponse::text("bad field").status(400)),
            settings(),
        );
        exporter.record_span(a_span());

        exporter.flush().await;

        assert_eq!(fake.recorded().iter().filter(|r| r.url.ends_with("/v1/traces")).count(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_collector_costs_dropped_spans_and_nothing_else() {
        // No fallback: the fake errors, which is what a refused connection
        // looks like from the client's side.
        let (exporter, _) = exporter_with(Fake::new(), settings());
        exporter.record_span(a_span());

        tokio::time::timeout(Duration::from_secs(10), exporter.flush())
            .await
            .expect("a flush against a dead collector must still finish");

        assert_eq!(exporter.pending(), 0);
        assert_eq!(exporter.dropped(), 1);
    }

    #[tokio::test]
    async fn the_background_loop_flushes_on_its_interval_and_on_shutdown() {
        let (exporter, fake) = exporter_with(
            Fake::new().fallback(FakeResponse::text("")),
            Settings { interval: Duration::from_millis(30), ..settings() },
        );

        assert!(exporter.start());
        // Starting twice must not spawn a second loop competing for the queue.
        assert!(!exporter.start());

        exporter.record_span(a_span());
        tokio::time::sleep(Duration::from_millis(120)).await;
        fake.assert_sent("/v1/traces");

        // Whatever arrives after the last tick still leaves on shutdown.
        exporter.record_span(a_span());
        exporter.shutdown().await;
        assert_eq!(exporter.pending(), 0);
    }

    #[tokio::test]
    async fn shutdown_without_a_running_loop_still_flushes() {
        let (exporter, fake) =
            exporter_with(Fake::new().fallback(FakeResponse::text("")), settings());
        exporter.record_span(a_span());

        exporter.shutdown().await;

        fake.assert_sent("/v1/traces");
    }

    #[tokio::test]
    async fn the_exporters_own_requests_are_recognisable_so_they_are_not_measured() {
        assert!(!is_exporting());
        let (exporter, _) = exporter_with(
            Fake::new().fallback(FakeResponse::text("")),
            settings(),
        );
        exporter.record_span(a_span());

        // Anything the client reports during a flush happens inside the scope.
        exporter.flush().await;
        assert!(!is_exporting());
    }

    #[test]
    fn a_base_endpoint_gains_the_signal_path_without_doubling_the_slash() {
        assert_eq!(signal_endpoint("http://localhost:4318", "traces"), "http://localhost:4318/v1/traces");
        assert_eq!(signal_endpoint("http://localhost:4318/", "metrics"), "http://localhost:4318/v1/metrics");
    }

    #[test]
    fn only_overload_and_server_faults_are_retried() {
        assert!(is_retryable(Status(429)));
        assert!(is_retryable(Status(500)));
        assert!(is_retryable(Status(503)));
        assert!(!is_retryable(Status(400)));
        assert!(!is_retryable(Status(401)));
        assert!(!is_retryable(Status(404)));
    }

    #[test]
    fn backoff_grows_and_a_retry_after_header_is_capped() {
        assert_eq!(backoff_for(0), Duration::from_millis(250));
        assert_eq!(backoff_for(1), Duration::from_millis(500));
        assert!(backoff_for(20) < Duration::from_secs(30));

        let mut headers = rustlavel_http::Headers::new();
        headers.set("retry-after", "600");
        let response = rustlavel_client::ClientResponse {
            status: Status(429),
            headers,
            body: Vec::new(),
        };
        assert_eq!(retry_after(&response), Some(Duration::from_secs(30)));
    }

    #[test]
    fn the_protocol_is_read_from_the_names_the_specification_uses() {
        assert_eq!(Protocol::parse("http/protobuf"), Some(Protocol::Protobuf));
        assert_eq!(Protocol::parse("HTTP/JSON"), Some(Protocol::Json));
        assert_eq!(Protocol::parse("grpc"), None);
    }
}
