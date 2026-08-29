//! The two handlers every social login needs, so an application does not write
//! them again for each provider.
//!
//! ```ignore
//! App::new().plugin(
//!     Socialite::new()
//!         .provider(google_client)
//!         .provider(github_client)
//!         .on_login(|user, req| async move {
//!             let account = Account::link(&user).await?;
//!             req.auth().login(&account);
//!             Response::see_other("/dashboard")
//!         }),
//! )
//! ```
//!
//! That registers `GET /auth/{provider}/redirect` and
//! `GET /auth/{provider}/callback`. Attached with one explicit line like every
//! other package — nothing is discovered, and a provider that is not listed
//! here is a 404 rather than a route that half exists.

use crate::client::OAuthClient;
use crate::state::{SessionState, StateGuard};
use crate::user::SocialUser;
use rustlavel_auth::{AppKey, AuthExt, SessionExt};
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::plugin::{Plugin, Setup};
use rustlavel_http::response::IntoResponse;
use rustlavel_http::{Request, Response, Status};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

/// What happens once a provider has vouched for somebody.
type OnLogin = Arc<dyn Fn(SocialUser, Request) -> BoxFuture<Response> + Send + Sync>;

/// Social login for one or more providers, mounted under a shared prefix.
#[derive(Clone)]
pub struct Socialite {
    prefix: String,
    providers: BTreeMap<String, OAuthClient>,
    /// Present when the application asked for the stateless mode.
    sealed_with: Option<AppKey>,
    on_login: OnLogin,
}

impl Socialite {
    pub fn new() -> Socialite {
        Socialite {
            prefix: "/auth".to_string(),
            providers: BTreeMap::new(),
            sealed_with: None,
            on_login: Arc::new(|user, request| Box::pin(default_login(user, request))),
        }
    }

    /// Add a configured client, keyed by its provider's name.
    pub fn provider(mut self, client: OAuthClient) -> Socialite {
        self.providers.insert(client.provider().name.clone(), client);
        self
    }

    /// Mount somewhere other than `/auth`.
    pub fn at(mut self, prefix: impl Into<String>) -> Socialite {
        self.prefix = prefix.into().trim_end_matches('/').to_string();
        self
    }

    /// Keep the pending flow in a signed, expiring `state` rather than in the
    /// session — for a fleet with no shared session store.
    ///
    /// Read the trade-off on [`crate::state::SealedState`] before reaching for
    /// this: a sealed state cannot be spent, so it verifies more than once
    /// within its lifetime.
    pub fn stateless(mut self, key: AppKey) -> Socialite {
        self.sealed_with = Some(key);
        self
    }

    /// What to do with the user the provider vouched for: find or create the
    /// account, log them in, and say where to go next.
    pub fn on_login<F, Fut>(mut self, action: F) -> Socialite
    where
        F: Fn(SocialUser, Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response> + Send + 'static,
    {
        self.on_login = Arc::new(move |user, request| Box::pin(action(user, request)));
        self
    }

    /// The guard this request's flow should use.
    ///
    /// Namespaced by provider, so opening "sign in with Google" and "sign in
    /// with GitHub" in two tabs leaves both completable rather than only the
    /// one that was clicked second.
    fn guard(&self, request: &Request, provider: &str) -> Result<StateGuard, Response> {
        if let Some(key) = &self.sealed_with {
            return Ok(StateGuard::sealed(key));
        }

        match request.try_session() {
            Some(session) => Ok(StateGuard::Session(
                SessionState::new(session.clone()).keyed(format!("_oauth_state.{provider}")),
            )),
            // Neither mode configured and no session to fall back on. Say which
            // of the two lines is missing rather than 500-ing on a panic from
            // deep inside `req.session()`.
            None => Err(misconfigured(
                "social login needs somewhere to keep the `state` that stops login CSRF. Add \
                 the session middleware (`router.middleware(SessionManager::from_config(…))`) \
                 before this plugin, or choose the stateless mode with \
                 `Socialite::stateless(app_key)`.",
            )),
        }
    }

    /// The client named in the path, and the name it was looked up by.
    fn client<'r>(&self, request: &'r Request) -> Result<(&OAuthClient, &'r str), Response> {
        let name = request.param("provider").unwrap_or_default();
        match self.providers.get(name) {
            Some(client) => Ok((client, name)),
            // Not 500: an unknown provider in the path is a request for
            // something this application does not offer.
            None => Err(Response::new(Status::NOT_FOUND)
                .with_text(format!("`{name}` is not a configured sign-in provider"))),
        }
    }
}

impl Default for Socialite {
    fn default() -> Socialite {
        Socialite::new()
    }
}

impl std::fmt::Debug for Socialite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socialite")
            .field("prefix", &self.prefix)
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("stateless", &self.sealed_with.is_some())
            .finish()
    }
}

impl Plugin for Socialite {
    fn name(&self) -> &'static str {
        "oauth"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        let socialite = Arc::new(*self);

        let redirect = Arc::clone(&socialite);
        setup
            .router
            .get(&format!("{}/{{provider}}/redirect", socialite.prefix), move |request: Request| {
                let socialite = Arc::clone(&redirect);
                async move { socialite.redirect(request).await }
            })
            .name("oauth.redirect");

        let callback = Arc::clone(&socialite);
        setup
            .router
            .get(&format!("{}/{{provider}}/callback", socialite.prefix), move |request: Request| {
                let socialite = Arc::clone(&callback);
                async move { socialite.callback(request).await }
            })
            .name("oauth.callback");
    }
}

impl Socialite {
    /// Leg one: remember the state, then send the visitor to the provider.
    async fn redirect(&self, request: Request) -> Response {
        let (client, name) = match self.client(&request) {
            Ok(found) => found,
            Err(response) => return response,
        };
        let guard = match self.guard(&request, name) {
            Ok(guard) => guard,
            Err(response) => return response,
        };

        match client.begin(&guard) {
            Ok(start) => Response::see_other(start.into_url()),
            Err(error) => error.into_response(),
        }
    }

    /// Leg two: check the state, exchange the code, load the profile.
    async fn callback(&self, request: Request) -> Response {
        let (client, name) = match self.client(&request) {
            Ok(found) => found,
            Err(response) => return response,
        };
        let guard = match self.guard(&request, name) {
            Ok(guard) => guard,
            Err(response) => return response,
        };

        // `target` is the path plus query; everything before `?` is not ours.
        let query = request.target().split_once('?').map(|(_, query)| query).unwrap_or_default();

        let user = match client.callback(query, &guard).await {
            Ok(token) => match client.user(&token).await {
                Ok(user) => user,
                Err(error) => return error.into_response(),
            },
            Err(error) => return error.into_response(),
        };

        (self.on_login)(user, request).await
    }
}

/// What happens when the application did not say.
///
/// Logs the visitor in under `provider:id` and sends them home. That is the
/// right shape for a prototype and the wrong one for an application with its
/// own users table — which is why the identifier is provider-qualified rather
/// than the provider's bare id: an application that later joins these to real
/// accounts is not left with GitHub's `1` and GitLab's `1` in the same column.
async fn default_login(user: SocialUser, request: Request) -> Response {
    match request.try_auth() {
        Some(guard) => {
            guard.login(&user);
            Response::see_other("/")
        }
        None => misconfigured(
            "the default `on_login` logs the visitor into the session, and there is no session \
             middleware installed. Add it, or give `Socialite::on_login` a handler of your own.",
        ),
    }
}

/// A 500 that names the missing line of `main.rs`.
///
/// A misconfiguration is not the visitor's fault and not something they can
/// act on, so the page says nothing; the developer gets the sentence in the
/// log, where the fix belongs.
fn misconfigured(message: &str) -> Response {
    rustlavel_core::error!("{message}");
    Response::new(Status::INTERNAL_ERROR).with_text("Sign-in is not available.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use crate::url;
    use rustlavel_auth::{MemoryStore, SessionManager};
    use rustlavel_client::fake::{Fake, FakeResponse};
    use rustlavel_client::Client;
    use rustlavel_core::{Config, Context, Json};
    use rustlavel_http::router::Router;
    use rustlavel_http::TestClient;

    fn key() -> AppKey {
        AppKey::from_base64(&AppKey::generate()).unwrap()
    }

    /// A GitHub client whose token and userinfo endpoints both answer.
    fn github() -> OAuthClient {
        let fake = Fake::new()
            .on(
                "github.com/login/oauth/access_token",
                FakeResponse::json(Json::parse(r#"{"access_token":"at-1"}"#).unwrap()),
            )
            .on(
                "api.github.com/user",
                FakeResponse::json(
                    Json::parse(r#"{"id":7,"login":"ada","name":"Ada"}"#).unwrap(),
                ),
            );

        OAuthClient::new(Provider::github())
            .credentials("id", "secret")
            .redirect_uri("https://app.test/auth/github/callback")
            .using(Client::new().faking(fake))
    }

    /// A Google client whose token and userinfo endpoints both answer.
    fn google() -> OAuthClient {
        let fake = Fake::new()
            .on(
                "oauth2.googleapis.com/token",
                FakeResponse::json(Json::parse(r#"{"access_token":"at-g"}"#).unwrap()),
            )
            .on(
                "openidconnect.googleapis.com/v1/userinfo",
                FakeResponse::json(Json::parse(r#"{"sub":"g-9","name":"Ada"}"#).unwrap()),
            );

        OAuthClient::new(Provider::google())
            .credentials("id", "secret")
            .redirect_uri("https://app.test/auth/google/callback")
            .using(Client::new().faking(fake))
    }

    /// The `state` out of a redirect response's `Location`.
    fn state_from(location: &str) -> String {
        url::form_decode(location.split_once('?').expect("a query string").1)
            .into_iter()
            .find(|(name, _)| name == "state")
            .map(|(_, value)| value)
            .expect("a state parameter")
    }

    /// Register the plugin the way an application would.
    fn mount(socialite: Socialite, with_session: bool) -> TestClient {
        let mut router = Router::new();
        let config = Config::with_defaults();
        config.set("app.key", AppKey::generate());

        if with_session {
            router.middleware(
                SessionManager::from_config(&config, MemoryStore::new()).expect("a key is set"),
            );
        }

        let mut context = Some(Context::builder());
        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };
        Box::new(socialite).register(&mut setup);

        TestClient::new(router).with_context(context.expect("the builder survives setup").build())
    }

    #[tokio::test]
    async fn the_redirect_route_sends_the_visitor_to_the_provider() {
        let client = mount(Socialite::new().provider(github()), true);

        let response = client.get("/auth/github/redirect").await.assert_status(303);
        let location = response.header("location").unwrap();

        assert!(location.starts_with("https://github.com/login/oauth/authorize?"));
        let query = url::form_decode(location.split_once('?').unwrap().1);
        assert!(query.iter().any(|(name, _)| name == "state"));
        assert!(query.iter().any(|(name, _)| name == "code_challenge"));
    }

    #[tokio::test]
    async fn a_provider_that_was_not_configured_is_a_404_and_not_a_crash() {
        let client = mount(Socialite::new().provider(github()), true);

        client.get("/auth/facebook/redirect").await.assert_not_found();
    }

    #[tokio::test]
    async fn the_two_legs_join_up_through_the_session() {
        // The state issued by the redirect is the one the callback needs, and
        // the TestClient carries the session cookie between them the way a
        // browser does.
        let client = mount(Socialite::new().provider(github()), true);

        let location =
            client.get("/auth/github/redirect").await.header("location").unwrap().to_string();
        let state = state_from(&location);

        client
            .get(&format!("/auth/github/callback?code=c&state={}", url::encode(&state)))
            .await
            .assert_redirect("/");
    }

    #[tokio::test]
    async fn a_callback_that_never_started_here_is_refused() {
        // The same login-CSRF hole, reached through the routes rather than the
        // client: a bare callback URL an attacker mailed to somebody.
        let client = mount(Socialite::new().provider(github()), true);

        client
            .get("/auth/github/callback?code=attackers-code&state=made-up")
            .await
            .assert_status(400);
    }

    #[tokio::test]
    async fn the_user_reaches_the_applications_own_handler() {
        let seen: Arc<std::sync::Mutex<Option<SocialUser>>> = Arc::default();
        let recorder = Arc::clone(&seen);

        let client = mount(
            Socialite::new().provider(github()).on_login(move |user, _request| {
                let recorder = Arc::clone(&recorder);
                async move {
                    *recorder.lock().expect("not poisoned") = Some(user);
                    Response::see_other("/dashboard")
                }
            }),
            true,
        );

        let location =
            client.get("/auth/github/redirect").await.header("location").unwrap().to_string();
        let state = state_from(&location);

        client
            .get(&format!("/auth/github/callback?code=c&state={}", url::encode(&state)))
            .await
            .assert_redirect("/dashboard");

        let user = seen.lock().expect("not poisoned").clone().expect("on_login ran");
        assert_eq!(user.qualified_id(), "github:7");
        assert_eq!(user.name.as_deref(), Some("Ada"));
    }

    #[tokio::test]
    async fn the_stateless_mode_needs_no_session_middleware_at_all() {
        let client = mount(
            Socialite::new()
                .provider(github())
                .stateless(key())
                .on_login(|_user, _request| async { Response::see_other("/done") }),
            false,
        );

        let location =
            client.get("/auth/github/redirect").await.header("location").unwrap().to_string();
        let state = state_from(&location);

        client.clear_cookies();
        client
            .get(&format!("/auth/github/callback?code=c&state={}", url::encode(&state)))
            .await
            .assert_redirect("/done");
    }

    #[tokio::test]
    async fn neither_a_session_nor_a_key_is_a_configuration_error_that_says_which_line_is_missing()
    {
        let client = mount(Socialite::new().provider(github()), false);

        client.get("/auth/github/redirect").await.assert_status(500);
    }

    #[tokio::test]
    async fn two_providers_opened_in_two_tabs_do_not_evict_each_other() {
        // One session key for every provider would mean whichever button was
        // clicked second is the only flow that can finish.
        let client = mount(Socialite::new().provider(github()).provider(google()), true);

        let github_state = state_from(
            client.get("/auth/github/redirect").await.header("location").unwrap(),
        );
        let google_state = state_from(
            client.get("/auth/google/redirect").await.header("location").unwrap(),
        );

        client
            .get(&format!("/auth/github/callback?code=c&state={}", url::encode(&github_state)))
            .await
            .assert_redirect("/");
        client
            .get(&format!("/auth/google/callback?code=c&state={}", url::encode(&google_state)))
            .await
            .assert_redirect("/");
    }

    #[tokio::test]
    async fn a_state_issued_for_one_provider_does_not_open_another_ones_callback() {
        let client = mount(Socialite::new().provider(github()).provider(google()), true);

        let github_state = state_from(
            client.get("/auth/github/redirect").await.header("location").unwrap(),
        );

        client
            .get(&format!("/auth/google/callback?code=c&state={}", url::encode(&github_state)))
            .await
            .assert_status(400);
    }

    #[tokio::test]
    async fn the_mount_point_can_be_moved() {
        let client = mount(Socialite::new().at("/social/").provider(github()), true);

        client.get("/social/github/redirect").await.assert_status(303);
        client.get("/auth/github/redirect").await.assert_not_found();
    }

    #[test]
    fn debug_lists_the_providers_and_says_nothing_about_their_secrets() {
        let printed = format!(
            "{:?}",
            Socialite::new().provider(
                OAuthClient::new(Provider::github()).credentials("id", "TOP-SECRET")
            )
        );

        assert!(printed.contains("github"));
        assert!(!printed.contains("TOP-SECRET"));
    }
}
