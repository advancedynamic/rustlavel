//! The router: what an application's `routes/web.rs` fills in.
//!
//! ```ignore
//! pub fn routes(r: &mut Router) {
//!     r.get("/", home);
//!     r.get("/users/{id}", show).name("users.show");
//!
//!     r.group("/admin", |r| {
//!         r.middleware(auth);
//!         r.get("/dashboard", dashboard);
//!     });
//! }
//! ```

use crate::handler::Handler;
use crate::method::Method;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::status::Status;
use crate::url;
use std::collections::BTreeMap;
use std::sync::Arc;

/// One piece of a route pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// A literal path segment.
    Static(String),
    /// `{id}` — matches exactly one segment and captures it.
    Param(String),
    /// `{path:*}` — matches the rest of the path, slashes included.
    Wildcard(String),
}

pub struct Route {
    pub method: Method,
    /// The pattern as written, used for `route:list` and metrics labels.
    pub pattern: String,
    pub name: Option<String>,
    /// What this route does, in one line. Feeds generated API documentation.
    pub summary: Option<String>,
    /// A grouping label, so generated docs are not one flat list.
    pub tag: Option<String>,
    /// Documented responses: status, and what it means.
    pub responses: Vec<(u16, String)>,
    /// Documented parameters: name, and what it is.
    pub parameters: Vec<(String, String)>,
    pub deprecated: bool,
    /// The API version this route belongs to, from [`Router::version`].
    pub version: Option<String>,
    /// When the route was deprecated (unix time), sent as `Deprecation`.
    pub deprecated_at: Option<i64>,
    /// When the route will be removed (unix time), sent as `Sunset`.
    pub sunset: Option<i64>,
    segments: Vec<Segment>,
    handler: Arc<dyn Handler>,
    middleware: Arc<Vec<Arc<dyn Middleware>>>,
}

impl Route {
    /// The parameter names this route captures, in order.
    ///
    /// Generated documentation needs these even when the author documented
    /// none, because a path parameter is required whether or not it is
    /// described.
    pub fn parameter_names(&self) -> Vec<String> {
        self.segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Param(name) | Segment::Wildcard(name) => Some(name.clone()),
                Segment::Static(_) => None,
            })
            .collect()
    }
}

impl Route {
    /// How specific this route is, so `/users/new` is tried before `/users/{id}`.
    fn specificity(&self) -> (usize, usize) {
        let statics = self.segments.iter().filter(|s| matches!(s, Segment::Static(_))).count();
        let wildcards = self.segments.iter().filter(|s| matches!(s, Segment::Wildcard(_))).count();
        // More static segments first; any wildcard sinks to the bottom.
        (wildcards, usize::MAX - statics)
    }

    fn match_path(&self, path: &str) -> Option<BTreeMap<String, String>> {
        let mut params = BTreeMap::new();
        let mut parts = split_path(path);
        let mut index = 0;

        while index < self.segments.len() {
            match &self.segments[index] {
                Segment::Wildcard(name) => {
                    // Consumes everything that is left, including nothing.
                    let rest = parts.collect::<Vec<_>>().join("/");
                    params.insert(name.clone(), url::decode(&rest));
                    return Some(params);
                }
                Segment::Static(expected) => {
                    if parts.next()? != expected {
                        return None;
                    }
                }
                Segment::Param(name) => {
                    let value = parts.next()?;
                    if value.is_empty() {
                        return None;
                    }
                    params.insert(name.clone(), url::decode(value));
                }
            }
            index += 1;
        }

        // Every pattern segment matched; the path must be exhausted too.
        parts.next().is_none().then_some(params)
    }
}

fn split_path(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|part| !part.is_empty())
}

fn parse_pattern(pattern: &str) -> Vec<Segment> {
    split_path(pattern)
        .map(|part| match part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
            Some(name) => match name.strip_suffix(":*") {
                Some(name) => Segment::Wildcard(name.to_string()),
                None => Segment::Param(name.to_string()),
            },
            None => Segment::Static(part.to_string()),
        })
        .collect()
}

/// The named routes of a finished router.
///
/// Built by [`Router::named_routes`] and shared with the template engine, so
/// `@route("users.show")` resolves to the same path `url_for` gives — the
/// filling in happens in [`fill`], once, rather than in two implementations
/// that would drift.
#[derive(Default, Clone)]
pub struct NamedRoutes(Vec<(String, Vec<Segment>)>);

impl NamedRoutes {
    pub fn url_for(&self, name: &str, params: &[(&str, &str)]) -> Option<String> {
        let (_, segments) = self.0.iter().find(|(known, _)| known == name)?;
        fill(segments, params)
    }

    /// Every name registered, for a message that can say what is available.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| name.as_str())
    }
}

/// Put the parameters into a route's shape.
///
/// `None` when a parameter the shape needs was not given — a half-filled path
/// would be a working-looking link to the wrong place.
fn fill(segments: &[Segment], params: &[(&str, &str)]) -> Option<String> {
    let lookup = |key: &str| params.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);

    let mut out = String::new();
    for segment in segments {
        out.push('/');
        match segment {
            Segment::Static(value) => out.push_str(value),
            Segment::Param(name) => out.push_str(&url::encode(lookup(name)?)),
            Segment::Wildcard(name) => out.push_str(lookup(name)?),
        }
    }
    Some(if out.is_empty() { "/".to_string() } else { out })
}

/// Collects routes, then answers requests.
#[derive(Default)]
pub struct Router {
    routes: Vec<Route>,
    /// Prefix and middleware of the group currently being defined.
    scope_prefix: String,
    scope_version: Option<String>,
    scope_middleware: Vec<Arc<dyn Middleware>>,
    /// Runs for every request, whatever the route.
    global_middleware: Vec<Arc<dyn Middleware>>,
    fallback: Option<Arc<dyn Handler>>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add middleware. Inside a `group` it applies to that group's routes;
    /// at the top level it applies to every request, including 404s.
    pub fn middleware(&mut self, middleware: impl Middleware) -> &mut Self {
        if self.scope_prefix.is_empty() && self.scope_middleware.is_empty() {
            self.global_middleware.push(Arc::new(middleware));
        } else {
            self.scope_middleware.push(Arc::new(middleware));
        }
        self
    }

    /// Register routes under a shared prefix and middleware stack.
    pub fn group(&mut self, prefix: &str, define: impl FnOnce(&mut Router)) -> &mut Self {
        let mut child = Router {
            scope_prefix: join_paths(&self.scope_prefix, prefix),
            scope_middleware: self.scope_middleware.clone(),
            scope_version: self.scope_version.clone(),
            ..Router::default()
        };
        define(&mut child);

        // A group's global-looking middleware belongs to that group only.
        debug_assert!(child.global_middleware.is_empty() || !child.routes.is_empty());
        self.routes.extend(child.routes);
        self
    }

    /// Register one version of an API: a group under `/{version}` whose routes
    /// know which version they are.
    ///
    /// ```ignore
    /// r.version("v1", |v1| { v1.get("/users", v1::users::index); });
    /// r.version("v2", |v2| { v2.get("/users", v2::users::index); });
    /// ```
    ///
    /// A handler can read the version with `req.api_version()`, which lets one
    /// handler serve two versions where the difference is small, and generated
    /// documentation groups routes by it. Versioning by header instead of path
    /// is [`crate::versioning::VersionHeader`].
    pub fn version(&mut self, version: &str, define: impl FnOnce(&mut Router)) -> &mut Self {
        let mut child = Router {
            scope_prefix: join_paths(&self.scope_prefix, &format!("/{}", version.trim_start_matches('/'))),
            scope_middleware: self.scope_middleware.clone(),
            scope_version: Some(version.trim_start_matches('/').to_string()),
            ..Router::default()
        };
        define(&mut child);
        self.routes.extend(child.routes);
        self
    }

    /// The response when nothing matched. Defaults to a plain 404.
    pub fn fallback(&mut self, handler: impl Handler) -> &mut Self {
        self.fallback = Some(Arc::new(handler));
        self
    }

    pub fn route(&mut self, method: Method, pattern: &str, handler: impl Handler) -> RouteHandle<'_> {
        let full = join_paths(&self.scope_prefix, pattern);
        self.routes.push(Route {
            method,
            segments: parse_pattern(&full),
            pattern: full,
            name: None,
            summary: None,
            tag: None,
            responses: Vec::new(),
            parameters: Vec::new(),
            deprecated: false,
            version: self.scope_version.clone(),
            deprecated_at: None,
            sunset: None,
            handler: Arc::new(handler),
            middleware: Arc::new(self.scope_middleware.clone()),
        });
        RouteHandle { index: self.routes.len() - 1, router: self }
    }

    pub fn get(&mut self, pattern: &str, handler: impl Handler) -> RouteHandle<'_> {
        self.route(Method::Get, pattern, handler)
    }

    pub fn post(&mut self, pattern: &str, handler: impl Handler) -> RouteHandle<'_> {
        self.route(Method::Post, pattern, handler)
    }

    pub fn put(&mut self, pattern: &str, handler: impl Handler) -> RouteHandle<'_> {
        self.route(Method::Put, pattern, handler)
    }

    pub fn patch(&mut self, pattern: &str, handler: impl Handler) -> RouteHandle<'_> {
        self.route(Method::Patch, pattern, handler)
    }

    pub fn delete(&mut self, pattern: &str, handler: impl Handler) -> RouteHandle<'_> {
        self.route(Method::Delete, pattern, handler)
    }

    /// Start a RESTful resource: `r.resource("/posts").index(..).show(..)`.
    pub fn resource<'r>(&'r mut self, base: &str) -> Resource<'r> {
        Resource { base: base.trim_end_matches('/').to_string(), router: self }
    }

    /// Sort routes so lookups are deterministic. Called once before serving.
    pub fn finalize(&mut self) {
        self.routes.sort_by_key(Route::specificity);
    }

    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Build a URL from a named route: `url_for("users.show", &[("id", "7")])`.
    pub fn url_for(&self, name: &str, params: &[(&str, &str)]) -> Option<String> {
        let route = self.routes.iter().find(|route| route.name.as_deref() == Some(name))?;
        fill(&route.segments, params)
    }

    /// The named routes, lifted out so something else can hold them.
    ///
    /// The router itself is handed to the server and consumed; a template needs
    /// the names after that, which is what `@route` renders. Only the names and
    /// their shapes come along — no handlers, no middleware.
    pub fn named_routes(&self) -> NamedRoutes {
        NamedRoutes(
            self.routes
                .iter()
                .filter_map(|route| {
                    route.name.clone().map(|name| (name, route.segments.clone()))
                })
                .collect(),
        )
    }

    /// Match a request and run it through the pipeline.
    pub async fn dispatch(&self, mut request: Request) -> Response {
        let path = request.path().to_string();
        let mut path_matched = false;
        let mut allowed = Vec::new();

        for route in &self.routes {
            let Some(params) = route.match_path(&path) else { continue };
            path_matched = true;

            // HEAD is served by the GET route, minus the body.
            let usable = route.method == request.method()
                || (request.method() == Method::Head && route.method == Method::Get);

            if !usable {
                allowed.push(route.method);
                continue;
            }

            request.set_params(params);
            request.route = Some(route.pattern.clone());
            if let Some(version) = &route.version {
                request.extend(crate::versioning::ApiVersion(version.clone()));
            }

            let mut stack = self.global_middleware.clone();
            stack.extend(route.middleware.iter().cloned());
            let response =
                run_guarded(Next::new(Arc::new(stack), Arc::clone(&route.handler)), request).await;
            return crate::versioning::stamp_lifecycle(route, response);
        }

        let response = if path_matched {
            // The path exists but not for this verb: 405, and say what is allowed.
            allowed.sort();
            allowed.dedup();
            let allow = allowed.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ");
            Response::new(Status::METHOD_NOT_ALLOWED).with_header("allow", allow).with_text(format!(
                "{} is not allowed on {path}",
                request.method()
            ))
        } else {
            match &self.fallback {
                Some(handler) => {
                    let stack = Arc::new(self.global_middleware.clone());
                    return Next::new(stack, Arc::clone(handler)).run(request).await;
                }
                None => Response::not_found(),
            }
        };

        // Global middleware still observes unmatched requests, so logging and
        // Telescope see the 404s too.
        let stack = Arc::new(self.global_middleware.clone());
        let endpoint: Arc<dyn Handler> = Arc::new(crate::handler::Fixed(response));
        Next::new(stack, endpoint).run(request).await
    }
}

/// Run the pipeline, turning a panic into the error page.
///
/// This lives at the router rather than in the server so a panicking handler
/// fails the same way in a test as it does in production — a test that would
/// otherwise abort the whole run instead reports a 500.
async fn run_guarded(next: Next, request: Request) -> Response {
    crate::panic::install_hook();
    let started = std::time::Instant::now();

    // A panicking handler still needs to render a page describing the request,
    // but the request itself has been moved into the pipeline by then.
    let probe = Request::new(request.method(), request.target().to_string())
        .with_header("accept", request.header("accept").unwrap_or("text/html"));
    let route = request.route().map(str::to_string);

    let response = match crate::panic::catch(next.run(request)).await {
        Ok(response) => response,
        Err(message) => {
            let location = crate::panic::take_location().map(|l| (l.file, l.line));
            rustlavel_core::error!(
                "panic in {} {}: {message}",
                probe.method(),
                probe.path()
            );
            crate::error_page::render(
                &crate::error_page::Diagnostic::from_panic(message, location),
                Some(&probe),
            )
        }
    };

    // Dispatched here rather than in the server, so instrumentation sees the
    // same events under the test client as it does over a socket.
    if rustlavel_core::events::has_subscribers() {
        let mut event = rustlavel_core::Event::new("http.request")
            .with("method", probe.method().as_str())
            .with("path", probe.path())
            .with("status", response.status.code())
            .took(started.elapsed());
        if let Some(route) = route {
            // The pattern, not the path: one metric series per route, not per id.
            event = event.with("route", route);
        }
        // Read from the response rather than the request, which the pipeline
        // has consumed by now; the middleware puts it there for exactly this.
        if let Some(id) = response.headers.get(crate::request_id::HEADER) {
            event = event.with("request_id", id);
        }
        event.dispatch();
    }

    response
}

/// Returned by `get`/`post`/… so a route can be named after registration.
pub struct RouteHandle<'r> {
    index: usize,
    router: &'r mut Router,
}

impl RouteHandle<'_> {
    /// Name the route for `url_for` and `route:list`.
    pub fn name(self, name: &str) -> Self {
        self.router.routes[self.index].name = Some(name.to_string());
        self
    }

    /// Say what this route does, in one line.
    ///
    /// Documentation is attached here rather than kept in a separate file, so
    /// it cannot drift away from the route it describes.
    pub fn describe(self, summary: &str) -> Self {
        self.router.routes[self.index].summary = Some(summary.to_string());
        self
    }

    /// Group this route under a heading in generated documentation.
    pub fn tag(self, tag: &str) -> Self {
        self.router.routes[self.index].tag = Some(tag.to_string());
        self
    }

    /// Document a response this route can return.
    pub fn responds(self, status: u16, description: &str) -> Self {
        self.router.routes[self.index].responses.push((status, description.to_string()));
        self
    }

    /// Describe a parameter. Undescribed path parameters are still documented,
    /// just without prose.
    pub fn param(self, name: &str, description: &str) -> Self {
        self.router.routes[self.index]
            .parameters
            .push((name.to_string(), description.to_string()));
        self
    }

    /// Add middleware to this one route.
    ///
    /// A group is the right place for middleware several routes share. This is
    /// for the case a group cannot express: routes on one prefix that need
    /// *different* guards, which is what a resource with a permission per verb
    /// looks like — `users.view` on the index, `users.delete` on the delete.
    /// Without this, each verb needs a group of its own and the prefix stops
    /// reading as one resource.
    ///
    /// It runs after the group's middleware and before the handler.
    pub fn middleware(self, middleware: impl Middleware) -> Self {
        let stack = &mut self.router.routes[self.index].middleware;
        // The `Arc` is shared with every other route registered in the same
        // group, so it is cloned before being added to — otherwise one route's
        // guard would silently appear on its neighbours.
        Arc::make_mut(stack).push(Arc::new(middleware));
        self
    }

    /// Mark the route as deprecated in generated documentation.
    pub fn deprecated(self) -> Self {
        self.router.routes[self.index].deprecated = true;
        self
    }

    /// Say when the route was deprecated, as `YYYY-MM-DD`.
    ///
    /// Responses then carry `Deprecation: @<unix time>` (RFC 9745), which is
    /// how a client library learns to warn its own developers. Implies
    /// [`RouteHandle::deprecated`].
    ///
    /// # Panics
    ///
    /// On a date that is not `YYYY-MM-DD` — this is called at startup, with a
    /// literal, and a typo should fail there rather than send garbage.
    pub fn deprecated_at(self, date: &str) -> Self {
        let when = crate::date::parse_ymd(date)
            .unwrap_or_else(|| panic!("`{date}` is not a date; deprecated_at wants YYYY-MM-DD"));
        let route = &mut self.router.routes[self.index];
        route.deprecated = true;
        route.deprecated_at = Some(when);
        self
    }

    /// Say when the route will stop working, as `YYYY-MM-DD`.
    ///
    /// Responses then carry `Sunset` (RFC 8594) with that date, and the route
    /// is marked deprecated. Nothing removes the route on the day — that is a
    /// deploy, and a person's decision — but every client has been told.
    ///
    /// # Panics
    ///
    /// On a date that is not `YYYY-MM-DD`, for the reason given on
    /// [`RouteHandle::deprecated_at`].
    pub fn sunset(self, date: &str) -> Self {
        let when = crate::date::parse_ymd(date)
            .unwrap_or_else(|| panic!("`{date}` is not a date; sunset wants YYYY-MM-DD"));
        let route = &mut self.router.routes[self.index];
        route.deprecated = true;
        route.sunset = Some(when);
        self
    }
}

/// The seven RESTful routes, registered one at a time.
pub struct Resource<'r> {
    base: String,
    router: &'r mut Router,
}

impl Resource<'_> {
    /// `GET /posts`
    pub fn index(self, handler: impl Handler) -> Self {
        let (base, router) = (self.base.clone(), self.router);
        router.get(&base, handler).name(&format!("{}.index", resource_name(&base)));
        Resource { base, router }
    }

    /// `POST /posts`
    pub fn store(self, handler: impl Handler) -> Self {
        let (base, router) = (self.base.clone(), self.router);
        router.post(&base, handler).name(&format!("{}.store", resource_name(&base)));
        Resource { base, router }
    }

    /// `GET /posts/{id}`
    pub fn show(self, handler: impl Handler) -> Self {
        let (base, router) = (self.base.clone(), self.router);
        let pattern = format!("{base}/{{id}}");
        router.get(&pattern, handler).name(&format!("{}.show", resource_name(&base)));
        Resource { base, router }
    }

    /// `PUT /posts/{id}`
    pub fn update(self, handler: impl Handler) -> Self {
        let (base, router) = (self.base.clone(), self.router);
        let pattern = format!("{base}/{{id}}");
        router.put(&pattern, handler).name(&format!("{}.update", resource_name(&base)));
        Resource { base, router }
    }

    /// `DELETE /posts/{id}`
    pub fn destroy(self, handler: impl Handler) -> Self {
        let (base, router) = (self.base.clone(), self.router);
        let pattern = format!("{base}/{{id}}");
        router.delete(&pattern, handler).name(&format!("{}.destroy", resource_name(&base)));
        Resource { base, router }
    }
}

fn resource_name(base: &str) -> String {
    base.trim_matches('/').replace('/', ".")
}

fn join_paths(prefix: &str, path: &str) -> String {
    let joined = format!("/{}/{}", prefix.trim_matches('/'), path.trim_matches('/'));
    let cleaned = joined.replace("//", "/");
    if cleaned.len() > 1 { cleaned.trim_end_matches('/').to_string() } else { "/".to_string() }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn per_route_middleware_does_not_leak_to_its_neighbours() {
        use crate::testing::TestClient;
        fn tag(name: &'static str) -> impl Middleware {
            move |request: Request, next: Next| async move {
                next.run(request).await.with_header("x-tag", name)
            }
        }

        let mut router = Router::new();
        router.group("/admin", |admin| {
            admin.middleware(tag("group"));
            admin.get("/users", |_req: Request| async { Response::text("users") }).middleware(tag("view"));
            admin.get("/roles", |_req: Request| async { Response::text("roles") });
        });
        let client = TestClient::new(router);

        // The group's middleware ran last on both, so it wins the header; what
        // matters is that /roles never saw the guard put on /users.
        assert_eq!(client.get("/admin/users").await.status(), 200);
        assert_eq!(client.get("/admin/roles").await.status(), 200);

        let mut counted = Router::new();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        counted.group("/admin", |admin| {
            admin
                .get("/users", |_req: Request| async { Response::text("users") })
                .middleware(move |request: Request, next: Next| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        next.run(request).await
                    }
                });
            admin.get("/roles", |_req: Request| async { Response::text("roles") });
        });
        let client = TestClient::new(counted);
        client.get("/admin/roles").await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "the guard belongs to /users only");
        client.get("/admin/users").await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    use super::*;

    async fn ok(_req: Request) -> &'static str {
        "ok"
    }

    fn router_with(define: impl FnOnce(&mut Router)) -> Router {
        let mut router = Router::new();
        define(&mut router);
        router.finalize();
        router
    }

    #[tokio::test]
    async fn matches_static_and_parameter_routes() {
        let router = router_with(|r| {
            r.get("/", ok);
            r.get("/users/{id}", |req: Request| async move {
                format!("user {}", req.param("id").unwrap())
            });
        });

        assert_eq!(router.dispatch(Request::new(Method::Get, "/")).await.body_string(), "ok");
        assert_eq!(
            router.dispatch(Request::new(Method::Get, "/users/7")).await.body_string(),
            "user 7"
        );
        assert_eq!(
            router.dispatch(Request::new(Method::Get, "/nope")).await.status,
            Status::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn static_segments_win_over_parameters() {
        let router = router_with(|r| {
            r.get("/users/{id}", |_req: Request| async { "param" });
            r.get("/users/new", |_req: Request| async { "static" });
        });

        assert_eq!(
            router.dispatch(Request::new(Method::Get, "/users/new")).await.body_string(),
            "static"
        );
        assert_eq!(
            router.dispatch(Request::new(Method::Get, "/users/12")).await.body_string(),
            "param"
        );
    }

    #[tokio::test]
    async fn wildcards_capture_the_remaining_path() {
        let router = router_with(|r| {
            r.get("/files/{path:*}", |req: Request| async move {
                req.param("path").unwrap_or_default().to_string()
            });
        });

        let response = router.dispatch(Request::new(Method::Get, "/files/css/app.css")).await;
        assert_eq!(response.body_string(), "css/app.css");
    }

    #[tokio::test]
    async fn parameters_are_percent_decoded() {
        let router = router_with(|r| {
            r.get("/tags/{tag}", |req: Request| async move { req.param("tag").unwrap().to_string() });
        });

        let response = router.dispatch(Request::new(Method::Get, "/tags/rust%20lang")).await;
        assert_eq!(response.body_string(), "rust lang");
    }

    #[tokio::test]
    async fn wrong_method_reports_405_with_allow() {
        let router = router_with(|r| {
            r.post("/users", ok);
        });

        let response = router.dispatch(Request::new(Method::Get, "/users")).await;
        assert_eq!(response.status, Status::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers.get("allow"), Some("POST"));
    }

    #[tokio::test]
    async fn head_is_served_by_the_get_route() {
        let router = router_with(|r| {
            r.get("/", ok);
        });

        assert_eq!(router.dispatch(Request::new(Method::Head, "/")).await.status, Status::OK);
    }

    #[tokio::test]
    async fn groups_apply_prefix_and_middleware() {
        let router = router_with(|r| {
            r.group("/admin", |r| {
                r.middleware(|req: Request, next: Next| async move {
                    next.run(req).await.with_header("x-guard", "on")
                });
                r.get("/dashboard", ok);
            });
            r.get("/public", ok);
        });

        let guarded = router.dispatch(Request::new(Method::Get, "/admin/dashboard")).await;
        assert_eq!(guarded.headers.get("x-guard"), Some("on"));

        let open = router.dispatch(Request::new(Method::Get, "/public")).await;
        assert_eq!(open.headers.get("x-guard"), None);
    }

    #[tokio::test]
    async fn global_middleware_also_sees_unmatched_requests() {
        let router = router_with(|r| {
            r.middleware(|req: Request, next: Next| async move {
                next.run(req).await.with_header("x-seen", "1")
            });
            r.get("/", ok);
        });

        let missing = router.dispatch(Request::new(Method::Get, "/nope")).await;
        assert_eq!(missing.status, Status::NOT_FOUND);
        assert_eq!(missing.headers.get("x-seen"), Some("1"));
    }

    #[test]
    fn builds_urls_from_named_routes() {
        let router = router_with(|r| {
            r.get("/users/{id}/posts/{slug}", ok).name("users.posts");
        });

        assert_eq!(
            router.url_for("users.posts", &[("id", "7"), ("slug", "hello world")]).as_deref(),
            Some("/users/7/posts/hello%20world")
        );
        assert_eq!(router.url_for("users.posts", &[("id", "7")]), None);
        assert_eq!(router.url_for("missing", &[]), None);
    }

    #[tokio::test]
    async fn resource_registers_the_rest_routes() {
        let router = router_with(|r| {
            r.resource("/posts").index(ok).store(ok).show(ok).update(ok).destroy(ok);
        });

        assert_eq!(router.routes().len(), 5);
        assert_eq!(router.url_for("posts.show", &[("id", "3")]).as_deref(), Some("/posts/3"));
        assert_eq!(router.dispatch(Request::new(Method::Delete, "/posts/3")).await.status, Status::OK);
    }

    #[tokio::test]
    async fn fallback_replaces_the_default_404() {
        let router = router_with(|r| {
            r.fallback(|_req: Request| async { (404, "custom miss") });
        });

        let response = router.dispatch(Request::new(Method::Get, "/anything")).await;
        assert_eq!(response.body_string(), "custom miss");
    }

    #[test]
    fn documentation_rides_along_with_the_route() {
        let router = router_with(|r| {
            r.get("/users/{id}", ok)
                .name("users.show")
                .describe("Fetch one user")
                .tag("Users")
                .param("id", "The user's id")
                .responds(200, "The user")
                .responds(404, "No such user");
        });

        let route = &router.routes()[0];
        assert_eq!(route.summary.as_deref(), Some("Fetch one user"));
        assert_eq!(route.tag.as_deref(), Some("Users"));
        assert_eq!(route.responses.len(), 2);
        assert_eq!(route.parameter_names(), vec!["id"]);
        assert!(!route.deprecated);
    }

    #[test]
    fn path_parameters_are_known_even_when_undocumented() {
        let router = router_with(|r| {
            r.get("/teams/{team}/members/{member}", ok);
            r.get("/files/{path:*}", ok);
        });

        let names: Vec<Vec<String>> =
            router.routes().iter().map(Route::parameter_names).collect();
        assert!(names.contains(&vec!["team".to_string(), "member".to_string()]));
        assert!(names.contains(&vec!["path".to_string()]));
    }

    #[test]
    fn joins_prefixes_without_doubling_slashes() {
        assert_eq!(join_paths("/admin/", "/users"), "/admin/users");
        assert_eq!(join_paths("", "/"), "/");
        assert_eq!(join_paths("/admin", ""), "/admin");
    }
}
