//! `POST /oauth/revoke` — RFC 7009.
//!
//! Two rules from §2.2 shape the whole endpoint:
//!
//! * **An unknown token is a success.** The client asked for the token to stop
//!   working and it does not work; there is nothing to report. Answering 400
//!   for "no such token" would turn this into an oracle that says which tokens
//!   exist, to anyone who can authenticate as any client.
//! * **A token belonging to another client is also a success**, and is not
//!   revoked. Saying "that is not yours" leaks the same fact.
//!
//! Revoking a refresh token takes its family with it. §2.1 says a server SHOULD
//! invalidate the access tokens derived from a revoked refresh token, and the
//! reason is the obvious one: leaving them live means "log this application out"
//! does not log it out for another hour.

use crate::endpoints::client_auth;
use crate::endpoints::params::Params;
use crate::server::AuthorizationServer;
use crate::store::digest;
use rustlavel_http::{IntoResponse, Request, Response, Status};
use rustlavel_oauth::OAuthError;

/// `POST /oauth/revoke`.
pub async fn revoke(server: AuthorizationServer, mut request: Request) -> Response {
    let params = Params::from_body(&mut request);

    match run(&server, &request, &params).await {
        Ok(()) => Response::new(Status::OK).with_header("cache-control", "no-store"),
        Err(error) => error.into_response().with_header("cache-control", "no-store"),
    }
}

async fn run(
    server: &AuthorizationServer,
    request: &Request,
    params: &Params,
) -> Result<(), OAuthError> {
    let credentials = client_auth::presented(request, params)?;
    let client = client_auth::authenticate(server, &credentials).await?;

    // §2.1 makes `token` required; a request without one is malformed rather
    // than a revocation of nothing.
    let presented = params.required("token")?;
    let hint = params.get("token_type_hint")?;
    let hash = digest(presented);

    // The hint is an optimisation, never a restriction: §2.1 says a server must
    // extend its search to the other type if the hint does not find it, because
    // a client that guesses wrong still expects its token to stop working.
    let refresh_first = hint != Some("access_token");

    if refresh_first && revoke_refresh(server, &hash, &client.id).await {
        return Ok(());
    }
    if revoke_access(server, &hash, &client.id).await {
        return Ok(());
    }
    if !refresh_first {
        revoke_refresh(server, &hash, &client.id).await;
    }

    // Unknown, or somebody else's. Either way: 200, and nothing revoked.
    Ok(())
}

async fn revoke_refresh(server: &AuthorizationServer, hash: &str, client_id: &str) -> bool {
    let Ok(Some(record)) = server.tokens().find_refresh(hash).await else { return false };
    if record.client_id != client_id {
        return false;
    }

    // The family, not the token: revoking one half of a pair leaves the other
    // working, which is not what "revoke" means to the person who asked.
    if let Err(error) = server.tokens().revoke_family(&record.family).await {
        rustlavel_core::error!("oauth: could not revoke family {}: {error}", record.family);
    }
    true
}

async fn revoke_access(server: &AuthorizationServer, hash: &str, client_id: &str) -> bool {
    let Ok(Some(record)) = server.tokens().find_access(hash).await else { return false };
    if record.client_id != client_id {
        return false;
    }

    if let Err(error) = server.tokens().revoke_access(&record.id).await {
        rustlavel_core::error!("oauth: could not revoke access token: {error}");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, MemoryClientStore};
    use rustlavel_http::Method;
    use rustlavel_oauth::{Scopes, TokenResponse};

    fn server() -> AuthorizationServer {
        AuthorizationServer::new(
            MemoryClientStore::new()
                .with(Client::confidential("web", "s3cret").scopes(Scopes::of(["read"])))
                .with(Client::confidential("other", "s3cret")),
        )
    }

    async fn issued(server: &AuthorizationServer) -> TokenResponse {
        server
            .issue("web", Some("7"), &Scopes::of(["read"]), "family-1", true)
            .await
            .expect("issued")
    }

    async fn post(server: &AuthorizationServer, fields: &[(&str, &str)]) -> Result<(), OAuthError> {
        let mut request = Request::new(Method::Post, "/oauth/revoke").with_form(fields);
        let params = Params::from_body(&mut request);
        run(server, &request, &params).await
    }

    fn credentials(id: &str) -> [(&str, &str); 2] {
        [("client_id", id), ("client_secret", "s3cret")]
    }

    #[tokio::test]
    async fn revoking_an_access_token_stops_it_working() {
        let server = server();
        let token = issued(&server).await;
        let [id, secret] = credentials("web");

        post(&server, &[("token", &token.access_token), id, secret]).await.expect("revoked");

        assert!(server.validate_bearer(&token.access_token).await.is_err());
    }

    #[tokio::test]
    async fn revoking_a_refresh_token_takes_its_access_token_with_it() {
        // Otherwise "log this application out" leaves it logged in for an hour.
        let server = server();
        let token = issued(&server).await;
        let refresh = token.refresh_token.clone().expect("refresh");
        let [id, secret] = credentials("web");

        post(&server, &[("token", &refresh), id, secret]).await.expect("revoked");

        assert!(server.validate_bearer(&token.access_token).await.is_err());
    }

    #[tokio::test]
    async fn an_unknown_token_is_a_success_rather_than_an_oracle() {
        let [id, secret] = credentials("web");
        assert!(post(&server(), &[("token", "no-such-token"), id, secret]).await.is_ok());
    }

    #[tokio::test]
    async fn another_clients_token_is_a_success_and_stays_live() {
        // Refusing would say "that token exists, but not for you", which is
        // exactly what an attacker enumerating tokens wants to hear.
        let server = server();
        let token = issued(&server).await;
        let [id, secret] = credentials("other");

        post(&server, &[("token", &token.access_token), id, secret]).await.expect("no error");

        assert!(server.validate_bearer(&token.access_token).await.is_ok());
    }

    #[tokio::test]
    async fn a_wrong_hint_still_finds_the_token() {
        // §2.1: the hint is an optimisation. A client that guesses wrong still
        // expects its token to stop working.
        let server = server();
        let token = issued(&server).await;
        let [id, secret] = credentials("web");

        post(
            &server,
            &[("token", &token.access_token), ("token_type_hint", "refresh_token"), id, secret],
        )
        .await
        .expect("revoked");

        assert!(server.validate_bearer(&token.access_token).await.is_err());
    }

    #[tokio::test]
    async fn revocation_requires_a_client_and_a_token() {
        let server = server();
        let token = issued(&server).await;

        let unauthenticated = post(&server, &[("token", &token.access_token)]).await.unwrap_err();
        assert_eq!(unauthenticated.code, rustlavel_oauth::OAuthErrorCode::InvalidClient);
        assert!(server.validate_bearer(&token.access_token).await.is_ok());

        let [id, secret] = credentials("web");
        let no_token = post(&server, &[id, secret]).await.unwrap_err();
        assert!(no_token.to_string().contains("`token` parameter is required"));
    }

    #[tokio::test]
    async fn a_wrong_secret_revokes_nothing() {
        let server = server();
        let token = issued(&server).await;

        let error = post(
            &server,
            &[("token", &token.access_token), ("client_id", "web"), ("client_secret", "wrong")],
        )
        .await
        .unwrap_err();

        assert_eq!(error.code, rustlavel_oauth::OAuthErrorCode::InvalidClient);
        assert!(server.validate_bearer(&token.access_token).await.is_ok());
    }

    #[tokio::test]
    async fn revoking_twice_is_still_a_success() {
        let server = server();
        let token = issued(&server).await;
        let [id, secret] = credentials("web");

        post(&server, &[("token", &token.access_token), id, secret]).await.expect("revoked");
        post(&server, &[("token", &token.access_token), id, secret]).await.expect("still fine");
    }
}
