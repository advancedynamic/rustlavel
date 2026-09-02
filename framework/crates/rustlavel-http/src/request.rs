use crate::cookie;
use crate::headers::Headers;
use crate::method::Method;
use crate::url;
use rustlavel_core::{Config, Context, Json};
use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;

/// An incoming request, already parsed and matched against a route.
pub struct Request {
    pub(crate) method: Method,
    pub(crate) target: String,
    pub(crate) path: String,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: Headers,
    pub(crate) body: Vec<u8>,
    pub(crate) params: BTreeMap<String, String>,
    pub(crate) context: Context,
    pub(crate) peer: Option<SocketAddr>,
    pub(crate) route: Option<String>,
    /// Values attached by middleware — the authenticated user, a request id.
    extensions: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    /// Parsed lazily on first access, since most requests never read a body.
    parsed_body: Option<ParsedBody>,
}

enum ParsedBody {
    Json(Json),
    Form(Vec<(String, String)>),
    None,
}

impl Request {
    /// Build a request directly. This is what the test client and the server
    /// parser both go through.
    pub fn new(method: Method, target: impl Into<String>) -> Self {
        let target = target.into();
        let (path, query) = url::split_target(&target);
        Request {
            method,
            path: path.to_string(),
            query: url::parse_query(query),
            target,
            headers: Headers::new(),
            body: Vec::new(),
            params: BTreeMap::new(),
            context: Context::default(),
            peer: None,
            route: None,
            extensions: HashMap::new(),
            parsed_body: None,
        }
    }

    pub fn method(&self) -> Method {
        self.method
    }

    /// The path with no query string: `/users/7`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The raw request target, query string included.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The pattern this request matched: `/users/{id}`. Useful for metrics
    /// that must not explode into one series per id.
    pub fn route(&self) -> Option<&str> {
        self.route.as_deref()
    }

    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    pub fn headers_mut(&mut self) -> &mut Headers {
        &mut self.headers
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn context(&self) -> &Context {
        &self.context
    }

    pub fn config(&self) -> &Config {
        self.context.config()
    }

    /// A service registered on the application: `req.state::<Database>()`.
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.context.state::<T>()
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// The client IP, honouring `X-Forwarded-For` when behind a proxy.
    pub fn ip(&self) -> Option<String> {
        if let Some(forwarded) = self.headers.get("x-forwarded-for")
            && let Some(first) = forwarded.split(',').next() {
                return Some(first.trim().to_string());
            }
        self.peer.map(|addr| addr.ip().to_string())
    }

    /// A route parameter: for `/users/{id}` matching `/users/7`, `param("id")`
    /// is `"7"`.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }

    /// A route parameter parsed into a type, so a handler can ask for an id as
    /// a number without unwrapping twice.
    pub fn param_as<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.param(name)?.parse().ok()
    }

    pub fn params(&self) -> &BTreeMap<String, String> {
        &self.params
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// Every value for a repeated query key: `?tag=a&tag=b`.
    pub fn query_all(&self, name: &str) -> Vec<&str> {
        self.query
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    pub fn query_pairs(&self) -> &[(String, String)] {
        &self.query
    }

    pub fn content_type(&self) -> Option<&str> {
        self.headers.content_type()
    }

    pub fn is_json(&self) -> bool {
        self.content_type().is_some_and(|ct| ct.ends_with("json"))
    }

    /// Whether the client wants JSON back — an API client or a fetch() call.
    pub fn wants_json(&self) -> bool {
        self.is_json()
            || self.headers.get("accept").is_some_and(|a| a.contains("application/json"))
            || self.headers.get("x-requested-with").is_some_and(|x| x == "XMLHttpRequest")
    }

    /// The body parsed as JSON, or `None` if it is absent or malformed.
    pub fn json(&mut self) -> Option<&Json> {
        self.parse_body();
        match self.parsed_body.as_ref()? {
            ParsedBody::Json(value) => Some(value),
            _ => None,
        }
    }

    /// One input value, looked up in the JSON body, then the form body, then
    /// the query string — the resolution order of Laravel's `$request->input()`.
    pub fn input(&mut self, name: &str) -> Option<String> {
        self.parse_body();
        match self.parsed_body.as_ref() {
            Some(ParsedBody::Json(value)) => {
                if let Some(found) = value.get(name) {
                    return Some(match found {
                        Json::String(s) => s.clone(),
                        Json::Null => String::new(),
                        other => other.to_string(),
                    });
                }
            }
            Some(ParsedBody::Form(pairs)) => {
                if let Some((_, value)) = pairs.iter().find(|(key, _)| key == name) {
                    return Some(value.clone());
                }
            }
            _ => {}
        }
        self.query(name).map(str::to_string)
    }

    /// All decoded form fields of a `application/x-www-form-urlencoded` body.
    pub fn form(&mut self) -> &[(String, String)] {
        self.parse_body();
        match self.parsed_body.as_ref() {
            Some(ParsedBody::Form(pairs)) => pairs,
            _ => &[],
        }
    }

    fn parse_body(&mut self) {
        if self.parsed_body.is_some() {
            return;
        }
        let parsed = match self.headers.content_type() {
            _ if self.body.is_empty() => ParsedBody::None,
            Some(ct) if ct.ends_with("json") => match std::str::from_utf8(&self.body) {
                Ok(text) => Json::parse(text).map_or(ParsedBody::None, ParsedBody::Json),
                Err(_) => ParsedBody::None,
            },
            Some("application/x-www-form-urlencoded") => {
                ParsedBody::Form(url::parse_query(&String::from_utf8_lossy(&self.body)))
            }
            _ => ParsedBody::None,
        };
        self.parsed_body = Some(parsed);
    }

    pub fn cookies(&self) -> BTreeMap<String, String> {
        self.headers.get("cookie").map(cookie::parse_header).unwrap_or_default()
    }

    pub fn cookie(&self, name: &str) -> Option<String> {
        self.cookies().remove(name)
    }

    /// Attach a value for later middleware or the handler to read.
    pub fn extend<T: Send + Sync + 'static>(&mut self, value: T) {
        self.extensions.insert(TypeId::of::<T>(), Box::new(value));
    }

    /// The identifier the [`RequestId`](crate::request_id::RequestId)
    /// middleware assigned, for log lines and error reports.
    pub fn request_id(&self) -> Option<&str> {
        self.extension::<crate::request_id::Assigned>().map(|id| id.0.as_str())
    }

    /// Read a value attached by earlier middleware.
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get(&TypeId::of::<T>()).and_then(|value| value.downcast_ref::<T>())
    }

    // --- Builders, used by the server, the router, and the test client. ---

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.set(name, value);
        self
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self.parsed_body = None;
        self
    }

    pub fn with_json(self, value: Json) -> Self {
        self.with_header("content-type", "application/json").with_body(value.to_string())
    }

    pub fn with_form(self, fields: &[(&str, &str)]) -> Self {
        let encoded = fields
            .iter()
            .map(|(key, value)| format!("{}={}", url::encode(key), url::encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        self.with_header("content-type", "application/x-www-form-urlencoded").with_body(encoded)
    }

    pub fn with_context(mut self, context: Context) -> Self {
        self.context = context;
        self
    }

    pub(crate) fn set_params(&mut self, params: BTreeMap<String, String>) {
        self.params = params;
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("target", &self.target)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_path_and_query() {
        let request = Request::new(Method::Get, "/users?page=2&tag=a&tag=b");

        assert_eq!(request.path(), "/users");
        assert_eq!(request.query("page"), Some("2"));
        assert_eq!(request.query_all("tag"), ["a", "b"]);
        assert_eq!(request.query("missing"), None);
    }

    #[test]
    fn input_prefers_the_body_over_the_query() {
        let mut request = Request::new(Method::Post, "/users?name=from-query")
            .with_json(Json::object([("name", "from-body".into())]));

        assert_eq!(request.input("name").as_deref(), Some("from-body"));
        // A key absent from the body still falls through to the query string.
        assert_eq!(request.input("missing"), None);
    }

    #[test]
    fn reads_urlencoded_form_bodies() {
        let mut request =
            Request::new(Method::Post, "/login").with_form(&[("email", "a@b.com"), ("password", "s e c")]);

        assert_eq!(request.input("email").as_deref(), Some("a@b.com"));
        assert_eq!(request.input("password").as_deref(), Some("s e c"));
        assert_eq!(request.form().len(), 2);
    }

    #[test]
    fn parses_cookies_from_the_header() {
        let request = Request::new(Method::Get, "/").with_header("cookie", "session=abc; theme=dark");

        assert_eq!(request.cookie("session").as_deref(), Some("abc"));
        assert_eq!(request.cookies().len(), 2);
    }

    #[test]
    fn extensions_round_trip_through_middleware() {
        struct User(&'static str);
        let mut request = Request::new(Method::Get, "/");
        request.extend(User("ada"));

        assert_eq!(request.extension::<User>().unwrap().0, "ada");
    }

    #[test]
    fn forwarded_header_wins_over_socket_address() {
        let request = Request::new(Method::Get, "/").with_header("x-forwarded-for", "203.0.113.9, 10.0.0.1");
        assert_eq!(request.ip().as_deref(), Some("203.0.113.9"));
    }

    #[test]
    fn detects_clients_that_want_json() {
        let api = Request::new(Method::Get, "/").with_header("accept", "application/json");
        let browser = Request::new(Method::Get, "/").with_header("accept", "text/html");

        assert!(api.wants_json());
        assert!(!browser.wants_json());
    }
}
