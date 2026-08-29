//! `POST /oauth/introspect` — RFC 7662.
//!
//! What a resource server calls to ask "is this token any good, and what does
//! it allow?". Two properties matter more than the fields it returns.
//!
//! # It requires authentication, and not the token-shaped kind
//!
//! §2.1 says the endpoint MUST require authorisation. Without it, anyone on the
//! network has a free oracle: feed it guessed or captured tokens and it reports
//! which ones are live, who they belong to and what they can do. This server
//! goes one step further and requires a *confidential* client — a public
//! client's only credential is its id, which appears in every redirect URL its
//! users ever see, so accepting one would be authentication in name only.
//!
//! # Every failure is the same flat answer
//!
//! §2.2: an invalid, expired or revoked token produces `{"active": false}` and
//! nothing else. No reason, no `exp`, no client id, no hint that the token once
//! existed. The distinction between "never issued" and "revoked ten minutes
//! ago" is worth nothing to an honest caller and quite a lot to an attacker
//! working out which of a captured batch of tokens are worth attacking.

use crate::endpoints::client_auth;
use crate::endpoints::params::Params;
use crate::server::AuthorizationServer;
use crate::store::digest;
use crate::token::{AccessToken, RefreshToken};
use rustlavel_core::Json;
use rustlavel_http::{IntoResponse, Request, Response};
use rustlavel_oauth::OAuthError;

/// `POST /oauth/introspect`.
pub async fn introspect(server: AuthorizationServer, mut request: Request) -> Response {
    let params = Params::from_body(&mut request);

    match run(&server, &request, &params).await {
        Ok(body) => Response::json(body).with_header("cache-control", "no-store"),
        Err(error) => error.into_response().with_header("cache-control", "no-store"),
    }
}

/// The flat negative answer. Deliberately the only shape a failure takes.
fn inactive() -> Json {
    Json::object([("active", Json::Bool(false))])
}

async fn run(
    server: &AuthorizationServer,
    request: &Request,
    params: &Params,
) -> Result<Json, OAuthError> {
    let credentials = client_auth::presented(request, params)?;
    client_auth::authenticate_confidential(server, &credentials).await?;

    // §2.1 makes `token` required. This is a malformed request rather than an
    // introspection of nothing, and it is the one thing a caller can fix.
    let presented = params.required("token")?;
    let hash = digest(presented);
    let now = server.now();

    if let Ok(Some(token)) = server.tokens().find_access(&hash).await {
        return Ok(if token.is_live(now) { describe_access(&token) } else { inactive() });
    }
    if let Ok(Some(token)) = server.tokens().find_refresh(&hash).await {
        return Ok(if token.is_live(now) { describe_refresh(&token) } else { inactive() });
    }

    Ok(inactive())
}

fn describe_access(token: &AccessToken) -> Json {
    let mut fields = vec![
        ("active".to_string(), Json::Bool(true)),
        ("token_type".to_string(), Json::String("Bearer".into())),
        ("scope".to_string(), Json::String(token.scopes.to_string())),
        ("client_id".to_string(), Json::String(token.client_id.clone())),
        ("exp".to_string(), Json::Number(token.expires_at as f64)),
        ("iat".to_string(), Json::Number(token.issued_at as f64)),
    ];
    // §2.2's `sub`. Absent for `client_credentials`, which is how a resource
    // server tells a machine-to-machine token from a user's.
    if let Some(user_id) = &token.user_id {
        fields.push(("sub".to_string(), Json::String(user_id.clone())));
    }
    Json::object(fields)
}

fn describe_refresh(token: &RefreshToken) -> Json {
    let mut fields = vec![
        ("active".to_string(), Json::Bool(true)),
        ("token_type".to_string(), Json::String("refresh_token".into())),
        ("scope".to_string(), Json::String(token.scopes.to_string())),
        ("client_id".to_string(), Json::String(token.client_id.clone())),
        ("exp".to_string(), Json::Number(token.expires_at as f64)),
        ("iat".to_string(), Json::Number(token.issued_at as f64)),
    ];
    if let Some(user_id) = &token.user_id {
        fields.push(("sub".to_string(), Json::String(user_id.clone())));
    }
    Json::object(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, MemoryClientStore};
    use rustlavel_http::Method;
    use rustlavel_oauth::{OAuthErrorCode, Scopes, TokenResponse};

    fn server() -> AuthorizationServer {
        AuthorizationServer::new(
            MemoryClientStore::new()
                .with(Client::confidential("web", "s3cret").scopes(Scopes::of(["read"])))
                .with(Client::public("spa")),
        )
    }

    async fn issued(server: &AuthorizationServer) -> TokenResponse {
        server
            .issue("web", Some("7"), &Scopes::of(["read", "write"]), "family-1", true)
            .await
            .expect("issued")
    }

    async fn post(server: &AuthorizationServer, fields: &[(&str, &str)]) -> Result<Json, OAuthError> {
        let mut request = Request::new(Method::Post, "/oauth/introspect").with_form(fields);
        let params = Params::from_body(&mut request);
        run(server, &request, &params).await
    }

    async fn ask(server: &AuthorizationServer, token: &str) -> Json {
        post(server, &[("token", token), ("client_id", "web"), ("client_secret", "s3cret")])
            .await
            .expect("answered")
    }

    #[tokio::test]
    async fn a_live_access_token_is_described() {
        let server = server();
        let token = issued(&server).await;

        let body = ask(&server, &token.access_token).await;

        assert_eq!(body.get("active").and_then(Json::as_bool), Some(true));
        assert_eq!(body.get("scope").and_then(Json::as_str), Some("read write"));
        assert_eq!(body.get("client_id").and_then(Json::as_str), Some("web"));
        assert_eq!(body.get("sub").and_then(Json::as_str), Some("7"));
        assert_eq!(body.get("token_type").and_then(Json::as_str), Some("Bearer"));
    }

    #[tokio::test]
    async fn a_client_credentials_token_has_no_subject() {
        // How a resource server tells a machine's token from a person's.
        let server = server();
        let token = server
            .issue("web", None, &Scopes::of(["read"]), "family-2", false)
            .await
            .expect("issued");

        let body = ask(&server, &token.access_token).await;

        assert_eq!(body.get("active").and_then(Json::as_bool), Some(true));
        assert!(body.get("sub").is_none());
    }

    #[tokio::test]
    async fn every_kind_of_bad_token_gives_the_same_flat_answer() {
        // Unknown, expired and revoked must be indistinguishable, or the
        // endpoint sorts a captured batch of tokens for an attacker.
        let server = server().access_ttl(60);

        let expired = issued(&server).await;
        let revoked = issued(&server).await;
        server.revoke_family("family-1", "test").await;
        server.clock().advance(61);

        for token in ["never-existed", expired.access_token.as_str(), revoked.access_token.as_str()]
        {
            let body = ask(&server, token).await;
            assert_eq!(body.to_string(), r#"{"active":false}"#, "for {token}");
        }
    }

    #[tokio::test]
    async fn introspection_without_authentication_is_refused() {
        // The whole endpoint's value to an attacker: a free oracle telling them
        // which captured tokens are live.
        let server = server();
        let token = issued(&server).await;

        let error = post(&server, &[("token", &token.access_token)]).await.unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidClient);
    }

    #[tokio::test]
    async fn introspection_with_a_wrong_secret_is_refused() {
        let server = server();
        let token = issued(&server).await;

        let error = post(
            &server,
            &[("token", &token.access_token), ("client_id", "web"), ("client_secret", "wrong")],
        )
        .await
        .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidClient);
    }

    #[tokio::test]
    async fn a_public_client_may_not_introspect() {
        let server = server();
        let token = issued(&server).await;

        let error = post(&server, &[("token", &token.access_token), ("client_id", "spa")])
            .await
            .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidClient);
        assert!(error.to_string().contains("public by construction"));
    }

    #[tokio::test]
    async fn a_missing_token_is_a_malformed_request_not_an_inactive_one() {
        let error = post(&server(), &[("client_id", "web"), ("client_secret", "s3cret")])
            .await
            .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn a_refresh_token_can_be_introspected_too() {
        let server = server();
        let token = issued(&server).await;
        let refresh = token.refresh_token.expect("refresh");

        let body = ask(&server, &refresh).await;

        assert_eq!(body.get("active").and_then(Json::as_bool), Some(true));
        assert_eq!(body.get("token_type").and_then(Json::as_str), Some("refresh_token"));
    }
}
