//! The `WhenActive` middleware, and asking about a flag from inside a handler.
//!
//! Shaped like `rustlavel_rbac::Can`, because it is a neighbouring
//! conversation: that middleware decides whether this user *may* see a route,
//! this one decides whether the route is *there* for them yet.
//!
//! ```ignore
//! router.group("/checkout", |r| {
//!     r.middleware(Authenticate::from_config(&config));
//!     r.middleware(WhenActive::new("new-checkout"));
//!     r.post("", place_order);
//! });
//!
//! router.get("/search", |req: Request| async move {
//!     // Or ask inside the handler, when the flag changes the page rather
//!     // than removing it.
//!     let template = if req.flag("beta-search").await? { "search.beta" } else { "search" };
//!     Ok(Response::html(render(template)))
//! });
//! ```

use crate::flags::{Flags, ScopedFlags, missing_registry};
use crate::scope::Scope;
use rustlavel_auth::guard::AuthExt;
use rustlavel_core::{Error, Result};
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::middleware::{Middleware, Next};
use rustlavel_http::response::IntoResponse;
use rustlavel_http::{Request, Response};

/// A route that is not there yet.
///
/// # Why 404 and not 403
///
/// A `403` is an admission. It says "there is something at this address, and
/// you are not it" — which tells anyone who tries the URL that the feature
/// exists, roughly what it is called, and that it is worth coming back for.
/// Half-built features get found this way: somebody diffs the routes between
/// two deploys, walks the ones that answer `403`, and reads the names.
///
/// A route behind a flag that is off has not shipped. The honest answer, and
/// the same one the router gives for a path nobody ever wrote, is `404` — and
/// it is byte-for-byte the router's own 404 here, because a body that differed
/// would give the whole thing away again.
///
/// This is concealment, not authorization. It keeps a URL from being
/// *interesting*; it does not keep anybody out. If the feature behind the flag
/// must not be reachable by the people outside the rollout — because it costs
/// money, or exposes somebody else's data — put `rustlavel_rbac::Can` in front
/// of it as well. A flag is a release tool.
///
/// # What it does to the request
///
/// On the way through, the [`ScopedFlags`] it resolved is attached to the
/// request, so a handler that asks the same or another flag reuses the memo
/// instead of running the resolvers again.
#[derive(Debug, Clone)]
pub struct WhenActive {
    flag: String,
}

impl WhenActive {
    /// Guard the routes below with one flag.
    pub fn new(flag: impl Into<String>) -> Self {
        WhenActive { flag: flag.into() }
    }

    /// The flag this guard is checking.
    pub fn flag(&self) -> &str {
        &self.flag
    }
}

impl Middleware for WhenActive {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        let flag = self.flag.clone();

        Box::pin(async move {
            let mut request = request;

            let scoped = match request.scoped_flags() {
                Ok(scoped) => scoped,
                Err(error) => return error.into_response(),
            };

            match scoped.active(&flag).await {
                Ok(true) => {
                    // Hand the memo to the handler: it very often wants the
                    // same answer again, and this way it is free.
                    request.extend(scoped);
                    next.run(request).await
                }
                Ok(false) => Response::not_found(),
                // Not a 404. A flag that could not be resolved is not a flag
                // that is off, and answering "no such route" would hide a
                // broken store behind a page that merely looks unfinished —
                // for as long as it takes somebody to notice the feature never
                // arrived. The cost is that a 500 admits something is here,
                // which is the leak this middleware otherwise exists to close;
                // we take that trade, because the alternative is an outage
                // nobody can see.
                Err(error) => Error::msg(format!(
                    "checking the `{flag}` feature flag failed: {error}"
                ))
                .into_response(),
            }
        })
    }
}

/// `req.flag(...)` — asking about a flag from inside a handler.
///
/// The companion to [`WhenActive`], for when the flag changes the page rather
/// than removing it: a new panel, a different template, an extra field on a
/// form.
///
/// Every method returns `Result`, and that is deliberate. "I could not find
/// out" is a third answer, distinct from on and from off, and collapsing it
/// into `false` would turn a store outage into a silent, global rollback that
/// looks exactly like the flag being off on purpose.
pub trait FlagsExt {
    /// The registry from application state.
    fn flags(&self) -> Result<Flags>;

    /// The scope this request is checked as.
    ///
    /// The logged-in `rustlavel_auth::Identity` if there is one — attached by
    /// the `auth` middleware, or read from the session if only the session
    /// middleware is installed — and [`Scope::none`] otherwise. A guest is not
    /// a scope of their own: they are everybody, and giving them one would mean
    /// a percentage rollout that reshuffles on every visit.
    fn flag_scope(&self) -> Scope;

    /// A [`ScopedFlags`] for this request, reusing the one [`WhenActive`]
    /// attached if there is one.
    ///
    /// Take it once and keep it when a handler checks several flags. Calling
    /// [`FlagsExt::flag`] repeatedly on a request that did not come through
    /// `WhenActive` runs the resolver once per call — the memo lives in the
    /// view, and a fresh view remembers nothing.
    fn scoped_flags(&self) -> Result<ScopedFlags>;

    /// Is this flag on for whoever is making this request?
    fn flag(&self, flag: &str) -> impl Future<Output = Result<bool>> + Send;

    /// The negation, for the reading that comes out shorter.
    fn flag_off(&self, flag: &str) -> impl Future<Output = Result<bool>> + Send;
}

impl FlagsExt for Request {
    fn flags(&self) -> Result<Flags> {
        self.state::<Flags>().cloned().ok_or_else(missing_registry)
    }

    fn flag_scope(&self) -> Scope {
        if let Some(identity) = self.identity() {
            return Scope::user(identity.id());
        }

        match self.try_auth().and_then(|guard| guard.user_id()) {
            Some(id) => Scope::user(id),
            None => Scope::none(),
        }
    }

    fn scoped_flags(&self) -> Result<ScopedFlags> {
        if let Some(scoped) = self.extension::<ScopedFlags>() {
            return Ok(scoped.clone());
        }

        Ok(self.flags()?.for_scope(self.flag_scope()))
    }

    async fn flag(&self, flag: &str) -> Result<bool> {
        self.scoped_flags()?.active(flag).await
    }

    async fn flag_off(&self, flag: &str) -> Result<bool> {
        Ok(!self.flag(flag).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use rustlavel_auth::key::AppKey;
    use rustlavel_auth::store::MemoryStore as SessionStore;
    use rustlavel_auth::SessionManager;
    use rustlavel_core::Context;
    use rustlavel_http::{Method, Router, TestClient, TestResponse};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `new-checkout` is on for user ids ending in 7; `beta-search` is off.
    fn flags() -> Flags {
        Flags::new()
            .store(MemoryStore::new())
            .define("new-checkout", |scope: Scope| async move { scope.id().ends_with('7') })
            .define("beta-search", |_| async { false })
    }

    /// The routes, with or without a registry in application state.
    fn client(flags: Option<Flags>) -> TestClient {
        let mut router = Router::new();
        router.middleware(SessionManager::new(&AppKey::from_bytes([9u8; 32]), SessionStore::new()));

        router.get("/login-as/{id}", |request: Request| async move {
            let id = request.param("id").unwrap_or_default().to_string();
            request.auth().login_using_id(id);
            "logged in"
        });

        router.group("/checkout", |r| {
            r.middleware(WhenActive::new("new-checkout"));
            r.get("", |_request: Request| async { "the new checkout" });
        });

        router.get("/who", |request: Request| async move {
            let scope = request.flag_scope();
            let on = request.flag("new-checkout").await?;
            Ok::<_, Error>(format!("{scope}|new-checkout={on}"))
        });

        let mut builder = Context::builder();
        if let Some(flags) = flags {
            builder = builder.state(flags);
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

    async fn logged_in(client: &TestClient, id: &str) -> String {
        cookie_of(&client.get(&format!("/login-as/{id}")).await.assert_ok())
    }

    async fn get_as(client: &TestClient, cookie: &str, path: &str) -> TestResponse {
        client.send(Request::new(Method::Get, path).with_header("cookie", cookie)).await
    }

    #[tokio::test]
    async fn a_route_behind_a_flag_that_is_on_is_reachable() {
        let client = client(Some(flags()));
        let cookie = logged_in(&client, "17").await;

        get_as(&client, &cookie, "/checkout").await.assert_ok().assert_see("the new checkout");
    }

    #[tokio::test]
    async fn a_route_behind_a_flag_that_is_off_answers_404() {
        let client = client(Some(flags()));
        let cookie = logged_in(&client, "18").await;

        get_as(&client, &cookie, "/checkout").await.assert_not_found();
    }

    #[tokio::test]
    async fn the_404_is_indistinguishable_from_a_route_that_never_existed() {
        // The point of the middleware. If these two differed by so much as a
        // header or a byte of body, the guard would be announcing the feature
        // to anybody who compared them.
        let client = client(Some(flags()));
        let cookie = logged_in(&client, "18").await;

        let flagged = get_as(&client, &cookie, "/checkout").await;
        let imaginary = get_as(&client, &cookie, "/no-such-route").await;

        assert_eq!(flagged.status(), imaginary.status());
        assert_eq!(flagged.body(), imaginary.body());
        assert_eq!(flagged.header("content-type"), imaginary.header("content-type"));
    }

    #[tokio::test]
    async fn a_guest_falls_back_to_the_global_scope() {
        let client = client(Some(flags()));

        // Not logged in, so the resolver sees `Scope::none()`, whose id is
        // empty and does not end in 7.
        client.get("/checkout").await.assert_not_found();
        client.get("/who").await.assert_ok().assert_see("global|new-checkout=false");
    }

    #[tokio::test]
    async fn an_override_reopens_a_route_the_resolver_had_closed() {
        // What an operator does when one customer needs the feature today.
        let flags = flags();
        let client = client(Some(flags.clone()));
        let cookie = logged_in(&client, "18").await;

        get_as(&client, &cookie, "/checkout").await.assert_not_found();

        flags.activate_for("new-checkout", &Scope::user("18")).await.unwrap();

        get_as(&client, &cookie, "/checkout").await.assert_ok().assert_see("the new checkout");
    }

    #[tokio::test]
    async fn the_incident_switch_closes_a_route_the_resolver_had_opened() {
        let flags = flags().force_off(["new-checkout"]);
        let client = client(Some(flags.clone()));
        let cookie = logged_in(&client, "17").await;

        // On for user 17 by the resolver, and on for them by an override too.
        flags.activate_for("new-checkout", &Scope::user("17")).await.unwrap();

        get_as(&client, &cookie, "/checkout").await.assert_not_found();
    }

    #[tokio::test]
    async fn a_handler_can_ask_for_itself_and_sees_the_logged_in_user() {
        let client = client(Some(flags()));
        let cookie = logged_in(&client, "17").await;

        get_as(&client, &cookie, "/who").await.assert_ok().assert_see("user:17|new-checkout=true");
    }

    #[tokio::test]
    async fn a_missing_registry_is_a_configuration_error_and_not_a_404() {
        // 500, not 404: nobody decided this route was hidden, the application
        // was assembled wrongly. A 404 here would look exactly like a flag that
        // is off, and the mistake would ship.
        let client = client(None);
        let cookie = logged_in(&client, "17").await;

        get_as(&client, &cookie, "/checkout").await.assert_status(500);
        get_as(&client, &cookie, "/who").await.assert_status(500);
    }

    #[tokio::test]
    async fn the_guard_hands_its_answers_to_the_handler() {
        // The resolver runs once for the whole request, not once for the
        // middleware and again for each question the handler asks.
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);

        let flags = Flags::new().define("counted", move |_| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                true
            }
        });

        let mut router = Router::new();
        router.group("/page", |r| {
            r.middleware(WhenActive::new("counted"));
            r.get("", |request: Request| async move {
                for _ in 0..5 {
                    assert!(request.flag("counted").await?);
                }
                Ok::<_, Error>("drawn")
            });
        });

        let client =
            TestClient::new(router).with_context(Context::builder().state(flags).build());

        client.get("/page").await.assert_ok().assert_see("drawn");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_handler_holding_the_view_itself_asks_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);

        let flags = Flags::new().define("counted", move |_| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                true
            }
        });

        let mut router = Router::new();
        router.get("/page", |request: Request| async move {
            let view = request.scoped_flags()?;
            let values = view.values(["counted", "counted"]).await?;
            Ok::<_, Error>(format!("{}", values["counted"]))
        });

        let client =
            TestClient::new(router).with_context(Context::builder().state(flags).build());

        client.get("/page").await.assert_ok().assert_see("true");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_resolver_that_fails_gives_500_rather_than_hiding_the_route() {
        let flags =
            Flags::new().define("flaky", |_| async { Result::<bool>::Err(Error::msg("nope")) });

        let mut router = Router::new();
        router.group("/page", |r| {
            r.middleware(WhenActive::new("flaky"));
            r.get("", |_request: Request| async { "drawn" });
        });

        let client =
            TestClient::new(router).with_context(Context::builder().state(flags).build());

        client.get("/page").await.assert_status(500);
    }

    #[tokio::test]
    async fn flag_off_is_the_negation() {
        let mut router = Router::new();
        router.get("/page", |request: Request| async move {
            let off = request.flag_off("beta-search").await?;
            Ok::<_, Error>(format!("off={off}"))
        });

        let client =
            TestClient::new(router).with_context(Context::builder().state(flags()).build());

        client.get("/page").await.assert_ok().assert_see("off=true");
    }

    #[test]
    fn the_guard_names_the_flag_it_checks() {
        assert_eq!(WhenActive::new("new-checkout").flag(), "new-checkout");
    }

    #[test]
    fn a_request_with_no_session_at_all_is_the_global_scope() {
        assert_eq!(Request::new(Method::Get, "/").flag_scope(), Scope::none());
    }
}
