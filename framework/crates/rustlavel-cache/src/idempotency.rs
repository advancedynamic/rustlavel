//! Idempotency keys: make `POST /payments` safe to retry.
//!
//! A client sends a payment, the network drops before the answer arrives, and
//! now it does not know whether the payment happened. Retrying risks charging
//! twice; not retrying risks not charging at all. The way out, which Stripe
//! made standard, is for the client to name each attempt: an `Idempotency-Key`
//! header, chosen by the client, that the server remembers. The first request
//! with a key runs; every later one with the same key gets the first one's
//! response back, without running anything.
//!
//! ```ignore
//! r.group("/api", |api| {
//!     api.middleware(Idempotency::new(&cache));
//!     api.post("/payments", PaymentController::store);
//! });
//! ```
//!
//! Three things the client can see:
//!
//! - a replay carries `Idempotent-Replayed: true`, so a client can tell a
//!   remembered answer from a fresh one;
//! - the same key with a *different* request — another amount, another path —
//!   is a `422`, because silently returning the old answer to a new question
//!   is how a client ends up believing something that is not true;
//! - a key whose first request is still running is a `409`, with
//!   `Retry-After: 1`, rather than a second execution.
//!
//! Only 2xx and 4xx answers are remembered. A 5xx means the server failed,
//! and the client's retry should get another go, not a copy of the failure.
//!
//! Keys are scoped to the caller — the `Authorization` header when there is
//! one, otherwise the client address — so two tenants who both chose
//! `order-1` do not collide. Change the scope with [`Idempotency::scope_by`]
//! when the application has a better notion of who is calling.

use crate::store::Cache;
use rustlavel_core::Json;
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::{Middleware, Method, Next, Request, Response, Status};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

type ScopeFn = Arc<dyn Fn(&Request) -> String + Send + Sync>;

#[derive(Clone)]
pub struct Idempotency {
    store: Arc<dyn Cache>,
    header: String,
    ttl: Duration,
    scope: ScopeFn,
    required: bool,
}

impl Idempotency {
    /// Remember answers for 24 hours, which is Stripe's window and long
    /// enough for any retry policy a client would plausibly run.
    pub fn new(cache: &crate::CacheStore) -> Self {
        Idempotency::with_driver(cache.driver_handle())
    }

    pub fn with_driver(store: Arc<dyn Cache>) -> Self {
        Idempotency {
            store,
            header: "idempotency-key".to_string(),
            ttl: Duration::from_secs(24 * 60 * 60),
            scope: Arc::new(default_scope),
            required: false,
        }
    }

    /// Read the key from a different header.
    pub fn header(mut self, name: &str) -> Self {
        self.header = name.to_ascii_lowercase();
        self
    }

    /// How long an answer is remembered.
    pub fn remember_for(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Decide whose key it is — a user id from the auth middleware, a tenant.
    pub fn scope_by(mut self, scope: impl Fn(&Request) -> String + Send + Sync + 'static) -> Self {
        self.scope = Arc::new(scope);
        self
    }

    /// Refuse a write that carries no key, with a 400 that says so.
    ///
    /// Off by default, because most endpoints are fine without one. On for a
    /// payments API, where a client that forgets the header has a bug that
    /// should be found in development rather than in the ledger.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
}

/// `Authorization` when present, since that names the caller; the address
/// otherwise. Unknown callers share one scope rather than escaping it.
fn default_scope(request: &Request) -> String {
    if let Some(auth) = request.header("authorization") {
        let mut hasher = std::hash::DefaultHasher::new();
        auth.hash(&mut hasher);
        return format!("auth:{:016x}", hasher.finish());
    }
    format!("ip:{}", request.ip().unwrap_or_else(|| "unknown".to_string()))
}

/// What a request *is*, so the same key with a different request is caught.
fn fingerprint(request: &Request) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    request.method().as_str().hash(&mut hasher);
    request.path().hash(&mut hasher);
    request.body().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn key_looks_valid(key: &str) -> bool {
    !key.is_empty() && key.len() <= 255 && key.bytes().all(|b| b.is_ascii_graphic())
}

/// A response flattened to JSON for the store, and back.
///
/// Bodies are kept as text when they are text, which for an API they nearly
/// always are, and as hex when they are not. Hex rather than base64 because
/// the cache crate has no base64 and a second copy of one would be a second
/// place for it to be wrong.
fn freeze(response: &Response, fingerprint: &str) -> Json {
    let headers: Vec<Json> = response
        .headers
        .iter()
        .map(|(name, value)| Json::Array(vec![Json::from(name), Json::from(value)]))
        .collect();
    let (encoding, body) = match std::str::from_utf8(&response.body) {
        Ok(text) => ("utf8", text.to_string()),
        Err(_) => ("hex", response.body.iter().map(|b| format!("{b:02x}")).collect()),
    };
    Json::object([
        ("status", Json::from(i64::from(response.status.code()))),
        ("headers", Json::Array(headers)),
        ("encoding", Json::from(encoding)),
        ("body", Json::from(body)),
        ("fingerprint", Json::from(fingerprint)),
    ])
}

fn thaw(frozen: &Json) -> Option<Response> {
    let status = u16::try_from(frozen.get("status")?.as_i64()?).ok()?;
    let mut response = Response::new(Status::from(status));
    for pair in frozen.get("headers")?.as_array()? {
        let pair = pair.as_array()?;
        response.headers.append(pair.first()?.as_str()?, pair.get(1)?.as_str()?);
    }
    let body = frozen.get("body")?.as_str()?;
    response.body = match frozen.get("encoding")?.as_str()? {
        "hex" => body
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
            .collect::<Option<Vec<u8>>>()?,
        _ => body.as_bytes().to_vec(),
    };
    Some(response)
}

fn problem(status: Status, message: &str) -> Response {
    Response::new(status).with_json(Json::object([("message", Json::from(message))]))
}

impl Middleware for Idempotency {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        // Reads are idempotent by definition; only a write needs remembering.
        if matches!(request.method(), Method::Get | Method::Head | Method::Options) {
            return next.run(request);
        }

        let key = match request.header(&self.header) {
            Some(key) if key_looks_valid(key) => key.to_string(),
            Some(_) => {
                return Box::pin(async move {
                    problem(
                        Status::BAD_REQUEST,
                        "The idempotency key must be 1–255 printable ASCII characters.",
                    )
                });
            }
            None if self.required => {
                let header = self.header.clone();
                return Box::pin(async move {
                    problem(
                        Status::BAD_REQUEST,
                        &format!("This endpoint requires an {header} header on every write."),
                    )
                });
            }
            None => return next.run(request),
        };

        let store = Arc::clone(&self.store);
        let ttl = self.ttl;
        let scope = (self.scope)(&request);
        let print = fingerprint(&request);
        let lock_key = format!("idempotency:{scope}:{key}:lock");
        let response_key = format!("idempotency:{scope}:{key}:response");

        Box::pin(async move {
            // The increment is atomic in every driver, which makes it a lock:
            // exactly one caller sees 1 and runs; everyone else finds either a
            // stored answer or a request still in flight.
            let claim = match store.increment_within(&lock_key, 1, ttl).await {
                Ok(n) => n,
                // A store that is down must not stop the API. The request runs
                // once, unprotected, which is what would have happened anyway.
                Err(_) => return next.run(request).await,
            };

            if claim > 1 {
                return match store.get(&response_key).await {
                    Ok(Some(frozen)) => {
                        if frozen.get("fingerprint").and_then(Json::as_str) != Some(print.as_str()) {
                            return problem(
                                Status::UNPROCESSABLE,
                                "This idempotency key was already used for a different request.",
                            );
                        }
                        match thaw(&frozen) {
                            Some(response) => response.with_header("idempotent-replayed", "true"),
                            None => problem(Status::INTERNAL_ERROR, "The remembered response could not be read."),
                        }
                    }
                    Ok(None) => problem(
                        Status::CONFLICT,
                        "A request with this idempotency key is still being processed.",
                    )
                    .with_header("retry-after", "1"),
                    Err(_) => next.run(request).await,
                };
            }

            let response = next.run(request).await;

            if response.status.code() >= 500 {
                // Our failure, not the client's. Let the retry try again.
                let _ = store.forget(&lock_key).await;
            } else {
                let _ = store.put(&response_key, freeze(&response, &print), ttl).await;
            }
            response
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    use rustlavel_http::{Router, TestClient};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn store() -> Arc<dyn Cache> {
        Arc::new(MemoryStore::new())
    }

    fn client(idempotency: Idempotency) -> (TestClient, Arc<AtomicUsize>) {
        let executions = Arc::new(AtomicUsize::new(0));
        let counter = executions.clone();
        let mut router = Router::new();
        router.middleware(idempotency);
        router.post("/payments", move |req: Request| {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                Response::new(Status::CREATED)
                    .with_json(Json::object([("execution", Json::from(n as i64)), ("body", Json::from(req.body_string()))]))
                    .with_header("location", format!("/payments/{n}"))
            }
        });
        router.post("/failing", |_req: Request| async { Response::new(Status::INTERNAL_ERROR).with_text("boom") });
        router.post("/binary", |_req: Request| async {
            Response::ok().with_header("content-type", "application/octet-stream").with_body(vec![0u8, 255, 1, 254])
        });
        router.get("/payments", |_req: Request| async { Response::text("list") });
        (TestClient::new(router), executions)
    }

    fn post(path: &str, key: &str, body: &str) -> Request {
        Request::new(Method::Post, path)
            .with_header("idempotency-key", key)
            .with_peer("10.0.0.1:44321".parse().expect("an address"))
            .with_body(body.as_bytes().to_vec())
    }

    #[tokio::test]
    async fn the_second_request_with_a_key_is_a_replay_not_an_execution() {
        let (client, executions) = client(Idempotency::with_driver(store()));

        let first = client.send(post("/payments", "order-1", "amount=10")).await;
        let first = first.assert_status(201);
        assert_eq!(first.header("idempotent-replayed"), None);

        let second = client.send(post("/payments", "order-1", "amount=10")).await;
        let second = second.assert_status(201);
        assert_eq!(second.header("idempotent-replayed"), Some("true"));
        assert_eq!(second.body(), first.body(), "byte for byte the first answer");
        assert_eq!(second.header("location"), Some("/payments/1"), "headers come back too");
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_different_key_is_a_different_request() {
        let (client, executions) = client(Idempotency::with_driver(store()));
        client.send(post("/payments", "order-1", "amount=10")).await;
        client.send(post("/payments", "order-2", "amount=10")).await;
        assert_eq!(executions.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_same_key_with_a_different_body_is_refused() {
        let (client, executions) = client(Idempotency::with_driver(store()));
        client.send(post("/payments", "order-1", "amount=10")).await;

        let response = client.send(post("/payments", "order-1", "amount=99")).await;
        let response = response.assert_status(422);
        assert!(response.body().contains("different request"));
        assert_eq!(executions.load(Ordering::SeqCst), 1, "and nothing ran");
    }

    #[tokio::test]
    async fn keys_are_scoped_to_the_caller() {
        let (client, executions) = client(Idempotency::with_driver(store()));
        client.send(post("/payments", "order-1", "amount=10")).await;

        let other_tenant = post("/payments", "order-1", "amount=10").with_header("authorization", "Bearer other");
        client.send(other_tenant).await.assert_status(201);
        assert_eq!(executions.load(Ordering::SeqCst), 2, "same key, different caller, runs again");
    }

    #[tokio::test]
    async fn a_custom_scope_is_honoured() {
        let idempotency = Idempotency::with_driver(store()).scope_by(|req| req.header("x-tenant").unwrap_or("none").to_string());
        let (client, executions) = client(idempotency);

        client.send(post("/payments", "k", "a").with_header("x-tenant", "acme")).await;
        client.send(post("/payments", "k", "a").with_header("x-tenant", "acme")).await;
        client.send(post("/payments", "k", "a").with_header("x-tenant", "globex")).await;
        assert_eq!(executions.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn without_a_key_every_request_runs() {
        let (client, executions) = client(Idempotency::with_driver(store()));
        let plain = || Request::new(Method::Post, "/payments").with_body(b"x".to_vec());
        client.send(plain()).await.assert_status(201);
        client.send(plain()).await.assert_status(201);
        assert_eq!(executions.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_key_can_be_required() {
        let (client, executions) = client(Idempotency::with_driver(store()).required());
        let response = client.send(Request::new(Method::Post, "/payments")).await;
        let response = response.assert_status(400);
        assert!(response.body().contains("idempotency-key"));
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_malformed_key_is_a_400() {
        let (client, _) = client(Idempotency::with_driver(store()));
        client.send(post("/payments", "has space", "x")).await.assert_status(400);
        client.send(post("/payments", &"k".repeat(256), "x")).await.assert_status(400);
    }

    #[tokio::test]
    async fn reads_are_never_touched() {
        let (client, _) = client(Idempotency::with_driver(store()).required());
        let response = client.send(Request::new(Method::Get, "/payments")).await;
        let response = response.assert_ok();
        assert_eq!(response.body(), "list");
    }

    #[tokio::test]
    async fn a_server_error_is_not_remembered_so_the_retry_runs() {
        let (client, _) = client(Idempotency::with_driver(store()));
        client.send(post("/failing", "k", "x")).await.assert_status(500);
        let retry = client.send(post("/failing", "k", "x")).await;
        let retry = retry.assert_status(500);
        assert_eq!(retry.header("idempotent-replayed"), None, "ran again rather than replayed");
    }

    #[tokio::test]
    async fn a_request_still_in_flight_is_a_409() {
        let store = store();
        // Take the lock the way a first request would, but never store an answer.
        store.increment_within("idempotency:ip:10.0.0.1:k:lock", 1, Duration::from_secs(60)).await.unwrap();
        let (client, executions) = client(Idempotency::with_driver(store));

        let response = client.send(post("/payments", "k", "x")).await;
        let response = response.assert_status(409);
        assert_eq!(response.header("retry-after"), Some("1"));
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn binary_bodies_survive_the_round_trip() {
        let (client, _) = client(Idempotency::with_driver(store()));
        client.send(post("/binary", "k", "x")).await.assert_ok();
        let replay = client.send(post("/binary", "k", "x")).await;
        assert_eq!(replay.header("idempotent-replayed"), Some("true"));
        assert_eq!(replay.body_bytes(), &[0u8, 255, 1, 254]);
    }

    #[tokio::test]
    async fn the_answer_expires_with_the_ttl() {
        let idempotency = Idempotency::with_driver(store()).remember_for(Duration::from_millis(30));
        let (client, executions) = client(idempotency);
        client.send(post("/payments", "k", "x")).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        client.send(post("/payments", "k", "x")).await;
        assert_eq!(executions.load(Ordering::SeqCst), 2, "forgotten, so it ran again");
    }

    #[test]
    fn freezing_and_thawing_keeps_status_headers_and_body() {
        let original = Response::new(Status::CREATED)
            .with_header("location", "/x/1")
            .with_header("content-type", "application/json")
            .with_body(b"{\"a\":1}".to_vec());
        let thawed = thaw(&freeze(&original, "fp")).expect("readable");
        assert_eq!(thawed.status, Status::CREATED);
        assert_eq!(thawed.headers.get("location"), Some("/x/1"));
        assert_eq!(thawed.body, original.body);
    }
}
