//! The `csrf` middleware.
//!
//! A browser attaches cookies to *any* request it makes to your domain,
//! including one triggered by a form on someone else's site. The session cookie
//! alone therefore proves the visitor is logged in, not that they meant to
//! submit this. The token closes that gap: it lives in the session and has to
//! be echoed back in the request body or a header, which a cross-origin page
//! cannot read and so cannot echo.
//!
//! ```ignore
//! router.middleware(SessionManager::from_config(&config, store)?);
//! router.middleware(Csrf::new().except("/webhooks/"));
//! ```
//!
//! Order matters: the session has to be loaded before the token can be checked.

use crate::constant_time_eq;
use crate::middleware::{SessionExt, SessionHandle};
use crate::session::Session;
use rustlavel_core::{Error, Json};
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::middleware::{Middleware, Next};
use rustlavel_http::response::IntoResponse;
use rustlavel_http::{Method, Request, Response, Status};

/// The form field carrying the token, as Laravel names it.
pub const TOKEN_FIELD: &str = Session::TOKEN_KEY;

/// The header a fetch() or XHR client sends the token in instead.
pub const TOKEN_HEADER: &str = "x-csrf-token";

/// `419 Page Expired` — what a missing or stale token gets.
///
/// Laravel's choice, and a good one: the overwhelmingly common cause is a form
/// left open until its session lapsed, not an attack, and "page expired" tells
/// that visitor what to do. A 403 would suggest they lack permission.
pub const REJECTED: Status = Status(419);

/// Rejects unsafe requests that do not carry the session's CSRF token.
#[derive(Debug, Clone, Default)]
pub struct Csrf {
    except: Vec<String>,
}

impl Csrf {
    pub fn new() -> Self {
        Csrf::default()
    }

    /// Exempt every path starting with `prefix`.
    ///
    /// For endpoints called by machines rather than browsers — an incoming
    /// payment webhook has no session to hold a token, and authenticates
    /// itself some other way.
    pub fn except(mut self, prefix: impl Into<String>) -> Self {
        self.except.push(prefix.into());
        self
    }

    fn is_exempt(&self, path: &str) -> bool {
        self.except.iter().any(|prefix| path.starts_with(prefix.as_str()))
    }
}

/// Whether a method is safe, and so exempt.
///
/// These are the methods that are supposed to have no side effects. A GET that
/// deletes something is a bug that CSRF protection cannot fix.
fn is_safe(method: Method) -> bool {
    matches!(method, Method::Get | Method::Head | Method::Options)
}

/// The token for this request, for a template to render into a form.
pub fn token(request: &Request) -> Option<String> {
    request.try_session().map(SessionHandle::token)
}

/// A ready-made hidden input: `{{ csrf::field(req) }}`.
///
/// The token is hexadecimal, so it needs no HTML escaping — but it is quoted
/// anyway rather than relying on that.
pub fn field(request: &Request) -> String {
    match token(request) {
        Some(token) => format!(r#"<input type="hidden" name="{TOKEN_FIELD}" value="{token}">"#),
        None => String::new(),
    }
}

impl Middleware for Csrf {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        let middleware = self.clone();

        Box::pin(async move {
            let Some(session) = request.try_session().cloned() else {
                return Error::msg(
                    "the `csrf` middleware needs a session. Register the `session` middleware \
                     before it: `router.middleware(SessionManager::from_config(&config, store)?)`.",
                )
                .into_response();
            };

            // Generating the token here — before the method check — is what
            // makes it available to the GET that renders the form.
            let expected = session.token();

            if is_safe(request.method()) || middleware.is_exempt(request.path()) {
                return next.run(request).await;
            }

            let from_body = request.input(TOKEN_FIELD);
            let presented = match from_body {
                Some(token) => Some(token),
                None => request.header(TOKEN_HEADER).map(str::to_string),
            };

            // Constant time so a token cannot be recovered a character at a
            // time by measuring how long the comparison takes.
            let accepted = presented
                .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));

            if accepted { next.run(request).await } else { rejection(&request) }
        })
    }
}

fn rejection(request: &Request) -> Response {
    if request.wants_json() {
        return Response::new(REJECTED).with_json(Json::object([(
            "message",
            Json::from("CSRF token mismatch."),
        )]));
    }

    Response::new(REJECTED).with_html(format!(
        "<h1>{} {}</h1><p>Your session has expired. Please refresh the page and try again.</p>",
        REJECTED.code(),
        REJECTED.reason()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::AppKey;
    use crate::middleware::SessionManager;
    use crate::store::MemoryStore;
    use rustlavel_http::{Router, TestClient, TestResponse};

    fn router() -> Router {
        let mut router = Router::new();
        router.middleware(SessionManager::new(&AppKey::from_bytes([5u8; 32]), MemoryStore::new()));
        router.middleware(Csrf::new().except("/webhooks/"));

        router.get("/form", |request: Request| async move { field(&request) });
        router.post("/posts", |_request: Request| async { "created" });
        router.put("/posts/1", |_request: Request| async { "updated" });
        router.patch("/posts/1", |_request: Request| async { "patched" });
        router.delete("/posts/1", |_request: Request| async { "deleted" });
        router.post("/webhooks/stripe", |_request: Request| async { "accepted" });
        router
    }

    /// The name=value part of the response's session cookie.
    fn cookie_of(response: &TestResponse) -> String {
        response
            .header("set-cookie")
            .expect("the session middleware should have set a cookie")
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    /// Visit the form page, returning its token and session cookie.
    async fn open_form(client: &TestClient) -> (String, String) {
        let response = client.get("/form").await.assert_ok();
        let body = response.body();
        let token = body
            .split("value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the form should contain a token")
            .to_string();

        (token, cookie_of(&response))
    }

    #[tokio::test]
    async fn a_get_request_passes_through_and_is_handed_a_token() {
        let client = TestClient::new(router());
        let (token, _) = open_form(&client).await;

        assert_eq!(token.len(), 64, "the token should be 32 bytes of entropy in hex");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn a_post_without_a_token_is_rejected_with_419() {
        TestClient::new(router())
            .post("/posts", &[("title", "hello")])
            .await
            .assert_status(419)
            .assert_see("expired");
    }

    #[tokio::test]
    async fn a_post_with_the_session_token_is_accepted() {
        let client = TestClient::new(router());
        let (token, cookie) = open_form(&client).await;

        let response = client
            .send(
                Request::new(Method::Post, "/posts")
                    .with_header("cookie", cookie)
                    .with_form(&[("title", "hello"), (TOKEN_FIELD, &token)]),
            )
            .await;

        response.assert_ok().assert_see("created");
    }

    #[tokio::test]
    async fn the_token_may_arrive_in_a_header_instead() {
        let client = TestClient::new(router());
        let (token, cookie) = open_form(&client).await;

        client
            .send(
                Request::new(Method::Post, "/posts")
                    .with_header("cookie", cookie)
                    .with_header(TOKEN_HEADER, token)
                    .with_json(Json::object([("title", "hello".into())])),
            )
            .await
            .assert_ok();
    }

    #[tokio::test]
    async fn a_token_from_another_session_is_rejected() {
        // Two clients, because two sessions means two browsers: the test client
        // keeps a cookie jar, so one client would simply reuse its own session.
        let client = TestClient::new(router());
        let other_browser = TestClient::new(router());

        let (token, _) = open_form(&client).await;
        let (_, other_cookie) = open_form(&other_browser).await;

        client
            .send(
                Request::new(Method::Post, "/posts")
                    .with_header("cookie", other_cookie)
                    .with_form(&[(TOKEN_FIELD, &token)]),
            )
            .await
            .assert_status(419);
    }

    #[tokio::test]
    async fn a_wrong_or_truncated_token_is_rejected() {
        let client = TestClient::new(router());
        let (token, cookie) = open_form(&client).await;

        let candidates = [
            String::new(),
            token[..token.len() - 1].to_string(),
            format!("{token}0"),
            "a".repeat(64),
        ];

        for candidate in &candidates {
            client
                .send(
                    Request::new(Method::Post, "/posts")
                        .with_header("cookie", cookie.clone())
                        .with_form(&[(TOKEN_FIELD, candidate.as_str())]),
                )
                .await
                .assert_status(419);
        }
    }

    #[tokio::test]
    async fn every_state_changing_method_is_guarded() {
        let client = TestClient::new(router());

        client.send(Request::new(Method::Put, "/posts/1")).await.assert_status(419);
        client.send(Request::new(Method::Patch, "/posts/1")).await.assert_status(419);
        client.delete("/posts/1").await.assert_status(419);
    }

    #[tokio::test]
    async fn exempt_paths_are_left_alone() {
        TestClient::new(router())
            .post("/webhooks/stripe", &[("event", "paid")])
            .await
            .assert_ok()
            .assert_see("accepted");
    }

    #[tokio::test]
    async fn a_json_client_is_rejected_with_json() {
        TestClient::new(router())
            .post_json("/posts", Json::object([("title", "hello".into())]))
            .await
            .assert_status(419)
            .assert_json("message", "CSRF token mismatch.");
    }

    #[tokio::test]
    async fn the_token_is_stable_for_the_life_of_the_session() {
        let client = TestClient::new(router());
        let (first, cookie) = open_form(&client).await;

        let response = client
            .send(Request::new(Method::Get, "/form").with_header("cookie", cookie))
            .await;
        let second = response.body();

        assert!(second.contains(&first), "the token must not change between page loads");
    }

    #[tokio::test]
    async fn without_the_session_middleware_the_error_says_so() {
        let mut router = Router::new();
        router.middleware(Csrf::new());
        router.get("/", |_request: Request| async { "ok" });

        // The failure surfaces as a server error; the explanation reaches the
        // developer through the error page or the log, depending on `app.debug`.
        TestClient::new(router).get("/").await.assert_status(500);
    }

    #[test]
    fn safe_methods_are_the_ones_with_no_side_effects() {
        assert!(is_safe(Method::Get));
        assert!(is_safe(Method::Head));
        assert!(is_safe(Method::Options));
        assert!(!is_safe(Method::Post));
        assert!(!is_safe(Method::Put));
        assert!(!is_safe(Method::Patch));
        assert!(!is_safe(Method::Delete));
    }
}
