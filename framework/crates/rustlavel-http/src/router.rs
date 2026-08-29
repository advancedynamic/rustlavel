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
    segments: Vec<Segment>,
    handler: Arc<dyn Handler>,
    middleware: Arc<Vec<Arc<dyn Middleware>>>,
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

/// Collects routes, then answers requests.
#[derive(Default)]
pub struct Router {
    routes: Vec<Route>,
    /// Prefix and middleware of the group currently being defined.
    scope_prefix: String,
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
            ..Router::default()
        };
        define(&mut child);

        // A group's global-looking middleware belongs to that group only.
        debug_assert!(child.global_middleware.is_empty() || !child.routes.is_empty());
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
        let lookup = |key: &str| params.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);

        let mut out = String::new();
        for segment in &route.segments {
            out.push('/');
            match segment {
                Segment::Static(value) => out.push_str(value),
                Segment::Param(name) => out.push_str(&url::encode(lookup(name)?)),
                Segment::Wildcard(name) => out.push_str(lookup(name)?),
            }
        }
        Some(if out.is_empty() { "/".to_string() } else { out })
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

            let mut stack = self.global_middleware.clone();
            stack.extend(route.middleware.iter().cloned());
            return run_guarded(Next::new(Arc::new(stack), Arc::clone(&route.handler)), request).await;
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

    // A panicking handler still needs to render a page describing the request,
    // but the request itself has been moved into the pipeline by then.
    let probe = Request::new(request.method(), request.target().to_string())
        .with_header("accept", request.header("accept").unwrap_or("text/html"));

    match crate::panic::catch(next.run(request)).await {
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
    }
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
    fn joins_prefixes_without_doubling_slashes() {
        assert_eq!(join_paths("/admin/", "/users"), "/admin/users");
        assert_eq!(join_paths("", "/"), "/");
        assert_eq!(join_paths("/admin", ""), "/admin");
    }
}
