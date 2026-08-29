//! `GET /oauth/authorize` and the consent form's `POST` back to it.
//!
//! # The order of validation is the security property
//!
//! Everything here turns on one question: *has the redirect URI been proven to
//! belong to this client yet?* Before that point there is nowhere safe to send
//! an error, because the only URI available came from the request — which is to
//! say, possibly from the attacker. Redirecting an error to an unvalidated URI
//! is not a smaller version of the open-redirect bug, it *is* the bug: an
//! attacker sends a victim to `/oauth/authorize?client_id=real&redirect_uri=
//! https://evil.example` and, if the server redirects the resulting error, has
//! a redirector on the authorisation server's own origin — and, with a little
//! more luck, the code.
//!
//! So validation runs in exactly this order, and [`Refusal`] makes the two
//! halves different types so the distinction cannot be lost in a refactor:
//!
//! 1. `client_id` — present, and a client we know. *Rendered locally.*
//! 2. `redirect_uri` — present, and byte-identical to one this client
//!    registered. *Rendered locally.*
//! 3. Everything else: `response_type`, PKCE, scope. *Redirected back*, with
//!    `error=` and the client's `state`, per RFC 6749 §4.1.2.1.
//!
//! # PKCE is required, and `plain` is not accepted
//!
//! OAuth 2.1 §4.1.1 makes `code_challenge` mandatory for every client, not just
//! public ones, and this server additionally refuses `plain`. RFC 7636 §4.2
//! permits `plain` only where SHA-256 is genuinely unavailable, and a `plain`
//! challenge is the verifier written into the URL — an interceptor who has the
//! code has the verifier too, so it protects against precisely nothing. A
//! server that accepts `plain` can be downgraded to it by the request itself,
//! which makes supporting it equivalent to not supporting PKCE at all.

use crate::client::Client;
use crate::code::AuthorizationCode;
use crate::consent::Grant;
use crate::endpoints::params::Params;
use crate::page;
use crate::server::AuthorizationServer;
use crate::token::generate as generate_token;
use rustlavel_auth::guard::Identity;
use rustlavel_http::{Request, Response};
use rustlavel_oauth::{ChallengeMethod, OAuthError, OAuthErrorCode, Scopes, url};

/// A refused authorisation request, and where the refusal may be delivered.
pub enum Refusal {
    /// The redirect URI is not yet trusted. This must be rendered on our page.
    Local(OAuthError),
    /// The redirect URI is registered, so the client may be told what happened.
    Redirect { uri: String, error: Box<OAuthError> },
}

/// `Debug` names which half of the split a refusal fell into, because that is
/// the thing worth seeing when a test about redirect URIs fails.
impl std::fmt::Debug for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Local(error) => write!(f, "Local({error})"),
            Refusal::Redirect { uri, error } => write!(f, "Redirect to {uri}: {error}"),
        }
    }
}

impl Refusal {
    fn into_response(self) -> Response {
        match self {
            Refusal::Local(error) => page::error(&error),
            Refusal::Redirect { uri, error } => page::redirect_error(&uri, &error),
        }
    }
}

/// A request that has passed every check.
pub struct Authorization {
    pub client: Client,
    pub redirect_uri: String,
    pub scopes: Scopes,
    pub state: Option<String>,
    pub challenge: String,
    pub challenge_method: ChallengeMethod,
}

/// Validate an authorisation request, in the order described above.
pub async fn validate(
    server: &AuthorizationServer,
    params: &Params,
) -> Result<Authorization, Refusal> {
    // --- Before the redirect URI is trusted: nothing may be redirected. ---

    let client_id = params.required("client_id").map_err(Refusal::Local)?;
    let client = server.client(client_id).await.ok_or_else(|| {
        Refusal::Local(OAuthError::because(
            OAuthErrorCode::InvalidClient,
            "unknown client_id",
        ))
    })?;

    // Required unconditionally, even for a client with a single registered URI
    // that RFC 6749 §3.1.2.3 would let omit it. Requiring it means the code is
    // always bound to a URI the client named, so the token endpoint has
    // something to compare against.
    let requested_uri = params.required("redirect_uri").map_err(Refusal::Local)?;

    if !client.allows_redirect(requested_uri) {
        return Err(Refusal::Local(OAuthError::invalid_request(
            "the `redirect_uri` is not one this client registered. It is compared byte for \
             byte: a trailing slash, an added query string or a different port makes it a \
             different URI.",
        )));
    }
    let redirect_uri = requested_uri.to_string();

    // --- From here the client may be told what went wrong. ---

    let state = params.get("state").map_err(|error| redirected(&redirect_uri, error, None))?;
    let state = state.map(str::to_string);
    let bounce = |error: OAuthError| redirected(&redirect_uri, error, state.clone());

    let response_type = params.required("response_type").map_err(&bounce)?;
    if response_type != "code" {
        let error = if response_type == "token" || response_type.contains("token") {
            OAuthError::because(
                OAuthErrorCode::UnsupportedResponseType,
                "the implicit flow (`response_type=token`) was removed in OAuth 2.1: it returns \
                 an access token in a URL fragment, where it lands in browser history and in \
                 every script on the page. Use `response_type=code` with PKCE.",
            )
        } else {
            OAuthError::because(
                OAuthErrorCode::UnsupportedResponseType,
                "this server supports `response_type=code` only",
            )
        };
        return Err(bounce(error));
    }

    let (challenge, challenge_method) = pkce(params).map_err(&bounce)?;

    let requested = match params.get("scope").map_err(&bounce)? {
        Some(raw) => Scopes::parse(raw),
        None => Scopes::new(),
    };
    let beyond = requested.beyond(&client.scopes);
    if !beyond.is_empty() {
        return Err(bounce(OAuthError::invalid_scope(format!(
            "this client is not registered for: {beyond}"
        ))));
    }

    let scopes = requested.intersect(&client.scopes);

    Ok(Authorization { client, redirect_uri, scopes, state, challenge, challenge_method })
}

/// PKCE, required and S256-only.
fn pkce(params: &Params) -> Result<(String, ChallengeMethod), OAuthError> {
    let Some(challenge) = params.get("code_challenge")? else {
        return Err(OAuthError::invalid_request(
            "a `code_challenge` is required. OAuth 2.1 §4.1.1 makes PKCE mandatory for every \
             client: without it, anything that intercepts the authorization code can redeem it.",
        ));
    };

    // No default. RFC 7636 §4.3 defaults an absent method to `plain`, which
    // would let a client opt out of PKCE by leaving the parameter off.
    let Some(raw) = params.get("code_challenge_method")? else {
        return Err(OAuthError::invalid_request(
            "a `code_challenge_method` is required, and must be `S256`. It is not defaulted, \
             because RFC 7636 §4.3 defaults it to `plain` — which would let a client turn PKCE \
             off by omitting a parameter.",
        ));
    };

    match ChallengeMethod::parse(raw) {
        Some(ChallengeMethod::S256) => Ok((challenge.to_string(), ChallengeMethod::S256)),
        Some(ChallengeMethod::Plain) => Err(OAuthError::invalid_request(
            "`code_challenge_method=plain` is not accepted. A plain challenge is the verifier \
             itself, written into the URL, so whoever intercepts the code has the verifier too. \
             Use `S256`.",
        )),
        None => Err(OAuthError::invalid_request(format!(
            "unknown `code_challenge_method`: {raw:?}. This server accepts `S256`."
        ))),
    }
}

fn redirected(uri: &str, error: OAuthError, state: Option<String>) -> Refusal {
    Refusal::Redirect { uri: uri.to_string(), error: Box::new(error.with_state(state)) }
}

/// `GET /oauth/authorize`.
pub async fn show(server: AuthorizationServer, request: Request) -> Response {
    let params = Params::from_query(&request);
    let authorization = match validate(&server, &params).await {
        Ok(authorization) => authorization,
        Err(refusal) => return refusal.into_response(),
    };

    // Who is this? The application's own `auth` middleware puts it there; this
    // crate does not implement sessions or logins, it consumes them.
    let Some(identity) = request.extension::<Identity>().cloned() else {
        return sign_in_first(&server, &request);
    };

    if skips_consent(&server, &authorization, identity.id()).await {
        return issue(&server, &authorization, identity.id()).await;
    }

    page::Consent {
        client: &authorization.client,
        scopes: &authorization.scopes,
        action: &server.settings().endpoint("authorize"),
        redirect_uri: &authorization.redirect_uri,
        state: authorization.state.as_deref(),
        challenge: &authorization.challenge,
        challenge_method: authorization.challenge_method.as_str(),
        csrf_field: &rustlavel_auth::csrf::field(&request),
    }
    .render()
}

/// `POST /oauth/authorize` — the consent form coming back.
///
/// Every parameter is validated again from scratch. The hidden fields are the
/// browser's copy of the request, not this server's memory of it, so a tampered
/// `redirect_uri` or a widened `scope` has to survive [`validate`] exactly as
/// it would have on the way in.
pub async fn decide(server: AuthorizationServer, mut request: Request) -> Response {
    let params = Params::from_body(&mut request);
    let authorization = match validate(&server, &params).await {
        Ok(authorization) => authorization,
        Err(refusal) => return refusal.into_response(),
    };

    let Some(identity) = request.extension::<Identity>().cloned() else {
        return sign_in_first(&server, &request);
    };

    let approved = matches!(params.get("approve").ok().flatten(), Some("yes"));
    if !approved {
        return page::redirect_error(
            &authorization.redirect_uri,
            &OAuthError::because(OAuthErrorCode::AccessDenied, "the user declined")
                .with_state(authorization.state.clone()),
        );
    }

    let grant = Grant::new(
        authorization.client.id.clone(),
        identity.id(),
        authorization.scopes.clone(),
        server.now(),
    );
    if let Err(error) = server.consent().record(grant).await {
        rustlavel_core::error!("oauth: could not record consent: {error}");
    }

    issue(&server, &authorization, identity.id()).await
}

/// Whether this request can skip the screen.
///
/// Only a first-party client with a grant already covering these scopes. A
/// third-party client is asked every time even when a grant exists, so that
/// declining remains a way to stop it; and a first-party client asking for one
/// more scope than last time is asked about that scope, because otherwise the
/// first modest consent quietly becomes permission for everything.
async fn skips_consent(
    server: &AuthorizationServer,
    authorization: &Authorization,
    user_id: &str,
) -> bool {
    if !authorization.client.first_party {
        return false;
    }

    match server.consent().find(&authorization.client.id, user_id).await {
        Ok(Some(grant)) => grant.scopes.covers(&authorization.scopes),
        Ok(None) => false,
        Err(error) => {
            // A consent store that is down must mean "ask", never "assume yes".
            rustlavel_core::error!("oauth: the consent store failed: {error}");
            false
        }
    }
}

/// Mint a code and send the browser back to the client.
async fn issue(
    server: &AuthorizationServer,
    authorization: &Authorization,
    user_id: &str,
) -> Response {
    let plaintext = generate_token();
    let record = AuthorizationCode::issued(
        &plaintext,
        server.now(),
        server.settings().code_ttl,
    )
    .for_client(&authorization.client.id)
    .for_user(user_id)
    .redirecting_to(&authorization.redirect_uri)
    .granting(authorization.scopes.clone())
    .challenged(&authorization.challenge, authorization.challenge_method);

    if let Err(error) = server.codes().store(record).await {
        rustlavel_core::error!("oauth: could not store the authorization code: {error}");
        return page::redirect_error(
            &authorization.redirect_uri,
            &OAuthError::server_error("the authorization code could not be stored")
                .with_state(authorization.state.clone()),
        );
    }

    let mut query = format!("code={}", url::encode(&plaintext));
    if let Some(state) = &authorization.state {
        query.push_str(&format!("&state={}", url::encode(state)));
    }

    Response::redirect(url::append_query(&authorization.redirect_uri, &query))
        // The location carries a live authorization code.
        .with_header("cache-control", "no-store")
}

/// Send an anonymous visitor to the application's login page first.
///
/// The `next` parameter is this server's own path, built here rather than taken
/// from the request, so it cannot be turned into a redirect somewhere else.
fn sign_in_first(server: &AuthorizationServer, request: &Request) -> Response {
    let back = url::encode(request.target());
    Response::see_other(format!("{}?next={back}", server.settings().login_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MemoryClientStore;
    use rustlavel_http::Method;

    fn server() -> AuthorizationServer {
        AuthorizationServer::new(
            MemoryClientStore::new()
                .with(
                    Client::public("spa")
                        .redirect_uri("https://a.test/cb")
                        .scopes(Scopes::of(["read", "write"])),
                )
                .with(Client::public("bare").redirect_uri("https://b.test/cb")),
        )
    }

    fn params(query: &str) -> Params {
        Params::from_query(&Request::new(Method::Get, format!("/oauth/authorize?{query}")))
    }

    const VALID: &str = "response_type=code&client_id=spa&redirect_uri=https://a.test/cb\
                         &code_challenge=abc&code_challenge_method=S256";

    async fn refuse(query: &str) -> Refusal {
        match validate(&server(), &params(query)).await {
            Err(refusal) => refusal,
            Ok(_) => panic!("expected a refusal for {query}"),
        }
    }

    fn local(refusal: Refusal) -> OAuthError {
        match refusal {
            Refusal::Local(error) => error,
            Refusal::Redirect { uri, error } => {
                panic!("this must not be redirected to {uri}: {error}")
            }
        }
    }

    fn redirect(refusal: Refusal) -> (String, OAuthError) {
        match refusal {
            Refusal::Redirect { uri, error } => (uri, *error),
            Refusal::Local(error) => panic!("expected a redirect, got a local page: {error}"),
        }
    }

    #[tokio::test]
    async fn a_complete_request_passes_and_narrows_the_scopes() {
        let authorization = validate(&server(), &params(&format!("{VALID}&scope=read&state=xyz")))
            .await
            .expect("valid");

        assert_eq!(authorization.client.id, "spa");
        assert_eq!(authorization.scopes, Scopes::of(["read"]));
        assert_eq!(authorization.state.as_deref(), Some("xyz"));
        assert_eq!(authorization.challenge_method, ChallengeMethod::S256);
    }

    #[tokio::test]
    async fn an_unknown_client_is_refused_on_our_own_page() {
        // Nothing is redirected: at this point the redirect URI has not been
        // checked against anything, so it is the attacker's to choose.
        let error = local(refuse("response_type=code&client_id=ghost&redirect_uri=https://evil.test/cb").await);
        assert_eq!(error.code, OAuthErrorCode::InvalidClient);
    }

    #[tokio::test]
    async fn a_missing_redirect_uri_is_refused_on_our_own_page() {
        let error = local(refuse("response_type=code&client_id=spa&code_challenge=abc").await);
        assert!(error.to_string().contains("`redirect_uri` parameter is required"));
    }

    #[tokio::test]
    async fn redirect_uri_substitution_is_refused_on_our_own_page() {
        // The attack: a real client_id with somebody else's redirect_uri. If
        // the error were redirected, this endpoint would be an open redirect
        // on the origin that issues tokens.
        for uri in [
            "https://evil.test/cb",
            "https://a.test.evil.com/cb",
            "https://a.test/cb/",
            "https://a.test/cb?x=1",
            "https://a.test/cb/../evil",
            "//evil.com",
            "http://a.test/cb",
            "https://a.test:443/cb",
            "https://A.TEST/cb",
        ] {
            let query = format!(
                "response_type=code&client_id=spa&redirect_uri={}&code_challenge=abc\
                 &code_challenge_method=S256",
                url::encode(uri)
            );
            let error = local(refuse(&query).await);
            assert!(
                error.to_string().contains("not one this client registered"),
                "{uri} was not refused locally"
            );
        }
    }

    #[tokio::test]
    async fn a_repeated_redirect_uri_is_refused_before_either_copy_is_used() {
        let error = local(
            refuse(
                "response_type=code&client_id=spa&redirect_uri=https://a.test/cb\
                 &redirect_uri=https://evil.test/cb&code_challenge=abc&code_challenge_method=S256",
            )
            .await,
        );

        assert!(error.to_string().contains("more than once"));
    }

    #[tokio::test]
    async fn a_request_with_no_pkce_at_all_is_refused() {
        let query = "response_type=code&client_id=spa&redirect_uri=https://a.test/cb";
        let (uri, error) = redirect(refuse(query).await);

        assert_eq!(uri, "https://a.test/cb");
        assert_eq!(error.code, OAuthErrorCode::InvalidRequest);
        assert!(error.to_string().contains("`code_challenge` is required"));
    }

    #[tokio::test]
    async fn a_challenge_with_no_method_is_not_quietly_treated_as_plain() {
        // RFC 7636 §4.3 says an absent method means `plain`. Following that
        // would let any client switch PKCE off by omitting a parameter.
        let query =
            "response_type=code&client_id=spa&redirect_uri=https://a.test/cb&code_challenge=abc";
        let (_, error) = redirect(refuse(query).await);

        assert!(error.to_string().contains("`code_challenge_method` is required"));
    }

    #[tokio::test]
    async fn a_downgrade_to_plain_is_refused() {
        let query = "response_type=code&client_id=spa&redirect_uri=https://a.test/cb\
                     &code_challenge=abc&code_challenge_method=plain";
        let (_, error) = redirect(refuse(query).await);

        assert!(error.to_string().contains("not accepted"), "got {error}");
        assert!(error.to_string().contains("S256"));
    }

    #[tokio::test]
    async fn an_unknown_challenge_method_is_refused_rather_than_ignored() {
        let query = "response_type=code&client_id=spa&redirect_uri=https://a.test/cb\
                     &code_challenge=abc&code_challenge_method=S512";
        let (_, error) = redirect(refuse(query).await);

        assert!(error.to_string().contains("S512"));
    }

    #[tokio::test]
    async fn the_implicit_flow_is_refused_with_a_reason() {
        let query = "response_type=token&client_id=spa&redirect_uri=https://a.test/cb\
                     &code_challenge=abc&code_challenge_method=S256";
        let (_, error) = redirect(refuse(query).await);

        assert_eq!(error.code, OAuthErrorCode::UnsupportedResponseType);
        assert!(error.to_string().contains("OAuth 2.1"));
    }

    #[tokio::test]
    async fn a_scope_beyond_what_the_client_registered_is_refused() {
        let (_, error) = redirect(refuse(&format!("{VALID}&scope=read admin")).await);

        assert_eq!(error.code, OAuthErrorCode::InvalidScope);
        assert!(error.to_string().contains("admin"));
        assert!(!error.to_string().contains("read"), "the message names only what was refused");
    }

    #[tokio::test]
    async fn a_client_registered_for_nothing_may_ask_for_nothing() {
        let query = "response_type=code&client_id=bare&redirect_uri=https://b.test/cb\
                     &code_challenge=abc&code_challenge_method=S256";

        assert!(validate(&server(), &params(query)).await.is_ok());
        assert!(validate(&server(), &params(&format!("{query}&scope=read"))).await.is_err());
    }

    #[tokio::test]
    async fn a_redirected_error_carries_the_state_back() {
        // Without it the client cannot match the response to the request, and
        // its CSRF check on `state` has nothing to check.
        let (_, error) = redirect(
            refuse("response_type=token&client_id=spa&redirect_uri=https://a.test/cb&state=xyz")
                .await,
        );

        assert_eq!(error.state.as_deref(), Some("xyz"));
        assert!(error.to_query().contains("state=xyz"));
    }
}
