//! Turning the provider on.
//!
//! ```ignore
//! let clients = MemoryClientStore::new().with(
//!     Client::confidential("checkout", &secret)
//!         .redirect_uri("https://checkout.test/callback")
//!         .scopes(Scopes::of(["orders.read"])),
//! );
//!
//! App::new()?
//!     .routes(routes::web::routes)
//!     .plugin(OAuthProvider::new(AuthorizationServer::new(clients)))
//!     .serve()
//!     .await
//! ```
//!
//! One line, no auto-discovery. The plugin mounts the five endpoints and the
//! discovery document, and registers the [`AuthorizationServer`] as application
//! state so the application's own handlers can reach it — to list a user's
//! authorised applications, or to revoke one.
//!
//! # The consent screen needs a session in front of it
//!
//! `/oauth/authorize` reads `Identity` from the request, which the application
//! puts there with `SessionManager` and `Authenticate`. This crate registers
//! neither: it does not own the application's session store, and silently
//! installing one would be exactly the runtime magic the framework refuses.
//!
//! It *does* register `Csrf` on the two `/authorize` routes, because a consent
//! form nothing checks is forgeable — an attacker's page auto-submits "approve"
//! in the victim's logged-in browser and collects an authorization code for its
//! own client. That is not a policy an application should have to remember to
//! opt into, so it is not one this crate leaves to the application. The
//! consequence is that the consent screen fails closed with a clear message
//! when no session middleware is registered, rather than quietly accepting a
//! forged post. The token, revoke and introspect endpoints are deliberately
//! outside that group: they are called by servers with client credentials, and
//! CSRF there would reject every legitimate caller.

use crate::server::AuthorizationServer;
use rustlavel_core::Config;
use rustlavel_http::plugin::{Plugin, Setup};

pub struct OAuthProvider {
    server: AuthorizationServer,
}

impl OAuthProvider {
    pub fn new(server: AuthorizationServer) -> OAuthProvider {
        OAuthProvider { server }
    }

    pub fn server(&self) -> &AuthorizationServer {
        &self.server
    }

    /// Complain about a configuration that will leak credentials.
    ///
    /// A warning rather than a refusal: this is a production feature, and a
    /// server that will not boot because someone is testing behind a TLS-
    /// terminating proxy helps nobody. But an issuer of `http://localhost` in
    /// production means every redirect and every token response is going
    /// somewhere in clear text, and that is worth saying loudly.
    fn warn_about(&self, config: &Config) {
        let issuer = &self.server.settings().issuer;

        if config.is_production() && issuer.starts_with("http://") {
            rustlavel_core::warn!(
                "oauth: the issuer is {issuer}, which is plain http. Authorization codes and \
                 access tokens will travel in clear text, and the redirect back from a browser \
                 will too. Set `oauth.issuer` (or `app.url`) to the https origin clients reach."
            );
        }

        if issuer.trim().is_empty() {
            rustlavel_core::warn!(
                "oauth: no issuer is configured, so the discovery document will advertise \
                 relative endpoints that no client can use. Set `oauth.issuer`."
            );
        }
    }
}

impl Plugin for OAuthProvider {
    fn name(&self) -> &'static str {
        "oauth-provider"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        self.warn_about(setup.config);
        self.server.install(setup.router);

        rustlavel_core::info!(
            "oauth: authorization server mounted at {} — {} needs the application's session \
             middleware in front of it",
            self.server.settings().mount,
            self.server.settings().endpoint("authorize"),
        );

        // So an application handler can revoke a grant, or list what a user has
        // authorised, without building a second server.
        setup.state(self.server.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, MemoryClientStore};
    use crate::code::AuthorizationCode;
    use crate::resource::RequireToken;
    use crate::server::DISCOVERY_PATH;
    use rustlavel_auth::guard::Identity;
    use rustlavel_core::Json;
    use rustlavel_http::{Method, Request, Response, Router, TestClient};
    use rustlavel_oauth::{ChallengeMethod, Pkce, Scopes, url};

    fn clients() -> MemoryClientStore {
        MemoryClientStore::new()
            .with(
                Client::confidential("web", "s3cret")
                    .named("Checkout")
                    .redirect_uri("https://a.test/cb")
                    .scopes(Scopes::of(["read", "write"])),
            )
            .with(
                Client::public("first")
                    .named("Our App")
                    .redirect_uri("https://b.test/cb")
                    .scopes(Scopes::of(["read"]))
                    .first_party(),
            )
    }

    fn server() -> AuthorizationServer {
        AuthorizationServer::new(clients()).issued_by("https://accounts.test")
    }

    /// A router with the endpoints mounted, a session so `Csrf` has somewhere
    /// to keep its token, and a stand-in for the application's `auth`
    /// middleware that signs everybody in as user 7.
    fn mounted(server: &AuthorizationServer, signed_in: bool) -> TestClient {
        let mut router = Router::new();
        let key = rustlavel_auth::AppKey::from_base64(&rustlavel_auth::AppKey::generate())
            .expect("a generated key parses");
        router.middleware(rustlavel_auth::SessionManager::new(
            &key,
            rustlavel_auth::MemoryStore::new(),
        ));
        if signed_in {
            router.middleware(|mut request: Request, next: rustlavel_http::Next| async move {
                request.extend(Identity::new("7"));
                next.run(request).await
            });
        }
        server.install(&mut router);

        // A guarded API route beside the endpoints, so a test can show that a
        // token this server issued opens it — and that a revoked one does not.
        let guard = RequireToken::new(server).scope("read");
        router.group("/api", |r| {
            r.middleware(guard);
            r.get("/me", |request: Request| async move {
                use crate::resource::BearerExt;
                let claims = request.oauth().expect("claims");
                Response::json(Json::object([
                    ("client", Json::String(claims.client_id.clone())),
                    ("user", Json::String(claims.user_id.clone().unwrap_or_default())),
                ]))
            });
        });
        TestClient::new(router)
    }

    /// The CSRF token out of a rendered consent screen.
    ///
    /// Read from the HTML rather than reached for behind the scenes, so these
    /// tests approve a grant exactly the way a browser does — through the form
    /// that was served, with the token that was in it.
    fn csrf_token(html: &str) -> String {
        let field = html
            .split_once(r#"name="_token" value=""#)
            .map(|(_, rest)| rest)
            .unwrap_or_else(|| panic!("the consent screen rendered no _token field: {html}"));
        field.split_once('"').expect("an unterminated _token value").0.to_string()
    }

    /// Render the consent screen and come back with the token it carried.
    ///
    /// The GET is what establishes the session, so a test that posts without
    /// one first is a test of a browser that could not have got the form.
    async fn shown_consent(client: &TestClient, url: &str) -> String {
        csrf_token(&client.get(url).await.body())
    }

    fn authorize_url(client_id: &str, redirect: &str, pkce: &Pkce, extra: &str) -> String {
        format!(
            "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}\
             &code_challenge={}&code_challenge_method=S256{extra}",
            url::encode(redirect),
            url::encode(&pkce.challenge()),
        )
    }

    /// The `code` out of a `302` back to the client.
    fn code_from(location: &str) -> String {
        let (_, query) = location.split_once('?').expect("a query string");
        url::form_decode(query)
            .into_iter()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value)
            .expect("a code")
    }

    #[test]
    fn the_plugin_mounts_every_endpoint() {
        let mut router = Router::new();
        server().install(&mut router);

        let patterns: Vec<(String, String)> = router
            .routes()
            .iter()
            .map(|route| (route.method.as_str().to_string(), route.pattern.clone()))
            .collect();

        for expected in [
            ("GET", "/oauth/authorize"),
            ("POST", "/oauth/authorize"),
            ("POST", "/oauth/token"),
            ("POST", "/oauth/revoke"),
            ("POST", "/oauth/introspect"),
            ("GET", DISCOVERY_PATH),
        ] {
            assert!(
                patterns.iter().any(|(m, p)| m == expected.0 && p == expected.1),
                "{expected:?} was not mounted; got {patterns:?}"
            );
        }
    }

    #[test]
    fn a_custom_mount_point_moves_the_endpoints_but_not_the_discovery_document() {
        // RFC 8414 §3 fixes the well-known path; that is the point of it.
        let mut router = Router::new();
        server().at("/id").install(&mut router);

        let patterns: Vec<&str> =
            router.routes().iter().map(|route| route.pattern.as_str()).collect();

        assert!(patterns.contains(&"/id/token"));
        assert!(patterns.contains(&DISCOVERY_PATH));
    }

    #[tokio::test]
    async fn the_discovery_document_is_served() {
        let client = mounted(&server(), false);

        client
            .get(DISCOVERY_PATH)
            .await
            .assert_ok()
            .assert_json("issuer", "https://accounts.test")
            .assert_json("token_endpoint", "https://accounts.test/oauth/token");
    }

    #[tokio::test]
    async fn the_whole_flow_works_end_to_end() {
        let server = server();
        let client = mounted(&server, true);
        let pkce = Pkce::generate();

        // The consent screen, naming the client and the scopes.
        let consent = client
            .get(&authorize_url("web", "https://a.test/cb", &pkce, "&scope=read&state=xyz"))
            .await
            .assert_ok()
            .assert_see("Checkout")
            .assert_see("read");
        assert!(consent.body().contains(r#"value="xyz""#), "the state is carried forward");
        let token = csrf_token(&consent.body());

        // Approving it.
        let approved = client
            .post(
                "/oauth/authorize",
                &[
                    ("response_type", "code"),
                    ("client_id", "web"),
                    ("redirect_uri", "https://a.test/cb"),
                    ("scope", "read"),
                    ("state", "xyz"),
                    ("code_challenge", &pkce.challenge()),
                    ("code_challenge_method", "S256"),
                    ("approve", "yes"),
                    ("_token", &token),
                ],
            )
            .await
            .assert_status(302);
        let location = approved.header("location").expect("a redirect").to_string();
        assert!(location.starts_with("https://a.test/cb?code="));
        assert!(location.contains("state=xyz"));

        // Exchanging the code.
        let code = code_from(&location);
        let token = client
            .post(
                "/oauth/token",
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("redirect_uri", "https://a.test/cb"),
                    ("code_verifier", pkce.verifier()),
                    ("client_id", "web"),
                    ("client_secret", "s3cret"),
                ],
            )
            .await
            .assert_ok();
        assert_eq!(token.header("cache-control"), Some("no-store"), "RFC 6749 §5.1");

        let body = token.json();
        let access = body.get("access_token").and_then(Json::as_str).expect("a token").to_string();
        assert_eq!(body.get("token_type").and_then(Json::as_str), Some("Bearer"));

        // And the token opens the guarded route.
        client
            .send(
                Request::new(Method::Get, "/api/me")
                    .with_header("authorization", format!("Bearer {access}")),
            )
            .await
            .assert_ok()
            .assert_json("user", "7");
    }

    #[tokio::test]
    async fn a_first_party_client_with_a_prior_grant_skips_the_screen() {
        let server = server();
        let client = mounted(&server, true);
        server
            .consent()
            .record(crate::consent::Grant::new("first", "7", Scopes::of(["read"]), server.now()))
            .await
            .expect("recorded");
        let pkce = Pkce::generate();

        let response = client
            .get(&authorize_url("first", "https://b.test/cb", &pkce, "&scope=read"))
            .await
            .assert_status(302);

        assert!(response.header("location").expect("location").starts_with("https://b.test/cb?code="));
    }

    #[tokio::test]
    async fn a_first_party_client_asking_for_more_than_it_was_granted_still_asks() {
        // Otherwise the first, modest consent quietly becomes permission for
        // everything the client is registered for.
        let server = AuthorizationServer::new(
            MemoryClientStore::new().with(
                Client::public("first")
                    .named("Our App")
                    .redirect_uri("https://b.test/cb")
                    .scopes(Scopes::of(["read", "write"]))
                    .first_party(),
            ),
        );
        let client = mounted(&server, true);
        server
            .consent()
            .record(crate::consent::Grant::new("first", "7", Scopes::of(["read"]), server.now()))
            .await
            .expect("recorded");
        let pkce = Pkce::generate();

        client
            .get(&authorize_url("first", "https://b.test/cb", &pkce, "&scope=read write"))
            .await
            .assert_ok()
            .assert_see("Authorize");
    }

    #[tokio::test]
    async fn a_third_party_client_is_asked_every_time() {
        let server = server();
        let client = mounted(&server, true);
        server
            .consent()
            .record(crate::consent::Grant::new("web", "7", Scopes::of(["read"]), server.now()))
            .await
            .expect("recorded");
        let pkce = Pkce::generate();

        client
            .get(&authorize_url("web", "https://a.test/cb", &pkce, "&scope=read"))
            .await
            .assert_ok()
            .assert_see("Checkout");
    }

    #[tokio::test]
    async fn declining_sends_access_denied_back_to_the_client() {
        let client = mounted(&server(), true);
        let pkce = Pkce::generate();

        let url = authorize_url("web", "https://a.test/cb", &pkce, "&state=xyz");
        let token = shown_consent(&client, &url).await;

        let response = client
            .post(
                "/oauth/authorize",
                &[
                    ("response_type", "code"),
                    ("client_id", "web"),
                    ("redirect_uri", "https://a.test/cb"),
                    ("code_challenge", &pkce.challenge()),
                    ("code_challenge_method", "S256"),
                    ("state", "xyz"),
                    ("approve", "no"),
                    ("_token", &token),
                ],
            )
            .await
            .assert_status(302);

        let location = response.header("location").expect("location");
        assert!(location.contains("error=access_denied"));
        assert!(location.contains("state=xyz"));
        assert!(!location.contains("code="));
    }

    #[tokio::test]
    async fn a_consent_form_forged_by_another_site_is_refused() {
        // The attack: a page on evil.test auto-submits this exact form in the
        // victim's logged-in browser. Every field is one an attacker knows or
        // chooses, so nothing in the request itself gives them away — the only
        // thing they cannot supply is the token from a form this server served.
        // Without the check, the attacker walks away with an authorization code
        // for their own client, against the victim's account.
        let client = mounted(&server(), true);
        let pkce = Pkce::generate();

        let forged = client
            .post(
                "/oauth/authorize",
                &[
                    ("response_type", "code"),
                    ("client_id", "web"),
                    ("redirect_uri", "https://a.test/cb"),
                    ("code_challenge", &pkce.challenge()),
                    ("code_challenge_method", "S256"),
                    ("approve", "yes"),
                ],
            )
            .await
            .assert_status(419);

        assert_eq!(forged.header("location"), None, "no code may be handed out");
    }

    #[tokio::test]
    async fn a_stale_token_from_another_session_is_refused() {
        // A token is only good for the session it was minted in, so one lifted
        // from another user's page does not work here either.
        let server = server();
        let victim = mounted(&server, true);
        let attacker = mounted(&server, true);
        let pkce = Pkce::generate();

        let url = authorize_url("web", "https://a.test/cb", &pkce, "");
        let elsewhere = shown_consent(&attacker, &url).await;
        // Establish the victim's own session, so this is a token mismatch
        // rather than a missing session.
        let _ = shown_consent(&victim, &url).await;

        victim
            .post(
                "/oauth/authorize",
                &[
                    ("response_type", "code"),
                    ("client_id", "web"),
                    ("redirect_uri", "https://a.test/cb"),
                    ("code_challenge", &pkce.challenge()),
                    ("code_challenge_method", "S256"),
                    ("approve", "yes"),
                    ("_token", &elsewhere),
                ],
            )
            .await
            .assert_status(419);
    }

    #[tokio::test]
    async fn the_machine_endpoints_are_not_behind_csrf() {
        // The token endpoint is called by a server with client credentials, not
        // by a browser with a cookie. Putting CSRF in front of it would reject
        // every legitimate caller, so the protection is scoped to the one
        // endpoint a browser actually posts to.
        let client = mounted(&server(), false);

        // No `_token`, and the request still reaches the protocol logic — the
        // error is about the grant, not about a missing CSRF token.
        client
            .post("/oauth/token", &[("grant_type", "authorization_code")])
            .await
            .assert_status(401)
            .assert_json("error", "invalid_client");
    }

    #[tokio::test]
    async fn a_tampered_hidden_field_does_not_survive_the_post() {
        // The consent form's fields are the browser's copy of the request, not
        // the server's memory of it, so the POST re-validates all of them.
        let client = mounted(&server(), true);
        let pkce = Pkce::generate();

        let url = authorize_url("web", "https://a.test/cb", &pkce, "");
        let token = shown_consent(&client, &url).await;

        let response = client
            .post(
                "/oauth/authorize",
                &[
                    ("response_type", "code"),
                    ("client_id", "web"),
                    ("redirect_uri", "https://evil.test/cb"),
                    ("code_challenge", &pkce.challenge()),
                    ("code_challenge_method", "S256"),
                    ("approve", "yes"),
                    ("_token", &token),
                ],
            )
            .await
            .assert_status(400);

        assert_eq!(response.header("location"), None, "nothing may be redirected");
        response.assert_see("not one this client registered");
    }

    #[tokio::test]
    async fn an_anonymous_visitor_is_sent_to_the_login_page_first() {
        let client = mounted(&server().login_at("/sign-in"), false);
        let pkce = Pkce::generate();

        let response = client
            .get(&authorize_url("web", "https://a.test/cb", &pkce, ""))
            .await
            .assert_status(303);

        let location = response.header("location").expect("location");
        assert!(location.starts_with("/sign-in?next="), "got {location}");
        assert!(location.contains("oauth"), "and it comes back here afterwards");
    }

    #[tokio::test]
    async fn redirect_uri_substitution_renders_here_and_redirects_nowhere() {
        let client = mounted(&server(), true);
        let pkce = Pkce::generate();

        let response = client
            .get(&authorize_url("web", "https://evil.test/cb", &pkce, ""))
            .await
            .assert_status(400);

        assert_eq!(response.header("location"), None, "an error must not be redirected");
        response.assert_see("Nothing was sent back");
    }

    #[tokio::test]
    async fn a_pkce_downgrade_to_plain_is_refused() {
        let client = mounted(&server(), true);

        let response = client
            .get(
                "/oauth/authorize?response_type=code&client_id=web\
                 &redirect_uri=https://a.test/cb&code_challenge=abc&code_challenge_method=plain",
            )
            .await
            .assert_status(302);

        let location = response.header("location").expect("location");
        assert!(location.starts_with("https://a.test/cb?error=invalid_request"));
    }

    #[tokio::test]
    async fn a_code_replayed_through_the_endpoint_revokes_the_first_exchange() {
        let server = server();
        let client = mounted(&server, true);
        let pkce = Pkce::generate();

        let record = AuthorizationCode::issued("the-code", server.now(), 60)
            .for_client("web")
            .for_user("7")
            .redirecting_to("https://a.test/cb")
            .granting(Scopes::of(["read"]))
            .challenged(pkce.challenge(), ChallengeMethod::S256);
        server.codes().store(record).await.expect("stored");

        let grant = [
            ("grant_type", "authorization_code"),
            ("code", "the-code"),
            ("redirect_uri", "https://a.test/cb"),
            ("code_verifier", pkce.verifier()),
            ("client_id", "web"),
            ("client_secret", "s3cret"),
        ];
        let exchange = || client.post("/oauth/token", &grant);

        let first = exchange().await.assert_ok().json();
        let access =
            first.get("access_token").and_then(Json::as_str).expect("a token").to_string();

        exchange().await.assert_status(400).assert_see("already been used");

        client
            .send(
                Request::new(Method::Get, "/api/me")
                    .with_header("authorization", format!("Bearer {access}")),
            )
            .await
            .assert_status(401);
    }

    #[tokio::test]
    async fn introspection_without_authentication_is_refused_through_the_endpoint() {
        let server = server();
        let client = mounted(&server, false);
        let token = server
            .issue("web", Some("7"), &Scopes::of(["read"]), "family-1", false)
            .await
            .expect("issued");

        client
            .post("/oauth/introspect", &[("token", &token.access_token)])
            .await
            .assert_status(401)
            .assert_dont_see("active");

        // And with credentials it answers.
        client
            .post(
                "/oauth/introspect",
                &[
                    ("token", &token.access_token),
                    ("client_id", "web"),
                    ("client_secret", "s3cret"),
                ],
            )
            .await
            .assert_ok()
            .assert_see(r#""active":true"#);
    }

    #[tokio::test]
    async fn revocation_through_the_endpoint_stops_a_token_working() {
        let server = server();
        let client = mounted(&server, false);
        let token = server
            .issue("web", Some("7"), &Scopes::of(["read"]), "family-1", false)
            .await
            .expect("issued");

        client
            .post(
                "/oauth/revoke",
                &[
                    ("token", &token.access_token),
                    ("client_id", "web"),
                    ("client_secret", "s3cret"),
                ],
            )
            .await
            .assert_ok();

        client
            .send(
                Request::new(Method::Get, "/api/me")
                    .with_header("authorization", format!("Bearer {}", token.access_token)),
            )
            .await
            .assert_status(401);
    }

    #[tokio::test]
    async fn the_token_endpoint_refuses_a_get() {
        // Credentials in a query string are credentials in an access log.
        mounted(&server(), false).get("/oauth/token").await.assert_status(405);
    }
}
