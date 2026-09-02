//! Cross-Origin Resource Sharing.
//!
//! A browser will not let a page on one origin read a response from another
//! unless the response says so. Without this middleware an API works from
//! `curl`, from a mobile app, and from a server — and fails, silently, from
//! every single-page application that is not served from the same host.
//!
//! The shape follows Laravel's `config/cors.php`, key for key, so a person who
//! has configured CORS there already knows how to configure it here:
//!
//! ```ignore
//! App::new()?.middleware(Cors::from_config(app.config()))
//! // or, in code:
//! App::new()?.middleware(
//!     Cors::new()
//!         .allow_origins(["https://app.example.com"])
//!         .allow_credentials(),
//! )
//! ```
//!
//! Two things are worth knowing before reaching for [`Cors::permissive`]. A
//! wildcard origin cannot be combined with credentials — the specification
//! forbids `Access-Control-Allow-Origin: *` on a response that also allows
//! cookies — so with credentials on, this echoes the requesting origin instead,
//! which allows *every* origin to make credentialled requests. That is rarely
//! what anyone means. Name the origins.

use crate::handler::BoxFuture;
use crate::method::Method;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use rustlavel_core::Config;
use std::sync::Arc;
use std::time::Duration;

/// Which origins may read responses.
#[derive(Clone)]
enum Origins {
    Any,
    /// Exact origins, or patterns with one `*` (`https://*.example.com`).
    List(Vec<String>),
    Predicate(Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

/// Which request headers a preflight may ask for.
#[derive(Clone)]
enum AllowedHeaders {
    /// Echo whatever the browser asked for.
    Any,
    List(Vec<String>),
}

#[derive(Clone)]
pub struct Cors {
    /// Only requests under these paths are treated as CORS requests; the rest
    /// pass through untouched. `*` means everything, and is the default.
    paths: Vec<String>,
    origins: Origins,
    methods: Vec<Method>,
    allowed_headers: AllowedHeaders,
    exposed_headers: Vec<String>,
    credentials: bool,
    max_age: Option<Duration>,
}

impl Default for Cors {
    fn default() -> Self {
        Self::new()
    }
}

impl Cors {
    /// Deny everything until told otherwise: no origins, no credentials.
    ///
    /// Starting closed means a missing line of configuration fails a request
    /// in the browser console, where somebody will see it, rather than opening
    /// the API to the world.
    pub fn new() -> Self {
        Cors {
            paths: vec!["*".to_string()],
            origins: Origins::List(Vec::new()),
            methods: vec![
                Method::Get,
                Method::Head,
                Method::Post,
                Method::Put,
                Method::Patch,
                Method::Delete,
            ],
            allowed_headers: AllowedHeaders::Any,
            exposed_headers: Vec::new(),
            credentials: false,
            max_age: None,
        }
    }

    /// Any origin, any method, any header, no credentials.
    ///
    /// Right for a genuinely public API. Wrong for anything that reads a
    /// session cookie, because the moment `allow_credentials` is added this
    /// becomes "every site on the internet may act as the logged-in user".
    pub fn permissive() -> Self {
        Cors { origins: Origins::Any, ..Cors::new() }
    }

    /// Read `config/cors.json`, with Laravel's key names.
    ///
    /// ```json
    /// {
    ///   "paths": ["api/*"],
    ///   "allowed_origins": "${CORS_ALLOWED_ORIGINS:*}",
    ///   "allowed_methods": ["*"],
    ///   "allowed_headers": ["*"],
    ///   "exposed_headers": [],
    ///   "max_age": 0,
    ///   "supports_credentials": false
    /// }
    /// ```
    ///
    /// Every list accepts either a JSON array or one comma-separated string,
    /// which is what makes it settable from `.env`: a variable can only hold a
    /// string.
    pub fn from_config(config: &Config) -> Self {
        let mut cors = Cors::new();

        let paths = config.list("cors.paths");
        if !paths.is_empty() {
            cors.paths = paths;
        }

        let origins = config.list("cors.allowed_origins");
        cors.origins =
            if origins.iter().any(|o| o == "*") { Origins::Any } else { Origins::List(origins) };

        let methods = config.list("cors.allowed_methods");
        if !methods.is_empty() && !methods.iter().any(|m| m == "*") {
            cors.methods = methods.iter().filter_map(|m| Method::parse(m)).collect();
        }

        let headers = config.list("cors.allowed_headers");
        if !headers.is_empty() && !headers.iter().any(|h| h == "*") {
            cors.allowed_headers = AllowedHeaders::List(headers);
        }

        cors.exposed_headers = config.list("cors.exposed_headers");
        cors.credentials = config.bool("cors.supports_credentials", false);

        let max_age = config.int("cors.max_age", 0);
        if max_age > 0 {
            cors.max_age = Some(Duration::from_secs(max_age as u64));
        }

        cors
    }

    /// Apply only under these paths, Laravel-style: `api/*`, `sanctum/csrf-cookie`.
    pub fn paths<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// Allow these origins. A single `*` inside a pattern matches one label:
    /// `https://*.example.com` allows `https://app.example.com` and not
    /// `https://example.com.evil.net`.
    pub fn allow_origins<I, S>(mut self, origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.origins = Origins::List(origins.into_iter().map(Into::into).collect());
        self
    }

    pub fn allow_any_origin(mut self) -> Self {
        self.origins = Origins::Any;
        self
    }

    /// Decide per origin — for a list that lives in a database, say.
    pub fn allow_origin_if(mut self, allow: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        self.origins = Origins::Predicate(Arc::new(allow));
        self
    }

    pub fn allow_methods(mut self, methods: impl IntoIterator<Item = Method>) -> Self {
        self.methods = methods.into_iter().collect();
        self
    }

    pub fn allow_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_headers = AllowedHeaders::List(headers.into_iter().map(Into::into).collect());
        self
    }

    /// Let scripts read these response headers. By default a browser exposes
    /// only the handful the specification calls "safelisted" — a custom
    /// `X-Request-Id` or `X-RateLimit-Remaining` is invisible until listed.
    pub fn expose_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exposed_headers = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Allow cookies and `Authorization` headers to travel with the request.
    pub fn allow_credentials(mut self) -> Self {
        self.credentials = true;
        self
    }

    /// How long a browser may cache a preflight answer.
    pub fn max_age(mut self, duration: Duration) -> Self {
        self.max_age = Some(duration);
        self
    }

    fn applies_to(&self, path: &str) -> bool {
        let path = path.trim_start_matches('/');
        self.paths.iter().any(|pattern| {
            let pattern = pattern.trim_start_matches('/');
            match pattern.strip_suffix('*') {
                Some(prefix) => pattern == "*" || path.starts_with(prefix),
                None => path == pattern,
            }
        })
    }

    fn allows_origin(&self, origin: &str) -> bool {
        match &self.origins {
            Origins::Any => true,
            Origins::List(list) => list.iter().any(|pattern| origin_matches(pattern, origin)),
            Origins::Predicate(allow) => allow(origin),
        }
    }

    /// The `Access-Control-Allow-Origin` value, or `None` when the origin is
    /// not allowed and the response should say nothing at all.
    fn allow_origin_value(&self, origin: &str) -> Option<String> {
        if !self.allows_origin(origin) {
            return None;
        }
        // `*` is not permitted alongside credentials (Fetch §3.2.3), so the
        // origin is echoed. Being a wildcard in disguise is documented on the type.
        let wildcard = matches!(self.origins, Origins::Any) && !self.credentials;
        Some(if wildcard { "*".to_string() } else { origin.to_string() })
    }

    fn preflight(&self, request: &Request, origin: &str) -> Response {
        // A preflight never reaches a handler, so it always ends here: 204 and,
        // if the origin is allowed, the headers the browser is asking about.
        // Answering an unknown origin with a bare 204 rather than an error is
        // deliberate — the browser blocks either way, and this reveals nothing.
        let mut response = Response::no_content();
        vary(&mut response, "origin");
        vary(&mut response, "access-control-request-method");
        vary(&mut response, "access-control-request-headers");

        let Some(allow_origin) = self.allow_origin_value(origin) else { return response };
        response.headers.set("access-control-allow-origin", allow_origin);

        let methods = self.methods.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ");
        response.headers.set("access-control-allow-methods", methods);

        let allow_headers = match &self.allowed_headers {
            AllowedHeaders::List(list) => Some(list.join(", ")),
            // Echo what was asked for: this is what `*` means in practice, since
            // a literal `*` is ignored by browsers when credentials are on.
            AllowedHeaders::Any => {
                request.header("access-control-request-headers").map(str::to_string)
            }
        };
        if let Some(headers) = allow_headers.filter(|h| !h.is_empty()) {
            response.headers.set("access-control-allow-headers", headers);
        }

        if self.credentials {
            response.headers.set("access-control-allow-credentials", "true");
        }
        if let Some(max_age) = self.max_age {
            response.headers.set("access-control-max-age", max_age.as_secs().to_string());
        }
        response
    }

    fn decorate(&self, mut response: Response, origin: &str) -> Response {
        vary(&mut response, "origin");
        let Some(allow_origin) = self.allow_origin_value(origin) else { return response };

        response.headers.set("access-control-allow-origin", allow_origin);
        if self.credentials {
            response.headers.set("access-control-allow-credentials", "true");
        }
        if !self.exposed_headers.is_empty() {
            response.headers.set("access-control-expose-headers", self.exposed_headers.join(", "));
        }
        response
    }
}

impl Middleware for Cors {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        // Only a request carrying `Origin` is cross-origin; everything else is
        // none of this middleware's business.
        let origin = match request.header("origin") {
            Some(origin) if self.applies_to(request.path()) => origin.to_string(),
            _ => return next.run(request),
        };

        let is_preflight = request.method() == Method::Options
            && request.header("access-control-request-method").is_some();
        if is_preflight {
            let response = self.preflight(&request, &origin);
            return Box::pin(async move { response });
        }

        let cors = self.clone();
        Box::pin(async move {
            let response = next.run(request).await;
            cors.decorate(response, &origin)
        })
    }
}

/// Add to `Vary` without clobbering what a handler already put there.
fn vary(response: &mut Response, name: &str) {
    let existing = response.headers.get("vary").unwrap_or("").to_string();
    if existing.split(',').any(|v| v.trim().eq_ignore_ascii_case(name)) {
        return;
    }
    let value = if existing.is_empty() { name.to_string() } else { format!("{existing}, {name}") };
    response.headers.set("vary", value);
}

/// Exact match, or a pattern with one `*` standing for a single DNS label.
///
/// One label only, deliberately. `https://*.example.com` must not match
/// `https://a.b.example.com` — that is a different origin, possibly hosted by a
/// different team — and it certainly must not match anything that merely ends
/// in `example.com`.
fn origin_matches(pattern: &str, origin: &str) -> bool {
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return pattern.eq_ignore_ascii_case(origin);
    };
    let origin_lower = origin.to_ascii_lowercase();
    let (prefix, suffix) = (prefix.to_ascii_lowercase(), suffix.to_ascii_lowercase());

    let Some(rest) = origin_lower.strip_prefix(prefix.as_str()) else { return false };
    let Some(label) = rest.strip_suffix(suffix.as_str()) else { return false };
    !label.is_empty() && !label.contains(['.', '/', ':'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::Router;
    use rustlavel_core::Json;
    use crate::testing::TestClient;

    fn client(cors: Cors) -> TestClient {
        let mut router = Router::new();
        router.middleware(cors);
        router.get("/api/users", |_req: Request| async { Response::text("users") });
        router.post("/api/users", |_req: Request| async { Response::text("created") });
        router.get("/web/page", |_req: Request| async {
            Response::text("page").with_header("vary", "Accept-Encoding")
        });
        TestClient::new(router)
    }

    fn preflight(path: &str, origin: &str) -> Request {
        Request::new(Method::Options, path)
            .with_header("origin", origin)
            .with_header("access-control-request-method", "POST")
            .with_header("access-control-request-headers", "content-type, x-custom")
    }

    #[tokio::test]
    async fn a_request_without_an_origin_is_left_alone() {
        let response = client(Cors::permissive()).get("/api/users").await;
        let response = response.assert_ok();
        assert_eq!(response.header("access-control-allow-origin"), None);
    }

    #[tokio::test]
    async fn a_listed_origin_is_allowed_and_echoed() {
        let cors = Cors::new().allow_origins(["https://app.example.com"]);
        let request = Request::new(Method::Get, "/api/users").with_header("origin", "https://app.example.com");
        let response = client(cors).send(request).await;

        let response = response.assert_ok();
        assert_eq!(response.header("access-control-allow-origin"), Some("https://app.example.com"));
        assert_eq!(response.header("vary"), Some("origin"));
    }

    #[tokio::test]
    async fn an_unlisted_origin_gets_no_cors_headers_at_all() {
        let cors = Cors::new().allow_origins(["https://app.example.com"]);
        let request = Request::new(Method::Get, "/api/users").with_header("origin", "https://evil.example");
        let response = client(cors).send(request).await;

        // The handler still runs — CORS is enforced by the browser, not here —
        // but the browser gets nothing it could use to release the response.
        let response = response.assert_ok();
        assert_eq!(response.header("access-control-allow-origin"), None);
        assert_eq!(response.header("vary"), Some("origin"), "caches must still key on origin");
    }

    #[tokio::test]
    async fn permissive_answers_with_a_wildcard() {
        let request = Request::new(Method::Get, "/api/users").with_header("origin", "https://anyone.example");
        let response = client(Cors::permissive()).send(request).await;
        assert_eq!(response.header("access-control-allow-origin"), Some("*"));
        assert_eq!(response.header("access-control-allow-credentials"), None);
    }

    #[tokio::test]
    async fn credentials_forbid_the_wildcard_so_the_origin_is_echoed() {
        let cors = Cors::permissive().allow_credentials();
        let request = Request::new(Method::Get, "/api/users").with_header("origin", "https://anyone.example");
        let response = client(cors).send(request).await;

        assert_eq!(response.header("access-control-allow-origin"), Some("https://anyone.example"));
        assert_eq!(response.header("access-control-allow-credentials"), Some("true"));
    }

    #[tokio::test]
    async fn a_preflight_is_answered_without_reaching_the_handler() {
        let cors = Cors::new()
            .allow_origins(["https://app.example.com"])
            .allow_methods([Method::Get, Method::Post])
            .max_age(Duration::from_secs(600));
        let response = client(cors).send(preflight("/api/users", "https://app.example.com")).await;

        let response = response.assert_status(204);
        assert_eq!(response.body(), "", "a preflight has no body and never ran the handler");
        assert_eq!(response.header("access-control-allow-origin"), Some("https://app.example.com"));
        assert_eq!(response.header("access-control-allow-methods"), Some("GET, POST"));
        assert_eq!(
            response.header("access-control-allow-headers"),
            Some("content-type, x-custom"),
            "with no list configured, the requested headers are echoed"
        );
        assert_eq!(response.header("access-control-max-age"), Some("600"));
        assert_eq!(
            response.header("vary"),
            Some("origin, access-control-request-method, access-control-request-headers")
        );
    }

    #[tokio::test]
    async fn a_preflight_for_a_path_with_no_options_route_still_succeeds() {
        // Without this middleware the router would answer 405, and the browser
        // would refuse to send the real request. This is the whole point.
        client(Cors::permissive()).send(preflight("/api/users", "https://x.example")).await.assert_status(204);
    }

    #[tokio::test]
    async fn a_preflight_from_a_forbidden_origin_is_a_bare_204() {
        let cors = Cors::new().allow_origins(["https://app.example.com"]);
        let response = client(cors).send(preflight("/api/users", "https://evil.example")).await;

        let response = response.assert_status(204);
        assert_eq!(response.header("access-control-allow-origin"), None);
        assert_eq!(response.header("access-control-allow-methods"), None);
    }

    #[tokio::test]
    async fn configured_headers_are_listed_rather_than_echoed() {
        let cors = Cors::permissive().allow_headers(["Content-Type", "Authorization"]);
        let response = client(cors).send(preflight("/api/users", "https://x.example")).await;
        assert_eq!(response.header("access-control-allow-headers"), Some("Content-Type, Authorization"));
    }

    #[tokio::test]
    async fn plain_options_without_a_request_method_is_not_a_preflight() {
        let request = Request::new(Method::Options, "/api/users").with_header("origin", "https://x.example");
        let response = client(Cors::permissive()).send(request).await;
        // No handler for OPTIONS, so the router's 405 comes through — decorated.
        let response = response.assert_status(405);
        assert_eq!(response.header("access-control-allow-origin"), Some("*"));
    }

    #[tokio::test]
    async fn exposed_headers_are_named_on_real_responses() {
        let cors = Cors::permissive().expose_headers(["X-Request-Id", "X-RateLimit-Remaining"]);
        let request = Request::new(Method::Get, "/api/users").with_header("origin", "https://x.example");
        let response = client(cors).send(request).await;
        assert_eq!(
            response.header("access-control-expose-headers"),
            Some("X-Request-Id, X-RateLimit-Remaining")
        );
    }

    #[tokio::test]
    async fn vary_is_appended_not_replaced() {
        let request = Request::new(Method::Get, "/web/page").with_header("origin", "https://x.example");
        let response = client(Cors::permissive()).send(request).await;
        assert_eq!(response.header("vary"), Some("Accept-Encoding, origin"));
    }

    #[tokio::test]
    async fn paths_restrict_where_cors_applies() {
        let cors = Cors::permissive().paths(["api/*"]);
        let client = client(cors);

        let api = Request::new(Method::Get, "/api/users").with_header("origin", "https://x.example");
        assert_eq!(client.send(api).await.header("access-control-allow-origin"), Some("*"));

        let web = Request::new(Method::Get, "/web/page").with_header("origin", "https://x.example");
        assert_eq!(client.send(web).await.header("access-control-allow-origin"), None);
    }

    #[test]
    fn a_wildcard_matches_exactly_one_label() {
        assert!(origin_matches("https://*.example.com", "https://app.example.com"));
        assert!(origin_matches("https://*.example.com", "HTTPS://APP.example.com"));
        assert!(!origin_matches("https://*.example.com", "https://example.com"));
        assert!(!origin_matches("https://*.example.com", "https://a.b.example.com"));
        assert!(!origin_matches("https://*.example.com", "https://example.com.evil.net"));
        assert!(!origin_matches("https://*.example.com", "http://app.example.com"));
        assert!(origin_matches("https://app.example.com", "https://app.example.com"));
        assert!(!origin_matches("https://app.example.com", "https://app.example.com.evil"));
    }

    #[tokio::test]
    async fn a_predicate_decides_per_origin() {
        let cors = Cors::new().allow_origin_if(|origin| origin.ends_with(".trusted.example"));
        let client = client(cors);

        let yes = Request::new(Method::Get, "/api/users").with_header("origin", "https://a.trusted.example");
        assert_eq!(
            client.send(yes).await.header("access-control-allow-origin"),
            Some("https://a.trusted.example")
        );
        let no = Request::new(Method::Get, "/api/users").with_header("origin", "https://a.other.example");
        assert_eq!(client.send(no).await.header("access-control-allow-origin"), None);
    }

    #[test]
    fn from_config_reads_laravels_keys_and_env_friendly_strings() {
        let config = Config::new();
        config.set("cors.paths", Json::from(vec!["api/*"]));
        // A comma-separated string, as a `.env` variable would deliver it.
        config.set("cors.allowed_origins", "https://a.example, https://b.example");
        config.set("cors.allowed_methods", Json::from(vec!["GET", "POST"]));
        config.set("cors.allowed_headers", Json::from(vec!["*"]));
        config.set("cors.exposed_headers", "X-Request-Id");
        config.set("cors.max_age", Json::from(3600_i64));
        config.set("cors.supports_credentials", true);

        let cors = Cors::from_config(&config);
        assert!(cors.allows_origin("https://b.example"));
        assert!(!cors.allows_origin("https://c.example"));
        assert_eq!(cors.methods, vec![Method::Get, Method::Post]);
        assert!(matches!(cors.allowed_headers, AllowedHeaders::Any));
        assert_eq!(cors.exposed_headers, vec!["X-Request-Id"]);
        assert_eq!(cors.max_age, Some(Duration::from_secs(3600)));
        assert!(cors.credentials);
        assert!(cors.applies_to("/api/users"));
        assert!(!cors.applies_to("/web"));
    }

    #[test]
    fn from_config_with_nothing_set_denies_every_origin() {
        let cors = Cors::from_config(&Config::new());
        assert!(!cors.allows_origin("https://anyone.example"));
    }

    #[test]
    fn a_star_origin_in_config_means_any() {
        let config = Config::new();
        config.set("cors.allowed_origins", "*");
        assert!(Cors::from_config(&config).allows_origin("https://anyone.example"));
    }
}
