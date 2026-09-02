//! API versions, and telling clients when one is going away.
//!
//! Two ways to version, because APIs use both. By path — `/v1/users`,
//! `/v2/users` — with [`Router::version`](crate::Router::version), which is
//! the visible, cacheable, curl-friendly form. Or by header — `X-API-Version:
//! 2`, `Accept: application/vnd.example.v2+json` — with [`VersionHeader`],
//! for APIs that want one URL per resource forever. Either way the handler
//! asks `req.api_version()` and gets the same answer.
//!
//! The other half is the lifecycle. A version is not retired by deleting it;
//! it is retired by telling every client, for months, that the day is coming.
//! [`RouteHandle::deprecated_at`](crate::RouteHandle::deprecated_at) sends
//! `Deprecation` (RFC 9745) and [`RouteHandle::sunset`](crate::RouteHandle::sunset)
//! sends `Sunset` (RFC 8594), on every response from the route, so a client
//! library can log a warning its own developers will see.

use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::router::Route;
use crate::status::Status;
use rustlavel_core::Json;

/// The version a request is for, attached as an extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiVersion(pub String);

/// Add `Deprecation` and `Sunset` to a response from a route that has them.
pub(crate) fn stamp_lifecycle(route: &Route, mut response: Response) -> Response {
    if let Some(at) = route.deprecated_at {
        // RFC 9745 §2: a structured-field Date, which is `@` and unix seconds.
        response.headers.set("deprecation", format!("@{at}"));
    }
    if let Some(at) = route.sunset {
        // RFC 8594 §3: an HTTP-date.
        response.headers.set("sunset", crate::date::http_date(at));
    }
    response
}

/// Read the API version from a header.
///
/// ```ignore
/// App::new()?.middleware(
///     VersionHeader::new("X-API-Version")
///         .default("2")
///         .allow(["1", "2"]),
/// )
/// ```
///
/// A vendor media type in `Accept` — `application/vnd.example.v2+json` — is
/// read as well when [`VersionHeader::from_accept`] names the vendor prefix.
/// A request naming a version that is not allowed is a 400 that lists what
/// is, rather than a silent fall-through to the default: a client that asked
/// for v3 and got v1's shape would be worse off than one that got an error.
#[derive(Debug, Clone)]
pub struct VersionHeader {
    header: String,
    accept_vendor: Option<String>,
    default: Option<String>,
    allowed: Option<Vec<String>>,
}

impl VersionHeader {
    pub fn new(header: &str) -> Self {
        VersionHeader {
            header: header.to_ascii_lowercase(),
            accept_vendor: None,
            default: None,
            allowed: None,
        }
    }

    /// The version to assume when the client names none.
    ///
    /// Stripe pins this per account; most APIs pin it to the oldest still
    /// supported, so a client written against it keeps working unchanged.
    pub fn default(mut self, version: &str) -> Self {
        self.default = Some(version.to_string());
        self
    }

    /// Also accept `Accept: application/vnd.{vendor}.v{N}+json`.
    pub fn from_accept(mut self, vendor: &str) -> Self {
        self.accept_vendor = Some(vendor.to_string());
        self
    }

    /// The versions that exist. Anything else is a 400.
    pub fn allow<I, S>(mut self, versions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed = Some(versions.into_iter().map(Into::into).collect());
        self
    }

    fn requested(&self, request: &Request) -> Option<String> {
        if let Some(version) = request.header(&self.header) {
            let version = version.trim();
            if !version.is_empty() {
                return Some(version.to_string());
            }
        }
        let vendor = self.accept_vendor.as_deref()?;
        let accept = request.header("accept")?;
        // application/vnd.example.v2+json → "2"
        let marker = format!("application/vnd.{vendor}.v");
        accept.split(',').find_map(|part| {
            let rest = part.trim().strip_prefix(marker.as_str())?;
            let version: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '.').collect();
            (!version.is_empty()).then_some(version)
        })
    }
}

impl Middleware for VersionHeader {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        // A version chosen by the route's path wins over a header; the URL is
        // the more specific statement of what the client asked for.
        if request.api_version().is_some() {
            return next.run(request);
        }

        let version = self.requested(&request).or_else(|| self.default.clone());

        if let (Some(version), Some(allowed)) = (&version, &self.allowed)
            && !allowed.contains(version)
        {
            let header = self.header.clone();
            let allowed = allowed.join(", ");
            let version = version.clone();
            return Box::pin(async move {
                Response::new(Status::BAD_REQUEST).with_json(Json::object([
                    ("message", Json::from(format!("API version `{version}` does not exist."))),
                    ("header", Json::from(header)),
                    ("available", Json::from(allowed)),
                ]))
            });
        }

        if let Some(version) = version {
            request.extend(ApiVersion(version));
        }
        next.run(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;
    use crate::router::Router;
    use crate::testing::TestClient;

    fn echo(req: Request) -> impl std::future::Future<Output = Response> {
        let version = req.api_version().unwrap_or("none").to_string();
        async move { Response::text(version) }
    }

    #[tokio::test]
    async fn path_versions_prefix_routes_and_name_themselves() {
        let mut router = Router::new();
        router.version("v1", |v1| {
            v1.get("/users", echo);
        });
        router.version("v2", |v2| {
            v2.get("/users", echo);
            v2.group("/admin", |admin| {
                admin.get("/stats", echo);
            });
        });
        let client = TestClient::new(router);

        assert_eq!(client.get("/v1/users").await.body(), "v1");
        assert_eq!(client.get("/v2/users").await.body(), "v2");
        assert_eq!(client.get("/v2/admin/stats").await.body(), "v2", "a group inside keeps the version");
        client.get("/users").await.assert_not_found();
    }

    #[tokio::test]
    async fn a_sunset_route_says_so_on_every_response() {
        let mut router = Router::new();
        router.version("v1", |v1| {
            v1.get("/users", echo).deprecated_at("2026-06-01").sunset("2027-01-01");
        });
        router.get("/fresh", echo);
        let client = TestClient::new(router);

        let old = client.get("/v1/users").await;
        assert_eq!(old.header("deprecation"), Some("@1780272000"));
        assert_eq!(old.header("sunset"), Some("Fri, 01 Jan 2027 00:00:00 GMT"));

        let fresh = client.get("/fresh").await;
        assert_eq!(fresh.header("deprecation"), None);
        assert_eq!(fresh.header("sunset"), None);
    }

    #[test]
    fn sunset_marks_the_route_deprecated_for_the_docs() {
        let mut router = Router::new();
        router.get("/old", echo).sunset("2027-01-01");
        assert!(router.routes()[0].deprecated);
    }

    #[test]
    #[should_panic(expected = "wants YYYY-MM-DD")]
    fn a_typo_in_a_sunset_date_fails_at_startup() {
        let mut router = Router::new();
        router.get("/old", echo).sunset("next year");
    }

    fn header_client(header: VersionHeader) -> TestClient {
        let mut router = Router::new();
        router.middleware(header);
        router.get("/users", echo);
        router.version("v9", |v9| {
            v9.get("/users", echo);
        });
        TestClient::new(router)
    }

    #[tokio::test]
    async fn the_header_names_the_version_and_the_default_fills_in() {
        let client = header_client(VersionHeader::new("X-API-Version").default("1"));
        let asked = Request::new(Method::Get, "/users").with_header("x-api-version", "2");
        assert_eq!(client.send(asked).await.body(), "2");
        assert_eq!(client.get("/users").await.body(), "1");
    }

    #[tokio::test]
    async fn without_a_default_an_unversioned_request_has_no_version() {
        let client = header_client(VersionHeader::new("X-API-Version"));
        assert_eq!(client.get("/users").await.body(), "none");
    }

    #[tokio::test]
    async fn a_vendor_media_type_in_accept_works_too() {
        let client = header_client(VersionHeader::new("X-API-Version").from_accept("example"));
        let request = Request::new(Method::Get, "/users")
            .with_header("accept", "application/vnd.example.v3+json, application/json;q=0.5");
        assert_eq!(client.send(request).await.body(), "3");
    }

    #[tokio::test]
    async fn an_unknown_version_is_a_400_listing_the_real_ones() {
        let client = header_client(VersionHeader::new("X-API-Version").default("1").allow(["1", "2"]));
        let request = Request::new(Method::Get, "/users").with_header("x-api-version", "7");
        let response = client.send(request).await;
        let response = response.assert_status(400);
        let body = response.json();
        assert!(body.get("message").and_then(Json::as_str).unwrap().contains("`7`"));
        assert_eq!(body.get("available").and_then(Json::as_str), Some("1, 2"));
    }

    #[tokio::test]
    async fn the_path_wins_over_the_header() {
        let client = header_client(VersionHeader::new("X-API-Version").default("1"));
        let request = Request::new(Method::Get, "/v9/users").with_header("x-api-version", "2");
        assert_eq!(client.send(request).await.body(), "v9");
    }
}
