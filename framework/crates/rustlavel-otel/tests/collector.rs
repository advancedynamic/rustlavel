//! Export against a real OpenTelemetry Collector.
//!
//! The unit tests assert what the *bytes* look like. These assert that a
//! collector accepts them and — the part that actually matters — that it parsed
//! them into the span and metric that were meant. A collector answers 200 the
//! moment it can unmarshal the request; a field number written into the wrong
//! slot unmarshals perfectly and produces a span with the wrong name, no
//! attributes, or a timestamp in 2554. "It returned 200" proves almost nothing,
//! so every test here reads the collector's own log and looks for the values it
//! sent.
//!
//! They run only when both variables are set, so `cargo test` stays green on a
//! machine with no collector.
//!
//! ```text
//! cat > /tmp/otelcol.yaml <<'YAML'
//! receivers:
//!   otlp:
//!     protocols:
//!       http:
//!         endpoint: 0.0.0.0:4318
//!
//! exporters:
//!   debug:
//!     verbosity: detailed
//!
//! service:
//!   pipelines:
//!     traces:
//!       receivers: [otlp]
//!       exporters: [debug]
//!     metrics:
//!       receivers: [otlp]
//!       exporters: [debug]
//! YAML
//!
//! docker run -d --name rustlavel-otel -p 4318:4318 \
//!   -v /tmp/otelcol.yaml:/etc/otelcol/config.yaml \
//!   otel/opentelemetry-collector:latest
//!
//! export OTEL_TEST_ENDPOINT=http://localhost:4318
//! export OTEL_TEST_CONTAINER=rustlavel-otel
//! cargo test -p rustlavel-otel --test collector
//!
//! docker rm -f rustlavel-otel
//! ```
//!
//! `verbosity: detailed` is what makes the debug exporter print span names,
//! attributes and data points rather than a count. Without it the log says
//! "1 span" and proves nothing.

use rustlavel_client::Client;
use rustlavel_core::{Config, Event};
use rustlavel_http::{Request, Router, TestClient};
use rustlavel_otel::exporter::{Protocol, signal_endpoint};
use rustlavel_otel::metrics::{Meter, instruments};
use rustlavel_otel::plugin::OpenTelemetry;
use rustlavel_otel::resource::{Resource, Value};
use rustlavel_otel::trace::{Span, SpanContext, SpanKind, SpanStatus};
use std::process::Command;
use std::time::Duration;

/// The collector's base URL, or a skip.
macro_rules! collector {
    () => {
        match (std::env::var("OTEL_TEST_ENDPOINT"), std::env::var("OTEL_TEST_CONTAINER")) {
            (Ok(endpoint), Ok(container)) if !endpoint.is_empty() && !container.is_empty() => {
                (endpoint, container)
            }
            _ => {
                eprintln!(
                    "skipping: OTEL_TEST_ENDPOINT and OTEL_TEST_CONTAINER are not both set"
                );
                return;
            }
        }
    };
}

/// A marker unique to one test run.
///
/// The collector's log is shared by every test in this file and by any earlier
/// run against the same container, so each test has to be able to find its own
/// telemetry in it. Rule six, applied to a fixture that happens to be a
/// container rather than a directory.
fn marker(prefix: &str) -> String {
    let context = SpanContext::root();
    format!("{prefix}-{}", &context.span_id.to_hex()[..8])
}

/// POST one payload and insist on a 2xx.
async fn post(endpoint: &str, protocol: Protocol, body: Vec<u8>) {
    let content_type = match protocol {
        Protocol::Protobuf => "application/x-protobuf",
        Protocol::Json => "application/json",
    };

    let response = Client::new()
        .timeout(Duration::from_secs(10))
        .post(endpoint)
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .expect("the collector should be reachable");

    assert!(
        response.is_success(),
        "the collector refused the payload with {}: {}",
        response.status,
        response.text()
    );
}

/// One collector, one log, four tests.
///
/// Every test here posts to the same container and then reads `docker logs` to
/// see what it made of the payload — a fixture that is process-wide in the way
/// rule six in CLAUDE.md is about. Each test already anchors on its own service
/// name, which is enough when they run one at a time; under the whole workspace
/// it is not, because the log interleaves and a window anchored on one service
/// can swallow another's metrics, which share these names.
///
/// So they take turns. It costs a few seconds and buys a suite that does not
/// fail once a run for a reason nobody can reproduce — and a nightly job that
/// is flaky is a nightly job people learn to ignore.
static COLLECTOR: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A poisoned guard is still a guard: one test panicking must not turn the rest
/// into a different, more confusing failure.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    COLLECTOR.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Everything the collector has logged so far.
fn logs(container: &str) -> String {
    let output = Command::new("docker")
        .args(["logs", container])
        .output()
        .expect("docker logs should run; is docker on PATH?");

    // The collector logs to stderr and the debug exporter prints to stdout.
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Wait until every needle appears in the collector's log.
///
/// Polls rather than sleeping a fixed time: the debug exporter flushes on its
/// own schedule, and a fixed sleep is either flaky or slow.
async fn await_logged(container: &str, needles: &[&str]) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);

    loop {
        let output = logs(container);
        if needles.iter().all(|needle| output.contains(needle)) {
            return output;
        }
        if std::time::Instant::now() >= deadline {
            let missing: Vec<&&str> =
                needles.iter().filter(|needle| !output.contains(**needle)).collect();
            panic!(
                "the collector never logged {missing:?}. It answered 200, so it unmarshalled the \
                 payload — but what it parsed is not what was sent. Recent log:\n{}",
                tail(&output, 80)
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// A span with the shape a real request produces: a parent, attributes of three
/// different types, a status, and a duration.
fn sample_span(name: &str, parent: SpanContext) -> Span {
    let start = std::time::SystemTime::now() - Duration::from_millis(42);
    Span::new(parent.child(), name, SpanKind::Server)
        .parent(parent.span_id)
        .between(start, start + Duration::from_millis(42))
        .with("http.request.method", "GET")
        .with("http.route", "/orders/{id}")
        .with("http.response.status_code", 200u16)
        .with("rustlavel.cached", false)
        .status(SpanStatus::Ok)
}

#[tokio::test]
async fn the_collector_parses_a_protobuf_span_into_the_span_that_was_sent() {
    let (endpoint, container) = collector!();
    let _turn = exclusive();
    let name = marker("protobuf-span");
    let parent = SpanContext::root();
    let span = sample_span(&name, parent);
    let trace_id = span.context.trace_id.to_hex();
    let span_id = span.context.span_id.to_hex();

    let resource = Resource::new("rustlavel-protobuf-test").with("service.version", "9.9.9");
    let body = rustlavel_otel::trace::export_request(&resource, &[span]);

    post(&signal_endpoint(&endpoint, "traces"), Protocol::Protobuf, body).await;

    // Every one of these is a separate field number in the encoder. If any is
    // wrong, the collector still says 200 and this is what catches it.
    let output = await_logged(
        &container,
        &[
            &name,
            // The resource reached the collector intact.
            "rustlavel-protobuf-test",
            "service.version",
            // Ids survived as 16 and 8 raw bytes rather than as text.
            &trace_id,
            &span_id,
            &parent.span_id.to_hex(),
            // Attributes of three different AnyValue types.
            "http.route",
            "/orders/{id}",
            "Int(200)",
            "Bool(false)",
            // The scope this crate attributes its telemetry to.
            "rustlavel-otel",
        ],
    )
    .await;

    let block = around(&output, &name);
    assert!(field_says(&block, "Kind", "Server"), "wrong span kind:\n{block}");
    assert!(field_says(&block, "Status code", "Ok"), "wrong status:\n{block}");

    // And the timestamps were read as a start and an end 42ms apart, rather
    // than as a pair of nonsense values that happen to have parsed.
    assert!(block.contains("Start time"), "no start time near the span:\n{block}");
    assert!(block.contains("End time"), "no end time near the span:\n{block}");
    // A `fixed64` misread as a varint lands centuries away; this is the cheap
    // proof that it did not.
    assert!(
        block.contains("20") && !block.contains("1754-") && !block.contains("2554-"),
        "the timestamps did not decode to a plausible date:\n{block}"
    );
}

#[tokio::test]
async fn the_collector_parses_a_json_span_too() {
    let (endpoint, container) = collector!();
    let _turn = exclusive();
    let name = marker("json-span");
    let parent = SpanContext::root();
    let span = sample_span(&name, parent);
    let trace_id = span.context.trace_id.to_hex();

    let resource = Resource::new("rustlavel-json-test");
    let body = rustlavel_otel::trace::export_request_json(&resource, &[span])
        .to_string()
        .into_bytes();

    post(&signal_endpoint(&endpoint, "traces"), Protocol::Json, body).await;

    let output = await_logged(
        &container,
        &[
            &name,
            "rustlavel-json-test",
            // Hex ids in JSON have to decode to the same bytes protobuf sent.
            &trace_id,
            "Int(200)",
            // A false boolean is the one an unset `oneof` arm would silently
            // turn into `Empty()`.
            "Bool(false)",
        ],
    )
    .await;

    let block = around(&output, &name);
    assert!(field_says(&block, "Kind", "Server"), "wrong span kind:\n{block}");
    assert!(field_says(&block, "Status code", "Ok"), "wrong status:\n{block}");
}

#[tokio::test]
async fn the_collector_parses_a_histogram_with_the_buckets_it_was_given() {
    let (endpoint, container) = collector!();
    let _turn = exclusive();
    let service = marker("metrics-service");

    let meter = Meter::new();
    // Three observations that land in three different buckets, so a cumulative
    // bug (Prometheus semantics leaking in) or an off-by-one in the bucket
    // array shows up as the wrong count rather than as nothing at all.
    for seconds in [0.003, 0.02, 30.0] {
        meter.record(
            instruments::HTTP_SERVER_DURATION,
            vec![("http.route".to_string(), Value::from("/orders/{id}"))],
            seconds,
        );
    }
    meter.add(
        instruments::AI_TOKENS,
        vec![("gen_ai.token.type".to_string(), Value::from("input"))],
        1234,
    );
    meter.set(instruments::QUEUE_DEPTH, Vec::new(), 7.0);

    let resource = Resource::new(&service);
    let body = meter.export_request(&resource).expect("a payload");

    post(&signal_endpoint(&endpoint, "metrics"), Protocol::Protobuf, body).await;

    let output = await_logged(
        &container,
        &[
            &service,
            "http.server.request.duration",
            "gen_ai.client.token.usage",
            "rustlavel.otel.queue.spans",
            // The sum's value, the gauge's value, and the histogram's count.
            "1234",
            "AggregationTemporality: Cumulative",
        ],
    )
    .await;

    // Anchored on this run's own service name: the collector's log holds every
    // other test's metrics too, and they share these metric names.
    let mine = section(&output, &service, 200);
    let histogram = section(&mine, "Name: http.server.request.duration", 60);
    assert!(histogram.contains("Count: 3"), "the histogram lost observations:\n{histogram}");
    assert!(
        histogram.contains("ExplicitBounds #0: 0.005"),
        "the bucket boundaries did not survive:\n{histogram}"
    );
    // One in the first bucket, one in the third, one in the overflow: bucket
    // counts are per-bucket in OTLP, and a cumulative encoding would show 1, 2,
    // 3 climbing instead.
    assert!(
        histogram.contains("Buckets #0, Count: 1"),
        "the first bucket is wrong:\n{histogram}"
    );
    assert!(
        histogram.contains("Buckets #11, Count: 1"),
        "the overflow bucket is wrong; there must be one more count than bound:\n{histogram}"
    );
    assert!(histogram.contains("Sum: 30.023"), "the sum is wrong:\n{histogram}");
}

/// The whole path an application actually uses: a request through the router,
/// a query dispatched from inside the handler, and both spans shipped by the
/// plugin's own exporter.
#[tokio::test]
async fn a_request_and_the_query_inside_it_arrive_as_one_trace() {
    let (endpoint, container) = collector!();
    let _turn = exclusive();
    let service = marker("plugin-service");
    let table = marker("orders").replace('-', "_");

    let config = Config::new();
    config.set("app.name", service.as_str());
    config.set("otel.endpoint", endpoint.as_str());
    config.set("otel.protocol", "http/protobuf");
    config.set("otel.headers", "");

    let mut router = Router::new();
    let query = format!("select * from {table} where id = $1");
    router.get("/orders/{id}", move |_request: Request| {
        let query = query.clone();
        async move {
            // What a database driver dispatches from inside a handler.
            Event::new("db.query")
                .with("sql", query)
                .with("rows", 1)
                .with("ok", true)
                .took(Duration::from_millis(3))
                .dispatch();
            "an order"
        }
    });

    let exporter =
        OpenTelemetry::new().install(&mut router, &config).expect("the plugin should mount");

    // An upstream trace this service has to continue rather than replace.
    let upstream = SpanContext::root();
    let response = TestClient::new(router)
        .send(
            Request::new(rustlavel_http::Method::Get, "/orders/7")
                .with_header("traceparent", upstream.traceparent()),
        )
        .await;
    assert_eq!(response.status(), 200);

    exporter.shutdown().await;
    rustlavel_core::events::clear_subscribers();

    let output = await_logged(
        &container,
        &[
            &service,
            "GET /orders/{id}",
            &format!("SELECT {table}"),
            &upstream.trace_id.to_hex(),
        ],
    )
    .await;

    // Both spans carry the upstream trace id, which is the whole point of
    // reading `traceparent`: one trace across two services, not two traces.
    let request_span = around(&output, "GET /orders/{id}");
    let query_span = around(&output, &format!("SELECT {table}"));
    for (label, block) in [("request", &request_span), ("query", &query_span)] {
        assert!(
            block.contains(&upstream.trace_id.to_hex()),
            "the {label} span is not in the upstream trace:\n{block}"
        );
    }
    assert!(
        request_span.contains(&upstream.span_id.to_hex()),
        "the request span is not parented to the caller:\n{request_span}"
    );
    assert!(
        query_span.contains("db.query.text"),
        "the statement did not reach the collector:\n{query_span}"
    );
    // The path is an attribute; the *name* stays the pattern.
    assert!(request_span.contains("/orders/7"), "url.path is missing:\n{request_span}");

    // Nothing about the export itself may have been rejected along the way.
    assert!(
        !output.contains("Permanent error") && !output.contains("failed to unmarshal"),
        "the collector reported a rejection:\n{}",
        tail(&output, 40)
    );
}

/// The lines around a needle, which is where the debug exporter prints the ids
/// above a span's name and the attributes below it.
fn around(output: &str, needle: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let at = locate(&lines, needle);

    lines[at.saturating_sub(12)..(at + 25).min(lines.len())].join("\n")
}

/// The lines from a needle forward, for a block whose interesting part is all
/// underneath it.
fn section(output: &str, needle: &str, lines: usize) -> String {
    let all: Vec<&str> = output.lines().collect();
    let at = locate(&all, needle);

    all[at..(at + lines).min(all.len())].join("\n")
}

/// The *last* match: the collector's log accumulates across runs, and the
/// telemetry this test sent is the most recent.
fn locate(lines: &[&str], needle: &str) -> usize {
    lines
        .iter()
        .rposition(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not in the collector log"))
}

/// Whether a `Name : Value` line in the debug exporter's output says what it
/// should. Matched by field rather than by exact string because the exporter
/// pads its labels to a column, and a padding change is not a regression.
fn field_says(block: &str, field: &str, value: &str) -> bool {
    block
        .lines()
        .any(|line| line.trim_start().starts_with(field) && line.contains(value))
}
