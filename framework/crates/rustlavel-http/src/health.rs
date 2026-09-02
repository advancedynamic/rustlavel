//! Health endpoints, for whatever is deciding whether to send traffic here.
//!
//! Two questions, two routes, because they have different answers. `GET /up`
//! asks *is the process alive* — it returns 200 as long as the server can
//! answer at all, and a load balancer or Kubernetes liveness probe uses it to
//! decide whether to restart the process. `GET /up/ready` asks *can it do
//! useful work* — it runs every registered check and returns 503 if any fails,
//! and a readiness probe uses it to decide whether to route requests here.
//! Conflating the two is how a database outage turns into a restart loop.
//!
//! ```ignore
//! App::new()?.plugin(
//!     Health::new()
//!         .check("database", |req| {
//!             let db = req.state::<Database>().cloned();
//!             async move {
//!                 let db = db.ok_or("no database configured")?;
//!                 db.scalar::<i64>("select 1", &[]).await.map(|_| ()).map_err(|e| e.to_string())
//!             }
//!         })
//!         .check("cache", |req| { … }),
//! )
//! ```
//!
//! The path is `/up`, as in Laravel 11, so anything already probing a Laravel
//! application needs no change.

use crate::handler::BoxFuture;
use crate::plugin::{Plugin, Setup};
use crate::request::Request;
use crate::response::Response;
use crate::status::Status;
use rustlavel_core::Json;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

type CheckFn = Arc<dyn Fn(&Request) -> BoxFuture<Result<(), String>> + Send + Sync>;

#[derive(Clone)]
pub struct Health {
    path: String,
    checks: Vec<(String, CheckFn)>,
    /// How long a single check may take before it counts as failed.
    timeout: Duration,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl Health {
    pub fn new() -> Self {
        Health { path: "/up".to_string(), checks: Vec::new(), timeout: Duration::from_secs(5) }
    }

    /// Serve from a different path: `/healthz`, `/_health`.
    pub fn at(mut self, path: &str) -> Self {
        self.path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
        self
    }

    /// Register a readiness check. It receives the request so it can reach
    /// application state; `Err(reason)` marks the check — and the whole
    /// readiness response — as failed.
    pub fn check<F, Fut>(mut self, name: &str, check: F) -> Self
    where
        F: Fn(&Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.checks.push((name.to_string(), Arc::new(move |req| Box::pin(check(req)))));
        self
    }

    /// The most a check may take. A dependency that hangs must be reported
    /// as down, not left to hang the probe with it.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn readiness(checks: Vec<(String, CheckFn)>, timeout: Duration, request: Request) -> Response {
        // All checks run at once: a probe that waits for the database and
        // *then* for the cache takes the sum of their latencies for no reason.
        let futures: Vec<_> = checks
            .iter()
            .map(|(name, check)| {
                let name = name.clone();
                let future = check(&request);
                async move {
                    let started = Instant::now();
                    let outcome = match tokio::time::timeout(timeout, future).await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(reason)) => Err(reason),
                        Err(_) => Err(format!("no answer within {} ms", timeout.as_millis())),
                    };
                    (name, outcome, started.elapsed())
                }
            })
            .collect();

        let results = join_all(futures).await;
        let healthy = results.iter().all(|(_, outcome, _)| outcome.is_ok());

        let checks = Json::object(results.into_iter().map(|(name, outcome, took)| {
            let mut fields = vec![
                ("status", Json::from(if outcome.is_ok() { "ok" } else { "failed" })),
                ("duration_ms", Json::Number(took.as_secs_f64() * 1000.0)),
            ];
            if let Err(reason) = outcome {
                fields.push(("error", Json::from(reason)));
            }
            (name, Json::object(fields))
        }));

        let body = Json::object([
            ("status", Json::from(if healthy { "ok" } else { "failed" })),
            ("checks", checks),
        ]);
        let status = if healthy { Status::OK } else { Status::SERVICE_UNAVAILABLE };
        Response::new(status).with_json(body).with_header("cache-control", "no-store")
    }
}

/// Wait for every future, keeping the order.
///
/// A small join rather than a dependency: the checks are few and the result
/// is needed in the order they were registered, which is the order a person
/// reading the JSON expects.
async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut handles = Vec::with_capacity(futures.len());
    for future in futures {
        handles.push(Box::pin(future));
    }
    let mut results: Vec<Option<F::Output>> = (0..handles.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (slot, handle) in results.iter_mut().zip(handles.iter_mut()) {
            if slot.is_none() {
                match handle.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => *slot = Some(value),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending { std::task::Poll::Pending } else { std::task::Poll::Ready(()) }
    })
    .await;
    results.into_iter().map(|slot| slot.expect("every future completed")).collect()
}

impl Plugin for Health {
    fn name(&self) -> &'static str {
        "health"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        let ready_path = format!("{}/ready", self.path);
        let checks = self.checks;
        let timeout = self.timeout;

        setup
            .router
            .get(&self.path, |_req: Request| async {
                Response::json(Json::object([("status", Json::from("ok"))]))
                    .with_header("cache-control", "no-store")
            })
            .name("health.up")
            .describe("Liveness: the process is running and answering")
            .tag("Health");

        setup
            .router
            .get(&ready_path, move |req: Request| Health::readiness(checks.clone(), timeout, req))
            .name("health.ready")
            .describe("Readiness: every dependency check passes")
            .responds(503, "At least one check failed")
            .tag("Health");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::Router;
    use crate::testing::TestClient;

    fn client(health: Health) -> TestClient {
        let mut router = Router::new();
        let config = rustlavel_core::Config::new();
        let mut context = Some(rustlavel_core::Context::builder());
        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };
        Box::new(health).register(&mut setup);
        TestClient::new(router)
    }

    #[tokio::test]
    async fn liveness_is_always_ok() {
        let response = client(Health::new()).get("/up").await;
        let response = response.assert_ok();
        assert_eq!(response.json().get("status").and_then(Json::as_str), Some("ok"));
        assert_eq!(response.header("cache-control"), Some("no-store"));
    }

    #[tokio::test]
    async fn readiness_with_no_checks_is_ok() {
        let response = client(Health::new()).get("/up/ready").await;
        response.assert_ok().assert_json("status", "ok");
    }

    #[tokio::test]
    async fn readiness_reports_each_check_and_fails_if_any_does() {
        let health = Health::new()
            .check("database", |_req| async { Ok(()) })
            .check("cache", |_req| async { Err("connection refused".to_string()) });

        let response = client(health).get("/up/ready").await;
        let response = response.assert_status(503);
        let body = response.json();
        assert_eq!(body.get("status").and_then(Json::as_str), Some("failed"));
        assert_eq!(body.get("checks.database.status").and_then(Json::as_str), Some("ok"));
        assert_eq!(body.get("checks.cache.status").and_then(Json::as_str), Some("failed"));
        assert_eq!(body.get("checks.cache.error").and_then(Json::as_str), Some("connection refused"));
        assert!(body.get("checks.database.duration_ms").is_some());
    }

    #[tokio::test]
    async fn a_hanging_check_is_reported_as_failed_not_waited_for() {
        let health = Health::new()
            .timeout(Duration::from_millis(50))
            .check("slow", |_req| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            });

        let started = Instant::now();
        let response = client(health).get("/up/ready").await;
        assert!(started.elapsed() < Duration::from_secs(5), "the probe must not hang with the check");
        let response = response.assert_status(503);
        let error = response.json().get("checks.slow.error").and_then(Json::as_str).unwrap().to_string();
        assert!(error.contains("no answer within 50 ms"), "{error}");
    }

    #[tokio::test]
    async fn checks_run_concurrently() {
        let mut health = Health::new();
        for i in 0..5 {
            health = health.check(&format!("dep{i}"), |_req| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(())
            });
        }
        let started = Instant::now();
        client(health).get("/up/ready").await.assert_ok();
        // Five 100 ms checks in series would be half a second.
        assert!(started.elapsed() < Duration::from_millis(400), "took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_check_can_reach_application_state() {
        struct Flag(bool);
        let health = Health::new().check("flag", |req| {
            let ok = req.state::<Flag>().map(|f| f.0);
            async move { if ok == Some(true) { Ok(()) } else { Err("flag not set".to_string()) } }
        });

        let mut router = Router::new();
        let config = rustlavel_core::Config::new();
        let mut context = Some(rustlavel_core::Context::builder().state(Flag(true)));
        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };
        Box::new(health).register(&mut setup);
        let client = TestClient::new(router).with_context(context.unwrap().build());

        client.get("/up/ready").await.assert_ok();
    }

    #[tokio::test]
    async fn the_path_is_configurable() {
        let client = client(Health::new().at("healthz"));
        client.get("/healthz").await.assert_ok();
        client.get("/healthz/ready").await.assert_ok();
        client.get("/up").await.assert_not_found();
    }
}
