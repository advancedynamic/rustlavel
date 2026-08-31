//! Turning OpenTelemetry on.
//!
//! ```ignore
//! App::new()?
//!     .routes(routes::web::routes)
//!     .plugin(OpenTelemetry::default())
//!     .serve()
//!     .await
//! ```
//!
//! One line, and the request span, its children, and the metrics all follow.
//! The configuration is read from the environment variables the OpenTelemetry
//! specification defines, because anyone deploying this already has them set
//! for their other services and a framework that insists on its own names makes
//! them keep two copies of the same fact.

use crate::exporter::{Exporter, Protocol, Settings, signal_endpoint};
use crate::metrics;
use crate::resource::{Resource, Value, percent_decode};
use crate::trace::{Span, SpanContext, SpanKind, SpanStatus};
use rustlavel_core::events::{Event, Subscriber, subscribe};
use rustlavel_core::{Config, env};
use rustlavel_http::plugin::{Plugin, Setup};
use rustlavel_http::{Next, Request, Response, Router};
use std::time::{Duration, Instant, SystemTime};

/// Where a collector listens when nothing says otherwise: the OTLP/HTTP port,
/// on this machine, which is where a sidecar or an agent normally is.
pub const DEFAULT_ENDPOINT: &str = "http://localhost:4318";

/// The OpenTelemetry plugin.
///
/// Every setting can come from configuration or the environment instead, so a
/// deployment tunes it without recompiling. A builder call always wins over
/// configuration, and configuration over the environment: a value written in
/// `main.rs` is a decision, a config key is a default, and an environment
/// variable is whatever the platform happened to inject.
///
/// | Config key           | Environment                      | Meaning                                   |
/// |----------------------|----------------------------------|-------------------------------------------|
/// | `otel.enabled`       | `OTEL_SDK_DISABLED`              | Export at all. On by default.             |
/// | `otel.endpoint`      | `OTEL_EXPORTER_OTLP_ENDPOINT`    | Collector base URL.                       |
/// | `otel.service`       | `OTEL_SERVICE_NAME`              | `service.name`. Falls back to `app.name`. |
/// | `otel.headers`       | `OTEL_EXPORTER_OTLP_HEADERS`     | `k=v,k=v`, percent-decoded.               |
/// | `otel.protocol`      | `OTEL_EXPORTER_OTLP_PROTOCOL`    | `http/protobuf` or `http/json`.           |
/// | `otel.interval_ms`   |                                  | Flush interval (5000).                    |
/// | `otel.batch`         |                                  | Spans per request (512).                  |
/// | `otel.queue`         |                                  | Spans held before dropping (2048).        |
/// | `otel.retries`       |                                  | Retries per batch (3).                    |
#[derive(Default)]
pub struct OpenTelemetry {
    endpoint: Option<String>,
    service: Option<String>,
    protocol: Option<Protocol>,
    headers: Vec<(String, String)>,
    attributes: Vec<(String, Value)>,
    interval: Option<Duration>,
    max_batch: Option<usize>,
    max_queue: Option<usize>,
    max_retries: Option<u32>,
    enabled: Option<bool>,
    client: Option<rustlavel_client::Client>,
}

impl OpenTelemetry {
    pub fn new() -> Self {
        OpenTelemetry::default()
    }

    /// The collector's base URL. `/v1/traces` and `/v1/metrics` are appended,
    /// which is what the specification says a base endpoint means over HTTP.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn service(mut self, name: impl Into<String>) -> Self {
        self.service = Some(name.into());
        self
    }

    pub fn protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = Some(protocol);
        self
    }

    /// A header on every export — an API key for a hosted collector, usually.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// A resource attribute: `deployment.environment.name`, `service.version`,
    /// the pod name, anything that identifies this process rather than this
    /// request.
    pub fn attribute(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.attributes.push((key.into(), value.into()));
        self
    }

    pub fn flush_every(mut self, interval: Duration) -> Self {
        self.interval = Some(interval);
        self
    }

    pub fn batch_size(mut self, spans: usize) -> Self {
        self.max_batch = Some(spans);
        self
    }

    /// How many spans may wait before the newest are dropped.
    pub fn queue_limit(mut self, spans: usize) -> Self {
        self.max_queue = Some(spans);
        self
    }

    pub fn retries(mut self, retries: u32) -> Self {
        self.max_retries = Some(retries);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// Export through a caller-supplied client. Tests fake it; production might
    /// use it to carry a proxy or a client certificate.
    pub fn with_client(mut self, client: rustlavel_client::Client) -> Self {
        self.client = Some(client);
        self
    }

    fn resolve(&self, config: &Config) -> (Settings, Resource) {
        let endpoint = self
            .endpoint
            .clone()
            .unwrap_or_else(|| {
                config.string(
                    "otel.endpoint",
                    &env::env_or("OTEL_EXPORTER_OTLP_ENDPOINT", DEFAULT_ENDPOINT),
                )
            })
            .trim_end_matches('/')
            .to_string();

        let protocol = self.protocol.unwrap_or_else(|| {
            Protocol::parse(&config.string(
                "otel.protocol",
                &env::env_or("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
            ))
            .unwrap_or_default()
        });

        let mut headers = parse_headers(&config.string(
            "otel.headers",
            &env::env_or("OTEL_EXPORTER_OTLP_HEADERS", ""),
        ));
        headers.extend(self.headers.iter().cloned());

        let settings = Settings {
            traces_endpoint: signal_endpoint(&endpoint, "traces"),
            metrics_endpoint: signal_endpoint(&endpoint, "metrics"),
            protocol,
            headers,
            timeout: Duration::from_millis(
                config.int("otel.timeout_ms", 10_000).clamp(100, 120_000) as u64
            ),
            interval: self.interval.unwrap_or_else(|| {
                Duration::from_millis(config.int("otel.interval_ms", 5_000).clamp(50, 600_000) as u64)
            }),
            max_batch: self
                .max_batch
                .unwrap_or_else(|| config.int("otel.batch", 512).clamp(1, 10_000) as usize),
            max_queue: self
                .max_queue
                .unwrap_or_else(|| config.int("otel.queue", 2_048).clamp(1, 1_000_000) as usize),
            max_retries: self
                .max_retries
                .unwrap_or_else(|| config.int("otel.retries", 3).clamp(0, 10) as u32),
        };

        let service = self.service.clone().unwrap_or_else(|| {
            config.string(
                "otel.service",
                &env::env_or("OTEL_SERVICE_NAME", &config.string("app.name", "rustlavel")),
            )
        });

        let mut resource = Resource::new(&service)
            .with_pairs(&env::env_or("OTEL_RESOURCE_ATTRIBUTES", ""))
            // Written after the environment so a value set in code wins, and
            // last of all so it also wins over `OTEL_RESOURCE_ATTRIBUTES`.
            .with("deployment.environment.name", config.environment());
        for (key, value) in &self.attributes {
            resource = resource.with(key.clone(), value.clone());
        }

        (settings, resource)
    }

    /// Whether exporting is switched on.
    ///
    /// Unlike Telescope, the default is on in every environment: an exporter
    /// leaks nothing — it talks to a collector the operator configured — and
    /// production is the environment where the traces are actually wanted.
    fn allowed(&self, config: &Config) -> bool {
        if let Some(enabled) = self.enabled {
            return enabled;
        }
        // `OTEL_SDK_DISABLED` is the specification's own kill switch, and it
        // has to work without a rebuild or a config file.
        let disabled = env::env_or("OTEL_SDK_DISABLED", "false").eq_ignore_ascii_case("true");
        config.bool("otel.enabled", !disabled)
    }

    /// Install onto a router directly.
    ///
    /// Returns the [`Exporter`] when it mounted. Applications go through
    /// [`Plugin`]; this is for tests and for an application that builds its own
    /// router.
    pub fn install(self, router: &mut Router, config: &Config) -> Option<Exporter> {
        if !self.allowed(config) {
            rustlavel_core::debug!("otel: export is disabled");
            return None;
        }

        let (settings, resource) = self.resolve(config);
        let endpoint = settings.traces_endpoint.clone();
        let service = resource.service_name().to_string();

        let exporter = match self.client.clone() {
            Some(client) => Exporter::with_client(settings, resource, client),
            None => Exporter::new(settings, resource),
        };

        router.middleware(trace_requests(exporter.clone()));
        subscribe(Collector { exporter: exporter.clone() });

        // `start` spawns onto the current runtime. An application that builds
        // its router outside one — a test, or a CLI assembling routes to list
        // them — gets a working exporter that simply has no background loop
        // until something flushes it, rather than a panic during boot.
        if tokio::runtime::Handle::try_current().is_ok() {
            exporter.start();
        } else {
            rustlavel_core::debug!(
                "otel: no tokio runtime during setup, so nothing is flushing yet; \
                 call `Exporter::start` once the runtime is up"
            );
        }

        rustlavel_core::info!("otel: exporting `{service}` to {endpoint}");
        Some(exporter)
    }
}

impl Plugin for OpenTelemetry {
    fn name(&self) -> &'static str {
        "otel"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        if let Some(exporter) = (*self).install(setup.router, setup.config) {
            // Registered as state so a handler can record its own spans and
            // metrics, and so an application can flush on shutdown.
            setup.state(exporter);
        }
    }
}

/// Global middleware that opens a server span around every request.
///
/// This is middleware rather than another bus subscriber for two reasons the
/// bus cannot give: the incoming `traceparent` is only visible here, and the
/// span has to be *open* while the handler runs so that queries inside it can
/// find their parent. The `http.request` event arrives after the response is
/// finished, which is too late for either.
fn trace_requests(exporter: Exporter) -> impl rustlavel_http::Middleware {
    move |request: Request, next: Next| {
        let exporter = exporter.clone();
        async move { trace_request(exporter, request, next).await }
    }
}

async fn trace_request(exporter: Exporter, request: Request, next: Next) -> Response {
    let incoming = request.header("traceparent").and_then(SpanContext::parse_traceparent);
    let context = match incoming {
        Some(parent) => parent.child(),
        None => SpanContext::root(),
    };

    let method = request.method().as_str();
    // The route pattern, not the path. A span name is what a backend groups by,
    // and `/orders/1` as a name makes one group per order.
    let route = request.route().map(str::to_string);
    let name = match &route {
        Some(route) => format!("{method} {route}"),
        None => method.to_string(),
    };
    let path = request.path().to_string();
    let user_agent = request.header("user-agent").map(str::to_string);
    let client_address = request.ip();

    let start = SystemTime::now();
    let elapsed = Instant::now();
    let response = crate::trace::in_span(context, next.run(request)).await;
    // The end is derived from the monotonic clock rather than read from the
    // wall clock again, so an NTP step during the request cannot produce a span
    // that ends before it started.
    let end = start + elapsed.elapsed();

    let mut span = Span::new(context, name, SpanKind::Server)
        .between(start, end)
        .with("http.request.method", method)
        .with("url.path", path)
        .with("http.response.status_code", response.status.code());
    if let Some(parent) = incoming {
        span = span.parent(parent.span_id);
    }
    if let Some(route) = route {
        span = span.with("http.route", route);
    }
    if let Some(agent) = user_agent {
        span = span.with("user_agent.original", agent);
    }
    if let Some(address) = client_address {
        span = span.with("client.address", address);
    }
    // Only a 5xx is the server's fault. A 404 or a 422 is the span working
    // exactly as intended, and marking those as errors turns an error rate into
    // a measure of how many people mistype URLs.
    if response.status.code() >= 500 {
        span = span.status(SpanStatus::Error(response.status.reason().to_string()));
    }

    exporter.record_span(span);

    // Returned so a caller — a browser, a load test, the service that made the
    // request — can look up the trace for the response it is holding.
    response.with_header("traceparent", context.traceparent())
}

/// Turns framework events into metrics, and into child spans of whatever
/// request is in scope.
///
/// Metrics are recorded for every event; spans only for events that happened
/// inside a span. Work with no request around it — a queue worker, a scheduled
/// job, anything a handler moved onto a `tokio::spawn` — is therefore measured
/// but not traced, because a span with no trace to belong to is noise. Wrap
/// that work in [`crate::trace::in_span`] to give it one.
struct Collector {
    exporter: Exporter,
}

impl Subscriber for Collector {
    fn interested_in(&self, kind: &str) -> bool {
        matches!(
            kind,
            "http.request"
                | "http.client"
                | "db.query"
                | "ai.call"
                | "mcp.call"
                | "queue.processed"
                | "queue.failed"
        )
    }

    fn handle(&self, event: &Event) {
        // The exporter ships over HTTP, and the client reports that request on
        // this same bus. Measuring it would make every flush generate the data
        // the next flush reports.
        if crate::exporter::is_exporting() {
            return;
        }

        metrics::record_event(self.exporter.meter(), event);

        // `http.request` has no child span: the middleware above already
        // recorded the request, with the response and the incoming
        // `traceparent` in hand.
        if let Some(parent) = crate::trace::current()
            && parent.sampled
            && let Some(span) = crate::trace::span_from_event(event, parent)
        {
            self.exporter.record_span(span);
        }
    }
}

/// `OTEL_EXPORTER_OTLP_HEADERS`: comma-separated `key=value`, values
/// percent-decoded so a token containing a comma survives.
fn parse_headers(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            let name = name.trim();
            (!name.is_empty())
                .then(|| (name.to_ascii_lowercase(), percent_decode(value.trim())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::fake::{Fake, FakeResponse};
    use rustlavel_client::Client;
    use rustlavel_core::events::clear_subscribers;
    use rustlavel_http::TestClient;
    use std::sync::Arc;

    /// The instrumentation bus and the environment are both process-wide, so
    /// these tests run one at a time. Rule six: serialise when the state really
    /// is global.
    ///
    /// A Tokio mutex because the tests await while holding it, which a `std`
    /// guard must never do.
    static BUS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn config() -> Config {
        let config = Config::new();
        config.set("app.name", "checkout");
        config.set("app.env", "testing");
        // Pin every environment-backed setting so a developer's own
        // OTEL_* variables cannot change what these assert.
        config.set("otel.endpoint", "http://collector.test");
        config.set("otel.protocol", "http/protobuf");
        config.set("otel.headers", "");
        config.set("otel.service", "checkout");
        config
    }

    fn faking() -> (OpenTelemetry, Arc<Fake>) {
        let client = Client::new().faking(Fake::new().fallback(FakeResponse::text("")));
        let fake = client.fake().expect("faked").clone();
        (OpenTelemetry::new().with_client(client), fake)
    }

    async fn mounted(plugin: OpenTelemetry, router: &mut Router) -> Exporter {
        plugin.install(router, &config()).expect("mounted")
    }

    #[tokio::test]
    async fn a_request_becomes_a_server_span_with_the_route_as_its_name() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        router.get("/orders/{id}", |_req: Request| async { "order" });
        let (plugin, fake) = faking();
        let exporter = mounted(plugin, &mut router).await;

        TestClient::new(router).get("/orders/7").await.assert_ok();
        exporter.shutdown().await;
        clear_subscribers();

        let body = fake
            .recorded()
            .into_iter()
            .find(|r| r.url.ends_with("/v1/traces"))
            .expect("a trace export")
            .body;
        // The pattern, not the path — otherwise every order id is its own span
        // name and the trace view is useless.
        assert!(contains(&body, b"GET /orders/{id}"), "span name missing");
        assert!(!contains(&body, b"GET /orders/7"), "the path leaked into the name");
        assert!(contains(&body, b"http.route"), "the route attribute is missing");
        assert!(contains(&body, b"/orders/7"), "url.path should still carry the real path");
    }

    #[tokio::test]
    async fn an_incoming_traceparent_continues_the_trace_and_the_response_carries_one_back() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        router.get("/orders", |_req: Request| async { "orders" });
        let (plugin, fake) = faking();
        let exporter = mounted(plugin, &mut router).await;

        let upstream = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let response = TestClient::new(router)
            .send(
                Request::new(rustlavel_http::Method::Get, "/orders")
                    .with_header("traceparent", upstream),
            )
            .await;
        exporter.shutdown().await;
        clear_subscribers();

        let returned = response.header("traceparent").expect("a traceparent on the way out");
        let context = SpanContext::parse_traceparent(returned).expect("valid");
        assert_eq!(context.trace_id.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        // A new span in the same trace, not the upstream span repeated.
        assert_ne!(context.span_id.to_hex(), "00f067aa0ba902b7");

        let body = fake
            .recorded()
            .into_iter()
            .find(|r| r.url.ends_with("/v1/traces"))
            .expect("a trace export")
            .body;
        // The upstream ids have to appear as raw bytes: the trace id on the
        // span, the upstream span id as its parent.
        assert!(contains(&body, context.trace_id.as_bytes()), "the trace was not continued");
        assert!(
            contains(&body, crate::trace::SpanId::from_hex("00f067aa0ba902b7").unwrap().as_bytes()),
            "the parent span id is missing"
        );
    }

    #[tokio::test]
    async fn an_unsampled_upstream_stops_this_service_recording_too() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        router.get("/orders", |_req: Request| async { "orders" });
        let (plugin, fake) = faking();
        let exporter = mounted(plugin, &mut router).await;

        TestClient::new(router)
            .send(Request::new(rustlavel_http::Method::Get, "/orders").with_header(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
            ))
            .await;
        exporter.shutdown().await;
        clear_subscribers();

        assert_eq!(exporter.pending(), 0);
        fake.assert_not_sent("/v1/traces");
    }

    #[tokio::test]
    async fn a_query_inside_a_request_becomes_a_child_of_the_request_span() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        router.get("/orders", |_req: Request| async {
            // What the database driver dispatches from inside the handler.
            Event::new("db.query")
                .with("sql", "select * from orders")
                .with("ok", true)
                .took(Duration::from_millis(3))
                .dispatch();
            "orders"
        });
        let (plugin, _fake) = faking();
        let exporter = mounted(plugin, &mut router).await;

        TestClient::new(router).get("/orders").await.assert_ok();
        clear_subscribers();

        let queued: Vec<Span> = exporter_queue(&exporter);
        assert_eq!(queued.len(), 2, "expected the query span and the request span");

        let query = queued.iter().find(|s| s.name == "SELECT orders").expect("a query span");
        let request = queued.iter().find(|s| s.name.starts_with("GET")).expect("a request span");

        assert_eq!(query.context.trace_id, request.context.trace_id, "not in the same trace");
        assert_eq!(query.parent, Some(request.context.span_id), "not parented to the request");
        // And the request is a root within this service.
        assert_eq!(request.parent, None);
    }

    #[tokio::test]
    async fn a_five_hundred_is_an_error_and_a_four_hundred_is_not() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        router.get("/broken", |_req: Request| async {
            Response::new(rustlavel_http::Status::INTERNAL_ERROR)
        });
        router.get("/missing", |_req: Request| async { Response::not_found() });
        let (plugin, _fake) = faking();
        let exporter = mounted(plugin, &mut router).await;

        let client = TestClient::new(router);
        client.get("/broken").await;
        client.get("/missing").await;
        clear_subscribers();

        let queued = exporter_queue(&exporter);
        let broken = queued.iter().find(|s| s.name.contains("/broken")).expect("a span");
        let missing = queued.iter().find(|s| s.name.contains("/missing")).expect("a span");

        assert!(matches!(broken.status, SpanStatus::Error(_)));
        assert_eq!(missing.status, SpanStatus::Unset);
    }

    #[tokio::test]
    async fn requests_are_measured_as_well_as_traced() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        router.get("/orders/{id}", |_req: Request| async { "order" });
        let (plugin, _fake) = faking();
        let exporter = mounted(plugin, &mut router).await;

        let client = TestClient::new(router);
        client.get("/orders/1").await.assert_ok();
        client.get("/orders/2").await.assert_ok();
        clear_subscribers();

        let key = vec![
            ("http.request.method".to_string(), Value::from("GET")),
            ("http.route".to_string(), Value::from("/orders/{id}")),
            ("http.response.status_code".to_string(), Value::Int(200)),
        ];
        assert_eq!(
            exporter.meter().observations(metrics::instruments::HTTP_SERVER_DURATION, &key),
            2
        );
    }

    #[tokio::test]
    async fn the_exporters_own_traffic_is_not_measured_as_application_traffic() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        let (plugin, _fake) = faking();
        let exporter = mounted(plugin, &mut router).await;

        exporter.meter().add(metrics::instruments::QUEUE_JOBS, Vec::new(), 1);
        exporter.flush().await;
        exporter.flush().await;
        clear_subscribers();

        // Two flushes, each posting metrics, and not one outbound-request
        // measurement to show for it — otherwise the exporter would report on
        // itself for ever.
        assert_eq!(
            exporter.meter().observations(metrics::instruments::HTTP_CLIENT_DURATION, &Vec::new()),
            0
        );
    }

    #[tokio::test]
    async fn disabling_it_mounts_nothing_and_subscribes_to_nothing() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        let mounted = OpenTelemetry::new().enabled(false).install(&mut router, &config());

        assert!(mounted.is_none());
        assert!(!rustlavel_core::events::has_subscribers());
        clear_subscribers();
    }

    #[test]
    fn configuration_supplies_the_endpoint_service_and_limits() {
        let config = config();
        config.set("otel.endpoint", "http://collector.internal:4318/");
        config.set("otel.service", "billing");
        config.set("otel.queue", 10);
        config.set("otel.batch", 5);
        config.set("otel.interval_ms", 250);

        let (settings, resource) = OpenTelemetry::new().resolve(&config);

        assert_eq!(settings.traces_endpoint, "http://collector.internal:4318/v1/traces");
        assert_eq!(settings.metrics_endpoint, "http://collector.internal:4318/v1/metrics");
        assert_eq!(settings.max_queue, 10);
        assert_eq!(settings.max_batch, 5);
        assert_eq!(settings.interval, Duration::from_millis(250));
        assert_eq!(resource.service_name(), "billing");
    }

    #[test]
    fn a_builder_call_wins_over_configuration() {
        let config = config();
        config.set("otel.queue", 10);
        config.set("otel.service", "from-config");

        let (settings, resource) =
            OpenTelemetry::new().queue_limit(99).service("from-code").resolve(&config);

        assert_eq!(settings.max_queue, 99);
        assert_eq!(resource.service_name(), "from-code");
    }

    #[test]
    fn the_service_name_falls_back_to_the_application_name() {
        let config = Config::new();
        config.set("app.name", "checkout");

        let (_, resource) = OpenTelemetry::new().resolve(&config);

        // Only meaningful when the developer's shell has not set the variable.
        if std::env::var("OTEL_SERVICE_NAME").is_err() {
            assert_eq!(resource.service_name(), "checkout");
        }
    }

    #[test]
    fn nonsense_limits_are_clamped_rather_than_trusted() {
        let config = config();
        config.set("otel.queue", 0);
        config.set("otel.batch", -5);
        config.set("otel.retries", 500);

        let (settings, _) = OpenTelemetry::new().resolve(&config);

        assert_eq!(settings.max_queue, 1);
        assert_eq!(settings.max_batch, 1);
        assert_eq!(settings.max_retries, 10);
    }

    #[test]
    fn headers_are_parsed_and_percent_decoded() {
        let parsed = parse_headers("api-key=secret,X-Tenant=a%2Cb, spaced = value ");

        assert_eq!(parsed[0], ("api-key".to_string(), "secret".to_string()));
        // Header names are lower-cased so a later `.header()` call replaces
        // rather than duplicating one that came from the environment.
        assert_eq!(parsed[1], ("x-tenant".to_string(), "a,b".to_string()));
        assert_eq!(parsed[2], ("spaced".to_string(), "value".to_string()));
        assert!(parse_headers("").is_empty());
        assert!(parse_headers("nonsense").is_empty());
    }

    #[test]
    fn the_environment_is_recorded_on_the_resource() {
        let (_, resource) = OpenTelemetry::new().resolve(&config());

        assert!(
            resource
                .attributes
                .iter()
                .any(|(k, v)| k == "deployment.environment.name" && *v == Value::from("testing")),
            "{:?}",
            resource.attributes
        );
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| window == needle)
    }

    fn exporter_queue(exporter: &Exporter) -> Vec<Span> {
        // Reaching into the queue rather than flushing: these tests are about
        // what was recorded, and a flush would only prove the fake was called.
        exporter.drain_for_test()
    }
}
