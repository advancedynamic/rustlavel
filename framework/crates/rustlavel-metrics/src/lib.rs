//! rustlavel-metrics: Prometheus metrics, gathered from what the framework
//! already reports.
//!
//! Nothing has to be instrumented twice. Every package dispatches on the
//! instrumentation bus; this listens and keeps counters and histograms, and
//! serves them in the text format Prometheus scrapes.
//!
//! ```ignore
//! App::new()?.plugin(Metrics::default())   // exposes /metrics
//! ```

use rustlavel_core::events::{Event, Subscriber, subscribe};
use rustlavel_core::{Config, Json};
use rustlavel_http::plugin::{Plugin, Setup};
use rustlavel_http::{Request, Response, Status};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// The bucket boundaries, in seconds.
///
/// Chosen around what a web request actually looks like: most under 100ms, a
/// long tail that matters. Prometheus needs the boundaries fixed up front,
/// which is why they are a constant rather than computed.
pub const BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// A label set, kept ordered so one series always renders identically.
type Labels = BTreeMap<String, String>;

struct Histogram {
    /// Counts per bucket, aligned with [`BUCKETS`], plus a final +Inf bucket.
    counts: Vec<u64>,
    sum: f64,
    total: u64,
}

/// Sized to the bucket boundaries, because an empty `counts` would panic on the
/// first observation — the one place a derived `Default` would be wrong.
impl Default for Histogram {
    fn default() -> Self {
        Histogram { counts: vec![0; BUCKETS.len() + 1], sum: 0.0, total: 0 }
    }
}

impl Histogram {
    fn observe(&mut self, seconds: f64) {
        // Prometheus histograms are cumulative: an observation lands in its own
        // bucket and every wider one.
        let index = BUCKETS.iter().position(|edge| seconds <= *edge).unwrap_or(BUCKETS.len());
        for count in self.counts.iter_mut().skip(index) {
            *count += 1;
        }
        self.sum += seconds;
        self.total += 1;
    }
}

#[derive(Default)]
struct Store {
    counters: BTreeMap<(String, Labels), u64>,
    histograms: BTreeMap<(String, Labels), Histogram>,
    gauges: BTreeMap<(String, Labels), f64>,
}

/// The collected metrics, cheap to clone.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<RwLock<Store>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    pub fn increment(&self, name: &str, labels: &[(&str, &str)]) {
        self.add(name, labels, 1);
    }

    pub fn add(&self, name: &str, labels: &[(&str, &str)], amount: u64) {
        let mut store = self.inner.write().expect("metrics lock poisoned");
        *store.counters.entry((name.to_string(), label_set(labels))).or_insert(0) += amount;
    }

    pub fn observe(&self, name: &str, labels: &[(&str, &str)], seconds: f64) {
        let mut store = self.inner.write().expect("metrics lock poisoned");
        store
            .histograms
            .entry((name.to_string(), label_set(labels)))
            .or_default()
            .observe(seconds);
    }

    pub fn set(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        let mut store = self.inner.write().expect("metrics lock poisoned");
        store.gauges.insert((name.to_string(), label_set(labels)), value);
    }

    pub fn counter(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        let store = self.inner.read().expect("metrics lock poisoned");
        store.counters.get(&(name.to_string(), label_set(labels))).copied().unwrap_or(0)
    }

    /// Total observations recorded for a histogram.
    pub fn observations(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        let store = self.inner.read().expect("metrics lock poisoned");
        store
            .histograms
            .get(&(name.to_string(), label_set(labels)))
            .map(|histogram| histogram.total)
            .unwrap_or(0)
    }

    pub fn clear(&self) {
        let mut store = self.inner.write().expect("metrics lock poisoned");
        *store = Store::default();
    }

    /// Render the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let store = self.inner.read().expect("metrics lock poisoned");
        let mut out = String::new();
        let mut described: Vec<&str> = Vec::new();

        for ((name, labels), value) in &store.counters {
            if !described.contains(&name.as_str()) {
                out.push_str(&format!("# TYPE {name} counter\n"));
                described.push(name);
            }
            out.push_str(&format!("{name}{} {value}\n", render_labels(labels, None)));
        }

        for ((name, labels), value) in &store.gauges {
            if !described.contains(&name.as_str()) {
                out.push_str(&format!("# TYPE {name} gauge\n"));
                described.push(name);
            }
            out.push_str(&format!("{name}{} {value}\n", render_labels(labels, None)));
        }

        for ((name, labels), histogram) in &store.histograms {
            if !described.contains(&name.as_str()) {
                out.push_str(&format!("# TYPE {name} histogram\n"));
                described.push(name);
            }
            for (index, edge) in BUCKETS.iter().enumerate() {
                out.push_str(&format!(
                    "{name}_bucket{} {}\n",
                    render_labels(labels, Some(&edge.to_string())),
                    histogram.counts[index]
                ));
            }
            out.push_str(&format!(
                "{name}_bucket{} {}\n",
                render_labels(labels, Some("+Inf")),
                histogram.counts[BUCKETS.len()]
            ));
            out.push_str(&format!("{name}_sum{} {}\n", render_labels(labels, None), histogram.sum));
            out.push_str(&format!(
                "{name}_count{} {}\n",
                render_labels(labels, None),
                histogram.total
            ));
        }

        out
    }
}

fn label_set(labels: &[(&str, &str)]) -> Labels {
    labels.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
}

fn render_labels(labels: &Labels, bucket: Option<&str>) -> String {
    if labels.is_empty() && bucket.is_none() {
        return String::new();
    }

    let mut parts: Vec<String> = labels
        .iter()
        .map(|(name, value)| format!("{name}=\"{}\"", escape(value)))
        .collect();
    if let Some(edge) = bucket {
        parts.push(format!("le=\"{edge}\""));
    }
    format!("{{{}}}", parts.join(","))
}

/// A label value may contain anything, and unescaped it would break the format.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Turns framework events into metrics.
struct Collector {
    registry: Registry,
}

impl Subscriber for Collector {
    fn interested_in(&self, kind: &str) -> bool {
        matches!(kind, "http.request" | "db.query" | "queue.processed" | "queue.failed" | "ai.call")
    }

    fn handle(&self, event: &Event) {
        let seconds = event.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0);

        match event.kind {
            "http.request" => {
                // Labelled by route pattern, not path: one series per route
                // rather than one per id, which is what keeps cardinality sane.
                let route = event
                    .field("route")
                    .and_then(Json::as_str)
                    .or_else(|| event.field("path").and_then(Json::as_str))
                    .unwrap_or("unknown")
                    .to_string();
                let method =
                    event.field("method").and_then(Json::as_str).unwrap_or("GET").to_string();
                let status = event
                    .field("status")
                    .and_then(Json::as_i64)
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "0".into());

                let labels = [("method", method.as_str()), ("route", route.as_str())];
                self.registry.observe("rustlavel_http_request_duration_seconds", &labels, seconds);
                self.registry.increment(
                    "rustlavel_http_requests_total",
                    &[
                        ("method", method.as_str()),
                        ("route", route.as_str()),
                        ("status", status.as_str()),
                    ],
                );
            }
            "db.query" => {
                let ok = event.field("ok").and_then(Json::as_bool).unwrap_or(true);
                self.registry.observe("rustlavel_db_query_duration_seconds", &[], seconds);
                self.registry
                    .increment("rustlavel_db_queries_total", &[("ok", if ok { "true" } else { "false" })]);
            }
            "queue.processed" | "queue.failed" => {
                let job = event.field("job").and_then(Json::as_str).unwrap_or("unknown").to_string();
                let outcome = if event.kind == "queue.failed" { "failed" } else { "processed" };
                self.registry.increment(
                    "rustlavel_queue_jobs_total",
                    &[("job", job.as_str()), ("outcome", outcome)],
                );
                if event.kind == "queue.processed" {
                    self.registry.observe(
                        "rustlavel_queue_job_duration_seconds",
                        &[("job", job.as_str())],
                        seconds,
                    );
                }
            }
            "ai.call" => {
                let provider =
                    event.field("provider").and_then(Json::as_str).unwrap_or("unknown").to_string();
                let model = event.field("model").and_then(Json::as_str).unwrap_or("").to_string();
                let labels = [("provider", provider.as_str()), ("model", model.as_str())];

                self.registry.increment("rustlavel_ai_calls_total", &labels);
                self.registry.observe("rustlavel_ai_call_duration_seconds", &labels, seconds);
                for (field, name) in
                    [("input_tokens", "rustlavel_ai_input_tokens_total"), ("output_tokens", "rustlavel_ai_output_tokens_total")]
                {
                    if let Some(tokens) = event.field(field).and_then(Json::as_i64) {
                        self.registry.add(name, &labels, tokens.max(0) as u64);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Exposes `/metrics` and starts collecting.
pub struct Metrics {
    path: String,
    registry: Registry,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics { path: "/metrics".into(), registry: Registry::new() }
    }
}

impl Metrics {
    pub fn new() -> Self {
        Metrics::default()
    }

    pub fn at(mut self, path: &str) -> Self {
        self.path = path.to_string();
        self
    }

    pub fn from_config(config: &Config) -> Self {
        Metrics::new().at(&config.string("metrics.route", "/metrics"))
    }

    /// The registry, so an application can record its own metrics too.
    pub fn registry(&self) -> Registry {
        self.registry.clone()
    }

    /// Start collecting without mounting a route — for a worker process, which
    /// has metrics worth pushing but no HTTP server.
    pub fn collect_only(&self) {
        subscribe(Collector { registry: self.registry.clone() });
    }
}

impl Plugin for Metrics {
    fn name(&self) -> &'static str {
        "metrics"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        let registry = self.registry.clone();
        subscribe(Collector { registry: registry.clone() });

        // Registered as state so an application can record its own counters.
        setup.state(registry.clone());

        setup.router.get(&self.path, move |_request: Request| {
            let registry = registry.clone();
            async move {
                Response::new(Status::OK)
                    // The exposition format's own content type; Prometheus
                    // accepts text/plain but this is what it advertises.
                    .with_header("content-type", "text/plain; version=0.0.4; charset=utf-8")
                    .with_body(registry.render())
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::events::clear_subscribers;
    use rustlavel_http::{Router, TestClient};

    /// The event bus is process-wide, so tests that use it run one at a time.
    ///
    /// A tokio mutex rather than a std one: these tests await while holding it,
    /// which a std guard must never do.
    static BUS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn counters_render_with_their_labels() {
        let registry = Registry::new();
        registry.increment("http_requests_total", &[("method", "GET"), ("status", "200")]);
        registry.increment("http_requests_total", &[("method", "GET"), ("status", "200")]);

        let rendered = registry.render();
        assert!(rendered.contains("# TYPE http_requests_total counter"));
        assert!(rendered.contains(r#"http_requests_total{method="GET",status="200"} 2"#));
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let registry = Registry::new();
        registry.observe("latency_seconds", &[], 0.003);
        registry.observe("latency_seconds", &[], 0.4);

        let rendered = registry.render();
        // The 5ms bucket holds only the fast one; +Inf holds both.
        assert!(rendered.contains(r#"latency_seconds_bucket{le="0.005"} 1"#));
        assert!(rendered.contains(r#"latency_seconds_bucket{le="0.5"} 2"#));
        assert!(rendered.contains(r#"latency_seconds_bucket{le="+Inf"} 2"#));
        assert!(rendered.contains("latency_seconds_count 2"));
    }

    #[test]
    fn an_observation_past_every_bucket_still_counts() {
        let registry = Registry::new();
        registry.observe("slow_seconds", &[], 30.0);

        let rendered = registry.render();
        assert!(rendered.contains(r#"slow_seconds_bucket{le="10"} 0"#));
        assert!(rendered.contains(r#"slow_seconds_bucket{le="+Inf"} 1"#));
    }

    #[test]
    fn label_values_are_escaped() {
        let registry = Registry::new();
        registry.increment("thing_total", &[("label", "a\"b\\c")]);

        assert!(registry.render().contains(r#"label="a\"b\\c""#));
    }

    #[test]
    fn a_gauge_replaces_rather_than_accumulates() {
        let registry = Registry::new();
        registry.set("workers", &[], 3.0);
        registry.set("workers", &[], 5.0);

        assert!(registry.render().contains("workers 5"));
    }

    #[tokio::test]
    async fn requests_are_counted_by_route_not_by_path() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        router.get("/orders/{id}", |_req: Request| async { "order" });

        let metrics = Metrics::new();
        let registry = metrics.registry();
        let mut context = Some(rustlavel_core::Context::builder());
        let config = Config::new();
        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };
        Box::new(metrics).register(&mut setup);

        let client = TestClient::new(router);
        client.get("/orders/1").await.assert_ok();
        client.get("/orders/2").await.assert_ok();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let rendered = client.get("/metrics").await.assert_ok().body();

        // Two different ids, one series — otherwise every id becomes a
        // timeseries and the scrape falls over.
        assert!(
            rendered.contains(r#"route="/orders/{id}",status="200"} 2"#),
            "{rendered}"
        );
        assert!(!rendered.contains("/orders/1"));
        assert_eq!(
            registry.observations(
                "rustlavel_http_request_duration_seconds",
                &[("method", "GET"), ("route", "/orders/{id}")]
            ),
            2
        );
        clear_subscribers();
    }

    #[tokio::test]
    async fn the_endpoint_advertises_the_exposition_format() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let mut router = Router::new();
        let config = Config::new();
        let mut context = Some(rustlavel_core::Context::builder());
        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };
        Box::new(Metrics::new().at("/internal/metrics")).register(&mut setup);

        TestClient::new(router)
            .get("/internal/metrics")
            .await
            .assert_ok()
            .assert_header("content-type", "text/plain; version=0.0.4; charset=utf-8");

        clear_subscribers();
    }

    #[tokio::test]
    async fn database_and_ai_events_become_metrics() {
        let _guard = BUS.lock().await;
        clear_subscribers();

        let registry = Registry::new();
        subscribe(Collector { registry: registry.clone() });

        Event::new("db.query")
            .with("sql", "select 1")
            .with("ok", true)
            .took(std::time::Duration::from_millis(4))
            .dispatch();
        Event::new("ai.call")
            .with("provider", "anthropic")
            .with("model", "claude-sonnet-5")
            .with("input_tokens", 120)
            .with("output_tokens", 40)
            .took(std::time::Duration::from_millis(900))
            .dispatch();

        assert_eq!(registry.counter("rustlavel_db_queries_total", &[("ok", "true")]), 1);

        let ai = [("provider", "anthropic"), ("model", "claude-sonnet-5")];
        assert_eq!(registry.counter("rustlavel_ai_calls_total", &ai), 1);
        assert_eq!(registry.counter("rustlavel_ai_input_tokens_total", &ai), 120);
        assert_eq!(registry.counter("rustlavel_ai_output_tokens_total", &ai), 40);

        clear_subscribers();
    }
}
