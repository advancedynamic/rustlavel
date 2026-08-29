//! Who is logged in: the guard, the `auth` middleware, and the `guest` one.
//!
//! The session already survives between requests, so authentication is just one
//! more value in it — the user's identifier — plus the discipline around
//! putting it there and taking it out.
//!
//! ```ignore
//! router.middleware(SessionManager::from_config(&config, store)?);
//!
//! router.post("/login", |mut req: Request| async move {
//!     let user = User::by_email(&req.input("email").unwrap_or_default()).await?;
//!     if verify_password(&req.input("password").unwrap_or_default(), &user.password) {
//!         req.auth().login(&user);
//!         return Ok(Response::see_other("/dashboard"));
//!     }
//!     Ok(Response::see_other("/login"))
//! });
//!
//! router.group("/dashboard", |r| {
//!     r.middleware(Authenticate::from_config(&config));
//!     r.get("", |req: Request| async move {
//!         format!("hello, {}", req.identity().unwrap().id())
//!     });
//! });
//! ```

use crate::middleware::{SessionExt, SessionHandle};
use rustlavel_core::{Config, Error, Json};
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::middleware::{Middleware, Next};
use rustlavel_http::response::IntoResponse;
use rustlavel_http::{Request, Response, Status};

/// Where the authenticated user's identifier is kept in the session.
pub const IDENTIFIER_KEY: &str = "_auth_id";

/// Where the `auth` middleware remembers the page the visitor was after.
pub const INTENDED_KEY: &str = "_auth_intended";

/// The login path used when nothing is configured.
pub const DEFAULT_LOGIN_PATH: &str = "/login";

/// Where `guest` sends an already-authenticated visitor.
pub const DEFAULT_HOME_PATH: &str = "/";

/// Anything that can be logged in.
///
/// Only an identifier is required, because that is genuinely all the session
/// needs to hold: everything else about the user is loaded from wherever users
/// live. Keeping a whole serialized user in a session cookie is how stale — and
/// eventually wrong — permissions get carried around for hours.
pub trait Authenticatable {
    /// The value stored in the session, usually a primary key.
    fn auth_identifier(&self) -> String;
}

impl Authenticatable for str {
    fn auth_identifier(&self) -> String {
        self.to_string()
    }
}

impl Authenticatable for String {
    fn auth_identifier(&self) -> String {
        self.clone()
    }
}

impl Authenticatable for i64 {
    fn auth_identifier(&self) -> String {
        self.to_string()
    }
}

/// The identifier of the user this request belongs to.
///
/// Attached by the `auth` middleware with `Request::extend`, so a handler
/// downstream reads it with `req.extension::<Identity>()` — or, more
/// readably, `req.identity()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity(String);

impl Identity {
    pub fn new(id: impl Into<String>) -> Self {
        Identity(id.into())
    }

    pub fn id(&self) -> &str {
        &self.0
    }

    /// The identifier parsed into a type: `identity.id_as::<i64>()`.
    pub fn id_as<T: std::str::FromStr>(&self) -> Option<T> {
        self.0.parse().ok()
    }
}

impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Logging in and out, on top of one session.
pub struct Guard {
    session: SessionHandle,
}

impl Guard {
    pub fn new(session: SessionHandle) -> Self {
        Guard { session }
    }

    /// Log a user in, returning the session's new id.
    ///
    /// The session id is rotated first. See [`crate::Session::regenerate`]: a
    /// session id that was known before the login must not still be valid
    /// after it, or an attacker who planted the cookie inherits the login.
    pub fn login(&self, user: &impl Authenticatable) -> String {
        self.login_using_id(user.auth_identifier())
    }

    /// Log in by identifier, for a "log in as" or "remember me" flow that
    /// already knows the key without loading the user.
    pub fn login_using_id(&self, id: impl Into<String>) -> String {
        let new_id = self.session.regenerate();
        self.session.put(IDENTIFIER_KEY, id.into());
        new_id
    }

    /// Log out: empty the session and rotate its id.
    ///
    /// Everything goes, not just the identifier. A cart, a half-filled form, a
    /// flashed message — all of it belonged to the person who just left, and on
    /// a shared machine the next person must not find any of it.
    pub fn logout(&self) -> String {
        self.session.invalidate()
    }

    /// The authenticated user's identifier, if there is one.
    pub fn user_id(&self) -> Option<String> {
        self.session.get_string(IDENTIFIER_KEY)
    }

    pub fn check(&self) -> bool {
        self.user_id().is_some()
    }

    pub fn guest(&self) -> bool {
        !self.check()
    }

    /// Remember where the visitor was heading before being sent to log in.
    ///
    /// Stored rather than flashed: the visitor sees the login form, submits it,
    /// and only then is the destination needed — two requests later, by which
    /// time flash data would already be gone.
    pub fn remember_intended(&self, path: impl Into<String>) {
        self.session.put(INTENDED_KEY, path.into());
    }

    /// Take back that destination, or `fallback` if there is none.
    ///
    /// Consumed on read, so a second login does not bounce the visitor to a
    /// page they asked for hours ago.
    pub fn intended(&self, fallback: &str) -> String {
        self.session
            .forget(INTENDED_KEY)
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| fallback.to_string())
    }

    pub fn session(&self) -> &SessionHandle {
        &self.session
    }
}

/// `req.auth()` and `req.identity()` — reaching authentication from a handler.
pub trait AuthExt {
    /// The guard, or `None` when the `session` middleware is not installed.
    fn try_auth(&self) -> Option<Guard>;

    /// The guard, panicking with an actionable message if there is no session.
    fn auth(&self) -> Guard;

    /// The identity attached by the `auth` middleware.
    fn identity(&self) -> Option<&Identity>;
}

impl AuthExt for Request {
    fn try_auth(&self) -> Option<Guard> {
        self.try_session().cloned().map(Guard::new)
    }

    fn auth(&self) -> Guard {
        Guard::new(self.session().clone())
    }

    fn identity(&self) -> Option<&Identity> {
        self.extension::<Identity>()
    }
}

/// The `auth` middleware: no identity, no handler.
#[derive(Debug, Clone)]
pub struct Authenticate {
    login_path: String,
}

impl Authenticate {
    pub fn new() -> Self {
        Authenticate { login_path: DEFAULT_LOGIN_PATH.to_string() }
    }

    /// Read `auth.login_path` from configuration.
    pub fn from_config(config: &Config) -> Self {
        Authenticate { login_path: config.string("auth.login_path", DEFAULT_LOGIN_PATH) }
    }

    pub fn login_path(mut self, path: impl Into<String>) -> Self {
        self.login_path = path.into();
        self
    }
}

impl Default for Authenticate {
    fn default() -> Self {
        Authenticate::new()
    }
}

impl Middleware for Authenticate {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        let login_path = self.login_path.clone();

        Box::pin(async move {
            let Some(guard) = request.try_auth() else {
                return missing_session("auth").into_response();
            };

            match guard.user_id() {
                Some(id) => {
                    // The handler reads this with `req.identity()`.
                    request.extend(Identity::new(id));
                    next.run(request).await
                }
                // An API client gets a status it can act on; a browser gets
                // sent somewhere it can actually log in. Redirecting an XHR
                // would hand it the login page's HTML as the answer to its
                // request, which is how "why is my JSON parser failing?"
                // becomes a two-hour debugging session.
                None if request.wants_json() => Response::new(Status::UNAUTHORIZED)
                    .with_json(Json::object([("message", Json::from("Unauthenticated."))])),
                None => {
                    guard.remember_intended(request.target());
                    Response::see_other(login_path)
                }
            }
        })
    }
}

/// The `guest` middleware: keeps an authenticated visitor off the login page.
#[derive(Debug, Clone)]
pub struct Guest {
    home: String,
}

impl Guest {
    pub fn new() -> Self {
        Guest { home: DEFAULT_HOME_PATH.to_string() }
    }

    /// Read `auth.home` from configuration.
    pub fn from_config(config: &Config) -> Self {
        Guest { home: config.string("auth.home", DEFAULT_HOME_PATH) }
    }

    pub fn home(mut self, path: impl Into<String>) -> Self {
        self.home = path.into();
        self
    }
}

impl Default for Guest {
    fn default() -> Self {
        Guest::new()
    }
}

impl Middleware for Guest {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        let home = self.home.clone();

        Box::pin(async move {
            let Some(guard) = request.try_auth() else {
                return missing_session("guest").into_response();
            };

            if guard.check() { Response::see_other(home) } else { next.run(request).await }
        })
    }
}

fn missing_session(name: &str) -> Error {
    Error::msg(format!(
        "the `{name}` middleware needs a session. Register the `session` middleware before it: \
         `router.middleware(SessionManager::from_config(&config, store)?)`."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::AppKey;
    use crate::middleware::SessionManager;
    use crate::store::MemoryStore;
    use rustlavel_http::{Method, Router, TestClient, TestResponse};

    struct User {
        id: i64,
    }

    impl Authenticatable for User {
        fn auth_identifier(&self) -> String {
            self.id.to_string()
        }
    }

    fn router() -> Router {
        let mut router = Router::new();
        router.middleware(SessionManager::new(&AppKey::from_bytes([6u8; 32]), MemoryStore::new()));

        router.get("/login-as-41", |request: Request| async move {
            request.auth().login(&User { id: 41 });
            "logged in"
        });
        router.get("/logout", |request: Request| async move {
            request.auth().logout();
            "logged out"
        });

        router.group("/dashboard", |r| {
            r.middleware(Authenticate::new().login_path("/login"));
            r.get("", |request: Request| async move {
                let identity = request.identity().expect("the middleware attaches an identity");
                format!("user {} ({:?})", identity.id(), identity.id_as::<i64>())
            });
        });

        router.group("/api", |r| {
            r.middleware(Authenticate::new());
            r.get("/me", |_request: Request| async { "private" });
        });

        router.group("/login", |r| {
            r.middleware(Guest::new().home("/dashboard"));
            r.get("", |_request: Request| async { "the login form" });
        });

        router
    }

    fn cookie_of(response: &TestResponse) -> String {
        response
            .header("set-cookie")
            .expect("the session middleware should have set a cookie")
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    async fn logged_in(client: &TestClient) -> String {
        cookie_of(&client.get("/login-as-41").await.assert_ok())
    }

    #[test]
    fn a_guard_stores_reads_and_clears_the_user_id() {
        let session = SessionHandle::new(crate::Session::new());
        let guard = Guard::new(session.clone());

        assert!(guard.guest());
        assert_eq!(guard.user_id(), None);

        guard.login(&User { id: 7 });

        assert!(guard.check());
        assert_eq!(guard.user_id().as_deref(), Some("7"));
        assert_eq!(session.get_string(IDENTIFIER_KEY).as_deref(), Some("7"));

        guard.logout();

        assert!(guard.guest());
        assert!(session.all().is_empty());
    }

    #[test]
    fn logging_in_rotates_the_session_id() {
        let session = SessionHandle::new(crate::Session::new());
        let before = session.id();

        let after = Guard::new(session.clone()).login(&User { id: 7 });

        assert_ne!(before, after, "session fixation: the pre-login id must stop working");
        assert_eq!(session.id(), after);
    }

    #[test]
    fn logging_out_rotates_the_session_id_too() {
        let session = SessionHandle::new(crate::Session::new());
        let guard = Guard::new(session.clone());
        let signed_in = guard.login(&User { id: 7 });

        let after = guard.logout();

        assert_ne!(signed_in, after);
        assert_eq!(session.id(), after);
    }

    #[test]
    fn the_intended_destination_is_remembered_once() {
        let guard = Guard::new(SessionHandle::new(crate::Session::new()));

        assert_eq!(guard.intended("/home"), "/home");

        guard.remember_intended("/reports?year=2026");
        assert_eq!(guard.intended("/home"), "/reports?year=2026");
        assert_eq!(guard.intended("/home"), "/home", "it is consumed on read");
    }

    #[test]
    fn common_identifier_types_are_authenticatable() {
        assert_eq!("ada".auth_identifier(), "ada");
        assert_eq!(String::from("ada").auth_identifier(), "ada");
        assert_eq!(41i64.auth_identifier(), "41");
    }

    #[tokio::test]
    async fn the_auth_middleware_redirects_a_browser_to_the_login_path() {
        let response = TestClient::new(router()).get("/dashboard").await;

        response.assert_redirect("/login");
    }

    #[tokio::test]
    async fn the_auth_middleware_returns_401_to_a_json_client() {
        let response = TestClient::new(router())
            .send(Request::new(Method::Get, "/api/me").with_header("accept", "application/json"))
            .await;

        response.assert_status(401).assert_json("message", "Unauthenticated.");
    }

    #[tokio::test]
    async fn an_authenticated_request_reaches_the_handler_with_its_identity() {
        let client = TestClient::new(router());
        let cookie = logged_in(&client).await;

        client
            .send(Request::new(Method::Get, "/dashboard").with_header("cookie", cookie))
            .await
            .assert_ok()
            .assert_see("user 41")
            .assert_see("Some(41)");
    }

    #[tokio::test]
    async fn logging_out_closes_the_door_again() {
        let client = TestClient::new(router());
        let cookie = logged_in(&client).await;

        let goodbye = client
            .send(Request::new(Method::Get, "/logout").with_header("cookie", cookie.clone()))
            .await
            .assert_ok();

        // The old cookie no longer opens the dashboard...
        client
            .send(Request::new(Method::Get, "/dashboard").with_header("cookie", cookie))
            .await
            .assert_redirect("/login");

        // ...and neither does the one handed back by the logout itself.
        client
            .send(
                Request::new(Method::Get, "/dashboard").with_header("cookie", cookie_of(&goodbye)),
            )
            .await
            .assert_redirect("/login");
    }

    #[tokio::test]
    async fn the_redirect_remembers_where_the_visitor_was_going() {
        let client = TestClient::new(router());

        let redirected = client.get("/dashboard").await.assert_redirect("/login");
        let cookie = cookie_of(&redirected);

        // The login form itself is where the intended path would be read.
        let form = client
            .send(Request::new(Method::Get, "/login").with_header("cookie", cookie.clone()))
            .await;
        form.assert_ok().assert_see("the login form");
    }

    #[tokio::test]
    async fn the_guest_middleware_pushes_an_authenticated_visitor_home() {
        let client = TestClient::new(router());

        client.get("/login").await.assert_ok().assert_see("the login form");

        let cookie = logged_in(&client).await;
        client
            .send(Request::new(Method::Get, "/login").with_header("cookie", cookie))
            .await
            .assert_redirect("/dashboard");
    }

    #[tokio::test]
    async fn both_middleware_fail_loudly_without_a_session() {
        for name in ["auth", "guest"] {
            let mut router = Router::new();
            match name {
                "auth" => router.middleware(Authenticate::new()),
                _ => router.middleware(Guest::new()),
            };
            router.get("/", |_request: Request| async { "ok" });

            // A server error rather than a silent pass: a guard that cannot
            // read the session must never let the request through.
            TestClient::new(router).get("/").await.assert_status(500);
        }
    }

    #[test]
    fn the_missing_session_error_says_how_to_fix_it() {
        let message = missing_session("auth").to_string();

        assert!(message.contains("`auth` middleware needs a session"), "message was {message}");
        assert!(message.contains("SessionManager::from_config"), "message was {message}");
    }

    #[test]
    fn configuration_supplies_the_login_and_home_paths() {
        let config = Config::new();
        config.set("auth.login_path", "/sign-in");
        config.set("auth.home", "/app");

        assert_eq!(Authenticate::from_config(&config).login_path, "/sign-in");
        assert_eq!(Guest::from_config(&config).home, "/app");

        assert_eq!(Authenticate::from_config(&Config::new()).login_path, DEFAULT_LOGIN_PATH);
        assert_eq!(Guest::from_config(&Config::new()).home, DEFAULT_HOME_PATH);
    }
}
