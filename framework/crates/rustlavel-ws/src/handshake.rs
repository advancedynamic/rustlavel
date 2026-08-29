//! The RFC 6455 opening handshake.
//!
//! A WebSocket connection starts life as an ordinary HTTP GET that asks to stop
//! being HTTP:
//!
//! ```text
//! GET /ws HTTP/1.1
//! Upgrade: websocket
//! Connection: Upgrade
//! Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==
//! Sec-WebSocket-Version: 13
//! ```
//!
//! The server answers `101` and proves it understood the protocol — rather than
//! being a cache or a proxy that echoed the request back — by hashing the key
//! together with a fixed GUID:
//!
//! ```text
//! Sec-WebSocket-Accept: base64(sha1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))
//! ```
//!
//! That is SHA-1, not SHA-256. It is not being used as a security primitive
//! here — it is a fingerprint that says "I speak RFC 6455" — but it is still a
//! hash, and hashes are the one thing this framework does not write by hand.

use crate::base64;
use rustlavel_http::{IntoResponse, Method, Request, Response, Status};
use sha1::{Digest, Sha1};

/// The constant every RFC 6455 implementation concatenates onto the key.
pub const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The only protocol version this crate speaks. There is no 12, and there will
/// not be a 14.
pub const VERSION: &str = "13";

/// `base64(sha1(key + GUID))` — the value of `Sec-WebSocket-Accept`.
pub fn accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(ACCEPT_GUID.as_bytes());
    base64::encode(&hasher.finalize())
}

/// Everything the opening handshake settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub key: String,
    pub accept: String,
    /// The subprotocol both ends agreed on, if any.
    pub protocol: Option<String>,
    /// What the client offered, in its order of preference.
    pub offered: Vec<String>,
}

/// Why a handshake was refused.
///
/// Each variant becomes a `400` whose body names the header that was wrong,
/// because the person reading it is usually holding a half-written client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    NotAGet(Method),
    MissingUpgrade,
    NotWebSocket(String),
    MissingConnection,
    ConnectionNotUpgrade(String),
    MissingVersion,
    UnsupportedVersion(String),
    MissingKey,
    MalformedKey(String),
}

impl HandshakeError {
    pub fn message(&self) -> String {
        match self {
            HandshakeError::NotAGet(method) => {
                format!("a WebSocket handshake must be a GET, not a {method}")
            }
            HandshakeError::MissingUpgrade => {
                "missing the `Upgrade: websocket` header".to_string()
            }
            HandshakeError::NotWebSocket(value) => {
                format!("`Upgrade` was `{value}`, expected `websocket`")
            }
            HandshakeError::MissingConnection => {
                "missing the `Connection: Upgrade` header".to_string()
            }
            HandshakeError::ConnectionNotUpgrade(value) => {
                format!("`Connection` was `{value}` and does not list `Upgrade`")
            }
            HandshakeError::MissingVersion => {
                format!("missing the `Sec-WebSocket-Version: {VERSION}` header")
            }
            HandshakeError::UnsupportedVersion(value) => format!(
                "`Sec-WebSocket-Version` was `{value}`; this server speaks version {VERSION}"
            ),
            HandshakeError::MissingKey => "missing the `Sec-WebSocket-Key` header".to_string(),
            HandshakeError::MalformedKey(value) => format!(
                "`Sec-WebSocket-Key` was `{value}`; it must be 16 random bytes, base64-encoded"
            ),
        }
    }
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for HandshakeError {}

/// A refused handshake answers `400` and says what was wrong, plus the version
/// header a client would need to retry with — which is what the RFC asks for
/// and what makes the failure debuggable from the browser console.
impl IntoResponse for HandshakeError {
    fn into_response(self) -> Response {
        Response::new(Status::BAD_REQUEST)
            .with_header("sec-websocket-version", VERSION)
            .with_text(format!("WebSocket handshake failed: {}", self.message()))
    }
}

/// Validate the request and pick a subprotocol.
///
/// `supported` is the server's list, most preferred first; an empty list means
/// the server does not care and never sends the header back.
pub fn negotiate(request: &Request, supported: &[String]) -> Result<Handshake, HandshakeError> {
    if request.method() != Method::Get {
        return Err(HandshakeError::NotAGet(request.method()));
    }

    // Header values are compared case-insensitively: `Upgrade: WebSocket` is a
    // real thing browsers and proxies send.
    match request.header("upgrade") {
        None => return Err(HandshakeError::MissingUpgrade),
        Some(value) if !value.eq_ignore_ascii_case("websocket") => {
            return Err(HandshakeError::NotWebSocket(value.to_string()));
        }
        Some(_) => {}
    }

    match request.header("connection") {
        None => return Err(HandshakeError::MissingConnection),
        // `Connection` is a comma-separated list, and a proxy in the middle is
        // entitled to have added `keep-alive` next to `Upgrade`.
        Some(value) if !lists_token(value, "upgrade") => {
            return Err(HandshakeError::ConnectionNotUpgrade(value.to_string()));
        }
        Some(_) => {}
    }

    match request.header("sec-websocket-version") {
        None => return Err(HandshakeError::MissingVersion),
        Some(value) if value.trim() != VERSION => {
            return Err(HandshakeError::UnsupportedVersion(value.to_string()));
        }
        Some(_) => {}
    }

    let key = request.header("sec-websocket-key").ok_or(HandshakeError::MissingKey)?.trim();
    // The key is not a secret and not a credential, but it must be exactly 16
    // decoded bytes: a client that gets this wrong is usually a hand-written
    // one, and saying so beats an opaque failure three frames later.
    match base64::decode(key) {
        Some(bytes) if bytes.len() == 16 => {}
        _ => return Err(HandshakeError::MalformedKey(key.to_string())),
    }

    let offered = offered_protocols(request);
    let protocol = supported.iter().find(|wanted| offered.iter().any(|o| o == *wanted)).cloned();

    Ok(Handshake { key: key.to_string(), accept: accept_key(key), protocol, offered })
}

/// The `101` that completes the handshake. The caller attaches the upgrade.
pub fn response(handshake: &Handshake) -> Response {
    let mut response = Response::new(Status(101))
        .with_header("upgrade", "websocket")
        .with_header("connection", "Upgrade")
        .with_header("sec-websocket-accept", handshake.accept.clone());

    // Only echoed when one was actually agreed: sending a protocol the client
    // never offered is a handshake failure on the client's side.
    if let Some(protocol) = &handshake.protocol {
        response = response.with_header("sec-websocket-protocol", protocol.clone());
    }
    response
}

/// The subprotocols the client offered, in order.
fn offered_protocols(request: &Request) -> Vec<String> {
    request
        .headers()
        .get_all("sec-websocket-protocol")
        .iter()
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .collect()
}

/// Whether a comma-separated header value contains `token`.
fn lists_token(value: &str, token: &str) -> bool {
    value.split(',').any(|part| part.trim().eq_ignore_ascii_case(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handshake request with every required header, ready to be broken.
    fn request() -> Request {
        Request::new(Method::Get, "/ws")
            .with_header("upgrade", "websocket")
            .with_header("connection", "Upgrade")
            .with_header("sec-websocket-version", "13")
            .with_header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
    }

    #[test]
    fn computes_the_accept_key_from_the_rfc_6455_example() {
        // RFC 6455, section 1.3, worked through by hand.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn accepts_a_well_formed_handshake() {
        let handshake = negotiate(&request(), &[]).unwrap();

        assert_eq!(handshake.key, "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(handshake.accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(handshake.protocol, None);

        let response = response(&handshake);
        assert_eq!(response.status, Status(101));
        assert_eq!(response.headers.get("upgrade"), Some("websocket"));
        assert_eq!(response.headers.get("connection"), Some("Upgrade"));
        assert_eq!(
            response.headers.get("sec-websocket-accept"),
            Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
        assert_eq!(response.headers.get("sec-websocket-protocol"), None);
    }

    #[test]
    fn header_names_and_values_are_matched_case_insensitively() {
        let request = Request::new(Method::Get, "/ws")
            .with_header("Upgrade", "WebSocket")
            .with_header("Connection", "keep-alive, Upgrade")
            .with_header("Sec-WebSocket-Version", "13")
            .with_header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==");

        assert!(negotiate(&request, &[]).is_ok());
    }

    #[test]
    fn a_missing_upgrade_header_is_refused_by_name() {
        let mut request = request();
        request.headers_mut().remove("upgrade");

        let error = negotiate(&request, &[]).unwrap_err();
        assert_eq!(error, HandshakeError::MissingUpgrade);
        assert!(error.message().contains("Upgrade: websocket"), "{error}");
    }

    #[test]
    fn an_upgrade_to_something_else_is_refused() {
        let request = request().with_header("upgrade", "h2c");

        let error = negotiate(&request, &[]).unwrap_err();
        assert_eq!(error, HandshakeError::NotWebSocket("h2c".to_string()));
    }

    #[test]
    fn a_connection_header_that_does_not_list_upgrade_is_refused() {
        let mut missing = request();
        missing.headers_mut().remove("connection");
        assert_eq!(negotiate(&missing, &[]).unwrap_err(), HandshakeError::MissingConnection);

        let wrong = request().with_header("connection", "keep-alive");
        assert_eq!(
            negotiate(&wrong, &[]).unwrap_err(),
            HandshakeError::ConnectionNotUpgrade("keep-alive".to_string())
        );
    }

    #[test]
    fn only_version_thirteen_is_supported() {
        let mut missing = request();
        missing.headers_mut().remove("sec-websocket-version");
        assert_eq!(negotiate(&missing, &[]).unwrap_err(), HandshakeError::MissingVersion);

        let old = request().with_header("sec-websocket-version", "8");
        let error = negotiate(&old, &[]).unwrap_err();
        assert_eq!(error, HandshakeError::UnsupportedVersion("8".to_string()));
        assert!(error.message().contains("version 13"), "{error}");
    }

    #[test]
    fn a_key_that_is_not_sixteen_base64_bytes_is_refused() {
        let mut missing = request();
        missing.headers_mut().remove("sec-websocket-key");
        assert_eq!(negotiate(&missing, &[]).unwrap_err(), HandshakeError::MissingKey);

        // Valid base64, but only three bytes.
        let short = request().with_header("sec-websocket-key", "Zm9v");
        assert!(matches!(negotiate(&short, &[]), Err(HandshakeError::MalformedKey(_))));

        // Not base64 at all.
        let junk = request().with_header("sec-websocket-key", "not a key!");
        assert!(matches!(negotiate(&junk, &[]), Err(HandshakeError::MalformedKey(_))));
    }

    #[test]
    fn a_post_cannot_open_a_socket() {
        let request = Request::new(Method::Post, "/ws")
            .with_header("upgrade", "websocket")
            .with_header("connection", "Upgrade")
            .with_header("sec-websocket-version", "13")
            .with_header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");

        assert_eq!(negotiate(&request, &[]).unwrap_err(), HandshakeError::NotAGet(Method::Post));
    }

    #[test]
    fn a_refused_handshake_answers_400_and_says_what_was_wrong() {
        let response = HandshakeError::MissingKey.into_response();

        assert_eq!(response.status, Status::BAD_REQUEST);
        assert!(response.body_string().contains("Sec-WebSocket-Key"), "{}", response.body_string());
        assert_eq!(response.headers.get("sec-websocket-version"), Some("13"));
    }

    #[test]
    fn subprotocol_negotiation_follows_the_servers_preference() {
        let request = request().with_header("sec-websocket-protocol", "soap, chat, superchat");
        let supported = vec!["superchat".to_string(), "chat".to_string()];

        let handshake = negotiate(&request, &supported).unwrap();

        assert_eq!(handshake.offered, ["soap", "chat", "superchat"]);
        assert_eq!(handshake.protocol.as_deref(), Some("superchat"));
        assert_eq!(
            response(&handshake).headers.get("sec-websocket-protocol"),
            Some("superchat")
        );
    }

    #[test]
    fn no_shared_subprotocol_still_opens_a_plain_socket() {
        let request = request().with_header("sec-websocket-protocol", "soap");

        let handshake = negotiate(&request, &["chat".to_string()]).unwrap();

        // The RFC lets the server simply not select one, and a client that
        // insists is free to close. Refusing outright would break clients that
        // offer a protocol only opportunistically.
        assert_eq!(handshake.protocol, None);
        assert_eq!(response(&handshake).headers.get("sec-websocket-protocol"), None);
    }
}
