//! `POST /oauth/token` — the three grants this server supports, and the two it
//! refuses.
//!
//! # What is refused, and why
//!
//! * **`password`** — the resource owner password credentials grant. It asks
//!   the user to type their password into a third-party application, which
//!   defeats the point of OAuth, cannot carry a second factor, and hands every
//!   client a credential that works everywhere. OAuth 2.1 removes it.
//! * **`implicit`** — returns the access token in a URL fragment, where it
//!   reaches browser history, the `Referer` header and every script on the
//!   page. Also removed by OAuth 2.1. It is not really a `grant_type` at all,
//!   but clients send it, so it gets its own answer rather than a shrug.
//!
//! Both are answered with `unsupported_grant_type` and a sentence saying what
//! to use instead, because a developer who sends `password` needs to know the
//! flow is gone, not that they made a typo.

use crate::client::Client;
use crate::code::Consumption;
use crate::endpoints::client_auth;
use crate::endpoints::params::Params;
use crate::server::AuthorizationServer;
use crate::store::digest;
use crate::token::generate as generate_family;
use rustlavel_http::{IntoResponse, Request, Response};
use rustlavel_oauth::{OAuthError, OAuthErrorCode, Scopes, TokenResponse, pkce};

/// `POST /oauth/token`.
pub async fn issue(server: AuthorizationServer, mut request: Request) -> Response {
    let params = Params::from_body(&mut request);

    match exchange(&server, &request, &params).await {
        Ok(token) => success(token),
        Err(error) => {
            // RFC 6749 §5.1: a token response must never be cached, and an
            // error response carries the same `WWW-Authenticate` obligations.
            error.into_response().with_header("cache-control", "no-store")
        }
    }
}

/// RFC 6749 §5.1: `Cache-Control: no-store` and `Pragma: no-cache`, because the
/// body is a live credential and a shared proxy would otherwise keep it.
fn success(token: TokenResponse) -> Response {
    Response::json(token.to_json())
        .with_header("cache-control", "no-store")
        .with_header("pragma", "no-cache")
}

async fn exchange(
    server: &AuthorizationServer,
    request: &Request,
    params: &Params,
) -> Result<TokenResponse, OAuthError> {
    let credentials = client_auth::presented(request, params)?;
    let client = client_auth::authenticate(server, &credentials).await?;

    match params.required("grant_type")? {
        "authorization_code" => authorization_code(server, &client, params).await,
        "refresh_token" => refresh_token(server, &client, params).await,
        "client_credentials" => client_credentials(server, &client, params).await,
        "password" => Err(OAuthError::because(
            OAuthErrorCode::UnsupportedGrantType,
            "the `password` grant was removed in OAuth 2.1. It requires the user to hand their \
             password to the client, which cannot carry a second factor and gives the client a \
             credential that works everywhere. Use `authorization_code` with PKCE.",
        )),
        "implicit" | "token" => Err(OAuthError::because(
            OAuthErrorCode::UnsupportedGrantType,
            "the implicit flow was removed in OAuth 2.1: it delivers the access token in a URL \
             fragment, where it reaches browser history and every script on the page. Use \
             `authorization_code` with PKCE.",
        )),
        other => Err(OAuthError::because(
            OAuthErrorCode::UnsupportedGrantType,
            format!(
                "unsupported grant_type {other:?}. This server supports `authorization_code`, \
                 `refresh_token` and `client_credentials`."
            ),
        )),
    }
}

/// The authorisation code grant, RFC 6749 §4.1.3 plus RFC 7636 §4.6.
///
/// The code is spent *first*, before any of the checks that follow. That order
/// is deliberate: spending it is the atomic operation, and doing it last would
/// leave a window in which two requests both pass their checks. The visible
/// consequence is that presenting a code with the wrong client, the wrong
/// redirect URI or the wrong verifier burns it — which is correct, because a
/// code that arrives with any of those wrong is a code that reached somebody it
/// was not issued to.
async fn authorization_code(
    server: &AuthorizationServer,
    client: &Client,
    params: &Params,
) -> Result<TokenResponse, OAuthError> {
    let presented = params.required("code")?;
    let redirect_uri = params.required("redirect_uri")?;
    let verifier = params.get("code_verifier")?.ok_or_else(|| {
        OAuthError::invalid_request(
            "a `code_verifier` is required: every code this server issues is bound to a PKCE \
             challenge (OAuth 2.1 §4.1.1)",
        )
    })?;

    let family = generate_family();
    let consumption = server
        .codes()
        .consume(&digest(presented), &family, server.now())
        .await
        .map_err(|error| {
            rustlavel_core::error!("oauth: the code store failed: {error}");
            OAuthError::server_error("the authorization code could not be read")
        })?;

    let code = match consumption {
        Consumption::Fresh(code) => code,
        Consumption::Replayed { family } => {
            // RFC 6749 §10.5. The legitimate client redeems a code once, so a
            // second presentation means somebody else has it — and whichever
            // exchange was theirs, tokens exist that should not.
            if let Some(family) = family {
                server.revoke_family(&family, "the authorization code was presented twice").await;
            }
            return Err(OAuthError::invalid_grant(
                "this authorization code has already been used. Every token issued from it has \
                 been revoked, because a code presented twice is a code that leaked.",
            ));
        }
        Consumption::Expired => {
            return Err(OAuthError::invalid_grant("this authorization code has expired"));
        }
        Consumption::Unknown => {
            return Err(OAuthError::invalid_grant("no such authorization code"));
        }
    };

    // RFC 6749 §4.1.3: "ensure that the authorization code was issued to the
    // authenticated confidential client". Without this, any registered client
    // can redeem a code intended for another — and every client id is public.
    if code.client_id != client.id {
        return Err(OAuthError::invalid_grant(
            "this authorization code was issued to a different client",
        ));
    }

    // §4.1.3 again: the same `redirect_uri` as the authorization request. This
    // is what stops a code obtained through one registered URI being redeemed
    // as though it came through another.
    if code.redirect_uri != redirect_uri {
        return Err(OAuthError::invalid_grant(
            "the `redirect_uri` does not match the one this code was issued for",
        ));
    }

    // RFC 7636 §4.6. Constant-time, inside `verify`.
    if !pkce::verify(verifier, &code.challenge, code.challenge_method) {
        return Err(OAuthError::invalid_grant(
            "the `code_verifier` does not match the `code_challenge` this code was issued with",
        ));
    }

    // A `scope` here may only narrow. Widening it would let a client that was
    // granted `read` walk away with `write` by asking again at a step the user
    // never sees.
    let scopes = narrowed(params, &code.scopes)?;

    server
        .issue(&client.id, Some(&code.user_id), &scopes, &family, true)
        .await
        .map_err(store_failed)
}

/// The refresh grant, with rotation — RFC 6749 §6 and RFC 9700 §4.14.
///
/// Every refresh mints a new refresh token and retires the one presented. The
/// reason to bother is the case where a refresh token is stolen: the thief and
/// the legitimate client now hold the same token, one of them uses it, and the
/// other's next attempt presents a token that has already been rotated. That is
/// the only signal there is, and it is only a signal if presenting a rotated
/// token kills the whole family — revoking just the presented token would leave
/// whichever of them went first still holding live credentials.
async fn refresh_token(
    server: &AuthorizationServer,
    client: &Client,
    params: &Params,
) -> Result<TokenResponse, OAuthError> {
    let presented = params.required("refresh_token")?;

    let record = server
        .tokens()
        .find_refresh(&digest(presented))
        .await
        .map_err(|error| {
            rustlavel_core::error!("oauth: the token store failed: {error}");
            OAuthError::server_error("the refresh token could not be read")
        })?
        .ok_or_else(|| OAuthError::invalid_grant("no such refresh token"))?;

    // Presented by a client it was not issued to: it left the client that owns
    // it, so the family goes down with it.
    if record.client_id != client.id {
        server
            .revoke_family(&record.family, "a refresh token was presented by a different client")
            .await;
        return Err(OAuthError::invalid_grant(
            "this refresh token was issued to a different client",
        ));
    }

    if record.rotated {
        server.revoke_family(&record.family, "a rotated refresh token was presented again").await;
        return Err(OAuthError::invalid_grant(
            "this refresh token has already been exchanged. Every token in its family has been \
             revoked, because two parties holding one refresh token means it leaked.",
        ));
    }

    if record.revoked {
        return Err(OAuthError::invalid_grant("this refresh token has been revoked"));
    }

    if server.now() >= record.expires_at {
        return Err(OAuthError::invalid_grant("this refresh token has expired"));
    }

    // The compare-and-set. Losing it means another request rotated the same
    // token between the read above and here — the race version of a reuse, and
    // treated identically.
    let rotated = server.tokens().rotate(&record.id).await.map_err(store_failed)?;
    if !rotated {
        server.revoke_family(&record.family, "two requests presented the same refresh token").await;
        return Err(OAuthError::invalid_grant(
            "this refresh token has already been exchanged. Every token in its family has been \
             revoked.",
        ));
    }

    // The access token minted alongside it would otherwise stay live for the
    // rest of its hour, which is exactly the credential a thief would keep.
    server.tokens().revoke_access(&record.access_token_id).await.map_err(store_failed)?;

    let scopes = narrowed(params, &record.scopes)?;

    server
        .issue(&client.id, record.user_id.as_deref(), &scopes, &record.family, true)
        .await
        .map_err(store_failed)
}

/// The client credentials grant, RFC 6749 §4.4.
///
/// There is no user here, so there is nothing to stay signed in as and
/// §4.4.3 says not to issue a refresh token: the client already holds a
/// credential that works forever and can simply ask again.
async fn client_credentials(
    server: &AuthorizationServer,
    client: &Client,
    params: &Params,
) -> Result<TokenResponse, OAuthError> {
    if !client.is_confidential() {
        return Err(OAuthError::because(
            OAuthErrorCode::UnauthorizedClient,
            "`client_credentials` needs a client that can authenticate, and this one is public. \
             Its id is printed in every redirect URL its users see.",
        ));
    }

    let scopes = narrowed(params, &client.scopes)?;

    server
        .issue(&client.id, None, &scopes, &generate_family(), false)
        .await
        .map_err(store_failed)
}

/// A requested `scope`, refused if it asks for anything beyond `granted`.
///
/// Narrowing is allowed and useful — a client can ask for a token weaker than
/// what it holds. Widening is the escalation this exists to stop: the user
/// approved a set of scopes at the authorisation step, and the token step is
/// not another chance to change it.
fn narrowed(params: &Params, granted: &Scopes) -> Result<Scopes, OAuthError> {
    let Some(raw) = params.get("scope")? else { return Ok(granted.clone()) };
    let requested = Scopes::parse(raw);

    let beyond = requested.beyond(granted);
    if beyond.is_empty() {
        Ok(requested)
    } else {
        Err(OAuthError::invalid_scope(format!(
            "this grant does not cover: {beyond}. A `scope` at the token endpoint may only \
             narrow what was already granted."
        )))
    }
}

fn store_failed(error: rustlavel_core::Error) -> OAuthError {
    rustlavel_core::error!("oauth: the token store failed: {error}");
    OAuthError::server_error("the token could not be issued")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MemoryClientStore;
    use crate::code::AuthorizationCode;
    use rustlavel_http::Method;
    use rustlavel_oauth::{ChallengeMethod, Pkce};

    fn server() -> AuthorizationServer {
        AuthorizationServer::new(
            MemoryClientStore::new()
                .with(
                    Client::confidential("web", "s3cret")
                        .redirect_uri("https://a.test/cb")
                        .scopes(Scopes::of(["read", "write"])),
                )
                .with(
                    Client::public("spa")
                        .redirect_uri("https://b.test/cb")
                        .scopes(Scopes::of(["read"])),
                ),
        )
    }

    /// Put a code in the store as though `/authorize` had just issued it.
    async fn plant_code(
        server: &AuthorizationServer,
        code: &str,
        client_id: &str,
        pkce: &Pkce,
        scopes: Scopes,
    ) {
        let record = AuthorizationCode::issued(code, server.now(), server.settings().code_ttl)
            .for_client(client_id)
            .for_user("7")
            .redirecting_to("https://a.test/cb")
            .granting(scopes)
            .challenged(pkce.challenge(), ChallengeMethod::S256);
        server.codes().store(record).await.expect("stored");
    }

    async fn post(server: &AuthorizationServer, fields: &[(&str, &str)]) -> Result<TokenResponse, OAuthError> {
        let mut request = Request::new(Method::Post, "/oauth/token").with_form(fields);
        let params = Params::from_body(&mut request);
        exchange(server, &request, &params).await
    }

    fn code_grant<'a>(code: &'a str, verifier: &'a str) -> Vec<(&'a str, &'a str)> {
        vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", "https://a.test/cb"),
            ("code_verifier", verifier),
            ("client_id", "web"),
            ("client_secret", "s3cret"),
        ]
    }

    #[tokio::test]
    async fn the_happy_path_returns_an_access_and_a_refresh_token() {
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        let token = post(&server, &code_grant("the-code", pkce.verifier())).await.expect("granted");

        assert_eq!(token.token_type, "Bearer");
        assert!(token.refresh_token.is_some());
        assert_eq!(token.scope.expect("scope"), Scopes::of(["read"]));
        assert!(server.validate_bearer(&token.access_token).await.is_ok());
    }

    #[tokio::test]
    async fn replaying_a_code_fails_and_revokes_what_the_first_use_got() {
        // RFC 6749 §10.5: refusing the replay alone would leave the attacker
        // holding whatever the first exchange produced.
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        let first = post(&server, &code_grant("the-code", pkce.verifier())).await.expect("granted");
        let second = post(&server, &code_grant("the-code", pkce.verifier())).await.unwrap_err();

        assert_eq!(second.code, OAuthErrorCode::InvalidGrant);
        assert!(second.to_string().contains("already been used"));
        assert!(
            server.validate_bearer(&first.access_token).await.is_err(),
            "the first exchange's access token is still live"
        );
    }

    #[tokio::test]
    async fn a_code_issued_to_one_client_cannot_be_redeemed_by_another() {
        // Every client id is public, so without this check any registered
        // client could redeem a code intended for another.
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        let error = post(
            &server,
            &[
                ("grant_type", "authorization_code"),
                ("code", "the-code"),
                ("redirect_uri", "https://a.test/cb"),
                ("code_verifier", pkce.verifier()),
                ("client_id", "spa"),
            ],
        )
        .await
        .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidGrant);
        assert!(error.to_string().contains("different client"));
    }

    #[tokio::test]
    async fn a_wrong_pkce_verifier_is_refused() {
        // What an attacker who intercepted the code but not the verifier has.
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        let error = post(&server, &code_grant("the-code", Pkce::generate().verifier()))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("does not match the `code_challenge`"));
    }

    #[tokio::test]
    async fn the_challenge_cannot_be_passed_off_as_the_verifier() {
        // The downgrade an attacker would try if `plain` were reachable: the
        // challenge is in the URL, the verifier is not.
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        let challenge = pkce.challenge();
        let error = post(&server, &code_grant("the-code", &challenge)).await.unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidGrant);
    }

    #[tokio::test]
    async fn a_missing_code_verifier_is_refused() {
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        let error = post(
            &server,
            &[
                ("grant_type", "authorization_code"),
                ("code", "the-code"),
                ("redirect_uri", "https://a.test/cb"),
                ("client_id", "web"),
                ("client_secret", "s3cret"),
            ],
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("`code_verifier` is required"));
    }

    #[tokio::test]
    async fn a_different_redirect_uri_at_the_token_step_is_refused() {
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        let error = post(
            &server,
            &[
                ("grant_type", "authorization_code"),
                ("code", "the-code"),
                ("redirect_uri", "https://a.test/cb/"),
                ("code_verifier", pkce.verifier()),
                ("client_id", "web"),
                ("client_secret", "s3cret"),
            ],
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("does not match the one this code was issued for"));
    }

    #[tokio::test]
    async fn an_expired_code_is_refused() {
        let server = server().code_ttl(60);
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        server.clock().advance(61);
        let error = post(&server, &code_grant("the-code", pkce.verifier())).await.unwrap_err();

        assert!(error.to_string().contains("expired"));
    }

    #[tokio::test]
    async fn a_wrong_client_secret_is_refused_before_the_code_is_touched() {
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        let error = post(
            &server,
            &[
                ("grant_type", "authorization_code"),
                ("code", "the-code"),
                ("redirect_uri", "https://a.test/cb"),
                ("code_verifier", pkce.verifier()),
                ("client_id", "web"),
                ("client_secret", "wrong"),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::InvalidClient);

        // And the code survived, so a wrong secret is not a way to burn it.
        assert!(post(&server, &code_grant("the-code", pkce.verifier())).await.is_ok());
    }

    #[tokio::test]
    async fn scope_cannot_be_widened_at_the_token_step() {
        // The escalation: the user approved `read` on the consent screen, and
        // the token request quietly asks for `write` as well.
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;

        let mut fields = code_grant("the-code", pkce.verifier());
        fields.push(("scope", "read write"));
        let error = post(&server, &fields).await.unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidScope);
        assert!(error.to_string().contains("write"));
    }

    #[tokio::test]
    async fn scope_may_be_narrowed_at_the_token_step() {
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read", "write"])).await;

        let mut fields = code_grant("the-code", pkce.verifier());
        fields.push(("scope", "read"));
        let token = post(&server, &fields).await.expect("granted");

        assert_eq!(token.scope.expect("scope"), Scopes::of(["read"]));
    }

    #[tokio::test]
    async fn a_refresh_rotates_and_retires_the_token_it_was_given() {
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;
        let first = post(&server, &code_grant("the-code", pkce.verifier())).await.expect("granted");
        let old_refresh = first.refresh_token.clone().expect("refresh");

        let second = post(
            &server,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &old_refresh),
                ("client_id", "web"),
                ("client_secret", "s3cret"),
            ],
        )
        .await
        .expect("refreshed");

        let new_refresh = second.refresh_token.clone().expect("refresh");
        assert_ne!(new_refresh, old_refresh, "a refresh must rotate");
        assert_ne!(second.access_token, first.access_token);
        // And the access token it replaced is dead, not left live for an hour.
        assert!(server.validate_bearer(&first.access_token).await.is_err());
        assert!(server.validate_bearer(&second.access_token).await.is_ok());
    }

    #[tokio::test]
    async fn replaying_a_rotated_refresh_token_revokes_the_whole_family() {
        // The point of rotation. The thief and the client hold the same token;
        // whichever goes second is refused, and both are cut off.
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;
        let first = post(&server, &code_grant("the-code", pkce.verifier())).await.expect("granted");
        let stolen = first.refresh_token.clone().expect("refresh");

        let refresh = |token: String| {
            let server = server.clone();
            async move {
                post(
                    &server,
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", &token),
                        ("client_id", "web"),
                        ("client_secret", "s3cret"),
                    ],
                )
                .await
            }
        };

        let second = refresh(stolen.clone()).await.expect("the first use succeeds");
        let error = refresh(stolen).await.unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidGrant);
        assert!(error.to_string().contains("already been exchanged"));

        // Everything descended from that authorization is now dead, including
        // the tokens the legitimate use had just been issued.
        assert!(server.validate_bearer(&second.access_token).await.is_err());
        let after = refresh(second.refresh_token.expect("refresh")).await.unwrap_err();
        assert_eq!(after.code, OAuthErrorCode::InvalidGrant);
    }

    #[tokio::test]
    async fn a_refresh_token_presented_by_another_client_kills_the_family() {
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;
        let first = post(&server, &code_grant("the-code", pkce.verifier())).await.expect("granted");
        let refresh = first.refresh_token.clone().expect("refresh");

        let error = post(
            &server,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", "spa"),
            ],
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("different client"));
        assert!(server.validate_bearer(&first.access_token).await.is_err());
    }

    #[tokio::test]
    async fn scope_cannot_be_widened_at_a_refresh_either() {
        let server = server();
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;
        let first = post(&server, &code_grant("the-code", pkce.verifier())).await.expect("granted");
        let refresh = first.refresh_token.expect("refresh");

        let error = post(
            &server,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("scope", "read write"),
                ("client_id", "web"),
                ("client_secret", "s3cret"),
            ],
        )
        .await
        .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidScope);
    }

    #[tokio::test]
    async fn an_expired_refresh_token_is_refused() {
        let server = server().refresh_ttl(3600);
        let pkce = Pkce::generate();
        plant_code(&server, "the-code", "web", &pkce, Scopes::of(["read"])).await;
        let first = post(&server, &code_grant("the-code", pkce.verifier())).await.expect("granted");
        let refresh = first.refresh_token.expect("refresh");

        server.clock().advance(3600);
        let error = post(
            &server,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", "web"),
                ("client_secret", "s3cret"),
            ],
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("expired"));
    }

    #[tokio::test]
    async fn an_unknown_refresh_token_is_refused() {
        let error = post(
            &server(),
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", "nope"),
                ("client_id", "web"),
                ("client_secret", "s3cret"),
            ],
        )
        .await
        .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidGrant);
    }

    #[tokio::test]
    async fn client_credentials_issues_a_userless_token_with_no_refresh() {
        let server = server();
        let token = post(
            &server,
            &[
                ("grant_type", "client_credentials"),
                ("client_id", "web"),
                ("client_secret", "s3cret"),
            ],
        )
        .await
        .expect("granted");

        assert!(token.refresh_token.is_none(), "RFC 6749 §4.4.3");
        let record = server.validate_bearer(&token.access_token).await.expect("live");
        assert!(record.user_id.is_none());
        assert_eq!(record.scopes, Scopes::of(["read", "write"]));
    }

    #[tokio::test]
    async fn client_credentials_is_refused_for_a_public_client() {
        let error = post(&server(), &[("grant_type", "client_credentials"), ("client_id", "spa")])
            .await
            .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::UnauthorizedClient);
    }

    #[tokio::test]
    async fn client_credentials_cannot_exceed_what_the_client_registered() {
        let error = post(
            &server(),
            &[
                ("grant_type", "client_credentials"),
                ("scope", "read admin"),
                ("client_id", "web"),
                ("client_secret", "s3cret"),
            ],
        )
        .await
        .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::InvalidScope);
        assert!(error.to_string().contains("admin"));
    }

    #[tokio::test]
    async fn the_password_grant_is_refused_and_says_why() {
        let error = post(
            &server(),
            &[
                ("grant_type", "password"),
                ("username", "ada"),
                ("password", "hunter2"),
                ("client_id", "web"),
                ("client_secret", "s3cret"),
            ],
        )
        .await
        .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::UnsupportedGrantType);
        assert!(error.to_string().contains("removed in OAuth 2.1"));
        assert!(error.to_string().contains("authorization_code"));
    }

    #[tokio::test]
    async fn the_implicit_flow_is_refused_and_says_why() {
        let error = post(
            &server(),
            &[("grant_type", "implicit"), ("client_id", "web"), ("client_secret", "s3cret")],
        )
        .await
        .unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::UnsupportedGrantType);
        assert!(error.to_string().contains("URL fragment"));
    }

    #[tokio::test]
    async fn an_unknown_grant_type_lists_the_ones_that_work() {
        let error = post(
            &server(),
            &[("grant_type", "magic"), ("client_id", "web"), ("client_secret", "s3cret")],
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("client_credentials"));
    }
}
