//! rustlavel-otel: OpenTelemetry traces and metrics, over OTLP.
//!
//! ```ignore
//! App::new()?
//!     .routes(routes::web::routes)
//!     .plugin(OpenTelemetry::default())
//!     .serve()
//!     .await
//! ```
//!
//! Nothing is instrumented twice. Every package in the framework already
//! reports what it does on `rustlavel_core::events`; this listens, the way
//! Telescope and the Prometheus exporter do, and turns what it hears into OTLP.
//! Adding a span to a new package therefore means dispatching an event, not
//! reaching for this crate.
//!
//! # What it produces
//!
//! * A **server span** per request, named for the route pattern, carrying the
//!   method, path, status and route. Everything the request awaits — a query, a
//!   model call, an MCP tool, an outbound request — becomes a **child span**.
//! * **Metrics** for the same work: request, query, model and job durations as
//!   histograms, token usage and job outcomes as counters, and the exporter's
//!   own queue depth and drop count so a broken telemetry pipeline is visible
//!   from inside the telemetry.
//!
//! # Propagation
//!
//! W3C `traceparent` is read from the request and written to the response, so a
//! trace that began upstream continues here instead of starting again. For an
//! outbound call, attach [`trace::traceparent`] to carry it onwards.
//!
//! # Encoding
//!
//! Protobuf by default, because every collector must accept
//! `application/x-protobuf` while JSON is optional. The encoder is written here
//! rather than generated ([`protobuf`]), which rule one asks for and which the
//! narrow, fixed schema makes reasonable. `application/json` is a
//! configuration switch away when a payload needs to be readable in a proxy log.
//!
//! # Failure
//!
//! Telemetry must not be able to take down what it observes. Recording a span
//! is a mutex and a push; the queue is bounded and drops with a counter and a
//! log line; an unreachable collector costs spans and never a request. See
//! [`exporter`] for the drop and retry policy in full.

pub mod exporter;
pub mod metrics;
pub mod plugin;
pub mod protobuf;
pub mod resource;
pub mod trace;

pub use exporter::{Exporter, Protocol, Settings};
pub use metrics::{Instrument, Meter};
pub use plugin::OpenTelemetry;
pub use protobuf::Encoder;
pub use resource::{Resource, Value};
pub use trace::{Span, SpanContext, SpanId, SpanKind, SpanStatus, TraceId, in_span, traceparent};
