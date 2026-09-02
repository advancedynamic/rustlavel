//! The `throttle` middleware.
//!
//! ```ignore
//! let cache = CacheStore::from_config(&config)?;
//! router.group("/api", |r| {
//!     r.get("/search", search);
//! })
//! .middleware(Throttle::per_minute(&cache, 60));
//! ```
//!
//! Every response carries `X-RateLimit-Limit` and `X-RateLimit-Remaining`, so a
//! client can slow itself down before it is refused. A refused request gets 429
//! with `Retry-After` as well — the one header that actually tells a
//! well-behaved client what to do.

use crate::config::CacheStore;
use crate::rate_limit::{RateLimit, RateLimiter};
use crate::store::Cache;
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::{Middleware, Next, Request, Response, Status};
use rustlavel_core::Json;
use std::sync::Arc;
use std::time::Duration;

/// Builds the bucket key for a request.
type KeyFn = Arc<dyn Fn(&Request) -> String + Send + Sync>;

/// Limits how often one client may hit the routes it guards.
#[derive(Clone)]
pub struct Throttle {
    limiter: RateLimiter,
    max: u64,
    window: Duration,
    key: KeyFn,
}

impl Throttle {
    /// `max` requests per `window`, keyed by client IP and route.
    pub fn new(cache: &CacheStore, max: u64, window: Duration) -> Self {
        Throttle {
            limiter: RateLimiter::new(cache.driver_handle()),
            max,
            window,
            key: Arc::new(default_key),
        }
    }

    /// The common case, and the one Laravel spells `throttle:60,1`.
    pub fn per_minute(cache: &CacheStore, max: u64) -> Self {
        Throttle::new(cache, max, Duration::from_secs(60))
    }

    pub fn per_second(cache: &CacheStore, max: u64) -> Self {
        Throttle::new(cache, max, Duration::from_secs(1))
    }

    /// Build directly on a driver, for tests and for callers that never made a
    /// [`CacheStore`].
    pub fn with_driver(store: Arc<dyn Cache>, max: u64, window: Duration) -> Self {
        Throttle { limiter: RateLimiter::new(store), max, window, key: Arc::new(default_key) }
    }

    /// Key the bucket by something other than the client IP: an API token, a
    /// tenant, an authenticated user id.
    ///
    /// Worth doing whenever requests arrive through a NAT or a mobile carrier,
    /// where thousands of unrelated users share one address.
    pub fn by(mut self, key: impl Fn(&Request) -> String + Send + Sync + 'static) -> Self {
        self.key = Arc::new(key);
        self
    }

    fn headers(response: Response, outcome: &RateLimit) -> Response {
        response
            .with_header("x-ratelimit-limit", outcome.limit.to_string())
            .with_header("x-ratelimit-remaining", outcome.remaining.to_string())
    }
}

/// IP plus route, so a client that is being throttled on `/api/search` can
/// still reach `/api/health`.
///
/// A request with no discoverable IP falls into one shared bucket rather than
/// escaping the limit — failing closed is the only safe direction here.
fn default_key(request: &Request) -> String {
    let who = request.ip().unwrap_or_else(|| "unknown".to_string());
    let what = request.route().unwrap_or_else(|| request.path());
    format!("{who}|{what}")
}

impl Middleware for Throttle {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        let limiter = self.limiter.clone();
        let max = self.max;
        let window = self.window;
        let key = (self.key)(&request);

        Box::pin(async move {
            let outcome = match limiter.attempt(&key, max, window).await {
                Ok(outcome) => outcome,
                // A cache that is down must not take the whole site with it:
                // the request goes through unthrottled rather than 500ing.
                Err(_) => return next.run(request).await,
            };

            if outcome.exceeded {
                let retry_after = outcome.retry_after_seconds();
                let body = Json::object([
                    ("message", Json::from("Too many requests.")),
                    ("retry_after", Json::from(retry_after)),
                ]);

                let response = Response::new(Status::TOO_MANY_REQUESTS)
                    .with_json(body)
                    .with_header("retry-after", retry_after.to_string())
                    .with_header("x-ratelimit-reset", outcome.reset_at().to_string());
                return Throttle::headers(response, &outcome);
            }

            Throttle::headers(next.run(request).await, &outcome)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    use rustlavel_http::{Method, Router, TestClient};

    fn client(throttle: Throttle) -> TestClient {
        let mut router = Router::new();
        router.get("/api/search", |_req: Request| async { "results" });
        router.get("/api/health", |_req: Request| async { "ok" });
        router.middleware(throttle);
        TestClient::new(router)
    }

    fn store() -> Arc<dyn Cache> {
        Arc::new(MemoryStore::new())
    }

    /// A request from a given address.
    ///
    /// The peer address, not `X-Forwarded-For`: a header is not evidence of
    /// where a request came from unless `TrustProxies` says the connection
    /// came from a proxy, and a limiter keyed on an unverified header is one
    /// a client escapes by sending a different value each time.
    fn from(ip: &str, path: &str) -> Request {
        Request::new(Method::Get, path).with_peer(format!("{ip}:44321").parse().expect("an address"))
    }

    #[tokio::test]
    async fn the_first_requests_pass_and_carry_the_rate_limit_headers() {
        let client = client(Throttle::with_driver(store(), 3, Duration::from_secs(60)));

        for expected_remaining in ["2", "1", "0"] {
            client
                .send(from("10.0.0.1", "/api/search"))
                .await
                .assert_ok()
                .assert_see("results")
                .assert_header("x-ratelimit-limit", "3")
                .assert_header("x-ratelimit-remaining", expected_remaining);
        }
    }

    #[tokio::test]
    async fn the_request_after_the_limit_is_refused_with_429_and_retry_after() {
        let client = client(Throttle::with_driver(store(), 2, Duration::from_secs(60)));

        client.send(from("10.0.0.2", "/api/search")).await.assert_ok();
        client.send(from("10.0.0.2", "/api/search")).await.assert_ok();

        let refused = client
            .send(from("10.0.0.2", "/api/search"))
            .await
            .assert_status(429)
            .assert_header("x-ratelimit-limit", "2")
            .assert_header("x-ratelimit-remaining", "0")
            .assert_json("message", "Too many requests.");

        let retry_after: u64 =
            refused.header("retry-after").expect("a 429 must say when to come back").parse().unwrap();
        assert!((1..=60).contains(&retry_after), "retry-after was {retry_after}");
        assert!(refused.header("x-ratelimit-reset").is_some());
    }

    #[tokio::test]
    async fn two_client_addresses_get_their_own_allowance() {
        let client = client(Throttle::with_driver(store(), 1, Duration::from_secs(60)));

        client.send(from("10.0.0.3", "/api/search")).await.assert_ok();
        client.send(from("10.0.0.3", "/api/search")).await.assert_status(429);

        // A different address is a different bucket entirely.
        client.send(from("10.0.0.4", "/api/search")).await.assert_ok();
    }

    #[tokio::test]
    async fn two_routes_get_their_own_allowance() {
        let client = client(Throttle::with_driver(store(), 1, Duration::from_secs(60)));

        client.send(from("10.0.0.5", "/api/search")).await.assert_ok();
        client.send(from("10.0.0.5", "/api/search")).await.assert_status(429);

        client.send(from("10.0.0.5", "/api/health")).await.assert_ok();
    }

    #[tokio::test]
    async fn a_custom_key_function_replaces_the_ip() {
        let throttle = Throttle::with_driver(store(), 1, Duration::from_secs(60))
            .by(|request: &Request| request.header("x-api-key").unwrap_or("anonymous").to_string());

        let client = client(throttle);

        let with_token = |token: &str| {
            Request::new(Method::Get, "/api/search")
                .with_peer("10.0.0.6:44321".parse().expect("an address"))
                .with_header("x-api-key", token)
        };

        client.send(with_token("alpha")).await.assert_ok();
        client.send(with_token("alpha")).await.assert_status(429);
        // Same IP, different token: the IP is no longer what is being counted.
        client.send(with_token("beta")).await.assert_ok();
    }

    #[tokio::test]
    async fn the_allowance_comes_back_when_the_window_passes() {
        // A short window, and only one request inside it. Asserting the 429
        // here too would need both requests to land inside 100ms, which a busy
        // machine cannot promise — that is covered separately, with a window
        // long enough that timing cannot enter into it.
        let window = Duration::from_millis(200);
        let client = client(Throttle::with_driver(store(), 1, window));

        client.send(from("10.0.0.7", "/api/search")).await.assert_ok();

        tokio::time::sleep(window * 3).await;
        client.send(from("10.0.0.7", "/api/search")).await.assert_ok();
    }

    #[tokio::test]
    async fn the_second_request_inside_the_window_is_refused() {
        // A minute-long window, so the two requests are inside it whatever else
        // the machine is doing.
        let client = client(Throttle::with_driver(store(), 1, Duration::from_secs(60)));

        client.send(from("10.0.0.8", "/api/search")).await.assert_ok();
        client.send(from("10.0.0.8", "/api/search")).await.assert_status(429);
    }

    #[tokio::test]
    async fn the_handler_never_runs_once_the_limit_is_reached() {
        let mut router = Router::new();
        router.get("/once", |_req: Request| async {
            // Passing this a second time would mean the middleware let a
            // refused request through to the handler.
            static SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let count = SEEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(count, 0, "the handler ran after the limit was reached");
            "ok"
        });
        router.middleware(Throttle::with_driver(store(), 1, Duration::from_secs(60)));

        let client = TestClient::new(router);
        client.send(from("10.0.0.8", "/once")).await.assert_ok();
        client.send(from("10.0.0.8", "/once")).await.assert_status(429);
    }

    #[tokio::test]
    async fn a_request_without_an_ip_still_falls_under_a_limit() {
        let client = client(Throttle::with_driver(store(), 1, Duration::from_secs(60)));

        // No peer address and no forwarded header: fail closed, not open.
        client.send(Request::new(Method::Get, "/api/search")).await.assert_ok();
        client.send(Request::new(Method::Get, "/api/search")).await.assert_status(429);
    }

    #[tokio::test]
    async fn a_throttle_built_from_a_cache_store_works_the_same_way() {
        let store = CacheStore::from_driver(MemoryStore::new());
        let client = client(Throttle::per_minute(&store, 1));

        client.send(from("10.0.0.9", "/api/search")).await.assert_ok();
        client.send(from("10.0.0.9", "/api/search")).await.assert_status(429);
    }
}
