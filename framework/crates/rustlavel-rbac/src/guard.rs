//! The `can` and `role` middleware, and the two questions a handler asks.
//!
//! Shaped like [`Authenticate`](rustlavel_auth::Authenticate), because it is
//! the same conversation one step later: that middleware decides *who* this is,
//! this one decides *whether they may*.
//!
//! ```ignore
//! router.group("/admin", |r| {
//!     r.middleware(Authenticate::from_config(&config));
//!     r.middleware(Can::role("admin"));
//!
//!     r.group("/users", |r| {
//!         r.middleware(Can::permission("users.create"));
//!         r.post("", create_user);
//!     });
//!
//!     r.get("/dashboard", |req: Request| async move {
//!         // Or ask inside the handler, when the answer changes the page
//!         // rather than closing it.
//!         let banner = if req.can("billing.view").await? { billing() } else { String::new() };
//!         Ok(Response::html(banner))
//!     });
//! });
//! ```

use crate::store::Permissions;
use rustlavel_auth::guard::{AuthExt, Identity, DEFAULT_LOGIN_PATH};
use rustlavel_core::{Config, Error, Json, Result};
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::middleware::{Middleware, Next};
use rustlavel_http::response::IntoResponse;
use rustlavel_http::{Request, Response, Status};

/// What a guard is checking for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Requirement {
    Permission(String),
    Role(String),
}

impl Requirement {
    fn noun(&self) -> &'static str {
        match self {
            Requirement::Permission(_) => "permission",
            Requirement::Role(_) => "role",
        }
    }

    fn name(&self) -> &str {
        match self {
            Requirement::Permission(name) | Requirement::Role(name) => name,
        }
    }
}

/// The authorization middleware: no permission, no handler.
///
/// A guest is treated exactly as [`Authenticate`](rustlavel_auth::Authenticate)
/// treats one — redirected to the login page, or answered `401` if the client
/// asked for JSON — because "you are not allowed" is the wrong thing to tell
/// somebody who simply has not logged in yet, and a `403` gives them nothing to
/// do about it.
#[derive(Debug, Clone)]
pub struct Can {
    requirement: Requirement,
    login_path: String,
}

impl Can {
    /// Require a permission. Wildcards stored against the user apply, so a user
    /// holding `users.*` passes `Can::permission("users.create")`.
    pub fn permission(name: impl Into<String>) -> Self {
        Can::with(Requirement::Permission(name.into()))
    }

    /// Require a role, by exact name.
    ///
    /// Prefer [`Can::permission`]: a route guarded by a role has to be edited
    /// when the roles change, and a route guarded by a permission does not.
    /// This exists because sometimes the role really is the thing being
    /// checked.
    pub fn role(name: impl Into<String>) -> Self {
        Can::with(Requirement::Role(name.into()))
    }

    fn with(requirement: Requirement) -> Self {
        Can { requirement, login_path: DEFAULT_LOGIN_PATH.to_string() }
    }

    /// Read `auth.login_path` from configuration, so a guest is sent to the
    /// same place `Authenticate` would have sent them.
    pub fn from_config(mut self, config: &Config) -> Self {
        self.login_path = config.string("auth.login_path", DEFAULT_LOGIN_PATH);
        self
    }

    pub fn login_path(mut self, path: impl Into<String>) -> Self {
        self.login_path = path.into();
        self
    }
}

impl Middleware for Can {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        let requirement = self.requirement.clone();
        let login_path = self.login_path.clone();

        Box::pin(async move {
            let Some(identity) = current_identity(&request) else {
                // Not logged in. Same two answers as `Authenticate`.
                if request.wants_json() {
                    return Response::new(Status::UNAUTHORIZED)
                        .with_json(Json::object([("message", Json::from("Unauthenticated."))]));
                }
                if let Some(guard) = request.try_auth() {
                    guard.remember_intended(request.target());
                }
                return Response::see_other(login_path);
            };

            let store = match resolve(&request) {
                Ok(store) => store,
                Err(error) => return error.into_response(),
            };

            let user_id = match user_id(&identity) {
                Ok(id) => id,
                Err(error) => return error.into_response(),
            };

            let allowed = match &requirement {
                Requirement::Permission(name) => store.has_permission(user_id, name).await,
                Requirement::Role(name) => store.has_role(user_id, name).await,
            };

            match allowed {
                Ok(true) => next.run(request).await,
                Ok(false) => forbidden(&requirement, request.wants_json()),
                // The database is down, or the tables are not there. There is
                // no safe guess to make: a check that could not be performed
                // has not passed.
                Err(error) => Error::msg(format!(
                    "checking the `{}` {} failed: {error}",
                    requirement.name(),
                    requirement.noun()
                ))
                .into_response(),
            }
        })
    }
}

/// Who this request belongs to.
///
/// The extension `Authenticate` attaches, falling back to the session — so
/// `Can` works whether or not `Authenticate` is in front of it, and a route
/// that only needs authorization does not have to stack two middleware to get
/// the redirect behaviour right.
fn current_identity(request: &Request) -> Option<Identity> {
    request
        .identity()
        .cloned()
        .or_else(|| request.try_auth().and_then(|guard| guard.user_id()).map(Identity::new))
}

/// The store, or a 500 with instructions.
///
/// Never a pass. A missing store means the application was assembled wrongly,
/// and the one thing an authorization guard must not do when it is confused is
/// let the request through.
fn resolve(request: &Request) -> Result<Permissions> {
    request.state::<Permissions>().cloned().ok_or_else(|| {
        Error::msg(
            "the `can` middleware needs a `Permissions` store in application state, and there is \
             none. Register it with `App::new().plugin(Rbac::new(db))`, or put it there yourself \
             with `Context::builder().state(Permissions::new(db))`. Refusing the request rather \
             than allowing it.",
        )
    })
}

/// The RBAC tables key users by `bigint`, so the identity has to be one.
fn user_id(identity: &Identity) -> Result<i64> {
    identity.id_as::<i64>().ok_or_else(|| {
        Error::msg(format!(
            "the authenticated identity `{identity}` is not a number, and the RBAC tables key \
             users by a 64-bit integer id. Either log users in by their numeric primary key, or \
             use a different authorization layer for this application."
        ))
    })
}

/// Refused, naming what would have been needed.
///
/// 403 and not 401: the request was authenticated and understood. Logging in
/// again changes nothing, and a client that retries on 401 must not retry here.
fn forbidden(requirement: &Requirement, wants_json: bool) -> Response {
    let response = Response::new(Status::FORBIDDEN);

    if wants_json {
        response.with_json(Json::object([
            ("message", Json::from("This action is unauthorized.")),
            // Named, because a client that is told only "no" cannot tell a
            // missing permission from a bug. This is not a leak: the requirement
            // is already visible in the route's own definition.
            (requirement.noun(), Json::from(requirement.name())),
        ]))
    } else {
        response.with_text(format!(
            "This action requires the `{}` {}.",
            requirement.name(),
            requirement.noun()
        ))
    }
}

/// `req.can(...)` and `req.has_role(...)` — asking from inside a handler.
///
/// The companion to [`Can`], for when the answer shapes the page rather than
/// closing it: a button that is only drawn for someone who may press it.
///
/// Every method returns `Result`, and that is deliberate. "I could not find
/// out" is a third answer, distinct from yes and from no, and collapsing it
/// into `false` would hide a broken database behind a UI that merely looks a
/// little emptier than usual.
pub trait RbacExt {
    /// The store from application state.
    fn permissions(&self) -> Result<Permissions>;

    /// The identity this request is authorized as, if any.
    fn rbac_user_id(&self) -> Option<i64>;

    /// May the current user do this? `false` for a guest, who is nobody.
    fn can(&self, permission: &str) -> impl Future<Output = Result<bool>> + Send;

    /// Does the current user hold this role, by exact name?
    fn has_role(&self, role: &str) -> impl Future<Output = Result<bool>> + Send;

    /// Everything the current user's rules say, for an admin screen or a
    /// debugging endpoint. Empty for a guest.
    fn permission_list(&self) -> impl Future<Output = Result<Vec<String>>> + Send;
}

impl RbacExt for Request {
    fn permissions(&self) -> Result<Permissions> {
        resolve(self)
    }

    fn rbac_user_id(&self) -> Option<i64> {
        current_identity(self).and_then(|identity| identity.id_as::<i64>())
    }

    async fn can(&self, permission: &str) -> Result<bool> {
        match self.rbac_user_id() {
            Some(id) => self.permissions()?.has_permission(id, permission).await,
            None => Ok(false),
        }
    }

    async fn has_role(&self, role: &str) -> Result<bool> {
        match self.rbac_user_id() {
            Some(id) => self.permissions()?.has_role(id, role).await,
            None => Ok(false),
        }
    }

    async fn permission_list(&self) -> Result<Vec<String>> {
        match self.rbac_user_id() {
            Some(id) => self.permissions()?.permissions_for(id).await,
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::Grants;
    use rustlavel_auth::key::AppKey;
    use rustlavel_auth::store::MemoryStore;
    use rustlavel_auth::{Authenticate, SessionManager};
    use rustlavel_core::Context;
    use rustlavel_db::{Database, DatabaseConfig};
    use rustlavel_http::{Method, Router, TestClient, TestResponse};
    use std::collections::BTreeSet;

    /// A database handle pointed at a port nothing listens on.
    ///
    /// Every test below primes the cache, so no statement is ever sent — and if
    /// one were, it would fail loudly rather than quietly succeed against
    /// somebody else's database.
    fn offline() -> Database {
        Database::lazy(
            DatabaseConfig::from_url("postgres://nobody:nothing@127.0.0.1:1/none").unwrap(),
        )
        .unwrap()
    }

    fn set<const N: usize>(names: [&str; N]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    /// A store where user 41 is an editor who may publish posts.
    fn store() -> Permissions {
        let store = Permissions::new(offline());
        store.prime(
            41,
            Grants {
                roles: set(["editor"]),
                granted: set(["posts.*"]),
                denied: set(["posts.delete"]),
                is_super: false,
            },
        );
        store.prime(7, Grants::default());
        store
    }

    /// The routes, with or without a store in application state.
    fn client(with_store: bool) -> TestClient {
        let mut router = Router::new();
        router.middleware(SessionManager::new(&AppKey::from_bytes([9u8; 32]), MemoryStore::new()));

        router.get("/login-as/{id}", |request: Request| async move {
            let id = request.param("id").unwrap_or_default().to_string();
            request.auth().login_using_id(id);
            "logged in"
        });

        router.group("/posts", |r| {
            r.middleware(Authenticate::new().login_path("/login"));
            r.group("/publish", |r| {
                r.middleware(Can::permission("posts.publish"));
                r.get("", |_request: Request| async { "published" });
            });
            r.group("/delete", |r| {
                r.middleware(Can::permission("posts.delete"));
                r.get("", |_request: Request| async { "deleted" });
            });
        });

        router.group("/admin", |r| {
            r.middleware(Can::role("admin"));
            r.get("", |_request: Request| async { "the admin area" });
        });

        // No `Authenticate` in front: `Can` has to do the guest handling itself.
        router.group("/editor", |r| {
            r.middleware(Can::role("editor"));
            r.get("", |_request: Request| async { "the editor area" });
        });

        router.group("/api", |r| {
            r.middleware(Can::permission("posts.publish"));
            r.get("/publish", |_request: Request| async { "published" });
        });

        router.get("/what-can-i-do", |request: Request| async move {
            let list = request.permission_list().await?;
            let editor = request.has_role("editor").await?;
            Ok::<_, Error>(format!("{}|editor={editor}", list.join(",")))
        });

        let mut builder = Context::builder();
        if with_store {
            builder = builder.state(store());
        }

        TestClient::new(router).with_context(builder.build())
    }

    fn cookie_of(response: &TestResponse) -> String {
        response
            .header("set-cookie")
            .expect("the session middleware sets a cookie")
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    async fn logged_in(client: &TestClient, id: i64) -> String {
        cookie_of(&client.get(&format!("/login-as/{id}")).await.assert_ok())
    }

    async fn get_as(client: &TestClient, cookie: &str, path: &str) -> TestResponse {
        client.send(Request::new(Method::Get, path).with_header("cookie", cookie)).await
    }

    #[tokio::test]
    async fn a_guest_is_sent_to_the_login_page() {
        // Behind `Authenticate`, which redirects first...
        client(true).get("/posts/publish").await.assert_redirect("/login");
        // ...and on its own, where `Can` has to do it.
        client(true).get("/editor").await.assert_redirect("/login");
    }

    #[tokio::test]
    async fn a_guest_asking_for_json_gets_401_rather_than_a_redirect() {
        let response = client(true)
            .send(
                Request::new(Method::Get, "/api/publish")
                    .with_header("accept", "application/json"),
            )
            .await;

        response.assert_status(401).assert_json("message", "Unauthenticated.");
    }

    #[tokio::test]
    async fn a_user_with_the_permission_gets_through() {
        let client = client(true);
        let cookie = logged_in(&client, 41).await;

        // Granted by the wildcard `posts.*`, which no query had to expand.
        get_as(&client, &cookie, "/posts/publish").await.assert_ok().assert_see("published");
    }

    #[tokio::test]
    async fn a_user_without_the_permission_gets_403() {
        let client = client(true);
        let cookie = logged_in(&client, 7).await;

        get_as(&client, &cookie, "/posts/publish")
            .await
            .assert_status(403)
            .assert_see("posts.publish");
    }

    #[tokio::test]
    async fn a_direct_deny_closes_a_route_the_wildcard_would_have_opened() {
        let client = client(true);
        let cookie = logged_in(&client, 41).await;

        // 41 holds `posts.*` and is still refused, because `posts.delete` is
        // denied directly. This is the precedence rule, on a real route.
        get_as(&client, &cookie, "/posts/delete").await.assert_status(403);
    }

    #[tokio::test]
    async fn a_logged_in_client_asking_for_json_gets_403_with_the_missing_permission() {
        let client = client(true);
        let cookie = logged_in(&client, 7).await;

        client
            .send(
                Request::new(Method::Get, "/api/publish")
                    .with_header("cookie", cookie)
                    .with_header("accept", "application/json"),
            )
            .await
            .assert_status(403)
            .assert_json("message", "This action is unauthorized.")
            .assert_json("permission", "posts.publish");
    }

    #[tokio::test]
    async fn a_role_guard_admits_the_role_and_refuses_the_rest() {
        let client = client(true);

        let editor = logged_in(&client, 41).await;
        get_as(&client, &editor, "/editor").await.assert_ok().assert_see("the editor area");
        get_as(&client, &editor, "/admin").await.assert_status(403).assert_see("admin");

        let nobody = logged_in(&client, 7).await;
        get_as(&client, &nobody, "/editor").await.assert_status(403);
    }

    #[tokio::test]
    async fn a_missing_store_fails_closed() {
        let client = client(false);
        let cookie = logged_in(&client, 41).await;

        // 500, not 200: a guard that cannot find its store has not decided
        // anything, and an undecided guard does not open the door.
        get_as(&client, &cookie, "/posts/publish").await.assert_status(500);
    }

    #[tokio::test]
    async fn an_identity_that_is_not_a_number_is_a_configuration_error() {
        let client = client(true);
        let cookie = cookie_of(&client.get("/login-as/ada").await.assert_ok());

        get_as(&client, &cookie, "/posts/publish").await.assert_status(500);
    }

    #[tokio::test]
    async fn a_handler_can_ask_for_itself() {
        let client = client(true);
        let cookie = logged_in(&client, 41).await;

        get_as(&client, &cookie, "/what-can-i-do")
            .await
            .assert_ok()
            .assert_see("posts.*|editor=true");
    }

    #[tokio::test]
    async fn a_guest_asking_from_a_handler_is_told_no_rather_than_asked_to_log_in() {
        // The extension trait answers a question; it does not guard anything.
        client(true).get("/what-can-i-do").await.assert_ok().assert_see("|editor=false");
    }

    #[test]
    fn the_missing_store_error_says_how_to_fix_it() {
        let message = resolve(&Request::new(Method::Get, "/")).unwrap_err().to_string();

        assert!(message.contains("`Permissions` store"), "{message}");
        assert!(message.contains("Rbac::new(db)"), "{message}");
        assert!(message.contains("Refusing the request"), "{message}");
    }

    #[test]
    fn a_non_numeric_identity_is_explained() {
        let message = user_id(&Identity::new("ada")).unwrap_err().to_string();

        assert!(message.contains("is not a number"), "{message}");
        assert_eq!(user_id(&Identity::new("41")).unwrap(), 41);
    }
}
