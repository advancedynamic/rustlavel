//! The `api` middleware: no valid bearer token, no handler.
//!
//! Shaped like [`Authenticate`](crate::guard::Authenticate) — the same
//! short-circuit, the same extension trait for reading the result from a
//! handler — but it answers an API rather than a browser, so it never redirects
//! anywhere. There is no login page a mobile app can be sent to.
//!
//! ```ignore
//! router.group("/api", |r| {
//!     r.middleware(RequireApiToken::shared(Arc::clone(&store)));
//!     r.get("/me", |req: Request| async move {
//!         format!("user {}", req.identity().unwrap().id())
//!     });
//!
//!     r.group("/posts", |r| {
//!         r.middleware(RequireScope::new("posts:write"));
//!         r.post("", publish);
//!     });
//! });
//! ```

use super::store::{SharedTokenStore, TokenStore};
use super::{Token, TokenError, authenticate};
use rustlavel_core::{Error, Json};
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::middleware::{Middleware, Next};
use rustlavel_http::response::IntoResponse;
use rustlavel_http::{Request, Response, Status};
use std::sync::Arc;

/// The header the credential arrives in.
pub const AUTHORIZATION_HEADER: &str = "authorization";

/// The scheme, matched case-insensitively as RFC 7235 requires.
pub const BEARER_SCHEME: &str = "Bearer";

/// The credential out of `Authorization: Bearer <token>`, if there is one.
///
/// Anything else — no header, a different scheme, a scheme with nothing after
/// it — is `None`. In particular `Basic` credentials are not quietly treated as
/// a token: a client sending the wrong scheme has a bug, and answering 401
/// tells it so.
pub fn bearer(request: &Request) -> Option<&str> {
    let (scheme, credentials) = request.header(AUTHORIZATION_HEADER)?.split_once(' ')?;

    scheme.eq_ignore_ascii_case(BEARER_SCHEME).then(|| credentials.trim())
}

/// `req.api_token()` — reading the token this request authenticated with.
///
/// The companion to [`AuthExt`](crate::guard::AuthExt), which is still what
/// gives you `req.identity()`: [`RequireApiToken`] attaches the identity too,
/// so a handler behind either middleware reads the current user the same way.
pub trait TokenExt {
    /// The token attached by [`RequireApiToken`], or `None` for a session
    /// request that never presented one.
    fn api_token(&self) -> Option<&Token>;

    /// Whether this request's token grants `scope`.
    ///
    /// `false` when there is no token at all, which is the safe reading: a
    /// handler asking "may they?" should not be told yes because nobody asked.
    fn token_can(&self, scope: &str) -> bool;
}

impl TokenExt for Request {
    fn api_token(&self) -> Option<&Token> {
        self.extension::<Token>()
    }

    fn token_can(&self, scope: &str) -> bool {
        self.api_token().is_some_and(|token| token.can(scope))
    }
}

/// Refused, with the scope that would have been needed.
///
/// 403 and not 401: the credential was understood and accepted. Presenting it
/// again, or presenting a better one, changes nothing — this token will never
/// be allowed to do this, and a client that retries on 401 must not retry here.
fn forbidden(scope: &str) -> Response {
    Response::new(Status::FORBIDDEN).with_json(Json::object([
        ("message", Json::from("This API token is missing a required scope.")),
        ("scope", Json::from(scope)),
    ]))
}

/// Named the way [`crate::guard`] names its equivalent: a configuration
/// mistake is a 500 with instructions, never a silent pass.
fn missing_token_middleware() -> Error {
    Error::msg(
        "the `scope` middleware needs an authenticated API token. Register the `api` middleware \
         before it: `router.middleware(RequireApiToken::shared(store))`.",
    )
}

/// The `api` middleware: authenticate a bearer token or answer 401.
///
/// On success the [`Identity`](crate::guard::Identity) and the [`Token`] are
/// both attached to the request, so a handler reads them with `req.identity()`
/// and `req.api_token()`.
pub struct RequireApiToken {
    store: SharedTokenStore,
    scope: Option<String>,
}

impl RequireApiToken {
    pub fn new(store: impl TokenStore) -> Self {
        RequireApiToken::shared(Arc::new(store))
    }

    /// Share one store between the middleware and whatever issues tokens.
    pub fn shared(store: SharedTokenStore) -> Self {
        RequireApiToken { store, scope: None }
    }

    /// Also require `scope`, answering 403 without it.
    ///
    /// The one-middleware form. Use [`RequireScope`] instead when a whole group
    /// is already behind this middleware and one route inside it needs more.
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }
}

/// The store is not printable and the scope is not a secret.
impl std::fmt::Debug for RequireApiToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequireApiToken").field("scope", &self.scope).finish_non_exhaustive()
    }
}

impl Middleware for RequireApiToken {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        let store = Arc::clone(&self.store);
        let required = self.scope.clone();

        Box::pin(async move {
            // Copied out because the header borrows the request, and the
            // request has to be mutated further down.
            let Some(presented) = bearer(&request).map(str::to_string) else {
                return TokenError::Missing.into_response();
            };

            let token = match authenticate(&*store, &presented).await {
                Ok(token) => token,
                Err(error) => return error.into_response(),
            };

            if let Some(scope) = &required
                && !token.can(scope)
            {
                return forbidden(scope);
            }

            // Both, so that a handler written against the session guard —
            // `req.identity()` — works unchanged behind an API token.
            request.extend(token.identity().clone());
            request.extend(token);
            next.run(request).await
        })
    }
}

/// The `scope` middleware: refuse a token that lacks one named permission.
///
/// Goes *after* [`RequireApiToken`], which is what put the token on the
/// request. On its own it has nothing to check and says so with a 500 rather
/// than letting the request through.
#[derive(Debug, Clone)]
pub struct RequireScope {
    scope: String,
}

impl RequireScope {
    pub fn new(scope: impl Into<String>) -> Self {
        RequireScope { scope: scope.into() }
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }
}

impl Middleware for RequireScope {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        let scope = self.scope.clone();

        Box::pin(async move {
            let Some(token) = request.api_token() else {
                return missing_token_middleware().into_response();
            };

            if !token.can(&scope) {
                return forbidden(&scope);
            }

            next.run(request).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::{AuthExt, Identity};
    use crate::tokens::store::MemoryTokenStore;
    use crate::tokens::{NewToken, PlainTextToken, Scopes};
    use rustlavel_http::{Method, Router, TestClient};

    fn store() -> SharedTokenStore {
        Arc::new(MemoryTokenStore::new())
    }

    async fn issue(store: &SharedTokenStore, owner: &str, scopes: Scopes) -> PlainTextToken {
        NewToken::new(Identity::new(owner), "iPhone")
            .scopes(scopes)
            .issue(&**store)
            .await
            .expect("the memory store never fails")
    }

    fn router(store: &SharedTokenStore) -> Router {
        let mut router = Router::new();
        let outer = Arc::clone(store);
        let inline = Arc::clone(store);

        router.group("/api", move |r| {
            r.middleware(RequireApiToken::shared(outer));

            r.get("/me", |request: Request| async move {
                let identity = request.identity().expect("the middleware attaches an identity");
                let token = request.api_token().expect("and the token");
                format!(
                    "user {} via {} [{}] write={}",
                    identity.id(),
                    token.name(),
                    token.scopes(),
                    request.token_can("posts:write")
                )
            });

            r.group("/posts", |r| {
                r.middleware(RequireScope::new("posts:write"));
                r.get("", |_request: Request| async { "the posts" });
            });
        });

        // The same check written as one middleware rather than two.
        router.group("/inline", move |r| {
            r.middleware(RequireApiToken::shared(inline).scope("posts:write"));
            r.get("", |_request: Request| async { "the posts" });
        });

        // A route with no token middleware at all, to prove the scope
        // middleware refuses to guess.
        router.group("/loose", |r| {
            r.middleware(RequireScope::new("posts:write"));
            r.get("", |_request: Request| async { "unreachable" });
        });

        router
    }

    fn bearing(path: &str, credential: &str) -> Request {
        Request::new(Method::Get, path).with_header("authorization", format!("Bearer {credential}"))
    }

    #[tokio::test]
    async fn a_valid_token_reaches_the_handler_with_its_identity_and_token() {
        let store = store();
        let issued = issue(&store, "41", Scopes::new().with("posts:read")).await;

        TestClient::new(router(&store))
            .send(bearing("/api/me", issued.plain_text()))
            .await
            .assert_ok()
            .assert_see("user 41 via iPhone [posts:read] write=false");
    }

    #[tokio::test]
    async fn the_middleware_answers_401_without_a_usable_authorization_header() {
        let store = store();
        let issued = issue(&store, "41", Scopes::any()).await;
        let client = TestClient::new(router(&store));

        // No header at all.
        client
            .get("/api/me")
            .await
            .assert_status(401)
            .assert_json("message", "Unauthenticated.")
            .assert_header("www-authenticate", "Bearer");

        // The right credential under the wrong scheme, and a scheme with
        // nothing after it.
        for value in [
            format!("Basic {}", issued.plain_text()),
            "Bearer".to_string(),
            String::new(),
            " ".to_string(),
        ] {
            client
                .send(Request::new(Method::Get, "/api/me").with_header("authorization", value))
                .await
                .assert_status(401);
        }
    }

    #[tokio::test]
    async fn the_middleware_answers_401_to_garbage_rather_than_500() {
        let store = store();
        let issued = issue(&store, "41", Scopes::any()).await;
        let client = TestClient::new(router(&store));

        let wrong_secret = format!("{}|not-the-secret", issued.token().id());
        let huge = "a".repeat(20_000);

        for credential in
            ["no-pipe", "|", "a|", "|b", "\0", "%%%", wrong_secret.as_str(), huge.as_str()]
        {
            let response = client.send(bearing("/api/me", credential)).await;

            assert_eq!(
                response.status(),
                401,
                "{credential:?} produced {} instead of a 401",
                response.status()
            );
            response.assert_json("message", "Invalid API token.");
        }
    }

    #[tokio::test]
    async fn the_scheme_is_matched_case_insensitively_and_extra_spacing_is_ignored() {
        let store = store();
        let issued = issue(&store, "41", Scopes::any()).await;
        let client = TestClient::new(router(&store));

        for value in [
            format!("bearer {}", issued.plain_text()),
            format!("BEARER {}", issued.plain_text()),
            format!("Bearer  {}", issued.plain_text()),
        ] {
            client
                .send(Request::new(Method::Get, "/api/me").with_header("authorization", value))
                .await
                .assert_ok();
        }
    }

    #[tokio::test]
    async fn an_expired_token_is_refused_by_the_middleware() {
        let store = store();
        let issued = NewToken::new(Identity::new("41"), "expired")
            .any()
            .expires_at(1)
            .issue(&*store)
            .await
            .unwrap();

        TestClient::new(router(&store))
            .send(bearing("/api/me", issued.plain_text()))
            .await
            .assert_status(401)
            .assert_json("message", "This API token has expired.")
            .assert_header("www-authenticate", r#"Bearer error="invalid_token""#);
    }

    #[tokio::test]
    async fn a_revoked_token_stops_working_on_the_next_request() {
        let store = store();
        let issued = issue(&store, "41", Scopes::any()).await;
        let client = TestClient::new(router(&store));

        client.send(bearing("/api/me", issued.plain_text())).await.assert_ok();

        store.delete(issued.token().id()).await.unwrap();

        client.send(bearing("/api/me", issued.plain_text())).await.assert_status(401);
    }

    #[tokio::test]
    async fn the_scope_middleware_answers_403_when_the_scope_is_missing() {
        let store = store();
        let reader = issue(&store, "41", Scopes::new().with("posts:read")).await;
        let client = TestClient::new(router(&store));

        for path in ["/api/posts", "/inline"] {
            client
                .send(bearing(path, reader.plain_text()))
                .await
                .assert_status(403)
                .assert_json("message", "This API token is missing a required scope.")
                .assert_json("scope", "posts:write");
        }
    }

    #[tokio::test]
    async fn the_scope_middleware_lets_a_scoped_token_through() {
        let store = store();
        let writer = issue(&store, "41", Scopes::new().with("posts:write")).await;
        let wildcard = issue(&store, "41", Scopes::any()).await;
        let client = TestClient::new(router(&store));

        for token in [&writer, &wildcard] {
            for path in ["/api/posts", "/inline"] {
                let response = client.send(bearing(path, token.plain_text())).await;
                response.assert_ok().assert_see("the posts");
            }
        }
    }

    #[tokio::test]
    async fn the_scope_middleware_still_needs_a_token_first() {
        let store = store();
        let writer = issue(&store, "41", Scopes::new().with("posts:write")).await;

        // Even with a perfectly good token: without the `api` middleware there
        // is nothing on the request to check, and a guard that cannot check
        // must not let the request through.
        TestClient::new(router(&store))
            .send(bearing("/loose", writer.plain_text()))
            .await
            .assert_status(500);
    }

    #[test]
    fn the_missing_middleware_error_says_how_to_fix_it() {
        let message = missing_token_middleware().to_string();

        assert!(message.contains("needs an authenticated API token"), "message was {message}");
        assert!(message.contains("RequireApiToken::shared"), "message was {message}");
    }

    #[test]
    fn the_bearer_scheme_is_read_from_the_header_and_nothing_else() {
        let get = |value: &str| {
            let request = Request::new(Method::Get, "/").with_header("authorization", value);
            bearer(&request).map(str::to_string)
        };

        assert_eq!(get("Bearer 7|secret").as_deref(), Some("7|secret"));
        assert_eq!(get("bearer 7|secret").as_deref(), Some("7|secret"));

        assert_eq!(get("Basic 7|secret"), None);
        assert_eq!(get("Bearer"), None);
        assert_eq!(get(""), None);
        assert_eq!(bearer(&Request::new(Method::Get, "/")), None);
    }

    #[test]
    fn a_request_without_a_token_can_do_nothing() {
        let request = Request::new(Method::Get, "/");

        assert!(request.api_token().is_none());
        assert!(!request.token_can("posts:read"));
        assert!(!request.token_can("*"));
    }
}
