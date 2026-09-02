//! One identifier per request, from the first log line to the response header.
//!
//! When a user reports "it failed around 3pm", the only thing that connects
//! their screenshot to a line in the log is an identifier that appeared in
//! both. This middleware assigns one — or keeps the one a load balancer already
//! attached — puts it on the request for handlers to read, on the response for
//! the client to quote, and on the `http.request` instrumentation event for
//! Telescope, the debug bar and OTLP traces.
//!
//! ```ignore
//! App::new()?.middleware(RequestId::default())
//!
//! // In a handler:
//! let id = req.request_id().unwrap_or("-");
//!
//! // Anywhere inside the request, without a `Request` in hand:
//! if let Some(id) = request_id::current() { … }
//! ```
//!
//! Incoming identifiers are trusted by default, because the useful case — an
//! edge proxy that stamps every request and logs it — is far more common than
//! the harmful one, and a forged id can do nothing except confuse the forger's
//! own log search. Anything that does not look like an identifier (too long,
//! not printable ASCII) is replaced rather than passed on.

use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// The header the identifier travels in, unless configured otherwise.
pub const HEADER: &str = "x-request-id";

/// The identifier assigned to the current request, attached as an extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assigned(pub String);

tokio::task_local! {
    static CURRENT: String;
}

/// The identifier of the request this task is serving, if any.
///
/// Available to code that has no `Request` to hand — a repository, a mailer,
/// a log formatter — for as long as it runs inside the middleware's scope.
pub fn current() -> Option<String> {
    CURRENT.try_with(|id| id.clone()).ok()
}

#[derive(Debug, Clone)]
pub struct RequestId {
    header: String,
    trust_incoming: bool,
}

impl Default for RequestId {
    fn default() -> Self {
        RequestId { header: HEADER.to_string(), trust_incoming: true }
    }
}

impl RequestId {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a different header — `X-Correlation-Id`, `X-Amzn-Trace-Id`.
    pub fn header(mut self, name: &str) -> Self {
        self.header = name.to_ascii_lowercase();
        self
    }

    /// Always mint a fresh identifier, ignoring whatever the client sent.
    ///
    /// For an application reached directly from the internet with no proxy in
    /// front, where nothing upstream is trusted to have chosen one.
    pub fn ignore_incoming(mut self) -> Self {
        self.trust_incoming = false;
        self
    }

    fn choose(&self, request: &Request) -> String {
        if self.trust_incoming
            && let Some(incoming) = request.header(&self.header)
            && looks_like_an_id(incoming)
        {
            return incoming.to_string();
        }
        generate()
    }
}

/// Printable ASCII, no whitespace, and short enough to be an identifier
/// rather than a payload someone is trying to smuggle into the logs.
fn looks_like_an_id(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= 128
        && candidate.bytes().all(|b| b.is_ascii_graphic())
}

/// A fresh identifier in the shape of a version-4 UUID.
///
/// The shape is borrowed because every log tool already knows how to spot and
/// index it. The randomness is not cryptographic and does not pretend to be —
/// the process-random seed the standard library hands every `RandomState`,
/// stirred with a counter through SplitMix64 — which is exactly enough for two
/// requests, on two machines, to never share an identifier by accident.
pub fn generate() -> String {
    static STATE: AtomicU64 = AtomicU64::new(0);

    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        let mut hasher = std::hash::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        hasher.write_u32(std::process::id());
        // Two racing initialisers both compute a seed; whichever loses the
        // exchange simply uses the winner's, and both continue from it.
        let seed = hasher.finish() | 1;
        state = match STATE.compare_exchange(0, seed, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => seed,
            Err(existing) => existing,
        };
    }

    // SplitMix64: each output is the mix of a distinct counter value, so two
    // calls can only collide if the counter itself wraps 2^64 times.
    let next = || {
        let counter = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        let mut z = counter.wrapping_add(state);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let (high, low) = (next(), next());

    // RFC 9562 §5.4: version nibble 4, variant bits 10.
    let high = (high & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000;
    let low = (low & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xFFFF,
        high & 0xFFFF,
        low >> 48,
        low & 0xFFFF_FFFF_FFFF
    )
}

impl Middleware for RequestId {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        let id = self.choose(&request);
        let header = self.header.clone();
        request.extend(Assigned(id.clone()));

        Box::pin(CURRENT.scope(id.clone(), async move {
            let mut response = next.run(request).await;
            // Set rather than appended: a handler that echoed the id itself
            // must not produce two copies, and nothing else may overwrite it.
            response.headers.set(&header, id);
            response
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;
    use crate::router::Router;
    use crate::testing::TestClient;
    use std::collections::HashSet;

    fn client(middleware: RequestId) -> TestClient {
        let mut router = Router::new();
        router.middleware(middleware);
        router.get("/", |req: Request| async move {
            let from_request = req.request_id().unwrap_or("none").to_string();
            let from_task = current().unwrap_or_else(|| "none".to_string());
            Response::text(format!("{from_request}|{from_task}"))
        });
        TestClient::new(router)
    }

    #[test]
    fn generated_ids_look_like_uuids_and_do_not_repeat() {
        let ids: HashSet<String> = (0..10_000).map(|_| generate()).collect();
        assert_eq!(ids.len(), 10_000, "ten thousand ids, ten thousand distinct values");

        for id in ids.iter().take(50) {
            assert_eq!(id.len(), 36, "{id}");
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12], "{id}");
            assert!(parts[2].starts_with('4'), "version nibble: {id}");
            assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'), "variant: {id}");
        }
    }

    #[tokio::test]
    async fn a_request_without_an_id_is_given_one_everywhere() {
        let response = client(RequestId::new()).get("/").await;
        let id = response.header(HEADER).expect("the response carries the id").to_string();
        assert_eq!(id.len(), 36);
        assert_eq!(response.body(), format!("{id}|{id}"), "request, task-local and header all agree");
    }

    #[tokio::test]
    async fn an_incoming_id_is_kept_by_default() {
        let request = Request::new(Method::Get, "/").with_header(HEADER, "edge-7f3a");
        let response = client(RequestId::new()).send(request).await;
        assert_eq!(response.header(HEADER), Some("edge-7f3a"));
        assert_eq!(response.body(), "edge-7f3a|edge-7f3a");
    }

    #[tokio::test]
    async fn an_incoming_id_can_be_ignored() {
        let request = Request::new(Method::Get, "/").with_header(HEADER, "edge-7f3a");
        let response = client(RequestId::new().ignore_incoming()).send(request).await;
        assert_ne!(response.header(HEADER), Some("edge-7f3a"));
        assert_eq!(response.header(HEADER).unwrap().len(), 36);
    }

    #[tokio::test]
    async fn garbage_in_the_header_is_replaced_not_forwarded() {
        for bad in ["", "has space", "tab\there", "x".repeat(129).as_str(), "ünïcödé"] {
            let request = Request::new(Method::Get, "/").with_header(HEADER, bad);
            let response = client(RequestId::new()).send(request).await;
            let id = response.header(HEADER).unwrap();
            assert_ne!(id, bad);
            assert_eq!(id.len(), 36, "replaced with a generated one");
        }
    }

    #[tokio::test]
    async fn the_header_name_is_configurable() {
        let request = Request::new(Method::Get, "/").with_header("x-correlation-id", "corr-1");
        let response = client(RequestId::new().header("X-Correlation-Id")).send(request).await;
        assert_eq!(response.header("x-correlation-id"), Some("corr-1"));
        assert_eq!(response.header(HEADER), None);
    }

    #[tokio::test]
    async fn outside_a_request_there_is_no_current_id() {
        assert_eq!(current(), None);
    }

    #[tokio::test]
    async fn the_event_for_the_request_carries_the_id() {
        // The subscriber list is process-wide and this test only ever adds to
        // it, filtering for its own id, so it cannot disturb a neighbour.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        rustlavel_core::events::subscribe(move |event: &rustlavel_core::Event| {
            if event.kind == "http.request" {
                sink.lock().unwrap().push(event.field("request_id").and_then(|v| v.as_str().map(str::to_string)));
            }
        });

        let request = Request::new(Method::Get, "/").with_header(HEADER, "traced-1");
        client(RequestId::new()).send(request).await;

        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|id| id.as_deref() == Some("traced-1")), "{seen:?}");
    }
}
