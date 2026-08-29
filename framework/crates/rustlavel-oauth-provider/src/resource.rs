//! The other side of the exchange: guarding your own routes with a token this
//! server issued.
//!
//! ```ignore
//! router.group("/api", |r| {
//!     r.middleware(RequireToken::new(&server).scope("orders.read"));
//!     r.get("/orders", |req: Request| async move {
//!         let claims = req.oauth().expect("the middleware ran");
//!         Json::from(orders_for(claims.user_id.as_deref()))
//!     });
//! });
//! ```
//!
//! Two failures, and RFC 6750 §3.1 is specific about which is which: a token
//! that is missing, unreadable, expired or revoked is `invalid_token` and 401 —
//! *authenticate and try again*. A perfectly good token that does not carry the
//! required scope is `insufficient_scope` and 403 — *trying again will not
//! help; ask for more when you next authorise*. A client that retries a 403 in
//! a loop is usually a server that returned the wrong one.

use crate::server::AuthorizationServer;
use rustlavel_auth::guard::Identity;
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::middleware::{Middleware, Next};
use rustlavel_http::response::IntoResponse;
use rustlavel_http::{Request, Response};
use rustlavel_oauth::{OAuthError, OAuthErrorCode, Scopes};

/// What a validated token says, attached to the request for the handler.
///
/// It holds no token — only what the token was found to mean. A handler that
/// needs to call another service on the user's behalf should ask for its own
/// token rather than forwarding this one.
#[derive(Debug, Clone)]
pub struct TokenClaims {
    /// The stored record's id, for revoking this token later.
    pub token_id: String,
    pub client_id: String,
    /// `None` for a `client_credentials` token: there is no user behind it.
    pub user_id: Option<String>,
    pub scopes: Scopes,
    pub expires_at: u64,
}

impl TokenClaims {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}

/// Reading what the middleware attached.
pub trait BearerExt {
    /// The claims of the token this request presented, if one was validated.
    fn oauth(&self) -> Option<&TokenClaims>;
}

impl BearerExt for Request {
    fn oauth(&self) -> Option<&TokenClaims> {
        self.extension::<TokenClaims>()
    }
}

/// Middleware that requires a valid bearer token, and optionally some scopes.
#[derive(Clone)]
pub struct RequireToken {
    server: AuthorizationServer,
    required: Scopes,
}

impl RequireToken {
    pub fn new(server: &AuthorizationServer) -> RequireToken {
        RequireToken { server: server.clone(), required: Scopes::new() }
    }

    /// Require one more scope. Calls accumulate, and all of them must be held.
    pub fn scope(mut self, scope: &str) -> RequireToken {
        self.required.add(scope);
        self
    }

    pub fn scopes(mut self, scopes: Scopes) -> RequireToken {
        self.required = scopes;
        self
    }

    async fn check(&self, request: &mut Request) -> Result<(), OAuthError> {
        let presented = bearer(request).ok_or_else(|| {
            OAuthError::because(
                OAuthErrorCode::InvalidToken,
                "this endpoint needs an `Authorization: Bearer <token>` header",
            )
        })?;

        let token = self.server.validate_bearer(&presented).await?;

        // Scope is checked after the token, so an expired token never reports
        // `insufficient_scope` — which would tell the caller their credential
        // was fine when it was not.
        if !token.scopes.covers(&self.required) {
            let missing = self.required.beyond(&token.scopes);
            return Err(OAuthError::because(
                OAuthErrorCode::InsufficientScope,
                format!("this token does not carry: {missing}"),
            ));
        }

        // The identity too, so anything already written against `req.identity()`
        // works for an API request without knowing about OAuth.
        if let Some(user_id) = &token.user_id {
            request.extend(Identity::new(user_id.clone()));
        }
        request.extend(TokenClaims {
            token_id: token.id.clone(),
            client_id: token.client_id.clone(),
            user_id: token.user_id.clone(),
            scopes: token.scopes.clone(),
            expires_at: token.expires_at,
        });

        Ok(())
    }
}

impl Middleware for RequireToken {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        let guard = self.clone();

        Box::pin(async move {
            match guard.check(&mut request).await {
                Ok(()) => next.run(request).await,
                Err(error) => error.into_response(),
            }
        })
    }
}

/// The token from an `Authorization: Bearer` header.
///
/// The header only. RFC 6750 §2.3 also defines a `access_token` query
/// parameter; OAuth 2.1 removes it, and for good reason — a URL is logged by
/// every proxy it passes, kept in browser history, and sent onward in
/// `Referer`. A token that arrives that way is a token to rotate, not one to
/// accept.
fn bearer(request: &Request) -> Option<String> {
    let header = request.header("authorization")?;
    // RFC 7235 §2.1: the scheme is case-insensitive.
    let (scheme, token) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }

    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, MemoryClientStore};
    use rustlavel_http::{Method, Router, TestClient};

    fn server() -> AuthorizationServer {
        AuthorizationServer::new(MemoryClientStore::new().with(Client::public("spa")))
    }

    fn guarded(server: &AuthorizationServer, guard: RequireToken) -> TestClient {
        let _ = server;
        let mut router = Router::new();
        router.middleware(guard);
        router.get("/api/orders", |request: Request| async move {
            let claims = request.oauth().expect("the middleware attached claims");
            format!("{}:{}", claims.client_id, claims.user_id.clone().unwrap_or_default())
        });
        TestClient::new(router)
    }

    async fn token_for(server: &AuthorizationServer, scopes: &[&str]) -> String {
        server
            .issue("spa", Some("7"), &Scopes::of(scopes.to_vec()), "family-1", false)
            .await
            .expect("issued")
            .access_token
    }

    fn with_bearer(token: &str) -> Request {
        Request::new(Method::Get, "/api/orders")
            .with_header("authorization", format!("Bearer {token}"))
    }

    #[tokio::test]
    async fn a_valid_token_reaches_the_handler_with_its_claims() {
        let server = server();
        let client = guarded(&server, RequireToken::new(&server));
        let token = token_for(&server, &["orders.read"]).await;

        client.send(with_bearer(&token)).await.assert_ok().assert_see("spa:7");
    }

    #[tokio::test]
    async fn a_missing_token_is_a_401_that_says_how_to_authenticate() {
        let server = server();
        let client = guarded(&server, RequireToken::new(&server));

        let response = client.get("/api/orders").await.assert_status(401);
        let challenge = response.header("WWW-Authenticate").expect("a challenge");
        assert!(challenge.contains(r#"error="invalid_token""#));
    }

    #[tokio::test]
    async fn an_expired_or_revoked_token_is_a_401() {
        let server = server().access_ttl(60);
        let client = guarded(&server, RequireToken::new(&server));

        let expired = token_for(&server, &["orders.read"]).await;
        let revoked = token_for(&server, &["orders.read"]).await;
        server.revoke_family("family-1", "test").await;
        server.clock().advance(61);

        client.send(with_bearer(&expired)).await.assert_status(401);
        client.send(with_bearer(&revoked)).await.assert_status(401);
        client.send(with_bearer("never-existed")).await.assert_status(401);
    }

    #[tokio::test]
    async fn a_token_without_the_required_scope_is_a_403_not_a_401() {
        // The distinction a client branches on: 401 means try again with a
        // better token, 403 means trying again will not help.
        let server = server();
        let client = guarded(&server, RequireToken::new(&server).scope("orders.write"));
        let token = token_for(&server, &["orders.read"]).await;

        let response = client.send(with_bearer(&token)).await.assert_status(403);
        let challenge = response.header("WWW-Authenticate").expect("a challenge");
        assert!(challenge.contains(r#"error="insufficient_scope""#));
        assert!(response.body().contains("orders.write"));
    }

    #[tokio::test]
    async fn an_expired_token_reports_the_token_and_not_the_scope() {
        // Reporting `insufficient_scope` here would tell the caller their
        // credential was fine when it had in fact expired.
        let server = server().access_ttl(60);
        let client = guarded(&server, RequireToken::new(&server).scope("orders.write"));
        let token = token_for(&server, &["orders.read"]).await;
        server.clock().advance(61);

        client.send(with_bearer(&token)).await.assert_status(401);
    }

    #[tokio::test]
    async fn every_required_scope_has_to_be_held() {
        let server = server();
        let guard = RequireToken::new(&server).scope("orders.read").scope("orders.write");
        let client = guarded(&server, guard);

        let partial = token_for(&server, &["orders.read"]).await;
        client.send(with_bearer(&partial)).await.assert_status(403);

        let full = token_for(&server, &["orders.read", "orders.write"]).await;
        client.send(with_bearer(&full)).await.assert_ok();
    }

    #[tokio::test]
    async fn the_scheme_is_case_insensitive_and_nothing_else_is_accepted() {
        let server = server();
        let client = guarded(&server, RequireToken::new(&server));
        let token = token_for(&server, &[]).await;

        let lowercase = Request::new(Method::Get, "/api/orders")
            .with_header("authorization", format!("bearer {token}"));
        client.send(lowercase).await.assert_ok();

        // Basic is not a bearer token, however good the credential inside it.
        let basic = Request::new(Method::Get, "/api/orders")
            .with_header("authorization", format!("Basic {token}"));
        client.send(basic).await.assert_status(401);
    }

    #[tokio::test]
    async fn a_token_in_the_query_string_is_not_accepted() {
        // RFC 6750 §2.3's query form, removed by OAuth 2.1: a URL is logged by
        // every proxy it passes and kept in browser history.
        let server = server();
        let client = guarded(&server, RequireToken::new(&server));
        let token = token_for(&server, &[]).await;

        client.get(&format!("/api/orders?access_token={token}")).await.assert_status(401);
    }

    #[tokio::test]
    async fn a_client_credentials_token_has_no_identity_attached() {
        let server = server();
        let mut router = Router::new();
        router.middleware(RequireToken::new(&server));
        router.get("/api/orders", |request: Request| async move {
            match request.extension::<Identity>() {
                Some(identity) => format!("user {identity}"),
                None => "no user".to_string(),
            }
        });
        let client = TestClient::new(router);

        let machine = server
            .issue("spa", None, &Scopes::new(), "family-2", false)
            .await
            .expect("issued")
            .access_token;

        client.send(with_bearer(&machine)).await.assert_ok().assert_see("no user");
    }
}
