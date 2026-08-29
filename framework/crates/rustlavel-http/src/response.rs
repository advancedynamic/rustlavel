use crate::cookie::Cookie;
use crate::headers::Headers;
use crate::status::Status;
use rustlavel_core::{Error, Json};

/// An outgoing response.
#[derive(Clone)]
pub struct Response {
    pub status: Status,
    pub headers: Headers,
    pub body: Vec<u8>,
    /// Set when the handler answered `101` and wants the socket.
    pub(crate) upgrade: Option<crate::upgrade::Upgrader>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .field("upgrades", &self.upgrade.is_some())
            .finish()
    }
}

impl Response {
    pub fn new(status: Status) -> Self {
        Response { status, headers: Headers::new(), body: Vec::new(), upgrade: None }
    }

    /// Answer `101` and hand the connection to another protocol.
    ///
    /// The caller sets whatever headers the new protocol's handshake requires;
    /// this only arranges for the socket to be handed over afterwards.
    pub fn upgrading(mut self, upgrade: impl crate::upgrade::Upgrade) -> Self {
        self.status = Status(101);
        self.upgrade = Some(std::sync::Arc::new(upgrade));
        self
    }

    /// Whether this response takes the connection over.
    pub fn upgrades(&self) -> bool {
        self.upgrade.is_some()
    }

    /// Take the upgrade out of a dispatched response.
    ///
    /// Public so a test can drive an upgrade through the router rather than
    /// only over a real socket.
    pub fn take_upgrade(&mut self) -> Option<crate::upgrade::Upgrader> {
        self.upgrade.take()
    }

    pub fn ok() -> Self {
        Response::new(Status::OK)
    }

    pub fn no_content() -> Self {
        Response::new(Status::NO_CONTENT)
    }

    pub fn not_found() -> Self {
        Response::new(Status::NOT_FOUND).with_html("<h1>404 Not Found</h1>")
    }

    /// A `302` redirect. Use [`Response::see_other`] after a form submission so
    /// the browser switches to GET.
    pub fn redirect(location: impl Into<String>) -> Self {
        Response::new(Status::FOUND).with_header("location", location)
    }

    pub fn see_other(location: impl Into<String>) -> Self {
        Response::new(Status::SEE_OTHER).with_header("location", location)
    }

    pub fn json(value: impl Into<Json>) -> Self {
        Response::ok().with_json(value)
    }

    pub fn html(body: impl Into<String>) -> Self {
        Response::ok().with_html(body)
    }

    pub fn text(body: impl Into<String>) -> Self {
        Response::ok().with_text(body)
    }

    pub fn with_status(mut self, status: impl Into<Status>) -> Self {
        self.status = status.into();
        self
    }

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.set(name, value);
        self
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_json(self, value: impl Into<Json>) -> Self {
        self.with_header("content-type", "application/json; charset=utf-8")
            .with_body(value.into().to_string())
    }

    pub fn with_html(self, body: impl Into<String>) -> Self {
        self.with_header("content-type", "text/html; charset=utf-8").with_body(body.into())
    }

    pub fn with_text(self, body: impl Into<String>) -> Self {
        self.with_header("content-type", "text/plain; charset=utf-8").with_body(body.into())
    }

    pub fn with_cookie(mut self, cookie: Cookie) -> Self {
        self.headers.append("set-cookie", cookie.to_header());
        self
    }

    /// Expire a cookie on the client.
    pub fn without_cookie(self, name: &str) -> Self {
        self.with_cookie(Cookie::forget(name))
    }

    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Serialize into an HTTP/1.1 wire response.
    ///
    /// `include_body` is false for HEAD requests, which must report the length
    /// they would have sent while sending nothing.
    pub fn to_bytes(&self, include_body: bool) -> Vec<u8> {
        let bodyless = self.status.is_bodyless();
        let body: &[u8] = if bodyless { &[] } else { &self.body };

        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status.code(), self.status.reason());
        for (name, value) in self.headers.iter() {
            if name == "content-length" {
                continue;
            }
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if !bodyless {
            head.push_str(&format!("content-length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");

        let mut out = head.into_bytes();
        if include_body && !bodyless {
            out.extend_from_slice(body);
        }
        out
    }
}

impl Default for Response {
    fn default() -> Self {
        Response::ok()
    }
}

/// Anything a handler is allowed to return.
///
/// This is what lets a handler return a `&str`, a `Json`, a `Result`, or a full
/// `Response` without wrapping it by hand.
pub trait IntoResponse {
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for Status {
    fn into_response(self) -> Response {
        Response::new(self)
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Response {
        Response::html(self)
    }
}

impl IntoResponse for &str {
    fn into_response(self) -> Response {
        Response::html(self.to_string())
    }
}

impl IntoResponse for Json {
    fn into_response(self) -> Response {
        Response::json(self)
    }
}

impl IntoResponse for () {
    fn into_response(self) -> Response {
        Response::no_content()
    }
}

impl<T: IntoResponse> IntoResponse for (u16, T) {
    fn into_response(self) -> Response {
        let (status, inner) = self;
        inner.into_response().with_status(status)
    }
}

/// `None` becomes a 404, which makes `req.param_as::<i64>("id")?` style
/// lookups read naturally in a handler.
impl<T: IntoResponse> IntoResponse for Option<T> {
    fn into_response(self) -> Response {
        match self {
            Some(value) => value.into_response(),
            None => Response::not_found(),
        }
    }
}

/// A framework error renders as the development error page, or a generic 500
/// in production.
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        crate::error_page::response_for(&self)
    }
}

/// `?` in a handler works for any error that knows how to become a response.
///
/// This is what lets validation return a 422 with its own JSON body while a
/// framework error still reaches the error page: each error type decides how it
/// is rendered, rather than everything collapsing into a 500.
impl<T: IntoResponse, E: IntoResponse> IntoResponse for Result<T, E> {
    fn into_response(self) -> Response {
        match self {
            Ok(value) => value.into_response(),
            Err(error) => error.into_response(),
        }
    }
}

/// An I/O failure in a handler is a server error; carried separately because
/// `std::io::Error` cannot implement a foreign trait here.
impl IntoResponse for std::io::Error {
    fn into_response(self) -> Response {
        crate::error_page::response_for(&Error::Io(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_a_wire_response() {
        let response = Response::json(Json::object([("ok", true.into())]));
        let wire = String::from_utf8(response.to_bytes(true)).unwrap();

        assert!(wire.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(wire.contains("content-type: application/json; charset=utf-8\r\n"));
        assert!(wire.contains("content-length: 11\r\n"));
        assert!(wire.ends_with("\r\n\r\n{\"ok\":true}"));
    }

    #[test]
    fn head_requests_keep_the_length_but_drop_the_body() {
        let response = Response::text("hello");
        let wire = String::from_utf8(response.to_bytes(false)).unwrap();

        assert!(wire.contains("content-length: 5\r\n"));
        assert!(wire.ends_with("\r\n\r\n"));
    }

    #[test]
    fn bodyless_statuses_send_no_content_length() {
        let wire = String::from_utf8(Response::no_content().to_bytes(true)).unwrap();
        assert!(!wire.contains("content-length"));
    }

    #[test]
    fn repeated_cookies_each_get_their_own_header() {
        let response = Response::ok()
            .with_cookie(Cookie::new("a", "1"))
            .with_cookie(Cookie::new("b", "2"));
        let wire = String::from_utf8(response.to_bytes(true)).unwrap();

        assert_eq!(wire.matches("set-cookie:").count(), 2);
    }

    #[test]
    fn common_return_types_convert() {
        assert_eq!("hi".into_response().status, Status::OK);
        assert_eq!(().into_response().status, Status::NO_CONTENT);
        assert_eq!(Option::<String>::None.into_response().status, Status::NOT_FOUND);
        assert_eq!((201, "made").into_response().status, Status::CREATED);

        let failed: Result<&str, Error> = Err(Error::msg("boom"));
        assert!(failed.into_response().status.is_error());
    }
}
