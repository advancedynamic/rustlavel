//! Counters, gauges and histograms, aggregated in process and shipped as OTLP.
//!
//! The same events that become spans become metrics, because the two answer
//! different questions about the same work: a trace explains one slow request,
//! a metric says how often requests are slow. Nothing is instrumented twice.
//!
//! Aggregation happens here rather than at the collector. A span per request is
//! affordable; a data point per request is not, and an exporter that sent one
//! would spend more bandwidth on telemetry than the application spends on
//! traffic.

use crate::protobuf::Encoder;
use crate::resource::{
    Attributes, Resource, Value, attributes_json, encode_attributes, scope, scope_json, unix_nanos,
};
use rustlavel_core::{Event, Json};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

/// Explicit bucket boundaries, in seconds.
///
/// The same boundaries `rustlavel-metrics` uses for its Prometheus histograms,
/// deliberately: an application that exports both should not see two different
/// p95s for the same traffic and have to work out which one to believe.
pub const BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// A metric's identity: what it is called, what it is measured in, and what it
/// means. Held as constants so a name and its unit cannot drift apart between
/// two call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instrument {
    pub name: &'static str,
    pub unit: &'static str,
    pub description: &'static str,
}

/// The instruments the framework's own events feed.
///
/// Names follow the OpenTelemetry semantic conventions wherever there is one,
/// so a stock dashboard finds them. Where there is none — the queue, MCP, and
/// the exporter's own health — the name is namespaced under `rustlavel` rather
/// than invented in someone else's namespace.
pub mod instruments {
    use super::Instrument;

    pub const HTTP_SERVER_DURATION: Instrument = Instrument {
        name: "http.server.request.duration",
        unit: "s",
        description: "Duration of inbound HTTP requests.",
    };
    pub const HTTP_CLIENT_DURATION: Instrument = Instrument {
        name: "http.client.request.duration",
        unit: "s",
        description: "Duration of outbound HTTP requests.",
    };
    pub const DB_DURATION: Instrument = Instrument {
        name: "db.client.operation.duration",
        unit: "s",
        description: "Duration of database queries.",
    };
    pub const AI_DURATION: Instrument = Instrument {
        name: "gen_ai.client.operation.duration",
        unit: "s",
        description: "Duration of model calls.",
    };
    /// Token usage is a `Sum` here, where the semantic conventions ask for a
    /// histogram. A histogram of token counts answers a question nobody has;
    /// the total spend per model, which is what a bill is made of, needs a sum.
    pub const AI_TOKENS: Instrument = Instrument {
        name: "gen_ai.client.token.usage",
        unit: "{token}",
        description: "Tokens consumed by model calls.",
    };
    pub const MCP_DURATION: Instrument = Instrument {
        name: "rustlavel.mcp.tool.duration",
        unit: "s",
        description: "Duration of MCP tool calls.",
    };
    pub const QUEUE_DURATION: Instrument = Instrument {
        name: "rustlavel.queue.job.duration",
        unit: "s",
        description: "Duration of processed queue jobs.",
    };
    pub const QUEUE_JOBS: Instrument = Instrument {
        name: "rustlavel.queue.jobs",
        unit: "{job}",
        description: "Queue jobs by outcome.",
    };
    /// The exporter's own health. Telemetry that drops silently is worse than
    /// no telemetry, because a flat graph reads as a quiet service.
    pub const SPANS_DROPPED: Instrument = Instrument {
        name: "rustlavel.otel.spans.dropped",
        unit: "{span}",
        description: "Spans discarded because the export queue was full.",
    };
    pub const QUEUE_DEPTH: Instrument = Instrument {
        name: "rustlavel.otel.queue.spans",
        unit: "{span}",
        description: "Spans waiting to be exported.",
    };
}

/// One bucketed distribution.
struct Histogram {
    /// One slot per bucket plus a final overflow slot.
    ///
    /// These are *not* cumulative, which is the trap when coming from
    /// Prometheus: OTLP counts each observation in exactly one bucket and the
    /// consumer accumulates. Filling them cumulatively here would multiply
    /// every count by the number of buckets above it.
    counts: Vec<u64>,
    sum: f64,
    total: u64,
    min: f64,
    max: f64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            counts: vec![0; BUCKETS.len() + 1],
            sum: 0.0,
            total: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }
}

impl Histogram {
    fn observe(&mut self, value: f64) {
        let index = BUCKETS.iter().position(|edge| value <= *edge).unwrap_or(BUCKETS.len());
        self.counts[index] += 1;
        self.sum += value;
        self.total += 1;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }
}

/// Every data point recorded for one instrument, keyed by its attribute set.
struct Series<P> {
    instrument: Instrument,
    points: BTreeMap<Attributes, P>,
}

impl<P> Series<P> {
    fn new(instrument: Instrument) -> Self {
        Series { instrument, points: BTreeMap::new() }
    }
}

#[derive(Default)]
struct Store {
    sums: BTreeMap<&'static str, Series<u64>>,
    gauges: BTreeMap<&'static str, Series<f64>>,
    histograms: BTreeMap<&'static str, Series<Histogram>>,
}

/// The aggregated metrics, cheap to clone and safe to share.
#[derive(Clone)]
pub struct Meter {
    inner: Arc<Inner>,
}

struct Inner {
    store: RwLock<Store>,
    /// Cumulative temporality needs a fixed start, and this is it: the moment
    /// the process began collecting. A backend uses it to tell a genuine reset
    /// (a restart, with a later start time) from a counter that went backwards
    /// because something is wrong.
    start: SystemTime,
}

impl Default for Meter {
    fn default() -> Self {
        Meter::new()
    }
}

impl Meter {
    pub fn new() -> Meter {
        Meter {
            inner: Arc::new(Inner { store: RwLock::new(Store::default()), start: SystemTime::now() }),
        }
    }

    /// Add to a monotonic counter.
    pub fn add(&self, instrument: Instrument, attributes: Attributes, amount: u64) {
        let mut store = self.inner.store.write().expect("meter lock poisoned");
        let series = store.sums.entry(instrument.name).or_insert_with(|| Series::new(instrument));
        *series.points.entry(attributes).or_insert(0) += amount;
    }

    /// Replace a gauge's value.
    pub fn set(&self, instrument: Instrument, attributes: Attributes, value: f64) {
        let mut store = self.inner.store.write().expect("meter lock poisoned");
        let series = store.gauges.entry(instrument.name).or_insert_with(|| Series::new(instrument));
        series.points.insert(attributes, value);
    }

    /// Observe one value into a histogram.
    pub fn record(&self, instrument: Instrument, attributes: Attributes, value: f64) {
        let mut store = self.inner.store.write().expect("meter lock poisoned");
        let series =
            store.histograms.entry(instrument.name).or_insert_with(|| Series::new(instrument));
        series.points.entry(attributes).or_default().observe(value);
    }

    /// The current value of a counter, for tests and health pages.
    pub fn counter(&self, instrument: Instrument, attributes: &Attributes) -> u64 {
        let store = self.inner.store.read().expect("meter lock poisoned");
        store
            .sums
            .get(instrument.name)
            .and_then(|series| series.points.get(attributes))
            .copied()
            .unwrap_or(0)
    }

    /// How many observations a histogram has taken.
    pub fn observations(&self, instrument: Instrument, attributes: &Attributes) -> u64 {
        let store = self.inner.store.read().expect("meter lock poisoned");
        store
            .histograms
            .get(instrument.name)
            .and_then(|series| series.points.get(attributes))
            .map(|histogram| histogram.total)
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        let store = self.inner.store.read().expect("meter lock poisoned");
        store.sums.is_empty() && store.gauges.is_empty() && store.histograms.is_empty()
    }

    pub fn clear(&self) {
        *self.inner.store.write().expect("meter lock poisoned") = Store::default();
    }

    fn start(&self) -> SystemTime {
        self.inner.start
    }

    /// Encode everything collected so far as an `ExportMetricsServiceRequest`.
    ///
    /// Cumulative temporality means a snapshot is a full statement of the
    /// process's history, not a delta, so nothing is consumed and a flush that
    /// fails to reach the collector loses nothing but resolution.
    pub fn export_request(&self, resource: &Resource) -> Option<Vec<u8>> {
        let store = self.inner.store.read().expect("meter lock poisoned");
        let now = unix_nanos(SystemTime::now());
        let start = unix_nanos(self.start());

        let mut scope_metrics = Encoder::new();
        let mut any = false;
        scope_metrics.message(1, &scope());

        for series in store.sums.values() {
            any = true;
            scope_metrics.message(2, &encode_sum(series, start, now));
        }
        for series in store.gauges.values() {
            any = true;
            scope_metrics.message(2, &encode_gauge(series, start, now));
        }
        for series in store.histograms.values() {
            any = true;
            scope_metrics.message(2, &encode_histogram(series, start, now));
        }

        if !any {
            return None;
        }

        let mut resource_metrics = Encoder::new();
        resource_metrics.message(1, &resource.encode());
        resource_metrics.message(2, &scope_metrics);

        let mut request = Encoder::new();
        request.message(1, &resource_metrics);
        Some(request.into_bytes())
    }

    /// The same snapshot under the OTLP/JSON mapping.
    pub fn export_request_json(&self, resource: &Resource) -> Option<Json> {
        let store = self.inner.store.read().expect("meter lock poisoned");
        let now = unix_nanos(SystemTime::now()).to_string();
        let start = unix_nanos(self.start()).to_string();

        let mut metrics = Vec::new();
        for series in store.sums.values() {
            metrics.push(sum_json(series, &start, &now));
        }
        for series in store.gauges.values() {
            metrics.push(gauge_json(series, &start, &now));
        }
        for series in store.histograms.values() {
            metrics.push(histogram_json(series, &start, &now));
        }

        if metrics.is_empty() {
            return None;
        }

        Some(Json::object([(
            "resourceMetrics",
            Json::Array(vec![Json::object([
                ("resource", resource.to_json()),
                (
                    "scopeMetrics",
                    Json::Array(vec![Json::object([
                        ("scope", scope_json()),
                        ("metrics", Json::Array(metrics)),
                    ])]),
                ),
            ])]),
        )]))
    }
}

/// `AGGREGATION_TEMPORALITY_CUMULATIVE`.
///
/// Cumulative rather than delta because it is the temporality that survives a
/// lost batch: a dropped delta is a permanent hole in a counter, while a
/// dropped cumulative point is corrected by the next one.
const CUMULATIVE: i32 = 2;

fn metric_header(instrument: Instrument) -> Encoder {
    let mut metric = Encoder::new();
    metric.string(1, instrument.name);
    metric.string(2, instrument.description);
    metric.string(3, instrument.unit);
    metric
}

fn encode_sum(series: &Series<u64>, start: u64, now: u64) -> Encoder {
    let mut sum = Encoder::new();
    for (attributes, value) in &series.points {
        let mut point = Encoder::new();
        point.fixed64(2, start);
        point.fixed64(3, now);
        // as_int is `sfixed64`, field 6 — a count is an integer and sending it
        // as a double would round once it passes 2^53. Written even at zero:
        // `NumberDataPoint.value` is a `oneof`, so a skipped default leaves the
        // point with no value rather than with the value zero.
        point.sfixed64_present(6, *value as i64);
        encode_attributes(&mut point, 7, attributes);
        sum.message(1, &point);
    }
    sum.enumeration(2, CUMULATIVE);
    sum.bool(3, true);

    let mut metric = metric_header(series.instrument);
    metric.message(7, &sum);
    metric
}

fn encode_gauge(series: &Series<f64>, start: u64, now: u64) -> Encoder {
    let mut gauge = Encoder::new();
    for (attributes, value) in &series.points {
        let mut point = Encoder::new();
        point.fixed64(2, start);
        point.fixed64(3, now);
        // See `encode_sum`: the value is a `oneof` member, so a gauge reading
        // of zero has to be written rather than skipped.
        point.present_double(4, *value);
        encode_attributes(&mut point, 7, attributes);
        gauge.message(1, &point);
    }

    let mut metric = metric_header(series.instrument);
    metric.message(5, &gauge);
    metric
}

fn encode_histogram(series: &Series<Histogram>, start: u64, now: u64) -> Encoder {
    let mut histogram = Encoder::new();
    for (attributes, distribution) in &series.points {
        let mut point = Encoder::new();
        point.fixed64(2, start);
        point.fixed64(3, now);
        point.fixed64(4, distribution.total);
        // `sum` has explicit presence: a histogram whose observations happen to
        // total zero still reported a sum, and dropping the field would say it
        // did not.
        point.present_double(5, distribution.sum);
        point.packed_fixed64(6, &distribution.counts);
        point.packed_double(7, BUCKETS);
        encode_attributes(&mut point, 9, attributes);
        if distribution.total > 0 {
            point.present_double(11, distribution.min);
            point.present_double(12, distribution.max);
        }
        histogram.message(1, &point);
    }
    histogram.enumeration(2, CUMULATIVE);

    let mut metric = metric_header(series.instrument);
    metric.message(9, &histogram);
    metric
}

fn metric_header_json(instrument: Instrument, body: (&str, Json)) -> Json {
    Json::object([
        ("name", Json::from(instrument.name)),
        ("description", Json::from(instrument.description)),
        ("unit", Json::from(instrument.unit)),
        (body.0, body.1),
    ])
}

fn sum_json(series: &Series<u64>, start: &str, now: &str) -> Json {
    let points = series
        .points
        .iter()
        .map(|(attributes, value)| {
            Json::object([
                ("attributes", attributes_json(attributes)),
                ("startTimeUnixNano", Json::from(start)),
                ("timeUnixNano", Json::from(now)),
                ("asInt", Json::from(value.to_string())),
            ])
        })
        .collect();

    metric_header_json(
        series.instrument,
        (
            "sum",
            Json::object([
                ("dataPoints", Json::Array(points)),
                ("aggregationTemporality", Json::Number(f64::from(CUMULATIVE))),
                ("isMonotonic", Json::from(true)),
            ]),
        ),
    )
}

fn gauge_json(series: &Series<f64>, start: &str, now: &str) -> Json {
    let points = series
        .points
        .iter()
        .map(|(attributes, value)| {
            Json::object([
                ("attributes", attributes_json(attributes)),
                ("startTimeUnixNano", Json::from(start)),
                ("timeUnixNano", Json::from(now)),
                ("asDouble", Json::Number(*value)),
            ])
        })
        .collect();

    metric_header_json(series.instrument, ("gauge", Json::object([("dataPoints", Json::Array(points))])))
}

fn histogram_json(series: &Series<Histogram>, start: &str, now: &str) -> Json {
    let points = series
        .points
        .iter()
        .map(|(attributes, distribution)| {
            let mut fields = vec![
                ("attributes", attributes_json(attributes)),
                ("startTimeUnixNano", Json::from(start)),
                ("timeUnixNano", Json::from(now)),
                ("count", Json::from(distribution.total.to_string())),
                ("sum", Json::Number(distribution.sum)),
                (
                    "bucketCounts",
                    Json::Array(
                        distribution.counts.iter().map(|c| Json::from(c.to_string())).collect(),
                    ),
                ),
                (
                    "explicitBounds",
                    Json::Array(BUCKETS.iter().map(|edge| Json::Number(*edge)).collect()),
                ),
            ];
            if distribution.total > 0 {
                fields.push(("min", Json::Number(distribution.min)));
                fields.push(("max", Json::Number(distribution.max)));
            }
            Json::object(fields)
        })
        .collect();

    metric_header_json(
        series.instrument,
        (
            "histogram",
            Json::object([
                ("dataPoints", Json::Array(points)),
                ("aggregationTemporality", Json::Number(f64::from(CUMULATIVE))),
            ]),
        ),
    )
}

/// Fold one instrumentation event into the meter.
pub fn record_event(meter: &Meter, event: &Event) {
    let seconds = event.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let text = |field: &str, fallback: &str| {
        event.field(field).and_then(Json::as_str).unwrap_or(fallback).to_string()
    };

    match event.kind {
        "http.request" => {
            // Labelled by route *pattern*, never by path. `/orders/1` and
            // `/orders/2` are one series under `/orders/{id}`; keyed by path
            // they would be two, and a busy application would create a new
            // series for every identifier it has ever served until the
            // collector's memory or the backend's index gives out.
            let route = event
                .field("route")
                .and_then(Json::as_str)
                .unwrap_or("unmatched")
                .to_string();
            let mut attributes = vec![
                ("http.request.method".to_string(), Value::String(text("method", "GET"))),
                ("http.route".to_string(), Value::String(route)),
            ];
            if let Some(status) = event.field("status").and_then(Json::as_i64) {
                attributes.push(("http.response.status_code".to_string(), Value::Int(status)));
            }
            meter.record(instruments::HTTP_SERVER_DURATION, attributes, seconds);
        }
        "http.client" => {
            let mut attributes =
                vec![("http.request.method".to_string(), Value::String(text("method", "GET")))];
            if let Some(status) = event.field("status").and_then(Json::as_i64) {
                attributes.push(("http.response.status_code".to_string(), Value::Int(status)));
            }
            meter.record(instruments::HTTP_CLIENT_DURATION, attributes, seconds);
        }
        "db.query" => {
            // No table or statement attribute: the cardinality argument that
            // applies to a request path applies twice over to SQL. The
            // statement is on the span, where it belongs.
            let ok = event.field("ok").and_then(Json::as_bool).unwrap_or(true);
            let attributes = vec![("rustlavel.ok".to_string(), Value::Bool(ok))];
            meter.record(instruments::DB_DURATION, attributes, seconds);
        }
        "ai.call" => {
            let attributes = vec![
                ("gen_ai.system".to_string(), Value::String(text("provider", "unknown"))),
                ("gen_ai.request.model".to_string(), Value::String(text("model", "unknown"))),
            ];
            meter.record(instruments::AI_DURATION, attributes.clone(), seconds);

            for (field, kind) in [("input_tokens", "input"), ("output_tokens", "output")] {
                if let Some(tokens) = event.field(field).and_then(Json::as_i64) {
                    let mut tagged = attributes.clone();
                    tagged.push(("gen_ai.token.type".to_string(), Value::String(kind.to_string())));
                    meter.add(instruments::AI_TOKENS, tagged, tokens.max(0) as u64);
                }
            }
        }
        "mcp.call" => {
            let attributes = vec![
                ("mcp.tool.name".to_string(), Value::String(text("tool", "unknown"))),
                (
                    "rustlavel.ok".to_string(),
                    Value::Bool(event.field("ok").and_then(Json::as_bool).unwrap_or(true)),
                ),
            ];
            meter.record(instruments::MCP_DURATION, attributes, seconds);
        }
        "queue.processed" | "queue.failed" => {
            let job = text("job", "unknown");
            let outcome = if event.kind == "queue.failed" { "failed" } else { "processed" };
            meter.add(
                instruments::QUEUE_JOBS,
                vec![
                    ("rustlavel.job".to_string(), Value::String(job.clone())),
                    ("rustlavel.outcome".to_string(), Value::String(outcome.to_string())),
                ],
                1,
            );
            if event.kind == "queue.processed" {
                meter.record(
                    instruments::QUEUE_DURATION,
                    vec![("rustlavel.job".to_string(), Value::String(job))],
                    seconds,
                );
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn attributes(pairs: &[(&str, &str)]) -> Attributes {
        pairs.iter().map(|(k, v)| (k.to_string(), Value::from(*v))).collect()
    }

    #[test]
    fn histogram_buckets_are_not_cumulative() {
        let meter = Meter::new();
        meter.record(instruments::DB_DURATION, Vec::new(), 0.003);
        meter.record(instruments::DB_DURATION, Vec::new(), 0.4);

        let store = meter.inner.store.read().expect("lock");
        let distribution = &store.histograms["db.client.operation.duration"].points[&Vec::new()];

        // 0.003 lands in the first bucket alone, 0.4 in the one for 0.5.
        assert_eq!(distribution.counts[0], 1);
        assert_eq!(distribution.counts[BUCKETS.iter().position(|e| *e == 0.5).unwrap()], 1);
        assert_eq!(distribution.counts.iter().sum::<u64>(), 2);
        assert_eq!(distribution.total, 2);
    }

    #[test]
    fn an_observation_past_every_boundary_lands_in_the_overflow_bucket() {
        let meter = Meter::new();
        meter.record(instruments::DB_DURATION, Vec::new(), 30.0);

        let store = meter.inner.store.read().expect("lock");
        let distribution = &store.histograms["db.client.operation.duration"].points[&Vec::new()];

        assert_eq!(distribution.counts[BUCKETS.len()], 1);
        assert_eq!(distribution.max, 30.0);
    }

    #[test]
    fn a_gauge_replaces_and_a_counter_accumulates() {
        let meter = Meter::new();
        meter.add(instruments::QUEUE_JOBS, Vec::new(), 2);
        meter.add(instruments::QUEUE_JOBS, Vec::new(), 3);
        meter.set(instruments::QUEUE_DEPTH, Vec::new(), 7.0);
        meter.set(instruments::QUEUE_DEPTH, Vec::new(), 4.0);

        assert_eq!(meter.counter(instruments::QUEUE_JOBS, &Vec::new()), 5);
        let store = meter.inner.store.read().expect("lock");
        assert_eq!(store.gauges["rustlavel.otel.queue.spans"].points[&Vec::new()], 4.0);
    }

    #[test]
    fn an_empty_meter_produces_no_payload_at_all() {
        let meter = Meter::new();

        assert!(meter.is_empty());
        assert!(meter.export_request(&Resource::new("x")).is_none());
        assert!(meter.export_request_json(&Resource::new("x")).is_none());
    }

    #[test]
    fn a_sum_encodes_as_a_monotonic_cumulative_integer_point() {
        let meter = Meter::new();
        meter.add(instruments::QUEUE_JOBS, Vec::new(), 5);
        let bytes = meter.export_request(&Resource::new("x")).expect("a payload");

        // Metric.sum is field 7, wire type 2.
        assert!(bytes.contains(&0x3a), "no sum field in {bytes:02x?}");
        // NumberDataPoint.as_int is sfixed64 field 6: (6 << 3) | 1 = 0x31.
        let mut expected = vec![0x31u8];
        expected.extend_from_slice(&5i64.to_le_bytes());
        assert!(bytes.windows(9).any(|w| w == expected), "{bytes:02x?}");
        // Sum.aggregation_temporality = 2 and is_monotonic = true.
        assert!(bytes.windows(2).any(|w| w == [0x10, 0x02]), "{bytes:02x?}");
        assert!(bytes.windows(2).any(|w| w == [0x18, 0x01]), "{bytes:02x?}");
    }

    /// `NumberDataPoint.value` is a `oneof` too, so a gauge sitting at zero has
    /// to carry a value rather than decode as one that was never set.
    #[test]
    fn a_zero_valued_point_still_carries_a_value() {
        let meter = Meter::new();
        meter.set(instruments::QUEUE_DEPTH, Vec::new(), 0.0);
        meter.add(instruments::SPANS_DROPPED, Vec::new(), 0);
        let bytes = meter.export_request(&Resource::new("x")).expect("a payload");

        // as_double, field 4, wire type 1: eight zero bytes behind tag 0x21.
        assert!(bytes.windows(9).any(|w| w == [0x21, 0, 0, 0, 0, 0, 0, 0, 0]), "{bytes:02x?}");
        // as_int, field 6, wire type 1: tag 0x31.
        assert!(bytes.windows(9).any(|w| w == [0x31, 0, 0, 0, 0, 0, 0, 0, 0]), "{bytes:02x?}");
    }

    #[test]
    fn a_histogram_sends_one_more_bucket_count_than_it_has_bounds() {
        let meter = Meter::new();
        meter.record(instruments::HTTP_SERVER_DURATION, Vec::new(), 0.02);
        let bytes = meter.export_request(&Resource::new("x")).expect("a payload");

        // bucket_counts is packed fixed64 in field 6: tag 0x32, then the byte
        // length. A collector rejects the point unless the array is exactly one
        // longer than explicit_bounds.
        let counts_length = (BUCKETS.len() + 1) * 8;
        assert!(
            bytes.windows(2).any(|w| w == [0x32, counts_length as u8]),
            "bucket_counts had the wrong length in {bytes:02x?}"
        );
        let bounds_length = BUCKETS.len() * 8;
        assert!(
            bytes.windows(2).any(|w| w == [0x3a, bounds_length as u8]),
            "explicit_bounds had the wrong length in {bytes:02x?}"
        );
    }

    #[test]
    fn the_json_mapping_renders_counts_as_strings_and_bounds_as_numbers() {
        let meter = Meter::new();
        meter.record(instruments::HTTP_SERVER_DURATION, Vec::new(), 0.02);
        let rendered = meter.export_request_json(&Resource::new("x")).expect("a payload").to_string();

        assert!(rendered.contains(r#""count":"1""#), "{rendered}");
        assert!(rendered.contains(r#""aggregationTemporality":2"#), "{rendered}");
        assert!(rendered.contains(r#""explicitBounds":[0.005"#), "{rendered}");
        assert!(rendered.contains(r#""bucketCounts":["#), "{rendered}");
    }

    #[test]
    fn requests_are_measured_by_route_pattern_rather_than_path() {
        let meter = Meter::new();
        for id in 1..=3 {
            meter_event(
                &meter,
                Event::new("http.request")
                    .with("method", "GET")
                    .with("path", format!("/orders/{id}"))
                    .with("route", "/orders/{id}")
                    .with("status", 200)
                    .took(Duration::from_millis(9)),
            );
        }

        let key: Attributes = vec![
            ("http.request.method".to_string(), Value::from("GET")),
            ("http.route".to_string(), Value::from("/orders/{id}")),
            ("http.response.status_code".to_string(), Value::Int(200)),
        ];
        assert_eq!(meter.observations(instruments::HTTP_SERVER_DURATION, &key), 3);

        // One series, not three.
        let store = meter.inner.store.read().expect("lock");
        assert_eq!(store.histograms["http.server.request.duration"].points.len(), 1);
    }

    #[test]
    fn an_unmatched_request_does_not_become_a_series_per_path() {
        let meter = Meter::new();
        for path in ["/a", "/b", "/c"] {
            meter_event(
                &meter,
                Event::new("http.request").with("method", "GET").with("path", path).with("status", 404),
            );
        }

        let store = meter.inner.store.read().expect("lock");
        assert_eq!(store.histograms["http.server.request.duration"].points.len(), 1);
    }

    #[test]
    fn model_calls_produce_a_duration_and_a_token_sum_per_direction() {
        let meter = Meter::new();
        meter_event(
            &meter,
            Event::new("ai.call")
                .with("provider", "anthropic")
                .with("model", "claude-sonnet-5")
                .with("input_tokens", 120)
                .with("output_tokens", 40)
                .took(Duration::from_millis(900)),
        );

        let base = attributes(&[
            ("gen_ai.system", "anthropic"),
            ("gen_ai.request.model", "claude-sonnet-5"),
        ]);
        assert_eq!(meter.observations(instruments::AI_DURATION, &base), 1);

        let mut input = base.clone();
        input.push(("gen_ai.token.type".to_string(), Value::from("input")));
        assert_eq!(meter.counter(instruments::AI_TOKENS, &input), 120);

        let mut output = base;
        output.push(("gen_ai.token.type".to_string(), Value::from("output")));
        assert_eq!(meter.counter(instruments::AI_TOKENS, &output), 40);
    }

    #[test]
    fn queue_outcomes_are_counted_separately() {
        let meter = Meter::new();
        meter_event(&meter, Event::new("queue.processed").with("job", "SendMail"));
        meter_event(&meter, Event::new("queue.failed").with("job", "SendMail"));

        assert_eq!(
            meter.counter(
                instruments::QUEUE_JOBS,
                &attributes(&[("rustlavel.job", "SendMail"), ("rustlavel.outcome", "failed")])
            ),
            1
        );
        assert_eq!(
            meter.observations(
                instruments::QUEUE_DURATION,
                &attributes(&[("rustlavel.job", "SendMail")])
            ),
            1
        );
    }

    #[test]
    fn an_unknown_event_kind_is_ignored_rather_than_guessed_at() {
        let meter = Meter::new();
        meter_event(&meter, Event::new("something.else").with("x", 1));

        assert!(meter.is_empty());
    }

    /// Fold an event without going near the process-wide bus, so these tests
    /// stay independent of every other test in the workspace.
    fn meter_event(meter: &Meter, event: Event) {
        record_event(meter, &event);
    }
}
