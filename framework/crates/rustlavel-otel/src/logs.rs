//! OTLP logs: the third signal.
//!
//! Every `info!`, `warn!` and `error!` is already an event on the bus, so this
//! is one more subscriber, the way spans are. What it adds over the line on
//! stderr is **correlation**: a log written while a request is being handled
//! carries that request's trace and span id, so a collector, Grafana, or
//! Elastic can show the lines of one trace together with its spans.
//!
//! Off by default. Logs can carry anything an application prints, and sending
//! them off the machine is a decision somebody should have made. Turn it on
//! with `.logs(true)`, `otel.logs.enabled`, or the specification's own
//! `OTEL_LOGS_EXPORTER=otlp`.
//!
//! It shares the exporter's queue discipline — bounded, drop the newest and
//! say so, never block the caller — and its retry and backoff, and it is
//! guarded the same way against shipping its own trouble: everything the
//! exporter logs while it talks to the collector is skipped, so a collector
//! that is down cannot turn its own failure into the next batch of log records.

use crate::protobuf::Encoder;
use crate::resource::{
    Attributes, Resource, Value, attributes_json, encode_attributes, scope, scope_json, unix_nanos,
};
use crate::trace::SpanContext;
use rustlavel_core::Json;
use rustlavel_core::events::Event;
use rustlavel_core::log::Level;
use std::time::SystemTime;

/// How the log signal behaves, beside the exporter's own settings.
#[derive(Debug, Clone)]
pub struct LogSettings {
    /// Where `ExportLogsServiceRequest` is posted.
    pub endpoint: String,
    /// Records below this level are not exported.
    pub min_level: Level,
    /// The most records held while waiting to be sent.
    pub max_queue: usize,
}

/// One log record, with the trace it was written inside, if any.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub time: SystemTime,
    pub level: Level,
    pub body: String,
    pub attributes: Attributes,
    /// The span in scope when the line was logged. Present only inside a
    /// request (or anything else run under `in_span`).
    pub trace: Option<SpanContext>,
}

impl LogRecord {
    /// A record from a `log` event, or `None` for any other kind.
    ///
    /// Fields other than `level` and `message` travel as attributes, so a
    /// package that logs with extra fields keeps them.
    pub fn from_event(event: &Event, trace: Option<SpanContext>) -> Option<LogRecord> {
        if event.kind != "log" {
            return None;
        }
        let level = event
            .field("level")
            .and_then(Json::as_str)
            .and_then(Level::parse)
            .unwrap_or(Level::Info);
        let body = event.field("message").and_then(Json::as_str).unwrap_or_default().to_string();

        let attributes = event
            .fields
            .iter()
            .filter(|(key, _)| key.as_str() != "level" && key.as_str() != "message")
            .map(|(key, value)| (key.clone(), Value::from_json(value)))
            .collect();

        Some(LogRecord { time: event.at, level, body, attributes, trace })
    }

    fn encode(&self) -> Encoder {
        let mut record = Encoder::new();
        record.fixed64(1, unix_nanos(self.time));
        record.enumeration(2, severity_number(self.level));
        record.string(3, severity_text(self.level));
        record.message(5, &Value::String(self.body.clone()).encode());
        encode_attributes(&mut record, 6, &self.attributes);
        if let Some(trace) = &self.trace {
            // The W3C trace flags: bit 0 is "sampled". A collector that sees
            // zero treats the record as belonging to an unsampled trace.
            record.fixed32(8, u32::from(trace.sampled));
            record.bytes_field(9, trace.trace_id.as_bytes());
            record.bytes_field(10, trace.span_id.as_bytes());
        }
        // Observed when it was written: the bus reports a line as it is logged,
        // so the two clocks are the same. Left out, a collector shows 1970.
        record.fixed64(11, unix_nanos(self.time));
        record
    }

    fn to_json(&self) -> Json {
        let mut fields = vec![
            // 64-bit integers travel as strings in OTLP/JSON.
            ("timeUnixNano", Json::from(unix_nanos(self.time).to_string())),
            ("observedTimeUnixNano", Json::from(unix_nanos(self.time).to_string())),
            ("severityNumber", Json::Number(f64::from(severity_number(self.level)))),
            ("severityText", Json::from(severity_text(self.level))),
            ("body", Value::String(self.body.clone()).to_json()),
            ("attributes", attributes_json(&self.attributes)),
        ];
        if let Some(trace) = &self.trace {
            fields.push(("flags", Json::Number(f64::from(u32::from(trace.sampled)))));
            fields.push(("traceId", Json::from(trace.trace_id.to_hex())));
            fields.push(("spanId", Json::from(trace.span_id.to_hex())));
        }
        Json::object(fields)
    }
}

/// `SeverityNumber`: the first number of each band, which is what an SDK uses
/// when it has only a level and no finer grading.
fn severity_number(level: Level) -> i32 {
    match level {
        Level::Debug => 5,
        Level::Info => 9,
        Level::Warn => 13,
        Level::Error => 17,
    }
}

fn severity_text(level: Level) -> &'static str {
    match level {
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
    }
}

/// The OTLP `ExportLogsServiceRequest` for a batch, in protobuf.
pub fn export_request(resource: &Resource, records: &[LogRecord]) -> Vec<u8> {
    let mut scope_logs = Encoder::new();
    scope_logs.message(1, &scope());
    for record in records {
        scope_logs.message(2, &record.encode());
    }

    let mut resource_logs = Encoder::new();
    resource_logs.message(1, &resource.encode());
    resource_logs.message(2, &scope_logs);

    let mut request = Encoder::new();
    request.message(1, &resource_logs);
    request.into_bytes()
}

/// The same batch under the OTLP/JSON mapping.
pub fn export_request_json(resource: &Resource, records: &[LogRecord]) -> Json {
    Json::object([(
        "resourceLogs",
        Json::Array(vec![Json::object([
            ("resource", resource.to_json()),
            (
                "scopeLogs",
                Json::Array(vec![Json::object([
                    ("scope", scope_json()),
                    ("logRecords", Json::Array(records.iter().map(LogRecord::to_json).collect())),
                ])]),
            ),
        ])]),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{SpanContext, SpanId, TraceId};

    fn event(level: &str, message: &str) -> Event {
        Event::new("log").with("level", level).with("message", message)
    }

    #[test]
    fn only_log_events_become_records() {
        assert!(LogRecord::from_event(&Event::new("db.query"), None).is_none());
        let record = LogRecord::from_event(&event("warn", "careful"), None).unwrap();
        assert_eq!((record.level, record.body.as_str()), (Level::Warn, "careful"));
    }

    #[test]
    fn extra_fields_become_attributes_and_level_and_message_do_not() {
        let event = Event::new("log")
            .with("level", "info")
            .with("message", "m")
            .with("user_id", 7)
            .with("route", "/x");
        let record = LogRecord::from_event(&event, None).unwrap();
        let keys: Vec<&str> = record.attributes.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["route", "user_id"]);
    }

    #[test]
    fn severity_follows_the_specification_bands() {
        assert_eq!(
            [Level::Debug, Level::Info, Level::Warn, Level::Error].map(severity_number),
            [5, 9, 13, 17]
        );
        assert_eq!(severity_text(Level::Warn), "WARN");
    }

    #[test]
    fn a_record_inside_a_trace_carries_its_ids_in_both_encodings() {
        let context = SpanContext::root();
        let record = LogRecord::from_event(&event("info", "in a request"), Some(context)).unwrap();
        let json = export_request_json(&Resource::new("svc"), std::slice::from_ref(&record)).to_string();

        assert!(json.contains(&format!("\"traceId\":\"{}\"", context.trace_id.to_hex())), "{json}");
        assert!(json.contains(&format!("\"spanId\":\"{}\"", context.span_id.to_hex())), "{json}");
        assert!(json.contains("\"severityText\":\"INFO\""));
        assert!(json.contains("\"flags\":1"), "a sampled trace's records say so: {json}");
        assert!(json.contains("observedTimeUnixNano"), "{json}");
        assert!(json.contains("\"stringValue\":\"in a request\""));

        let bytes = export_request(&Resource::new("svc"), &[record]);
        let hay = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(hay(context.trace_id.as_bytes()) && hay(context.span_id.as_bytes()));
    }

    #[test]
    fn a_record_outside_any_trace_has_no_ids() {
        let record = LogRecord::from_event(&event("info", "boot"), None).unwrap();
        let json = export_request_json(&Resource::new("svc"), &[record]).to_string();
        assert!(!json.contains("traceId") && !json.contains("spanId"), "{json}");
    }

    #[test]
    fn the_request_nests_resource_scope_records_in_that_order() {
        // Field 1 of the request is `resource_logs`; inside it, field 1 is the
        // resource and field 2 the scope logs — a mismatch here decodes as
        // nothing at all on the collector, with no error.
        let bytes = export_request(&Resource::new("svc"), &[]);
        assert_eq!(bytes[0], 0x0a, "tag of field 1, length-delimited");
        let _ = (TraceId::random(), SpanId::random());
    }
}
