//! Spans: identity, W3C propagation, and the OTLP `ResourceSpans` payload.
//!
//! A span is only useful next to the spans around it, which makes propagation
//! the whole problem. Two halves solve it:
//!
//! * **Across processes**, the W3C `traceparent` header. A request that arrives
//!   carrying one continues that trace instead of starting a new one, which is
//!   the difference between a distributed trace and a pile of unrelated
//!   single-service traces that happen to be about the same click.
//! * **Within this process**, a task-local [`SpanContext`]. Every query, AI call
//!   and outbound request runs inside the request's own Tokio task, so the
//!   request span is ambient there and a child can find its parent without
//!   every intermediate API growing a context argument. The one place this does
//!   not reach is a `tokio::spawn` inside a handler — a spawned task starts
//!   with an empty task-local map, so work moved off the request loses the
//!   parent. [`in_span`] is public so that code can put it back.

use crate::protobuf::Encoder;
use crate::resource::{Attributes, Resource, Value, attributes_json, encode_attributes, scope, scope_json, unix_nanos};
use rustlavel_core::{Event, Json};
use std::future::Future;
use std::io::Read;
use std::time::SystemTime;

/// A 16-byte trace identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId([u8; 16]);

/// An 8-byte span identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId([u8; 8]);

impl TraceId {
    /// A fresh identifier.
    ///
    /// Retried until non-zero: an all-zero trace id is explicitly invalid in
    /// the W3C specification, and a collector is entitled to reject the whole
    /// batch that carries one.
    pub fn random() -> TraceId {
        loop {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&random_bytes(16));
            if bytes != [0u8; 16] {
                return TraceId(bytes);
            }
        }
    }

    pub fn from_hex(hex: &str) -> Option<TraceId> {
        let bytes: [u8; 16] = decode_hex(hex, 16)?.try_into().ok()?;
        (bytes != [0u8; 16]).then_some(TraceId(bytes))
    }

    pub fn to_hex(self) -> String {
        encode_hex(&self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl SpanId {
    pub fn random() -> SpanId {
        loop {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&random_bytes(8));
            if bytes != [0u8; 8] {
                return SpanId(bytes);
            }
        }
    }

    pub fn from_hex(hex: &str) -> Option<SpanId> {
        let bytes: [u8; 8] = decode_hex(hex, 8)?.try_into().ok()?;
        (bytes != [0u8; 8]).then_some(SpanId(bytes))
    }

    pub fn to_hex(self) -> String {
        encode_hex(&self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 8] {
        &self.0
    }
}

/// What identifies a span and gets handed to its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    /// The W3C sampled flag. An upstream that says "not sampled" is telling
    /// this service not to record either, and recording anyway produces a trace
    /// with a hole where the caller should be.
    pub sampled: bool,
}

impl SpanContext {
    /// The start of a new trace.
    pub fn root() -> SpanContext {
        SpanContext { trace_id: TraceId::random(), span_id: SpanId::random(), sampled: true }
    }

    /// A new span in the same trace, parented to this one.
    pub fn child(&self) -> SpanContext {
        SpanContext { trace_id: self.trace_id, span_id: SpanId::random(), sampled: self.sampled }
    }

    /// The `traceparent` header value for this context.
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{}",
            self.trace_id.to_hex(),
            self.span_id.to_hex(),
            if self.sampled { "01" } else { "00" }
        )
    }

    /// Read an incoming `traceparent`.
    ///
    /// Returns `None` for anything malformed, and the caller then starts a new
    /// trace. That is deliberately more forgiving than failing the request: a
    /// broken header from some proxy is not a reason to return a 400, it is a
    /// reason to lose the link to whatever sent it.
    ///
    /// Version `00` is the only one defined. A higher version is accepted for
    /// its first four fields and its extra ones ignored, which is what the
    /// specification asks of a forwards-compatible parser; version `ff` is
    /// invalid and refused.
    pub fn parse_traceparent(header: &str) -> Option<SpanContext> {
        let header = header.trim();
        let mut parts = header.split('-');

        let version = parts.next()?;
        if version.len() != 2 || version == "ff" || !version.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }

        let trace_id = TraceId::from_hex(parts.next()?)?;
        let span_id = SpanId::from_hex(parts.next()?)?;
        let flags = parts.next()?;
        if flags.len() != 2 || !flags.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        // Version 00 has exactly four fields; later versions may append more.
        if version == "00" && parts.next().is_some() {
            return None;
        }

        let sampled = u8::from_str_radix(flags, 16).ok()? & 0x01 == 0x01;
        Some(SpanContext { trace_id, span_id, sampled })
    }
}

tokio::task_local! {
    /// The span every child in this task should attach itself to.
    static CURRENT: SpanContext;
}

/// The span currently in scope, if any.
pub fn current() -> Option<SpanContext> {
    CURRENT.try_with(|context| *context).ok()
}

/// Run a future with `context` as the current span.
pub fn in_span<F: Future>(context: SpanContext, future: F) -> impl Future<Output = F::Output> {
    CURRENT.scope(context, future)
}

/// The `traceparent` to attach to an outbound request, so a downstream service
/// continues this trace rather than starting its own.
pub fn traceparent() -> Option<String> {
    current().map(|context| context.traceparent())
}

/// What kind of work a span describes. The numbers are OTLP's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Internal = 1,
    Server = 2,
    Client = 3,
    Producer = 4,
    Consumer = 5,
}

/// A span's outcome. `Unset` is not a synonym for success — it means nothing
/// claimed either way, and a backend will not colour it red.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanStatus {
    Unset,
    Ok,
    Error(String),
}

/// One finished unit of work.
///
/// Only finished spans exist here. Nothing streams a span that is still open,
/// so there is no half-built state to lose on a crash and no bookkeeping to get
/// wrong when a handler panics — the middleware records the span on the way out
/// either way.
#[derive(Debug, Clone)]
pub struct Span {
    pub context: SpanContext,
    pub parent: Option<SpanId>,
    pub name: String,
    pub kind: SpanKind,
    pub start: SystemTime,
    pub end: SystemTime,
    pub attributes: Attributes,
    pub status: SpanStatus,
}

impl Span {
    pub fn new(context: SpanContext, name: impl Into<String>, kind: SpanKind) -> Span {
        let now = SystemTime::now();
        Span {
            context,
            parent: None,
            name: name.into(),
            kind,
            start: now,
            end: now,
            attributes: Vec::new(),
            status: SpanStatus::Unset,
        }
    }

    pub fn parent(mut self, parent: SpanId) -> Span {
        self.parent = Some(parent);
        self
    }

    pub fn between(mut self, start: SystemTime, end: SystemTime) -> Span {
        self.start = start;
        self.end = end;
        self
    }

    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Span {
        self.attributes.push((key.into(), value.into()));
        self
    }

    pub fn status(mut self, status: SpanStatus) -> Span {
        self.status = status;
        self
    }

    fn encode(&self) -> Encoder {
        let mut span = Encoder::new();
        span.bytes_field(1, self.context.trace_id.as_bytes());
        span.bytes_field(2, self.context.span_id.as_bytes());
        if let Some(parent) = self.parent {
            span.bytes_field(4, parent.as_bytes());
        }
        span.string(5, &self.name);
        span.enumeration(6, self.kind as i32);
        span.fixed64(7, unix_nanos(self.start));
        span.fixed64(8, unix_nanos(self.end));
        encode_attributes(&mut span, 9, &self.attributes);

        match &self.status {
            // Field 15 is left out entirely for an unset status: an empty
            // `Status` message and no `Status` message decode the same, and the
            // shorter one is what other SDKs send.
            SpanStatus::Unset => {}
            SpanStatus::Ok => {
                let mut status = Encoder::new();
                status.enumeration(3, 1);
                span.message(15, &status);
            }
            SpanStatus::Error(message) => {
                let mut status = Encoder::new();
                status.string(2, message);
                status.enumeration(3, 2);
                span.message(15, &status);
            }
        }

        span
    }

    fn to_json(&self) -> Json {
        let mut fields = vec![
            // Trace and span ids are the one place OTLP/JSON departs from the
            // proto3 mapping: hex, not base64.
            ("traceId", Json::from(self.context.trace_id.to_hex())),
            ("spanId", Json::from(self.context.span_id.to_hex())),
            ("name", Json::from(self.name.clone())),
            ("kind", Json::Number(self.kind as i32 as f64)),
            // 64-bit integers travel as strings, for the same reason attribute
            // integers do.
            ("startTimeUnixNano", Json::from(unix_nanos(self.start).to_string())),
            ("endTimeUnixNano", Json::from(unix_nanos(self.end).to_string())),
            ("attributes", attributes_json(&self.attributes)),
        ];

        if let Some(parent) = self.parent {
            fields.push(("parentSpanId", Json::from(parent.to_hex())));
        }
        match &self.status {
            SpanStatus::Unset => {}
            SpanStatus::Ok => fields.push(("status", Json::object([("code", Json::Number(1.0))]))),
            SpanStatus::Error(message) => fields.push((
                "status",
                Json::object([
                    ("code", Json::Number(2.0)),
                    ("message", Json::from(message.clone())),
                ]),
            )),
        }

        Json::object(fields)
    }
}

/// Turn one bus event into a child of the span in scope.
///
/// The bus reports work that has already happened: `Event::new` runs at the end
/// of the query or the call, so `event.at` is the *finish* and the start is
/// that minus the recorded duration. Reading it the other way round is the easy
/// mistake, and it puts every child span one duration later than the request
/// that contains it.
///
/// `http.request` is deliberately absent. The plugin's middleware already
/// records the request span, with the response in hand and with the incoming
/// `traceparent` honoured; building a second one here would double every
/// request in the trace view.
pub fn span_from_event(event: &Event, parent: SpanContext) -> Option<Span> {
    let duration = event.duration.unwrap_or_default();
    let end = event.at;
    let start = end.checked_sub(duration).unwrap_or(end);
    let context = parent.child();

    let mut span = match event.kind {
        "db.query" => {
            let statement = event.field("sql").and_then(Json::as_str).unwrap_or("query");
            Span::new(context, database_span_name(statement), SpanKind::Client)
                .with("db.query.text", statement)
        }
        "http.client" => {
            let method = event.field("method").and_then(Json::as_str).unwrap_or("GET");
            Span::new(context, method.to_string(), SpanKind::Client)
                .with("http.request.method", method)
        }
        "ai.call" => {
            let model = event.field("model").and_then(Json::as_str).unwrap_or("model");
            Span::new(context, format!("chat {model}"), SpanKind::Client)
        }
        "mcp.call" => {
            let tool = event.field("tool").and_then(Json::as_str).unwrap_or("tool");
            Span::new(context, format!("mcp {tool}"), SpanKind::Client)
        }
        "queue.processed" | "queue.failed" => {
            let job = event.field("job").and_then(Json::as_str).unwrap_or("job");
            Span::new(context, format!("process {job}"), SpanKind::Consumer)
        }
        _ => return None,
    };

    span = span.parent(parent.span_id).between(start, end);

    // Everything else the emitter chose to report becomes an attribute. Fields
    // already spent on the span's name or a dedicated attribute are skipped so
    // the same string is not carried twice in every payload.
    for (key, value) in &event.fields {
        if matches!(key.as_str(), "sql" | "method") {
            continue;
        }
        span.attributes.push((attribute_name(event.kind, key), Value::from_json(value)));
    }

    let failed = event.field("ok").and_then(Json::as_bool) == Some(false)
        || event.kind == "queue.failed"
        || event.field("status").and_then(Json::as_i64).is_some_and(|code| code >= 400);
    if failed {
        let message = event
            .field("error")
            .and_then(Json::as_str)
            .unwrap_or("the operation reported a failure")
            .to_string();
        span = span.status(SpanStatus::Error(message));
    }

    Some(span)
}

/// A span name has to be low-cardinality — it is what a backend groups by — so
/// a query becomes its verb and its table rather than its full text. The text
/// itself is kept as an attribute, where high cardinality costs storage instead
/// of making the trace view useless.
fn database_span_name(sql: &str) -> String {
    let mut words = sql.split_whitespace();
    let verb = words.next().unwrap_or("query").to_uppercase();
    let target = match verb.as_str() {
        "SELECT" | "DELETE" => words.find(|word| word.eq_ignore_ascii_case("from")).and(words.next()),
        "INSERT" | "REPLACE" => words.find(|word| word.eq_ignore_ascii_case("into")).and(words.next()),
        "UPDATE" => words.next(),
        _ => None,
    };

    match target {
        Some(table) => format!("{verb} {}", table.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')),
        None => verb,
    }
}

/// Namespace an event field so `ok` from a query and `ok` from a tool call do
/// not become the same attribute with two meanings.
fn attribute_name(kind: &str, field: &str) -> String {
    let namespace = match kind {
        "db.query" => "db",
        "http.client" => "http",
        "ai.call" => "gen_ai",
        "mcp.call" => "mcp",
        _ => "rustlavel",
    };
    format!("{namespace}.{field}")
}

/// The OTLP `ExportTraceServiceRequest` for a batch, in protobuf.
pub fn export_request(resource: &Resource, spans: &[Span]) -> Vec<u8> {
    let mut scope_spans = Encoder::new();
    scope_spans.message(1, &scope());
    for span in spans {
        scope_spans.message(2, &span.encode());
    }

    let mut resource_spans = Encoder::new();
    resource_spans.message(1, &resource.encode());
    resource_spans.message(2, &scope_spans);

    let mut request = Encoder::new();
    request.message(1, &resource_spans);
    request.into_bytes()
}

/// The same batch under the OTLP/JSON mapping.
pub fn export_request_json(resource: &Resource, spans: &[Span]) -> Json {
    Json::object([(
        "resourceSpans",
        Json::Array(vec![Json::object([
            ("resource", resource.to_json()),
            (
                "scopeSpans",
                Json::Array(vec![Json::object([
                    ("scope", scope_json()),
                    ("spans", Json::Array(spans.iter().map(Span::to_json).collect())),
                ])]),
            ),
        ])]),
    )])
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(hex: &str, length: usize) -> Option<Vec<u8>> {
    if hex.len() != length * 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..length).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()).collect()
}

/// Identifier bytes from the operating system.
///
/// Trace ids do not need to be unpredictable, only unique, so this is not a
/// cryptographic requirement and rule one's crypto exception does not apply —
/// reading `/dev/urandom` the way `rustlavel-db` and `rustlavel-auth` already
/// do keeps the dependency list unchanged.
fn random_bytes(length: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; length];

    if let Ok(mut source) = std::fs::File::open("/dev/urandom")
        && source.read_exact(&mut buffer).is_ok()
    {
        return buffer;
    }

    // Without an entropy source, mix the clock with a counter that cannot
    // repeat within a process. Two ids from the same nanosecond would otherwise
    // collide, and a collision here silently merges two unrelated traces.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = now ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9e37_79b9_7f4a_7c15);

    for slot in buffer.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = (state >> 24) as u8;
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SAMPLE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn a_traceparent_round_trips_through_parse_and_format() {
        let context = SpanContext::parse_traceparent(SAMPLE).expect("valid");

        assert_eq!(context.trace_id.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(context.span_id.to_hex(), "00f067aa0ba902b7");
        assert!(context.sampled);
        assert_eq!(context.traceparent(), SAMPLE);
    }

    #[test]
    fn the_sampled_flag_is_the_low_bit_of_the_last_field() {
        let unsampled = SpanContext::parse_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
        )
        .expect("valid");

        assert!(!unsampled.sampled);
        assert!(unsampled.traceparent().ends_with("-00"));
    }

    #[test]
    fn malformed_traceparents_are_refused_rather_than_half_read() {
        for header in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            // An all-zero trace id or span id is invalid by specification.
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            // Wrong lengths, non-hex, and the forbidden version.
            "00-4bf92f3577b34da6-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473g-00f067aa0ba902b7-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            // Version 00 defines exactly four fields.
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        ] {
            assert!(SpanContext::parse_traceparent(header).is_none(), "accepted {header:?}");
        }
    }

    #[test]
    fn a_future_version_is_read_for_the_fields_it_shares() {
        let context =
            SpanContext::parse_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x")
                .expect("forwards compatible");

        assert_eq!(context.span_id.to_hex(), "00f067aa0ba902b7");
    }

    #[test]
    fn a_child_keeps_the_trace_and_takes_a_new_span_id() {
        let parent = SpanContext::root();
        let child = parent.child();

        assert_eq!(child.trace_id, parent.trace_id);
        assert_ne!(child.span_id, parent.span_id);
        assert_eq!(child.sampled, parent.sampled);
    }

    #[test]
    fn identifiers_are_unique_and_never_zero() {
        let ids: std::collections::HashSet<_> = (0..64).map(|_| TraceId::random()).collect();

        assert_eq!(ids.len(), 64);
        assert!(TraceId::from_hex(&"0".repeat(32)).is_none());
    }

    #[tokio::test]
    async fn the_current_span_is_visible_to_everything_awaited_inside_it() {
        assert!(current().is_none());

        let context = SpanContext::root();
        let seen = in_span(context, async { current() }).await;

        assert_eq!(seen, Some(context));
        // And it is gone again outside the scope.
        assert!(current().is_none());
    }

    #[tokio::test]
    async fn traceparent_reports_the_span_in_scope() {
        let context = SpanContext::root();
        let header = in_span(context, async { traceparent() }).await;

        assert_eq!(header, Some(context.traceparent()));
    }

    #[test]
    fn a_span_encodes_its_ids_as_raw_bytes_not_hex() {
        let context = SpanContext::parse_traceparent(SAMPLE).expect("valid");
        let span = Span::new(context, "GET /orders", SpanKind::Server);
        let bytes = span.encode().into_bytes();

        // Field 1, wire type 2, sixteen bytes, then the id itself.
        assert_eq!(&bytes[..2], &[0x0a, 0x10]);
        assert_eq!(&bytes[2..18], context.trace_id.as_bytes());
        // Field 2, eight bytes, the span id.
        assert_eq!(&bytes[18..20], &[0x12, 0x08]);
        assert_eq!(&bytes[20..28], context.span_id.as_bytes());
    }

    #[test]
    fn a_server_span_carries_kind_two_and_both_timestamps() {
        let context = SpanContext::parse_traceparent(SAMPLE).expect("valid");
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let span = Span::new(context, "GET /orders", SpanKind::Server)
            .between(start, start + Duration::from_millis(5));
        let bytes = span.encode().into_bytes();

        // kind is field 6, a varint: (6 << 3) | 0 = 0x30, value 2.
        assert!(bytes.windows(2).any(|w| w == [0x30, 0x02]), "{bytes:02x?}");
        // start_time_unix_nano is field 7, fixed64: (7 << 3) | 1 = 0x39.
        let mut expected = vec![0x39u8];
        expected.extend_from_slice(&1_000_000_000u64.to_le_bytes());
        assert!(
            bytes.windows(expected.len()).any(|w| w == expected),
            "start time missing from {bytes:02x?}"
        );
    }

    /// The fixed sample context matters here, not just for repeatability: this
    /// asserts a tag byte is *absent*, and random identifiers contain an
    /// arbitrary 0x7a about one run in eleven. The sample's ids contain none,
    /// so the absence really is the absence of the field.
    #[test]
    fn an_unset_status_writes_no_status_field_and_an_error_writes_both_parts() {
        let context = SpanContext::parse_traceparent(SAMPLE).expect("valid");
        let unset = Span::new(context, "x", SpanKind::Internal).encode().into_bytes();
        // Field 15, wire type 2, is tag 0x7a.
        assert!(!unset.contains(&0x7a), "{unset:02x?}");

        let failed = Span::new(context, "x", SpanKind::Internal)
            .status(SpanStatus::Error("boom".into()))
            .encode()
            .into_bytes();
        // Status { message = "boom" (field 2), code = 2 (field 3) }.
        assert!(
            failed.windows(8).any(|w| w == [0x7a, 0x08, 0x12, 0x04, b'b', b'o', b'o', b'm']),
            "{failed:02x?}"
        );
        assert!(failed.ends_with(&[0x18, 0x02]), "{failed:02x?}");
    }

    #[test]
    fn a_query_event_becomes_a_child_span_that_ends_when_the_event_was_recorded() {
        let parent = SpanContext::root();
        let event = Event::new("db.query")
            .with("sql", "select * from orders where id = $1")
            .with("rows", 1)
            .with("ok", true)
            .took(Duration::from_millis(12));

        let span = span_from_event(&event, parent).expect("a span");

        assert_eq!(span.context.trace_id, parent.trace_id);
        assert_eq!(span.parent, Some(parent.span_id));
        assert_eq!(span.name, "SELECT orders");
        assert_eq!(span.kind, SpanKind::Client);
        assert_eq!(span.end, event.at);
        assert_eq!(span.end.duration_since(span.start).expect("ordered"), Duration::from_millis(12));
        assert_eq!(span.status, SpanStatus::Unset);

        let attributes: Vec<&str> = span.attributes.iter().map(|(k, _)| k.as_str()).collect();
        assert!(attributes.contains(&"db.query.text"));
        assert!(attributes.contains(&"db.rows"));
        // The statement is not repeated under both its own name and `db.sql`.
        assert!(!attributes.contains(&"db.sql"));
    }

    #[test]
    fn a_failed_event_becomes_a_span_with_an_error_status() {
        let event = Event::new("db.query").with("sql", "select 1").with("ok", false);
        let span = span_from_event(&event, SpanContext::root()).expect("a span");

        assert!(matches!(span.status, SpanStatus::Error(_)));
    }

    #[test]
    fn http_request_events_are_left_to_the_middleware() {
        let event = Event::new("http.request").with("route", "/orders/{id}");

        assert!(span_from_event(&event, SpanContext::root()).is_none());
    }

    #[test]
    fn span_names_stay_low_cardinality_whatever_the_statement() {
        assert_eq!(database_span_name("select id from users where id = $1"), "SELECT users");
        assert_eq!(database_span_name("INSERT INTO orders (id) VALUES ($1)"), "INSERT orders");
        assert_eq!(database_span_name("update  public.users set x = 1"), "UPDATE public.users");
        assert_eq!(database_span_name("delete from sessions"), "DELETE sessions");
        assert_eq!(database_span_name("begin"), "BEGIN");
        assert_eq!(database_span_name(""), "QUERY");
    }

    #[test]
    fn the_export_request_nests_resource_scope_and_spans_in_that_order() {
        let resource = Resource::new("checkout");
        let span = Span::new(SpanContext::root(), "GET /orders", SpanKind::Server);
        let bytes = export_request(&resource, &[span]);

        // ExportTraceServiceRequest.resource_spans is field 1, wire type 2.
        assert_eq!(bytes[0], 0x0a);
        // The service name has to survive into the payload, or everything lands
        // under `unknown_service`.
        assert!(
            bytes.windows(8).any(|w| w == b"checkout"),
            "service name missing from the payload"
        );
    }

    #[test]
    fn the_json_mapping_uses_hex_ids_and_string_timestamps() {
        let context = SpanContext::parse_traceparent(SAMPLE).expect("valid");
        let span = Span::new(context, "GET /orders", SpanKind::Server)
            .parent(SpanId::from_hex("00f067aa0ba902b7").expect("valid"))
            .between(SystemTime::UNIX_EPOCH + Duration::from_secs(2), SystemTime::UNIX_EPOCH + Duration::from_secs(3));
        let rendered = export_request_json(&Resource::new("checkout"), &[span]).to_string();

        assert!(rendered.contains(r#""traceId":"4bf92f3577b34da6a3ce929d0e0e4736""#), "{rendered}");
        assert!(rendered.contains(r#""parentSpanId":"00f067aa0ba902b7""#), "{rendered}");
        assert!(rendered.contains(r#""startTimeUnixNano":"2000000000""#), "{rendered}");
        assert!(rendered.contains(r#""kind":2"#), "{rendered}");
    }
}
